use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::time::{MissedTickBehavior, interval};

use super::{
    root_session_service::RootSessionStatus, runtime::automation_state_file_source,
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{
    AutomationAgentState, WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot,
    manager::Workspace,
};

const STATE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum AutomationStateFileServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for AutomationStateFileServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

pub async fn automation_state_file_service(
    manager: Arc<WorkspaceManager>,
    workspace_directory_path: PathBuf,
) -> Result<(), AutomationStateFileServiceError> {
    monitor_workspace_snapshots(manager, move |key, workspace, workspace_rx| {
        let workspace_directory_path = workspace_directory_path.clone();
        async move {
            tokio::spawn(async move {
                watch_workspace(
                    workspace,
                    workspace_rx,
                    automation_state_file_source(&workspace_directory_path, &key),
                )
                .await;
            });
            Ok(())
        }
    })
    .await
}

async fn watch_workspace(
    workspace: Workspace,
    mut workspace_rx: tokio::sync::watch::Receiver<WorkspaceSnapshot>,
    state_file: PathBuf,
) {
    let mut refresh = interval(STATE_REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        let snapshot = workspace_rx.borrow().clone();
        let should_track = snapshot.transient.is_some()
            && !snapshot.persistent.archived
            && !snapshot.persistent.automation_paused
            && snapshot.persistent.automation_issue.is_some();

        if should_track {
            apply_state_file_snapshot(&workspace, read_state_file(&state_file).await);
        } else {
            clear_automation_state(&workspace);
        }

        tokio::select! {
            changed = workspace_rx.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            _ = refresh.tick() => {}
        }
    }
}

fn apply_state_file_snapshot(workspace: &Workspace, next: Option<ParsedAutomationState>) {
    workspace.update(|snapshot| {
        let next_session_id = next.as_ref().and_then(|state| state.thread_id.clone());
        let next_agent_state = next.as_ref().map(|state| state.state);
        let next_session_status = next.as_ref().map(|state| state.state.root_status());

        let mut changed = false;
        if snapshot.automation_session_id != next_session_id {
            snapshot.automation_session_id = next_session_id;
            changed = true;
        }
        if snapshot.automation_agent_state != next_agent_state {
            snapshot.automation_agent_state = next_agent_state;
            changed = true;
        }
        if snapshot.automation_session_status != next_session_status {
            snapshot.automation_session_status = next_session_status;
            changed = true;
        }
        changed
    });
}

fn clear_automation_state(workspace: &Workspace) {
    workspace.update(|snapshot| {
        let mut changed = false;
        if snapshot.automation_session_id.take().is_some() {
            changed = true;
        }
        if snapshot.automation_agent_state.take().is_some() {
            changed = true;
        }
        if snapshot.automation_session_status.take().is_some() {
            changed = true;
        }
        changed
    });
}

async fn read_state_file(path: &Path) -> Option<ParsedAutomationState> {
    tokio::fs::metadata(path).await.ok()?;
    let contents = tokio::fs::read_to_string(path).await.ok()?;
    parse_state_file(&contents)
}

fn parse_state_file(contents: &str) -> Option<ParsedAutomationState> {
    let trimmed = contents.trim();
    let (state, thread_id) =
        trimmed
            .split_once(':')
            .map_or((trimmed, None), |(state, thread_id)| {
                let thread_id = thread_id.trim();
                (
                    state.trim(),
                    (!thread_id.is_empty()).then(|| thread_id.to_string()),
                )
            });

    let state = if state.eq_ignore_ascii_case("working") {
        AutomationAgentState::Working
    } else if state.eq_ignore_ascii_case("question") {
        AutomationAgentState::Question
    } else if state.eq_ignore_ascii_case("review") {
        AutomationAgentState::Review
    } else if state.eq_ignore_ascii_case("idle") {
        AutomationAgentState::Idle
    } else {
        AutomationAgentState::Stale
    };

    Some(ParsedAutomationState { state, thread_id })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedAutomationState {
    state: AutomationAgentState,
    thread_id: Option<String>,
}

impl AutomationAgentState {
    fn root_status(self) -> RootSessionStatus {
        match self {
            AutomationAgentState::Working => RootSessionStatus::Busy,
            AutomationAgentState::Question => RootSessionStatus::Question,
            AutomationAgentState::Review
            | AutomationAgentState::Idle
            | AutomationAgentState::Stale => RootSessionStatus::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_file_maps_known_states() {
        let parsed = parse_state_file("question:thread-123\n").expect("state exists");

        assert_eq!(parsed.state, AutomationAgentState::Question);
        assert_eq!(parsed.thread_id.as_deref(), Some("thread-123"));
    }

    #[test]
    fn parse_state_file_marks_unknown_state_as_stale() {
        let parsed = parse_state_file("bogus\n").expect("state exists");

        assert_eq!(parsed.state, AutomationAgentState::Stale);
    }
}
