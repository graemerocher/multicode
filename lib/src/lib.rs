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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationAgentState {
    Working,
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
            usage_total_tokens: None,
            usage_total_cost: None,
            usage_cpu_percent: None,
            usage_ram_bytes: None,
            oom_kill_count: None,
        }
    }
}
