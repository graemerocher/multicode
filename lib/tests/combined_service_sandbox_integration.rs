#![cfg(target_os = "linux")]

use std::{
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command as StdCommand,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use multicode_lib::{WorkspaceSnapshot, services::CombinedService};
use serde_json::Value;
use tokio::process::Command;

static ENV_LOCK: Mutex<()> = Mutex::new(());

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
            "multicode-sandbox-integration-{}-{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&path).expect("test root should be created");
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
fn starts_and_stops_systemd_bwrap_workspace_process() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let root = TestDir::new();
        let workspace_directory = root.path().join("workspaces");
        let home = root.path().join("home");
        let runtime_dir = root.path().join("runtime");
        let bin_dir = root.path().join("bin");

        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(home.join(".config/opencode"))
            .expect("home config path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".cache/opencode"))
            .expect("home cache path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".local/share/opencode"))
            .expect("home share path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".local/state"))
            .expect("home state path should exist before tmpfs mount");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");

        let fake_opencode = bin_dir.join("opencode");
        fs::write(
            &fake_opencode,
            "#!/bin/bash\nexec /bin/bash -lc 'trap : TERM INT; while true; do sleep 1; done'\n",
        )
        .expect("fake opencode script should be written");
        let mut perms = fs::metadata(&fake_opencode)
            .expect("fake opencode metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_opencode, perms).expect("fake opencode should be executable");

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "workspace-directory = \"{}\"\nopencode = [\"opencode-cli\", \"opencode\"]\n\n[isolation]\ninherit-env = [\"PATH\", \"HOME\", \"XDG_RUNTIME_DIR\"]\n",
                workspace_directory.display()
            ),
        )
        .expect("config should be written");

        let isolated_auth_file = workspace_directory
            .join(".multicode")
            .join("isolate")
            .join("alpha")
            .join(
                home.join(".local/share/opencode/auth.json")
                    .strip_prefix("/")
                    .expect("auth path should be absolute"),
            );
        if let Some(parent) = isolated_auth_file.parent() {
            fs::create_dir_all(parent).expect("isolated auth parent should exist");
        }
        fs::write(&isolated_auth_file, r#"{"token":"sandbox"}"#)
            .expect("isolated auth file should be written");

        let service = CombinedService::from_config_path(&config_path)
            .await
            .expect("combined service should start");
        service
            .create_workspace("alpha")
            .await
            .expect("workspace should be created");

        service
            .start_workspace("alpha")
            .await
            .expect("workspace should start");

        let mut workspace_rx = service
            .manager
            .get_workspace("alpha")
            .expect("workspace should exist")
            .subscribe();
        let started_snapshot = workspace_rx.borrow().clone();
        let transient = started_snapshot
            .transient
            .clone()
            .expect("transient snapshot should be present after start");
        assert!(started_snapshot.opencode_client.is_none());

        wait_for_active_unit(&transient.unit)
            .await
            .expect("systemd unit should become active/activating");

        service
            .stop_workspace("alpha")
            .await
            .expect("workspace should stop");

        let stopped_snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = workspace_rx.borrow().clone();
                if snapshot.transient.is_none() && snapshot.opencode_client.is_none() {
                    return snapshot;
                }
                if workspace_rx.changed().await.is_err() {
                    panic!("workspace watch channel closed while waiting for stop state");
                }
            }
        })
        .await
        .expect("workspace should clear transient and client after stop");
        assert_eq!(stopped_snapshot.transient, None);
        assert!(stopped_snapshot.opencode_client.is_none());
    });
}

#[test]
fn starts_real_opencode_with_added_skills_from_and_lists_agent() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        ensure_opencode_cli_available().await;

        let root = TestDir::new();
        let workspace_directory = root.path().join("workspaces");
        let home = workspace_directory.join("alpha");
        let runtime_dir = root.path().join("runtime");
        let skill_root = root.path().join("workspace-skills");
        let skill_dir = skill_root.join("sample-skill");

        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&skill_dir).expect("skill dir should exist");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---
name: sample-skill
description: sample skill
---

# Sample
",
        )
        .expect("skill file should exist");

        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{}"
opencode = ["opencode-cli", "opencode"]

[isolation]
tmpfs = ["~/.config/opencode", "~/.cache/opencode", "~/.local/share/opencode", "~/.local/state"]
add-skills-from = ["workspace-skills"]
inherit-env = ["PATH", "HOME", "XDG_RUNTIME_DIR"]
"#,
                workspace_directory.display()
            ),
        )
        .expect("config should be written");

        let service = CombinedService::from_config_path(&config_path)
            .await
            .expect("combined service should start");
        service
            .create_workspace("alpha")
            .await
            .expect("workspace should be created");

        fs::create_dir_all(home.join(".config/opencode"))
            .expect("home config path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".cache/opencode"))
            .expect("home cache path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".local/share/opencode"))
            .expect("home share path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".local/state"))
            .expect("home state path should exist before tmpfs mount");

        let target_skill_dir = home.join(".config/opencode/skills/sample-skill");
        let output = Command::new("bwrap")
            .args(["--ro-bind", "/", "/"])
            .args(["--tmpfs", &home.join(".config/opencode").to_string_lossy()])
            .args(["--tmpfs", &home.join(".cache/opencode").to_string_lossy()])
            .args([
                "--tmpfs",
                &home.join(".local/share/opencode").to_string_lossy(),
            ])
            .args(["--tmpfs", &home.join(".local/state").to_string_lossy()])
            .args([
                "--bind",
                &workspace_directory.join("alpha").to_string_lossy(),
                &workspace_directory.join("alpha").to_string_lossy(),
            ])
            .args([
                "--ro-bind",
                &skill_dir.to_string_lossy(),
                &target_skill_dir.to_string_lossy(),
            ])
            .args([
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "sh",
                "-lc",
                &format!(
                    "HOME={} XDG_RUNTIME_DIR={} opencode-cli agent list",
                    shell_escape_for_test(&home),
                    shell_escape_for_test(&runtime_dir)
                ),
            ])
            .output()
            .await
            .expect("agent list probe should run");

        assert!(
            output.status.success(),
            "agent list should succeed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("sample-skill"),
            "agent list should include added skill, stdout={stdout}"
        );
    });
}

#[test]
fn starts_real_opencode_with_strict_isolation_and_serves_health_get() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        ensure_opencode_cli_available().await;

        let root = TestDir::new();
        let workspace_directory = root.path().join("workspaces");
        let home = workspace_directory.join("alpha");
        let runtime_dir = root.path().join("runtime");

        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");

        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "workspace-directory = \"{}\"\n\n[isolation]\ntmpfs = [\"~/.config/opencode\", \"~/.cache/opencode\", \"~/.local/share/opencode\", \"~/.local/state\"]\ninherit-env = [\"PATH\", \"HOME\", \"XDG_RUNTIME_DIR\"]\n",
                workspace_directory.display()
            ),
        )
        .expect("config should be written");

        let service = CombinedService::from_config_path(&config_path)
            .await
            .expect("combined service should start");
        service
            .create_workspace("alpha")
            .await
            .expect("workspace should be created");

        fs::create_dir_all(home.join(".config/opencode"))
            .expect("home config path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".cache/opencode"))
            .expect("home cache path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".local/share/opencode"))
            .expect("home share path should exist before tmpfs mount");
        fs::create_dir_all(home.join(".local/state"))
            .expect("home state path should exist before tmpfs mount");

        service
            .start_workspace("alpha")
            .await
            .expect("workspace should start");

        let mut workspace_rx = service
            .manager
            .get_workspace("alpha")
            .expect("workspace should exist")
            .subscribe();
        let started_snapshot = workspace_rx.borrow().clone();
        let transient = started_snapshot
            .transient
            .clone()
            .expect("transient snapshot should be present after start");
        let _cleanup = UnitCleanup {
            unit: transient.unit.clone(),
        };

        wait_for_active_unit(&transient.unit)
            .await
            .expect("systemd unit should become active/activating");

        let exec_start = systemd_exec_start(&transient.unit)
            .await
            .expect("systemd unit should expose ExecStart");
        assert!(exec_start.contains("--ro-bind / /"));
        assert!(exec_start.contains("--tmpfs"));

        let workspace_path = workspace_directory.join("alpha");
        let workspace_path = workspace_path.to_string_lossy();
        assert!(exec_start.contains("--bind"));
        assert!(exec_start.contains(workspace_path.as_ref()));
        assert_eq!(exec_start.matches(" --bind ").count(), 1);

        let config_tmpfs = home.join(".config/opencode");
        let cache_tmpfs = home.join(".cache/opencode");
        let share_tmpfs = home.join(".local/share/opencode");
        let state_tmpfs = home.join(".local/state");
        assert!(exec_start.contains(config_tmpfs.to_string_lossy().as_ref()));
        assert!(exec_start.contains(cache_tmpfs.to_string_lossy().as_ref()));
        assert!(exec_start.contains(share_tmpfs.to_string_lossy().as_ref()));
        assert!(exec_start.contains(state_tmpfs.to_string_lossy().as_ref()));

        let health_body = match wait_for_health_response_json(&transient.uri).await {
            Ok(body) => body,
            Err(err) => {
                let diagnostics = systemd_unit_diagnostics(&transient.unit)
                    .await
                    .unwrap_or_else(|diag_err| format!("failed to capture diagnostics: {diag_err}"));
                panic!(
                    "backend health endpoint should respond: {err}\nunit diagnostics:\n{diagnostics}"
                );
            }
        };
        assert_eq!(
            health_body.get("healthy").and_then(Value::as_bool),
            Some(true),
            "health endpoint should report healthy=true"
        );

        let started_snapshot = match wait_for_workspace_started_state(&mut workspace_rx).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let diagnostics = systemd_unit_diagnostics(&transient.unit)
                    .await
                    .unwrap_or_else(|diag_err| format!("failed to capture diagnostics: {diag_err}"));
                panic!(
                    "workspace never transitioned to Started after healthy endpoint: {err}\nunit diagnostics:\n{diagnostics}"
                );
            }
        };
        assert!(started_snapshot.transient.is_some());
        assert!(
            started_snapshot.opencode_client.is_some(),
            "Started workspace should expose opencode client snapshot"
        );

        service
            .stop_workspace("alpha")
            .await
            .expect("workspace should stop");

        let stopped_snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = workspace_rx.borrow().clone();
                if snapshot.transient.is_none() && snapshot.opencode_client.is_none() {
                    return snapshot;
                }
                if workspace_rx.changed().await.is_err() {
                    panic!("workspace watch channel closed while waiting for stop state");
                }
            }
        })
        .await
        .expect("workspace should clear transient and client after stop");
        assert!(stopped_snapshot.transient.is_none());
        assert!(stopped_snapshot.opencode_client.is_none());
    });
}

#[test]
fn workspace_bwrap_sets_github_git_credentials_via_env_config() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let root = TestDir::new();
        let workspace_directory = root.path().join("workspaces");
        let home = root.path().join("home");
        let runtime_dir = root.path().join("runtime");
        let bin_dir = root.path().join("bin");
        let github_api_dir = root.path().join("github-api");
        let github_server = github_api_dir.join("server.py");
        let github_log = github_api_dir.join("requests.log");
        let github_port = reserve_local_port();

        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        fs::create_dir_all(&github_api_dir).expect("github api dir should exist");

        let fake_opencode = bin_dir.join("opencode");
        fs::write(
            &fake_opencode,
            r#"#!/bin/bash
set -euo pipefail
out_dir="$PWD/.multicode-test-output"
mkdir -p "$out_dir"
printf "%s" "${GIT_CONFIG_COUNT:-}" > "$out_dir/git-config-count.txt"
printf "%s" "${GIT_CONFIG_KEY_0:-}" > "$out_dir/git-config-key.txt"
printf "%s" "${GIT_CONFIG_VALUE_0:-}" > "$out_dir/git-config-value.txt"
printf "%s" "${MULTICODE_GITHUB_USERNAME:-}" > "$out_dir/github-username.txt"
printf "%s" "${MULTICODE_GITHUB_TOKEN:-}" > "$out_dir/github-token.txt"
printf "%s" "${PATH:-}" > "$out_dir/path.txt"
printf "%s" "${HOME:-}" > "$out_dir/home.txt"
printf "%s" "${XDG_RUNTIME_DIR:-}" > "$out_dir/runtime-dir.txt"
printf "%s" "${MULTICODE_SHOULD_NOT_INHERIT:-}" > "$out_dir/not-inherited.txt"
exec /bin/bash -lc 'trap : TERM INT; while true; do sleep 1; done'
"#,
        )
        .expect("fake opencode script should be written");
        let mut perms = fs::metadata(&fake_opencode)
            .expect("fake opencode metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_opencode, perms).expect("fake opencode should be executable");

        let github_server_body = format!(
            r#"from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

log_path = Path({log_path:?})

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        log_path.write_text(self.path + "\n")
        if self.path == "/user":
            body = b'{{"login":"sandbox-user","id":1,"node_id":"MDQ6VXNlcjE=","avatar_url":"https://example.com/avatar","gravatar_id":"","url":"https://api.github.com/users/sandbox-user","html_url":"https://github.com/sandbox-user","followers_url":"https://api.github.com/users/sandbox-user/followers","following_url":"https://api.github.com/users/sandbox-user/following{{/other_user}}","gists_url":"https://api.github.com/users/sandbox-user/gists{{/gist_id}}","starred_url":"https://api.github.com/users/sandbox-user/starred{{/owner}}{{/repo}}","subscriptions_url":"https://api.github.com/users/sandbox-user/subscriptions","organizations_url":"https://api.github.com/users/sandbox-user/orgs","repos_url":"https://api.github.com/users/sandbox-user/repos","events_url":"https://api.github.com/users/sandbox-user/events{{/privacy}}","received_events_url":"https://api.github.com/users/sandbox-user/received_events","type":"User","site_admin":false,"name":"Sandbox User","company":null,"blog":"","location":null,"email":null,"hireable":null,"bio":null,"twitter_username":null,"public_repos":0,"public_gists":0,"followers":0,"following":0,"created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","private_gists":0,"total_private_repos":0,"owned_private_repos":0,"disk_usage":0,"collaborators":0,"two_factor_authentication":false}}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self.send_response(404)
            self.end_headers()

    def log_message(self, format, *args):
        pass

HTTPServer(("127.0.0.1", {github_port}), Handler).serve_forever()
"#,
            log_path = github_log.display().to_string(),
            github_port = github_port,
        );
        fs::write(&github_server, github_server_body)
            .expect("github server script should be written");

        let mut github_process = Command::new("python3")
            .arg(&github_server)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("github api server should start");

        tokio::time::sleep(Duration::from_millis(250)).await;
        let probe = Command::new("python3")
            .args([
                "-c",
                &format!(
                    "import urllib.request; urllib.request.urlopen('http://127.0.0.1:{github_port}/user').read()"
                ),
            ])
            .output()
            .await
            .expect("probe should run");
        assert!(
            probe.status.success(),
            "github api server should become ready: stdout={} stderr={}",
            String::from_utf8_lossy(&probe.stdout),
            String::from_utf8_lossy(&probe.stderr)
        );

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);
        let _token_guard = EnvVarGuard::set("MULTICODE_GITHUB_TEST_TOKEN", "sandbox-token");
        let _not_inherited_guard =
            EnvVarGuard::set("MULTICODE_SHOULD_NOT_INHERIT", "host-only-secret");
        let _api_guard = EnvVarGuard::set("GITHUB_API_URL", format!("http://127.0.0.1:{github_port}"));

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{}"
opencode = ["opencode"]
create-ssh-agent = false

[github]
populate-git-credentials = true
token = {{ env = "MULTICODE_GITHUB_TEST_TOKEN" }}

[isolation]
inherit-env = ["PATH", "HOME", "XDG_RUNTIME_DIR"]
"#,
                workspace_directory.display()
            ),
        )
        .expect("config should be written");

        let service = CombinedService::from_config_path(&config_path)
            .await
            .expect("combined service should start");
        service
            .create_workspace("alpha")
            .await
            .expect("workspace should be created");
        service
            .start_workspace("alpha")
            .await
            .expect("workspace should start");

        let mut workspace_rx = service
            .manager
            .get_workspace("alpha")
            .expect("workspace should exist")
            .subscribe();
        let started_snapshot = workspace_rx.borrow().clone();
        let transient = started_snapshot
            .transient
            .clone()
            .expect("transient snapshot should be present after start");
        let _cleanup = UnitCleanup {
            unit: transient.unit.clone(),
        };

        wait_for_active_unit(&transient.unit)
            .await
            .expect("systemd unit should become active/activating");

        let exec_start = systemd_exec_start(&transient.unit)
            .await
            .expect("systemd unit should expose ExecStart");
        assert!(
            !exec_start.contains("--setenv"),
            "bwrap argv must not expose env through --setenv: {exec_start}"
        );
        assert!(
            !exec_start.contains("MULTICODE_GITHUB_TOKEN=sandbox-token"),
            "bwrap argv must not expose imported env values: {exec_start}"
        );
        assert!(
            !exec_start.contains("OPENCODE_SERVER_PASSWORD="),
            "bwrap argv must not expose opencode credentials in argv: {exec_start}"
        );
        assert!(
            !exec_start.contains(" env "),
            "bwrap argv must not use env wrapper anymore: {exec_start}"
        );

        let git_config_count = match wait_for_file_contents(workspace_directory.join("alpha/.multicode-test-output/git-config-count.txt")).await {
            Ok(value) => value,
            Err(err) => {
                let diagnostics = systemd_unit_diagnostics(&transient.unit)
                    .await
                    .unwrap_or_else(|diag_err| format!("failed to capture diagnostics: {diag_err}"));
                let exec_start = systemd_exec_start(&transient.unit)
                    .await
                    .unwrap_or_else(|| "<missing exec start>".to_string());
                panic!(
                    "sandbox should report git config count: {err}
exec_start:
{exec_start}
unit diagnostics:
{diagnostics}"
                );
            }
        };
        let git_config_key = match wait_for_file_contents(workspace_directory.join("alpha/.multicode-test-output/git-config-key.txt")).await {
            Ok(value) => value,
            Err(err) => {
                let diagnostics = systemd_unit_diagnostics(&transient.unit)
                    .await
                    .unwrap_or_else(|diag_err| format!("failed to capture diagnostics: {diag_err}"));
                let exec_start = systemd_exec_start(&transient.unit)
                    .await
                    .unwrap_or_else(|| "<missing exec start>".to_string());
                panic!(
                    "sandbox should report git config key: {err}
exec_start:
{exec_start}
unit diagnostics:
{diagnostics}"
                );
            }
        };
        let git_config_value = wait_for_file_contents(workspace_directory.join("alpha/.multicode-test-output/git-config-value.txt"))
            .await
            .expect("sandbox should report git config value");
        let github_username = wait_for_file_contents(workspace_directory.join("alpha/.multicode-test-output/github-username.txt"))
            .await
            .expect("sandbox should report GitHub username env");
        let github_token = wait_for_file_contents(workspace_directory.join("alpha/.multicode-test-output/github-token.txt"))
            .await
            .expect("sandbox should report GitHub token env");
        let inherited_path = wait_for_file_contents(workspace_directory.join("alpha/.multicode-test-output/path.txt"))
            .await
            .expect("sandbox should report inherited PATH env");
        let inherited_home = wait_for_file_contents(workspace_directory.join("alpha/.multicode-test-output/home.txt"))
            .await
            .expect("sandbox should report inherited HOME env");
        let inherited_runtime_dir = wait_for_file_contents(workspace_directory.join("alpha/.multicode-test-output/runtime-dir.txt"))
            .await
            .expect("sandbox should report inherited XDG_RUNTIME_DIR env");
        let not_inherited = tokio::fs::read_to_string(
            workspace_directory.join("alpha/.multicode-test-output/not-inherited.txt"),
        )
        .await
        .expect("sandbox should report non-inherited env marker")
        .trim()
        .to_string();
        assert_eq!(git_config_count, "1");
        assert_eq!(git_config_key, "credential.helper");
        assert_eq!(
            git_config_value,
            r#"!f() { test "$1" = get || exit 0; echo username=$MULTICODE_GITHUB_USERNAME; echo password=$MULTICODE_GITHUB_TOKEN; }; f"#
        );
        assert_eq!(github_username, "sandbox-user");
        assert_eq!(github_token, "sandbox-token");
        assert_eq!(inherited_path, test_path);
        assert_eq!(inherited_home, home.to_string_lossy());
        assert_eq!(inherited_runtime_dir, runtime_dir.to_string_lossy());
        assert_eq!(not_inherited, "");
        assert!(!home.join(".git-credentials").exists(), "host should not need .git-credentials file");

        service
            .stop_workspace("alpha")
            .await
            .expect("workspace should stop");
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = workspace_rx.borrow().clone();
                if snapshot.transient.is_none() {
                    break;
                }
                if workspace_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;

        let _ = github_process.start_kill();
        let _ = github_process.wait().await;
    });
}

#[test]
fn workspace_bwrap_preserves_readable_auth_json_file_mount() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let root = TestDir::new();
        let workspace_directory = root.path().join("workspaces");
        let home = root.path().join("home");
        let runtime_dir = root.path().join("runtime");
        let bin_dir = root.path().join("bin");
        let auth_dir = home.join(".local/share/opencode");
        let auth_file = auth_dir.join("auth.json");

        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        fs::create_dir_all(&auth_dir).expect("auth parent should exist");
        fs::write(&auth_file, r#"{"token":"sandbox"}"#).expect("auth file should be written");

        let fake_opencode = bin_dir.join("opencode");
        fs::write(
            &fake_opencode,
            r#"#!/bin/bash
set -euo pipefail
while [ ! -f "$HOME/.local/share/opencode/auth.json" ]; do
  sleep 0.1
done
out_dir="$PWD/.multicode-test-output"
mkdir -p "$out_dir"
printf "file" > "$out_dir/auth-kind.txt"
cat "$HOME/.local/share/opencode/auth.json" > "$out_dir/auth-contents.txt"
exec /bin/bash -lc 'trap : TERM INT; while true; do sleep 1; done'
"#,
        )
        .expect("fake opencode script should be written");
        let mut perms = fs::metadata(&fake_opencode)
            .expect("fake opencode metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_opencode, perms).expect("fake opencode should be executable");

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{}"
opencode = ["opencode"]

[isolation]
readable = ["~/.local/share/opencode/auth.json"]
isolated = ["~/.local/share/opencode"]
inherit-env = ["PATH", "HOME", "XDG_RUNTIME_DIR"]
"#,
                workspace_directory.display()
            ),
        )
        .expect("config should be written");

        let service = CombinedService::from_config_path(&config_path)
            .await
            .expect("combined service should start");
        service
            .create_workspace("alpha")
            .await
            .expect("workspace should be created");
        service
            .start_workspace("alpha")
            .await
            .expect("workspace should start with readable auth.json");

        let mut workspace_rx = service
            .manager
            .get_workspace("alpha")
            .expect("workspace should exist")
            .subscribe();
        let started_snapshot = workspace_rx.borrow().clone();
        let transient = started_snapshot
            .transient
            .clone()
            .expect("transient snapshot should be present after start");
        let _cleanup = UnitCleanup {
            unit: transient.unit.clone(),
        };

        wait_for_active_unit(&transient.unit)
            .await
            .expect("systemd unit should become active/activating");

        let exec_start = systemd_exec_start(&transient.unit)
            .await
            .unwrap_or_else(|| "<missing exec start>".to_string());
        let host_auth_source_str = auth_file.to_string_lossy().into_owned();
        assert!(
            exec_start.contains(&host_auth_source_str),
            "nested auth.json bind source should remain the host path: {exec_start}"
        );

        let auth_kind = match wait_for_file_contents(
            workspace_directory.join("alpha/.multicode-test-output/auth-kind.txt"),
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                let diagnostics = systemd_unit_diagnostics(&transient.unit)
                    .await
                    .unwrap_or_else(|diag_err| {
                        format!("failed to capture diagnostics: {diag_err}")
                    });
                let exec_start = systemd_exec_start(&transient.unit)
                    .await
                    .unwrap_or_else(|| "<missing exec start>".to_string());
                panic!(
                    "sandbox should report auth kind: {err}
exec_start:
{exec_start}
unit diagnostics:
{diagnostics}"
                );
            }
        };
        assert_eq!(auth_kind, "file");

        service
            .stop_workspace("alpha")
            .await
            .expect("workspace should stop");
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = workspace_rx.borrow().clone();
                if snapshot.transient.is_none() {
                    break;
                }
                if workspace_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    });
}

#[test]
fn workspace_bwrap_runs_gradle_with_fresh_home_from_config_toml() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let root = TestDir::new();
        let workspace_directory = root.path().join("workspaces");
        let home = root.path().join("home");
        let runtime_dir = root.path().join("runtime");
        let bin_dir = root.path().join("bin");
        let docker_home = root.path().join("docker-home");

        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        fs::create_dir_all(&docker_home).expect("docker home should exist");

        ensure_binary_available_for_test("docker", &["version"] as &[&str]);

        let gradle_image = format!("multicode-gradle-test:{}", std::process::id());
        let dockerfile = root.path().join("Dockerfile.gradle");
        fs::write(
            &dockerfile,
            r#"FROM gradle:8.14.3-jdk21
RUN mkdir -p /multicode-bin
CMD ["/bin/sh", "-lc", "gradle --version"]
"#,
        )
        .expect("dockerfile should be written");
        let docker_build = StdCommand::new("docker")
            .args(["build", "-t", &gradle_image, "-f"])
            .arg(&dockerfile)
            .arg(root.path())
            .status()
            .expect("docker build should run");
        assert!(docker_build.success(), "docker build should succeed");

        let gradle_wrapper = bin_dir.join("gradle");
        fs::write(
            &gradle_wrapper,
            format!(
                "#!/bin/sh\nexec docker run --rm -e HOME={} -v {}:{} -v {}:{} -w {} {} gradle \"$@\"\n",
                shell_escape_for_test(&docker_home),
                shell_escape_for_test(&docker_home),
                shell_escape_for_test(&docker_home),
                shell_escape_for_test(&home),
                shell_escape_for_test(&home),
                shell_escape_for_test(&home),
                gradle_image,
            ),
        )
        .expect("gradle wrapper should be written");
        let mut gradle_perms = fs::metadata(&gradle_wrapper)
            .expect("gradle wrapper metadata should exist")
            .permissions();
        gradle_perms.set_mode(0o755);
        fs::set_permissions(&gradle_wrapper, gradle_perms)
            .expect("gradle wrapper should be executable");

        let fake_opencode = bin_dir.join("opencode");
        fs::write(
            &fake_opencode,
            r#"#!/bin/bash
set -euo pipefail
out_dir="$PWD/.multicode-test-output"
mkdir -p "$out_dir"
if [ -d "$HOME/.gradle/.tmp" ]; then
  printf dir > "$out_dir/gradle-tmp-kind.txt"
elif [ -f "$HOME/.gradle/.tmp" ]; then
  printf file > "$out_dir/gradle-tmp-kind.txt"
else
  printf missing > "$out_dir/gradle-tmp-kind.txt"
fi
gradle --version > "$out_dir/gradle-version.txt"
printf "%s" "$HOME" > "$out_dir/home.txt"
if [ -d "$HOME/.gradle" ]; then
  printf dir > "$out_dir/gradle-home-kind.txt"
else
  printf missing > "$out_dir/gradle-home-kind.txt"
fi
exec /bin/bash -lc 'trap : TERM INT; while true; do sleep 1; done'
"#,
        )
        .expect("fake opencode script should be written");
        let mut perms = fs::metadata(&fake_opencode)
            .expect("fake opencode metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_opencode, perms).expect("fake opencode should be executable");

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{}"
opencode = ["opencode"]

[isolation]
writable = ["~/.gradle"]
isolated = ["~/.gradle/daemon", "~/.gradle/.tmp", "~/.gradle/kotlin-profile"]
inherit-env = ["PATH", "HOME", "XDG_RUNTIME_DIR"]
"#,
                workspace_directory.display()
            ),
        )
        .expect("config should be written");

        let service = CombinedService::from_config_path(&config_path)
            .await
            .expect("combined service should start");
        service
            .create_workspace("alpha")
            .await
            .expect("workspace should be created");
        service
            .start_workspace("alpha")
            .await
            .expect("workspace should start with gradle configured in isolation");

        let mut workspace_rx = service
            .manager
            .get_workspace("alpha")
            .expect("workspace should exist")
            .subscribe();
        let started_snapshot = workspace_rx.borrow().clone();
        let transient = started_snapshot
            .transient
            .clone()
            .expect("transient snapshot should be present after start");
        let _cleanup = UnitCleanup {
            unit: transient.unit.clone(),
        };

        wait_for_active_unit(&transient.unit)
            .await
            .expect("systemd unit should become active/activating");

        let gradle_version = match wait_for_file_contents(
            workspace_directory.join("alpha/.multicode-test-output/gradle-version.txt"),
        )
        .await
        {
            Ok(value) => value,
            Err(err) => {
                let diagnostics = systemd_unit_diagnostics(&transient.unit)
                    .await
                    .unwrap_or_else(|diag_err| {
                        format!("failed to capture diagnostics: {diag_err}")
                    });
                let exec_start = systemd_exec_start(&transient.unit)
                    .await
                    .unwrap_or_else(|| "<missing exec start>".to_string());
                panic!(
                    "sandbox should run gradle successfully: {err}\nexec_start:\n{exec_start}\nunit diagnostics:\n{diagnostics}"
                );
            }
        };
        let gradle_home_kind = wait_for_file_contents(
            workspace_directory.join("alpha/.multicode-test-output/gradle-home-kind.txt"),
        )
        .await
        .expect("sandbox should report gradle home kind");
        let gradle_tmp_kind = wait_for_file_contents(
            workspace_directory.join("alpha/.multicode-test-output/gradle-tmp-kind.txt"),
        )
        .await
        .expect("sandbox should report gradle tmp kind");
        let inherited_home = wait_for_file_contents(
            workspace_directory.join("alpha/.multicode-test-output/home.txt"),
        )
        .await
        .expect("sandbox should report inherited HOME env");

        assert!(
            gradle_version.contains("Gradle "),
            "gradle --version should succeed inside sandbox: {gradle_version}"
        );
        assert_eq!(gradle_home_kind, "dir");
        assert_eq!(gradle_tmp_kind, "dir");
        assert_eq!(inherited_home, home.to_string_lossy());

        service
            .stop_workspace("alpha")
            .await
            .expect("workspace should stop");
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = workspace_rx.borrow().clone();
                if snapshot.transient.is_none() {
                    break;
                }
                if workspace_rx.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    });
}

async fn wait_for_file_contents(path: PathBuf) -> Result<String, String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match tokio::fs::read_to_string(&path).await {
                Ok(contents) if !contents.is_empty() => return Ok(contents.trim().to_string()),
                Ok(_) | Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for {}", path.display()))?
}

fn reserve_local_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("ephemeral port bind should succeed")
        .local_addr()
        .expect("local addr should be available")
        .port()
}

async fn systemd_active_state(unit: &str) -> Option<String> {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
            "--property",
            "ActiveState",
            "--value",
        ])
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        tokio::time::sleep(Duration::from_millis(50)).await;
        None
    }
}

async fn wait_for_active_unit(unit: &str) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if matches!(
                systemd_active_state(unit).await.as_deref(),
                Some("active") | Some("activating")
            ) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for systemd unit {unit} to become active"))?
}

async fn systemd_exec_start(unit: &str) -> Option<String> {
    let output = Command::new("systemctl")
        .args(["--user", "show", unit, "--property", "ExecStart", "--value"])
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

async fn wait_for_health_response_json(base_uri: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|err| format!("failed to build reqwest client: {err}"))?;
    let url = format!("{}global/health", base_uri);

    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            match client.get(&url).send().await {
                Ok(response) if response.status().is_success() => {
                    match response.json::<Value>().await {
                        Ok(body) => return Ok(body),
                        Err(err) => {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            let _ = err;
                        }
                    }
                }
                Ok(_) | Err(_) => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for GET {url}"))?
}

async fn wait_for_workspace_started_state(
    workspace_rx: &mut tokio::sync::watch::Receiver<WorkspaceSnapshot>,
) -> Result<WorkspaceSnapshot, String> {
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let snapshot = workspace_rx.borrow().clone();
            if snapshot.transient.is_some() && snapshot.opencode_client.is_some() {
                return Ok(snapshot);
            }
            if workspace_rx.changed().await.is_err() {
                return Err("workspace watch channel closed while waiting for Started".to_string());
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for workspace state to become Started".to_string())?
}

fn shell_escape_for_test(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("'{}'", text.replace('\'', "'\\''"))
}

async fn ensure_opencode_cli_available() {
    match Command::new("opencode-cli").arg("--help").output().await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            panic!("opencode-cli must be installed and available in PATH for this integration test")
        }
        Err(err) => {
            panic!("failed to execute opencode-cli --help: {err}");
        }
    }
}

fn ensure_binary_available_for_test(name: &str, args: &[&str]) {
    let status = StdCommand::new(name)
        .args(args)
        .status()
        .unwrap_or_else(|err| panic!("{name} must be available for this integration test: {err}"));
    assert!(status.success() || status.code().is_some());
}

async fn systemd_unit_diagnostics(unit: &str) -> Result<String, String> {
    let status = Command::new("systemctl")
        .args(["--user", "status", unit, "--no-pager"])
        .output()
        .await
        .map_err(|err| format!("failed to run systemctl status: {err}"))?;
    let journal = Command::new("journalctl")
        .args(["--user", "-u", unit, "-n", "50", "--no-pager"])
        .output()
        .await
        .map_err(|err| format!("failed to run journalctl: {err}"))?;

    Ok(format!(
        "systemctl status:\n{}\n\njournalctl:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&journal.stdout)
    ))
}

struct UnitCleanup {
    unit: String,
}

impl Drop for UnitCleanup {
    fn drop(&mut self) {
        let _ = StdCommand::new("systemctl")
            .args(["--user", "stop", &self.unit])
            .status();
    }
}
