diesel::table! {
    github_link_statuses (url) {
        url -> Text,
        kind -> Text,
        host -> Text,
        owner -> Text,
        repo -> Text,
        resource_number -> BigInt,
        issue_state -> Nullable<Text>,
        pr_state -> Nullable<Text>,
        build_state -> Nullable<Text>,
        review_state -> Nullable<Text>,
        pr_is_draft -> Nullable<Bool>,
        fetched_at_epoch_seconds -> BigInt,
        refresh_after_epoch_seconds -> Nullable<BigInt>,
        last_error -> Nullable<Text>,
    }
}
