CREATE TABLE github_link_statuses (
    url TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    host TEXT NOT NULL,
    owner TEXT NOT NULL,
    repo TEXT NOT NULL,
    resource_number BIGINT NOT NULL,
    issue_state TEXT,
    pr_state TEXT,
    build_state TEXT,
    review_state TEXT,
    pr_is_draft BOOLEAN,
    fetched_at_epoch_seconds BIGINT NOT NULL,
    refresh_after_epoch_seconds BIGINT,
    last_error TEXT
);

CREATE INDEX github_link_statuses_kind_refresh_idx
    ON github_link_statuses (kind, refresh_after_epoch_seconds);
