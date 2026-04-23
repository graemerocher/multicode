use std::{collections::BTreeSet, sync::Arc, time::Duration};

use tokio::sync::{broadcast, watch};

use super::{
    codex_app_server::{
        CodexAppServerClient, CodexServerNotification, forward_codex_notifications_forever,
    },
    workspace_task_watch::watch_workspace_task,
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{
    WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot, manager::Workspace, opencode,
};

const INITIAL_SYNC_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const INITIAL_SYNC_RETRY_ATTEMPTS: usize = 5;
const EXCLUDED_REPO_EXAMPLE: &str = "/home/example/work/repo_path";
const EXCLUDED_ISSUE_EXAMPLE: &str = "https://github.com/example/example-core/issue/12345";
const EXCLUDED_PR_EXAMPLE: &str = "https://github.com/example/example-core/pull/12345";

#[derive(Debug)]
pub enum MulticodeMetadataServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for MulticodeMetadataServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MulticodeMetadata {
    repositories: BTreeSet<String>,
    issues: BTreeSet<String>,
    prs: BTreeSet<String>,
}

#[derive(Clone)]
enum MetadataTaskKey {
    Opencode {
        session_id: String,
        client: Arc<opencode::client::Client>,
        event_tx: broadcast::Sender<opencode::client::types::GlobalEvent>,
        uri: String,
    },
    Codex {
        thread_id: String,
        uri: String,
    },
}

impl PartialEq for MetadataTaskKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Opencode {
                    session_id: left_session_id,
                    client: left_client,
                    uri: left_uri,
                    ..
                },
                Self::Opencode {
                    session_id: right_session_id,
                    client: right_client,
                    uri: right_uri,
                    ..
                },
            ) => {
                left_session_id == right_session_id
                    && left_uri == right_uri
                    && Arc::ptr_eq(left_client, right_client)
            }
            (
                Self::Codex {
                    thread_id: left_thread_id,
                    uri: left_uri,
                },
                Self::Codex {
                    thread_id: right_thread_id,
                    uri: right_uri,
                },
            ) => left_thread_id == right_thread_id && left_uri == right_uri,
            _ => false,
        }
    }
}

/// Watch the agent transcript for machine-readable metadata as specified by
/// /workspace-skills/machine-readable-*
pub async fn multicode_metadata_service(
    manager: Arc<WorkspaceManager>,
) -> Result<(), MulticodeMetadataServiceError> {
    monitor_workspace_snapshots(manager, |_, workspace, workspace_rx| async move {
        tokio::spawn(async move {
            watch_workspace_snapshot(workspace, workspace_rx).await;
        });
        Ok(())
    })
    .await
}

async fn watch_workspace_snapshot(
    workspace: Workspace,
    workspace_rx: watch::Receiver<WorkspaceSnapshot>,
) {
    watch_workspace_task(
        workspace,
        workspace_rx,
        |snapshot| {
            let session_or_thread_id = snapshot.root_session_id.clone()?;
            let transient_uri = snapshot.transient.as_ref()?.uri.clone();
            let parsed_uri = url::Url::parse(&transient_uri).ok()?;

            if matches!(parsed_uri.scheme(), "ws" | "wss") {
                return Some(MetadataTaskKey::Codex {
                    thread_id: session_or_thread_id,
                    uri: normalize_base_uri(&transient_uri),
                });
            }

            let opencode_client = snapshot.opencode_client.as_ref()?;
            Some(MetadataTaskKey::Opencode {
                session_id: session_or_thread_id,
                client: opencode_client.client.clone(),
                event_tx: opencode_client.events.clone(),
                uri: normalize_base_uri(&transient_uri),
            })
        },
        |_: &Workspace| {},
        |_: &Workspace, _: &MetadataTaskKey, _: Option<MetadataTaskKey>| {},
        |workspace: &Workspace, key: &MetadataTaskKey| {
            let task_workspace = workspace.clone();
            let task_key = key.clone();
            tokio::spawn(async move {
                match task_key {
                    MetadataTaskKey::Opencode {
                        session_id,
                        client,
                        event_tx,
                        uri,
                    } => {
                        sync_multicode_metadata_from_history_and_events(
                            task_workspace,
                            client,
                            event_tx,
                            session_id,
                            uri,
                        )
                        .await;
                    }
                    MetadataTaskKey::Codex { thread_id, uri } => {
                        sync_codex_multicode_metadata(task_workspace, thread_id, uri).await;
                    }
                }
            })
        },
    )
    .await;
}

async fn sync_multicode_metadata_from_history_and_events(
    workspace: Workspace,
    client: Arc<opencode::client::Client>,
    event_tx: broadcast::Sender<opencode::client::types::GlobalEvent>,
    session_id: String,
    expected_uri: String,
) {
    let mut event_rx = event_tx.subscribe();
    let mut metadata = MulticodeMetadata::default();
    let mut initialized = false;

    for attempt in 0..INITIAL_SYNC_RETRY_ATTEMPTS {
        match query_multicode_metadata(client.as_ref(), &session_id).await {
            Ok(next_metadata) => {
                metadata = next_metadata;
                refresh_snapshot_multicode_metadata(
                    &workspace,
                    &client,
                    &session_id,
                    &expected_uri,
                    &metadata,
                );
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
                    && let Ok(next_metadata) =
                        query_multicode_metadata(client.as_ref(), &session_id).await
                {
                    metadata = next_metadata;
                    refresh_snapshot_multicode_metadata(
                        &workspace,
                        &client,
                        &session_id,
                        &expected_uri,
                        &metadata,
                    );
                    initialized = true;
                }
                if should_refresh_from_event(&event, &session_id)
                    && let Ok(next_metadata) =
                        query_multicode_metadata(client.as_ref(), &session_id).await
                {
                    let changed = metadata != next_metadata;
                    metadata = next_metadata;
                    if changed || !initialized {
                        refresh_snapshot_multicode_metadata(
                            &workspace,
                            &client,
                            &session_id,
                            &expected_uri,
                            &metadata,
                        );
                    }
                    initialized = true;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                if let Ok(next_metadata) =
                    query_multicode_metadata(client.as_ref(), &session_id).await
                {
                    metadata = next_metadata;
                    refresh_snapshot_multicode_metadata(
                        &workspace,
                        &client,
                        &session_id,
                        &expected_uri,
                        &metadata,
                    );
                    initialized = true;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn query_multicode_metadata(
    client: &opencode::client::Client,
    session_id: &str,
) -> Result<MulticodeMetadata, opencode::client::Error<opencode::client::types::BadRequestError>> {
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
    Ok(collect_metadata_from_messages(messages.iter()))
}

fn collect_metadata_from_messages<'a>(
    messages: impl IntoIterator<Item = &'a opencode::client::types::SessionMessagesResponseItem>,
) -> MulticodeMetadata {
    let mut metadata = MulticodeMetadata::default();
    for message in messages {
        apply_message_metadata(&message.info, &message.parts, &mut metadata);
    }
    metadata
}

fn should_refresh_from_event(
    event: &opencode::client::types::GlobalEvent,
    session_id: &str,
) -> bool {
    match &event.payload {
        opencode::client::types::GlobalEventPayload::EventMessageUpdated(message_updated) => {
            message_session_id(&message_updated.properties.info) == Some(session_id)
        }
        opencode::client::types::GlobalEventPayload::SyncEventMessageUpdated(message_updated) => {
            message_session_id(&message_updated.data.info) == Some(session_id)
        }
        opencode::client::types::GlobalEventPayload::EventMessageRemoved(message_removed) => {
            message_removed.properties.session_id.as_str() == session_id
        }
        opencode::client::types::GlobalEventPayload::SyncEventMessageRemoved(message_removed) => {
            message_removed.data.session_id.as_str() == session_id
        }
        _ => false,
    }
}

fn apply_message_metadata(
    message: &opencode::client::types::Message,
    parts: &[opencode::client::types::Part],
    metadata: &mut MulticodeMetadata,
) {
    let opencode::client::types::Message::AssistantMessage(_) = message else {
        return;
    };

    for part in parts {
        if let opencode::client::types::Part::TextPart(text_part) = part {
            merge_text_metadata(&text_part.text, metadata);
        }
    }
}

fn merge_text_metadata(text: &str, metadata: &mut MulticodeMetadata) {
    for repository in extract_tag_values(text, "repo") {
        if repository != EXCLUDED_REPO_EXAMPLE {
            metadata.repositories.insert(repository);
        }
    }
    for issue in extract_tag_values(text, "issue") {
        if issue != EXCLUDED_ISSUE_EXAMPLE {
            metadata.issues.insert(issue);
        }
    }
    for pr in extract_tag_values(text, "pr") {
        if pr != EXCLUDED_PR_EXAMPLE {
            metadata.prs.insert(pr);
        }
    }
}

fn extract_tag_values(text: &str, tag: &str) -> Vec<String> {
    let opening = format!("<multicode:{tag}>");
    let closing = format!("</multicode:{tag}>");
    let mut values = Vec::new();
    let mut search_start = 0;

    while let Some(open_index) = text[search_start..].find(&opening) {
        let content_start = search_start + open_index + opening.len();
        let Some(close_index) = text[content_start..].find(&closing) else {
            break;
        };
        let content_end = content_start + close_index;
        let value = text[content_start..content_end].trim();
        if !value.is_empty() {
            values.push(value.to_string());
        }
        search_start = content_end + closing.len();
    }

    values
}

fn message_session_id(message: &opencode::client::types::Message) -> Option<&str> {
    match message {
        opencode::client::types::Message::AssistantMessage(message) => Some(&message.session_id),
        opencode::client::types::Message::UserMessage(message) => Some(&message.session_id),
    }
}

fn refresh_snapshot_multicode_metadata(
    workspace: &Workspace,
    client: &Arc<opencode::client::Client>,
    session_id: &str,
    expected_uri: &str,
    metadata: &MulticodeMetadata,
) {
    let repositories = metadata.repositories.iter().cloned().collect::<Vec<_>>();
    let issues = metadata.issues.iter().cloned().collect::<Vec<_>>();
    let prs = metadata.prs.iter().cloned().collect::<Vec<_>>();

    workspace.update(|snapshot| {
        let still_tracking_same_client = snapshot
            .opencode_client
            .as_ref()
            .map(|opencode_client| Arc::ptr_eq(&opencode_client.client, client))
            .unwrap_or(false);
        let still_tracking_same_session = snapshot.root_session_id.as_deref() == Some(session_id);
        let still_attached_to_expected_uri = snapshot
            .transient
            .as_ref()
            .map(|transient| normalize_base_uri(&transient.uri))
            .as_deref()
            == Some(expected_uri);
        let should_update = still_tracking_same_client
            && still_tracking_same_session
            && still_attached_to_expected_uri
            && (snapshot.persistent.agent_provided.repo != repositories
                || snapshot.persistent.agent_provided.issue != issues
                || snapshot.persistent.agent_provided.pr != prs);
        if should_update {
            snapshot.persistent.agent_provided.repo = repositories;
            snapshot.persistent.agent_provided.issue = issues;
            snapshot.persistent.agent_provided.pr = prs;
            true
        } else {
            false
        }
    });
}

fn normalize_base_uri(uri: &str) -> String {
    uri.trim_end_matches('/').to_string()
}

async fn sync_codex_multicode_metadata(
    workspace: Workspace,
    thread_id: String,
    expected_uri: String,
) {
    let client = CodexAppServerClient::new(expected_uri.clone());
    let event_tx = broadcast::channel(256).0;
    let forwarder = tokio::spawn(forward_codex_notifications_forever(
        client.clone(),
        event_tx.clone(),
    ));
    let mut event_rx = event_tx.subscribe();

    refresh_snapshot_codex_multicode_metadata(&workspace, &client, &thread_id, &expected_uri).await;

    loop {
        match event_rx.recv().await {
            Ok(CodexServerNotification::ThreadStarted { thread }) if thread.id == thread_id => {
                refresh_snapshot_codex_multicode_metadata(
                    &workspace,
                    &client,
                    &thread_id,
                    &expected_uri,
                )
                .await;
            }
            Ok(CodexServerNotification::TurnCompleted {
                thread_id: completed_thread_id,
            }) if completed_thread_id == thread_id => {
                refresh_snapshot_codex_multicode_metadata(
                    &workspace,
                    &client,
                    &thread_id,
                    &expected_uri,
                )
                .await;
            }
            Ok(CodexServerNotification::ThreadStatusChanged {
                thread_id: changed_thread_id,
                ..
            }) if changed_thread_id == thread_id => {
                refresh_snapshot_codex_multicode_metadata(
                    &workspace,
                    &client,
                    &thread_id,
                    &expected_uri,
                )
                .await;
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                refresh_snapshot_codex_multicode_metadata(
                    &workspace,
                    &client,
                    &thread_id,
                    &expected_uri,
                )
                .await;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    forwarder.abort();
}

async fn refresh_snapshot_codex_multicode_metadata(
    workspace: &Workspace,
    client: &CodexAppServerClient,
    thread_id: &str,
    expected_uri: &str,
) {
    let Ok(response) = client.thread_read_with_turns(thread_id, true).await else {
        return;
    };
    let metadata = collect_metadata_from_codex_turns(response.thread.turns.iter());
    let repositories = metadata.repositories.iter().cloned().collect::<Vec<_>>();
    let issues = metadata.issues.iter().cloned().collect::<Vec<_>>();
    let prs = metadata.prs.iter().cloned().collect::<Vec<_>>();

    workspace.update(|snapshot| {
        let still_tracking_same_session = snapshot.root_session_id.as_deref() == Some(thread_id);
        let still_attached_to_expected_uri = snapshot
            .transient
            .as_ref()
            .map(|transient| normalize_base_uri(&transient.uri))
            .as_deref()
            == Some(expected_uri);
        let should_update = still_tracking_same_session
            && still_attached_to_expected_uri
            && (snapshot.persistent.agent_provided.repo != repositories
                || snapshot.persistent.agent_provided.issue != issues
                || snapshot.persistent.agent_provided.pr != prs);
        if should_update {
            snapshot.persistent.agent_provided.repo = repositories;
            snapshot.persistent.agent_provided.issue = issues;
            snapshot.persistent.agent_provided.pr = prs;
            true
        } else {
            false
        }
    });
}

fn collect_metadata_from_codex_turns<'a>(
    turns: impl IntoIterator<Item = &'a super::codex_app_server::CodexThreadTurn>,
) -> MulticodeMetadata {
    let mut metadata = MulticodeMetadata::default();
    for turn in turns {
        for item in &turn.items {
            merge_json_metadata(item, &mut metadata);
        }
    }
    metadata
}

fn merge_json_metadata(item: &serde_json::Value, metadata: &mut MulticodeMetadata) {
    match item {
        serde_json::Value::String(text) => merge_text_metadata(text, metadata),
        serde_json::Value::Array(items) => {
            for value in items {
                merge_json_metadata(value, metadata);
            }
        }
        serde_json::Value::Object(entries) => {
            for value in entries.values() {
                merge_json_metadata(value, metadata);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant_message_json(message_id: &str, session_id: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "agent": "assistant",
            "cost": 0.0,
            "id": message_id,
            "mode": "default",
            "modelID": "model",
            "parentID": "msg-parent",
            "parts": [
                {
                    "id": format!("prt-{message_id}"),
                    "messageID": message_id,
                    "sessionID": session_id,
                    "text": text,
                    "type": "text"
                }
            ],
            "path": {
                "cwd": "/workspace",
                "root": "/workspace"
            },
            "providerID": "provider",
            "role": "assistant",
            "sessionID": session_id,
            "time": {
                "created": 1
            },
            "tokens": {
                "cache": {
                    "read": 0,
                    "write": 0
                },
                "input": 0,
                "output": 0,
                "reasoning": 0,
                "total": 0
            }
        })
    }

    fn user_message_json(message_id: &str, session_id: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "agent": "user",
            "id": message_id,
            "model": {
                "modelID": "model",
                "providerID": "provider"
            },
            "parts": [
                {
                    "id": format!("prt-{message_id}"),
                    "messageID": message_id,
                    "sessionID": session_id,
                    "text": text,
                    "type": "text"
                }
            ],
            "role": "user",
            "sessionID": session_id,
            "time": {
                "created": 1
            },
            "tools": {}
        })
    }

    fn text_part(message_id: &str, session_id: &str, text: &str) -> opencode::client::types::Part {
        opencode::client::types::TextPart {
            id: format!("prt-{message_id}")
                .parse()
                .expect("text part ID should parse"),
            ignored: None,
            message_id: message_id
                .parse()
                .expect("text part message ID should parse"),
            metadata: Default::default(),
            session_id: session_id
                .parse()
                .expect("text part session ID should parse"),
            synthetic: None,
            text: text.to_string(),
            time: None,
            type_: opencode::client::types::TextPartType::Text,
        }
        .into()
    }

    fn message_updated_event(
        message_id: &str,
        session_id: &str,
        text: &str,
    ) -> opencode::client::types::GlobalEvent {
        serde_json::from_value(serde_json::json!({
            "directory": "/workspace",
            "payload": {
                "type": "message.updated",
                "properties": {
                    "info": assistant_message_json(message_id, session_id, text),
                    "sessionID": session_id
                }
            }
        }))
        .expect("message.updated event should parse")
    }

    #[test]
    fn extracts_tag_values_from_text() {
        let text = concat!(
            "intro ",
            "<multicode:repo>/tmp/repo-a</multicode:repo>",
            " middle ",
            "<multicode:issue>https://github.com/acme/core/issue/10</multicode:issue>",
            " end ",
            "<multicode:pr>https://github.com/acme/core/pull/11</multicode:pr>"
        );

        assert_eq!(extract_tag_values(text, "repo"), vec!["/tmp/repo-a"]);
        assert_eq!(
            extract_tag_values(text, "issue"),
            vec!["https://github.com/acme/core/issue/10"]
        );
        assert_eq!(
            extract_tag_values(text, "pr"),
            vec!["https://github.com/acme/core/pull/11"]
        );
    }

    #[test]
    fn collects_metadata_from_history_items_and_ignores_prompt_examples() {
        let messages = vec![
            opencode::client::types::SessionMessagesResponseItem {
                info: serde_json::from_value(assistant_message_json(
                    "msg-1",
                    "ses-root",
                    concat!(
                        "ignore examples ",
                        "<multicode:repo>/home/example/work/repo_path</multicode:repo>",
                        " ",
                        "<multicode:issue>https://github.com/example/example-core/issue/12345</multicode:issue>",
                        " ",
                        "<multicode:pr>https://github.com/example/example-core/pull/12345</multicode:pr>"
                    ),
                ))
                .expect("assistant message should parse"),
                parts: vec![text_part(
                    "msg-1",
                    "ses-root",
                    concat!(
                        "ignore examples ",
                        "<multicode:repo>/home/example/work/repo_path</multicode:repo>",
                        " ",
                        "<multicode:issue>https://github.com/example/example-core/issue/12345</multicode:issue>",
                        " ",
                        "<multicode:pr>https://github.com/example/example-core/pull/12345</multicode:pr>"
                    ),
                )],
            },
            opencode::client::types::SessionMessagesResponseItem {
                info: serde_json::from_value(assistant_message_json(
                    "msg-2",
                    "ses-root",
                    concat!(
                        "real values ",
                        "<multicode:repo>/srv/work/core</multicode:repo>",
                        " ",
                        "<multicode:issue>https://github.com/acme/core/issue/42</multicode:issue>",
                        " ",
                        "<multicode:pr>https://github.com/acme/core/pull/99</multicode:pr>"
                    ),
                ))
                .expect("assistant message should parse"),
                parts: vec![text_part(
                    "msg-2",
                    "ses-root",
                    concat!(
                        "real values ",
                        "<multicode:repo>/srv/work/core</multicode:repo>",
                        " ",
                        "<multicode:issue>https://github.com/acme/core/issue/42</multicode:issue>",
                        " ",
                        "<multicode:pr>https://github.com/acme/core/pull/99</multicode:pr>"
                    ),
                )],
            },
            opencode::client::types::SessionMessagesResponseItem {
                info: serde_json::from_value(user_message_json(
                    "msg-user",
                    "ses-root",
                    "<multicode:repo>/srv/ignored-user-message</multicode:repo>",
                ))
                .expect("user message should parse"),
                parts: vec![text_part(
                    "msg-user",
                    "ses-root",
                    "<multicode:repo>/srv/ignored-user-message</multicode:repo>",
                )],
            },
        ];

        let metadata = collect_metadata_from_messages(messages.iter());
        assert_eq!(
            metadata.repositories,
            BTreeSet::from(["/srv/work/core".to_string()])
        );
        assert_eq!(
            metadata.issues,
            BTreeSet::from(["https://github.com/acme/core/issue/42".to_string()])
        );
        assert_eq!(
            metadata.prs,
            BTreeSet::from(["https://github.com/acme/core/pull/99".to_string()])
        );
    }

    #[test]
    fn message_updated_event_triggers_history_refresh_for_same_session() {
        assert!(should_refresh_from_event(
            &message_updated_event(
                "msg-2",
                "ses-root",
                "later <multicode:issue>https://github.com/acme/core/issue/100</multicode:issue>",
            ),
            "ses-root",
        ));
        assert!(!should_refresh_from_event(
            &message_updated_event(
                "msg-2",
                "ses-other",
                "later <multicode:issue>https://github.com/acme/core/issue/100</multicode:issue>",
            ),
            "ses-root",
        ));
    }

    #[test]
    fn collects_metadata_from_codex_turn_items() {
        let turns = vec![super::super::codex_app_server::CodexThreadTurn {
            items: vec![
                serde_json::json!({
                    "type": "agentMessage",
                    "text": "<multicode:repo>/srv/work/core</multicode:repo>"
                }),
                serde_json::json!({
                    "type": "agentMessage",
                    "text": "<multicode:issue>https://github.com/acme/core/issue/42</multicode:issue>"
                }),
                serde_json::json!({
                    "type": "agentMessage",
                    "text": "<multicode:pr>https://github.com/acme/core/pull/99</multicode:pr>"
                }),
                serde_json::json!({
                    "type": "userMessage",
                    "content": [{ "type": "text", "text": "ignored" }]
                }),
            ],
        }];

        let metadata = collect_metadata_from_codex_turns(turns.iter());

        assert_eq!(
            metadata.repositories,
            BTreeSet::from(["/srv/work/core".to_string()])
        );
        assert_eq!(
            metadata.issues,
            BTreeSet::from(["https://github.com/acme/core/issue/42".to_string()])
        );
        assert_eq!(
            metadata.prs,
            BTreeSet::from(["https://github.com/acme/core/pull/99".to_string()])
        );
    }

    #[test]
    fn collects_metadata_from_codex_tool_output_items() {
        let turns = vec![super::super::codex_app_server::CodexThreadTurn {
            items: vec![serde_json::json!({
                "type": "toolCallOutput",
                "output": "stdout\n<multicode:pr>https://github.com/acme/core/pull/101</multicode:pr>\n"
            })],
        }];

        let metadata = collect_metadata_from_codex_turns(turns.iter());

        assert_eq!(
            metadata.prs,
            BTreeSet::from(["https://github.com/acme/core/pull/101".to_string()])
        );
    }
}
