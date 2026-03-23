use std::{collections::HashSet, future::Future, sync::Arc};

use tokio::sync::watch;

use crate::{WorkspaceManager, WorkspaceManagerError, manager::Workspace};

/// Run a function for each workspace. If a new workspace appears, run the function for that
/// workspace too.
pub async fn monitor_workspaces<E, F, Fut>(
    manager: Arc<WorkspaceManager>,
    mut on_new_workspace: F,
) -> Result<(), E>
where
    E: From<WorkspaceManagerError>,
    F: FnMut(String, Workspace) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    let mut initialized = HashSet::<String>::new();
    let mut workspace_set_rx = manager.subscribe();
    let initial_workspaces = workspace_set_rx.borrow().clone();

    process_workspace_set(
        manager.clone(),
        initial_workspaces,
        &mut initialized,
        &mut on_new_workspace,
    )
    .await?;

    while workspace_set_rx.changed().await.is_ok() {
        let known_workspaces = workspace_set_rx.borrow_and_update().clone();
        process_workspace_set(
            manager.clone(),
            known_workspaces,
            &mut initialized,
            &mut on_new_workspace,
        )
        .await?;
    }

    Ok(())
}

/// Run a function for each workspace with an already-subscribed snapshot receiver. If a new
/// workspace appears, run the function for that workspace too.
pub async fn monitor_workspace_snapshots<E, F, Fut>(
    manager: Arc<WorkspaceManager>,
    mut on_new_workspace: F,
) -> Result<(), E>
where
    E: From<WorkspaceManagerError>,
    F: FnMut(String, Workspace, watch::Receiver<crate::WorkspaceSnapshot>) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    monitor_workspaces(manager, move |key, workspace| {
        let workspace_rx = workspace.subscribe();
        on_new_workspace(key, workspace, workspace_rx)
    })
    .await
}

async fn process_workspace_set<E, F, Fut>(
    manager: Arc<WorkspaceManager>,
    known_workspaces: std::collections::BTreeSet<String>,
    initialized: &mut HashSet<String>,
    on_new_workspace: &mut F,
) -> Result<(), E>
where
    E: From<WorkspaceManagerError>,
    F: FnMut(String, Workspace) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    initialized.retain(|key| known_workspaces.contains(key));

    for key in known_workspaces {
        if !initialized.insert(key.clone()) {
            continue;
        }

        let workspace = match manager.get_workspace(&key) {
            Ok(workspace) => workspace,
            Err(WorkspaceManagerError::WorkspaceNotFound(_)) => continue,
            Err(err) => return Err(err.into()),
        };
        on_new_workspace(key, workspace).await?;
    }

    Ok(())
}
