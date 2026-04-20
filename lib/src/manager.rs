use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, RwLock},
};

use tokio::sync::watch;

use crate::WorkspaceSnapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceManagerError {
    WorkspaceAlreadyExists(String),
    WorkspaceNotFound(String),
}

#[derive(Debug, Clone)]
pub struct Workspace {
    snapshot_tx: Arc<RwLock<Option<watch::Sender<WorkspaceSnapshot>>>>,
}

impl Workspace {
    pub fn new(snapshot: WorkspaceSnapshot) -> Self {
        let (snapshot_tx, _) = watch::channel(snapshot);
        Self {
            snapshot_tx: Arc::new(RwLock::new(Some(snapshot_tx))),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<WorkspaceSnapshot> {
        self.snapshot_tx
            .read()
            .expect("workspace lock poisoned")
            .as_ref()
            .expect("workspace is closed")
            .subscribe()
    }

    pub fn update<F>(&self, updater: F)
    where
        F: FnOnce(&mut WorkspaceSnapshot) -> bool,
    {
        if let Some(snapshot_tx) = self
            .snapshot_tx
            .read()
            .expect("workspace lock poisoned")
            .as_ref()
        {
            let _ = snapshot_tx.send_if_modified(updater);
        }
    }

    pub fn close(&self) {
        self.snapshot_tx
            .write()
            .expect("workspace lock poisoned")
            .take();
    }
}

#[derive(Debug)]
pub struct WorkspaceManager {
    workspaces: RwLock<HashMap<String, Workspace>>,
    workspace_keys_tx: watch::Sender<BTreeSet<String>>,
}

impl Default for WorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceManager {
    pub fn new() -> Self {
        let (workspace_keys_tx, _) = watch::channel(BTreeSet::new());
        Self {
            workspaces: RwLock::new(HashMap::new()),
            workspace_keys_tx,
        }
    }

    pub fn add(
        &self,
        key: impl Into<String>,
    ) -> Result<watch::Receiver<WorkspaceSnapshot>, WorkspaceManagerError> {
        self.add_with_snapshot(key, WorkspaceSnapshot::default())
    }

    pub fn add_with_snapshot(
        &self,
        key: impl Into<String>,
        snapshot: WorkspaceSnapshot,
    ) -> Result<watch::Receiver<WorkspaceSnapshot>, WorkspaceManagerError> {
        let key = key.into();
        let mut workspaces = self.workspaces.write().expect("workspace lock poisoned");
        if workspaces.contains_key(&key) {
            return Err(WorkspaceManagerError::WorkspaceAlreadyExists(key));
        }

        let workspace = Workspace::new(snapshot);
        let receiver = workspace.subscribe();
        workspaces.insert(key, workspace);
        drop(workspaces);
        self.publish_workspace_keys();
        Ok(receiver)
    }

    pub fn get_workspace(&self, key: &str) -> Result<Workspace, WorkspaceManagerError> {
        self.workspaces
            .read()
            .expect("workspace lock poisoned")
            .get(key)
            .cloned()
            .ok_or_else(|| WorkspaceManagerError::WorkspaceNotFound(key.to_string()))
    }

    pub fn subscribe(&self) -> watch::Receiver<BTreeSet<String>> {
        self.workspace_keys_tx.subscribe()
    }

    pub fn remove(&self, key: &str) -> Result<(), WorkspaceManagerError> {
        let workspace = self
            .workspaces
            .write()
            .expect("workspace lock poisoned")
            .remove(key)
            .ok_or_else(|| WorkspaceManagerError::WorkspaceNotFound(key.to_string()))?;
        workspace.close();
        self.publish_workspace_keys();
        Ok(())
    }

    fn publish_workspace_keys(&self) {
        let keys = self
            .workspaces
            .read()
            .expect("workspace lock poisoned")
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        self.workspace_keys_tx.send_replace(keys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        PersistentWorkspaceSnapshot, RuntimeBackend, RuntimeHandleSnapshot,
        TransientWorkspaceSnapshot,
    };

    #[test]
    fn add_notifies_workspace_set_watch() {
        let manager = WorkspaceManager::new();
        let mut workspace_set_rx = manager.subscribe();

        assert!(workspace_set_rx.borrow().is_empty());
        assert!(!workspace_set_rx.has_changed().expect("watch still open"));

        let workspace_rx = manager.add("alpha").expect("workspace should be added");
        let initial_snapshot = workspace_rx.borrow().clone();
        assert_eq!(
            initial_snapshot.persistent,
            PersistentWorkspaceSnapshot::default()
        );
        assert!(initial_snapshot.transient.is_none());
        assert!(initial_snapshot.opencode_client.is_none());
        assert!(initial_snapshot.root_session_id.is_none());
        assert!(initial_snapshot.root_session_title.is_none());
        assert!(initial_snapshot.root_session_status.is_none());
        assert!(initial_snapshot.usage_total_tokens.is_none());
        assert!(initial_snapshot.usage_total_cost.is_none());
        assert!(initial_snapshot.usage_cpu_percent.is_none());
        assert!(initial_snapshot.usage_ram_bytes.is_none());
        assert!(initial_snapshot.oom_kill_count.is_none());

        assert!(workspace_set_rx.has_changed().expect("watch still open"));
        assert_eq!(
            workspace_set_rx.borrow_and_update().clone(),
            BTreeSet::from(["alpha".to_string()])
        );
    }

    #[test]
    fn add_fails_if_workspace_already_exists() {
        let manager = WorkspaceManager::new();
        let mut workspace_set_rx = manager.subscribe();

        manager.add("alpha").expect("workspace should be added");
        assert!(workspace_set_rx.has_changed().expect("watch still open"));
        let _ = workspace_set_rx.borrow_and_update();

        let err = manager.add("alpha").expect_err("duplicate must fail");
        assert_eq!(
            err,
            WorkspaceManagerError::WorkspaceAlreadyExists("alpha".to_string())
        );
        assert!(!workspace_set_rx.has_changed().expect("watch still open"));
    }

    #[test]
    fn subscribe_workspace_fails_if_workspace_missing() {
        let manager = WorkspaceManager::new();
        let err = manager
            .get_workspace("missing")
            .expect_err("missing workspace must fail");
        assert_eq!(
            err,
            WorkspaceManagerError::WorkspaceNotFound("missing".to_string())
        );
    }

    #[test]
    fn update_applies_incremental_mutations_atomically() {
        let manager = WorkspaceManager::new();
        let mut workspace_set_rx = manager.subscribe();
        let mut workspace_rx = manager.add("alpha").expect("workspace should be added");
        let workspace = manager
            .get_workspace("alpha")
            .expect("workspace should exist");

        assert!(workspace_set_rx.has_changed().expect("watch still open"));
        let _ = workspace_set_rx.borrow_and_update();

        workspace.update(|snapshot| {
            snapshot.persistent.description = "incrementally updated".to_string();
            snapshot.transient = Some(TransientWorkspaceSnapshot {
                uri: "http://opencode:secret@127.0.0.1:31337/".to_string(),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::LinuxSystemdBwrap,
                    id: "run-u42.service".to_string(),
                    metadata: Default::default(),
                },
            });
            true
        });

        assert!(workspace_rx.has_changed().expect("watch still open"));
        let updated = workspace_rx.borrow_and_update().clone();
        assert_eq!(updated.persistent.description, "incrementally updated");
        assert_eq!(
            updated.transient.as_ref().map(|t| t.runtime.id.as_str()),
            Some("run-u42.service")
        );
        assert!(!workspace_set_rx.has_changed().expect("watch still open"));
    }

    #[test]
    fn remove_notifies_workspace_set_watch_and_closes_workspace() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let manager = WorkspaceManager::new();
            let mut workspace_set_rx = manager.subscribe();
            let mut workspace_rx = manager.add("alpha").expect("workspace should be added");
            let _ = workspace_set_rx.borrow_and_update();

            manager
                .remove("alpha")
                .expect("workspace should be removed");

            assert!(workspace_set_rx.has_changed().expect("watch still open"));
            assert!(workspace_set_rx.borrow_and_update().is_empty());
            assert!(workspace_rx.changed().await.is_err());
            assert!(manager.get_workspace("alpha").is_err());
        });
    }
}
