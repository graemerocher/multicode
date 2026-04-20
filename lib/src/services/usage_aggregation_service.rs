use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::{broadcast, watch};

use super::{
    workspace_task_watch::watch_workspace_task, workspace_watch::monitor_workspace_snapshots,
};
use crate::{
    WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot, manager::Workspace, opencode,
};

#[derive(Debug)]
pub enum UsageAggregationServiceError {
    Manager(WorkspaceManagerError),
}

const INITIAL_SYNC_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const INITIAL_SYNC_RETRY_ATTEMPTS: usize = 5;

impl From<WorkspaceManagerError> for UsageAggregationServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct MessageUsage {
    tokens: u64,
    cost: f64,
}

pub async fn usage_aggregation_service(
    manager: Arc<WorkspaceManager>,
) -> Result<(), UsageAggregationServiceError> {
    monitor_workspace_snapshots(manager, |_, workspace, workspace_rx| async move {
        tokio::spawn(async move {
            watch_workspace_snapshot(workspace, workspace_rx).await;
        });
        Ok(())
    })
    .await
}

#[derive(Clone)]
struct UsageTaskKey {
    session_id: String,
    client: Arc<opencode::client::Client>,
    event_tx: broadcast::Sender<opencode::client::types::GlobalEvent>,
}

impl PartialEq for UsageTaskKey {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id && Arc::ptr_eq(&self.client, &other.client)
    }
}

async fn watch_workspace_snapshot(
    workspace: Workspace,
    workspace_rx: watch::Receiver<WorkspaceSnapshot>,
) {
    watch_workspace_task(
        workspace,
        workspace_rx,
        |snapshot| {
            Some(UsageTaskKey {
                session_id: snapshot.root_session_id.clone()?,
                client: snapshot.opencode_client.as_ref()?.client.clone(),
                event_tx: snapshot.opencode_client.as_ref()?.events.clone(),
            })
        },
        clear_usage_if_detached,
        |workspace: &Workspace, next_key: &UsageTaskKey, _: Option<UsageTaskKey>| {
            clear_usage_before_restart(workspace, next_key)
        },
        |workspace: &Workspace, key: &UsageTaskKey| {
            let task_workspace = workspace.clone();
            let task_client = key.client.clone();
            let task_session_id = key.session_id.clone();
            let task_event_tx = key.event_tx.clone();
            tokio::spawn(async move {
                sync_usage_from_history_and_events(
                    task_workspace,
                    task_client,
                    task_event_tx,
                    task_session_id,
                )
                .await;
            })
        },
    )
    .await;
}

fn clear_usage_before_restart(workspace: &Workspace, next_key: &UsageTaskKey) {
    workspace.update(|snapshot| {
        let same_session =
            snapshot.root_session_id.as_deref() == Some(next_key.session_id.as_str());
        let same_client = snapshot
            .opencode_client
            .as_ref()
            .map(|current_client| Arc::ptr_eq(&current_client.client, &next_key.client))
            .unwrap_or(false);
        if same_session && same_client {
            let mut changed = false;
            if snapshot.usage_total_tokens.is_some() {
                snapshot.usage_total_tokens = None;
                changed = true;
            }
            if snapshot.usage_total_cost.is_some() {
                snapshot.usage_total_cost = None;
                changed = true;
            }
            changed
        } else {
            false
        }
    });
}

fn clear_usage_if_detached(workspace: &Workspace) {
    workspace.update(|snapshot| {
        let has_usage_metadata =
            snapshot.usage_total_tokens.is_some() || snapshot.usage_total_cost.is_some();
        let should_clear = has_usage_metadata
            && (snapshot.opencode_client.is_none()
                || snapshot.transient.is_none()
                || snapshot.root_session_id.is_none());
        if should_clear {
            snapshot.usage_total_tokens = None;
            snapshot.usage_total_cost = None;
            true
        } else {
            false
        }
    });
}

async fn sync_usage_from_history_and_events(
    workspace: Workspace,
    client: Arc<opencode::client::Client>,
    event_tx: broadcast::Sender<opencode::client::types::GlobalEvent>,
    session_id: String,
) {
    let mut event_rx = event_tx.subscribe();
    let mut usage_by_message = HashMap::new();
    let mut initialized = false;

    for attempt in 0..INITIAL_SYNC_RETRY_ATTEMPTS {
        match query_usage_map(client.as_ref(), &session_id).await {
            Ok(usage_map) => {
                usage_by_message = usage_map;
                refresh_snapshot_usage(&workspace, &client, &session_id, &usage_by_message);
                initialized = true;
                break;
            }
            Err(_) => {
                if attempt + 1 < INITIAL_SYNC_RETRY_ATTEMPTS {
                    tokio::time::sleep(INITIAL_SYNC_RETRY_INTERVAL).await;
                }
            }
        }
    }

    loop {
        match event_rx.recv().await {
            Ok(event) => {
                if !initialized
                    && let Ok(usage_map) = query_usage_map(client.as_ref(), &session_id).await
                {
                    usage_by_message = usage_map;
                    refresh_snapshot_usage(&workspace, &client, &session_id, &usage_by_message);
                    initialized = true;
                }
                if apply_usage_event(&event, &session_id, &mut usage_by_message) {
                    refresh_snapshot_usage(&workspace, &client, &session_id, &usage_by_message);
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                if let Ok(usage_map) = query_usage_map(client.as_ref(), &session_id).await {
                    usage_by_message = usage_map;
                    refresh_snapshot_usage(&workspace, &client, &session_id, &usage_by_message);
                    initialized = true;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn refresh_snapshot_usage(
    workspace: &Workspace,
    client: &Arc<opencode::client::Client>,
    session_id: &str,
    usage_by_message: &HashMap<String, MessageUsage>,
) {
    let (total_tokens, total_cost) = sum_usage(usage_by_message);
    workspace.update(|snapshot| {
        let still_tracking_same_client = snapshot
            .opencode_client
            .as_ref()
            .map(|opencode_client| Arc::ptr_eq(&opencode_client.client, client))
            .unwrap_or(false);
        let still_tracking_same_session = snapshot.root_session_id.as_deref() == Some(session_id);
        let next_tokens = Some(total_tokens);
        let next_cost = Some(total_cost);
        let should_update = still_tracking_same_client
            && still_tracking_same_session
            && (snapshot.usage_total_tokens != next_tokens
                || snapshot.usage_total_cost != next_cost);
        if should_update {
            snapshot.usage_total_tokens = next_tokens;
            snapshot.usage_total_cost = next_cost;
            true
        } else {
            false
        }
    });
}

async fn query_usage_map(
    client: &opencode::client::Client,
    session_id: &str,
) -> Result<
    HashMap<String, MessageUsage>,
    opencode::client::Error<opencode::client::types::BadRequestError>,
> {
    let session_id = session_id
        .parse::<opencode::client::types::SessionMessagesSessionId>()
        .map_err(|error| {
            opencode::client::Error::InvalidRequest(format!(
                "invalid session ID '{session_id}': {error}"
            ))
        })?;
    let messages = client
        .session_messages(&session_id, None, None, None, None)
        .await?
        .into_inner();
    let mut usage_by_message = HashMap::new();

    for message in messages {
        if let Some((message_id, usage)) = usage_from_message(&message.info) {
            usage_by_message.insert(message_id, usage);
        }
    }

    Ok(usage_by_message)
}

fn apply_usage_event(
    event: &opencode::client::types::GlobalEvent,
    session_id: &str,
    usage_by_message: &mut HashMap<String, MessageUsage>,
) -> bool {
    match &event.payload {
        opencode::client::types::GlobalEventPayload::EventMessageUpdated(message_updated) => {
            let Some((message_id, updated_session_id, usage)) =
                usage_update_from_message(&message_updated.properties.info)
            else {
                return false;
            };
            if updated_session_id != session_id {
                return false;
            }
            match usage {
                Some(usage) => match usage_by_message.insert(message_id, usage) {
                    Some(previous) => previous != usage,
                    None => true,
                },
                None => usage_by_message.remove(&message_id).is_some(),
            }
        }
        opencode::client::types::GlobalEventPayload::SyncEventMessageUpdated(message_updated) => {
            let Some((message_id, updated_session_id, usage)) =
                usage_update_from_message(&message_updated.data.info)
            else {
                return false;
            };
            if updated_session_id != session_id {
                return false;
            }
            match usage {
                Some(usage) => match usage_by_message.insert(message_id, usage) {
                    Some(previous) => previous != usage,
                    None => true,
                },
                None => usage_by_message.remove(&message_id).is_some(),
            }
        }
        opencode::client::types::GlobalEventPayload::EventMessageRemoved(message_removed) => {
            if message_removed.properties.session_id.as_str() != session_id {
                return false;
            }
            usage_by_message
                .remove(message_removed.properties.message_id.as_str())
                .is_some()
        }
        opencode::client::types::GlobalEventPayload::SyncEventMessageRemoved(message_removed) => {
            if message_removed.data.session_id.as_str() != session_id {
                return false;
            }
            usage_by_message
                .remove(message_removed.data.message_id.as_str())
                .is_some()
        }
        _ => false,
    }
}

fn usage_update_from_message(
    message: &opencode::client::types::Message,
) -> Option<(String, String, Option<MessageUsage>)> {
    match message {
        opencode::client::types::Message::AssistantMessage(message) => Some((
            message.id.to_string(),
            message.session_id.to_string(),
            Some(message_usage_from_assistant_message(message)),
        )),
        opencode::client::types::Message::UserMessage(message) => {
            Some((message.id.to_string(), message.session_id.to_string(), None))
        }
    }
}

fn usage_from_message(
    message: &opencode::client::types::Message,
) -> Option<(String, MessageUsage)> {
    match message {
        opencode::client::types::Message::AssistantMessage(message) => Some((
            message.id.to_string(),
            message_usage_from_assistant_message(message),
        )),
        opencode::client::types::Message::UserMessage(_) => None,
    }
}

fn message_usage_from_assistant_message(
    message: &opencode::client::types::AssistantMessage,
) -> MessageUsage {
    MessageUsage {
        tokens: normalize_tokens(&message.tokens),
        cost: normalize_cost(message.cost),
    }
}

fn normalize_tokens(tokens: &opencode::client::types::AssistantMessageTokens) -> u64 {
    let total = tokens.total.unwrap_or(
        tokens.input + tokens.output + tokens.reasoning + tokens.cache.read + tokens.cache.write,
    );
    normalize_count(total)
}

fn normalize_count(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value.round() as u64
    } else {
        0
    }
}

fn normalize_cost(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

fn sum_usage(usage_by_message: &HashMap<String, MessageUsage>) -> (u64, f64) {
    usage_by_message
        .values()
        .fold((0_u64, 0.0_f64), |(total_tokens, total_cost), usage| {
            (
                total_tokens.saturating_add(usage.tokens),
                total_cost + usage.cost,
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        OpencodeClientSnapshot, RuntimeBackend, RuntimeHandleSnapshot, TransientWorkspaceSnapshot,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn transient_snapshot(uri: String, runtime_id: &str) -> TransientWorkspaceSnapshot {
        TransientWorkspaceSnapshot {
            uri,
            runtime: RuntimeHandleSnapshot {
                backend: RuntimeBackend::LinuxSystemdBwrap,
                id: runtime_id.to_string(),
                metadata: Default::default(),
            },
        }
    }

    fn assistant_message_json(
        message_id: &str,
        session_id: &str,
        created_at: f64,
        cost: f64,
        total_tokens: f64,
    ) -> serde_json::Value {
        serde_json::json!({
            "agent": "assistant",
            "cost": cost,
            "id": message_id,
            "mode": "default",
            "modelID": "model",
            "parentID": "msg-parent",
            "path": {
                "cwd": "/workspace",
                "root": "/workspace"
            },
            "providerID": "provider",
            "role": "assistant",
            "sessionID": session_id,
            "time": {
                "created": created_at
            },
            "tokens": {
                "cache": {
                    "read": 0,
                    "write": 0
                },
                "input": 10,
                "output": 5,
                "reasoning": 0,
                "total": total_tokens
            }
        })
    }

    fn session_messages_json(messages: Vec<serde_json::Value>) -> String {
        serde_json::Value::Array(
            messages
                .into_iter()
                .map(|info| serde_json::json!({"info": info, "parts": []}))
                .collect(),
        )
        .to_string()
    }

    fn message_updated_event(
        message_id: &str,
        session_id: &str,
        created_at: f64,
        cost: f64,
        total_tokens: f64,
    ) -> opencode::client::types::GlobalEvent {
        serde_json::from_value(serde_json::json!({
            "directory": "/workspace",
            "payload": {
                "type": "message.updated",
                "properties": {
                    "sessionID": session_id,
                    "info": assistant_message_json(
                        message_id,
                        session_id,
                        created_at,
                        cost,
                        total_tokens,
                    )
                }
            }
        }))
        .expect("message.updated event should parse")
    }

    fn message_removed_event(
        message_id: &str,
        session_id: &str,
    ) -> opencode::client::types::GlobalEvent {
        serde_json::from_value(serde_json::json!({
            "directory": "/workspace",
            "payload": {
                "type": "message.removed",
                "properties": {
                    "messageID": message_id,
                    "sessionID": session_id
                }
            }
        }))
        .expect("message.removed event should parse")
    }

    #[test]
    fn service_sets_initial_usage_from_message_history() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should expose local addr");
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);

                        if request.contains("GET /session/ses-root/message HTTP/1.1") {
                            let body = session_messages_json(vec![
                                assistant_message_json("msg-1", "ses-root", 1.0, 0.75, 1_500.0),
                                assistant_message_json("msg-2", "ses-root", 2.0, 1.0, 2_000.0),
                                assistant_message_json("msg-3", "ses-root", 3.0, 0.0, 0.0),
                            ]);
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        let response =
                            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");
            let workspace = manager
                .get_workspace("alpha")
                .expect("workspace should exist");
            let mut workspace_rx = workspace.subscribe();

            let base_uri = format!("http://{addr}");
            let (event_tx, _) = broadcast::channel(64);
            let client_snapshot = OpencodeClientSnapshot {
                client: Arc::new(opencode::client::Client::new(&base_uri)),
                events: event_tx,
            };
            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    format!("{base_uri}/"),
                    "run-u-usage.service",
                ));
                snapshot.root_session_id = Some("ses-root".to_string());
                snapshot.opencode_client = Some(client_snapshot.clone());
                true
            });

            let service_task = tokio::spawn(usage_aggregation_service(manager.clone()));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.usage_total_tokens == Some(3_500)
                        && snapshot.usage_total_cost == Some(1.75)
                    {
                        break;
                    }
                }
            })
            .await
            .expect("usage totals should be initialized from message history");

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn message_usage_preserves_large_cost_values() {
        let message: opencode::client::types::AssistantMessage = serde_json::from_value(
            assistant_message_json("msg-1", "ses-root", 1.0, 43_000_000_000.0, 1_000_000.0),
        )
        .expect("assistant message should parse");

        let usage = message_usage_from_assistant_message(&message);

        assert_eq!(usage.tokens, 1_000_000);
        assert_eq!(usage.cost, 43_000_000_000.0);
    }

    #[test]
    fn message_usage_preserves_normal_dollar_costs() {
        let message: opencode::client::types::AssistantMessage = serde_json::from_value(
            assistant_message_json("msg-1", "ses-root", 1.0, 0.015315, 1_000.0),
        )
        .expect("assistant message should parse");

        let usage = message_usage_from_assistant_message(&message);

        assert_eq!(usage.tokens, 1_000);
        assert_eq!(usage.cost, 0.015315);
    }

    #[test]
    fn sum_usage_sums_all_message_costs() {
        let usage_by_message = std::collections::HashMap::from([
            (
                "msg-1".to_string(),
                MessageUsage {
                    tokens: 100,
                    cost: 0.1,
                },
            ),
            (
                "msg-2".to_string(),
                MessageUsage {
                    tokens: 200,
                    cost: 0.3,
                },
            ),
            (
                "msg-3".to_string(),
                MessageUsage {
                    tokens: 250,
                    cost: 0.25,
                },
            ),
        ]);

        let (tokens, cost) = sum_usage(&usage_by_message);
        assert_eq!(tokens, 550);
        assert!((cost - 0.65).abs() < 1e-9);
    }

    #[test]
    fn service_updates_usage_sums_on_message_events() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should expose local addr");
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);

                        if request.contains("GET /session/ses-root/message HTTP/1.1") {
                            let body = session_messages_json(vec![assistant_message_json(
                                "msg-1",
                                "ses-root",
                                1.0,
                                1.0,
                                1_000.0,
                            )]);
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        let response =
                            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });

            let workspace = Workspace::new(WorkspaceSnapshot::default());
            let mut workspace_rx = workspace.subscribe();

            let base_uri = format!("http://{addr}");
            let (event_tx, _) = broadcast::channel(64);
            let client_snapshot = OpencodeClientSnapshot {
                client: Arc::new(opencode::client::Client::new(&base_uri)),
                events: event_tx.clone(),
            };
            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    format!("{base_uri}/"),
                    "run-u-usage-events.service",
                ));
                snapshot.root_session_id = Some("ses-root".to_string());
                snapshot.opencode_client = Some(client_snapshot.clone());
                true
            });

            let sync_task = tokio::spawn(watch_workspace_snapshot(
                workspace.clone(),
                workspace.subscribe(),
            ));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.usage_total_tokens == Some(1_000)
                        && snapshot.usage_total_cost == Some(1.0)
                    {
                        break;
                    }
                }
            })
            .await
            .expect("initial usage should be synchronized from history");

            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if event_tx.receiver_count() > 0 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("usage watcher should subscribe to event broadcast");

            event_tx
                .send(message_updated_event("msg-2", "ses-root", 2.0, 1.5, 3_500.0))
                .expect("message.updated event should send");

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.usage_total_tokens == Some(4_500)
                        && snapshot.usage_total_cost == Some(2.5)
                    {
                        break;
                    }
                }
            })
            .await
            .expect("message.updated should increment aggregate usage sum");

            event_tx
                .send(message_updated_event("msg-3", "ses-root", 3.0, 0.0, 0.0))
                .expect("message.updated event should send");

            event_tx
                .send(message_removed_event("msg-2", "ses-root"))
                .expect("message.removed event should send");

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.usage_total_tokens == Some(1_000)
                        && snapshot.usage_total_cost == Some(1.0)
                    {
                        break;
                    }
                }
            })
            .await
            .expect("message.removed should decrement aggregate usage sum");

            sync_task.abort();
            server_task.abort();
        });
    }
}
