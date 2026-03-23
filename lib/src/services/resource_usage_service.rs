use std::{
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{process::Command, sync::watch};

use super::workspace_watch::monitor_workspace_snapshots;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UnitUsageSample {
    memory_current: Option<u64>,
    cpu_usage_nsec: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitUsageState {
    Active(UnitUsageSample),
    Stopped,
    Unknown,
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

        let unit_changed = sampled_unit.as_deref() != Some(transient.unit.as_str());
        if unit_changed {
            previous_cpu_sample = None;
            sampled_unit = Some(transient.unit.clone());
            next_sample_at = None;
        }

        let now = Instant::now();
        if should_sample_usage(now, next_sample_at) {
            match read_unit_usage(&transient.unit).await {
                UnitUsageState::Active(usage_sample) => {
                    let (cpu_percent, next_cpu_sample) = cpu_percent_from_sample(
                        previous_cpu_sample,
                        usage_sample.cpu_usage_nsec,
                        now,
                    );
                    previous_cpu_sample = next_cpu_sample;
                    refresh_resource_usage(
                        &workspace,
                        &transient.unit,
                        cpu_percent,
                        usage_sample.memory_current,
                    );
                }
                UnitUsageState::Stopped | UnitUsageState::Unknown => {
                    previous_cpu_sample = None;
                    clear_resource_usage_for_unit(&workspace, &transient.unit);
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
            .map(|transient| transient.unit.as_str() == unit)
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
            .map(|transient| transient.unit.as_str() == unit)
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

async fn read_unit_usage(unit: &str) -> UnitUsageState {
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
                return UnitUsageState::Unknown;
            }
            return UnitUsageState::Unknown;
        }
    };

    if !output.status.success() {
        return UnitUsageState::Stopped;
    }

    parse_unit_usage_show_output(&String::from_utf8_lossy(&output.stdout))
}

fn parse_unit_usage_show_output(output: &str) -> UnitUsageState {
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
    if !matches!(active_state, "active" | "activating") {
        return UnitUsageState::Stopped;
    }

    UnitUsageState::Active(UnitUsageSample {
        memory_current,
        cpu_usage_nsec,
    })
}

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
            UnitUsageState::Active(UnitUsageSample {
                memory_current: Some(4096),
                cpu_usage_nsec: Some(2_000_000_000),
            })
        );
    }

    #[test]
    fn parse_unit_usage_show_output_handles_missing_values() {
        let output = "ActiveState=active\nMemoryCurrent=[not set]\n";
        assert_eq!(
            parse_unit_usage_show_output(output),
            UnitUsageState::Active(UnitUsageSample {
                memory_current: None,
                cpu_usage_nsec: None,
            })
        );
    }

    #[test]
    fn parse_unit_usage_show_output_reports_stopped_for_inactive_state() {
        assert_eq!(
            parse_unit_usage_show_output("ActiveState=inactive\nMemoryCurrent=0\nCPUUsageNSec=0\n"),
            UnitUsageState::Stopped
        );
    }

    #[test]
    fn parse_unit_usage_show_output_handles_reordered_properties() {
        let output = "CPUUsageNSec=300\nActiveState=active\nMemoryCurrent=1024\n";
        assert_eq!(
            parse_unit_usage_show_output(output),
            UnitUsageState::Active(UnitUsageSample {
                memory_current: Some(1024),
                cpu_usage_nsec: Some(300),
            })
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
