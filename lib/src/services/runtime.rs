use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::OnceLock,
};

use tokio::{process::Command, sync::Mutex};
use uuid::Uuid;

use super::{
    combined::{CombinedServiceError, SpawnCommand},
    config::{ExpandedIsolationConfig, RuntimeConfig, path_looks_like_file},
};
use crate::{RuntimeBackend, RuntimeHandleSnapshot, TransientWorkspaceSnapshot};

pub(super) const RUNTIME_SPEC_METADATA_KEY: &str = "runtime-spec";
const APPLE_GITCONFIG_DIR: &str = "/multicode-host/git";
const APPLE_GITCONFIG_FILE_NAME: &str = ".gitconfig";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeActivity {
    Active,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeUsageState {
    Active,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct RuntimeUsageSample {
    pub(super) memory_current: Option<u64>,
    pub(super) cpu_usage_nsec: Option<u64>,
    pub(super) state: Option<RuntimeUsageState>,
}

#[derive(Debug, Clone)]
pub(super) struct RuntimeStartResult {
    pub(super) transient: TransientWorkspaceSnapshot,
}

#[derive(Debug, Clone)]
struct RuntimeContext {
    runtime: RuntimeConfig,
    workspace_directory_path: PathBuf,
    expanded_isolation: ExpandedIsolationConfig,
    host_opencode_command: String,
    container_opencode_command: String,
}

#[derive(Debug, Clone)]
pub(super) enum WorkspaceRuntime {
    Linux(LinuxSystemdBwrapRuntime),
    AppleContainer(AppleContainerRuntime),
}

impl WorkspaceRuntime {
    pub(super) fn new(
        runtime: RuntimeConfig,
        workspace_directory_path: PathBuf,
        expanded_isolation: ExpandedIsolationConfig,
        host_opencode_command: String,
        container_opencode_command: String,
    ) -> Self {
        let context = RuntimeContext {
            runtime: runtime.clone(),
            workspace_directory_path,
            expanded_isolation,
            host_opencode_command,
            container_opencode_command,
        };
        match runtime.backend {
            RuntimeBackend::LinuxSystemdBwrap => Self::Linux(LinuxSystemdBwrapRuntime { context }),
            RuntimeBackend::AppleContainer => {
                Self::AppleContainer(AppleContainerRuntime { context })
            }
        }
    }

    pub(super) async fn start_server(
        &self,
        key: &str,
        inherited_env: &[(String, String)],
    ) -> Result<RuntimeStartResult, CombinedServiceError> {
        match self {
            Self::Linux(runtime) => runtime.start_server(key, inherited_env).await,
            Self::AppleContainer(runtime) => runtime.start_server(key, inherited_env).await,
        }
    }

    pub(super) async fn stop_server(
        &self,
        runtime_handle: &RuntimeHandleSnapshot,
    ) -> Result<(), CombinedServiceError> {
        match runtime_handle.backend {
            RuntimeBackend::LinuxSystemdBwrap => {
                LinuxSystemdBwrapRuntime::stop_server(runtime_handle).await
            }
            RuntimeBackend::AppleContainer => {
                AppleContainerRuntime::stop_server(runtime_handle).await
            }
        }
    }

    pub(super) async fn build_pty_command(
        &self,
        key: &str,
        runtime_handle: Option<&RuntimeHandleSnapshot>,
        inherited_env: &[(String, String)],
        command: Vec<String>,
    ) -> Result<SpawnCommand, CombinedServiceError> {
        match self {
            Self::Linux(runtime) => {
                runtime
                    .build_pty_command(key, runtime_handle, inherited_env, command)
                    .await
            }
            Self::AppleContainer(runtime) => {
                runtime
                    .build_pty_command(key, runtime_handle, inherited_env, command)
                    .await
            }
        }
    }

    pub(super) async fn build_linux_start_command(
        &self,
        key: &str,
        password: &str,
        port: u16,
        unit: &str,
        inherited_env: &[(String, String)],
    ) -> Result<SpawnCommand, CombinedServiceError> {
        match self {
            Self::Linux(runtime) => {
                runtime
                    .build_systemd_bwrap_command(key, password, port, unit, inherited_env)
                    .await
            }
            Self::AppleContainer(_) => Err(CombinedServiceError::UnsupportedRuntimeBackend(
                "build_systemd_bwrap_command is only available for the linux-systemd-bwrap backend"
                    .to_string(),
            )),
        }
    }

    pub(super) async fn read_activity(runtime_handle: &RuntimeHandleSnapshot) -> RuntimeActivity {
        match runtime_handle.backend {
            RuntimeBackend::LinuxSystemdBwrap => {
                LinuxSystemdBwrapRuntime::read_activity(runtime_handle).await
            }
            RuntimeBackend::AppleContainer => {
                AppleContainerRuntime::read_activity(runtime_handle).await
            }
        }
    }

    pub(super) async fn read_usage(runtime_handle: &RuntimeHandleSnapshot) -> RuntimeUsageSample {
        match runtime_handle.backend {
            RuntimeBackend::LinuxSystemdBwrap => {
                LinuxSystemdBwrapRuntime::read_usage(runtime_handle).await
            }
            RuntimeBackend::AppleContainer => {
                AppleContainerRuntime::read_usage(runtime_handle).await
            }
        }
    }

    pub(super) fn backend(&self) -> RuntimeBackend {
        match self {
            Self::Linux(_) => RuntimeBackend::LinuxSystemdBwrap,
            Self::AppleContainer(_) => RuntimeBackend::AppleContainer,
        }
    }

    pub(super) fn runtime_spec(&self) -> String {
        let context = match self {
            Self::Linux(runtime) => &runtime.context,
            Self::AppleContainer(runtime) => &runtime.context,
        };

        let mut parts = vec![
            format!("backend={:?}", context.runtime.backend),
            format!(
                "image={}",
                context.runtime.image.as_deref().unwrap_or_default()
            ),
            format!("host-opencode={}", context.host_opencode_command),
            format!("container-opencode={}", context.container_opencode_command),
            format!(
                "readable={}",
                format_path_list(&context.expanded_isolation.readable)
            ),
            format!(
                "writable={}",
                format_path_list(&context.expanded_isolation.writable)
            ),
            format!(
                "isolated={}",
                format_path_list(&context.expanded_isolation.isolated)
            ),
            format!(
                "tmpfs={}",
                format_path_list(&context.expanded_isolation.tmpfs)
            ),
            format!(
                "skills={}",
                format_skill_mounts(&context.expanded_isolation.added_skills)
            ),
            format!(
                "inherit-env={}",
                context.expanded_isolation.inherit_env.join(",")
            ),
            format!(
                "memory-high={}",
                context
                    .expanded_isolation
                    .memory_high_bytes
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            ),
            format!(
                "memory-max={}",
                context
                    .expanded_isolation
                    .memory_max_bytes
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            ),
            format!(
                "cpu={}",
                context
                    .expanded_isolation
                    .cpu
                    .as_deref()
                    .unwrap_or_default()
            ),
        ];
        parts.push(format!(
            "workspace-root={}",
            context.workspace_directory_path.to_string_lossy()
        ));
        parts.join("\n")
    }
}

#[derive(Debug, Clone)]
pub(super) struct LinuxSystemdBwrapRuntime {
    context: RuntimeContext,
}

impl LinuxSystemdBwrapRuntime {
    async fn start_server(
        &self,
        key: &str,
        inherited_env: &[(String, String)],
    ) -> Result<RuntimeStartResult, CombinedServiceError> {
        let password = generate_random_password();
        let port = pick_random_free_port().await?;
        let unit = generate_linux_runtime_id();
        let command = self
            .build_systemd_bwrap_command(key, &password, port, &unit, inherited_env)
            .await?;
        let mut process = Command::new(&command.program);
        process
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .args(&command.args);
        for (name, value) in &command.inherited_env {
            process.env(name, value);
        }
        let output = process.output().await?;

        if !output.status.success() {
            return Err(CombinedServiceError::StartWorkspaceFailed {
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        Ok(RuntimeStartResult {
            transient: TransientWorkspaceSnapshot {
                uri: format!("http://opencode:{password}@127.0.0.1:{port}/"),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::LinuxSystemdBwrap,
                    id: unit,
                    metadata: BTreeMap::from([(
                        RUNTIME_SPEC_METADATA_KEY.to_string(),
                        WorkspaceRuntime::Linux(self.clone()).runtime_spec(),
                    )]),
                },
            },
        })
    }

    async fn stop_server(
        runtime_handle: &RuntimeHandleSnapshot,
    ) -> Result<(), CombinedServiceError> {
        let args = vec![
            "--user".to_string(),
            "stop".to_string(),
            "--no-block".to_string(),
            runtime_handle.id.clone(),
        ];
        let output = Command::new("systemctl")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(CombinedServiceError::StopWorkspaceFailed {
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    async fn build_pty_command(
        &self,
        key: &str,
        _runtime_handle: Option<&RuntimeHandleSnapshot>,
        inherited_env: &[(String, String)],
        command: Vec<String>,
    ) -> Result<SpawnCommand, CombinedServiceError> {
        let unit = generate_linux_runtime_id();
        let mut args = vec![
            "--user".to_string(),
            "--wait".to_string(),
            "--collect".to_string(),
            "--pty".to_string(),
        ];
        append_systemd_run_inherit_env(&mut args, inherited_env);
        args.push("--unit".to_string());
        args.push(unit);
        self.append_systemd_limits(&mut args);
        self.append_bwrap_sandbox_args(&mut args, key).await?;
        args.extend(command);

        Ok(SpawnCommand {
            program: "systemd-run".to_string(),
            args,
            inherited_env: inherited_env.to_vec(),
        })
    }

    async fn build_systemd_bwrap_command(
        &self,
        key: &str,
        password: &str,
        port: u16,
        unit: &str,
        inherited_env: &[(String, String)],
    ) -> Result<SpawnCommand, CombinedServiceError> {
        let mut args = vec!["--user".to_string(), "--no-block".to_string()];
        let mut env = inherited_env.to_vec();
        env.push((
            "OPENCODE_SERVER_USERNAME".to_string(),
            "opencode".to_string(),
        ));
        env.push(("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string()));
        append_systemd_run_inherit_env(&mut args, &env);
        args.push("--unit".to_string());
        args.push(unit.to_string());
        self.append_systemd_limits(&mut args);

        self.append_bwrap_sandbox_args(&mut args, key).await?;
        args.push(self.context.host_opencode_command.clone());
        args.push("serve".to_string());
        args.push("--hostname".to_string());
        args.push("127.0.0.1".to_string());
        args.push("--port".to_string());
        args.push(port.to_string());

        Ok(SpawnCommand {
            program: "systemd-run".to_string(),
            args,
            inherited_env: env,
        })
    }

    fn append_systemd_limits(&self, args: &mut Vec<String>) {
        if let Some(memory_high_bytes) = self.context.expanded_isolation.memory_high_bytes {
            args.push("-p".to_string());
            args.push(format!("MemoryHigh={memory_high_bytes}"));
        }
        if let Some(memory_max_bytes) = self.context.expanded_isolation.memory_max_bytes {
            args.push("-p".to_string());
            args.push(format!("MemoryMax={memory_max_bytes}"));
            args.push("-p".to_string());
            args.push("MemorySwapMax=0".to_string());
        }
        if let Some(cpu) = &self.context.expanded_isolation.cpu {
            args.push("-p".to_string());
            args.push(format!("CPUQuota={cpu}"));
        }
    }

    async fn append_bwrap_sandbox_args(
        &self,
        args: &mut Vec<String>,
        key: &str,
    ) -> Result<(), CombinedServiceError> {
        let workspace_path = self.context.workspace_directory_path.join(key);
        let workspace_path_str = workspace_path.to_string_lossy().into_owned();

        args.push("bwrap".to_string());
        args.push("--chdir".to_string());
        args.push(workspace_path_str.clone());

        args.push("--ro-bind".to_string());
        args.push("/".to_string());
        args.push("/".to_string());

        let mut mount_specs = Vec::new();
        mount_specs.extend(
            self.context
                .expanded_isolation
                .readable
                .iter()
                .cloned()
                .map(|path| MountSpec::new(path, None, MountKind::Readable)),
        );
        mount_specs.extend(
            self.context
                .expanded_isolation
                .writable
                .iter()
                .cloned()
                .map(|path| MountSpec::new(path.clone(), Some(path), MountKind::Writable)),
        );
        mount_specs.push(MountSpec::new(
            workspace_path.clone(),
            Some(workspace_path.clone()),
            MountKind::Writable,
        ));
        mount_specs.extend(
            self.context
                .expanded_isolation
                .isolated
                .iter()
                .cloned()
                .map(|path| {
                    let source = self.isolated_storage_path(key, &path);
                    MountSpec::new(path.clone(), Some(source), MountKind::Isolated)
                }),
        );
        mount_specs.extend(
            self.context
                .expanded_isolation
                .tmpfs
                .iter()
                .cloned()
                .map(|path| MountSpec::new(path, None, MountKind::Tmpfs)),
        );
        mount_specs.extend(
            self.context
                .expanded_isolation
                .added_skills
                .iter()
                .cloned()
                .map(|mount| MountSpec::new(mount.target, Some(mount.source), MountKind::Readable)),
        );
        mount_specs.sort_by(|a, b| {
            a.depth()
                .cmp(&b.depth())
                .then_with(|| a.target.cmp(&b.target))
                .then_with(|| a.kind.cmp(&b.kind))
        });

        let mut resolved_mounts = Vec::with_capacity(mount_specs.len());
        for (index, mount_spec) in mount_specs.iter().enumerate() {
            let resolved_mount = mount_spec.resolve_effective(&resolved_mounts);
            let owns_node = !mount_specs.iter().skip(index + 1).any(|other| {
                other.target.starts_with(&mount_spec.target) && other.target != mount_spec.target
            });
            let owns_source_node = owns_node
                || (mount_spec.is_file
                    && mount_spec
                        .source
                        .as_ref()
                        .is_some_and(|source| source != &resolved_mount.effective_source));
            resolved_mount.prepare_source_node(owns_source_node).await?;
            resolved_mounts.push(resolved_mount);
        }

        for resolved_mount in resolved_mounts {
            resolved_mount.append_args(args);
        }

        args.push("--proc".to_string());
        args.push("/proc".to_string());
        args.push("--dev".to_string());
        args.push("/dev".to_string());
        args.push("--die-with-parent".to_string());

        Ok(())
    }

    fn isolated_storage_path(&self, key: &str, target: &Path) -> PathBuf {
        let relative = target
            .strip_prefix("/")
            .expect("isolated path is validated as absolute");
        self.context
            .workspace_directory_path
            .join(".multicode")
            .join("isolate")
            .join(key)
            .join(relative)
    }

    async fn read_activity(runtime_handle: &RuntimeHandleSnapshot) -> RuntimeActivity {
        let output = match Command::new("systemctl")
            .args([
                "--user",
                "show",
                runtime_handle.id.as_str(),
                "--property",
                "ActiveState",
                "--value",
            ])
            .stdin(Stdio::null())
            .output()
            .await
        {
            Ok(output) => output,
            Err(_) => return RuntimeActivity::Unknown,
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

    async fn read_usage(runtime_handle: &RuntimeHandleSnapshot) -> RuntimeUsageSample {
        let output = match Command::new("systemctl")
            .args([
                "--user",
                "show",
                runtime_handle.id.as_str(),
                "--property",
                "ActiveState",
                "--property",
                "MemoryCurrent",
                "--property",
                "CPUUsageNSec",
            ])
            .stdin(Stdio::null())
            .output()
            .await
        {
            Ok(output) => output,
            Err(_) => {
                return RuntimeUsageSample {
                    state: Some(RuntimeUsageState::Unknown),
                    ..Default::default()
                };
            }
        };

        if !output.status.success() {
            return RuntimeUsageSample {
                state: Some(RuntimeUsageState::Stopped),
                ..Default::default()
            };
        }

        parse_linux_unit_usage(&String::from_utf8_lossy(&output.stdout))
    }
}

#[derive(Debug, Clone)]
pub(super) struct AppleContainerRuntime {
    context: RuntimeContext,
}

impl AppleContainerRuntime {
    async fn start_server(
        &self,
        key: &str,
        inherited_env: &[(String, String)],
    ) -> Result<RuntimeStartResult, CombinedServiceError> {
        let _start_guard = apple_container_start_lock().lock().await;
        let password = generate_random_password();
        let port = pick_random_free_port().await?;
        let container_name = self.generate_runtime_id(key);
        let command = self
            .build_run_command(key, &container_name, &password, port, inherited_env)
            .await?;

        let output = self
            .run_container_start_command(command.program.clone(), command.args.clone())
            .await?;

        if !output.status.success() {
            return Err(CombinedServiceError::StartWorkspaceFailed {
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let mut metadata = BTreeMap::new();
        metadata.insert("workspace-key".to_string(), key.to_string());
        metadata.insert("port".to_string(), port.to_string());
        metadata.insert(
            RUNTIME_SPEC_METADATA_KEY.to_string(),
            WorkspaceRuntime::AppleContainer(self.clone()).runtime_spec(),
        );

        Ok(RuntimeStartResult {
            transient: TransientWorkspaceSnapshot {
                uri: format!("http://opencode:{password}@127.0.0.1:{port}/"),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::AppleContainer,
                    id: container_name,
                    metadata,
                },
            },
        })
    }

    async fn run_container_start_command(
        &self,
        program: String,
        args: Vec<String>,
    ) -> Result<Output, CombinedServiceError> {
        let output = run_blocking_process(program.clone(), args.clone()).await?;
        if output.status.success() {
            return Ok(output);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !container_start_reports_allocator_exhaustion(&stderr) {
            return Ok(output);
        }

        tracing::warn!(
            stderr = %stderr.trim(),
            "apple container allocator exhausted; pruning containers before retry"
        );
        let prune_output = run_blocking_process(
            container_program(),
            vec!["prune".to_string(), "-f".to_string()],
        )
        .await?;
        if !prune_output.status.success() {
            let prune_stderr = String::from_utf8_lossy(&prune_output.stderr)
                .trim()
                .to_string();
            return Err(CombinedServiceError::StartWorkspaceFailed {
                status: output.status.code(),
                stderr: if prune_stderr.is_empty() {
                    format!("{stderr}\nautomatic container prune failed")
                } else {
                    format!("{stderr}\nautomatic container prune failed: {prune_stderr}")
                },
            });
        }

        tracing::info!("apple container prune succeeded; retrying workspace startup");
        let retry_output = run_blocking_process(program, args).await?;
        if !retry_output.status.success()
            && container_start_reports_allocator_exhaustion(&String::from_utf8_lossy(
                &retry_output.stderr,
            ))
        {
            return Err(CombinedServiceError::StartWorkspaceFailed {
                status: retry_output.status.code(),
                stderr: format!(
                    "{}\nautomatic container prune was insufficient; restart the Apple container backend",
                    String::from_utf8_lossy(&retry_output.stderr).trim()
                ),
            });
        }
        Ok(retry_output)
    }

    async fn stop_server(
        runtime_handle: &RuntimeHandleSnapshot,
    ) -> Result<(), CombinedServiceError> {
        let output = run_blocking_process(
            container_program(),
            vec![
                "rm".to_string(),
                "-f".to_string(),
                runtime_handle.id.clone(),
            ],
        )
        .await?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() || container_delete_reports_missing(&stderr) {
            Ok(())
        } else {
            Err(CombinedServiceError::StopWorkspaceFailed {
                status: output.status.code(),
                stderr: stderr.into_owned(),
            })
        }
    }

    async fn build_pty_command(
        &self,
        key: &str,
        runtime_handle: Option<&RuntimeHandleSnapshot>,
        inherited_env: &[(String, String)],
        command: Vec<String>,
    ) -> Result<SpawnCommand, CombinedServiceError> {
        if let Some(runtime_handle) = runtime_handle
            && runtime_handle.backend == RuntimeBackend::AppleContainer
        {
            return self
                .build_exec_command(runtime_handle, key, inherited_env, command)
                .await;
        }

        let image = self.context.runtime.image.as_deref().ok_or_else(|| {
            CombinedServiceError::InvalidRuntimeConfig {
                field: "runtime.image".to_string(),
                message: "apple-container backend requires a runtime image".to_string(),
            }
        })?;
        let mut env = inherited_env.to_vec();
        let host_gitconfig = self.host_gitconfig_path_for_env(&env);
        self.append_implicit_env(&mut env, host_gitconfig.as_deref())
            .await?;
        let env_file = self.write_env_file(key, "exec.env", &env).await?;
        let workspace_path = self.context.workspace_directory_path.join(key);
        let mut args = vec![
            "run".to_string(),
            "--rm".to_string(),
            "--tty".to_string(),
            "--interactive".to_string(),
            "--env-file".to_string(),
            env_file.to_string_lossy().into_owned(),
            "--workdir".to_string(),
            workspace_path.to_string_lossy().into_owned(),
        ];
        self.append_container_limits(&mut args);
        self.append_container_mounts(args.as_mut(), key, host_gitconfig.as_deref())
            .await?;
        args.push(image.to_string());
        args.extend(command);

        Ok(SpawnCommand {
            program: container_program(),
            args,
            inherited_env: Vec::new(),
        })
    }

    async fn build_exec_command(
        &self,
        runtime_handle: &RuntimeHandleSnapshot,
        key: &str,
        inherited_env: &[(String, String)],
        command: Vec<String>,
    ) -> Result<SpawnCommand, CombinedServiceError> {
        let mut env = inherited_env.to_vec();
        let host_gitconfig = self.host_gitconfig_path_for_env(&env);
        self.append_implicit_env(&mut env, host_gitconfig.as_deref())
            .await?;
        let env_file = self.write_env_file(key, "exec.env", &env).await?;
        let workspace_path = self.context.workspace_directory_path.join(key);
        let args = vec![
            "exec".to_string(),
            "--tty".to_string(),
            "--interactive".to_string(),
            "--env-file".to_string(),
            env_file.to_string_lossy().into_owned(),
            "--workdir".to_string(),
            workspace_path.to_string_lossy().into_owned(),
            runtime_handle.id.clone(),
        ]
        .into_iter()
        .chain(command)
        .collect();

        Ok(SpawnCommand {
            program: container_program(),
            args,
            inherited_env: Vec::new(),
        })
    }

    async fn read_activity(runtime_handle: &RuntimeHandleSnapshot) -> RuntimeActivity {
        let inspect_output = run_blocking_process(
            container_program(),
            vec!["inspect".to_string(), runtime_handle.id.clone()],
        )
        .await;
        if let Ok(output) = inspect_output {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains(r#""status":"running""#) {
                    return RuntimeActivity::Active;
                }
                if stdout.contains(r#""status":"stopped""#)
                    || stdout.contains(r#""status":"exited""#)
                {
                    return RuntimeActivity::Stopped;
                }
            } else {
                return RuntimeActivity::Stopped;
            }
        }

        let output = match run_blocking_process(container_program(), vec!["list".to_string()]).await
        {
            Ok(output) => output,
            Err(_) => return RuntimeActivity::Unknown,
        };
        if !output.status.success() {
            return RuntimeActivity::Unknown;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout
            .lines()
            .any(|line| line.contains(runtime_handle.id.as_str()))
        {
            RuntimeActivity::Active
        } else {
            RuntimeActivity::Stopped
        }
    }

    async fn read_usage(_runtime_handle: &RuntimeHandleSnapshot) -> RuntimeUsageSample {
        RuntimeUsageSample {
            state: Some(RuntimeUsageState::Unknown),
            ..Default::default()
        }
    }

    async fn build_run_command(
        &self,
        key: &str,
        container_name: &str,
        password: &str,
        port: u16,
        inherited_env: &[(String, String)],
    ) -> Result<SpawnCommand, CombinedServiceError> {
        let image = self.context.runtime.image.as_deref().ok_or_else(|| {
            CombinedServiceError::InvalidRuntimeConfig {
                field: "runtime.image".to_string(),
                message: "apple-container backend requires a runtime image".to_string(),
            }
        })?;
        let mut env = inherited_env.to_vec();
        env.push((
            "OPENCODE_SERVER_USERNAME".to_string(),
            "opencode".to_string(),
        ));
        env.push(("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string()));
        let host_gitconfig = self.host_gitconfig_path_for_env(&env);
        self.append_implicit_env(&mut env, host_gitconfig.as_deref())
            .await?;

        let workspace_path = self.context.workspace_directory_path.join(key);
        tokio::fs::create_dir_all(&workspace_path).await?;
        let env_file = self.write_env_file(key, "server.env", &env).await?;

        let mut args = vec![
            "run".to_string(),
            "--detach".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            container_name.to_string(),
            "--env-file".to_string(),
            env_file.to_string_lossy().into_owned(),
            "--workdir".to_string(),
            workspace_path.to_string_lossy().into_owned(),
            "--publish".to_string(),
            format!("127.0.0.1:{port}:{port}/tcp"),
        ];
        self.append_container_limits(&mut args);
        self.append_container_mounts(&mut args, key, host_gitconfig.as_deref())
            .await?;
        args.push(image.to_string());
        args.push(self.context.container_opencode_command.clone());
        args.push("serve".to_string());
        args.push("--hostname".to_string());
        args.push("0.0.0.0".to_string());
        args.push("--port".to_string());
        args.push(port.to_string());

        Ok(SpawnCommand {
            program: container_program(),
            args,
            inherited_env: Vec::new(),
        })
    }

    fn append_container_limits(&self, args: &mut Vec<String>) {
        if let Some(cpu) = self
            .context
            .expanded_isolation
            .cpu
            .as_deref()
            .and_then(container_cpu_value)
        {
            args.push("--cpus".to_string());
            args.push(cpu);
        }

        let memory_limit = self
            .context
            .expanded_isolation
            .memory_max_bytes
            .or(self.context.expanded_isolation.memory_high_bytes);
        if let Some(memory_limit) = memory_limit {
            args.push("--memory".to_string());
            args.push(memory_limit.to_string());
        }
    }

    async fn append_container_mounts(
        &self,
        args: &mut Vec<String>,
        key: &str,
        host_gitconfig: Option<&Path>,
    ) -> Result<(), CombinedServiceError> {
        let workspace_path = self.context.workspace_directory_path.join(key);
        let implicit_gitconfig_mount = self
            .build_implicit_gitconfig_mount(key, host_gitconfig)
            .await?;
        let mut mount_specs = Vec::new();
        mount_specs.extend(
            self.context
                .expanded_isolation
                .readable
                .iter()
                .cloned()
                .filter(|path| !self.is_implicitly_handled_gitconfig(path, host_gitconfig))
                .map(|path| MountSpec::new(path, None, MountKind::Readable)),
        );
        mount_specs.extend(
            self.context
                .expanded_isolation
                .writable
                .iter()
                .cloned()
                .map(|path| MountSpec::new(path.clone(), Some(path), MountKind::Writable)),
        );
        mount_specs.push(MountSpec::new(
            workspace_path.clone(),
            Some(workspace_path.clone()),
            MountKind::Writable,
        ));
        mount_specs.extend(
            self.context
                .expanded_isolation
                .isolated
                .iter()
                .cloned()
                .map(|path| {
                    let source = self.isolated_storage_path(key, &path);
                    MountSpec::new(path.clone(), Some(source), MountKind::Isolated)
                }),
        );
        mount_specs.extend(
            self.context
                .expanded_isolation
                .tmpfs
                .iter()
                .cloned()
                .map(|path| MountSpec::new(path, None, MountKind::Tmpfs)),
        );
        if let Some(skill_mount) = self.build_aggregated_skill_mount(key).await? {
            mount_specs.push(skill_mount);
        } else {
            mount_specs.extend(
                self.context
                    .expanded_isolation
                    .added_skills
                    .iter()
                    .cloned()
                    .map(|mount| {
                        MountSpec::new(mount.target, Some(mount.source), MountKind::Readable)
                    }),
            );
        }
        if let Some(implicit_gitconfig_mount) = implicit_gitconfig_mount {
            mount_specs.push(implicit_gitconfig_mount);
        }
        mount_specs.sort_by(|a, b| {
            a.depth()
                .cmp(&b.depth())
                .then_with(|| a.target.cmp(&b.target))
                .then_with(|| a.kind.cmp(&b.kind))
        });

        let mut resolved_mounts = Vec::with_capacity(mount_specs.len());
        for (index, mount_spec) in mount_specs.iter().enumerate() {
            let resolved_mount = mount_spec.resolve_effective(&resolved_mounts);
            let owns_node = !mount_specs.iter().skip(index + 1).any(|other| {
                other.target.starts_with(&mount_spec.target) && other.target != mount_spec.target
            });
            let owns_source_node = owns_node
                || (mount_spec.is_file
                    && mount_spec
                        .source
                        .as_ref()
                        .is_some_and(|source| source != &resolved_mount.effective_source));
            resolved_mount.prepare_source_node(owns_source_node).await?;
            if resolved_mount.needs_container_target_materialization() {
                resolved_mount.prepare_target_node(owns_node).await?;
                resolved_mount.prepare_container_materialized_file().await?;
            }
            resolved_mounts.push(resolved_mount);
        }

        for resolved_mount in resolved_mounts {
            resolved_mount.append_container_args(args);
        }

        Ok(())
    }

    async fn append_implicit_env(
        &self,
        env: &mut Vec<(String, String)>,
        host_gitconfig: Option<&Path>,
    ) -> Result<(), CombinedServiceError> {
        if host_gitconfig.is_some() {
            env.push((
                "GIT_CONFIG_GLOBAL".to_string(),
                format!("{APPLE_GITCONFIG_DIR}/{APPLE_GITCONFIG_FILE_NAME}"),
            ));
        }
        Ok(())
    }

    async fn build_implicit_gitconfig_mount(
        &self,
        key: &str,
        host_gitconfig: Option<&Path>,
    ) -> Result<Option<MountSpec>, CombinedServiceError> {
        let Some(host_gitconfig) = host_gitconfig else {
            return Ok(None);
        };

        let source_root = self.apple_runtime_root(key).join("gitconfig");
        clear_directory_contents(&source_root).await?;
        let gitconfig_contents = std::fs::read(host_gitconfig)?;
        tokio::fs::write(
            source_root.join(APPLE_GITCONFIG_FILE_NAME),
            gitconfig_contents,
        )
        .await?;

        Ok(Some(MountSpec::new(
            PathBuf::from(APPLE_GITCONFIG_DIR),
            Some(source_root),
            MountKind::Readable,
        )))
    }

    fn host_gitconfig_path_for_env(&self, env: &[(String, String)]) -> Option<PathBuf> {
        let home = env
            .iter()
            .find(|(name, _)| name == "HOME")
            .map(|(_, value)| value)?;
        let path = PathBuf::from(home).join(".gitconfig");
        (path.is_absolute() && path.is_file() && std::fs::read(&path).is_ok()).then_some(path)
    }

    fn is_implicitly_handled_gitconfig(&self, path: &Path, host_gitconfig: Option<&Path>) -> bool {
        host_gitconfig.is_some_and(|gitconfig| gitconfig == path)
    }

    async fn build_aggregated_skill_mount(
        &self,
        key: &str,
    ) -> Result<Option<MountSpec>, CombinedServiceError> {
        let added_skills = &self.context.expanded_isolation.added_skills;
        if added_skills.is_empty() {
            return Ok(None);
        }

        let Some(target_root) = added_skills
            .first()
            .and_then(|mount| mount.target.parent())
            .map(Path::to_path_buf)
        else {
            return Ok(None);
        };

        if added_skills
            .iter()
            .any(|mount| mount.target.parent() != Some(target_root.as_path()))
        {
            return Ok(None);
        }

        let aggregate_root = self.apple_runtime_root(key).join("skills");
        clear_directory_contents(&aggregate_root).await?;

        if tokio::fs::metadata(&target_root).await.is_ok() {
            copy_directory_tree(&target_root, &aggregate_root).await?;
        }

        for skill in added_skills {
            let Some(skill_name) = skill.target.file_name() else {
                continue;
            };
            copy_directory_tree(&skill.source, &aggregate_root.join(skill_name)).await?;
        }

        Ok(Some(MountSpec::new(
            target_root,
            Some(aggregate_root),
            MountKind::Readable,
        )))
    }

    async fn write_env_file(
        &self,
        key: &str,
        file_name: &str,
        env: &[(String, String)],
    ) -> Result<PathBuf, CombinedServiceError> {
        let runtime_root = self.apple_runtime_root(key);
        tokio::fs::create_dir_all(&runtime_root).await?;
        let path = runtime_root.join(file_name);
        let mut content = String::new();
        for (name, value) in env {
            if value.contains('\n') || value.contains('\r') {
                return Err(CombinedServiceError::InvalidRuntimeConfig {
                    field: "runtime.env-file".to_string(),
                    message: format!(
                        "environment variable '{name}' contains newlines and cannot be written to a container env file"
                    ),
                });
            }
            content.push_str(name);
            content.push('=');
            content.push_str(value);
            content.push('\n');
        }
        tokio::fs::write(&path, content).await?;
        Ok(path)
    }

    fn apple_runtime_root(&self, key: &str) -> PathBuf {
        self.context
            .workspace_directory_path
            .join(".multicode")
            .join("apple-container")
            .join(key)
    }

    fn isolated_storage_path(&self, key: &str, target: &Path) -> PathBuf {
        let relative = target
            .strip_prefix("/")
            .expect("isolated path is validated as absolute");
        self.apple_runtime_root(key).join("isolate").join(relative)
    }

    fn generate_runtime_id(&self, key: &str) -> String {
        format!("multicode-{key}-{}", Uuid::new_v4().as_simple())
    }
}

fn format_path_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(",")
}

fn format_skill_mounts(skills: &[super::config::AddedSkillMount]) -> String {
    let mut pairs = skills
        .iter()
        .map(|skill| {
            format!(
                "{}=>{}",
                skill.source.to_string_lossy(),
                skill.target.to_string_lossy()
            )
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs.join(",")
}

fn append_systemd_run_inherit_env(args: &mut Vec<String>, env: &[(String, String)]) {
    for (name, _) in env {
        args.push("--setenv".to_string());
        args.push(name.clone());
    }
}

fn generate_random_password() -> String {
    Uuid::new_v4().as_simple().to_string()
}

fn generate_linux_runtime_id() -> String {
    format!("multicode-{}.service", Uuid::new_v4().as_simple())
}

async fn pick_random_free_port() -> Result<u16, CombinedServiceError> {
    if let Some(port) = std::env::var_os("MULTICODE_FIXED_PORT") {
        let port = port.to_string_lossy();
        let parsed =
            port.parse::<u16>()
                .map_err(|err| CombinedServiceError::InvalidRuntimeConfig {
                    field: "MULTICODE_FIXED_PORT".to_string(),
                    message: err.to_string(),
                })?;
        return Ok(parsed);
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn parse_linux_unit_usage(output: &str) -> RuntimeUsageSample {
    let mut active_state: Option<&str> = None;
    let mut memory_current: Option<u64> = None;
    let mut cpu_usage_nsec: Option<u64> = None;

    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "ActiveState" => active_state = Some(value),
            "MemoryCurrent" => memory_current = parse_systemctl_u64(value),
            "CPUUsageNSec" => cpu_usage_nsec = parse_systemctl_u64(value),
            _ => {}
        }
    }

    let state = match active_state.unwrap_or_default() {
        "active" | "activating" => RuntimeUsageState::Active,
        "" => RuntimeUsageState::Unknown,
        _ => RuntimeUsageState::Stopped,
    };

    RuntimeUsageSample {
        memory_current,
        cpu_usage_nsec,
        state: Some(state),
    }
}

fn parse_systemctl_u64(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "[not set]" {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

fn container_cpu_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let cpus = if let Some(percent) = value.strip_suffix('%') {
        percent.trim().parse::<f64>().ok()? / 100.0
    } else {
        value.parse::<f64>().ok()?
    };
    if !cpus.is_finite() || cpus <= 0.0 {
        return None;
    }
    Some(cpus.ceil().max(1.0).to_string())
}

fn container_program() -> String {
    std::env::var("MULTICODE_CONTAINER_COMMAND").unwrap_or_else(|_| "container".to_string())
}

fn apple_container_start_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn container_delete_reports_missing(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("not found")
        || stderr.contains("no such")
        || stderr.contains("no matching containers")
        || stderr.contains("does not exist")
}

fn container_start_reports_allocator_exhaustion(stderr: &str) -> bool {
    stderr
        .to_ascii_lowercase()
        .contains("no free indices are available for allocation")
}

async fn run_blocking_process(
    program: String,
    args: Vec<String>,
) -> Result<Output, std::io::Error> {
    tokio::task::spawn_blocking(move || {
        std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()
    })
    .await
    .map_err(|err| std::io::Error::other(err.to_string()))?
}

async fn copy_directory_tree(source: &Path, target: &Path) -> Result<(), std::io::Error> {
    let mut pending = vec![(source.to_path_buf(), target.to_path_buf())];

    while let Some((source_dir, target_dir)) = pending.pop() {
        tokio::fs::create_dir_all(&target_dir).await?;
        let mut entries = tokio::fs::read_dir(&source_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let source_path = entry.path();
            let target_path = target_dir.join(entry.file_name());
            let metadata = tokio::fs::metadata(&source_path).await?;
            if metadata.is_dir() {
                pending.push((source_path, target_path));
            } else if metadata.is_file() {
                tokio::fs::copy(&source_path, &target_path).await?;
            }
        }
    }

    Ok(())
}

async fn clear_directory_contents(path: &Path) -> Result<(), std::io::Error> {
    tokio::fs::create_dir_all(path).await?;
    let mut entries = tokio::fs::read_dir(path).await?;
    while let Some(entry) = entries.next_entry().await? {
        let entry_path = entry.path();
        let metadata = entry.metadata().await?;
        if metadata.is_dir() {
            tokio::fs::remove_dir_all(entry_path).await?;
        } else {
            tokio::fs::remove_file(entry_path).await?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MountKind {
    Readable,
    Writable,
    Isolated,
    Tmpfs,
}

#[derive(Debug, Clone)]
pub(crate) struct MountSpec {
    target: PathBuf,
    source: Option<PathBuf>,
    kind: MountKind,
    is_file: bool,
}

impl MountSpec {
    pub(crate) fn new(target: PathBuf, source: Option<PathBuf>, kind: MountKind) -> Self {
        let is_file = match source.as_ref() {
            Some(source) => std::fs::metadata(source)
                .map(|metadata| metadata.is_file())
                .unwrap_or_else(|_| {
                    std::fs::metadata(&target)
                        .map(|metadata| metadata.is_file())
                        .unwrap_or_else(|_| {
                            path_looks_like_file(source) || path_looks_like_file(&target)
                        })
                }),
            None => std::fs::metadata(&target)
                .map(|metadata| metadata.is_file())
                .unwrap_or_else(|_| path_looks_like_file(&target)),
        };
        Self {
            target,
            source,
            kind,
            is_file,
        }
    }

    fn depth(&self) -> usize {
        self.target.components().count()
    }

    fn resolve_backing_mount<'a>(
        path: &Path,
        prior_mounts: &'a [ResolvedMountSpec],
    ) -> Option<&'a ResolvedMountSpec> {
        prior_mounts.iter().rev().find(|prior_mount| {
            path == prior_mount.mount.target || path.starts_with(&prior_mount.mount.target)
        })
    }

    fn resolve_backing_path(path: &Path, prior_mounts: &[ResolvedMountSpec]) -> PathBuf {
        if let Some(prior_mount) = Self::resolve_backing_mount(path, prior_mounts) {
            let relative = path
                .strip_prefix(&prior_mount.mount.target)
                .expect("path should be under prior mount target");
            prior_mount.effective_source.join(relative)
        } else {
            path.to_path_buf()
        }
    }

    pub(crate) fn resolve_effective(
        &self,
        prior_mounts: &[ResolvedMountSpec],
    ) -> ResolvedMountSpec {
        let backing_mount_kind =
            Self::resolve_backing_mount(&self.target, prior_mounts).map(|mount| mount.mount.kind);
        let effective_target = Self::resolve_backing_path(&self.target, prior_mounts);
        let effective_source = match self.kind {
            MountKind::Isolated => self
                .source
                .as_ref()
                .map(|source| Self::resolve_backing_path(source, prior_mounts))
                .unwrap_or_else(|| effective_target.clone()),
            MountKind::Readable | MountKind::Writable => {
                self.source.clone().unwrap_or_else(|| self.target.clone())
            }
            MountKind::Tmpfs => effective_target.clone(),
        };
        ResolvedMountSpec {
            mount: self.clone(),
            backing_mount_kind,
            effective_target,
            effective_source,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedMountSpec {
    mount: MountSpec,
    backing_mount_kind: Option<MountKind>,
    effective_target: PathBuf,
    effective_source: PathBuf,
}

impl ResolvedMountSpec {
    fn needs_container_target_materialization(&self) -> bool {
        self.backing_mount_kind.is_some() && self.effective_target != self.mount.target
    }

    pub(crate) async fn prepare_source_node(
        &self,
        owns_node: bool,
    ) -> Result<(), CombinedServiceError> {
        self.prepare_node(
            &self.effective_source,
            owns_node,
            self.mount
                .source
                .as_ref()
                .filter(|original| *original != &self.effective_source),
        )
        .await
    }

    pub(crate) async fn prepare_target_node(
        &self,
        owns_node: bool,
    ) -> Result<(), CombinedServiceError> {
        let should_materialize = if self.mount.is_file { owns_node } else { true };
        self.prepare_node(&self.effective_target, should_materialize, None)
            .await
    }

    pub(crate) async fn prepare_container_materialized_file(
        &self,
    ) -> Result<(), CombinedServiceError> {
        if !self.should_materialize_container_file() {
            return Ok(());
        }

        if let Some(parent) = self.effective_target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        if tokio::fs::metadata(&self.effective_source).await.is_ok() {
            tokio::fs::copy(&self.effective_source, &self.effective_target).await?;
        } else if tokio::fs::metadata(&self.effective_target).await.is_err() {
            tokio::fs::File::create(&self.effective_target).await?;
        }

        Ok(())
    }

    async fn prepare_node(
        &self,
        path: &Path,
        materialize_node: bool,
        seed_file: Option<&PathBuf>,
    ) -> Result<(), CombinedServiceError> {
        if self.mount.is_file {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            if materialize_node && tokio::fs::metadata(path).await.is_err() {
                if let Some(seed_file) = seed_file {
                    if tokio::fs::metadata(seed_file).await.is_ok() {
                        tokio::fs::copy(seed_file, path).await?;
                        return Ok(());
                    }
                }
                tokio::fs::File::create(path).await?;
            }
        } else if materialize_node {
            tokio::fs::create_dir_all(path).await?;
        } else if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    fn append_args(&self, args: &mut Vec<String>) {
        match self.mount.kind {
            MountKind::Readable => {
                args.push("--ro-bind".to_string());
                args.push(self.effective_source.to_string_lossy().into_owned());
                args.push(self.mount.target.to_string_lossy().into_owned());
            }
            MountKind::Writable | MountKind::Isolated => {
                args.push("--bind".to_string());
                args.push(self.effective_source.to_string_lossy().into_owned());
                args.push(self.mount.target.to_string_lossy().into_owned());
            }
            MountKind::Tmpfs => {
                args.push("--tmpfs".to_string());
                args.push(self.mount.target.to_string_lossy().into_owned());
            }
        }
    }

    fn append_container_args(&self, args: &mut Vec<String>) {
        if self.should_materialize_container_file() {
            return;
        }

        match self.mount.kind {
            MountKind::Tmpfs => {
                args.push("--tmpfs".to_string());
                args.push(self.mount.target.to_string_lossy().into_owned());
            }
            MountKind::Readable | MountKind::Writable | MountKind::Isolated => {
                args.push("--mount".to_string());
                let mut mount = format!(
                    "type=bind,source={},target={}",
                    self.effective_source.to_string_lossy(),
                    self.mount.target.to_string_lossy()
                );
                if matches!(self.mount.kind, MountKind::Readable) {
                    mount.push_str(",readonly");
                }
                args.push(mount);
            }
        }
    }

    fn should_materialize_container_file(&self) -> bool {
        self.mount.kind == MountKind::Readable
            && self.mount.is_file
            && self.backing_mount_kind == Some(MountKind::Isolated)
            && self.effective_target != self.mount.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::config::{AddedSkillMount, IsolationConfig};
    use std::fs;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "multicode-runtime-test-{}-{}",
                std::process::id(),
                Uuid::new_v4().as_simple()
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

    fn apple_runtime(root: &TestDir, isolation: IsolationConfig) -> AppleContainerRuntime {
        let expanded_isolation =
            ExpandedIsolationConfig::from_config(&isolation, None).expect("config should expand");
        AppleContainerRuntime {
            context: RuntimeContext {
                runtime: RuntimeConfig {
                    backend: RuntimeBackend::AppleContainer,
                    image: Some("ghcr.io/example/multicode-java25:latest".to_string()),
                },
                workspace_directory_path: root.path().join("workspaces"),
                expanded_isolation,
                host_opencode_command: "/opt/opencode/bin/opencode".to_string(),
                container_opencode_command: "opencode".to_string(),
            },
        }
    }

    fn contains_sequence(args: &[String], sequence: &[&str]) -> bool {
        args.windows(sequence.len()).any(|window| {
            window
                .iter()
                .map(String::as_str)
                .eq(sequence.iter().copied())
        })
    }

    #[test]
    fn container_cpu_value_converts_percent_to_cpu_count() {
        assert_eq!(container_cpu_value("300%"), Some("3".to_string()));
        assert_eq!(container_cpu_value("150%"), Some("2".to_string()));
        assert_eq!(container_cpu_value("2"), Some("2".to_string()));
        assert_eq!(container_cpu_value("1.5"), Some("2".to_string()));
        assert_eq!(container_cpu_value(""), None);
    }

    #[test]
    fn container_delete_reports_missing_matches_common_container_rm_errors() {
        assert!(container_delete_reports_missing(
            "Error: failed to delete one or more containers: [\"multicode-alpha\"]: no matching containers found"
        ));
        assert!(container_delete_reports_missing(
            "Error: container not found"
        ));
        assert!(container_delete_reports_missing("Error: No such container"));
        assert!(!container_delete_reports_missing(
            "Error: failed to delete one or more containers: permission denied"
        ));
    }

    #[test]
    fn container_start_reports_allocator_exhaustion_matches_apple_error() {
        assert!(container_start_reports_allocator_exhaustion(
            "Error: failed to bootstrap container (cause: \"unknown: \"no free indices are available for allocation\"\")"
        ));
        assert!(!container_start_reports_allocator_exhaustion(
            "Error: failed to bootstrap container: permission denied"
        ));
    }

    #[test]
    fn apple_container_run_command_honors_limits_and_mounts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            let readable = root.path().join("readonly");
            let writable = root.path().join("writable");
            fs::create_dir_all(&workspace_root).expect("workspace root should exist");
            fs::create_dir_all(&readable).expect("readable should exist");
            fs::create_dir_all(&writable).expect("writable should exist");

            let runtime = apple_runtime(
                &root,
                IsolationConfig {
                    readable: vec![readable.to_string_lossy().into_owned()],
                    writable: vec![writable.to_string_lossy().into_owned()],
                    isolated: vec!["/var/tmp".to_string()],
                    tmpfs: vec!["/tmp".to_string()],
                    add_skills_from: Vec::new(),
                    inherit_env: vec!["HOME".to_string()],
                    memory_high: Some("8 GB".to_string()),
                    memory_max: Some("10 GB".to_string()),
                    cpu: Some("300%".to_string()),
                },
            );

            let command = runtime
                .build_run_command(
                    "alpha",
                    "multicode-alpha",
                    "secret",
                    31337,
                    &[(
                        "HOME".to_string(),
                        root.path().to_string_lossy().into_owned(),
                    )],
                )
                .await
                .expect("command should build");

            assert_eq!(command.program, "container");
            assert!(contains_sequence(
                &command.args,
                &["run", "--detach", "--rm"]
            ));
            assert!(contains_sequence(
                &command.args,
                &["--name", "multicode-alpha"]
            ));
            assert!(contains_sequence(&command.args, &["--cpus", "3"]));
            assert!(contains_sequence(
                &command.args,
                &["--memory", "10000000000"]
            ));
            assert!(contains_sequence(
                &command.args,
                &["--publish", "127.0.0.1:31337:31337/tcp"]
            ));
            assert!(contains_sequence(&command.args, &["--tmpfs", "/tmp"]));
            assert!(
                command
                    .args
                    .iter()
                    .any(|arg| arg.contains("type=bind") && arg.contains("readonly"))
            );
            assert!(
                command
                    .args
                    .iter()
                    .any(|arg| arg.contains("/var/tmp") && arg.contains("type=bind"))
            );
            assert!(contains_sequence(
                &command.args,
                &[
                    "ghcr.io/example/multicode-java25:latest",
                    "opencode",
                    "serve",
                    "--hostname",
                    "0.0.0.0",
                    "--port",
                    "31337"
                ]
            ));
        });
    }

    #[test]
    fn apple_container_implicitly_mounts_host_gitconfig() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            let home = root.path().join("home");
            let gitconfig = home.join(".gitconfig");
            fs::create_dir_all(&workspace_root).expect("workspace root should exist");
            fs::create_dir_all(&home).expect("home should exist");
            fs::write(&gitconfig, "[user]\nname = Test User\n").expect("gitconfig should exist");

            let previous_home = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("HOME", &home);
            }

            let runtime = apple_runtime(&root, IsolationConfig::default());
            let command = runtime
                .build_run_command(
                    "alpha",
                    "multicode-alpha",
                    "secret",
                    31337,
                    &[("HOME".to_string(), home.to_string_lossy().into_owned())],
                )
                .await
                .expect("command should build");
            let server_env = workspace_root
                .join(".multicode")
                .join("apple-container")
                .join("alpha")
                .join("server.env");

            if let Some(previous_home) = previous_home {
                unsafe {
                    std::env::set_var("HOME", previous_home);
                }
            } else {
                unsafe {
                    std::env::remove_var("HOME");
                }
            }

            let gitconfig_mount = format!(
                "type=bind,source={},target={},readonly",
                workspace_root
                    .join(".multicode")
                    .join("apple-container")
                    .join("alpha")
                    .join("gitconfig")
                    .to_string_lossy(),
                APPLE_GITCONFIG_DIR
            );
            assert!(
                command.args.iter().any(|arg| arg == &gitconfig_mount),
                "apple backend should implicitly mount host gitconfig through a synthetic directory"
            );
            let env_contents =
                fs::read_to_string(&server_env).expect("server env file should be written");
            assert!(
                env_contents.contains(&format!(
                    "GIT_CONFIG_GLOBAL={APPLE_GITCONFIG_DIR}/{APPLE_GITCONFIG_FILE_NAME}"
                )),
                "apple backend should point git at the synthetic mounted gitconfig"
            );
        });
    }

    #[test]
    fn apple_container_pty_command_uses_one_shot_container_run() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            fs::create_dir_all(workspace_root.join("alpha")).expect("workspace should exist");
            let runtime = apple_runtime(&root, IsolationConfig::default());

            let command = runtime
                .build_pty_command(
                    "alpha",
                    None,
                    &[(
                        "HOME".to_string(),
                        root.path().to_string_lossy().into_owned(),
                    )],
                    vec!["/bin/bash".to_string()],
                )
                .await
                .expect("pty command should build");

            assert_eq!(command.program, "container");
            assert!(contains_sequence(
                &command.args,
                &["run", "--rm", "--tty", "--interactive"]
            ));
            assert!(command.args.iter().any(|arg| arg.ends_with("exec.env")));
            assert!(
                command
                    .args
                    .iter()
                    .any(|arg| arg == "ghcr.io/example/multicode-java25:latest")
            );
            assert!(command.args.iter().any(|arg| arg == "/bin/bash"));
            assert!(command.inherited_env.is_empty());
        });
    }

    #[test]
    fn apple_container_pty_command_uses_container_exec_for_running_workspace() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            fs::create_dir_all(workspace_root.join("alpha")).expect("workspace should exist");
            let runtime = apple_runtime(&root, IsolationConfig::default());
            let runtime_handle = RuntimeHandleSnapshot {
                backend: RuntimeBackend::AppleContainer,
                id: "multicode-alpha-running".to_string(),
                metadata: BTreeMap::new(),
            };

            let command = runtime
                .build_pty_command(
                    "alpha",
                    Some(&runtime_handle),
                    &[(
                        "HOME".to_string(),
                        root.path().to_string_lossy().into_owned(),
                    )],
                    vec!["/bin/bash".to_string()],
                )
                .await
                .expect("pty command should build");

            assert_eq!(command.program, "container");
            assert!(contains_sequence(
                &command.args,
                &["exec", "--tty", "--interactive", "--env-file",]
            ));
            assert!(command.args.iter().any(|arg| arg.ends_with("exec.env")));
            assert!(contains_sequence(
                &command.args,
                &[
                    "--workdir",
                    workspace_root.join("alpha").to_string_lossy().as_ref(),
                    "multicode-alpha-running",
                    "/bin/bash",
                ]
            ));
            assert!(
                !command.args.iter().any(|arg| arg == "run"),
                "running workspaces should reuse the active container"
            );
            assert!(command.inherited_env.is_empty());
        });
    }

    #[test]
    fn apple_container_materializes_nested_readable_file_inside_isolated_mount() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            let home = root.path().join("home");
            let auth_dir = home.join(".local/share/opencode");
            let auth_file = auth_dir.join("auth.json");
            fs::create_dir_all(&workspace_root).expect("workspace root should exist");
            fs::create_dir_all(&auth_dir).expect("auth directory should exist");
            fs::write(&auth_file, r#"{"token":"apple"}"#).expect("auth file should exist");

            let runtime = apple_runtime(
                &root,
                IsolationConfig {
                    readable: vec![auth_file.to_string_lossy().into_owned()],
                    writable: Vec::new(),
                    isolated: vec![auth_dir.to_string_lossy().into_owned()],
                    tmpfs: Vec::new(),
                    add_skills_from: Vec::new(),
                    inherit_env: vec!["HOME".to_string()],
                    memory_high: None,
                    memory_max: None,
                    cpu: None,
                },
            );

            let command = runtime
                .build_run_command(
                    "alpha",
                    "multicode-alpha",
                    "secret",
                    31337,
                    &[(
                        "HOME".to_string(),
                        home.to_string_lossy().into_owned(),
                    )],
                )
                .await
                .expect("command should build");

            let isolated_storage = workspace_root
                .join(".multicode")
                .join("apple-container")
                .join("alpha")
                .join("isolate")
                .join(
                    auth_dir
                        .strip_prefix("/")
                        .expect("auth directory should be absolute"),
                );
            let materialized_auth = isolated_storage.join("auth.json");
            let host_auth_mount = format!(
                "type=bind,source={},target={}",
                auth_file.to_string_lossy(),
                auth_file.to_string_lossy()
            );
            let isolated_dir_mount = format!(
                "type=bind,source={},target={}",
                isolated_storage.to_string_lossy(),
                auth_dir.to_string_lossy()
            );

            assert!(
                command.args.iter().any(|arg| arg == &isolated_dir_mount),
                "isolated parent directory should still be mounted"
            );
            assert!(
                command.args.iter().all(|arg| arg != &host_auth_mount),
                "nested readable file should be materialized into the isolated backing tree instead of emitted as a separate bind mount"
            );
            assert_eq!(
                fs::read_to_string(&materialized_auth).expect("materialized auth should exist"),
                r#"{"token":"apple"}"#
            );
        });
    }

    #[test]
    fn apple_container_coalesces_added_skills_into_single_mount() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            let skills_root = root.path().join("workspace-skills");
            let skill_one = skills_root.join("skill-one");
            let skill_two = skills_root.join("skill-two");
            let container_skills_target =
                root.path().join("container-home/.config/opencode/skills");
            fs::create_dir_all(&workspace_root).expect("workspace root should exist");
            fs::create_dir_all(&skill_one).expect("first skill should exist");
            fs::create_dir_all(&skill_two).expect("second skill should exist");
            fs::write(skill_one.join("SKILL.md"), "# one").expect("first skill file should exist");
            fs::write(skill_two.join("SKILL.md"), "# two").expect("second skill file should exist");

            let runtime = AppleContainerRuntime {
                context: RuntimeContext {
                    runtime: RuntimeConfig {
                        backend: RuntimeBackend::AppleContainer,
                        image: Some("ghcr.io/example/multicode-java25:latest".to_string()),
                    },
                    workspace_directory_path: workspace_root.clone(),
                    expanded_isolation: ExpandedIsolationConfig {
                        readable: Vec::new(),
                        writable: Vec::new(),
                        isolated: Vec::new(),
                        tmpfs: Vec::new(),
                        added_skills: vec![
                            AddedSkillMount {
                                source: skill_one.clone(),
                                target: container_skills_target.join("skill-one"),
                            },
                            AddedSkillMount {
                                source: skill_two.clone(),
                                target: container_skills_target.join("skill-two"),
                            },
                        ],
                        inherit_env: Vec::new(),
                        memory_high_bytes: None,
                        memory_max_bytes: None,
                        cpu: None,
                    },
                    host_opencode_command: "/opt/opencode/bin/opencode".to_string(),
                    container_opencode_command: "opencode".to_string(),
                },
            };

            let command = runtime
                .build_run_command("alpha", "multicode-alpha", "secret", 31337, &[])
                .await
                .expect("command should build");

            let aggregated_source = workspace_root
                .join(".multicode")
                .join("apple-container")
                .join("alpha")
                .join("skills");
            let aggregated_mount = format!(
                "type=bind,source={},target={},readonly",
                aggregated_source.to_string_lossy(),
                container_skills_target.to_string_lossy()
            );

            assert!(
                command.args.iter().any(|arg| arg == &aggregated_mount),
                "apple backend should mount one aggregated skills directory"
            );
            assert!(
                command
                    .args
                    .iter()
                    .all(|arg| !arg.contains("container-home/.config/opencode/skills/skill-one")),
                "individual skill mounts should be omitted"
            );
            assert_eq!(
                fs::read_to_string(aggregated_source.join("skill-one/SKILL.md"))
                    .expect("aggregated skill one should exist"),
                "# one"
            );
            assert_eq!(
                fs::read_to_string(aggregated_source.join("skill-two/SKILL.md"))
                    .expect("aggregated skill two should exist"),
                "# two"
            );
        });
    }

    #[test]
    fn apple_container_aggregated_skill_mount_preserves_host_skills() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            let host_home = root.path().join("host-home");
            let host_skills_target = host_home.join(".config/opencode/skills");
            let host_skill = host_skills_target.join("host-skill");
            let added_skills_root = root.path().join("workspace-skills");
            let added_skill = added_skills_root.join("workspace-skill");
            fs::create_dir_all(&workspace_root).expect("workspace root should exist");
            fs::create_dir_all(&host_skill).expect("host skill should exist");
            fs::create_dir_all(&added_skill).expect("added skill should exist");
            fs::write(host_skill.join("SKILL.md"), "# host").expect("host skill file should exist");
            fs::write(added_skill.join("SKILL.md"), "# workspace")
                .expect("added skill file should exist");

            let runtime = AppleContainerRuntime {
                context: RuntimeContext {
                    runtime: RuntimeConfig {
                        backend: RuntimeBackend::AppleContainer,
                        image: Some("ghcr.io/example/multicode-java25:latest".to_string()),
                    },
                    workspace_directory_path: workspace_root.clone(),
                    expanded_isolation: ExpandedIsolationConfig {
                        readable: vec![host_home.join(".config/opencode")],
                        writable: Vec::new(),
                        isolated: Vec::new(),
                        tmpfs: Vec::new(),
                        added_skills: vec![AddedSkillMount {
                            source: added_skill.clone(),
                            target: host_skills_target.join("workspace-skill"),
                        }],
                        inherit_env: Vec::new(),
                        memory_high_bytes: None,
                        memory_max_bytes: None,
                        cpu: None,
                    },
                    host_opencode_command: "/opt/opencode/bin/opencode".to_string(),
                    container_opencode_command: "opencode".to_string(),
                },
            };

            let command = runtime
                .build_run_command("alpha", "multicode-alpha", "secret", 31337, &[])
                .await
                .expect("command should build");

            let aggregated_source = workspace_root
                .join(".multicode")
                .join("apple-container")
                .join("alpha")
                .join("skills");
            let aggregated_mount = format!(
                "type=bind,source={},target={},readonly",
                aggregated_source.to_string_lossy(),
                host_skills_target.to_string_lossy()
            );

            assert!(
                command.args.iter().any(|arg| arg == &aggregated_mount),
                "apple backend should expose a merged skills directory"
            );
            assert_eq!(
                fs::read_to_string(aggregated_source.join("host-skill/SKILL.md"))
                    .expect("host skill should be preserved"),
                "# host"
            );
            assert_eq!(
                fs::read_to_string(aggregated_source.join("workspace-skill/SKILL.md"))
                    .expect("workspace skill should be included"),
                "# workspace"
            );
        });
    }

    #[test]
    fn apple_container_reuses_aggregated_skills_directory_across_commands() {
        use std::os::unix::fs::MetadataExt;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            let host_home = root.path().join("host-home");
            let host_skills_target = host_home.join(".config/opencode/skills");
            let skill_root = root.path().join("workspace-skills");
            let skill = skill_root.join("machine-readable-pr");
            fs::create_dir_all(&workspace_root).expect("workspace root should exist");
            fs::create_dir_all(&skill).expect("skill should exist");
            fs::write(skill.join("SKILL.md"), "# pr").expect("skill file should exist");

            let runtime = AppleContainerRuntime {
                context: RuntimeContext {
                    runtime: RuntimeConfig {
                        backend: RuntimeBackend::AppleContainer,
                        image: Some("ghcr.io/example/multicode-java25:latest".to_string()),
                    },
                    workspace_directory_path: workspace_root.clone(),
                    expanded_isolation: ExpandedIsolationConfig {
                        readable: vec![host_home.join(".config/opencode")],
                        writable: Vec::new(),
                        isolated: Vec::new(),
                        tmpfs: Vec::new(),
                        added_skills: vec![AddedSkillMount {
                            source: skill.clone(),
                            target: host_skills_target.join("machine-readable-pr"),
                        }],
                        inherit_env: Vec::new(),
                        memory_high_bytes: None,
                        memory_max_bytes: None,
                        cpu: None,
                    },
                    host_opencode_command: "/opt/opencode/bin/opencode".to_string(),
                    container_opencode_command: "opencode".to_string(),
                },
            };

            runtime
                .build_run_command("alpha", "multicode-alpha", "secret", 31337, &[])
                .await
                .expect("run command should build");
            let aggregate_root = workspace_root
                .join(".multicode")
                .join("apple-container")
                .join("alpha")
                .join("skills");
            let first_ino = fs::metadata(&aggregate_root)
                .expect("aggregate root should exist")
                .ino();

            runtime
                .build_pty_command("alpha", None, &[], vec!["/bin/sh".to_string()])
                .await
                .expect("pty command should build");
            let second_ino = fs::metadata(&aggregate_root)
                .expect("aggregate root should still exist")
                .ino();

            assert_eq!(
                first_ino, second_ino,
                "apple backend should update the aggregated skills directory in place so existing mounts stay valid"
            );
            assert_eq!(
                fs::read_to_string(aggregate_root.join("machine-readable-pr/SKILL.md"))
                    .expect("aggregated skill should remain present"),
                "# pr"
            );
        });
    }
}
