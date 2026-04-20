use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use tokio::sync::watch;

use super::workspace_watch::monitor_workspace_snapshots;
use crate::{
    PersistentWorkspaceSnapshot, WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot,
};

#[derive(Debug)]
pub enum PersistentStorageError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Manager(WorkspaceManagerError),
    StoragePathNotDirectory(PathBuf),
}

impl From<std::io::Error> for PersistentStorageError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for PersistentStorageError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<WorkspaceManagerError> for PersistentStorageError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

pub async fn persistent_storage(
    manager: Arc<WorkspaceManager>,
    persistent_storage_directory: impl AsRef<Path>,
    workspace_directory: impl AsRef<Path>,
) -> Result<(), PersistentStorageError> {
    let storage_dir = ensure_storage_directory(persistent_storage_directory.as_ref()).await?;
    let storage_dir = Arc::new(storage_dir);
    let workspace_dir = Arc::new(workspace_directory.as_ref().to_path_buf());

    monitor_workspace_snapshots(manager, move |key, workspace, mut workspace_rx| {
        let storage_dir = storage_dir.clone();
        let workspace_dir = workspace_dir.clone();
        async move {
            let snapshot_path = snapshot_file_path(storage_dir.as_ref(), &key);
            let workspace_path = workspace_path(workspace_dir.as_ref(), &key);
            let mut loaded_snapshot_from_disk = false;
            if let Some(mut persistent_snapshot) = read_persistent_snapshot(&snapshot_path).await? {
                loaded_snapshot_from_disk = true;
                let backfilled = backfill_created_at_from_workspace_mtime(
                    &mut persistent_snapshot,
                    &workspace_path,
                )
                .await?;
                if backfilled {
                    write_persistent_snapshot(&snapshot_path, &persistent_snapshot).await?;
                }
                workspace.update(|snapshot| {
                    if snapshot.persistent != persistent_snapshot {
                        snapshot.persistent = persistent_snapshot.clone();
                        true
                    } else {
                        false
                    }
                });
            }

            let current_persistent = workspace_rx.borrow_and_update().persistent.clone();
            if !loaded_snapshot_from_disk {
                write_persistent_snapshot(&snapshot_path, &current_persistent).await?;
            }

            tokio::spawn(async move {
                if let Err(err) =
                    watch_workspace_snapshot(snapshot_path, workspace_rx, current_persistent).await
                {
                    tracing::error!(error = ?err, "persistent storage watcher exited with error");
                }
            });
            Ok(())
        }
    })
    .await
}

async fn watch_workspace_snapshot(
    snapshot_path: PathBuf,
    mut workspace_rx: watch::Receiver<WorkspaceSnapshot>,
    mut previous_persistent: PersistentWorkspaceSnapshot,
) -> Result<(), PersistentStorageError> {
    while workspace_rx.changed().await.is_ok() {
        let next_persistent = workspace_rx.borrow_and_update().persistent.clone();
        let should_persist = previous_persistent != next_persistent;

        if should_persist {
            write_persistent_snapshot(&snapshot_path, &next_persistent).await?;
            previous_persistent = next_persistent;
        }
    }

    Ok(())
}

async fn ensure_storage_directory(
    persistent_storage_directory: &Path,
) -> Result<PathBuf, PersistentStorageError> {
    match tokio::fs::metadata(persistent_storage_directory).await {
        Ok(metadata) => {
            if metadata.is_dir() {
                Ok(persistent_storage_directory.to_path_buf())
            } else {
                Err(PersistentStorageError::StoragePathNotDirectory(
                    persistent_storage_directory.to_path_buf(),
                ))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(persistent_storage_directory).await?;
            Ok(persistent_storage_directory.to_path_buf())
        }
        Err(err) => Err(err.into()),
    }
}

fn snapshot_file_path(storage_dir: &Path, key: &str) -> PathBuf {
    storage_dir.join(format!("{key}.json"))
}

fn workspace_path(workspace_dir: &Path, key: &str) -> PathBuf {
    workspace_dir.join(key)
}

async fn backfill_created_at_from_workspace_mtime(
    snapshot: &mut PersistentWorkspaceSnapshot,
    workspace_path: &Path,
) -> Result<bool, PersistentStorageError> {
    if snapshot.created_at.is_some() {
        return Ok(false);
    }

    if let Some(modified) = workspace_directory_modified_time(workspace_path).await? {
        snapshot.created_at = Some(modified);
        return Ok(true);
    }
    Ok(false)
}

async fn workspace_directory_modified_time(
    workspace_path: &Path,
) -> Result<Option<SystemTime>, PersistentStorageError> {
    match tokio::fs::metadata(workspace_path).await {
        Ok(metadata) if metadata.is_dir() || metadata.is_file() => Ok(metadata.modified().ok()),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

async fn read_persistent_snapshot(
    snapshot_path: &Path,
) -> Result<Option<PersistentWorkspaceSnapshot>, PersistentStorageError> {
    match tokio::fs::metadata(snapshot_path).await {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Ok(None);
            }

            let raw = tokio::fs::read(snapshot_path).await?;
            let snapshot = serde_json::from_slice::<PersistentWorkspaceSnapshot>(&raw)?;
            Ok(Some(snapshot))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

async fn write_persistent_snapshot(
    snapshot_path: &Path,
    snapshot: &PersistentWorkspaceSnapshot,
) -> Result<(), PersistentStorageError> {
    let raw = serde_json::to_vec_pretty(snapshot)?;
    let tmp_path = snapshot_path.with_extension("json.tmp");
    tokio::fs::write(&tmp_path, raw).await?;
    tokio::fs::rename(tmp_path, snapshot_path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceManager;
    use std::{
        fs::{self, File},
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
                "multicode-persistent-storage-{}-{}",
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

    #[test]
    fn persistent_storage_rejects_file_path() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let file_path = root.path().join("not-a-directory");
            File::create(&file_path).expect("test file should be created");

            let err = ensure_storage_directory(&file_path)
                .await
                .expect_err("file path should be rejected");
            assert!(matches!(
                err,
                PersistentStorageError::StoragePathNotDirectory(path) if path == file_path
            ));
        });
    }

    #[test]
    fn persistent_storage_loads_existing_snapshots_and_persists_updates() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let storage_dir = root.path().join("storage");
            let workspace_dir = root.path().join("workspaces");
            tokio::fs::create_dir_all(&storage_dir)
                .await
                .expect("storage dir should exist");
            tokio::fs::create_dir_all(workspace_dir.join("alpha"))
                .await
                .expect("workspace dir should exist");

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");

            let initial_persistent = PersistentWorkspaceSnapshot {
                archived: true,
                description: "loaded from disk".to_string(),
                created_at: Some(UNIX_EPOCH + Duration::from_secs(10)),
                assigned_repository: None,
                automation_issue: None,
                automation_paused: false,
                archive_format: None,
                agent_provided: Default::default(),
                custom_links: Default::default(),
                ignored_issue_urls: Vec::new(),
                tasks: Vec::new(),
            };
            let snapshot_path = storage_dir.join("alpha.json");
            tokio::fs::write(
                &snapshot_path,
                serde_json::to_vec(&initial_persistent).expect("json should serialize"),
            )
            .await
            .expect("initial json should be written");

            let mut alpha_rx = manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .subscribe();
            let service_task = tokio::spawn(persistent_storage(
                manager.clone(),
                storage_dir.clone(),
                workspace_dir.clone(),
            ));

            tokio::time::timeout(Duration::from_secs(2), alpha_rx.changed())
                .await
                .expect("workspace should receive initial persistent update")
                .expect("workspace watch should stay open");
            assert_eq!(alpha_rx.borrow().persistent, initial_persistent);

            manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .update(|snapshot| {
                    snapshot.persistent.description = "updated in memory".to_string();
                    snapshot.persistent.archived = false;
                    true
                });

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let content = tokio::fs::read(&snapshot_path)
                        .await
                        .expect("snapshot file should stay readable");
                    let snapshot: PersistentWorkspaceSnapshot =
                        serde_json::from_slice(&content).expect("snapshot should parse");
                    if snapshot.description == "updated in memory" && !snapshot.archived {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("persistent snapshot should be persisted after update");

            service_task.abort();
        });
    }

    #[test]
    fn persistent_storage_loads_snapshot_for_workspace_added_after_start() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let storage_dir = root.path().join("storage");
            let workspace_dir = root.path().join("workspaces");
            tokio::fs::create_dir_all(&storage_dir)
                .await
                .expect("storage dir should exist");
            tokio::fs::create_dir_all(workspace_dir.join("beta"))
                .await
                .expect("workspace dir should exist");

            let manager = Arc::new(WorkspaceManager::new());
            let service_task = tokio::spawn(persistent_storage(
                manager.clone(),
                storage_dir.clone(),
                workspace_dir.clone(),
            ));

            let on_disk = PersistentWorkspaceSnapshot {
                archived: true,
                description: "added later".to_string(),
                created_at: Some(UNIX_EPOCH + Duration::from_secs(20)),
                assigned_repository: None,
                automation_issue: None,
                automation_paused: false,
                archive_format: None,
                agent_provided: Default::default(),
                custom_links: Default::default(),
                ignored_issue_urls: Vec::new(),
                tasks: Vec::new(),
            };
            tokio::fs::write(
                storage_dir.join("beta.json"),
                serde_json::to_vec(&on_disk).expect("json should serialize"),
            )
            .await
            .expect("snapshot file should be written");

            let mut beta_rx = manager.add("beta").expect("workspace should be added");
            tokio::time::timeout(Duration::from_secs(2), beta_rx.changed())
                .await
                .expect("workspace should receive persistent load")
                .expect("workspace watch should stay open");

            assert_eq!(beta_rx.borrow().persistent, on_disk);
            service_task.abort();
        });
    }

    #[test]
    fn persistent_storage_writes_snapshot_for_new_workspace_when_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let storage_dir = root.path().join("storage");
            let workspace_dir = root.path().join("workspaces");
            tokio::fs::create_dir_all(&storage_dir)
                .await
                .expect("storage dir should exist");
            tokio::fs::create_dir_all(workspace_dir.join("delta"))
                .await
                .expect("workspace dir should exist");

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("delta")
                .expect("workspace should be added before service starts");
            manager
                .get_workspace("delta")
                .expect("workspace should exist")
                .update(|snapshot| {
                    snapshot.persistent.description = "created workspace".to_string();
                    true
                });

            let snapshot_path = storage_dir.join("delta.json");
            assert!(
                tokio::fs::metadata(&snapshot_path).await.is_err(),
                "snapshot file should not exist before service starts"
            );

            let service_task = tokio::spawn(persistent_storage(
                manager.clone(),
                storage_dir.clone(),
                workspace_dir.clone(),
            ));

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Ok(content) = tokio::fs::read(&snapshot_path).await {
                        let snapshot: PersistentWorkspaceSnapshot =
                            serde_json::from_slice(&content).expect("snapshot should parse");
                        if snapshot.description == "created workspace" {
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("missing snapshot should be written for new workspace");

            service_task.abort();
        });
    }

    #[test]
    fn persistent_storage_backfills_missing_created_at_from_workspace_mtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let storage_dir = root.path().join("storage");
            let workspace_dir = root.path().join("workspaces");
            tokio::fs::create_dir_all(&storage_dir)
                .await
                .expect("storage dir should exist");
            tokio::fs::create_dir_all(workspace_dir.join("gamma"))
                .await
                .expect("workspace dir should exist");

            let expected_created_at = tokio::fs::metadata(workspace_dir.join("gamma"))
                .await
                .expect("workspace dir metadata should be readable")
                .modified()
                .expect("workspace dir modified time should be readable");

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("gamma")
                .expect("workspace should be added before service starts");

            let on_disk = PersistentWorkspaceSnapshot {
                archived: false,
                description: "missing created_at".to_string(),
                created_at: None,
                assigned_repository: None,
                automation_issue: None,
                automation_paused: false,
                archive_format: None,
                agent_provided: Default::default(),
                custom_links: Default::default(),
                ignored_issue_urls: Vec::new(),
                tasks: Vec::new(),
            };
            let snapshot_path = storage_dir.join("gamma.json");
            tokio::fs::write(
                &snapshot_path,
                serde_json::to_vec(&on_disk).expect("json should serialize"),
            )
            .await
            .expect("snapshot file should be written");

            let mut gamma_rx = manager
                .get_workspace("gamma")
                .expect("workspace should exist")
                .subscribe();
            let service_task = tokio::spawn(persistent_storage(
                manager.clone(),
                storage_dir.clone(),
                workspace_dir.clone(),
            ));

            tokio::time::timeout(Duration::from_secs(2), gamma_rx.changed())
                .await
                .expect("workspace should receive persistent load")
                .expect("workspace watch should stay open");

            let loaded = gamma_rx.borrow().persistent.clone();
            assert_eq!(loaded.description, on_disk.description);
            assert_eq!(loaded.created_at, Some(expected_created_at));

            let persisted_raw = tokio::fs::read(&snapshot_path)
                .await
                .expect("snapshot file should stay readable");
            let persisted: PersistentWorkspaceSnapshot =
                serde_json::from_slice(&persisted_raw).expect("snapshot should parse");
            assert_eq!(persisted.created_at, Some(expected_created_at));

            service_task.abort();
        });
    }
}
