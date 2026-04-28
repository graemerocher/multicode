use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use diesel::{
    ExpressionMethods, Insertable, OptionalExtension, QueryDsl, Queryable, RunQueryDsl, insert_into,
};
use octocrab::{
    Octocrab,
    models::{
        IssueState, Status, StatusState,
        pulls::{MergeableState, ReviewState},
        teams::RequestedTeam,
    },
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::{Notify, watch};
use url::Url;

use crate::{
    database::{Database, DatabaseError, SqlitePool},
    schema::github_link_statuses,
    services::GithubTokenConfig,
};

const ISSUE_OPEN_REFRESH_INTERVAL: Duration = Duration::from_mins(10);
const ISSUE_CLOSED_REFRESH_INTERVAL: Duration = Duration::from_mins(60);
const PR_BUILDING_REFRESH_INTERVAL: Duration = Duration::from_mins(1);
const PR_BUILD_SUCCESS_REVIEW_PENDING_INTERVAL: Duration = Duration::from_mins(5);
const PR_REVIEW_ACCEPTED_PENDING_MERGE_INTERVAL: Duration = Duration::from_mins(10);
const PR_CLOSED_RECHECK_INTERVAL: Duration = Duration::from_mins(60);
const FETCH_ERROR_RETRY_INTERVAL: Duration = Duration::from_mins(5);
const UPSERT_LOCK_RETRY_ATTEMPTS: usize = 5;
const UPSERT_LOCK_RETRY_BASE_DELAY_MILLIS: u64 = 100;
const COPILOT_REVIEWER_LOGIN: &str = "copilot-pull-request-reviewer";
const COPILOT_REVIEWER_BOT_LOGIN: &str = "copilot-pull-request-reviewer[bot]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubIssueState {
    Open,
    Closed,
}

impl GithubIssueState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubPrState {
    Open,
    Merged,
    Rejected,
}

impl GithubPrState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Rejected => "rejected",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "merged" => Some(Self::Merged),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubPrBuildState {
    Building,
    Failed,
    Succeeded,
}

impl GithubPrBuildState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Failed => "failed",
            Self::Succeeded => "succeeded",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "building" => Some(Self::Building),
            "failed" => Some(Self::Failed),
            "succeeded" => Some(Self::Succeeded),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubPrMergeState {
    Behind,
    Blocked,
    Clean,
    Dirty,
    Draft,
    HasHooks,
    Unknown,
    Unstable,
}

impl GithubPrMergeState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Behind => "behind",
            Self::Blocked => "blocked",
            Self::Clean => "clean",
            Self::Dirty => "dirty",
            Self::Draft => "draft",
            Self::HasHooks => "has_hooks",
            Self::Unknown => "unknown",
            Self::Unstable => "unstable",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "behind" => Some(Self::Behind),
            "blocked" => Some(Self::Blocked),
            "clean" => Some(Self::Clean),
            "dirty" => Some(Self::Dirty),
            "draft" => Some(Self::Draft),
            "has_hooks" => Some(Self::HasHooks),
            "unknown" => Some(Self::Unknown),
            "unstable" => Some(Self::Unstable),
            _ => None,
        }
    }

    fn from_github(value: Option<&MergeableState>) -> Option<Self> {
        match value {
            Some(MergeableState::Behind) => Some(Self::Behind),
            Some(MergeableState::Blocked) => Some(Self::Blocked),
            Some(MergeableState::Clean) => Some(Self::Clean),
            Some(MergeableState::Dirty) => Some(Self::Dirty),
            Some(MergeableState::Draft) => Some(Self::Draft),
            Some(MergeableState::HasHooks) => Some(Self::HasHooks),
            Some(MergeableState::Unknown) => Some(Self::Unknown),
            Some(MergeableState::Unstable) => Some(Self::Unstable),
            None => None,
            Some(_) => Some(Self::Unknown),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubPrSonarState {
    Building,
    Failed,
    Succeeded,
}

impl GithubPrSonarState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::Failed => "failed",
            Self::Succeeded => "succeeded",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "building" => Some(Self::Building),
            "failed" => Some(Self::Failed),
            "succeeded" => Some(Self::Succeeded),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubPrReviewState {
    None,
    Requested,
    Outstanding,
    Rejected,
    Accepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubPrCopilotReviewState {
    None,
    Requested,
    Done,
}

impl GithubPrCopilotReviewState {
    fn as_db(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Requested => "requested",
            Self::Done => "done",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "requested" => Some(Self::Requested),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

impl GithubPrReviewState {
    fn as_db(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Requested => "requested",
            Self::Outstanding => "outstanding",
            Self::Rejected => "rejected",
            Self::Accepted => "accepted",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "requested" => Some(Self::Requested),
            "outstanding" => Some(Self::Outstanding),
            "rejected" => Some(Self::Rejected),
            "accepted" => Some(Self::Accepted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GithubIssueStatus {
    pub state: GithubIssueState,
    pub fetched_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubPrStatus {
    pub state: GithubPrState,
    pub target_branch: Option<String>,
    pub merge_state: Option<GithubPrMergeState>,
    pub build: GithubPrBuildState,
    pub sonar: Option<GithubPrSonarState>,
    pub review: GithubPrReviewState,
    pub requested_reviewers: Option<String>,
    pub copilot_review: GithubPrCopilotReviewState,
    pub is_draft: bool,
    pub unresolved_review_threads: u32,
    pub fetched_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubCopilotReviewRequestOutcome {
    pub removed_copilot_assignees: Vec<String>,
    pub review_requested: bool,
    pub review_request_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubStatus {
    Issue(GithubIssueStatus),
    Pr(GithubPrStatus),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GithubLinkKind {
    Issue,
    PullRequest,
}

impl GithubLinkKind {
    fn as_db(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pr",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "issue" => Some(Self::Issue),
            "pr" => Some(Self::PullRequest),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubLinkRef {
    kind: GithubLinkKind,
    url: String,
    host: String,
    owner: String,
    repo: String,
    resource_number: i64,
}

#[derive(Debug, Clone)]
struct CachedLinkStatus {
    reference: GithubLinkRef,
    issue_state: Option<GithubIssueState>,
    pr_state: Option<GithubPrState>,
    build_state: Option<GithubPrBuildState>,
    sonar_state: Option<GithubPrSonarState>,
    review_state: Option<GithubPrReviewState>,
    requested_reviewers: Option<String>,
    copilot_review_state: Option<GithubPrCopilotReviewState>,
    target_branch: Option<String>,
    merge_state: Option<GithubPrMergeState>,
    pr_is_draft: Option<bool>,
    unresolved_review_thread_count: Option<i64>,
    fetched_at_epoch_seconds: Option<i64>,
    refresh_after_epoch_seconds: Option<i64>,
    last_error: Option<String>,
}

impl CachedLinkStatus {
    fn new_pending(reference: GithubLinkRef, now_epoch_seconds: i64) -> Self {
        Self {
            reference,
            issue_state: None,
            pr_state: None,
            build_state: None,
            sonar_state: None,
            review_state: None,
            requested_reviewers: None,
            copilot_review_state: None,
            target_branch: None,
            merge_state: None,
            pr_is_draft: None,
            unresolved_review_thread_count: None,
            fetched_at_epoch_seconds: None,
            refresh_after_epoch_seconds: Some(now_epoch_seconds),
            last_error: None,
        }
    }

    fn new_pr_error_placeholder(reference: GithubLinkRef, now_epoch_seconds: i64) -> Self {
        Self {
            reference,
            issue_state: None,
            pr_state: Some(GithubPrState::Open),
            build_state: Some(GithubPrBuildState::Building),
            sonar_state: None,
            review_state: Some(GithubPrReviewState::None),
            requested_reviewers: None,
            copilot_review_state: Some(GithubPrCopilotReviewState::None),
            target_branch: None,
            merge_state: Some(GithubPrMergeState::Unknown),
            pr_is_draft: Some(false),
            unresolved_review_thread_count: Some(0),
            fetched_at_epoch_seconds: Some(now_epoch_seconds),
            refresh_after_epoch_seconds: Some(now_epoch_seconds),
            last_error: None,
        }
    }

    fn issue_status(&self) -> Option<GithubIssueStatus> {
        Some(GithubIssueStatus {
            state: self.issue_state?,
            fetched_at: system_time_from_epoch_seconds(self.fetched_at_epoch_seconds?),
        })
    }

    fn pr_status(&self) -> Option<GithubPrStatus> {
        Some(GithubPrStatus {
            state: self.pr_state?,
            target_branch: self.target_branch.clone(),
            merge_state: self.merge_state,
            build: self.build_state?,
            sonar: self.sonar_state,
            review: self.review_state?,
            requested_reviewers: self.requested_reviewers.clone(),
            copilot_review: self.copilot_review_state?,
            is_draft: self.pr_is_draft?,
            unresolved_review_threads: self.unresolved_review_thread_count.unwrap_or(0).max(0)
                as u32,
            fetched_at: system_time_from_epoch_seconds(self.fetched_at_epoch_seconds?),
        })
    }

    fn github_status(&self) -> Option<GithubStatus> {
        match self.reference.kind {
            GithubLinkKind::Issue => self.issue_status().map(GithubStatus::Issue),
            GithubLinkKind::PullRequest => self.pr_status().map(GithubStatus::Pr),
        }
    }

    fn to_row(&self) -> Option<GithubLinkStatusRow> {
        let fetched_at_epoch_seconds = self.fetched_at_epoch_seconds?;

        match self.reference.kind {
            GithubLinkKind::Issue => Some(GithubLinkStatusRow {
                url: self.reference.url.clone(),
                kind: self.reference.kind.as_db().to_string(),
                host: self.reference.host.clone(),
                owner: self.reference.owner.clone(),
                repo: self.reference.repo.clone(),
                resource_number: self.reference.resource_number,
                issue_state: self.issue_state.map(|state| state.as_db().to_string()),
                pr_state: None,
                build_state: None,
                sonar_state: None,
                review_state: None,
                requested_reviewers: None,
                copilot_review_state: None,
                target_branch: None,
                merge_state: None,
                pr_is_draft: None,
                unresolved_review_thread_count: None,
                fetched_at_epoch_seconds,
                refresh_after_epoch_seconds: self.refresh_after_epoch_seconds,
                last_error: self.last_error.clone(),
            }),
            GithubLinkKind::PullRequest => Some(GithubLinkStatusRow {
                url: self.reference.url.clone(),
                kind: self.reference.kind.as_db().to_string(),
                host: self.reference.host.clone(),
                owner: self.reference.owner.clone(),
                repo: self.reference.repo.clone(),
                resource_number: self.reference.resource_number,
                issue_state: None,
                pr_state: self.pr_state.map(|state| state.as_db().to_string()),
                build_state: self.build_state.map(|state| state.as_db().to_string()),
                sonar_state: self.sonar_state.map(|state| state.as_db().to_string()),
                review_state: self.review_state.map(|state| state.as_db().to_string()),
                requested_reviewers: self.requested_reviewers.clone(),
                copilot_review_state: self
                    .copilot_review_state
                    .map(|state| state.as_db().to_string()),
                target_branch: self.target_branch.clone(),
                merge_state: self.merge_state.map(|state| state.as_db().to_string()),
                pr_is_draft: self.pr_is_draft,
                unresolved_review_thread_count: self.unresolved_review_thread_count,
                fetched_at_epoch_seconds,
                refresh_after_epoch_seconds: self.refresh_after_epoch_seconds,
                last_error: self.last_error.clone(),
            }),
        }
    }

    fn from_row(row: GithubLinkStatusRow) -> Option<Self> {
        let kind = GithubLinkKind::from_db(&row.kind)?;
        let reference = GithubLinkRef {
            kind,
            url: row.url,
            host: row.host,
            owner: row.owner,
            repo: row.repo,
            resource_number: row.resource_number,
        };

        let issue_state = row
            .issue_state
            .and_then(|state| GithubIssueState::from_db(&state));
        let pr_state = row
            .pr_state
            .and_then(|state| GithubPrState::from_db(&state));
        let build_state = row
            .build_state
            .and_then(|state| GithubPrBuildState::from_db(&state));
        let sonar_state = row
            .sonar_state
            .and_then(|state| GithubPrSonarState::from_db(&state));
        let review_state = row
            .review_state
            .and_then(|state| GithubPrReviewState::from_db(&state));
        let pr_is_draft = row.pr_is_draft;
        let unresolved_review_thread_count = row.unresolved_review_thread_count;
        let requested_reviewers = row.requested_reviewers;
        let mut copilot_review_state = row
            .copilot_review_state
            .and_then(|state| GithubPrCopilotReviewState::from_db(&state));
        let target_branch = row.target_branch;
        let mut merge_state = row
            .merge_state
            .and_then(|state| GithubPrMergeState::from_db(&state));
        if kind == GithubLinkKind::PullRequest {
            if copilot_review_state.is_none() {
                copilot_review_state = Some(GithubPrCopilotReviewState::None);
            }
            if pr_state == Some(GithubPrState::Open) && merge_state.is_none() {
                merge_state = Some(GithubPrMergeState::Unknown);
            }
        }

        match kind {
            GithubLinkKind::Issue if issue_state.is_none() => None,
            GithubLinkKind::PullRequest
                if pr_state.is_none()
                    || build_state.is_none()
                    || review_state.is_none()
                    || copilot_review_state.is_none()
                    || pr_is_draft.is_none() =>
            {
                None
            }
            _ => Some(Self {
                reference,
                issue_state,
                pr_state,
                build_state,
                sonar_state,
                review_state,
                requested_reviewers,
                copilot_review_state,
                target_branch,
                merge_state,
                pr_is_draft,
                unresolved_review_thread_count,
                fetched_at_epoch_seconds: Some(row.fetched_at_epoch_seconds),
                refresh_after_epoch_seconds: row.refresh_after_epoch_seconds,
                last_error: row.last_error,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GithubStatusService {
    database: Database,
    watch_entries: Arc<Mutex<HashMap<String, Arc<StatusWatchEntry>>>>,
    token_source: Option<GithubTokenConfig>,
    token: Arc<Mutex<Option<String>>>,
    client: Arc<Mutex<Option<Octocrab>>>,
    authenticated_login: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
struct StatusWatchEntry {
    reference: GithubLinkRef,
    sender: watch::Sender<Option<GithubStatus>>,
    receivers_available: Arc<Notify>,
    refresh_requested: Arc<Notify>,
}

impl GithubStatusService {
    pub async fn new(
        database: Database,
        token_source: Option<GithubTokenConfig>,
    ) -> Result<Self, GithubStatusServiceError> {
        Ok(Self {
            database,
            watch_entries: Arc::new(Mutex::new(HashMap::new())),
            token_source,
            token: Arc::new(Mutex::new(None)),
            client: Arc::new(Mutex::new(None)),
            authenticated_login: Arc::new(Mutex::new(None)),
        })
    }

    pub fn watch_status(&self, url: &str) -> Option<watch::Receiver<Option<GithubStatus>>> {
        let reference = parse_github_status_reference(url)?;
        let key = reference.url.clone();

        let mut watch_entries = self
            .watch_entries
            .lock()
            .expect("github status watch entries lock poisoned");

        let entry = if let Some(existing) = watch_entries.get(&key) {
            existing.clone()
        } else {
            let (sender, _) = watch::channel(None);
            let entry = Arc::new(StatusWatchEntry {
                reference,
                sender,
                receivers_available: Arc::new(Notify::new()),
                refresh_requested: Arc::new(Notify::new()),
            });
            let service = self.clone();
            let task_entry = entry.clone();
            tokio::spawn(async move {
                service.run_watch_task(task_entry).await;
            });
            watch_entries.insert(key, entry.clone());
            entry
        };

        let receiver = entry.sender.subscribe();
        entry.receivers_available.notify_waiters();
        Some(receiver)
    }

    pub async fn resolved_github_token(&self) -> Result<String, GithubStatusServiceError> {
        self.github_token().await
    }

    pub async fn authenticated_login(&self) -> Result<Option<String>, GithubStatusServiceError> {
        if let Some(login) = self
            .authenticated_login
            .lock()
            .expect("github authenticated login lock poisoned")
            .clone()
        {
            return Ok(Some(login));
        }

        self.refresh_authenticated_login().await
    }

    pub async fn refresh_authenticated_login(
        &self,
    ) -> Result<Option<String>, GithubStatusServiceError> {
        let client = self.github_client().await?;
        let user = client.current().user().await?;
        let login = user.login.trim().to_string();
        if login.is_empty() {
            return Err(GithubStatusServiceError::Auth(
                "GitHub authenticated user login was empty".to_string(),
            ));
        }
        *self
            .authenticated_login
            .lock()
            .expect("github authenticated login lock poisoned") = Some(login.clone());
        Ok(Some(login))
    }

    pub fn request_refresh(&self, url: &str) -> bool {
        let Some(reference) = parse_github_status_reference(url) else {
            return false;
        };
        let key = reference.url;

        let watch_entries = self
            .watch_entries
            .lock()
            .expect("github status watch entries lock poisoned");
        let Some(entry) = watch_entries.get(&key) else {
            return false;
        };

        entry.refresh_requested.notify_one();
        true
    }

    pub async fn request_copilot_review(
        &self,
        pr_url: &str,
    ) -> Result<GithubCopilotReviewRequestOutcome, GithubStatusServiceError> {
        let reference = parse_github_status_reference(pr_url).ok_or_else(|| {
            GithubStatusServiceError::InvalidReference(format!(
                "'{pr_url}' is not a supported GitHub PR URL"
            ))
        })?;
        if reference.kind != GithubLinkKind::PullRequest {
            return Err(GithubStatusServiceError::InvalidReference(format!(
                "'{pr_url}' is not a GitHub PR URL"
            )));
        }

        let client = self.github_client().await?;
        let issue = client
            .issues(&reference.owner, &reference.repo)
            .get(reference.resource_number as u64)
            .await?;
        let copilot_assignees = issue
            .assignees
            .iter()
            .filter(|assignee| is_copilot_assignee_login(assignee.login.trim()))
            .map(|assignee| (assignee.login.trim().to_string(), assignee.node_id.clone()))
            .collect::<Vec<_>>();
        let removed_copilot_assignees = copilot_assignees
            .iter()
            .map(|(login, _)| login.clone())
            .collect::<Vec<_>>();

        if !copilot_assignees.is_empty() {
            let pull_request = client
                .pulls(&reference.owner, &reference.repo)
                .get(reference.resource_number as u64)
                .await?;
            let assignable_id = pull_request.node_id.as_deref().unwrap_or(&issue.node_id);
            let assignee_ids = copilot_assignees
                .iter()
                .map(|(_, node_id)| node_id.as_str())
                .collect::<Vec<_>>();
            let body = serde_json::json!({
                "query": REMOVE_ASSIGNEES_FROM_ASSIGNABLE_GRAPHQL_MUTATION,
                "variables": RemoveAssigneesFromAssignableGraphqlVariables {
                    assignable: assignable_id,
                    assignees: assignee_ids,
                },
            });
            let response: RemoveAssigneesFromAssignableGraphqlResponse =
                client.post("/graphql", Some(&body)).await?;
            if let Some(errors) = response.errors.filter(|errors| !errors.is_empty()) {
                return Err(GithubStatusServiceError::Graphql(graphql_errors_label(
                    &errors,
                )));
            }
        }

        let review_request_error = match self.request_copilot_review_with_gh(&reference).await {
            Ok(()) => match self
                .fetch_pr_requested_reviewer_logins(&client, &reference)
                .await
            {
                Ok(logins) if logins.iter().any(|login| is_copilot_review_login(login)) => None,
                Ok(_) => Some(
                    "GitHub accepted @copilot, but Copilot was not visible in PR review requests"
                        .to_string(),
                ),
                Err(err) => Some(format!(
                    "GitHub accepted @copilot, but review request verification failed: {err}"
                )),
            },
            Err(err) => Some(err.to_string()),
        };

        let _ = self.request_refresh(&reference.url);

        Ok(GithubCopilotReviewRequestOutcome {
            removed_copilot_assignees,
            review_requested: review_request_error.is_none(),
            review_request_error,
        })
    }

    async fn run_watch_task(&self, entry: Arc<StatusWatchEntry>) {
        let mut current_status = match load_cache_row(
            self.database.pool().clone(),
            &entry.reference.url,
        )
        .await
        {
            Ok(row) => row.and_then(CachedLinkStatus::from_row),
            Err(error) => {
                tracing::error!(url = %entry.reference.url, error = %error, "failed to load github status cache row");
                None
            }
        };

        if let Some(status) = current_status
            .as_ref()
            .and_then(CachedLinkStatus::github_status)
        {
            entry.sender.send_replace(Some(status));
        }

        loop {
            let wait_duration =
                next_refresh_wait_duration(current_status.as_ref(), now_epoch_seconds());
            tracing::info!(
                url = %entry.reference.url,
                issue_state = ?current_status.clone().map(|status| status.issue_state).flatten(),
                pr_state = ?current_status.clone().map(|status| status.pr_state).flatten(),
                build_state = ?current_status.clone().map(|status| status.build_state).flatten(),
                review_state = ?current_status.clone().map(|status| status.review_state).flatten(),
                pr_is_draft = ?current_status.clone().map(|status| status.pr_is_draft).flatten(),
                unresolved_review_threads = ?current_status
                    .clone()
                    .map(|status| status.unresolved_review_thread_count)
                    .flatten(),
                last_error = ?current_status.clone().map(|status| status.last_error).flatten(),
                wait_seconds = wait_duration.as_secs_f64(),
                "scheduled GitHub status refresh"
            );

            if !wait_duration.is_zero() {
                let _ =
                    tokio::time::timeout(wait_duration, entry.refresh_requested.notified()).await;
            }

            while entry.sender.receiver_count() == 0 {
                entry.receivers_available.notified().await;
            }

            match self.fetch_latest_status(&entry.reference).await {
                Ok(updated_status) => {
                    if let Some(status) = updated_status.github_status() {
                        entry.sender.send_replace(Some(status));
                    }
                    if let Some(row) = updated_status.to_row()
                        && let Err(error) =
                            upsert_cache_row(self.database.pool().clone(), row).await
                    {
                        tracing::error!(url = %entry.reference.url, error = %error, "failed to persist github status cache entry");
                    }
                    current_status = Some(updated_status);
                }
                Err(error) => {
                    let now = now_epoch_seconds();
                    let mut next_status =
                        current_status
                            .take()
                            .unwrap_or_else(|| match entry.reference.kind {
                                GithubLinkKind::PullRequest => {
                                    CachedLinkStatus::new_pr_error_placeholder(
                                        entry.reference.clone(),
                                        now,
                                    )
                                }
                                GithubLinkKind::Issue => {
                                    CachedLinkStatus::new_pending(entry.reference.clone(), now)
                                }
                            });
                    next_status.last_error = Some(error.to_string());
                    next_status.refresh_after_epoch_seconds =
                        Some(now + FETCH_ERROR_RETRY_INTERVAL.as_secs() as i64);
                    if let Some(status) = next_status.github_status() {
                        entry.sender.send_replace(Some(status));
                    }
                    if let Some(row) = next_status.to_row()
                        && let Err(persist_error) =
                            upsert_cache_row(self.database.pool().clone(), row).await
                    {
                        tracing::error!(url = %entry.reference.url, error = %persist_error, "failed to persist github status cache entry after fetch error");
                    }
                    current_status = Some(next_status);
                }
            }
        }
    }

    async fn fetch_latest_status(
        &self,
        reference: &GithubLinkRef,
    ) -> Result<CachedLinkStatus, GithubStatusServiceError> {
        match reference.kind {
            GithubLinkKind::Issue => self.fetch_issue_status(reference).await,
            GithubLinkKind::PullRequest => self.fetch_pr_status(reference).await,
        }
    }

    async fn fetch_issue_status(
        &self,
        reference: &GithubLinkRef,
    ) -> Result<CachedLinkStatus, GithubStatusServiceError> {
        let client = self.github_client().await?;
        let issue = client
            .issues(&reference.owner, &reference.repo)
            .get(reference.resource_number as u64)
            .await?;
        let state = if issue.state == IssueState::Open {
            GithubIssueState::Open
        } else {
            GithubIssueState::Closed
        };

        let fetched_at_epoch_seconds = now_epoch_seconds();
        let refresh_after_epoch_seconds = Some(
            fetched_at_epoch_seconds
                + match state {
                    GithubIssueState::Open => ISSUE_OPEN_REFRESH_INTERVAL.as_secs() as i64,
                    GithubIssueState::Closed => ISSUE_CLOSED_REFRESH_INTERVAL.as_secs() as i64,
                },
        );

        Ok(CachedLinkStatus {
            reference: reference.clone(),
            issue_state: Some(state),
            pr_state: None,
            build_state: None,
            sonar_state: None,
            review_state: None,
            requested_reviewers: None,
            copilot_review_state: None,
            target_branch: None,
            merge_state: None,
            pr_is_draft: None,
            unresolved_review_thread_count: None,
            fetched_at_epoch_seconds: Some(fetched_at_epoch_seconds),
            refresh_after_epoch_seconds,
            last_error: None,
        })
    }

    async fn fetch_pr_status(
        &self,
        reference: &GithubLinkRef,
    ) -> Result<CachedLinkStatus, GithubStatusServiceError> {
        let client = self.github_client().await?;
        let pull = client
            .pulls(&reference.owner, &reference.repo)
            .get(reference.resource_number as u64)
            .await?;

        let pr_state = if pull.merged_at.is_some() {
            GithubPrState::Merged
        } else if pull.state == Some(IssueState::Closed) {
            GithubPrState::Rejected
        } else {
            GithubPrState::Open
        };

        let check_runs = self
            .fetch_check_runs(&client, reference, &pull.head.sha)
            .await
            .unwrap_or_default();
        let commit_statuses = self
            .fetch_commit_statuses(&client, reference, &pull.head.sha)
            .await
            .unwrap_or_default();
        let rollup_nodes = self
            .fetch_pr_status_check_rollup(&client, reference)
            .await
            .unwrap_or_default();
        let build_state = derive_pr_build_state_from_rollup(&rollup_nodes)
            .unwrap_or_else(|| derive_pr_build_state(&check_runs, &commit_statuses));
        let check_sonar_state = derive_pr_sonar_state(&check_runs, &commit_statuses);
        let rollup_sonar_state = derive_pr_sonar_state_from_rollup(&rollup_nodes);
        let comment_sonar_state = self
            .fetch_pr_comments(&client, reference)
            .await
            .ok()
            .and_then(|comments| derive_pr_sonar_state_from_comments(&comments));
        let sonar_state = merge_pr_sonar_states(
            merge_pr_sonar_states(check_sonar_state, rollup_sonar_state),
            comment_sonar_state,
        );

        let reviews = self
            .fetch_reviews(&client, reference)
            .await
            .unwrap_or_default();
        let all_requested_reviewer_logins = pull
            .requested_reviewers
            .as_ref()
            .map(|reviewers| {
                reviewers
                    .iter()
                    .map(|reviewer| reviewer.login.trim().to_string())
                    .filter(|login| !login.is_empty())
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let mut all_requested_reviewer_logins = all_requested_reviewer_logins;
        all_requested_reviewer_logins.extend(
            self.fetch_pr_requested_reviewer_logins(&client, reference)
                .await
                .unwrap_or_default(),
        );
        let requested_reviewer_logins = all_requested_reviewer_logins
            .iter()
            .filter(|login| !is_automation_review_login(login.as_str()))
            .cloned()
            .collect::<HashSet<_>>();
        let requested_teams = pull.requested_teams.as_deref().unwrap_or_default();
        let requested_human_team_count = requested_teams
            .iter()
            .filter(|team| !is_automation_review_login(team.slug.trim()))
            .count();
        let requested_reviewer_count = requested_reviewer_logins.len() + requested_human_team_count;
        let requested_reviewers =
            requested_reviewers_label(&requested_reviewer_logins, requested_teams);
        let pr_is_draft = pull.draft.unwrap_or(false);
        let target_branch =
            Some(pull.base.ref_field.trim().to_string()).filter(|branch| !branch.is_empty());
        let merge_state = if pr_state == GithubPrState::Open {
            Some(
                GithubPrMergeState::from_github(pull.mergeable_state.as_ref())
                    .unwrap_or(GithubPrMergeState::Unknown),
            )
        } else {
            None
        };
        let unresolved_review_thread_count = if pr_state == GithubPrState::Open {
            match self
                .fetch_unresolved_review_thread_count(&client, reference)
                .await
            {
                Ok(count) => i64::from(count),
                Err(error) => {
                    tracing::warn!(
                        owner = %reference.owner,
                        repo = %reference.repo,
                        number = reference.resource_number,
                        error = %error,
                        "failed to fetch unresolved PR review threads; treating as outstanding"
                    );
                    1
                }
            }
        } else {
            0
        };
        let review_state = derive_pr_review_state(
            &reviews,
            &requested_reviewer_logins,
            requested_reviewer_count,
            unresolved_review_thread_count,
        );
        let copilot_review_state =
            derive_copilot_review_state(&reviews, &all_requested_reviewer_logins);

        let fetched_at_epoch_seconds = now_epoch_seconds();
        let refresh_after_epoch_seconds = next_pr_refresh_after_epoch_seconds(
            fetched_at_epoch_seconds,
            pr_state,
            build_state,
            review_state,
        );

        Ok(CachedLinkStatus {
            reference: reference.clone(),
            issue_state: None,
            pr_state: Some(pr_state),
            build_state: Some(build_state),
            sonar_state,
            review_state: Some(review_state),
            requested_reviewers,
            copilot_review_state: Some(copilot_review_state),
            target_branch,
            merge_state,
            pr_is_draft: Some(pr_is_draft),
            unresolved_review_thread_count: Some(unresolved_review_thread_count),
            fetched_at_epoch_seconds: Some(fetched_at_epoch_seconds),
            refresh_after_epoch_seconds,
            last_error: None,
        })
    }

    async fn fetch_check_runs(
        &self,
        client: &Octocrab,
        reference: &GithubLinkRef,
        head_sha: &str,
    ) -> Result<Vec<octocrab::models::checks::CheckRun>, GithubStatusServiceError> {
        let route = check_runs_route(reference, head_sha);
        let check_runs: octocrab::models::checks::ListCheckRuns =
            client.get(route, None::<&()>).await?;
        Ok(check_runs.check_runs)
    }

    async fn fetch_commit_statuses(
        &self,
        client: &Octocrab,
        reference: &GithubLinkRef,
        head_sha: &str,
    ) -> Result<Vec<Status>, GithubStatusServiceError> {
        let page = client
            .repos(&reference.owner, &reference.repo)
            .list_statuses(head_sha.to_string())
            .send()
            .await?;
        Ok(page.items)
    }

    async fn fetch_reviews(
        &self,
        client: &Octocrab,
        reference: &GithubLinkRef,
    ) -> Result<Vec<octocrab::models::pulls::Review>, GithubStatusServiceError> {
        let page = client
            .pulls(&reference.owner, &reference.repo)
            .list_reviews(reference.resource_number as u64)
            .send()
            .await?;
        Ok(page.items)
    }

    async fn fetch_unresolved_review_thread_count(
        &self,
        client: &Octocrab,
        reference: &GithubLinkRef,
    ) -> Result<u32, GithubStatusServiceError> {
        let mut unresolved_threads = 0_u32;
        let mut cursor: Option<String> = None;

        loop {
            let body = serde_json::json!({
                "query": REVIEW_THREADS_GRAPHQL_QUERY,
                "variables": ReviewThreadsGraphqlVariables {
                    owner: &reference.owner,
                    repo: &reference.repo,
                    number: reference.resource_number,
                    cursor: cursor.as_deref(),
                },
            });
            let response: ReviewThreadsGraphqlResponse =
                client.post("/graphql", Some(&body)).await?;

            if let Some(errors) = response.errors.filter(|errors| !errors.is_empty()) {
                return Err(GithubStatusServiceError::Graphql(graphql_errors_label(
                    &errors,
                )));
            }

            let Some(review_threads) = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repository| repository.pull_request)
                .map(|pull_request| pull_request.review_threads)
            else {
                return Err(GithubStatusServiceError::Graphql(
                    "reviewThreads response did not include pull request data".to_string(),
                ));
            };

            unresolved_threads = unresolved_threads.saturating_add(
                review_threads
                    .nodes
                    .into_iter()
                    .filter(|thread| !thread.is_resolved)
                    .count() as u32,
            );

            if !review_threads.page_info.has_next_page {
                break;
            }

            cursor = review_threads.page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(unresolved_threads)
    }

    async fn fetch_pr_comments(
        &self,
        client: &Octocrab,
        reference: &GithubLinkRef,
    ) -> Result<Vec<GithubIssueComment>, GithubStatusServiceError> {
        let route = format!(
            "/repos/{owner}/{repo}/issues/{number}/comments?per_page=100",
            owner = reference.owner,
            repo = reference.repo,
            number = reference.resource_number
        );
        let comments: Vec<GithubIssueComment> = client.get(route, None::<&()>).await?;
        Ok(comments)
    }

    async fn github_client(&self) -> Result<Octocrab, GithubStatusServiceError> {
        if let Some(client) = self
            .client
            .lock()
            .expect("github client lock poisoned")
            .clone()
        {
            return Ok(client);
        }

        let token = self.github_token().await?;
        let mut builder = Octocrab::builder().personal_token(token);
        if let Ok(base_uri) = std::env::var("GITHUB_API_URL") {
            builder = builder.base_uri(base_uri)?;
        }
        let client = builder.build()?;
        *self.client.lock().expect("github client lock poisoned") = Some(client.clone());
        Ok(client)
    }

    async fn github_token(&self) -> Result<String, GithubStatusServiceError> {
        if let Some(token) = self
            .token
            .lock()
            .expect("github token lock poisoned")
            .clone()
        {
            return Ok(token);
        }

        let token = self.resolve_github_token().await?;

        *self.token.lock().expect("github token lock poisoned") = Some(token.clone());
        Ok(token)
    }

    async fn resolve_github_token(&self) -> Result<String, GithubStatusServiceError> {
        match self.token_source.as_ref() {
            Some(GithubTokenConfig {
                env: Some(env),
                command: None,
                keychain_service: None,
                keychain_account: None,
            }) => {
                let token = std::env::var(env).map_err(|err| {
                    GithubStatusServiceError::Auth(format!(
                        "failed to load GitHub token from environment variable `{env}`: {err}"
                    ))
                })?;
                validate_github_token(&token, &format!("environment variable `{env}`"))
            }
            Some(GithubTokenConfig {
                env: None,
                command: Some(command),
                keychain_service: None,
                keychain_account: None,
            }) => load_github_token_from_command(command).await,
            Some(GithubTokenConfig {
                env: None,
                command: None,
                keychain_service: Some(service),
                keychain_account,
            }) => load_github_token_from_keychain(service, keychain_account.as_deref()).await,
            Some(GithubTokenConfig {
                env: None,
                command: None,
                keychain_service: None,
                keychain_account: Some(_),
            }) => Err(GithubStatusServiceError::Auth(
                "GitHub token config cannot set `keychain-account` without `keychain-service`"
                    .to_string(),
            )),
            Some(GithubTokenConfig {
                env: Some(_),
                command: Some(_),
                keychain_service: _,
                keychain_account: _,
            })
            | Some(GithubTokenConfig {
                env: Some(_),
                command: None,
                keychain_service: Some(_),
                keychain_account: _,
            })
            | Some(GithubTokenConfig {
                env: None,
                command: Some(_),
                keychain_service: Some(_),
                keychain_account: _,
            }) => Err(GithubStatusServiceError::Auth(
                "GitHub token config must set exactly one of `env`, `command`, or `keychain-service`".to_string(),
            )),
            Some(GithubTokenConfig {
                env: None,
                command: None,
                keychain_service: None,
                keychain_account: None,
            }) => Err(GithubStatusServiceError::Auth(
                "GitHub token config must set exactly one of `env`, `command`, or `keychain-service`".to_string(),
            )),
            Some(_) => Err(GithubStatusServiceError::Auth(
                "GitHub token config must set exactly one of `env`, `command`, or `keychain-service`".to_string(),
            )),
            None => load_github_token_from_command("gh auth token").await,
        }
    }

    async fn fetch_pr_status_check_rollup(
        &self,
        client: &Octocrab,
        reference: &GithubLinkRef,
    ) -> Result<Vec<PrStatusCheckRollupNode>, GithubStatusServiceError> {
        let mut nodes = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let body = serde_json::json!({
                "query": PR_STATUS_CHECK_ROLLUP_GRAPHQL_QUERY,
                "variables": PrStatusCheckRollupGraphqlVariables {
                    owner: &reference.owner,
                    repo: &reference.repo,
                    number: reference.resource_number,
                    cursor: cursor.as_deref(),
                },
            });
            let response: PrStatusCheckRollupGraphqlResponse =
                client.post("/graphql", Some(&body)).await?;

            if response
                .errors
                .as_ref()
                .is_some_and(|errors| !errors.is_empty())
            {
                return Ok(Vec::new());
            }

            let Some(contexts) = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repository| repository.pull_request)
                .and_then(|pull_request| pull_request.status_check_rollup)
                .map(|status_check_rollup| status_check_rollup.contexts)
            else {
                break;
            };

            nodes.extend(contexts.nodes);
            if !contexts.page_info.has_next_page {
                break;
            }
            cursor = contexts.page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(nodes)
    }

    async fn fetch_pr_requested_reviewer_logins(
        &self,
        client: &Octocrab,
        reference: &GithubLinkRef,
    ) -> Result<HashSet<String>, GithubStatusServiceError> {
        let mut reviewers = HashSet::new();
        let mut cursor: Option<String> = None;

        loop {
            let body = serde_json::json!({
                "query": PR_REVIEW_REQUESTS_GRAPHQL_QUERY,
                "variables": PrReviewRequestsGraphqlVariables {
                    owner: &reference.owner,
                    repo: &reference.repo,
                    number: reference.resource_number,
                    cursor: cursor.as_deref(),
                },
            });
            let response: PrReviewRequestsGraphqlResponse =
                client.post("/graphql", Some(&body)).await?;

            if let Some(errors) = response.errors.filter(|errors| !errors.is_empty()) {
                return Err(GithubStatusServiceError::Graphql(graphql_errors_label(
                    &errors,
                )));
            }

            let Some(review_requests) = response
                .data
                .and_then(|data| data.repository)
                .and_then(|repository| repository.pull_request)
                .map(|pull_request| pull_request.review_requests)
            else {
                break;
            };

            reviewers.extend(
                review_requests
                    .nodes
                    .into_iter()
                    .filter_map(|node| node.requested_reviewer.login())
                    .map(|login| login.trim().to_string())
                    .filter(|login| !login.is_empty()),
            );
            if !review_requests.page_info.has_next_page {
                break;
            }
            cursor = review_requests.page_info.end_cursor;
            if cursor.is_none() {
                break;
            }
        }

        Ok(reviewers)
    }

    async fn request_copilot_review_with_gh(
        &self,
        reference: &GithubLinkRef,
    ) -> Result<(), GithubStatusServiceError> {
        let token = self.github_token().await?;
        let repo = format!("{}/{}", reference.owner, reference.repo);
        let number = reference.resource_number.to_string();
        let output = Command::new("gh")
            .args([
                "pr",
                "edit",
                &number,
                "-R",
                &repo,
                "--add-reviewer",
                "@copilot",
            ])
            .env("GH_TOKEN", &token)
            .env("GITHUB_TOKEN", &token)
            .output()
            .await?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("gh exited with status {}", output.status)
        };
        Err(GithubStatusServiceError::GitHubCli(format!(
            "failed to request @copilot review for {repo}#{number}: {detail}"
        )))
    }
}

fn validate_github_token(
    token: &str,
    source_description: &str,
) -> Result<String, GithubStatusServiceError> {
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(GithubStatusServiceError::Auth(format!(
            "GitHub token from {source_description} was empty"
        )));
    }
    Ok(token)
}

async fn load_github_token_from_command(command: &str) -> Result<String, GithubStatusServiceError> {
    let shell = if cfg!(unix) { "/bin/sh" } else { "sh" };
    let output = Command::new(shell).args(["-c", command]).output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!(
                "GitHub token command `{command}` failed with status {}",
                output.status
            )
        } else {
            format!("GitHub token command `{command}` failed: {stderr}")
        };
        return Err(GithubStatusServiceError::Auth(message));
    }

    let source_description = format!("command `{command}`");
    validate_github_token(
        &String::from_utf8_lossy(&output.stdout),
        &source_description,
    )
}

async fn load_github_token_from_keychain(
    service: &str,
    account: Option<&str>,
) -> Result<String, GithubStatusServiceError> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("security");
        command.arg("find-generic-password").arg("-s").arg(service);
        if let Some(account) = account {
            command.arg("-a").arg(account);
        }
        command.arg("-w");
        let output = command.output().await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let source_description = account
                .map(|account| format!("service `{service}` account `{account}`"))
                .unwrap_or_else(|| format!("service `{service}`"));
            let message = if stderr.is_empty() {
                format!(
                    "GitHub token Keychain lookup for {source_description} failed with status {}",
                    output.status
                )
            } else {
                format!("GitHub token Keychain lookup for {source_description} failed: {stderr}")
            };
            return Err(GithubStatusServiceError::Auth(message));
        }

        let source_description = account
            .map(|account| format!("macOS Keychain item service `{service}` account `{account}`"))
            .unwrap_or_else(|| format!("macOS Keychain item service `{service}`"));
        return validate_github_token(
            &String::from_utf8_lossy(&output.stdout),
            &source_description,
        );
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (service, account);
        Err(GithubStatusServiceError::Auth(
            "GitHub token `keychain-service` is only supported on macOS".to_string(),
        ))
    }
}

#[cfg(test)]
fn parse_github_issue_reference(url: &str) -> Option<GithubLinkRef> {
    let reference = parse_github_status_reference(url)?;
    (reference.kind == GithubLinkKind::Issue).then_some(reference)
}

#[cfg(test)]
fn parse_github_pr_reference(url: &str) -> Option<GithubLinkRef> {
    let reference = parse_github_status_reference(url)?;
    (reference.kind == GithubLinkKind::PullRequest).then_some(reference)
}

fn parse_github_status_reference(url: &str) -> Option<GithubLinkRef> {
    let parsed = Url::parse(url.trim()).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    if host != "github.com" && host != "www.github.com" {
        return None;
    }

    let segments = parsed.path_segments()?.collect::<Vec<_>>();
    if segments.len() < 4 {
        return None;
    }

    let owner = segments[0].trim();
    let repo = segments[1].trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    let path_kind = segments[2].trim().to_ascii_lowercase();
    let resource_number = segments[3].trim().parse::<i64>().ok()?;
    if resource_number <= 0 {
        return None;
    }

    let (kind, canonical_path_kind) = match path_kind.as_str() {
        "issue" | "issues" => (GithubLinkKind::Issue, "issue"),
        "pull" | "pulls" => (GithubLinkKind::PullRequest, "pull"),
        _ => return None,
    };

    Some(GithubLinkRef {
        kind,
        url: format!("https://github.com/{owner}/{repo}/{canonical_path_kind}/{resource_number}"),
        host: "github.com".to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        resource_number,
    })
}

const REVIEW_THREADS_GRAPHQL_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        nodes {
          isResolved
        }
        pageInfo {
          hasNextPage
          endCursor
        }
      }
    }
  }
}
"#;

const PR_STATUS_CHECK_ROLLUP_GRAPHQL_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      statusCheckRollup {
        contexts(first: 100, after: $cursor) {
          nodes {
            __typename
            ... on CheckRun {
              name
              status
              conclusion
              detailsUrl
              checkSuite {
                app {
                  slug
                }
                workflowRun {
                  workflow {
                    name
                  }
                }
              }
            }
            ... on StatusContext {
              context
              state
              targetUrl
            }
          }
          pageInfo {
            hasNextPage
            endCursor
          }
        }
      }
    }
  }
}
"#;

const PR_REVIEW_REQUESTS_GRAPHQL_QUERY: &str = r#"
query($owner: String!, $repo: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewRequests(first: 100, after: $cursor) {
        nodes {
          requestedReviewer {
            __typename
            ... on User {
              login
            }
            ... on Team {
              slug
            }
            ... on Bot {
              login
            }
          }
        }
        pageInfo {
          hasNextPage
          endCursor
        }
      }
    }
  }
}
"#;

const REMOVE_ASSIGNEES_FROM_ASSIGNABLE_GRAPHQL_MUTATION: &str = r#"
mutation($assignable: ID!, $assignees: [ID!]!) {
  removeAssigneesFromAssignable(input: {assignableId: $assignable, assigneeIds: $assignees}) {
    assignable {
      ... on PullRequest {
        id
      }
      ... on Issue {
        id
      }
    }
  }
}
"#;

#[derive(Debug, Serialize)]
struct ReviewThreadsGraphqlVariables<'a> {
    owner: &'a str,
    repo: &'a str,
    number: i64,
    cursor: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct PrStatusCheckRollupGraphqlVariables<'a> {
    owner: &'a str,
    repo: &'a str,
    number: i64,
    cursor: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct PrReviewRequestsGraphqlVariables<'a> {
    owner: &'a str,
    repo: &'a str,
    number: i64,
    cursor: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct RemoveAssigneesFromAssignableGraphqlVariables<'a> {
    assignable: &'a str,
    assignees: Vec<&'a str>,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsGraphqlResponse {
    data: Option<ReviewThreadsGraphqlData>,
    errors: Option<Vec<ReviewThreadsGraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupGraphqlResponse {
    data: Option<PrStatusCheckRollupGraphqlData>,
    errors: Option<Vec<ReviewThreadsGraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct PrReviewRequestsGraphqlResponse {
    data: Option<PrReviewRequestsGraphqlData>,
    errors: Option<Vec<ReviewThreadsGraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct RemoveAssigneesFromAssignableGraphqlResponse {
    #[allow(dead_code)]
    data: Option<serde_json::Value>,
    errors: Option<Vec<ReviewThreadsGraphqlError>>,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsGraphqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsGraphqlData {
    repository: Option<ReviewThreadsGraphqlRepository>,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupGraphqlData {
    repository: Option<PrStatusCheckRollupGraphqlRepository>,
}

#[derive(Debug, Deserialize)]
struct PrReviewRequestsGraphqlData {
    repository: Option<PrReviewRequestsGraphqlRepository>,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsGraphqlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<ReviewThreadsGraphqlPullRequest>,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupGraphqlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PrStatusCheckRollupGraphqlPullRequest>,
}

#[derive(Debug, Deserialize)]
struct PrReviewRequestsGraphqlRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PrReviewRequestsGraphqlPullRequest>,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsGraphqlPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: ReviewThreadsGraphqlConnection,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupGraphqlPullRequest {
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<PrStatusCheckRollupGraphqlRollup>,
}

#[derive(Debug, Deserialize)]
struct PrReviewRequestsGraphqlPullRequest {
    #[serde(rename = "reviewRequests")]
    review_requests: PrReviewRequestsGraphqlConnection,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsGraphqlConnection {
    #[serde(default)]
    nodes: Vec<ReviewThreadsGraphqlThread>,
    #[serde(rename = "pageInfo")]
    page_info: ReviewThreadsGraphqlPageInfo,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsGraphqlThread {
    #[serde(rename = "isResolved")]
    is_resolved: bool,
}

#[derive(Debug, Deserialize)]
struct ReviewThreadsGraphqlPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupGraphqlRollup {
    contexts: PrStatusCheckRollupGraphqlContextConnection,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupGraphqlContextConnection {
    #[serde(default)]
    nodes: Vec<PrStatusCheckRollupNode>,
    #[serde(rename = "pageInfo")]
    page_info: ReviewThreadsGraphqlPageInfo,
}

#[derive(Debug, Deserialize)]
struct PrReviewRequestsGraphqlConnection {
    #[serde(default)]
    nodes: Vec<PrReviewRequestsGraphqlNode>,
    #[serde(rename = "pageInfo")]
    page_info: ReviewThreadsGraphqlPageInfo,
}

#[derive(Debug, Deserialize)]
struct PrReviewRequestsGraphqlNode {
    #[serde(rename = "requestedReviewer")]
    requested_reviewer: PrReviewRequestsGraphqlReviewer,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum PrReviewRequestsGraphqlReviewer {
    User {
        login: String,
    },
    Team {},
    Bot {
        login: String,
    },
    #[serde(other)]
    Other,
}

impl PrReviewRequestsGraphqlReviewer {
    fn login(self) -> Option<String> {
        match self {
            Self::User { login } | Self::Bot { login } => Some(login),
            Self::Team {} => None,
            Self::Other => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum PrStatusCheckRollupNode {
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        #[serde(rename = "detailsUrl")]
        details_url: Option<String>,
        #[serde(rename = "checkSuite")]
        check_suite: Option<PrStatusCheckRollupCheckSuite>,
    },
    StatusContext {
        context: String,
        state: String,
        #[serde(rename = "targetUrl")]
        target_url: Option<String>,
    },
}

impl PrStatusCheckRollupNode {
    fn is_sonar_related(&self) -> bool {
        match self {
            Self::CheckRun {
                name,
                details_url,
                check_suite,
                ..
            } => {
                is_sonar_related_text(name)
                    || details_url.as_deref().is_some_and(is_sonar_related_text)
                    || check_suite.as_ref().is_some_and(|suite| {
                        suite
                            .app
                            .as_ref()
                            .and_then(|app| app.slug.as_deref())
                            .is_some_and(is_sonar_related_text)
                            || suite
                                .workflow_run
                                .as_ref()
                                .and_then(|run| run.workflow.as_ref())
                                .and_then(|workflow| workflow.name.as_deref())
                                .is_some_and(is_sonar_related_text)
                    })
            }
            Self::StatusContext {
                context,
                target_url,
                ..
            } => {
                is_sonar_related_text(context)
                    || target_url.as_deref().is_some_and(is_sonar_related_text)
            }
        }
    }

    fn is_in_progress(&self) -> bool {
        match self {
            Self::CheckRun { status, .. } => !status.eq_ignore_ascii_case("completed"),
            Self::StatusContext { state, .. } => state.eq_ignore_ascii_case("pending"),
        }
    }

    fn is_failed(&self) -> bool {
        match self {
            Self::CheckRun { conclusion, .. } => is_failed_check_conclusion(conclusion.as_deref()),
            Self::StatusContext { state, .. } => {
                state.eq_ignore_ascii_case("failure") || state.eq_ignore_ascii_case("error")
            }
        }
    }

    fn is_success(&self) -> bool {
        match self {
            Self::CheckRun {
                status, conclusion, ..
            } => {
                status.eq_ignore_ascii_case("completed")
                    && conclusion
                        .as_deref()
                        .is_some_and(|conclusion| conclusion.eq_ignore_ascii_case("success"))
            }
            Self::StatusContext { state, .. } => state.eq_ignore_ascii_case("success"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupCheckSuite {
    app: Option<PrStatusCheckRollupApp>,
    #[serde(rename = "workflowRun")]
    workflow_run: Option<PrStatusCheckRollupWorkflowRun>,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupApp {
    slug: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupWorkflowRun {
    workflow: Option<PrStatusCheckRollupWorkflow>,
}

#[derive(Debug, Deserialize)]
struct PrStatusCheckRollupWorkflow {
    name: Option<String>,
}

fn is_failed_check_conclusion(conclusion: Option<&str>) -> bool {
    conclusion.is_some_and(|conclusion| {
        matches!(
            conclusion.to_ascii_lowercase().as_str(),
            "failure" | "timed_out" | "cancelled" | "startup_failure" | "action_required"
        )
    })
}

fn are_all_checks_successful(check_runs: &[octocrab::models::checks::CheckRun]) -> bool {
    !check_runs.is_empty()
        && check_runs.iter().all(|check| {
            check.completed_at.is_some() && check.conclusion.as_deref() == Some("success")
        })
}

fn is_failed_status_state(state: StatusState) -> bool {
    matches!(state, StatusState::Failure | StatusState::Error)
}

fn are_all_statuses_successful(commit_statuses: &[Status]) -> bool {
    !commit_statuses.is_empty()
        && commit_statuses
            .iter()
            .all(|status| status.state == StatusState::Success)
}

fn has_in_progress_checks(check_runs: &[octocrab::models::checks::CheckRun]) -> bool {
    check_runs.iter().any(|check| check.completed_at.is_none())
}

fn has_in_progress_statuses(commit_statuses: &[Status]) -> bool {
    commit_statuses
        .iter()
        .any(|status| status.state == StatusState::Pending)
}

fn derive_pr_build_state(
    check_runs: &[octocrab::models::checks::CheckRun],
    commit_statuses: &[Status],
) -> GithubPrBuildState {
    let has_in_progress_checks = has_in_progress_checks(check_runs);
    let has_failed_checks = check_runs
        .iter()
        .any(|check| is_failed_check_conclusion(check.conclusion.as_deref()));
    let has_successful_checks = are_all_checks_successful(check_runs);

    let has_in_progress_status = has_in_progress_statuses(commit_statuses);
    let has_failed_status = commit_statuses
        .iter()
        .any(|status| is_failed_status_state(status.state));
    let has_success_status = are_all_statuses_successful(commit_statuses);

    if has_failed_checks || has_failed_status {
        GithubPrBuildState::Failed
    } else if has_in_progress_checks || has_in_progress_status {
        GithubPrBuildState::Building
    } else if has_successful_checks && has_success_status {
        GithubPrBuildState::Succeeded
    } else {
        GithubPrBuildState::Building
    }
}

fn derive_pr_build_state_from_rollup(
    nodes: &[PrStatusCheckRollupNode],
) -> Option<GithubPrBuildState> {
    if nodes.is_empty() {
        return None;
    }

    if nodes.iter().any(PrStatusCheckRollupNode::is_in_progress) {
        Some(GithubPrBuildState::Building)
    } else if nodes.iter().any(PrStatusCheckRollupNode::is_failed) {
        Some(GithubPrBuildState::Failed)
    } else if nodes.iter().all(PrStatusCheckRollupNode::is_success) {
        Some(GithubPrBuildState::Succeeded)
    } else {
        Some(GithubPrBuildState::Building)
    }
}

fn derive_pr_sonar_state(
    check_runs: &[octocrab::models::checks::CheckRun],
    commit_statuses: &[Status],
) -> Option<GithubPrSonarState> {
    let sonar_checks = check_runs
        .iter()
        .filter(|check| is_sonar_related_text(&check.name))
        .collect::<Vec<_>>();
    let sonar_statuses = commit_statuses
        .iter()
        .filter(|status| status.context.as_deref().is_some_and(is_sonar_related_text))
        .collect::<Vec<_>>();

    if sonar_checks.is_empty() && sonar_statuses.is_empty() {
        return None;
    }

    let has_in_progress = sonar_checks
        .iter()
        .any(|check| check.completed_at.is_none())
        || sonar_statuses
            .iter()
            .any(|status| status.state == StatusState::Pending);
    let has_failed = sonar_checks
        .iter()
        .any(|check| is_failed_check_conclusion(check.conclusion.as_deref()))
        || sonar_statuses
            .iter()
            .any(|status| is_failed_status_state(status.state));
    let has_success = (!sonar_checks.is_empty()
        && sonar_checks.iter().all(|check| {
            check.completed_at.is_some() && check.conclusion.as_deref() == Some("success")
        }))
        || (!sonar_statuses.is_empty()
            && sonar_statuses
                .iter()
                .all(|status| status.state == StatusState::Success));

    Some(if has_in_progress {
        GithubPrSonarState::Building
    } else if has_failed {
        GithubPrSonarState::Failed
    } else if has_success {
        GithubPrSonarState::Succeeded
    } else {
        GithubPrSonarState::Building
    })
}

fn derive_pr_sonar_state_from_comments(
    comments: &[GithubIssueComment],
) -> Option<GithubPrSonarState> {
    comments.iter().rev().find_map(sonar_state_from_comment)
}

fn derive_pr_sonar_state_from_rollup(
    nodes: &[PrStatusCheckRollupNode],
) -> Option<GithubPrSonarState> {
    let related_nodes = nodes
        .iter()
        .filter(|node| node.is_sonar_related())
        .collect::<Vec<_>>();

    if related_nodes.is_empty() {
        return None;
    }

    let has_in_progress = related_nodes.iter().any(|node| node.is_in_progress());
    let has_failed = related_nodes.iter().any(|node| node.is_failed());
    let has_success = related_nodes.iter().all(|node| node.is_success());

    Some(if has_in_progress {
        GithubPrSonarState::Building
    } else if has_failed {
        GithubPrSonarState::Failed
    } else if has_success {
        GithubPrSonarState::Succeeded
    } else {
        GithubPrSonarState::Building
    })
}

fn is_sonar_related_text(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    !value.is_empty()
        && (value.contains("sonar")
            || value.contains("sonatype")
            || value.contains("vuln")
            || value.contains("vulnerability")
            || value.contains("cve")
            || value.contains("dependency-check")
            || value.contains("dependency check")
            || value.contains("oss index")
            || value.contains("security scan")
            || value.contains("security-scan"))
}

fn merge_pr_sonar_states(
    left: Option<GithubPrSonarState>,
    right: Option<GithubPrSonarState>,
) -> Option<GithubPrSonarState> {
    use GithubPrSonarState::{Building, Failed, Succeeded};

    match (left, right) {
        (Some(Failed), _) | (_, Some(Failed)) => Some(Failed),
        (Some(Building), _) | (_, Some(Building)) => Some(Building),
        (Some(Succeeded), _) | (_, Some(Succeeded)) => Some(Succeeded),
        _ => None,
    }
}

fn sonar_state_from_comment(comment: &GithubIssueComment) -> Option<GithubPrSonarState> {
    let author_login = comment.user.login.trim().to_ascii_lowercase();
    if !author_login.contains("sonar") {
        return None;
    }

    let body = comment.body.trim().to_ascii_lowercase();
    if body.contains("quality gate failed") {
        Some(GithubPrSonarState::Failed)
    } else if body.contains("quality gate passed") {
        Some(GithubPrSonarState::Succeeded)
    } else if body.contains("quality gate pending") || body.contains("quality gate in progress") {
        Some(GithubPrSonarState::Building)
    } else {
        None
    }
}

fn derive_pr_review_state(
    reviews: &[octocrab::models::pulls::Review],
    _requested_reviewer_logins: &HashSet<String>,
    requested_reviewer_count: usize,
    unresolved_review_thread_count: i64,
) -> GithubPrReviewState {
    let mut latest_review_state_by_user = HashMap::<String, String>::new();
    for review in reviews {
        let Some(user) = review.user.as_ref() else {
            continue;
        };
        let login = user.login.trim();
        if login.is_empty() {
            continue;
        }
        if is_automation_review_login(login) {
            continue;
        }
        let Some(state) = review.state else {
            continue;
        };
        let state = match state {
            ReviewState::Approved => "APPROVED",
            ReviewState::ChangesRequested => "CHANGES_REQUESTED",
            ReviewState::Commented => "COMMENTED",
            ReviewState::Dismissed => "DISMISSED",
            ReviewState::Open => "OPEN",
            ReviewState::Pending => "PENDING",
            _ => continue,
        };
        latest_review_state_by_user.insert(login.to_string(), state.to_string());
    }

    if latest_review_state_by_user.is_empty() {
        return if requested_reviewer_count > 0 {
            GithubPrReviewState::Requested
        } else {
            GithubPrReviewState::None
        };
    }

    if latest_review_state_by_user
        .values()
        .any(|state| state == "CHANGES_REQUESTED")
    {
        return GithubPrReviewState::Rejected;
    }

    if unresolved_review_thread_count > 0 {
        return GithubPrReviewState::Outstanding;
    }

    if latest_review_state_by_user
        .values()
        .any(|state| matches!(state.as_str(), "OPEN" | "PENDING"))
    {
        return GithubPrReviewState::Outstanding;
    }

    let has_approval = latest_review_state_by_user
        .values()
        .any(|state| state == "APPROVED");

    if has_approval && requested_reviewer_count == 0 {
        GithubPrReviewState::Accepted
    } else if requested_reviewer_count > 0 {
        GithubPrReviewState::Requested
    } else {
        GithubPrReviewState::None
    }
}

fn derive_copilot_review_state(
    reviews: &[octocrab::models::pulls::Review],
    requested_reviewer_logins: &HashSet<String>,
) -> GithubPrCopilotReviewState {
    let has_completed_copilot_review = reviews.iter().any(|review| {
        let Some(user) = review.user.as_ref() else {
            return false;
        };
        if !is_copilot_review_login(user.login.trim()) {
            return false;
        }
        matches!(
            review.state,
            Some(ReviewState::Approved)
                | Some(ReviewState::ChangesRequested)
                | Some(ReviewState::Commented)
        )
    });

    if has_completed_copilot_review {
        GithubPrCopilotReviewState::Done
    } else if requested_reviewer_logins
        .iter()
        .any(|login| is_copilot_review_login(login.trim()))
    {
        GithubPrCopilotReviewState::Requested
    } else {
        GithubPrCopilotReviewState::None
    }
}

fn is_copilot_review_login(login: &str) -> bool {
    login.eq_ignore_ascii_case("@copilot")
        || login.eq_ignore_ascii_case("Copilot")
        || login.eq_ignore_ascii_case("github-copilot")
        || login.eq_ignore_ascii_case(COPILOT_REVIEWER_LOGIN)
        || login.eq_ignore_ascii_case(COPILOT_REVIEWER_BOT_LOGIN)
}

fn is_copilot_assignee_login(login: &str) -> bool {
    login.eq_ignore_ascii_case("Copilot")
        || login.eq_ignore_ascii_case("github-copilot")
        || login.eq_ignore_ascii_case(COPILOT_REVIEWER_LOGIN)
        || login.eq_ignore_ascii_case(COPILOT_REVIEWER_BOT_LOGIN)
}

fn graphql_errors_label(errors: &[ReviewThreadsGraphqlError]) -> String {
    errors
        .iter()
        .map(|error| error.message.trim())
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

fn requested_reviewers_label(
    requested_reviewer_logins: &HashSet<String>,
    requested_teams: &[RequestedTeam],
) -> Option<String> {
    let mut reviewers = requested_reviewer_logins
        .iter()
        .map(String::as_str)
        .filter(|login| !login.trim().is_empty())
        .filter(|login| !is_automation_review_login(login))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    reviewers.extend(
        requested_teams
            .iter()
            .map(|team| team.slug.trim())
            .filter(|slug| !slug.is_empty())
            .filter(|slug| !is_automation_review_login(slug))
            .map(|slug| format!("@{slug}")),
    );
    reviewers.sort();
    reviewers.dedup();
    (!reviewers.is_empty()).then(|| reviewers.join(", "))
}

fn is_automation_review_login(login: &str) -> bool {
    is_copilot_review_login(login) || login.eq_ignore_ascii_case("github-actions")
}

fn check_runs_route(reference: &GithubLinkRef, head_sha: &str) -> String {
    format!(
        "/repos/{owner}/{repo}/commits/{head_sha}/check-runs",
        owner = reference.owner,
        repo = reference.repo,
        head_sha = head_sha,
    )
}

fn next_pr_refresh_after_epoch_seconds(
    now_epoch_seconds: i64,
    pr_state: GithubPrState,
    build_state: GithubPrBuildState,
    review_state: GithubPrReviewState,
) -> Option<i64> {
    let interval = if pr_state == GithubPrState::Rejected || pr_state == GithubPrState::Merged {
        PR_CLOSED_RECHECK_INTERVAL
    } else if build_state == GithubPrBuildState::Building {
        PR_BUILDING_REFRESH_INTERVAL
    } else if build_state == GithubPrBuildState::Succeeded
        && review_state != GithubPrReviewState::Accepted
    {
        PR_BUILD_SUCCESS_REVIEW_PENDING_INTERVAL
    } else if review_state == GithubPrReviewState::Accepted {
        PR_REVIEW_ACCEPTED_PENDING_MERGE_INTERVAL
    } else if build_state == GithubPrBuildState::Failed {
        PR_BUILD_SUCCESS_REVIEW_PENDING_INTERVAL
    } else {
        PR_REVIEW_ACCEPTED_PENDING_MERGE_INTERVAL
    };

    Some(now_epoch_seconds + interval.as_secs() as i64)
}

fn next_refresh_wait_duration(
    status: Option<&CachedLinkStatus>,
    now_epoch_seconds: i64,
) -> Duration {
    let refresh_after = status.and_then(|status| status.refresh_after_epoch_seconds);
    match refresh_after {
        Some(refresh_after_epoch_seconds) if refresh_after_epoch_seconds > now_epoch_seconds => {
            Duration::from_secs((refresh_after_epoch_seconds - now_epoch_seconds) as u64)
        }
        _ => Duration::ZERO,
    }
}

#[derive(Debug)]
pub enum GithubStatusServiceError {
    Database(DatabaseError),
    Pool(diesel::r2d2::PoolError),
    Diesel(diesel::result::Error),
    Io(std::io::Error),
    Octocrab(octocrab::Error),
    Join(tokio::task::JoinError),
    Auth(String),
    InvalidReference(String),
    Graphql(String),
    GitHubCli(String),
}

impl std::fmt::Display for GithubStatusServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database(err) => write!(f, "database error: {err:?}"),
            Self::Pool(err) => write!(f, "database pool error: {err}"),
            Self::Diesel(err) => write!(f, "diesel query error: {err}"),
            Self::Io(err) => write!(f, "io error: {err}"),
            Self::Octocrab(err) => write!(f, "github api error: {err}"),
            Self::Join(err) => write!(f, "task join error: {err}"),
            Self::Auth(err) => write!(f, "github authentication error: {err}"),
            Self::InvalidReference(err) => write!(f, "{err}"),
            Self::Graphql(err) => write!(f, "github graphql error: {err}"),
            Self::GitHubCli(err) => write!(f, "github cli error: {err}"),
        }
    }
}

impl std::error::Error for GithubStatusServiceError {}

impl From<DatabaseError> for GithubStatusServiceError {
    fn from(value: DatabaseError) -> Self {
        Self::Database(value)
    }
}

impl From<diesel::r2d2::PoolError> for GithubStatusServiceError {
    fn from(value: diesel::r2d2::PoolError) -> Self {
        Self::Pool(value)
    }
}

impl From<diesel::result::Error> for GithubStatusServiceError {
    fn from(value: diesel::result::Error) -> Self {
        Self::Diesel(value)
    }
}

impl From<std::io::Error> for GithubStatusServiceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<octocrab::Error> for GithubStatusServiceError {
    fn from(value: octocrab::Error) -> Self {
        Self::Octocrab(value)
    }
}

impl From<tokio::task::JoinError> for GithubStatusServiceError {
    fn from(value: tokio::task::JoinError) -> Self {
        Self::Join(value)
    }
}

#[derive(Debug, Clone, Queryable, Insertable)]
#[diesel(table_name = github_link_statuses)]
struct GithubLinkStatusRow {
    url: String,
    kind: String,
    host: String,
    owner: String,
    repo: String,
    resource_number: i64,
    issue_state: Option<String>,
    pr_state: Option<String>,
    build_state: Option<String>,
    sonar_state: Option<String>,
    review_state: Option<String>,
    requested_reviewers: Option<String>,
    copilot_review_state: Option<String>,
    target_branch: Option<String>,
    merge_state: Option<String>,
    pr_is_draft: Option<bool>,
    unresolved_review_thread_count: Option<i64>,
    fetched_at_epoch_seconds: i64,
    refresh_after_epoch_seconds: Option<i64>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubIssueComment {
    user: GithubIssueCommentUser,
    body: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubIssueCommentUser {
    login: String,
}

async fn load_cache_row(
    pool: SqlitePool,
    lookup_url: &str,
) -> Result<Option<GithubLinkStatusRow>, GithubStatusServiceError> {
    let lookup_url = lookup_url.to_string();
    tokio::task::spawn_blocking(move || {
        use crate::schema::github_link_statuses::dsl as link_status_dsl;

        let mut connection = pool.get()?;
        let row = link_status_dsl::github_link_statuses
            .filter(link_status_dsl::url.eq(lookup_url))
            .first::<GithubLinkStatusRow>(&mut connection)
            .optional()?;
        Ok::<_, GithubStatusServiceError>(row)
    })
    .await?
}

async fn upsert_cache_row(
    pool: SqlitePool,
    row: GithubLinkStatusRow,
) -> Result<(), GithubStatusServiceError> {
    tokio::task::spawn_blocking(move || {
        use crate::schema::github_link_statuses::dsl as link_status_dsl;

        let mut connection = pool.get()?;
        let mut retry_attempt = 0;
        loop {
            let result = insert_into(link_status_dsl::github_link_statuses)
                .values(&row)
                .on_conflict(link_status_dsl::url)
                .do_update()
                .set((
                    link_status_dsl::kind.eq(&row.kind),
                    link_status_dsl::host.eq(&row.host),
                    link_status_dsl::owner.eq(&row.owner),
                    link_status_dsl::repo.eq(&row.repo),
                    link_status_dsl::resource_number.eq(row.resource_number),
                    link_status_dsl::issue_state.eq(&row.issue_state),
                    link_status_dsl::pr_state.eq(&row.pr_state),
                    link_status_dsl::build_state.eq(&row.build_state),
                    link_status_dsl::sonar_state.eq(&row.sonar_state),
                    link_status_dsl::review_state.eq(&row.review_state),
                    link_status_dsl::requested_reviewers.eq(&row.requested_reviewers),
                    link_status_dsl::copilot_review_state.eq(&row.copilot_review_state),
                    link_status_dsl::target_branch.eq(&row.target_branch),
                    link_status_dsl::merge_state.eq(&row.merge_state),
                    link_status_dsl::pr_is_draft.eq(&row.pr_is_draft),
                    link_status_dsl::unresolved_review_thread_count
                        .eq(&row.unresolved_review_thread_count),
                    link_status_dsl::fetched_at_epoch_seconds.eq(row.fetched_at_epoch_seconds),
                    link_status_dsl::refresh_after_epoch_seconds
                        .eq(row.refresh_after_epoch_seconds),
                    link_status_dsl::last_error.eq(&row.last_error),
                ))
                .execute(&mut connection);

            match result {
                Ok(_) => break,
                Err(error)
                    if is_sqlite_lock_contention(&error)
                        && retry_attempt < UPSERT_LOCK_RETRY_ATTEMPTS =>
                {
                    retry_attempt += 1;
                    std::thread::sleep(Duration::from_millis(
                        UPSERT_LOCK_RETRY_BASE_DELAY_MILLIS * retry_attempt as u64,
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }

        Ok::<_, GithubStatusServiceError>(())
    })
    .await?
}

fn is_sqlite_lock_contention(error: &diesel::result::Error) -> bool {
    match error {
        diesel::result::Error::DatabaseError(_, info) => {
            let message = info.message().to_ascii_lowercase();
            message.contains("database is locked") || message.contains("database table is locked")
        }
        _ => false,
    }
}

fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_secs() as i64
}

fn system_time_from_epoch_seconds(epoch_seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(epoch_seconds.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let counter = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "multicode-github-status-service-{}-{}-{}",
                std::process::id(),
                unique,
                counter
            ));
            fs::create_dir_all(&path).expect("test dir should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn parse_github_issue_reference_accepts_singular_and_plural_issue_links() {
        let parsed = parse_github_issue_reference("https://github.com/owner/repo/issue/42")
            .expect("singular github issue link should parse");
        assert_eq!(parsed.kind, GithubLinkKind::Issue);
        assert_eq!(parsed.url, "https://github.com/owner/repo/issue/42");
        assert_eq!(parsed.owner, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.resource_number, 42);

        let parsed_plural = parse_github_issue_reference("https://github.com/owner/repo/issues/43")
            .expect("plural github issue link should parse");
        assert_eq!(parsed_plural.kind, GithubLinkKind::Issue);
        assert_eq!(parsed_plural.url, "https://github.com/owner/repo/issue/43");
        assert_eq!(parsed_plural.owner, "owner");
        assert_eq!(parsed_plural.repo, "repo");
        assert_eq!(parsed_plural.resource_number, 43);
    }

    #[test]
    fn parse_github_pr_reference_accepts_singular_and_plural_pull_links() {
        let parsed_pull = parse_github_pr_reference("https://github.com/owner/repo/pull/5")
            .expect("singular pull path should parse");
        assert_eq!(parsed_pull.kind, GithubLinkKind::PullRequest);
        assert_eq!(parsed_pull.url, "https://github.com/owner/repo/pull/5");

        let parsed_pulls = parse_github_pr_reference("https://github.com/owner/repo/pulls/9")
            .expect("plural pulls path should parse");
        assert_eq!(parsed_pulls.kind, GithubLinkKind::PullRequest);
        assert_eq!(parsed_pulls.url, "https://github.com/owner/repo/pull/9");
    }

    #[test]
    fn parse_github_reference_rejects_non_github_or_mismatched_kind() {
        assert!(parse_github_issue_reference("https://example.com/owner/repo/issue/1").is_none());
        assert!(parse_github_pr_reference("https://github.com/owner/repo/issue/1").is_none());
        assert!(parse_github_issue_reference("http://github.com/owner/repo/issue/1").is_none());
    }

    #[test]
    fn check_runs_route_targets_commit_sha_endpoint() {
        let reference = GithubLinkRef {
            kind: GithubLinkKind::PullRequest,
            url: "https://github.com/owner/repo/pull/1".to_string(),
            host: "github.com".to_string(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            resource_number: 1,
        };

        let route = check_runs_route(&reference, "deadbeef");

        assert_eq!(route, "/repos/owner/repo/commits/deadbeef/check-runs");
        assert!(!route.contains("refs/heads/"));
    }

    #[test]
    fn failed_check_conclusions_match_requested_precedence() {
        assert!(is_failed_check_conclusion(Some("failure")));
        assert!(is_failed_check_conclusion(Some("cancelled")));
        assert!(!is_failed_check_conclusion(Some("success")));
        assert!(!is_failed_check_conclusion(None));
    }

    #[test]
    fn failed_status_states_match_requested_precedence() {
        assert!(is_failed_status_state(StatusState::Failure));
        assert!(is_failed_status_state(StatusState::Error));
        assert!(!is_failed_status_state(StatusState::Pending));
        assert!(!is_failed_status_state(StatusState::Success));
    }

    #[test]
    fn successful_statuses_require_all_steps_to_succeed() {
        let success: Status = serde_json::from_value(serde_json::json!({
            "avatar_url": null,
            "context": "ci/test",
            "created_at": null,
            "creator": null,
            "description": null,
            "id": null,
            "node_id": null,
            "state": "success",
            "target_url": null,
            "updated_at": null,
            "url": null
        }))
        .expect("successful status should deserialize");
        let pending: Status = serde_json::from_value(serde_json::json!({
            "avatar_url": null,
            "context": "ci/test",
            "created_at": null,
            "creator": null,
            "description": null,
            "id": null,
            "node_id": null,
            "state": "pending",
            "target_url": null,
            "updated_at": null,
            "url": null
        }))
        .expect("pending status should deserialize");

        assert!(are_all_statuses_successful(std::slice::from_ref(&success)));
        assert!(!are_all_statuses_successful(&[success, pending.clone()]));
        assert_eq!(
            derive_pr_build_state(&[], &[]),
            GithubPrBuildState::Building
        );
        assert_eq!(
            derive_pr_build_state(&[], &[pending]),
            GithubPrBuildState::Building
        );
    }

    #[test]
    fn derive_pr_build_state_prefers_failed_over_success_or_in_progress_statuses() {
        let failure: Status = serde_json::from_value(serde_json::json!({
            "avatar_url": null,
            "context": "ci/test",
            "created_at": null,
            "creator": null,
            "description": null,
            "id": null,
            "node_id": null,
            "state": "failure",
            "target_url": null,
            "updated_at": null,
            "url": null
        }))
        .expect("failed status should deserialize");
        let pending: Status = serde_json::from_value(serde_json::json!({
            "avatar_url": null,
            "context": "ci/test",
            "created_at": null,
            "creator": null,
            "description": null,
            "id": null,
            "node_id": null,
            "state": "pending",
            "target_url": null,
            "updated_at": null,
            "url": null
        }))
        .expect("pending status should deserialize");
        let success: Status = serde_json::from_value(serde_json::json!({
            "avatar_url": null,
            "context": "ci/test",
            "created_at": null,
            "creator": null,
            "description": null,
            "id": null,
            "node_id": null,
            "state": "success",
            "target_url": null,
            "updated_at": null,
            "url": null
        }))
        .expect("successful status should deserialize");

        assert_eq!(
            derive_pr_build_state(&[], &[failure, pending, success.clone()]),
            GithubPrBuildState::Failed
        );
        assert_eq!(
            derive_pr_build_state(&[], &[success.clone()]),
            GithubPrBuildState::Building
        );
    }

    #[test]
    fn derive_pr_build_state_requires_successful_checks_and_statuses_for_success() {
        let success: Status = serde_json::from_value(serde_json::json!({
            "avatar_url": null,
            "context": "ci/test",
            "created_at": null,
            "creator": null,
            "description": null,
            "id": null,
            "node_id": null,
            "state": "success",
            "target_url": null,
            "updated_at": null,
            "url": null
        }))
        .expect("successful status should deserialize");
        let successful_check_run: octocrab::models::checks::CheckRun =
            serde_json::from_value(serde_json::json!({
                "id": 1,
                "node_id": "CHK_1",
                "details_url": "https://github.com/owner/repo/actions/runs/1",
                "head_sha": "deadbeef",
                "url": "https://api.github.com/check-runs/1",
                "html_url": "https://github.com/owner/repo/runs/1",
                "conclusion": "success",
                "output": {
                    "title": "CI",
                    "summary": "all good",
                    "text": null,
                    "annotations_count": 0,
                    "annotations_url": "https://api.github.com/check-runs/1/annotations"
                },
                "started_at": "2026-03-11T20:00:00Z",
                "completed_at": "2026-03-11T20:01:00Z",
                "name": "ci/test",
                "pull_requests": []
            }))
            .expect("successful check run should deserialize");

        assert_eq!(
            derive_pr_build_state(&[successful_check_run], &[success]),
            GithubPrBuildState::Succeeded
        );
    }

    fn review(login: &str, state: &str) -> octocrab::models::pulls::Review {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "node_id": "PRR_kwDOAA",
            "user": {
                "login": login,
                "id": 1,
                "node_id": "MDQ6VXNlcjE=",
                "avatar_url": "https://example.com/avatar.png",
                "gravatar_id": "",
                "url": "https://api.github.com/users/tester",
                "html_url": "https://github.com/tester",
                "followers_url": "https://api.github.com/users/tester/followers",
                "following_url": "https://api.github.com/users/tester/following{/other_user}",
                "gists_url": "https://api.github.com/users/tester/gists{/gist_id}",
                "starred_url": "https://api.github.com/users/tester/starred{/owner}{/repo}",
                "subscriptions_url": "https://api.github.com/users/tester/subscriptions",
                "organizations_url": "https://api.github.com/users/tester/orgs",
                "repos_url": "https://api.github.com/users/tester/repos",
                "events_url": "https://api.github.com/users/tester/events{/privacy}",
                "received_events_url": "https://api.github.com/users/tester/received_events",
                "type": "User",
                "site_admin": false
            },
            "body": null,
            "state": state,
            "html_url": "https://github.com/owner/repo/pull/1#pullrequestreview-1",
            "pull_request_url": "https://api.github.com/repos/owner/repo/pulls/1",
            "author_association": "MEMBER",
            "_links": {
                "html": { "href": "https://github.com/owner/repo/pull/1#pullrequestreview-1" },
                "pull_request": { "href": "https://api.github.com/repos/owner/repo/pulls/1" }
            },
            "submitted_at": "2026-03-11T20:01:00Z",
            "commit_id": "deadbeef"
        }))
        .expect("review should deserialize")
    }

    fn status_rollup_check_run(
        name: &str,
        status: &str,
        conclusion: Option<&str>,
        workflow_name: Option<&str>,
        app_slug: Option<&str>,
    ) -> PrStatusCheckRollupNode {
        serde_json::from_value(serde_json::json!({
            "__typename": "CheckRun",
            "name": name,
            "status": status,
            "conclusion": conclusion,
            "detailsUrl": "https://github.com/owner/repo/actions/runs/1/job/1",
            "checkSuite": {
                "app": {
                    "slug": app_slug
                },
                "workflowRun": workflow_name.map(|name| serde_json::json!({
                    "workflow": {
                        "name": name
                    }
                }))
            }
        }))
        .expect("status rollup check run should deserialize")
    }

    fn requested_reviewers(logins: &[&str]) -> HashSet<String> {
        logins.iter().map(|login| (*login).to_string()).collect()
    }

    #[test]
    fn derive_pr_build_state_from_rollup_prefers_in_progress_over_failures() {
        let running = status_rollup_check_run("build (17)", "IN_PROGRESS", None, None, None);
        let failed =
            status_rollup_check_run("build (21)", "COMPLETED", Some("FAILURE"), None, None);

        assert_eq!(
            derive_pr_build_state_from_rollup(&[failed, running]),
            Some(GithubPrBuildState::Building)
        );
    }

    #[test]
    fn derive_pr_build_state_from_rollup_reports_success_only_when_all_contexts_pass() {
        let success =
            status_rollup_check_run("build (17)", "COMPLETED", Some("SUCCESS"), None, None);
        let sonar = status_rollup_check_run(
            "SonarCloud Code Analysis",
            "COMPLETED",
            Some("SUCCESS"),
            None,
            Some("sonarqubecloud"),
        );

        assert_eq!(
            derive_pr_build_state_from_rollup(&[success, sonar]),
            Some(GithubPrBuildState::Succeeded)
        );
        assert_eq!(derive_pr_build_state_from_rollup(&[]), None);
    }

    #[test]
    fn derive_copilot_review_state_tracks_requested_and_completed_review() {
        assert_eq!(
            derive_copilot_review_state(&[], &HashSet::new()),
            GithubPrCopilotReviewState::None
        );
        assert_eq!(
            derive_copilot_review_state(
                &[],
                &requested_reviewers(&["copilot-pull-request-reviewer"]),
            ),
            GithubPrCopilotReviewState::Requested
        );
        assert_eq!(
            derive_copilot_review_state(
                &[review("copilot-pull-request-reviewer", "COMMENTED")],
                &HashSet::new(),
            ),
            GithubPrCopilotReviewState::Done
        );
        assert_eq!(
            derive_copilot_review_state(
                &[review("copilot-pull-request-reviewer[bot]", "COMMENTED")],
                &HashSet::new(),
            ),
            GithubPrCopilotReviewState::Done
        );
    }

    #[test]
    fn copilot_assignee_detection_matches_pr_metadata_logins() {
        assert!(is_copilot_assignee_login("Copilot"));
        assert!(is_copilot_assignee_login("github-copilot"));
        assert!(is_copilot_assignee_login("copilot-pull-request-reviewer"));
        assert!(is_copilot_assignee_login(
            "copilot-pull-request-reviewer[bot]"
        ));
        assert!(!is_copilot_assignee_login("alvarosanchez"));
    }

    #[test]
    fn derive_pr_review_state_matches_requested_pr_review_rules() {
        assert_eq!(
            derive_pr_review_state(&[], &HashSet::new(), 0, 0),
            GithubPrReviewState::None
        );

        assert_eq!(
            derive_pr_review_state(&[review("alice", "COMMENTED")], &HashSet::new(), 0, 1),
            GithubPrReviewState::Outstanding
        );

        assert_eq!(
            derive_pr_review_state(&[review("alice", "COMMENTED")], &HashSet::new(), 0, 0),
            GithubPrReviewState::None
        );

        assert_eq!(
            derive_pr_review_state(
                &[review("alice", "CHANGES_REQUESTED")],
                &HashSet::new(),
                0,
                0,
            ),
            GithubPrReviewState::Rejected
        );

        assert_eq!(
            derive_pr_review_state(
                &[review("alice", "APPROVED"), review("bob", "APPROVED")],
                &HashSet::new(),
                0,
                0,
            ),
            GithubPrReviewState::Accepted
        );

        assert_eq!(
            derive_pr_review_state(&[review("alice", "APPROVED")], &HashSet::new(), 1, 1),
            GithubPrReviewState::Outstanding
        );

        assert_eq!(
            derive_pr_review_state(&[], &requested_reviewers(&["alice"]), 1, 0),
            GithubPrReviewState::Requested
        );

        assert_eq!(
            derive_pr_review_state(
                &[],
                &requested_reviewers(&["copilot-pull-request-reviewer"]),
                0,
                0,
            ),
            GithubPrReviewState::None
        );

        assert_eq!(
            derive_pr_review_state(
                &[review(
                    "copilot-pull-request-reviewer[bot]",
                    "CHANGES_REQUESTED"
                )],
                &HashSet::new(),
                0,
                0,
            ),
            GithubPrReviewState::None
        );

        assert_eq!(
            derive_pr_review_state(&[review("Copilot", "APPROVED")], &HashSet::new(), 0, 0),
            GithubPrReviewState::None
        );

        assert_eq!(
            derive_pr_review_state(
                &[review("alice", "APPROVED"), review("bob", "PENDING")],
                &HashSet::new(),
                0,
                0,
            ),
            GithubPrReviewState::Outstanding
        );

        assert_eq!(
            derive_pr_review_state(
                &[review("alice", "CHANGES_REQUESTED")],
                &requested_reviewers(&["alice"]),
                1,
                0,
            ),
            GithubPrReviewState::Rejected
        );
    }

    #[test]
    fn requested_reviewers_label_filters_automation_and_sorts_humans() {
        let reviewers = requested_reviewers(&[
            "zara",
            "copilot-pull-request-reviewer",
            "copilot-pull-request-reviewer[bot]",
            "github-copilot",
            "Copilot",
            "@copilot",
            "github-actions",
            "alvarosanchez",
        ]);

        assert_eq!(
            requested_reviewers_label(&reviewers, &[]).as_deref(),
            Some("alvarosanchez, zara")
        );
    }

    #[test]
    fn graphql_team_review_requests_are_not_treated_as_user_logins() {
        let reviewer = PrReviewRequestsGraphqlReviewer::Team {};

        assert_eq!(reviewer.login(), None);
    }

    #[test]
    fn graphql_status_rollup_contexts_include_page_info() {
        let connection: PrStatusCheckRollupGraphqlContextConnection =
            serde_json::from_value(serde_json::json!({
                "nodes": [],
                "pageInfo": {
                    "hasNextPage": true,
                    "endCursor": "cursor-2"
                }
            }))
            .expect("status rollup connection should deserialize");

        assert!(connection.page_info.has_next_page);
        assert_eq!(connection.page_info.end_cursor.as_deref(), Some("cursor-2"));
    }

    #[test]
    fn derive_pr_sonar_state_from_rollup_treats_sonatype_vuln_scan_as_sonar_failure() {
        let nodes = vec![
            status_rollup_check_run(
                "build (25)",
                "COMPLETED",
                Some("FAILURE"),
                Some("Sonatype Vuln Scan"),
                Some("github-actions"),
            ),
            status_rollup_check_run(
                "SonarCloud Code Analysis",
                "COMPLETED",
                Some("SUCCESS"),
                None,
                Some("sonarqubecloud"),
            ),
        ];

        assert_eq!(
            derive_pr_sonar_state_from_rollup(&nodes),
            Some(GithubPrSonarState::Failed)
        );
        assert_eq!(
            merge_pr_sonar_states(
                Some(GithubPrSonarState::Succeeded),
                derive_pr_sonar_state_from_rollup(&nodes),
            ),
            Some(GithubPrSonarState::Failed)
        );
    }

    #[test]
    fn derive_pr_sonar_state_from_rollup_ignores_unrelated_ci_workflows() {
        let nodes = vec![status_rollup_check_run(
            "build (25)",
            "COMPLETED",
            Some("FAILURE"),
            Some("Java CI"),
            Some("github-actions"),
        )];

        assert_eq!(derive_pr_sonar_state_from_rollup(&nodes), None);
    }

    #[test]
    fn next_pr_refresh_policy_matches_required_intervals() {
        let now = 1_000_000_i64;

        assert_eq!(
            next_pr_refresh_after_epoch_seconds(
                now,
                GithubPrState::Open,
                GithubPrBuildState::Building,
                GithubPrReviewState::Outstanding,
            ),
            Some(now + PR_BUILDING_REFRESH_INTERVAL.as_secs() as i64)
        );

        assert_eq!(
            next_pr_refresh_after_epoch_seconds(
                now,
                GithubPrState::Open,
                GithubPrBuildState::Succeeded,
                GithubPrReviewState::Outstanding,
            ),
            Some(now + PR_BUILD_SUCCESS_REVIEW_PENDING_INTERVAL.as_secs() as i64)
        );

        assert_eq!(
            next_pr_refresh_after_epoch_seconds(
                now,
                GithubPrState::Open,
                GithubPrBuildState::Succeeded,
                GithubPrReviewState::Accepted,
            ),
            Some(now + PR_REVIEW_ACCEPTED_PENDING_MERGE_INTERVAL.as_secs() as i64)
        );

        assert_eq!(
            next_pr_refresh_after_epoch_seconds(
                now,
                GithubPrState::Merged,
                GithubPrBuildState::Succeeded,
                GithubPrReviewState::Accepted,
            ),
            Some(now + PR_CLOSED_RECHECK_INTERVAL.as_secs() as i64)
        );
    }

    #[test]
    fn next_refresh_wait_duration_uses_refresh_after_timestamp() {
        let now = 2_000_i64;
        let status = CachedLinkStatus {
            reference: GithubLinkRef {
                kind: GithubLinkKind::Issue,
                url: "https://github.com/owner/repo/issue/7".to_string(),
                host: "github.com".to_string(),
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                resource_number: 7,
            },
            issue_state: Some(GithubIssueState::Open),
            pr_state: None,
            build_state: None,
            sonar_state: None,
            review_state: None,
            requested_reviewers: None,
            copilot_review_state: None,
            target_branch: None,
            merge_state: None,
            pr_is_draft: None,
            unresolved_review_thread_count: None,
            fetched_at_epoch_seconds: Some(now - 30),
            refresh_after_epoch_seconds: Some(now + 45),
            last_error: None,
        };

        assert_eq!(
            next_refresh_wait_duration(Some(&status), now),
            Duration::from_secs(45)
        );
        assert_eq!(
            next_refresh_wait_duration(Some(&status), now + 60),
            Duration::ZERO
        );
    }

    #[test]
    fn pr_error_placeholder_produces_renderable_and_persistable_status() {
        let now = 2_000_i64;
        let status = CachedLinkStatus::new_pr_error_placeholder(
            GithubLinkRef {
                kind: GithubLinkKind::PullRequest,
                url: "https://github.com/owner/repo/pull/7".to_string(),
                host: "github.com".to_string(),
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                resource_number: 7,
            },
            now,
        );

        assert_eq!(
            status.github_status(),
            Some(GithubStatus::Pr(GithubPrStatus {
                state: GithubPrState::Open,
                target_branch: None,
                merge_state: Some(GithubPrMergeState::Unknown),
                build: GithubPrBuildState::Building,
                sonar: None,
                review: GithubPrReviewState::None,
                requested_reviewers: None,
                copilot_review: GithubPrCopilotReviewState::None,
                is_draft: false,
                unresolved_review_threads: 0,
                fetched_at: system_time_from_epoch_seconds(now),
            }))
        );

        let row = status.to_row().expect("placeholder should persist");
        let round_trip = CachedLinkStatus::from_row(row).expect("placeholder should reload");
        assert_eq!(
            round_trip.github_status(),
            Some(GithubStatus::Pr(GithubPrStatus {
                state: GithubPrState::Open,
                target_branch: None,
                merge_state: Some(GithubPrMergeState::Unknown),
                build: GithubPrBuildState::Building,
                sonar: None,
                review: GithubPrReviewState::None,
                requested_reviewers: None,
                copilot_review: GithubPrCopilotReviewState::None,
                is_draft: false,
                unresolved_review_threads: 0,
                fetched_at: system_time_from_epoch_seconds(now),
            }))
        );
    }

    #[test]
    fn pr_status_round_trip_preserves_target_branch_and_merge_state() {
        let now = 2_000_i64;
        let status = CachedLinkStatus {
            reference: GithubLinkRef {
                kind: GithubLinkKind::PullRequest,
                url: "https://github.com/owner/repo/pull/7".to_string(),
                host: "github.com".to_string(),
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                resource_number: 7,
            },
            issue_state: None,
            pr_state: Some(GithubPrState::Open),
            build_state: Some(GithubPrBuildState::Succeeded),
            sonar_state: None,
            review_state: Some(GithubPrReviewState::Accepted),
            requested_reviewers: None,
            copilot_review_state: Some(GithubPrCopilotReviewState::None),
            target_branch: Some("6.0.x".to_string()),
            merge_state: Some(GithubPrMergeState::Dirty),
            pr_is_draft: Some(false),
            unresolved_review_thread_count: Some(0),
            fetched_at_epoch_seconds: Some(now),
            refresh_after_epoch_seconds: Some(now + 60),
            last_error: None,
        };

        let row = status.to_row().expect("PR status should persist");
        assert_eq!(row.target_branch.as_deref(), Some("6.0.x"));
        assert_eq!(row.merge_state.as_deref(), Some("dirty"));
        let round_trip = CachedLinkStatus::from_row(row).expect("PR status should reload");
        let round_trip = round_trip.pr_status().expect("PR status should render");
        assert_eq!(round_trip.target_branch, Some("6.0.x".to_string()));
        assert_eq!(round_trip.merge_state, Some(GithubPrMergeState::Dirty));
    }

    #[test]
    fn old_pr_cache_rows_without_new_nullable_columns_still_render_conservatively() {
        let now = 2_000_i64;
        let status = CachedLinkStatus::from_row(GithubLinkStatusRow {
            url: "https://github.com/owner/repo/pull/7".to_string(),
            kind: GithubLinkKind::PullRequest.as_db().to_string(),
            host: "github.com".to_string(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            resource_number: 7,
            issue_state: None,
            pr_state: Some(GithubPrState::Open.as_db().to_string()),
            build_state: Some(GithubPrBuildState::Succeeded.as_db().to_string()),
            sonar_state: None,
            review_state: Some(GithubPrReviewState::Accepted.as_db().to_string()),
            requested_reviewers: None,
            copilot_review_state: None,
            target_branch: None,
            merge_state: None,
            pr_is_draft: Some(false),
            unresolved_review_thread_count: None,
            fetched_at_epoch_seconds: now,
            refresh_after_epoch_seconds: Some(now + 60),
            last_error: None,
        })
        .expect("old PR cache row should remain usable");

        assert_eq!(
            status.pr_status(),
            Some(GithubPrStatus {
                state: GithubPrState::Open,
                target_branch: None,
                merge_state: Some(GithubPrMergeState::Unknown),
                build: GithubPrBuildState::Succeeded,
                sonar: None,
                review: GithubPrReviewState::Accepted,
                requested_reviewers: None,
                copilot_review: GithubPrCopilotReviewState::None,
                is_draft: false,
                unresolved_review_threads: 0,
                fetched_at: system_time_from_epoch_seconds(now),
            })
        );
    }

    #[test]
    fn service_loads_github_token_from_environment_variable() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let variable_name = "MULTICODE_TEST_GITHUB_TOKEN_ENV";
            let previous = std::env::var_os(variable_name);
            unsafe {
                std::env::set_var(variable_name, "env-token-value");
            }

            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            tokio::fs::create_dir_all(&workspace_root)
                .await
                .expect("workspace root should exist");
            let database = Database::open_in_workspace(&workspace_root)
                .await
                .expect("database should open");

            let service = GithubStatusService::new(
                database,
                Some(GithubTokenConfig {
                    env: Some(variable_name.to_string()),
                    command: None,
                    keychain_service: None,
                    keychain_account: None,
                }),
            )
            .await
            .expect("service should construct");

            let token = service
                .github_token()
                .await
                .expect("token should load from env");
            assert_eq!(token, "env-token-value");

            match previous {
                Some(value) => unsafe { std::env::set_var(variable_name, value) },
                None => unsafe { std::env::remove_var(variable_name) },
            }
        });
    }

    #[test]
    fn service_loads_github_token_from_command() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            tokio::fs::create_dir_all(&workspace_root)
                .await
                .expect("workspace root should exist");
            let database = Database::open_in_workspace(&workspace_root)
                .await
                .expect("database should open");

            let service = GithubStatusService::new(
                database,
                Some(GithubTokenConfig {
                    env: None,
                    command: Some("printf 'command-token-value\n'".to_string()),
                    keychain_service: None,
                    keychain_account: None,
                }),
            )
            .await
            .expect("service should construct");

            let token = service
                .github_token()
                .await
                .expect("token should load from command");
            assert_eq!(token, "command-token-value");
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn service_loads_github_token_from_keychain() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            tokio::fs::create_dir_all(&workspace_root)
                .await
                .expect("workspace root should exist");
            let database = Database::open_in_workspace(&workspace_root)
                .await
                .expect("database should open");

            let bin_dir = root.path().join("bin");
            fs::create_dir_all(&bin_dir).expect("bin dir should exist");
            let security_path = bin_dir.join("security");
            fs::write(
                &security_path,
                "#!/bin/sh\n[ \"$1\" = \"find-generic-password\" ] || exit 11\n[ \"$2\" = \"-s\" ] || exit 12\n[ \"$3\" = \"multicode.github\" ] || exit 13\n[ \"$4\" = \"-a\" ] || exit 14\n[ \"$5\" = \"github-mcp-token\" ] || exit 15\n[ \"$6\" = \"-w\" ] || exit 16\nprintf 'keychain-token-value\\n'\n",
            )
            .expect("fake security should be written");
            let mut perms = fs::metadata(&security_path)
                .expect("fake security metadata should be readable")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&security_path, perms)
                .expect("fake security should be executable");

            let previous_path = std::env::var_os("PATH");
            let path = match &previous_path {
                Some(previous_path) => format!(
                    "{}:{}",
                    bin_dir.display(),
                    previous_path.to_string_lossy()
                ),
                None => bin_dir.display().to_string(),
            };
            unsafe {
                std::env::set_var("PATH", path);
            }

            let service = GithubStatusService::new(
                database,
                Some(GithubTokenConfig {
                    env: None,
                    command: None,
                    keychain_service: Some("multicode.github".to_string()),
                    keychain_account: Some("github-mcp-token".to_string()),
                }),
            )
            .await
            .expect("service should construct");

            let token = service
                .github_token()
                .await
                .expect("token should load from keychain");
            assert_eq!(token, "keychain-token-value");

            match previous_path {
                Some(value) => unsafe { std::env::set_var("PATH", value) },
                None => unsafe { std::env::remove_var("PATH") },
            }
        });
    }

    #[test]
    fn watch_status_rejects_non_github_links() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            tokio::fs::create_dir_all(&workspace_root)
                .await
                .expect("workspace root should exist");

            let database = Database::open_in_workspace(&workspace_root)
                .await
                .expect("database should open");
            let service = GithubStatusService::new(database, None)
                .await
                .expect("service should construct");

            assert!(
                service
                    .watch_status("https://example.com/owner/repo/issue/7")
                    .is_none()
            );
        });
    }

    #[test]
    fn service_loads_cached_issue_status_and_avoids_immediate_refetch() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_root = root.path().join("workspaces");
            tokio::fs::create_dir_all(&workspace_root)
                .await
                .expect("workspace root should exist");

            let database = Database::open_in_workspace(&workspace_root)
                .await
                .expect("database should open");

            let now = now_epoch_seconds();
            let row = GithubLinkStatusRow {
                url: "https://github.com/owner/repo/issue/7".to_string(),
                kind: "issue".to_string(),
                host: "github.com".to_string(),
                owner: "owner".to_string(),
                repo: "repo".to_string(),
                resource_number: 7,
                issue_state: Some("open".to_string()),
                pr_state: None,
                build_state: None,
                sonar_state: None,
                review_state: None,
                requested_reviewers: None,
                copilot_review_state: None,
                target_branch: None,
                merge_state: None,
                pr_is_draft: None,
                unresolved_review_thread_count: None,
                fetched_at_epoch_seconds: now,
                refresh_after_epoch_seconds: Some(now + Duration::from_mins(60).as_secs() as i64),
                last_error: None,
            };
            upsert_cache_row(database.pool().clone(), row)
                .await
                .expect("cache row should be inserted");

            let service = GithubStatusService::new(database, None)
                .await
                .expect("service should load cache");

            let mut status_rx = service
                .watch_status("https://github.com/owner/repo/issue/7")
                .expect("github link should create watch receiver");

            if status_rx.borrow().is_none() {
                tokio::time::timeout(Duration::from_secs(2), status_rx.changed())
                    .await
                    .expect("status watch should publish cached state")
                    .expect("status watch should remain open");
            }

            let status = status_rx.borrow().clone();
            match status {
                Some(GithubStatus::Issue(issue_status)) => {
                    assert_eq!(issue_status.state, GithubIssueState::Open)
                }
                other => panic!("unexpected status from watch: {other:?}"),
            }
        });
    }
}
