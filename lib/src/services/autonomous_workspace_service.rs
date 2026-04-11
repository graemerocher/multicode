use std::{collections::HashSet, time::Duration};

use serde::Deserialize;
use tokio::{
    process::Command,
    sync::watch,
    time::{Instant, sleep_until},
};

use super::{
    CombinedService, GithubStatus,
    runtime::{AUTOMATION_STATE_ENV, automation_state_file_source},
    workspace_watch::monitor_workspace_snapshots,
};
use crate::{
    AutomationAgentState, RootSessionStatus, WorkspaceManagerError, WorkspaceSnapshot,
    manager::Workspace,
};

const ISSUE_PRIORITY_LABELS: [&str; 4] = [
    "type: bug",
    "type:docs",
    "type: improvement",
    "type: enhancement",
];
const ISSUE_PRIORITY_BOOST_LABELS: [&str; 2] = ["type: regression", "priority: high"];
const IN_PROGRESS_LABEL: &str = "status: in progress";
const ISSUE_SCAN_RETRY_DELAY: Duration = Duration::from_secs(60);

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
                tokio::spawn(async move {
                    watch_workspace(service, key, workspace, workspace_rx).await;
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
    let issue_scan_delay = Duration::from_secs(service.config.autonomous.issue_scan_delay_seconds);
    let issue_scan_delay = issue_scan_delay.max(Duration::from_secs(1));
    let mut next_scan_at: Option<Instant> = None;
    let mut watched_issue_url: Option<String> = None;
    let mut issue_status_rx: Option<watch::Receiver<Option<GithubStatus>>> = None;
    let mut previous_root_status: Option<RootSessionStatus> = None;
    let mut previous_scan_request_nonce: u64 = 0;
    let mut blocked_start_scan_request_nonce: Option<u64> = None;

    loop {
        let snapshot = workspace_rx.borrow().clone();
        let assigned_repository = snapshot.persistent.assigned_repository.clone();
        let scan_requested = snapshot.automation_scan_request_nonce != previous_scan_request_nonce;
        previous_scan_request_nonce = snapshot.automation_scan_request_nonce;

        if snapshot.persistent.archived || assigned_repository.is_none() {
            watched_issue_url = None;
            issue_status_rx = None;
            next_scan_at = None;
            blocked_start_scan_request_nonce = None;
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
            next_scan_at = None;
            blocked_start_scan_request_nonce = None;
            previous_root_status = snapshot.root_session_status;
            clear_automation_runtime_state(&workspace);
            set_automation_status(&workspace, Some(format!("Stopped {repository_label}")));
            if workspace_rx.changed().await.is_err() {
                break;
            }
            continue;
        }
        if scan_requested {
            blocked_start_scan_request_nonce = None;
        }
        if scan_requested
            && snapshot.persistent.automation_issue.is_none()
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

        let current_issue_url = snapshot.persistent.automation_issue.clone();
        if let Some(current_issue_url) = current_issue_url {
            let root_status = snapshot
                .root_session_status
                .unwrap_or(RootSessionStatus::Idle);
            let should_resume_assigned_issue = matches!(root_status, RootSessionStatus::Idle)
                && (scan_requested
                    || (previous_root_status.is_none()
                        && snapshot.automation_agent_state.is_none()
                        && snapshot.automation_session_status.is_none()));

            if !should_resume_assigned_issue
                && snapshot.automation_session_id.is_none()
                && snapshot.root_session_id.is_some()
                && !matches!(root_status, RootSessionStatus::Idle)
            {
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
                workspace.update(|next| {
                    let mut changed = false;
                    if next.persistent.automation_issue.as_deref()
                        == Some(current_issue_url.as_str())
                    {
                        next.persistent.automation_issue = None;
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
                watched_issue_url = None;
                issue_status_rx = None;
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

        set_automation_status(&workspace, Some(format!("Scan issues {repository_label}")));
        match claim_next_issue(
            &service,
            &workspace,
            &workspace_key,
            &snapshot,
            &assigned_repository,
        )
        .await
        {
            Ok(Some(issue)) => {
                watched_issue_url = Some(issue.url.clone());
                issue_status_rx = service.github_status_service().watch_status(&issue.url);
                next_scan_at = None;
                set_automation_status(
                    &workspace,
                    Some(format!("Working {}", issue.display_reference())),
                );
            }
            Ok(None) => {
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

async fn claim_next_issue(
    service: &CombinedService,
    workspace: &Workspace,
    workspace_key: &str,
    snapshot: &WorkspaceSnapshot,
    assigned_repository: &str,
) -> Result<Option<SelectedIssue>, String> {
    let excluded_issue_urls = active_issue_urls(service, workspace_key);
    let token = resolved_gh_token(service).await?;
    let Some(issue) = find_next_issue(assigned_repository, &excluded_issue_urls, &token).await?
    else {
        return Ok(None);
    };

    set_automation_status(
        workspace,
        Some(format!("Claiming {}", issue.display_reference())),
    );
    persist_automation_issue_claim(workspace, assigned_repository, &issue);
    clear_automation_state_file(service, workspace_key).await;

    if let Err(err) = prompt_root_session(service, snapshot, assigned_repository, &issue).await {
        clear_automation_issue_claim(workspace, &issue.url);
        tracing::warn!(
            workspace_key,
            issue_url = %issue.url,
            error = %err,
            "failed to start autonomous issue work after reserving issue"
        );
        return Err(err);
    }

    set_automation_runtime_state(
        workspace,
        snapshot.root_session_id.clone(),
        Some(RootSessionStatus::Busy),
    );

    if let Err(err) =
        add_work_started_comment(assigned_repository, &issue, workspace_key, &token).await
    {
        tracing::warn!(
            workspace_key,
            issue_url = %issue.url,
            error = %err,
            "autonomous issue prompt started but adding work-started comment failed"
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
            "autonomous issue prompt started but adding in-progress label failed"
        );
        return Err(err);
    }

    Ok(Some(issue))
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

    set_automation_status(
        workspace,
        Some(format!("Claiming {}", issue.display_reference())),
    );
    clear_automation_state_file(service, workspace_key).await;

    if let Err(err) = prompt_root_session(service, snapshot, assigned_repository, &issue).await {
        tracing::warn!(
            workspace_key,
            issue_url = %issue.url,
            error = %err,
            "failed to start manually assigned issue work"
        );
        return Err(err);
    }

    set_automation_runtime_state(
        workspace,
        snapshot.root_session_id.clone(),
        Some(RootSessionStatus::Busy),
    );

    if let Err(err) =
        add_work_started_comment(assigned_repository, &issue, workspace_key, &token).await
    {
        tracing::warn!(
            workspace_key,
            issue_url = %issue.url,
            error = %err,
            "manual issue prompt started but adding work-started comment failed"
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

    Ok(Some(issue))
}

fn persist_automation_issue_claim(
    workspace: &Workspace,
    assigned_repository: &str,
    issue: &SelectedIssue,
) {
    workspace.update(|next| {
        let changed_issue = next.persistent.automation_issue.as_deref() != Some(issue.url.as_str());
        let changed_repo =
            next.persistent.assigned_repository.as_deref() != Some(assigned_repository);
        if changed_issue || changed_repo {
            next.persistent.assigned_repository = Some(assigned_repository.to_string());
            next.persistent.automation_issue = Some(issue.url.clone());
            true
        } else {
            false
        }
    });
}

fn clear_automation_issue_claim(workspace: &Workspace, issue_url: &str) {
    workspace.update(|next| {
        let mut changed = false;
        if next.persistent.automation_issue.as_deref() == Some(issue_url) {
            next.persistent.automation_issue = None;
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
        let next_agent_state = session_status.map(|status| match status {
            RootSessionStatus::Busy => AutomationAgentState::Working,
            RootSessionStatus::Question => AutomationAgentState::Question,
            RootSessionStatus::Idle => AutomationAgentState::Idle,
        });
        if snapshot.automation_agent_state != next_agent_state {
            snapshot.automation_agent_state = next_agent_state;
            changed = true;
        }
        if snapshot.automation_session_status != session_status {
            snapshot.automation_session_status = session_status;
            changed = true;
        }
        changed
    });
}

fn active_issue_urls(service: &CombinedService, current_workspace_key: &str) -> HashSet<String> {
    let workspace_keys = service.manager.subscribe().borrow().clone();
    let mut urls = HashSet::new();
    for key in workspace_keys {
        if key == current_workspace_key {
            continue;
        }
        let Ok(workspace) = service.manager.get_workspace(&key) else {
            continue;
        };
        let snapshot = workspace.subscribe().borrow().clone();
        if let Some(url) = snapshot.persistent.automation_issue {
            urls.insert(url);
        }
        urls.extend(snapshot.persistent.custom_links.issue);
        urls.extend(snapshot.persistent.agent_provided.issue);
    }
    urls
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
    snapshot.automation_agent_state.or_else(|| {
        snapshot.root_session_status.map(|status| match status {
            RootSessionStatus::Busy => AutomationAgentState::Working,
            RootSessionStatus::Question => AutomationAgentState::Question,
            RootSessionStatus::Idle => AutomationAgentState::Idle,
        })
    })
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

async fn prompt_root_session(
    service: &CombinedService,
    snapshot: &WorkspaceSnapshot,
    assigned_repository: &str,
    issue: &SelectedIssue,
) -> Result<(), String> {
    let prompt = build_issue_prompt(assigned_repository, issue);
    service.prompt_root_session(snapshot, &prompt).await
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

fn build_issue_prompt(assigned_repository: &str, issue: &SelectedIssue) -> String {
    format!(
        "You are operating in an autonomous multicode workspace for repository {assigned_repository}.\n\
Start work on GitHub issue {issue_url}.\n\
Issue title: {issue_title}\n\
Before you proceed, load and follow these workspace skills as appropriate: `independent-fix`, `machine-readable-clone`, `machine-readable-issue`, `machine-readable-pr`, `git-commit-coauthorship`, `micronaut-projects-guide`, and `autonomous-state`.\n\
The environment variable `{automation_state_env}` points to a multicode-owned state file. Maintain it throughout the run using the `autonomous-state` skill so multicode can track whether you are working, waiting for a question, or ready for review.\n\
Your job is to:\n\
1. Ensure the repository is available in this workspace.\n\
2. Understand and reproduce the issue, creating a minimal reproducer or failing test when possible.\n\
3. Implement the fix.\n\
4. Run focused verification and summarize the evidence.\n\
5. Emit the machine-readable repository / issue / PR tags while you work.\n\
6. Run repository commands, builds, Gradle tasks, and focused tests as needed without asking for permission.\n\
7. Do not commit, push, comment, or open/update a pull request until the user explicitly approves publishing. When the change is ready, stop and ask for permission.\n\
\n\
Prefer an upstream pull request if you have write access. Keep going until the workspace is ready for review or you need human feedback.",
        automation_state_env = AUTOMATION_STATE_ENV,
        issue_url = issue.url,
        issue_title = issue.title
    )
}

async fn find_next_issue(
    assigned_repository: &str,
    excluded_issue_urls: &HashSet<String>,
    token: &str,
) -> Result<Option<SelectedIssue>, String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    for label in ISSUE_PRIORITY_LABELS {
        let issues = list_issues_for_label(assigned_repository, label, token).await?;
        for issue in issues {
            if issue.has_label(IN_PROGRESS_LABEL)
                || excluded_issue_urls.contains(&issue.url)
                || !seen.insert(issue.url.clone())
            {
                continue;
            }
            candidates.push(issue);
        }
    }

    Ok(candidates.into_iter().min_by(issue_priority_cmp))
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
            "number,title,createdAt,labels,url,state",
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

    let issue = serde_json::from_slice::<SelectedIssue>(&output.stdout)
        .map_err(|err| format!("failed to parse gh issue view output: {err}"))?;
    Ok(issue.is_open_issue_candidate().then_some(issue))
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

async fn add_work_started_comment(
    assigned_repository: &str,
    issue: &SelectedIssue,
    workspace_key: &str,
    token: &str,
) -> Result<(), String> {
    let mut command = Command::new(gh_program());
    apply_gh_env(&mut command, token);
    let output = command
        .args([
            "issue",
            "comment",
            &issue.url,
            "--repo",
            assigned_repository,
            "--body",
            &format!("multicode has started work on this issue in workspace `{workspace_key}`."),
        ])
        .output()
        .await
        .map_err(|err| format!("failed to run gh issue comment for {}: {err}", issue.url))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "gh issue comment failed for {}: {}",
            issue.url,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
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
    labels: Vec<SelectedIssueLabel>,
}

impl SelectedIssue {
    fn has_label(&self, label: &str) -> bool {
        self.labels
            .iter()
            .any(|candidate| candidate.name.eq_ignore_ascii_case(label))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceSnapshot;

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

    #[test]
    fn find_next_issue_prioritizes_boost_labels_then_base_priority_then_newest() {
        let excluded = HashSet::from(["https://github.com/example/repo/issue/5".to_string()]);
        let mut issues = vec![
            SelectedIssue {
                number: 5,
                title: "already claimed".to_string(),
                url: "https://github.com/example/repo/issue/5".to_string(),
                created_at: "2026-04-09T10:00:00Z".to_string(),
                state: Some("OPEN".to_string()),
                is_pull_request: Some(false),
                labels: vec![SelectedIssueLabel {
                    name: "type: bug".to_string(),
                }],
            },
            SelectedIssue {
                number: 6,
                title: "busy".to_string(),
                url: "https://github.com/example/repo/issue/6".to_string(),
                created_at: "2026-04-09T11:00:00Z".to_string(),
                state: Some("OPEN".to_string()),
                is_pull_request: Some(false),
                labels: vec![
                    SelectedIssueLabel {
                        name: "type: bug".to_string(),
                    },
                    SelectedIssueLabel {
                        name: IN_PROGRESS_LABEL.to_string(),
                    },
                ],
            },
            SelectedIssue {
                number: 7,
                title: "plain bug".to_string(),
                url: "https://github.com/example/repo/issue/7".to_string(),
                created_at: "2026-04-09T09:00:00Z".to_string(),
                state: Some("OPEN".to_string()),
                is_pull_request: Some(false),
                labels: vec![SelectedIssueLabel {
                    name: "type: bug".to_string(),
                }],
            },
            SelectedIssue {
                number: 8,
                title: "high priority enhancement".to_string(),
                url: "https://github.com/example/repo/issue/8".to_string(),
                created_at: "2026-04-09T08:00:00Z".to_string(),
                state: Some("OPEN".to_string()),
                is_pull_request: Some(false),
                labels: vec![
                    SelectedIssueLabel {
                        name: "type: enhancement".to_string(),
                    },
                    SelectedIssueLabel {
                        name: "priority: high".to_string(),
                    },
                ],
            },
            SelectedIssue {
                number: 9,
                title: "regression bug".to_string(),
                url: "https://github.com/example/repo/issue/9".to_string(),
                created_at: "2026-04-09T07:00:00Z".to_string(),
                state: Some("OPEN".to_string()),
                is_pull_request: Some(false),
                labels: vec![
                    SelectedIssueLabel {
                        name: "type: bug".to_string(),
                    },
                    SelectedIssueLabel {
                        name: "type: regression".to_string(),
                    },
                ],
            },
        ];

        issues.sort_by(issue_priority_cmp);

        let selected = issues
            .into_iter()
            .filter(|issue| !issue.has_label(IN_PROGRESS_LABEL))
            .filter(|issue| !excluded.contains(&issue.url))
            .next()
            .expect("one issue should remain");

        assert_eq!(selected.number, 9);
    }

    #[test]
    fn issue_priority_cmp_prefers_newer_issue_with_same_priority_bucket() {
        let older = SelectedIssue {
            number: 10,
            title: "older".to_string(),
            url: "https://github.com/example/repo/issues/10".to_string(),
            created_at: "2026-04-09T07:00:00Z".to_string(),
            state: Some("OPEN".to_string()),
            is_pull_request: Some(false),
            labels: vec![SelectedIssueLabel {
                name: "type: bug".to_string(),
            }],
        };
        let newer = SelectedIssue {
            number: 11,
            title: "newer".to_string(),
            url: "https://github.com/example/repo/issues/11".to_string(),
            created_at: "2026-04-09T08:00:00Z".to_string(),
            state: Some("OPEN".to_string()),
            is_pull_request: Some(false),
            labels: vec![SelectedIssueLabel {
                name: "type: bug".to_string(),
            }],
        };

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
    fn selected_issue_candidate_must_be_open_and_not_a_pull_request() {
        let open_issue = SelectedIssue {
            number: 1,
            title: "candidate".to_string(),
            url: "https://github.com/example/repo/issues/1".to_string(),
            created_at: "2026-04-09T10:00:00Z".to_string(),
            state: Some("OPEN".to_string()),
            is_pull_request: Some(false),
            labels: vec![],
        };
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
    fn persist_automation_issue_claim_updates_workspace_state() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        let issue = SelectedIssue {
            number: 810,
            title: "candidate".to_string(),
            url: "https://github.com/example/repo/issues/810".to_string(),
            created_at: "2026-04-09T10:00:00Z".to_string(),
            state: Some("OPEN".to_string()),
            is_pull_request: Some(false),
            labels: vec![],
        };

        persist_automation_issue_claim(&workspace, "example/repo", &issue);

        let snapshot = workspace.subscribe().borrow().clone();
        assert_eq!(
            snapshot.persistent.assigned_repository.as_deref(),
            Some("example/repo")
        );
        assert_eq!(
            snapshot.persistent.automation_issue.as_deref(),
            Some(issue.url.as_str())
        );
    }

    #[test]
    fn clear_automation_issue_claim_only_clears_matching_issue() {
        let workspace = Workspace::new(WorkspaceSnapshot::default());
        let issue = SelectedIssue {
            number: 810,
            title: "candidate".to_string(),
            url: "https://github.com/example/repo/issues/810".to_string(),
            created_at: "2026-04-09T10:00:00Z".to_string(),
            state: Some("OPEN".to_string()),
            is_pull_request: Some(false),
            labels: vec![],
        };

        persist_automation_issue_claim(&workspace, "example/repo", &issue);
        clear_automation_issue_claim(&workspace, "https://github.com/example/repo/issues/999");
        let unchanged = workspace.subscribe().borrow().clone();
        assert_eq!(
            unchanged.persistent.automation_issue.as_deref(),
            Some(issue.url.as_str())
        );

        clear_automation_issue_claim(&workspace, &issue.url);
        let cleared = workspace.subscribe().borrow().clone();
        assert!(cleared.persistent.automation_issue.is_none());
        assert_eq!(
            cleared.persistent.assigned_repository.as_deref(),
            Some("example/repo")
        );
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
    fn build_issue_prompt_requires_skills_and_publish_approval() {
        let issue = SelectedIssue {
            number: 980,
            title: "candidate".to_string(),
            url: "https://github.com/example/repo/issues/980".to_string(),
            created_at: "2026-04-09T10:00:00Z".to_string(),
            state: Some("OPEN".to_string()),
            is_pull_request: Some(false),
            labels: vec![],
        };

        let prompt = build_issue_prompt("example/repo", &issue);

        assert!(prompt.contains("`independent-fix`"));
        assert!(prompt.contains("`machine-readable-pr`"));
        assert!(prompt.contains("`autonomous-state`"));
        assert!(prompt.contains(AUTOMATION_STATE_ENV));
        assert!(prompt.contains(
            "Run repository commands, builds, Gradle tasks, and focused tests as needed without asking for permission."
        ));
        assert!(prompt.contains("Do not commit, push, comment, or open/update a pull request until the user explicitly approves publishing."));
    }
}
