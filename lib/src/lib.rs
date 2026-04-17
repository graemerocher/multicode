pub mod database;
pub mod logging;
pub mod manager;
pub mod opencode;
pub mod remote_action;
pub mod schema;
pub mod services;
#[cfg(test)]
mod test_support;
pub mod tree_scan;

pub use manager::{WorkspaceManager, WorkspaceManagerError};
pub use remote_action::{
    HandlerArgumentMode, RemoteAction, RemoteActionRequest, build_handler_command,
    decode_remote_action_request, encode_remote_action_request,
};
pub use services::root_session_service::RootSessionStatus;
pub use services::workspace_archive::WorkspaceArchiveFormat;

use std::{collections::BTreeMap, fmt, sync::Arc, time::SystemTime};

use serde::{Deserialize, Serialize};

/// Workspace metadata provided in machine-readable form by the agent. See
/// /workspace-skills/machine-readable-*
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProvidedPersistentSnapshot {
    #[serde(default)]
    pub repo: Vec<String>,
    #[serde(default)]
    pub issue: Vec<String>,
    #[serde(default)]
    pub pr: Vec<String>,
}

impl Default for AgentProvidedPersistentSnapshot {
    fn default() -> AgentProvidedPersistentSnapshot {
        AgentProvidedPersistentSnapshot {
            repo: Vec::new(),
            issue: Vec::new(),
            pr: Vec::new(),
        }
    }
}

/// Manually entered workspace metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomLinksPersistentSnapshot {
    #[serde(default)]
    pub issue: Vec<String>,
    #[serde(default)]
    pub pr: Vec<String>,
}

impl Default for CustomLinksPersistentSnapshot {
    fn default() -> CustomLinksPersistentSnapshot {
        CustomLinksPersistentSnapshot {
            issue: Vec::new(),
            pr: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceTaskSource {
    #[default]
    Manual,
    Scan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceIssueType {
    Bug,
    Docs,
    Enhancement,
    Improvement,
    Regression,
    DependencyUpgrade,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceTaskPersistentSnapshot {
    pub id: String,
    pub issue_url: String,
    #[serde(default)]
    pub backing_pr_url: Option<String>,
    #[serde(default)]
    pub dependency_upgrade_backing_pr: bool,
    #[serde(default)]
    pub issue_type: Option<WorkspaceIssueType>,
    #[serde(default)]
    pub source: WorkspaceTaskSource,
    #[serde(default)]
    pub created_at: Option<SystemTime>,
}

impl WorkspaceTaskPersistentSnapshot {
    pub fn new(id: String, issue_url: String, source: WorkspaceTaskSource) -> Self {
        Self {
            id,
            issue_url,
            backing_pr_url: None,
            dependency_upgrade_backing_pr: false,
            issue_type: None,
            source,
            created_at: Some(SystemTime::now()),
        }
    }

    pub fn with_backing_pr_url(mut self, backing_pr_url: Option<String>) -> Self {
        self.backing_pr_url = backing_pr_url;
        self
    }

    pub fn with_dependency_upgrade_backing_pr(
        mut self,
        dependency_upgrade_backing_pr: bool,
    ) -> Self {
        self.dependency_upgrade_backing_pr = dependency_upgrade_backing_pr;
        self
    }

    pub fn with_issue_type(mut self, issue_type: Option<WorkspaceIssueType>) -> Self {
        self.issue_type = issue_type;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceTaskRuntimeSnapshot {
    pub session_id: Option<String>,
    pub session_status: Option<services::root_session_service::RootSessionStatus>,
    pub agent_state: Option<AutomationAgentState>,
    pub status: Option<String>,
    pub resume_prompt: Option<String>,
    pub usage_total_tokens: Option<u64>,
    pub waiting_on_vm: bool,
    pub repository: Vec<String>,
    pub issue: Vec<String>,
    pub pr: Vec<String>,
    pub last_error: Option<String>,
}

/// Workspace metadata that is saved in persistent storage, i.e. survives a host reboot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistentWorkspaceSnapshot {
    pub archived: bool,
    pub description: String,
    pub created_at: Option<SystemTime>,
    #[serde(default)]
    pub assigned_repository: Option<String>,
    #[serde(default)]
    pub automation_issue: Option<String>,
    #[serde(default)]
    pub automation_paused: bool,
    #[serde(default)]
    pub archive_format: Option<WorkspaceArchiveFormat>,
    #[serde(default)]
    pub agent_provided: AgentProvidedPersistentSnapshot,
    #[serde(default)]
    pub custom_links: CustomLinksPersistentSnapshot,
    #[serde(default)]
    pub ignored_issue_urls: Vec<String>,
    #[serde(default)]
    pub tasks: Vec<WorkspaceTaskPersistentSnapshot>,
}

impl Default for PersistentWorkspaceSnapshot {
    fn default() -> PersistentWorkspaceSnapshot {
        PersistentWorkspaceSnapshot {
            archived: false,
            description: String::new(),
            created_at: None,
            assigned_repository: None,
            automation_issue: None,
            automation_paused: false,
            archive_format: None,
            agent_provided: AgentProvidedPersistentSnapshot::default(),
            custom_links: CustomLinksPersistentSnapshot::default(),
            ignored_issue_urls: Vec::new(),
            tasks: Vec::new(),
        }
    }
}

/// Workspace metadata that is saved in transient storage (`/run`) and does not survive a reboot.
/// This is useful for process metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeBackend {
    #[default]
    LinuxSystemdBwrap,
    AppleContainer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeHandleSnapshot {
    #[serde(default)]
    pub backend: RuntimeBackend,
    #[serde(default, alias = "unit")]
    pub id: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

impl Default for RuntimeHandleSnapshot {
    fn default() -> Self {
        Self {
            backend: RuntimeBackend::default(),
            id: String::new(),
            metadata: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransientWorkspaceSnapshot {
    pub uri: String,
    #[serde(flatten)]
    pub runtime: RuntimeHandleSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutomationAgentState {
    Working,
    WaitingOnVm,
    Question,
    Review,
    Idle,
    Stale,
}

/// Holder for the HTTP connection to the opencode server.
#[derive(Clone)]
pub struct OpencodeClientSnapshot {
    pub client: Arc<opencode::client::Client>,
    pub events: tokio::sync::broadcast::Sender<opencode::client::types::GlobalEvent>,
}

impl fmt::Debug for OpencodeClientSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpencodeClientSnapshot").finish()
    }
}

/// All metadata for a workspace. Includes transient and persistent metadata files, and in-memory
/// metadata that needs to be repopulated when multicode-tui restarts.
#[derive(Debug, Clone)]
pub struct WorkspaceSnapshot {
    pub persistent: PersistentWorkspaceSnapshot,
    pub transient: Option<TransientWorkspaceSnapshot>,
    pub opencode_client: Option<OpencodeClientSnapshot>,
    pub root_session_id: Option<String>,
    pub root_session_title: Option<String>,
    pub root_session_status: Option<RootSessionStatus>,
    pub automation_session_id: Option<String>,
    pub automation_session_status: Option<RootSessionStatus>,
    pub automation_agent_state: Option<AutomationAgentState>,
    pub automation_status: Option<String>,
    pub automation_scan_request_nonce: u64,
    pub automation_queue_next_request_nonce: u64,
    pub active_task_id: Option<String>,
    pub task_states: BTreeMap<String, WorkspaceTaskRuntimeSnapshot>,
    pub usage_total_tokens: Option<u64>,
    pub usage_total_cost: Option<f64>,
    pub usage_cpu_percent: Option<u16>,
    pub usage_ram_bytes: Option<u64>,
    pub oom_kill_count: Option<u64>,
}

impl Default for WorkspaceSnapshot {
    fn default() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            persistent: Default::default(),
            transient: None,
            opencode_client: None,
            root_session_id: None,
            root_session_title: None,
            root_session_status: None,
            automation_session_id: None,
            automation_session_status: None,
            automation_agent_state: None,
            automation_status: None,
            automation_scan_request_nonce: 0,
            automation_queue_next_request_nonce: 0,
            active_task_id: None,
            task_states: BTreeMap::new(),
            usage_total_tokens: None,
            usage_total_cost: None,
            usage_cpu_percent: None,
            usage_ram_bytes: None,
            oom_kill_count: None,
        }
    }
}

impl WorkspaceSnapshot {
    pub fn task_persistent_snapshot(
        &self,
        task_id: &str,
    ) -> Option<&WorkspaceTaskPersistentSnapshot> {
        self.persistent.tasks.iter().find(|task| task.id == task_id)
    }

    pub fn task_issue_url_for_id(&self, task_id: &str) -> Option<&str> {
        self.task_persistent_snapshot(task_id)
            .map(|task| task.issue_url.as_str())
    }

    pub fn resolved_active_task_id(&self) -> Option<String> {
        self.active_task_id
            .as_deref()
            .filter(|task_id| self.task_persistent_snapshot(task_id).is_some())
            .map(ToOwned::to_owned)
            .or_else(|| {
                self.persistent
                    .automation_issue
                    .as_deref()
                    .and_then(|issue_url| {
                        self.persistent
                            .tasks
                            .iter()
                            .find(|task| task.issue_url == issue_url)
                            .map(|task| task.id.clone())
                    })
            })
    }

    pub fn resolved_active_issue_url(&self) -> Option<String> {
        self.resolved_active_task_id()
            .as_deref()
            .and_then(|task_id| self.task_issue_url_for_id(task_id))
            .map(ToOwned::to_owned)
            .or_else(|| self.persistent.automation_issue.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_active_task_id_preserves_explicit_review_task_selection() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-7".to_string(),
                "https://github.com/example/repo/issues/7".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.active_task_id = Some("task-7".to_string());
        snapshot.persistent.automation_issue =
            Some("https://github.com/example/repo/issues/7".to_string());
        snapshot.task_states.insert(
            "task-7".to_string(),
            WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-7".to_string()),
                session_status: Some(RootSessionStatus::Idle),
                agent_state: Some(AutomationAgentState::Review),
                ..Default::default()
            },
        );

        assert_eq!(
            snapshot.resolved_active_task_id().as_deref(),
            Some("task-7")
        );
        assert_eq!(
            snapshot.resolved_active_issue_url().as_deref(),
            Some("https://github.com/example/repo/issues/7")
        );
    }

    #[test]
    fn resolved_active_task_id_falls_back_to_claimed_issue_when_active_task_id_missing() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-13".to_string(),
                "https://github.com/example/repo/issues/13".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.persistent.automation_issue =
            Some("https://github.com/example/repo/issues/13".to_string());
        snapshot.task_states.insert(
            "task-13".to_string(),
            WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-13".to_string()),
                session_status: Some(RootSessionStatus::Idle),
                agent_state: Some(AutomationAgentState::Review),
                ..Default::default()
            },
        );

        assert_eq!(
            snapshot.resolved_active_task_id().as_deref(),
            Some("task-13")
        );
    }
}
