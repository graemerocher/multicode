use std::{
    collections::{BTreeSet, HashMap, HashSet},
    error::Error,
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use multicode_lib::{
    AutomationAgentState, RootSessionStatus, WorkspaceSnapshot, logging, opencode,
    services::{
        CombinedService, GithubIssueState, GithubIssueStatus, GithubPrBuildState,
        GithubPrReviewState, GithubPrState, GithubPrStatus, GithubStatus, ToolConfig, ToolType,
    },
    tree_scan,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use size::{Base, Size, Style as SizeStyle};
use tokio::{
    process::Command,
    sync::{oneshot, watch},
};
use url::Url;

use crate::render::draw_ui;

mod app;
mod icons;
mod ops;
mod render;
mod system;
#[cfg(test)]
mod tests;

const CREATE_ROW_LABEL: &str = "Create new workspace…";
const SECONDARY_ROW_COLOR: Color = Color::DarkGray;
const CREATE_ROW_COLOR: Color = Color::LightBlue;
const IDLE_COLOR: Color = Color::Indexed(208);
const BUSY_COLOR: Color = Color::Green;
const WAITING_FOR_INPUT_COLOR: Color = Color::Yellow;
const DESCRIPTION_COLOR: Color = Color::LightBlue;
const AGENT_LINK_COLOR: Color = Color::Rgb(255, 182, 193);
const OOM_COLOR: Color = Color::Red;
const RAM_LIMIT_WARNING_HEADROOM_BYTES: u64 = 512 * 1024 * 1024;
const RAM_COLUMN_WIDTH: u16 = 10;
const LINK_COLUMN_WIDTH: u16 = 2;
const STATUS_COLUMN_WIDTH: u16 = 2;
const REVIEW_STATUS_COLUMN_WIDTH: u16 = 2;
const CPU_COLUMN_MIN_WIDTH: u16 = 5;
const MACHINE_USAGE_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const ROOT_SESSION_ATTACH_WAIT_TIMEOUT: Duration = Duration::from_secs(1);
const PROMPT_TOOL_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const UI_IDLE_POLL_INTERVAL: Duration = Duration::from_millis(16);
const CREATE_MODAL_WIDTH: u16 = 72;
const CREATE_MODAL_HEIGHT: u16 = 13;
const STARTING_MODAL_WIDTH: u16 = 62;
const STARTING_MODAL_HEIGHT: u16 = 8;
const TOOL_PROGRESS_MODAL_WIDTH: u16 = 72;
const TOOL_PROGRESS_MODAL_HEIGHT: u16 = 14;
const CUSTOM_LINK_MODAL_WIDTH: u16 = 72;
const CUSTOM_LINK_MODAL_HEIGHT: u16 = 10;
const CONFIRM_DELETE_MODAL_WIDTH: u16 = 72;
const CONFIRM_DELETE_MODAL_HEIGHT: u16 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiMode {
    Normal,
    CreateModal,
    EditDescription,
    EditIssue,
    EditCustomLink,
    ConfirmDelete,
    StartingModal,
    ToolProgressModal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateModalField {
    Key,
    Repository,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CustomLinkModalAction {
    Add,
    Edit,
}

struct TuiState {
    service: CombinedService,
    relay_socket: Option<PathBuf>,
    workspace_keys_rx: watch::Receiver<BTreeSet<String>>,
    workspace_rxs: HashMap<String, watch::Receiver<WorkspaceSnapshot>>,
    snapshots: HashMap<String, WorkspaceSnapshot>,
    workspace_link_validation_results: HashMap<WorkspaceLink, WorkspaceLinkValidationResult>,
    pending_workspace_link_validations:
        HashMap<WorkspaceLink, oneshot::Receiver<io::Result<String>>>,
    github_link_status_rxs: HashMap<WorkspaceLink, watch::Receiver<Option<GithubStatus>>>,
    github_link_statuses: HashMap<WorkspaceLink, GithubLinkStatusView>,
    ordered_keys: Vec<String>,
    selected_row: usize,
    selected_link_index: Option<usize>,
    selected_link_target_index: usize,
    mode: UiMode,
    create_input: String,
    repository_input: String,
    create_field: CreateModalField,
    edit_input: String,
    issue_input: String,
    custom_link_input: String,
    custom_link_kind: Option<WorkspaceLinkKind>,
    custom_link_action: Option<CustomLinkModalAction>,
    custom_link_original_value: Option<String>,
    pending_delete_workspace_key: Option<String>,
    starting_workspace_key: Option<String>,
    started_wait_since: Option<Instant>,
    previous_machine_cpu_totals: Option<ProcCpuTotals>,
    machine_cpu_count: usize,
    machine_cpu_percent: Option<u16>,
    machine_used_ram_bytes: Option<u64>,
    machine_total_ram_bytes: Option<u64>,
    machine_agent_directory_disk_usage: Option<DiskUsage>,
    next_machine_sample_at: Option<Instant>,
    running_operation: Option<RunningOperation>,
    status: String,
    should_quit: bool,
}

struct RunningOperation {
    workspace_key: String,
    operation_name: String,
    progress_rx: watch::Receiver<String>,
    result_rx: oneshot::Receiver<Result<(), String>>,
    cancel: Option<tokio::task::AbortHandle>,
}

fn workspace_is_usable(snapshot: &WorkspaceSnapshot) -> bool {
    !snapshot.persistent.archived
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WorkspaceLinkKind {
    Review,
    Issue,
    Pr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorkspaceLink {
    kind: WorkspaceLinkKind,
    value: String,
    source: WorkspaceLinkSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WorkspaceLinkSource {
    Custom,
    Automation,
    AgentProvided,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkspaceLinkValidationResult {
    Valid(String),
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GithubLinkStatusView {
    Issue(GithubIssueStatus),
    Pr(GithubPrStatus),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum StatusIconKind {
    Eye,
    Server,
    FileDiff,
    GitPullRequest,
    GitPullRequestDraft,
    GitPullRequestClosed,
    GitMerge,
    IssueOpened,
    IssueClosed,
}

impl WorkspaceLink {
    fn label(&self) -> &'static str {
        self.kind.short_label()
    }

    fn handler_template<'a>(&self, service: &'a CombinedService) -> &'a str {
        match self.kind {
            WorkspaceLinkKind::Review => service.config.handler.review.as_str(),
            WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr => service.config.handler.web.as_str(),
        }
    }
    fn is_custom(&self) -> bool {
        self.source == WorkspaceLinkSource::Custom
    }
}

impl WorkspaceLinkKind {
    fn short_label(&self) -> &'static str {
        match self {
            WorkspaceLinkKind::Review => "RE",
            WorkspaceLinkKind::Issue => "IS",
            WorkspaceLinkKind::Pr => "PR",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachTarget {
    Opencode {
        uri: String,
        username: String,
        password: String,
        session_id: Option<String>,
    },
    Codex {
        uri: String,
        thread_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceUiState {
    Stopped,
    Starting,
    Started,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcCpuTotals {
    active: u64,
    total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskUsage {
    free_bytes: u64,
    total_bytes: u64,
}

fn workspace_state(snapshot: &WorkspaceSnapshot) -> WorkspaceUiState {
    let agent_ready = snapshot
        .transient
        .as_ref()
        .and_then(|transient| url::Url::parse(&transient.uri).ok())
        .map(|uri| match uri.scheme() {
            "ws" | "wss" => snapshot.root_session_id.is_some(),
            _ => snapshot.opencode_client.is_some(),
        })
        .unwrap_or(false);
    match (snapshot.transient.is_some(), agent_ready) {
        (false, _) => WorkspaceUiState::Stopped,
        (true, false) => WorkspaceUiState::Starting,
        (true, true) => WorkspaceUiState::Started,
    }
}

fn next_non_stopped_row(
    current_row: usize,
    ordered_keys: &[String],
    snapshots: &HashMap<String, WorkspaceSnapshot>,
    direction: isize,
) -> Option<usize> {
    if direction == 0 {
        return None;
    }

    let mut row = current_row as isize + direction;
    while row > 0 && (row as usize) <= ordered_keys.len() {
        let key = &ordered_keys[row as usize - 1];
        if snapshots
            .get(key)
            .is_some_and(|snapshot| workspace_state(snapshot) != WorkspaceUiState::Stopped)
        {
            return Some(row as usize);
        }
        row += direction;
    }

    None
}

fn server_cell_label(snapshot: &WorkspaceSnapshot) -> &'static str {
    match workspace_state(snapshot) {
        WorkspaceUiState::Stopped => "",
        WorkspaceUiState::Starting => "Starting",
        WorkspaceUiState::Started => match effective_server_status(snapshot) {
            RootSessionStatus::Idle => "Idle",
            RootSessionStatus::Busy => "Busy",
            RootSessionStatus::Question => "Question",
        },
    }
}

fn effective_server_status(snapshot: &WorkspaceSnapshot) -> RootSessionStatus {
    if snapshot.persistent.automation_issue.is_some() {
        if let Some(agent_state) = snapshot.automation_agent_state {
            return match agent_state {
                AutomationAgentState::Working => RootSessionStatus::Busy,
                AutomationAgentState::Question => RootSessionStatus::Question,
                AutomationAgentState::Review
                | AutomationAgentState::Idle
                | AutomationAgentState::Stale => RootSessionStatus::Idle,
            };
        }
        if let Some(status) = snapshot.automation_session_status {
            return status;
        }
    }
    snapshot
        .root_session_status
        .unwrap_or(RootSessionStatus::Idle)
}

fn format_tokens_spaced(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut reversed = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            reversed.push(' ');
        }
        reversed.push(ch);
    }
    reversed.chars().rev().collect()
}

fn format_price(cost: f64) -> String {
    format!("${cost:.2}")
}

fn cost_cell_label(snapshot: &WorkspaceSnapshot) -> String {
    if let Some(cost) = snapshot
        .usage_total_cost
        .filter(|cost| cost.is_finite() && *cost > 0.0)
    {
        return format_price(cost);
    }
    if let Some(tokens) = snapshot.usage_total_tokens {
        return format_tokens_spaced(tokens);
    }
    String::new()
}

fn cpu_cell_label(snapshot: &WorkspaceSnapshot) -> String {
    snapshot
        .usage_cpu_percent
        .map(|cpu_percent| format!("{cpu_percent}%"))
        .unwrap_or_default()
}

fn format_ram_bytes(ram_bytes: u64) -> String {
    Size::from_bytes(ram_bytes as f64)
        .format()
        .with_base(Base::Base2)
        .with_style(SizeStyle::Abbreviated)
        .to_string()
}

fn ram_cell_label(snapshot: &WorkspaceSnapshot) -> String {
    snapshot
        .usage_ram_bytes
        .map(format_ram_bytes)
        .unwrap_or_default()
}

fn ram_cell_style(
    snapshot: &WorkspaceSnapshot,
    memory_max_bytes: Option<u64>,
    archived: bool,
) -> Style {
    if archived {
        return Style::default();
    }

    let Some(usage_ram_bytes) = snapshot.usage_ram_bytes else {
        return Style::default();
    };
    let Some(memory_max_bytes) = memory_max_bytes else {
        return Style::default();
    };

    let warning_threshold = memory_max_bytes.saturating_sub(RAM_LIMIT_WARNING_HEADROOM_BYTES);
    if usage_ram_bytes >= warning_threshold {
        Style::default().fg(OOM_COLOR)
    } else {
        Style::default()
    }
}

fn machine_cpu_cell_label(cpu_percent: Option<u16>) -> String {
    cpu_percent
        .map(|cpu_percent| format!("{cpu_percent}%"))
        .unwrap_or_default()
}

fn machine_ram_cell_label(ram_bytes: Option<u64>) -> String {
    ram_bytes.map(format_ram_bytes).unwrap_or_default()
}

fn machine_description(
    cpu_count: usize,
    total_ram_bytes: Option<u64>,
    agent_directory_disk_usage: Option<DiskUsage>,
) -> String {
    let mut parts = Vec::new();
    parts.push(format!("CPUs: {cpu_count}"));
    if let Some(total_ram_bytes) = total_ram_bytes {
        parts.push(format!("Total RAM: {}", format_ram_bytes(total_ram_bytes)));
    }
    if let Some(agent_directory_disk_usage) = agent_directory_disk_usage {
        parts.push(format!(
            "Free disk: {}",
            format_ram_bytes(agent_directory_disk_usage.free_bytes)
        ));
    }
    parts.join(" · ")
}

#[cfg(test)]
fn description_cell_text(snapshot: &WorkspaceSnapshot, user_description: &str) -> String {
    let automation_status = snapshot.automation_status.as_deref().unwrap_or("").trim();
    let session_title = snapshot.root_session_title.as_deref().unwrap_or("").trim();
    let mut parts = Vec::new();
    if !user_description.is_empty() {
        parts.push(user_description.to_string());
    }
    if !automation_status.is_empty() {
        parts.push(automation_status.to_string());
    }
    if !session_title.is_empty() {
        parts.push(session_title.to_string());
    }
    parts.join(" · ")
}

fn workspace_links(snapshot: &WorkspaceSnapshot) -> Vec<WorkspaceLink> {
    let mut links = Vec::new();

    links.extend(
        snapshot
            .persistent
            .automation_issue
            .iter()
            .cloned()
            .map(|value| WorkspaceLink {
                kind: WorkspaceLinkKind::Issue,
                value,
                source: WorkspaceLinkSource::Automation,
            }),
    );
    links.extend(
        snapshot
            .persistent
            .agent_provided
            .repo
            .iter()
            .cloned()
            .map(|value| WorkspaceLink {
                kind: WorkspaceLinkKind::Review,
                value,
                source: WorkspaceLinkSource::AgentProvided,
            }),
    );
    links.extend(
        snapshot
            .persistent
            .custom_links
            .issue
            .iter()
            .cloned()
            .map(|value| WorkspaceLink {
                kind: WorkspaceLinkKind::Issue,
                value,
                source: WorkspaceLinkSource::Custom,
            }),
    );
    links.extend(
        snapshot
            .persistent
            .agent_provided
            .issue
            .iter()
            .cloned()
            .map(|value| WorkspaceLink {
                kind: WorkspaceLinkKind::Issue,
                value,
                source: WorkspaceLinkSource::AgentProvided,
            }),
    );
    links.extend(
        snapshot
            .persistent
            .custom_links
            .pr
            .iter()
            .cloned()
            .map(|value| WorkspaceLink {
                kind: WorkspaceLinkKind::Pr,
                value,
                source: WorkspaceLinkSource::Custom,
            }),
    );
    links.extend(
        snapshot
            .persistent
            .agent_provided
            .pr
            .iter()
            .cloned()
            .map(|value| WorkspaceLink {
                kind: WorkspaceLinkKind::Pr,
                value,
                source: WorkspaceLinkSource::AgentProvided,
            }),
    );

    links
}

#[cfg(test)]
fn description_line(
    snapshot: &WorkspaceSnapshot,
    user_description: &str,
    archived: bool,
) -> Line<'static> {
    description_line_for_snapshot(snapshot, user_description, archived)
}

fn description_line_for_snapshot(
    snapshot: &WorkspaceSnapshot,
    user_description: &str,
    archived: bool,
) -> Line<'static> {
    let session_title = snapshot.root_session_title.as_deref().unwrap_or("").trim();
    let automation_status = snapshot.automation_status.as_deref().unwrap_or("").trim();
    let has_session_title = !session_title.is_empty();
    let has_automation_status = !automation_status.is_empty();
    let has_description = !user_description.is_empty();

    let mut spans = Vec::new();
    if snapshot.oom_kill_count.unwrap_or(0) > 0 {
        spans.push(Span::styled(
            "OOM ",
            Style::default().fg(OOM_COLOR).add_modifier(Modifier::BOLD),
        ));
    }

    let mut has_content = false;

    if has_description {
        has_content = true;
        if archived {
            spans.push(Span::raw(user_description.to_string()));
        } else {
            spans.push(Span::styled(
                user_description.to_string(),
                Style::default().fg(DESCRIPTION_COLOR),
            ));
        }
    }

    if has_automation_status {
        if has_content {
            spans.push(Span::raw(" · "));
        }
        let automation_text = if archived || !automation_status_shows_activity(automation_status) {
            automation_status.to_string()
        } else {
            format!("{} {}", automation_activity_glyph(), automation_status)
        };
        spans.push(Span::raw(automation_text));
        has_content = true;
    }

    if has_session_title {
        if has_content {
            spans.push(Span::raw(" · "));
        }
        spans.push(Span::raw(session_title.to_string()));
    }

    Line::from(spans)
}

fn automation_activity_glyph() -> &'static str {
    const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];
    let frame = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| ((elapsed.as_millis() / 200) as usize) % FRAMES.len())
        .unwrap_or(0);
    FRAMES[frame]
}

fn automation_status_shows_activity(status: &str) -> bool {
    !matches!(
        status,
        status if status.starts_with("Start failed")
            || status.starts_with("Scan failed")
            || status.starts_with("No issues")
    )
}

fn first_validated_workspace_link_by_kind(
    snapshot: &WorkspaceSnapshot,
    validations: &HashMap<WorkspaceLink, WorkspaceLinkValidationResult>,
    kind: WorkspaceLinkKind,
) -> Option<WorkspaceLink> {
    workspace_links(snapshot).into_iter().find(|link| {
        link.kind == kind
            && matches!(
                validations.get(link),
                Some(WorkspaceLinkValidationResult::Valid(_))
            )
    })
}

fn validated_workspace_links_by_kind(
    snapshot: &WorkspaceSnapshot,
    validations: &HashMap<WorkspaceLink, WorkspaceLinkValidationResult>,
    kind: WorkspaceLinkKind,
) -> Vec<WorkspaceLink> {
    workspace_links(snapshot)
        .into_iter()
        .filter(|link| {
            link.kind == kind
                && matches!(
                    validations.get(link),
                    Some(WorkspaceLinkValidationResult::Valid(_))
                )
        })
        .collect()
}

fn visible_workspace_links(
    snapshot: &WorkspaceSnapshot,
    validations: &HashMap<WorkspaceLink, WorkspaceLinkValidationResult>,
) -> Vec<WorkspaceLink> {
    let mut visible = Vec::new();

    if workspace_is_usable(snapshot)
        && let Some(review) =
            first_validated_workspace_link_by_kind(snapshot, validations, WorkspaceLinkKind::Review)
    {
        visible.push(review);
    }

    for kind in [WorkspaceLinkKind::Issue, WorkspaceLinkKind::Pr] {
        if let Some(link) = first_validated_workspace_link_by_kind(snapshot, validations, kind) {
            visible.push(link);
        } else {
            visible.push(WorkspaceLink {
                kind,
                value: String::new(),
                source: WorkspaceLinkSource::Custom,
            });
        }
    }

    visible
}

fn selectable_workspace_links(
    snapshot: &WorkspaceSnapshot,
    validations: &HashMap<WorkspaceLink, WorkspaceLinkValidationResult>,
    github_statuses: &HashMap<WorkspaceLink, GithubLinkStatusView>,
) -> Vec<WorkspaceLink> {
    visible_workspace_links(snapshot, validations)
        .into_iter()
        .filter(|link| {
            let _ = github_statuses;
            match link.kind {
                WorkspaceLinkKind::Review | WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr => {
                    true
                }
            }
        })
        .collect()
}

fn server_cell_style(snapshot: &WorkspaceSnapshot, archived: bool) -> Style {
    if archived {
        return Style::default();
    }
    match server_cell_label(snapshot) {
        "Idle" => Style::default().fg(IDLE_COLOR),
        "Busy" => Style::default().fg(BUSY_COLOR),
        "Question" => Style::default().fg(WAITING_FOR_INPUT_COLOR),
        _ => Style::default(),
    }
}

fn workspace_row_style(snapshot: &WorkspaceSnapshot) -> Style {
    if snapshot.persistent.archived {
        Style::default().fg(SECONDARY_ROW_COLOR)
    } else {
        Style::default()
    }
}

fn push_hotkey(spans: &mut Vec<Span<'static>>, key: impl Into<String>, rest: impl Into<String>) {
    spans.push(Span::styled(
        key.into(),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(rest.into()));
}

fn next_link_selection_right(current: Option<usize>, link_count: usize) -> Option<usize> {
    if link_count == 0 {
        return None;
    }

    match current {
        None => Some(0),
        Some(index) if index + 1 < link_count => Some(index + 1),
        Some(_) => None,
    }
}

fn content_width(text: &str) -> u16 {
    text.chars().count().min(u16::MAX as usize) as u16
}

fn right_align_cell_text(text: &str, width: u16) -> String {
    let target_width = width as usize;
    let text_width = content_width(text) as usize;
    if text_width >= target_width {
        return text.to_string();
    }
    format!("{text:>target_width$}")
}

fn table_column_widths(
    ordered_keys: &[String],
    snapshots: &HashMap<String, WorkspaceSnapshot>,
    create_row_server: &str,
    create_row_cpu: &str,
    create_row_ram: &str,
) -> (u16, u16, u16, u16, u16, u16, u16, u16, u16, u16) {
    let mut workspace_width = content_width("Workspace").max(content_width(CREATE_ROW_LABEL));
    let mut server_width = content_width("Server").max(content_width(create_row_server));
    let mut cpu_width = content_width("CPU")
        .max(content_width("100%"))
        .max(content_width("2200%"))
        .max(CPU_COLUMN_MIN_WIDTH)
        .max(content_width(create_row_cpu));
    let ram_width = RAM_COLUMN_WIDTH
        .max(content_width("RAM"))
        .max(content_width(create_row_ram));
    let mut cost_width = content_width("Cost");
    let re_width = content_width("RE").max(LINK_COLUMN_WIDTH);
    let is_width = content_width("IS").max(LINK_COLUMN_WIDTH);
    let pr_width = content_width("PR").max(LINK_COLUMN_WIDTH);
    let build_width = content_width("B").max(STATUS_COLUMN_WIDTH);
    let review_width = content_width("R").max(REVIEW_STATUS_COLUMN_WIDTH);
    for key in ordered_keys {
        workspace_width = workspace_width.max(content_width(key));
        if let Some(snapshot) = snapshots.get(key) {
            server_width = server_width.max(content_width(server_cell_label(snapshot)));
            cpu_width = cpu_width.max(content_width(&cpu_cell_label(snapshot)));
            cost_width = cost_width.max(content_width(&cost_cell_label(snapshot)));
        }
    }
    (
        workspace_width,
        server_width,
        cpu_width,
        ram_width,
        cost_width,
        re_width,
        is_width,
        pr_width,
        build_width,
        review_width,
    )
}

fn push_direction_hotkeys(
    spans: &mut Vec<Span<'static>>,
    selected_row: usize,
    workspace_count: usize,
) {
    let can_move_up = selected_row > 0;
    let can_move_down = selected_row < workspace_count;
    match (can_move_up, can_move_down) {
        (true, true) => push_hotkey(spans, "↑/↓", " move  "),
        (true, false) => push_hotkey(spans, "↑", " move  "),
        (false, true) => push_hotkey(spans, "↓", " move  "),
        (false, false) => {}
    }
}

fn help_line(
    mode: UiMode,
    selected_row: usize,
    workspace_count: usize,
    selected_workspace: Option<&WorkspaceSnapshot>,
    selected_workspace_link_count: usize,
    selected_link_index: Option<usize>,
    selected_link_is_custom: bool,
    selected_link_is_placeholder: bool,
    selected_link_kind: Option<WorkspaceLinkKind>,
    selected_workspace_has_refreshable_github_link: bool,
    selected_workspace_can_assign_issue: bool,
    tool_hotkeys: &[(String, String)],
    status: &str,
) -> Line<'static> {
    let mut spans = Vec::new();
    match mode {
        UiMode::Normal => {
            push_direction_hotkeys(&mut spans, selected_row, workspace_count);
            match selected_workspace {
                None => {
                    push_hotkey(&mut spans, "Enter", " create  ");
                }
                Some(snapshot) => {
                    if selected_workspace_link_count > 0 {
                        push_hotkey(&mut spans, "←/→", " select link  ");
                    }
                    if selected_link_index.is_some() {
                        push_hotkey(&mut spans, "↑/↓", " select target  ");
                        if selected_link_is_placeholder {
                            let add_label = match selected_link_kind {
                                Some(WorkspaceLinkKind::Issue) => " add issue link  ",
                                Some(WorkspaceLinkKind::Pr) => " add PR link  ",
                                _ => " add link  ",
                            };
                            push_hotkey(&mut spans, "Enter", add_label);
                        } else {
                            push_hotkey(&mut spans, "Enter", " open link  ");
                            push_hotkey(&mut spans, "a", " add link  ");
                        }
                        if selected_link_is_custom && !selected_link_is_placeholder {
                            push_hotkey(&mut spans, "d", " edit/delete link  ");
                        }
                        push_hotkey(&mut spans, "Esc", " row focus  ");
                    } else {
                        if workspace_is_usable(snapshot)
                            && workspace_state(snapshot) == WorkspaceUiState::Started
                        {
                            push_hotkey(&mut spans, "Enter", " attach  ");
                        } else if workspace_is_usable(snapshot)
                            && workspace_state(snapshot) == WorkspaceUiState::Stopped
                        {
                            push_hotkey(&mut spans, "Enter", " start+attach  ");
                        }
                        if workspace_is_usable(snapshot) {
                            let start_stop_action =
                                if workspace_state(snapshot) == WorkspaceUiState::Stopped {
                                    " start  "
                                } else {
                                    " stop  "
                                };
                            push_hotkey(&mut spans, "s", start_stop_action);
                            if selected_workspace_has_refreshable_github_link {
                                push_hotkey(&mut spans, "r", " recheck GH status  ");
                            }
                        }
                        if selected_workspace_can_assign_issue {
                            push_hotkey(&mut spans, "i", " issue  ");
                        }
                        push_hotkey(&mut spans, "d", " edit description  ");
                        push_hotkey(&mut spans, "x", " delete  ");
                        let archive_action = if snapshot.persistent.archived {
                            " unarchive  "
                        } else {
                            " archive  "
                        };
                        push_hotkey(&mut spans, "a", archive_action);
                        for (tool_key, tool_name) in tool_hotkeys {
                            push_hotkey(&mut spans, tool_key.clone(), format!(" {}  ", tool_name));
                        }
                    }
                }
            }
            push_hotkey(&mut spans, "q", " quit");
        }
        UiMode::CreateModal => {
            spans.push(Span::raw("Create workspace: type key and repository, "));
            push_hotkey(&mut spans, "Tab", " next field, ");
            push_hotkey(&mut spans, "Enter", " confirm, ");
            push_hotkey(&mut spans, "Esc", " cancel");
        }
        UiMode::EditDescription => {
            spans.push(Span::raw("Edit description: type, "));
            push_hotkey(&mut spans, "Enter", " save, ");
            push_hotkey(&mut spans, "Esc", " cancel");
        }
        UiMode::EditIssue => {
            spans.push(Span::raw("Assign issue: type number or GitHub issue URL, "));
            push_hotkey(&mut spans, "Enter", " save, ");
            push_hotkey(&mut spans, "Esc", " cancel");
        }
        UiMode::EditCustomLink => {
            spans.push(Span::raw("Edit link: type, "));
            push_hotkey(&mut spans, "Enter", " save, ");
            push_hotkey(&mut spans, "Del", " delete, ");
            push_hotkey(&mut spans, "Esc", " cancel");
        }
        UiMode::ConfirmDelete => {
            spans.push(Span::raw("Delete workspace: "));
            push_hotkey(&mut spans, "Enter", " confirm, ");
            push_hotkey(&mut spans, "Esc", " cancel");
        }
        UiMode::StartingModal => {
            spans.push(Span::raw(
                "Starting workspace and waiting for server readiness...",
            ));
        }
        UiMode::ToolProgressModal => {
            spans.push(Span::raw(
                "Operation is running in the selected workspace... ",
            ));
            push_hotkey(&mut spans, "Esc", " cancel");
        }
    }
    if !status.is_empty() {
        spans.push(Span::raw(" | "));
        spans.push(Span::raw(status.to_string()));
    }
    Line::from(spans)
}

fn tool_key_char(tool: &ToolConfig) -> Option<char> {
    let mut chars = tool.key.chars();
    let key_char = chars.next()?;
    (chars.next().is_none()).then_some(key_char)
}

fn find_tool_for_key(tools: &[ToolConfig], key: char) -> Option<ToolConfig> {
    tools
        .iter()
        .find(|tool| tool_key_char(tool) == Some(key))
        .cloned()
}

fn tool_is_usable(tool: &ToolConfig, snapshot: &WorkspaceSnapshot) -> bool {
    if !workspace_is_usable(snapshot) {
        return false;
    }

    match tool.type_ {
        ToolType::Exec => true,
        ToolType::Prompt => workspace_state(snapshot) == WorkspaceUiState::Started,
    }
}

fn contextual_tool_hotkeys(
    tools: &[ToolConfig],
    selected_workspace: Option<&WorkspaceSnapshot>,
) -> Vec<(String, String)> {
    let Some(snapshot) = selected_workspace else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    tools
        .iter()
        .filter(|tool| tool_is_usable(tool, snapshot))
        .filter_map(|tool| {
            let key_char = tool_key_char(tool)?;
            if !seen.insert(key_char) {
                return None;
            }
            Some((key_char.to_string(), tool.name.clone()))
        })
        .collect()
}
#[derive(Debug, Clone, Parser)]
#[command(name = "multicode-tui")]
struct CliArgs {
    config_path: PathBuf,
    #[arg(long = "relay-socket")]
    relay_socket: Option<PathBuf>,
    #[arg(long = "github-token-env", hide = true)]
    github_token_env: Option<String>,
    #[arg(long = "remote-sanity-check", hide = true)]
    remote_sanity_check: bool,
    #[arg(long = "recency-scan-path", hide = true)]
    recency_scan_path: Option<PathBuf>,
    #[arg(long = "recency-scan-is-dir", hide = true)]
    recency_scan_is_dir: bool,
    #[arg(long = "recency-scan-exclude", hide = true)]
    recency_scan_exclude: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = CliArgs::parse();
    if cli.remote_sanity_check {
        return run_remote_sanity_check(cli).await;
    }
    if cli.recency_scan_path.is_some() {
        return run_recency_scan(cli).await;
    }
    logging::init_stdout_logging();
    let config_path = cli.config_path.clone();
    logging::log_startup(&config_path);
    let mut terminal = setup_terminal()?;
    let stdout_logging_guard = logging::suppress_stdout_logging();
    let run_result = run_app(&mut terminal, cli).await;
    let restore_result = restore_terminal(&mut terminal);
    drop(stdout_logging_guard);

    if let Err(err) = run_result {
        restore_result?;
        return Err(err);
    }

    restore_result?;
    Ok(())
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cli: CliArgs,
) -> Result<(), Box<dyn Error>> {
    let mut app = TuiState::new(cli.config_path, cli.relay_socket, cli.github_token_env).await?;
    let mut needs_redraw = true;

    while !app.should_quit {
        if needs_redraw {
            terminal.draw(|frame| draw_ui(frame, &mut app))?;
            needs_redraw = false;
        }

        if event::poll(UI_IDLE_POLL_INTERVAL)? {
            let event = event::read()?;
            if let Event::Key(key) = event {
                app.handle_key(terminal, key).await;
                needs_redraw = true;
            }
        } else {
            app.sync_from_manager();
            app.refresh_machine_usage_if_due(Instant::now()).await;
            app.poll_running_prompt_tool();
            app.poll_workspace_link_validations();
            app.handle_auto_attach_when_ready(terminal).await;
            needs_redraw = true;
        }
    }

    Ok(())
}

async fn run_remote_sanity_check(cli: CliArgs) -> Result<(), Box<dyn Error>> {
    std::fs::write("relay-sanity-enter.txt", "enter")?;
    let relay_socket = cli.relay_socket.clone();
    std::fs::write("relay-sanity-after-new.txt", "skipped-new")?;
    std::fs::write("relay-sanity-before.txt", "before")?;
    if let Some(socket_path) = relay_socket.as_deref() {
        use std::os::unix::fs::FileTypeExt;
        let metadata = std::fs::metadata(socket_path)?;
        let file_type = metadata.file_type();
        std::fs::write(
            "relay-sanity-metadata.txt",
            format!(
                "path={} is_socket={} is_file={} is_dir={}\n",
                socket_path.display(),
                file_type.is_socket(),
                file_type.is_file(),
                file_type.is_dir()
            ),
        )?;
    } else {
        std::fs::write("relay-sanity-metadata.txt", "path=<none>\n")?;
    }
    let argument = "https://relay.example/test".to_string();
    crate::app::dispatch_handler_action(
        relay_socket.as_deref(),
        "/bin/true",
        &Vec::<String>::new(),
        &WorkspaceLink {
            kind: WorkspaceLinkKind::Issue,
            value: argument.clone(),
            source: WorkspaceLinkSource::Custom,
        },
        &argument,
    )
    .await?;
    std::fs::write("relay-sanity-after.txt", "after")?;
    Ok(())
}

async fn run_recency_scan(cli: CliArgs) -> Result<(), Box<dyn Error>> {
    let path = cli
        .recency_scan_path
        .as_deref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing recency scan path"))?;
    let latest =
        tree_scan::latest_modified_time(path, cli.recency_scan_is_dir, &cli.recency_scan_exclude)?;
    match latest {
        Some(latest) => println!(
            "{}",
            latest
                .duration_since(UNIX_EPOCH)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?
                .as_secs()
        ),
        None => println!("none"),
    }
    Ok(())
}
