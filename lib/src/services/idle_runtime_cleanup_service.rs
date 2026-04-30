use std::{path::PathBuf, process::Stdio, time::Duration};

use tokio::{process::Command, sync::watch, time::Instant};

use super::{
    combined::CombinedService, root_session_service::RootSessionStatus, runtime::container_program,
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{AutomationAgentState, RuntimeBackend, WorkspaceManagerError, WorkspaceSnapshot};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const IDLE_GRADLE_CLEANUP_COMMAND: &str = "if command -v gradle >/dev/null 2>&1; then gradle --stop >/dev/null 2>&1 || true; fi; if command -v pkill >/dev/null 2>&1; then pkill -TERM -f 'GradleDaemon|Gradle Worker Daemon' >/dev/null 2>&1 || true; sleep 1; pkill -KILL -f 'GradleDaemon|Gradle Worker Daemon' >/dev/null 2>&1 || true; fi";

#[derive(Debug)]
pub enum IdleRuntimeCleanupServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for IdleRuntimeCleanupServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

pub async fn idle_runtime_cleanup_service(
    service: CombinedService,
) -> Result<(), IdleRuntimeCleanupServiceError> {
    if !service.config.autonomous.idle_runtime_cleanup {
        return Ok(());
    }

    monitor_workspace_snapshots(service.manager.clone(), |key, _, workspace_rx| {
        let service = service.clone();
        async move {
            tokio::spawn(async move {
                watch_workspace_snapshot(service, key, workspace_rx).await;
            });
            Ok(())
        }
    })
    .await
}

async fn watch_workspace_snapshot(
    service: CombinedService,
    key: String,
    mut workspace_rx: watch::Receiver<WorkspaceSnapshot>,
) {
    let idle_delay =
        Duration::from_secs(service.config.autonomous.idle_runtime_cleanup_delay_seconds);
    let cleanup_interval = Duration::from_secs(
        service
            .config
            .autonomous
            .idle_runtime_cleanup_interval_seconds
            .max(1),
    );
    let fallback_poll = DEFAULT_POLL_INTERVAL.min(cleanup_interval.max(Duration::from_secs(1)));
    let mut tracked_runtime_id: Option<String> = None;
    let mut next_cleanup_at: Option<Instant> = None;

    loop {
        let snapshot = workspace_rx.borrow_and_update().clone();
        let runtime_id = snapshot
            .transient
            .as_ref()
            .map(|transient| transient.runtime.id.clone());
        if runtime_id != tracked_runtime_id {
            tracked_runtime_id = runtime_id;
            next_cleanup_at = None;
        }

        let now = Instant::now();
        let idle_for_cleanup = workspace_is_idle_for_gradle_cleanup(&snapshot);
        if idle_for_cleanup {
            let scheduled_at = next_cleanup_at.get_or_insert(now + idle_delay);
            if now >= *scheduled_at {
                if let Some(transient) = snapshot.transient.as_ref() {
                    match run_idle_gradle_cleanup_command(
                        service.workspace_directory_path().to_path_buf(),
                        &key,
                        &transient.runtime.id,
                    )
                    .await
                    {
                        Ok(output) if output.status.success() => {
                            tracing::info!(
                                workspace_key = %key,
                                runtime_id = %transient.runtime.id,
                                "cleaned idle Gradle daemons in workspace runtime"
                            );
                        }
                        Ok(output) => {
                            tracing::warn!(
                                workspace_key = %key,
                                runtime_id = %transient.runtime.id,
                                status = ?output.status.code(),
                                stderr = %String::from_utf8_lossy(&output.stderr).trim(),
                                "idle Gradle cleanup command failed"
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                workspace_key = %key,
                                runtime_id = %transient.runtime.id,
                                error = %err,
                                "failed to run idle Gradle cleanup command"
                            );
                        }
                    }

                    if service.config.autonomous.idle_runtime_restart
                        && let Ok(workspace) = service.manager.get_workspace(&key)
                    {
                        let latest_snapshot = workspace.subscribe().borrow().clone();
                        if workspace_is_idle_for_gradle_cleanup(&latest_snapshot) {
                            match service
                                .restart_workspace_runtime_preserving_state(&key)
                                .await
                            {
                                Ok(()) => {
                                    tracing::info!(
                                        workspace_key = %key,
                                        runtime_id = %transient.runtime.id,
                                        "restarted idle Apple workspace runtime after Gradle cleanup"
                                    );
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        workspace_key = %key,
                                        runtime_id = %transient.runtime.id,
                                        error = ?err,
                                        "failed to restart idle Apple workspace runtime after Gradle cleanup"
                                    );
                                }
                            }
                        }
                    }
                }
                next_cleanup_at = Some(now + cleanup_interval);
            }
        } else {
            next_cleanup_at = None;
        }

        let wait_timeout = next_poll_timeout(Instant::now(), next_cleanup_at, fallback_poll);
        if !wait_for_change_or_timeout(&mut workspace_rx, wait_timeout).await {
            break;
        }
    }
}

fn workspace_is_idle_for_gradle_cleanup(snapshot: &WorkspaceSnapshot) -> bool {
    let Some(transient) = snapshot.transient.as_ref() else {
        return false;
    };
    if transient.runtime.backend != RuntimeBackend::AppleContainer {
        return false;
    }
    if snapshot.root_session_status == Some(RootSessionStatus::Busy)
        || snapshot.automation_session_status == Some(RootSessionStatus::Busy)
    {
        return false;
    }
    if snapshot.task_states.values().any(task_is_busy_for_cleanup) {
        return false;
    }

    snapshot.persistent.automation_paused
        || snapshot.root_session_status == Some(RootSessionStatus::Idle)
        || snapshot.automation_session_status == Some(RootSessionStatus::Idle)
        || snapshot
            .task_states
            .values()
            .any(task_is_idle_signal_for_cleanup)
}

fn task_is_busy_for_cleanup(task_state: &crate::WorkspaceTaskRuntimeSnapshot) -> bool {
    task_state.session_status == Some(RootSessionStatus::Busy)
        || task_state.agent_state == Some(AutomationAgentState::Working)
}

fn task_is_idle_signal_for_cleanup(task_state: &crate::WorkspaceTaskRuntimeSnapshot) -> bool {
    matches!(
        task_state.session_status,
        Some(RootSessionStatus::Idle | RootSessionStatus::Question)
    ) || matches!(
        task_state.agent_state,
        Some(
            AutomationAgentState::Idle
                | AutomationAgentState::Review
                | AutomationAgentState::Question
                | AutomationAgentState::Stale
        )
    )
}

async fn run_idle_gradle_cleanup_command(
    workspace_directory_path: PathBuf,
    key: &str,
    runtime_id: &str,
) -> Result<std::process::Output, std::io::Error> {
    let workspace_path = workspace_directory_path.join(key);
    let workspace_path = workspace_path.to_string_lossy().into_owned();
    Command::new(container_program())
        .args([
            "exec",
            "--workdir",
            workspace_path.as_str(),
            runtime_id,
            "/bin/sh",
            "-lc",
            IDLE_GRADLE_CLEANUP_COMMAND,
        ])
        .stdin(Stdio::null())
        .output()
        .await
}

fn next_poll_timeout(
    now: Instant,
    next_cleanup_at: Option<Instant>,
    fallback_poll: Duration,
) -> Duration {
    match next_cleanup_at {
        Some(next_cleanup_at) => next_cleanup_at
            .saturating_duration_since(now)
            .min(fallback_poll),
        None => fallback_poll,
    }
}

async fn wait_for_change_or_timeout(
    workspace_rx: &mut watch::Receiver<WorkspaceSnapshot>,
    timeout: Duration,
) -> bool {
    match tokio::time::timeout(timeout, workspace_rx.changed()).await {
        Ok(changed) => changed.is_ok(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RuntimeHandleSnapshot, TransientWorkspaceSnapshot, WorkspaceTaskRuntimeSnapshot,
        services::root_session_service::RootSessionStatus,
    };

    fn apple_snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            transient: Some(TransientWorkspaceSnapshot {
                uri: "ws://127.0.0.1:1234".to_string(),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::AppleContainer,
                    id: "mc-alpha".to_string(),
                    metadata: Default::default(),
                },
            }),
            ..Default::default()
        }
    }

    #[test]
    fn cleanup_runs_for_idle_root_session() {
        let mut snapshot = apple_snapshot();
        snapshot.root_session_status = Some(RootSessionStatus::Idle);

        assert!(workspace_is_idle_for_gradle_cleanup(&snapshot));
    }

    #[test]
    fn cleanup_skips_busy_workspace() {
        let mut snapshot = apple_snapshot();
        snapshot.root_session_status = Some(RootSessionStatus::Busy);

        assert!(!workspace_is_idle_for_gradle_cleanup(&snapshot));
    }

    #[test]
    fn cleanup_runs_for_paused_workspace_without_live_session_status() {
        let mut snapshot = apple_snapshot();
        snapshot.persistent.automation_paused = true;

        assert!(workspace_is_idle_for_gradle_cleanup(&snapshot));
    }

    #[test]
    fn cleanup_skips_busy_task_even_when_root_is_idle() {
        let mut snapshot = apple_snapshot();
        snapshot.root_session_status = Some(RootSessionStatus::Idle);
        snapshot.task_states.insert(
            "task-1".to_string(),
            WorkspaceTaskRuntimeSnapshot {
                session_status: Some(RootSessionStatus::Busy),
                ..Default::default()
            },
        );

        assert!(!workspace_is_idle_for_gradle_cleanup(&snapshot));
    }

    #[test]
    fn cleanup_skips_non_apple_runtime() {
        let mut snapshot = apple_snapshot();
        snapshot.root_session_status = Some(RootSessionStatus::Idle);
        snapshot
            .transient
            .as_mut()
            .expect("snapshot should be attached")
            .runtime
            .backend = RuntimeBackend::LinuxSystemdBwrap;

        assert!(!workspace_is_idle_for_gradle_cleanup(&snapshot));
    }
}
