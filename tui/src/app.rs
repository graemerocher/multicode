use crate::ops::*;
use crate::system::*;
use crate::*;
use multicode_lib::services::GithubTokenConfig;
use std::os::unix::fs::FileTypeExt;

const NERD_FONT_GITHUB_GLYPH: &str = "\u{f408}";
const CODEX_AUTO_RESUME_PROMPT: &str = "Continue autonomously from where you left off. Do not wait for approval for repository commands, builds, Gradle tasks, or focused tests. Only stop to ask before committing, pushing, commenting on GitHub, or opening or updating a pull request.";

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

pub(crate) fn should_request_autonomous_issue_scan(snapshot: &WorkspaceSnapshot) -> bool {
    snapshot.persistent.assigned_repository.is_some()
        && snapshot.persistent.automation_issue.is_none()
}

pub(crate) fn should_auto_resume_autonomous_codex_after_attach(
    snapshot: &WorkspaceSnapshot,
) -> bool {
    snapshot.persistent.assigned_repository.is_some()
        && snapshot.persistent.automation_issue.is_some()
}

impl TuiState {
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
            pending_delete_workspace_key: None,
            starting_workspace_key: None,
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
        let previous_selected_key = if self.selected_row > 0 {
            self.ordered_keys.get(self.selected_row - 1).cloned()
        } else {
            None
        };
        let selected_create_row = self.selected_row == 0;

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

        if selected_create_row {
            self.selected_row = 0;
        } else if let Some(previous_selected_key) = previous_selected_key {
            self.selected_row = self
                .ordered_keys
                .iter()
                .position(|key| key == &previous_selected_key)
                .map(|position| position + 1)
                .unwrap_or(0);
        } else {
            let max_row = self.ordered_keys.len();
            if self.selected_row > max_row {
                self.selected_row = max_row;
            }
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
                    self.pending_delete_workspace_key = None;
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
                Some(WorkspaceUiState::Started) => {}
                Some(WorkspaceUiState::Stopped) => {
                    if let Some(key) = self.starting_workspace_key.as_deref() {
                        self.status = starting_modal_failure_status(key, self.snapshots.get(key));
                    }
                    self.mode = UiMode::Normal;
                    self.starting_workspace_key = None;
                    self.started_wait_since = None;
                }
                None => {
                    self.mode = UiMode::Normal;
                    self.starting_workspace_key = None;
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

        if self.mode == UiMode::ConfirmDelete
            && self
                .pending_delete_workspace_key
                .as_deref()
                .is_some_and(|key| !self.snapshots.contains_key(key))
        {
            self.mode = UiMode::Normal;
            self.pending_delete_workspace_key = None;
        }
    }

    pub(crate) fn selected_workspace_key(&self) -> Option<&str> {
        if self.selected_row == 0 {
            None
        } else {
            self.ordered_keys
                .get(self.selected_row - 1)
                .map(String::as_str)
        }
    }

    pub(crate) fn selected_workspace_snapshot(&self) -> Option<&WorkspaceSnapshot> {
        self.selected_workspace_key()
            .and_then(|key| self.snapshots.get(key))
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

        validated_workspace_links_by_kind(
            snapshot,
            &self.workspace_link_validation_results,
            link.kind,
        )
        .into_iter()
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
        self.selected_workspace_link()
            .filter(|link| matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr))
            .map(|link| link.kind)
    }

    fn selected_workspace_selectable_links(&self) -> Vec<WorkspaceLink> {
        self.selected_workspace_key()
            .and_then(|key| self.snapshots.get(key))
            .map(|snapshot| {
                selectable_workspace_links(
                    snapshot,
                    &self.workspace_link_validation_results,
                    &self.github_link_statuses,
                )
            })
            .unwrap_or_default()
    }

    fn selected_workspace_link_argument(&self, link: &WorkspaceLink) -> Option<&str> {
        match self.workspace_link_validation_results.get(link) {
            Some(WorkspaceLinkValidationResult::Valid(argument)) => Some(argument.as_str()),
            Some(WorkspaceLinkValidationResult::Invalid(_)) | None => None,
        }
    }

    fn normalize_selected_link_index(&mut self) {
        if self.selected_row == 0 {
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
        let active_links = self
            .snapshots
            .values()
            .flat_map(workspace_links)
            .collect::<HashSet<_>>();

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
        let active_issue_or_pr_links = self
            .snapshots
            .values()
            .flat_map(workspace_links)
            .filter(|link| matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr))
            .collect::<HashSet<_>>();

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
        self.selected_workspace_selectable_links()
            .into_iter()
            .any(|link| matches!(link.kind, WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr))
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
            .filter(|snapshot| should_request_autonomous_issue_scan(snapshot))
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
        contextual_tool_hotkeys(
            &self.service.config.tool,
            self.selected_workspace_snapshot(),
        )
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
        self.snapshots
            .get(key)
            .ok_or_else(|| io::Error::other(format!("workspace snapshot missing for '{key}'")))
            .and_then(workspace_attach_target)
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
            UiMode::StartingModal => {}
            UiMode::ToolProgressModal => self.handle_tool_progress_key(key),
        }
    }

    pub(crate) async fn handle_auto_attach_when_ready(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) {
        let Some(key) = self.auto_attach_ready_key(Instant::now()) else {
            return;
        };

        self.mode = UiMode::Normal;
        self.starting_workspace_key = None;
        self.started_wait_since = None;

        match self.snapshot_attach_target(&key) {
            Ok(target) => {
                let custom_description = self
                    .snapshots
                    .get(&key)
                    .map(|snapshot| snapshot.persistent.description.clone())
                    .unwrap_or_default();
                match attach_in_tmux(
                    terminal,
                    self.service.agent_command(),
                    &target,
                    &self.attach_env_for_workspace(&key),
                    &key,
                    &custom_description,
                )
                .await
                {
                    Ok(_) => {
                        self.handle_attach_exit(&key).await;
                    }
                    Err(err) => {
                        self.status = format!("Failed to attach to workspace '{key}': {err}");
                    }
                }
            }
            Err(err) => {
                self.status = format!("Failed to attach to workspace '{key}': {err}");
            }
        }
    }

    async fn handle_attach_exit(&mut self, key: &str) {
        if self.maybe_resume_autonomous_codex_after_attach(key).await {
            return;
        }
        self.status = format!("Detached from workspace '{key}' agent session");
    }

    async fn maybe_resume_autonomous_codex_after_attach(&mut self, key: &str) -> bool {
        if self.service.agent_provider() != multicode_lib::services::AgentProvider::Codex {
            return false;
        }

        for _ in 0..10 {
            self.sync_from_manager();
            let snapshot = self.snapshots.get(key).cloned();
            let Some(snapshot) = snapshot else {
                return false;
            };

            if should_auto_resume_autonomous_codex_after_attach(&snapshot) {
                match self
                    .service
                    .prompt_root_session(&snapshot, CODEX_AUTO_RESUME_PROMPT)
                    .await
                {
                    Ok(()) => {
                        self.status = format!(
                            "Detached from workspace '{key}'; autonomous Codex work was no longer running interactively, so multicode resumed it automatically"
                        );
                    }
                    Err(err) => {
                        self.status = format!(
                            "Detached from workspace '{key}'; autonomous Codex work stopped after attach, but multicode failed to resume it automatically: {err}"
                        );
                    }
                }
                return true;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        false
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
            self.mode = UiMode::Normal;
            match result {
                Ok(()) => {
                    self.status = format!(
                        "{} completed for workspace '{}'",
                        running_tool.operation_name, running_tool.workspace_key
                    );
                }
                Err(err) => {
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

    fn handle_tool_progress_key(&mut self, key: KeyEvent) {
        if key.code != KeyCode::Esc {
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

        if !tool_is_usable(&tool, &snapshot) {
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
                self.run_exec_tool(terminal, &workspace_key, &tool.name, exec_command)
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
    ) {
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
        self.status = match run_tmux_new_session_command(
            terminal,
            &inherited_env,
            tmux_command,
            workspace_key,
            &custom_description,
        )
        .await
        {
            Ok(()) => format!(
                "Tool '{}' finished for workspace '{}'",
                tool_name, workspace_key
            ),
            Err(err) => format!("Failed to run tool '{}': {err}", tool_name),
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
            progress_rx,
            result_rx,
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

    async fn handle_normal_key(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        key: KeyEvent,
    ) {
        let link_selected = self.selected_link_index.is_some();
        let control_held = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Up => {
                if link_selected {
                    self.move_selected_link_target_up();
                } else if control_held {
                    if let Some(row) = next_non_stopped_row(
                        self.selected_row,
                        &self.ordered_keys,
                        &self.snapshots,
                        -1,
                    ) {
                        self.selected_row = row;
                        self.selected_link_index = None;
                        self.selected_link_target_index = 0;
                    }
                } else if self.selected_row > 0 {
                    self.selected_row -= 1;
                    self.selected_link_index = None;
                    self.selected_link_target_index = 0;
                }
            }
            KeyCode::Down => {
                if link_selected {
                    self.move_selected_link_target_down();
                } else if control_held {
                    if let Some(row) = next_non_stopped_row(
                        self.selected_row,
                        &self.ordered_keys,
                        &self.snapshots,
                        1,
                    ) {
                        self.selected_row = row;
                        self.selected_link_index = None;
                        self.selected_link_target_index = 0;
                    }
                } else if self.selected_row < self.ordered_keys.len() {
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
                                    let custom_description = self
                                        .snapshots
                                        .get(&key)
                                        .map(|snapshot| snapshot.persistent.description.clone())
                                        .unwrap_or_default();
                                    match attach_in_tmux(
                                        terminal,
                                        self.service.agent_command(),
                                        &target,
                                        &self.attach_env_for_workspace(&key),
                                        &key,
                                        &custom_description,
                                    )
                                    .await
                                    {
                                        Ok(_) => {
                                            self.handle_attach_exit(&key).await;
                                        }
                                        Err(err) => {
                                            self.status = format!(
                                                "Failed to attach to workspace '{key}': {err}"
                                            )
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
                            match self.service.start_workspace(&key).await {
                                Ok(_) => {
                                    self.mode = UiMode::StartingModal;
                                    self.starting_workspace_key = Some(key.clone());
                                    self.started_wait_since = None;
                                    self.status =
                                        format!("Starting workspace '{key}' before attaching");
                                }
                                Err(err) => {
                                    self.status = format!(
                                        "Failed to start workspace '{key}' before attaching: {}",
                                        err.summary()
                                    );
                                }
                            }
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
                        progress_rx,
                        result_rx,
                        cancel: Some(join_handle.abort_handle()),
                    });
                    self.mode = UiMode::ToolProgressModal;
                    self.status = format!("{} workspace '{}'", operation_name, key);
                }
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
                    self.issue_input = snapshot
                        .persistent
                        .automation_issue
                        .clone()
                        .and_then(|issue| issue.rsplit('/').next().map(ToOwned::to_owned))
                        .unwrap_or_default();
                    self.mode = UiMode::EditIssue;
                }
            }
            KeyCode::Char('s') => {
                if link_selected {
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
                            match self.service.start_workspace(&key).await {
                                Ok(_) => self.status = format!("Starting workspace '{key}'"),
                                Err(err) => {
                                    self.status = format!(
                                        "Failed to start workspace '{key}': {}",
                                        err.summary()
                                    )
                                }
                            }
                        }
                        Some(WorkspaceUiState::Starting) | Some(WorkspaceUiState::Started) => {
                            match self.service.stop_workspace(&key).await {
                                Ok(_) => self.status = format!("Stopped workspace '{key}'"),
                                Err(err) => {
                                    self.status = format!("Failed to stop workspace: {err:?}")
                                }
                            }
                        }
                        None => {}
                    }
                }
            }
            KeyCode::Char('r') => {
                if link_selected {
                    return;
                }
                self.request_selected_workspace_github_status_refresh();
            }
            KeyCode::Char('x') => {
                if link_selected {
                    return;
                }
                if let Some(key) = self.selected_workspace_key().map(str::to_string) {
                    self.pending_delete_workspace_key = Some(key);
                    self.mode = UiMode::ConfirmDelete;
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
                        self.status = format!("Assigned issue '{normalized}' to workspace '{key}'");
                        self.mode = UiMode::Normal;
                        self.issue_input.clear();
                    }
                    Ok(None) => {
                        self.status =
                            format!("Cleared direct issue assignment for workspace '{key}'");
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
                self.pending_delete_workspace_key = None;
            }
            KeyCode::Enter => {
                let Some(workspace_key) = self.pending_delete_workspace_key.clone() else {
                    self.mode = UiMode::Normal;
                    return;
                };
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
                self.mode = UiMode::Normal;
                self.pending_delete_workspace_key = None;
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
