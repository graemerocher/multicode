use std::{collections::BTreeMap, path::Path};

use crate::{
    WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot,
    services::workspace_archive::ArchiveWorkspaceEntry,
};

#[derive(Debug)]
pub enum WorkspaceDirectoryError {
    Io(std::io::Error),
    Manager(WorkspaceManagerError),
    DuplicateWorkspaceEntry(String),
}

impl From<std::io::Error> for WorkspaceDirectoryError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<WorkspaceManagerError> for WorkspaceDirectoryError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

/// Automatically discover workspaces from the workspace directory and add them to the manager.
pub async fn workspace_directory(
    manager: &WorkspaceManager,
    workspace_root: impl AsRef<Path>,
) -> Result<(), WorkspaceDirectoryError> {
    let mut discovered = BTreeMap::<String, WorkspaceSnapshot>::new();
    let mut entries = tokio::fs::read_dir(workspace_root.as_ref()).await?;
    while let Some(entry) = entries.next_entry().await? {
        let file_type = entry.file_type().await?;
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }

        if file_type.is_dir() {
            if discovered
                .insert(name.clone(), WorkspaceSnapshot::default())
                .is_some()
            {
                return Err(WorkspaceDirectoryError::DuplicateWorkspaceEntry(name));
            }
            continue;
        }

        if file_type.is_file()
            && let Some(archive_entry) = ArchiveWorkspaceEntry::parse(Path::new(&name))
        {
            if discovered.contains_key(&archive_entry.key) {
                return Err(WorkspaceDirectoryError::DuplicateWorkspaceEntry(
                    archive_entry.key,
                ));
            }

            let mut snapshot = WorkspaceSnapshot::default();
            snapshot.persistent.archived = true;
            snapshot.persistent.archive_format = Some(archive_entry.format);
            discovered.insert(archive_entry.key, snapshot);
        }
    }

    for (key, snapshot) in discovered {
        manager.add_with_snapshot(key, snapshot)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PersistentWorkspaceSnapshot, WorkspaceArchiveFormat, WorkspaceManagerError};
    use std::{
        collections::BTreeSet,
        fs::{self, File},
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
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
                "multicode-workspace-directory-{}-{}",
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
    fn workspace_directory_adds_non_hidden_directories_with_default_snapshots() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            fs::create_dir(root.path().join("alpha")).expect("alpha directory should exist");
            fs::create_dir(root.path().join("beta")).expect("beta directory should exist");
            fs::create_dir(root.path().join(".hidden")).expect("hidden directory should exist");
            File::create(root.path().join("README.txt")).expect("test file should be created");

            let manager = WorkspaceManager::new();
            workspace_directory(&manager, root.path())
                .await
                .expect("directory scan should succeed");

            let workspace_keys_rx = manager.subscribe();
            assert_eq!(
                workspace_keys_rx.borrow().clone(),
                BTreeSet::from(["alpha".to_string(), "beta".to_string()])
            );

            let alpha = manager
                .get_workspace("alpha")
                .expect("alpha workspace should exist")
                .subscribe();
            let beta = manager
                .get_workspace("beta")
                .expect("beta workspace should exist")
                .subscribe();

            let alpha_snapshot = alpha.borrow().clone();
            assert_eq!(
                alpha_snapshot.persistent,
                PersistentWorkspaceSnapshot::default()
            );
            assert!(alpha_snapshot.transient.is_none());
            assert!(alpha_snapshot.opencode_client.is_none());

            let beta_snapshot = beta.borrow().clone();
            assert_eq!(
                beta_snapshot.persistent,
                PersistentWorkspaceSnapshot::default()
            );
            assert!(beta_snapshot.transient.is_none());
            assert!(beta_snapshot.opencode_client.is_none());

            let hidden_err = manager
                .get_workspace(".hidden")
                .expect_err("hidden workspace should not be added");
            assert_eq!(
                hidden_err,
                WorkspaceManagerError::WorkspaceNotFound(".hidden".to_string())
            );
        });
    }

    #[test]
    fn workspace_directory_fails_when_workspace_already_exists() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            fs::create_dir(root.path().join("alpha")).expect("alpha directory should exist");

            let manager = WorkspaceManager::new();
            manager
                .add("alpha")
                .expect("seed workspace should be added");

            let err = workspace_directory(&manager, root.path())
                .await
                .expect_err("duplicate workspace must fail");
            assert!(matches!(
                err,
                WorkspaceDirectoryError::Manager(WorkspaceManagerError::WorkspaceAlreadyExists(key)) if key == "alpha"
            ));
        });
    }

    #[test]
    fn workspace_directory_adds_supported_archive_files_as_archived_workspaces() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            File::create(root.path().join("alpha.tar.zstd")).expect("archive should exist");
            File::create(root.path().join("beta.tar.xz")).expect("archive should exist");
            File::create(root.path().join("gamma.zip")).expect("archive should exist");

            let manager = WorkspaceManager::new();
            workspace_directory(&manager, root.path())
                .await
                .expect("directory scan should succeed");

            let keys = manager.subscribe().borrow().clone();
            assert_eq!(
                keys,
                BTreeSet::from(["alpha".to_string(), "beta".to_string(), "gamma".to_string()])
            );

            let alpha = manager
                .get_workspace("alpha")
                .expect("alpha workspace should exist")
                .subscribe();
            assert!(alpha.borrow().persistent.archived);
            assert_eq!(
                alpha.borrow().persistent.archive_format,
                Some(WorkspaceArchiveFormat::TarZstd)
            );
        });
    }

    #[test]
    fn workspace_directory_fails_when_directory_and_archive_share_key() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            fs::create_dir(root.path().join("alpha")).expect("alpha directory should exist");
            File::create(root.path().join("alpha.tar.zstd")).expect("archive should exist");

            let manager = WorkspaceManager::new();
            let err = workspace_directory(&manager, root.path())
                .await
                .expect_err("duplicate logical workspace must fail");
            assert!(matches!(
                err,
                WorkspaceDirectoryError::DuplicateWorkspaceEntry(key) if key == "alpha"
            ));
        });
    }

    #[test]
    fn workspace_directory_ignores_hidden_isolate_archive_files() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            fs::create_dir_all(root.path().join(".multicode").join("isolate"))
                .expect("hidden isolate dir should exist");
            File::create(
                root.path()
                    .join(".multicode")
                    .join("isolate")
                    .join("alpha.tar.zstd"),
            )
            .expect("hidden isolate archive should exist");

            let manager = WorkspaceManager::new();
            workspace_directory(&manager, root.path())
                .await
                .expect("directory scan should succeed");

            assert!(manager.subscribe().borrow().is_empty());
        });
    }
}
