use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::SystemTime,
};

use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCommand {
    pub program: String,
    pub args: Vec<String>,
    pub inherited_env: Vec<(String, String)>,
}

use crate::{
    WorkspaceArchiveFormat, WorkspaceManager, WorkspaceManagerError, database::Database, logging,
};

use super::{
    GithubStatusService, GithubStatusServiceError, WorkspaceDirectoryError,
    autonomous_workspace_service::autonomous_workspace_service,
    config::{
        AddedSkillMount, Config, ExpandedIsolationConfig, expand_shell_path, inherited_env_value,
        read_config, resolve_opencode_command, validate_handler_config, validate_remote_config,
        validate_tool_config_entries, validate_workspace_key,
    },
    multicode_metadata_service, opencode_client_service, persistent_storage,
    resource_usage_service, root_session_service,
    runtime::WorkspaceRuntime,
    runtime_reconciliation_service::runtime_reconciliation_service,
    transient_storage, usage_aggregation_service,
    workspace_archive::ArchiveWorkspaceEntry,
    workspace_directory,
};

#[derive(Debug, Clone)]
pub struct CombinedService {
    pub config: Config,
    pub manager: Arc<WorkspaceManager>,
    pub database: Database,
    github_status_service: GithubStatusService,
    workspace_directory_path: PathBuf,
    expanded_isolation: ExpandedIsolationConfig,
    opencode_command: String,
    runtime: WorkspaceRuntime,
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
        let container_opencode_command =
            resolve_container_opencode_command(config.runtime.backend, &config.opencode);
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
        let runtime = WorkspaceRuntime::new(
            config.runtime.clone(),
            workspace_directory_path.clone(),
            expanded_isolation.clone(),
            opencode_command.clone(),
            container_opencode_command,
        );

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
        spawn_runtime_reconciliation_service(manager.clone(), runtime.clone());
        spawn_opencode_client_service(manager.clone());
        spawn_root_session_service(manager.clone());
        spawn_multicode_metadata_service(manager.clone());
        spawn_usage_aggregation_service(manager.clone());
        spawn_resource_usage_service(manager.clone());

        let service = Self {
            config,
            manager,
            database,
            github_status_service,
            workspace_directory_path,
            expanded_isolation,
            opencode_command,
            runtime,
            github_git_credentials_env,
        };

        spawn_autonomous_workspace_service(service.clone());

        Ok(service)
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
        strip_workspace_git_identity_overrides(&workspace_path).await?;

        let inherited_env = self
            .sandbox_env_pairs(Vec::<(String, String)>::new())
            .await?;
        let start = self.runtime.start_server(&key, &inherited_env).await?;
        tracing::info!(
            workspace_key = %key,
            backend = ?self.config.runtime.backend,
            runtime_id = %start.transient.runtime.id,
            "started workspace runtime"
        );
        let mut replaced = false;
        workspace.update(|snapshot| {
            if snapshot.transient.is_none() {
                snapshot.transient = Some(start.transient.clone());
                replaced = true;
                true
            } else {
                false
            }
        });

        if !replaced {
            self.runtime.stop_server(&start.transient.runtime).await?;
            return Err(CombinedServiceError::TransientSnapshotAlreadyPresent(
                key.to_string(),
            ));
        }

        Ok(())
    }

    pub async fn assign_workspace_repository(
        &self,
        key: &str,
        repository: Option<&str>,
    ) -> Result<Option<String>, CombinedServiceError> {
        let key = validate_workspace_key(key)?;
        let normalized = repository
            .map(|repository| {
                super::autonomous_workspace_service::normalize_github_repository_spec(repository)
                    .ok_or_else(|| {
                        CombinedServiceError::InvalidRepositorySpec(repository.trim().to_string())
                    })
            })
            .transpose()?;

        let workspace = self.manager.get_workspace(&key)?;
        workspace.update(|snapshot| {
            if snapshot.persistent.assigned_repository == normalized {
                return false;
            }
            snapshot.persistent.assigned_repository = normalized.clone();
            snapshot.persistent.automation_issue = None;
            snapshot.automation_status = normalized
                .as_ref()
                .map(|repository| format!("Repository assigned; scan queued for {repository}"));
            if normalized.is_some() {
                snapshot.automation_scan_request_nonce =
                    snapshot.automation_scan_request_nonce.saturating_add(1);
            }
            true
        });
        Ok(normalized)
    }

    pub fn request_workspace_issue_scan(&self, key: &str) -> Result<(), CombinedServiceError> {
        let key = validate_workspace_key(key)?;
        let workspace = self.manager.get_workspace(&key)?;
        workspace.update(|snapshot| {
            if let Some(repository) = snapshot.persistent.assigned_repository.as_deref() {
                if snapshot.persistent.automation_issue.is_none() {
                    snapshot.automation_status = Some(format!("Scan requested for {repository}"));
                }
            }
            snapshot.automation_scan_request_nonce =
                snapshot.automation_scan_request_nonce.saturating_add(1);
            true
        });
        Ok(())
    }

    pub async fn stop_workspace(&self, key: &str) -> Result<(), CombinedServiceError> {
        let key = validate_workspace_key(key)?;
        let workspace = self.manager.get_workspace(&key)?;
        let workspace_rx = workspace.subscribe();
        let runtime_handle = workspace_rx
            .borrow()
            .transient
            .as_ref()
            .map(|transient| transient.runtime.clone())
            .ok_or_else(|| CombinedServiceError::TransientSnapshotMissing(key.clone()))?;

        self.runtime.stop_server(&runtime_handle).await?;
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

    pub async fn delete_workspace(&self, key: &str) -> Result<(), CombinedServiceError> {
        let key = validate_workspace_key(key)?;
        let workspace = self.manager.get_workspace(&key)?;
        let snapshot = workspace.subscribe().borrow().clone();

        if let Some(transient) = snapshot.transient.as_ref() {
            self.runtime.stop_server(&transient.runtime).await?;
        }

        self.remove_workspace_disk_state(&key).await?;
        self.manager.remove(&key)?;
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
        let runtime_handle = self
            .manager
            .get_workspace(&key)?
            .subscribe()
            .borrow()
            .transient
            .as_ref()
            .map(|transient| transient.runtime.clone());

        let workspace_path = self.workspace_directory_path.join(&key);
        tokio::fs::create_dir_all(&workspace_path).await?;

        let inherited_env = self
            .sandbox_env_pairs(Vec::<(String, String)>::new())
            .await?;
        self.runtime
            .build_pty_command(&key, runtime_handle.as_ref(), &inherited_env, command)
            .await
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

    #[cfg_attr(not(test), allow(dead_code))]
    async fn build_systemd_bwrap_command(
        &self,
        key: &str,
        password: &str,
        port: u16,
        unit: &str,
    ) -> Result<SpawnCommand, CombinedServiceError> {
        let inherited_env = self
            .sandbox_env_pairs(Vec::<(String, String)>::new())
            .await?;
        self.runtime
            .build_linux_start_command(key, password, port, unit, &inherited_env)
            .await
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
                    inherited_env_value(env_name).map(|value| (env_name.clone(), value))
                }),
        );
        Ok(env)
    }

    fn isolate_path_for_key(&self, key: &str) -> PathBuf {
        self.workspace_directory_path
            .join(".multicode")
            .join("isolate")
            .join(key)
    }

    fn persistent_snapshot_path_for_key(&self, key: &str) -> PathBuf {
        self.workspace_directory_path
            .join(".multicode")
            .join("persistent")
            .join(format!("{key}.json"))
    }

    fn transient_snapshot_path_for_key(&self, key: &str) -> PathBuf {
        self.workspace_directory_path
            .join(".multicode")
            .join("transient")
            .join(format!("{key}.json"))
    }

    async fn remove_workspace_disk_state(&self, key: &str) -> Result<(), CombinedServiceError> {
        remove_path_if_exists(&self.workspace_directory_path.join(key)).await?;
        remove_path_if_exists(&self.isolate_path_for_key(key)).await?;
        remove_path_if_exists(&self.persistent_snapshot_path_for_key(key)).await?;
        remove_path_if_exists(&self.transient_snapshot_path_for_key(key)).await?;

        for format in WorkspaceArchiveFormat::all() {
            let archive_entry = ArchiveWorkspaceEntry::new(key, format);
            remove_path_if_exists(&archive_entry.to_path(&self.workspace_directory_path)).await?;
            remove_path_if_exists(&archive_entry.to_isolate_path(&self.workspace_directory_path))
                .await?;
        }

        Ok(())
    }

    fn github_git_credentials_env_vars(&self) -> Vec<(String, String)> {
        github_git_credentials_env_vars(self.github_git_credentials_env.as_ref())
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

fn resolve_container_opencode_command(
    backend: crate::RuntimeBackend,
    candidates: &[String],
) -> String {
    if backend == crate::RuntimeBackend::AppleContainer {
        return candidates
            .iter()
            .filter_map(|candidate| {
                let candidate = candidate.trim();
                if candidate.is_empty() {
                    return None;
                }
                let name = Path::new(candidate)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(candidate);
                if name == "opencode" {
                    Some("opencode".to_string())
                } else {
                    None
                }
            })
            .next()
            .unwrap_or_else(|| "opencode".to_string());
    }

    candidates
        .iter()
        .find_map(|candidate| {
            let candidate = candidate.trim();
            if candidate.is_empty() {
                return None;
            }
            Some(
                Path::new(candidate)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(candidate)
                    .to_string(),
            )
        })
        .unwrap_or_else(|| "opencode".to_string())
}

fn github_git_credentials_env_vars(
    github_git_credentials_env: Option<&GithubGitCredentialsEnv>,
) -> Vec<(String, String)> {
    let Some(github_git_credentials_env) = github_git_credentials_env else {
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
        (
            "GH_TOKEN".to_string(),
            github_git_credentials_env.token.clone(),
        ),
        (
            "GITHUB_TOKEN".to_string(),
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

async fn remove_path_if_exists(path: &Path) -> Result<(), CombinedServiceError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.is_dir() => {
            tokio::fs::remove_dir_all(path).await?;
            Ok(())
        }
        Ok(_) => {
            tokio::fs::remove_file(path).await?;
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

async fn strip_workspace_git_identity_overrides(
    workspace_path: &Path,
) -> Result<(), CombinedServiceError> {
    let workspace_path = workspace_path.to_path_buf();
    let repo_roots = tokio::task::spawn_blocking(move || find_git_repo_roots(&workspace_path))
        .await
        .map_err(|err| std::io::Error::other(err.to_string()))??;

    for repo_root in repo_roots {
        unset_repo_local_git_config(&repo_root, "user.name").await?;
        unset_repo_local_git_config(&repo_root, "user.email").await?;
    }

    Ok(())
}

fn find_git_repo_roots(workspace_path: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut stack = vec![workspace_path.to_path_buf()];
    let mut repo_roots = std::collections::BTreeSet::new();

    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if entry.file_name() == ".git" {
                repo_roots.insert(directory.clone());
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            }
        }
    }

    Ok(repo_roots.into_iter().collect())
}

async fn unset_repo_local_git_config(
    repo_root: &Path,
    key: &str,
) -> Result<(), CombinedServiceError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["config", "--local", "--unset-all", key])
        .stdin(Stdio::null())
        .output()
        .await?;

    if output.status.success() || output.status.code() == Some(5) {
        return Ok(());
    }

    Err(std::io::Error::other(format!(
        "failed to remove repo-local git config {key} from {}: {}",
        repo_root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
    .into())
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
    InvalidRuntimeConfig {
        field: String,
        message: String,
    },
    InvalidToolExecution(String),
    InvalidRepositorySpec(String),
    UnsupportedRuntimeBackend(String),
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

impl CombinedServiceError {
    pub fn summary(&self) -> String {
        match self {
            Self::StartWorkspaceFailed { status, stderr } => {
                summarize_workspace_start_failure(*status, stderr)
            }
            Self::StopWorkspaceFailed { status, stderr } => {
                summarize_workspace_stop_failure(*status, stderr)
            }
            _ => format!("{self:?}"),
        }
    }
}

pub fn summarize_workspace_start_failure(status: Option<i32>, stderr: &str) -> String {
    let stderr = compact_process_stderr(stderr);
    if stderr.contains("no free indices are available for allocation") {
        return format!(
            "Apple container vmnet allocator exhausted{}; restart the Apple container backend",
            exit_status_suffix(status),
        );
    }

    if stderr.is_empty() {
        format!("workspace start failed{}", exit_status_suffix(status))
    } else {
        format!(
            "workspace start failed{}: {stderr}",
            exit_status_suffix(status),
        )
    }
}

fn summarize_workspace_stop_failure(status: Option<i32>, stderr: &str) -> String {
    let stderr = compact_process_stderr(stderr);
    if stderr.is_empty() {
        format!("workspace stop failed{}", exit_status_suffix(status))
    } else {
        format!(
            "workspace stop failed{}: {stderr}",
            exit_status_suffix(status),
        )
    }
}

fn compact_process_stderr(stderr: &str) -> String {
    stderr
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .trim_matches('"')
        .to_string()
}

fn exit_status_suffix(status: Option<i32>) -> String {
    status
        .map(|status| format!(" (exit {status})"))
        .unwrap_or_default()
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

#[cfg_attr(not(test), allow(dead_code))]
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

fn spawn_runtime_reconciliation_service(manager: Arc<WorkspaceManager>, runtime: WorkspaceRuntime) {
    tokio::spawn(async move {
        if let Err(err) = runtime_reconciliation_service(manager, runtime).await {
            tracing::error!(error = ?err, "runtime reconciliation service exited with error");
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

fn spawn_autonomous_workspace_service(service: CombinedService) {
    tokio::spawn(async move {
        if let Err(err) = autonomous_workspace_service(service).await {
            tracing::error!(error = ?err, "autonomous workspace service exited with error");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{
        GithubTokenConfig, ToolType,
        runtime::{MountKind, MountSpec},
    };
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

    #[test]
    fn summarize_workspace_start_failure_reports_allocator_exhaustion_hint() {
        let summary = summarize_workspace_start_failure(
            Some(1),
            r#"Error: failed to bootstrap container (cause: "unknown: "no free indices are available for allocation"")"#,
        );

        assert_eq!(
            summary,
            "Apple container vmnet allocator exhausted (exit 1); restart the Apple container backend"
        );
    }

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
    fn resolve_container_opencode_command_prefers_opencode_for_apple_backend() {
        assert_eq!(
            resolve_container_opencode_command(
                crate::RuntimeBackend::AppleContainer,
                &["opencode-cli".to_string(), "opencode".to_string()]
            ),
            "opencode"
        );
        assert_eq!(
            resolve_container_opencode_command(
                crate::RuntimeBackend::AppleContainer,
                &["/opt/homebrew/bin/opencode-cli".to_string()]
            ),
            "opencode"
        );
    }

    #[test]
    fn resolve_container_opencode_command_keeps_first_candidate_for_linux_backend() {
        assert_eq!(
            resolve_container_opencode_command(
                crate::RuntimeBackend::LinuxSystemdBwrap,
                &["opencode-cli".to_string(), "opencode".to_string()]
            ),
            "opencode-cli"
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
        let env_vars = github_git_credentials_env_vars(Some(&GithubGitCredentialsEnv {
            username: "sandbox-user".to_string(),
            token: "secret-token".to_string(),
        }));
        assert!(env_vars.contains(&(
            "MULTICODE_GITHUB_USERNAME".to_string(),
            "sandbox-user".to_string(),
        )));
        assert!(env_vars.contains(&(
            "MULTICODE_GITHUB_TOKEN".to_string(),
            "secret-token".to_string(),
        )));
        assert!(env_vars.contains(&("GH_TOKEN".to_string(), "secret-token".to_string(),)));
        assert!(env_vars.contains(&("GITHUB_TOKEN".to_string(), "secret-token".to_string(),)));
        assert!(env_vars.contains(&("GIT_CONFIG_COUNT".to_string(), "1".to_string(),)));
        assert!(env_vars.contains(&(
            "GIT_CONFIG_KEY_0".to_string(),
            "credential.helper".to_string(),
        )));
        assert!(env_vars.contains(&(
                "GIT_CONFIG_VALUE_0".to_string(),
                r#"!f() { test "$1" = get || exit 0; echo username=$MULTICODE_GITHUB_USERNAME; echo password=$MULTICODE_GITHUB_TOKEN; }; f"#.to_string(),
            )));
    }

    #[test]
    fn assign_workspace_repository_normalizes_and_clears_automation_issue() {
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
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");

            let fake_opencode = bin_dir.join("opencode");
            fs::write(&fake_opencode, "#!/bin/sh\nexit 0\n")
                .expect("fake opencode should be written");
            make_executable(&fake_opencode);

            let _path_guard = EnvVarGuard::set("PATH", &bin_dir);
            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                format!(
                    "workspace-directory = \"{}\"\nopencode = [\"opencode\"]\n\n[isolation]\n",
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
                .manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .update(|snapshot| {
                    snapshot.persistent.automation_issue =
                        Some("https://github.com/example/repo/issue/1".to_string());
                    true
                });

            let normalized = service
                .assign_workspace_repository("alpha", Some("https://github.com/example/repo.git"))
                .await
                .expect("repository assignment should succeed");
            assert_eq!(normalized.as_deref(), Some("example/repo"));

            let snapshot = service
                .manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .subscribe()
                .borrow()
                .clone();
            assert_eq!(
                snapshot.persistent.assigned_repository.as_deref(),
                Some("example/repo")
            );
            assert!(snapshot.persistent.automation_issue.is_none());
            assert_eq!(snapshot.automation_scan_request_nonce, 1);

            let cleared = service
                .assign_workspace_repository("alpha", None)
                .await
                .expect("clearing repository assignment should succeed");
            assert!(cleared.is_none());
            let cleared_snapshot = service
                .manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .subscribe()
                .borrow()
                .clone();
            assert!(cleared_snapshot.persistent.assigned_repository.is_none());
            assert_eq!(cleared_snapshot.automation_scan_request_nonce, 1);
        });
    }

    #[test]
    fn delete_workspace_stops_runtime_and_removes_workspace_state() {
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
            fs::create_dir_all(&workspace_directory).expect("workspace root should exist");
            fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");

            let fake_opencode = bin_dir.join("opencode");
            fs::write(&fake_opencode, "#!/bin/sh\nexit 0\n")
                .expect("fake opencode should be written");
            make_executable(&fake_opencode);

            let stop_log = root.path().join("systemctl-stop.log");
            let fake_systemctl = bin_dir.join("systemctl");
            fs::write(
                &fake_systemctl,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nprintf -- '---\\n' >> '{}'\nexit 0\n",
                    stop_log.display(),
                    stop_log.display()
                ),
            )
            .expect("fake systemctl should be written");
            make_executable(&fake_systemctl);

            let _path_guard = EnvVarGuard::set("PATH", &bin_dir);
            let _home_guard = EnvVarGuard::set("HOME", &home);
            let _xdg_guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let config_path = root.path().join("config.toml");
            fs::write(
                &config_path,
                format!(
                    "workspace-directory = \"{}\"\nopencode = [\"opencode\"]\n\n[isolation]\n",
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

            let workspace = service
                .manager
                .get_workspace("alpha")
                .expect("workspace should exist");
            workspace.update(|snapshot| {
                snapshot.transient = Some(crate::TransientWorkspaceSnapshot {
                    uri: "http://opencode:secret@127.0.0.1:31337/".to_string(),
                    runtime: crate::RuntimeHandleSnapshot {
                        backend: crate::RuntimeBackend::LinuxSystemdBwrap,
                        id: "alpha.service".to_string(),
                        metadata: Default::default(),
                    },
                });
                true
            });

            let live_dir = workspace_directory.join("alpha");
            let isolate_dir = workspace_directory.join(".multicode").join("isolate").join("alpha");
            let persistent_snapshot = workspace_directory
                .join(".multicode")
                .join("persistent")
                .join("alpha.json");
            let transient_snapshot = workspace_directory
                .join(".multicode")
                .join("transient")
                .join("alpha.json");
            let archive_path =
                ArchiveWorkspaceEntry::new("alpha", WorkspaceArchiveFormat::TarZstd)
                    .to_path(&workspace_directory);
            let isolate_archive_path =
                ArchiveWorkspaceEntry::new("alpha", WorkspaceArchiveFormat::TarZstd)
                    .to_isolate_path(&workspace_directory);

            tokio::fs::create_dir_all(&live_dir)
                .await
                .expect("live dir should exist");
            tokio::fs::create_dir_all(&isolate_dir)
                .await
                .expect("isolate dir should exist");
            tokio::fs::write(live_dir.join("README.md"), "workspace")
                .await
                .expect("workspace file should exist");
            tokio::fs::write(isolate_dir.join("marker.txt"), "isolate")
                .await
                .expect("isolate file should exist");
            tokio::fs::write(&persistent_snapshot, "{}")
                .await
                .expect("persistent snapshot should exist");
            tokio::fs::write(&transient_snapshot, "{}")
                .await
                .expect("transient snapshot should exist");
            if let Some(parent) = archive_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .expect("archive parent should exist");
            }
            if let Some(parent) = isolate_archive_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .expect("isolate archive parent should exist");
            }
            tokio::fs::write(&archive_path, "archive")
                .await
                .expect("archive should exist");
            tokio::fs::write(&isolate_archive_path, "isolate archive")
                .await
                .expect("isolate archive should exist");

            service
                .delete_workspace("alpha")
                .await
                .expect("workspace deletion should succeed");

            assert!(service.manager.get_workspace("alpha").is_err());
            assert!(!live_dir.exists());
            assert!(!isolate_dir.exists());
            assert!(!persistent_snapshot.exists());
            assert!(!transient_snapshot.exists());
            assert!(!archive_path.exists());
            assert!(!isolate_archive_path.exists());

            let stop_invocation =
                fs::read_to_string(&stop_log).expect("runtime stop should be recorded");
            assert!(stop_invocation.contains("alpha.service"));
        });
    }

    #[test]
    fn strip_workspace_git_identity_overrides_removes_repo_local_user_identity() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace = root.path().join("workspace");
            let repo = workspace.join("repo");
            fs::create_dir_all(&repo).expect("repo dir should exist");

            let init = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["init"])
                .stdin(Stdio::null())
                .output()
                .await
                .expect("git init should run");
            assert!(init.status.success(), "git init should succeed");

            let set_name = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["config", "--local", "user.name", "Local Name"])
                .stdin(Stdio::null())
                .output()
                .await
                .expect("git config user.name should run");
            assert!(
                set_name.status.success(),
                "git config user.name should succeed"
            );

            let set_email = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["config", "--local", "user.email", "local@example.com"])
                .stdin(Stdio::null())
                .output()
                .await
                .expect("git config user.email should run");
            assert!(
                set_email.status.success(),
                "git config user.email should succeed"
            );

            strip_workspace_git_identity_overrides(&workspace)
                .await
                .expect("workspace git identity cleanup should succeed");

            let get_name = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["config", "--local", "--get", "user.name"])
                .stdin(Stdio::null())
                .output()
                .await
                .expect("git config get user.name should run");
            assert_eq!(get_name.status.code(), Some(1));

            let get_email = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["config", "--local", "--get", "user.email"])
                .stdin(Stdio::null())
                .output()
                .await
                .expect("git config get user.email should run");
            assert_eq!(get_email.status.code(), Some(1));

            let remote = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["config", "--local", "core.repositoryformatversion"])
                .stdin(Stdio::null())
                .output()
                .await
                .expect("git config core.repositoryformatversion should run");
            assert!(remote.status.success(), "repo config should remain intact");
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
