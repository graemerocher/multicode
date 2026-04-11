use std::{path::PathBuf, sync::Arc};

use tokio::sync::broadcast;

use super::{
    codex_app_server::{
        CodexAppServerClient, CodexServerNotification, CodexThread,
        forward_codex_notifications_forever,
    },
    config::CodexAgentConfig,
    workspace_task_watch::watch_workspace_task,
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{RootSessionStatus, WorkspaceManager, WorkspaceManagerError, manager::Workspace};

#[derive(Debug)]
pub enum CodexRootSessionServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for CodexRootSessionServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct RootSessionTaskKey {
    uri: String,
    cwd: String,
}

pub async fn codex_root_session_service(
    manager: Arc<WorkspaceManager>,
    workspace_directory_path: PathBuf,
    config: CodexAgentConfig,
) -> Result<(), CodexRootSessionServiceError> {
    monitor_workspace_snapshots(manager, move |key, workspace, workspace_rx| {
        let workspace_directory_path = workspace_directory_path.clone();
        let config = config.clone();
        async move {
            let cwd = workspace_directory_path
                .join(key)
                .to_string_lossy()
                .into_owned();
            tokio::spawn(async move {
                watch_workspace_snapshot(workspace, workspace_rx, cwd, config).await;
            });
            Ok(())
        }
    })
    .await
}

async fn watch_workspace_snapshot(
    workspace: Workspace,
    workspace_rx: tokio::sync::watch::Receiver<crate::WorkspaceSnapshot>,
    cwd: String,
    config: CodexAgentConfig,
) {
    watch_workspace_task(
        workspace,
        workspace_rx,
        |snapshot| {
            let transient = snapshot.transient.as_ref()?;
            let parsed = url::Url::parse(&transient.uri).ok()?;
            if !matches!(parsed.scheme(), "ws" | "wss") {
                return None;
            }
            Some(RootSessionTaskKey {
                uri: transient.uri.clone(),
                cwd: cwd.clone(),
            })
        },
        clear_root_session_if_detached,
        clear_root_session_on_uri_change,
        move |workspace, key| {
            let task_workspace = workspace.clone();
            let task_key = key.clone();
            let task_config = config.clone();
            tokio::spawn(async move {
                sync_root_session(task_workspace, task_key, task_config).await;
            })
        },
    )
    .await;
}

fn clear_root_session_if_detached(workspace: &Workspace) {
    workspace.update(|snapshot| {
        let should_clear = snapshot.transient.is_none()
            && (snapshot.root_session_id.is_some()
                || snapshot.root_session_title.is_some()
                || snapshot.root_session_status.is_some());
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
    previous_key: Option<RootSessionTaskKey>,
) {
    if previous_key
        .as_ref()
        .is_none_or(|previous_key| previous_key.uri == next_key.uri)
    {
        return;
    }

    workspace.update(|snapshot| {
        if snapshot
            .transient
            .as_ref()
            .map(|transient| transient.uri.as_str())
            != Some(next_key.uri.as_str())
        {
            return false;
        }
        let changed = snapshot.root_session_id.is_some()
            || snapshot.root_session_title.is_some()
            || snapshot.root_session_status.is_some();
        snapshot.root_session_id = None;
        snapshot.root_session_title = None;
        snapshot.root_session_status = None;
        changed
    });
}

async fn sync_root_session(
    workspace: Workspace,
    key: RootSessionTaskKey,
    config: CodexAgentConfig,
) {
    let client = CodexAppServerClient::new(key.uri.clone());
    let event_tx = broadcast::channel(256).0;
    let forwarder = tokio::spawn(forward_codex_notifications_forever(
        client.clone(),
        event_tx.clone(),
    ));
    let mut event_rx = event_tx.subscribe();
    let mut refresh_interval = tokio::time::interval(std::time::Duration::from_secs(5));
    refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut active_turn_thread_id: Option<String> = None;

    refresh_root_session(
        &workspace,
        &client,
        &key,
        &config,
        active_turn_thread_id.as_deref(),
    )
    .await;

    loop {
        tokio::select! {
            _ = refresh_interval.tick() => {
                refresh_root_session(&workspace, &client, &key, &config, active_turn_thread_id.as_deref()).await;
            }
            event = event_rx.recv() => {
                match event {
                    Ok(CodexServerNotification::ThreadStarted { thread }) => {
                        update_from_started_thread(
                            &workspace,
                            &key,
                            &thread,
                            active_turn_thread_id.as_deref(),
                        );
                    }
                    Ok(CodexServerNotification::ThreadStatusChanged { thread_id, status }) => {
                        workspace.update(|snapshot| {
                            if snapshot
                                .transient
                                .as_ref()
                                .map(|transient| transient.uri.as_str())
                                != Some(key.uri.as_str())
                            {
                                return false;
                            }
                            if snapshot.root_session_id.as_deref() != Some(thread_id.as_str()) {
                                return false;
                            }
                            let next_status =
                                effective_status(&thread_id, &status, active_turn_thread_id.as_deref());
                            if snapshot.root_session_status == Some(next_status) {
                                false
                            } else {
                                snapshot.root_session_status = Some(next_status);
                                true
                            }
                        });
                    }
                    Ok(CodexServerNotification::TurnStarted { thread_id }) => {
                        active_turn_thread_id = Some(thread_id.clone());
                        workspace.update(|snapshot| {
                            if snapshot.root_session_id.as_deref() == Some(thread_id.as_str())
                                && snapshot.root_session_status != Some(RootSessionStatus::Busy)
                            {
                                snapshot.root_session_status = Some(RootSessionStatus::Busy);
                                true
                            } else {
                                false
                            }
                        });
                    }
                    Ok(CodexServerNotification::TurnCompleted { thread_id }) => {
                        if active_turn_thread_id.as_deref() == Some(thread_id.as_str()) {
                            active_turn_thread_id = None;
                        }
                        if workspace.subscribe().borrow().root_session_id.as_deref()
                            == Some(thread_id.as_str())
                        {
                            refresh_root_session(
                                &workspace,
                                &client,
                                &key,
                                &config,
                                active_turn_thread_id.as_deref(),
                            )
                            .await;
                        }
                    }
                    Ok(CodexServerNotification::Other) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        refresh_root_session(
                            &workspace,
                            &client,
                            &key,
                            &config,
                            active_turn_thread_id.as_deref(),
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    forwarder.abort();
}

async fn refresh_root_session(
    workspace: &Workspace,
    client: &CodexAppServerClient,
    key: &RootSessionTaskKey,
    config: &CodexAgentConfig,
    active_turn_thread_id: Option<&str>,
) {
    let response = match client.thread_list(&key.cwd).await {
        Ok(response) => response,
        Err(_) => return,
    };

    let current_root_session_id = workspace.subscribe().borrow().root_session_id.clone();
    let thread =
        match select_thread_for_tracking(current_root_session_id.as_deref(), &response.data) {
            Some(thread) => thread,
            None => match client.thread_start(&key.cwd, config).await {
                Ok(response) => response.thread,
                Err(_) => return,
            },
        };

    update_from_thread(workspace, key, &thread, active_turn_thread_id);
}

fn select_thread_for_tracking(
    current_thread_id: Option<&str>,
    threads: &[CodexThread],
) -> Option<CodexThread> {
    if let Some(current_thread_id) = current_thread_id
        && let Some(thread) = threads
            .iter()
            .find(|thread| thread.id == current_thread_id && !thread.status.requires_replacement())
    {
        return Some(thread.clone());
    }

    threads
        .iter()
        .find(|thread| !thread.status.requires_replacement())
        .cloned()
}

fn update_from_started_thread(
    workspace: &Workspace,
    key: &RootSessionTaskKey,
    thread: &CodexThread,
    active_turn_thread_id: Option<&str>,
) {
    workspace.update(|snapshot| {
        if snapshot
            .transient
            .as_ref()
            .map(|transient| transient.uri.as_str())
            != Some(key.uri.as_str())
        {
            return false;
        }
        if snapshot.root_session_id.is_some()
            && snapshot.root_session_id.as_deref() != Some(thread.id.as_str())
        {
            return false;
        }

        let next_title = thread.title.clone().unwrap_or_else(|| "Codex".to_string());
        let next_status = effective_status(&thread.id, &thread.status, active_turn_thread_id);
        if snapshot.root_session_id.as_deref() == Some(thread.id.as_str())
            && snapshot.root_session_title.as_deref() == Some(next_title.as_str())
            && snapshot.root_session_status == Some(next_status)
        {
            return false;
        }

        snapshot.root_session_id = Some(thread.id.clone());
        snapshot.root_session_title = Some(next_title);
        snapshot.root_session_status = Some(next_status);
        true
    });
}

fn update_from_thread(
    workspace: &Workspace,
    key: &RootSessionTaskKey,
    thread: &CodexThread,
    active_turn_thread_id: Option<&str>,
) {
    workspace.update(|snapshot| {
        if snapshot
            .transient
            .as_ref()
            .map(|transient| transient.uri.as_str())
            != Some(key.uri.as_str())
        {
            return false;
        }

        let next_title = thread.title.clone().unwrap_or_else(|| "Codex".to_string());
        let next_status = effective_status(&thread.id, &thread.status, active_turn_thread_id);
        if snapshot.root_session_id.as_deref() == Some(thread.id.as_str())
            && snapshot.root_session_title.as_deref() == Some(next_title.as_str())
            && snapshot.root_session_status == Some(next_status)
        {
            return false;
        }

        snapshot.root_session_id = Some(thread.id.clone());
        snapshot.root_session_title = Some(next_title);
        snapshot.root_session_status = Some(next_status);
        true
    });
}

fn effective_status(
    thread_id: &str,
    status: &super::codex_app_server::CodexThreadStatus,
    active_turn_thread_id: Option<&str>,
) -> RootSessionStatus {
    match map_status(status) {
        RootSessionStatus::Idle if active_turn_thread_id == Some(thread_id) => {
            RootSessionStatus::Busy
        }
        other => other,
    }
}

fn map_status(status: &super::codex_app_server::CodexThreadStatus) -> RootSessionStatus {
    if status.waits_for_human_input() {
        RootSessionStatus::Question
    } else if status.is_idle() {
        RootSessionStatus::Idle
    } else {
        RootSessionStatus::Busy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_app_server::{CodexThreadActiveFlag, CodexThreadStatus};
    use crate::{
        RuntimeBackend, RuntimeHandleSnapshot, TransientWorkspaceSnapshot, WorkspaceSnapshot,
    };

    #[test]
    fn map_status_treats_waiting_on_approval_as_question() {
        let status = CodexThreadStatus::Active {
            active_flags: vec![CodexThreadActiveFlag::WaitingOnApproval],
        };

        assert_eq!(map_status(&status), RootSessionStatus::Question);
    }

    #[test]
    fn effective_status_keeps_idle_thread_busy_while_turn_is_active() {
        assert_eq!(
            effective_status("thread-1", &CodexThreadStatus::Idle, Some("thread-1")),
            RootSessionStatus::Busy
        );
        assert_eq!(
            effective_status("thread-2", &CodexThreadStatus::Idle, Some("thread-1")),
            RootSessionStatus::Idle
        );
    }

    #[test]
    fn select_thread_for_tracking_prefers_current_thread_over_newer_idle_thread() {
        let current_busy = CodexThread {
            id: "thread-busy".to_string(),
            title: Some("Busy".to_string()),
            status: CodexThreadStatus::Active {
                active_flags: Vec::new(),
            },
        };
        let newer_idle = CodexThread {
            id: "thread-idle".to_string(),
            title: Some("Idle".to_string()),
            status: CodexThreadStatus::Idle,
        };

        let selected = select_thread_for_tracking(
            Some("thread-busy"),
            &[newer_idle.clone(), current_busy.clone()],
        )
        .expect("current thread should be selected");

        assert_eq!(selected.id, current_busy.id);
    }

    #[test]
    fn update_from_started_thread_ignores_unrelated_new_thread_when_already_tracking_one() {
        let workspace = WorkspaceSnapshot::default();
        let workspace = crate::manager::Workspace::new(workspace);
        workspace.update(|snapshot| {
            snapshot.transient = Some(TransientWorkspaceSnapshot {
                uri: "ws://127.0.0.1:31337".to_string(),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::AppleContainer,
                    id: "runtime-1".to_string(),
                    metadata: Default::default(),
                },
            });
            snapshot.root_session_id = Some("thread-current".to_string());
            snapshot.root_session_title = Some("Current".to_string());
            snapshot.root_session_status = Some(RootSessionStatus::Busy);
            true
        });

        update_from_started_thread(
            &workspace,
            &RootSessionTaskKey {
                uri: "ws://127.0.0.1:31337".to_string(),
                cwd: "/tmp/workspace".to_string(),
            },
            &CodexThread {
                id: "thread-other".to_string(),
                title: Some("Other".to_string()),
                status: CodexThreadStatus::Idle,
            },
            None,
        );

        let snapshot = workspace.subscribe().borrow().clone();
        assert_eq!(snapshot.root_session_id.as_deref(), Some("thread-current"));
        assert_eq!(snapshot.root_session_title.as_deref(), Some("Current"));
        assert_eq!(snapshot.root_session_status, Some(RootSessionStatus::Busy));
    }
}
