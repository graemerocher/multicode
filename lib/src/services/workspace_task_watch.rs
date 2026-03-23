use tokio::{sync::watch, task::JoinHandle};

use crate::{WorkspaceSnapshot, manager::Workspace};

pub(crate) async fn watch_workspace_task<K, FKey, FDetached, FRestart, FSpawn>(
    workspace: Workspace,
    mut workspace_rx: watch::Receiver<WorkspaceSnapshot>,
    mut derive_key: FKey,
    mut on_detached: FDetached,
    mut on_restart: FRestart,
    mut spawn_task: FSpawn,
) where
    K: PartialEq,
    FKey: FnMut(&WorkspaceSnapshot) -> Option<K>,
    FDetached: FnMut(&Workspace),
    FRestart: FnMut(&Workspace, &K, Option<K>),
    FSpawn: FnMut(&Workspace, &K) -> JoinHandle<()>,
{
    let mut current_task: Option<(K, JoinHandle<()>)> = None;

    loop {
        let snapshot = workspace_rx.borrow().clone();
        let next_key = derive_key(&snapshot);

        let Some(next_key) = next_key else {
            abort_task(&mut current_task);
            on_detached(&workspace);
            if workspace_rx.changed().await.is_err() {
                break;
            }
            continue;
        };

        let should_restart = match &current_task {
            Some((current_key, handle)) => current_key != &next_key || handle.is_finished(),
            None => true,
        };

        if should_restart {
            let previous_key = current_task.take().map(|(key, handle)| {
                handle.abort();
                key
            });
            on_restart(&workspace, &next_key, previous_key);
            let handle = spawn_task(&workspace, &next_key);
            current_task = Some((next_key, handle));
        }

        if workspace_rx.changed().await.is_err() {
            break;
        }
    }

    abort_task(&mut current_task);
}

fn abort_task<K>(task: &mut Option<(K, JoinHandle<()>)>) {
    if let Some((_, handle)) = task.take() {
        handle.abort();
    }
}
