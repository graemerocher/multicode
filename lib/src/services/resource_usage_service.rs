use std::{
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{process::Command, sync::watch};

use super::{
    runtime::{RuntimeActivity, RuntimeUsageSample, RuntimeUsageState, WorkspaceRuntime},
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{WorkspaceManager, WorkspaceManagerError, WorkspaceSnapshot, manager::Workspace};

const RESOURCE_MONITOR_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum ResourceUsageServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for ResourceUsageServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

pub async fn resource_usage_service(
    manager: Arc<WorkspaceManager>,
) -> Result<(), ResourceUsageServiceError> {
    monitor_workspace_snapshots(manager, |_, workspace, workspace_rx| async move {
        tokio::spawn(async move {
            watch_workspace_snapshot(workspace, workspace_rx).await;
        });
        Ok(())
    })
    .await
}

async fn watch_workspace_snapshot(
    workspace: Workspace,
    mut workspace_rx: watch::Receiver<WorkspaceSnapshot>,
) {
    let mut previous_cpu_sample: Option<(u64, Instant)> = None;
    let mut sampled_unit: Option<String> = None;
    let mut next_sample_at: Option<Instant> = None;

    loop {
        let snapshot = workspace_rx.borrow().clone();
        let transient = snapshot.transient.clone();

        let Some(transient) = transient else {
            clear_resource_usage_if_detached(&workspace);
            previous_cpu_sample = None;
            sampled_unit = None;
            next_sample_at = None;
            if workspace_rx.changed().await.is_err() {
                break;
            }
            continue;
        };

        let unit_changed = sampled_unit.as_deref() != Some(transient.runtime.id.as_str());
        if unit_changed {
            previous_cpu_sample = None;
            sampled_unit = Some(transient.runtime.id.clone());
            next_sample_at = None;
        }

        let now = Instant::now();
        if should_sample_usage(now, next_sample_at) {
            match WorkspaceRuntime::read_activity(&transient.runtime).await {
                RuntimeActivity::Stopped => {
                    previous_cpu_sample = None;
                    clear_stale_runtime_for_unit(&workspace, &transient.runtime.id);
                    next_sample_at = Some(now + RESOURCE_MONITOR_INTERVAL);
                    let wait_timeout = next_poll_timeout(Instant::now(), next_sample_at);
                    if !wait_for_change_or_timeout(&mut workspace_rx, wait_timeout).await {
                        break;
                    }
                    continue;
                }
                RuntimeActivity::Active | RuntimeActivity::Unknown => {}
            }

            match WorkspaceRuntime::read_usage(&transient.runtime).await {
                RuntimeUsageSample {
                    state: Some(RuntimeUsageState::Active),
                    memory_current,
                    cpu_usage_nsec,
                } => {
                    let (cpu_percent, next_cpu_sample) =
                        cpu_percent_from_sample(previous_cpu_sample, cpu_usage_nsec, now);
                    previous_cpu_sample = next_cpu_sample;
                    refresh_resource_usage(
                        &workspace,
                        &transient.runtime.id,
                        cpu_percent,
                        memory_current,
                    );
                }
                RuntimeUsageSample { .. } => {
                    previous_cpu_sample = None;
                    clear_resource_usage_for_unit(&workspace, &transient.runtime.id);
                }
            }
            next_sample_at = Some(now + RESOURCE_MONITOR_INTERVAL);
        }

        let wait_timeout = next_poll_timeout(Instant::now(), next_sample_at);
        if !wait_for_change_or_timeout(&mut workspace_rx, wait_timeout).await {
            break;
        }
    }
}

fn should_sample_usage(now: Instant, next_sample_at: Option<Instant>) -> bool {
    match next_sample_at {
        None => true,
        Some(next_sample_at) => now >= next_sample_at,
    }
}

fn next_poll_timeout(now: Instant, next_sample_at: Option<Instant>) -> Duration {
    match next_sample_at {
        Some(next_sample_at) => next_sample_at.saturating_duration_since(now),
        None => RESOURCE_MONITOR_INTERVAL,
    }
}

fn refresh_resource_usage(
    workspace: &Workspace,
    unit: &str,
    cpu_percent: Option<u16>,
    ram_bytes: Option<u64>,
) {
    workspace.update(|snapshot| {
        let still_tracking_same_unit = snapshot
            .transient
            .as_ref()
            .map(|transient| transient.runtime.id.as_str() == unit)
            .unwrap_or(false);
        let should_update = still_tracking_same_unit
            && (snapshot.usage_cpu_percent != cpu_percent || snapshot.usage_ram_bytes != ram_bytes);
        if should_update {
            snapshot.usage_cpu_percent = cpu_percent;
            snapshot.usage_ram_bytes = ram_bytes;
            true
        } else {
            false
        }
    });
}

fn clear_resource_usage_if_detached(workspace: &Workspace) {
    workspace.update(|snapshot| {
        let has_usage = snapshot.usage_cpu_percent.is_some() || snapshot.usage_ram_bytes.is_some();
        if has_usage && snapshot.transient.is_none() {
            snapshot.usage_cpu_percent = None;
            snapshot.usage_ram_bytes = None;
            true
        } else {
            false
        }
    });
}

fn clear_resource_usage_for_unit(workspace: &Workspace, unit: &str) {
    workspace.update(|snapshot| {
        let has_usage = snapshot.usage_cpu_percent.is_some() || snapshot.usage_ram_bytes.is_some();
        let still_tracking_same_unit = snapshot
            .transient
            .as_ref()
            .map(|transient| transient.runtime.id.as_str() == unit)
            .unwrap_or(false);
        if has_usage && still_tracking_same_unit {
            snapshot.usage_cpu_percent = None;
            snapshot.usage_ram_bytes = None;
            true
        } else {
            false
        }
    });
}

fn clear_stale_runtime_for_unit(workspace: &Workspace, unit: &str) {
    workspace.update(|snapshot| {
        let still_tracking_same_unit = snapshot
            .transient
            .as_ref()
            .map(|transient| transient.runtime.id.as_str() == unit)
            .unwrap_or(false);
        if !still_tracking_same_unit {
            return false;
        }

        let mut changed = false;
        if snapshot.transient.is_some() {
            snapshot.transient = None;
            changed = true;
        }
        if snapshot.opencode_client.is_some() {
            snapshot.opencode_client = None;
            changed = true;
        }
        if snapshot.root_session_id.is_some() {
            snapshot.root_session_id = None;
            changed = true;
        }
        if snapshot.root_session_title.is_some() {
            snapshot.root_session_title = None;
            changed = true;
        }
        if snapshot.root_session_status.is_some() {
            snapshot.root_session_status = None;
            changed = true;
        }
        if snapshot.automation_session_id.is_some() {
            snapshot.automation_session_id = None;
            changed = true;
        }
        if snapshot.automation_session_status.is_some() {
            snapshot.automation_session_status = None;
            changed = true;
        }
        if snapshot.automation_agent_state.is_some() {
            snapshot.automation_agent_state = None;
            changed = true;
        }
        if let Some(active_task_id) = snapshot.active_task_id.clone()
            && let Some(task_state) = snapshot.task_states.get_mut(&active_task_id)
        {
            if task_state.session_id.take().is_some() {
                changed = true;
            }
            if task_state.session_status.take().is_some() {
                changed = true;
            }
            if task_state.agent_state.take().is_some() {
                changed = true;
            }
            if task_state.waiting_on_vm {
                task_state.waiting_on_vm = false;
                changed = true;
            }
        }
        if snapshot.usage_cpu_percent.is_some() {
            snapshot.usage_cpu_percent = None;
            changed = true;
        }
        if snapshot.usage_ram_bytes.is_some() {
            snapshot.usage_ram_bytes = None;
            changed = true;
        }
        changed
    });
}

fn cpu_percent_from_sample(
    previous_sample: Option<(u64, Instant)>,
    current_cpu_usage_nsec: Option<u64>,
    now: Instant,
) -> (Option<u16>, Option<(u64, Instant)>) {
    let Some(current_cpu_usage_nsec) = current_cpu_usage_nsec else {
        return (None, None);
    };

    let cpu_percent = previous_sample.and_then(|(previous_cpu_usage_nsec, previous_instant)| {
        let cpu_delta = current_cpu_usage_nsec.saturating_sub(previous_cpu_usage_nsec);
        let elapsed_ns = now.saturating_duration_since(previous_instant).as_nanos();
        if elapsed_ns == 0 {
            return None;
        }

        let percent = ((cpu_delta as f64 / elapsed_ns as f64) * 100.0).round();
        Some(percent.clamp(0.0, u16::MAX as f64) as u16)
    });

    (cpu_percent, Some((current_cpu_usage_nsec, now)))
}

#[cfg_attr(not(test), allow(dead_code))]
async fn read_unit_usage(unit: &str) -> RuntimeUsageSample {
    let output = match Command::new("systemctl")
        .args([
            "--user",
            "show",
            unit,
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
        Err(err) => {
            if err.kind() == std::io::ErrorKind::NotFound {
                return RuntimeUsageSample {
                    state: Some(RuntimeUsageState::Unknown),
                    ..Default::default()
                };
            }
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

    parse_unit_usage_show_output(&String::from_utf8_lossy(&output.stdout))
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_unit_usage_show_output(output: &str) -> RuntimeUsageSample {
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

    let active_state = active_state.unwrap_or_default();
    RuntimeUsageSample {
        memory_current,
        cpu_usage_nsec,
        state: Some(if matches!(active_state, "active" | "activating") {
            RuntimeUsageState::Active
        } else {
            RuntimeUsageState::Stopped
        }),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_systemctl_u64(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "[not set]" {
        return None;
    }
    trimmed.parse::<u64>().ok()
}

async fn wait_for_change_or_timeout(
    workspace_rx: &mut watch::Receiver<WorkspaceSnapshot>,
    timeout: Duration,
) -> bool {
    match tokio::time::timeout(timeout, workspace_rx.changed()).await {
        Ok(changed) => changed.is_ok(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_unit_usage_show_output_parses_active_values() {
        let output = "ActiveState=active\nMemoryCurrent=4096\nCPUUsageNSec=2000000000\n";
        assert_eq!(
            parse_unit_usage_show_output(output),
            RuntimeUsageSample {
                memory_current: Some(4096),
                cpu_usage_nsec: Some(2_000_000_000),
                state: Some(RuntimeUsageState::Active),
            }
        );
    }

    #[test]
    fn parse_unit_usage_show_output_handles_missing_values() {
        let output = "ActiveState=active\nMemoryCurrent=[not set]\n";
        assert_eq!(
            parse_unit_usage_show_output(output),
            RuntimeUsageSample {
                memory_current: None,
                cpu_usage_nsec: None,
                state: Some(RuntimeUsageState::Active),
            }
        );
    }

    #[test]
    fn parse_unit_usage_show_output_reports_stopped_for_inactive_state() {
        assert_eq!(
            parse_unit_usage_show_output("ActiveState=inactive\nMemoryCurrent=0\nCPUUsageNSec=0\n"),
            RuntimeUsageSample {
                memory_current: Some(0),
                cpu_usage_nsec: Some(0),
                state: Some(RuntimeUsageState::Stopped),
            }
        );
    }

    #[test]
    fn parse_unit_usage_show_output_handles_reordered_properties() {
        let output = "CPUUsageNSec=300\nActiveState=active\nMemoryCurrent=1024\n";
        assert_eq!(
            parse_unit_usage_show_output(output),
            RuntimeUsageSample {
                memory_current: Some(1024),
                cpu_usage_nsec: Some(300),
                state: Some(RuntimeUsageState::Active),
            }
        );
    }

    #[test]
    fn cpu_percent_from_sample_uses_delta_between_samples() {
        let now = Instant::now();
        let previous = (1_000_000_000, now - Duration::from_secs(1));
        let (cpu_percent, next_sample) =
            cpu_percent_from_sample(Some(previous), Some(1_500_000_000), now);

        assert_eq!(cpu_percent, Some(50));
        assert_eq!(next_sample, Some((1_500_000_000, now)));
    }

    #[test]
    fn cpu_percent_from_sample_returns_none_without_cpu_usage() {
        let now = Instant::now();
        let (cpu_percent, next_sample) = cpu_percent_from_sample(None, None, now);

        assert_eq!(cpu_percent, None);
        assert_eq!(next_sample, None);
    }

    #[test]
    fn clear_stale_runtime_for_unit_clears_matching_runtime_state() {
        let workspace = crate::manager::Workspace::new(crate::WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot.transient = Some(crate::TransientWorkspaceSnapshot {
                uri: "ws://127.0.0.1:1234".to_string(),
                runtime: crate::RuntimeHandleSnapshot {
                    backend: crate::RuntimeBackend::AppleContainer,
                    id: "runtime-1".to_string(),
                    metadata: Default::default(),
                },
            });
            snapshot.root_session_id = Some("thread-1".to_string());
            snapshot.root_session_title = Some("Codex".to_string());
            snapshot.root_session_status = Some(crate::RootSessionStatus::Busy);
            snapshot.active_task_id = Some("task-42".to_string());
            snapshot.task_states.insert(
                "task-42".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("thread-task-42".to_string()),
                    session_status: Some(crate::RootSessionStatus::Busy),
                    agent_state: Some(crate::AutomationAgentState::Working),
                    ..Default::default()
                },
            );
            snapshot.usage_cpu_percent = Some(25);
            snapshot.usage_ram_bytes = Some(1024);
            true
        });

        clear_stale_runtime_for_unit(&workspace, "runtime-1");

        let snapshot = workspace.subscribe().borrow().clone();
        assert!(snapshot.transient.is_none());
        assert!(snapshot.root_session_id.is_none());
        assert!(snapshot.root_session_title.is_none());
        assert!(snapshot.root_session_status.is_none());
        let task_state = snapshot
            .task_states
            .get("task-42")
            .expect("task state should remain");
        assert!(task_state.session_id.is_none());
        assert!(task_state.session_status.is_none());
        assert!(task_state.agent_state.is_none());
        assert!(snapshot.usage_cpu_percent.is_none());
        assert!(snapshot.usage_ram_bytes.is_none());
    }

    #[test]
    fn should_sample_usage_only_when_interval_has_elapsed() {
        let now = Instant::now();
        assert!(should_sample_usage(now, None));
        assert!(!should_sample_usage(
            now,
            Some(now + Duration::from_millis(1))
        ));
        assert!(should_sample_usage(
            now,
            Some(now - Duration::from_millis(1))
        ));
    }

    #[test]
    fn next_poll_timeout_uses_remaining_interval() {
        let now = Instant::now();
        assert_eq!(next_poll_timeout(now, None), RESOURCE_MONITOR_INTERVAL);
        assert!(
            next_poll_timeout(now, Some(now + Duration::from_millis(250)))
                <= Duration::from_millis(250)
        );
        assert_eq!(
            next_poll_timeout(now, Some(now - Duration::from_millis(1))),
            Duration::ZERO
        );
    }
}
