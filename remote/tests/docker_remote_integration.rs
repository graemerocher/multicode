#![cfg(target_os = "linux")]

use std::{
    fs,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command as StdCommand,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use multicode_remote::{CliArgs, RemoteCliDependencies, RemoteCliOptions, run_remote_cli};
use tokio::process::Command;

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
            "multicode-remote-docker-test-{}-{}",
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

struct DockerContainerGuard {
    name: String,
}

impl Drop for DockerContainerGuard {
    fn drop(&mut self) {
        let _ = StdCommand::new("docker")
            .args(["rm", "-f", &self.name])
            .status();
    }
}

fn ensure_command_available(name: &str, args: &[&str]) {
    let status = StdCommand::new(name)
        .args(args)
        .status()
        .unwrap_or_else(|err| panic!("{name} must be available for this integration test: {err}"));
    assert!(status.success() || status.code().is_some());
}

fn ensure_binary_exists(name: &str) {
    let status = StdCommand::new("sh")
        .args(["-lc", &format!("command -v {name} >/dev/null")])
        .status()
        .unwrap_or_else(|err| panic!("failed to check presence of {name}: {err}"));
    assert!(
        status.success(),
        "{name} must be available for this integration test"
    );
}

fn reserve_tcp_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("ephemeral port should bind")
        .local_addr()
        .expect("local addr should resolve")
        .port()
}

async fn wait_for_ssh(port: u16, key: &Path, known_hosts: &Path) {
    for _ in 0..60 {
        let status = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                &format!("UserKnownHostsFile={}", known_hosts.display()),
                "-i",
                &key.to_string_lossy(),
                "-p",
                &port.to_string(),
                "root@127.0.0.1",
                "true",
            ])
            .status()
            .await;
        if matches!(status, Ok(s) if s.success()) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("ssh server in docker container did not become ready");
}

#[test]
fn docker_remote_flow_syncs_and_launches_probe_binary() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        ensure_command_available("docker", &(["version"] as [&str; 1]));
        ensure_command_available("ssh", &(["-V"] as [&str; 1]));
        ensure_command_available("rsync", &(["--version"] as [&str; 1]));
        ensure_binary_exists("ssh-keygen");

        let root = TestDir::new();
        let local_workspace = root.path().join("local-workspace");
        let bidi_local = root.path().join("agent-work-local");
        let upload_only = root.path().join("upload-only");
        let bin_dir = root.path().join("bin");
        let relay_marker = root.path().join("relay-marker.txt");
        let relay_writer = root.path().join("write-relay-marker.sh");
        fs::create_dir_all(&local_workspace).expect("local workspace should exist");
        fs::create_dir_all(&bidi_local).expect("bidi local should exist");
        fs::create_dir_all(&upload_only).expect("upload-only local should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");

        fs::write(upload_only.join("upload.txt"), "upload-data").expect("upload file should exist");
        fs::write(bidi_local.join("seed.txt"), "seed-data").expect("seed file should exist");
        fs::write(
            &relay_writer,
            format!(
                "#!/bin/sh\nprintf '%s' \"$1\" > {}\n",
                relay_marker.display()
            ),
        )
        .expect("relay writer should be written");
        let mut relay_writer_perms = fs::metadata(&relay_writer)
            .expect("relay writer metadata should exist")
            .permissions();
        relay_writer_perms.set_mode(0o755);
        fs::set_permissions(&relay_writer, relay_writer_perms)
            .expect("relay writer permissions should be set");

        let launch_wrapper_log = bidi_local.join(".multicode/remote/launch-wrapper.log");
        let launch_stdout = bidi_local.join(".multicode/remote/launch.stdout");
        let launch_stderr = bidi_local.join(".multicode/remote/launch.stderr");
        let manual_wrapper_log = bidi_local.join("manual-wrapper.log");
        let synced_strace_log = bidi_local.join("strace.log");
        let before_exec = bidi_local.join("before-exec.txt");
        let tui_stdout = bidi_local.join("tui.stdout");
        let tui_stderr = bidi_local.join("tui.stderr");
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root should exist")
            .to_path_buf();
        let build_status = StdCommand::new("cargo")
            .args(["build", "-p", "multicode-tui"])
            .current_dir(&repo_root)
            .status()
            .expect("cargo build for multicode-tui should run");
        assert!(build_status.success(), "multicode-tui should build for integration test");
        let probe_binary = repo_root.join("target/debug/multicode-tui");
        assert!(probe_binary.exists(), "built multicode-tui binary should exist");

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "workspace-directory = \"~/agent-work\"\ncreate-ssh-agent = true\n\n[isolation]\n\n[handler]\nweb = \"{} {{}}\"\n\n[remote]\nforward-ssh-agent = true\n\n[remote.install]\ncommand = \"mkdir -p ~/agent-work && printf install-ran > ~/.install-marker\"\n\n[[remote.sync-up]]\nlocal = \"{}\"\nremote = \"~/upload-only\"\n\n[[remote.sync-bidi]]\nlocal = \"{}\"\nremote = \"~/agent-work\"\nexclude = [\".multicode/remote\"]\n",
                relay_writer.display(),
                upload_only.display(),
                bidi_local.display(),
            ),
        )
        .expect("config should be written");

        let key_path = root.path().join("id_ed25519");
        let pub_key_path = root.path().join("id_ed25519.pub");
        let known_hosts = root.path().join("known_hosts");
        let keygen = StdCommand::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .expect("ssh-keygen should run");
        assert!(keygen.success(), "ssh-keygen should succeed");
        let public_key = fs::read_to_string(&pub_key_path).expect("public key should exist");

        let dockerfile = root.path().join("Dockerfile");
        fs::write(
            &dockerfile,
            r#"FROM ubuntu:24.04
RUN apt-get update && apt-get install -y openssh-server rsync ca-certificates strace && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /var/run/sshd /root/.ssh && chmod 700 /root/.ssh
CMD ["/usr/sbin/sshd", "-D", "-e"]
"#,
        )
        .expect("dockerfile should be written");

        let image = format!("multicode-remote-test:{}", std::process::id());
        let build = StdCommand::new("docker")
            .args(["build", "-t", &image, "."])
            .current_dir(root.path())
            .status()
            .expect("docker build should run");
        assert!(build.success(), "docker build should succeed");

        let port = reserve_tcp_port();
        let container_name = format!("multicode-remote-test-{}", std::process::id());
        let run = StdCommand::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &container_name,
                "-p",
                &format!("127.0.0.1:{port}:22"),
                "-e",
                &format!("AUTHORIZED_KEY={}", public_key.trim()),
                &image,
                "/bin/sh",
                "-lc",
                "mkdir -p /root/.ssh && chmod 700 /root/.ssh && printf '%s\n' \"$AUTHORIZED_KEY\" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys && exec /usr/sbin/sshd -D -e",
            ])
            .status()
            .expect("docker run should execute");
        assert!(run.success(), "docker run should succeed");
        let _container = DockerContainerGuard { name: container_name.clone() };

        wait_for_ssh(port, &key_path, &known_hosts).await;

        let args = CliArgs {
            config_path: config_path.clone(),
            ssh_uri: "root@127.0.0.1".to_string(),
        };
        let options = RemoteCliOptions {
            ssh_port: Some(port),
            ssh_identity_file: Some(key_path.clone()),
            ssh_known_hosts_file: Some(known_hosts.clone()),
            ssh_strict_host_key_checking: false,
            remote_tui_sanity_check: true,
        };
        let deps = RemoteCliDependencies {
            local_tui_binary_override: Some(probe_binary.clone()),
            local_tui_stage_root_override: None,
        };

        let result = match run_remote_cli(args, options, deps).await {
            Ok(result) => result,
            Err(err) => {
                let remote_dir_probe = Command::new("ssh")
                    .args([
                        "-o",
                        "StrictHostKeyChecking=no",
                        "-o",
                        &format!("UserKnownHostsFile={}", known_hosts.display()),
                        "-i",
                        &key_path.to_string_lossy(),
                        "-p",
                        &port.to_string(),
                        "root@127.0.0.1",
                        "sh",
                        "-lc",
                        "pwd; echo '---'; ls -lad /root /root/agent-work /root/agent-work/.multicode /root/agent-work/.multicode/remote || true",
                    ])
                    .output()
                    .await
                    .expect("remote directory probe should run");
                eprintln!(
                    "remote directory probe stdout:\n{}\nremote directory probe stderr:\n{}",
                    String::from_utf8_lossy(&remote_dir_probe.stdout),
                    String::from_utf8_lossy(&remote_dir_probe.stderr)
                );
                if synced_strace_log.exists() {
                    let trace = fs::read_to_string(&synced_strace_log).unwrap_or_default();
                    let tail = trace
                        .lines()
                        .rev()
                        .take(120)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n");
                    eprintln!("synced strace log tail:\n{}", tail);
                }
                let remote_strace = Command::new("ssh")
                    .args([
                        "-o",
                        "StrictHostKeyChecking=no",
                        "-o",
                        &format!("UserKnownHostsFile={}", known_hosts.display()),
                        "-i",
                        &key_path.to_string_lossy(),
                        "-p",
                        &port.to_string(),
                        "root@127.0.0.1",
                        "sh",
                        "-lc",
                        "if [ -f /root/agent-work/strace.log ]; then tail -n 120 /root/agent-work/strace.log; fi",
                    ])
                    .output()
                    .await
                    .expect("remote strace fetch should run");
                eprintln!(
                    "remote strace tail via ssh:\n{}",
                    String::from_utf8_lossy(&remote_strace.stdout)
                );
                for (label, path) in [
                    ("launch-wrapper.log", &launch_wrapper_log),
                    ("launch.stdout", &launch_stdout),
                    ("launch.stderr", &launch_stderr),
                    ("manual-wrapper.log", &manual_wrapper_log),
                    ("before-exec.txt", &before_exec),
                    ("tui.stdout", &tui_stdout),
                    ("tui.stderr", &tui_stderr),
                ] {
                    if path.exists() {
                        eprintln!("{}:\n{}", label, fs::read_to_string(path).unwrap_or_default());
                    } else {
                        eprintln!("{}: <missing>", label);
                    }
                }
                panic!("remote cli flow should succeed: {err:?}");
            }
        };

        let install_marker = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                &format!("UserKnownHostsFile={}", known_hosts.display()),
                "-i",
                &key_path.to_string_lossy(),
                "-p",
                &port.to_string(),
                "root@127.0.0.1",
                "cat ~/.install-marker",
            ])
            .output()
            .await
            .expect("ssh cat install marker should run");
        assert!(install_marker.status.success());
        assert_eq!(String::from_utf8_lossy(&install_marker.stdout).trim(), "install-ran");

        for _ in 0..20 {
            if relay_marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(relay_marker.exists(), "relay handler marker should be written on the host side");
        assert_eq!(
            fs::read_to_string(&relay_marker).expect("relay marker should be readable"),
            "https://relay.example/test"
        );

        let remote_upload = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                &format!("UserKnownHostsFile={}", known_hosts.display()),
                "-i",
                &key_path.to_string_lossy(),
                "-p",
                &port.to_string(),
                "root@127.0.0.1",
                "cat /root/upload-only/upload.txt",
            ])
            .output()
            .await
            .expect("ssh cat upload file should run");
        assert!(remote_upload.status.success());
        assert_eq!(String::from_utf8_lossy(&remote_upload.stdout).trim(), "upload-data");

        let no_literal_tilde_dir = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                &format!("UserKnownHostsFile={}", known_hosts.display()),
                "-i",
                &key_path.to_string_lossy(),
                "-p",
                &port.to_string(),
                "root@127.0.0.1",
                "test ! -e '/root/~'",
            ])
            .status()
            .await
            .expect("ssh literal tilde directory probe should run");
        assert!(
            no_literal_tilde_dir.success(),
            "remote sync should not create a literal '~' directory under /root"
        );

        assert_eq!(
            result.remote_tui_path,
            PathBuf::from("/root/agent-work/.multicode/remote/multicode-tui")
        );
        assert_eq!(
            result.remote_config_path,
            PathBuf::from("/root/agent-work/.multicode/remote/config.toml")
        );
    });
}

#[test]
fn docker_remote_flow_skips_bidi_upload_when_remote_tree_is_newer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        ensure_command_available("docker", &(["version"] as [&str; 1]));
        ensure_command_available("ssh", &(["-V"] as [&str; 1]));
        ensure_command_available("rsync", &(["--version"] as [&str; 1]));
        ensure_binary_exists("ssh-keygen");

        let root = TestDir::new();
        let bidi_local = root.path().join("agent-work-local");
        let bin_dir = root.path().join("bin");
        fs::create_dir_all(&bidi_local).expect("bidi local should exist");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        let shared_file = bidi_local.join("shared.txt");
        fs::write(&shared_file, "local-v1").expect("local seed file should exist");

        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root should exist")
            .to_path_buf();
        let build_status = StdCommand::new("cargo")
            .args(["build", "-p", "multicode-tui"])
            .current_dir(&repo_root)
            .status()
            .expect("cargo build for multicode-tui should run");
        assert!(build_status.success(), "multicode-tui should build for integration test");
        let probe_binary = repo_root.join("target/debug/multicode-tui");
        assert!(probe_binary.exists(), "built multicode-tui binary should exist");

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "workspace-directory = \"~/agent-work\"\ncreate-ssh-agent = true\n\n[isolation]\n\n[handler]\nweb = \"/bin/true {{}}\"\n\n[remote]\nforward-ssh-agent = true\n\n[remote.install]\ncommand = \"mkdir -p ~/agent-work && printf install-ran > ~/.install-marker\"\n\n[[remote.sync-bidi]]\nlocal = \"{}\"\nremote = \"~/agent-work\"\nexclude = [\".multicode/remote\"]\n",
                bidi_local.display(),
            ),
        )
        .expect("config should be written");

        let key_path = root.path().join("id_ed25519");
        let pub_key_path = root.path().join("id_ed25519.pub");
        let known_hosts = root.path().join("known_hosts");
        let keygen = StdCommand::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .expect("ssh-keygen should run");
        assert!(keygen.success(), "ssh-keygen should succeed");
        let public_key = fs::read_to_string(&pub_key_path).expect("public key should exist");

        let dockerfile = root.path().join("Dockerfile");
        fs::write(
            &dockerfile,
            r#"FROM ubuntu:24.04
RUN apt-get update && apt-get install -y openssh-server rsync ca-certificates && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /var/run/sshd /root/.ssh && chmod 700 /root/.ssh
CMD ["/usr/sbin/sshd", "-D", "-e"]
"#,
        )
        .expect("dockerfile should be written");

        let image = format!("multicode-remote-test-bidi-newer:{}", std::process::id());
        let build = StdCommand::new("docker")
            .args(["build", "-t", &image, "."])
            .current_dir(root.path())
            .status()
            .expect("docker build should run");
        assert!(build.success(), "docker build should succeed");

        let port = reserve_tcp_port();
        let container_name = format!("multicode-remote-test-bidi-newer-{}", std::process::id());
        let run = StdCommand::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &container_name,
                "-p",
                &format!("127.0.0.1:{port}:22"),
                "-e",
                &format!("AUTHORIZED_KEY={}", public_key.trim()),
                &image,
                "/bin/sh",
                "-lc",
                "mkdir -p /root/.ssh && chmod 700 /root/.ssh && printf '%s\n' \"$AUTHORIZED_KEY\" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys && exec /usr/sbin/sshd -D -e",
            ])
            .status()
            .expect("docker run should execute");
        assert!(run.success(), "docker run should succeed");
        let _container = DockerContainerGuard { name: container_name.clone() };

        wait_for_ssh(port, &key_path, &known_hosts).await;

        let args = CliArgs {
            config_path: config_path.clone(),
            ssh_uri: "root@127.0.0.1".to_string(),
        };
        let options = RemoteCliOptions {
            ssh_port: Some(port),
            ssh_identity_file: Some(key_path.clone()),
            ssh_known_hosts_file: Some(known_hosts.clone()),
            ssh_strict_host_key_checking: false,
            remote_tui_sanity_check: true,
        };
        let deps = RemoteCliDependencies {
            local_tui_binary_override: Some(probe_binary.clone()),
            local_tui_stage_root_override: None,
        };

        run_remote_cli(args.clone(), options.clone(), deps.clone())
            .await
            .expect("initial remote cli flow should succeed");

        tokio::time::sleep(Duration::from_secs(2)).await;
        let remote_update = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                &format!("UserKnownHostsFile={}", known_hosts.display()),
                "-i",
                &key_path.to_string_lossy(),
                "-p",
                &port.to_string(),
                "root@127.0.0.1",
                "rm -f /root/agent-work/shared.txt && printf remote-v2 > /root/agent-work/remote-only.txt",
            ])
            .status()
            .await
            .expect("remote update should run");
        assert!(remote_update.success(), "remote update should succeed");

        run_remote_cli(args, options, deps)
            .await
            .expect("second remote cli flow should succeed");

        let remote_probe = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                &format!("UserKnownHostsFile={}", known_hosts.display()),
                "-i",
                &key_path.to_string_lossy(),
                "-p",
                &port.to_string(),
                "root@127.0.0.1",
                "test ! -e /root/agent-work/shared.txt && cat /root/agent-work/remote-only.txt",
            ])
            .output()
            .await
            .expect("remote probe should run");
        assert!(remote_probe.status.success(), "remote tree should keep newer remote state");
        assert_eq!(String::from_utf8_lossy(&remote_probe.stdout).trim(), "remote-v2");

        assert!(
            !shared_file.exists(),
            "final sync-down should propagate the remote deletion locally"
        );
        assert_eq!(
            fs::read_to_string(bidi_local.join("remote-only.txt"))
                .expect("remote-only file should sync down locally"),
            "remote-v2"
        );
    });
}

#[test]
fn docker_remote_flow_forwards_multicode_managed_ssh_agent() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        ensure_command_available("docker", &(["version"] as [&str; 1]));
        ensure_command_available("ssh", &(["-V"] as [&str; 1]));
        ensure_command_available("rsync", &(["--version"] as [&str; 1]));
        ensure_binary_exists("ssh-keygen");
        ensure_binary_exists("ssh-add");

        let root = TestDir::new();
        let upload_only = root.path().join("upload-only");
        let relay_marker = root.path().join("relay-marker.txt");
        let relay_writer = root.path().join("write-relay-marker.sh");
        fs::create_dir_all(&upload_only).expect("upload-only local should exist");
        fs::write(upload_only.join("upload.txt"), "upload-data").expect("upload file should exist");
        fs::write(
            &relay_writer,
            format!(
                "#!/bin/sh\nprintf '%s' \"$1\" > {}\n",
                relay_marker.display()
            ),
        )
        .expect("relay writer should be written");
        let mut relay_writer_perms = fs::metadata(&relay_writer)
            .expect("relay writer metadata should exist")
            .permissions();
        relay_writer_perms.set_mode(0o755);
        fs::set_permissions(&relay_writer, relay_writer_perms)
            .expect("relay writer permissions should be set");

        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root should exist")
            .to_path_buf();
        let build_status = StdCommand::new("cargo")
            .args(["build", "-p", "multicode-tui"])
            .current_dir(&repo_root)
            .status()
            .expect("cargo build for multicode-tui should run");
        assert!(build_status.success(), "multicode-tui should build for integration test");
        let probe_binary = repo_root.join("target/debug/multicode-tui");
        assert!(probe_binary.exists(), "built multicode-tui binary should exist");

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "workspace-directory = \"~/agent-work\"\ncreate-ssh-agent = true\n\n[isolation]\n\n[handler]\nweb = \"{} {{}}\"\n\n[remote]\nforward-ssh-agent = true\n\n[remote.install]\ncommand = \"mkdir -p ~/agent-work && printf install-ran > ~/.install-marker\"\n\n[[remote.sync-up]]\nlocal = \"{}\"\nremote = \"~/upload-only\"\n",
                relay_writer.display(),
                upload_only.display(),
            ),
        )
        .expect("config should be written");

        let key_path = root.path().join("id_ed25519");
        let pub_key_path = root.path().join("id_ed25519.pub");
        let known_hosts = root.path().join("known_hosts");
        let keygen = StdCommand::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .expect("ssh-keygen should run");
        assert!(keygen.success(), "ssh-keygen should succeed");
        let public_key = fs::read_to_string(&pub_key_path).expect("public key should exist");

        let dockerfile = root.path().join("Dockerfile");
        fs::write(
            &dockerfile,
            r#"FROM ubuntu:24.04
RUN apt-get update && apt-get install -y openssh-server rsync ca-certificates strace && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /var/run/sshd /root/.ssh && chmod 700 /root/.ssh
CMD ["/usr/sbin/sshd", "-D", "-e"]
"#,
        )
        .expect("dockerfile should be written");

        let image = format!("multicode-remote-test-agent:{}", std::process::id());
        let build = StdCommand::new("docker")
            .args(["build", "-t", &image, "."])
            .current_dir(root.path())
            .status()
            .expect("docker build should run");
        assert!(build.success(), "docker build should succeed");

        let port = reserve_tcp_port();
        let container_name = format!("multicode-remote-test-agent-{}", std::process::id());
        let run = StdCommand::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &container_name,
                "-p",
                &format!("127.0.0.1:{port}:22"),
                "-e",
                &format!("AUTHORIZED_KEY={}", public_key.trim()),
                &image,
                "/bin/sh",
                "-lc",
                "mkdir -p /root/.ssh && chmod 700 /root/.ssh && printf '%s\n' \"$AUTHORIZED_KEY\" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys && exec /usr/sbin/sshd -D -e",
            ])
            .status()
            .expect("docker run should execute");
        assert!(run.success(), "docker run should succeed");
        let _container = DockerContainerGuard { name: container_name.clone() };

        wait_for_ssh(port, &key_path, &known_hosts).await;

        let args = CliArgs {
            config_path: config_path.clone(),
            ssh_uri: "root@127.0.0.1".to_string(),
        };
        let options = RemoteCliOptions {
            ssh_port: Some(port),
            ssh_identity_file: Some(key_path.clone()),
            ssh_known_hosts_file: Some(known_hosts.clone()),
            ssh_strict_host_key_checking: false,
            remote_tui_sanity_check: true,
        };
        let deps = RemoteCliDependencies {
            local_tui_binary_override: Some(probe_binary.clone()),
            local_tui_stage_root_override: None,
        };

        let result = run_remote_cli(args, options, deps)
            .await
            .expect("remote cli flow should succeed");

        let remote_runtime_dir = result.remote_root;
        let remote_debug = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                &format!("UserKnownHostsFile={}", known_hosts.display()),
                "-i",
                &key_path.to_string_lossy(),
                "-p",
                &port.to_string(),
                "root@127.0.0.1",
                "sh",
                "-lc",
                &format!(
                    "pwd; echo '---'; ls -la {} || true; echo '---'; for f in {}/launch-wrapper.log {}/launch.stdout {}/launch.stderr {}/relay-sanity-ssh-auth-sock.txt {}/relay-sanity-ssh-add-status.txt {}/relay-sanity-ssh-add-output.txt; do echo \"FILE:$f\"; [ -f \"$f\" ] && cat \"$f\" || echo '<missing>'; echo '---'; done",
                    remote_runtime_dir.display(),
                    remote_runtime_dir.display(),
                    remote_runtime_dir.display(),
                    remote_runtime_dir.display(),
                    remote_runtime_dir.display(),
                    remote_runtime_dir.display(),
                    remote_runtime_dir.display(),
                ),
            ])
            .output()
            .await
            .expect("remote debug probe should run");
        let remote_debug_stdout = String::from_utf8_lossy(&remote_debug.stdout).to_string();
        eprintln!(
            "remote debug stdout:\n{}\nremote debug stderr:\n{}",
            remote_debug_stdout,
            String::from_utf8_lossy(&remote_debug.stderr)
        );

        assert!(
            remote_debug_stdout.contains("FILE:/root/agent-work/.multicode/remote/relay-sanity-ssh-auth-sock.txt\n/root/agent-work/.multicode/remote/ssh-agent/")
                || remote_debug_stdout.contains("FILE:/root/agent-work/.multicode/remote/relay-sanity-ssh-auth-sock.txt\n/tmp/ssh-"),
            "forwarded ssh auth sock should be set for remote multicode-tui: {remote_debug_stdout}"
        );
        assert!(
            remote_debug_stdout.contains("FILE:/root/agent-work/.multicode/remote/relay-sanity-ssh-add-status.txt\nexit status: 0"),
            "ssh-add -l should succeed through forwarded agent: {remote_debug_stdout}"
        );
        assert!(
            remote_debug_stdout.contains("SHA256:") || remote_debug_stdout.contains("The agent has no identities"),
            "ssh-add output should reflect reachable forwarded agent: {remote_debug_stdout}"
        );

        for _ in 0..20 {
            if relay_marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(relay_marker.exists(), "relay handler marker should be written on the host side");
    });
}

#[test]
fn docker_remote_flow_syncs_dangling_transient_symlink() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should build");

    runtime.block_on(async {
        ensure_command_available("docker", &(["version"] as [&str; 1]));
        ensure_command_available("ssh", &(["-V"] as [&str; 1]));
        ensure_command_available("rsync", &(["--version"] as [&str; 1]));
        ensure_binary_exists("ssh-keygen");

        let root = TestDir::new();
        let upload_only = root.path().join("upload-only");
        fs::create_dir_all(&upload_only).expect("upload-only local should exist");
        fs::write(upload_only.join("upload.txt"), "upload-data").expect("upload file should exist");

        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repo root should exist")
            .to_path_buf();
        let build_status = StdCommand::new("cargo")
            .args(["build", "-p", "multicode-tui"])
            .current_dir(&repo_root)
            .status()
            .expect("cargo build for multicode-tui should run");
        assert!(build_status.success(), "multicode-tui should build for integration test");
        let probe_binary = repo_root.join("target/debug/multicode-tui");
        assert!(probe_binary.exists(), "built multicode-tui binary should exist");

        let config_path = root.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "workspace-directory = \"~/agent-work\"\ncreate-ssh-agent = true\n\n[isolation]\n\n[handler]\nweb = \"/bin/true {{}}\"\n\n[remote]\nforward-ssh-agent = true\n\n[remote.install]\ncommand = \"mkdir -p ~/agent-work && printf install-ran > ~/.install-marker\"\n\n[[remote.sync-up]]\nlocal = \"{}\"\nremote = \"~/upload-only\"\n",
                upload_only.display(),
            ),
        )
        .expect("config should be written");

        let key_path = root.path().join("id_ed25519");
        let pub_key_path = root.path().join("id_ed25519.pub");
        let known_hosts = root.path().join("known_hosts");
        let keygen = StdCommand::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .expect("ssh-keygen should run");
        assert!(keygen.success(), "ssh-keygen should succeed");
        let public_key = fs::read_to_string(&pub_key_path).expect("public key should exist");

        let dockerfile = root.path().join("Dockerfile");
        fs::write(
            &dockerfile,
            r#"FROM ubuntu:24.04
RUN apt-get update && apt-get install -y openssh-server rsync ca-certificates strace && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /var/run/sshd /root/.ssh && chmod 700 /root/.ssh
CMD ["/usr/sbin/sshd", "-D", "-e"]
"#,
        )
        .expect("dockerfile should be written");

        let image = format!("multicode-remote-test-dangling-symlink:{}", std::process::id());
        let build = StdCommand::new("docker")
            .args(["build", "-t", &image, "."])
            .current_dir(root.path())
            .status()
            .expect("docker build should run");
        assert!(build.success(), "docker build should succeed");

        let port = reserve_tcp_port();
        let container_name = format!("multicode-remote-test-dangling-symlink-{}", std::process::id());
        let run = StdCommand::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &container_name,
                "-p",
                &format!("127.0.0.1:{port}:22"),
                "-e",
                &format!("AUTHORIZED_KEY={}", public_key.trim()),
                &image,
                "/bin/sh",
                "-lc",
                "mkdir -p /root/.ssh && chmod 700 /root/.ssh && printf '%s\n' \"$AUTHORIZED_KEY\" > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys && exec /usr/sbin/sshd -D -e",
            ])
            .status()
            .expect("docker run should execute");
        assert!(run.success(), "docker run should succeed");
        let _container = DockerContainerGuard { name: container_name.clone() };

        wait_for_ssh(port, &key_path, &known_hosts).await;

        let args = CliArgs {
            config_path: config_path.clone(),
            ssh_uri: "root@127.0.0.1".to_string(),
        };
        let options = RemoteCliOptions {
            ssh_port: Some(port),
            ssh_identity_file: Some(key_path.clone()),
            ssh_known_hosts_file: Some(known_hosts.clone()),
            ssh_strict_host_key_checking: false,
            remote_tui_sanity_check: true,
        };
        let deps = RemoteCliDependencies {
            local_tui_binary_override: Some(probe_binary.clone()),
            local_tui_stage_root_override: None,
        };

        let result = run_remote_cli(args, options, deps)
            .await
            .expect("remote cli flow should succeed when rsync preserves the dangling symlink");

        assert_eq!(
            result.remote_tui_path,
            PathBuf::from("/root/agent-work/.multicode/remote/multicode-tui")
        );
    });
}
