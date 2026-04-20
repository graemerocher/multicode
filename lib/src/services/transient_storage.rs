use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::watch;
use uuid::Uuid;

use super::{config::synthesized_xdg_runtime_dir, workspace_watch::monitor_workspace_snapshots};
use crate::{
    TransientWorkspaceSnapshot, WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot,
};

#[derive(Debug)]
pub enum TransientStorageError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Manager(WorkspaceManagerError),
    MissingXdgRuntimeDir,
    InvalidXdgRuntimeDir(PathBuf),
    StorageLinkNotSymlink(PathBuf),
    StorageTargetNotDirectory(PathBuf),
    TaskJoin(String),
}

impl From<std::io::Error> for TransientStorageError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for TransientStorageError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<WorkspaceManagerError> for TransientStorageError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

/// Load transient snapshots from /run, and then save any changes made back to /run.
pub async fn transient_storage(
    manager: Arc<WorkspaceManager>,
    transient_storage_link: impl AsRef<Path>,
) -> Result<(), TransientStorageError> {
    let storage_dir = ensure_storage_directory(transient_storage_link.as_ref()).await?;
    let storage_dir = Arc::new(storage_dir);

    monitor_workspace_snapshots(manager, move |key, workspace, mut workspace_rx| {
        let storage_dir = storage_dir.clone();
        async move {
            let snapshot_path = snapshot_file_path(storage_dir.as_ref(), &key);
            let transient_snapshot = read_transient_snapshot(&snapshot_path).await?;
            workspace.update(|snapshot| {
                if snapshot.transient.is_none() && transient_snapshot.is_some() {
                    snapshot.transient = transient_snapshot.clone();
                    true
                } else {
                    false
                }
            });

            let current_transient = workspace_rx.borrow_and_update().transient.clone();
            tokio::spawn(async move {
                if let Err(err) =
                    persist_transient_snapshot(&snapshot_path, current_transient.as_ref()).await
                {
                    tracing::error!(error = ?err, "failed to persist initial transient snapshot");
                    return;
                }
                if let Err(err) =
                    watch_workspace_snapshot(snapshot_path, workspace_rx, current_transient).await
                {
                    tracing::error!(error = ?err, "transient storage watcher exited with error");
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
    mut previous_transient: Option<TransientWorkspaceSnapshot>,
) -> Result<(), TransientStorageError> {
    while workspace_rx.changed().await.is_ok() {
        let next_transient = workspace_rx.borrow_and_update().transient.clone();
        let should_persist = previous_transient != next_transient;

        if should_persist {
            persist_transient_snapshot(&snapshot_path, next_transient.as_ref()).await?;
            previous_transient = next_transient;
        }
    }

    Ok(())
}

async fn ensure_storage_directory(
    transient_storage_link: &Path,
) -> Result<PathBuf, TransientStorageError> {
    match tokio::fs::symlink_metadata(transient_storage_link).await {
        Ok(metadata) => {
            if !metadata.file_type().is_symlink() {
                return Err(TransientStorageError::StorageLinkNotSymlink(
                    transient_storage_link.to_path_buf(),
                ));
            }

            let raw_target = tokio::fs::read_link(transient_storage_link).await?;
            let target = resolve_link_target(transient_storage_link, &raw_target);
            ensure_directory_exists(&target).await?;
            Ok(target)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .or_else(synthesized_xdg_runtime_dir)
                .ok_or(TransientStorageError::MissingXdgRuntimeDir)?;
            if !runtime_dir.is_absolute() {
                return Err(TransientStorageError::InvalidXdgRuntimeDir(runtime_dir));
            }

            let target = runtime_dir
                .join("multicode")
                .join(Uuid::new_v4().to_string());
            tokio::fs::create_dir_all(&target).await?;

            if let Some(parent) = transient_storage_link.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            create_symlink(target.clone(), transient_storage_link.to_path_buf()).await?;
            Ok(target)
        }
        Err(err) => Err(err.into()),
    }
}

async fn ensure_directory_exists(path: &Path) -> Result<(), TransientStorageError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => {
            if metadata.is_dir() {
                Ok(())
            } else {
                Err(TransientStorageError::StorageTargetNotDirectory(
                    path.to_path_buf(),
                ))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(path).await?;
            Ok(())
        }
        Err(err) => Err(err.into()),
    }
}

fn resolve_link_target(link_path: &Path, raw_target: &Path) -> PathBuf {
    if raw_target.is_absolute() {
        raw_target.to_path_buf()
    } else {
        link_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(raw_target)
    }
}

async fn create_symlink(
    source: PathBuf,
    destination: PathBuf,
) -> Result<(), TransientStorageError> {
    tokio::fs::symlink(source, destination).await?;
    Ok(())
}

fn snapshot_file_path(storage_dir: &Path, key: &str) -> PathBuf {
    storage_dir.join(format!("{key}.json"))
}

async fn read_transient_snapshot(
    snapshot_path: &Path,
) -> Result<Option<TransientWorkspaceSnapshot>, TransientStorageError> {
    match tokio::fs::metadata(snapshot_path).await {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Ok(None);
            }

            let raw = tokio::fs::read(snapshot_path).await?;
            let snapshot = serde_json::from_slice::<TransientWorkspaceSnapshot>(&raw)?;
            Ok(Some(snapshot))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

async fn write_transient_snapshot(
    snapshot_path: &Path,
    snapshot: &TransientWorkspaceSnapshot,
) -> Result<(), TransientStorageError> {
    let raw = serde_json::to_vec_pretty(snapshot)?;
    let tmp_path = snapshot_path.with_extension("json.tmp");
    write_owner_only_file(&tmp_path, &raw).await?;
    tokio::fs::rename(tmp_path, snapshot_path).await?;
    enforce_owner_only_permissions(snapshot_path).await?;
    Ok(())
}

#[cfg(unix)]
async fn write_owner_only_file(path: &Path, raw: &[u8]) -> Result<(), TransientStorageError> {
    let path = path.to_path_buf();
    let raw = raw.to_vec();
    tokio::task::spawn_blocking(move || {
        use std::{
            fs::{OpenOptions, Permissions},
            io::Write,
            os::unix::fs::{OpenOptionsExt, PermissionsExt},
        };

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(Permissions::from_mode(0o600))?;
        file.write_all(&raw)?;
        file.sync_all()?;
        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|err| TransientStorageError::TaskJoin(err.to_string()))??;
    Ok(())
}

#[cfg(not(unix))]
async fn write_owner_only_file(path: &Path, raw: &[u8]) -> Result<(), TransientStorageError> {
    tokio::fs::write(path, raw).await?;
    Ok(())
}

#[cfg(unix)]
async fn enforce_owner_only_permissions(path: &Path) -> Result<(), TransientStorageError> {
    use std::{fs::Permissions, os::unix::fs::PermissionsExt};

    tokio::fs::set_permissions(path, Permissions::from_mode(0o600)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn enforce_owner_only_permissions(_path: &Path) -> Result<(), TransientStorageError> {
    Ok(())
}

async fn persist_transient_snapshot(
    snapshot_path: &Path,
    snapshot: Option<&TransientWorkspaceSnapshot>,
) -> Result<(), TransientStorageError> {
    if let Some(snapshot) = snapshot {
        write_transient_snapshot(snapshot_path, snapshot).await
    } else {
        match tokio::fs::remove_file(snapshot_path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_VAR_LOCK;
    use crate::{RuntimeBackend, RuntimeHandleSnapshot, WorkspaceManager};
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        time::Duration,
    };
    use uuid::Uuid;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "multicode-transient-storage-{}-{}",
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

        fn remove(key: &'static str) -> Self {
            let old_value = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, old_value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.old_value {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    #[test]
    fn ensure_storage_directory_creates_symlink_when_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let runtime_dir = root.path().join("runtime");
            fs::create_dir_all(&runtime_dir).expect("runtime dir should exist");
            let _guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

            let link = root.path().join("state/transient-link");
            let target = ensure_storage_directory(&link)
                .await
                .expect("storage directory should be created");

            let metadata = tokio::fs::symlink_metadata(&link)
                .await
                .expect("link should exist");
            assert!(metadata.file_type().is_symlink());
            assert!(target.starts_with(runtime_dir.join("multicode")));

            let target_meta = tokio::fs::metadata(&target)
                .await
                .expect("target should exist");
            assert!(target_meta.is_dir());
        });
    }

    #[test]
    fn ensure_storage_directory_rejects_relative_xdg_runtime_dir() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let _guard = EnvVarGuard::set("XDG_RUNTIME_DIR", Path::new("relative/runtime"));

            let link = root.path().join("state/transient-link");
            let err = ensure_storage_directory(&link)
                .await
                .expect_err("relative runtime dir must be rejected");
            assert!(matches!(
                err,
                TransientStorageError::InvalidXdgRuntimeDir(path) if path == PathBuf::from("relative/runtime")
            ));
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ensure_storage_directory_synthesizes_xdg_runtime_dir_on_macos() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let _guard = EnvVarGuard::remove("XDG_RUNTIME_DIR");

            let link = root.path().join("state/transient-link");
            let target = ensure_storage_directory(&link)
                .await
                .expect("storage directory should be created");

            assert!(
                target.starts_with(
                    synthesized_xdg_runtime_dir()
                        .expect("macOS should synthesize XDG runtime dir")
                        .join("multicode")
                )
            );
        });
    }

    #[test]
    fn ensure_storage_directory_creates_missing_symlink_target() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let link = root.path().join("transient-link");
            let target = root.path().join("missing-target");
            tokio::fs::symlink(&target, &link)
                .await
                .expect("symlink should be created");

            let resolved = ensure_storage_directory(&link)
                .await
                .expect("service should create missing target dir");

            assert_eq!(resolved, target);
            let metadata = tokio::fs::metadata(&target)
                .await
                .expect("target should exist");
            assert!(metadata.is_dir());
        });
    }

    #[test]
    fn transient_storage_loads_existing_snapshots_and_persists_updates() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let storage_dir = root.path().join("storage");
            tokio::fs::create_dir_all(&storage_dir)
                .await
                .expect("storage dir should exist");

            let link = root.path().join("transient-link");
            tokio::fs::symlink(&storage_dir, &link)
                .await
                .expect("symlink should be created");

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");

            let initial_transient = TransientWorkspaceSnapshot {
                uri: "file:///initial".to_string(),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::LinuxSystemdBwrap,
                    id: "run-u-initial.service".to_string(),
                    metadata: Default::default(),
                },
            };
            let snapshot_path = storage_dir.join("alpha.json");
            tokio::fs::write(
                &snapshot_path,
                serde_json::to_vec(&initial_transient).expect("json should serialize"),
            )
            .await
            .expect("initial json should be written");

            let mut alpha_rx = manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .subscribe();
            let service_task = tokio::spawn(transient_storage(manager.clone(), link.clone()));

            tokio::time::timeout(Duration::from_secs(2), alpha_rx.changed())
                .await
                .expect("workspace should receive initial transient update")
                .expect("workspace watch should stay open");
            assert_eq!(alpha_rx.borrow().transient, Some(initial_transient));
            assert!(alpha_rx.borrow().opencode_client.is_none());

            manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .update(|snapshot| {
                    snapshot.transient = Some(TransientWorkspaceSnapshot {
                        uri: "file:///updated".to_string(),
                        runtime: RuntimeHandleSnapshot {
                            backend: RuntimeBackend::LinuxSystemdBwrap,
                            id: "run-u-updated.service".to_string(),
                            metadata: Default::default(),
                        },
                    });
                    true
                });

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let content = tokio::fs::read(&snapshot_path)
                        .await
                        .expect("snapshot file should stay readable");
                    let snapshot: TransientWorkspaceSnapshot =
                        serde_json::from_slice(&content).expect("snapshot should parse");
                    if snapshot.runtime.id == "run-u-updated.service" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("transient snapshot should be persisted after update");

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let metadata = tokio::fs::metadata(&snapshot_path)
                    .await
                    .expect("snapshot metadata should be readable");
                assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            }

            manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .update(|snapshot| {
                    snapshot.transient = None;
                    true
                });

            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match tokio::fs::metadata(&snapshot_path).await {
                        Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
                        Err(err) => panic!("unexpected metadata error: {err}"),
                    }
                }
            })
            .await
            .expect("transient snapshot file should be removed when transient is None");

            service_task.abort();
        });
    }

    #[test]
    fn transient_storage_sets_state_stopped_when_snapshot_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let storage_dir = root.path().join("storage");
            tokio::fs::create_dir_all(&storage_dir)
                .await
                .expect("storage dir should exist");

            let link = root.path().join("transient-link");
            tokio::fs::symlink(&storage_dir, &link)
                .await
                .expect("symlink should be created");

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");

            let alpha_rx = manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .subscribe();
            let service_task = tokio::spawn(transient_storage(manager.clone(), link.clone()));

            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                !alpha_rx
                    .has_changed()
                    .expect("workspace watch should stay open")
            );

            assert_eq!(alpha_rx.borrow().transient, None);
            assert!(alpha_rx.borrow().opencode_client.is_none());
            service_task.abort();
        });
    }

    #[test]
    fn transient_storage_does_not_clobber_live_transient_state_when_disk_snapshot_is_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let storage_dir = root.path().join("storage");
            tokio::fs::create_dir_all(&storage_dir)
                .await
                .expect("storage dir should exist");

            let link = root.path().join("transient-link");
            tokio::fs::symlink(&storage_dir, &link)
                .await
                .expect("symlink should be created");

            let manager = Arc::new(WorkspaceManager::new());
            manager
                .add("alpha")
                .expect("workspace should be added before service starts");

            let live_transient = TransientWorkspaceSnapshot {
                uri: "http://opencode:secret@127.0.0.1:31337/".to_string(),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::AppleContainer,
                    id: "multicode-alpha".to_string(),
                    metadata: Default::default(),
                },
            };
            manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .update(|snapshot| {
                    snapshot.transient = Some(live_transient.clone());
                    true
                });

            let alpha_rx = manager
                .get_workspace("alpha")
                .expect("workspace should exist")
                .subscribe();
            let service_task = tokio::spawn(transient_storage(manager.clone(), link.clone()));

            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(alpha_rx.borrow().transient, Some(live_transient.clone()));

            let snapshot_path = storage_dir.join("alpha.json");
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let content = tokio::fs::read(&snapshot_path)
                        .await
                        .expect("snapshot file should be written");
                    let snapshot: TransientWorkspaceSnapshot =
                        serde_json::from_slice(&content).expect("snapshot should parse");
                    if snapshot == live_transient {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("live transient state should be persisted");

            service_task.abort();
        });
    }
}
