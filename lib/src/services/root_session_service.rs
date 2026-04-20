use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::{broadcast, watch};

use super::{
    workspace_task_watch::watch_workspace_task, workspace_watch::monitor_workspace_snapshots,
};
use crate::{
    WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot, manager::Workspace, opencode,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSessionStatus {
    Idle,
    Busy,
    Question,
}

/// One opencode session is designated as "root". This structure holds metadata for that session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RootSessionDetails {
    id: String,
    title: String,
    status: Option<RootSessionStatus>,
}

#[derive(Debug)]
pub enum RootSessionServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for RootSessionServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

pub async fn root_session_service(
    manager: Arc<WorkspaceManager>,
) -> Result<(), RootSessionServiceError> {
    monitor_workspace_snapshots(manager, |_, workspace, workspace_rx| async move {
        tokio::spawn(async move {
            watch_workspace_snapshot(workspace, workspace_rx).await;
        });
        Ok(())
    })
    .await
}

#[derive(Clone)]
struct RootSessionTaskKey {
    uri: String,
    client: Arc<opencode::client::Client>,
    event_tx: broadcast::Sender<opencode::client::types::GlobalEvent>,
}

impl PartialEq for RootSessionTaskKey {
    fn eq(&self, other: &Self) -> bool {
        self.uri == other.uri && Arc::ptr_eq(&self.client, &other.client)
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
            Some(RootSessionTaskKey {
                uri: normalize_base_uri(&snapshot.transient.as_ref()?.uri),
                client: snapshot.opencode_client.as_ref()?.client.clone(),
                event_tx: snapshot.opencode_client.as_ref()?.events.clone(),
            })
        },
        clear_root_session_if_detached,
        |workspace: &Workspace,
         next_key: &RootSessionTaskKey,
         previous_key: Option<RootSessionTaskKey>| {
            clear_root_session_on_uri_change(workspace, next_key, previous_key.as_ref())
        },
        |workspace: &Workspace, key: &RootSessionTaskKey| {
            let task_workspace = workspace.clone();
            let task_client = key.client.clone();
            let task_event_tx = key.event_tx.clone();
            let task_uri = key.uri.clone();

            tokio::spawn(async move {
                sync_root_session_from_events(task_workspace, task_client, task_event_tx, task_uri)
                    .await;
            })
        },
    )
    .await;
}

fn clear_root_session_if_detached(workspace: &Workspace) {
    workspace.update(|snapshot| {
        let has_root_session_metadata = snapshot.root_session_id.is_some()
            || snapshot.root_session_title.is_some()
            || snapshot.root_session_status.is_some();
        let should_clear = has_root_session_metadata
            && (snapshot.opencode_client.is_none() || snapshot.transient.is_none());
        if should_clear {
            snapshot.root_session_id = None;
            snapshot.root_session_title = None;
            snapshot.root_session_status = None;
            true
        } else {
            false
        }
    });
}

fn clear_root_session_on_uri_change(
    workspace: &Workspace,
    next_key: &RootSessionTaskKey,
    previous_key: Option<&RootSessionTaskKey>,
) {
    let uri_changed = previous_key
        .map(|previous_key| previous_key.uri != next_key.uri)
        .unwrap_or(false);
    if !uri_changed {
        return;
    }

    workspace.update(|snapshot| {
        let still_on_current_uri = snapshot
            .transient
            .as_ref()
            .map(|transient| normalize_base_uri(&transient.uri))
            .as_deref()
            == Some(next_key.uri.as_str());
        if still_on_current_uri {
            let mut changed = false;
            if snapshot.root_session_id.is_some() {
                snapshot.root_session_id = None;
                changed = true;
            }
            if snapshot.root_session_title.is_some() {
                snapshot.root_session_title = None;
                changed = true;
            }
            if snapshot.root_session_status.is_some() {
                snapshot.root_session_status = None;
                changed = true;
            }
            changed
        } else {
            false
        }
    });
}

async fn sync_root_session_from_events(
    workspace: Workspace,
    client: Arc<opencode::client::Client>,
    event_tx: broadcast::Sender<opencode::client::types::GlobalEvent>,
    expected_uri: String,
) {
    let mut event_rx = event_tx.subscribe();
    refresh_root_session_details(&workspace, &client, &expected_uri).await;

    loop {
        match event_rx.recv().await {
            Ok(event) => {
                if should_refresh_from_event(&event) {
                    refresh_root_session_details(&workspace, &client, &expected_uri).await;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                refresh_root_session_details(&workspace, &client, &expected_uri).await;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn refresh_root_session_details(
    workspace: &Workspace,
    client: &Arc<opencode::client::Client>,
    expected_uri: &str,
) {
    let next_root_session = match query_current_root_session_details(client.as_ref()).await {
        Ok(root_session) => root_session,
        Err(_) => return,
    };

    workspace.update(|snapshot| {
        let still_tracking_same_client = snapshot
            .opencode_client
            .as_ref()
            .map(|opencode_client| Arc::ptr_eq(&opencode_client.client, client))
            .unwrap_or(false);
        let still_attached_to_expected_uri = snapshot
            .transient
            .as_ref()
            .map(|transient| normalize_base_uri(&transient.uri))
            .as_deref()
            == Some(expected_uri);
        let next_root_session_id = next_root_session.as_ref().map(|session| session.id.clone());
        let next_root_session_title = next_root_session
            .as_ref()
            .map(|session| session.title.clone());
        let next_root_session_status = next_root_session
            .as_ref()
            .and_then(|session| session.status);
        let should_update = still_tracking_same_client
            && still_attached_to_expected_uri
            && (snapshot.root_session_id != next_root_session_id
                || snapshot.root_session_title != next_root_session_title
                || snapshot.root_session_status != next_root_session_status);
        if should_update {
            snapshot.root_session_id = next_root_session_id;
            snapshot.root_session_title = next_root_session_title;
            snapshot.root_session_status = next_root_session_status;
            true
        } else {
            false
        }
    });
}

async fn query_current_root_session_details(
    client: &opencode::client::Client,
) -> Result<Option<RootSessionDetails>, opencode::client::Error<()>> {
    let root_session_candidate = client
        .session_list(None, Some(1.0), Some(true), None, None, None)
        .await?
        .into_inner()
        .into_iter()
        .next();

    let sessions = match client
        .session_list(None, None, None, None, None, None)
        .await
    {
        Ok(sessions) => sessions.into_inner(),
        Err(_) => root_session_candidate
            .clone()
            .into_iter()
            .collect::<Vec<_>>(),
    };

    let root_session = match select_root_session(root_session_candidate, &sessions) {
        Some(root_session) => root_session,
        None => match bootstrap_root_session(client).await {
            Ok(Some(root_session)) => root_session,
            Ok(None) => return Ok(None),
            Err(err) => {
                tracing::warn!(error = %err, "failed to bootstrap root session");
                return Ok(None);
            }
        },
    };

    let root_session_id: String = root_session.id.clone().into();
    let root_session_title = root_session.title.clone();
    let has_pending_question = match client.question_list(None, None).await {
        Ok(questions) => questions
            .into_inner()
            .into_iter()
            .any(|question| question.session_id.to_string() == root_session_id),
        Err(_) => false,
    };

    let session_status = match client.session_status(None, None).await {
        Ok(statuses) => derive_root_session_status(
            &sessions,
            &statuses.into_inner(),
            &root_session_id,
            has_pending_question,
        ),
        Err(_) => has_pending_question.then_some(RootSessionStatus::Question),
    };

    Ok(Some(RootSessionDetails {
        id: root_session_id,
        title: root_session_title,
        status: session_status,
    }))
}

async fn bootstrap_root_session(
    client: &opencode::client::Client,
) -> Result<Option<opencode::client::types::Session>, String> {
    client
        .session_create(
            None,
            None,
            &opencode::client::types::SessionCreateBody::default(),
        )
        .await
        .map(|response| Some(response.into_inner()))
        .map_err(|err| err.to_string())
}

fn select_root_session(
    root_session_candidate: Option<opencode::client::types::Session>,
    sessions: &[opencode::client::types::Session],
) -> Option<opencode::client::types::Session> {
    if let Some(candidate) = root_session_candidate {
        if candidate.parent_id.is_none() {
            return Some(candidate);
        }

        let mut visited = HashSet::new();
        visited.insert(candidate.id.to_string());
        let mut next_parent_id = candidate.parent_id.as_deref().map(|id| id.to_string());

        while let Some(parent_id) = next_parent_id {
            if !visited.insert(parent_id.clone()) {
                break;
            }

            let Some(parent_session) = sessions
                .iter()
                .find(|session| session.id.to_string() == parent_id)
            else {
                break;
            };

            if parent_session.parent_id.is_none() {
                return Some(parent_session.clone());
            }

            next_parent_id = parent_session.parent_id.as_deref().map(|id| id.to_string());
        }
    }

    sessions
        .iter()
        .find(|session| session.parent_id.is_none())
        .cloned()
}

fn derive_root_session_status(
    sessions: &[opencode::client::types::Session],
    statuses: &HashMap<String, opencode::client::types::SessionStatus>,
    root_session_id: &str,
    has_pending_question: bool,
) -> Option<RootSessionStatus> {
    let root_status = statuses.get(root_session_id).map(map_session_status);
    let parent_by_session_id = build_parent_index(sessions);
    let has_busy_subagent = statuses.iter().any(|(session_id, status)| {
        is_session_busy(status)
            && session_id != root_session_id
            && is_descendant_of_root(session_id, root_session_id, &parent_by_session_id)
    });

    if has_pending_question {
        return Some(RootSessionStatus::Question);
    }

    match root_status {
        Some(RootSessionStatus::Busy) => Some(RootSessionStatus::Busy),
        Some(RootSessionStatus::Idle) => Some(if has_busy_subagent {
            RootSessionStatus::Busy
        } else {
            RootSessionStatus::Idle
        }),
        Some(RootSessionStatus::Question) => Some(RootSessionStatus::Question),
        None if has_busy_subagent => Some(RootSessionStatus::Busy),
        None => None,
    }
}

fn build_parent_index(
    sessions: &[opencode::client::types::Session],
) -> HashMap<String, Option<String>> {
    sessions
        .iter()
        .map(|session| {
            let id: String = session.id.clone().into();
            let parent_id = session.parent_id.as_ref().map(|parent| parent.to_string());
            (id, parent_id)
        })
        .collect()
}

fn is_descendant_of_root(
    session_id: &str,
    root_session_id: &str,
    parent_by_session_id: &HashMap<String, Option<String>>,
) -> bool {
    let mut current = session_id;
    let mut visited = HashSet::new();
    while visited.insert(current.to_string()) {
        let Some(parent) = parent_by_session_id
            .get(current)
            .and_then(|parent| parent.as_deref())
        else {
            return false;
        };
        if parent == root_session_id {
            return true;
        }
        current = parent;
    }
    false
}

fn is_session_busy(status: &opencode::client::types::SessionStatus) -> bool {
    map_session_status(status) == RootSessionStatus::Busy
}

fn map_session_status(status: &opencode::client::types::SessionStatus) -> RootSessionStatus {
    match status {
        opencode::client::types::SessionStatus::Idle => RootSessionStatus::Idle,
        opencode::client::types::SessionStatus::Busy
        | opencode::client::types::SessionStatus::Retry { .. } => RootSessionStatus::Busy,
    }
}

fn should_refresh_from_event(event: &opencode::client::types::GlobalEvent) -> bool {
    matches!(
        &event.payload,
        opencode::client::types::GlobalEventPayload::EventSessionStatus(_)
            | opencode::client::types::GlobalEventPayload::EventSessionIdle(_)
            | opencode::client::types::GlobalEventPayload::EventSessionCompacted(_)
            | opencode::client::types::GlobalEventPayload::EventSessionCreated(_)
            | opencode::client::types::GlobalEventPayload::EventSessionUpdated(_)
            | opencode::client::types::GlobalEventPayload::EventSessionDeleted(_)
            | opencode::client::types::GlobalEventPayload::SyncEventSessionCreated(_)
            | opencode::client::types::GlobalEventPayload::SyncEventSessionUpdated(_)
            | opencode::client::types::GlobalEventPayload::SyncEventSessionDeleted(_)
            | opencode::client::types::GlobalEventPayload::EventSessionDiff(_)
            | opencode::client::types::GlobalEventPayload::EventSessionError(_)
            | opencode::client::types::GlobalEventPayload::EventQuestionAsked(_)
            | opencode::client::types::GlobalEventPayload::EventQuestionReplied(_)
            | opencode::client::types::GlobalEventPayload::EventQuestionRejected(_)
    )
}

fn normalize_base_uri(uri: &str) -> String {
    uri.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        OpencodeClientSnapshot, RuntimeBackend, RuntimeHandleSnapshot, TransientWorkspaceSnapshot,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::Notify,
    };

    fn session_json(session_id: &str, title: &str) -> String {
        serde_json::json!([
            {
                "directory": "/workspace",
                "id": session_id,
                "projectID": "project-1",
                "slug": "root",
                "time": {
                    "created": 1,
                    "updated": 1
                },
                "title": title,
                "version": "1"
            }
        ])
        .to_string()
    }

    fn single_session_json(session_id: &str, title: &str) -> String {
        serde_json::json!({
            "directory": "/workspace",
            "id": session_id,
            "projectID": "project-1",
            "slug": "root",
            "time": {
                "created": 1,
                "updated": 1
            },
            "title": title,
            "version": "1"
        })
        .to_string()
    }

    fn transient_snapshot(uri: &str, runtime_id: &str) -> TransientWorkspaceSnapshot {
        TransientWorkspaceSnapshot {
            uri: uri.to_string(),
            runtime: RuntimeHandleSnapshot {
                backend: RuntimeBackend::LinuxSystemdBwrap,
                id: runtime_id.to_string(),
                metadata: Default::default(),
            },
        }
    }

    fn sessions_json_with_subagent(
        root_session_id: &str,
        root_title: &str,
        subagent_session_id: &str,
    ) -> String {
        serde_json::json!([
            {
                "directory": "/workspace",
                "id": root_session_id,
                "projectID": "project-1",
                "slug": "root",
                "time": {
                    "created": 1,
                    "updated": 1
                },
                "title": root_title,
                "version": "1"
            },
            {
                "directory": "/workspace",
                "id": subagent_session_id,
                "parentID": root_session_id,
                "projectID": "project-1",
                "slug": "subagent",
                "time": {
                    "created": 1,
                    "updated": 1
                },
                "title": "Subagent session",
                "version": "1"
            }
        ])
        .to_string()
    }

    fn session_status_json(session_id: &str, status: &str) -> String {
        serde_json::json!({
            session_id: {
                "type": status
            }
        })
        .to_string()
    }

    fn parse_session(value: serde_json::Value) -> opencode::client::types::Session {
        serde_json::from_value(value).expect("session JSON should parse")
    }

    #[test]
    fn select_root_session_prefers_ancestor_of_non_root_candidate() {
        let candidate = parse_session(serde_json::json!({
            "directory": "/workspace",
            "id": "ses-child",
            "parentID": "ses-root",
            "projectID": "project-1",
            "slug": "subagent",
            "time": {"created": 1, "updated": 3},
            "title": "Child",
            "version": "1"
        }));

        let sessions = vec![
            parse_session(serde_json::json!({
                "directory": "/workspace",
                "id": "ses-other-root",
                "projectID": "project-1",
                "slug": "other",
                "time": {"created": 1, "updated": 10},
                "title": "Other root",
                "version": "1"
            })),
            parse_session(serde_json::json!({
                "directory": "/workspace",
                "id": "ses-root",
                "projectID": "project-1",
                "slug": "root",
                "time": {"created": 1, "updated": 2},
                "title": "Actual root",
                "version": "1"
            })),
            candidate.clone(),
        ];

        let selected = select_root_session(Some(candidate), &sessions)
            .expect("should resolve root ancestor from sessions list");
        assert_eq!(selected.id.to_string(), "ses-root");
    }

    #[test]
    fn select_root_session_returns_none_when_no_parentless_session_exists() {
        let candidate = parse_session(serde_json::json!({
            "directory": "/workspace",
            "id": "ses-child",
            "parentID": "ses-missing-root",
            "projectID": "project-1",
            "slug": "subagent",
            "time": {"created": 1, "updated": 3},
            "title": "Child",
            "version": "1"
        }));

        let sessions = vec![candidate.clone()];
        let selected = select_root_session(Some(candidate), &sessions);
        assert!(selected.is_none());
    }

    fn session_updated_event(session_id: &str) -> opencode::client::types::GlobalEvent {
        serde_json::from_value(serde_json::json!({
            "directory": "/workspace",
            "payload": {
                "type": "session.updated",
                "properties": {
                    "sessionID": session_id,
                    "info": {
                        "directory": "/workspace",
                        "id": session_id,
                        "projectID": "project-1",
                        "slug": "root",
                        "time": {
                            "created": 1,
                            "updated": 2
                        },
                        "title": "Root session",
                        "version": "1"
                    }
                }
            }
        }))
        .expect("session.updated event JSON should parse")
    }

    fn global_disposed_event() -> opencode::client::types::GlobalEvent {
        serde_json::from_value(serde_json::json!({
            "directory": "/workspace",
            "payload": {
                "type": "global.disposed",
                "properties": {}
            }
        }))
        .expect("global.disposed event JSON should parse")
    }

    #[test]
    fn service_sets_initial_root_session_id_from_query() {
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

                        if request.contains("GET /session/status") {
                            let body = session_status_json("ses-root-initial", "busy");
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            let body = session_json("ses-root-initial", "Root session initial");
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

            let (event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let client_snapshot = OpencodeClientSnapshot {
                client: Arc::new(opencode::client::Client::new(&base_uri)),
                events: event_tx,
            };

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    &format!("{base_uri}/"),
                    "run-u-root-session.service",
                ));
                snapshot.opencode_client = Some(client_snapshot.clone());
                true
            });

            let service_task = tokio::spawn(root_session_service(manager.clone()));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    if workspace_rx.borrow().root_session_id.as_deref() == Some("ses-root-initial") {
                        break;
                    }
                }
            })
            .await
            .expect("root session id should be set from initial query");

            let snapshot = workspace_rx.borrow().clone();
            assert_eq!(snapshot.root_session_id.as_deref(), Some("ses-root-initial"));
            assert_eq!(
                snapshot.root_session_title.as_deref(),
                Some("Root session initial")
            );
            assert_eq!(snapshot.root_session_status, Some(RootSessionStatus::Busy));

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn service_bootstraps_root_session_when_server_starts_empty() {
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

                        if request.contains("GET /question") {
                            let body = "[]";
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session/status") {
                            let body = "{}";
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.starts_with("POST /session") {
                            let body =
                                single_session_json("ses-root-created", "Root session created");
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            let body = "[]";
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

            let base_uri = format!("http://{addr}");
            let client = opencode::client::Client::new(&base_uri);
            let root_session = query_current_root_session_details(&client)
                .await
                .expect("query should succeed")
                .expect("root session should be bootstrapped when session list is empty");
            assert_eq!(root_session.id, "ses-root-created");
            assert_eq!(root_session.title, "Root session created");
            assert_eq!(root_session.status, None);

            server_task.abort();
        });
    }

    #[test]
    fn service_marks_pending_question_for_root_session() {
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

                        if request.contains("GET /question") {
                            let body = serde_json::json!([{
                                "id": "que-root",
                                "questions": [{
                                    "header": "Need input",
                                    "question": "Continue?",
                                    "options": [{
                                        "label": "Yes",
                                        "description": "Continue work"
                                    }]
                                }],
                                "sessionID": "ses-root"
                            }])
                            .to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session/status") {
                            let body = session_status_json("ses-root", "busy");
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            let body = session_json("ses-root", "Root session initial");
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

            let (event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let client_snapshot = OpencodeClientSnapshot {
                client: Arc::new(opencode::client::Client::new(&base_uri)),
                events: event_tx,
            };

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    &format!("{base_uri}/"),
                    "run-u-root-session.service",
                ));
                snapshot.opencode_client = Some(client_snapshot.clone());
                true
            });

            let service_task = tokio::spawn(root_session_service(manager.clone()));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.root_session_id.as_deref() == Some("ses-root")
                        && snapshot.root_session_status == Some(RootSessionStatus::Question)
                    {
                        break;
                    }
                }
            })
            .await
            .expect("root session pending question should be set from query");

            let snapshot = workspace_rx.borrow().clone();
            assert_eq!(snapshot.root_session_id.as_deref(), Some("ses-root"));
            assert_eq!(snapshot.root_session_status, Some(RootSessionStatus::Question));

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn reports_busy_when_root_idle_but_subagent_busy() {
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

                        if request.contains("GET /session/status") {
                            let body = serde_json::json!({
                                "ses-root": {"type": "idle"},
                                "ses-subagent": {"type": "busy"}
                            })
                            .to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            let body = sessions_json_with_subagent(
                                "ses-root",
                                "Root session initial",
                                "ses-subagent",
                            );
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

            let (event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let client_snapshot = OpencodeClientSnapshot {
                client: Arc::new(opencode::client::Client::new(&base_uri)),
                events: event_tx,
            };

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    &format!("{base_uri}/"),
                    "run-u-root-session.service",
                ));
                snapshot.opencode_client = Some(client_snapshot.clone());
                true
            });

            let service_task = tokio::spawn(root_session_service(manager.clone()));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    if workspace_rx.borrow().root_session_id.as_deref() == Some("ses-root") {
                        break;
                    }
                }
            })
            .await
            .expect("root session id should be set from initial query");

            let snapshot = workspace_rx.borrow().clone();
            assert_eq!(snapshot.root_session_id.as_deref(), Some("ses-root"));
            assert_eq!(snapshot.root_session_status, Some(RootSessionStatus::Busy));

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn root_session_selection_uses_root_filtered_query() {
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

                        if request.contains("GET /session/status") {
                            let body = serde_json::json!({
                                "ses-root": {"type": "idle"},
                                "ses-subagent": {"type": "busy"}
                            })
                            .to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session?") && request.contains("root=true") {
                            let body = serde_json::json!([
                                {
                                    "directory": "/workspace",
                                    "id": "ses-root",
                                    "projectID": "project-1",
                                    "slug": "workspace-main",
                                    "time": {
                                        "created": 1,
                                        "updated": 1
                                    },
                                    "title": "Root session initial",
                                    "version": "1"
                                }
                            ])
                            .to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            let body = serde_json::json!([
                                {
                                    "directory": "/workspace",
                                    "id": "ses-root",
                                    "projectID": "project-1",
                                    "slug": "workspace-main",
                                    "time": {
                                        "created": 1,
                                        "updated": 1
                                    },
                                    "title": "Root session initial",
                                    "version": "1"
                                },
                                {
                                    "directory": "/workspace",
                                    "id": "ses-subagent",
                                    "parentID": "ses-root",
                                    "projectID": "project-1",
                                    "slug": "agent-sub",
                                    "time": {
                                        "created": 1,
                                        "updated": 1
                                    },
                                    "title": "Subagent session",
                                    "version": "1"
                                }
                            ])
                            .to_string();
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

            let (event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let client_snapshot = OpencodeClientSnapshot {
                client: Arc::new(opencode::client::Client::new(&base_uri)),
                events: event_tx,
            };

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    &format!("{base_uri}/"),
                    "run-u-root-session.service",
                ));
                snapshot.opencode_client = Some(client_snapshot.clone());
                true
            });

            let service_task = tokio::spawn(root_session_service(manager.clone()));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    if workspace_rx.borrow().root_session_id.as_deref() == Some("ses-root") {
                        break;
                    }
                }
            })
            .await
            .expect("root session id should be set from root-filtered query");

            let snapshot = workspace_rx.borrow().clone();
            assert_eq!(snapshot.root_session_id.as_deref(), Some("ses-root"));
            assert_eq!(
                snapshot.root_session_title.as_deref(),
                Some("Root session initial")
            );
            assert_eq!(snapshot.root_session_status, Some(RootSessionStatus::Busy));

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn root_session_selection_falls_back_when_root_filter_returns_child_session() {
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

                        if request.contains("GET /session/status") {
                            let body = serde_json::json!({
                                "ses-root": {"type": "idle"},
                                "ses-subagent": {"type": "busy"}
                            })
                            .to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session?") && request.contains("root=true") {
                            let body = serde_json::json!([
                                {
                                    "directory": "/workspace",
                                    "id": "ses-subagent",
                                    "parentID": "ses-root",
                                    "projectID": "project-1",
                                    "slug": "agent-sub",
                                    "time": {
                                        "created": 1,
                                        "updated": 3
                                    },
                                    "title": "Subagent session",
                                    "version": "1"
                                }
                            ])
                            .to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            let body = sessions_json_with_subagent(
                                "ses-root",
                                "Root session initial",
                                "ses-subagent",
                            );
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

            let (event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let client_snapshot = OpencodeClientSnapshot {
                client: Arc::new(opencode::client::Client::new(&base_uri)),
                events: event_tx,
            };

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    &format!("{base_uri}/"),
                    "run-u-root-session.service",
                ));
                snapshot.opencode_client = Some(client_snapshot.clone());
                true
            });

            let service_task = tokio::spawn(root_session_service(manager.clone()));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    if workspace_rx.borrow().root_session_id.as_deref() == Some("ses-root") {
                        break;
                    }
                }
            })
            .await
            .expect("root session should fall back to parentless session");

            let snapshot = workspace_rx.borrow().clone();
            assert_eq!(snapshot.root_session_id.as_deref(), Some("ses-root"));
            assert_eq!(
                snapshot.root_session_title.as_deref(),
                Some("Root session initial")
            );
            assert_eq!(snapshot.root_session_status, Some(RootSessionStatus::Busy));

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn race_safe_order_subscribe_then_query_then_process_events() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let query_started = Arc::new(Notify::new());
            let first_query_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let query_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should expose local addr");

            let server_query_started = query_started.clone();
            let server_first_query_seen = first_query_seen.clone();
            let server_query_count = query_count.clone();
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let server_query_started = server_query_started.clone();
                    let server_first_query_seen = server_first_query_seen.clone();
                    let server_query_count = server_query_count.clone();
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);

                        if request.contains("GET /session/status") {
                            let body = "{}";
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            let call_index = server_query_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            if !server_first_query_seen.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                server_query_started.notify_waiters();
                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            }

                            let body = if call_index == 0 {
                                session_json("ses-root-old", "Root session old")
                            } else {
                                session_json("ses-root-new", "Root session new")
                            };
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
            let (event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let client = Arc::new(opencode::client::Client::new(&base_uri));
            workspace.update(|snapshot| {
                snapshot.transient =
                    Some(transient_snapshot(&format!("{base_uri}/"), "run-u-root-race.service"));
                snapshot.opencode_client = Some(OpencodeClientSnapshot {
                    client: client.clone(),
                    events: event_tx.clone(),
                });
                true
            });

            let sync_task = tokio::spawn(sync_root_session_from_events(
                workspace.clone(),
                client,
                event_tx.clone(),
                normalize_base_uri(&base_uri),
            ));

            tokio::time::timeout(std::time::Duration::from_secs(2), query_started.notified())
                .await
                .expect("first session query should start");

            assert!(
                event_tx.receiver_count() > 0,
                "event receiver must be subscribed before the first root-session query begins"
            );

            event_tx
                .send(session_updated_event("ses-root-new"))
                .expect("event send should succeed");

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    if workspace_rx.borrow().root_session_id.as_deref() == Some("ses-root-new") {
                        break;
                    }
                }
            })
            .await
            .expect("queued event should update root session id after initial query");

            let snapshot = workspace_rx.borrow().clone();
            assert_eq!(snapshot.root_session_id.as_deref(), Some("ses-root-new"));
            assert_eq!(snapshot.root_session_title.as_deref(), Some("Root session new"));
            assert_eq!(snapshot.root_session_status, None);

            sync_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn stale_task_does_not_override_root_session_after_client_replacement_on_same_uri() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let query_started = Arc::new(Notify::new());
            let first_query_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should expose local addr");

            let server_query_started = query_started.clone();
            let server_first_query_seen = first_query_seen.clone();
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let server_query_started = server_query_started.clone();
                    let server_first_query_seen = server_first_query_seen.clone();
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);

                        if request.contains("GET /session/status") {
                            let body = "{}";
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            if !server_first_query_seen
                                .swap(true, std::sync::atomic::Ordering::SeqCst)
                            {
                                server_query_started.notify_waiters();
                                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                            }

                            let body = session_json("ses-root-old", "Root session old");
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
            let (old_event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let old_client = Arc::new(opencode::client::Client::new(&base_uri));
            workspace.update(|snapshot| {
                snapshot.transient =
                    Some(transient_snapshot(&format!("{base_uri}/"), "run-u-root-stale.service"));
                snapshot.opencode_client = Some(OpencodeClientSnapshot {
                    client: old_client.clone(),
                    events: old_event_tx.clone(),
                });
                true
            });

            let stale_task = tokio::spawn(sync_root_session_from_events(
                workspace.clone(),
                old_client,
                old_event_tx,
                normalize_base_uri(&base_uri),
            ));

            tokio::time::timeout(std::time::Duration::from_secs(2), query_started.notified())
                .await
                .expect("stale task query should start");

            let (new_event_tx, _) = broadcast::channel(64);
            let new_client = Arc::new(opencode::client::Client::new(&base_uri));
            workspace.update(|snapshot| {
                snapshot.opencode_client = Some(OpencodeClientSnapshot {
                    client: new_client,
                    events: new_event_tx,
                });
                snapshot.root_session_id = Some("ses-root-new".to_string());
                snapshot.root_session_title = Some("Root session new".to_string());
                snapshot.root_session_status = Some(RootSessionStatus::Idle);
                true
            });

            tokio::time::sleep(std::time::Duration::from_millis(450)).await;
            let snapshot = workspace.subscribe().borrow().clone();
            assert_eq!(snapshot.root_session_id.as_deref(), Some("ses-root-new"));
            assert_eq!(snapshot.root_session_title.as_deref(), Some("Root session new"));
            assert_eq!(snapshot.root_session_status, Some(RootSessionStatus::Idle));

            stale_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn watcher_restarts_when_client_changes_on_same_uri() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let session_query_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should expose local addr");

            let server_session_query_count = session_query_count.clone();
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let server_session_query_count = server_session_query_count.clone();
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);

                        if request.contains("GET /session/status") {
                            let body = serde_json::json!({
                                "ses-root-old": {"type": "idle"},
                                "ses-root-new": {"type": "busy"}
                            })
                            .to_string();
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            let call_index = server_session_query_count
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let body = if call_index == 0 {
                                session_json("ses-root-old", "Root session old")
                            } else {
                                session_json("ses-root-new", "Root session new")
                            };
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
            let (old_event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let old_client = Arc::new(opencode::client::Client::new(&base_uri));

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    &format!("{base_uri}/"),
                    "run-u-root-same-uri.service",
                ));
                snapshot.opencode_client = Some(OpencodeClientSnapshot {
                    client: old_client,
                    events: old_event_tx,
                });
                true
            });

            let watcher_task = tokio::spawn(watch_workspace_snapshot(
                workspace.clone(),
                workspace.subscribe(),
            ));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.root_session_id.as_deref() == Some("ses-root-old")
                        && snapshot.root_session_status == Some(RootSessionStatus::Idle)
                    {
                        break;
                    }
                }
            })
            .await
            .expect("initial client should populate old root session metadata");

            let (new_event_tx, _) = broadcast::channel(64);
            let new_client = Arc::new(opencode::client::Client::new(&base_uri));
            workspace.update(|snapshot| {
                snapshot.opencode_client = Some(OpencodeClientSnapshot {
                    client: new_client,
                    events: new_event_tx,
                });
                true
            });

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.root_session_id.as_deref() == Some("ses-root-new")
                        && snapshot.root_session_title.as_deref() == Some("Root session new")
                        && snapshot.root_session_status == Some(RootSessionStatus::Busy)
                    {
                        break;
                    }
                }
            })
            .await
            .expect("watcher should restart and refresh root session metadata on same-uri client swap");

            watcher_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn ignores_non_session_events_without_requerying_sessions() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let session_query_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should expose local addr");

            let server_session_query_count = session_query_count.clone();
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let server_session_query_count = server_session_query_count.clone();
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);

                        if request.contains("GET /session/status") {
                            let body = session_status_json("ses-root", "idle");
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /session") {
                            server_session_query_count
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let body = session_json("ses-root", "Root session");
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
            let (event_tx, _) = broadcast::channel(64);
            let base_uri = format!("http://{addr}");
            let client = Arc::new(opencode::client::Client::new(&base_uri));
            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    &format!("{base_uri}/"),
                    "run-u-root-ignore-non-session.service",
                ));
                snapshot.opencode_client = Some(OpencodeClientSnapshot {
                    client: client.clone(),
                    events: event_tx.clone(),
                });
                true
            });

            let sync_task = tokio::spawn(sync_root_session_from_events(
                workspace.clone(),
                client,
                event_tx.clone(),
                normalize_base_uri(&base_uri),
            ));

            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    if workspace_rx.borrow().root_session_id.as_deref() == Some("ses-root") {
                        break;
                    }
                }
            })
            .await
            .expect("initial root session query should populate metadata");

            let query_count_before = session_query_count.load(std::sync::atomic::Ordering::SeqCst);
            event_tx
                .send(global_disposed_event())
                .expect("sending non-session event should succeed");
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;

            let query_count_after = session_query_count.load(std::sync::atomic::Ordering::SeqCst);
            assert_eq!(
                query_count_after, query_count_before,
                "non-session events should not trigger session list requery"
            );

            sync_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn changing_transient_uri_clears_stale_root_session_id_before_resync() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let workspace = Workspace::new(WorkspaceSnapshot::default());
            let mut workspace_rx = workspace.subscribe();

            let (old_event_tx, _) = broadcast::channel(64);
            let old_client = Arc::new(opencode::client::Client::new("http://127.0.0.1:9"));
            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    "http://127.0.0.1:9/",
                    "run-u-root-old-uri.service",
                ));
                snapshot.opencode_client = Some(OpencodeClientSnapshot {
                    client: old_client,
                    events: old_event_tx,
                });
                snapshot.root_session_id = Some("ses-root-stale".to_string());
                snapshot.root_session_title = Some("Stale title".to_string());
                snapshot.root_session_status = Some(RootSessionStatus::Busy);
                true
            });

            let watcher_task = tokio::spawn(watch_workspace_snapshot(
                workspace.clone(),
                workspace.subscribe(),
            ));

            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

            let (new_event_tx, _) = broadcast::channel(64);
            let new_client = Arc::new(opencode::client::Client::new("http://127.0.0.1:10"));
            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    "http://127.0.0.1:10/",
                    "run-u-root-new-uri.service",
                ));
                snapshot.opencode_client = Some(OpencodeClientSnapshot {
                    client: new_client,
                    events: new_event_tx,
                });
                true
            });

            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.root_session_id.is_none()
                        && snapshot.root_session_title.is_none()
                        && snapshot.root_session_status.is_none()
                    {
                        break;
                    }
                }
            })
            .await
            .expect("root session metadata should clear after URI change");

            watcher_task.abort();
        });
    }
}
