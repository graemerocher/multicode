use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::config::{CodexAgentConfig, CodexApprovalPolicy, CodexNetworkAccess, CodexSandboxMode};

const INITIALIZE_REQUEST_ID: i64 = 1;
const INITIAL_REQUEST_ID: i64 = 2;

type CodexSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

static NEXT_REQUEST_ID: AtomicI64 = AtomicI64::new(INITIAL_REQUEST_ID);
static SHARED_CONNECTIONS: OnceLock<std::sync::Mutex<HashMap<String, Arc<SharedCodexConnection>>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
pub struct CodexAppServerClient {
    uri: String,
}

#[derive(Debug)]
struct SharedCodexConnection {
    uri: String,
    socket: tokio::sync::Mutex<Option<CodexSocket>>,
}

impl CodexAppServerClient {
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub async fn probe(&self) -> Result<(), String> {
        let _: ThreadListResponse = self
            .request(
                "thread/list",
                json!({
                    "limit": 1,
                    "archived": false,
                    "sourceKinds": ["appServer"],
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn thread_list(&self, cwd: &str) -> Result<ThreadListResponse, String> {
        self.request(
            "thread/list",
            json!({
                "cwd": cwd,
                "archived": false,
                "limit": 100,
                "sourceKinds": ["appServer"],
                "sortKey": "updated_at",
            }),
        )
        .await
    }

    pub async fn thread_read(&self, thread_id: &str) -> Result<ThreadReadResponse, String> {
        self.thread_read_with_turns(thread_id, false).await
    }

    pub async fn thread_read_with_turns(
        &self,
        thread_id: &str,
        include_turns: bool,
    ) -> Result<ThreadReadResponse, String> {
        self.request(
            "thread/read",
            json!({
                "threadId": thread_id,
                "includeTurns": include_turns,
            }),
        )
        .await
    }

    pub async fn thread_start(
        &self,
        cwd: &str,
        config: &CodexAgentConfig,
    ) -> Result<ThreadStartResponse, String> {
        self.request("thread/start", build_thread_start_params(cwd, config))
            .await
    }

    pub async fn turn_start(
        &self,
        thread_id: &str,
        prompt: &str,
        config: &CodexAgentConfig,
    ) -> Result<TurnStartResponse, String> {
        self.request(
            "turn/start",
            build_turn_start_params(thread_id, prompt, config),
        )
        .await
    }

    pub async fn stream_notifications(
        &self,
        tx: tokio::sync::broadcast::Sender<CodexServerNotification>,
    ) -> Result<(), String> {
        let mut socket = self.connect_initialized().await?;
        while let Some(message) = socket.next().await {
            let message = message.map_err(|err| err.to_string())?;
            let Some(text) = message_to_text(message) else {
                continue;
            };
            let Ok(notification) = serde_json::from_str::<CodexServerNotificationEnvelope>(&text)
            else {
                continue;
            };
            let _ = tx.send(notification.into_notification());
        }
        Ok(())
    }

    async fn request<T>(&self, method: &str, params: Value) -> Result<T, String>
    where
        T: for<'de> Deserialize<'de>,
    {
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        self.shared_connection()
            .request(request_id, method, params)
            .await
    }

    async fn connect_initialized(&self) -> Result<CodexSocket, String> {
        connect_initialized_socket(&self.uri).await
    }

    fn shared_connection(&self) -> Arc<SharedCodexConnection> {
        let registry = SHARED_CONNECTIONS.get_or_init(Default::default);
        let mut registry = registry
            .lock()
            .expect("codex shared connection registry poisoned");
        registry
            .entry(self.uri.clone())
            .or_insert_with(|| {
                Arc::new(SharedCodexConnection {
                    uri: self.uri.clone(),
                    socket: tokio::sync::Mutex::new(None),
                })
            })
            .clone()
    }
}

impl SharedCodexConnection {
    async fn request<T>(&self, request_id: i64, method: &str, params: Value) -> Result<T, String>
    where
        T: for<'de> Deserialize<'de>,
    {
        let request = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        })
        .to_string();

        let mut socket = self.socket.lock().await;
        for attempt in 0..2 {
            if socket.is_none() {
                *socket = Some(connect_initialized_socket(&self.uri).await?);
            }

            let Some(active_socket) = socket.as_mut() else {
                continue;
            };

            if let Err(err) = active_socket
                .send(Message::Text(request.clone().into()))
                .await
            {
                *socket = None;
                if attempt == 0 {
                    continue;
                }
                return Err(err.to_string());
            }

            loop {
                match active_socket.next().await {
                    Some(Ok(message)) => {
                        let Some(text) = message_to_text(message) else {
                            continue;
                        };
                        let value: Value =
                            serde_json::from_str(&text).map_err(|err| err.to_string())?;
                        if value.get("id").and_then(Value::as_i64) != Some(request_id) {
                            continue;
                        }
                        if let Some(result) = value.get("result") {
                            return serde_json::from_value(result.clone())
                                .map_err(|err| err.to_string());
                        }
                        if let Some(error) = value.get("error") {
                            return Err(error.to_string());
                        }
                    }
                    Some(Err(err)) => {
                        *socket = None;
                        if attempt == 0 {
                            break;
                        }
                        return Err(err.to_string());
                    }
                    None => {
                        *socket = None;
                        if attempt == 0 {
                            break;
                        }
                        return Err(format!(
                            "codex app-server connection to '{}' closed",
                            self.uri
                        ));
                    }
                }
            }
        }

        Err(format!(
            "failed to complete codex app-server request '{}' for '{}'",
            method, self.uri
        ))
    }
}

async fn connect_initialized_socket(uri: &str) -> Result<CodexSocket, String> {
    let (mut socket, _) = connect_async(uri).await.map_err(|err| err.to_string())?;
    socket
        .send(Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": INITIALIZE_REQUEST_ID,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "multicode",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "experimentalApi": true,
                    },
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|err| err.to_string())?;

    while let Some(message) = socket.next().await {
        let message = message.map_err(|err| err.to_string())?;
        let Some(text) = message_to_text(message) else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).map_err(|err| err.to_string())?;
        if value.get("id").and_then(Value::as_i64) != Some(INITIALIZE_REQUEST_ID) {
            continue;
        }
        if value.get("error").is_some() {
            return Err(value["error"].to_string());
        }
        socket
            .send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "method": "initialized",
                })
                .to_string()
                .into(),
            ))
            .await
            .map_err(|err| err.to_string())?;
        return Ok(socket);
    }

    Err(format!(
        "codex app-server initialize did not complete for '{}'",
        uri
    ))
}

fn build_thread_start_params(cwd: &str, config: &CodexAgentConfig) -> Value {
    json!({
        "cwd": cwd,
        "model": config.model.as_deref(),
        "modelProvider": config.model_provider.as_deref(),
        "sandbox": thread_start_sandbox(config.sandbox_mode),
        "approvalPolicy": approval_policy_value(config.approval_policy),
        "personality": "pragmatic",
    })
}

fn build_turn_start_params(thread_id: &str, prompt: &str, config: &CodexAgentConfig) -> Value {
    json!({
        "threadId": thread_id,
        "approvalPolicy": approval_policy_value(config.approval_policy),
        "personality": "pragmatic",
        "sandboxPolicy": sandbox_policy_value(config.sandbox_mode, config.network_access),
        "input": [
            {
                "type": "text",
                "text": prompt,
            }
        ],
    })
}

fn approval_policy_value(policy: CodexApprovalPolicy) -> &'static str {
    match policy {
        CodexApprovalPolicy::Untrusted => "untrusted",
        CodexApprovalPolicy::OnFailure => "on-failure",
        CodexApprovalPolicy::OnRequest => "on-request",
        CodexApprovalPolicy::Never => "never",
    }
}

fn thread_start_sandbox(mode: CodexSandboxMode) -> &'static str {
    match mode {
        CodexSandboxMode::ReadOnly => "read-only",
        CodexSandboxMode::WorkspaceWrite => "workspace-write",
        CodexSandboxMode::DangerFullAccess | CodexSandboxMode::ExternalSandbox => {
            "danger-full-access"
        }
    }
}

fn sandbox_policy_value(mode: CodexSandboxMode, network: CodexNetworkAccess) -> Value {
    match mode {
        CodexSandboxMode::ReadOnly => json!({
            "type": "readOnly",
            "networkAccess": matches!(network, CodexNetworkAccess::Enabled),
        }),
        CodexSandboxMode::WorkspaceWrite => json!({
            "type": "workspaceWrite",
            "networkAccess": matches!(network, CodexNetworkAccess::Enabled),
        }),
        CodexSandboxMode::DangerFullAccess => json!({
            "type": "dangerFullAccess",
        }),
        CodexSandboxMode::ExternalSandbox => json!({
            "type": "externalSandbox",
            "networkAccess": match network {
                CodexNetworkAccess::Restricted => "restricted",
                CodexNetworkAccess::Enabled => "enabled",
            },
        }),
    }
}

fn message_to_text(message: Message) -> Option<String> {
    match message {
        Message::Text(text) => Some(text.to_string()),
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec()).ok(),
        Message::Close(_) => None,
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_start_params_include_codex_permissions() {
        let params = build_thread_start_params(
            "/workspace",
            &CodexAgentConfig {
                commands: vec!["codex".to_string()],
                profile: Some("default".to_string()),
                model: Some("gpt-5-codex".to_string()),
                model_provider: Some("openai".to_string()),
                approval_policy: CodexApprovalPolicy::Never,
                sandbox_mode: CodexSandboxMode::ExternalSandbox,
                network_access: CodexNetworkAccess::Enabled,
            },
        );

        assert_eq!(params["cwd"], "/workspace");
        assert_eq!(params["model"], "gpt-5-codex");
        assert_eq!(params["modelProvider"], "openai");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["sandbox"], "danger-full-access");
        assert_eq!(params["personality"], "pragmatic");
    }

    #[test]
    fn turn_start_params_use_external_sandbox_policy() {
        let params = build_turn_start_params(
            "thread-123",
            "Fix the bug",
            &CodexAgentConfig {
                commands: vec!["codex".to_string()],
                profile: None,
                model: None,
                model_provider: None,
                approval_policy: CodexApprovalPolicy::Never,
                sandbox_mode: CodexSandboxMode::ExternalSandbox,
                network_access: CodexNetworkAccess::Enabled,
            },
        );

        assert_eq!(params["threadId"], "thread-123");
        assert_eq!(params["approvalPolicy"], "never");
        assert_eq!(params["personality"], "pragmatic");
        assert_eq!(params["sandboxPolicy"]["type"], "externalSandbox");
        assert_eq!(params["sandboxPolicy"]["networkAccess"], "enabled");
        assert_eq!(params["input"][0]["type"], "text");
        assert_eq!(params["input"][0]["text"], "Fix the bug");
    }

    #[test]
    fn turn_start_params_use_workspace_write_policy() {
        let params = build_turn_start_params(
            "thread-123",
            "Fix the bug",
            &CodexAgentConfig {
                commands: vec!["codex".to_string()],
                profile: None,
                model: None,
                model_provider: None,
                approval_policy: CodexApprovalPolicy::OnRequest,
                sandbox_mode: CodexSandboxMode::WorkspaceWrite,
                network_access: CodexNetworkAccess::Restricted,
            },
        );

        assert_eq!(params["approvalPolicy"], "on-request");
        assert_eq!(params["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(params["sandboxPolicy"]["networkAccess"], false);
    }

    #[test]
    fn codex_thread_status_treats_command_approvals_as_human_input() {
        let status = CodexThreadStatus::Active {
            active_flags: vec![CodexThreadActiveFlag::WaitingOnApproval],
        };

        assert!(status.waits_for_human_input());
    }

    #[test]
    fn codex_thread_status_marks_system_error_for_replacement() {
        assert!(CodexThreadStatus::SystemError.requires_replacement());
        assert!(!CodexThreadStatus::Idle.requires_replacement());
    }
}

pub async fn forward_codex_notifications_forever(
    client: CodexAppServerClient,
    tx: tokio::sync::broadcast::Sender<CodexServerNotification>,
) {
    loop {
        let _ = client.stream_notifications(tx.clone()).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadListResponse {
    pub data: Vec<CodexThread>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadStartResponse {
    pub thread: CodexThread,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ThreadReadResponse {
    pub thread: CodexThreadRead,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TurnStartResponse {
    pub turn: CodexTurn,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexTurn {
    pub id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexThreadRead {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub status: Option<CodexThreadStatus>,
    #[serde(default)]
    pub turns: Vec<CodexThreadTurn>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexThreadTurn {
    #[serde(default)]
    pub items: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexThread {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    pub status: CodexThreadStatus,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum CodexThreadStatus {
    #[serde(rename = "notLoaded")]
    NotLoaded,
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "systemError")]
    SystemError,
    #[serde(rename = "active")]
    Active {
        #[serde(default, rename = "activeFlags")]
        active_flags: Vec<CodexThreadActiveFlag>,
    },
}

impl CodexThreadStatus {
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    pub fn requires_replacement(&self) -> bool {
        matches!(self, Self::SystemError)
    }

    pub fn waits_for_human_input(&self) -> bool {
        matches!(
            self,
            Self::Active {
                active_flags
            } if active_flags.contains(&CodexThreadActiveFlag::WaitingOnUserInput)
                || active_flags.contains(&CodexThreadActiveFlag::WaitingOnApproval)
        )
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub enum CodexThreadActiveFlag {
    #[serde(rename = "waitingOnApproval")]
    WaitingOnApproval,
    #[serde(rename = "waitingOnUserInput")]
    WaitingOnUserInput,
}

#[derive(Debug, Clone)]
pub enum CodexServerNotification {
    ThreadStarted {
        thread: CodexThread,
    },
    ThreadStatusChanged {
        thread_id: String,
        status: CodexThreadStatus,
    },
    TurnStarted {
        thread_id: String,
    },
    TurnCompleted {
        thread_id: String,
    },
    Other,
}

#[derive(Debug, Deserialize)]
struct CodexServerNotificationEnvelope {
    method: String,
    #[serde(default)]
    params: Value,
}

impl CodexServerNotificationEnvelope {
    fn into_notification(self) -> CodexServerNotification {
        match self.method.as_str() {
            "thread/started" => serde_json::from_value::<ThreadStartedNotification>(self.params)
                .map(|params| CodexServerNotification::ThreadStarted {
                    thread: params.thread,
                })
                .unwrap_or(CodexServerNotification::Other),
            "thread/status/changed" => {
                serde_json::from_value::<ThreadStatusChangedNotification>(self.params)
                    .map(|params| CodexServerNotification::ThreadStatusChanged {
                        thread_id: params.thread_id,
                        status: params.status,
                    })
                    .unwrap_or(CodexServerNotification::Other)
            }
            "turn/started" => serde_json::from_value::<TurnLifecycleNotification>(self.params)
                .map(|params| CodexServerNotification::TurnStarted {
                    thread_id: params.thread_id,
                })
                .unwrap_or(CodexServerNotification::Other),
            "turn/completed" => serde_json::from_value::<TurnLifecycleNotification>(self.params)
                .map(|params| CodexServerNotification::TurnCompleted {
                    thread_id: params.thread_id,
                })
                .unwrap_or(CodexServerNotification::Other),
            _ => CodexServerNotification::Other,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ThreadStartedNotification {
    thread: CodexThread,
}

#[derive(Debug, Deserialize)]
struct ThreadStatusChangedNotification {
    #[serde(rename = "threadId")]
    thread_id: String,
    status: CodexThreadStatus,
}

#[derive(Debug, Deserialize)]
struct TurnLifecycleNotification {
    #[serde(rename = "threadId")]
    thread_id: String,
}
