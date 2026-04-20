use std::sync::Arc;

use tokio::sync::watch;

use super::{
    runtime::{RUNTIME_SPEC_METADATA_KEY, WorkspaceRuntime},
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{
    RuntimeBackend, TransientWorkspaceSnapshot, WorkspaceManager, WorkspaceManagerError,
    WorkspaceSnapshot, manager::Workspace,
};

#[derive(Debug)]
#[allow(dead_code)]
pub(super) enum RuntimeReconciliationServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for RuntimeReconciliationServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

pub(super) async fn runtime_reconciliation_service(
    manager: Arc<WorkspaceManager>,
    runtime: WorkspaceRuntime,
) -> Result<(), RuntimeReconciliationServiceError> {
    let expected_backend = runtime.backend();
    let expected_spec = runtime.runtime_spec();
    monitor_workspace_snapshots(manager, move |key, workspace, workspace_rx| {
        let runtime = runtime.clone();
        let expected_spec = expected_spec.clone();
        async move {
            tokio::spawn(async move {
                watch_workspace_snapshot(
                    key,
                    workspace,
                    workspace_rx,
                    runtime,
                    expected_backend,
                    expected_spec,
                )
                .await;
            });
            Ok(())
        }
    })
    .await
}

async fn watch_workspace_snapshot(
    key: String,
    workspace: Workspace,
    mut workspace_rx: watch::Receiver<WorkspaceSnapshot>,
    runtime: WorkspaceRuntime,
    expected_backend: RuntimeBackend,
    expected_spec: String,
) {
    loop {
        let current_transient = workspace_rx.borrow().transient.clone();
        if let Some(transient) = current_transient {
            if should_invalidate_runtime(&transient, expected_backend, &expected_spec) {
                tracing::info!(
                    workspace_key = %key,
                    runtime_id = %transient.runtime.id,
                    expected_backend = ?expected_backend,
                    actual_backend = ?transient.runtime.backend,
                    "stopping stale workspace runtime because the runtime specification changed"
                );
                if let Err(err) = runtime.stop_server(&transient.runtime).await {
                    tracing::warn!(
                        workspace_key = %key,
                        runtime_id = %transient.runtime.id,
                        error = ?err,
                        "failed to stop stale workspace runtime during reconciliation"
                    );
                }
                workspace.update(|snapshot| {
                    if snapshot.transient.as_ref() == Some(&transient) {
                        snapshot.transient = None;
                        true
                    } else {
                        false
                    }
                });
            }
        }

        if workspace_rx.changed().await.is_err() {
            break;
        }
    }
}

fn should_invalidate_runtime(
    transient: &TransientWorkspaceSnapshot,
    expected_backend: RuntimeBackend,
    expected_spec: &str,
) -> bool {
    if transient.runtime.backend != expected_backend {
        return true;
    }

    transient.runtime.backend == RuntimeBackend::AppleContainer
        && transient
            .runtime
            .metadata
            .get(RUNTIME_SPEC_METADATA_KEY)
            .map(String::as_str)
            != Some(expected_spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RuntimeHandleSnapshot, TransientWorkspaceSnapshot};
    use std::collections::BTreeMap;

    #[test]
    fn runtime_reconciliation_invalidates_apple_runtime_without_matching_spec() {
        let transient = TransientWorkspaceSnapshot {
            uri: "http://opencode:secret@127.0.0.1:31337/".to_string(),
            runtime: RuntimeHandleSnapshot {
                backend: RuntimeBackend::AppleContainer,
                id: "multicode-alpha".to_string(),
                metadata: BTreeMap::new(),
            },
        };

        assert!(should_invalidate_runtime(
            &transient,
            RuntimeBackend::AppleContainer,
            "expected"
        ));
    }

    #[test]
    fn runtime_reconciliation_keeps_apple_runtime_with_matching_spec() {
        let transient = TransientWorkspaceSnapshot {
            uri: "http://opencode:secret@127.0.0.1:31337/".to_string(),
            runtime: RuntimeHandleSnapshot {
                backend: RuntimeBackend::AppleContainer,
                id: "multicode-alpha".to_string(),
                metadata: BTreeMap::from([(
                    RUNTIME_SPEC_METADATA_KEY.to_string(),
                    "expected".to_string(),
                )]),
            },
        };

        assert!(!should_invalidate_runtime(
            &transient,
            RuntimeBackend::AppleContainer,
            "expected"
        ));
    }
}
