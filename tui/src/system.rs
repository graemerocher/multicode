use crate::*;
use rustix::fs::statvfs;
use std::path::Path;

pub(crate) fn started_workspace_attach_ready(
    snapshot: &WorkspaceSnapshot,
    wait_started_at: Option<Instant>,
    now: Instant,
) -> (bool, Option<Instant>) {
    if workspace_state(snapshot) != WorkspaceUiState::Started {
        return (false, None);
    }

    if snapshot.root_session_id.is_some() || codex_workspace_reachable_without_root_thread(snapshot)
    {
        return (true, None);
    }

    match wait_started_at {
        None => (false, Some(now)),
        Some(started_at) if now.duration_since(started_at) < ROOT_SESSION_ATTACH_WAIT_TIMEOUT => {
            (false, Some(started_at))
        }
        Some(_) => (true, None),
    }
}

fn codex_workspace_reachable_without_root_thread(snapshot: &WorkspaceSnapshot) -> bool {
    snapshot
        .transient
        .as_ref()
        .and_then(|transient| url::Url::parse(&transient.uri).ok())
        .is_some_and(|uri| matches!(uri.scheme(), "ws" | "wss"))
        && snapshot.root_session_id.is_none()
        && snapshot.root_session_status.is_some()
}

pub(crate) async fn read_machine_cpu_totals() -> io::Result<ProcCpuTotals> {
    let proc_stat = tokio::fs::read_to_string("/proc/stat").await?;
    parse_proc_cpu_totals(&proc_stat).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing aggregate cpu totals in /proc/stat",
        )
    })
}

pub(crate) fn parse_proc_cpu_totals(contents: &str) -> Option<ProcCpuTotals> {
    let cpu_line = contents.lines().find(|line| line.starts_with("cpu "))?;
    let mut fields = cpu_line.split_whitespace();
    let _cpu_label = fields.next()?;
    let values = fields
        .map(|field| field.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;

    let total = values.iter().copied().sum::<u64>();
    if total == 0 {
        return None;
    }

    let active = values.first().copied().unwrap_or(0)
        + values.get(1).copied().unwrap_or(0)
        + values.get(2).copied().unwrap_or(0)
        + values.get(5).copied().unwrap_or(0)
        + values.get(6).copied().unwrap_or(0)
        + values.get(7).copied().unwrap_or(0);

    Some(ProcCpuTotals { active, total })
}

pub(crate) fn machine_cpu_percent(
    previous: ProcCpuTotals,
    current: ProcCpuTotals,
    logical_cpu_count: usize,
) -> Option<u16> {
    let total_delta = current.total.saturating_sub(previous.total);
    if total_delta == 0 {
        return None;
    }

    let active_delta = current.active.saturating_sub(previous.active);
    let logical_cpu_count = logical_cpu_count.max(1) as f64;
    let usage = (active_delta as f64 / total_delta as f64) * 100.0 * logical_cpu_count;
    Some(usage.round().clamp(0.0, u16::MAX as f64) as u16)
}

pub(crate) async fn read_machine_used_ram_bytes() -> io::Result<Option<u64>> {
    let meminfo = tokio::fs::read_to_string("/proc/meminfo").await?;
    Ok(parse_proc_meminfo_used_ram_bytes(&meminfo))
}

pub(crate) async fn read_machine_total_ram_bytes() -> io::Result<Option<u64>> {
    let meminfo = tokio::fs::read_to_string("/proc/meminfo").await?;
    Ok(parse_proc_meminfo_total_ram_bytes(&meminfo))
}

pub(crate) async fn read_disk_usage(path: &Path) -> io::Result<Option<DiskUsage>> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_disk_usage_blocking(&path))
        .await
        .map_err(|err| io::Error::other(format!("disk usage task failed: {err}")))?
}

fn read_disk_usage_blocking(path: &Path) -> io::Result<Option<DiskUsage>> {
    let stats = statvfs(path).map_err(io::Error::other)?;
    Ok(disk_usage_from_statvfs(
        stats.f_blocks,
        stats.f_bavail,
        stats.f_frsize,
    ))
}

pub(crate) fn disk_usage_from_statvfs(
    total_blocks: u64,
    available_blocks: u64,
    fragment_size: u64,
) -> Option<DiskUsage> {
    if fragment_size == 0 {
        return None;
    }
    let total_bytes = total_blocks.checked_mul(fragment_size)?;
    let free_bytes = available_blocks.checked_mul(fragment_size)?;
    Some(DiskUsage {
        free_bytes,
        total_bytes,
    })
}

pub(crate) fn parse_proc_meminfo_used_ram_bytes(contents: &str) -> Option<u64> {
    let mut total_kib = None;
    let mut available_kib = None;

    for line in contents.lines() {
        if line.starts_with("MemTotal:") {
            total_kib = parse_meminfo_kib_value(line);
        } else if line.starts_with("MemAvailable:") {
            available_kib = parse_meminfo_kib_value(line);
        }
    }

    let total_kib = total_kib?;
    let available_kib = available_kib?;
    Some(total_kib.saturating_sub(available_kib).saturating_mul(1024))
}

pub(crate) fn parse_proc_meminfo_total_ram_bytes(contents: &str) -> Option<u64> {
    contents
        .lines()
        .find(|line| line.starts_with("MemTotal:"))
        .and_then(parse_meminfo_kib_value)
        .map(|total_kib| total_kib.saturating_mul(1024))
}

pub(crate) fn parse_meminfo_kib_value(line: &str) -> Option<u64> {
    let mut parts = line.split_whitespace();
    let _label = parts.next()?;
    let value = parts.next()?.parse::<u64>().ok()?;
    let unit = parts.next()?;
    if unit == "kB" { Some(value) } else { None }
}

pub(crate) fn centered_rect_fixed(width: u16, height: u16, area: Rect) -> Rect {
    let popup_width = width.clamp(3, area.width.max(3));
    let popup_height = height.clamp(3, area.height.max(3));

    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(popup_height),
            Constraint::Fill(1),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(popup_width),
            Constraint::Fill(1),
        ])
        .split(popup_layout[1])[1]
}
