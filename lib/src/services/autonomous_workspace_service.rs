use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};

use diesel::{
    Connection, QueryableByName, RunQueryDsl, connection::SimpleConnection, sql_query,
    sqlite::SqliteConnection,
};
use serde::Deserialize;
use tokio::{
    process::Command,
    sync::watch,
    task::spawn_blocking,
    time::{Instant, sleep, sleep_until},
};
use url::Url;
use uuid::Uuid;

use super::{
    CombinedService, GithubStatus,
    codex_app_server::CodexAppServerClient,
    runtime::{
        automation_state_file_source, automation_task_state_file_source,
        synthetic_codex_home_source,
    },
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{
    AutomationAgentState, RootSessionStatus, WorkspaceIssueType, WorkspaceManagerError,
    WorkspaceSnapshot, WorkspaceTaskPersistentSnapshot, WorkspaceTaskSource, manager::Workspace,
    opencode, services::config::AgentProvider, workspace_issue_type_glyph,
};

const ISSUE_PRIORITY_LABELS: [&str; 4] = [
    "type: bug",
    "type:docs",
    "type: improvement",
    "type: enhancement",
];
const ISSUE_PRIORITY_BOOST_LABELS: [&str; 2] = ["type: regression", "priority: high"];
const DEPENDENCY_UPGRADE_LABEL: &str = "type: dependency-upgrade";
const NON_MAJOR_DEPENDENCY_UPGRADE_LABELS: [&str; 4] = ["minor", "patch", "pin", "digest"];
const MAJOR_DEPENDENCY_UPGRADE_LABELS: [&str; 1] = ["major"];
const RENOVATE_LOGINS: [&str; 2] = ["renovate[bot]", "app/renovate"];
const DEPENDENCY_UPGRADE_ISSUE_TITLE_PREFIX: &str =
    "Dependency upgrade follow-up for Renovate batch";
const SINGLE_DEPENDENCY_UPGRADE_ISSUE_TITLE_PREFIX: &str =
    "Dependency upgrade follow-up for Renovate PR";
const DEPENDENCY_UPGRADE_PR_MARKER_PREFIX: &str = "<!-- multicode:dependency-upgrade-pr=";
const DEPENDENCY_UPGRADE_PR_BATCH_MARKER_PREFIX: &str = "<!-- multicode:dependency-upgrade-prs=";
const IN_PROGRESS_LABEL: &str = "status: in progress";
const IN_PROGRESS_LABEL_ALIASES: [&str; 2] = [IN_PROGRESS_LABEL, "status: in-progress"];
const AWAITING_VALIDATION_LABEL: &str = "status: awaiting validation";
const ISSUE_SCAN_RETRY_DELAY: Duration = Duration::from_secs(60);
const DOC_ISSUE_LABELS: [&str; 4] = [
    "type: docs",
    "type:docs",
    "status: docs",
    "status: documentation",
];
const BUG_ISSUE_LABELS: [&str; 2] = ["type: bug", "status: bug"];
const REGRESSION_ISSUE_LABELS: [&str; 2] = ["type: regression", "status: regression"];
const IMPROVEMENT_ISSUE_LABELS: [&str; 1] = ["type: improvement"];
const ENHANCEMENT_ISSUE_LABELS: [&str; 1] = ["type: enhancement"];

#[derive(Debug, Clone)]
struct QueuedIssueCandidate {
    issue: SelectedIssue,
    backing_pr_url: Option<String>,
    dependency_upgrade_backing_pr: bool,
}

#[derive(Debug)]
pub enum AutonomousWorkspaceServiceError {
    Manager(WorkspaceManagerError),
}

impl From<WorkspaceManagerError> for AutonomousWorkspaceServiceError {
    fn from(value: WorkspaceManagerError) -> Self {
        Self::Manager(value)
    }
}

pub async fn autonomous_workspace_service(
    service: CombinedService,
) -> Result<(), AutonomousWorkspaceServiceError> {
    monitor_workspace_snapshots(
        service.manager.clone(),
        move |key, workspace, workspace_rx| {
            let service = service.clone();
            async move {
                tracing::info!(workspace_key = %key, "starting autonomous workspace watcher");
                tokio::spawn(async move {
                    let workspace_key = key.clone();
                    let handle = tokio::spawn(async move {
                        watch_workspace(service, key, workspace, workspace_rx).await;
                    });
                    if let Err(error) = handle.await {
                        tracing::error!(
                            workspace_key,
                            error = ?error,
                            "autonomous workspace watcher terminated unexpectedly"
                        );
                    }
                });
                Ok(())
            }
        },
    )
    .await
}

async fn watch_workspace(
    service: CombinedService,
    workspace_key: String,
    workspace: Workspace,
    mut workspace_rx: watch::Receiver<WorkspaceSnapshot>,
) {
    tracing::info!(workspace_key, "autonomous workspace watcher active");
    let issue_scan_delay = Duration::from_secs(service.config.autonomous.issue_scan_delay_seconds);
    let issue_scan_delay = issue_scan_delay.max(Duration::from_secs(1));
    let mut next_scan_at: Option<Instant> = None;
    let mut watched_issue_url: Option<String> = None;
    let mut issue_status_rx: Option<watch::Receiver<Option<GithubStatus>>> = None;
    let mut task_issue_status_rxs: HashMap<String, watch::Receiver<Option<GithubStatus>>> =
        HashMap::new();
    let mut task_pr_status_rxs: HashMap<String, watch::Receiver<Option<GithubStatus>>> =
        HashMap::new();
    let mut previous_root_status: Option<RootSessionStatus> = None;
    let mut previous_scan_request_nonce: u64 = 0;
    let mut previous_queue_next_request_nonce: u64 = 0;
    let mut blocked_start_scan_request_nonce: Option<u64> = None;
    let mut previous_active_issue_url: Option<String> = None;
    let mut startup_scan_pending = !service.config.autonomous.scan_on_startup;

    loop {
        let mut snapshot = workspace_rx.borrow().clone();
        let assigned_repository = snapshot.persistent.assigned_repository.clone();
        let scan_requested = snapshot.automation_scan_request_nonce != previous_scan_request_nonce;
        previous_scan_request_nonce = snapshot.automation_scan_request_nonce;
        let queue_next_requested =
            snapshot.automation_queue_next_request_nonce != previous_queue_next_request_nonce;
        previous_queue_next_request_nonce = snapshot.automation_queue_next_request_nonce;
        if scan_requested || queue_next_requested || !snapshot.persistent.tasks.is_empty() {
            startup_scan_pending = false;
        }

        if assigned_repository.is_some() || !snapshot.persistent.tasks.is_empty() {
            tracing::warn!(
                workspace_key,
                assigned_repository = assigned_repository.as_deref(),
                transient_uri = snapshot.transient.as_ref().map(|transient| transient.uri.as_str()),
                active_task_id = snapshot.active_task_id.as_deref(),
                resolved_active_task_id = snapshot.resolved_active_task_id().as_deref(),
                automation_issue = snapshot.persistent.automation_issue.as_deref(),
                task_count = snapshot.persistent.tasks.len(),
                root_session_id = snapshot.root_session_id.as_deref(),
                root_status = ?snapshot.root_session_status,
                scan_requested,
                queue_next_requested,
                paused = snapshot.persistent.automation_paused,
                "autonomous workspace iteration"
            );
        }

        if snapshot.persistent.archived || assigned_repository.is_none() {
            watched_issue_url = None;
            issue_status_rx = None;
            task_issue_status_rxs.clear();
            task_pr_status_rxs.clear();
            next_scan_at = None;
            blocked_start_scan_request_nonce = None;
            previous_active_issue_url = None;
            previous_root_status = snapshot.root_session_status;
            clear_automation_runtime_state(&workspace);
            set_automation_status(&workspace, None);
            if workspace_rx.changed().await.is_err() {
                break;
            }
            continue;
        }

        let assigned_repository = assigned_repository.expect("checked above");
        let repository_label = compact_repository_label(&assigned_repository);
        if snapshot.persistent.automation_paused {
            watched_issue_url = None;
            issue_status_rx = None;
            task_issue_status_rxs.clear();
            task_pr_status_rxs.clear();
            next_scan_at = None;
            blocked_start_scan_request_nonce = None;
            previous_active_issue_url = None;
            previous_root_status = snapshot.root_session_status;
            set_paused_automation_state(&workspace, repository_label);
            if workspace_rx.changed().await.is_err() {
                break;
            }
            continue;
        }
        sync_task_runtime_state(&workspace, &snapshot);
        snapshot = workspace.subscribe().borrow().clone();
        recover_codex_task_sessions(
            &service,
            &workspace,
            &snapshot,
            &workspace_key,
            &assigned_repository,
        )
        .await;
        snapshot = workspace.subscribe().borrow().clone();
        reconcile_codex_task_runtime_states(
            &service,
            &workspace,
            &snapshot,
            &workspace_key,
            &assigned_repository,
        )
        .await;
        snapshot = workspace.subscribe().borrow().clone();
        sync_task_issue_status_receivers(&service, &snapshot, &mut task_issue_status_rxs);
        sync_task_pr_status_receivers(&service, &snapshot, &mut task_pr_status_rxs);
        let current_issue_url = active_issue_url_for_snapshot(&snapshot);
        if previous_active_issue_url != current_issue_url {
            request_refresh_for_task_issues(
                &service,
                task_issue_status_rxs.keys().map(String::as_str),
                current_issue_url.as_deref(),
            );
            previous_active_issue_url = current_issue_url.clone();
        }
        let closed_background_issue_urls = closed_background_task_issue_urls(
            &snapshot,
            current_issue_url.as_deref(),
            &task_issue_status_rxs,
        );
        if !closed_background_issue_urls.is_empty() {
            for closed_issue_url in &closed_background_issue_urls {
                if let Some(task_id) = task_id_for_issue(&snapshot, closed_issue_url) {
                    clear_task_automation_state_file(&service, &workspace_key, &task_id).await;
                }
                if let Err(err) = service
                    .remove_workspace_task_checkout(
                        &workspace_key,
                        &assigned_repository,
                        closed_issue_url,
                    )
                    .await
                {
                    tracing::warn!(
                        workspace_key,
                        issue_url = %closed_issue_url,
                        error = ?err,
                        "failed to remove closed background task worktree"
                    );
                }
                clear_automation_issue_claim(&workspace, closed_issue_url);
                task_issue_status_rxs.remove(closed_issue_url);
                task_pr_status_rxs.remove(closed_issue_url);
            }
            next_scan_at = Some(Instant::now());
            set_automation_status(&workspace, Some(format!("Next issue {repository_label}")));
            previous_root_status = snapshot.root_session_status;
            continue;
        }
        let merged_pr_issue_urls =
            merged_dependency_upgrade_issue_urls(&snapshot, &task_pr_status_rxs);
        if !merged_pr_issue_urls.is_empty() {
            let token = match resolved_gh_token(&service).await {
                Ok(token) => token,
                Err(err) => {
                    set_automation_status(
                        &workspace,
                        Some(format!("Merge close failed {repository_label}: {err}")),
                    );
                    previous_root_status = snapshot.root_session_status;
                    if workspace_rx.changed().await.is_err() {
                        break;
                    }
                    continue;
                }
            };
            for issue_url in &merged_pr_issue_urls {
                if let Some(task_id) = task_id_for_issue(&snapshot, issue_url) {
                    clear_task_automation_state_file(&service, &workspace_key, &task_id).await;
                }
                if let Err(err) = close_dependency_upgrade_issue(
                    &assigned_repository,
                    issue_url,
                    &snapshot,
                    &token,
                )
                .await
                {
                    tracing::warn!(
                        workspace_key,
                        issue_url = %issue_url,
                        error = %err,
                        "failed to close dependency-upgrade issue after backing PR merged"
                    );
                    continue;
                }
                if let Err(err) = service
                    .remove_workspace_task_checkout(&workspace_key, &assigned_repository, issue_url)
                    .await
                {
                    tracing::warn!(
                        workspace_key,
                        issue_url = %issue_url,
                        error = ?err,
                        "failed to remove merged dependency-upgrade task worktree"
                    );
                }
                clear_automation_issue_claim(&workspace, issue_url);
                task_issue_status_rxs.remove(issue_url);
                task_pr_status_rxs.remove(issue_url);
            }
            next_scan_at = Some(Instant::now());
            set_automation_status(&workspace, Some(format!("Next issue {repository_label}")));
            previous_root_status = snapshot.root_session_status;
            continue;
        }
        if !snapshot.persistent.tasks.is_empty() {
            tracing::warn!(
                workspace_key,
                task_count = snapshot.persistent.tasks.len(),
                active_task_id = snapshot.active_task_id.as_deref(),
                automation_issue = snapshot.persistent.automation_issue.as_deref(),
                root_session_id = snapshot.root_session_id.as_deref(),
                root_status = ?snapshot.root_session_status,
                scan_requested,
                queue_next_requested,
                "autonomous workspace loop snapshot"
            );
        }
        if scan_requested || queue_next_requested {
            blocked_start_scan_request_nonce = None;
        }
        if (scan_requested || queue_next_requested)
            && active_task_id_for_snapshot(&snapshot).is_none()
            && matches!(
                snapshot
                    .root_session_status
                    .unwrap_or(RootSessionStatus::Idle),
                RootSessionStatus::Idle
            )
        {
            next_scan_at = Some(Instant::now());
            set_automation_status(&workspace, Some(format!("Scan now {repository_label}")));
        }

        if active_task_id_for_snapshot(&snapshot).is_none() {
            if let Some(next_issue_url) = next_schedulable_task_issue_url(&snapshot, None) {
                tracing::info!(
                    workspace_key,
                    issue_url = %next_issue_url,
                    task_count = snapshot.persistent.tasks.len(),
                    root_session_id = snapshot.root_session_id.as_deref(),
                    root_status = ?snapshot.root_session_status,
                    "leasing autonomous task onto workspace VM"
                );
                lease_task_issue(&workspace, &snapshot, &next_issue_url);
                clear_automation_state_file(&service, &workspace_key).await;
                set_automation_status(
                    &workspace,
                    Some(format!(
                        "Scheduling {}",
                        issue_reference(&next_issue_url).unwrap_or(next_issue_url.clone())
                    )),
                );
                previous_root_status = snapshot.root_session_status;
                if workspace_rx.changed().await.is_err() {
                    break;
                }
                continue;
            }
            if !snapshot.persistent.tasks.is_empty() {
                tracing::warn!(
                    workspace_key,
                    task_count = snapshot.persistent.tasks.len(),
                    root_session_id = snapshot.root_session_id.as_deref(),
                    root_status = ?snapshot.root_session_status,
                    active_task_id = snapshot.active_task_id.as_deref(),
                    automation_issue = snapshot.persistent.automation_issue.as_deref(),
                    "autonomous workspace has queued tasks but no schedulable active lease"
                );
            }
        }

        if snapshot.transient.is_none() {
            if start_retry_is_blocked(
                blocked_start_scan_request_nonce,
                snapshot.automation_scan_request_nonce,
            ) {
                previous_root_status = snapshot.root_session_status;
                if workspace_rx.changed().await.is_err() {
                    break;
                }
                continue;
            }
            set_automation_status(&workspace, Some(format!("Start {repository_label}")));
            match service.start_workspace(&workspace_key).await {
                Ok(()) => {
                    blocked_start_scan_request_nonce = None;
                }
                Err(err) => {
                    blocked_start_scan_request_nonce = Some(snapshot.automation_scan_request_nonce);
                    set_automation_status(
                        &workspace,
                        Some(format!(
                            "Start failed {repository_label}: {}",
                            err.summary()
                        )),
                    );
                    if workspace_rx.changed().await.is_err() {
                        break;
                    }
                }
            }
            previous_root_status = snapshot.root_session_status;
            continue;
        }

        let agent_ready = snapshot
            .transient
            .as_ref()
            .and_then(|transient| url::Url::parse(&transient.uri).ok())
            .map(|uri| match uri.scheme() {
                "ws" | "wss" => snapshot.root_session_id.is_some(),
                _ => snapshot.opencode_client.is_some() && snapshot.root_session_id.is_some(),
            })
            .unwrap_or(false);
        if !agent_ready {
            set_automation_status(&workspace, Some(format!("Wait server {repository_label}")));
            previous_root_status = snapshot.root_session_status;
            if workspace_rx.changed().await.is_err() {
                break;
            }
            continue;
        }

        if let Some(current_issue_url) = current_issue_url {
            tracing::warn!(
                workspace_key,
                issue_url = %current_issue_url,
                root_session_id = snapshot.root_session_id.as_deref(),
                root_status = ?snapshot.root_session_status,
                task_session_id = snapshot
                    .resolved_active_task_id()
                    .as_deref()
                    .and_then(|task_id| snapshot.task_states.get(task_id))
                    .and_then(|state| state.session_id.as_deref()),
                "autonomous workspace has active issue candidate"
            );
            let active_task_can_yield = active_task_can_yield_vm(&snapshot);
            let next_schedulable_issue_url =
                next_schedulable_task_issue_url(&snapshot, Some(current_issue_url.as_str()));
            tracing::warn!(
                workspace_key,
                issue_url = %current_issue_url,
                active_task_can_yield,
                next_schedulable_issue_url = next_schedulable_issue_url.as_deref(),
                task_states = ?snapshot
                    .persistent
                    .tasks
                    .iter()
                    .map(|task| {
                        let task_state = snapshot.task_states.get(&task.id);
                        (
                            task.id.as_str(),
                            task.issue_url.as_str(),
                            task_state.and_then(|state| state.session_id.as_deref()),
                            task_state.and_then(|state| state.agent_state),
                            task_state.and_then(|state| state.session_status),
                            task_state.is_some_and(|state| state.waiting_on_vm),
                        )
                    })
                    .collect::<Vec<_>>(),
                "autonomous workspace scheduling decision"
            );
            if scan_requested || queue_next_requested {
                match resolved_gh_token(&service).await {
                    Ok(token) => {
                        if let Err(err) = refresh_existing_task_backing_pr_urls(
                            &workspace,
                            &snapshot,
                            &assigned_repository,
                            &token,
                        )
                        .await
                        {
                            tracing::warn!(
                                workspace_key,
                                error = %err,
                                "failed to refresh backing pull requests for existing tasks"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            workspace_key,
                            error = %err,
                            "failed to resolve GitHub token while refreshing existing task pull requests"
                        );
                    }
                }
                let available_slots = if queue_next_requested {
                    1
                } else {
                    service
                        .config
                        .autonomous
                        .max_parallel_issues
                        .saturating_sub(snapshot.persistent.tasks.len())
                };
                if available_slots > 0 {
                    match enqueue_next_issues(
                        &service,
                        &workspace,
                        &workspace_key,
                        &snapshot,
                        &assigned_repository,
                        available_slots,
                    )
                    .await
                    {
                        Ok(queued) if queued > 0 => {
                            set_automation_status(
                                &workspace,
                                Some(format!("Queued {queued} issue(s) for {repository_label}")),
                            );
                            previous_root_status = snapshot.root_session_status;
                            continue;
                        }
                        Ok(_) => {}
                        Err(err) => {
                            set_automation_status(
                                &workspace,
                                Some(format!("Scan failed {repository_label}: {err}")),
                            );
                            previous_root_status = snapshot.root_session_status;
                            if !wait_for_workspace_change_until(
                                &mut workspace_rx,
                                &mut issue_status_rx,
                                Some(Instant::now() + ISSUE_SCAN_RETRY_DELAY),
                            )
                            .await
                            {
                                break;
                            }
                            continue;
                        }
                    }
                }
            }
            if active_task_can_yield && let Some(next_issue_url) = next_schedulable_issue_url {
                lease_task_issue(&workspace, &snapshot, &next_issue_url);
                clear_automation_state_file(&service, &workspace_key).await;
                watched_issue_url = None;
                issue_status_rx = None;
                set_automation_status(
                    &workspace,
                    Some(format!(
                        "Scheduling {}",
                        issue_reference(&next_issue_url).unwrap_or(next_issue_url.clone())
                    )),
                );
                previous_root_status = snapshot.root_session_status;
                if workspace_rx.changed().await.is_err() {
                    break;
                }
                continue;
            }
            let root_status = snapshot
                .root_session_status
                .unwrap_or(RootSessionStatus::Idle);
            let should_resume_assigned_issue =
                should_start_assigned_issue_work(&snapshot, root_status);

            if should_bridge_root_runtime_state(
                service.agent_provider(),
                &snapshot,
                should_resume_assigned_issue,
                root_status,
            ) {
                set_automation_runtime_state(
                    &workspace,
                    snapshot.root_session_id.clone(),
                    snapshot.root_session_status,
                );
            }
            if watched_issue_url.as_deref() != Some(current_issue_url.as_str()) {
                watched_issue_url = Some(current_issue_url.clone());
                issue_status_rx = service
                    .github_status_service()
                    .watch_status(&current_issue_url);
            }

            if should_resume_assigned_issue {
                tracing::info!(
                    workspace_key,
                    issue_url = %current_issue_url,
                    "starting autonomous work for assigned issue"
                );
                match start_assigned_issue_work(
                    &service,
                    &workspace,
                    &workspace_key,
                    &snapshot,
                    &assigned_repository,
                    &current_issue_url,
                )
                .await
                {
                    Ok(Some(issue)) => {
                        watched_issue_url = Some(issue.url.clone());
                        issue_status_rx = service.github_status_service().watch_status(&issue.url);
                        set_automation_status(
                            &workspace,
                            Some(format!("Working {}", issue.display_reference())),
                        );
                    }
                    Ok(None) => {
                        clear_automation_issue_claim(&workspace, &current_issue_url);
                        watched_issue_url = None;
                        issue_status_rx = None;
                        task_issue_status_rxs.remove(&current_issue_url);
                        task_pr_status_rxs.remove(&current_issue_url);
                        previous_active_issue_url = None;
                        next_scan_at = Some(Instant::now() + issue_scan_delay);
                        set_automation_status(
                            &workspace,
                            Some(format!(
                                "Issue unavailable {repository_label}; next {}m",
                                (issue_scan_delay.as_secs() + 59) / 60
                            )),
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            workspace_key,
                            issue_url = %current_issue_url,
                            error = %err,
                            "failed to resume assigned autonomous issue work"
                        );
                        set_automation_status(
                            &workspace,
                            Some(format!("Issue start failed {repository_label}: {err}")),
                        );
                    }
                }
                previous_root_status = snapshot.root_session_status;
                if !wait_for_workspace_change_until(
                    &mut workspace_rx,
                    &mut issue_status_rx,
                    next_scan_at,
                )
                .await
                {
                    break;
                }
                continue;
            }

            if snapshot.root_session_status == Some(RootSessionStatus::Idle)
                && previous_root_status != Some(RootSessionStatus::Idle)
            {
                let _ = service
                    .github_status_service()
                    .request_refresh(&current_issue_url);
            }

            if issue_is_closed(issue_status_rx.as_ref()) {
                if let Err(err) = service
                    .remove_workspace_task_checkout(
                        &workspace_key,
                        &assigned_repository,
                        &current_issue_url,
                    )
                    .await
                {
                    tracing::warn!(
                        workspace_key,
                        issue_url = %current_issue_url,
                        error = ?err,
                        "failed to remove closed task worktree"
                    );
                }
                clear_automation_issue_claim(&workspace, &current_issue_url);
                watched_issue_url = None;
                issue_status_rx = None;
                task_issue_status_rxs.remove(&current_issue_url);
                task_pr_status_rxs.remove(&current_issue_url);
                previous_active_issue_url = None;
                next_scan_at = Some(Instant::now());
                set_automation_status(&workspace, Some(format!("Next issue {repository_label}")));
                previous_root_status = snapshot.root_session_status;
                continue;
            }

            set_automation_status(
                &workspace,
                Some(issue_progress_status(
                    &assigned_repository,
                    &current_issue_url,
                    effective_automation_agent_state(&snapshot),
                )),
            );
            previous_root_status = snapshot.root_session_status;
            if !wait_for_workspace_change_until(&mut workspace_rx, &mut issue_status_rx, None).await
            {
                break;
            }
            continue;
        }

        if matches!(
            snapshot.root_session_status,
            Some(RootSessionStatus::Busy | RootSessionStatus::Question)
        ) {
            set_automation_status(
                &workspace,
                Some(format!("Wait current session {repository_label}")),
            );
            previous_root_status = snapshot.root_session_status;
            if !wait_for_workspace_change_until(&mut workspace_rx, &mut issue_status_rx, None).await
            {
                break;
            }
            continue;
        }

        let now = Instant::now();
        let current_deadline = next_scan_at.unwrap_or(now);
        if current_deadline > now {
            set_automation_status(
                &workspace,
                Some(format!(
                    "No issues {repository_label}; next {}m",
                    ((current_deadline - now).as_secs() + 59) / 60
                )),
            );
            previous_root_status = snapshot.root_session_status;
            if !wait_for_workspace_change_until(
                &mut workspace_rx,
                &mut issue_status_rx,
                Some(current_deadline),
            )
            .await
            {
                break;
            }
            continue;
        }

        if should_defer_startup_issue_scan(
            startup_scan_pending,
            &snapshot,
            scan_requested,
            queue_next_requested,
        ) {
            set_automation_status(
                &workspace,
                Some(format!(
                    "Startup issue scan disabled for {repository_label}"
                )),
            );
            previous_root_status = snapshot.root_session_status;
            if !wait_for_workspace_change_until(&mut workspace_rx, &mut issue_status_rx, None).await
            {
                break;
            }
            continue;
        }

        set_automation_status(&workspace, Some(format!("Scan issues {repository_label}")));
        let token = match resolved_gh_token(&service).await {
            Ok(token) => token,
            Err(err) => {
                next_scan_at = Some(Instant::now() + ISSUE_SCAN_RETRY_DELAY);
                set_automation_status(
                    &workspace,
                    Some(format!("Scan failed {repository_label}: {err}")),
                );
                previous_root_status = snapshot.root_session_status;
                if !wait_for_workspace_change_until(
                    &mut workspace_rx,
                    &mut issue_status_rx,
                    next_scan_at,
                )
                .await
                {
                    break;
                }
                continue;
            }
        };
        if let Err(err) = refresh_existing_task_backing_pr_urls(
            &workspace,
            &snapshot,
            &assigned_repository,
            &token,
        )
        .await
        {
            tracing::warn!(
                workspace_key,
                error = %err,
                "failed to refresh backing pull requests for existing tasks during scan"
            );
        }
        let available_slots = available_issue_scan_slots(
            if queue_next_requested {
                snapshot.persistent.tasks.len() + 1
            } else {
                service.config.autonomous.max_parallel_issues
            },
            snapshot.persistent.tasks.len(),
        );
        match enqueue_next_issues(
            &service,
            &workspace,
            &workspace_key,
            &snapshot,
            &assigned_repository,
            available_slots,
        )
        .await
        {
            Ok(queued) if queued > 0 => {
                next_scan_at = None;
                let post_enqueue_snapshot = workspace.subscribe().borrow().clone();
                let next_issue_url = next_schedulable_task_issue_url(&post_enqueue_snapshot, None);
                tracing::warn!(
                    workspace_key,
                    queued,
                    task_count = post_enqueue_snapshot.persistent.tasks.len(),
                    active_task_id = post_enqueue_snapshot.active_task_id.as_deref(),
                    automation_issue = post_enqueue_snapshot.persistent.automation_issue.as_deref(),
                    next_issue_url = next_issue_url.as_deref(),
                    "autonomous workspace post-enqueue state"
                );
                if let Some(next_issue_url) = next_issue_url {
                    lease_task_issue(&workspace, &post_enqueue_snapshot, &next_issue_url);
                }
                set_automation_status(
                    &workspace,
                    Some(format!("Queued {queued} issue(s) for {repository_label}")),
                );
            }
            Ok(_) => {
                next_scan_at = Some(Instant::now() + issue_scan_delay);
                set_automation_status(
                    &workspace,
                    Some(format!(
                        "No issues {repository_label}; next {}m",
                        (issue_scan_delay.as_secs() + 59) / 60
                    )),
                );
            }
            Err(err) => {
                next_scan_at = Some(Instant::now() + ISSUE_SCAN_RETRY_DELAY);
                set_automation_status(
                    &workspace,
                    Some(format!("Scan failed {repository_label}: {err}")),
                );
            }
        }

        previous_root_status = snapshot.root_session_status;
        if !wait_for_workspace_change_until(&mut workspace_rx, &mut issue_status_rx, next_scan_at)
            .await
        {
            break;
        }
    }
}

fn start_retry_is_blocked(blocked_nonce: Option<u64>, current_nonce: u64) -> bool {
    blocked_nonce == Some(current_nonce)
}

async fn start_assigned_issue_work(
    service: &CombinedService,
    workspace: &Workspace,
    workspace_key: &str,
    snapshot: &WorkspaceSnapshot,
    assigned_repository: &str,
    issue_url: &str,
) -> Result<Option<SelectedIssue>, String> {
    let token = resolved_gh_token(service).await?;
    let Some(issue) = fetch_issue(assigned_repository, issue_url, &token).await? else {
        return Ok(None);
    };
    let existing_task = task_persistent_snapshot_for_issue(snapshot, &issue.url);
    let backing_pr_url = issue
        .legacy_dependency_upgrade_pr_url()
        .map(ToOwned::to_owned)
        .or_else(|| existing_task.and_then(|task| task.backing_pr_url.clone()));
    let dependency_upgrade_backing_pr = !issue.dependency_upgrade_pr_urls().is_empty()
        || existing_task.is_some_and(|task| task.dependency_upgrade_backing_pr);

    ensure_workspace_task_claim(
        workspace,
        assigned_repository,
        &issue,
        backing_pr_url.as_deref(),
        dependency_upgrade_backing_pr,
        WorkspaceTaskSource::Manual,
    );
    let task_session_id = ensure_task_session(
        service,
        workspace,
        snapshot,
        workspace_key,
        assigned_repository,
        &issue,
    )
    .await?;

    set_automation_runtime_state(
        workspace,
        Some(task_session_id.clone()),
        Some(RootSessionStatus::Busy),
    );

    set_automation_status(
        workspace,
        Some(format!("Claiming {}", issue.display_reference())),
    );
    clear_automation_state_file(service, workspace_key).await;
    let task_id = workspace
        .subscribe()
        .borrow()
        .persistent
        .tasks
        .iter()
        .find(|task| task.issue_url == issue.url)
        .map(|task| task.id.clone())
        .ok_or_else(|| format!("failed to resolve task id for issue {}", issue.url))?;

    if let Err(err) = prompt_task_session(
        service,
        snapshot,
        workspace_key,
        assigned_repository,
        &task_id,
        &issue,
        dependency_upgrade_backing_pr
            .then_some(backing_pr_url.as_deref())
            .flatten(),
        &task_session_id,
        task_cwd_path(service, workspace_key, assigned_repository, &issue.url),
    )
    .await
    {
        tracing::warn!(
            workspace_key,
            issue_url = %issue.url,
            error = %err,
            "failed to start manually assigned issue work"
        );
        return Err(err);
    }

    if let Err(err) = add_issue_label(assigned_repository, &issue, IN_PROGRESS_LABEL, &token).await
    {
        tracing::warn!(
            workspace_key,
            issue_url = %issue.url,
            label = IN_PROGRESS_LABEL,
            error = %err,
            "manual issue prompt started but adding in-progress label failed"
        );
        return Err(err);
    }

    if should_assign_issue_to_current_user(&issue) {
        if let Err(err) = assign_issue_to_me(assigned_repository, &issue, &token).await {
            tracing::warn!(
                workspace_key,
                issue_url = %issue.url,
                error = %err,
                "manual issue prompt started but assigning issue to current user failed"
            );
            return Err(err);
        }
    }

    Ok(Some(issue))
}

fn should_assign_issue_to_current_user(issue: &SelectedIssue) -> bool {
    !issue.requires_validation()
}

async fn enqueue_next_issues(
    service: &CombinedService,
    workspace: &Workspace,
    workspace_key: &str,
    snapshot: &WorkspaceSnapshot,
    assigned_repository: &str,
    limit: usize,
) -> Result<usize, String> {
    if limit == 0 {
        return Ok(0);
    }

    let token = resolved_gh_token(service).await?;
    let mut queued = 0usize;
    for _ in 0..limit {
        let current_snapshot = workspace.subscribe().borrow().clone();
        let excluded_issue_urls =
            reserved_issue_urls(service, workspace_key, Some(&current_snapshot));
        let Some(candidate) =
            find_next_issue(assigned_repository, &excluded_issue_urls, &token).await?
        else {
            break;
        };
        queue_issue_task(
            workspace,
            &candidate.issue,
            candidate.backing_pr_url.as_deref(),
            candidate.dependency_upgrade_backing_pr,
            WorkspaceTaskSource::Scan,
        );
        queued += 1;
    }

    let _ = snapshot;
    Ok(queued)
}

async fn refresh_existing_task_backing_pr_urls(
    workspace: &Workspace,
    snapshot: &WorkspaceSnapshot,
    assigned_repository: &str,
    token: &str,
) -> Result<usize, String> {
    let open_pull_requests = list_open_pull_requests(assigned_repository, token).await?;
    let mut updates = Vec::new();

    for task in &snapshot.persistent.tasks {
        let fetched_issue = if task.issue_type.is_none() {
            fetch_issue(assigned_repository, &task.issue_url, token).await?
        } else {
            None
        };
        let Some(issue_number) = issue_number_from_url(&task.issue_url) else {
            continue;
        };
        let issue = fetched_issue.unwrap_or(SelectedIssue {
            number: issue_number,
            title: String::new(),
            url: task.issue_url.clone(),
            created_at: String::new(),
            state: Some("OPEN".to_string()),
            is_pull_request: Some(false),
            body: None,
            labels: Vec::new(),
            dependency_upgrade_pr_urls: Vec::new(),
        });
        let discovered =
            discover_issue_backing_pr_url(assigned_repository, &issue, &open_pull_requests);
        let issue_type = issue.issue_type().or(task.issue_type);
        let issue_type_glyph = issue_type.map(|kind| workspace_issue_type_glyph(kind).to_string());
        if discovered.as_deref() != task.backing_pr_url.as_deref()
            || issue_type != task.issue_type
            || issue_type_glyph != task.issue_type_glyph
        {
            updates.push((task.id.clone(), discovered, issue_type, issue_type_glyph));
        }
    }

    if updates.is_empty() {
        return Ok(0);
    }

    workspace.update(|next| {
        let mut changed = false;
        for (task_id, backing_pr_url, issue_type, issue_type_glyph) in &updates {
            if let Some(task) = next
                .persistent
                .tasks
                .iter_mut()
                .find(|task| &task.id == task_id)
            {
                if task.backing_pr_url != *backing_pr_url {
                    task.backing_pr_url = backing_pr_url.clone();
                    changed = true;
                }
                if task.issue_type != *issue_type {
                    task.set_issue_type(*issue_type);
                    changed = true;
                } else if task.issue_type_glyph != *issue_type_glyph {
                    task.issue_type_glyph = issue_type_glyph.clone();
                    changed = true;
                }
            }
        }
        changed
    });

    Ok(updates.len())
}

fn ensure_workspace_task_claim(
    workspace: &Workspace,
    assigned_repository: &str,
    issue: &SelectedIssue,
    backing_pr_url: Option<&str>,
    dependency_upgrade_backing_pr: bool,
    source: WorkspaceTaskSource,
) {
    let issue_type = issue.issue_type();
    workspace.update(|snapshot| {
        let mut changed = false;
        if snapshot.persistent.assigned_repository.as_deref() != Some(assigned_repository) {
            snapshot.persistent.assigned_repository = Some(assigned_repository.to_string());
            changed = true;
        }
        let task_id = task_id_for_issue(snapshot, &issue.url).unwrap_or_else(|| {
            let task = WorkspaceTaskPersistentSnapshot::new(
                format!("task-{}", issue.number),
                issue.url.clone(),
                source,
            )
            .with_backing_pr_url(backing_pr_url.map(ToOwned::to_owned))
            .with_issue_type(issue_type)
            .with_dependency_upgrade_backing_pr(dependency_upgrade_backing_pr);
            let task_id = task.id.clone();
            snapshot.persistent.tasks.push(task);
            task_id
        });
        if let Some(task) = snapshot
            .persistent
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
        {
            if task.backing_pr_url.as_deref() != backing_pr_url {
                task.backing_pr_url = backing_pr_url.map(ToOwned::to_owned);
                changed = true;
            }
            if task.dependency_upgrade_backing_pr != dependency_upgrade_backing_pr {
                task.dependency_upgrade_backing_pr = dependency_upgrade_backing_pr;
                changed = true;
            }
            if task.issue_type != issue_type {
                task.set_issue_type(issue_type);
                changed = true;
            } else {
                let issue_type_glyph =
                    issue_type.map(|kind| workspace_issue_type_glyph(kind).to_string());
                if task.issue_type_glyph != issue_type_glyph {
                    task.issue_type_glyph = issue_type_glyph;
                    changed = true;
                }
            }
        }
        if snapshot.active_task_id.as_deref() != Some(task_id.as_str()) {
            snapshot.active_task_id = Some(task_id);
            changed = true;
        }
        changed
    });
}

fn queue_issue_task(
    workspace: &Workspace,
    issue: &SelectedIssue,
    backing_pr_url: Option<&str>,
    dependency_upgrade_backing_pr: bool,
    source: WorkspaceTaskSource,
) {
    let issue_type = issue.issue_type();
    workspace.update(|snapshot| {
        if let Some(existing) = snapshot
            .persistent
            .tasks
            .iter()
            .find(|task| task.issue_url == issue.url)
            .map(|task| task.id.clone())
        {
            if let Some(task) = snapshot
                .persistent
                .tasks
                .iter_mut()
                .find(|task| task.id == existing)
            {
                let mut changed = false;
                if task.backing_pr_url.as_deref() != backing_pr_url {
                    task.backing_pr_url = backing_pr_url.map(ToOwned::to_owned);
                    changed = true;
                }
                if task.dependency_upgrade_backing_pr != dependency_upgrade_backing_pr {
                    task.dependency_upgrade_backing_pr = dependency_upgrade_backing_pr;
                    changed = true;
                }
                if task.issue_type != issue_type {
                    task.set_issue_type(issue_type);
                    changed = true;
                } else {
                    let issue_type_glyph =
                        issue_type.map(|kind| workspace_issue_type_glyph(kind).to_string());
                    if task.issue_type_glyph != issue_type_glyph {
                        task.issue_type_glyph = issue_type_glyph;
                        changed = true;
                    }
                }
                return changed;
            }
            return false;
        }
        snapshot.persistent.tasks.push(
            WorkspaceTaskPersistentSnapshot::new(
                format!("task-{}", issue.number),
                issue.url.clone(),
                source,
            )
            .with_backing_pr_url(backing_pr_url.map(ToOwned::to_owned))
            .with_issue_type(issue_type)
            .with_dependency_upgrade_backing_pr(dependency_upgrade_backing_pr),
        );
        true
    });
}

fn clear_automation_issue_claim(workspace: &Workspace, issue_url: &str) {
    workspace.update(|next| {
        let mut changed = false;
        let cleared_task_id = task_id_for_issue(next, issue_url);
        let before = next.persistent.tasks.len();
        next.persistent
            .tasks
            .retain(|task| task.issue_url != issue_url);
        if next.persistent.tasks.len() != before {
            changed = true;
        }
        if let Some(task_id) = cleared_task_id.as_deref()
            && next.task_states.remove(task_id).is_some()
        {
            changed = true;
        }
        let next_active_task_id = if next.active_task_id.as_deref() == cleared_task_id.as_deref() {
            next.persistent.tasks.first().map(|task| task.id.clone())
        } else {
            next.active_task_id
                .as_deref()
                .filter(|task_id| task_persistent_snapshot(next, task_id).is_some())
                .map(ToOwned::to_owned)
        };
        if next.active_task_id != next_active_task_id {
            next.active_task_id = next_active_task_id.clone();
            changed = true;
        }
        let next_active_issue_url = next_active_task_id
            .as_deref()
            .and_then(|task_id| task_issue_url_for_id(next, task_id))
            .map(ToOwned::to_owned);
        if next.persistent.automation_issue != next_active_issue_url {
            next.persistent.automation_issue = next_active_issue_url;
            changed = true;
        }
        if next.automation_session_id.take().is_some() {
            changed = true;
        }
        if next.automation_agent_state.take().is_some() {
            changed = true;
        }
        if next.automation_session_status.take().is_some() {
            changed = true;
        }
        changed
    });
}

fn clear_automation_runtime_state(workspace: &Workspace) {
    workspace.update(|snapshot| {
        let mut changed = false;
        if let Some(active_task_id) = snapshot.active_task_id.clone()
            && let Some(task_state) = snapshot.task_states.get_mut(&active_task_id)
        {
            let mut next_task_state = task_state.clone();
            next_task_state.session_id = None;
            next_task_state.session_status = None;
            next_task_state.agent_state = None;
            next_task_state.waiting_on_vm = false;
            if &next_task_state != task_state {
                *task_state = next_task_state;
                changed = true;
            }
        }
        if snapshot.automation_session_id.take().is_some() {
            changed = true;
        }
        if snapshot.automation_agent_state.take().is_some() {
            changed = true;
        }
        if snapshot.automation_session_status.take().is_some() {
            changed = true;
        }
        changed
    });
}

fn set_paused_automation_state(workspace: &Workspace, repository_label: &str) {
    clear_automation_runtime_state(workspace);
    set_automation_status(workspace, Some(format!("Paused {repository_label}")));
}

fn lease_task_issue(workspace: &Workspace, snapshot: &WorkspaceSnapshot, issue_url: &str) {
    let next_task_id = task_id_for_issue(snapshot, issue_url);
    workspace.update(|next| {
        let mut changed = false;
        if next.persistent.automation_issue.as_deref() != Some(issue_url) {
            next.persistent.automation_issue = Some(issue_url.to_string());
            changed = true;
        }
        if next.active_task_id != next_task_id {
            next.active_task_id = next_task_id.clone();
            next.automation_session_id = None;
            next.automation_agent_state = None;
            next.automation_session_status = None;
            changed = true;
        }
        changed
    });
}

fn sync_task_runtime_state(workspace: &Workspace, snapshot: &WorkspaceSnapshot) {
    workspace.update(|next| {
        let mut changed = false;
        let task_ids = snapshot
            .persistent
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<HashSet<_>>();
        let stale_task_ids = next
            .task_states
            .keys()
            .filter(|task_id| !task_ids.contains(task_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for task_id in stale_task_ids {
            if next.task_states.remove(&task_id).is_some() {
                changed = true;
            }
        }

        let resolved_active_task_id = next
            .active_task_id
            .as_deref()
            .filter(|task_id| active_task_lease_must_be_preserved(next, task_id))
            .map(ToOwned::to_owned)
            .or_else(|| {
                let claimed_task_id = snapshot
                    .persistent
                    .automation_issue
                    .as_deref()
                    .and_then(|issue_url| task_id_for_issue(snapshot, issue_url));
                match claimed_task_id.as_deref() {
                    Some(task_id) if snapshot.active_task_id.as_deref() != Some(task_id) => {
                        Some(task_id.to_string())
                    }
                    Some(task_id) if snapshot.active_task_id.is_none() => Some(task_id.to_string()),
                    _ => None,
                }
            });
        if next.active_task_id != resolved_active_task_id {
            next.active_task_id = resolved_active_task_id.clone();
            changed = true;
        }
        let resolved_issue_url = resolved_active_task_id
            .as_deref()
            .and_then(|task_id| task_issue_url_for_id(snapshot, task_id))
            .map(ToOwned::to_owned);
        if next.persistent.automation_issue != resolved_issue_url {
            next.persistent.automation_issue = resolved_issue_url;
            changed = true;
        }
        if next.active_task_id.is_none() {
            if next.automation_session_id.take().is_some() {
                changed = true;
            }
            if next.automation_agent_state.take().is_some() {
                changed = true;
            }
            if next.automation_session_status.take().is_some() {
                changed = true;
            }
        }
        for task in &snapshot.persistent.tasks {
            let is_active = next.active_task_id.as_deref() == Some(task.id.as_str());
            let task_state = next.task_states.entry(task.id.clone()).or_default();
            let normalized_agent_state = normalized_task_agent_state(task_state);
            if task_state.agent_state != normalized_agent_state {
                task_state.agent_state = normalized_agent_state;
                changed = true;
            }
            if is_active {
                if next.automation_agent_state != normalized_agent_state {
                    next.automation_agent_state = normalized_agent_state;
                    changed = true;
                }
                let normalized_session_status = normalized_agent_state.map(agent_state_root_status);
                if next.automation_session_status != normalized_session_status {
                    next.automation_session_status = normalized_session_status;
                    changed = true;
                }
            }
            let should_wait = task_should_wait_on_vm(is_active, normalized_agent_state);
            if task_state.waiting_on_vm != should_wait {
                task_state.waiting_on_vm = should_wait;
                changed = true;
            }
        }
        changed
    });
}

fn active_task_lease_must_be_preserved(snapshot: &WorkspaceSnapshot, task_id: &str) -> bool {
    task_persistent_snapshot(snapshot, task_id).is_some()
        && snapshot
            .task_states
            .get(task_id)
            .is_some_and(|task_state| !task_can_yield_vm(normalized_task_agent_state(task_state)))
}

fn active_task_id_for_snapshot(snapshot: &WorkspaceSnapshot) -> Option<String> {
    snapshot.resolved_active_task_id()
}

fn active_issue_url_for_snapshot(snapshot: &WorkspaceSnapshot) -> Option<String> {
    snapshot.resolved_active_issue_url()
}

fn task_issue_url_for_id<'a>(snapshot: &'a WorkspaceSnapshot, task_id: &str) -> Option<&'a str> {
    snapshot.task_issue_url_for_id(task_id)
}

fn task_persistent_snapshot<'a>(
    snapshot: &'a WorkspaceSnapshot,
    task_id: &str,
) -> Option<&'a WorkspaceTaskPersistentSnapshot> {
    snapshot.task_persistent_snapshot(task_id)
}

fn task_persistent_snapshot_for_issue<'a>(
    snapshot: &'a WorkspaceSnapshot,
    issue_url: &str,
) -> Option<&'a WorkspaceTaskPersistentSnapshot> {
    snapshot
        .persistent
        .tasks
        .iter()
        .find(|task| task.issue_url == issue_url)
}

fn set_automation_runtime_state(
    workspace: &Workspace,
    session_id: Option<String>,
    session_status: Option<RootSessionStatus>,
) {
    workspace.update(|snapshot| {
        let mut changed = false;
        if snapshot.automation_session_id != session_id {
            snapshot.automation_session_id = session_id.clone();
            changed = true;
        }
        let next_agent_state = session_status.map(root_status_to_agent_state);
        if snapshot.automation_agent_state != next_agent_state {
            snapshot.automation_agent_state = next_agent_state;
            changed = true;
        }
        if snapshot.automation_session_status != session_status {
            snapshot.automation_session_status = session_status;
            changed = true;
        }
        if let Some(active_task_id) = snapshot.active_task_id.clone() {
            let task_state = snapshot.task_states.entry(active_task_id).or_default();
            let can_update_task_state =
                task_state.session_id.is_none() || task_state.session_id == session_id;
            if can_update_task_state {
                if task_state.session_id != session_id {
                    task_state.session_id = session_id.clone();
                    changed = true;
                }
                if task_state.session_status != session_status {
                    task_state.session_status = session_status;
                    changed = true;
                }
                if task_state.agent_state != next_agent_state {
                    task_state.agent_state = next_agent_state;
                    changed = true;
                }
            }
            if task_state.waiting_on_vm {
                task_state.waiting_on_vm = false;
                changed = true;
            }
        }
        changed
    });
}

fn reserved_issue_urls(
    service: &CombinedService,
    current_workspace_key: &str,
    current_snapshot: Option<&WorkspaceSnapshot>,
) -> HashSet<String> {
    let workspace_keys = service.manager.subscribe().borrow().clone();
    let mut urls = HashSet::new();
    if let Some(snapshot) = current_snapshot {
        urls.extend(
            snapshot
                .persistent
                .tasks
                .iter()
                .map(|task| task.issue_url.clone()),
        );
        urls.extend(snapshot.persistent.ignored_issue_urls.iter().cloned());
    }
    for key in workspace_keys {
        if key == current_workspace_key {
            continue;
        }
        let Ok(workspace) = service.manager.get_workspace(&key) else {
            continue;
        };
        let snapshot = workspace.subscribe().borrow().clone();
        urls.extend(
            snapshot
                .persistent
                .tasks
                .into_iter()
                .map(|task| task.issue_url),
        );
        urls.extend(snapshot.persistent.custom_links.issue);
        urls.extend(snapshot.persistent.agent_provided.issue);
    }
    urls
}

fn next_schedulable_task_issue_url(
    snapshot: &WorkspaceSnapshot,
    exclude_issue_url: Option<&str>,
) -> Option<String> {
    snapshot
        .persistent
        .tasks
        .iter()
        .find(|task| {
            if Some(task.issue_url.as_str()) == exclude_issue_url {
                return false;
            }
            let Some(task_state) = snapshot.task_states.get(&task.id) else {
                return true;
            };
            if task_state.waiting_on_vm {
                return true;
            }
            !matches!(
                normalized_task_agent_state(task_state),
                Some(
                    AutomationAgentState::Working
                        | AutomationAgentState::Question
                        | AutomationAgentState::Review
                        | AutomationAgentState::Idle
                )
            )
        })
        .map(|task| task.issue_url.clone())
}

fn task_can_yield_vm(agent_state: Option<AutomationAgentState>) -> bool {
    matches!(
        agent_state,
        Some(
            AutomationAgentState::Question
                | AutomationAgentState::Review
                | AutomationAgentState::Idle
                | AutomationAgentState::Stale
        )
    )
}

fn task_should_wait_on_vm(is_active: bool, agent_state: Option<AutomationAgentState>) -> bool {
    !is_active && !task_can_yield_vm(agent_state)
}

fn active_task_can_yield_vm(snapshot: &WorkspaceSnapshot) -> bool {
    let Some(active_task_id) = snapshot.active_task_id.as_deref() else {
        return false;
    };
    let Some(task_state) = snapshot.task_states.get(active_task_id) else {
        return false;
    };
    if task_state.session_id.is_none() {
        return false;
    }
    task_can_yield_vm(normalized_task_agent_state(task_state))
}

fn normalized_task_agent_state(
    task_state: &crate::WorkspaceTaskRuntimeSnapshot,
) -> Option<AutomationAgentState> {
    match task_state.session_status {
        Some(RootSessionStatus::Question) => Some(AutomationAgentState::Question),
        Some(RootSessionStatus::Idle) if task_state.session_id.is_some() => {
            Some(AutomationAgentState::Review)
        }
        Some(RootSessionStatus::Idle) => Some(AutomationAgentState::Idle),
        Some(RootSessionStatus::Busy)
            if matches!(
                task_state.agent_state,
                Some(AutomationAgentState::WaitingOnVm)
            ) =>
        {
            Some(AutomationAgentState::Working)
        }
        Some(RootSessionStatus::Busy) => task_state.agent_state,
        None => match task_state.agent_state {
            Some(AutomationAgentState::WaitingOnVm) => None,
            other => other,
        },
    }
}

fn task_id_for_issue(snapshot: &WorkspaceSnapshot, issue_url: &str) -> Option<String> {
    snapshot
        .persistent
        .tasks
        .iter()
        .find(|task| task.issue_url == issue_url)
        .map(|task| task.id.clone())
}

fn should_start_assigned_issue_work(
    snapshot: &WorkspaceSnapshot,
    root_status: RootSessionStatus,
) -> bool {
    if !matches!(root_status, RootSessionStatus::Idle) {
        return false;
    }

    let Some(active_task_id) = snapshot
        .active_task_id
        .clone()
        .or_else(|| active_task_id_for_snapshot(snapshot))
    else {
        return false;
    };

    match snapshot.task_states.get(&active_task_id) {
        None => true,
        Some(task_state) => {
            if task_state.session_id.is_none() {
                return true;
            }
            if task_state.agent_state != Some(AutomationAgentState::Stale) {
                return false;
            }

            !snapshot
                .persistent
                .tasks
                .iter()
                .find(|task| task.id == active_task_id)
                .is_some_and(|task| {
                    task.backing_pr_url.is_some() && !task.dependency_upgrade_backing_pr
                })
        }
    }
}

fn should_bridge_root_runtime_state(
    agent_provider: AgentProvider,
    snapshot: &WorkspaceSnapshot,
    should_resume_assigned_issue: bool,
    root_status: RootSessionStatus,
) -> bool {
    if agent_provider == AgentProvider::Codex {
        return false;
    }
    !should_resume_assigned_issue
        && snapshot.automation_session_id.is_none()
        && snapshot.root_session_id.is_some()
        && !matches!(root_status, RootSessionStatus::Idle)
}

fn set_automation_status(workspace: &Workspace, next_status: Option<String>) {
    workspace.update(|snapshot| {
        if snapshot.automation_status != next_status {
            snapshot.automation_status = next_status.clone();
            true
        } else {
            false
        }
    });
}

fn effective_automation_agent_state(snapshot: &WorkspaceSnapshot) -> Option<AutomationAgentState> {
    snapshot
        .active_task_id
        .as_deref()
        .and_then(|task_id| snapshot.task_states.get(task_id))
        .and_then(|state| state.agent_state)
        .or(snapshot.automation_agent_state)
        .or_else(|| snapshot.root_session_status.map(root_status_to_agent_state))
}

fn root_status_to_agent_state(status: RootSessionStatus) -> AutomationAgentState {
    match status {
        RootSessionStatus::Busy => AutomationAgentState::Working,
        RootSessionStatus::Question => AutomationAgentState::Question,
        RootSessionStatus::Idle => AutomationAgentState::Idle,
    }
}

fn agent_state_root_status(state: AutomationAgentState) -> RootSessionStatus {
    match state {
        AutomationAgentState::Working => RootSessionStatus::Busy,
        AutomationAgentState::Question => RootSessionStatus::Question,
        AutomationAgentState::WaitingOnVm
        | AutomationAgentState::Review
        | AutomationAgentState::Idle
        | AutomationAgentState::Stale => RootSessionStatus::Idle,
    }
}

#[derive(Debug, QueryableByName)]
struct CodexStateThreadRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    cwd: String,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    updated_at: i64,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    has_user_event: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRecoveredThreadCandidate {
    id: String,
    cwd: String,
    sort_key: String,
    has_user_event: bool,
    autonomous_issue_urls: Vec<String>,
    autonomous_pr_urls: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CodexTaskMetadata {
    repositories: Vec<String>,
    issues: Vec<String>,
    prs: Vec<String>,
}

fn codex_session_log_root(workspace_directory_path: &Path, workspace_key: &str) -> PathBuf {
    workspace_directory_path
        .join(".multicode")
        .join("codex")
        .join(workspace_key)
        .join("home")
        .join("sessions")
}

fn find_codex_session_log_path(
    workspace_directory_path: &Path,
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

fn codex_usage_total_tokens_from_session_log_contents(contents: &str) -> Option<u64> {
    contents
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            let payload = value.get("payload")?;
            if payload.get("type").and_then(serde_json::Value::as_str) != Some("token_count") {
                return None;
            }
            payload
                .get("info")
                .and_then(|info| info.get("total_token_usage"))
                .and_then(|usage| usage.get("total_tokens"))
                .and_then(serde_json::Value::as_u64)
        })
        .last()
}

async fn read_codex_task_usage_total_tokens(
    workspace_directory_path: PathBuf,
    workspace_key: String,
    session_id: String,
) -> Option<u64> {
    spawn_blocking(move || {
        let path =
            find_codex_session_log_path(&workspace_directory_path, &workspace_key, &session_id)?;
        let contents = std::fs::read_to_string(path).ok()?;
        codex_usage_total_tokens_from_session_log_contents(&contents)
    })
    .await
    .ok()
    .flatten()
}

async fn recover_codex_task_sessions(
    service: &CombinedService,
    workspace: &Workspace,
    snapshot: &WorkspaceSnapshot,
    workspace_key: &str,
    assigned_repository: &str,
) {
    if service.agent_provider() != AgentProvider::Codex {
        return;
    }

    let missing_tasks = snapshot
        .persistent
        .tasks
        .iter()
        .filter(|task| {
            snapshot
                .task_states
                .get(&task.id)
                .and_then(|state| state.session_id.as_deref())
                .is_none()
        })
        .map(|task| CodexTaskRecoveryDescriptor {
            task_id: task.id.clone(),
            cwd: task_cwd_path(service, workspace_key, assigned_repository, &task.issue_url)
                .to_string_lossy()
                .into_owned(),
            issue_url: task.issue_url.clone(),
            backing_pr_url: task.backing_pr_url.clone(),
        })
        .collect::<Vec<_>>();
    if missing_tasks.is_empty() {
        return;
    }

    let codex_home = synthetic_codex_home_source(service.workspace_directory_path(), workspace_key);
    let recovered =
        spawn_blocking(move || recover_codex_thread_ids_for_tasks(&codex_home, &missing_tasks))
            .await
            .ok()
            .and_then(Result::ok);
    let Some(recovered) = recovered else {
        return;
    };
    if recovered.is_empty() {
        return;
    }

    workspace.update(|next| {
        let mut changed = false;
        for (task_id, session_id) in &recovered {
            let task_state = next.task_states.entry(task_id.clone()).or_default();
            if task_state.session_id.as_deref() != Some(session_id.as_str()) {
                task_state.session_id = Some(session_id.clone());
                changed = true;
            }
        }
        changed
    });
}

async fn reconcile_codex_task_runtime_states(
    service: &CombinedService,
    workspace: &Workspace,
    snapshot: &WorkspaceSnapshot,
    workspace_key: &str,
    assigned_repository: &str,
) {
    if service.agent_provider() != AgentProvider::Codex {
        return;
    }

    let Some(uri) = snapshot
        .transient
        .as_ref()
        .map(|transient| transient.uri.clone())
    else {
        return;
    };

    let _ = (workspace_key, assigned_repository);
    let client = CodexAppServerClient::new(uri);
    let active_task_id = snapshot.active_task_id.as_deref();
    let mut clear_state_file = false;
    for task in &snapshot.persistent.tasks {
        let Some(task_state) = snapshot.task_states.get(&task.id) else {
            continue;
        };
        let Some(task_session_id) = task_state.session_id.clone() else {
            continue;
        };
        let preserve_review = task.backing_pr_url.is_some() && !task.dependency_upgrade_backing_pr;
        let task_cwd = task_cwd_path(service, workspace_key, assigned_repository, &task.issue_url);

        let response = match client.thread_read_with_turns(&task_session_id, true).await {
            Ok(response) => response,
            Err(error) => {
                if !task_session_id_is_current(workspace, &task.id, &task_session_id) {
                    continue;
                }
                if (error.contains("thread not loaded") || error.contains("thread not found"))
                    && let Some(recovered_session_id) = recover_latest_codex_task_session_id(
                        service.workspace_directory_path(),
                        workspace_key,
                        &task_cwd,
                        &task.issue_url,
                        task.backing_pr_url.as_deref(),
                        Some(&task_session_id),
                    )
                    .await
                {
                    tracing::info!(
                        workspace_key,
                        task_id = %task.id,
                        issue_url = %task.issue_url,
                        previous_task_session_id = %task_session_id,
                        recovered_task_session_id = %recovered_session_id,
                        cwd = %task_cwd.display(),
                        "recovered newer codex task session from persisted session history"
                    );
                    workspace.update(|snapshot| {
                        let task_state = snapshot.task_states.entry(task.id.clone()).or_default();
                        if task_state.session_id.as_deref() == Some(recovered_session_id.as_str()) {
                            false
                        } else {
                            task_state.session_id = Some(recovered_session_id.clone());
                            true
                        }
                    });
                    continue;
                }
                if error.contains("thread not loaded") {
                    let (session_status, agent_state) =
                        unloaded_codex_task_runtime(task_state, preserve_review);
                    set_task_runtime_state_from_codex(
                        workspace,
                        &task.id,
                        &task_session_id,
                        session_status,
                        agent_state,
                        Some(CodexTaskMetadata {
                            issues: vec![task.issue_url.clone()],
                            ..Default::default()
                        }),
                        None,
                    );
                    write_authoritative_task_state_file(
                        service.workspace_directory_path(),
                        workspace_key,
                        &task.id,
                        &task_session_id,
                        agent_state,
                    )
                    .await;
                }
                continue;
            }
        };
        let Some(status) = response.thread.status.as_ref() else {
            continue;
        };
        if matches!(
            status,
            super::codex_app_server::CodexThreadStatus::NotLoaded
        ) && let Some(recovered_session_id) = recover_latest_codex_task_session_id(
            service.workspace_directory_path(),
            workspace_key,
            &task_cwd,
            &task.issue_url,
            task.backing_pr_url.as_deref(),
            Some(&task_session_id),
        )
        .await
        {
            tracing::info!(
                workspace_key,
                task_id = %task.id,
                issue_url = %task.issue_url,
                previous_task_session_id = %task_session_id,
                recovered_task_session_id = %recovered_session_id,
                cwd = %task_cwd.display(),
                "recovered newer codex task session from persisted session history after not-loaded status"
            );
            workspace.update(|snapshot| {
                let task_state = snapshot.task_states.entry(task.id.clone()).or_default();
                if task_state.session_id.as_deref() == Some(recovered_session_id.as_str()) {
                    false
                } else {
                    task_state.session_id = Some(recovered_session_id.clone());
                    true
                }
            });
            continue;
        }

        let (next_session_status, next_agent_state) = if matches!(
            status,
            super::codex_app_server::CodexThreadStatus::NotLoaded
        ) {
            unloaded_codex_task_runtime(task_state, preserve_review)
        } else {
            codex_runtime_state_for_task(status, task_state)
        };
        let usage_total_tokens = read_codex_task_usage_total_tokens(
            service.workspace_directory_path().to_path_buf(),
            workspace_key.to_string(),
            task_session_id.clone(),
        )
        .await;
        tracing::warn!(
            workspace_key,
            task_id = %task.id,
            issue_url = %task.issue_url,
            task_session_id = %task_session_id,
            previous_agent_state = ?task_state.agent_state,
            previous_session_status = ?task_state.session_status,
            status = ?status,
            next_agent_state = ?next_agent_state,
            next_session_status = ?next_session_status,
            "reconciled codex task runtime state"
        );
        if !task_session_id_is_current(workspace, &task.id, &task_session_id) {
            tracing::info!(
                workspace_key,
                task_id = %task.id,
                task_session_id = %task_session_id,
                "skipping stale codex task reconciliation update because task session changed"
            );
            continue;
        }
        let metadata = codex_task_metadata_from_turns(&response.thread.turns, &task.issue_url);
        let should_refresh_after_review = should_refresh_task_links_after_codex_review(
            task_state,
            next_session_status,
            next_agent_state,
        );
        let refresh_urls = should_refresh_after_review
            .then(|| task_link_refresh_urls(task, task_state, &metadata))
            .unwrap_or_default();
        set_task_runtime_state_from_codex(
            workspace,
            &task.id,
            &task_session_id,
            next_session_status,
            next_agent_state,
            Some(metadata),
            usage_total_tokens,
        );
        if should_refresh_after_review {
            if let Some(backing_pr_url) = refresh_task_backing_pr_url_after_codex_review(
                service,
                workspace,
                assigned_repository,
                &task.id,
                &task.issue_url,
            )
            .await
            {
                let _ = service
                    .github_status_service()
                    .request_refresh(&backing_pr_url);
            }
            request_refresh_for_task_links(&service, refresh_urls.iter().map(String::as_str));
        }
        write_authoritative_task_state_file(
            service.workspace_directory_path(),
            workspace_key,
            &task.id,
            &task_session_id,
            next_agent_state,
        )
        .await;
        if active_task_id == Some(task.id.as_str())
            && next_agent_state != AutomationAgentState::Working
        {
            clear_state_file = true;
        }
    }

    if clear_state_file {
        clear_automation_state_file(service, workspace_key).await;
    }
}

fn task_session_id_is_current(workspace: &Workspace, task_id: &str, session_id: &str) -> bool {
    workspace
        .subscribe()
        .borrow()
        .task_states
        .get(task_id)
        .and_then(|task_state| task_state.session_id.as_deref())
        == Some(session_id)
}

async fn write_authoritative_task_state_file(
    workspace_directory_path: &Path,
    workspace_key: &str,
    task_id: &str,
    task_session_id: &str,
    agent_state: AutomationAgentState,
) {
    let line = format!(
        "{}:{task_session_id}\n",
        authoritative_task_state_label(agent_state)
    );
    let path = automation_task_state_file_source(workspace_directory_path, workspace_key, task_id);
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(path, line).await;
}

fn authoritative_task_state_label(agent_state: AutomationAgentState) -> &'static str {
    match agent_state {
        AutomationAgentState::Working | AutomationAgentState::WaitingOnVm => "working",
        AutomationAgentState::Question => "question",
        AutomationAgentState::Review => "review",
        AutomationAgentState::Idle => "idle",
        AutomationAgentState::Stale => "stale",
    }
}

fn latest_codex_state_db_path(codex_home: &Path) -> Option<PathBuf> {
    let mut candidates = std::fs::read_dir(codex_home)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            if !file_type.is_file() {
                return None;
            }
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            (file_name.starts_with("state_") && file_name.ends_with(".sqlite")).then(|| {
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok());
                (modified, entry.path())
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    candidates.pop().map(|(_, path)| path)
}

fn latest_codex_sessions_dir(codex_home: &Path) -> PathBuf {
    codex_home.join("sessions")
}

#[derive(Debug)]
struct DisposableDirectory {
    path: PathBuf,
}

impl DisposableDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for DisposableDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct CodexStateDbSnapshot {
    database_path: PathBuf,
}

#[derive(Debug)]
struct CachedCodexStateDbSnapshot {
    fingerprint: CodexStateDbFingerprint,
    _directory: DisposableDirectory,
    database_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexStateDbFingerprint {
    database: CodexStateDbFileState,
    wal: Option<CodexStateDbFileState>,
    shm: Option<CodexStateDbFileState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexStateDbFileState {
    len: u64,
    modified: Option<SystemTime>,
}

static CODEX_STATE_DB_SNAPSHOT_CACHE: OnceLock<
    Mutex<HashMap<PathBuf, CachedCodexStateDbSnapshot>>,
> = OnceLock::new();

fn codex_state_db_snapshot_cache() -> &'static Mutex<HashMap<PathBuf, CachedCodexStateDbSnapshot>> {
    CODEX_STATE_DB_SNAPSHOT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn copy_codex_state_db_sidecar(
    state_db_path: &Path,
    suffix: &str,
    snapshot_db_path: &Path,
) -> Result<(), String> {
    let source = state_db_path.with_extension(format!(
        "{}{suffix}",
        state_db_path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
    ));
    if !source.exists() {
        return Ok(());
    }
    let target = snapshot_db_path.with_extension(format!(
        "{}{suffix}",
        snapshot_db_path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
    ));
    std::fs::copy(&source, &target).map_err(|error| {
        format!(
            "failed to copy Codex state database sidecar '{}' to '{}': {error}",
            source.display(),
            target.display()
        )
    })?;
    Ok(())
}

fn codex_state_db_sidecar_path(state_db_path: &Path, suffix: &str) -> PathBuf {
    state_db_path.with_extension(format!(
        "{}{suffix}",
        state_db_path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
    ))
}

fn codex_state_db_file_state(path: &Path) -> Result<Option<CodexStateDbFileState>, String> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(CodexStateDbFileState {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "failed to read metadata for '{}': {error}",
            path.display()
        )),
    }
}

fn codex_state_db_fingerprint(state_db_path: &Path) -> Result<CodexStateDbFingerprint, String> {
    let database = codex_state_db_file_state(state_db_path)?.ok_or_else(|| {
        format!(
            "Codex state database '{}' does not exist",
            state_db_path.display()
        )
    })?;
    let wal = codex_state_db_file_state(&codex_state_db_sidecar_path(state_db_path, "-wal"))?;
    let shm = codex_state_db_file_state(&codex_state_db_sidecar_path(state_db_path, "-shm"))?;
    Ok(CodexStateDbFingerprint { database, wal, shm })
}

fn snapshot_codex_state_db(state_db_path: &Path) -> Result<CodexStateDbSnapshot, String> {
    let fingerprint = codex_state_db_fingerprint(state_db_path)?;
    let cache = codex_state_db_snapshot_cache();
    let database_key = state_db_path.to_path_buf();
    if let Some(database_path) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&database_key)
        .and_then(|snapshot| {
            (snapshot.fingerprint == fingerprint).then(|| snapshot.database_path.clone())
        })
    {
        return Ok(CodexStateDbSnapshot { database_path });
    }

    let Some(file_name) = state_db_path.file_name() else {
        return Err(format!(
            "Codex state database path '{}' has no file name",
            state_db_path.display()
        ));
    };
    let snapshot_dir =
        std::env::temp_dir().join(format!("multicode-codex-state-db-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&snapshot_dir).map_err(|error| {
        format!(
            "failed to create Codex state database snapshot dir '{}': {error}",
            snapshot_dir.display()
        )
    })?;
    let snapshot_dir = DisposableDirectory::new(snapshot_dir);
    let snapshot_db_path = snapshot_dir.path.join(file_name);
    std::fs::copy(state_db_path, &snapshot_db_path).map_err(|error| {
        format!(
            "failed to copy Codex state database '{}' to '{}': {error}",
            state_db_path.display(),
            snapshot_db_path.display()
        )
    })?;
    copy_codex_state_db_sidecar(state_db_path, "-wal", &snapshot_db_path)?;
    copy_codex_state_db_sidecar(state_db_path, "-shm", &snapshot_db_path)?;

    let database_path = snapshot_db_path.clone();
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            database_key,
            CachedCodexStateDbSnapshot {
                fingerprint,
                _directory: snapshot_dir,
                database_path: snapshot_db_path,
            },
        );

    Ok(CodexStateDbSnapshot { database_path })
}

fn recover_latest_codex_thread_candidate_for_cwd(
    codex_home: &Path,
    cwd: &str,
) -> Option<CodexRecoveredThreadCandidate> {
    let mut candidate =
        recover_codex_thread_candidates_from_state_db(codex_home, &[cwd.to_string()])
            .ok()
            .and_then(|mut entries| entries.remove(cwd));

    let log_candidate =
        recover_codex_thread_candidates_from_session_logs(codex_home, &[cwd.to_string()])
            .remove(cwd)
            .and_then(|candidates| {
                candidates.into_iter().reduce(|current, next| {
                    if should_prefer_recovered_candidate(Some(&next), Some(&current)) {
                        next
                    } else {
                        current
                    }
                })
            });
    if should_prefer_recovered_candidate(log_candidate.as_ref(), candidate.as_ref()) {
        candidate = log_candidate;
    }

    candidate
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexTaskRecoveryDescriptor {
    task_id: String,
    cwd: String,
    issue_url: String,
    backing_pr_url: Option<String>,
}

fn recover_codex_thread_ids_for_tasks(
    codex_home: &Path,
    tasks: &[CodexTaskRecoveryDescriptor],
) -> Result<HashMap<String, String>, String> {
    Ok(tasks
        .iter()
        .filter_map(|task| {
            recover_latest_codex_task_thread_candidate(codex_home, task)
                .map(|candidate| (task.task_id.clone(), candidate.id))
        })
        .collect())
}

fn recover_latest_codex_task_thread_candidate(
    codex_home: &Path,
    task: &CodexTaskRecoveryDescriptor,
) -> Option<CodexRecoveredThreadCandidate> {
    let mut owned_candidate = None;
    let log_candidates = recover_codex_thread_candidates_from_session_logs(
        codex_home,
        std::slice::from_ref(&task.cwd),
    );
    if let Some(candidates) = log_candidates.get(task.cwd.as_str()) {
        for candidate in candidates {
            if recovered_candidate_matches_task(candidate, task)
                && should_prefer_recovered_candidate(Some(candidate), owned_candidate.as_ref())
            {
                owned_candidate = Some(candidate.clone());
            }
        }
    }
    if owned_candidate.is_some() {
        return owned_candidate;
    }

    recover_latest_codex_thread_candidate_for_cwd(codex_home, &task.cwd)
}

fn recover_codex_thread_candidates_from_state_db(
    codex_home: &Path,
    task_cwds: &[String],
) -> Result<HashMap<String, CodexRecoveredThreadCandidate>, String> {
    let Some(state_db_path) = latest_codex_state_db_path(codex_home) else {
        return Ok(HashMap::new());
    };
    let snapshot = snapshot_codex_state_db(&state_db_path)?;
    let database_url = snapshot.database_path.to_string_lossy().into_owned();
    let mut connection =
        SqliteConnection::establish(&database_url).map_err(|error| error.to_string())?;
    connection
        .batch_execute("PRAGMA query_only = 1; PRAGMA mmap_size = 0;")
        .map_err(|error| error.to_string())?;
    let rows = sql_query(
        "SELECT id, cwd, updated_at, has_user_event \
         FROM threads \
         WHERE archived = 0 \
         ORDER BY updated_at DESC, created_at DESC",
    )
    .load::<CodexStateThreadRow>(&mut connection)
    .map_err(|error| error.to_string())?;

    let mut threads_by_cwd = HashMap::<String, CodexRecoveredThreadCandidate>::new();
    for row in rows {
        if !task_cwds.iter().any(|cwd| cwd == &row.cwd) {
            continue;
        }
        let row_candidate = CodexRecoveredThreadCandidate {
            id: row.id.clone(),
            cwd: row.cwd.clone(),
            sort_key: format!("{:020}", row.updated_at),
            has_user_event: row.has_user_event != 0,
            autonomous_issue_urls: Vec::new(),
            autonomous_pr_urls: Vec::new(),
        };
        let prefer_row = match threads_by_cwd.get(&row.cwd) {
            None => true,
            Some(existing) => {
                should_prefer_recovered_candidate(Some(&row_candidate), Some(existing))
            }
        };
        if prefer_row {
            threads_by_cwd.insert(row.cwd.clone(), row_candidate);
        }
    }

    Ok(threads_by_cwd)
}

fn recover_codex_thread_candidates_from_session_logs(
    codex_home: &Path,
    task_cwds: &[String],
) -> HashMap<String, Vec<CodexRecoveredThreadCandidate>> {
    let sessions_dir = latest_codex_sessions_dir(codex_home);
    let mut results = HashMap::<String, Vec<CodexRecoveredThreadCandidate>>::new();
    let mut stack = vec![sessions_dir];

    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !file_type.is_file()
                || entry.path().extension().and_then(|ext| ext.to_str()) != Some("jsonl")
            {
                continue;
            }
            let Some(candidate) = recover_codex_thread_candidate_from_session_log(&entry.path())
            else {
                continue;
            };
            if !task_cwds.iter().any(|cwd| cwd == &candidate.cwd) {
                continue;
            }
            results
                .entry(candidate.cwd.clone())
                .or_default()
                .push(candidate);
        }
    }

    results
}

fn recover_codex_thread_candidate_from_session_log(
    path: &Path,
) -> Option<CodexRecoveredThreadCandidate> {
    let contents = std::fs::read_to_string(path).ok()?;
    let first_line = contents.lines().next()?;
    let value: serde_json::Value = serde_json::from_str(first_line).ok()?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    let id = payload
        .get("id")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let cwd = payload
        .get("cwd")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let timestamp = payload
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut has_user_event = false;
    let mut autonomous_issue_urls = Vec::new();
    let mut autonomous_pr_urls = Vec::new();
    for line in contents.lines().skip(1) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(text) = codex_session_log_user_text(&value) else {
            continue;
        };
        has_user_event = true;
        if !is_autonomous_multicode_task_prompt(text) {
            continue;
        }
        let (issue_urls, pr_urls) = extract_github_issue_and_pr_urls(text);
        autonomous_issue_urls.extend(issue_urls);
        autonomous_pr_urls.extend(pr_urls);
    }
    Some(CodexRecoveredThreadCandidate {
        id,
        cwd,
        sort_key: timestamp,
        has_user_event,
        autonomous_issue_urls,
        autonomous_pr_urls,
    })
}

fn codex_session_log_user_text<'a>(value: &'a serde_json::Value) -> Option<&'a str> {
    if value.get("type").and_then(serde_json::Value::as_str) == Some("event_msg") {
        let payload = value.get("payload")?;
        if payload.get("type").and_then(serde_json::Value::as_str) == Some("user_message") {
            return payload.get("message").and_then(serde_json::Value::as_str);
        }
    }

    if value.get("type").and_then(serde_json::Value::as_str) == Some("response_item") {
        let payload = value.get("payload")?;
        if payload.get("type").and_then(serde_json::Value::as_str) != Some("message")
            || payload.get("role").and_then(serde_json::Value::as_str) != Some("user")
        {
            return None;
        }
        let content = payload.get("content")?.as_array()?;
        return content.iter().find_map(|item| {
            (item.get("type").and_then(serde_json::Value::as_str) == Some("input_text"))
                .then(|| item.get("text").and_then(serde_json::Value::as_str))
                .flatten()
        });
    }

    None
}

fn is_autonomous_multicode_task_prompt(text: &str) -> bool {
    text.contains("You are operating in an autonomous multicode workspace")
        && text.contains("Start work on GitHub issue ")
}

fn extract_github_issue_and_pr_urls(text: &str) -> (Vec<String>, Vec<String>) {
    let mut issues = HashSet::new();
    let mut prs = HashSet::new();
    for token in text.split_whitespace() {
        let token = token.trim_matches(|ch: char| {
            matches!(
                ch,
                '"' | '\'' | '`' | ',' | '.' | ';' | ':' | '(' | ')' | '[' | ']'
            )
        });
        if let Some(issue_url) = normalize_github_issue_url(token) {
            issues.insert(issue_url);
        } else if let Some(pr_url) = normalize_github_pull_request_url(token) {
            prs.insert(pr_url);
        }
    }

    (issues.into_iter().collect(), prs.into_iter().collect())
}

fn normalize_github_issue_url(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))?;
    let segments = rest
        .split('/')
        .filter(|segment| !segment.trim().is_empty())
        .map(|segment| segment.trim())
        .collect::<Vec<_>>();
    let [owner, repo, kind, number, ..] = segments.as_slice() else {
        return None;
    };
    if *kind != "issues" {
        return None;
    }
    let repository = normalize_github_repository_spec(&format!("{owner}/{repo}"))?;
    let number = number.parse::<u64>().ok()?;
    Some(format!("https://github.com/{repository}/issues/{number}"))
}

fn normalize_github_pull_request_url(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('/');
    let rest = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))?;
    let segments = rest
        .split('/')
        .filter(|segment| !segment.trim().is_empty())
        .map(|segment| segment.trim())
        .collect::<Vec<_>>();
    let [owner, repo, kind, number, ..] = segments.as_slice() else {
        return None;
    };
    if *kind != "pull" {
        return None;
    }
    let repository = normalize_github_repository_spec(&format!("{owner}/{repo}"))?;
    let number = number.parse::<u64>().ok()?;
    Some(format!("https://github.com/{repository}/pull/{number}"))
}

fn recovered_candidate_matches_task(
    candidate: &CodexRecoveredThreadCandidate,
    task: &CodexTaskRecoveryDescriptor,
) -> bool {
    candidate.cwd == task.cwd
        && (candidate
            .autonomous_issue_urls
            .iter()
            .any(|issue_url| issue_url == &task.issue_url)
            || task.backing_pr_url.as_ref().is_some_and(|backing_pr_url| {
                candidate
                    .autonomous_pr_urls
                    .iter()
                    .any(|pr_url| pr_url == backing_pr_url)
            }))
}

fn should_prefer_recovered_candidate(
    next: Option<&CodexRecoveredThreadCandidate>,
    current: Option<&CodexRecoveredThreadCandidate>,
) -> bool {
    match (next, current) {
        (Some(_), None) => true,
        (Some(next), Some(current)) => {
            (next.has_user_event && !current.has_user_event)
                || (next.has_user_event == current.has_user_event
                    && next.sort_key > current.sort_key)
        }
        _ => false,
    }
}

async fn recover_latest_codex_task_session_id(
    workspace_directory_path: &Path,
    workspace_key: &str,
    task_cwd: &Path,
    task_issue_url: &str,
    backing_pr_url: Option<&str>,
    current_session_id: Option<&str>,
) -> Option<String> {
    let codex_home = synthetic_codex_home_source(workspace_directory_path, workspace_key);
    let task = CodexTaskRecoveryDescriptor {
        task_id: String::new(),
        cwd: task_cwd.to_string_lossy().into_owned(),
        issue_url: task_issue_url.to_string(),
        backing_pr_url: backing_pr_url.map(ToOwned::to_owned),
    };
    let current_session_id = current_session_id.map(ToOwned::to_owned);
    spawn_blocking(move || {
        recover_latest_codex_task_thread_candidate(&codex_home, &task).and_then(|candidate| {
            (current_session_id.as_deref() != Some(candidate.id.as_str())).then_some(candidate.id)
        })
    })
    .await
    .ok()
    .flatten()
}

fn unloaded_codex_task_runtime(
    task_state: &crate::WorkspaceTaskRuntimeSnapshot,
    preserve_review: bool,
) -> (RootSessionStatus, AutomationAgentState) {
    match task_state.agent_state {
        Some(AutomationAgentState::Question) => {
            (RootSessionStatus::Question, AutomationAgentState::Question)
        }
        Some(AutomationAgentState::Stale) => (RootSessionStatus::Idle, AutomationAgentState::Stale),
        Some(AutomationAgentState::Review | AutomationAgentState::Idle) => {
            (RootSessionStatus::Idle, AutomationAgentState::Review)
        }
        _ if preserve_review => (RootSessionStatus::Idle, AutomationAgentState::Review),
        _ => (RootSessionStatus::Idle, AutomationAgentState::Stale),
    }
}

fn codex_runtime_state_for_task(
    status: &super::codex_app_server::CodexThreadStatus,
    task_state: &crate::WorkspaceTaskRuntimeSnapshot,
) -> (RootSessionStatus, AutomationAgentState) {
    let next_agent_state = codex_task_thread_status_to_agent_state(status, task_state.agent_state);
    (agent_state_root_status(next_agent_state), next_agent_state)
}

fn codex_task_metadata_from_turns(
    turns: &[super::codex_app_server::CodexThreadTurn],
    issue_url: &str,
) -> CodexTaskMetadata {
    let mut repositories = std::collections::BTreeSet::new();
    let mut issues = std::collections::BTreeSet::from([issue_url.to_string()]);
    let mut prs = std::collections::BTreeSet::new();

    for turn in turns {
        for item in &turn.items {
            extend_multicode_tag_values_from_json_value(&mut repositories, item, "repo");
            extend_multicode_tag_values_from_json_value(&mut issues, item, "issue");
            extend_multicode_tag_values_from_json_value(&mut prs, item, "pr");
        }
    }

    CodexTaskMetadata {
        repositories: repositories.into_iter().collect(),
        issues: issues.into_iter().collect(),
        prs: prs.into_iter().collect(),
    }
}

fn extend_multicode_tag_values_from_json_value(
    values: &mut std::collections::BTreeSet<String>,
    item: &serde_json::Value,
    tag: &str,
) {
    match item {
        serde_json::Value::String(text) => {
            values.extend(extract_multicode_tag_values(text, tag));
        }
        serde_json::Value::Array(items) => {
            for value in items {
                extend_multicode_tag_values_from_json_value(values, value, tag);
            }
        }
        serde_json::Value::Object(entries) => {
            for value in entries.values() {
                extend_multicode_tag_values_from_json_value(values, value, tag);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn extract_multicode_tag_values(text: &str, tag: &str) -> Vec<String> {
    let opening = format!("<multicode:{tag}>");
    let closing = format!("</multicode:{tag}>");
    let mut values = Vec::new();
    let mut search_start = 0;

    while let Some(open_index) = text[search_start..].find(&opening) {
        let content_start = search_start + open_index + opening.len();
        let Some(close_index) = text[content_start..].find(&closing) else {
            break;
        };
        let content_end = content_start + close_index;
        let value = text[content_start..content_end].trim();
        if !value.is_empty() {
            values.push(value.to_string());
        }
        search_start = content_end + closing.len();
    }

    values
}

fn codex_task_thread_status_to_agent_state(
    status: &super::codex_app_server::CodexThreadStatus,
    _previous_state: Option<AutomationAgentState>,
) -> AutomationAgentState {
    match status {
        super::codex_app_server::CodexThreadStatus::SystemError => AutomationAgentState::Stale,
        _ if status.waits_for_human_input() => AutomationAgentState::Question,
        _ if status.is_idle() => AutomationAgentState::Review,
        _ => AutomationAgentState::Working,
    }
}

fn set_task_runtime_state_from_codex(
    workspace: &Workspace,
    task_id: &str,
    session_id: &str,
    session_status: RootSessionStatus,
    agent_state: AutomationAgentState,
    metadata: Option<CodexTaskMetadata>,
    usage_total_tokens: Option<u64>,
) {
    workspace.update(|snapshot| {
        let mut changed = false;
        let is_active = snapshot.active_task_id.as_deref() == Some(task_id);
        let task_issue_url = snapshot
            .persistent
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .map(|task| task.issue_url.clone());
        let expected_repository = snapshot.persistent.assigned_repository.clone().or_else(|| {
            task_issue_url
                .as_deref()
                .and_then(github_repository_from_issue_or_pr_url)
        });
        let metadata = metadata.map(|metadata| {
            let assigned_repository = expected_repository.as_deref();
            CodexTaskMetadata {
                repositories: metadata.repositories,
                issues: metadata
                    .issues
                    .into_iter()
                    .filter(|issue_url| {
                        assigned_repository.is_none_or(|repository| {
                            github_issue_or_pr_url_matches_repository(issue_url, repository)
                        })
                    })
                    .collect(),
                prs: metadata
                    .prs
                    .into_iter()
                    .filter(|pr_url| {
                        assigned_repository.is_none_or(|repository| {
                            github_issue_or_pr_url_matches_repository(pr_url, repository)
                        })
                    })
                    .collect(),
            }
        });
        let next_status_text =
            codex_task_status_text(agent_state, metadata.as_ref(), task_id, snapshot);
        if is_active {
            if snapshot.automation_session_id.as_deref() != Some(session_id) {
                snapshot.automation_session_id = Some(session_id.to_string());
                changed = true;
            }
            if snapshot.automation_agent_state != Some(agent_state) {
                snapshot.automation_agent_state = Some(agent_state);
                changed = true;
            }
            if snapshot.automation_session_status != Some(session_status) {
                snapshot.automation_session_status = Some(session_status);
                changed = true;
            }
        }
        let task_state = snapshot.task_states.entry(task_id.to_string()).or_default();
        if task_state.session_id.as_deref() != Some(session_id) {
            task_state.session_id = Some(session_id.to_string());
            changed = true;
        }
        if task_state.agent_state != Some(agent_state) {
            task_state.agent_state = Some(agent_state);
            changed = true;
        }
        if task_state.session_status != Some(session_status) {
            task_state.session_status = Some(session_status);
            changed = true;
        }
        if task_state.status != next_status_text {
            task_state.status = next_status_text.clone();
            changed = true;
        }
        let should_wait = task_should_wait_on_vm(is_active, Some(agent_state));
        if task_state.waiting_on_vm != should_wait {
            task_state.waiting_on_vm = should_wait;
            changed = true;
        }
        if task_state.usage_total_tokens != usage_total_tokens {
            task_state.usage_total_tokens = usage_total_tokens;
            changed = true;
        }
        if let Some(metadata) = metadata {
            let detected_backing_pr_url = metadata.prs.first().cloned();
            if task_state.repository != metadata.repositories {
                task_state.repository = metadata.repositories;
                changed = true;
            }
            if task_state.issue != metadata.issues {
                task_state.issue = metadata.issues;
                changed = true;
            }
            if task_state.pr != metadata.prs {
                task_state.pr = metadata.prs;
                changed = true;
            }
            if let Some(backing_pr_url) = detected_backing_pr_url
                && let Some(task) = snapshot
                    .persistent
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == task_id)
                && task.backing_pr_url.as_deref() != Some(backing_pr_url.as_str())
            {
                task.backing_pr_url = Some(backing_pr_url);
                changed = true;
            }
        }
        changed
    });
}

fn should_refresh_task_links_after_codex_review(
    task_state: &crate::WorkspaceTaskRuntimeSnapshot,
    next_session_status: RootSessionStatus,
    next_agent_state: AutomationAgentState,
) -> bool {
    next_agent_state == AutomationAgentState::Review
        && next_session_status == RootSessionStatus::Idle
        && (task_state.agent_state != Some(AutomationAgentState::Review)
            || task_state.session_status != Some(RootSessionStatus::Idle)
            || task_state.status.as_deref() == Some("Resuming in background"))
}

fn task_link_refresh_urls(
    task: &WorkspaceTaskPersistentSnapshot,
    task_state: &crate::WorkspaceTaskRuntimeSnapshot,
    metadata: &CodexTaskMetadata,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    for url in std::iter::once(task.issue_url.as_str())
        .chain(metadata.prs.iter().map(String::as_str))
        .chain(task_state.pr.iter().map(String::as_str))
        .chain(task.backing_pr_url.iter().map(String::as_str))
    {
        if seen.insert(url.to_string()) {
            urls.push(url.to_string());
        }
    }
    urls
}

async fn refresh_task_backing_pr_url_after_codex_review(
    service: &CombinedService,
    workspace: &Workspace,
    assigned_repository: &str,
    task_id: &str,
    issue_url: &str,
) -> Option<String> {
    let issue_number = issue_number_from_url(issue_url)?;
    let token = match resolved_gh_token(service).await {
        Ok(token) => token,
        Err(err) => {
            tracing::warn!(
                task_id,
                issue_url,
                error = %err,
                "failed to resolve GitHub token while refreshing task PR after codex review"
            );
            return None;
        }
    };
    let open_pull_requests = match list_open_pull_requests(assigned_repository, &token).await {
        Ok(open_pull_requests) => open_pull_requests,
        Err(err) => {
            tracing::warn!(
                task_id,
                issue_url,
                assigned_repository,
                error = %err,
                "failed to list open pull requests while refreshing task PR after codex review"
            );
            return None;
        }
    };
    let issue = SelectedIssue {
        number: issue_number,
        title: String::new(),
        url: issue_url.to_string(),
        created_at: String::new(),
        state: Some("OPEN".to_string()),
        is_pull_request: Some(false),
        body: None,
        labels: Vec::new(),
        dependency_upgrade_pr_urls: Vec::new(),
    };
    let backing_pr_url =
        discover_issue_backing_pr_url(assigned_repository, &issue, &open_pull_requests)?;
    workspace.update(|snapshot| {
        let mut changed = false;
        if let Some(task) = snapshot
            .persistent
            .tasks
            .iter_mut()
            .find(|task| task.id == task_id)
            && task.backing_pr_url.as_deref() != Some(backing_pr_url.as_str())
        {
            task.backing_pr_url = Some(backing_pr_url.clone());
            changed = true;
        }
        if let Some(task_state) = snapshot.task_states.get_mut(task_id)
            && !task_state
                .pr
                .iter()
                .any(|candidate| candidate == &backing_pr_url)
        {
            task_state.pr.insert(0, backing_pr_url.clone());
            changed = true;
        }
        changed
    });
    Some(backing_pr_url)
}

fn codex_task_status_text(
    agent_state: AutomationAgentState,
    metadata: Option<&CodexTaskMetadata>,
    task_id: &str,
    snapshot: &WorkspaceSnapshot,
) -> Option<String> {
    if agent_state != AutomationAgentState::Review {
        return None;
    }

    metadata
        .and_then(|metadata| metadata.prs.first())
        .or_else(|| {
            snapshot
                .task_states
                .get(task_id)
                .and_then(|task_state| task_state.pr.first())
        })
        .or_else(|| {
            snapshot
                .persistent
                .tasks
                .iter()
                .find(|task| task.id == task_id)
                .and_then(|task| task.backing_pr_url.as_ref())
        })
        .and_then(|pr| pull_request_reference(pr))
        .map(|pr| format!("PR created {pr}"))
}

fn issue_progress_status(
    assigned_repository: &str,
    issue_url: &str,
    agent_state: Option<AutomationAgentState>,
) -> String {
    let issue_ref = issue_reference(issue_url).unwrap_or_else(|| issue_url.to_string());
    let _ = assigned_repository;
    match agent_state.unwrap_or(AutomationAgentState::Idle) {
        AutomationAgentState::Working => format!("Working {issue_ref}"),
        AutomationAgentState::WaitingOnVm => format!("Waiting on VM {issue_ref}"),
        AutomationAgentState::Question => format!("Question {issue_ref}"),
        AutomationAgentState::Review => format!("Review {issue_ref}"),
        AutomationAgentState::Idle => format!("Wait close {issue_ref}"),
        AutomationAgentState::Stale => format!("Stalled {issue_ref}"),
    }
}

fn compact_repository_label(assigned_repository: &str) -> &str {
    assigned_repository
        .rsplit('/')
        .next()
        .unwrap_or(assigned_repository)
}

fn issue_is_closed(issue_status_rx: Option<&watch::Receiver<Option<GithubStatus>>>) -> bool {
    matches!(
        issue_status_rx.and_then(|receiver| *receiver.borrow()),
        Some(GithubStatus::Issue(issue_status))
            if issue_status.state == super::github_status_service::GithubIssueState::Closed
    )
}

fn sync_task_issue_status_receivers(
    service: &CombinedService,
    snapshot: &WorkspaceSnapshot,
    task_issue_status_rxs: &mut HashMap<String, watch::Receiver<Option<GithubStatus>>>,
) {
    let tracked_issue_urls = snapshot
        .persistent
        .tasks
        .iter()
        .map(|task| task.issue_url.as_str())
        .collect::<HashSet<_>>();
    task_issue_status_rxs.retain(|issue_url, _| tracked_issue_urls.contains(issue_url.as_str()));
    for task in &snapshot.persistent.tasks {
        task_issue_status_rxs
            .entry(task.issue_url.clone())
            .or_insert_with(|| {
                service
                    .github_status_service()
                    .watch_status(&task.issue_url)
                    .expect("queued workspace tasks must have valid GitHub issue URLs")
            });
    }
}

fn sync_task_pr_status_receivers(
    service: &CombinedService,
    snapshot: &WorkspaceSnapshot,
    task_pr_status_rxs: &mut HashMap<String, watch::Receiver<Option<GithubStatus>>>,
) {
    let tracked_issue_urls = snapshot
        .persistent
        .tasks
        .iter()
        .filter(|task| task.backing_pr_url.is_some())
        .map(|task| task.issue_url.as_str())
        .collect::<HashSet<_>>();
    task_pr_status_rxs.retain(|issue_url, _| tracked_issue_urls.contains(issue_url.as_str()));
    for task in &snapshot.persistent.tasks {
        let Some(backing_pr_url) = task.backing_pr_url.as_deref() else {
            continue;
        };
        task_pr_status_rxs
            .entry(task.issue_url.clone())
            .or_insert_with(|| {
                service
                    .github_status_service()
                    .watch_status(backing_pr_url)
                    .expect("dependency-upgrade tasks must have valid GitHub PR URLs")
            });
    }
}

fn request_refresh_for_task_issues<'a>(
    service: &CombinedService,
    issue_urls: impl Iterator<Item = &'a str>,
    exclude_issue_url: Option<&str>,
) {
    for issue_url in issue_urls {
        if Some(issue_url) == exclude_issue_url {
            continue;
        }
        let _ = service.github_status_service().request_refresh(issue_url);
    }
}

fn request_refresh_for_task_links<'a>(
    service: &CombinedService,
    urls: impl Iterator<Item = &'a str>,
) {
    for url in urls {
        let _ = service.github_status_service().request_refresh(url);
    }
}

fn closed_background_task_issue_urls(
    snapshot: &WorkspaceSnapshot,
    active_issue_url: Option<&str>,
    task_issue_status_rxs: &HashMap<String, watch::Receiver<Option<GithubStatus>>>,
) -> Vec<String> {
    snapshot
        .persistent
        .tasks
        .iter()
        .filter(|task| Some(task.issue_url.as_str()) != active_issue_url)
        .filter_map(|task| {
            issue_is_closed(task_issue_status_rxs.get(&task.issue_url))
                .then(|| task.issue_url.clone())
        })
        .collect()
}

fn merged_dependency_upgrade_issue_urls(
    snapshot: &WorkspaceSnapshot,
    task_pr_status_rxs: &HashMap<String, watch::Receiver<Option<GithubStatus>>>,
) -> Vec<String> {
    snapshot
        .persistent
        .tasks
        .iter()
        .filter(|task| task.backing_pr_url.is_some() && task.dependency_upgrade_backing_pr)
        .filter_map(|task| {
            matches!(
                task_pr_status_rxs.get(&task.issue_url).and_then(|receiver| *receiver.borrow()),
                Some(GithubStatus::Pr(pr_status))
                    if pr_status.state == super::github_status_service::GithubPrState::Merged
            )
            .then(|| task.issue_url.clone())
        })
        .collect()
}

async fn close_dependency_upgrade_issue(
    assigned_repository: &str,
    issue_url: &str,
    snapshot: &WorkspaceSnapshot,
    token: &str,
) -> Result<(), String> {
    let Some(task) = task_persistent_snapshot_for_issue(snapshot, issue_url) else {
        return Ok(());
    };
    let Some(backing_pr_url) = task
        .dependency_upgrade_backing_pr
        .then_some(task.backing_pr_url.as_deref())
        .flatten()
    else {
        return Ok(());
    };

    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "close",
            issue_url,
            "--repo",
            assigned_repository,
            "--comment",
            &format!(
                "Closed automatically after dependency-upgrade PR {backing_pr_url} was merged."
            ),
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue close for {issue_url}: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh issue close failed for {issue_url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

async fn wait_for_workspace_change_until(
    workspace_rx: &mut watch::Receiver<WorkspaceSnapshot>,
    issue_status_rx: &mut Option<watch::Receiver<Option<GithubStatus>>>,
    deadline: Option<Instant>,
) -> bool {
    match (issue_status_rx.as_mut(), deadline) {
        (Some(issue_status_rx), Some(deadline)) => {
            tokio::select! {
                changed = workspace_rx.changed() => changed.is_ok(),
                changed = issue_status_rx.changed() => changed.is_ok(),
                _ = sleep_until(deadline.into()) => true,
            }
        }
        (Some(issue_status_rx), None) => {
            tokio::select! {
                changed = workspace_rx.changed() => changed.is_ok(),
                changed = issue_status_rx.changed() => changed.is_ok(),
            }
        }
        (None, Some(deadline)) => {
            tokio::select! {
                changed = workspace_rx.changed() => changed.is_ok(),
                _ = sleep_until(deadline.into()) => true,
            }
        }
        (None, None) => workspace_rx.changed().await.is_ok(),
    }
}

async fn ensure_task_session(
    service: &CombinedService,
    workspace: &Workspace,
    snapshot: &WorkspaceSnapshot,
    workspace_key: &str,
    assigned_repository: &str,
    issue: &SelectedIssue,
) -> Result<String, String> {
    let task_cwd = service
        .ensure_workspace_task_checkout(workspace_key, assigned_repository, &issue.url)
        .await
        .map_err(|err| err.summary())?;
    let task_id = task_id_for_issue(snapshot, &issue.url)
        .ok_or_else(|| format!("task missing for issue {}", issue.url))?;
    if let Some(existing) = snapshot
        .task_states
        .get(&task_id)
        .and_then(|state| state.session_id.clone())
    {
        let mut reuse_existing = true;
        if service.agent_provider() == AgentProvider::Codex {
            let Some(uri) = snapshot
                .transient
                .as_ref()
                .map(|transient| transient.uri.clone())
            else {
                return Err("workspace has no active runtime uri".to_string());
            };
            match CodexAppServerClient::new(uri).thread_read(&existing).await {
                Ok(response) => {
                    reuse_existing = response.thread.id.as_deref() == Some(existing.as_str())
                        && response.thread.status.as_ref().is_some_and(|status| {
                            !matches!(
                                status,
                                super::codex_app_server::CodexThreadStatus::NotLoaded
                            ) && !status.requires_replacement()
                        });
                }
                Err(error) => {
                    reuse_existing = !(error.contains("thread not loaded")
                        || error.contains("thread not found"));
                    if !reuse_existing {
                        tracing::info!(
                            workspace_key,
                            issue_url = %issue.url,
                            task_id = %task_id,
                            session_id = %existing,
                            error = %error,
                            "discarding stale autonomous codex task session"
                        );
                    }
                }
            }
        }
        if reuse_existing {
            tracing::info!(
                workspace_key,
                issue_url = %issue.url,
                task_id = %task_id,
                session_id = %existing,
                cwd = %task_cwd.display(),
                "reusing existing autonomous task session"
            );
            return Ok(existing);
        }
    }

    let session_id = match service.agent_provider() {
        AgentProvider::Opencode => {
            let client = snapshot
                .opencode_client
                .as_ref()
                .ok_or_else(|| "workspace has no healthy opencode client".to_string())?;
            let root_session_id = snapshot
                .root_session_id
                .clone()
                .ok_or_else(|| "workspace has no root session id".to_string())?;
            let root_session_id =
                opencode::client::types::SessionForkSessionId::try_from(root_session_id.as_str())
                    .map_err(|err| format!("invalid root session id '{root_session_id}': {err}"))?;
            client
                .client
                .session_fork(
                    &root_session_id,
                    None,
                    None,
                    &opencode::client::types::SessionForkBody::default(),
                )
                .await
                .map_err(|err| format!("failed to fork task session: {err}"))?
                .into_inner()
                .id
                .to_string()
        }
        AgentProvider::Codex => {
            let uri = snapshot
                .transient
                .as_ref()
                .map(|transient| transient.uri.clone())
                .ok_or_else(|| "workspace has no active runtime uri".to_string())?;
            let client = CodexAppServerClient::new(uri);
            let session_id = client
                .thread_start(
                    task_cwd.to_string_lossy().as_ref(),
                    &service.config.agent.codex,
                )
                .await?
                .thread
                .id;
            wait_for_codex_task_thread_ready(&client, &session_id).await?;
            session_id
        }
    };

    tracing::info!(
        workspace_key,
        issue_url = %issue.url,
        task_id = %task_id,
        session_id = %session_id,
        cwd = %task_cwd.display(),
        "created autonomous task session"
    );

    workspace.update(|next| {
        let task_state = next.task_states.entry(task_id.clone()).or_default();
        if task_state.session_id.as_deref() == Some(session_id.as_str()) {
            false
        } else {
            task_state.session_id = Some(session_id.clone());
            true
        }
    });

    Ok(session_id)
}

async fn prompt_task_session(
    service: &CombinedService,
    snapshot: &WorkspaceSnapshot,
    workspace_key: &str,
    assigned_repository: &str,
    task_id: &str,
    issue: &SelectedIssue,
    backing_pr_url: Option<&str>,
    task_session_id: &str,
    cwd: std::path::PathBuf,
) -> Result<(), String> {
    let task_state_path = automation_task_state_file_source(
        service.workspace_directory_path(),
        workspace_key,
        task_id,
    );
    let prompt = resolved_autonomous_task_prompt(
        snapshot,
        task_id,
        assigned_repository,
        issue,
        backing_pr_url,
        task_session_id,
        &cwd,
        &task_state_path,
    );
    match service.agent_provider() {
        AgentProvider::Opencode => {
            let opencode_client = snapshot
                .opencode_client
                .as_ref()
                .ok_or_else(|| "workspace has no healthy opencode client".to_string())?;
            let session_id = task_session_id
                .parse::<opencode::client::types::SessionPromptAsyncSessionId>()
                .map_err(|err| format!("invalid task session id '{task_session_id}': {err}"))?;
            let prompt_body = opencode::client::types::SessionPromptAsyncBody {
                agent: None,
                format: None,
                message_id: None,
                model: None,
                no_reply: None,
                parts: vec![
                    opencode::client::types::TextPartInput {
                        id: None,
                        ignored: None,
                        metadata: Default::default(),
                        synthetic: None,
                        text: prompt,
                        time: None,
                        type_: opencode::client::types::TextPartInputType::Text,
                    }
                    .into(),
                ],
                system: None,
                tools: Default::default(),
                variant: None,
            };
            opencode_client
                .client
                .session_prompt_async(&session_id, None, None, &prompt_body)
                .await
                .map(|_| ())
                .map_err(|err| format!("failed to send prompt: {err}"))
        }
        AgentProvider::Codex => {
            let uri = snapshot
                .transient
                .as_ref()
                .map(|transient| transient.uri.clone())
                .ok_or_else(|| "workspace has no active runtime uri".to_string())?;
            let _ = cwd;
            let client = CodexAppServerClient::new(uri);
            let mut last_error: Option<String> = None;
            for attempt in 0..5 {
                match client
                    .turn_start(task_session_id, &prompt, &service.config.agent.codex)
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(error)
                        if (error.contains("thread not found")
                            || error.contains("thread not loaded"))
                            && attempt < 4 =>
                    {
                        last_error = Some(error);
                        wait_for_codex_task_thread_ready(&client, task_session_id).await?;
                        sleep(Duration::from_millis(200)).await;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last_error.unwrap_or_else(|| {
                format!("failed to start codex turn for task session {task_session_id}")
            }))
        }
    }
}

fn resolved_autonomous_task_prompt(
    snapshot: &WorkspaceSnapshot,
    task_id: &str,
    assigned_repository: &str,
    issue: &SelectedIssue,
    backing_pr_url: Option<&str>,
    task_session_id: &str,
    cwd: &std::path::Path,
    task_state_path: &std::path::Path,
) -> String {
    snapshot
        .task_states
        .get(task_id)
        .and_then(|task_state| task_state.resume_prompt.clone())
        .unwrap_or_else(|| {
            build_issue_prompt(
                assigned_repository,
                issue,
                backing_pr_url,
                task_session_id,
                cwd,
                task_state_path,
            )
        })
}

async fn wait_for_codex_task_thread_ready(
    client: &CodexAppServerClient,
    task_session_id: &str,
) -> Result<(), String> {
    let mut last_error: Option<String> = None;
    for attempt in 0..25 {
        match client.thread_read(task_session_id).await {
            Ok(response) => {
                let status_ready = response.thread.status.as_ref().is_some_and(|status| {
                    !matches!(
                        status,
                        super::codex_app_server::CodexThreadStatus::NotLoaded
                    )
                });
                if status_ready {
                    return Ok(());
                }
                last_error = Some(format!(
                    "thread '{task_session_id}' read succeeded but is not materialized yet"
                ));
            }
            Err(error)
                if error.contains("thread not found") || error.contains("thread not loaded") =>
            {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
        if attempt < 24 {
            sleep(Duration::from_millis(200)).await;
        }
    }

    Err(last_error.unwrap_or_else(|| {
        format!("timed out waiting for codex task thread '{task_session_id}' to materialize")
    }))
}

fn task_cwd_path(
    service: &CombinedService,
    workspace_key: &str,
    assigned_repository: &str,
    issue_url: &str,
) -> std::path::PathBuf {
    service.workspace_task_checkout_path(workspace_key, assigned_repository, issue_url)
}

async fn clear_automation_state_file(service: &CombinedService, workspace_key: &str) {
    let path = automation_state_file_source(service.workspace_directory_path(), workspace_key);
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(workspace_key, error = %err, "failed to clear automation state file");
        }
    }
}

async fn clear_task_automation_state_file(
    service: &CombinedService,
    workspace_key: &str,
    task_id: &str,
) {
    let path = automation_task_state_file_source(
        service.workspace_directory_path(),
        workspace_key,
        task_id,
    );
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(
                workspace_key,
                task_id,
                error = %err,
                "failed to clear task automation state file"
            );
        }
    }
}

fn build_issue_prompt(
    assigned_repository: &str,
    issue: &SelectedIssue,
    backing_pr_url: Option<&str>,
    task_session_id: &str,
    cwd: &std::path::Path,
    task_state_path: &std::path::Path,
) -> String {
    if issue.requires_validation() {
        return build_issue_validation_prompt(
            assigned_repository,
            issue,
            task_session_id,
            cwd,
            task_state_path,
        );
    }

    let dependency_upgrade_pr_urls = issue.dependency_upgrade_pr_urls();
    let publish_instruction = if !dependency_upgrade_pr_urls.is_empty() {
        let renovate_pr_list = dependency_upgrade_pr_urls
            .iter()
            .map(|url| format!("- {url}"))
            .collect::<Vec<_>>()
            .join("\n");
        if dependency_upgrade_pr_urls.len() == 1 {
            let pr_url = dependency_upgrade_pr_urls
                .first()
                .cloned()
                .or_else(|| backing_pr_url.map(ToOwned::to_owned))
                .unwrap_or_else(|| "<missing-pr-url>".to_string());
            format!(
                "7. This task tracks the following Renovate pull request:\n\
{renovate_pr_list}\n\
8. Confirm {pr_url} is still a safe non-major update.\n\
9. If CI is passing, rebase and merge {pr_url} directly without waiting for human review, then close GitHub issue {issue_url}.\n\
10. Do not create a combined dependency-upgrade pull request when this is the only Renovate candidate.\n\
11. Do not leave placeholder comments, placeholder reviews, or dummy approvals on {pr_url}; only comment or review when it is strictly required to complete the merge.",
                issue_url = issue.url
            )
        } else if let Some(backing_pr_url) = backing_pr_url {
            format!(
                "7. This task tracks the following Renovate pull requests:\n\
{renovate_pr_list}\n\
8. Treat {backing_pr_url} as the single combined dependency-upgrade PR for GitHub issue {issue_url}. Do not merge the individual Renovate PRs directly.\n\
9. Confirm the source Renovate PRs are still safe non-major updates and keep the combined PR aligned with that set.\n\
10. Monitor CI for {backing_pr_url} until it completes. If any required check fails, investigate the failure, push follow-up fixes to the combined PR, and keep monitoring until all required checks are green.\n\
11. Merge only the combined PR once its CI is green, then close GitHub issue {issue_url}.\n\
11. Do not leave placeholder comments, placeholder reviews, or dummy approvals on {backing_pr_url}; only comment or review when it is strictly required to complete the merge.",
                issue_url = issue.url
            )
        } else {
            format!(
                "7. This task tracks the following Renovate pull requests:\n\
{renovate_pr_list}\n\
8. Confirm the source Renovate PRs are still safe non-major updates and combine all still-valid candidates into a single dependency-upgrade branch and pull request associated with GitHub issue {issue_url}.\n\
9. Do not merge the individual Renovate PRs directly.\n\
10. Monitor CI for the combined PR until it completes. If any required check fails, investigate the failure, push follow-up fixes to the combined PR, and keep monitoring until all required checks are green.\n\
11. Merge only the combined PR once it is green, and then close GitHub issue {issue_url}.\n\
12. Do not leave placeholder comments, placeholder reviews, or dummy approvals on the combined PR; only comment or review when it is strictly required to complete the merge.",
                issue_url = issue.url
            )
        }
    } else {
        "7. Do not commit, push, comment, or open/update a pull request until the user explicitly approves publishing.\n\
8. When the change is ready for review, stage the full task checkout with `git add -A` so new files are included, then stop and ask for permission.\n\
9. When you do create or update the pull request after approval, include an appropriate type label such as `type: docs` for documentation-only changes, `type: bug` for bug fixes, `type: improvement` for minor improvements, or `type: enhancement` for broader enhancements."
            .to_string()
    };
    format!(
        "You are operating in an autonomous multicode workspace for repository {assigned_repository}.\n\
Start work on GitHub issue {issue_url}.\n\
Issue title: {issue_title}\n\
Primary checkout for this task: {cwd}\n\
Before you proceed, load and follow these workspace skills as appropriate: `independent-fix`, `machine-readable-clone`, `machine-readable-issue`, `machine-readable-pr`, `git-commit-coauthorship`, `micronaut-projects-guide`, and `autonomous-state`.\n\
For this task, write autonomous state updates to `{task_state_path}`. Do not write task state to any shared workspace file.\n\
For this task session/thread, write autonomous state updates in the format `<state>:{task_session_id}` so multicode can attribute the state to this specific session.\n\
Your job is to:\n\
1. Use the existing checkout at `{cwd}` for this issue. Keep this task isolated to that checkout instead of sharing another task's repository state.\n\
2. Understand and reproduce the issue, creating a minimal reproducer or failing test when possible.\n\
3. Implement the fix.\n\
4. Run focused verification and summarize the evidence.\n\
5. Emit the machine-readable repository / issue / PR tags while you work.\n\
6. Run repository commands, builds, Gradle tasks, and focused tests as needed without asking for permission.\n\
{publish_instruction}\n\
\n\
Prefer an upstream pull request if you have write access. Keep going until the workspace is ready for review or you need human feedback.",
        issue_url = issue.url,
        issue_title = issue.title,
        task_session_id = task_session_id,
        cwd = cwd.display(),
        task_state_path = task_state_path.display(),
        publish_instruction = publish_instruction
    )
}

fn build_issue_validation_prompt(
    assigned_repository: &str,
    issue: &SelectedIssue,
    task_session_id: &str,
    cwd: &std::path::Path,
    task_state_path: &std::path::Path,
) -> String {
    format!(
        "You are operating in an autonomous multicode workspace for repository {assigned_repository}.\n\
Start validation work on GitHub issue {issue_url}.\n\
Issue title: {issue_title}\n\
Primary checkout for this task: {cwd}\n\
Before you proceed, load and follow these workspace skills as appropriate: `independent-fix`, `machine-readable-clone`, `machine-readable-issue`, `git-commit-coauthorship`, `micronaut-projects-guide`, and `autonomous-state`.\n\
For this task, write autonomous state updates to `{task_state_path}`. Do not write task state to any shared workspace file.\n\
For this task session/thread, write autonomous state updates in the format `<state>:{task_session_id}` so multicode can attribute the state to this specific session.\n\
You are acting as a QA engineer validating whether this issue is actionable.\n\
Your job is to:\n\
1. Use the existing checkout at `{cwd}` for this issue. Keep this task isolated to that checkout instead of sharing another task's repository state.\n\
2. Understand the report and attempt to reproduce it using focused commands, builds, Gradle tasks, tests, or a minimal local reproducer as needed.\n\
3. Do not implement a fix, do not commit, do not push, and do not create or update any pull request during this validation phase.\n\
4. Do not assign the issue to anyone. This phase is only for verification and triage.\n\
5. If the issue is reproducible, add a detailed GitHub comment with the reproduction steps, environment, and evidence, and attach or paste the reproducer when possible. Apply the most appropriate result label such as `status: bug`, `status: regression`, `status: docs`, or another repository-appropriate validation label.\n\
6. If the issue is not reproducible or not actionable, add a GitHub comment summarizing the evidence and apply the most appropriate closing label such as `closed: question`, `closed: notabug`, `closed: duplicate`, or `closed: cannot reproduce`.\n\
7. Replace `status: awaiting validation` and any temporary in-progress claim label with the final validation outcome labels before you finish.\n\
8. Emit the machine-readable repository / issue tags while you work.\n\
9. When validation is complete, leave the issue in review state for a human to make the final close / follow-up decision.\n\
\n\
Run repository commands, builds, and focused verification without asking for permission. Keep going until validation evidence is complete and the issue is ready for human review.",
        issue_url = issue.url,
        issue_title = issue.title,
        task_session_id = task_session_id,
        cwd = cwd.display(),
        task_state_path = task_state_path.display(),
    )
}

async fn find_next_issue(
    assigned_repository: &str,
    excluded_issue_urls: &HashSet<String>,
    token: &str,
) -> Result<Option<QueuedIssueCandidate>, String> {
    if let Some(candidate) =
        find_next_dependency_upgrade_issue(assigned_repository, excluded_issue_urls, token).await?
    {
        return Ok(Some(candidate));
    }

    if let Some(candidate) =
        find_next_validation_issue(assigned_repository, excluded_issue_urls, token).await?
    {
        return Ok(Some(candidate));
    }

    let open_pull_requests = list_open_pull_requests(assigned_repository, token).await?;
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    for label in ISSUE_PRIORITY_LABELS {
        let issues = list_issues_for_label(assigned_repository, label, token).await?;
        for issue in issues {
            if issue.is_in_progress()
                || excluded_issue_urls.contains(&issue.url)
                || !seen.insert(issue.url.clone())
            {
                continue;
            }
            candidates.push(QueuedIssueCandidate {
                backing_pr_url: discover_issue_backing_pr_url(
                    assigned_repository,
                    &issue,
                    &open_pull_requests,
                ),
                dependency_upgrade_backing_pr: false,
                issue,
            });
        }
    }

    if let Some(candidate) = candidates
        .into_iter()
        .min_by(|left, right| issue_priority_cmp(&left.issue, &right.issue))
    {
        return Ok(Some(candidate));
    }

    Ok(None)
}

async fn find_next_validation_issue(
    assigned_repository: &str,
    excluded_issue_urls: &HashSet<String>,
    token: &str,
) -> Result<Option<QueuedIssueCandidate>, String> {
    let issues =
        list_issues_for_label(assigned_repository, AWAITING_VALIDATION_LABEL, token).await?;
    let Some(issue) = select_validation_issue_candidate(issues, excluded_issue_urls) else {
        return Ok(None);
    };

    Ok(Some(QueuedIssueCandidate {
        issue,
        backing_pr_url: None,
        dependency_upgrade_backing_pr: false,
    }))
}

fn select_validation_issue_candidate(
    issues: Vec<SelectedIssue>,
    excluded_issue_urls: &HashSet<String>,
) -> Option<SelectedIssue> {
    issues
        .into_iter()
        .filter(SelectedIssue::requires_validation)
        .filter(|issue| !issue.is_in_progress())
        .filter(|issue| !excluded_issue_urls.contains(&issue.url))
        .min_by(issue_priority_cmp)
}

async fn list_issues_for_label(
    assigned_repository: &str,
    label: &str,
    token: &str,
) -> Result<Vec<SelectedIssue>, String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args(issue_search_args(assigned_repository, label))
        .output()
        .await
        .map_err(|err| {
            format!("failed to run gh search issues for {assigned_repository}: {err}")
        })?;

    if !output.status.success() {
        return Err(format!(
            "gh search issues failed for {assigned_repository}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    serde_json::from_slice::<Vec<SelectedIssue>>(&output.stdout)
        .map(|issues| {
            issues
                .into_iter()
                .filter(SelectedIssue::is_open_issue_candidate)
                .collect()
        })
        .map_err(|err| format!("failed to parse gh search issues output: {err}"))
}

async fn fetch_issue(
    assigned_repository: &str,
    issue_url: &str,
    token: &str,
) -> Result<Option<SelectedIssue>, String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "view",
            issue_url,
            "--repo",
            assigned_repository,
            "--json",
            "number,title,createdAt,labels,url,state,body",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue view for {issue_url}: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "gh issue view failed for {issue_url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let mut issue = serde_json::from_slice::<SelectedIssue>(&output.stdout)
        .map_err(|err| format!("failed to parse gh issue view output: {err}"))?;
    issue.dependency_upgrade_pr_urls = issue.dependency_upgrade_pr_urls();
    Ok(issue.is_open_issue_candidate().then_some(issue))
}

fn issue_number_from_url(issue_url: &str) -> Option<u64> {
    let parsed = Url::parse(issue_url).ok()?;
    let segments = parsed.path_segments()?.collect::<Vec<_>>();
    if segments.len() < 4 || segments[2] != "issues" {
        return None;
    }
    segments[3].parse::<u64>().ok()
}

fn github_repository_from_issue_or_pr_url(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    if parsed.host_str()? != "github.com" {
        return None;
    }
    let segments = parsed.path_segments()?.collect::<Vec<_>>();
    let [owner, repo, kind, _number, ..] = segments.as_slice() else {
        return None;
    };
    if !matches!(*kind, "issues" | "issue" | "pull") {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn github_issue_or_pr_url_matches_repository(url: &str, assigned_repository: &str) -> bool {
    github_repository_from_issue_or_pr_url(url)
        .as_deref()
        .is_some_and(|repository| repository == assigned_repository)
}

fn available_issue_scan_slots(max_parallel_issues: usize, task_count: usize) -> usize {
    max_parallel_issues.saturating_sub(task_count)
}

fn should_defer_startup_issue_scan(
    startup_scan_pending: bool,
    snapshot: &WorkspaceSnapshot,
    scan_requested: bool,
    queue_next_requested: bool,
) -> bool {
    startup_scan_pending
        && snapshot.persistent.tasks.is_empty()
        && snapshot.active_task_id.is_none()
        && snapshot.persistent.assigned_repository.is_some()
        && !scan_requested
        && !queue_next_requested
}

fn issue_search_args(assigned_repository: &str, label: &str) -> Vec<String> {
    vec![
        "search".to_string(),
        "issues".to_string(),
        "--repo".to_string(),
        assigned_repository.to_string(),
        "--state".to_string(),
        "open".to_string(),
        "--label".to_string(),
        label.to_string(),
        "--sort".to_string(),
        "created".to_string(),
        "--order".to_string(),
        "desc".to_string(),
        "--limit".to_string(),
        "100".to_string(),
        "--json".to_string(),
        "number,title,createdAt,labels,url,state,isPullRequest".to_string(),
        "--".to_string(),
        "-linked:pr".to_string(),
    ]
}

async fn find_next_dependency_upgrade_issue(
    assigned_repository: &str,
    excluded_issue_urls: &HashSet<String>,
    token: &str,
) -> Result<Option<QueuedIssueCandidate>, String> {
    let pull_requests = list_dependency_upgrade_prs(assigned_repository, token).await?;
    let batch_candidates = pull_requests
        .into_iter()
        .filter(|pr| {
            pr.is_open_dependency_upgrade_candidate() && pr.is_non_major_dependency_upgrade()
        })
        .collect::<Vec<_>>();
    if batch_candidates.is_empty() {
        return Ok(None);
    }

    let issue = if batch_candidates.len() == 1 {
        find_or_create_single_dependency_upgrade_issue(
            assigned_repository,
            batch_candidates
                .first()
                .expect("single dependency-upgrade candidate should exist"),
            token,
        )
        .await?
    } else {
        find_or_create_dependency_upgrade_issue(assigned_repository, &batch_candidates, token)
            .await?
    };
    let Some(issue) = issue else {
        return Ok(None);
    };
    if issue.is_in_progress() || excluded_issue_urls.contains(&issue.url) {
        return Ok(None);
    }

    let open_pull_requests = list_open_pull_requests(assigned_repository, token).await?;
    Ok(Some(QueuedIssueCandidate {
        backing_pr_url: discover_issue_backing_pr_url(
            assigned_repository,
            &issue,
            &open_pull_requests,
        ),
        issue,
        dependency_upgrade_backing_pr: true,
    }))
}

async fn list_dependency_upgrade_prs(
    assigned_repository: &str,
    token: &str,
) -> Result<Vec<SelectedPullRequest>, String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "search",
            "prs",
            "--repo",
            assigned_repository,
            "--state",
            "open",
            "--label",
            DEPENDENCY_UPGRADE_LABEL,
            "--sort",
            "created",
            "--order",
            "desc",
            "--limit",
            "100",
            "--json",
            "number,title,url,labels,state,isDraft,body,author",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh search prs for {assigned_repository}: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "gh search prs failed for {assigned_repository}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    serde_json::from_slice::<Vec<SelectedPullRequest>>(&output.stdout)
        .map_err(|err| format!("failed to parse gh search prs output: {err}"))
}

async fn list_open_pull_requests(
    assigned_repository: &str,
    token: &str,
) -> Result<Vec<SelectedPullRequest>, String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "pr",
            "list",
            "--repo",
            assigned_repository,
            "--state",
            "open",
            "--limit",
            "100",
            "--json",
            "number,title,url,body,headRefName,createdAt,isDraft,state,labels,author",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh pr list for {assigned_repository}: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "gh pr list failed for {assigned_repository}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    serde_json::from_slice::<Vec<SelectedPullRequest>>(&output.stdout)
        .map_err(|err| format!("failed to parse gh pr list output: {err}"))
}

fn discover_issue_backing_pr_url(
    assigned_repository: &str,
    issue: &SelectedIssue,
    pull_requests: &[SelectedPullRequest],
) -> Option<String> {
    let mut matches = pull_requests
        .iter()
        .filter_map(|pr| {
            score_pull_request_issue_match(assigned_repository, issue, pr)
                .map(|score| (score, pr.created_at.as_deref(), pr.number, pr.url.as_str()))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| right.2.cmp(&left.2))
    });
    matches.first().map(|(_, _, _, url)| (*url).to_string())
}

fn score_pull_request_issue_match(
    assigned_repository: &str,
    issue: &SelectedIssue,
    pr: &SelectedPullRequest,
) -> Option<u8> {
    if !matches!(pr.state.as_deref(), Some("OPEN") | Some("open")) || pr.is_draft == Some(true) {
        return None;
    }

    let mut score = 0u8;
    if pull_request_has_closing_issue_reference(assigned_repository, issue, pr) {
        score = score.max(3);
    }
    if pull_request_mentions_issue_reference(assigned_repository, issue, pr) {
        score = score.max(2);
    }
    if pull_request_head_matches_issue_number(issue, pr) {
        score = score.max(1);
    }

    (score > 0).then_some(score)
}

fn pull_request_has_closing_issue_reference(
    assigned_repository: &str,
    issue: &SelectedIssue,
    pr: &SelectedPullRequest,
) -> bool {
    let body = pr.body.as_deref().unwrap_or_default();
    let title = pr.title.as_str();
    let issue_number_ref = format!("#{}", issue.number);
    let repo_issue_ref = format!("{assigned_repository}#{}", issue.number);
    let issue_url = issue.url.as_str();

    [
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ]
    .iter()
    .any(|keyword| {
        let body = body.to_ascii_lowercase();
        let title = title.to_ascii_lowercase();
        let keyword = keyword.to_ascii_lowercase();
        let refs = [
            format!("{keyword} {issue_number_ref}"),
            format!("{keyword} {repo_issue_ref}"),
            format!("{keyword} {issue_url}"),
        ];
        refs.iter().any(|candidate| {
            contains_standalone_reference(&body, candidate)
                || contains_standalone_reference(&title, candidate)
        })
    })
}

fn pull_request_mentions_issue_reference(
    assigned_repository: &str,
    issue: &SelectedIssue,
    pr: &SelectedPullRequest,
) -> bool {
    let issue_number_ref = format!("#{}", issue.number);
    let repo_issue_ref = format!("{assigned_repository}#{}", issue.number);
    let issue_text = format!("issue #{}", issue.number);
    let issue_url = issue.url.as_str();

    [pr.title.as_str(), pr.body.as_deref().unwrap_or_default()]
        .iter()
        .any(|text| {
            contains_standalone_reference(text, &issue_number_ref)
                || contains_standalone_reference(text, &repo_issue_ref)
                || contains_standalone_reference(&text.to_ascii_lowercase(), &issue_text)
                || contains_standalone_reference(text, issue_url)
        })
}

fn pull_request_head_matches_issue_number(issue: &SelectedIssue, pr: &SelectedPullRequest) -> bool {
    let issue_number = issue.number.to_string();
    pr.head_ref_name.as_deref().is_some_and(|head_ref_name| {
        let segments = head_ref_name
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();

        segments.iter().enumerate().any(|(index, segment)| {
            if *segment != issue_number {
                return false;
            }
            if issue.number >= 10 {
                return true;
            }

            index
                .checked_sub(1)
                .and_then(|previous| segments.get(previous))
                .copied()
                .is_some_and(is_issue_branch_keyword)
                || segments
                    .get(index + 1)
                    .copied()
                    .is_some_and(is_issue_branch_keyword)
        })
    })
}

fn contains_standalone_reference(text: &str, reference: &str) -> bool {
    if reference.is_empty() {
        return false;
    }

    let mut start_index = 0usize;
    while let Some(offset) = text[start_index..].find(reference) {
        let match_start = start_index + offset;
        let match_end = match_start + reference.len();
        let previous = text[..match_start].chars().next_back();
        let next = text[match_end..].chars().next();
        if is_reference_boundary(previous) && is_reference_boundary(next) {
            return true;
        }
        start_index = match_end;
    }

    false
}

fn is_reference_boundary(character: Option<char>) -> bool {
    character.is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
}

fn is_issue_branch_keyword(segment: &str) -> bool {
    matches!(
        segment,
        "issue" | "issues" | "fix" | "fixes" | "fixed" | "bug" | "bugs" | "feature"
    )
}

async fn find_or_create_dependency_upgrade_issue(
    assigned_repository: &str,
    prs: &[SelectedPullRequest],
    token: &str,
) -> Result<Option<SelectedIssue>, String> {
    if let Some(issue) = find_existing_dependency_upgrade_issue(assigned_repository, token).await? {
        return Ok(Some(
            sync_dependency_upgrade_issue(assigned_repository, issue, prs, token).await?,
        ));
    }

    create_dependency_upgrade_issue(assigned_repository, prs, token)
        .await
        .map(Some)
}

async fn find_or_create_single_dependency_upgrade_issue(
    assigned_repository: &str,
    pr: &SelectedPullRequest,
    token: &str,
) -> Result<Option<SelectedIssue>, String> {
    if let Some(issue) =
        find_existing_single_dependency_upgrade_issue(assigned_repository, &pr.url, token).await?
    {
        return Ok(Some(
            sync_single_dependency_upgrade_issue(assigned_repository, issue, pr, token).await?,
        ));
    }

    create_single_dependency_upgrade_issue(assigned_repository, pr, token)
        .await
        .map(Some)
}

async fn find_existing_dependency_upgrade_issue(
    assigned_repository: &str,
    token: &str,
) -> Result<Option<SelectedIssue>, String> {
    for mut issue in list_dependency_upgrade_issues(assigned_repository, token).await? {
        if !issue.is_open_issue_candidate() {
            continue;
        }
        issue.dependency_upgrade_pr_urls = issue.dependency_upgrade_pr_urls();
        if issue
            .title
            .starts_with(DEPENDENCY_UPGRADE_ISSUE_TITLE_PREFIX)
            || !issue.dependency_upgrade_pr_urls.is_empty()
        {
            return Ok(Some(issue));
        }
    }

    Ok(None)
}

async fn find_existing_single_dependency_upgrade_issue(
    assigned_repository: &str,
    pr_url: &str,
    token: &str,
) -> Result<Option<SelectedIssue>, String> {
    for mut issue in list_dependency_upgrade_issues(assigned_repository, token).await? {
        if !issue.is_open_issue_candidate() {
            continue;
        }
        issue.dependency_upgrade_pr_urls = issue.dependency_upgrade_pr_urls();
        if issue.dependency_upgrade_pr_urls() == vec![pr_url.to_string()] {
            return Ok(Some(issue));
        }
    }

    Ok(None)
}

async fn create_dependency_upgrade_issue(
    assigned_repository: &str,
    prs: &[SelectedPullRequest],
    token: &str,
) -> Result<SelectedIssue, String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "create",
            "--repo",
            assigned_repository,
            "--title",
            &dependency_upgrade_issue_title(prs),
            "--body",
            &dependency_upgrade_issue_body(prs),
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue create for dependency upgrade batch in {assigned_repository}: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "gh issue create failed for dependency upgrade batch in {assigned_repository}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let created_issue_url = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("https://github.com/"))
        .map(str::trim)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            format!(
                "gh issue create for dependency upgrade batch in {assigned_repository} did not return an issue URL"
            )
        })?;

    let mut issue = fetch_issue(assigned_repository, &created_issue_url, token)
        .await?
        .ok_or_else(|| {
            format!("created dependency-upgrade issue {created_issue_url} could not be fetched")
        })?;
    issue.dependency_upgrade_pr_urls = dependency_upgrade_pr_urls_from_pull_requests(prs);

    if let Err(err) =
        add_issue_label(assigned_repository, &issue, DEPENDENCY_UPGRADE_LABEL, token).await
    {
        tracing::info!(
            issue_url = %issue.url,
            pr_count = prs.len(),
            error = %err,
            "failed to add dependency-upgrade label to synthetic issue"
        );
    }

    Ok(issue)
}

async fn create_single_dependency_upgrade_issue(
    assigned_repository: &str,
    pr: &SelectedPullRequest,
    token: &str,
) -> Result<SelectedIssue, String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "create",
            "--repo",
            assigned_repository,
            "--title",
            &single_dependency_upgrade_issue_title(pr),
            "--body",
            &single_dependency_upgrade_issue_body(pr),
        ])
        .output()
        .await
        .map_err(|err| {
            format!(
                "failed to run gh issue create for single dependency upgrade in {assigned_repository}: {err}"
            )
        })?;

    if !output.status.success() {
        return Err(format!(
            "gh issue create failed for single dependency upgrade in {assigned_repository}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let created_issue_url = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("https://github.com/"))
        .map(str::trim)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            format!(
                "gh issue create for single dependency upgrade in {assigned_repository} did not return an issue URL"
            )
        })?;

    let mut issue = fetch_issue(assigned_repository, &created_issue_url, token)
        .await?
        .ok_or_else(|| {
            format!(
                "created single dependency-upgrade issue {created_issue_url} could not be fetched"
            )
        })?;
    issue.dependency_upgrade_pr_urls = vec![pr.url.clone()];

    if let Err(err) =
        add_issue_label(assigned_repository, &issue, DEPENDENCY_UPGRADE_LABEL, token).await
    {
        tracing::info!(
            issue_url = %issue.url,
            pr_url = %pr.url,
            error = %err,
            "failed to add dependency-upgrade label to single synthetic issue"
        );
    }

    Ok(issue)
}

async fn sync_dependency_upgrade_issue(
    assigned_repository: &str,
    mut issue: SelectedIssue,
    prs: &[SelectedPullRequest],
    token: &str,
) -> Result<SelectedIssue, String> {
    let expected_title = dependency_upgrade_issue_title(prs);
    let expected_body = dependency_upgrade_issue_body(prs);
    if issue.title != expected_title || issue.body.as_deref() != Some(expected_body.as_str()) {
        edit_issue_title_and_body(
            assigned_repository,
            &issue.url,
            &expected_title,
            &expected_body,
            token,
        )
        .await?;
        issue.title = expected_title;
        issue.body = Some(expected_body);
    }
    issue.dependency_upgrade_pr_urls = dependency_upgrade_pr_urls_from_pull_requests(prs);
    Ok(issue)
}

async fn sync_single_dependency_upgrade_issue(
    assigned_repository: &str,
    mut issue: SelectedIssue,
    pr: &SelectedPullRequest,
    token: &str,
) -> Result<SelectedIssue, String> {
    let expected_title = single_dependency_upgrade_issue_title(pr);
    let expected_body = single_dependency_upgrade_issue_body(pr);
    if issue.title != expected_title || issue.body.as_deref() != Some(expected_body.as_str()) {
        edit_issue_title_and_body(
            assigned_repository,
            &issue.url,
            &expected_title,
            &expected_body,
            token,
        )
        .await?;
        issue.title = expected_title;
        issue.body = Some(expected_body);
    }
    issue.dependency_upgrade_pr_urls = vec![pr.url.clone()];
    Ok(issue)
}

async fn list_dependency_upgrade_issues(
    assigned_repository: &str,
    token: &str,
) -> Result<Vec<SelectedIssue>, String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "search",
            "issues",
            "--repo",
            assigned_repository,
            "--state",
            "open",
            "--label",
            DEPENDENCY_UPGRADE_LABEL,
            "--sort",
            "created",
            "--order",
            "desc",
            "--limit",
            "20",
            "--json",
            "number,title,createdAt,labels,url,state,isPullRequest,body",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh search issues for dependency upgrade batches in {assigned_repository}: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "gh search issues failed while locating dependency-upgrade batches in {assigned_repository}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    serde_json::from_slice::<Vec<SelectedIssue>>(&output.stdout)
        .map_err(|err| format!("failed to parse gh dependency-upgrade issue search output: {err}"))
}

fn dependency_upgrade_issue_title(prs: &[SelectedPullRequest]) -> String {
    format!(
        "{DEPENDENCY_UPGRADE_ISSUE_TITLE_PREFIX} ({} PRs)",
        prs.len()
    )
}

fn single_dependency_upgrade_issue_title(pr: &SelectedPullRequest) -> String {
    format!(
        "{SINGLE_DEPENDENCY_UPGRADE_ISSUE_TITLE_PREFIX} #{}",
        pr.number
    )
}

fn dependency_upgrade_issue_body(prs: &[SelectedPullRequest]) -> String {
    let candidate_list = prs
        .iter()
        .map(|pr| format!("- {} ({})", pr.url, pr.title))
        .collect::<Vec<_>>()
        .join("\n");
    let marker = serde_json::to_string(&dependency_upgrade_pr_urls_from_pull_requests(prs))
        .expect("dependency upgrade batch marker should serialize");
    format!(
        "Track dependency-upgrade automation for the current batch of Renovate pull requests.\n\n\
Current Renovate candidates:\n\
{candidate_list}\n\n\
This issue was created automatically by multicode to process the Renovate batch.\n\
Create or update a single combined dependency-upgrade pull request associated with this issue. Monitor CI for that combined PR until it completes; if any required check fails, investigate it, push follow-up fixes, and keep monitoring until all required checks are green. Merge only the combined PR once it is green. Do not merge the individual Renovate PRs directly.\n\n\
{marker_prefix}{marker} -->",
        marker_prefix = DEPENDENCY_UPGRADE_PR_BATCH_MARKER_PREFIX
    )
}

fn single_dependency_upgrade_issue_body(pr: &SelectedPullRequest) -> String {
    format!(
        "Track dependency-upgrade automation for Renovate pull request {pr_url}.\n\n\
Current Renovate candidate:\n\
- {pr_url} ({pr_title})\n\n\
This issue was created automatically by multicode to prrocess the PR.\n\
If the update is still a non-major version bump and CI is passing, rebase and merge the PR without waiting for human review, then close this issue.\n\n\
{marker_prefix}{pr_url} -->",
        pr_url = pr.url,
        pr_title = pr.title,
        marker_prefix = DEPENDENCY_UPGRADE_PR_MARKER_PREFIX
    )
}

fn dependency_upgrade_pr_urls_from_pull_requests(prs: &[SelectedPullRequest]) -> Vec<String> {
    prs.iter().map(|pr| pr.url.clone()).collect()
}

async fn assign_issue_to_me(
    assigned_repository: &str,
    issue: &SelectedIssue,
    token: &str,
) -> Result<(), String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "edit",
            &issue.url,
            "--repo",
            assigned_repository,
            "--add-assignee",
            "@me",
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue edit for {}: {err}", issue.url))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh issue edit failed for {}: {}",
            issue.url,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub(crate) async fn clear_issue_claim_for_ignore(
    service: &CombinedService,
    assigned_repository: &str,
    issue_url: &str,
) -> Result<(), String> {
    let token = resolved_gh_token(service).await?;
    let Some(issue) = fetch_issue(assigned_repository, issue_url, &token).await? else {
        return Ok(());
    };

    for label in IN_PROGRESS_LABEL_ALIASES {
        if issue.has_label(label) {
            remove_issue_label(assigned_repository, &issue.url, label, &token).await?;
        }
    }

    remove_issue_assignee(assigned_repository, &issue.url, "@me", &token).await
}

async fn add_issue_label(
    assigned_repository: &str,
    issue: &SelectedIssue,
    label: &str,
    token: &str,
) -> Result<(), String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "edit",
            &issue.url,
            "--repo",
            assigned_repository,
            "--add-label",
            label,
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue edit for {}: {err}", issue.url))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh issue edit failed for {}: {}",
            issue.url,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

async fn edit_issue_title_and_body(
    assigned_repository: &str,
    issue_url: &str,
    title: &str,
    body: &str,
    token: &str,
) -> Result<(), String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "edit",
            issue_url,
            "--repo",
            assigned_repository,
            "--title",
            title,
            "--body",
            body,
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue edit for {issue_url}: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh issue edit failed for {issue_url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

async fn remove_issue_label(
    assigned_repository: &str,
    issue_url: &str,
    label: &str,
    token: &str,
) -> Result<(), String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "edit",
            issue_url,
            "--repo",
            assigned_repository,
            "--remove-label",
            label,
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue edit for {issue_url}: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh issue edit failed for {issue_url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

async fn remove_issue_assignee(
    assigned_repository: &str,
    issue_url: &str,
    assignee: &str,
    token: &str,
) -> Result<(), String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "edit",
            issue_url,
            "--repo",
            assigned_repository,
            "--remove-assignee",
            assignee,
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue edit for {issue_url}: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh issue edit failed for {issue_url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn apply_gh_env(command: &mut Command, token: &str) {
    command.env("GH_TOKEN", token);
    command.env("GITHUB_TOKEN", token);
}

async fn resolved_gh_token(service: &CombinedService) -> Result<String, String> {
    service
        .github_status_service()
        .resolved_github_token()
        .await
        .map_err(|err| format!("failed to resolve GitHub token: {err}"))
}

fn gh_program() -> String {
    std::env::var("MULTICODE_GH_COMMAND").unwrap_or_else(|_| "gh".to_string())
}

pub(crate) fn normalize_github_repository_spec(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        return normalize_github_repository_path(rest);
    }
    if let Some(rest) = trimmed.strip_prefix("http://github.com/") {
        return normalize_github_repository_path(rest);
    }
    normalize_github_repository_path(trimmed)
}

fn normalize_github_repository_path(path: &str) -> Option<String> {
    let mut segments = path
        .split('/')
        .filter(|segment| !segment.trim().is_empty())
        .map(|segment| segment.trim())
        .collect::<Vec<_>>();
    if segments.len() < 2 {
        return None;
    }
    let owner = segments.remove(0);
    let mut repo = segments.remove(0).to_string();
    if let Some(stripped) = repo.strip_suffix(".git") {
        repo = stripped.to_string();
    }
    (!owner.is_empty() && !repo.is_empty()).then(|| format!("{owner}/{repo}"))
}

pub(crate) fn issue_reference(url: &str) -> Option<String> {
    let stripped = url.strip_prefix("https://github.com/")?;
    let segments = stripped.split('/').collect::<Vec<_>>();
    if segments.len() < 4 {
        return None;
    }
    let owner = segments[0];
    let repo = segments[1];
    let number = segments[3];
    Some(format!("{owner}/{repo}#{number}"))
}

fn pull_request_reference(url: &str) -> Option<String> {
    let stripped = url.strip_prefix("https://github.com/")?;
    let segments = stripped.split('/').collect::<Vec<_>>();
    if segments.len() < 4 || segments[2] != "pull" {
        return None;
    }
    Some(format!("#{}", segments[3]))
}

pub(crate) fn normalize_github_issue_spec(
    assigned_repository: &str,
    input: &str,
) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix('#')
        && let Ok(number) = rest.parse::<u64>()
    {
        return Some(format!(
            "https://github.com/{assigned_repository}/issues/{number}"
        ));
    }

    if let Ok(number) = trimmed.parse::<u64>() {
        return Some(format!(
            "https://github.com/{assigned_repository}/issues/{number}"
        ));
    }

    if let Some((repository, issue_number)) = trimmed.split_once('#')
        && normalize_github_repository_spec(repository)? == assigned_repository
    {
        let number = issue_number.trim().parse::<u64>().ok()?;
        return Some(format!(
            "https://github.com/{assigned_repository}/issues/{number}"
        ));
    }

    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        return normalize_github_issue_path(assigned_repository, rest);
    }
    if let Some(rest) = trimmed.strip_prefix("http://github.com/") {
        return normalize_github_issue_path(assigned_repository, rest);
    }

    normalize_github_issue_path(assigned_repository, trimmed)
}

fn normalize_github_issue_path(assigned_repository: &str, path: &str) -> Option<String> {
    let segments = path
        .split('/')
        .filter(|segment| !segment.trim().is_empty())
        .map(|segment| segment.trim())
        .collect::<Vec<_>>();
    let [owner, repo, kind, number, ..] = segments.as_slice() else {
        return None;
    };
    if *kind != "issues" {
        return None;
    }
    let repository = normalize_github_repository_spec(&format!("{owner}/{repo}"))?;
    if repository != assigned_repository {
        return None;
    }
    let number = number.parse::<u64>().ok()?;
    Some(format!(
        "https://github.com/{assigned_repository}/issues/{number}"
    ))
}

#[derive(Debug, Clone, Deserialize)]
struct SelectedIssue {
    number: u64,
    title: String,
    url: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    state: Option<String>,
    #[serde(rename = "isPullRequest")]
    is_pull_request: Option<bool>,
    #[serde(default)]
    body: Option<String>,
    labels: Vec<SelectedIssueLabel>,
    #[serde(skip)]
    dependency_upgrade_pr_urls: Vec<String>,
}

impl SelectedIssue {
    fn has_label(&self, label: &str) -> bool {
        self.labels
            .iter()
            .any(|candidate| candidate.name.eq_ignore_ascii_case(label))
    }

    fn is_in_progress(&self) -> bool {
        IN_PROGRESS_LABEL_ALIASES
            .iter()
            .any(|label| self.has_label(label))
    }

    fn requires_validation(&self) -> bool {
        self.has_label(AWAITING_VALIDATION_LABEL)
    }

    fn has_priority_boost_label(&self) -> bool {
        ISSUE_PRIORITY_BOOST_LABELS
            .iter()
            .any(|label| self.has_label(label))
    }

    fn primary_priority_rank(&self) -> usize {
        ISSUE_PRIORITY_LABELS
            .iter()
            .position(|label| self.has_label(label))
            .unwrap_or(ISSUE_PRIORITY_LABELS.len())
    }

    fn display_reference(&self) -> String {
        issue_reference(&self.url).unwrap_or_else(|| format!("#{}", self.number))
    }

    fn is_open_issue_candidate(&self) -> bool {
        matches!(self.state.as_deref(), Some("OPEN") | Some("open"))
            && self.is_pull_request != Some(true)
    }

    fn dependency_upgrade_pr_urls(&self) -> Vec<String> {
        if !self.dependency_upgrade_pr_urls.is_empty() {
            return self.dependency_upgrade_pr_urls.clone();
        }
        self.body
            .as_deref()
            .map(extract_dependency_upgrade_pr_urls)
            .unwrap_or_default()
    }

    fn legacy_dependency_upgrade_pr_url(&self) -> Option<&str> {
        self.body
            .as_deref()
            .and_then(extract_dependency_upgrade_pr_marker)
    }

    fn issue_type(&self) -> Option<WorkspaceIssueType> {
        if self.has_label(DEPENDENCY_UPGRADE_LABEL) {
            return Some(WorkspaceIssueType::DependencyUpgrade);
        }
        if REGRESSION_ISSUE_LABELS
            .iter()
            .any(|label| self.has_label(label))
        {
            return Some(WorkspaceIssueType::Regression);
        }
        if BUG_ISSUE_LABELS.iter().any(|label| self.has_label(label)) {
            return Some(WorkspaceIssueType::Bug);
        }
        if DOC_ISSUE_LABELS.iter().any(|label| self.has_label(label)) {
            return Some(WorkspaceIssueType::Docs);
        }
        if IMPROVEMENT_ISSUE_LABELS
            .iter()
            .any(|label| self.has_label(label))
        {
            return Some(WorkspaceIssueType::Improvement);
        }
        if ENHANCEMENT_ISSUE_LABELS
            .iter()
            .any(|label| self.has_label(label))
        {
            return Some(WorkspaceIssueType::Enhancement);
        }
        None
    }
}

fn issue_priority_cmp(left: &SelectedIssue, right: &SelectedIssue) -> std::cmp::Ordering {
    right
        .has_priority_boost_label()
        .cmp(&left.has_priority_boost_label())
        .then_with(|| {
            left.primary_priority_rank()
                .cmp(&right.primary_priority_rank())
        })
        .then_with(|| right.created_at.cmp(&left.created_at))
}

#[derive(Debug, Clone, Deserialize)]
struct SelectedIssueLabel {
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SelectedPullRequest {
    number: u64,
    title: String,
    url: String,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "headRefName")]
    head_ref_name: Option<String>,
    state: Option<String>,
    #[serde(rename = "isDraft")]
    is_draft: Option<bool>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Vec<SelectedIssueLabel>,
    author: Option<SelectedGithubActor>,
}

impl SelectedPullRequest {
    fn has_label(&self, label: &str) -> bool {
        self.labels
            .iter()
            .any(|candidate| candidate.name.eq_ignore_ascii_case(label))
    }

    fn author_login(&self) -> Option<&str> {
        self.author.as_ref().map(|author| author.login.as_str())
    }

    fn is_open_dependency_upgrade_candidate(&self) -> bool {
        matches!(self.state.as_deref(), Some("OPEN") | Some("open"))
            && !self.is_draft.unwrap_or(false)
            && self.has_label(DEPENDENCY_UPGRADE_LABEL)
            && self
                .author_login()
                .is_some_and(|login| RENOVATE_LOGINS.iter().any(|candidate| login == *candidate))
    }

    fn is_non_major_dependency_upgrade(&self) -> bool {
        if MAJOR_DEPENDENCY_UPGRADE_LABELS
            .iter()
            .any(|label| self.has_label(label))
        {
            return false;
        }
        if NON_MAJOR_DEPENDENCY_UPGRADE_LABELS
            .iter()
            .any(|label| self.has_label(label))
        {
            return true;
        }
        dependency_upgrade_versions_from_text(&self.title)
            .or_else(|| {
                self.body
                    .as_deref()
                    .and_then(dependency_upgrade_versions_from_text)
            })
            .is_some_and(|(from_major, to_major)| from_major == to_major)
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SelectedGithubActor {
    login: String,
}

fn extract_dependency_upgrade_pr_marker(body: &str) -> Option<&str> {
    let marker_start = body.find(DEPENDENCY_UPGRADE_PR_MARKER_PREFIX)?;
    let content_start = marker_start + DEPENDENCY_UPGRADE_PR_MARKER_PREFIX.len();
    let content_end = body[content_start..]
        .find("-->")
        .map(|index| content_start + index)
        .unwrap_or(body.len());
    let value = body[content_start..content_end].trim();
    (!value.is_empty()).then_some(value)
}

fn extract_dependency_upgrade_pr_urls(body: &str) -> Vec<String> {
    if let Some(marker_start) = body.find(DEPENDENCY_UPGRADE_PR_BATCH_MARKER_PREFIX) {
        let content_start = marker_start + DEPENDENCY_UPGRADE_PR_BATCH_MARKER_PREFIX.len();
        let content_end = body[content_start..]
            .find("-->")
            .map(|index| content_start + index)
            .unwrap_or(body.len());
        let value = body[content_start..content_end].trim();
        if let Ok(pr_urls) = serde_json::from_str::<Vec<String>>(value) {
            return pr_urls;
        }
    }

    extract_dependency_upgrade_pr_marker(body)
        .map(|url| vec![url.to_string()])
        .unwrap_or_default()
}

fn dependency_upgrade_versions_from_text(text: &str) -> Option<(u64, u64)> {
    dependency_upgrade_versions_from_from_to_text(text)
        .or_else(|| dependency_upgrade_versions_from_arrow_text(text))
}

fn dependency_upgrade_versions_from_from_to_text(text: &str) -> Option<(u64, u64)> {
    let lower = text.to_ascii_lowercase();
    let from_index = lower.find(" from ")?;
    let to_index = lower[from_index + 6..].find(" to ")? + from_index + 6;
    let from_version = extract_leading_version(&text[from_index + 6..to_index])?;
    let to_version = extract_leading_version(&text[to_index + 4..])?;
    Some((from_version, to_version))
}

fn dependency_upgrade_versions_from_arrow_text(text: &str) -> Option<(u64, u64)> {
    for line in text.lines() {
        let Some((left, right)) = line
            .split_once('→')
            .or_else(|| line.split_once("->"))
            .or_else(|| line.split_once("=>"))
        else {
            continue;
        };
        let from_version = extract_trailing_version(left)?;
        let to_version = extract_leading_version(right)?;
        return Some((from_version, to_version));
    }
    None
}

fn extract_leading_version(text: &str) -> Option<u64> {
    let token = version_token_candidates(text)
        .find(|candidate| candidate.chars().any(|ch| ch.is_ascii_digit()))?;
    let token = token
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != '-')
        .trim_start_matches(['v', 'V']);
    let major = token
        .split(['.', '-'])
        .next()
        .filter(|segment| !segment.is_empty())?;
    major.parse::<u64>().ok()
}

fn extract_trailing_version(text: &str) -> Option<u64> {
    let token = version_token_candidates(text)
        .filter(|candidate| candidate.chars().any(|ch| ch.is_ascii_digit()))
        .last()?;
    let token = token
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.' && ch != '-')
        .trim_start_matches(['v', 'V']);
    let major = token
        .split(['.', '-'])
        .next()
        .filter(|segment| !segment.is_empty())?;
    major.parse::<u64>().ok()
}

fn version_token_candidates(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceSnapshot;
    use crate::services::codex_app_server::{CodexThreadActiveFlag, CodexThreadStatus};
    use crate::services::github_status_service::{
        GithubPrBuildState, GithubPrReviewState, GithubPrState, GithubPrStatus,
    };
    use crate::test_support::ENV_VAR_LOCK;
    use std::{
        collections::HashMap,
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::watch;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!("multicode-autonomous-tests-{unique}"));
            fs::create_dir_all(&path).expect("temp dir should be created");
            Self { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct EnvVarGuard {
        key: String,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &str, value: &std::path::Path) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: tests in this module serialize env-var mutation with ENV_VAR_LOCK.
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key: key.to_string(),
                original,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(original) = &self.original {
                // SAFETY: tests in this module serialize env-var mutation with ENV_VAR_LOCK.
                unsafe {
                    std::env::set_var(&self.key, original);
                }
            } else {
                // SAFETY: tests in this module serialize env-var mutation with ENV_VAR_LOCK.
                unsafe {
                    std::env::remove_var(&self.key);
                }
            }
        }
    }

    fn make_executable(path: &std::path::Path) {
        let mut permissions = fs::metadata(path)
            .expect("metadata should exist")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("permissions should be set");
    }

    #[test]
    fn normalize_github_repository_spec_accepts_owner_repo_and_urls() {
        assert_eq!(
            normalize_github_repository_spec("micronaut-projects/micronaut-core"),
            Some("micronaut-projects/micronaut-core".to_string())
        );
        assert_eq!(
            normalize_github_repository_spec(
                "https://github.com/micronaut-projects/micronaut-core"
            ),
            Some("micronaut-projects/micronaut-core".to_string())
        );
        assert_eq!(
            normalize_github_repository_spec(
                "https://github.com/micronaut-projects/micronaut-core.git/"
            ),
            Some("micronaut-projects/micronaut-core".to_string())
        );
        assert_eq!(normalize_github_repository_spec("invalid"), None);
    }

    #[test]
    fn normalize_github_issue_spec_accepts_numbers_refs_and_urls_for_assigned_repo() {
        let repository = "micronaut-projects/micronaut-core";
        assert_eq!(
            normalize_github_issue_spec(repository, "42"),
            Some("https://github.com/micronaut-projects/micronaut-core/issues/42".to_string())
        );
        assert_eq!(
            normalize_github_issue_spec(repository, "#42"),
            Some("https://github.com/micronaut-projects/micronaut-core/issues/42".to_string())
        );
        assert_eq!(
            normalize_github_issue_spec(repository, "micronaut-projects/micronaut-core#42"),
            Some("https://github.com/micronaut-projects/micronaut-core/issues/42".to_string())
        );
        assert_eq!(
            normalize_github_issue_spec(
                repository,
                "https://github.com/micronaut-projects/micronaut-core/issues/42",
            ),
            Some("https://github.com/micronaut-projects/micronaut-core/issues/42".to_string())
        );
        assert_eq!(
            normalize_github_issue_spec(
                repository,
                "https://github.com/micronaut-projects/micronaut-test/issues/42",
            ),
            None
        );
    }

    #[test]
    fn start_retry_is_blocked_only_for_same_scan_nonce() {
        assert!(start_retry_is_blocked(Some(4), 4));
        assert!(!start_retry_is_blocked(Some(4), 5));
        assert!(!start_retry_is_blocked(None, 4));
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("multicode-{name}-{nonce}"))
    }

    #[test]
    fn recover_codex_thread_candidates_from_session_logs_prefers_latest_for_matching_cwd() {
        let codex_home = unique_test_dir("codex-session-recovery");
        let sessions_dir = latest_codex_sessions_dir(&codex_home).join("2026/04/13");
        fs::create_dir_all(&sessions_dir).expect("session dir should be created");

        let cwd = "/tmp/multicode-codex-workspaces/e2e-test/work/multicode-test-39";
        let older = sessions_dir.join("rollout-2026-04-13T07-41-03-019d85c9.jsonl");
        let newer = sessions_dir.join("rollout-2026-04-13T07-46-51-019d85ce.jsonl");
        let unrelated = sessions_dir.join("rollout-2026-04-13T07-50-00-019d85ff.jsonl");

        fs::write(
            &older,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d85c9\",\"cwd\":\"{cwd}\",\"timestamp\":\"2026-04-13T07:41:03.609Z\"}}}}\n"
            ),
        )
        .expect("older session log should be written");
        fs::write(
            &newer,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d85ce\",\"cwd\":\"{cwd}\",\"timestamp\":\"2026-04-13T07:46:51.609Z\"}}}}\n"
            ),
        )
        .expect("newer session log should be written");
        fs::write(
            &unrelated,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"019d85ff\",\"cwd\":\"/tmp/other\",\"timestamp\":\"2026-04-13T07:50:00.000Z\"}}\n",
        )
        .expect("unrelated session log should be written");

        let recovered =
            recover_codex_thread_candidates_from_session_logs(&codex_home, &[cwd.to_string()]);
        let recovered = recovered
            .get(cwd)
            .expect("matching cwd should be present")
            .iter()
            .cloned()
            .reduce(|current, next| {
                if should_prefer_recovered_candidate(Some(&next), Some(&current)) {
                    next
                } else {
                    current
                }
            });

        assert_eq!(
            recovered.as_ref().map(|candidate| candidate.id.as_str()),
            Some("019d85ce")
        );

        let _ = fs::remove_dir_all(&codex_home);
    }

    #[test]
    fn recover_latest_codex_task_thread_candidate_prefers_autonomous_worker_over_newer_observer() {
        let codex_home = unique_test_dir("codex-task-session-recovery");
        let worker_dir = latest_codex_sessions_dir(&codex_home).join("2026/04/16");
        let observer_dir = latest_codex_sessions_dir(&codex_home).join("2026/04/17");
        fs::create_dir_all(&worker_dir).expect("worker session dir should be created");
        fs::create_dir_all(&observer_dir).expect("observer session dir should be created");

        let cwd = "/Users/graemerocher/dev/multicode-workspaces/sql/work/micronaut-sql-815";
        let issue_url = "https://github.com/micronaut-projects/micronaut-sql/issues/815";
        let pr_url = "https://github.com/micronaut-projects/micronaut-sql/pull/1921";
        let worker = worker_dir.join("rollout-2026-04-16T18-45-42-019d979d.jsonl");
        let observer = observer_dir.join("rollout-2026-04-17T07-28-28-019d9a57.jsonl");

        fs::write(
            &worker,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d979d\",\"cwd\":\"{}\",\"timestamp\":\"2026-04-16T18:45:42.032Z\"}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"You are operating in an autonomous multicode workspace for repository micronaut-projects/micronaut-sql.\\nStart work on GitHub issue {}.\\nFor this task, write autonomous state updates to `/Users/graemerocher/dev/multicode-workspaces/.multicode/automation/sql/tasks/task-815.state`.\\nThis issue already has a PR associated {}.\"}}]}}}}\n"
                ),
                cwd,
                issue_url,
                pr_url,
            ),
        )
        .expect("worker session log should be written");
        fs::write(
            &observer,
            format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"019d9a57\",\"cwd\":\"{}\",\"timestamp\":\"2026-04-17T07:28:28.902Z\"}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"this issue already has a PR associated {} ensure that is communicated to multicode UI\"}}]}}}}\n"
                ),
                cwd,
                pr_url,
            ),
        )
        .expect("observer session log should be written");

        let recovered = recover_latest_codex_task_thread_candidate(
            &codex_home,
            &CodexTaskRecoveryDescriptor {
                task_id: "task-815".to_string(),
                cwd: cwd.to_string(),
                issue_url: issue_url.to_string(),
                backing_pr_url: Some(pr_url.to_string()),
            },
        );

        assert_eq!(
            recovered.as_ref().map(|candidate| candidate.id.as_str()),
            Some("019d979d")
        );

        let _ = fs::remove_dir_all(&codex_home);
    }

    #[test]
    fn recover_codex_thread_candidates_from_state_db_reads_wal_snapshot() {
        let codex_home = unique_test_dir("codex-state-recovery");
        fs::create_dir_all(&codex_home).expect("codex home should be created");
        let state_db_path = codex_home.join("state_2026-04-14.sqlite");
        let mut connection = SqliteConnection::establish(state_db_path.to_string_lossy().as_ref())
            .expect("state database should be created");
        connection
            .batch_execute(
                "PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 CREATE TABLE threads (
                     id TEXT NOT NULL,
                     cwd TEXT NOT NULL,
                     updated_at BIGINT NOT NULL,
                     created_at BIGINT NOT NULL,
                     archived INTEGER NOT NULL DEFAULT 0,
                     has_user_event INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .expect("threads table should be created");
        connection
            .batch_execute(
                "INSERT INTO threads (
                     id,
                     cwd,
                     updated_at,
                     created_at,
                     archived,
                     has_user_event
                 ) VALUES (
                     'thread-322',
                     '/tmp/multicode-codex-workspaces/json-schema/work/micronaut-json-schema-322',
                     1713183212,
                     1713183200,
                     0,
                     1
                 );",
            )
            .expect("thread row should be inserted");
        drop(connection);

        let cwd = "/tmp/multicode-codex-workspaces/json-schema/work/micronaut-json-schema-322"
            .to_string();
        let recovered =
            recover_codex_thread_candidates_from_state_db(&codex_home, std::slice::from_ref(&cwd))
                .expect("WAL-backed state database should be recoverable");

        assert_eq!(
            recovered
                .get(cwd.as_str())
                .map(|candidate| candidate.id.as_str()),
            Some("thread-322")
        );

        let _ = fs::remove_dir_all(&codex_home);
    }

    #[test]
    fn snapshot_codex_state_db_reuses_cached_copy_when_source_is_unchanged() {
        let codex_home = unique_test_dir("codex-state-snapshot-reuse");
        fs::create_dir_all(&codex_home).expect("codex home should be created");
        let state_db_path = codex_home.join("state_2026-04-15.sqlite");
        let mut connection = SqliteConnection::establish(state_db_path.to_string_lossy().as_ref())
            .expect("state database should be created");
        connection
            .batch_execute(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE threads (
                     id TEXT NOT NULL,
                     cwd TEXT NOT NULL,
                     updated_at BIGINT NOT NULL,
                     created_at BIGINT NOT NULL,
                     archived INTEGER NOT NULL DEFAULT 0,
                     has_user_event INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO threads (
                     id,
                     cwd,
                     updated_at,
                     created_at,
                     archived,
                     has_user_event
                 ) VALUES (
                     'thread-1',
                     '/tmp/workspace-1',
                     1713183212,
                     1713183200,
                     0,
                     1
                 );",
            )
            .expect("state database should be populated");
        drop(connection);

        let first_snapshot =
            snapshot_codex_state_db(&state_db_path).expect("first snapshot should succeed");
        let second_snapshot =
            snapshot_codex_state_db(&state_db_path).expect("second snapshot should succeed");

        assert_eq!(first_snapshot.database_path, second_snapshot.database_path);

        let _ = fs::remove_dir_all(&codex_home);
    }

    #[test]
    fn snapshot_codex_state_db_refreshes_cached_copy_when_source_changes() {
        let codex_home = unique_test_dir("codex-state-snapshot-refresh");
        fs::create_dir_all(&codex_home).expect("codex home should be created");
        let state_db_path = codex_home.join("state_2026-04-15.sqlite");
        let mut connection = SqliteConnection::establish(state_db_path.to_string_lossy().as_ref())
            .expect("state database should be created");
        connection
            .batch_execute(
                "PRAGMA journal_mode = WAL;
                 PRAGMA wal_autocheckpoint = 0;
                 CREATE TABLE threads (
                     id TEXT NOT NULL,
                     cwd TEXT NOT NULL,
                     updated_at BIGINT NOT NULL,
                     created_at BIGINT NOT NULL,
                     archived INTEGER NOT NULL DEFAULT 0,
                     has_user_event INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO threads (
                     id,
                     cwd,
                     updated_at,
                     created_at,
                     archived,
                     has_user_event
                 ) VALUES (
                     'thread-1',
                     '/tmp/workspace-1',
                     1713183212,
                     1713183200,
                     0,
                     1
                 );",
            )
            .expect("state database should be populated");
        drop(connection);

        let first_snapshot =
            snapshot_codex_state_db(&state_db_path).expect("first snapshot should succeed");

        let mut connection = SqliteConnection::establish(state_db_path.to_string_lossy().as_ref())
            .expect("state database should reopen");
        connection
            .batch_execute(
                "INSERT INTO threads (
                     id,
                     cwd,
                     updated_at,
                     created_at,
                     archived,
                     has_user_event
                 ) VALUES (
                     'thread-2',
                     '/tmp/workspace-2',
                     1713183213,
                     1713183201,
                     0,
                     1
                 );",
            )
            .expect("second thread row should be inserted");
        drop(connection);

        let second_snapshot =
            snapshot_codex_state_db(&state_db_path).expect("second snapshot should succeed");

        assert_ne!(first_snapshot.database_path, second_snapshot.database_path);

        let _ = fs::remove_dir_all(&codex_home);
    }

    fn test_issue(
        number: u64,
        title: &str,
        url: &str,
        created_at: &str,
        labels: Vec<SelectedIssueLabel>,
    ) -> SelectedIssue {
        SelectedIssue {
            number,
            title: title.to_string(),
            url: url.to_string(),
            created_at: created_at.to_string(),
            state: Some("OPEN".to_string()),
            is_pull_request: Some(false),
            body: None,
            labels,
            dependency_upgrade_pr_urls: Vec::new(),
        }
    }

    fn test_pull_request(
        number: u64,
        title: &str,
        url: &str,
        labels: Vec<SelectedIssueLabel>,
        body: Option<&str>,
    ) -> SelectedPullRequest {
        SelectedPullRequest {
            number,
            title: title.to_string(),
            url: url.to_string(),
            created_at: Some("2026-04-09T10:00:00Z".to_string()),
            head_ref_name: None,
            state: Some("OPEN".to_string()),
            is_draft: Some(false),
            body: body.map(ToOwned::to_owned),
            labels,
            author: Some(SelectedGithubActor {
                login: "renovate[bot]".to_string(),
            }),
        }
    }

    #[test]
    fn find_next_issue_prioritizes_boost_labels_then_base_priority_then_newest() {
        let excluded = HashSet::from(["https://github.com/example/repo/issue/5".to_string()]);
        let mut issues = vec![
            test_issue(
                5,
                "already claimed",
                "https://github.com/example/repo/issue/5",
                "2026-04-09T10:00:00Z",
                vec![SelectedIssueLabel {
                    name: "type: bug".to_string(),
                }],
            ),
            test_issue(
                6,
                "busy",
                "https://github.com/example/repo/issue/6",
                "2026-04-09T11:00:00Z",
                vec![
                    SelectedIssueLabel {
                        name: "type: bug".to_string(),
                    },
                    SelectedIssueLabel {
                        name: IN_PROGRESS_LABEL.to_string(),
                    },
                ],
            ),
            test_issue(
                7,
                "plain bug",
                "https://github.com/example/repo/issue/7",
                "2026-04-09T09:00:00Z",
                vec![SelectedIssueLabel {
                    name: "type: bug".to_string(),
                }],
            ),
            test_issue(
                8,
                "high priority enhancement",
                "https://github.com/example/repo/issue/8",
                "2026-04-09T08:00:00Z",
                vec![
                    SelectedIssueLabel {
                        name: "type: enhancement".to_string(),
                    },
                    SelectedIssueLabel {
                        name: "priority: high".to_string(),
                    },
                ],
            ),
            test_issue(
                9,
                "regression bug",
                "https://github.com/example/repo/issue/9",
                "2026-04-09T07:00:00Z",
                vec![
                    SelectedIssueLabel {
                        name: "type: bug".to_string(),
                    },
                    SelectedIssueLabel {
                        name: "type: regression".to_string(),
                    },
                ],
            ),
        ];

        issues.sort_by(issue_priority_cmp);

        let selected = issues
            .into_iter()
            .filter(|issue| !issue.is_in_progress())
            .filter(|issue| !excluded.contains(&issue.url))
            .next()
            .expect("one issue should remain");

        assert_eq!(selected.number, 9);
    }

    #[test]
    fn selected_issue_treats_hyphenated_in_progress_label_as_in_progress() {
        let issue = test_issue(
            10,
            "busy elsewhere",
            "https://github.com/example/repo/issue/10",
            "2026-04-09T12:00:00Z",
            vec![
                SelectedIssueLabel {
                    name: "type: bug".to_string(),
                },
                SelectedIssueLabel {
                    name: "status: in-progress".to_string(),
                },
            ],
        );

        assert!(issue.is_in_progress());
    }

    #[test]
    fn selected_issue_detects_awaiting_validation_label() {
        let issue = test_issue(
            12,
            "needs validation",
            "https://github.com/example/repo/issues/12",
            "2026-04-09T12:00:00Z",
            vec![SelectedIssueLabel {
                name: AWAITING_VALIDATION_LABEL.to_string(),
            }],
        );

        assert!(issue.requires_validation());
        assert!(!should_assign_issue_to_current_user(&issue));
    }

    #[test]
    fn selected_issue_derives_issue_type_from_labels() {
        let regression = test_issue(
            14,
            "regression",
            "https://github.com/example/repo/issues/14",
            "2026-04-09T12:00:00Z",
            vec![SelectedIssueLabel {
                name: "type: regression".to_string(),
            }],
        );
        let docs = test_issue(
            15,
            "docs",
            "https://github.com/example/repo/issues/15",
            "2026-04-09T12:00:00Z",
            vec![SelectedIssueLabel {
                name: "type: docs".to_string(),
            }],
        );
        let dependency = test_issue(
            16,
            "deps",
            "https://github.com/example/repo/issues/16",
            "2026-04-09T12:00:00Z",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
        );

        assert_eq!(
            regression.issue_type(),
            Some(WorkspaceIssueType::Regression)
        );
        assert_eq!(docs.issue_type(), Some(WorkspaceIssueType::Docs));
        assert_eq!(
            dependency.issue_type(),
            Some(WorkspaceIssueType::DependencyUpgrade)
        );
    }

    #[test]
    fn select_validation_issue_candidate_skips_excluded_and_claimed_issues() {
        let excluded = HashSet::from(["https://github.com/example/repo/issues/13".to_string()]);
        let selected = select_validation_issue_candidate(
            vec![
                test_issue(
                    13,
                    "excluded validation issue",
                    "https://github.com/example/repo/issues/13",
                    "2026-04-09T12:00:00Z",
                    vec![SelectedIssueLabel {
                        name: AWAITING_VALIDATION_LABEL.to_string(),
                    }],
                ),
                test_issue(
                    14,
                    "already claimed validation issue",
                    "https://github.com/example/repo/issues/14",
                    "2026-04-09T13:00:00Z",
                    vec![
                        SelectedIssueLabel {
                            name: AWAITING_VALIDATION_LABEL.to_string(),
                        },
                        SelectedIssueLabel {
                            name: IN_PROGRESS_LABEL.to_string(),
                        },
                    ],
                ),
                test_issue(
                    15,
                    "fresh validation issue",
                    "https://github.com/example/repo/issues/15",
                    "2026-04-09T11:00:00Z",
                    vec![SelectedIssueLabel {
                        name: AWAITING_VALIDATION_LABEL.to_string(),
                    }],
                ),
            ],
            &excluded,
        )
        .expect("one validation issue should remain");

        assert_eq!(selected.number, 15);
    }

    #[test]
    fn discover_issue_backing_pr_url_prefers_explicit_issue_reference() {
        let issue = test_issue(
            322,
            "Dependency Injection Fails",
            "https://github.com/example/repo/issues/322",
            "2026-04-09T10:00:00Z",
            vec![],
        );
        let mut branch_only = test_pull_request(
            17,
            "Refactor configurer support",
            "https://github.com/example/repo/pull/17",
            vec![],
            None,
        );
        branch_only.head_ref_name = Some("fix-322-configurer".to_string());

        let explicit = test_pull_request(
            18,
            "Add regression coverage for issue #322",
            "https://github.com/example/repo/pull/18",
            vec![],
            Some("## Summary\n\nResolves #322"),
        );

        let discovered =
            discover_issue_backing_pr_url("example/repo", &issue, &[branch_only, explicit]);

        assert_eq!(
            discovered.as_deref(),
            Some("https://github.com/example/repo/pull/18")
        );
    }

    #[test]
    fn discover_issue_backing_pr_url_falls_back_to_branch_issue_number() {
        let issue = test_issue(
            161,
            "Generate plugins",
            "https://github.com/example/repo/issues/161",
            "2026-04-09T10:00:00Z",
            vec![],
        );
        let mut branch_match = test_pull_request(
            21,
            "WIP plugin generation changes",
            "https://github.com/example/repo/pull/21",
            vec![],
            None,
        );
        branch_match.head_ref_name = Some("fix-161-plugin-generation".to_string());

        let discovered = discover_issue_backing_pr_url("example/repo", &issue, &[branch_match]);

        assert_eq!(
            discovered.as_deref(),
            Some("https://github.com/example/repo/pull/21")
        );
    }

    #[test]
    fn discover_issue_backing_pr_url_does_not_match_partial_issue_reference() {
        let issue = test_issue(
            7,
            "Redis issue",
            "https://github.com/example/repo/issues/7",
            "2026-04-09T10:00:00Z",
            vec![],
        );
        let unrelated = test_pull_request(
            735,
            "chore(deps): update softprops/action-gh-release action to v2.6.2",
            "https://github.com/example/repo/pull/735",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
            Some(
                "This release fixes #705, #708, and #741 while discussing issue #764 in the changelog.",
            ),
        );

        let discovered = discover_issue_backing_pr_url("example/repo", &issue, &[unrelated]);

        assert_eq!(discovered, None);
    }

    #[test]
    fn discover_issue_backing_pr_url_allows_single_digit_issue_branch_with_keyword() {
        let issue = test_issue(
            7,
            "Redis issue",
            "https://github.com/example/repo/issues/7",
            "2026-04-09T10:00:00Z",
            vec![],
        );
        let mut branch_match = test_pull_request(
            22,
            "Fix redis issue",
            "https://github.com/example/repo/pull/22",
            vec![],
            None,
        );
        branch_match.head_ref_name = Some("issue-7-redis-timeout".to_string());

        let discovered = discover_issue_backing_pr_url("example/repo", &issue, &[branch_match]);

        assert_eq!(
            discovered.as_deref(),
            Some("https://github.com/example/repo/pull/22")
        );
    }

    #[test]
    fn startup_issue_scan_can_be_deferred_until_manual_request() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.persistent.assigned_repository = Some("example/repo".to_string());

        assert!(should_defer_startup_issue_scan(
            true, &snapshot, false, false
        ));
        assert!(!should_defer_startup_issue_scan(
            true, &snapshot, true, false
        ));
        assert!(!should_defer_startup_issue_scan(
            true, &snapshot, false, true
        ));

        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-1".to_string(),
                "https://github.com/example/repo/issues/1".to_string(),
                WorkspaceTaskSource::Manual,
            ));
        assert!(!should_defer_startup_issue_scan(
            true, &snapshot, false, false
        ));
    }

    #[test]
    fn issue_number_from_url_extracts_issue_number() {
        assert_eq!(
            issue_number_from_url("https://github.com/example/repo/issues/322"),
            Some(322)
        );
        assert_eq!(
            issue_number_from_url("https://github.com/example/repo/pull/322"),
            None
        );
    }

    #[test]
    fn issue_priority_cmp_prefers_newer_issue_with_same_priority_bucket() {
        let older = test_issue(
            10,
            "older",
            "https://github.com/example/repo/issues/10",
            "2026-04-09T07:00:00Z",
            vec![SelectedIssueLabel {
                name: "type: bug".to_string(),
            }],
        );
        let newer = test_issue(
            11,
            "newer",
            "https://github.com/example/repo/issues/11",
            "2026-04-09T08:00:00Z",
            vec![SelectedIssueLabel {
                name: "type: bug".to_string(),
            }],
        );

        assert_eq!(
            issue_priority_cmp(&older, &newer),
            std::cmp::Ordering::Greater
        );
        assert_eq!(issue_priority_cmp(&newer, &older), std::cmp::Ordering::Less);
    }

    #[test]
    fn issue_reference_formats_owner_repo_and_number() {
        assert_eq!(
            issue_reference("https://github.com/example/repo/issues/42"),
            Some("example/repo#42".to_string())
        );
        assert_eq!(
            issue_reference("https://github.com/example/repo/issue/43"),
            Some("example/repo#43".to_string())
        );
    }

    #[test]
    fn issue_search_args_excludes_linked_pull_requests() {
        let args = issue_search_args("example/repo", "type: bug");
        assert!(args.iter().any(|arg| arg == "-linked:pr"));
        assert!(args.windows(2).any(|pair| pair == ["--state", "open"]));
    }

    #[test]
    fn available_issue_scan_slots_stops_at_zero_when_queue_is_full() {
        assert_eq!(available_issue_scan_slots(5, 0), 5);
        assert_eq!(available_issue_scan_slots(5, 4), 1);
        assert_eq!(available_issue_scan_slots(5, 5), 0);
        assert_eq!(available_issue_scan_slots(5, 6), 0);
    }

    #[test]
    fn selected_issue_candidate_must_be_open_and_not_a_pull_request() {
        let open_issue = test_issue(
            1,
            "candidate",
            "https://github.com/example/repo/issues/1",
            "2026-04-09T10:00:00Z",
            vec![],
        );
        assert!(open_issue.is_open_issue_candidate());

        let closed_issue = SelectedIssue {
            state: Some("CLOSED".to_string()),
            ..open_issue.clone()
        };
        assert!(!closed_issue.is_open_issue_candidate());

        let pull_request = SelectedIssue {
            is_pull_request: Some(true),
            ..open_issue
        };
        assert!(!pull_request.is_open_issue_candidate());
    }

    #[test]
    fn ensure_workspace_task_claim_updates_workspace_state() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        let issue = test_issue(
            810,
            "candidate",
            "https://github.com/example/repo/issues/810",
            "2026-04-09T10:00:00Z",
            vec![SelectedIssueLabel {
                name: "type: bug".to_string(),
            }],
        );

        ensure_workspace_task_claim(
            &workspace,
            "example/repo",
            &issue,
            None,
            false,
            WorkspaceTaskSource::Scan,
        );

        let snapshot = workspace.subscribe().borrow().clone();
        assert_eq!(
            snapshot.persistent.assigned_repository.as_deref(),
            Some("example/repo")
        );
        assert!(snapshot.persistent.automation_issue.is_none());
        assert_eq!(snapshot.persistent.tasks.len(), 1);
        assert_eq!(snapshot.persistent.tasks[0].issue_url, issue.url);
        assert_eq!(
            snapshot.persistent.tasks[0].source,
            WorkspaceTaskSource::Scan
        );
        assert_eq!(
            snapshot.persistent.tasks[0].issue_type,
            Some(WorkspaceIssueType::Bug)
        );
        assert_eq!(snapshot.active_task_id.as_deref(), Some("task-810"));
    }

    #[test]
    fn refresh_existing_task_backing_pr_urls_backfills_missing_issue_type() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let bin_dir = root.path().join("bin");
            fs::create_dir_all(&bin_dir).expect("bin dir should exist");

            let fake_gh = bin_dir.join("gh");
            fs::write(
                &fake_gh,
                "#!/bin/sh\nif [ \"$1\" = \"pr\" ] && [ \"$2\" = \"list\" ]; then\n  printf '%s\\n' '[]'\n  exit 0\nfi\nif [ \"$1\" = \"issue\" ] && [ \"$2\" = \"view\" ]; then\n  printf '%s\\n' '{\"number\":42,\"title\":\"Investigate redis issue\",\"url\":\"https://github.com/example/repo/issues/42\",\"createdAt\":\"2026-04-09T10:00:00Z\",\"state\":\"OPEN\",\"body\":null,\"labels\":[{\"name\":\"type: bug\"}]}'\n  exit 0\nfi\nexit 1\n",
            )
            .expect("fake gh should be written");
            make_executable(&fake_gh);
            let _gh_guard = EnvVarGuard::set("MULTICODE_GH_COMMAND", &fake_gh);

            let workspace = Workspace::new(WorkspaceSnapshot::default());
            workspace.update(|snapshot| {
                snapshot
                    .persistent
                    .tasks
                    .push(WorkspaceTaskPersistentSnapshot::new(
                        "task-42".to_string(),
                        "https://github.com/example/repo/issues/42".to_string(),
                        WorkspaceTaskSource::Scan,
                    ));
                true
            });

            let snapshot = workspace.subscribe().borrow().clone();
            let updated = refresh_existing_task_backing_pr_urls(
                &workspace,
                &snapshot,
                "example/repo",
                "test-token",
            )
            .await
            .expect("refresh should succeed");

            assert_eq!(updated, 1);
            let next = workspace.subscribe().borrow().clone();
            assert_eq!(
                next.persistent.tasks[0].issue_type,
                Some(WorkspaceIssueType::Bug)
            );
            assert_eq!(
                next.persistent.tasks[0].issue_type_glyph.as_deref(),
                Some(workspace_issue_type_glyph(WorkspaceIssueType::Bug))
            );
        });
    }

    #[test]
    fn refresh_existing_task_backing_pr_urls_backfills_missing_issue_glyph_without_fetching_issue()
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let _env_lock = ENV_VAR_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let root = TestDir::new();
            let bin_dir = root.path().join("bin");
            fs::create_dir_all(&bin_dir).expect("bin dir should exist");

            let fake_gh = bin_dir.join("gh");
            fs::write(
                &fake_gh,
                "#!/bin/sh\nif [ \"$1\" = \"pr\" ] && [ \"$2\" = \"list\" ]; then\n  printf '%s\\n' '[]'\n  exit 0\nfi\nprintf '%s\\n' \"unexpected gh invocation: $*\" >&2\nexit 1\n",
            )
            .expect("fake gh should be written");
            make_executable(&fake_gh);
            let _gh_guard = EnvVarGuard::set("MULTICODE_GH_COMMAND", &fake_gh);

            let workspace = Workspace::new(WorkspaceSnapshot::default());
            workspace.update(|snapshot| {
                let mut task = WorkspaceTaskPersistentSnapshot::new(
                    "task-42".to_string(),
                    "https://github.com/example/repo/issues/42".to_string(),
                    WorkspaceTaskSource::Scan,
                );
                task.issue_type = Some(WorkspaceIssueType::Bug);
                snapshot.persistent.tasks.push(task);
                true
            });

            let snapshot = workspace.subscribe().borrow().clone();
            let updated = refresh_existing_task_backing_pr_urls(
                &workspace,
                &snapshot,
                "example/repo",
                "test-token",
            )
            .await
            .expect("refresh should succeed");

            assert_eq!(updated, 1);
            let next = workspace.subscribe().borrow().clone();
            assert_eq!(
                next.persistent.tasks[0].issue_type_glyph.as_deref(),
                Some(workspace_issue_type_glyph(WorkspaceIssueType::Bug))
            );
        });
    }

    #[test]
    fn sync_task_runtime_state_prunes_stale_entries_and_derives_active_task() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-810".to_string(),
                    "https://github.com/example/repo/issues/810".to_string(),
                    WorkspaceTaskSource::Manual,
                ));
            snapshot.persistent.automation_issue =
                Some("https://github.com/example/repo/issues/810".to_string());
            snapshot.task_states.insert(
                "task-stale".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    waiting_on_vm: true,
                    ..Default::default()
                },
            );
            snapshot.automation_session_id = Some("stale-session".to_string());
            snapshot.automation_agent_state = Some(AutomationAgentState::Working);
            snapshot.automation_session_status = Some(RootSessionStatus::Busy);
            true
        });

        let snapshot = workspace.subscribe().borrow().clone();
        sync_task_runtime_state(&workspace, &snapshot);

        let next = workspace.subscribe().borrow().clone();
        assert_eq!(next.active_task_id.as_deref(), Some("task-810"));
        assert_eq!(
            next.persistent.automation_issue.as_deref(),
            Some("https://github.com/example/repo/issues/810")
        );
        assert!(!next.task_states.contains_key("task-stale"));
        let task_state = next
            .task_states
            .get("task-810")
            .expect("task state should exist");
        assert_eq!(task_state.agent_state, None);
        assert!(!task_state.waiting_on_vm);
    }

    #[test]
    fn sync_task_runtime_state_clears_bridge_state_without_active_task() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot.active_task_id = Some("task-missing".to_string());
            snapshot.persistent.automation_issue =
                Some("https://github.com/example/repo/issues/999".to_string());
            snapshot.automation_session_id = Some("stale-session".to_string());
            snapshot.automation_agent_state = Some(AutomationAgentState::Working);
            snapshot.automation_session_status = Some(RootSessionStatus::Busy);
            true
        });

        let snapshot = workspace.subscribe().borrow().clone();
        sync_task_runtime_state(&workspace, &snapshot);

        let next = workspace.subscribe().borrow().clone();
        assert!(next.active_task_id.is_none());
        assert!(next.persistent.automation_issue.is_none());
        assert!(next.automation_session_id.is_none());
        assert!(next.automation_agent_state.is_none());
        assert!(next.automation_session_status.is_none());
    }

    #[test]
    fn sync_task_runtime_state_preserves_live_non_yieldable_active_task_lease() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-48".to_string(),
                    "https://github.com/example/repo/issues/48".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-42".to_string(),
                    "https://github.com/example/repo/issues/42".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-48".to_string());
            snapshot.persistent.automation_issue =
                Some("https://github.com/example/repo/issues/48".to_string());
            snapshot.task_states.insert(
                "task-48".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("thread-48".to_string()),
                    agent_state: Some(AutomationAgentState::Working),
                    session_status: Some(RootSessionStatus::Busy),
                    ..Default::default()
                },
            );
            true
        });

        let mut stale_snapshot = workspace.subscribe().borrow().clone();
        stale_snapshot.active_task_id = Some("task-42".to_string());
        stale_snapshot.persistent.automation_issue =
            Some("https://github.com/example/repo/issues/42".to_string());

        sync_task_runtime_state(&workspace, &stale_snapshot);

        let next = workspace.subscribe().borrow().clone();
        assert_eq!(next.active_task_id.as_deref(), Some("task-48"));
        assert_eq!(
            next.persistent.automation_issue.as_deref(),
            Some("https://github.com/example/repo/issues/48")
        );
    }

    #[test]
    fn sync_task_runtime_state_releases_yielded_active_task_lease() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-48".to_string(),
                    "https://github.com/example/repo/issues/48".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-48".to_string());
            snapshot.persistent.automation_issue =
                Some("https://github.com/example/repo/issues/48".to_string());
            snapshot.task_states.insert(
                "task-48".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("thread-48".to_string()),
                    agent_state: Some(AutomationAgentState::Review),
                    session_status: Some(RootSessionStatus::Idle),
                    ..Default::default()
                },
            );
            snapshot.automation_session_id = Some("thread-48".to_string());
            snapshot.automation_agent_state = Some(AutomationAgentState::Review);
            snapshot.automation_session_status = Some(RootSessionStatus::Idle);
            true
        });

        let snapshot = workspace.subscribe().borrow().clone();
        sync_task_runtime_state(&workspace, &snapshot);

        let next = workspace.subscribe().borrow().clone();
        assert!(next.active_task_id.is_none());
        assert!(next.persistent.automation_issue.is_none());
        assert!(next.automation_session_id.is_none());
        assert!(next.automation_agent_state.is_none());
        assert!(next.automation_session_status.is_none());
        let task_state = next
            .task_states
            .get("task-48")
            .expect("task state should remain");
        assert_eq!(task_state.session_id.as_deref(), Some("thread-48"));
        assert_eq!(task_state.agent_state, Some(AutomationAgentState::Review));
        assert_eq!(task_state.session_status, Some(RootSessionStatus::Idle));
        assert!(!task_state.waiting_on_vm);
    }

    #[test]
    fn set_paused_automation_state_is_idempotent() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot.persistent.assigned_repository = Some("example/repo".to_string());
            snapshot.active_task_id = Some("task-48".to_string());
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-48".to_string(),
                    "https://github.com/example/repo/issues/48".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.automation_session_id = Some("thread-48".to_string());
            snapshot.automation_agent_state = Some(AutomationAgentState::Working);
            snapshot.automation_session_status = Some(RootSessionStatus::Busy);
            snapshot.task_states.insert(
                "task-48".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("thread-48".to_string()),
                    agent_state: Some(AutomationAgentState::Working),
                    session_status: Some(RootSessionStatus::Busy),
                    waiting_on_vm: true,
                    ..Default::default()
                },
            );
            true
        });

        set_paused_automation_state(&workspace, "repo");
        let paused_once = workspace.subscribe().borrow().clone();
        set_paused_automation_state(&workspace, "repo");
        let paused_twice = workspace.subscribe().borrow().clone();

        assert_eq!(
            paused_once.automation_status,
            paused_twice.automation_status
        );
        assert_eq!(
            paused_once.automation_session_id,
            paused_twice.automation_session_id
        );
        assert_eq!(
            paused_once.automation_agent_state,
            paused_twice.automation_agent_state
        );
        assert_eq!(
            paused_once.automation_session_status,
            paused_twice.automation_session_status
        );
        assert_eq!(paused_once.task_states, paused_twice.task_states);
        assert_eq!(
            paused_twice.automation_status.as_deref(),
            Some("Paused repo")
        );
        assert!(paused_twice.automation_session_id.is_none());
        assert!(paused_twice.automation_agent_state.is_none());
        assert!(paused_twice.automation_session_status.is_none());
        let task_state = paused_twice
            .task_states
            .get("task-48")
            .expect("task state should remain");
        assert!(task_state.session_id.is_none());
        assert!(task_state.agent_state.is_none());
        assert!(task_state.session_status.is_none());
        assert!(!task_state.waiting_on_vm);
    }

    #[test]
    fn clear_automation_issue_claim_only_clears_matching_issue() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        let issue = test_issue(
            810,
            "candidate",
            "https://github.com/example/repo/issues/810",
            "2026-04-09T10:00:00Z",
            vec![],
        );

        ensure_workspace_task_claim(
            &workspace,
            "example/repo",
            &issue,
            None,
            false,
            WorkspaceTaskSource::Scan,
        );
        clear_automation_issue_claim(&workspace, "https://github.com/example/repo/issues/999");
        let unchanged = workspace.subscribe().borrow().clone();
        assert_eq!(
            unchanged.persistent.automation_issue.as_deref(),
            Some(issue.url.as_str())
        );

        clear_automation_issue_claim(&workspace, &issue.url);
        let cleared = workspace.subscribe().borrow().clone();
        assert!(cleared.persistent.automation_issue.is_none());
        assert!(cleared.persistent.tasks.is_empty());
        assert_eq!(
            cleared.persistent.assigned_repository.as_deref(),
            Some("example/repo")
        );
    }

    #[test]
    fn task_session_id_is_current_rejects_stale_session_id() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot.task_states.insert(
                "task-39".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("thread-new".to_string()),
                    ..Default::default()
                },
            );
            true
        });

        assert!(task_session_id_is_current(
            &workspace,
            "task-39",
            "thread-new"
        ));
        assert!(!task_session_id_is_current(
            &workspace,
            "task-39",
            "thread-old"
        ));
    }

    #[test]
    fn clear_automation_issue_claim_promotes_next_task_to_active_lease() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-810".to_string(),
                    "https://github.com/example/repo/issues/810".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-811".to_string(),
                    "https://github.com/example/repo/issues/811".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-810".to_string());
            snapshot.persistent.automation_issue =
                Some("https://github.com/example/repo/issues/810".to_string());
            snapshot.task_states.insert(
                "task-810".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("ses-810".to_string()),
                    agent_state: Some(AutomationAgentState::Working),
                    session_status: Some(RootSessionStatus::Busy),
                    ..Default::default()
                },
            );
            true
        });

        clear_automation_issue_claim(&workspace, "https://github.com/example/repo/issues/810");

        let next = workspace.subscribe().borrow().clone();
        assert_eq!(next.active_task_id.as_deref(), Some("task-811"));
        assert_eq!(
            next.persistent.automation_issue.as_deref(),
            Some("https://github.com/example/repo/issues/811")
        );
        assert!(!next.task_states.contains_key("task-810"));
        assert!(next.automation_session_id.is_none());
        assert!(next.automation_agent_state.is_none());
        assert!(next.automation_session_status.is_none());
    }

    #[test]
    fn issue_progress_status_uses_explicit_automation_states() {
        assert_eq!(
            issue_progress_status(
                "example/repo",
                "https://github.com/example/repo/issues/42",
                Some(AutomationAgentState::Working),
            ),
            "Working example/repo#42"
        );
        assert_eq!(
            issue_progress_status(
                "example/repo",
                "https://github.com/example/repo/issues/42",
                Some(AutomationAgentState::WaitingOnVm),
            ),
            "Waiting on VM example/repo#42"
        );
        assert_eq!(
            issue_progress_status(
                "example/repo",
                "https://github.com/example/repo/issues/42",
                Some(AutomationAgentState::Review),
            ),
            "Review example/repo#42"
        );
        assert_eq!(
            issue_progress_status(
                "example/repo",
                "https://github.com/example/repo/issues/42",
                Some(AutomationAgentState::Idle),
            ),
            "Wait close example/repo#42"
        );
    }

    #[test]
    fn sync_task_runtime_state_marks_only_blocked_non_active_tasks_waiting_on_vm() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-1".to_string(),
                    "https://github.com/example/repo/issues/1".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-2".to_string(),
                    "https://github.com/example/repo/issues/2".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-3".to_string(),
                    "https://github.com/example/repo/issues/3".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-4".to_string(),
                    "https://github.com/example/repo/issues/4".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-1".to_string());
            snapshot.task_states.insert(
                "task-1".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    agent_state: Some(AutomationAgentState::Working),
                    ..Default::default()
                },
            );
            snapshot.task_states.insert(
                "task-2".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    agent_state: Some(AutomationAgentState::Question),
                    ..Default::default()
                },
            );
            snapshot.task_states.insert(
                "task-3".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    agent_state: Some(AutomationAgentState::Idle),
                    ..Default::default()
                },
            );
            snapshot.task_states.insert(
                "task-4".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    ..Default::default()
                },
            );
            true
        });

        let snapshot = workspace.subscribe().borrow().clone();
        sync_task_runtime_state(&workspace, &snapshot);

        let next = workspace.subscribe().borrow().clone();
        assert!(
            !next
                .task_states
                .get("task-1")
                .expect("active task should exist")
                .waiting_on_vm
        );
        assert!(
            !next
                .task_states
                .get("task-2")
                .expect("question task should exist")
                .waiting_on_vm
        );
        assert!(
            !next
                .task_states
                .get("task-3")
                .expect("idle task should exist")
                .waiting_on_vm
        );
        let waiting_task = next
            .task_states
            .get("task-4")
            .expect("blocked task should exist");
        assert!(waiting_task.waiting_on_vm);
        assert_eq!(waiting_task.agent_state, None);
    }

    #[test]
    fn sync_task_runtime_state_relabels_blocked_working_task_as_waiting_on_vm() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-1".to_string(),
                    "https://github.com/example/repo/issues/1".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-2".to_string(),
                    "https://github.com/example/repo/issues/2".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-1".to_string());
            snapshot.task_states.insert(
                "task-2".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("thread-2".to_string()),
                    agent_state: Some(AutomationAgentState::Working),
                    session_status: Some(RootSessionStatus::Busy),
                    ..Default::default()
                },
            );
            true
        });

        let snapshot = workspace.subscribe().borrow().clone();
        sync_task_runtime_state(&workspace, &snapshot);

        let next = workspace.subscribe().borrow().clone();
        let task_state = next
            .task_states
            .get("task-2")
            .expect("blocked task should exist");
        assert!(task_state.waiting_on_vm);
        assert_eq!(task_state.agent_state, Some(AutomationAgentState::Working));
    }

    #[test]
    fn sync_task_runtime_state_prefers_claimed_issue_over_stale_unpreserved_active_task() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-1".to_string(),
                    "https://github.com/example/repo/issues/1".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-2".to_string(),
                    "https://github.com/example/repo/issues/2".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-1".to_string());
            snapshot.persistent.automation_issue =
                Some("https://github.com/example/repo/issues/2".to_string());
            snapshot.task_states.insert(
                "task-1".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("thread-1".to_string()),
                    agent_state: Some(AutomationAgentState::Review),
                    session_status: Some(RootSessionStatus::Idle),
                    ..Default::default()
                },
            );
            true
        });

        let snapshot = workspace.subscribe().borrow().clone();
        sync_task_runtime_state(&workspace, &snapshot);

        let next = workspace.subscribe().borrow().clone();
        assert_eq!(next.active_task_id.as_deref(), Some("task-2"));
        assert_eq!(
            next.persistent.automation_issue.as_deref(),
            Some("https://github.com/example/repo/issues/2")
        );
    }

    #[test]
    fn sync_task_runtime_state_updates_bridge_state_from_normalized_active_task() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-1".to_string(),
                    "https://github.com/example/repo/issues/1".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-1".to_string());
            snapshot.persistent.automation_issue =
                Some("https://github.com/example/repo/issues/1".to_string());
            snapshot.automation_agent_state = Some(AutomationAgentState::Working);
            snapshot.automation_session_status = Some(RootSessionStatus::Busy);
            snapshot.task_states.insert(
                "task-1".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("thread-1".to_string()),
                    session_status: Some(RootSessionStatus::Idle),
                    agent_state: Some(AutomationAgentState::Working),
                    ..Default::default()
                },
            );
            true
        });

        let snapshot = workspace.subscribe().borrow().clone();
        sync_task_runtime_state(&workspace, &snapshot);

        let next = workspace.subscribe().borrow().clone();
        assert!(next.active_task_id.is_none());
        assert_eq!(next.automation_agent_state, None);
        assert_eq!(next.automation_session_status, None);
    }

    #[test]
    fn closed_background_task_issue_urls_only_returns_closed_non_active_tasks() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-26".to_string(),
                "https://github.com/example/repo/issues/26".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-16".to_string(),
                "https://github.com/example/repo/issues/16".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-14".to_string(),
                "https://github.com/example/repo/issues/14".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.active_task_id = Some("task-16".to_string());
        snapshot.persistent.automation_issue =
            Some("https://github.com/example/repo/issues/16".to_string());

        let (closed_sender, closed_rx) = watch::channel(Some(GithubStatus::Issue(
            super::super::github_status_service::GithubIssueStatus {
                state: super::super::github_status_service::GithubIssueState::Closed,
                fetched_at: std::time::SystemTime::now(),
            },
        )));
        let (_open_sender, open_rx) = watch::channel(Some(GithubStatus::Issue(
            super::super::github_status_service::GithubIssueStatus {
                state: super::super::github_status_service::GithubIssueState::Open,
                fetched_at: std::time::SystemTime::now(),
            },
        )));
        let mut task_issue_status_rxs = HashMap::new();
        task_issue_status_rxs.insert(
            "https://github.com/example/repo/issues/26".to_string(),
            closed_rx,
        );
        task_issue_status_rxs.insert(
            "https://github.com/example/repo/issues/16".to_string(),
            open_rx,
        );
        drop(closed_sender);

        assert_eq!(
            closed_background_task_issue_urls(
                &snapshot,
                Some("https://github.com/example/repo/issues/16"),
                &task_issue_status_rxs
            ),
            vec!["https://github.com/example/repo/issues/26".to_string()]
        );
    }

    #[test]
    fn next_schedulable_task_issue_url_skips_tasks_with_live_or_terminal_agent_states() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-1".to_string(),
                "https://github.com/example/repo/issues/1".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-2".to_string(),
                "https://github.com/example/repo/issues/2".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-3".to_string(),
                "https://github.com/example/repo/issues/3".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-4".to_string(),
                "https://github.com/example/repo/issues/4".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.task_states.insert(
            "task-1".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                agent_state: Some(AutomationAgentState::Working),
                ..Default::default()
            },
        );
        snapshot.task_states.insert(
            "task-2".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                agent_state: Some(AutomationAgentState::Question),
                ..Default::default()
            },
        );
        snapshot.task_states.insert(
            "task-3".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                agent_state: Some(AutomationAgentState::Review),
                ..Default::default()
            },
        );

        assert_eq!(
            next_schedulable_task_issue_url(&snapshot, None).as_deref(),
            Some("https://github.com/example/repo/issues/4")
        );
        assert_eq!(
            next_schedulable_task_issue_url(
                &snapshot,
                Some("https://github.com/example/repo/issues/4")
            ),
            None
        );
    }

    #[test]
    fn next_schedulable_task_issue_url_includes_waiting_on_vm_task_with_working_state() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-1".to_string(),
                "https://github.com/example/repo/issues/1".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-2".to_string(),
                "https://github.com/example/repo/issues/2".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.task_states.insert(
            "task-1".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                agent_state: Some(AutomationAgentState::Review),
                session_status: Some(RootSessionStatus::Idle),
                session_id: Some("thread-1".to_string()),
                ..Default::default()
            },
        );
        snapshot.task_states.insert(
            "task-2".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                agent_state: Some(AutomationAgentState::Working),
                session_status: Some(RootSessionStatus::Busy),
                session_id: Some("thread-2".to_string()),
                waiting_on_vm: true,
                ..Default::default()
            },
        );

        assert_eq!(
            next_schedulable_task_issue_url(&snapshot, None).as_deref(),
            Some("https://github.com/example/repo/issues/2")
        );
    }

    #[test]
    fn task_can_yield_vm_only_for_non_working_states() {
        assert!(!task_can_yield_vm(Some(AutomationAgentState::Working)));
        assert!(task_can_yield_vm(Some(AutomationAgentState::Question)));
        assert!(task_can_yield_vm(Some(AutomationAgentState::Review)));
        assert!(task_can_yield_vm(Some(AutomationAgentState::Idle)));
        assert!(task_can_yield_vm(Some(AutomationAgentState::Stale)));
        assert!(!task_can_yield_vm(None));
    }

    #[test]
    fn active_task_can_yield_vm_requires_existing_session() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-5".to_string(),
                "https://github.com/example/repo/issues/5".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.active_task_id = Some("task-5".to_string());
        snapshot.task_states.insert(
            "task-5".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                session_id: None,
                agent_state: Some(AutomationAgentState::Idle),
                ..Default::default()
            },
        );

        assert!(!active_task_can_yield_vm(&snapshot));

        snapshot.task_states.insert(
            "task-5".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-5".to_string()),
                agent_state: Some(AutomationAgentState::Idle),
                ..Default::default()
            },
        );

        assert!(active_task_can_yield_vm(&snapshot));
    }

    #[test]
    fn active_task_can_yield_vm_requires_explicit_task_state() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-5".to_string(),
                "https://github.com/example/repo/issues/5".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.active_task_id = Some("task-5".to_string());
        snapshot.automation_agent_state = Some(AutomationAgentState::Idle);
        snapshot.task_states.insert(
            "task-5".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-5".to_string()),
                agent_state: None,
                ..Default::default()
            },
        );

        assert!(!active_task_can_yield_vm(&snapshot));

        snapshot.task_states.insert(
            "task-5".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-5".to_string()),
                agent_state: Some(AutomationAgentState::Review),
                ..Default::default()
            },
        );

        assert!(active_task_can_yield_vm(&snapshot));
    }

    #[test]
    fn set_automation_runtime_state_preserves_existing_task_session() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-5".to_string(),
                    "https://github.com/example/repo/issues/5".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-5".to_string());
            snapshot.task_states.insert(
                "task-5".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("task-session-5".to_string()),
                    agent_state: Some(AutomationAgentState::Working),
                    session_status: Some(RootSessionStatus::Busy),
                    ..Default::default()
                },
            );
            true
        });

        set_automation_runtime_state(
            &workspace,
            Some("root-session".to_string()),
            Some(RootSessionStatus::Idle),
        );

        let snapshot = workspace.subscribe().borrow().clone();
        assert_eq!(
            snapshot.automation_session_id.as_deref(),
            Some("root-session")
        );
        let task_state = snapshot
            .task_states
            .get("task-5")
            .expect("task state should remain");
        assert_eq!(task_state.session_id.as_deref(), Some("task-session-5"));
        assert_eq!(task_state.agent_state, Some(AutomationAgentState::Working));
        assert_eq!(task_state.session_status, Some(RootSessionStatus::Busy));
    }

    #[test]
    fn set_automation_runtime_state_updates_matching_task_session_to_working() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-7".to_string(),
                    "https://github.com/example/repo/issues/7".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-7".to_string());
            snapshot.persistent.automation_issue =
                Some("https://github.com/example/repo/issues/7".to_string());
            snapshot.task_states.insert(
                "task-7".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("task-session-7".to_string()),
                    ..Default::default()
                },
            );
            true
        });

        set_automation_runtime_state(
            &workspace,
            Some("task-session-7".to_string()),
            Some(RootSessionStatus::Busy),
        );

        let snapshot = workspace.subscribe().borrow().clone();
        assert_eq!(
            snapshot.automation_session_id.as_deref(),
            Some("task-session-7")
        );
        assert_eq!(
            snapshot.automation_agent_state,
            Some(AutomationAgentState::Working)
        );
        assert_eq!(
            snapshot.automation_session_status,
            Some(RootSessionStatus::Busy)
        );
        let task_state = snapshot
            .task_states
            .get("task-7")
            .expect("task state should remain");
        assert_eq!(task_state.session_id.as_deref(), Some("task-session-7"));
        assert_eq!(task_state.agent_state, Some(AutomationAgentState::Working));
        assert_eq!(task_state.session_status, Some(RootSessionStatus::Busy));
    }

    #[test]
    fn codex_task_thread_status_maps_idle_to_review() {
        assert_eq!(
            codex_task_thread_status_to_agent_state(&CodexThreadStatus::Idle, None),
            AutomationAgentState::Review
        );
        assert_eq!(
            agent_state_root_status(AutomationAgentState::Review),
            RootSessionStatus::Idle
        );
    }

    #[test]
    fn codex_task_thread_status_maps_waiting_flags_to_question() {
        let status = CodexThreadStatus::Active {
            active_flags: vec![CodexThreadActiveFlag::WaitingOnApproval],
        };

        assert_eq!(
            codex_task_thread_status_to_agent_state(&status, None),
            AutomationAgentState::Question
        );
        assert_eq!(
            agent_state_root_status(AutomationAgentState::Question),
            RootSessionStatus::Question
        );
    }

    #[test]
    fn codex_task_thread_status_preserves_review_for_ambiguous_active_state() {
        let status = CodexThreadStatus::Active {
            active_flags: vec![],
        };

        assert_eq!(
            codex_task_thread_status_to_agent_state(&status, Some(AutomationAgentState::Review)),
            AutomationAgentState::Working
        );
        assert_eq!(
            codex_task_thread_status_to_agent_state(&status, Some(AutomationAgentState::Question)),
            AutomationAgentState::Working
        );
    }

    #[test]
    fn set_task_runtime_state_from_codex_marks_reviewing_task_yieldable() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-7".to_string(),
                    "https://github.com/example/repo/issues/7".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-7".to_string());
            snapshot.task_states.insert(
                "task-7".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("task-session-7".to_string()),
                    agent_state: Some(AutomationAgentState::Working),
                    session_status: Some(RootSessionStatus::Busy),
                    ..Default::default()
                },
            );
            true
        });

        set_task_runtime_state_from_codex(
            &workspace,
            "task-7",
            "task-session-7",
            RootSessionStatus::Idle,
            AutomationAgentState::Review,
            Some(CodexTaskMetadata {
                issues: vec!["https://github.com/example/repo/issues/7".to_string()],
                prs: vec!["https://github.com/example/repo/pull/11".to_string()],
                ..Default::default()
            }),
            Some(54_611),
        );

        let snapshot = workspace.subscribe().borrow().clone();
        assert_eq!(
            snapshot.automation_session_id.as_deref(),
            Some("task-session-7")
        );
        assert_eq!(
            snapshot.automation_agent_state,
            Some(AutomationAgentState::Review)
        );
        assert_eq!(
            snapshot.automation_session_status,
            Some(RootSessionStatus::Idle)
        );
        let task_state = snapshot
            .task_states
            .get("task-7")
            .expect("task state should remain");
        assert_eq!(task_state.agent_state, Some(AutomationAgentState::Review));
        assert_eq!(task_state.session_status, Some(RootSessionStatus::Idle));
        assert_eq!(
            task_state.issue,
            vec!["https://github.com/example/repo/issues/7".to_string()]
        );
        assert_eq!(
            task_state.pr,
            vec!["https://github.com/example/repo/pull/11".to_string()]
        );
        assert_eq!(task_state.usage_total_tokens, Some(54_611));
        assert!(active_task_can_yield_vm(&snapshot));
    }

    #[test]
    fn set_task_runtime_state_from_codex_keeps_blocked_task_waiting_on_vm() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-1".to_string(),
                    "https://github.com/example/repo/issues/1".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-2".to_string(),
                    "https://github.com/example/repo/issues/2".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-1".to_string());
            snapshot.task_states.insert(
                "task-2".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    session_id: Some("task-session-2".to_string()),
                    waiting_on_vm: true,
                    ..Default::default()
                },
            );
            true
        });

        set_task_runtime_state_from_codex(
            &workspace,
            "task-2",
            "task-session-2",
            RootSessionStatus::Busy,
            AutomationAgentState::Working,
            None,
            None,
        );

        let snapshot = workspace.subscribe().borrow().clone();
        let task_state = snapshot
            .task_states
            .get("task-2")
            .expect("blocked task should exist");
        assert_eq!(task_state.agent_state, Some(AutomationAgentState::Working));
        assert_eq!(task_state.session_status, Some(RootSessionStatus::Busy));
        assert!(task_state.waiting_on_vm);
    }

    #[test]
    fn set_task_runtime_state_from_codex_sets_pr_created_status_for_review_tasks() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot.persistent.assigned_repository = Some("example/repo".to_string());
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-33".to_string(),
                    "https://github.com/example/repo/issues/33".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.active_task_id = Some("task-33".to_string());
            snapshot.task_states.insert(
                "task-33".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    status: Some("Resuming in background".to_string()),
                    ..Default::default()
                },
            );
            true
        });

        set_task_runtime_state_from_codex(
            &workspace,
            "task-33",
            "task-session-33",
            RootSessionStatus::Idle,
            AutomationAgentState::Review,
            Some(CodexTaskMetadata {
                prs: vec!["https://github.com/example/repo/pull/56".to_string()],
                ..Default::default()
            }),
            None,
        );

        let snapshot = workspace.subscribe().borrow().clone();
        let task_state = snapshot
            .task_states
            .get("task-33")
            .expect("task state should exist");
        assert_eq!(task_state.status.as_deref(), Some("PR created #56"));
    }

    #[test]
    fn set_task_runtime_state_from_codex_filters_foreign_metadata_prs() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot.persistent.assigned_repository =
                Some("micronaut-projects/micronaut-redis".to_string());
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-733".to_string(),
                    "https://github.com/micronaut-projects/micronaut-redis/issues/733".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            true
        });

        set_task_runtime_state_from_codex(
            &workspace,
            "task-733",
            "task-session-733",
            RootSessionStatus::Idle,
            AutomationAgentState::Review,
            Some(CodexTaskMetadata {
                issues: vec![
                    "https://github.com/micronaut-projects/micronaut-redis/issues/733".to_string(),
                    "https://github.com/example/repo/issues/733".to_string(),
                ],
                prs: vec![
                    "https://github.com/example/repo/pull/338".to_string(),
                    "https://github.com/micronaut-projects/micronaut-redis/pull/733".to_string(),
                ],
                ..Default::default()
            }),
            None,
        );

        let snapshot = workspace.subscribe().borrow().clone();
        let task_state = snapshot
            .task_states
            .get("task-733")
            .expect("task state should exist");
        assert_eq!(
            task_state.issue,
            vec!["https://github.com/micronaut-projects/micronaut-redis/issues/733".to_string()]
        );
        assert_eq!(
            task_state.pr,
            vec!["https://github.com/micronaut-projects/micronaut-redis/pull/733".to_string()]
        );
        let task = snapshot
            .persistent
            .tasks
            .iter()
            .find(|task| task.id == "task-733")
            .expect("task should exist");
        assert_eq!(
            task.backing_pr_url.as_deref(),
            Some("https://github.com/micronaut-projects/micronaut-redis/pull/733")
        );
    }

    #[test]
    fn should_refresh_task_links_after_codex_review_detects_resumed_background_completion() {
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            agent_state: Some(AutomationAgentState::Working),
            session_status: Some(RootSessionStatus::Busy),
            status: Some("Resuming in background".to_string()),
            ..Default::default()
        };

        assert!(should_refresh_task_links_after_codex_review(
            &task_state,
            RootSessionStatus::Idle,
            AutomationAgentState::Review
        ));
    }

    #[test]
    fn should_refresh_task_links_after_codex_review_ignores_stable_review_state() {
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            agent_state: Some(AutomationAgentState::Review),
            session_status: Some(RootSessionStatus::Idle),
            status: Some("PR created #56".to_string()),
            ..Default::default()
        };

        assert!(!should_refresh_task_links_after_codex_review(
            &task_state,
            RootSessionStatus::Idle,
            AutomationAgentState::Review
        ));
    }

    #[test]
    fn task_link_refresh_urls_collects_issue_and_prs_without_duplicates() {
        let task = WorkspaceTaskPersistentSnapshot::new(
            "task-33".to_string(),
            "https://github.com/example/repo/issues/33".to_string(),
            WorkspaceTaskSource::Scan,
        )
        .with_backing_pr_url(Some("https://github.com/example/repo/pull/56".to_string()));
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            pr: vec![
                "https://github.com/example/repo/pull/56".to_string(),
                "https://github.com/example/repo/pull/57".to_string(),
            ],
            ..Default::default()
        };
        let metadata = CodexTaskMetadata {
            prs: vec![
                "https://github.com/example/repo/pull/57".to_string(),
                "https://github.com/example/repo/pull/58".to_string(),
            ],
            ..Default::default()
        };

        assert_eq!(
            task_link_refresh_urls(&task, &task_state, &metadata),
            vec![
                "https://github.com/example/repo/issues/33".to_string(),
                "https://github.com/example/repo/pull/57".to_string(),
                "https://github.com/example/repo/pull/58".to_string(),
                "https://github.com/example/repo/pull/56".to_string(),
            ]
        );
    }

    #[test]
    fn codex_task_status_text_falls_back_to_persisted_backing_pr_url() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.persistent.tasks.push(
            WorkspaceTaskPersistentSnapshot::new(
                "task-33".to_string(),
                "https://github.com/example/repo/issues/33".to_string(),
                WorkspaceTaskSource::Scan,
            )
            .with_backing_pr_url(Some("https://github.com/example/repo/pull/56".to_string())),
        );

        assert_eq!(
            codex_task_status_text(AutomationAgentState::Review, None, "task-33", &snapshot)
                .as_deref(),
            Some("PR created #56")
        );
    }

    #[test]
    fn set_task_runtime_state_from_codex_clears_resuming_status_once_task_is_working() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-33".to_string(),
                    "https://github.com/example/repo/issues/33".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            snapshot.task_states.insert(
                "task-33".to_string(),
                crate::WorkspaceTaskRuntimeSnapshot {
                    status: Some("Resuming in background".to_string()),
                    ..Default::default()
                },
            );
            true
        });

        set_task_runtime_state_from_codex(
            &workspace,
            "task-33",
            "task-session-33",
            RootSessionStatus::Busy,
            AutomationAgentState::Working,
            Some(CodexTaskMetadata::default()),
            None,
        );

        let snapshot = workspace.subscribe().borrow().clone();
        let task_state = snapshot
            .task_states
            .get("task-33")
            .expect("task state should exist");
        assert_eq!(task_state.status, None);
    }

    #[test]
    fn unloaded_codex_task_runtime_preserves_question_state() {
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            agent_state: Some(AutomationAgentState::Question),
            ..Default::default()
        };

        assert_eq!(
            unloaded_codex_task_runtime(&task_state, false),
            (RootSessionStatus::Question, AutomationAgentState::Question)
        );
    }

    #[test]
    fn unloaded_codex_task_runtime_marks_interrupted_working_task_as_stale() {
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            agent_state: Some(AutomationAgentState::Working),
            ..Default::default()
        };

        assert_eq!(
            unloaded_codex_task_runtime(&task_state, false),
            (RootSessionStatus::Idle, AutomationAgentState::Stale)
        );
    }

    #[test]
    fn codex_task_metadata_from_turns_extracts_issue_and_pr_tags() {
        let turns = vec![crate::services::codex_app_server::CodexThreadTurn {
            items: vec![serde_json::json!({
                "type": "agentMessage",
                "text": "<multicode:repo>/tmp/multicode-codex-workspaces/e2e-test/work/multicode-test-1</multicode:repo>\n<multicode:issue>https://github.com/graemerocher/multicode-test/issues/1</multicode:issue>\n<multicode:pr>https://github.com/graemerocher/multicode-test/pull/8</multicode:pr>"
            })],
        }];

        let metadata = codex_task_metadata_from_turns(
            &turns,
            "https://github.com/graemerocher/multicode-test/issues/1",
        );

        assert_eq!(
            metadata.repositories,
            vec!["/tmp/multicode-codex-workspaces/e2e-test/work/multicode-test-1".to_string()]
        );
        assert_eq!(
            metadata.issues,
            vec!["https://github.com/graemerocher/multicode-test/issues/1".to_string()]
        );
        assert_eq!(
            metadata.prs,
            vec!["https://github.com/graemerocher/multicode-test/pull/8".to_string()]
        );
    }

    #[test]
    fn codex_task_metadata_from_turns_extracts_pr_tag_from_tool_output() {
        let turns = vec![crate::services::codex_app_server::CodexThreadTurn {
            items: vec![serde_json::json!({
                "type": "toolCallOutput",
                "output": "Chunk ID: c4018f\n<multicode:pr>https://github.com/graemerocher/multicode-test/pull/338</multicode:pr>\n"
            })],
        }];

        let metadata = codex_task_metadata_from_turns(
            &turns,
            "https://github.com/graemerocher/multicode-test/issues/322",
        );

        assert_eq!(
            metadata.issues,
            vec!["https://github.com/graemerocher/multicode-test/issues/322".to_string()]
        );
        assert_eq!(
            metadata.prs,
            vec!["https://github.com/graemerocher/multicode-test/pull/338".to_string()]
        );
    }

    #[test]
    fn set_task_runtime_state_from_codex_persists_detected_pr_on_task() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        workspace.update(|snapshot| {
            snapshot
                .persistent
                .tasks
                .push(WorkspaceTaskPersistentSnapshot::new(
                    "task-322".to_string(),
                    "https://github.com/example/repo/issues/322".to_string(),
                    WorkspaceTaskSource::Scan,
                ));
            true
        });

        set_task_runtime_state_from_codex(
            &workspace,
            "task-322",
            "task-session-322",
            RootSessionStatus::Idle,
            AutomationAgentState::Review,
            Some(CodexTaskMetadata {
                prs: vec!["https://github.com/example/repo/pull/338".to_string()],
                ..Default::default()
            }),
            None,
        );

        let snapshot = workspace.subscribe().borrow().clone();
        let task = snapshot
            .persistent
            .tasks
            .iter()
            .find(|task| task.id == "task-322")
            .expect("task should exist");
        assert_eq!(
            task.backing_pr_url.as_deref(),
            Some("https://github.com/example/repo/pull/338")
        );
    }

    #[test]
    fn codex_usage_total_tokens_from_session_log_contents_reads_latest_token_count() {
        let contents = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":123}}}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":456789}}}}
"#;

        assert_eq!(
            codex_usage_total_tokens_from_session_log_contents(contents),
            Some(456_789)
        );
    }

    #[test]
    fn non_active_codex_working_task_keeps_working_state_for_runtime_tracking() {
        let status = CodexThreadStatus::Active {
            active_flags: vec![],
        };
        let task_state = crate::WorkspaceTaskRuntimeSnapshot::default();
        assert_eq!(
            codex_runtime_state_for_task(&status, &task_state),
            (RootSessionStatus::Busy, AutomationAgentState::Working)
        );
    }

    #[test]
    fn active_codex_thread_does_not_preserve_review_state() {
        let status = CodexThreadStatus::Active {
            active_flags: vec![],
        };
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            agent_state: Some(AutomationAgentState::Review),
            session_status: Some(RootSessionStatus::Idle),
            ..Default::default()
        };

        assert_eq!(
            codex_runtime_state_for_task(&status, &task_state),
            (RootSessionStatus::Busy, AutomationAgentState::Working)
        );
    }

    #[test]
    fn unloaded_codex_status_preserves_review_runtime_state() {
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            agent_state: Some(AutomationAgentState::Review),
            session_status: Some(RootSessionStatus::Idle),
            ..Default::default()
        };

        let runtime = if matches!(CodexThreadStatus::NotLoaded, CodexThreadStatus::NotLoaded) {
            unloaded_codex_task_runtime(&task_state, false)
        } else {
            unreachable!()
        };

        assert_eq!(
            runtime,
            (RootSessionStatus::Idle, AutomationAgentState::Review)
        );
    }

    #[test]
    fn normalized_task_agent_state_recovers_legacy_waiting_on_vm_review_state() {
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-8".to_string()),
            session_status: Some(RootSessionStatus::Idle),
            agent_state: Some(AutomationAgentState::WaitingOnVm),
            ..Default::default()
        };

        assert_eq!(
            normalized_task_agent_state(&task_state),
            Some(AutomationAgentState::Review)
        );
    }

    #[test]
    fn normalized_task_agent_state_prefers_idle_session_status_over_stale_working_state() {
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-9".to_string()),
            session_status: Some(RootSessionStatus::Idle),
            agent_state: Some(AutomationAgentState::Working),
            ..Default::default()
        };

        assert_eq!(
            normalized_task_agent_state(&task_state),
            Some(AutomationAgentState::Review)
        );
    }

    #[test]
    fn unloaded_codex_status_preserves_review_for_pr_backed_working_task() {
        let task_state = crate::WorkspaceTaskRuntimeSnapshot {
            agent_state: Some(AutomationAgentState::Working),
            session_status: Some(RootSessionStatus::Busy),
            ..Default::default()
        };

        let runtime = if matches!(CodexThreadStatus::NotLoaded, CodexThreadStatus::NotLoaded) {
            unloaded_codex_task_runtime(&task_state, true)
        } else {
            unreachable!()
        };

        assert_eq!(
            runtime,
            (RootSessionStatus::Idle, AutomationAgentState::Review)
        );
    }

    #[test]
    fn should_start_assigned_issue_work_when_active_task_has_no_session() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-6".to_string(),
                "https://github.com/example/repo/issues/6".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.active_task_id = Some("task-6".to_string());
        snapshot.automation_agent_state = Some(AutomationAgentState::Idle);
        snapshot.automation_session_status = Some(RootSessionStatus::Idle);

        assert!(should_start_assigned_issue_work(
            &snapshot,
            RootSessionStatus::Idle
        ));
    }

    #[test]
    fn should_not_start_assigned_issue_work_when_active_task_already_has_session() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-6".to_string(),
                "https://github.com/example/repo/issues/6".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.active_task_id = Some("task-6".to_string());
        snapshot.task_states.insert(
            "task-6".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-task-6".to_string()),
                ..Default::default()
            },
        );

        assert!(!should_start_assigned_issue_work(
            &snapshot,
            RootSessionStatus::Idle
        ));
        assert!(!should_start_assigned_issue_work(
            &snapshot,
            RootSessionStatus::Busy
        ));
    }

    #[test]
    fn should_start_assigned_issue_work_when_active_task_is_stale() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot
            .persistent
            .tasks
            .push(WorkspaceTaskPersistentSnapshot::new(
                "task-6".to_string(),
                "https://github.com/example/repo/issues/6".to_string(),
                WorkspaceTaskSource::Scan,
            ));
        snapshot.active_task_id = Some("task-6".to_string());
        snapshot.task_states.insert(
            "task-6".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-task-6".to_string()),
                session_status: Some(RootSessionStatus::Idle),
                agent_state: Some(AutomationAgentState::Stale),
                ..Default::default()
            },
        );

        assert!(should_start_assigned_issue_work(
            &snapshot,
            RootSessionStatus::Idle
        ));
    }

    #[test]
    fn should_not_start_assigned_issue_work_when_stale_task_has_backing_pr() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.persistent.tasks.push(
            WorkspaceTaskPersistentSnapshot::new(
                "task-6".to_string(),
                "https://github.com/example/repo/issues/6".to_string(),
                WorkspaceTaskSource::Scan,
            )
            .with_backing_pr_url(Some("https://github.com/example/repo/pull/66".to_string())),
        );
        snapshot.active_task_id = Some("task-6".to_string());
        snapshot.task_states.insert(
            "task-6".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-task-6".to_string()),
                session_status: Some(RootSessionStatus::Idle),
                agent_state: Some(AutomationAgentState::Stale),
                ..Default::default()
            },
        );

        assert!(!should_start_assigned_issue_work(
            &snapshot,
            RootSessionStatus::Idle
        ));
    }

    #[test]
    fn should_start_assigned_issue_work_when_stale_task_has_dependency_upgrade_pr() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.persistent.tasks.push(
            WorkspaceTaskPersistentSnapshot::new(
                "task-6".to_string(),
                "https://github.com/example/repo/issues/6".to_string(),
                WorkspaceTaskSource::Scan,
            )
            .with_backing_pr_url(Some("https://github.com/example/repo/pull/66".to_string()))
            .with_dependency_upgrade_backing_pr(true),
        );
        snapshot.active_task_id = Some("task-6".to_string());
        snapshot.task_states.insert(
            "task-6".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-task-6".to_string()),
                session_status: Some(RootSessionStatus::Idle),
                agent_state: Some(AutomationAgentState::Stale),
                ..Default::default()
            },
        );

        assert!(should_start_assigned_issue_work(
            &snapshot,
            RootSessionStatus::Idle
        ));
    }

    #[test]
    fn should_not_bridge_root_runtime_state_for_codex() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.root_session_id = Some("root-thread".to_string());

        assert!(!should_bridge_root_runtime_state(
            AgentProvider::Codex,
            &snapshot,
            false,
            RootSessionStatus::Busy,
        ));
    }

    #[test]
    fn should_bridge_root_runtime_state_for_non_codex_busy_root() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.root_session_id = Some("root-thread".to_string());

        assert!(should_bridge_root_runtime_state(
            AgentProvider::Opencode,
            &snapshot,
            false,
            RootSessionStatus::Busy,
        ));
        assert!(!should_bridge_root_runtime_state(
            AgentProvider::Opencode,
            &snapshot,
            true,
            RootSessionStatus::Busy,
        ));
        assert!(!should_bridge_root_runtime_state(
            AgentProvider::Opencode,
            &snapshot,
            false,
            RootSessionStatus::Idle,
        ));
    }

    #[test]
    fn build_issue_prompt_requires_skills_and_publish_approval() {
        let issue = test_issue(
            980,
            "candidate",
            "https://github.com/example/repo/issues/980",
            "2026-04-09T10:00:00Z",
            vec![],
        );

        let prompt = build_issue_prompt(
            "example/repo",
            &issue,
            None,
            "thread-task-980",
            std::path::Path::new("/tmp/work/example-repo-980"),
            std::path::Path::new("/tmp/state/task-980.state"),
        );

        assert!(prompt.contains("`independent-fix`"));
        assert!(prompt.contains("`machine-readable-pr`"));
        assert!(prompt.contains("`autonomous-state`"));
        assert!(prompt.contains("Primary checkout for this task: /tmp/work/example-repo-980"));
        assert!(prompt.contains("write autonomous state updates to `/tmp/state/task-980.state`"));
        assert!(prompt.contains("Use the existing checkout at `/tmp/work/example-repo-980`"));
        assert!(
            prompt
                .contains("write autonomous state updates in the format `<state>:thread-task-980`")
        );
        assert!(prompt.contains(
            "Run repository commands, builds, Gradle tasks, and focused tests as needed without asking for permission."
        ));
        assert!(prompt.contains("Do not commit, push, comment, or open/update a pull request until the user explicitly approves publishing."));
        assert!(prompt.contains(
            "When the change is ready for review, stage the full task checkout with `git add -A`"
        ));
        assert!(prompt.contains("include an appropriate type label such as `type: docs`"));
        assert!(prompt.contains("`type: bug`"));
        assert!(prompt.contains("`type: improvement`"));
        assert!(prompt.contains("`type: enhancement`"));
    }

    #[test]
    fn build_issue_prompt_with_generic_backing_pr_still_requires_publish_approval() {
        let issue = test_issue(
            982,
            "ordinary issue with associated PR",
            "https://github.com/example/repo/issues/982",
            "2026-04-09T10:00:00Z",
            vec![],
        );

        let prompt = build_issue_prompt(
            "example/repo",
            &issue,
            None,
            "thread-task-982",
            std::path::Path::new("/tmp/work/example-repo-982"),
            std::path::Path::new("/tmp/state/task-982.state"),
        );

        assert!(prompt.contains("explicitly approves publishing"));
        assert!(prompt.contains("stage the full task checkout with `git add -A`"));
        assert!(!prompt.contains("backed by Renovate pull request"));
        assert!(!prompt.contains("merge it without waiting for human review"));
    }

    #[test]
    fn build_issue_validation_prompt_enforces_qa_only_flow() {
        let issue = test_issue(
            983,
            "awaiting validation",
            "https://github.com/example/repo/issues/983",
            "2026-04-09T10:00:00Z",
            vec![SelectedIssueLabel {
                name: AWAITING_VALIDATION_LABEL.to_string(),
            }],
        );

        let prompt = build_issue_prompt(
            "example/repo",
            &issue,
            None,
            "thread-task-983",
            std::path::Path::new("/tmp/work/example-repo-983"),
            std::path::Path::new("/tmp/state/task-983.state"),
        );

        assert!(prompt.contains("acting as a QA engineer"));
        assert!(prompt.contains("Do not implement a fix, do not commit, do not push"));
        assert!(prompt.contains("Do not assign the issue to anyone."));
        assert!(prompt.contains("status: bug"));
        assert!(prompt.contains("closed: cannot reproduce"));
        assert!(prompt.contains("Replace `status: awaiting validation`"));
        assert!(prompt.contains("leave the issue in review state"));
        assert!(!prompt.contains("explicitly approves publishing"));
        assert!(!prompt.contains("3. Implement the fix."));
    }

    #[test]
    fn resolved_autonomous_task_prompt_prefers_saved_resume_prompt() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.task_states.insert(
            "task-361".to_string(),
            crate::WorkspaceTaskRuntimeSnapshot {
                resume_prompt: Some(
                    "Apply the extra Redis fix from the attached session.".to_string(),
                ),
                ..Default::default()
            },
        );
        let issue = test_issue(
            361,
            "Redis issue",
            "https://github.com/example/repo/issues/361",
            "2026-04-09T10:00:00Z",
            vec![],
        );

        let prompt = resolved_autonomous_task_prompt(
            &snapshot,
            "task-361",
            "example/repo",
            &issue,
            None,
            "thread-task-361",
            std::path::Path::new("/tmp/work/example-repo-361"),
            std::path::Path::new("/tmp/state/task-361.state"),
        );

        assert_eq!(
            prompt,
            "Apply the extra Redis fix from the attached session."
        );
    }

    #[test]
    fn build_issue_prompt_for_dependency_upgrade_requires_combined_pr() {
        let mut issue = test_issue(
            981,
            "dependency upgrade",
            "https://github.com/example/repo/issues/981",
            "2026-04-09T10:00:00Z",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
        );
        issue.dependency_upgrade_pr_urls = vec![
            "https://github.com/example/repo/pull/88".to_string(),
            "https://github.com/example/repo/pull/91".to_string(),
        ];

        let prompt = build_issue_prompt(
            "example/repo",
            &issue,
            Some("https://github.com/example/repo/pull/999"),
            "thread-task-981",
            std::path::Path::new("/tmp/work/example-repo-981"),
            std::path::Path::new("/tmp/state/task-981.state"),
        );

        assert!(prompt.contains("tracks the following Renovate pull requests"));
        assert!(prompt.contains("https://github.com/example/repo/pull/88"));
        assert!(prompt.contains("https://github.com/example/repo/pull/91"));
        assert!(prompt.contains(
            "Treat https://github.com/example/repo/pull/999 as the single combined dependency-upgrade PR"
        ));
        assert!(prompt.contains(
            "Monitor CI for https://github.com/example/repo/pull/999 until it completes."
        ));
        assert!(prompt.contains(
            "push follow-up fixes to the combined PR, and keep monitoring until all required checks are green"
        ));
        assert!(prompt.contains("Do not merge the individual Renovate PRs directly."));
        assert!(prompt.contains("close GitHub issue https://github.com/example/repo/issues/981"));
        assert!(prompt.contains(
            "Do not leave placeholder comments, placeholder reviews, or dummy approvals"
        ));
        assert!(!prompt.contains("explicitly approves publishing"));
    }

    #[test]
    fn build_issue_prompt_for_single_dependency_upgrade_merges_direct_pr() {
        let mut issue = test_issue(
            982,
            "single dependency upgrade",
            "https://github.com/example/repo/issues/982",
            "2026-04-09T10:00:00Z",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
        );
        issue.dependency_upgrade_pr_urls =
            vec!["https://github.com/example/repo/pull/999".to_string()];

        let prompt = build_issue_prompt(
            "example/repo",
            &issue,
            Some("https://github.com/example/repo/pull/999"),
            "thread-task-982",
            std::path::Path::new("/tmp/work/example-repo-982"),
            std::path::Path::new("/tmp/state/task-982.state"),
        );

        assert!(prompt.contains("tracks the following Renovate pull request"));
        assert!(prompt.contains(
            "Confirm https://github.com/example/repo/pull/999 is still a safe non-major update."
        ));
        assert!(
            prompt.contains("rebase and merge https://github.com/example/repo/pull/999 directly")
        );
        assert!(prompt.contains("Do not create a combined dependency-upgrade pull request"));
        assert!(!prompt.contains("Treat https://github.com/example/repo/pull/999 as the single combined dependency-upgrade PR"));
    }

    #[test]
    fn dependency_upgrade_issue_body_uses_updated_queue_text() {
        let body = dependency_upgrade_issue_body(&[
            test_pull_request(
                91,
                "Update dependency io.micronaut:micronaut-core from 4.4.1 to 4.4.2",
                "https://github.com/example/repo/pull/91",
                vec![SelectedIssueLabel {
                    name: DEPENDENCY_UPGRADE_LABEL.to_string(),
                }],
                None,
            ),
            test_pull_request(
                92,
                "Update dependency io.micronaut:micronaut-json-core from 4.4.1 to 4.4.2",
                "https://github.com/example/repo/pull/92",
                vec![SelectedIssueLabel {
                    name: DEPENDENCY_UPGRADE_LABEL.to_string(),
                }],
                None,
            ),
        ]);

        assert!(body.contains("Current Renovate candidates:"));
        assert!(body.contains("https://github.com/example/repo/pull/91"));
        assert!(body.contains("https://github.com/example/repo/pull/92"));
        assert!(body.contains(
            "Create or update a single combined dependency-upgrade pull request associated with this issue."
        ));
        assert!(body.contains(
            "Monitor CI for that combined PR until it completes; if any required check fails, investigate it, push follow-up fixes, and keep monitoring until all required checks are green."
        ));
        assert!(body.contains("Do not merge the individual Renovate PRs directly."));
    }

    #[test]
    fn single_dependency_upgrade_issue_body_uses_direct_merge_text() {
        let body = single_dependency_upgrade_issue_body(&test_pull_request(
            734,
            "fix(deps): update dependency io.lettuce:lettuce-core to v7.5.1.release",
            "https://github.com/example/repo/pull/734",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
            None,
        ));

        assert!(body.contains("Track dependency-upgrade automation for Renovate pull request https://github.com/example/repo/pull/734."));
        assert!(
            body.contains("This issue was created automatically by multicode to prrocess the PR.")
        );
        assert!(body.contains("rebase and merge the PR without waiting for human review"));
        assert!(!body.contains("single combined dependency-upgrade pull request"));
        assert_eq!(
            extract_dependency_upgrade_pr_urls(&body),
            vec!["https://github.com/example/repo/pull/734".to_string()]
        );
    }

    #[test]
    fn merged_dependency_upgrade_issue_urls_ignores_non_dependency_tasks() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.persistent.tasks.push(
            WorkspaceTaskPersistentSnapshot::new(
                "task-485".to_string(),
                "https://github.com/example/repo/issues/485".to_string(),
                WorkspaceTaskSource::Scan,
            )
            .with_backing_pr_url(Some("https://github.com/example/repo/pull/900".to_string())),
        );
        let (tx, rx) = watch::channel(Some(GithubStatus::Pr(GithubPrStatus {
            state: GithubPrState::Merged,
            build: GithubPrBuildState::Succeeded,
            review: GithubPrReviewState::Accepted,
            is_draft: false,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
        })));
        let _ = tx;
        let receivers = HashMap::from([(snapshot.persistent.tasks[0].issue_url.clone(), rx)]);

        assert!(merged_dependency_upgrade_issue_urls(&snapshot, &receivers).is_empty());
    }

    #[test]
    fn merged_dependency_upgrade_issue_urls_includes_dependency_upgrade_tasks() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.persistent.tasks.push(
            WorkspaceTaskPersistentSnapshot::new(
                "task-981".to_string(),
                "https://github.com/example/repo/issues/981".to_string(),
                WorkspaceTaskSource::Scan,
            )
            .with_backing_pr_url(Some("https://github.com/example/repo/pull/88".to_string()))
            .with_dependency_upgrade_backing_pr(true),
        );
        let (tx, rx) = watch::channel(Some(GithubStatus::Pr(GithubPrStatus {
            state: GithubPrState::Merged,
            build: GithubPrBuildState::Succeeded,
            review: GithubPrReviewState::Accepted,
            is_draft: false,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
        })));
        let _ = tx;
        let receivers = HashMap::from([(snapshot.persistent.tasks[0].issue_url.clone(), rx)]);

        assert_eq!(
            merged_dependency_upgrade_issue_urls(&snapshot, &receivers),
            vec!["https://github.com/example/repo/issues/981".to_string()]
        );
    }

    #[test]
    fn selected_pull_request_non_major_detection_prefers_safe_signals() {
        let patch = test_pull_request(
            88,
            "Update dependency io.micronaut:micronaut-http-client from 4.4.0 to 4.4.1",
            "https://github.com/example/repo/pull/88",
            vec![
                SelectedIssueLabel {
                    name: DEPENDENCY_UPGRADE_LABEL.to_string(),
                },
                SelectedIssueLabel {
                    name: "patch".to_string(),
                },
            ],
            None,
        );
        assert!(patch.is_open_dependency_upgrade_candidate());
        assert!(patch.is_non_major_dependency_upgrade());

        let major = test_pull_request(
            89,
            "Update dependency io.micronaut:micronaut-http-client from 4.4.1 to 5.0.0",
            "https://github.com/example/repo/pull/89",
            vec![
                SelectedIssueLabel {
                    name: DEPENDENCY_UPGRADE_LABEL.to_string(),
                },
                SelectedIssueLabel {
                    name: "major".to_string(),
                },
            ],
            None,
        );
        assert!(!major.is_non_major_dependency_upgrade());

        let inferred_minor = test_pull_request(
            90,
            "Update dependency io.micronaut:micronaut-http-client from 4.4.1 to 4.5.0",
            "https://github.com/example/repo/pull/90",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
            None,
        );
        assert!(inferred_minor.is_non_major_dependency_upgrade());
    }

    #[test]
    fn extract_dependency_upgrade_pr_urls_reads_hidden_comment() {
        let body = dependency_upgrade_issue_body(&[
            test_pull_request(
                91,
                "Update dependency io.micronaut:micronaut-core from 4.4.1 to 4.4.2",
                "https://github.com/example/repo/pull/91",
                vec![SelectedIssueLabel {
                    name: DEPENDENCY_UPGRADE_LABEL.to_string(),
                }],
                None,
            ),
            test_pull_request(
                92,
                "Update dependency io.micronaut:micronaut-json-core from 4.4.1 to 4.4.2",
                "https://github.com/example/repo/pull/92",
                vec![SelectedIssueLabel {
                    name: DEPENDENCY_UPGRADE_LABEL.to_string(),
                }],
                None,
            ),
        ]);

        assert_eq!(
            extract_dependency_upgrade_pr_urls(&body),
            vec![
                "https://github.com/example/repo/pull/91".to_string(),
                "https://github.com/example/repo/pull/92".to_string()
            ]
        );
    }

    #[test]
    fn dependency_upgrade_issue_title_includes_batch_size() {
        let queries = dependency_upgrade_issue_title(&[
            test_pull_request(
                728,
                "fix(deps): update dependency io.lettuce:lettuce-core to v7.5.1.release",
                "https://github.com/micronaut-projects/micronaut-redis/pull/728",
                vec![SelectedIssueLabel {
                    name: DEPENDENCY_UPGRADE_LABEL.to_string(),
                }],
                None,
            ),
            test_pull_request(
                729,
                "fix(deps): update dependency io.micronaut.redis:micronaut-redis-lettuce to v6.6.0",
                "https://github.com/micronaut-projects/micronaut-redis/pull/729",
                vec![SelectedIssueLabel {
                    name: DEPENDENCY_UPGRADE_LABEL.to_string(),
                }],
                None,
            ),
        ]);

        assert_eq!(
            queries,
            "Dependency upgrade follow-up for Renovate batch (2 PRs)".to_string()
        );
    }

    #[test]
    fn single_dependency_upgrade_issue_title_uses_pull_request_number() {
        let title = single_dependency_upgrade_issue_title(&test_pull_request(
            728,
            "fix(deps): update dependency io.lettuce:lettuce-core to v7.5.1.release",
            "https://github.com/example/repo/pull/728",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
            None,
        ));

        assert_eq!(title, "Dependency upgrade follow-up for Renovate PR #728");
    }

    #[test]
    fn dependency_upgrade_versions_from_arrow_text_parses_renovate_table_rows() {
        let body = "This PR contains the following updates:\n\n| Package | Change |\n|---|---|\n| io.micronaut.security:micronaut-security-bom | `4.16.1` → `4.17.1` |\n";

        assert_eq!(dependency_upgrade_versions_from_text(body), Some((4, 4)));
    }

    #[test]
    fn dependency_upgrade_versions_from_arrow_text_detects_major_renovate_rows() {
        let body = "This PR contains the following updates:\n\n| Package | Type | Change |\n|---|---|---|\n| softprops/action-gh-release | action | `v2.5.0` → `v3.0.0` |\n";

        assert_eq!(dependency_upgrade_versions_from_text(body), Some((2, 3)));
    }

    #[test]
    fn dependency_upgrade_non_major_detection_accepts_renovate_body_without_minor_label() {
        let body = "This PR contains the following updates:\n\n| Package | Change |\n|---|---|\n| io.micronaut.security:micronaut-security-bom | `4.16.1` → `4.17.1` |\n";
        let pr = test_pull_request(
            682,
            "fix(deps): update dependency io.micronaut.security:micronaut-security-bom to v4.17.1",
            "https://github.com/example/repo/pull/682",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
            Some(body),
        );

        assert!(pr.is_non_major_dependency_upgrade());
    }

    #[test]
    fn dependency_upgrade_non_major_detection_rejects_renovate_major_body_without_major_label() {
        let body = "This PR contains the following updates:\n\n| Package | Type | Change |\n|---|---|---|\n| softprops/action-gh-release | action | `v2.5.0` → `v3.0.0` |\n";
        let pr = test_pull_request(
            696,
            "chore(deps): update softprops/action-gh-release action to v3",
            "https://github.com/example/repo/pull/696",
            vec![SelectedIssueLabel {
                name: DEPENDENCY_UPGRADE_LABEL.to_string(),
            }],
            Some(body),
        );

        assert!(!pr.is_non_major_dependency_upgrade());
    }
}
