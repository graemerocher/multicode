use crate::ops::*;
use crate::system::*;
use crate::*;
use multicode_lib::services::{
    GithubTokenConfig,
    codex_app_server::{CodexAppServerClient, CodexThreadStatus},
};
use std::os::unix::fs::FileTypeExt;

const NERD_FONT_GITHUB_GLYPH: &str = "\u{f408}";
const CODEX_AUTO_RESUME_PROMPT: &str = "Continue autonomously from where you left off. Do not wait for approval for repository commands, builds, Gradle tasks, or focused tests. Only stop to ask before committing, pushing, commenting on GitHub, or opening or updating a pull request.";
const CODEX_CREATE_PR_APPROVAL_PROMPT: &str = "The local changes for this task are approved for publishing. Create or update the pull request now from this task checkout. Push the branch if needed, use the correct upstream base branch, include an appropriate type label such as `type: docs` for documentation-only changes, `type: bug` for bug fixes, `type: improvement` for minor improvements, or `type: enhancement` for broader enhancements, assign the pull request to yourself, assign it automatically to the next Micronaut project release at the organization level under https://github.com/orgs/micronaut-projects/projects, prefer the next semantically versioned release project that is typically suffixed with a milestone such as `5.0.0-M2` and otherwise suffixed with `Release` such as `5.0.0 Release`, request Copilot review, emit the <multicode:pr> link, and stop once the PR is ready for human review. If a PR already exists, update it instead of creating a duplicate. Do not merge the PR.";
const CODEX_FIX_CI_INSTRUCTIONS: &str = "Fix any failing CI checks for this task's existing pull request, including Sonar failures. Existing tests must never be changed just to satisfy failing checks or to mask regressions; preserve the intended existing behavior. Push fixes as needed, monitor CI after each push, and continue until the pull request is green. Do not create a new pull request. Do not merge the pull request. Stop only when all CI is passing or you need human input.";

pub(crate) fn build_codex_fix_ci_prompt(
    assigned_repository: &str,
    issue_url: &str,
    backing_pr_url: Option<&str>,
    cwd: &std::path::Path,
    task_state_path: &std::path::Path,
    task_session_id: Option<&str>,
) -> String {
    let pr_instruction = backing_pr_url.map_or_else(
        || "If a pull request already exists for this issue, use that existing pull request instead of creating a duplicate.".to_string(),
        |backing_pr_url| format!("Use the existing pull request {backing_pr_url}."),
    );
    let session_instruction = task_session_id.map_or_else(
        String::new,
        |task_session_id| {
            format!(
                "For this task session/thread, write autonomous state updates in the format `<state>:{task_session_id}` so multicode can attribute the state to this specific session.\n\\\n"
            )
        },
    );
    format!(
        "You are operating in an autonomous multicode workspace for repository {assigned_repository}.\n\
Continue autonomously from where you left off.\n\
Start from the existing checkout for GitHub issue {issue_url}.\n\
Primary checkout for this task: {cwd}\n\
Before you proceed, load and follow these workspace skills as appropriate: `independent-fix`, `machine-readable-clone`, `machine-readable-issue`, `machine-readable-pr`, `git-commit-coauthorship`, `micronaut-projects-guide`, and `autonomous-state`.\n\
For this task, write autonomous state updates to `{task_state_path}`. Do not write task state to any shared workspace file.\n\
{session_instruction}\
{pr_instruction}\n\
Your job is to:\n\
1. Inspect the current failing CI status for this task and understand every failing check.\n\
2. Reproduce and fix the underlying problems in the existing checkout.\n\
3. Run focused verification locally.\n\
4. Commit and push branch updates as needed.\n\
5. Monitor CI after each push and keep addressing failures until it is green.\n\
6. Emit the machine-readable repository / issue / PR tags while you work.\n\
7. Run repository commands, builds, Gradle tasks, focused tests, git commits, branch pushes, and pull request updates as needed without asking for permission.\n\
{instructions}\n\
\n\
Keep going until CI is green or you need human feedback.",
        cwd = cwd.display(),
        task_state_path = task_state_path.display(),
        session_instruction = session_instruction,
        instructions = CODEX_FIX_CI_INSTRUCTIONS
    )
}

pub(crate) fn repository_diff_shell_command() -> &'static str {
    r#"tmp="$(mktemp -t multicode-diff.XXXXXX)" || exit 1
{
  echo "Git status"
  echo "=========="
  git status --short
  echo
  echo "Git diff"
  echo "========"
  git --no-pager diff --color=always --stat --patch HEAD --
} >"$tmp"
if [ ! -s "$tmp" ]; then
  printf 'No local changes\n' >"$tmp"
fi
if command -v less >/dev/null 2>&1; then
  less -R -X "$tmp"
else
  cat "$tmp"
  printf '\nPress Enter to return...'
  read -r _
fi
rm -f "$tmp""#
}

pub(crate) fn shell_command_in_repo(repo_dir: &str, shell_command: &str) -> String {
    format!("cd -- {} && {}", shell_escape_arg(repo_dir), shell_command)
}

pub(crate) fn count_codex_session_turn_metrics(contents: &str) -> CodexSessionTurnMetrics {
    CodexSessionTurnMetrics {
        started: contents.matches("\"type\":\"task_started\"").count(),
        completed: contents.matches("\"type\":\"task_complete\"").count(),
        aborted: contents.matches("\"type\":\"turn_aborted\"").count(),
    }
}

fn codex_session_log_root(
    workspace_directory_path: &std::path::Path,
    workspace_key: &str,
) -> PathBuf {
    workspace_directory_path
        .join(".multicode")
        .join("codex")
        .join(workspace_key)
        .join("home")
        .join("sessions")
}

fn find_codex_session_log_path(
    workspace_directory_path: &std::path::Path,
    workspace_key: &str,
    session_id: &str,
) -> Option<PathBuf> {
    let root = codex_session_log_root(workspace_directory_path, workspace_key);
    let mut stack = vec![root];
    let suffix = format!("{session_id}.jsonl");
    while let Some(path) = stack.pop() {
        let entries = std::fs::read_dir(&path).ok()?;
        for entry in entries.flatten() {
            let entry_path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry_path);
                continue;
            }
            if file_type.is_file()
                && entry_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(&suffix))
            {
                return Some(entry_path);
            }
        }
    }
    None
}

fn read_codex_session_turn_metrics(
    workspace_directory_path: &std::path::Path,
    workspace_key: &str,
    session_id: &str,
) -> Option<CodexSessionTurnMetrics> {
    let path = find_codex_session_log_path(workspace_directory_path, workspace_key, session_id)?;
    let contents = std::fs::read_to_string(path).ok()?;
    Some(count_codex_session_turn_metrics(&contents))
}

pub(crate) fn last_user_message_from_codex_session_log_contents(contents: &str) -> Option<String> {
    contents
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            let payload = value.get("payload")?;
            if payload.get("type").and_then(serde_json::Value::as_str) != Some("user_message") {
                return None;
            }
            payload
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .map(ToOwned::to_owned)
        })
        .last()
}

fn first_user_message_from_codex_session_log_contents(contents: &str) -> Option<String> {
    contents.lines().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let payload = value.get("payload")?;
        if payload.get("type").and_then(serde_json::Value::as_str) != Some("user_message") {
            return None;
        }
        payload
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn interrupted_codex_resume_prompt_from_session_log_contents(contents: &str) -> Option<String> {
    let first = first_user_message_from_codex_session_log_contents(contents);
    let last = last_user_message_from_codex_session_log_contents(contents);
    match (first, last) {
        (Some(first), Some(last)) if first != last => Some(format!(
            "{first}\n\nAdditional user instruction from the interrupted interactive attach:\n{last}"
        )),
        (_, Some(last)) => Some(last),
        (Some(first), None) => Some(first),
        (None, None) => None,
    }
}

async fn read_last_codex_session_user_message(
    workspace_directory_path: std::path::PathBuf,
    workspace_key: String,
    session_id: String,
) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let path =
            find_codex_session_log_path(&workspace_directory_path, &workspace_key, &session_id)?;
        let contents = std::fs::read_to_string(path).ok()?;
        last_user_message_from_codex_session_log_contents(&contents)
    })
    .await
    .ok()
    .flatten()
}

async fn read_interrupted_codex_resume_prompt(
    workspace_directory_path: std::path::PathBuf,
    workspace_key: String,
    session_id: String,
) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let path =
            find_codex_session_log_path(&workspace_directory_path, &workspace_key, &session_id)?;
        let contents = std::fs::read_to_string(path).ok()?;
        interrupted_codex_resume_prompt_from_session_log_contents(&contents)
    })
    .await
    .ok()
    .flatten()
}

pub(crate) fn should_resume_codex_task_after_incomplete_attached_turn(
    initial_metrics: Option<CodexSessionTurnMetrics>,
    current_metrics: Option<CodexSessionTurnMetrics>,
    thread_status: Option<&CodexThreadStatus>,
) -> bool {
    let Some(initial_metrics) = initial_metrics else {
        return false;
    };
    let Some(current_metrics) = current_metrics else {
        return false;
    };
    let started_new_turn = current_metrics.started > initial_metrics.started;
    let aborted_new_turn = current_metrics.aborted > initial_metrics.aborted;
    if !started_new_turn && !aborted_new_turn {
        return false;
    }
    if current_metrics.completed > initial_metrics.completed && !aborted_new_turn {
        return false;
    }
    match thread_status {
        Some(CodexThreadStatus::Active { .. }) => false,
        Some(CodexThreadStatus::SystemError) => false,
        Some(CodexThreadStatus::Idle | CodexThreadStatus::NotLoaded) | None => true,
    }
}

pub(crate) fn should_restart_codex_task_for_pr_request(
    task_state: Option<&WorkspaceTaskRuntimeSnapshot>,
) -> bool {
    let Some(task_state) = task_state else {
        return false;
    };

    if task_state.agent_state == Some(AutomationAgentState::Stale) {
        return true;
    }

    matches!(task_state.status.as_deref(), Some("NotLoaded"))
}

pub(crate) fn should_restart_codex_task_for_ci_fix(
    task_state: Option<&WorkspaceTaskRuntimeSnapshot>,
) -> bool {
    let Some(task_state) = task_state else {
        return false;
    };

    if should_restart_codex_task_for_pr_request(Some(task_state)) {
        return true;
    }

    if task_state.session_id.is_none() {
        return false;
    }

    match task_state.session_status {
        Some(RootSessionStatus::Busy) => false,
        Some(RootSessionStatus::Idle) | Some(RootSessionStatus::Question) | None => {
            task_state.agent_state != Some(AutomationAgentState::Working)
        }
    }
}

pub(crate) fn should_offer_codex_ci_fix(
    has_pr_link: bool,
    pr_status: Option<GithubPrStatus>,
) -> bool {
    has_pr_link
        && match pr_status {
            Some(GithubPrStatus {
                state: GithubPrState::Open,
                build,
                ..
            }) => build != GithubPrBuildState::Succeeded,
            Some(_) => false,
            None => true,
        }
}

pub(crate) fn compact_github_tooltip_target(target: &str) -> Option<String> {
    let url = Url::parse(target).ok()?;
    if url.host_str()? != "github.com" {
        return None;
    }

    let segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let [owner, repo, kind, number] = segments.as_slice() else {
        return None;
    };
    if !matches!(*kind, "issues" | "pull") || number.parse::<u64>().is_err() {
        return None;
    }

    Some(format!("{NERD_FONT_GITHUB_GLYPH} {owner}/{repo}#{number}"))
}

pub(crate) fn github_repository_url(repository: &str) -> Option<String> {
    let repository = github_repository_spec(repository)?;
    Some(format!("https://github.com/{repository}"))
}

pub(crate) fn github_repository_spec(repository: &str) -> Option<String> {
    let trimmed = repository.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(url) = Url::parse(trimmed) {
        if url.host_str()? != "github.com" {
            return None;
        }
        let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
        let owner = segments.next()?;
        let repo = segments.next()?;
        return Some(format!("{owner}/{repo}"));
    }

    let mut segments = trimmed.split('/').filter(|segment| !segment.is_empty());
    let owner = segments.next()?;
    let repo = segments.next()?;
    if segments.next().is_some() {
        return None;
    }

    Some(format!("{owner}/{repo}"))
}

pub(crate) fn task_repository_spec(snapshot: &WorkspaceSnapshot, task_id: &str) -> Option<String> {
    if let Some(repository) = snapshot.persistent.assigned_repository.as_deref() {
        return github_repository_spec(repository);
    }

    let task = task_persistent_snapshot(snapshot, task_id)?;
    github_repository_spec(&task.issue_url).or_else(|| {
        task_pr_link(task, task_runtime_snapshot(snapshot, task_id))
            .and_then(github_repository_spec)
    })
}

pub(crate) fn has_available_task_slot(
    snapshot: &WorkspaceSnapshot,
    max_parallel_issues: usize,
) -> bool {
    snapshot.persistent.tasks.len() < max_parallel_issues
}

pub(crate) fn should_request_autonomous_issue_scan(
    snapshot: &WorkspaceSnapshot,
    max_parallel_issues: usize,
) -> bool {
    snapshot.persistent.assigned_repository.is_some()
        && !snapshot.persistent.archived
        && has_available_task_slot(snapshot, max_parallel_issues)
}

pub(crate) fn workspace_can_queue_next_issue(snapshot: &WorkspaceSnapshot) -> bool {
    workspace_is_usable(snapshot) && snapshot.persistent.assigned_repository.is_some()
}

pub(crate) fn should_auto_resume_autonomous_codex_after_attach(
    snapshot: &WorkspaceSnapshot,
) -> bool {
    if snapshot.persistent.assigned_repository.is_none()
        || snapshot.resolved_active_task_id().is_none()
    {
        return false;
    }

    match snapshot.automation_agent_state {
        Some(AutomationAgentState::Working) => true,
        Some(
            AutomationAgentState::WaitingOnVm
            | AutomationAgentState::Question
            | AutomationAgentState::Review
            | AutomationAgentState::Idle
            | AutomationAgentState::Stale,
        ) => false,
        None => matches!(
            snapshot
                .automation_session_status
                .or(snapshot.root_session_status),
            Some(RootSessionStatus::Busy)
        ),
    }
}

pub(crate) fn should_auto_resume_task_codex_after_attach(
    task_state: Option<&WorkspaceTaskRuntimeSnapshot>,
    attached_session_id: Option<&str>,
    initial_agent_state: Option<AutomationAgentState>,
) -> bool {
    let Some(task_state) = task_state else {
        return matches!(initial_agent_state, Some(AutomationAgentState::Working));
    };

    if let Some(attached_session_id) = attached_session_id
        && let Some(current_session_id) = task_state.session_id.as_deref()
        && current_session_id != attached_session_id
    {
        return false;
    }

    if task_state.agent_state == Some(AutomationAgentState::Stale) {
        return matches!(initial_agent_state, Some(AutomationAgentState::Working));
    }

    let effective_agent_state = task_effective_agent_state(Some(task_state));
    if attached_session_id.is_some()
        && task_state.session_id.as_deref() == attached_session_id
        && effective_agent_state == Some(AutomationAgentState::Working)
    {
        return false;
    }

    match effective_agent_state {
        Some(AutomationAgentState::Working) => true,
        Some(
            AutomationAgentState::WaitingOnVm
            | AutomationAgentState::Question
            | AutomationAgentState::Review
            | AutomationAgentState::Idle,
        ) => false,
        Some(AutomationAgentState::Stale) => false,
        None if attached_session_id.is_some() && task_state.session_id.is_none() => {
            matches!(initial_agent_state, Some(AutomationAgentState::Working))
        }
        None => matches!(initial_agent_state, Some(AutomationAgentState::Working)),
    }
}

pub(crate) fn should_restart_task_codex_after_attach(
    attached_session_id: Option<&str>,
    fresh_codex_session: bool,
) -> bool {
    attached_session_id.is_some() && !fresh_codex_session
}

pub(crate) fn working_codex_task_attach_target(
    snapshot: &WorkspaceSnapshot,
    task_id: Option<&str>,
    _cwd: Option<String>,
) -> io::Result<Option<AttachTarget>> {
    let Some(task_id) = task_id else {
        return Ok(None);
    };
    let Some(task_state) = task_runtime_snapshot(snapshot, task_id) else {
        return Ok(None);
    };
    if task_effective_agent_state(Some(task_state)) != Some(AutomationAgentState::Working) {
        return Ok(None);
    }

    let Some(uri) = codex_attach_uri(snapshot)? else {
        return Ok(None);
    };
    Ok(Some(AttachTarget::Codex {
        uri,
        thread_id: task_state.session_id.clone(),
    }))
}

pub(crate) fn should_retry_codex_task_attach_with_last_thread(
    provider: multicode_lib::services::AgentProvider,
    snapshot: Option<&WorkspaceSnapshot>,
    workspace_key: &str,
    attached_workspace_key: Option<&str>,
    task_id: Option<&str>,
    session_id: Option<&str>,
) -> Option<AttachTarget> {
    if provider != multicode_lib::services::AgentProvider::Codex
        || attached_workspace_key != Some(workspace_key)
        || session_id.is_none()
    {
        return None;
    }

    let snapshot = snapshot?;
    let task_state = task_runtime_snapshot(snapshot, task_id?)?;
    if !matches!(task_state.status.as_deref(), Some("NotLoaded")) {
        return None;
    }

    let Ok(Some(uri)) = codex_attach_uri(snapshot) else {
        return None;
    };
    Some(AttachTarget::Codex {
        uri,
        thread_id: None,
    })
}

pub(crate) fn should_start_fresh_codex_task_session_after_failed_attach(
    provider: multicode_lib::services::AgentProvider,
    attached_workspace_key: Option<&str>,
    workspace_key: &str,
    task_id: Option<&str>,
    session_id: Option<&str>,
    initial_turn_metrics: Option<CodexSessionTurnMetrics>,
    current_turn_metrics: Option<CodexSessionTurnMetrics>,
    current_thread_status: Option<&CodexThreadStatus>,
) -> bool {
    let Some(_task_id) = task_id else {
        return false;
    };
    let Some(_session_id) = session_id else {
        return false;
    };
    let Some(initial_turn_metrics) = initial_turn_metrics else {
        return false;
    };
    let Some(current_turn_metrics) = current_turn_metrics else {
        return false;
    };

    provider == multicode_lib::services::AgentProvider::Codex
        && attached_workspace_key == Some(workspace_key)
        && initial_turn_metrics.completed == 0
        && initial_turn_metrics.aborted > 0
        && current_turn_metrics == initial_turn_metrics
        && matches!(
            current_thread_status,
            Some(CodexThreadStatus::NotLoaded | CodexThreadStatus::SystemError)
        )
}

fn task_can_yield_vm_for_attach(task_state: &WorkspaceTaskRuntimeSnapshot) -> bool {
    matches!(
        task_effective_agent_state(Some(task_state)),
        Some(
            AutomationAgentState::Question
                | AutomationAgentState::Review
                | AutomationAgentState::Idle
                | AutomationAgentState::Stale
        )
    )
}

pub(crate) fn should_queue_task_codex_resume_until_vm_available(
    snapshot: &WorkspaceSnapshot,
    task_id: &str,
) -> bool {
    let Some(active_task_id) = snapshot
        .active_task_id
        .clone()
        .or_else(|| snapshot.resolved_active_task_id())
    else {
        return matches!(
            snapshot.root_session_status,
            Some(RootSessionStatus::Busy | RootSessionStatus::Question)
        );
    };
    if active_task_id == task_id {
        return false;
    }
    let Some(active_task_state) = snapshot.task_states.get(&active_task_id) else {
        return matches!(
            snapshot.root_session_status,
            Some(RootSessionStatus::Busy | RootSessionStatus::Question)
        );
    };
    if active_task_state.waiting_on_vm {
        return false;
    }
    !task_can_yield_vm_for_attach(active_task_state)
}

pub(crate) fn restored_selected_row(
    entries: &[TableEntry],
    previous_selected_entry: Option<&TableEntry>,
    current_selected_row: usize,
) -> usize {
    let max_row = entries.len().saturating_sub(1);
    let Some(previous_selected_entry) = previous_selected_entry else {
        return current_selected_row.min(max_row);
    };

    if matches!(previous_selected_entry, TableEntry::Create) {
        return 0;
    }

    if let Some(position) = entries
        .iter()
        .position(|entry| entry == previous_selected_entry)
    {
        return position;
    }

    if let TableEntry::Task { workspace_key, .. } = previous_selected_entry
        && let Some(position) = entries.iter().position(|entry| {
            matches!(
                entry,
                TableEntry::Workspace {
                    workspace_key: candidate
                } if candidate == workspace_key
            )
        })
    {
        return position;
    }

    current_selected_row.min(max_row)
}

impl TuiState {
    pub(crate) fn table_entries(&self) -> Vec<TableEntry> {
        let mut entries = Vec::with_capacity(self.ordered_keys.len() + 1);
        entries.push(TableEntry::Create);
        for key in &self.ordered_keys {
            entries.push(TableEntry::Workspace {
                workspace_key: key.clone(),
            });
            if let Some(snapshot) = self.snapshots.get(key) {
                for task in &snapshot.persistent.tasks {
                    entries.push(TableEntry::Task {
                        workspace_key: key.clone(),
                        task_id: task.id.clone(),
                    });
                }
            }
        }
        entries
    }

    fn selected_entry(&self) -> Option<TableEntry> {
        self.table_entries().get(self.selected_row).cloned()
    }

    pub(crate) fn selected_task_id(&self) -> Option<&str> {
        if self.selected_row == 0 {
            return None;
        }
        let mut row = 1usize;
        for key in &self.ordered_keys {
            if row == self.selected_row {
                return None;
            }
            row += 1;
            if let Some(snapshot) = self.snapshots.get(key) {
                for task in &snapshot.persistent.tasks {
                    if row == self.selected_row {
                        return Some(task.id.as_str());
                    }
                    row += 1;
                }
            }
        }
        None
    }

    pub(crate) async fn new(
        config_path: PathBuf,
        relay_socket: Option<PathBuf>,
        github_token_env: Option<String>,
    ) -> Result<Self, Box<dyn Error>> {
        let mut config = multicode_lib::services::config::read_config(&config_path)
            .await
            .map_err(|err| io::Error::other(format!("failed to read config: {err:?}")))?;
        if let Some(github_token_env) = github_token_env {
            config.github.token = Some(GithubTokenConfig {
                env: Some(github_token_env),
                command: None,
                keychain_service: None,
                keychain_account: None,
            });
        }
        let service = CombinedService::from_config_path(&config_path)
            .await
            .map_err(|err| io::Error::other(format!("failed to start services: {err:?}")))?;
        let workspace_keys_rx = service.manager.subscribe();

        let mut state = Self {
            service,
            relay_socket,
            workspace_keys_rx,
            workspace_rxs: HashMap::new(),
            snapshots: HashMap::new(),
            workspace_link_validation_results: HashMap::new(),
            pending_workspace_link_validations: HashMap::new(),
            github_link_status_rxs: HashMap::new(),
            github_link_statuses: HashMap::new(),
            ordered_keys: Vec::new(),
            selected_row: 0,
            selected_link_index: None,
            selected_link_target_index: 0,
            mode: UiMode::Normal,
            create_input: String::new(),
            repository_input: String::new(),
            create_field: CreateModalField::Key,
            edit_input: String::new(),
            issue_input: String::new(),
            custom_link_input: String::new(),
            custom_link_kind: None,
            custom_link_action: None,
            custom_link_original_value: None,
            pending_delete_target: None,
            pending_task_removal_action: TaskRemovalAction::default(),
            attached_session: None,
            starting_workspace_key: None,
            starting_attach_when_ready: false,
            started_wait_since: None,
            previous_machine_cpu_totals: None,
            machine_cpu_count: 1,
            machine_cpu_percent: None,
            machine_used_ram_bytes: None,
            machine_total_ram_bytes: None,
            machine_agent_directory_disk_usage: None,
            next_machine_sample_at: None,
            running_operation: None,
            status: String::new(),
            should_quit: false,
        };
        state.sync_from_manager();
        Ok(state)
    }

    pub(crate) async fn refresh_machine_usage_if_due(&mut self, now: Instant) {
        if let Some(next_sample_at) = self.next_machine_sample_at
            && now < next_sample_at
        {
            return;
        }

        let logical_cpu_count = std::thread::available_parallelism()
            .map(|parallelism| parallelism.get())
            .unwrap_or(1);
        self.machine_cpu_count = logical_cpu_count;

        match read_machine_cpu_totals().await {
            Ok(current_totals) => {
                self.machine_cpu_percent = self.previous_machine_cpu_totals.and_then(|previous| {
                    machine_cpu_percent(previous, current_totals, logical_cpu_count)
                });
                self.previous_machine_cpu_totals = Some(current_totals);
            }
            Err(_) => {
                self.machine_cpu_percent = None;
                self.previous_machine_cpu_totals = None;
            }
        }

        self.machine_used_ram_bytes = read_machine_used_ram_bytes().await.ok().flatten();
        self.machine_total_ram_bytes = read_machine_total_ram_bytes().await.ok().flatten();
        self.machine_agent_directory_disk_usage =
            read_disk_usage(self.service.workspace_directory_path())
                .await
                .ok()
                .flatten();
        self.next_machine_sample_at = Some(now + MACHINE_USAGE_SAMPLE_INTERVAL);
    }

    pub(crate) fn sync_from_manager(&mut self) {
        let previous_selected_entry = self.selected_entry();
        let previous_selected_workspace_key = self.selected_workspace_key().map(str::to_string);
        let previous_selected_automation_status = previous_selected_workspace_key
            .as_ref()
            .and_then(|key| self.snapshots.get(key))
            .and_then(|snapshot| snapshot.automation_status.clone());

        let workspace_keys = self.workspace_keys_rx.borrow().clone();

        self.workspace_rxs
            .retain(|key, _| workspace_keys.contains(key));
        self.snapshots.retain(|key, _| workspace_keys.contains(key));

        for key in &workspace_keys {
            if !self.workspace_rxs.contains_key(key)
                && let Ok(workspace) = self.service.manager.get_workspace(key)
            {
                self.workspace_rxs
                    .insert(key.clone(), workspace.subscribe());
            }

            if let Some(rx) = self.workspace_rxs.get(key) {
                self.snapshots.insert(key.clone(), rx.borrow().clone());
            }
        }

        self.ordered_keys = workspace_keys.into_iter().collect();
        self.ordered_keys.sort_by(|left_key, right_key| {
            workspace_ordering(left_key, right_key, &self.snapshots)
        });

        self.refresh_workspace_link_validations();
        self.refresh_github_link_statuses();

        let table_entries = self.table_entries();
        self.selected_row = restored_selected_row(
            &table_entries,
            previous_selected_entry.as_ref(),
            self.selected_row,
        );

        if let Some(key) = self.selected_workspace_key()
            && previous_selected_workspace_key.as_deref() == Some(key)
            && let Some(current_status) = self
                .snapshots
                .get(key)
                .and_then(|snapshot| snapshot.automation_status.as_deref())
            && current_status.starts_with("Scan failed")
            && previous_selected_automation_status.as_deref() != Some(current_status)
        {
            self.status = current_status.to_string();
        }

        self.normalize_selected_link_index();

        if self.selected_workspace_key().is_none() {
            match self.mode {
                UiMode::EditDescription => {
                    self.mode = UiMode::Normal;
                    self.edit_input.clear();
                }
                UiMode::EditIssue => {
                    self.mode = UiMode::Normal;
                    self.issue_input.clear();
                }
                UiMode::EditCustomLink => {
                    self.mode = UiMode::Normal;
                    self.custom_link_input.clear();
                    self.custom_link_kind = None;
                    self.custom_link_action = None;
                    self.custom_link_original_value = None;
                }
                UiMode::ConfirmDelete => {
                    self.mode = UiMode::Normal;
                    self.pending_delete_target = None;
                    self.pending_task_removal_action = TaskRemovalAction::default();
                }
                UiMode::ConfirmTaskRemoval => {
                    self.mode = UiMode::Normal;
                    self.pending_delete_target = None;
                    self.pending_task_removal_action = TaskRemovalAction::default();
                }
                _ => {}
            }
        }

        if self.mode == UiMode::StartingModal {
            let starting_state = self
                .starting_workspace_key
                .as_deref()
                .and_then(|key| self.snapshots.get(key).map(workspace_state));
            match starting_state {
                Some(WorkspaceUiState::Starting) => {}
                Some(WorkspaceUiState::Started) => {
                    if !self.starting_attach_when_ready {
                        if let Some(key) = self.starting_workspace_key.as_deref() {
                            self.status = format!("Started workspace '{key}'");
                        }
                        self.mode = UiMode::Normal;
                        self.starting_workspace_key = None;
                        self.starting_attach_when_ready = false;
                        self.started_wait_since = None;
                    }
                }
                Some(WorkspaceUiState::Stopped) => {
                    if let Some(key) = self.starting_workspace_key.as_deref() {
                        self.status = starting_modal_failure_status(key, self.snapshots.get(key));
                    }
                    self.mode = UiMode::Normal;
                    self.starting_workspace_key = None;
                    self.starting_attach_when_ready = false;
                    self.started_wait_since = None;
                }
                None => {
                    self.mode = UiMode::Normal;
                    self.starting_workspace_key = None;
                    self.starting_attach_when_ready = false;
                    self.started_wait_since = None;
                }
            }
        }

        if self.mode == UiMode::ToolProgressModal {
            let workspace_missing = self
                .running_operation
                .as_ref()
                .map(|operation| !self.snapshots.contains_key(&operation.workspace_key))
                .unwrap_or(true);
            if workspace_missing {
                self.mode = UiMode::Normal;
                self.running_operation = None;
            }
        }

        if matches!(
            self.mode,
            UiMode::ConfirmDelete | UiMode::ConfirmTaskRemoval
        ) && self
            .pending_delete_target
            .as_ref()
            .is_some_and(|target| match target {
                PendingDeleteTarget::Workspace { workspace_key }
                | PendingDeleteTarget::Task { workspace_key, .. } => {
                    !self.snapshots.contains_key(workspace_key)
                }
            })
        {
            self.mode = UiMode::Normal;
            self.pending_delete_target = None;
            self.pending_task_removal_action = TaskRemovalAction::default();
        }
    }

    pub(crate) fn selected_workspace_key(&self) -> Option<&str> {
        if self.selected_row == 0 {
            return None;
        }
        let mut row = 1usize;
        for key in &self.ordered_keys {
            if row == self.selected_row {
                return Some(key.as_str());
            }
            row += 1;
            if let Some(snapshot) = self.snapshots.get(key) {
                for _task in &snapshot.persistent.tasks {
                    if row == self.selected_row {
                        return Some(key.as_str());
                    }
                    row += 1;
                }
            }
        }
        None
    }

    pub(crate) fn selected_workspace_snapshot(&self) -> Option<&WorkspaceSnapshot> {
        self.selected_workspace_key()
            .and_then(|key| self.snapshots.get(key))
    }

    pub(crate) fn selected_workspace_can_diff(&self) -> bool {
        if self.selected_link_index.is_some() {
            return false;
        }
        if self.selected_task_id().is_none() {
            return false;
        }

        self.selected_workspace_snapshot()
            .is_some_and(workspace_is_usable)
            && self.selected_workspace_repo_path().is_some()
    }

    pub(crate) fn selected_workspace_can_edit(&self) -> bool {
        self.selected_workspace_can_diff()
            && compare_tool_is_available(&self.service.config.compare)
    }

    fn selected_workspace_github_repository_url(&self) -> Option<String> {
        if self.selected_task_id().is_some() {
            return None;
        }

        self.selected_workspace_snapshot()?
            .persistent
            .assigned_repository
            .as_deref()
            .and_then(github_repository_url)
    }

    fn selected_workspace_repo_path(&self) -> Option<PathBuf> {
        let key = self.selected_workspace_key()?;
        let snapshot = self.snapshots.get(key)?;
        let workspace_path = self.service.workspace_directory_path().join(key);
        if let Some(task_id) = self.selected_task_id()
            && let Some(task) = task_persistent_snapshot(snapshot, task_id)
        {
            return compare_target_path_for_task(
                snapshot,
                task,
                task_runtime_snapshot(snapshot, task_id),
                &workspace_path,
            );
        }
        compare_target_path(
            snapshot,
            &self.workspace_link_validation_results,
            &workspace_path,
        )
    }

    fn selected_workspace_link_targets(&self) -> Vec<(WorkspaceLink, String)> {
        let Some(link) = self.selected_workspace_link() else {
            return Vec::new();
        };

        if link.value.is_empty() {
            return Vec::new();
        }
        let Some(snapshot) = self.selected_workspace_snapshot() else {
            return Vec::new();
        };
        let candidates = if let Some(task_id) = self.selected_task_id() {
            task_persistent_snapshot(snapshot, task_id)
                .map(|task| {
                    visible_task_links(
                        task,
                        task_runtime_snapshot(snapshot, task_id),
                        &self.workspace_link_validation_results,
                    )
                })
                .unwrap_or_default()
        } else {
            validated_workspace_links_by_kind(
                snapshot,
                &self.workspace_link_validation_results,
                link.kind,
            )
        };

        candidates
            .into_iter()
            .filter(|candidate| candidate.kind == link.kind)
            .filter_map(|candidate| {
                self.workspace_link_validation_results
                    .get(&candidate)
                    .and_then(|result| match result {
                        WorkspaceLinkValidationResult::Valid(argument) => {
                            Some((candidate, argument.clone()))
                        }
                        WorkspaceLinkValidationResult::Invalid(_) => None,
                    })
            })
            .collect()
    }

    fn normalize_selected_link_target_index(&mut self) {
        let target_count = self.selected_workspace_link_targets().len();
        if target_count == 0 {
            self.selected_link_target_index = 0;
        } else if self.selected_link_target_index >= target_count {
            self.selected_link_target_index = target_count - 1;
        }
    }

    pub(crate) fn selected_workspace_link_target_display_lines(&self) -> Vec<(String, bool)> {
        let Some(workspace_dir) = self
            .selected_workspace_key()
            .map(|key| self.service.workspace_directory_path().join(key))
        else {
            return Vec::new();
        };

        let targets = self.selected_workspace_link_targets();
        let Some(link) = self.selected_workspace_link() else {
            return Vec::new();
        };

        if link.value.is_empty() {
            let placeholder = match link.kind {
                WorkspaceLinkKind::Issue => "Add custom issue link…",
                WorkspaceLinkKind::Pr => "Add custom PR link…",
                WorkspaceLinkKind::Review => "Add custom link…",
            };
            return vec![(placeholder.to_string(), true)];
        }

        targets
            .into_iter()
            .map(|(target_link, target)| {
                let display = match link.kind {
                    WorkspaceLinkKind::Review => Path::new(&target)
                        .strip_prefix(&workspace_dir)
                        .map(|relative| relative.display().to_string())
                        .unwrap_or(target.clone()),
                    WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr => {
                        compact_github_tooltip_target(&target).unwrap_or(target.clone())
                    }
                };
                (display, target_link.is_custom())
            })
            .collect()
    }

    fn clear_custom_link_modal(&mut self) {
        self.custom_link_input.clear();
        self.custom_link_kind = None;
        self.custom_link_action = None;
        self.custom_link_original_value = None;
    }

    fn open_add_custom_link_modal(&mut self, kind: WorkspaceLinkKind) {
        self.custom_link_kind = Some(kind);
        self.custom_link_action = Some(CustomLinkModalAction::Add);
        self.custom_link_original_value = None;
        self.custom_link_input.clear();
        self.mode = UiMode::EditCustomLink;
    }

    fn open_edit_custom_link_modal(&mut self, link: &WorkspaceLink) {
        self.custom_link_kind = Some(link.kind);
        self.custom_link_action = Some(CustomLinkModalAction::Edit);
        self.custom_link_original_value = Some(link.value.clone());
        self.custom_link_input = link.value.clone();
        self.mode = UiMode::EditCustomLink;
    }

    fn can_add_custom_link_for_selected_kind(&self) -> Option<WorkspaceLinkKind> {
        if self.selected_task_id().is_some() {
            return None;
        }
        self.selected_workspace_link()
            .filter(|link| matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr))
            .map(|link| link.kind)
    }

    fn selected_workspace_selectable_links(&self) -> Vec<WorkspaceLink> {
        let Some(key) = self.selected_workspace_key() else {
            return Vec::new();
        };
        let Some(snapshot) = self.snapshots.get(key) else {
            return Vec::new();
        };

        if let Some(task_id) = self.selected_task_id() {
            return task_persistent_snapshot(snapshot, task_id)
                .map(|task| {
                    visible_task_links(
                        task,
                        task_runtime_snapshot(snapshot, task_id),
                        &self.workspace_link_validation_results,
                    )
                })
                .unwrap_or_default();
        }

        selectable_workspace_links(
            snapshot,
            &self.workspace_link_validation_results,
            &self.github_link_statuses,
        )
    }

    fn selected_workspace_link_argument(&self, link: &WorkspaceLink) -> Option<&str> {
        match self.workspace_link_validation_results.get(link) {
            Some(WorkspaceLinkValidationResult::Valid(argument)) => Some(argument.as_str()),
            Some(WorkspaceLinkValidationResult::Invalid(_)) | None => None,
        }
    }

    fn normalize_selected_link_index(&mut self) {
        if matches!(self.selected_entry(), Some(TableEntry::Create)) {
            self.selected_link_index = None;
            return;
        }

        let link_count = self.selected_workspace_selectable_links().len();
        if link_count == 0 {
            self.selected_link_index = None;
            self.selected_link_target_index = 0;
        } else if let Some(index) = self.selected_link_index
            && index >= link_count
        {
            self.selected_link_index = Some(link_count - 1);
        }
        self.normalize_selected_link_target_index();
    }

    fn refresh_workspace_link_validations(&mut self) {
        let mut active_links = HashSet::new();
        for snapshot in self.snapshots.values() {
            active_links.extend(workspace_links(snapshot));
            active_links.extend(workspace_issue_pr_links(snapshot));
            for task in &snapshot.persistent.tasks {
                active_links.extend(task_links(task, task_runtime_snapshot(snapshot, &task.id)));
            }
        }

        self.workspace_link_validation_results
            .retain(|link, _| active_links.contains(link));
        self.pending_workspace_link_validations
            .retain(|link, _| active_links.contains(link));

        for link in active_links {
            if self.workspace_link_validation_results.contains_key(&link)
                || self.pending_workspace_link_validations.contains_key(&link)
            {
                continue;
            }

            let workspace_dir = self.service.workspace_directory_path().to_path_buf();
            let link_for_task = link.clone();
            let (result_tx, result_rx) = oneshot::channel();
            tokio::spawn(async move {
                let result = validate_workspace_link_target(&link_for_task, &workspace_dir).await;
                let _ = result_tx.send(result);
            });
            self.pending_workspace_link_validations
                .insert(link, result_rx);
        }
    }

    pub(crate) fn poll_workspace_link_validations(&mut self) {
        let mut completed = Vec::new();
        for (link, result_rx) in &mut self.pending_workspace_link_validations {
            match result_rx.try_recv() {
                Ok(result) => completed.push((link.clone(), result)),
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => completed.push((
                    link.clone(),
                    Err(io::Error::other(
                        "workspace link validation task ended unexpectedly",
                    )),
                )),
            }
        }

        for (link, result) in completed {
            self.pending_workspace_link_validations.remove(&link);
            let validation = match result {
                Ok(argument) => WorkspaceLinkValidationResult::Valid(argument),
                Err(err) => WorkspaceLinkValidationResult::Invalid(err.to_string()),
            };
            self.workspace_link_validation_results
                .insert(link, validation);
        }

        self.normalize_selected_link_index();
        self.refresh_github_link_statuses();
    }

    fn refresh_github_link_statuses(&mut self) {
        let mut active_issue_or_pr_links = HashSet::new();
        for snapshot in self.snapshots.values() {
            active_issue_or_pr_links.extend(workspace_issue_pr_links(snapshot).into_iter().filter(
                |link| matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr),
            ));
            for task in &snapshot.persistent.tasks {
                active_issue_or_pr_links.extend(
                    task_links(task, task_runtime_snapshot(snapshot, &task.id))
                        .into_iter()
                        .filter(|link| {
                            matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr)
                        }),
                );
            }
        }

        self.github_link_status_rxs
            .retain(|link, _| active_issue_or_pr_links.contains(link));
        self.github_link_statuses
            .retain(|link, _| active_issue_or_pr_links.contains(link));

        for link in active_issue_or_pr_links {
            let Some(WorkspaceLinkValidationResult::Valid(validated_link)) =
                self.workspace_link_validation_results.get(&link)
            else {
                self.github_link_status_rxs.remove(&link);
                self.github_link_statuses.remove(&link);
                continue;
            };

            if !self.github_link_status_rxs.contains_key(&link) {
                let Some(receiver) = self
                    .service
                    .github_status_service()
                    .watch_status(validated_link)
                else {
                    self.github_link_statuses.remove(&link);
                    continue;
                };
                self.github_link_status_rxs.insert(link.clone(), receiver);
                let _ = self
                    .service
                    .github_status_service()
                    .request_refresh(validated_link);
            }

            let status = self
                .github_link_status_rxs
                .get(&link)
                .and_then(|receiver| *receiver.borrow())
                .and_then(|status| match (link.kind, status) {
                    (WorkspaceLinkKind::Issue, GithubStatus::Issue(issue_status)) => {
                        Some(GithubLinkStatusView::Issue(issue_status))
                    }
                    (WorkspaceLinkKind::Pr, GithubStatus::Pr(pr_status)) => {
                        Some(GithubLinkStatusView::Pr(pr_status))
                    }
                    _ => None,
                });

            if let Some(status) = status {
                self.github_link_statuses.insert(link, status);
            } else {
                self.github_link_statuses.remove(&link);
            }
        }
    }

    pub(crate) fn selected_workspace_link_count(&self) -> usize {
        self.selected_workspace_selectable_links().len()
    }

    pub(crate) fn selected_workspace_has_refreshable_github_link(&self) -> bool {
        self.selected_workspace_snapshot().is_some_and(|snapshot| {
            should_request_autonomous_issue_scan(
                snapshot,
                self.service.config.autonomous.max_parallel_issues,
            ) || self
                .selected_workspace_selectable_links()
                .into_iter()
                .any(|link| matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr))
        })
    }

    pub(crate) fn selected_workspace_link(&self) -> Option<WorkspaceLink> {
        let index = self.selected_link_index?;
        self.selected_workspace_selectable_links()
            .into_iter()
            .nth(index)
    }

    fn request_selected_workspace_github_status_refresh(&mut self) {
        let Some(workspace_key) = self.selected_workspace_key().map(str::to_string) else {
            return;
        };
        let autonomous_scan_requested = self
            .snapshots
            .get(&workspace_key)
            .filter(|snapshot| {
                should_request_autonomous_issue_scan(
                    snapshot,
                    self.service.config.autonomous.max_parallel_issues,
                )
            })
            .map(|_| self.service.request_workspace_issue_scan(&workspace_key))
            .transpose();

        let requested_refreshes = self
            .selected_workspace_selectable_links()
            .into_iter()
            .filter(|link| matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr))
            .filter_map(|link| self.selected_workspace_link_argument(&link))
            .filter(|url| self.service.github_status_service().request_refresh(url))
            .count();

        match autonomous_scan_requested {
            Err(err) => {
                self.status = format!(
                    "Failed to request autonomous issue scan for workspace '{workspace_key}': {err:?}"
                );
            }
            Ok(Some(())) => {
                if requested_refreshes > 0 {
                    self.status = format!(
                        "Requested GitHub status recheck and autonomous issue scan for workspace '{workspace_key}'"
                    );
                } else {
                    self.status =
                        format!("Requested autonomous issue scan for workspace '{workspace_key}'");
                }
            }
            Ok(None) => {
                if requested_refreshes > 0 {
                    self.status =
                        format!("Requested GitHub status recheck for workspace '{workspace_key}'");
                } else {
                    self.status = format!(
                        "No refreshable GitHub status links available for workspace '{workspace_key}'"
                    );
                }
            }
        }
    }

    fn request_selected_workspace_queue_next_issue(&mut self) {
        let Some(workspace_key) = self.selected_workspace_key().map(str::to_string) else {
            return;
        };
        let Some(snapshot) = self.snapshots.get(&workspace_key) else {
            return;
        };
        if !workspace_can_queue_next_issue(snapshot) {
            return;
        }

        match self.service.request_workspace_queue_next(&workspace_key) {
            Ok(()) => {
                self.status = format!("Requested next issue for workspace '{workspace_key}'");
            }
            Err(err) => {
                self.status = format!(
                    "Failed to request next issue for workspace '{workspace_key}': {err:?}"
                );
            }
        }
    }

    fn start_workspace_operation(&mut self, workspace_key: String, attach_when_ready: bool) {
        let initial_progress = if attach_when_ready {
            format!("Starting workspace '{workspace_key}' before attaching...")
        } else {
            format!("Starting workspace '{workspace_key}'...")
        };
        let (progress_tx, progress_rx) = watch::channel(initial_progress);
        let (result_tx, result_rx) = oneshot::channel();
        let service = self.service.clone();
        let workspace_key_for_task = workspace_key.clone();

        tokio::spawn(async move {
            let _ = progress_tx.send(format!(
                "Starting runtime for workspace '{workspace_key_for_task}'..."
            ));
            let result = service
                .start_workspace(&workspace_key_for_task)
                .await
                .map_err(|err| err.summary());
            if result.is_ok() {
                let _ = progress_tx.send(format!(
                    "Runtime started for workspace '{workspace_key_for_task}'. Waiting for server readiness..."
                ));
            }
            let _ = result_tx.send(result);
        });

        self.running_operation = Some(RunningOperation {
            workspace_key: workspace_key.clone(),
            operation_name: "Start".to_string(),
            success_status: None,
            progress_rx,
            result_rx,
            completion_action: RunningOperationCompletionAction::WaitForWorkspaceStart {
                attach_when_ready,
            },
            cancel: None,
        });
        self.mode = UiMode::ToolProgressModal;
        self.status = if attach_when_ready {
            format!("Starting workspace '{workspace_key}' before attaching")
        } else {
            format!("Starting workspace '{workspace_key}'")
        };
    }

    fn start_stop_workspace_operation(&mut self, workspace_key: String) {
        let (progress_tx, progress_rx) =
            watch::channel(format!("Stopping workspace '{workspace_key}'..."));
        let (result_tx, result_rx) = oneshot::channel();
        let service = self.service.clone();
        let workspace_key_for_task = workspace_key.clone();

        tokio::spawn(async move {
            let _ = progress_tx.send(format!(
                "Stopping runtime for workspace '{workspace_key_for_task}'..."
            ));
            let result = service
                .stop_workspace(&workspace_key_for_task)
                .await
                .map_err(|err| err.summary());
            if result.is_ok() {
                let _ = progress_tx.send(format!(
                    "Stopped workspace '{workspace_key_for_task}'. Refreshing UI state..."
                ));
            }
            let _ = result_tx.send(result);
        });

        self.running_operation = Some(RunningOperation {
            workspace_key: workspace_key.clone(),
            operation_name: "Stop".to_string(),
            success_status: Some(format!("Stopped workspace '{workspace_key}'")),
            progress_rx,
            result_rx,
            completion_action: RunningOperationCompletionAction::None,
            cancel: None,
        });
        self.mode = UiMode::ToolProgressModal;
        self.status = format!("Stopping workspace '{workspace_key}'");
    }

    fn move_selected_link_right(&mut self) {
        let link_count = self.selected_workspace_link_count();
        self.selected_link_index = next_link_selection_right(self.selected_link_index, link_count);
        self.selected_link_target_index = 0;
    }

    fn move_selected_link_left(&mut self) {
        let link_count = self.selected_workspace_link_count();
        if link_count == 0 {
            self.selected_link_index = None;
            return;
        }
        self.selected_link_index = match self.selected_link_index {
            Some(index) if index > 0 => Some(index - 1),
            Some(_) => None,
            None => None,
        };
        self.selected_link_target_index = 0;
    }

    fn move_selected_link_target_up(&mut self) {
        let target_count = self.selected_workspace_link_targets().len();
        if target_count == 0 {
            self.selected_link_target_index = 0;
        } else if self.selected_link_target_index > 0 {
            self.selected_link_target_index -= 1;
        }
    }

    fn move_selected_link_target_down(&mut self) {
        let target_count = self.selected_workspace_link_targets().len();
        if target_count == 0 {
            self.selected_link_target_index = 0;
        } else if self.selected_link_target_index + 1 < target_count {
            self.selected_link_target_index += 1;
        }
    }

    pub(crate) fn contextual_tool_hotkeys(&self) -> Vec<(String, String)> {
        let Some(snapshot) = self.selected_workspace_snapshot() else {
            return Vec::new();
        };
        let mut seen = HashSet::new();

        if self.selected_task_id().is_some() {
            if self.selected_workspace_repo_path().is_none() {
                return Vec::new();
            }
            return self
                .service
                .config
                .tool
                .iter()
                .filter(|tool| matches!(tool.type_, ToolType::Exec))
                .filter(|tool| tool_is_usable(tool, snapshot))
                .filter_map(|tool| {
                    let ch = tool_key_char(tool)?;
                    seen.insert(ch)
                        .then_some((ch.to_string(), tool.name.clone()))
                })
                .collect();
        }

        self.service
            .config
            .tool
            .iter()
            .filter(|tool| matches!(tool.type_, ToolType::Prompt))
            .filter(|tool| tool_is_usable(tool, snapshot))
            .filter_map(|tool| {
                let ch = tool_key_char(tool)?;
                seen.insert(ch)
                    .then_some((ch.to_string(), tool.name.clone()))
            })
            .collect()
    }

    fn auto_attach_ready_key(&mut self, now: Instant) -> Option<String> {
        let key = self.starting_workspace_key.as_deref()?;
        let snapshot = self.snapshots.get(key)?;
        let (ready, next_wait_since) =
            started_workspace_attach_ready(snapshot, self.started_wait_since, now);
        self.started_wait_since = next_wait_since;
        ready.then(|| key.to_string())
    }

    fn snapshot_attach_target(&self, key: &str) -> io::Result<AttachTarget> {
        let snapshot = self
            .snapshots
            .get(key)
            .ok_or_else(|| io::Error::other(format!("workspace snapshot missing for '{key}'")))?;
        if self.service.agent_provider() == multicode_lib::services::AgentProvider::Codex {
            let cwd = self
                .attach_cwd_for_workspace(key)
                .map(|path| path.to_string_lossy().into_owned());
            if let Some(target) =
                working_codex_task_attach_target(snapshot, self.selected_task_id(), cwd)?
            {
                return Ok(target);
            }
        }
        snapshot_attach_target_for_selection(snapshot, self.selected_task_id())
    }

    fn record_attached_session(&mut self, key: &str, target: &AttachTarget) {
        let task_id = self.selected_task_id().map(str::to_string);
        let fresh_codex_session = matches!(target, AttachTarget::CodexNew { .. });
        let initial_agent_state = self.snapshots.get(key).and_then(|snapshot| {
            task_id
                .as_deref()
                .and_then(|task_id| task_runtime_snapshot(snapshot, task_id))
                .and_then(|task_state| task_effective_agent_state(Some(task_state)))
                .or_else(|| {
                    if task_id.is_none() {
                        snapshot.automation_agent_state
                    } else {
                        None
                    }
                })
        });
        let session_id = match target {
            AttachTarget::Opencode { session_id, .. } => session_id.clone(),
            AttachTarget::Codex { thread_id, .. } => thread_id.clone(),
            AttachTarget::CodexNew { .. } => None,
        };
        let initial_turn_metrics =
            if self.service.agent_provider() == multicode_lib::services::AgentProvider::Codex {
                if fresh_codex_session && task_id.is_some() {
                    Some(CodexSessionTurnMetrics::default())
                } else {
                    session_id.as_deref().and_then(|session_id| {
                        read_codex_session_turn_metrics(
                            self.service.workspace_directory_path(),
                            key,
                            session_id,
                        )
                    })
                }
            } else {
                None
            };
        tracing::info!(
            workspace_key = %key,
            task_id = task_id.as_deref().unwrap_or("<root>"),
            session_id = session_id.as_deref().unwrap_or("<none>"),
            initial_agent_state = ?initial_agent_state,
            initial_turn_metrics = ?initial_turn_metrics,
            "recorded attached session"
        );
        self.attached_session = Some(AttachedSession {
            workspace_key: key.to_string(),
            task_id,
            session_id,
            initial_agent_state,
            initial_turn_metrics,
            fresh_codex_session,
        });
    }

    fn attach_env_for_workspace(&self, key: &str) -> Vec<(String, String)> {
        if self.service.agent_provider() != multicode_lib::services::AgentProvider::Codex {
            return Vec::new();
        }

        vec![(
            "CODEX_HOME".to_string(),
            self.service
                .workspace_directory_path()
                .join(".multicode")
                .join("codex")
                .join(key)
                .join("home")
                .to_string_lossy()
                .into_owned(),
        )]
    }

    fn attach_cwd_for_workspace(&self, key: &str) -> Option<PathBuf> {
        let snapshot = self.snapshots.get(key)?;
        let workspace_path = self.service.workspace_directory_path().join(key);
        snapshot_attach_cwd_for_selection(
            snapshot,
            self.selected_task_id(),
            &self.workspace_link_validation_results,
            &workspace_path,
        )
    }

    fn selected_task_can_request_pr_creation(&self) -> bool {
        if self.selected_link_index.is_some() {
            return false;
        }
        if self.service.agent_provider() != multicode_lib::services::AgentProvider::Codex {
            return false;
        }
        let Some(snapshot) = self.selected_workspace_snapshot() else {
            return false;
        };
        let Some(task_id) = self.selected_task_id() else {
            return false;
        };
        workspace_is_usable(snapshot)
            && workspace_state(snapshot) == WorkspaceUiState::Started
            && task_persistent_snapshot(snapshot, task_id).is_some()
    }

    fn selected_task_pr_link(&self) -> Option<String> {
        let snapshot = self.selected_workspace_snapshot()?;
        let task_id = self.selected_task_id()?;
        let task = task_persistent_snapshot(snapshot, task_id)?;
        Some(task_pr_link(task, task_runtime_snapshot(snapshot, task_id))?.to_string())
    }

    fn selected_task_pr_status(&self) -> Option<GithubPrStatus> {
        let pr_link = self.selected_task_pr_link()?;
        let link = WorkspaceLink {
            kind: WorkspaceLinkKind::Pr,
            value: pr_link,
            source: WorkspaceLinkSource::Task,
        };
        match self.github_link_statuses.get(&link) {
            Some(GithubLinkStatusView::Pr(pr_status)) => Some(*pr_status),
            _ => None,
        }
    }

    pub(crate) fn selected_task_can_request_ci_fix(&self) -> bool {
        self.selected_task_can_request_pr_creation()
            && should_offer_codex_ci_fix(
                self.selected_task_pr_link().is_some(),
                self.selected_task_pr_status(),
            )
            && self.selected_task_ci_fix_prompt().is_some()
    }

    fn selected_task_ci_fix_prompt(&self) -> Option<String> {
        let workspace_key = self.selected_workspace_key()?;
        let snapshot = self.selected_workspace_snapshot()?;
        let task_id = self.selected_task_id()?;
        let task = task_persistent_snapshot(snapshot, task_id)?;
        let assigned_repository = task_repository_spec(snapshot, task_id)?;
        let cwd = self.service.workspace_task_checkout_path(
            workspace_key,
            &assigned_repository,
            &task.issue_url,
        );
        let task_state_path = self
            .service
            .workspace_directory_path()
            .join(".multicode")
            .join("automation")
            .join(workspace_key)
            .join("tasks")
            .join(format!("{task_id}.state"));
        Some(build_codex_fix_ci_prompt(
            &assigned_repository,
            &task.issue_url,
            task_pr_link(task, task_runtime_snapshot(snapshot, task_id))
                .or(task.backing_pr_url.as_deref()),
            &cwd,
            &task_state_path,
            task_runtime_snapshot(snapshot, task_id)
                .and_then(|task_state| task_state.session_id.as_deref()),
        ))
    }

    fn selected_task_default_github_url(&self) -> Option<String> {
        let snapshot = self.selected_workspace_snapshot()?;
        let task_id = self.selected_task_id()?;
        let task = task_persistent_snapshot(snapshot, task_id)?;
        Some(task_issue_link(task, task_runtime_snapshot(snapshot, task_id)).to_string())
    }

    async fn approve_selected_task_for_pr_creation(&mut self) {
        if !self.selected_task_can_request_pr_creation() {
            return;
        }
        let Some(workspace_key) = self.selected_workspace_key().map(str::to_string) else {
            return;
        };
        let Some(task_id) = self.selected_task_id().map(str::to_string) else {
            return;
        };
        let Some(snapshot) = self.snapshots.get(&workspace_key).cloned() else {
            return;
        };
        let previous_snapshot = snapshot.clone();
        let should_restart =
            should_restart_codex_task_for_ci_fix(task_runtime_snapshot(&snapshot, &task_id));
        let (progress_tx, progress_rx) =
            watch::channel("Preparing PR approval request...".to_string());
        let (result_tx, result_rx) = oneshot::channel();
        let service = self.service.clone();
        let workspace_key_for_task = workspace_key.clone();
        let task_id_for_task = task_id.clone();

        self.mark_task_resuming_in_background(&workspace_key, &task_id);
        tokio::spawn(async move {
            let progress_message = if should_restart {
                "Restarting the Codex review session before asking for PR creation..."
            } else {
                "Asking Codex to create or update the PR..."
            };
            let _ = progress_tx.send(progress_message.to_string());
            let result = if should_restart {
                service
                    .restart_task_session(
                        &workspace_key_for_task,
                        &snapshot,
                        &task_id_for_task,
                        CODEX_CREATE_PR_APPROVAL_PROMPT,
                    )
                    .await
            } else {
                service
                    .prompt_task_session(
                        &workspace_key_for_task,
                        &snapshot,
                        &task_id_for_task,
                        CODEX_CREATE_PR_APPROVAL_PROMPT,
                    )
                    .await
            };

            let result = match result {
                Ok(()) => {
                    service
                        .prompt_task_session(
                            &workspace_key_for_task,
                            &snapshot,
                            &task_id_for_task,
                            CODEX_AUTO_RESUME_PROMPT,
                        )
                        .await
                }
                Err(err) => Err(err),
            };

            if let Err(err) = result {
                if let Ok(workspace) = service.manager.get_workspace(&workspace_key_for_task) {
                    workspace.update(|next| {
                        let mut changed = false;
                        match previous_snapshot
                            .task_states
                            .get(&task_id_for_task)
                            .cloned()
                        {
                            Some(previous_task_state) => {
                                if next.task_states.get(&task_id_for_task)
                                    != Some(&previous_task_state)
                                {
                                    next.task_states
                                        .insert(task_id_for_task.clone(), previous_task_state);
                                    changed = true;
                                }
                            }
                            None => {
                                if next.task_states.remove(&task_id_for_task).is_some() {
                                    changed = true;
                                }
                            }
                        }
                        if next.active_task_id != previous_snapshot.active_task_id {
                            next.active_task_id = previous_snapshot.active_task_id.clone();
                            changed = true;
                        }
                        if next.automation_agent_state != previous_snapshot.automation_agent_state {
                            next.automation_agent_state = previous_snapshot.automation_agent_state;
                            changed = true;
                        }
                        if next.automation_session_status
                            != previous_snapshot.automation_session_status
                        {
                            next.automation_session_status =
                                previous_snapshot.automation_session_status;
                            changed = true;
                        }
                        if next.automation_status != previous_snapshot.automation_status {
                            next.automation_status = previous_snapshot.automation_status.clone();
                            changed = true;
                        }
                        changed
                    });
                }
                let _ = result_tx.send(Err(err));
                return;
            }

            let _ = progress_tx.send(
                "Codex accepted the PR request and is continuing in the background.".to_string(),
            );
            let _ = result_tx.send(Ok(()));
        });

        self.running_operation = Some(RunningOperation {
            workspace_key: workspace_key.clone(),
            operation_name: format!("Approve {task_id}"),
            success_status: Some(format!(
                "PR request sent for '{task_id}' in workspace '{workspace_key}'; Codex is continuing in the background"
            )),
            progress_rx,
            result_rx,
            completion_action: RunningOperationCompletionAction::None,
            cancel: None,
        });
        self.status = format!(
            "Approved local changes for '{task_id}' in workspace '{workspace_key}'; sending the PR request to Codex in the background"
        );
    }

    async fn fix_selected_task_ci(&mut self) {
        if !self.selected_task_can_request_ci_fix() {
            self.status = "Fix CI is unavailable for the selected task".to_string();
            return;
        }
        let Some(workspace_key) = self.selected_workspace_key().map(str::to_string) else {
            self.status =
                "Fix CI is unavailable because the selected workspace could not be resolved"
                    .to_string();
            return;
        };
        let Some(task_id) = self.selected_task_id().map(str::to_string) else {
            self.status =
                "Fix CI is unavailable because the selected task could not be resolved".to_string();
            return;
        };
        let Some(prompt) = self.selected_task_ci_fix_prompt() else {
            self.status = format!(
                "Fix CI is unavailable for '{task_id}' in workspace '{workspace_key}' because its repository or checkout context could not be resolved"
            );
            return;
        };
        let Some(snapshot) = self.snapshots.get(&workspace_key).cloned() else {
            self.status = format!(
                "Fix CI is unavailable for '{task_id}' in workspace '{workspace_key}' because the latest workspace snapshot is missing"
            );
            return;
        };
        let previous_snapshot = snapshot.clone();
        let should_restart =
            should_restart_codex_task_for_ci_fix(task_runtime_snapshot(&snapshot, &task_id));
        let (progress_tx, progress_rx) = watch::channel("Preparing CI fix request...".to_string());
        let (result_tx, result_rx) = oneshot::channel();
        let service = self.service.clone();
        let workspace_key_for_task = workspace_key.clone();
        let task_id_for_task = task_id.clone();
        self.persist_task_resume_prompt(&workspace_key, &task_id, &prompt);

        self.mark_task_resuming_in_background(&workspace_key, &task_id);
        tokio::spawn(async move {
            let progress_message = if should_restart {
                "Restarting the Codex task session before asking it to fix CI..."
            } else {
                "Asking Codex to fix CI failures and continue in the background..."
            };
            let _ = progress_tx.send(progress_message.to_string());
            let result = if should_restart {
                service
                    .restart_task_session(
                        &workspace_key_for_task,
                        &snapshot,
                        &task_id_for_task,
                        &prompt,
                    )
                    .await
            } else {
                service
                    .prompt_task_session(
                        &workspace_key_for_task,
                        &snapshot,
                        &task_id_for_task,
                        &prompt,
                    )
                    .await
            };

            if let Err(err) = result {
                if let Ok(workspace) = service.manager.get_workspace(&workspace_key_for_task) {
                    workspace.update(|next| {
                        let mut changed = false;
                        match previous_snapshot
                            .task_states
                            .get(&task_id_for_task)
                            .cloned()
                        {
                            Some(previous_task_state) => {
                                if next.task_states.get(&task_id_for_task)
                                    != Some(&previous_task_state)
                                {
                                    next.task_states
                                        .insert(task_id_for_task.clone(), previous_task_state);
                                    changed = true;
                                }
                            }
                            None => {
                                if next.task_states.remove(&task_id_for_task).is_some() {
                                    changed = true;
                                }
                            }
                        }
                        if next.active_task_id != previous_snapshot.active_task_id {
                            next.active_task_id = previous_snapshot.active_task_id.clone();
                            changed = true;
                        }
                        if next.automation_agent_state != previous_snapshot.automation_agent_state {
                            next.automation_agent_state = previous_snapshot.automation_agent_state;
                            changed = true;
                        }
                        if next.automation_session_status
                            != previous_snapshot.automation_session_status
                        {
                            next.automation_session_status =
                                previous_snapshot.automation_session_status;
                            changed = true;
                        }
                        if next.automation_status != previous_snapshot.automation_status {
                            next.automation_status = previous_snapshot.automation_status.clone();
                            changed = true;
                        }
                        changed
                    });
                }
                let _ = result_tx.send(Err(err));
                return;
            }

            let _ = progress_tx.send(
                "Codex accepted the CI fix request and is continuing in the background."
                    .to_string(),
            );
            let _ = result_tx.send(Ok(()));
        });

        self.running_operation = Some(RunningOperation {
            workspace_key: workspace_key.clone(),
            operation_name: format!("Fix CI {task_id}"),
            success_status: Some(format!(
                "CI fix request sent for '{task_id}' in workspace '{workspace_key}'; Codex is continuing in the background"
            )),
            progress_rx,
            result_rx,
            completion_action: RunningOperationCompletionAction::None,
            cancel: None,
        });
        self.status = format!(
            "Requested CI fixes for '{task_id}' in workspace '{workspace_key}'; sending the request to Codex in the background"
        );
    }

    pub(crate) async fn handle_key(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        key: KeyEvent,
    ) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        match self.mode {
            UiMode::Normal => self.handle_normal_key(terminal, key).await,
            UiMode::CreateModal => self.handle_create_modal_key(key).await,
            UiMode::EditDescription => self.handle_edit_key(key),
            UiMode::EditIssue => self.handle_issue_key(key).await,
            UiMode::EditCustomLink => self.handle_custom_link_key(key),
            UiMode::ConfirmDelete => self.handle_confirm_delete_key(key).await,
            UiMode::ConfirmTaskRemoval => self.handle_confirm_task_removal_key(key).await,
            UiMode::StartingModal => {}
            UiMode::ToolProgressModal => self.handle_tool_progress_key(key),
        }
    }

    pub(crate) async fn handle_auto_attach_when_ready(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) {
        if !self.starting_attach_when_ready {
            return;
        }
        let Some(key) = self.auto_attach_ready_key(Instant::now()) else {
            return;
        };

        self.mode = UiMode::Normal;
        self.starting_workspace_key = None;
        self.starting_attach_when_ready = false;
        self.started_wait_since = None;

        match self.snapshot_attach_target(&key) {
            Ok(target) => {
                self.record_attached_session(&key, &target);
                let custom_description = self
                    .snapshots
                    .get(&key)
                    .map(|snapshot| snapshot.persistent.description.clone())
                    .unwrap_or_default();
                match attach_in_tmux(
                    terminal,
                    self.service.agent_command(),
                    &target,
                    self.attach_cwd_for_workspace(&key).as_deref(),
                    &self.attach_env_for_workspace(&key),
                    &key,
                    &custom_description,
                )
                .await
                {
                    Ok(_) => {
                        if !self
                            .retry_codex_task_attach_with_fresh_session(terminal, &key)
                            .await
                            && !self
                                .retry_codex_task_attach_with_last_thread(terminal, &key)
                                .await
                        {
                            self.handle_attach_exit(&key).await;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            workspace_key = %key,
                            error = %err,
                            "attach session exited with error"
                        );
                        if !self
                            .retry_codex_task_attach_with_fresh_session(terminal, &key)
                            .await
                            && !self
                                .retry_codex_task_attach_with_last_thread(terminal, &key)
                                .await
                            && !self.handle_attach_exit_after_error(&key, &err).await
                        {
                            self.status = format!("Failed to attach to workspace '{key}': {err}");
                        }
                    }
                }
            }
            Err(err) => {
                self.status = format!("Failed to attach to workspace '{key}': {err}");
            }
        }
    }

    async fn handle_attach_exit(&mut self, key: &str) {
        tracing::info!(workspace_key = %key, "handling attach exit");
        if self.maybe_resume_autonomous_codex_after_attach(key).await {
            return;
        }
        self.status = format!("Detached from workspace '{key}' agent session");
    }

    async fn handle_attach_exit_after_error(&mut self, key: &str, err: &io::Error) -> bool {
        tracing::info!(
            workspace_key = %key,
            error = %err,
            attached_session = ?self.attached_session,
            "handling attach exit after error"
        );
        if self
            .attached_session
            .as_ref()
            .is_some_and(|attached| attached.workspace_key == key)
            && self.maybe_resume_autonomous_codex_after_attach(key).await
        {
            return true;
        }
        false
    }

    async fn retry_codex_task_attach_with_last_thread(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        key: &str,
    ) -> bool {
        let attached_session = self.attached_session.clone();
        let Some(target) = should_retry_codex_task_attach_with_last_thread(
            self.service.agent_provider(),
            self.snapshots.get(key),
            key,
            attached_session
                .as_ref()
                .map(|attached| attached.workspace_key.as_str()),
            attached_session
                .as_ref()
                .and_then(|attached| attached.task_id.as_deref()),
            attached_session
                .as_ref()
                .and_then(|attached| attached.session_id.as_deref()),
        ) else {
            return false;
        };

        let previous_session_id = attached_session
            .as_ref()
            .and_then(|attached| attached.session_id.as_deref())
            .unwrap_or("<none>");
        let custom_description = self
            .snapshots
            .get(key)
            .map(|snapshot| snapshot.persistent.description.clone())
            .unwrap_or_default();
        tracing::info!(
            workspace_key = %key,
            previous_session_id,
            "retrying codex task attach with last thread after explicit thread exited"
        );
        self.record_attached_session(key, &target);
        match attach_in_tmux(
            terminal,
            self.service.agent_command(),
            &target,
            self.attach_cwd_for_workspace(key).as_deref(),
            &self.attach_env_for_workspace(key),
            key,
            &custom_description,
        )
        .await
        {
            Ok(_) => {
                self.handle_attach_exit(key).await;
            }
            Err(err) => {
                tracing::warn!(
                    workspace_key = %key,
                    error = %err,
                    "codex retry attach session exited with error"
                );
                if !self.handle_attach_exit_after_error(key, &err).await {
                    self.status = format!("Failed to attach to workspace '{key}': {err}");
                }
            }
        }
        true
    }

    async fn retry_codex_task_attach_with_fresh_session(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        key: &str,
    ) -> bool {
        let attached_session = self.attached_session.clone();
        let session_id = attached_session
            .as_ref()
            .and_then(|attached| attached.session_id.as_deref());
        let current_turn_metrics = session_id.and_then(|session_id| {
            read_codex_session_turn_metrics(
                self.service.workspace_directory_path(),
                key,
                session_id,
            )
        });
        let current_thread_status = match (
            self.snapshots
                .get(key)
                .and_then(|snapshot| snapshot.transient.as_ref()),
            session_id,
        ) {
            (Some(transient), Some(session_id)) => CodexAppServerClient::new(transient.uri.clone())
                .thread_read(session_id)
                .await
                .ok()
                .and_then(|response| response.thread.status),
            _ => None,
        };
        let should_start_fresh = should_start_fresh_codex_task_session_after_failed_attach(
            self.service.agent_provider(),
            attached_session
                .as_ref()
                .map(|attached| attached.workspace_key.as_str()),
            key,
            attached_session
                .as_ref()
                .and_then(|attached| attached.task_id.as_deref()),
            session_id,
            attached_session
                .as_ref()
                .and_then(|attached| attached.initial_turn_metrics),
            current_turn_metrics,
            current_thread_status.as_ref(),
        );
        tracing::info!(
            workspace_key = %key,
            session_id = session_id.unwrap_or("<none>"),
            initial_turn_metrics = ?attached_session
                .as_ref()
                .and_then(|attached| attached.initial_turn_metrics),
            current_turn_metrics = ?current_turn_metrics,
            current_thread_status = ?current_thread_status,
            should_start_fresh,
            "evaluated fresh codex task session fallback after failed attach"
        );
        if !should_start_fresh {
            return false;
        }

        let Some(snapshot) = self.snapshots.get(key) else {
            return false;
        };
        let Ok(Some(uri)) = codex_attach_uri(snapshot) else {
            return false;
        };
        let cwd = self
            .attach_cwd_for_workspace(key)
            .map(|path| path.to_string_lossy().into_owned());
        let prompt = if let Some(session_id) = attached_session
            .as_ref()
            .and_then(|attached| attached.session_id.as_deref())
        {
            read_interrupted_codex_resume_prompt(
                self.service.workspace_directory_path().to_path_buf(),
                key.to_string(),
                session_id.to_string(),
            )
            .await
            .or_else(|| Some("Continue work on this task.".to_string()))
        } else {
            Some("Continue work on this task.".to_string())
        };
        let target = AttachTarget::CodexNew { uri, cwd, prompt };
        let custom_description = self
            .snapshots
            .get(key)
            .map(|snapshot| snapshot.persistent.description.clone())
            .unwrap_or_default();
        tracing::info!(
            workspace_key = %key,
            previous_session_id = attached_session
                .as_ref()
                .and_then(|attached| attached.session_id.as_deref())
                .unwrap_or("<none>"),
            "starting fresh codex task session after failed resume attach"
        );
        self.record_attached_session(key, &target);
        match attach_in_tmux(
            terminal,
            self.service.agent_command(),
            &target,
            self.attach_cwd_for_workspace(key).as_deref(),
            &self.attach_env_for_workspace(key),
            key,
            &custom_description,
        )
        .await
        {
            Ok(_) => {
                self.handle_attach_exit(key).await;
            }
            Err(err) => {
                tracing::warn!(
                    workspace_key = %key,
                    error = %err,
                    "fresh codex attach session exited with error"
                );
                if !self.handle_attach_exit_after_error(key, &err).await {
                    self.status = format!("Failed to attach to workspace '{key}': {err}");
                }
            }
        }
        true
    }

    fn mark_task_resuming_in_background(&mut self, workspace_key: &str, task_id: &str) {
        let Ok(workspace) = self.service.manager.get_workspace(workspace_key) else {
            return;
        };
        workspace.update(|snapshot| {
            let task_state = snapshot.task_states.entry(task_id.to_string()).or_default();
            let mut changed = false;
            if task_state.agent_state != Some(AutomationAgentState::Working) {
                task_state.agent_state = Some(AutomationAgentState::Working);
                changed = true;
            }
            if task_state.session_status != Some(RootSessionStatus::Busy) {
                task_state.session_status = Some(RootSessionStatus::Busy);
                changed = true;
            }
            let task_status = Some("Resuming in background".to_string());
            if task_state.status != task_status {
                task_state.status = task_status;
                changed = true;
            }
            if task_state.waiting_on_vm {
                task_state.waiting_on_vm = false;
                changed = true;
            }
            if snapshot.active_task_id.as_deref() != Some(task_id) {
                snapshot.active_task_id = Some(task_id.to_string());
                changed = true;
            }
            if snapshot.automation_agent_state != Some(AutomationAgentState::Working) {
                snapshot.automation_agent_state = Some(AutomationAgentState::Working);
                changed = true;
            }
            if snapshot.automation_session_status != Some(RootSessionStatus::Busy) {
                snapshot.automation_session_status = Some(RootSessionStatus::Busy);
                changed = true;
            }
            let automation_status = Some(format!("Resuming {task_id} in background"));
            if snapshot.automation_status != automation_status {
                snapshot.automation_status = automation_status;
                changed = true;
            }
            changed
        });
    }

    fn mark_task_waiting_on_vm_after_attach(&mut self, workspace_key: &str, task_id: &str) {
        let Ok(workspace) = self.service.manager.get_workspace(workspace_key) else {
            return;
        };
        workspace.update(|snapshot| {
            let task_state = snapshot.task_states.entry(task_id.to_string()).or_default();
            let mut changed = false;
            if task_state.agent_state != Some(AutomationAgentState::Working) {
                task_state.agent_state = Some(AutomationAgentState::Working);
                changed = true;
            }
            if task_state.session_status != Some(RootSessionStatus::Busy) {
                task_state.session_status = Some(RootSessionStatus::Busy);
                changed = true;
            }
            let task_status = Some("Queued until VM is free".to_string());
            if task_state.status != task_status {
                task_state.status = task_status;
                changed = true;
            }
            if !task_state.waiting_on_vm {
                task_state.waiting_on_vm = true;
                changed = true;
            }
            changed
        });
    }

    fn persist_task_resume_prompt(&self, workspace_key: &str, task_id: &str, resume_prompt: &str) {
        let Ok(workspace) = self.service.manager.get_workspace(workspace_key) else {
            return;
        };
        let resume_prompt = resume_prompt.trim().to_string();
        if resume_prompt.is_empty() {
            return;
        }
        workspace.update(|snapshot| {
            let task_state = snapshot.task_states.entry(task_id.to_string()).or_default();
            if task_state.resume_prompt.as_deref() == Some(resume_prompt.as_str()) {
                false
            } else {
                task_state.resume_prompt = Some(resume_prompt.clone());
                true
            }
        });
    }

    fn queue_task_codex_resume_until_vm_available(
        &self,
        workspace_key: String,
        task_id: String,
        attached_session: AttachedSession,
        resume_prompt: Option<String>,
    ) {
        let service = self.service.clone();
        tokio::spawn(async move {
            loop {
                let Ok(workspace) = service.manager.get_workspace(&workspace_key) else {
                    return;
                };
                let snapshot = workspace.subscribe().borrow().clone();
                let task_state = task_runtime_snapshot(&snapshot, &task_id);
                let should_resume = should_auto_resume_task_codex_after_attach(
                    task_state,
                    attached_session.session_id.as_deref(),
                    attached_session.initial_agent_state,
                ) || resume_prompt.is_some();
                if !should_resume {
                    return;
                }
                if should_queue_task_codex_resume_until_vm_available(&snapshot, &task_id) {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }

                workspace.update(|snapshot| {
                    let task_state = snapshot.task_states.entry(task_id.clone()).or_default();
                    let mut changed = false;
                    if task_state.agent_state != Some(AutomationAgentState::Working) {
                        task_state.agent_state = Some(AutomationAgentState::Working);
                        changed = true;
                    }
                    if task_state.session_status != Some(RootSessionStatus::Busy) {
                        task_state.session_status = Some(RootSessionStatus::Busy);
                        changed = true;
                    }
                    if task_state.waiting_on_vm {
                        task_state.waiting_on_vm = false;
                        changed = true;
                    }
                    let task_status = Some("Resuming in background".to_string());
                    if task_state.status != task_status {
                        task_state.status = task_status;
                        changed = true;
                    }
                    if snapshot.active_task_id.as_deref() != Some(task_id.as_str()) {
                        snapshot.active_task_id = Some(task_id.clone());
                        changed = true;
                    }
                    if snapshot.automation_agent_state != Some(AutomationAgentState::Working) {
                        snapshot.automation_agent_state = Some(AutomationAgentState::Working);
                        changed = true;
                    }
                    if snapshot.automation_session_status != Some(RootSessionStatus::Busy) {
                        snapshot.automation_session_status = Some(RootSessionStatus::Busy);
                        changed = true;
                    }
                    let automation_status = Some(format!("Resuming {task_id} in background"));
                    if snapshot.automation_status != automation_status {
                        snapshot.automation_status = automation_status;
                        changed = true;
                    }
                    changed
                });

                let resume_result = if let Some(resume_prompt) = resume_prompt.clone() {
                    if !resume_prompt.trim().is_empty() {
                        let Ok(workspace) = service.manager.get_workspace(&workspace_key) else {
                            return;
                        };
                        workspace.update(|snapshot| {
                            let task_state =
                                snapshot.task_states.entry(task_id.clone()).or_default();
                            if task_state.resume_prompt.as_deref() == Some(resume_prompt.as_str()) {
                                false
                            } else {
                                task_state.resume_prompt = Some(resume_prompt.clone());
                                true
                            }
                        });
                    }
                    if should_restart_codex_task_for_pr_request(task_runtime_snapshot(
                        &snapshot, &task_id,
                    )) {
                        service
                            .restart_task_session(
                                &workspace_key,
                                &snapshot,
                                &task_id,
                                &resume_prompt,
                            )
                            .await
                    } else {
                        service
                            .prompt_task_session(
                                &workspace_key,
                                &snapshot,
                                &task_id,
                                &resume_prompt,
                            )
                            .await
                    }
                } else if let Some(session_id) = attached_session.session_id.clone() {
                    let resume_prompt = read_last_codex_session_user_message(
                        service.workspace_directory_path().to_path_buf(),
                        workspace_key.clone(),
                        session_id.clone(),
                    )
                    .await
                    .unwrap_or_else(|| CODEX_AUTO_RESUME_PROMPT.to_string());
                    if !resume_prompt.trim().is_empty() {
                        let Ok(workspace) = service.manager.get_workspace(&workspace_key) else {
                            return;
                        };
                        workspace.update(|snapshot| {
                            let task_state =
                                snapshot.task_states.entry(task_id.clone()).or_default();
                            if task_state.resume_prompt.as_deref() == Some(resume_prompt.as_str()) {
                                false
                            } else {
                                task_state.resume_prompt = Some(resume_prompt.clone());
                                true
                            }
                        });
                    }
                    if should_restart_task_codex_after_attach(
                        Some(session_id.as_str()),
                        attached_session.fresh_codex_session,
                    ) {
                        service
                            .restart_task_session(
                                &workspace_key,
                                &snapshot,
                                &task_id,
                                &resume_prompt,
                            )
                            .await
                    } else {
                        service
                            .prompt_task_session(
                                &workspace_key,
                                &snapshot,
                                &task_id,
                                &resume_prompt,
                            )
                            .await
                    }
                } else {
                    service
                        .prompt_task_session(
                            &workspace_key,
                            &snapshot,
                            &task_id,
                            CODEX_AUTO_RESUME_PROMPT,
                        )
                        .await
                };
                tracing::info!(
                    workspace_key = %workspace_key,
                    task_id = %task_id,
                    resume_result = ?resume_result,
                    "finished deferred codex auto-resume attempt after attach"
                );
                return;
            }
        });
    }

    async fn maybe_resume_autonomous_codex_after_attach(&mut self, key: &str) -> bool {
        if self.service.agent_provider() != multicode_lib::services::AgentProvider::Codex {
            tracing::info!(
                workspace_key = %key,
                provider = ?self.service.agent_provider(),
                "skipping codex auto-resume because agent provider is not Codex"
            );
            self.attached_session = None;
            return false;
        }

        let attached_session = self.attached_session.clone();
        tracing::info!(
            workspace_key = %key,
            attached_session = ?attached_session,
            "evaluating codex auto-resume after attach"
        );
        for _ in 0..8 {
            self.sync_from_manager();
            let snapshot = self.snapshots.get(key).cloned();
            let Some(snapshot) = snapshot else {
                tracing::info!(
                    workspace_key = %key,
                    "workspace disappeared while evaluating codex auto-resume after attach"
                );
                self.attached_session = None;
                return false;
            };

            let effective_task_session_id = match attached_session.as_ref() {
                Some(AttachedSession {
                    workspace_key,
                    task_id: Some(task_id),
                    session_id,
                    fresh_codex_session,
                    ..
                }) if workspace_key == key => session_id.clone().or_else(|| {
                    if *fresh_codex_session {
                        task_runtime_snapshot(&snapshot, task_id)
                            .and_then(|task_state| task_state.session_id.clone())
                    } else {
                        None
                    }
                }),
                _ => None,
            };
            let resume_attached_session = attached_session.clone().map(|mut attached| {
                if attached.fresh_codex_session && attached.session_id.is_none() {
                    attached.session_id = effective_task_session_id.clone();
                }
                attached
            });

            let interrupted_resume_prompt = match attached_session.as_ref() {
                Some(AttachedSession {
                    workspace_key,
                    task_id: Some(_),
                    initial_turn_metrics,
                    ..
                }) if workspace_key == key => {
                    let should_resume_interrupted = self
                        .should_resume_interrupted_task_codex_after_attach(
                            &snapshot,
                            workspace_key,
                            effective_task_session_id.as_deref(),
                            *initial_turn_metrics,
                        )
                        .await;
                    if should_resume_interrupted {
                        if let Some(session_id) = effective_task_session_id.clone() {
                            read_interrupted_codex_resume_prompt(
                                self.service.workspace_directory_path().to_path_buf(),
                                workspace_key.clone(),
                                session_id,
                            )
                            .await
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };

            let should_resume = match attached_session.as_ref() {
                Some(AttachedSession {
                    workspace_key,
                    task_id: Some(task_id),
                    initial_agent_state,
                    ..
                }) if workspace_key == key => {
                    should_auto_resume_task_codex_after_attach(
                        task_runtime_snapshot(&snapshot, task_id),
                        effective_task_session_id.as_deref(),
                        *initial_agent_state,
                    ) || interrupted_resume_prompt.is_some()
                }
                _ => should_auto_resume_autonomous_codex_after_attach(&snapshot),
            };
            tracing::info!(
                workspace_key = %key,
                should_resume,
                automation_agent_state = ?snapshot.automation_agent_state,
                automation_session_status = ?snapshot.automation_session_status,
                root_session_status = ?snapshot.root_session_status,
                "evaluated codex auto-resume after attach"
            );

            if should_resume {
                if let Some(AttachedSession {
                    workspace_key,
                    task_id: Some(task_id),
                    ..
                }) = attached_session.as_ref()
                    && workspace_key == key
                {
                    if should_queue_task_codex_resume_until_vm_available(&snapshot, task_id) {
                        if let Some(resume_prompt) = interrupted_resume_prompt.as_deref() {
                            self.persist_task_resume_prompt(key, task_id, resume_prompt);
                        }
                        self.mark_task_waiting_on_vm_after_attach(key, task_id);
                        self.queue_task_codex_resume_until_vm_available(
                            key.to_string(),
                            task_id.clone(),
                            resume_attached_session
                                .clone()
                                .expect("attached task session should exist"),
                            interrupted_resume_prompt.clone(),
                        );
                        self.status = format!(
                            "Detached from workspace '{key}'; task {task_id} is queued until the VM is free"
                        );
                        self.attached_session = None;
                        return true;
                    }
                    self.mark_task_resuming_in_background(key, task_id);
                    self.sync_from_manager();
                }
                let service = self.service.clone();
                let workspace_key = key.to_string();
                let snapshot_for_resume = snapshot.clone();
                let attached_session_for_resume = resume_attached_session.clone();
                tokio::spawn(async move {
                    let resume_result = match attached_session_for_resume.as_ref() {
                        Some(AttachedSession {
                            workspace_key: attached_workspace_key,
                            task_id: Some(task_id),
                            session_id: Some(session_id),
                            fresh_codex_session,
                            ..
                        }) if attached_workspace_key == &workspace_key => {
                            if let Some(resume_prompt) = interrupted_resume_prompt.clone() {
                                if !resume_prompt.trim().is_empty() {
                                    let Ok(workspace) =
                                        service.manager.get_workspace(&workspace_key)
                                    else {
                                        return;
                                    };
                                    workspace.update(|snapshot| {
                                        let task_state = snapshot
                                            .task_states
                                            .entry(task_id.clone())
                                            .or_default();
                                        if task_state.resume_prompt.as_deref()
                                            == Some(resume_prompt.as_str())
                                        {
                                            false
                                        } else {
                                            task_state.resume_prompt = Some(resume_prompt.clone());
                                            true
                                        }
                                    });
                                }
                                service
                                    .restart_task_session(
                                        &workspace_key,
                                        &snapshot_for_resume,
                                        task_id,
                                        &resume_prompt,
                                    )
                                    .await
                            } else {
                                let resume_prompt = read_last_codex_session_user_message(
                                    service.workspace_directory_path().to_path_buf(),
                                    workspace_key.clone(),
                                    session_id.clone(),
                                )
                                .await
                                .unwrap_or_else(|| CODEX_AUTO_RESUME_PROMPT.to_string());
                                if !resume_prompt.trim().is_empty() {
                                    let Ok(workspace) =
                                        service.manager.get_workspace(&workspace_key)
                                    else {
                                        return;
                                    };
                                    workspace.update(|snapshot| {
                                        let task_state = snapshot
                                            .task_states
                                            .entry(task_id.clone())
                                            .or_default();
                                        if task_state.resume_prompt.as_deref()
                                            == Some(resume_prompt.as_str())
                                        {
                                            false
                                        } else {
                                            task_state.resume_prompt = Some(resume_prompt.clone());
                                            true
                                        }
                                    });
                                }
                                if should_restart_task_codex_after_attach(
                                    Some(session_id.as_str()),
                                    *fresh_codex_session,
                                ) {
                                    service
                                        .restart_task_session(
                                            &workspace_key,
                                            &snapshot_for_resume,
                                            task_id,
                                            &resume_prompt,
                                        )
                                        .await
                                } else {
                                    service
                                        .prompt_task_session(
                                            &workspace_key,
                                            &snapshot_for_resume,
                                            task_id,
                                            &resume_prompt,
                                        )
                                        .await
                                }
                            }
                        }
                        _ => {
                            service
                                .prompt_root_session(&snapshot_for_resume, CODEX_AUTO_RESUME_PROMPT)
                                .await
                        }
                    };
                    tracing::info!(
                        workspace_key = %workspace_key,
                        resume_result = ?resume_result,
                        "finished codex auto-resume attempt after attach"
                    );
                });
                self.status = format!(
                    "Detached from workspace '{key}'; autonomous Codex resume was scheduled in the background"
                );
                self.attached_session = None;
                return true;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        self.attached_session = None;
        false
    }

    async fn should_resume_interrupted_task_codex_after_attach(
        &self,
        snapshot: &WorkspaceSnapshot,
        workspace_key: &str,
        session_id: Option<&str>,
        initial_turn_metrics: Option<CodexSessionTurnMetrics>,
    ) -> bool {
        let Some(session_id) = session_id else {
            return false;
        };
        let current_turn_metrics = read_codex_session_turn_metrics(
            self.service.workspace_directory_path(),
            workspace_key,
            session_id,
        );
        let current_thread_status = match snapshot.transient.as_ref() {
            Some(transient) => CodexAppServerClient::new(transient.uri.clone())
                .thread_read(session_id)
                .await
                .ok()
                .and_then(|response| response.thread.status),
            None => None,
        };
        let should_resume = should_resume_codex_task_after_incomplete_attached_turn(
            initial_turn_metrics,
            current_turn_metrics,
            current_thread_status.as_ref(),
        );
        tracing::info!(
            workspace_key = %workspace_key,
            session_id,
            initial_turn_metrics = ?initial_turn_metrics,
            current_turn_metrics = ?current_turn_metrics,
            current_thread_status = ?current_thread_status,
            should_resume,
            "evaluated interrupted codex task resume after attach"
        );
        should_resume
    }

    pub(crate) fn poll_running_prompt_tool(&mut self) {
        let completion = match self.running_operation.as_mut() {
            Some(running_tool) => match running_tool.result_rx.try_recv() {
                Ok(result) => Some(result),
                Err(oneshot::error::TryRecvError::Empty) => None,
                Err(oneshot::error::TryRecvError::Closed) => {
                    Some(Err("operation task ended unexpectedly".to_string()))
                }
            },
            None => None,
        };

        if let Some(result) = completion
            && let Some(running_tool) = self.running_operation.take()
        {
            match result {
                Ok(()) => match running_tool.completion_action {
                    RunningOperationCompletionAction::None => {
                        self.mode = UiMode::Normal;
                        self.status = running_tool.success_status.unwrap_or_else(|| {
                            format!(
                                "{} completed for workspace '{}'",
                                running_tool.operation_name, running_tool.workspace_key
                            )
                        });
                    }
                    RunningOperationCompletionAction::WaitForWorkspaceStart {
                        attach_when_ready,
                    } => {
                        self.mode = UiMode::StartingModal;
                        self.starting_workspace_key = Some(running_tool.workspace_key.clone());
                        self.starting_attach_when_ready = attach_when_ready;
                        self.started_wait_since = None;
                        self.status = if attach_when_ready {
                            format!(
                                "Starting workspace '{}' before attaching",
                                running_tool.workspace_key
                            )
                        } else {
                            format!(
                                "Waiting for workspace '{}' to become ready",
                                running_tool.workspace_key
                            )
                        };
                    }
                },
                Err(err) => {
                    self.mode = UiMode::Normal;
                    self.status = format!(
                        "{} failed for workspace '{}': {err}",
                        running_tool.operation_name, running_tool.workspace_key
                    );
                }
            }
        }
    }

    pub(crate) fn running_tool_progress(&self) -> Option<(&str, String)> {
        self.running_operation.as_ref().map(|running_tool| {
            (
                running_tool.operation_name.as_str(),
                running_tool.progress_rx.borrow().clone(),
            )
        })
    }

    pub(crate) fn running_operation_is_cancellable(&self) -> bool {
        self.running_operation
            .as_ref()
            .and_then(|operation| operation.cancel.as_ref())
            .is_some()
    }

    fn handle_tool_progress_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Esc {
            return;
        }
        if !self.running_operation_is_cancellable() {
            if let Some(operation) = self.running_operation.as_ref() {
                self.status = format!(
                    "{} is still running for workspace '{}'",
                    operation.operation_name, operation.workspace_key
                );
            }
            return;
        }
        let Some(mut operation) = self.running_operation.take() else {
            self.mode = UiMode::Normal;
            return;
        };
        if let Some(cancel) = operation.cancel.take() {
            cancel.abort();
        }
        self.mode = UiMode::Normal;
        self.status = format!(
            "{} cancelled for workspace '{}'",
            operation.operation_name, operation.workspace_key
        );
    }

    async fn run_tool_for_key(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        key_char: char,
    ) {
        let Some(workspace_key) = self.selected_workspace_key().map(str::to_string) else {
            return;
        };
        let Some(snapshot) = self.snapshots.get(&workspace_key).cloned() else {
            return;
        };
        let Some(tool) = find_tool_for_key(&self.service.config.tool, key_char) else {
            return;
        };
        let selected_repo_path = self.selected_workspace_repo_path();
        let selected_task = self.selected_task_id().is_some();

        if !tool_is_usable(&tool, &snapshot)
            || (selected_task && !matches!(tool.type_, ToolType::Exec))
            || (selected_task && selected_repo_path.is_none())
            || (!selected_task && !matches!(tool.type_, ToolType::Prompt))
        {
            self.status = format!(
                "Tool '{}' is unavailable for workspace '{}' in its current state",
                tool.name, workspace_key
            );
            return;
        }

        match tool.type_ {
            ToolType::Exec => {
                let Some(exec_command) = tool.exec.as_deref().map(str::trim) else {
                    self.status = format!("Tool '{}' is missing its exec command", tool.name);
                    return;
                };
                self.run_exec_tool(
                    terminal,
                    &workspace_key,
                    &tool.name,
                    exec_command,
                    selected_repo_path.as_deref(),
                )
                .await;
            }
            ToolType::Prompt => {
                let Some(prompt) = tool.prompt.as_deref().map(str::trim) else {
                    self.status = format!("Tool '{}' is missing its prompt", tool.name);
                    return;
                };
                self.start_prompt_tool(&workspace_key, &snapshot, &tool.name, prompt)
                    .await;
            }
        }
    }

    async fn run_exec_tool(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        workspace_key: &str,
        tool_name: &str,
        exec_command: &str,
        repo_path: Option<&Path>,
    ) {
        let result = if let Some(repo_path) = repo_path {
            self.run_repo_shell_command_in_pty(terminal, workspace_key, repo_path, exec_command)
                .await
        } else {
            let exec_command = match self
                .service
                .build_exec_tool_command(workspace_key, exec_command)
                .await
            {
                Ok(command) => command,
                Err(err) => {
                    self.status = format!("Failed to prepare exec tool '{}': {err:?}", tool_name);
                    return;
                }
            };

            let inherited_env = exec_command.inherited_env;
            let tmux_command = std::iter::once(exec_command.program)
                .chain(exec_command.args)
                .collect::<Vec<_>>();
            let custom_description = self
                .snapshots
                .get(workspace_key)
                .map(|snapshot| snapshot.persistent.description.clone())
                .unwrap_or_default();
            tracing::info!(
                workspace_key = workspace_key,
                tool_name = tool_name,
                command = %format_command_line("tmux", &{
                    let mut debug_command = vec![
                        "new-session".to_string(),
                        "-d".to_string(),
                        "-s".to_string(),
                        "<session>".to_string(),
                        "env".to_string(),
                    ];
                    debug_command.extend(
                        inherited_env
                            .iter()
                            .map(|(name, value)| format!("{name}={value}")),
                    );
                    debug_command.extend(tmux_command.clone());
                    debug_command
                }),
                "launching exec tool tmux command"
            );
            run_tmux_new_session_command(
                terminal,
                &inherited_env,
                tmux_command,
                None,
                workspace_key,
                &custom_description,
            )
            .await
        };

        self.status = match result {
            Ok(()) => format!(
                "Tool '{}' finished for workspace '{}'",
                tool_name, workspace_key
            ),
            Err(err) => format!("Failed to run tool '{}': {err}", tool_name),
        };
    }

    async fn run_repo_shell_command_in_pty(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        workspace_key: &str,
        repo_path: &Path,
        shell_command: &str,
    ) -> io::Result<()> {
        let repo_dir = repo_path.to_string_lossy().into_owned();
        let command = self
            .service
            .build_pty_tool_command(
                workspace_key,
                vec![
                    "/bin/sh".to_string(),
                    "-lc".to_string(),
                    shell_command_in_repo(&repo_dir, shell_command),
                ],
            )
            .await
            .map_err(|err| io::Error::other(format!("failed to prepare PTY command: {err:?}")))?;

        let inherited_env = command.inherited_env;
        let tmux_command = std::iter::once(command.program)
            .chain(command.args)
            .collect::<Vec<_>>();
        let custom_description = self
            .snapshots
            .get(workspace_key)
            .map(|snapshot| snapshot.persistent.description.clone())
            .unwrap_or_default();
        run_tmux_new_session_command(
            terminal,
            &inherited_env,
            tmux_command,
            None,
            workspace_key,
            &custom_description,
        )
        .await
    }

    async fn open_selected_workspace_in_editor(&mut self) {
        if let Some(key) = self.selected_workspace_key().map(str::to_string) {
            let Some(snapshot) = self.snapshots.get(&key) else {
                return;
            };
            if !workspace_is_usable(snapshot) {
                self.status = format!("Workspace '{key}' is archived and cannot be opened");
                return;
            }
            let Some(repo_path) = self.selected_workspace_repo_path() else {
                self.status = format!("Workspace '{key}' does not have a repository to edit");
                return;
            };

            let compare_tool_name = compare_tool_name(self.service.config.compare.tool);
            match compare_open_repo_command(&self.service.config.compare, &repo_path) {
                Ok((program, args)) => {
                    tracing::info!(
                        command = %format_command_line(&program, &args),
                        workspace = %key,
                        repo = %repo_path.display(),
                        compare_tool = compare_tool_name,
                        "opening workspace repository in editor"
                    );
                    let mut command = Command::new(&program);
                    match command
                        .args(&args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        Ok(_) => {
                            self.status =
                                format!("Opened workspace '{key}' in {compare_tool_name}");
                        }
                        Err(err) => {
                            self.status = format!(
                                "Failed to open workspace '{key}' in {compare_tool_name}: {err}"
                            );
                        }
                    }
                }
                Err(err) => {
                    self.status =
                        format!("Failed to open workspace '{key}' in {compare_tool_name}: {err}");
                }
            }
        }
    }

    async fn open_selected_workspace_diff_in_terminal(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) {
        let Some(key) = self.selected_workspace_key().map(str::to_string) else {
            return;
        };
        let Some(snapshot) = self.snapshots.get(&key) else {
            return;
        };
        if !workspace_is_usable(snapshot) {
            self.status = format!("Workspace '{key}' is archived and cannot be compared");
            return;
        }
        let Some(repo_path) = self.selected_workspace_repo_path() else {
            self.status = format!("Workspace '{key}' does not have a repository to compare");
            return;
        };

        let diff_command = repository_diff_shell_command();
        self.status = match self
            .run_repo_shell_command_in_pty(terminal, &key, &repo_path, diff_command)
            .await
        {
            Ok(()) => format!("Compared workspace '{key}'"),
            Err(err) => format!("Failed to compare workspace '{key}': {err}"),
        };
    }

    async fn run_review_handler_in_pty(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        workspace_key: &str,
        repo_dir: &str,
        program: &str,
        args: &[String],
    ) -> io::Result<()> {
        let command = self
            .service
            .build_pty_tool_command(
                workspace_key,
                vec![
                    "/bin/sh".to_string(),
                    "-lc".to_string(),
                    format!(
                        "cd -- {} && exec {}",
                        shell_escape_arg(repo_dir),
                        format_command_line(program, args)
                    ),
                ],
            )
            .await
            .map_err(|err| {
                io::Error::other(format!("failed to prepare PTY review handler: {err:?}"))
            })?;

        let inherited_env = command.inherited_env;
        let tmux_command = std::iter::once(command.program)
            .chain(command.args)
            .collect::<Vec<_>>();
        let custom_description = self
            .snapshots
            .get(workspace_key)
            .map(|snapshot| snapshot.persistent.description.clone())
            .unwrap_or_default();
        run_tmux_new_session_command(
            terminal,
            &inherited_env,
            tmux_command,
            None,
            workspace_key,
            &custom_description,
        )
        .await
    }

    async fn start_prompt_tool(
        &mut self,
        workspace_key: &str,
        snapshot: &WorkspaceSnapshot,
        tool_name: &str,
        prompt: &str,
    ) {
        let Some(root_session_id) = snapshot.root_session_id.clone() else {
            self.status = format!(
                "Tool '{}' requires a known root session ID for workspace '{}'",
                tool_name, workspace_key
            );
            return;
        };
        let workspace_key_owned = workspace_key.to_string();
        let tool_name_owned = tool_name.to_string();
        let prompt_text = prompt.to_string();

        let (progress_tx, progress_rx) =
            watch::channel(format!("Preparing tool '{}'...", tool_name_owned));
        let (result_tx, result_rx) = oneshot::channel();
        let service = self.service.clone();
        let snapshot = snapshot.clone();
        let task_tool_name = tool_name_owned.clone();

        tokio::spawn(async move {
            let result =
                if service.agent_provider() == multicode_lib::services::AgentProvider::Opencode {
                    let Some(opencode_client) = snapshot.opencode_client.as_ref() else {
                        let _ = result_tx
                            .send(Err("workspace has no healthy opencode client".to_string()));
                        return;
                    };
                    run_prompt_tool_workflow(
                        opencode_client.client.clone(),
                        opencode_client.events.clone(),
                        root_session_id,
                        prompt_text,
                        progress_tx,
                    )
                    .await
                } else {
                    let _ = progress_tx.send(format!("Starting tool '{}'...", task_tool_name));
                    service
                        .prompt_root_session(&snapshot, &prompt_text)
                        .await
                        .map_err(|err| err.to_string())
                };
            let _ = result_tx.send(result);
        });

        self.running_operation = Some(RunningOperation {
            workspace_key: workspace_key_owned,
            operation_name: tool_name_owned.clone(),
            success_status: None,
            progress_rx,
            result_rx,
            completion_action: RunningOperationCompletionAction::None,
            cancel: None,
        });
        self.mode = UiMode::ToolProgressModal;
        self.status = format!(
            "Running tool '{}' for workspace '{}'",
            tool_name_owned, workspace_key
        );
    }

    async fn run_workspace_link_handler(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        workspace_key: &str,
        link: &WorkspaceLink,
    ) {
        if self
            .snapshots
            .get(workspace_key)
            .is_some_and(|snapshot| snapshot.persistent.archived)
        {
            self.status = format!(
                "{} link for workspace '{}' is unavailable while archived",
                link.label(),
                workspace_key
            );
            return;
        }

        let targets = self.selected_workspace_link_targets();
        let Some((_, argument)) = targets.get(self.selected_link_target_index).cloned() else {
            self.status = format!(
                "{} link for workspace '{}' is still validating or invalid",
                link.label(),
                workspace_key
            );
            return;
        };

        let argument_mode = match link.kind {
            WorkspaceLinkKind::Review => multicode_lib::HandlerArgumentMode::Chdir,
            _ => multicode_lib::HandlerArgumentMode::Argument,
        };
        let (program, args) = match build_handler_command(
            link.handler_template(&self.service),
            argument_mode,
            &argument,
        ) {
            Ok(command) => command,
            Err(err) => {
                self.status = format!(
                    "Invalid {} handler configuration for workspace '{}': {err}",
                    link.label(),
                    workspace_key
                );
                return;
            }
        };

        let pty_review =
            link.kind == WorkspaceLinkKind::Review && self.service.config.handler.review_pty;
        let result = if pty_review {
            self.run_review_handler_in_pty(terminal, workspace_key, &argument, &program, &args)
                .await
        } else {
            dispatch_handler_action(
                self.relay_socket.as_deref(),
                &program,
                &args,
                link,
                &argument,
            )
            .await
        };

        match result {
            Ok(_) => {
                self.status = format!(
                    "Opened {} link for workspace '{}'",
                    link.label(),
                    workspace_key
                );
            }
            Err(err) => {
                self.status = format!(
                    "Failed to open {} link for workspace '{}': {err}",
                    link.label(),
                    workspace_key
                );
            }
        }
    }

    async fn open_selected_github_target(&mut self) {
        let Some(workspace_key) = self.selected_workspace_key().map(str::to_string) else {
            return;
        };
        let Some(snapshot) = self.snapshots.get(&workspace_key) else {
            return;
        };
        if snapshot.persistent.archived {
            self.status = format!(
                "GitHub links for workspace '{}' are unavailable while archived",
                workspace_key
            );
            return;
        }

        let (target_description, url) = if let Some(link) = self.selected_workspace_link() {
            if link.value.is_empty()
                || !matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr)
            {
                return;
            }
            let targets = self.selected_workspace_link_targets();
            let Some((_, argument)) = targets.get(self.selected_link_target_index) else {
                self.status = format!(
                    "{} link for workspace '{}' is still validating or invalid",
                    link.label(),
                    workspace_key
                );
                return;
            };
            ("GitHub link", argument.clone())
        } else if let Some(url) = self.selected_task_default_github_url() {
            ("GitHub issue", url)
        } else {
            let Some(url) = self.selected_workspace_github_repository_url() else {
                return;
            };
            ("GitHub repository", url)
        };

        let (program, args) = match build_handler_command(
            &self.service.config.handler.web,
            multicode_lib::HandlerArgumentMode::Argument,
            &url,
        ) {
            Ok(command) => command,
            Err(err) => {
                self.status = format!(
                    "Invalid web handler configuration for workspace '{}': {err}",
                    workspace_key
                );
                return;
            }
        };

        let result =
            dispatch_web_handler_action(self.relay_socket.as_deref(), &program, &args, &url).await;

        match result {
            Ok(()) => {
                self.status =
                    format!("Opened {target_description} for workspace '{workspace_key}'");
            }
            Err(err) => {
                self.status = format!(
                    "Failed to open {target_description} for workspace '{workspace_key}': {err}"
                );
            }
        }
    }

    async fn handle_normal_key(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        key: KeyEvent,
    ) {
        let link_selected = self.selected_link_index.is_some();
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Up => {
                if link_selected {
                    self.move_selected_link_target_up();
                } else if self.selected_row > 0 {
                    self.selected_row -= 1;
                    self.selected_link_index = None;
                    self.selected_link_target_index = 0;
                }
            }
            KeyCode::Down => {
                if link_selected {
                    self.move_selected_link_target_down();
                } else if self.selected_row + 1 < self.table_entries().len() {
                    self.selected_row += 1;
                    self.selected_link_index = None;
                    self.selected_link_target_index = 0;
                }
            }
            KeyCode::Left => {
                if self.selected_row > 0 {
                    self.move_selected_link_left();
                }
            }
            KeyCode::Right => {
                if self.selected_row > 0 {
                    self.move_selected_link_right();
                }
            }
            KeyCode::Tab => {
                if self.selected_row > 0 {
                    self.move_selected_link_right();
                }
            }
            KeyCode::Esc => {
                self.selected_link_index = None;
                self.selected_link_target_index = 0;
            }
            KeyCode::Enter if self.selected_row == 0 => {
                self.create_input.clear();
                self.repository_input.clear();
                self.create_field = CreateModalField::Key;
                self.mode = UiMode::CreateModal;
            }
            KeyCode::Enter => {
                if let Some(key) = self.selected_workspace_key().map(str::to_string) {
                    if let Some(link) = self.selected_workspace_link() {
                        if link.value.is_empty() {
                            self.open_add_custom_link_modal(link.kind);
                            return;
                        }
                        self.run_workspace_link_handler(terminal, &key, &link).await;
                        return;
                    }
                    let state = self.snapshots.get(&key).map(workspace_state);
                    match state {
                        Some(WorkspaceUiState::Started) => {
                            if self
                                .snapshots
                                .get(&key)
                                .is_some_and(|snapshot| snapshot.persistent.archived)
                            {
                                self.status =
                                    format!("Workspace '{key}' is archived and cannot be attached");
                                return;
                            }
                            match self.snapshot_attach_target(&key) {
                                Ok(target) => {
                                    self.record_attached_session(&key, &target);
                                    let custom_description = self
                                        .snapshots
                                        .get(&key)
                                        .map(|snapshot| snapshot.persistent.description.clone())
                                        .unwrap_or_default();
                                    match attach_in_tmux(
                                        terminal,
                                        self.service.agent_command(),
                                        &target,
                                        self.attach_cwd_for_workspace(&key).as_deref(),
                                        &self.attach_env_for_workspace(&key),
                                        &key,
                                        &custom_description,
                                    )
                                    .await
                                    {
                                        Ok(_) => {
                                            if !self
                                                .retry_codex_task_attach_with_fresh_session(
                                                    terminal, &key,
                                                )
                                                .await
                                                && !self
                                                    .retry_codex_task_attach_with_last_thread(
                                                        terminal, &key,
                                                    )
                                                    .await
                                            {
                                                self.handle_attach_exit(&key).await;
                                            }
                                        }
                                        Err(err) => {
                                            tracing::warn!(
                                                workspace_key = %key,
                                                error = %err,
                                                "attach session exited with error"
                                            );
                                            if !self
                                                .retry_codex_task_attach_with_fresh_session(
                                                    terminal, &key,
                                                )
                                                .await
                                                && !self
                                                    .retry_codex_task_attach_with_last_thread(
                                                        terminal, &key,
                                                    )
                                                    .await
                                                && !self
                                                    .handle_attach_exit_after_error(&key, &err)
                                                    .await
                                            {
                                                self.status = format!(
                                                    "Failed to attach to workspace '{key}': {err}"
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(err) => {
                                    self.status =
                                        format!("Failed to attach to workspace '{key}': {err}")
                                }
                            }
                        }
                        Some(WorkspaceUiState::Starting) => {
                            self.status = format!(
                                "Workspace '{key}' is still starting; wait until it is Started"
                            );
                        }
                        Some(WorkspaceUiState::Stopped) => {
                            if self
                                .snapshots
                                .get(&key)
                                .is_some_and(|snapshot| snapshot.persistent.archived)
                            {
                                self.status =
                                    format!("Workspace '{key}' is archived and cannot be started");
                                return;
                            }
                            self.start_workspace_operation(key.clone(), true);
                        }
                        None => {}
                    }
                }
            }
            KeyCode::Char('a') => {
                if link_selected {
                    if let Some(kind) = self.can_add_custom_link_for_selected_kind() {
                        self.open_add_custom_link_modal(kind);
                    }
                    return;
                }
                if self.selected_task_id().is_some() {
                    self.approve_selected_task_for_pr_creation().await;
                    return;
                }
                if let Some(key) = self.selected_workspace_key().map(str::to_string) {
                    let archived = self
                        .snapshots
                        .get(&key)
                        .map(|snapshot| snapshot.persistent.archived)
                        .unwrap_or(false);
                    let operation_name = if archived { "Unarchive" } else { "Archive" };
                    let (progress_tx, progress_rx) = watch::channel(format!(
                        "Preparing to {} workspace...",
                        operation_name.to_lowercase()
                    ));
                    let service = self.service.clone();
                    let workspace_key = key.clone();
                    let (result_tx, result_rx) = oneshot::channel();
                    let join_handle = tokio::spawn(async move {
                        let result = if archived {
                            service
                                .unarchive_workspace(&workspace_key, progress_tx)
                                .await
                        } else {
                            service.archive_workspace(&workspace_key, progress_tx).await
                        }
                        .map_err(|err| format!("{err:?}"));
                        let _ = result_tx.send(result);
                    });
                    self.running_operation = Some(RunningOperation {
                        workspace_key: key.clone(),
                        operation_name: operation_name.to_string(),
                        success_status: None,
                        progress_rx,
                        result_rx,
                        completion_action: RunningOperationCompletionAction::None,
                        cancel: Some(join_handle.abort_handle()),
                    });
                    self.mode = UiMode::ToolProgressModal;
                    self.status = format!("{} workspace '{}'", operation_name, key);
                }
            }
            KeyCode::Char('f') => {
                if link_selected {
                    return;
                }
                if self.selected_task_id().is_none() {
                    return;
                }
                if !self.selected_task_can_request_ci_fix() {
                    self.status = "Fix CI is unavailable for the selected task".to_string();
                    return;
                }
                self.fix_selected_task_ci().await;
            }
            KeyCode::Char('c') => {
                if link_selected {
                    return;
                }
                if self.selected_task_id().is_none() {
                    return;
                }
                self.open_selected_workspace_diff_in_terminal(terminal)
                    .await;
            }
            KeyCode::Char('e') => {
                if link_selected {
                    return;
                }
                if self.selected_task_id().is_none() {
                    return;
                }
                self.open_selected_workspace_in_editor().await;
            }
            KeyCode::Char('d') => {
                if link_selected {
                    if let Some(link) = self
                        .selected_workspace_link()
                        .filter(WorkspaceLink::is_custom)
                    {
                        self.open_edit_custom_link_modal(&link);
                    }
                    return;
                }
                if self.selected_task_id().is_some() {
                    return;
                }
                if let Some(key) = self.selected_workspace_key() {
                    let current = self
                        .snapshots
                        .get(key)
                        .map(|snapshot| snapshot.persistent.description.clone())
                        .unwrap_or_default();
                    self.edit_input = current;
                    self.mode = UiMode::EditDescription;
                }
            }
            KeyCode::Char('i') => {
                if link_selected {
                    return;
                }
                if self.selected_task_id().is_some() {
                    return;
                }
                if let Some(key) = self.selected_workspace_key() {
                    let Some(snapshot) = self.snapshots.get(key) else {
                        return;
                    };
                    if !workspace_is_usable(snapshot) {
                        return;
                    }
                    if snapshot.persistent.assigned_repository.is_none() {
                        return;
                    }
                    self.issue_input.clear();
                    self.mode = UiMode::EditIssue;
                }
            }
            KeyCode::Char('n') => {
                if link_selected {
                    return;
                }
                if self.selected_task_id().is_some() {
                    return;
                }
                self.request_selected_workspace_queue_next_issue();
            }
            KeyCode::Char('o') => {
                self.open_selected_github_target().await;
            }
            KeyCode::Char('s') => {
                if link_selected {
                    return;
                }
                if self.selected_task_id().is_some() {
                    return;
                }
                if let Some(key) = self.selected_workspace_key().map(str::to_string) {
                    let state = self.snapshots.get(&key).map(workspace_state);
                    match state {
                        Some(WorkspaceUiState::Stopped) => {
                            if self
                                .snapshots
                                .get(&key)
                                .is_some_and(|snapshot| snapshot.persistent.archived)
                            {
                                self.status =
                                    format!("Workspace '{key}' is archived and cannot be started");
                                return;
                            }
                            self.start_workspace_operation(key, false);
                        }
                        Some(WorkspaceUiState::Starting) | Some(WorkspaceUiState::Started) => {
                            self.start_stop_workspace_operation(key);
                        }
                        None => {}
                    }
                }
            }
            KeyCode::Char('p') => {
                if link_selected {
                    return;
                }
                if self.selected_task_id().is_some() {
                    return;
                }
                if let Some(key) = self.selected_workspace_key().map(str::to_string) {
                    let Some(snapshot) = self.snapshots.get(&key).cloned() else {
                        return;
                    };
                    if !workspace_supports_pause(&snapshot) {
                        return;
                    }
                    if snapshot.persistent.automation_paused {
                        match self.service.resume_workspace(&key) {
                            Ok(()) => {
                                self.status =
                                    format!("Resumed autonomous work for workspace '{key}'")
                            }
                            Err(err) => {
                                self.status = format!(
                                    "Failed to resume autonomous work for workspace '{key}': {err:?}"
                                )
                            }
                        }
                    } else {
                        match self.service.pause_workspace(&key).await {
                            Ok(()) => {
                                self.status =
                                    format!("Paused autonomous work for workspace '{key}'")
                            }
                            Err(err) => {
                                self.status = format!(
                                    "Failed to pause autonomous work for workspace '{key}': {err:?}"
                                )
                            }
                        }
                    }
                }
            }
            KeyCode::Char('r') => {
                if link_selected {
                    return;
                }
                if self.selected_task_id().is_some() {
                    return;
                }
                self.request_selected_workspace_github_status_refresh();
            }
            KeyCode::Char('x') => {
                if link_selected {
                    return;
                }
                match self.selected_entry() {
                    Some(TableEntry::Workspace { workspace_key }) => {
                        self.pending_delete_target =
                            Some(PendingDeleteTarget::Workspace { workspace_key });
                        self.pending_task_removal_action = TaskRemovalAction::default();
                        self.mode = UiMode::ConfirmDelete;
                    }
                    Some(TableEntry::Task {
                        workspace_key,
                        task_id,
                    }) => {
                        self.pending_delete_target = Some(PendingDeleteTarget::Task {
                            workspace_key,
                            task_id,
                        });
                        self.pending_task_removal_action = TaskRemovalAction::default();
                        self.mode = UiMode::ConfirmTaskRemoval;
                    }
                    _ => {}
                }
            }
            KeyCode::Char(ch) => {
                if link_selected {
                    return;
                }
                self.run_tool_for_key(terminal, ch).await;
            }
            _ => {}
        }
    }

    async fn handle_create_modal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = UiMode::Normal;
                self.create_input.clear();
                self.repository_input.clear();
                self.create_field = CreateModalField::Key;
            }
            KeyCode::Backspace => {
                self.active_create_modal_input_mut().pop();
            }
            KeyCode::Char(ch) => {
                self.active_create_modal_input_mut().push(ch);
            }
            KeyCode::Tab | KeyCode::Down => {
                self.create_field = CreateModalField::Repository;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.create_field = CreateModalField::Key;
            }
            KeyCode::Enter => {
                let key = self.create_input.trim().to_string();
                let repository = self.repository_input.trim().to_string();
                if key.is_empty() {
                    self.status = "Workspace key cannot be empty".to_string();
                    self.create_field = CreateModalField::Key;
                    return;
                }
                if repository.is_empty() {
                    self.status = "Repository cannot be empty".to_string();
                    self.create_field = CreateModalField::Repository;
                    return;
                }

                match self
                    .service
                    .create_workspace_with_repository(&key, &repository)
                    .await
                {
                    Ok(normalized_repository) => {
                        self.status = format!(
                            "Created workspace '{key}' for repository '{normalized_repository}'"
                        );
                        self.mode = UiMode::Normal;
                        self.create_input.clear();
                        self.repository_input.clear();
                        self.create_field = CreateModalField::Key;
                        self.sync_from_manager();
                        if let Some(position) =
                            self.ordered_keys.iter().position(|item| item == &key)
                        {
                            self.selected_row = position + 1;
                        }
                    }
                    Err(err) => {
                        self.status = format!("Failed to create workspace: {}", err.summary());
                    }
                }
            }
            _ => {}
        }
    }

    fn active_create_modal_input_mut(&mut self) -> &mut String {
        match self.create_field {
            CreateModalField::Key => &mut self.create_input,
            CreateModalField::Repository => &mut self.repository_input,
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = UiMode::Normal;
                self.edit_input.clear();
            }
            KeyCode::Backspace => {
                self.edit_input.pop();
            }
            KeyCode::Char(ch) => {
                self.edit_input.push(ch);
            }
            KeyCode::Enter => {
                if let Some(key) = self.selected_workspace_key().map(str::to_string) {
                    let description = self.edit_input.clone();
                    match self.service.manager.get_workspace(&key) {
                        Ok(workspace) => {
                            workspace.update(|snapshot| {
                                if snapshot.persistent.description != description {
                                    snapshot.persistent.description = description.clone();
                                    true
                                } else {
                                    false
                                }
                            });
                            self.status = format!("Updated description for '{key}'");
                            self.mode = UiMode::Normal;
                            self.edit_input.clear();
                        }
                        Err(err) => {
                            self.status = format!("Failed to update description: {err:?}");
                        }
                    }
                }
            }
            _ => {}
        }
    }

    async fn handle_issue_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = UiMode::Normal;
                self.issue_input.clear();
            }
            KeyCode::Backspace => {
                self.issue_input.pop();
            }
            KeyCode::Char(ch) => {
                self.issue_input.push(ch);
            }
            KeyCode::Enter => {
                let Some(key) = self.selected_workspace_key().map(str::to_string) else {
                    return;
                };
                let issue = self.issue_input.trim().to_string();
                let issue = (!issue.is_empty()).then_some(issue);
                match self
                    .service
                    .assign_workspace_issue(&key, issue.as_deref())
                    .await
                {
                    Ok(Some(normalized)) => {
                        self.status = format!("Queued issue '{normalized}' for workspace '{key}'");
                        self.mode = UiMode::Normal;
                        self.issue_input.clear();
                    }
                    Ok(None) => {
                        self.status = format!("No issue queued for workspace '{key}'");
                        self.mode = UiMode::Normal;
                        self.issue_input.clear();
                    }
                    Err(err) => {
                        self.status = format!("Failed to update issue assignment: {err:?}");
                    }
                }
            }
            _ => {}
        }
    }

    async fn handle_confirm_delete_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = UiMode::Normal;
                self.pending_delete_target = None;
                self.pending_task_removal_action = TaskRemovalAction::default();
            }
            KeyCode::Enter => {
                let Some(target) = self.pending_delete_target.clone() else {
                    self.mode = UiMode::Normal;
                    return;
                };
                match target {
                    PendingDeleteTarget::Workspace { workspace_key } => {
                        match self.service.delete_workspace(&workspace_key).await {
                            Ok(()) => {
                                self.status = format!("Deleted workspace '{workspace_key}'");
                            }
                            Err(err) => {
                                self.status = format!(
                                    "Failed to delete workspace '{workspace_key}': {}",
                                    err.summary()
                                );
                            }
                        }
                    }
                    PendingDeleteTarget::Task {
                        workspace_key,
                        task_id,
                    } => match self
                        .service
                        .delete_workspace_task(&workspace_key, &task_id)
                        .await
                    {
                        Ok(()) => {
                            self.status = format!(
                                "Deleted task '{task_id}' from workspace '{workspace_key}'"
                            );
                        }
                        Err(err) => {
                            self.status = format!(
                                "Failed to delete task '{task_id}' from workspace '{workspace_key}': {}",
                                err.summary()
                            );
                        }
                    },
                }
                self.mode = UiMode::Normal;
                self.pending_delete_target = None;
                self.pending_task_removal_action = TaskRemovalAction::default();
            }
            _ => {}
        }
    }

    async fn handle_confirm_task_removal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = UiMode::Normal;
                self.pending_delete_target = None;
                self.pending_task_removal_action = TaskRemovalAction::default();
            }
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                self.pending_task_removal_action = self.pending_task_removal_action.previous();
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                self.pending_task_removal_action = self.pending_task_removal_action.next();
            }
            KeyCode::Enter => {
                let Some(PendingDeleteTarget::Task {
                    workspace_key,
                    task_id,
                }) = self.pending_delete_target.clone()
                else {
                    self.mode = UiMode::Normal;
                    self.pending_delete_target = None;
                    self.pending_task_removal_action = TaskRemovalAction::default();
                    return;
                };

                match self.pending_task_removal_action {
                    TaskRemovalAction::Remove => {
                        match self
                            .service
                            .remove_workspace_task(&workspace_key, &task_id, false)
                            .await
                        {
                            Ok(()) => {
                                self.status = format!(
                                    "Removed task '{task_id}' from workspace '{workspace_key}'"
                                );
                            }
                            Err(err) => {
                                self.status = format!(
                                    "Failed to remove task '{task_id}' from workspace '{workspace_key}': {}",
                                    err.summary()
                                );
                            }
                        }
                    }
                    TaskRemovalAction::RemoveAndIgnore => {
                        match self
                            .service
                            .remove_workspace_task(&workspace_key, &task_id, true)
                            .await
                        {
                            Ok(()) => {
                                self.status = format!(
                                    "Removed and ignored task '{task_id}' in workspace '{workspace_key}'"
                                );
                            }
                            Err(err) => {
                                self.status = format!(
                                    "Failed to remove and ignore task '{task_id}' from workspace '{workspace_key}': {}",
                                    err.summary()
                                );
                            }
                        }
                    }
                    TaskRemovalAction::Cancel => {}
                }

                self.mode = UiMode::Normal;
                self.pending_delete_target = None;
                self.pending_task_removal_action = TaskRemovalAction::default();
            }
            _ => {}
        }
    }

    fn handle_custom_link_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.clear_custom_link_modal();
                self.mode = UiMode::Normal;
            }
            KeyCode::Backspace => {
                self.custom_link_input.pop();
            }
            KeyCode::Delete => {
                let Some(key) = self.selected_workspace_key().map(str::to_string) else {
                    return;
                };
                let Some(kind) = self.custom_link_kind else {
                    return;
                };
                let Some(original_value) = self.custom_link_original_value.clone() else {
                    return;
                };

                match self.service.manager.get_workspace(&key) {
                    Ok(workspace) => {
                        workspace.update(|snapshot| {
                            let links = match kind {
                                WorkspaceLinkKind::Issue => {
                                    &mut snapshot.persistent.custom_links.issue
                                }
                                WorkspaceLinkKind::Pr => &mut snapshot.persistent.custom_links.pr,
                                WorkspaceLinkKind::Review => return false,
                            };
                            let previous_len = links.len();
                            links.retain(|value| value != &original_value);
                            links.len() != previous_len
                        });
                        self.status =
                            format!("Deleted custom {} link for '{key}'", kind.short_label());
                        self.clear_custom_link_modal();
                        self.mode = UiMode::Normal;
                    }
                    Err(err) => {
                        self.status = format!("Failed to delete custom link: {err:?}");
                    }
                }
            }
            KeyCode::Char(ch) => {
                self.custom_link_input.push(ch);
            }
            KeyCode::Enter => {
                let Some(key) = self.selected_workspace_key().map(str::to_string) else {
                    return;
                };
                let Some(kind) = self.custom_link_kind else {
                    return;
                };
                let value = self.custom_link_input.trim().to_string();
                if value.is_empty() {
                    self.status = "Custom link cannot be empty".to_string();
                    return;
                }

                let original_value = self.custom_link_original_value.clone();
                let action = self.custom_link_action;
                match self.service.manager.get_workspace(&key) {
                    Ok(workspace) => {
                        workspace.update(|snapshot| {
                            let links = match kind {
                                WorkspaceLinkKind::Issue => {
                                    &mut snapshot.persistent.custom_links.issue
                                }
                                WorkspaceLinkKind::Pr => &mut snapshot.persistent.custom_links.pr,
                                WorkspaceLinkKind::Review => return false,
                            };
                            match action {
                                Some(CustomLinkModalAction::Add) => {
                                    links.push(value.clone());
                                    true
                                }
                                Some(CustomLinkModalAction::Edit) => {
                                    let Some(original_value) = original_value.as_ref() else {
                                        return false;
                                    };
                                    let Some(position) =
                                        links.iter().position(|entry| entry == original_value)
                                    else {
                                        return false;
                                    };
                                    if links[position] == value {
                                        false
                                    } else {
                                        links[position] = value.clone();
                                        true
                                    }
                                }
                                None => false,
                            }
                        });
                        self.status =
                            format!("Updated custom {} links for '{key}'", kind.short_label());
                        self.clear_custom_link_modal();
                        self.mode = UiMode::Normal;
                    }
                    Err(err) => {
                        self.status = format!("Failed to update custom link: {err:?}");
                    }
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn snapshot_attach_target_for_selection(
    snapshot: &WorkspaceSnapshot,
    selected_task_id: Option<&str>,
) -> io::Result<AttachTarget> {
    if let Some(task_id) = selected_task_id {
        let task_state = task_runtime_snapshot(snapshot, task_id);
        let has_task = snapshot.task_persistent_snapshot(task_id).is_some();
        let should_use_last_codex_thread = has_task
            && (matches!(
                task_effective_agent_state(task_state),
                Some(AutomationAgentState::Stale)
            ) || task_state.is_some_and(|task_state| {
                matches!(task_state.status.as_deref(), Some("NotLoaded"))
            }) || snapshot.persistent.automation_paused
                && task_state
                    .and_then(|task_state| task_state.session_id.as_deref())
                    .is_none());
        if should_use_last_codex_thread {
            if let Some(uri) = codex_attach_uri(snapshot)? {
                return Ok(AttachTarget::Codex {
                    uri,
                    thread_id: None,
                });
            }
            return workspace_attach_target(snapshot);
        }
        if let Some(task_state) = task_state {
            return task_attach_target(snapshot, task_state);
        }
    }
    workspace_attach_target(snapshot)
}

fn codex_attach_uri(snapshot: &WorkspaceSnapshot) -> io::Result<Option<String>> {
    let Some(uri) = snapshot
        .transient
        .as_ref()
        .map(|transient| transient.uri.as_str())
    else {
        return Ok(None);
    };
    let parsed = Url::parse(uri)
        .map_err(|err| io::Error::other(format!("workspace attach URI is invalid: {err}")))?;
    Ok(matches!(parsed.scheme(), "ws" | "wss").then(|| parsed.to_string()))
}

pub(crate) fn snapshot_attach_cwd_for_selection(
    snapshot: &WorkspaceSnapshot,
    selected_task_id: Option<&str>,
    validations: &HashMap<WorkspaceLink, WorkspaceLinkValidationResult>,
    workspace_path: &Path,
) -> Option<PathBuf> {
    if let Some(task_id) = selected_task_id
        && let Some(task) = task_persistent_snapshot(snapshot, task_id)
    {
        return compare_target_path_for_task(
            snapshot,
            task,
            task_runtime_snapshot(snapshot, task_id),
            workspace_path,
        );
    }

    compare_target_path(snapshot, validations, workspace_path)
}

pub(crate) fn starting_modal_failure_status(
    key: &str,
    snapshot: Option<&WorkspaceSnapshot>,
) -> String {
    if let Some(automation_status) = snapshot
        .and_then(|snapshot| snapshot.automation_status.as_deref())
        .map(str::trim)
        .filter(|status| !status.is_empty())
    {
        return format!("Workspace '{key}' failed to start: {automation_status}");
    }

    format!("Workspace '{key}' failed to start; server is still stopped")
}

pub(crate) async fn dispatch_handler_action(
    relay_socket: Option<&Path>,
    program: &str,
    args: &[String],
    link: &WorkspaceLink,
    argument: &str,
) -> io::Result<()> {
    if let Some(socket_path) = relay_socket {
        let metadata = tokio::fs::metadata(socket_path).await?;
        let file_type = metadata.file_type();
        tracing::debug!(
            path = %socket_path.display(),
            is_socket = file_type.is_socket(),
            is_file = file_type.is_file(),
            is_dir = file_type.is_dir(),
            "dispatching relay handler action"
        );
        let action = match link.kind {
            WorkspaceLinkKind::Review => multicode_lib::RemoteAction::Review,
            WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr => multicode_lib::RemoteAction::Web,
        };
        let request = multicode_lib::RemoteActionRequest {
            action,
            argument: argument.to_string(),
        };
        let payload = multicode_lib::encode_remote_action_request(&request);
        let mut stream = tokio::net::UnixStream::connect(&socket_path).await?;
        use tokio::io::AsyncWriteExt;
        stream.write_all(payload.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.shutdown().await?;
        return Ok(());
    }

    tracing::info!(
        command = %std::iter::once(program)
            .chain(args.iter().map(String::as_str))
            .map(|arg| {
                if arg.is_empty() {
                    "''".to_string()
                } else if arg.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | ':' | '_' | '-' | '.' | '=')) {
                    arg.to_string()
                } else {
                    format!("'{}'", arg.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        "starting application via local handler spawn"
    );
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if link.kind == WorkspaceLinkKind::Review {
        command.current_dir(argument);
    }
    command.spawn().map(|_| ())
}

async fn dispatch_web_handler_action(
    relay_socket: Option<&Path>,
    program: &str,
    args: &[String],
    argument: &str,
) -> io::Result<()> {
    if let Some(socket_path) = relay_socket {
        let request = multicode_lib::RemoteActionRequest {
            action: multicode_lib::RemoteAction::Web,
            argument: argument.to_string(),
        };
        let payload = multicode_lib::encode_remote_action_request(&request);
        let mut stream = tokio::net::UnixStream::connect(&socket_path).await?;
        use tokio::io::AsyncWriteExt;
        stream.write_all(payload.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.shutdown().await?;
        return Ok(());
    }

    tracing::info!(
        command = %std::iter::once(program)
            .chain(args.iter().map(String::as_str))
            .map(|arg| {
                if arg.is_empty() {
                    "''".to_string()
                } else if arg.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | ':' | '_' | '-' | '.' | '=')) {
                    arg.to_string()
                } else {
                    format!("'{}'", arg.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        "starting web handler via local spawn"
    );
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().map(|_| ())
}
