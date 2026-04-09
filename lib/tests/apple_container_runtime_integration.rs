use std::{
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use multicode_lib::{RuntimeBackend, services::CombinedService};

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
        let root = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .expect("current directory should be available")
                    .join("target")
                    .join("test-tmp")
            });
        let path = root.join(format!(
            "multicode-apple-container-integration-{}-{}",
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

fn make_executable(path: &Path) {
    let mut perms = fs::metadata(path)
        .expect("executable metadata should exist")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("permissions should be updated");
}

fn write_fake_container_cli(path: &Path) {
    fs::write(
        path,
        r#"#!/bin/bash
set -euo pipefail
root="${MULTICODE_FAKE_CONTAINER_ROOT:?missing MULTICODE_FAKE_CONTAINER_ROOT}"
state_dir="$root/state"
mkdir -p "$state_dir"
printf '%s\n' "$*" >> "$root/commands.log"

cmd="${1:-}"
shift || true
case "$cmd" in
  run)
    name=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --name)
          name="$2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    if [ -n "$name" ]; then
      : > "$state_dir/$name"
    fi
    ;;
  rm)
    if [ "${1:-}" = "-f" ] && [ -n "${2:-}" ]; then
      rm -f "$state_dir/$2"
    fi
    ;;
  inspect)
    if [ -n "${1:-}" ] && [ -e "$state_dir/$1" ]; then
      printf '[{\"status\":\"running\"}]\n'
      exit 0
    fi
    exit 1
    ;;
  list)
    for file in "$state_dir"/*; do
      [ -e "$file" ] || continue
      basename "$file"
    done
    ;;
  *)
    ;;
esac
"#,
    )
    .expect("fake container script should be written");
    make_executable(path);
}

fn write_fake_opencode(path: &Path) {
    fs::write(path, "#!/bin/bash\nexit 0\n").expect("fake opencode should be written");
    make_executable(path);
}

fn read_commands(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("commands log should be readable")
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

#[test]
fn starts_and_stops_workspace_with_apple_container_backend() {
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
        let fake_container_root = root.path().join("fake-container");
        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        fs::create_dir_all(&fake_container_root).expect("fake container root should exist");

        write_fake_container_cli(&bin_dir.join("container"));
        write_fake_opencode(&bin_dir.join("opencode"));

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _container_guard =
            EnvVarGuard::set("MULTICODE_CONTAINER_COMMAND", bin_dir.join("container"));
        let _port_guard = EnvVarGuard::set("MULTICODE_FIXED_PORT", "43123");
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);
        let _fake_root_guard =
            EnvVarGuard::set("MULTICODE_FAKE_CONTAINER_ROOT", &fake_container_root);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{workspace_directory}"
opencode = ["opencode"]

[runtime]
backend = "apple-container"
image = "ghcr.io/example/multicode-java25:latest"

[isolation]
writable = ["{home}/.gradle", "{home}/.config/gh"]
isolated = ["{home}/.local/share/opencode", "{home}/.local/state/opencode", "/var/tmp"]
tmpfs = ["/tmp"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR", "PATH"]
memory-max = "16 GiB"
cpu = "300%"
"#,
                workspace_directory = workspace_directory.display(),
                home = home.display(),
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

        let snapshot = service
            .manager
            .get_workspace("alpha")
            .expect("workspace should exist")
            .subscribe()
            .borrow()
            .clone();
        let transient = snapshot
            .transient
            .clone()
            .expect("transient snapshot should be present");
        assert_eq!(transient.runtime.backend, RuntimeBackend::AppleContainer);
        assert_eq!(transient.runtime.id, "multicode-alpha");
        assert!(transient.uri.starts_with("http://opencode:"));

        let commands = read_commands(&fake_container_root.join("commands.log"));
        let run_command = commands
            .iter()
            .find(|line| line.starts_with("run "))
            .expect("run command should be logged");
        assert!(run_command.contains("--name multicode-alpha"));
        assert!(run_command.contains("--cpus 3"));
        assert!(run_command.contains("--memory 17179869184"));
        assert!(run_command.contains("--tmpfs /tmp"));
        assert!(run_command.contains("ghcr.io/example/multicode-java25:latest"));
        assert!(run_command.contains("opencode serve --hostname 0.0.0.0"));

        let server_env = workspace_directory
            .join(".multicode")
            .join("apple-container")
            .join("alpha")
            .join("server.env");
        let env_contents =
            fs::read_to_string(&server_env).expect("server env file should be written");
        assert!(env_contents.contains("OPENCODE_SERVER_USERNAME=opencode"));
        assert!(env_contents.contains("OPENCODE_SERVER_PASSWORD="));
        assert!(env_contents.contains(&format!("HOME={}", home.display())));

        service
            .stop_workspace("alpha")
            .await
            .expect("workspace should stop");

        let stopped = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = service
                    .manager
                    .get_workspace("alpha")
                    .expect("workspace should exist")
                    .subscribe()
                    .borrow()
                    .clone();
                if snapshot.transient.is_none() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(stopped.is_ok(), "workspace should clear transient state");

        let commands = read_commands(&fake_container_root.join("commands.log"));
        assert!(
            commands.iter().any(|line| line == "rm -f multicode-alpha"),
            "stop should remove the container"
        );
    });
}

#[test]
fn start_workspace_removes_stale_named_container_before_run() {
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
        let fake_container_root = root.path().join("fake-container");
        let fake_state_dir = fake_container_root.join("state");
        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        fs::create_dir_all(&fake_state_dir).expect("fake container state dir should exist");

        write_fake_container_cli(&bin_dir.join("container"));
        write_fake_opencode(&bin_dir.join("opencode"));
        fs::write(fake_state_dir.join("multicode-alpha"), "")
            .expect("stale container should exist");

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _container_guard =
            EnvVarGuard::set("MULTICODE_CONTAINER_COMMAND", bin_dir.join("container"));
        let _port_guard = EnvVarGuard::set("MULTICODE_FIXED_PORT", "43123");
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);
        let _fake_root_guard =
            EnvVarGuard::set("MULTICODE_FAKE_CONTAINER_ROOT", &fake_container_root);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{workspace_directory}"
opencode = ["opencode"]

[runtime]
backend = "apple-container"
image = "ghcr.io/example/multicode-java25:latest"

[isolation]
inherit-env = ["HOME", "XDG_RUNTIME_DIR", "PATH"]
"#,
                workspace_directory = workspace_directory.display(),
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
            .expect("workspace should start after removing stale container");

        let commands = read_commands(&fake_container_root.join("commands.log"));
        let stale_rm_index = commands
            .iter()
            .position(|line| line == "rm -f multicode-alpha")
            .expect("stale container should be removed before start");
        let run_index = commands
            .iter()
            .position(|line| line.starts_with("run "))
            .expect("run command should be logged");
        assert!(
            stale_rm_index < run_index,
            "stale container removal should happen before run"
        );
    });
}

#[test]
fn build_exec_tool_command_uses_one_shot_apple_container_run() {
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
        fs::create_dir_all(workspace_directory.join("alpha")).expect("workspace root should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        write_fake_container_cli(&bin_dir.join("container"));
        write_fake_opencode(&bin_dir.join("opencode"));

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _container_guard =
            EnvVarGuard::set("MULTICODE_CONTAINER_COMMAND", bin_dir.join("container"));
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);
        let _fake_root_guard = EnvVarGuard::set(
            "MULTICODE_FAKE_CONTAINER_ROOT",
            root.path().join("fake-root"),
        );

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{workspace_directory}"
opencode = ["opencode"]

[runtime]
backend = "apple-container"
image = "ghcr.io/example/multicode-java25:latest"

[isolation]
inherit-env = ["HOME", "XDG_RUNTIME_DIR", "PATH"]
memory-max = "8 GiB"
cpu = "200%"
"#,
                workspace_directory = workspace_directory.display(),
            ),
        )
        .expect("config should be written");

        let service = CombinedService::from_config_path(&config_path)
            .await
            .expect("combined service should start");

        let command = service
            .build_exec_tool_command("alpha", "/bin/bash")
            .await
            .expect("exec tool command should build");
        assert_eq!(command.program, bin_dir.join("container").to_string_lossy());
        assert_eq!(command.inherited_env, Vec::<(String, String)>::new());
        assert!(
            command.args.windows(4).any(|window| {
                window
                    == ["run", "--rm", "--tty", "--interactive"]
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
            }),
            "apple backend should use one-shot container run for PTY tools"
        );
        assert!(command.args.iter().any(|arg| arg == "--cpus"));
        assert!(command.args.iter().any(|arg| arg == "2"));
        assert!(command.args.iter().any(|arg| arg == "--memory"));
        assert!(command.args.iter().any(|arg| arg == "8589934592"));
        assert!(command.args.iter().any(|arg| arg.ends_with("exec.env")));
        assert!(
            command
                .args
                .iter()
                .any(|arg| arg == "ghcr.io/example/multicode-java25:latest")
        );
        assert!(command.args.iter().any(|arg| arg == "/bin/bash"));
    });
}

#[test]
fn stale_apple_container_transient_is_cleared_on_startup() {
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
        let workspace_path = workspace_directory.join("alpha");
        let home = root.path().join("home");
        let runtime_dir = root.path().join("runtime");
        let transient_dir = root.path().join("transient-store");
        let bin_dir = root.path().join("bin");
        let fake_container_root = root.path().join("fake-container");
        let fake_state_dir = fake_container_root.join("state");
        fs::create_dir_all(&workspace_path).expect("workspace should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&transient_dir).expect("transient dir should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        fs::create_dir_all(&fake_state_dir).expect("fake container state dir should exist");

        write_fake_container_cli(&bin_dir.join("container"));
        write_fake_opencode(&bin_dir.join("opencode"));
        fs::write(fake_state_dir.join("multicode-alpha"), "")
            .expect("stale container should exist");

        let transient_link = workspace_directory.join(".multicode").join("transient");
        fs::create_dir_all(
            transient_link
                .parent()
                .expect("transient link parent should be available"),
        )
        .expect("transient link parent should exist");
        std::os::unix::fs::symlink(&transient_dir, &transient_link)
            .expect("transient link should be created");
        fs::write(
            transient_dir.join("alpha.json"),
            serde_json::to_vec_pretty(&multicode_lib::TransientWorkspaceSnapshot {
                uri: "http://opencode:secret@127.0.0.1:31337/".to_string(),
                runtime: multicode_lib::RuntimeHandleSnapshot {
                    backend: RuntimeBackend::AppleContainer,
                    id: "multicode-alpha".to_string(),
                    metadata: std::collections::BTreeMap::new(),
                },
            })
            .expect("transient snapshot should serialize"),
        )
        .expect("transient snapshot should be written");

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _container_guard =
            EnvVarGuard::set("MULTICODE_CONTAINER_COMMAND", bin_dir.join("container"));
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);
        let _fake_root_guard =
            EnvVarGuard::set("MULTICODE_FAKE_CONTAINER_ROOT", &fake_container_root);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{workspace_directory}"
opencode = ["opencode"]

[runtime]
backend = "apple-container"
image = "ghcr.io/example/multicode-java25:latest"

[isolation]
readable = ["{home}/.config/opencode"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR", "PATH"]
"#,
                workspace_directory = workspace_directory.display(),
                home = home.display(),
            ),
        )
        .expect("config should be written");

        let service = CombinedService::from_config_path(&config_path)
            .await
            .expect("combined service should start");

        let commands_log = fake_container_root.join("commands.log");
        let cleared = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = service
                    .manager
                    .get_workspace("alpha")
                    .expect("workspace should exist")
                    .subscribe()
                    .borrow()
                    .clone();
                let removed_stale_container = fs::read_to_string(&commands_log)
                    .map(|content| content.lines().any(|line| line == "rm -f multicode-alpha"))
                    .unwrap_or(false);
                if removed_stale_container && snapshot.transient.is_none() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(cleared.is_ok(), "stale transient should be cleared");

        let commands = read_commands(&commands_log);
        assert!(
            commands.iter().any(|line| line == "rm -f multicode-alpha"),
            "stale apple container should be removed during reconciliation"
        );
    });
}

#[test]
#[ignore = "requires a real Apple container image with opencode installed"]
fn real_apple_container_backend_starts_and_stops_with_supplied_image() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        let _env_lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let image = std::env::var("MULTICODE_APPLE_CONTAINER_TEST_IMAGE").expect(
            "set MULTICODE_APPLE_CONTAINER_TEST_IMAGE to a real image that contains opencode",
        );

        let root = TestDir::new();
        let workspace_directory = root.path().join("workspaces");
        let home = root.path().join("home");
        let runtime_dir = root.path().join("runtime");
        let bin_dir = root.path().join("bin");
        fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
        fs::create_dir_all(&home).expect("home should exist");
        fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        write_fake_opencode(&bin_dir.join("opencode"));

        let old_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{}", bin_dir.display(), old_path);
        let _path_guard = EnvVarGuard::set("PATH", &test_path);
        let _home_guard = EnvVarGuard::set("HOME", &home);
        let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"workspace-directory = "{workspace_directory}"
opencode = ["opencode"]

[runtime]
backend = "apple-container"
image = "{image}"

[isolation]
inherit-env = ["HOME", "XDG_RUNTIME_DIR", "PATH"]
memory-max = "4 GiB"
cpu = "100%"
"#,
                workspace_directory = workspace_directory.display(),
                image = image,
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
            .expect("workspace should start with real container backend");

        let snapshot = service
            .manager
            .get_workspace("alpha")
            .expect("workspace should exist")
            .subscribe()
            .borrow()
            .clone();
        let transient = snapshot
            .transient
            .clone()
            .expect("transient snapshot should be present");
        assert_eq!(transient.runtime.backend, RuntimeBackend::AppleContainer);

        service
            .stop_workspace("alpha")
            .await
            .expect("workspace should stop");
    });
}
