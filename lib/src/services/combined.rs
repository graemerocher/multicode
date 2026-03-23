use std::{
    env,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::SystemTime,
};

use tokio::process::Command;
use uuid::Uuid;

fn shell_escape_arg(arg: &str) -> String {
    if arg.is_empty() {
        "''".to_string()
    } else if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | ':' | '_' | '-' | '.' | '='))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

fn format_command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(shell_escape_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

fn append_systemd_run_inherit_env(args: &mut Vec<String>, env: &[(String, String)]) {
    for (name, _) in env {
        args.push("--setenv".to_string());
        args.push(name.clone());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCommand {
    pub args: Vec<String>,
    pub inherited_env: Vec<(String, String)>,
}

use crate::{
    TransientWorkspaceSnapshot, WorkspaceArchiveFormat, WorkspaceManager, WorkspaceManagerError,
    database::Database, logging,
};

use super::{
    GithubStatusService, GithubStatusServiceError, WorkspaceDirectoryError,
    config::{
        AddedSkillMount, Config, ExpandedIsolationConfig, expand_shell_path, path_looks_like_file,
        read_config, resolve_opencode_command, validate_handler_config, validate_remote_config,
        validate_tool_config_entries, validate_workspace_key,
    },
    multicode_metadata_service, opencode_client_service, persistent_storage,
    resource_usage_service, root_session_service, transient_storage, usage_aggregation_service,
    workspace_archive::ArchiveWorkspaceEntry,
    workspace_directory,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MountKind {
    Readable,
    Writable,
    Isolated,
    Tmpfs,
}

#[derive(Debug, Clone)]
struct MountSpec {
    target: PathBuf,
    source: Option<PathBuf>,
    kind: MountKind,
    is_file: bool,
}

impl MountSpec {
    fn new(target: PathBuf, source: Option<PathBuf>, kind: MountKind) -> Self {
        let is_file = match source.as_ref() {
            Some(source) => std::fs::metadata(source)
                .map(|metadata| metadata.is_file())
                .unwrap_or(false),
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

    fn resolve_backing_path(path: &Path, prior_mounts: &[ResolvedMountSpec]) -> PathBuf {
        for prior_mount in prior_mounts.iter().rev() {
            if path == prior_mount.mount.target || path.starts_with(&prior_mount.mount.target) {
                let relative = path
                    .strip_prefix(&prior_mount.mount.target)
                    .expect("path should be under prior mount target");
                return prior_mount.effective_source.join(relative);
            }
        }
        path.to_path_buf()
    }

    fn resolve_effective(&self, prior_mounts: &[ResolvedMountSpec]) -> ResolvedMountSpec {
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
            effective_target,
            effective_source,
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedMountSpec {
    mount: MountSpec,
    effective_target: PathBuf,
    effective_source: PathBuf,
}

impl ResolvedMountSpec {
    async fn prepare_source_node(&self, owns_node: bool) -> Result<(), CombinedServiceError> {
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

    async fn prepare_target_node(&self, owns_node: bool) -> Result<(), CombinedServiceError> {
        let should_materialize = owns_node
            && (!self.mount.is_file
                || matches!(self.mount.kind, MountKind::Writable | MountKind::Isolated));
        self.prepare_node(&self.effective_target, should_materialize, None)
            .await
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
}

#[derive(Debug, Clone)]
pub struct CombinedService {
    pub config: Config,
    pub manager: Arc<WorkspaceManager>,
    pub database: Database,
    github_status_service: GithubStatusService,
    workspace_directory_path: PathBuf,
    expanded_isolation: ExpandedIsolationConfig,
    opencode_command: String,
    github_git_credentials_env: Option<GithubGitCredentialsEnv>,
}

#[derive(Debug, Clone)]
struct GithubGitCredentialsEnv {
    username: String,
    token: String,
}

impl CombinedService {
    pub async fn from_config_path(
        config_path: impl AsRef<Path>,
    ) -> Result<Self, CombinedServiceError> {
        let config_path = config_path.as_ref();
        let config = read_config(config_path).await?;
        Self::from_config_with_path(config, Some(config_path)).await
    }

    pub async fn from_config(config: Config) -> Result<Self, CombinedServiceError> {
        Self::from_config_with_path(config, None).await
    }

    async fn from_config_with_path(
        config: Config,
        config_path: Option<&Path>,
    ) -> Result<Self, CombinedServiceError> {
        validate_tool_config_entries(&config.tool)?;
        validate_handler_config(&config.handler)?;
        validate_remote_config(config.remote.as_ref())?;
        let opencode_command = resolve_opencode_command(&config.opencode)?;
        let workspace_directory_path = expand_shell_path(&config.workspace_directory)?;
        if let Err(err) = logging::enable_workspace_file_logging(&workspace_directory_path).await {
            logging::log_file_enable_failed(
                &workspace_directory_path
                    .join(".multicode")
                    .join("multicode.log"),
                &err,
            );
        } else {
            logging::log_file_enabled(
                &workspace_directory_path
                    .join(".multicode")
                    .join("multicode.log"),
            );
        }
        let expanded_isolation =
            ExpandedIsolationConfig::from_config(&config.isolation, config_path)?;

        let manager = Arc::new(WorkspaceManager::new());

        workspace_directory(&manager, &workspace_directory_path).await?;
        let database = Database::open_in_workspace(&workspace_directory_path).await?;
        let github_status_service =
            GithubStatusService::new(database.clone(), config.github.token.clone()).await?;
        let github_git_credentials_env =
            github_git_credentials_env_from_config(&config, &github_status_service).await?;

        let persistent_path = workspace_directory_path
            .join(".multicode")
            .join("persistent");
        let transient_link = workspace_directory_path
            .join(".multicode")
            .join("transient");

        spawn_persistent_storage(
            manager.clone(),
            persistent_path,
            workspace_directory_path.clone(),
        );
        spawn_transient_storage(manager.clone(), transient_link);
        spawn_opencode_client_service(manager.clone());
        spawn_root_session_service(manager.clone());
        spawn_multicode_metadata_service(manager.clone());
        spawn_usage_aggregation_service(manager.clone());
        spawn_resource_usage_service(manager.clone());

        Ok(Self {
            config,
            manager,
            database,
            github_status_service,
            workspace_directory_path,
            expanded_isolation,
            opencode_command,
            github_git_credentials_env,
        })
    }

    pub fn workspace_directory_path(&self) -> &Path {
        &self.workspace_directory_path
    }

    pub fn added_skill_mounts(&self) -> &[AddedSkillMount] {
        &self.expanded_isolation.added_skills
    }

    pub fn github_status_service(&self) -> &GithubStatusService {
        &self.github_status_service
    }

    pub async fn create_workspace(&self, key: &str) -> Result<(), CombinedServiceError> {
        let key = validate_workspace_key(key)?;

        let workspace_path = self.workspace_directory_path.join(&key);
        tokio::fs::create_dir_all(&workspace_path).await?;
        self.manager.add(key.clone())?;
        self.manager.get_workspace(&key)?.update(|snapshot| {
            if snapshot.persistent.created_at.is_none() {
                snapshot.persistent.created_at = Some(SystemTime::now());
                true
            } else {
                false
            }
        });
        Ok(())
    }

    pub async fn start_workspace(&self, key: &str) -> Result<(), CombinedServiceError> {
        let key = validate_workspace_key(key)?;

        self.ensure_workspace_not_archived(&key)?;
        let workspace = self.manager.get_workspace(&key)?;
        let workspace_path = self.workspace_directory_path.join(&key);
        tokio::fs::create_dir_all(&workspace_path).await?;

        let password = generate_random_password();
        let port = pick_random_free_port().await?;
        let unit = generate_transient_unit_name();
        let args = self
            .build_systemd_bwrap_command(&key, &password, port, &unit)
            .await?;
        tracing::info!(
            workspace_key = %key,
            command = %format_command_line("systemd-run", &args.args),
            "starting application via systemd-run opencode serve"
        );
        let mut command = Command::new("systemd-run");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .args(&args.args);
        for (name, value) in &args.inherited_env {
            command.env(name, value);
        }
        let output = command.output().await?;

        if !output.status.success() {
            return Err(CombinedServiceError::StartWorkspaceFailed {
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let uri = format!("http://opencode:{password}@127.0.0.1:{port}/");
        let transient = TransientWorkspaceSnapshot {
            uri,
            unit: unit.clone(),
        };
        let mut replaced = false;
        workspace.update(|snapshot| {
            if snapshot.transient.is_none() {
                snapshot.transient = Some(transient.clone());
                replaced = true;
                true
            } else {
                false
            }
        });

        if !replaced {
            stop_systemd_unit(&unit).await?;
            return Err(CombinedServiceError::TransientSnapshotAlreadyPresent(
                key.to_string(),
            ));
        }

        Ok(())
    }

    pub async fn stop_workspace(&self, key: &str) -> Result<(), CombinedServiceError> {
        let key = validate_workspace_key(key)?;
        let workspace = self.manager.get_workspace(&key)?;
        let workspace_rx = workspace.subscribe();
        let unit = workspace_rx
            .borrow()
            .transient
            .as_ref()
            .map(|transient| transient.unit.clone())
            .ok_or_else(|| CombinedServiceError::TransientSnapshotMissing(key.clone()))?;

        stop_systemd_unit(&unit).await?;
        workspace.update(|snapshot| {
            if snapshot.transient.is_some() {
                snapshot.transient = None;
                true
            } else {
                false
            }
        });
        Ok(())
    }

    /// Build a command to run a user-defined exec-type tool.
    pub async fn build_exec_tool_command(
        &self,
        key: &str,
        exec_command: &str,
    ) -> Result<SpawnCommand, CombinedServiceError> {
        let exec_command = exec_command.trim();
        if exec_command.is_empty() {
            return Err(CombinedServiceError::InvalidToolExecution(
                "exec tool command must not be empty".to_string(),
            ));
        }

        self.build_pty_tool_command(key, vec![exec_command.to_string()])
            .await
    }

    /// Build a command to run a tool in an isolate, such as a user-defined exec-type tool, or the review tool.
    pub async fn build_pty_tool_command(
        &self,
        key: &str,
        command: Vec<String>,
    ) -> Result<SpawnCommand, CombinedServiceError> {
        let key = validate_workspace_key(key)?;
        self.ensure_workspace_not_archived(&key)?;
        if command.is_empty() {
            return Err(CombinedServiceError::InvalidToolExecution(
                "PTY tool command must not be empty".to_string(),
            ));
        }

        let workspace_path = self.workspace_directory_path.join(&key);
        tokio::fs::create_dir_all(&workspace_path).await?;

        let unit = generate_transient_unit_name();
        let mut args = vec![
            "--user".to_string(),
            "--wait".to_string(),
            "--collect".to_string(),
            "--pty".to_string(),
        ];
        let inherited_env = self
            .sandbox_env_pairs(Vec::<(String, String)>::new())
            .await?;
        append_systemd_run_inherit_env(&mut args, &inherited_env);
        args.push("--unit".to_string());
        args.push(unit);
        self.append_systemd_limits(&mut args);
        self.append_bwrap_sandbox_args(&mut args, &key).await?;
        args.extend(command);

        Ok(SpawnCommand {
            args,
            inherited_env,
        })
    }

    pub async fn archive_workspace(
        &self,
        key: &str,
        progress_tx: tokio::sync::watch::Sender<String>,
    ) -> Result<(), CombinedServiceError> {
        let key = validate_workspace_key(key)?;
        let workspace = self.manager.get_workspace(&key)?;
        let snapshot = workspace.subscribe().borrow().clone();
        if snapshot.persistent.archived {
            return Err(CombinedServiceError::WorkspaceArchived(key));
        }
        if snapshot.transient.is_some() || snapshot.opencode_client.is_some() {
            return Err(CombinedServiceError::ArchiveWorkspaceRunning(key));
        }

        self.ensure_no_archive_conflict(&key).await?;

        let workspace_path = self.workspace_directory_path.join(&key);
        let isolate_workspace_path = self.isolate_path_for_key(&key);
        let archive_entry = ArchiveWorkspaceEntry::new(&key, WorkspaceArchiveFormat::TarZstd);
        let isolate_archive_path = archive_entry.to_isolate_path(&self.workspace_directory_path);
        let isolate_exists = tokio::fs::metadata(&isolate_workspace_path)
            .await
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);
        let archive_path = archive_entry.to_path(&self.workspace_directory_path);
        let _ = progress_tx.send(format!("Compressing '{key}'"));
        let status = self
            .compress_directory_to_archive(&archive_path, &self.workspace_directory_path, &key)
            .await?;
        self.ensure_archive_command_succeeded(key.clone(), status)?;

        if isolate_exists {
            if let Some(parent) = isolate_archive_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let isolate_status = self
                .compress_directory_to_archive(
                    &isolate_archive_path,
                    &self
                        .workspace_directory_path
                        .join(".multicode")
                        .join("isolate"),
                    &key,
                )
                .await?;
            if let Err(err) = self.ensure_archive_command_succeeded(key.clone(), isolate_status) {
                let _ = tokio::fs::remove_file(&archive_path).await;
                return Err(err);
            }
        }

        let _ = progress_tx.send(format!("Removing live directory for '{key}'"));
        tokio::fs::remove_dir_all(&workspace_path).await?;
        if isolate_exists {
            tokio::fs::remove_dir_all(&isolate_workspace_path).await?;
        }
        workspace.update(|snapshot| {
            snapshot.persistent.archived = true;
            snapshot.persistent.archive_format = Some(WorkspaceArchiveFormat::TarZstd);
            true
        });
        let _ = progress_tx.send(format!("Archived '{key}'"));
        Ok(())
    }

    pub async fn unarchive_workspace(
        &self,
        key: &str,
        progress_tx: tokio::sync::watch::Sender<String>,
    ) -> Result<(), CombinedServiceError> {
        let key = validate_workspace_key(key)?;
        let workspace = self.manager.get_workspace(&key)?;

        let workspace_path = self.workspace_directory_path.join(&key);
        if tokio::fs::metadata(&workspace_path)
            .await
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
        {
            workspace.update(|snapshot| {
                snapshot.persistent.archived = false;
                snapshot.persistent.archive_format = None;
                true
            });
            let _ = progress_tx.send(format!("Reactivated legacy archived workspace '{key}'"));
            return Ok(());
        }

        let archive_path = match self.find_archive_path_for_key(&key).await {
            Ok(path) => path,
            Err(CombinedServiceError::ArchiveNotFound(_)) => {
                let snapshot = workspace.subscribe().borrow().clone();
                if !snapshot.persistent.archived {
                    return Err(CombinedServiceError::WorkspaceNotArchived(key));
                }
                return Err(CombinedServiceError::ArchiveNotFound(key));
            }
            Err(err) => return Err(err),
        };
        let archive_entry = ArchiveWorkspaceEntry::parse(&archive_path)
            .ok_or_else(|| CombinedServiceError::ArchiveNotFound(key.clone()))?;
        let archive_format = archive_entry.format.clone();
        let isolate_archive_path = ArchiveWorkspaceEntry::new(&key, archive_format.clone())
            .to_isolate_path(&self.workspace_directory_path);
        let _ = progress_tx.send(format!("Extracting '{}'", archive_path.display()));
        let status = self
            .extract_archive_to_directory(
                &archive_path,
                archive_format.clone(),
                &self.workspace_directory_path,
            )
            .await?;
        self.ensure_archive_command_succeeded(key.clone(), status)?;

        if tokio::fs::metadata(&isolate_archive_path).await.is_ok() {
            let isolate_status = self
                .extract_archive_to_directory(
                    &isolate_archive_path,
                    archive_format,
                    &self
                        .workspace_directory_path
                        .join(".multicode")
                        .join("isolate"),
                )
                .await?;
            self.ensure_archive_command_succeeded(key.clone(), isolate_status)?;
        }

        tokio::fs::remove_file(&archive_path).await?;
        match tokio::fs::remove_file(&isolate_archive_path).await {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        workspace.update(|snapshot| {
            snapshot.persistent.archived = false;
            snapshot.persistent.archive_format = None;
            true
        });
        let _ = progress_tx.send(format!("Unarchived '{key}'"));
        Ok(())
    }

    pub fn opencode_command(&self) -> &str {
        &self.opencode_command
    }

    async fn build_systemd_bwrap_command(
        &self,
        key: &str,
        password: &str,
        port: u16,
        unit: &str,
    ) -> Result<SpawnCommand, CombinedServiceError> {
        let mut args = vec!["--user".to_string(), "--no-block".to_string()];
        let inherited_env = self
            .sandbox_env_pairs(vec![
                (
                    "OPENCODE_SERVER_USERNAME".to_string(),
                    "opencode".to_string(),
                ),
                ("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string()),
            ])
            .await?;
        append_systemd_run_inherit_env(&mut args, &inherited_env);
        args.push("--unit".to_string());
        args.push(unit.to_string());
        self.append_systemd_limits(&mut args);

        self.append_bwrap_sandbox_args(&mut args, key).await?;
        args.push(self.opencode_command.clone());
        args.push("serve".to_string());
        args.push("--hostname".to_string());
        args.push("127.0.0.1".to_string());
        args.push("--port".to_string());
        args.push(port.to_string());

        Ok(SpawnCommand {
            args,
            inherited_env,
        })
    }

    fn append_systemd_limits(&self, args: &mut Vec<String>) {
        if let Some(memory_high_bytes) = self.expanded_isolation.memory_high_bytes {
            args.push("-p".to_string());
            args.push(format!("MemoryHigh={memory_high_bytes}"));
        }
        if let Some(memory_max_bytes) = self.expanded_isolation.memory_max_bytes {
            args.push("-p".to_string());
            args.push(format!("MemoryMax={memory_max_bytes}"));
            args.push("-p".to_string());
            args.push("MemorySwapMax=0".to_string());
        }
        if let Some(cpu) = &self.expanded_isolation.cpu {
            args.push("-p".to_string());
            args.push(format!("CPUQuota={cpu}"));
        }
    }

    async fn sandbox_env_pairs(
        &self,
        extra_env: Vec<(String, String)>,
    ) -> Result<Vec<(String, String)>, CombinedServiceError> {
        let mut env = self.github_git_credentials_env_vars();
        env.extend(extra_env);
        env.extend(
            self.expanded_isolation
                .inherit_env
                .iter()
                .filter_map(|env_name| {
                    env::var(env_name)
                        .ok()
                        .map(|env_value| (env_name.clone(), env_value))
                }),
        );
        Ok(env)
    }

    async fn append_bwrap_sandbox_args(
        &self,
        args: &mut Vec<String>,
        key: &str,
    ) -> Result<(), CombinedServiceError> {
        let workspace_path = self.workspace_directory_path.join(key);
        let workspace_path_str = workspace_path.to_string_lossy().into_owned();

        args.push("bwrap".to_string());
        args.push("--chdir".to_string());
        args.push(workspace_path_str.clone());

        args.push("--ro-bind".to_string());
        args.push("/".to_string());
        args.push("/".to_string());

        let mut mount_specs = Vec::new();
        mount_specs.extend(
            self.expanded_isolation
                .readable
                .iter()
                .cloned()
                .map(|path| MountSpec::new(path, None, MountKind::Readable)),
        );
        mount_specs.extend(
            self.expanded_isolation
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
            self.expanded_isolation
                .isolated
                .iter()
                .cloned()
                .map(|path| {
                    let source = self.isolated_storage_path(key, &path);
                    MountSpec::new(path.clone(), Some(source), MountKind::Isolated)
                }),
        );
        mount_specs.extend(
            self.expanded_isolation
                .tmpfs
                .iter()
                .cloned()
                .map(|path| MountSpec::new(path, None, MountKind::Tmpfs)),
        );
        mount_specs.extend(
            self.expanded_isolation
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
            resolved_mount.prepare_target_node(owns_node).await?;
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
        self.isolate_path_for_key(key).join(relative)
    }

    fn isolate_path_for_key(&self, key: &str) -> PathBuf {
        self.workspace_directory_path
            .join(".multicode")
            .join("isolate")
            .join(key)
    }

    fn github_git_credentials_env_vars(&self) -> Vec<(String, String)> {
        let Some(github_git_credentials_env) = &self.github_git_credentials_env else {
            return Vec::new();
        };

        let helper = r#"!f() { test "$1" = get || exit 0; echo username=$MULTICODE_GITHUB_USERNAME; echo password=$MULTICODE_GITHUB_TOKEN; }; f"#;
        vec![
            (
                "MULTICODE_GITHUB_USERNAME".to_string(),
                github_git_credentials_env.username.clone(),
            ),
            (
                "MULTICODE_GITHUB_TOKEN".to_string(),
                github_git_credentials_env.token.clone(),
            ),
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            (
                "GIT_CONFIG_KEY_0".to_string(),
                "credential.helper".to_string(),
            ),
            ("GIT_CONFIG_VALUE_0".to_string(), helper.to_string()),
        ]
    }

    async fn compress_directory_to_archive(
        &self,
        archive_path: &Path,
        base_directory: &Path,
        entry_name: &str,
    ) -> Result<ExitStatus, CombinedServiceError> {
        Ok(Command::new("tar")
            .arg("--use-compress-program")
            .arg("zstd -T0")
            .arg("-cf")
            .arg(archive_path)
            .arg("-C")
            .arg(base_directory)
            .arg(entry_name)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .await?)
    }

    async fn extract_archive_to_directory(
        &self,
        archive_path: &Path,
        format: WorkspaceArchiveFormat,
        destination_directory: &Path,
    ) -> Result<ExitStatus, CombinedServiceError> {
        let status = match format {
            WorkspaceArchiveFormat::TarZstd => {
                Command::new("tar")
                    .arg("--use-compress-program")
                    .arg("zstd -d -T0")
                    .arg("-xf")
                    .arg(archive_path)
                    .arg("-C")
                    .arg(destination_directory)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .status()
                    .await?
            }
            WorkspaceArchiveFormat::TarXz => {
                Command::new("tar")
                    .arg("-xJf")
                    .arg(archive_path)
                    .arg("-C")
                    .arg(destination_directory)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .status()
                    .await?
            }
            WorkspaceArchiveFormat::Zip => {
                Command::new("unzip")
                    .arg("-q")
                    .arg(archive_path)
                    .arg("-d")
                    .arg(destination_directory)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .status()
                    .await?
            }
        };
        Ok(status)
    }

    fn ensure_archive_command_succeeded(
        &self,
        key: String,
        status: ExitStatus,
    ) -> Result<(), CombinedServiceError> {
        if status.success() {
            Ok(())
        } else {
            Err(CombinedServiceError::ArchiveCommandFailed {
                key,
                status: status.code(),
            })
        }
    }

    fn ensure_workspace_not_archived(&self, key: &str) -> Result<(), CombinedServiceError> {
        let workspace = self.manager.get_workspace(key)?;
        if workspace.subscribe().borrow().persistent.archived {
            return Err(CombinedServiceError::WorkspaceArchived(key.to_string()));
        }
        Ok(())
    }

    async fn ensure_no_archive_conflict(&self, key: &str) -> Result<(), CombinedServiceError> {
        for format in [
            WorkspaceArchiveFormat::TarZstd,
            WorkspaceArchiveFormat::TarXz,
            WorkspaceArchiveFormat::Zip,
        ] {
            let archive_entry = ArchiveWorkspaceEntry::new(key, format.clone());
            let archive_path = archive_entry.to_path(&self.workspace_directory_path);
            if tokio::fs::metadata(&archive_path).await.is_ok() {
                return Err(CombinedServiceError::ArchiveConflict {
                    key: key.to_string(),
                    path: archive_path,
                });
            }
            let isolate_archive_path =
                archive_entry.to_isolate_path(&self.workspace_directory_path);
            if tokio::fs::metadata(&isolate_archive_path).await.is_ok() {
                return Err(CombinedServiceError::ArchiveConflict {
                    key: key.to_string(),
                    path: isolate_archive_path,
                });
            }
        }
        Ok(())
    }

    async fn find_archive_path_for_key(&self, key: &str) -> Result<PathBuf, CombinedServiceError> {
        let mut found = None;
        for format in [
            WorkspaceArchiveFormat::TarZstd,
            WorkspaceArchiveFormat::TarXz,
            WorkspaceArchiveFormat::Zip,
        ] {
            let archive_path =
                ArchiveWorkspaceEntry::new(key, format).to_path(&self.workspace_directory_path);
            if tokio::fs::metadata(&archive_path).await.is_ok() {
                if found.is_some() {
                    return Err(CombinedServiceError::ArchiveConflict {
                        key: key.to_string(),
                        path: archive_path,
                    });
                }
                found = Some(archive_path);
            }
        }
        found.ok_or_else(|| CombinedServiceError::ArchiveNotFound(key.to_string()))
    }
}

async fn github_git_credentials_env_from_config(
    config: &Config,
    github_status_service: &GithubStatusService,
) -> Result<Option<GithubGitCredentialsEnv>, CombinedServiceError> {
    if !config.github.populate_git_credentials {
        return Ok(None);
    }
    if config.github.token.is_none() {
        return Err(CombinedServiceError::GithubGitCredentials(
            "`github.populate-git-credentials` requires `github.token` to be configured"
                .to_string(),
        ));
    }

    let token = github_status_service.resolved_github_token().await?;
    let username = github_status_service
        .authenticated_login()
        .await?
        .ok_or_else(|| {
            CombinedServiceError::GithubGitCredentials(
                "GitHub authenticated login unavailable for git credentials".to_string(),
            )
        })?;
    Ok(Some(GithubGitCredentialsEnv { username, token }))
}

#[derive(Debug)]
pub enum CombinedServiceError {
    Io(std::io::Error),
    ParseToml(toml::de::Error),
    ShellExpand(String),
    Manager(WorkspaceManagerError),
    InvalidWorkspaceKey(String),
    InvalidIsolationPath {
        field: String,
        path: PathBuf,
    },
    InvalidIsolationSize {
        field: String,
        value: String,
        message: String,
    },
    TransientSnapshotMissing(String),
    TransientSnapshotAlreadyPresent(String),
    StopWorkspaceFailed {
        status: Option<i32>,
        stderr: String,
    },
    StartWorkspaceFailed {
        status: Option<i32>,
        stderr: String,
    },
    WorkspaceDirectory(WorkspaceDirectoryError),
    Database(crate::database::DatabaseError),
    GithubStatusService(GithubStatusServiceError),
    GithubGitCredentials(String),
    InvalidToolConfig {
        index: usize,
        message: String,
    },
    InvalidHandlerConfig {
        field: String,
        message: String,
    },
    InvalidRemoteConfig {
        field: String,
        message: String,
    },
    InvalidToolExecution(String),
    WorkspaceArchived(String),
    WorkspaceNotArchived(String),
    ArchiveWorkspaceRunning(String),
    ArchiveConflict {
        key: String,
        path: PathBuf,
    },
    ArchiveNotFound(String),
    ArchiveCommandFailed {
        key: String,
        status: Option<i32>,
    },
    OpencodeCommandNotFound {
        candidates: Vec<String>,
    },
}

impl From<std::io::Error> for CombinedServiceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<toml::de::Error> for CombinedServiceError {
    fn from(value: toml::de::Error) -> Self {
        Self::ParseToml(value)
    }
}

impl From<WorkspaceDirectoryError> for CombinedServiceError {
    fn from(value: WorkspaceDirectoryError) -> Self {
        Self::WorkspaceDirectory(value)
    }
}

impl From<WorkspaceManagerError> for CombinedServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

impl From<crate::database::DatabaseError> for CombinedServiceError {
    fn from(value: crate::database::DatabaseError) -> Self {
        Self::Database(value)
    }
}

impl From<GithubStatusServiceError> for CombinedServiceError {
    fn from(value: GithubStatusServiceError) -> Self {
        Self::GithubStatusService(value)
    }
}

fn generate_random_password() -> String {
    Uuid::new_v4().as_simple().to_string()
}

fn generate_transient_unit_name() -> String {
    format!("multicode-{}.service", Uuid::new_v4().as_simple())
}

async fn pick_random_free_port() -> Result<u16, CombinedServiceError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

async fn stop_systemd_unit(unit: &str) -> Result<(), CombinedServiceError> {
    let args = stop_systemd_args(unit);
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

fn stop_systemd_args(unit: &str) -> Vec<String> {
    vec![
        "--user".to_string(),
        "stop".to_string(),
        "--no-block".to_string(),
        unit.to_string(),
    ]
}

fn spawn_persistent_storage(
    manager: Arc<WorkspaceManager>,
    persistent_path: PathBuf,
    workspace_directory: PathBuf,
) {
    tokio::spawn(async move {
        if let Err(err) = persistent_storage(manager, persistent_path, workspace_directory).await {
            tracing::error!(error = ?err, "persistent storage service exited with error");
        }
    });
}

fn spawn_transient_storage(manager: Arc<WorkspaceManager>, transient_link: PathBuf) {
    tokio::spawn(async move {
        if let Err(err) = transient_storage(manager, transient_link).await {
            tracing::error!(error = ?err, "transient storage service exited with error");
        }
    });
}

fn spawn_opencode_client_service(manager: Arc<WorkspaceManager>) {
    tokio::spawn(async move {
        if let Err(err) = opencode_client_service(manager).await {
            tracing::error!(error = ?err, "opencode client service exited with error");
        }
    });
}

fn spawn_root_session_service(manager: Arc<WorkspaceManager>) {
    tokio::spawn(async move {
        if let Err(err) = root_session_service(manager).await {
            tracing::error!(error = ?err, "root session service exited with error");
        }
    });
}

fn spawn_multicode_metadata_service(manager: Arc<WorkspaceManager>) {
    tokio::spawn(async move {
        if let Err(err) = multicode_metadata_service(manager).await {
            tracing::error!(error = ?err, "multicode metadata service exited with error");
        }
    });
}

fn spawn_usage_aggregation_service(manager: Arc<WorkspaceManager>) {
    tokio::spawn(async move {
        if let Err(err) = usage_aggregation_service(manager).await {
            tracing::error!(error = ?err, "usage aggregation service terminated");
        }
    });
}

fn spawn_resource_usage_service(manager: Arc<WorkspaceManager>) {
    tokio::spawn(async move {
        if let Err(err) = resource_usage_service(manager).await {
            tracing::error!(error = ?err, "resource usage service terminated");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{GithubTokenConfig, ToolType};
    use crate::test_support::ENV_VAR_LOCK;
    use diesel::{QueryableByName, RunQueryDsl, sql_query, sqlite::SqliteConnection};
    use std::os::unix::fs::PermissionsExt;

    #[derive(Debug, QueryableByName)]
    struct SqliteTableExistsRow {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        table_exists: bool,
    }

    fn table_exists(
        connection: &mut SqliteConnection,
        table_name: &str,
    ) -> Result<bool, diesel::result::Error> {
        let rows = sql_query(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?) AS table_exists",
        )
        .bind::<diesel::sql_types::Text, _>(table_name)
        .load::<SqliteTableExistsRow>(connection)?;
        Ok(rows.first().is_some_and(|row| row.table_exists))
    }

    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

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
                "multicode-combined-service-{}-{}",
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
        fn set(key: &'static str, value: &Path) -> Self {
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

    fn config_with_isolation(workspace_directory: &str) -> String {
        format!(
            "workspace-directory = \"{workspace_directory}\"\ncreate-ssh-agent = false\n\n[isolation]\n"
        )
    }

    #[test]
    fn config_parses_github_token_env_source() {
        let config: Config = toml::from_str(
            r#"
workspace-directory = "/tmp/workspaces"

[github]
token = { env = "GITHUB_TOKEN" }

[isolation]
"#,
        )
        .expect("config should parse");

        assert_eq!(
            config.github.token,
            Some(GithubTokenConfig {
                env: Some("GITHUB_TOKEN".to_string()),
                command: None,
            })
        );
    }

    #[test]
    fn config_parses_github_populate_git_credentials_flag() {
        let config: Config = toml::from_str(
            r#"
workspace-directory = "/tmp/workspaces"

[github]
populate-git-credentials = true
token = { env = "GITHUB_TOKEN" }

[isolation]
"#,
        )
        .expect("config should parse");

        assert!(config.github.populate_git_credentials);
        assert_eq!(
            config.github.token,
            Some(GithubTokenConfig {
                env: Some("GITHUB_TOKEN".to_string()),
                command: None,
            })
        );
    }

    #[test]
    fn config_parses_github_token_command_source() {
        let config: Config = toml::from_str(
            r#"
workspace-directory = "/tmp/workspaces"

[github]
token = { command = "gh auth token" }

[isolation]
"#,
        )
        .expect("config should parse");

        assert_eq!(
            config.github.token,
            Some(GithubTokenConfig {
                env: None,
                command: Some("gh auth token".to_string()),
            })
        );
    }

    fn make_executable(path: &Path) {
        let mut perms = fs::metadata(path)
            .expect("executable metadata should be readable")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("executable permissions should be set");
    }

    #[test]
    fn github_git_credentials_helper_script_returns_expected_helper() {
        assert_eq!(
            r#"!f() { test "$1" = get || exit 0; echo username=$MULTICODE_GITHUB_USERNAME; echo password=$MULTICODE_GITHUB_TOKEN; }; f"#,
            r#"!f() { test "$1" = get || exit 0; echo username=$MULTICODE_GITHUB_USERNAME; echo password=$MULTICODE_GITHUB_TOKEN; }; f"#
        );
    }

    #[test]
    fn github_git_credentials_env_requires_token_when_enabled() {
        let config: Config = toml::from_str(
            r#"
workspace-directory = "/tmp/workspaces"

[github]
populate-git-credentials = true

[isolation]
"#,
        )
        .expect("config should parse");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        let err = runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            tokio::fs::create_dir_all(&workspace_root)
                .await
                .expect("workspace root should exist");
            let database = Database::open_in_workspace(&workspace_root)
                .await
                .expect("database should open");
            let github_status_service = GithubStatusService::new(database, None)
                .await
                .expect("service should construct");
            github_git_credentials_env_from_config(&config, &github_status_service)
                .await
                .expect_err("missing token should fail")
        });
        assert!(matches!(err, CombinedServiceError::GithubGitCredentials(_)));
    }

    #[test]
    fn combined_service_expands_workspace_directory_and_starts_subservices() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");

            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(workspace_directory.join("alpha"))
                .expect("workspace directory should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(&config_path, config_with_isolation("~/workspaces"))
                .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            assert_eq!(service.config.workspace_directory, "~/workspaces");

            let alpha = service
                .manager
                .get_workspace("alpha")
                .expect("workspace should have been scanned and added")
                .subscribe();
            let alpha_snapshot = alpha.borrow().clone();
            assert_eq!(
                alpha_snapshot.persistent,
                crate::PersistentWorkspaceSnapshot::default()
            );
            assert!(alpha_snapshot.transient.is_none());
            assert!(alpha_snapshot.opencode_client.is_none());
            assert!(alpha_snapshot.root_session_id.is_none());
            assert!(alpha_snapshot.root_session_title.is_none());
            assert!(alpha_snapshot.root_session_status.is_none());
            assert!(alpha_snapshot.usage_total_tokens.is_none());
            assert!(alpha_snapshot.usage_total_cost.is_none());
            assert!(alpha_snapshot.usage_cpu_percent.is_none());
            assert!(alpha_snapshot.usage_ram_bytes.is_none());
            assert!(alpha_snapshot.oom_kill_count.is_none());

            let multicode_dir = workspace_directory.join(".multicode");
            let persistent_dir = multicode_dir.join("persistent");
            let transient_link = multicode_dir.join("transient");
            let database_path = multicode_dir.join("cache.sqlite");

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if tokio::fs::metadata(&persistent_dir)
                        .await
                        .map(|m| m.is_dir())
                        .unwrap_or(false)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("persistent storage directory should be created");

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if tokio::fs::symlink_metadata(&transient_link)
                        .await
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("transient storage link should be created");

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if tokio::fs::metadata(&database_path)
                        .await
                        .map(|m| m.is_file())
                        .unwrap_or(false)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("database file should be created");

            let database = service.database.clone();
            let (has_migrations, has_github_statuses, has_workspace_metadata) =
                tokio::task::spawn_blocking(move || {
                    let mut connection = database
                        .pool()
                        .get()
                        .expect("database connection should be available");
                    let has_migrations =
                        table_exists(&mut connection, "__diesel_schema_migrations")
                            .expect("diesel migrations table should exist");
                    let has_github_statuses = table_exists(&mut connection, "github_link_statuses")
                        .expect("github status cache table should exist");
                    let has_workspace_metadata =
                        table_exists(&mut connection, "workspace_metadata")
                            .expect("workspace table check should succeed");
                    (has_migrations, has_github_statuses, has_workspace_metadata)
                })
                .await
                .expect("database table checks should finish");
            assert!(has_migrations);
            assert!(has_github_statuses);
            assert!(!has_workspace_metadata);
        });
    }

    #[test]
    fn combined_service_requires_workspace_directory_in_config() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let config_path = root.path().join("config.toml");
            fs::write(&config_path, "[isolation]\n").expect("config should be written");

            let err = CombinedService::from_config_path(&config_path)
                .await
                .expect_err("config without workspace_directory should fail");
            assert!(matches!(err, CombinedServiceError::ParseToml(_)));
        });
    }

    #[test]
    fn combined_service_create_workspace_creates_directory_and_registers_workspace() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(&config_path, config_with_isolation("~/workspaces"))
                .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("beta")
                .await
                .expect("workspace should be created");

            let beta_dir = workspace_directory.join("beta");
            assert!(
                tokio::fs::metadata(&beta_dir)
                    .await
                    .expect("workspace directory should exist")
                    .is_dir()
            );

            let beta = service
                .manager
                .get_workspace("beta")
                .expect("workspace should be registered in manager")
                .subscribe();
            assert!(beta.borrow().persistent.created_at.is_some());
        });
    }

    #[test]
    fn combined_service_create_workspace_fails_for_duplicate_workspace() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(&config_path, config_with_isolation("~/workspaces"))
                .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");
            service
                .create_workspace("beta")
                .await
                .expect("first workspace creation should succeed");

            let err = service
                .create_workspace("beta")
                .await
                .expect_err("duplicate workspace creation should fail");
            assert!(matches!(
                err,
                CombinedServiceError::Manager(WorkspaceManagerError::WorkspaceAlreadyExists(key)) if key == "beta"
            ));
        });
    }

    #[test]
    fn combined_service_resolves_first_available_opencode_command() {
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
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&bin_dir).expect("bin dir should exist");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
            fs::create_dir_all(&workspace_directory).expect("workspace directory should exist");

            let fallback = bin_dir.join("opencode");
            fs::write(&fallback, "#!/bin/sh\nexit 0\n").expect("fallback command should be written");
            make_executable(&fallback);

            let _path_guard = EnvVarGuard::set("PATH", &bin_dir);
            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                format!(
                    "workspace-directory = \"{}\"\nopencode = [\"opencode-cli\", \"opencode\"]\ncreate-ssh-agent = false\n\n[isolation]\n",
                    workspace_directory.display()
                ),
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            assert_eq!(service.config.opencode, vec!["opencode-cli", "opencode"]);
            assert_eq!(service.opencode_command(), fallback.to_string_lossy());
        });
    }

    #[test]
    fn combined_service_fails_when_no_opencode_command_is_available() {
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
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&empty_bin).expect("empty bin should exist");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
            fs::create_dir_all(&workspace_directory).expect("workspace directory should exist");

            let _path_guard = EnvVarGuard::set("PATH", &empty_bin);
            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                format!(
                    "workspace-directory = \"{}\"\nopencode = [\"missing-a\", \"missing-b\"]\ncreate-ssh-agent = false\n\n[isolation]\n",
                    workspace_directory.display()
                ),
            )
            .expect("config should be written");

            let err = CombinedService::from_config_path(&config_path)
                .await
                .expect_err("missing commands should fail");

            match err {
                CombinedServiceError::OpencodeCommandNotFound { candidates } => {
                    assert_eq!(candidates, vec!["missing-a", "missing-b"]);
                }
                other => panic!("unexpected error: {other:?}"),
            }
        });
    }

    #[test]
    fn start_workspace_builds_github_git_credentials_bind_mount_when_enabled() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config: Config = toml::from_str(
                r#"
workspace-directory = "~/workspaces"

[github]
populate-git-credentials = true
token = { command = "printf test-token" }

[isolation]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should parse");

            let service = CombinedService::from_config(config)
                .await
                .expect_err("startup should fail without live GitHub username lookup in test env");
            assert!(matches!(
                service,
                CombinedServiceError::GithubStatusService(_)
            ));
        });
    }

    #[test]
    fn github_git_credentials_env_vars_include_helper_and_secrets() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            let github_api_dir = root.path().join("github-api");
            let github_server = github_api_dir.join("server.py");
            let github_port = 38492;
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            fs::create_dir_all(&github_api_dir).expect("github api dir should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            fs::write(
                &github_server,
                format!(
                    r#"from http.server import BaseHTTPRequestHandler, HTTPServer
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
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
"#
                ),
            )
            .expect("github server script should be written");
            let mut github_process = std::process::Command::new("python3")
                .arg(&github_server)
                .spawn()
                .expect("github api server should start");
            std::thread::sleep(std::time::Duration::from_millis(250));

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);
            unsafe {
                std::env::set_var("MULTICODE_GITHUB_TEST_TOKEN", "secret-token");
                std::env::set_var("GITHUB_API_URL", format!("http://127.0.0.1:{github_port}"));
            }

            let config: Config = toml::from_str(
                &format!(
                    r#"workspace-directory = "{}"

[github]
populate-git-credentials = true
token = {{ env = "MULTICODE_GITHUB_TEST_TOKEN" }}

[isolation]
"#,
                    workspace_directory.display()
                ),
            )
            .expect("config should parse");

            let service = CombinedService::from_config(config)
                .await
                .expect("combined service should start");
            let env_vars = service.github_git_credentials_env_vars();
            assert!(env_vars.contains(&(
                "MULTICODE_GITHUB_USERNAME".to_string(),
                "sandbox-user".to_string(),
            )));
            assert!(env_vars.contains(&(
                "MULTICODE_GITHUB_TOKEN".to_string(),
                "secret-token".to_string(),
            )));
            assert!(env_vars.contains(&(
                "GIT_CONFIG_COUNT".to_string(),
                "1".to_string(),
            )));
            assert!(env_vars.contains(&(
                "GIT_CONFIG_KEY_0".to_string(),
                "credential.helper".to_string(),
            )));
            assert!(env_vars.contains(&(
                "GIT_CONFIG_VALUE_0".to_string(),
                r#"!f() { test "$1" = get || exit 0; echo username=$MULTICODE_GITHUB_USERNAME; echo password=$MULTICODE_GITHUB_TOKEN; }; f"#.to_string(),
            )));

            unsafe {
                std::env::remove_var("MULTICODE_GITHUB_TEST_TOKEN");
                std::env::remove_var("GITHUB_API_URL");
            }
            let _ = github_process.kill();
            let _ = github_process.wait();
        });
    }

    #[test]
    fn start_workspace_builds_expected_isolation_command_arguments() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
writable = ["~/.cache/opencode"]
isolated = ["/var/tmp"]
tmpfs = ["/tmp"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR", "MISSING_VAR"]
memory_high = "8 GB"
memory_max = "10 GB"
cpu = "400%"
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let command = service
                .build_systemd_bwrap_command(
                    "alpha",
                    "test-password",
                    33111,
                    "multicode-test.service",
                )
                .await
                .expect("command args should be built");
            let args = command.args;

            assert!(contains_sequence(&args, &["--user", "--no-block"]));
            assert!(contains_sequence(
                &args,
                &["--unit", "multicode-test.service"]
            ));
            assert!(contains_sequence(
                &args,
                &["-p", &format!("MemoryHigh={}", 8_000_000_000_u64)]
            ));
            assert!(contains_sequence(
                &args,
                &["-p", &format!("MemoryMax={}", 10_000_000_000_u64)]
            ));
            assert!(contains_sequence(&args, &["-p", "MemorySwapMax=0"]));
            assert!(contains_sequence(&args, &["-p", "CPUQuota=400%"]));
            assert!(args.iter().any(|arg| arg == "bwrap"));
            assert!(!args.iter().any(|arg| arg == "--clearenv"));
            assert!(
                !args
                    .iter()
                    .any(|arg| arg == "OPENCODE_SERVER_USERNAME=opencode")
            );
            assert!(
                !args
                    .iter()
                    .any(|arg| arg == "OPENCODE_SERVER_PASSWORD=test-password")
            );
            assert!(contains_sequence(
                &args,
                &["--setenv", "OPENCODE_SERVER_USERNAME"]
            ));
            assert!(contains_sequence(
                &args,
                &["--setenv", "OPENCODE_SERVER_PASSWORD"]
            ));
            assert!(!contains_sequence(
                &args,
                &["--setenv", "HOME", home.to_string_lossy().as_ref()]
            ));
            assert!(!contains_sequence(
                &args,
                &[
                    "--setenv",
                    "XDG_RUNTIME_DIR",
                    runtime_dir.to_string_lossy().as_ref()
                ]
            ));
            assert!(!contains_sequence(
                &args,
                &["--setenv", "SSH_AUTH_SOCK", "/"]
            ));
            assert!(!args.iter().any(|arg| arg == "MISSING_VAR"));
            assert!(
                !args
                    .iter()
                    .any(|arg| arg == home.to_string_lossy().as_ref())
            );
            assert!(
                !args
                    .iter()
                    .any(|arg| arg == runtime_dir.to_string_lossy().as_ref())
            );
            assert!(!args.iter().any(|arg| arg == "env"));
            assert!(command.inherited_env.contains(&(
                "OPENCODE_SERVER_USERNAME".to_string(),
                "opencode".to_string()
            )));
            assert!(command.inherited_env.contains(&(
                "OPENCODE_SERVER_PASSWORD".to_string(),
                "test-password".to_string()
            )));
            assert!(contains_sequence(&args, &["--tmpfs", "/tmp"]));
            assert!(contains_sequence(
                &args,
                &[
                    service.opencode_command(),
                    "serve",
                    "--hostname",
                    "127.0.0.1",
                    "--port",
                    "33111"
                ]
            ));

            let isolated_storage = workspace_directory
                .join(".multicode")
                .join("isolate")
                .join("alpha")
                .join("var")
                .join("tmp");
            let isolated_storage_str = isolated_storage.to_string_lossy().into_owned();
            assert!(contains_sequence(
                &args,
                &["--bind", isolated_storage_str.as_str(), "/var/tmp"]
            ));
            assert!(
                tokio::fs::metadata(&isolated_storage)
                    .await
                    .expect("isolated storage should exist")
                    .is_dir()
            );
            assert!(
                tokio::fs::metadata("/var/tmp")
                    .await
                    .expect("isolated target should exist")
                    .is_dir()
            );
        });
    }

    #[test]
    fn isolated_mount_targets_are_precreated_before_bind() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
isolated = ["~/.local/state/opencode/session"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let command = service
                .build_systemd_bwrap_command(
                    "alpha",
                    "test-password",
                    33111,
                    "multicode-test.service",
                )
                .await
                .expect("command args should be built");
            let args = command.args;

            let isolated_target = home.join(".local/state/opencode/session");
            let isolated_target_str = isolated_target.to_string_lossy().into_owned();
            let isolated_storage = workspace_directory
                .join(".multicode")
                .join("isolate")
                .join("alpha")
                .join(
                    isolated_target
                        .strip_prefix("/")
                        .expect("isolated target should be absolute"),
                );

            assert!(contains_sequence(
                &args,
                &[
                    "--bind",
                    isolated_storage.to_string_lossy().as_ref(),
                    isolated_target_str.as_str()
                ]
            ));
            assert!(
                tokio::fs::metadata(&isolated_storage)
                    .await
                    .expect("isolated storage should exist")
                    .is_dir()
            );
            assert!(
                tokio::fs::metadata(&isolated_target)
                    .await
                    .expect("isolated target should exist")
                    .is_dir()
            );
        });
    }

    #[test]
    fn build_systemd_bwrap_args_supports_readable_mounts_and_orders_parents_first() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let readable_parent = home.join(".config");
            let writable_child = readable_parent.join("opencode");
            fs::create_dir_all(&writable_child).expect("nested mount targets should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
readable = ["~/.config"]
writable = ["~/.config/opencode"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let command = service
                .build_systemd_bwrap_command(
                    "alpha",
                    "test-password",
                    33111,
                    "multicode-test.service",
                )
                .await
                .expect("command args should be built");
            let args = command.args;

            let readable_parent_str = readable_parent.to_string_lossy().into_owned();
            let writable_child_str = writable_child.to_string_lossy().into_owned();
            let readable_index = first_sequence_index(
                &args,
                &[
                    "--ro-bind",
                    readable_parent_str.as_str(),
                    readable_parent_str.as_str(),
                ],
            )
            .expect("readable mount should be present");
            let writable_index = first_sequence_index(
                &args,
                &[
                    "--bind",
                    writable_child_str.as_str(),
                    writable_child_str.as_str(),
                ],
            )
            .expect("writable child mount should be present");

            assert!(
                readable_index < writable_index,
                "parent readable mount should be emitted before child writable mount"
            );
        });
    }

    #[test]
    fn mount_spec_prepare_creates_readable_directory_targets() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let source = root.path().join("source-skill");
            let target = root
                .path()
                .join("target/.config/opencode/skills/source-skill");
            fs::create_dir_all(&source).expect("source dir should exist");

            let mount = MountSpec::new(target.clone(), Some(source.clone()), MountKind::Readable);
            let resolved_mount = mount.resolve_effective(&[]);
            resolved_mount
                .prepare_source_node(true)
                .await
                .expect("source prepare should succeed");
            resolved_mount
                .prepare_target_node(false)
                .await
                .expect("target prepare should succeed");

            assert!(
                target.is_dir(),
                "readable directory target should be created"
            );
        });
    }

    #[test]
    fn build_systemd_bwrap_args_mounts_added_skills_from_config_relative_directories() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            let workspace_directory = home.join("workspaces");
            let addon_root = root.path().join("skill-sources");
            let addon_skill = addon_root.join("sample-skill");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
            fs::create_dir_all(&addon_skill).expect("skill dir should exist");
            fs::write(addon_skill.join("SKILL.md"), "# sample").expect("skill file should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
add-skills-from = ["skill-sources"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let command = service
                .build_systemd_bwrap_command(
                    "alpha",
                    "test-password",
                    33111,
                    "multicode-test.service",
                )
                .await
                .expect("command args should be built");
            let args = command.args;

            let target = home.join(".config/opencode/skills/sample-skill");
            let canonical_skill =
                fs::canonicalize(&addon_skill).expect("skill path should canonicalize");
            assert!(contains_sequence(
                &args,
                &[
                    "--ro-bind",
                    canonical_skill.to_string_lossy().as_ref(),
                    target.to_string_lossy().as_ref(),
                ]
            ));
            assert!(
                target.is_dir(),
                "readable skill target directory should be created"
            );
        });
    }

    #[test]
    fn build_systemd_bwrap_args_keeps_readable_file_mounts_as_files() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let auth_dir = home.join(".local/share/opencode");
            fs::create_dir_all(&auth_dir).expect("auth parent should exist");
            let auth_file = auth_dir.join("auth.json");
            fs::write(&auth_file, "{}").expect("auth file should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
readable = ["~/.local/share/opencode/auth.json"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let args = service
                .build_systemd_bwrap_command(
                    "alpha",
                    "test-password",
                    33111,
                    "multicode-test.service",
                )
                .await
                .expect("command args should be built")
                .args;

            let auth_file_str = auth_file.to_string_lossy().into_owned();
            assert!(
                first_sequence_index(
                    &args,
                    &["--ro-bind", auth_file_str.as_str(), auth_file_str.as_str()],
                )
                .is_some(),
                "readable file mount should be emitted as a direct ro-bind"
            );
            assert!(auth_file.is_file(), "readable file should remain a file");
        });
    }

    #[test]
    fn build_systemd_bwrap_args_orders_writable_parents_before_readable_children() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let writable_parent = home.join(".cache");
            let readable_child = writable_parent.join("opencode");
            fs::create_dir_all(&readable_child).expect("nested mount targets should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
readable = ["~/.cache/opencode"]
writable = ["~/.cache"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let args = service
                .build_systemd_bwrap_command(
                    "alpha",
                    "test-password",
                    33111,
                    "multicode-test.service",
                )
                .await
                .expect("command args should be built")
                .args;

            let writable_parent_str = writable_parent.to_string_lossy().into_owned();
            let readable_child_str = readable_child.to_string_lossy().into_owned();
            let writable_index = first_sequence_index(
                &args,
                &[
                    "--bind",
                    writable_parent_str.as_str(),
                    writable_parent_str.as_str(),
                ],
            )
            .expect("writable parent mount should be present");
            let readable_index = first_sequence_index(
                &args,
                &[
                    "--ro-bind",
                    readable_child_str.as_str(),
                    readable_child_str.as_str(),
                ],
            )
            .expect("readable child mount should be present");

            assert!(
                writable_index < readable_index,
                "parent writable mount should be emitted before child readable mount"
            );
        });
    }

    #[test]
    fn build_systemd_bwrap_args_supports_nested_writable_and_isolated_mounts() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
writable = ["~/.gradle"]
isolated = ["~/.gradle/daemon", "~/.gradle/.tmp", "~/.gradle/kotlin-profile"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let args = service
                .build_systemd_bwrap_command(
                    "alpha",
                    "test-password",
                    33111,
                    "multicode-test.service",
                )
                .await
                .expect("command args should be built")
                .args;

            let gradle_home = home.join(".gradle");
            let gradle_home_str = gradle_home.to_string_lossy().into_owned();
            let daemon_target = gradle_home.join("daemon");
            let daemon_target_str = daemon_target.to_string_lossy().into_owned();
            let daemon_storage = workspace_directory
                .join(".multicode")
                .join("isolate")
                .join("alpha")
                .join(
                    daemon_target
                        .strip_prefix("/")
                        .expect("daemon target should be absolute"),
                );
            let tmp_target = gradle_home.join(".tmp");
            let tmp_storage = workspace_directory
                .join(".multicode")
                .join("isolate")
                .join("alpha")
                .join(
                    tmp_target
                        .strip_prefix("/")
                        .expect("tmp target should be absolute"),
                );

            assert!(contains_sequence(
                &args,
                &["--bind", gradle_home_str.as_str(), gradle_home_str.as_str()]
            ));
            assert!(contains_sequence(
                &args,
                &[
                    "--bind",
                    daemon_storage.to_string_lossy().as_ref(),
                    daemon_target_str.as_str()
                ]
            ));
            assert!(
                tokio::fs::metadata(&gradle_home)
                    .await
                    .expect("gradle home should exist")
                    .is_dir()
            );
            assert!(
                tokio::fs::metadata(&daemon_storage)
                    .await
                    .expect("isolated gradle daemon storage should exist")
                    .is_dir()
            );
            assert!(
                tokio::fs::metadata(&tmp_storage)
                    .await
                    .expect("isolated gradle tmp storage should exist")
                    .is_dir(),
                "isolated gradle tmp storage should be a directory"
            );
        });
    }

    #[test]
    fn build_systemd_bwrap_args_creates_nested_isolated_source_inside_prior_isolated_mount() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
isolated = ["~/.local/share/opencode", "~/.local/share/opencode/auth.json"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let args = service
                .build_systemd_bwrap_command(
                    "alpha",
                    "test-password",
                    33111,
                    "multicode-test.service",
                )
                .await
                .expect("command args should be built")
                .args;

            let isolated_root_target = home.join(".local/share/opencode");
            let isolated_root_storage = workspace_directory
                .join(".multicode")
                .join("isolate")
                .join("alpha")
                .join(
                    isolated_root_target
                        .strip_prefix("/")
                        .expect("isolated root target should be absolute"),
                );
            let nested_file_storage = isolated_root_storage.join("auth.json");

            assert!(contains_sequence(
                &args,
                &[
                    "--bind",
                    isolated_root_storage.to_string_lossy().as_ref(),
                    isolated_root_target.to_string_lossy().as_ref(),
                ]
            ));
            assert!(contains_sequence(
                &args,
                &[
                    "--bind",
                    nested_file_storage.to_string_lossy().as_ref(),
                    home.join(".local/share/opencode/auth.json")
                        .to_string_lossy()
                        .as_ref(),
                ]
            ));
            assert!(
                tokio::fs::metadata(&isolated_root_storage)
                    .await
                    .expect("isolated root storage should exist")
                    .is_dir()
            );
            assert!(
                tokio::fs::metadata(&nested_file_storage)
                    .await
                    .expect("nested isolated file storage should exist")
                    .is_file()
            );
        });
    }

    #[test]
    fn combined_service_parses_tool_entries_from_toml() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]

[handler]
review = "/usr/bin/smerge"
web = "/usr/bin/firefox {}"

[[tool]]
type = "exec"
name = "Shell"
key = "x"
exec = "/bin/bash"

[[tool]]
type = "prompt"
name = "Smerge"
key = "m"
prompt = "Start /usr/bin/smerge in the repository"
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should parse tool entries");

            assert_eq!(service.config.tool.len(), 2);
            assert!(matches!(service.config.tool[0].type_, ToolType::Exec));
            assert!(matches!(service.config.tool[1].type_, ToolType::Prompt));
            assert_eq!(service.config.tool[0].key, "x");
            assert_eq!(service.config.tool[1].key, "m");
            assert_eq!(service.config.handler.review, "/usr/bin/smerge");
            assert_eq!(service.config.handler.web, "/usr/bin/firefox {}");
        });
    }

    #[test]
    fn combined_service_defaults_handler_entries_when_section_is_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should load default handlers");

            assert_eq!(service.config.handler.review, "/usr/bin/smerge");
            assert_eq!(service.config.handler.web, "/usr/bin/firefox {}");
        });
    }

    #[test]
    fn combined_service_parses_remote_section_from_toml() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]

[remote]
sync-interval-seconds = 2

[[remote.sync-up]]
local = "./target/debug/multicode-tui"
remote = "/srv/multicode/bin"

[[remote.sync-bidi]]
local = "~/workspaces"
remote = "/srv/multicode/agent-work"

[remote.install]
command = "./install-deps.sh"
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should parse remote section");

            let remote = service
                .config
                .remote
                .expect("remote config should be present");
            assert_eq!(remote.sync_interval_seconds, 2);
            assert_eq!(remote.sync_up.len(), 1);
            assert_eq!(remote.sync_bidi.len(), 1);
            assert_eq!(remote.install.command, "./install-deps.sh");
        });
    }

    #[test]
    fn combined_service_rejects_invalid_remote_configuration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]

[remote]
[remote.install]
command = "./install-deps.sh"
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("remote config should be accepted");
            assert!(service.config.remote.is_some());
        });
    }

    #[test]
    fn combined_service_rejects_invalid_handler_configuration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]

[handler]
review = "smerge"
web = "/usr/bin/firefox"
"#,
            )
            .expect("config should be written");

            let err = CombinedService::from_config_path(&config_path)
                .await
                .expect_err("invalid handler config should be rejected");
            assert!(matches!(
                err,
                CombinedServiceError::InvalidHandlerConfig { .. }
            ));
        });
    }

    #[test]
    fn combined_service_rejects_invalid_tool_configuration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]

[[tool]]
type = "exec"
name = "Shell"
key = "q"
exec = "/bin/bash"
"#,
            )
            .expect("config should be written");

            let err = CombinedService::from_config_path(&config_path)
                .await
                .expect_err("reserved tool key should be rejected");
            assert!(matches!(
                err,
                CombinedServiceError::InvalidToolConfig { index: 0, .. }
            ));
        });
    }

    #[test]
    fn combined_service_rejects_mixed_tool_payload_fields() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]

[[tool]]
type = "prompt"
name = "Smerge"
key = "m"
prompt = "Start /usr/bin/smerge in the repository"
exec = "/bin/bash"
"#,
            )
            .expect("config should be written");

            let err = CombinedService::from_config_path(&config_path)
                .await
                .expect_err("mixed prompt/exec fields should be rejected");
            assert!(matches!(
                err,
                CombinedServiceError::InvalidToolConfig { index: 0, .. }
            ));
        });
    }

    #[test]
    fn build_exec_tool_args_uses_interactive_systemd_bwrap_invocation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
writable = ["~/.cache/opencode"]
isolated = ["/var/tmp"]
tmpfs = ["/tmp"]
inherit-env = ["HOME", "XDG_RUNTIME_DIR"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let command = service
                .build_exec_tool_command("alpha", "/bin/bash")
                .await
                .expect("exec tool args should be built");
            let args = command.args;

            assert!(contains_sequence(
                &args,
                &["--user", "--wait", "--collect", "--pty"]
            ));
            assert!(contains_sequence(&args, &["bwrap", "--chdir"]));
            assert!(!args.iter().any(|arg| arg == "--clearenv"));
            assert!(!contains_sequence(&args, &["env", "TERM=xterm-256color"]));
            assert!(
                command
                    .inherited_env
                    .contains(&("HOME".to_string(), home.to_string_lossy().into_owned()))
            );
            assert!(command.inherited_env.contains(&(
                "XDG_RUNTIME_DIR".to_string(),
                runtime_dir.to_string_lossy().into_owned()
            )));
            assert!(contains_sequence(&args, &["--tmpfs", "/tmp"]));
            assert!(contains_sequence(
                &args,
                &["--proc", "/proc", "--dev", "/dev"]
            ));
            assert!(args.iter().any(|arg| arg == "/bin/bash"));
            assert!(!args.iter().any(|arg| arg == "-lc"));
        });
    }

    #[test]
    fn build_exec_tool_args_does_not_inherit_term_without_config() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            // set TERM and COLORTERM in environment
            unsafe {
                std::env::set_var("TERM", "xterm-256color");
                std::env::set_var("COLORTERM", "truecolor");
            }

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let command = service
                .build_exec_tool_command("alpha", "/bin/bash")
                .await
                .expect("exec tool args should be built");
            let args = command.args;

            assert!(!contains_sequence(&args, &["--setenv", "TERM"]));
            assert!(!contains_sequence(&args, &["--setenv", "COLORTERM"]));
            assert!(command.inherited_env.is_empty());
        });
    }

    #[test]
    fn build_exec_tool_args_inherits_term_when_configured() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            // set TERM and COLORTERM in environment
            unsafe {
                std::env::set_var("TERM", "xterm-256color");
                std::env::set_var("COLORTERM", "truecolor");
            }

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
inherit-env = ["TERM", "COLORTERM"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");

            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let command = service
                .build_exec_tool_command("alpha", "/bin/bash")
                .await
                .expect("exec tool args should be built");
            let args = command.args;

            assert!(!contains_sequence(
                &args,
                &["--setenv", "TERM", "xterm-256color"]
            ));
            assert!(!contains_sequence(
                &args,
                &["--setenv", "COLORTERM", "truecolor"]
            ));
            assert!(!args.iter().any(|arg| arg == "env"));
            assert!(
                command
                    .inherited_env
                    .contains(&("TERM".to_string(), "xterm-256color".to_string()))
            );
            assert!(
                command
                    .inherited_env
                    .contains(&("COLORTERM".to_string(), "truecolor".to_string()))
            );
        });
    }

    #[test]
    fn stop_workspace_requires_transient_snapshot() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(&config_path, config_with_isolation("~/workspaces"))
                .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");
            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let err = service
                .stop_workspace("alpha")
                .await
                .expect_err("stop should fail without transient snapshot");
            assert!(matches!(
                err,
                CombinedServiceError::TransientSnapshotMissing(key) if key == "alpha"
            ));
        });
    }

    #[test]
    fn unarchive_workspace_reactivates_legacy_archived_directory_without_snapshot_flag_loaded() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(workspace_directory.join("alpha"))
                .expect("legacy archived workspace directory should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(&config_path, config_with_isolation("~/workspaces"))
                .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");
            service
                .manager
                .get_workspace("alpha")
                .expect("workspace should be discovered")
                .update(|snapshot| {
                    snapshot.persistent.archived = false;
                    true
                });

            let (progress_tx, _progress_rx) = tokio::sync::watch::channel(String::new());
            service
                .unarchive_workspace("alpha", progress_tx)
                .await
                .expect("legacy archived workspace should unarchive from filesystem state");

            let snapshot = service
                .manager
                .get_workspace("alpha")
                .expect("workspace should still exist")
                .subscribe()
                .borrow()
                .clone();
            assert!(!snapshot.persistent.archived);
        });
    }

    #[test]
    fn archive_and_unarchive_workspace_round_trip_restores_isolate_dir() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
isolated = ["~/.config/opencode"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");
            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let workspace_path = workspace_directory.join("alpha");
            fs::write(workspace_path.join("README.txt"), "workspace data")
                .expect("workspace file should be written");

            let isolate_path = workspace_directory
                .join(".multicode")
                .join("isolate")
                .join("alpha")
                .join("home")
                .join(Path::new(".config").join("opencode"));
            fs::create_dir_all(&isolate_path).expect("isolate dir should be created");
            fs::write(isolate_path.join("state.json"), "isolate data")
                .expect("isolate file should be written");

            let (progress_tx, _progress_rx) = tokio::sync::watch::channel(String::new());
            service
                .archive_workspace("alpha", progress_tx)
                .await
                .expect("workspace should archive");

            assert!(!workspace_path.exists());
            assert!(!isolate_path.exists());
            assert!(workspace_directory.join("alpha.tar.zstd").is_file());
            assert!(
                workspace_directory
                    .join(".multicode")
                    .join("isolate")
                    .join("alpha.tar.zstd")
                    .is_file()
            );

            let (progress_tx, _progress_rx) = tokio::sync::watch::channel(String::new());
            service
                .unarchive_workspace("alpha", progress_tx)
                .await
                .expect("workspace should unarchive");

            assert_eq!(
                fs::read_to_string(workspace_path.join("README.txt"))
                    .expect("workspace file should be restored"),
                "workspace data"
            );
            assert_eq!(
                fs::read_to_string(isolate_path.join("state.json"))
                    .expect("isolate file should be restored"),
                "isolate data"
            );

            let snapshot = service
                .manager
                .get_workspace("alpha")
                .expect("workspace should still exist")
                .subscribe()
                .borrow()
                .clone();
            assert!(!snapshot.persistent.archived);
            assert_eq!(snapshot.persistent.archive_format, None);
        });
    }

    #[test]
    fn archive_workspace_tolerates_missing_isolate_dir() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let home = root.path().join("home");
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&home).expect("home should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime should exist");
            let workspace_directory = home.join("workspaces");
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");

            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                r#"workspace-directory = "~/workspaces"

[isolation]
isolated = ["~/.config/opencode"]
"#,
            )
            .expect("config should be written");

            let service = CombinedService::from_config_path(&config_path)
                .await
                .expect("combined service should start");
            service
                .create_workspace("alpha")
                .await
                .expect("workspace should be created");

            let workspace_path = workspace_directory.join("alpha");
            fs::write(workspace_path.join("README.txt"), "workspace data")
                .expect("workspace file should be written");

            let isolate_root = workspace_directory
                .join(".multicode")
                .join("isolate")
                .join("alpha");
            assert!(!isolate_root.exists());

            let (progress_tx, _progress_rx) = tokio::sync::watch::channel(String::new());
            service
                .archive_workspace("alpha", progress_tx)
                .await
                .expect("workspace should archive without isolate dir");

            assert!(!workspace_path.exists());
            assert!(!isolate_root.exists());
            assert!(
                !workspace_directory
                    .join(".multicode")
                    .join("isolate")
                    .join("alpha.tar.zstd")
                    .exists()
            );

            let (progress_tx, _progress_rx) = tokio::sync::watch::channel(String::new());
            service
                .unarchive_workspace("alpha", progress_tx)
                .await
                .expect("workspace should unarchive without isolate dir");

            assert_eq!(
                fs::read_to_string(workspace_path.join("README.txt"))
                    .expect("workspace file should be restored"),
                "workspace data"
            );
            assert!(!isolate_root.exists());
        });
    }

    #[test]
    fn stop_systemd_args_use_no_block_mode() {
        assert_eq!(
            stop_systemd_args("unit.service"),
            vec!["--user", "stop", "--no-block", "unit.service"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }

    fn contains_sequence(args: &[String], sequence: &[&str]) -> bool {
        args.windows(sequence.len()).any(|window| {
            window
                .iter()
                .map(String::as_str)
                .zip(sequence.iter().copied())
                .all(|(left, right)| left == right)
        })
    }

    fn first_sequence_index(args: &[String], sequence: &[&str]) -> Option<usize> {
        args.windows(sequence.len()).position(|window| {
            window
                .iter()
                .map(String::as_str)
                .zip(sequence.iter().copied())
                .all(|(left, right)| left == right)
        })
    }
}
