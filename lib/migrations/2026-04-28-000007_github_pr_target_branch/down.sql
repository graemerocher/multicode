CREATE TABLE github_link_statuses_new (
    url TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    host TEXT NOT NULL,
    owner TEXT NOT NULL,
    repo TEXT NOT NULL,
    resource_number BIGINT NOT NULL,
    issue_state TEXT,
    pr_state TEXT,
    build_state TEXT,
    sonar_state TEXT,
    review_state TEXT,
    requested_reviewers TEXT,
    copilot_review_state TEXT,
    pr_is_draft BOOLEAN,
    unresolved_review_thread_count BIGINT,
    fetched_at_epoch_seconds BIGINT NOT NULL,
    refresh_after_epoch_seconds BIGINT,
    last_error TEXT
);

INSERT INTO github_link_statuses_new (
    url,
    kind,
    host,
    owner,
    repo,
    resource_number,
    issue_state,
    pr_state,
    build_state,
    sonar_state,
    review_state,
    requested_reviewers,
    copilot_review_state,
    pr_is_draft,
    unresolved_review_thread_count,
    fetched_at_epoch_seconds,
    refresh_after_epoch_seconds,
    last_error
)
SELECT
    url,
    kind,
    host,
    owner,
    repo,
    resource_number,
    issue_state,
    pr_state,
    build_state,
    sonar_state,
    review_state,
    requested_reviewers,
    copilot_review_state,
    pr_is_draft,
    unresolved_review_thread_count,
    fetched_at_epoch_seconds,
    refresh_after_epoch_seconds,
    last_error
FROM github_link_statuses;

DROP TABLE github_link_statuses;
ALTER TABLE github_link_statuses_new RENAME TO github_link_statuses;

CREATE INDEX github_link_statuses_kind_refresh_idx
    ON github_link_statuses (kind, refresh_after_epoch_seconds);
