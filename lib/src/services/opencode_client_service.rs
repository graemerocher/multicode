use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use tokio::{
    process::Command,
    sync::{broadcast, watch},
    task::JoinHandle,
};
use tokio_stream::StreamExt;
use url::Url;

use super::{
    runtime::{RuntimeActivity, WorkspaceRuntime},
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{
    OpencodeClientSnapshot, WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot,
    manager::Workspace, opencode,
};

const HEALTH_RETRY_INTERVAL: Duration = Duration::from_millis(200);
const HEALTH_MONITOR_INTERVAL: Duration = Duration::from_millis(500);
const SSE_RECONNECT_INTERVAL: Duration = Duration::from_millis(500);
const WORKSPACE_EVENT_CHANNEL_CAPACITY: usize = 512;

#[derive(Debug)]
pub enum OpencodeClientServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for OpencodeClientServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

pub async fn opencode_client_service(
    manager: Arc<WorkspaceManager>,
) -> Result<(), OpencodeClientServiceError> {
    let shared_http_client = build_shared_http_client();
    monitor_workspace_snapshots(manager, move |_, workspace, workspace_rx| {
        let shared_http_client = shared_http_client.clone();
        async move {
            tokio::spawn(async move {
                watch_workspace_snapshot(workspace, workspace_rx, shared_http_client).await;
            });
            Ok(())
        }
    })
    .await
}

async fn watch_workspace_snapshot(
    workspace: Workspace,
    mut workspace_rx: watch::Receiver<WorkspaceSnapshot>,
    shared_http_client: Option<reqwest::Client>,
) {
    let mut last_client_uri: Option<String> = None;
    let mut cached_probe_client: Option<(String, Arc<opencode::client::Client>)> = None;
    let mut event_forward_task: Option<(String, JoinHandle<()>)> = None;
    let event_generation = Arc::new(AtomicU64::new(0));

    loop {
        let snapshot = workspace_rx.borrow().clone();
        let transient = snapshot.transient.clone();
        let should_monitor = transient.is_some();

        if !should_monitor {
            abort_event_forward_task(&mut event_forward_task);
            event_generation.fetch_add(1, Ordering::Relaxed);
            cached_probe_client = None;
            if transient.is_none() && snapshot.opencode_client.is_some() {
                workspace.update(|next| {
                    if next.transient.is_none() {
                        if next.opencode_client.is_some() {
                            next.opencode_client = None;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                });
                last_client_uri = None;
            }

            if workspace_rx.changed().await.is_err() {
                break;
            }
            continue;
        }

        let transient = transient.expect("checked above");
        let current_uri = normalize_base_uri(&transient.uri);
        let active_client_snapshot = if snapshot.opencode_client.is_some()
            && (last_client_uri.as_deref() == Some(current_uri.as_str())
                || last_client_uri.is_none())
        {
            snapshot.opencode_client.clone()
        } else {
            None
        };
        let health_probe_client = health_probe_client_for_uri(
            &mut cached_probe_client,
            &current_uri,
            active_client_snapshot.as_ref(),
            shared_http_client.as_ref(),
        );
        let is_healthy = match health_probe_client.global_health().await {
            Ok(response) => response.into_inner().healthy,
            Err(_) => false,
        };

        if is_healthy {
            let active_client_snapshot = if let Some(existing_snapshot) = active_client_snapshot {
                existing_snapshot
            } else {
                let workspace_event_tx = snapshot
                    .opencode_client
                    .as_ref()
                    .map(|client_snapshot| client_snapshot.events.clone())
                    .unwrap_or_else(|| broadcast::channel(WORKSPACE_EVENT_CHANNEL_CAPACITY).0);
                let client_snapshot = OpencodeClientSnapshot {
                    client: health_probe_client.clone(),
                    events: workspace_event_tx,
                };
                workspace.update(|next| {
                    if next.transient.as_ref() == Some(&transient) {
                        let needs_update = next
                            .opencode_client
                            .as_ref()
                            .map(|current| !Arc::ptr_eq(&current.client, &client_snapshot.client))
                            .unwrap_or(true);
                        if needs_update {
                            next.opencode_client = Some(client_snapshot.clone());
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                });
                last_client_uri = Some(current_uri.clone());
                client_snapshot
            };

            let should_restart_event_forward_task = match &event_forward_task {
                Some((event_uri, handle)) => event_uri != &current_uri || handle.is_finished(),
                None => true,
            };

            if should_restart_event_forward_task {
                abort_event_forward_task(&mut event_forward_task);
                let generation = event_generation.fetch_add(1, Ordering::Relaxed) + 1;
                let event_uri = current_uri.clone();
                let event_client = active_client_snapshot.client.clone();
                let event_tx = active_client_snapshot.events.clone();
                let event_generation = event_generation.clone();
                let handle = tokio::spawn(async move {
                    forward_global_events(event_client, event_tx, generation, event_generation)
                        .await;
                });
                event_forward_task = Some((event_uri, handle));
            }

            if !wait_for_change_or_timeout(&mut workspace_rx, HEALTH_MONITOR_INTERVAL).await {
                break;
            }
            continue;
        }

        if snapshot.opencode_client.is_some() {
            workspace.update(|next| {
                if next.transient.as_ref() == Some(&transient) {
                    if next.opencode_client.is_some() {
                        next.opencode_client = None;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            });
            last_client_uri = None;
        }
        abort_event_forward_task(&mut event_forward_task);
        event_generation.fetch_add(1, Ordering::Relaxed);

        match WorkspaceRuntime::read_activity(&transient.runtime).await {
            RuntimeActivity::Stopped => {
                workspace.update(|next| {
                    if next.transient.as_ref() == Some(&transient) {
                        let mut changed = false;
                        if next.transient.is_some() {
                            next.transient = None;
                            changed = true;
                        }
                        if next.opencode_client.is_some() {
                            next.opencode_client = None;
                            changed = true;
                        }
                        changed
                    } else {
                        false
                    }
                });
                last_client_uri = None;
            }
            RuntimeActivity::Active | RuntimeActivity::Unknown => {}
        }

        if !wait_for_change_or_timeout(&mut workspace_rx, HEALTH_RETRY_INTERVAL).await {
            break;
        }
    }

    abort_event_forward_task(&mut event_forward_task);
    event_generation.fetch_add(1, Ordering::Relaxed);
}

fn abort_event_forward_task(event_forward_task: &mut Option<(String, JoinHandle<()>)>) {
    if let Some((_, handle)) = event_forward_task.take() {
        handle.abort();
    }
}

fn health_probe_client_for_uri(
    cached_probe_client: &mut Option<(String, Arc<opencode::client::Client>)>,
    current_uri: &str,
    active_client_snapshot: Option<&OpencodeClientSnapshot>,
    shared_http_client: Option<&reqwest::Client>,
) -> Arc<opencode::client::Client> {
    if let Some(active_client_snapshot) = active_client_snapshot {
        let active_client = active_client_snapshot.client.clone();
        *cached_probe_client = Some((current_uri.to_string(), active_client.clone()));
        return active_client;
    }

    if let Some((cached_uri, cached_client)) = cached_probe_client
        && cached_uri == current_uri
    {
        return cached_client.clone();
    }

    let client = Arc::new(create_opencode_client(current_uri, shared_http_client));
    *cached_probe_client = Some((current_uri.to_string(), client.clone()));
    client
}

fn create_opencode_client(
    current_uri: &str,
    shared_http_client: Option<&reqwest::Client>,
) -> opencode::client::Client {
    let (baseurl, auth_header) = opencode_client_target(current_uri);
    if let Some(auth_header) = auth_header {
        return opencode::client::Client::new_with_client(
            &baseurl,
            build_authenticated_http_client(auth_header),
        );
    }

    if let Some(shared_http_client) = shared_http_client {
        opencode::client::Client::new_with_client(&baseurl, shared_http_client.clone())
    } else {
        opencode::client::Client::new(&baseurl)
    }
}

fn build_shared_http_client() -> Option<reqwest::Client> {
    let timeout = Duration::from_secs(15);
    reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .build()
        .ok()
}

fn build_authenticated_http_client(auth_header: String) -> reqwest::Client {
    let timeout = Duration::from_secs(15);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        reqwest::header::HeaderValue::from_str(&auth_header)
            .expect("generated basic auth header should be valid"),
    );
    reqwest::Client::builder()
        .connect_timeout(timeout)
        .timeout(timeout)
        .default_headers(headers)
        .build()
        .expect("authenticated opencode http client should build")
}

fn opencode_client_target(current_uri: &str) -> (String, Option<String>) {
    let Ok(mut url) = Url::parse(current_uri) else {
        return (current_uri.to_string(), None);
    };

    let username = url.username().to_string();
    let password = url.password().map(str::to_string);
    if username.is_empty() {
        return (current_uri.to_string(), None);
    }

    let _ = url.set_username("");
    let _ = url.set_password(None);
    let credentials = match password {
        Some(password) => format!("{username}:{password}"),
        None => format!("{username}:"),
    };
    let encoded = BASE64_STANDARD.encode(credentials);
    (
        url.to_string().trim_end_matches('/').to_string(),
        Some(format!("Basic {encoded}")),
    )
}

async fn forward_global_events(
    client: Arc<opencode::client::Client>,
    event_tx: broadcast::Sender<opencode::client::types::GlobalEvent>,
    generation: u64,
    event_generation: Arc<AtomicU64>,
) {
    let mut parser = SseDataParser::default();

    loop {
        if event_generation.load(Ordering::Relaxed) != generation {
            return;
        }

        let stream = match client.global_event().await {
            Ok(response) => response.into_inner_stream(),
            Err(_) => {
                tokio::time::sleep(SSE_RECONNECT_INTERVAL).await;
                continue;
            }
        };

        tokio::pin!(stream);
        parser.reset_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(_) => break,
            };
            for data in parser.push_chunk(&chunk) {
                if event_generation.load(Ordering::Relaxed) != generation {
                    return;
                }
                if let Ok(event) =
                    serde_json::from_str::<opencode::client::types::GlobalEvent>(&data)
                {
                    let _ = event_tx.send(event);
                }
            }
        }

        if let Some(data) = parser.finish_stream() {
            if event_generation.load(Ordering::Relaxed) != generation {
                return;
            }
            if let Ok(event) = serde_json::from_str::<opencode::client::types::GlobalEvent>(&data) {
                let _ = event_tx.send(event);
            }
        }

        tokio::time::sleep(SSE_RECONNECT_INTERVAL).await;
    }
}

fn normalize_base_uri(uri: &str) -> String {
    uri.trim_end_matches('/').to_string()
}

#[derive(Debug, Default)]
struct SseDataParser {
    line_buffer: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseDataParser {
    fn reset_stream(&mut self) {
        self.line_buffer.clear();
        self.data_lines.clear();
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut events = Vec::new();

        for byte in chunk {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.line_buffer);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }

                if line.is_empty() {
                    if !self.data_lines.is_empty() {
                        events.push(self.data_lines.join("\n"));
                        self.data_lines.clear();
                    }
                    continue;
                }

                if line.starts_with(b":") {
                    continue;
                }

                if let Some(rest) = line.strip_prefix(b"data:") {
                    let rest = rest.strip_prefix(b" ").unwrap_or(rest);
                    self.data_lines
                        .push(String::from_utf8_lossy(rest).into_owned());
                }
            } else {
                self.line_buffer.push(*byte);
            }
        }

        events
    }

    fn finish_stream(&mut self) -> Option<String> {
        if self.data_lines.is_empty() {
            return None;
        }
        let event = self.data_lines.join("\n");
        self.data_lines.clear();
        Some(event)
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

#[cfg_attr(not(test), allow(dead_code))]
async fn read_unit_activity(unit: &str) -> RuntimeActivity {
    let output = match Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "--property",
            "ActiveState",
            "--value",
        ])
        .stdin(Stdio::null())
        .output()
        .await
    {
        Ok(output) => output,
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                return RuntimeActivity::Unknown;
            }
            return RuntimeActivity::Unknown;
        }
    };

    if !output.status.success() {
        return RuntimeActivity::Stopped;
    }

    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if matches!(state.as_str(), "active" | "activating") {
        RuntimeActivity::Active
    } else {
        RuntimeActivity::Stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_VAR_LOCK;
    use std::{
        ffi::OsString,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{RuntimeBackend, RuntimeHandleSnapshot, TransientWorkspaceSnapshot};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "multicode-opencode-client-service-{}-{}",
                std::process::id(),
                unique
            ));
            fs::create_dir_all(&path).expect("test dir should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        old_value: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old_value = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old_value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.old_value {
                unsafe {
                    std::env::set_var(self.key, value);
                }
            } else {
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn opencode_client_target_extracts_basic_auth_header_and_strips_userinfo() {
        let (baseurl, auth_header) =
            opencode_client_target("http://opencode:secret@127.0.0.1:1234/");
        assert_eq!(baseurl, "http://127.0.0.1:1234");
        assert_eq!(
            auth_header,
            Some(format!(
                "Basic {}",
                BASE64_STANDARD.encode("opencode:secret")
            ))
        );
    }

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

    #[test]
    fn health_probe_client_reuses_cached_client_for_same_uri() {
        let mut cached_probe_client = None;

        let first =
            health_probe_client_for_uri(&mut cached_probe_client, "http://127.0.0.1:9", None, None);
        let second =
            health_probe_client_for_uri(&mut cached_probe_client, "http://127.0.0.1:9", None, None);

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn health_probe_client_prefers_active_snapshot_client() {
        let cached_client = Arc::new(opencode::client::Client::new("http://127.0.0.1:9"));
        let active_client = Arc::new(opencode::client::Client::new("http://127.0.0.1:10"));
        let events = broadcast::channel(WORKSPACE_EVENT_CHANNEL_CAPACITY).0;
        let active_snapshot = OpencodeClientSnapshot {
            client: active_client.clone(),
            events,
        };
        let mut cached_probe_client =
            Some(("http://127.0.0.1:9".to_string(), cached_client.clone()));

        let selected = health_probe_client_for_uri(
            &mut cached_probe_client,
            "http://127.0.0.1:10",
            Some(&active_snapshot),
            None,
        );

        assert!(Arc::ptr_eq(&selected, &active_client));
        let (cached_uri, cached_after) =
            cached_probe_client.expect("cached probe client should track active client");
        assert_eq!(cached_uri, "http://127.0.0.1:10");
        assert!(Arc::ptr_eq(&cached_after, &active_client));
    }

    #[test]
    fn stores_client_and_marks_workspace_started_when_health_is_available() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");
            let workspace = manager
                .get_workspace("alpha")
                .expect("workspace should exist");
            let mut workspace_rx = workspace.subscribe();

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should have local address");
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let _ = socket.read(&mut buffer).await;
                        let body = r#"{"healthy":true,"version":"test"}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    format!("http://{addr}"),
                    "run-u-health.service",
                ));
                true
            });

            let service_task = tokio::spawn(opencode_client_service(manager.clone()));

            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.transient.is_some() && snapshot.opencode_client.is_some() {
                        break;
                    }
                }
            })
            .await
            .expect("workspace should become started with a client");

            let snapshot = workspace_rx.borrow().clone();
            let client_snapshot = snapshot
                .opencode_client
                .as_ref()
                .expect("client snapshot should be present");
            assert!(snapshot.transient.is_some());
            assert!(snapshot.opencode_client.is_some());

            let response = client_snapshot
                .client
                .global_health()
                .await
                .expect("stored client should be usable");
            assert!(response.into_inner().healthy);

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn trailing_slash_uri_is_normalized_before_health_requests() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");
            let workspace = manager
                .get_workspace("alpha")
                .expect("workspace should exist");
            let mut workspace_rx = workspace.subscribe();

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should have local address");
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);

                        let (status, body, content_type) = if request
                            .contains("GET /global/health ")
                        {
                            (
                                "200 OK",
                                r#"{"healthy":true,"version":"test"}"#,
                                "application/json",
                            )
                        } else {
                            (
                                "200 OK",
                                "<html>fallback</html>",
                                "text/html",
                            )
                        };

                        let response = format!(
                            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    format!("http://{addr}/"),
                    "run-u-health-trailing-slash.service",
                ));
                true
            });

            let service_task = tokio::spawn(opencode_client_service(manager.clone()));

            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.transient.is_some() && snapshot.opencode_client.is_some() {
                        break;
                    }
                }
            })
            .await
            .expect("workspace should become started when URI is normalized");

            assert!(workspace_rx.borrow().transient.is_some());
            assert!(workspace_rx.borrow().opencode_client.is_some());

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn forwards_global_event_stream_data_to_broadcast_channel() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");
            let workspace = manager
                .get_workspace("alpha")
                .expect("workspace should exist");
            let mut workspace_rx = workspace.subscribe();

            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("test listener should bind");
            let addr = listener
                .local_addr()
                .expect("listener should have local address");
            let server_task = tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut buffer = vec![0_u8; 4096];
                        let read = socket.read(&mut buffer).await.unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);

                        if request.contains("GET /global/event ") {
                            let body = "data: {\"directory\":\"/workspace\",\"payload\":{\"type\":\"global.disposed\",\"properties\":{}}}\n\n";
                            let response = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            );
                            let _ = socket.write_all(response.as_bytes()).await;
                            let _ = socket.shutdown().await;
                            return;
                        }

                        if request.contains("GET /global/health ") {
                            let body = r#"{"healthy":true,"version":"test"}"#;
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

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    format!("http://{addr}"),
                    "run-u-events.service",
                ));
                true
            });

            let service_task = tokio::spawn(opencode_client_service(manager.clone()));

            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    if workspace_rx.borrow().transient.is_some()
                        && workspace_rx.borrow().opencode_client.is_some()
                    {
                        break;
                    }
                }
            })
            .await
            .expect("workspace should become started before consuming events");

            let snapshot = workspace_rx.borrow().clone();
            let mut event_rx = snapshot
                .opencode_client
                .as_ref()
                .expect("workspace should expose opencode client snapshot")
                .events
                .subscribe();

            let event = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => return event,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(err) => panic!("event receiver should stay open: {err:?}"),
                    }
                }
            })
            .await
            .expect("global event should be published");

            assert_eq!(event.directory, "/workspace");

            service_task.abort();
            server_task.abort();
        });
    }

    #[test]
    fn clears_transient_and_marks_workspace_stopped_when_unit_is_stopped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let bin_dir = root.path().join("bin");
            fs::create_dir_all(&bin_dir).expect("bin dir should be created");

            let fake_systemctl = bin_dir.join("systemctl");
            fs::write(&fake_systemctl, "#!/bin/bash\necho inactive\nexit 0\n")
                .expect("fake systemctl should be written");
            let mut perms = fs::metadata(&fake_systemctl)
                .expect("fake systemctl metadata should be readable")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake_systemctl, perms)
                .expect("fake systemctl should be executable");

            let old_path = std::env::var("PATH").unwrap_or_default();
            let path = format!("{}:{old_path}", bin_dir.display());
            let _path_guard = EnvVarGuard::set("PATH", &path);

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");
            let workspace = manager
                .get_workspace("alpha")
                .expect("workspace should exist");
            let mut workspace_rx = workspace.subscribe();

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    "http://127.0.0.1:9".to_string(),
                    "run-u-stopped.service",
                ));
                true
            });

            let service_task = tokio::spawn(opencode_client_service(manager.clone()));

            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if workspace_rx.changed().await.is_err() {
                        panic!("workspace watch should remain open");
                    }
                    let snapshot = workspace_rx.borrow().clone();
                    if snapshot.transient.is_none() && snapshot.opencode_client.is_none() {
                        break;
                    }
                }
            })
            .await
            .expect("workspace should be stopped when unit is inactive");

            let snapshot = workspace_rx.borrow().clone();
            assert!(snapshot.transient.is_none());
            assert!(snapshot.opencode_client.is_none());

            service_task.abort();
        });
    }

    #[test]
    fn keeps_transient_without_storing_client_when_endpoint_unavailable_but_unit_is_active() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let bin_dir = root.path().join("bin");
            fs::create_dir_all(&bin_dir).expect("bin dir should be created");

            let fake_systemctl = bin_dir.join("systemctl");
            fs::write(&fake_systemctl, "#!/bin/bash\necho active\nexit 0\n")
                .expect("fake systemctl should be written");
            let mut perms = fs::metadata(&fake_systemctl)
                .expect("fake systemctl metadata should be readable")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&fake_systemctl, perms)
                .expect("fake systemctl should be executable");

            let old_path = std::env::var("PATH").unwrap_or_default();
            let path = format!("{}:{old_path}", bin_dir.display());
            let _path_guard = EnvVarGuard::set("PATH", &path);

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");
            let workspace = manager
                .get_workspace("alpha")
                .expect("workspace should exist");

            workspace.update(|snapshot| {
                snapshot.transient = Some(transient_snapshot(
                    "http://127.0.0.1:9".to_string(),
                    "run-u-active.service",
                ));
                true
            });

            let service_task = tokio::spawn(opencode_client_service(manager.clone()));
            tokio::time::sleep(Duration::from_millis(700)).await;

            let snapshot = workspace.subscribe().borrow().clone();
            assert!(snapshot.transient.is_some());
            assert!(snapshot.opencode_client.is_none());

            service_task.abort();
        });
    }

    #[test]
    fn unit_activity_is_unknown_when_systemctl_is_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let empty_bin = root.path().join("empty-bin");
            fs::create_dir_all(&empty_bin).expect("empty bin should be created");
            let _path_guard = EnvVarGuard::set("PATH", empty_bin.as_os_str());

            let activity = read_unit_activity("missing.service").await;
            assert_eq!(activity, RuntimeActivity::Unknown);
        });
    }
}
