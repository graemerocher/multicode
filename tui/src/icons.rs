use crate::*;

pub(crate) fn issue_type_icon_kind_and_color(
    issue_type: WorkspaceIssueType,
) -> (StatusIconKind, Color) {
    match issue_type {
        WorkspaceIssueType::Bug => (StatusIconKind::Bug, Color::Red),
        WorkspaceIssueType::Docs => (StatusIconKind::Docs, Color::LightBlue),
        WorkspaceIssueType::Enhancement => (StatusIconKind::Enhancement, Color::Green),
        WorkspaceIssueType::Improvement => (StatusIconKind::Improvement, Color::Yellow),
        WorkspaceIssueType::Regression => (StatusIconKind::Regression, Color::Magenta),
        WorkspaceIssueType::DependencyUpgrade => (StatusIconKind::DependencyUpgrade, Color::Cyan),
    }
}

pub(crate) fn issue_icon_kind_and_color(state: GithubIssueState) -> (StatusIconKind, Color) {
    match state {
        GithubIssueState::Open => (StatusIconKind::IssueOpened, Color::Green),
        GithubIssueState::Closed => (StatusIconKind::IssueClosed, Color::Magenta),
    }
}

pub(crate) fn pr_icon_kind_and_color(pr: GithubPrStatus) -> (StatusIconKind, Color) {
    if pr.state == GithubPrState::Open && pr.is_draft {
        return (StatusIconKind::GitPullRequestDraft, Color::DarkGray);
    }

    match pr.state {
        GithubPrState::Open => (StatusIconKind::GitPullRequest, Color::Green),
        GithubPrState::Rejected => (StatusIconKind::GitPullRequestClosed, Color::Red),
        GithubPrState::Merged => (StatusIconKind::GitMerge, Color::Magenta),
    }
}

pub(crate) fn pr_build_icon_color(pr: GithubPrStatus) -> Option<Color> {
    if pr.state != GithubPrState::Open {
        return None;
    }

    Some(match pr.build {
        GithubPrBuildState::Building => Color::Yellow,
        GithubPrBuildState::Succeeded => Color::Green,
        GithubPrBuildState::Failed => Color::Red,
    })
}

pub(crate) fn pr_review_icon_color(pr: GithubPrStatus) -> Option<Color> {
    if pr.state != GithubPrState::Open {
        return None;
    }

    Some(match pr.review {
        GithubPrReviewState::None => Color::DarkGray,
        GithubPrReviewState::Outstanding => Color::Yellow,
        GithubPrReviewState::Accepted => Color::Green,
        GithubPrReviewState::Rejected => Color::Red,
    })
}

pub(crate) fn icon_glyph(kind: StatusIconKind) -> &'static str {
    match kind {
        StatusIconKind::Eye => "\u{f441}",
        StatusIconKind::Server => "\u{f473}",
        StatusIconKind::FileDiff => "\u{f4d2}",
        StatusIconKind::Bug => "\u{f188}",
        StatusIconKind::Docs => "\u{f02d}",
        StatusIconKind::Enhancement => "\u{f135}",
        StatusIconKind::Improvement => "\u{f0ad}",
        StatusIconKind::Regression => "\u{f1da}",
        StatusIconKind::DependencyUpgrade => "\u{f1b2}",
        StatusIconKind::GitPullRequest => "\u{f407}",
        StatusIconKind::GitPullRequestDraft => "\u{f4dd}",
        StatusIconKind::GitPullRequestClosed => "\u{f4dc}",
        StatusIconKind::GitMerge => "\u{f419}",
        StatusIconKind::IssueOpened => "\u{f41b}",
        StatusIconKind::IssueClosed => "\u{f41d}",
    }
}
