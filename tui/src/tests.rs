use crate::*;

#[cfg(test)]
mod tests {
    use multicode_lib::{
        AutomationAgentState, RootSessionStatus,
        services::{
            CompareConfig, CompareTool, HandlerConfig, codex_app_server::CodexThreadStatus,
        },
    };

    use super::*;
    use crate::app::{
        build_codex_fix_ci_prompt, compact_github_tooltip_target, count_codex_session_turn_metrics,
        github_repository_spec, github_repository_url,
        last_user_message_from_codex_session_log_contents, repository_diff_shell_command,
        restored_selected_row, shell_command_in_repo,
        should_auto_resume_autonomous_codex_after_attach,
        should_auto_resume_task_codex_after_attach, should_offer_codex_ci_fix,
        should_queue_task_codex_resume_until_vm_available, should_restart_codex_task_for_ci_fix,
        should_restart_codex_task_for_pr_request, should_restart_task_codex_after_attach,
        should_resume_codex_task_after_incomplete_attached_turn,
        should_retry_codex_task_attach_with_last_thread,
        should_start_fresh_codex_task_session_after_failed_attach,
        snapshot_attach_cwd_for_selection, snapshot_attach_target_for_selection,
        starting_modal_failure_status, task_repository_spec, working_codex_task_attach_target,
    };
    use crate::icons::{
        icon_glyph, issue_icon_kind_and_color, pr_build_icon_color, pr_icon_kind_and_color,
        pr_review_icon_color,
    };
    use crate::ops::{
        SessionWaitState, attach_cli_args, build_handler_command, command_exists,
        compare_tool_is_available, compare_tool_name, session_wait_state_for_entry,
        task_attach_target, tmux_session_command, tmux_status_left, validate_workspace_link_target,
        workspace_attach_target, workspace_ordering,
    };
    use crate::render::selected_link_tooltip_area;
    use crate::system::{
        centered_rect_fixed, disk_usage_from_statvfs, machine_cpu_percent, parse_proc_cpu_totals,
        parse_proc_meminfo_total_ram_bytes, parse_proc_meminfo_used_ram_bytes,
        started_workspace_attach_ready,
    };
    use multicode_lib::{
        PersistentWorkspaceSnapshot, RuntimeBackend, RuntimeHandleSnapshot,
        TransientWorkspaceSnapshot, services::AgentProvider,
    };
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::broadcast;

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    fn help_line(
        mode: UiMode,
        selected_row: usize,
        workspace_count: usize,
        selected_workspace: Option<&WorkspaceSnapshot>,
        selected_task_row: bool,
        selected_workspace_link_count: usize,
        selected_link_index: Option<usize>,
        selected_link_is_custom: bool,
        selected_link_is_placeholder: bool,
        selected_link_kind: Option<WorkspaceLinkKind>,
        selected_workspace_has_refreshable_github_link: bool,
        selected_workspace_can_assign_issue: bool,
        selected_workspace_can_diff: bool,
        selected_workspace_can_edit: bool,
        tool_progress_can_cancel: bool,
        tool_hotkeys: &[(String, String)],
        status: &str,
    ) -> Line<'static> {
        super::help_line(
            mode,
            selected_row,
            workspace_count,
            selected_workspace,
            selected_task_row,
            selected_workspace_link_count,
            selected_link_index,
            selected_link_is_custom,
            selected_link_is_placeholder,
            selected_link_kind,
            selected_workspace_has_refreshable_github_link,
            selected_workspace_can_assign_issue,
            selected_workspace_can_diff,
            selected_workspace_can_edit,
            false,
            tool_progress_can_cancel,
            tool_hotkeys,
            status,
        )
    }

    fn help_line_with_task_fix(
        mode: UiMode,
        selected_row: usize,
        workspace_count: usize,
        selected_workspace: Option<&WorkspaceSnapshot>,
        selected_task_row: bool,
        selected_workspace_link_count: usize,
        selected_link_index: Option<usize>,
        selected_link_is_custom: bool,
        selected_link_is_placeholder: bool,
        selected_link_kind: Option<WorkspaceLinkKind>,
        selected_workspace_has_refreshable_github_link: bool,
        selected_workspace_can_assign_issue: bool,
        selected_workspace_can_diff: bool,
        selected_workspace_can_edit: bool,
        selected_task_can_fix_ci: bool,
        tool_progress_can_cancel: bool,
        tool_hotkeys: &[(String, String)],
        status: &str,
    ) -> Line<'static> {
        super::help_line(
            mode,
            selected_row,
            workspace_count,
            selected_workspace,
            selected_task_row,
            selected_workspace_link_count,
            selected_link_index,
            selected_link_is_custom,
            selected_link_is_placeholder,
            selected_link_kind,
            selected_workspace_has_refreshable_github_link,
            selected_workspace_can_assign_issue,
            selected_workspace_can_diff,
            selected_workspace_can_edit,
            selected_task_can_fix_ci,
            tool_progress_can_cancel,
            tool_hotkeys,
            status,
        )
    }

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let counter = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "multicode-tui-test-{}-{}-{}",
                std::process::id(),
                unique,
                counter,
            ));
            fs::create_dir_all(&path).expect("test dir should be created");
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

    fn create_fake_git_repo(path: &std::path::Path) {
        fs::create_dir_all(path).expect("repo root should be created");
        let git_dir = path.join(".git");
        fs::create_dir_all(&git_dir).expect("git dir should be created");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD should be created");
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("config should be created");
        fs::create_dir_all(git_dir.join("objects")).expect("objects dir should be created");
    }

    fn create_fake_git_worktree(path: &std::path::Path, common_dir: &std::path::Path) {
        fs::create_dir_all(path).expect("worktree root should be created");
        let git_dir = common_dir
            .join("worktrees")
            .join(path.file_name().expect("worktree should have name"));
        fs::create_dir_all(&git_dir).expect("worktree git dir should be created");
        fs::write(
            path.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .expect("worktree .git file should be created");
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
            .expect("worktree HEAD should be created");
        fs::write(git_dir.join("commondir"), "../..\n")
            .expect("worktree commondir should be created");
    }

    fn snapshot(started: bool, uri: Option<&str>) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            persistent: PersistentWorkspaceSnapshot::default(),
            transient: uri.map(|uri| TransientWorkspaceSnapshot {
                uri: uri.to_string(),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::LinuxSystemdBwrap,
                    id: "unit.service".to_string(),
                    metadata: Default::default(),
                },
            }),
            opencode_client: started.then(|| multicode_lib::OpencodeClientSnapshot {
                client: std::sync::Arc::new(multicode_lib::opencode::client::Client::new(
                    "http://127.0.0.1",
                )),
                events: broadcast::channel(1).0,
            }),
            root_session_id: None,
            root_session_title: None,
            root_session_status: None,
            automation_session_id: None,
            automation_session_status: None,
            automation_agent_state: None,
            automation_status: None,
            automation_scan_request_nonce: 0,
            automation_queue_next_request_nonce: 0,
            active_task_id: None,
            task_states: Default::default(),
            usage_total_tokens: None,
            usage_total_cost: None,
            usage_cpu_percent: None,
            usage_ram_bytes: None,
            oom_kill_count: None,
        }
    }

    fn starting_snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            persistent: PersistentWorkspaceSnapshot::default(),
            transient: Some(TransientWorkspaceSnapshot {
                uri: "http://127.0.0.1".to_string(),
                runtime: RuntimeHandleSnapshot {
                    backend: RuntimeBackend::LinuxSystemdBwrap,
                    id: "unit.service".to_string(),
                    metadata: Default::default(),
                },
            }),
            opencode_client: None,
            root_session_id: None,
            root_session_title: None,
            root_session_status: None,
            automation_session_id: None,
            automation_session_status: None,
            automation_agent_state: None,
            automation_status: None,
            automation_scan_request_nonce: 0,
            automation_queue_next_request_nonce: 0,
            active_task_id: None,
            task_states: Default::default(),
            usage_total_tokens: None,
            usage_total_cost: None,
            usage_cpu_percent: None,
            usage_ram_bytes: None,
            oom_kill_count: None,
        }
    }

    fn no_tool_hotkeys() -> &'static [(String, String)] {
        &[].as_slice()
    }

    fn assign_active_task(snapshot: &mut WorkspaceSnapshot, issue_url: &str) {
        let task_id = "task-42".to_string();
        snapshot
            .persistent
            .tasks
            .push(multicode_lib::WorkspaceTaskPersistentSnapshot::new(
                task_id.clone(),
                issue_url.to_string(),
                multicode_lib::WorkspaceTaskSource::Manual,
            ));
        snapshot.active_task_id = Some(task_id);
    }

    #[test]
    fn restored_selected_row_preserves_selected_task_row() {
        let entries = vec![
            TableEntry::Create,
            TableEntry::Workspace {
                workspace_key: "test123".to_string(),
            },
            TableEntry::Task {
                workspace_key: "test123".to_string(),
                task_id: "task-16".to_string(),
            },
            TableEntry::Task {
                workspace_key: "test123".to_string(),
                task_id: "task-14".to_string(),
            },
        ];

        let selected = restored_selected_row(
            &entries,
            Some(&TableEntry::Task {
                workspace_key: "test123".to_string(),
                task_id: "task-14".to_string(),
            }),
            3,
        );

        assert_eq!(selected, 3);
    }

    #[test]
    fn restored_selected_row_falls_back_to_workspace_when_task_disappears() {
        let entries = vec![
            TableEntry::Create,
            TableEntry::Workspace {
                workspace_key: "test123".to_string(),
            },
            TableEntry::Task {
                workspace_key: "test123".to_string(),
                task_id: "task-16".to_string(),
            },
        ];

        let selected = restored_selected_row(
            &entries,
            Some(&TableEntry::Task {
                workspace_key: "test123".to_string(),
                task_id: "task-14".to_string(),
            }),
            3,
        );

        assert_eq!(selected, 1);
    }

    #[test]
    fn task_issue_reference_uses_repo_and_issue_number_only() {
        let task = multicode_lib::WorkspaceTaskPersistentSnapshot::new(
            "task-8".to_string(),
            "https://github.com/graemerocher/multicode-test/issues/8".to_string(),
            multicode_lib::WorkspaceTaskSource::Scan,
        );

        assert_eq!(crate::task_row_label(&task), "➡️ multicode-test#8");
    }

    #[test]
    fn task_issue_reference_trims_micronaut_prefix_from_repo_name() {
        let task = multicode_lib::WorkspaceTaskPersistentSnapshot::new(
            "task-12".to_string(),
            "https://github.com/micronaut-projects/micronaut-graphql/issues/12".to_string(),
            multicode_lib::WorkspaceTaskSource::Scan,
        );

        assert_eq!(crate::task_row_label(&task), "➡️ graphql#12");
    }

    #[test]
    fn task_issue_link_defaults_to_persistent_issue_url() {
        let task = multicode_lib::WorkspaceTaskPersistentSnapshot::new(
            "task-1".to_string(),
            "https://github.com/graemerocher/multicode-test/issues/1".to_string(),
            multicode_lib::WorkspaceTaskSource::Scan,
        );

        assert_eq!(
            crate::task_issue_link(&task, None),
            "https://github.com/graemerocher/multicode-test/issues/1"
        );
    }

    #[test]
    fn github_link_badge_uses_terminal_issue_or_pr_number() {
        assert_eq!(
            crate::github_link_badge("https://github.com/graemerocher/multicode-test/issues/7"),
            "#7"
        );
        assert_eq!(
            crate::github_link_badge("https://github.com/graemerocher/multicode-test/pull/12"),
            "#12"
        );
    }

    #[test]
    fn task_links_expose_issue_and_pr_for_task_rows() {
        let task = multicode_lib::WorkspaceTaskPersistentSnapshot::new(
            "task-1".to_string(),
            "https://github.com/graemerocher/multicode-test/issues/1".to_string(),
            multicode_lib::WorkspaceTaskSource::Scan,
        );
        let task_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            pr: vec!["https://github.com/graemerocher/multicode-test/pull/8".to_string()],
            ..Default::default()
        };

        let links = crate::task_links(&task, Some(&task_state));
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].kind, WorkspaceLinkKind::Issue);
        assert_eq!(links[1].kind, WorkspaceLinkKind::Pr);
    }

    #[test]
    fn last_user_message_from_codex_session_log_prefers_real_user_events() {
        let contents = r#"{"type":"event_msg","payload":{"type":"user_message","message":"first prompt"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<turn_aborted>\nThe user interrupted the previous turn on purpose.\n</turn_aborted>"}]}}
{"type":"event_msg","payload":{"type":"user_message","message":"create a PR"}}"#;

        assert_eq!(
            last_user_message_from_codex_session_log_contents(contents).as_deref(),
            Some("create a PR")
        );
    }

    #[test]
    fn task_links_fall_back_to_persistent_backing_pr_url() {
        let task = multicode_lib::WorkspaceTaskPersistentSnapshot::new(
            "task-1".to_string(),
            "https://github.com/graemerocher/multicode-test/issues/1".to_string(),
            multicode_lib::WorkspaceTaskSource::Scan,
        )
        .with_backing_pr_url(Some(
            "https://github.com/graemerocher/multicode-test/pull/8".to_string(),
        ));

        let links = crate::task_links(&task, None);
        assert_eq!(links.len(), 2);
        assert_eq!(links[1].kind, WorkspaceLinkKind::Pr);
        assert_eq!(
            links[1].value,
            "https://github.com/graemerocher/multicode-test/pull/8"
        );
    }

    #[test]
    fn issue_type_icon_mappings_use_expected_glyph_kinds() {
        assert_eq!(
            crate::icons::issue_type_icon_kind_and_color(multicode_lib::WorkspaceIssueType::Bug),
            (StatusIconKind::Bug, Color::Red)
        );
        assert_eq!(
            crate::icons::issue_type_icon_kind_and_color(multicode_lib::WorkspaceIssueType::Docs),
            (StatusIconKind::Docs, Color::LightBlue)
        );
        assert_eq!(
            crate::icons::issue_type_icon_kind_and_color(
                multicode_lib::WorkspaceIssueType::DependencyUpgrade
            ),
            (StatusIconKind::DependencyUpgrade, Color::Cyan)
        );
    }

    #[test]
    fn content_width_uses_terminal_display_width() {
        assert_eq!(crate::content_width("A"), 1);
        assert_eq!(crate::content_width(icon_glyph(StatusIconKind::Bug)), 1);
        assert_eq!(crate::content_width(icon_glyph(StatusIconKind::Docs)), 1);
    }

    #[test]
    fn workspace_links_prefer_active_task_issue_and_pr_when_tasks_exist() {
        let mut started = snapshot(true, Some("http://example"));
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        if let Some(task) = started
            .persistent
            .tasks
            .iter_mut()
            .find(|task| task.id == "task-42")
        {
            task.backing_pr_url = Some("https://github.com/example/repo/pull/322".to_string());
        }
        started.task_states.insert(
            "task-42".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                ..Default::default()
            },
        );

        let links = workspace_issue_pr_links(&started);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].kind, WorkspaceLinkKind::Issue);
        assert_eq!(links[0].value, "https://github.com/example/repo/issues/42");
        assert_eq!(links[1].kind, WorkspaceLinkKind::Pr);
        assert_eq!(links[1].value, "https://github.com/example/repo/pull/322");
    }

    #[test]
    fn workspace_links_keep_active_task_pr_when_task_is_in_review() {
        let mut started = snapshot(true, Some("http://example"));
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        if let Some(task) = started
            .persistent
            .tasks
            .iter_mut()
            .find(|task| task.id == "task-42")
        {
            task.backing_pr_url = Some("https://github.com/example/repo/pull/322".to_string());
        }
        started.task_states.insert(
            "task-42".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-42".to_string()),
                session_status: Some(RootSessionStatus::Idle),
                agent_state: Some(AutomationAgentState::Review),
                ..Default::default()
            },
        );

        let links = workspace_issue_pr_links(&started);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].kind, WorkspaceLinkKind::Issue);
        assert_eq!(links[1].kind, WorkspaceLinkKind::Pr);
        assert_eq!(links[1].value, "https://github.com/example/repo/pull/322");
    }

    #[test]
    fn should_request_autonomous_issue_scan_for_assigned_workspace_without_active_issue() {
        let mut stopped = WorkspaceSnapshot::default();
        stopped.persistent.assigned_repository =
            Some("micronaut-projects/micronaut-serialization".to_string());
        assert!(crate::app::should_request_autonomous_issue_scan(
            &stopped, 5
        ));

        stopped
            .persistent
            .tasks
            .push(multicode_lib::WorkspaceTaskPersistentSnapshot::new(
                "task-989".to_string(),
                "https://github.com/micronaut-projects/micronaut-serialization/issues/989"
                    .to_string(),
                multicode_lib::WorkspaceTaskSource::Manual,
            ));
        assert!(crate::app::should_request_autonomous_issue_scan(
            &stopped, 5
        ));
        assert!(!crate::app::should_request_autonomous_issue_scan(
            &stopped, 1
        ));

        stopped.persistent.archived = true;
        assert!(!crate::app::should_request_autonomous_issue_scan(
            &stopped, 5
        ));

        let unassigned = WorkspaceSnapshot::default();
        assert!(!crate::app::should_request_autonomous_issue_scan(
            &unassigned,
            5
        ));
    }

    #[test]
    fn auto_resume_after_attach_only_resumes_active_autonomous_work() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.persistent.assigned_repository = Some("example/repo".to_string());
        assign_active_task(&mut snapshot, "https://github.com/example/repo/issues/42");
        snapshot.automation_agent_state = Some(AutomationAgentState::Working);

        assert!(should_auto_resume_autonomous_codex_after_attach(&snapshot));

        snapshot.automation_agent_state = Some(AutomationAgentState::Review);
        assert!(!should_auto_resume_autonomous_codex_after_attach(&snapshot));

        snapshot.automation_agent_state = Some(AutomationAgentState::Question);
        assert!(!should_auto_resume_autonomous_codex_after_attach(&snapshot));

        snapshot.automation_agent_state = None;
        snapshot.root_session_status = Some(RootSessionStatus::Busy);
        assert!(should_auto_resume_autonomous_codex_after_attach(&snapshot));

        snapshot.root_session_status = Some(RootSessionStatus::Idle);
        assert!(!should_auto_resume_autonomous_codex_after_attach(&snapshot));

        snapshot.active_task_id = None;
        snapshot.persistent.tasks.clear();
        snapshot.root_session_status = Some(RootSessionStatus::Busy);
        assert!(!should_auto_resume_autonomous_codex_after_attach(&snapshot));
    }

    #[test]
    fn task_server_label_prefers_idle_session_status_over_stale_working_agent_state() {
        let task_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-39".to_string()),
            session_status: Some(RootSessionStatus::Idle),
            agent_state: Some(AutomationAgentState::Working),
            ..Default::default()
        };
        let task = multicode_lib::WorkspaceTaskPersistentSnapshot::new(
            "task-39".to_string(),
            "https://github.com/graemerocher/multicode-test/issues/39".to_string(),
            multicode_lib::WorkspaceTaskSource::Scan,
        );

        assert_eq!(crate::task_server_label(Some(&task_state)), "Idle");
        assert_eq!(
            crate::task_description(&task, Some(&task_state)),
            "Review multicode-test#39"
        );
    }

    #[test]
    fn task_description_prefers_pr_created_for_review_task_with_persisted_pr() {
        let task = multicode_lib::WorkspaceTaskPersistentSnapshot::new(
            "task-39".to_string(),
            "https://github.com/graemerocher/multicode-test/issues/39".to_string(),
            multicode_lib::WorkspaceTaskSource::Scan,
        )
        .with_backing_pr_url(Some(
            "https://github.com/graemerocher/multicode-test/pull/338".to_string(),
        ));
        let task_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-39".to_string()),
            session_status: Some(RootSessionStatus::Idle),
            agent_state: Some(AutomationAgentState::Review),
            status: Some("Review multicode-test#39".to_string()),
            ..Default::default()
        };

        assert_eq!(
            crate::task_description(&task, Some(&task_state)),
            "PR created #338"
        );
    }

    #[test]
    fn task_auto_resume_after_attach_only_resumes_when_task_is_still_working() {
        let review_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-4".to_string()),
            session_status: Some(RootSessionStatus::Idle),
            agent_state: Some(AutomationAgentState::Working),
            ..Default::default()
        };
        assert!(!should_auto_resume_task_codex_after_attach(
            Some(&review_state),
            Some("thread-4"),
            Some(AutomationAgentState::Review)
        ));

        let busy_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-4".to_string()),
            session_status: Some(RootSessionStatus::Busy),
            agent_state: Some(AutomationAgentState::Working),
            ..Default::default()
        };
        assert!(!should_auto_resume_task_codex_after_attach(
            Some(&busy_state),
            Some("thread-4"),
            Some(AutomationAgentState::Working)
        ));
    }

    #[test]
    fn task_auto_resume_after_attach_resumes_busy_task_when_session_is_missing() {
        let busy_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-4".to_string()),
            session_status: Some(RootSessionStatus::Busy),
            agent_state: Some(AutomationAgentState::Working),
            ..Default::default()
        };

        assert!(should_auto_resume_task_codex_after_attach(
            Some(&busy_state),
            None,
            Some(AutomationAgentState::Working)
        ));
    }

    #[test]
    fn task_auto_resume_after_attach_resumes_when_working_task_loses_session() {
        let lost_session_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: None,
            session_status: None,
            agent_state: None,
            ..Default::default()
        };

        assert!(should_auto_resume_task_codex_after_attach(
            Some(&lost_session_state),
            Some("thread-4"),
            Some(AutomationAgentState::Working)
        ));
    }

    #[test]
    fn task_auto_resume_after_attach_resumes_stale_working_task() {
        let stale_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-4".to_string()),
            session_status: Some(RootSessionStatus::Idle),
            agent_state: Some(AutomationAgentState::Stale),
            ..Default::default()
        };

        assert!(should_auto_resume_task_codex_after_attach(
            Some(&stale_state),
            Some("thread-4"),
            Some(AutomationAgentState::Working)
        ));
    }

    #[test]
    fn direct_task_attach_restarts_background_codex_session_after_detach() {
        assert!(should_restart_task_codex_after_attach(
            Some("thread-4"),
            false
        ));
    }

    #[test]
    fn fresh_task_attach_keeps_existing_background_codex_session() {
        assert!(!should_restart_task_codex_after_attach(
            Some("thread-4"),
            true
        ));
        assert!(!should_restart_task_codex_after_attach(None, true));
    }

    #[test]
    fn detached_task_resume_queues_when_another_task_owns_vm() {
        let mut snapshot = multicode_lib::WorkspaceSnapshot::default();
        snapshot.active_task_id = Some("task-1".to_string());
        snapshot.root_session_status = Some(RootSessionStatus::Busy);
        snapshot.task_states.insert(
            "task-1".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-1".to_string()),
                session_status: Some(RootSessionStatus::Busy),
                agent_state: Some(AutomationAgentState::Working),
                ..Default::default()
            },
        );
        snapshot.task_states.insert(
            "task-2".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-2".to_string()),
                session_status: Some(RootSessionStatus::Busy),
                agent_state: Some(AutomationAgentState::Working),
                ..Default::default()
            },
        );

        assert!(should_queue_task_codex_resume_until_vm_available(
            &snapshot, "task-2"
        ));
    }

    #[test]
    fn detached_task_resume_does_not_queue_when_active_task_can_yield_vm() {
        let mut snapshot = multicode_lib::WorkspaceSnapshot::default();
        snapshot.active_task_id = Some("task-1".to_string());
        snapshot.root_session_status = Some(RootSessionStatus::Idle);
        snapshot.task_states.insert(
            "task-1".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-1".to_string()),
                session_status: Some(RootSessionStatus::Idle),
                agent_state: Some(AutomationAgentState::Review),
                ..Default::default()
            },
        );
        snapshot.task_states.insert(
            "task-2".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-2".to_string()),
                session_status: Some(RootSessionStatus::Busy),
                agent_state: Some(AutomationAgentState::Working),
                ..Default::default()
            },
        );

        assert!(!should_queue_task_codex_resume_until_vm_available(
            &snapshot, "task-2"
        ));
    }

    #[test]
    fn detached_task_resume_does_not_queue_when_it_already_owns_vm() {
        let mut snapshot = multicode_lib::WorkspaceSnapshot::default();
        snapshot.active_task_id = Some("task-2".to_string());
        snapshot.root_session_status = Some(RootSessionStatus::Busy);
        snapshot.task_states.insert(
            "task-2".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-2".to_string()),
                session_status: Some(RootSessionStatus::Busy),
                agent_state: Some(AutomationAgentState::Working),
                ..Default::default()
            },
        );

        assert!(!should_queue_task_codex_resume_until_vm_available(
            &snapshot, "task-2"
        ));
    }

    #[test]
    fn codex_session_turn_metrics_count_started_and_completed_turns() {
        let metrics = count_codex_session_turn_metrics(
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
        );

        assert_eq!(metrics.started, 2);
        assert_eq!(metrics.completed, 1);
    }

    #[test]
    fn incomplete_attached_codex_turn_resumes_when_thread_is_idle() {
        assert!(should_resume_codex_task_after_incomplete_attached_turn(
            Some(CodexSessionTurnMetrics {
                started: 3,
                completed: 3,
                aborted: 0,
            }),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 3,
                aborted: 0,
            }),
            Some(&CodexThreadStatus::Idle),
        ));
    }

    #[test]
    fn completed_attached_codex_turn_does_not_resume() {
        assert!(!should_resume_codex_task_after_incomplete_attached_turn(
            Some(CodexSessionTurnMetrics {
                started: 3,
                completed: 3,
                aborted: 0,
            }),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 4,
                aborted: 0,
            }),
            Some(&CodexThreadStatus::Idle),
        ));
    }

    #[test]
    fn active_attached_codex_turn_does_not_resume() {
        assert!(!should_resume_codex_task_after_incomplete_attached_turn(
            Some(CodexSessionTurnMetrics {
                started: 3,
                completed: 3,
                aborted: 0,
            }),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 3,
                aborted: 0,
            }),
            Some(&CodexThreadStatus::Active {
                active_flags: Vec::new(),
            }),
        ));
    }

    #[test]
    fn incomplete_attached_codex_turn_resumes_when_thread_status_is_unavailable() {
        assert!(should_resume_codex_task_after_incomplete_attached_turn(
            Some(CodexSessionTurnMetrics {
                started: 3,
                completed: 1,
                aborted: 0,
            }),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 1,
                aborted: 0,
            }),
            None,
        ));
    }

    #[test]
    fn aborted_attached_codex_turn_resumes_even_if_started_count_did_not_advance() {
        assert!(should_resume_codex_task_after_incomplete_attached_turn(
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 1,
                aborted: 0,
            }),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 1,
                aborted: 1,
            }),
            Some(&CodexThreadStatus::NotLoaded),
        ));
    }

    #[test]
    fn workspace_attach_target_requires_started_state() {
        let err = workspace_attach_target(&snapshot(false, Some("http://example")))
            .expect_err("non-started workspace should not provide attach URI");
        assert!(
            err.to_string()
                .contains("workspace must be in Started state before attaching")
        );
    }

    #[test]
    fn workspace_attach_target_requires_transient_uri() {
        let err = workspace_attach_target(&WorkspaceSnapshot::default())
            .expect_err("stopped workspace without transient snapshot should fail");
        assert!(
            err.to_string()
                .contains("workspace must be in Started state before attaching")
        );
    }

    #[test]
    fn workspace_attach_target_extracts_credentials_and_sanitizes_uri() {
        let started = snapshot(true, Some("http://opencode:secret@127.0.0.1:3000/"));
        let target = workspace_attach_target(&started)
            .expect("started workspace should expose attach target with auth");
        assert_eq!(
            target,
            AttachTarget::Opencode {
                uri: "http://127.0.0.1:3000/".to_string(),
                username: "opencode".to_string(),
                password: "secret".to_string(),
                session_id: None,
            }
        );
    }

    #[test]
    fn workspace_attach_target_includes_latest_root_session_id() {
        let mut started = snapshot(true, Some("http://opencode:secret@127.0.0.1:3000/"));
        started.root_session_id = Some("ses-root-latest".to_string());

        let target = workspace_attach_target(&started)
            .expect("started workspace should expose attach target with latest root session id");

        assert_eq!(
            target,
            AttachTarget::Opencode {
                uri: "http://127.0.0.1:3000/".to_string(),
                username: "opencode".to_string(),
                password: "secret".to_string(),
                session_id: Some("ses-root-latest".to_string()),
            }
        );
    }

    #[test]
    fn attach_cli_args_appends_session_when_present() {
        let target = AttachTarget::Opencode {
            uri: "http://127.0.0.1:3000/".to_string(),
            username: "opencode".to_string(),
            password: "secret".to_string(),
            session_id: Some("ses-root-latest".to_string()),
        };

        assert_eq!(
            attach_cli_args("opencode", &target),
            vec![
                "opencode".to_string(),
                "attach".to_string(),
                "--session".to_string(),
                "ses-root-latest".to_string(),
                "http://127.0.0.1:3000/".to_string(),
            ]
        );
    }

    #[test]
    fn attach_cli_args_omits_session_when_unavailable() {
        let target = AttachTarget::Opencode {
            uri: "http://127.0.0.1:3000/".to_string(),
            username: "opencode".to_string(),
            password: "secret".to_string(),
            session_id: None,
        };

        assert_eq!(
            attach_cli_args("opencode", &target),
            vec![
                "opencode".to_string(),
                "attach".to_string(),
                "http://127.0.0.1:3000/".to_string(),
            ]
        );
    }

    #[test]
    fn workspace_attach_target_uses_codex_variant_for_websocket_uri() {
        let mut started = snapshot(false, Some("ws://127.0.0.1:3456"));
        started.root_session_id = Some("thread-123".to_string());

        let target = workspace_attach_target(&started)
            .expect("codex workspace should expose websocket attach target");

        assert_eq!(
            target,
            AttachTarget::Codex {
                uri: "ws://127.0.0.1:3456/".to_string(),
                thread_id: Some("thread-123".to_string()),
            }
        );
    }

    #[test]
    fn task_attach_target_uses_task_session_for_opencode() {
        let started = snapshot(true, Some("http://opencode:secret@127.0.0.1:3000/"));
        let task_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("ses-task-1".to_string()),
            ..Default::default()
        };

        let target = task_attach_target(&started, &task_state)
            .expect("task attach target should use task session id");

        assert_eq!(
            target,
            AttachTarget::Opencode {
                uri: "http://127.0.0.1:3000/".to_string(),
                username: "opencode".to_string(),
                password: "secret".to_string(),
                session_id: Some("ses-task-1".to_string()),
            }
        );
    }

    #[test]
    fn task_attach_target_uses_task_thread_for_codex() {
        let mut started = snapshot(false, Some("ws://127.0.0.1:3456"));
        started.root_session_id = Some("thread-root".to_string());
        let task_state = multicode_lib::WorkspaceTaskRuntimeSnapshot {
            session_id: Some("thread-task-1".to_string()),
            ..Default::default()
        };

        let target = task_attach_target(&started, &task_state)
            .expect("task attach target should use task thread id");

        assert_eq!(
            target,
            AttachTarget::Codex {
                uri: "ws://127.0.0.1:3456/".to_string(),
                thread_id: Some("thread-task-1".to_string()),
            }
        );
    }

    #[test]
    fn snapshot_attach_target_for_selection_falls_back_to_workspace_attach_when_paused_opencode_task_has_no_session()
     {
        let mut started = snapshot(true, Some("http://opencode:secret@127.0.0.1:3000/"));
        started.root_session_id = Some("ses-root-1".to_string());
        started.persistent.automation_paused = true;
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");

        let target = snapshot_attach_target_for_selection(&started, Some("task-42"))
            .expect("paused task selection should fall back to workspace attach");

        assert_eq!(
            target,
            AttachTarget::Opencode {
                uri: "http://127.0.0.1:3000/".to_string(),
                username: "opencode".to_string(),
                password: "secret".to_string(),
                session_id: Some("ses-root-1".to_string()),
            }
        );
    }

    #[test]
    fn snapshot_attach_target_for_selection_uses_last_codex_thread_when_paused_task_has_no_session()
    {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        started.root_session_id = Some("thread-root".to_string());
        started.persistent.automation_paused = true;
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");

        let target = snapshot_attach_target_for_selection(&started, Some("task-42"))
            .expect("paused codex task selection should attach via last task thread");

        assert_eq!(
            target,
            AttachTarget::Codex {
                uri: "ws://127.0.0.1:3456/".to_string(),
                thread_id: None,
            }
        );
    }

    #[test]
    fn snapshot_attach_target_for_selection_uses_last_codex_thread_when_task_is_stale() {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        started.root_session_id = Some("thread-root".to_string());
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.task_states.insert(
            "task-42".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-stale".to_string()),
                agent_state: Some(AutomationAgentState::Stale),
                ..Default::default()
            },
        );

        let target = snapshot_attach_target_for_selection(&started, Some("task-42"))
            .expect("stale codex task should attach via last thread");

        assert_eq!(
            target,
            AttachTarget::Codex {
                uri: "ws://127.0.0.1:3456/".to_string(),
                thread_id: None,
            }
        );
    }

    #[test]
    fn snapshot_attach_target_for_selection_uses_last_codex_thread_when_task_is_not_loaded() {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        started.root_session_id = Some("thread-root".to_string());
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.task_states.insert(
            "task-42".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-missing".to_string()),
                agent_state: Some(AutomationAgentState::Review),
                status: Some("NotLoaded".to_string()),
                ..Default::default()
            },
        );

        let target = snapshot_attach_target_for_selection(&started, Some("task-42"))
            .expect("not-loaded codex task should attach via last thread");

        assert_eq!(
            target,
            AttachTarget::Codex {
                uri: "ws://127.0.0.1:3456/".to_string(),
                thread_id: None,
            }
        );
    }

    #[test]
    fn should_retry_codex_task_attach_with_last_thread_for_not_loaded_task() {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.task_states.insert(
            "task-42".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-stale".to_string()),
                agent_state: Some(AutomationAgentState::Review),
                status: Some("NotLoaded".to_string()),
                ..Default::default()
            },
        );

        let target = should_retry_codex_task_attach_with_last_thread(
            AgentProvider::Codex,
            Some(&started),
            "ws",
            Some("ws"),
            Some("task-42"),
            Some("thread-stale"),
        );

        assert_eq!(
            target,
            Some(AttachTarget::Codex {
                uri: "ws://127.0.0.1:3456/".to_string(),
                thread_id: None,
            })
        );
    }

    #[test]
    fn should_not_retry_codex_task_attach_without_explicit_thread() {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.task_states.insert(
            "task-42".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                status: Some("NotLoaded".to_string()),
                ..Default::default()
            },
        );

        let target = should_retry_codex_task_attach_with_last_thread(
            AgentProvider::Codex,
            Some(&started),
            "ws",
            Some("ws"),
            Some("task-42"),
            None,
        );

        assert_eq!(target, None);
    }

    #[test]
    fn should_start_fresh_codex_task_session_after_failed_attach_for_aborted_only_thread() {
        assert!(should_start_fresh_codex_task_session_after_failed_attach(
            AgentProvider::Codex,
            Some("ws"),
            "ws",
            Some("task-42"),
            Some("thread-42"),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 0,
                aborted: 4,
            }),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 0,
                aborted: 4,
            }),
            Some(&CodexThreadStatus::NotLoaded),
        ));
    }

    #[test]
    fn should_not_start_fresh_codex_task_session_when_attach_changed_thread_metrics() {
        assert!(!should_start_fresh_codex_task_session_after_failed_attach(
            AgentProvider::Codex,
            Some("ws"),
            "ws",
            Some("task-42"),
            Some("thread-42"),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 0,
                aborted: 1,
            }),
            Some(CodexSessionTurnMetrics {
                started: 5,
                completed: 0,
                aborted: 1,
            }),
            Some(&CodexThreadStatus::NotLoaded),
        ));
    }

    #[test]
    fn should_not_start_fresh_codex_task_session_when_thread_is_still_idle() {
        assert!(!should_start_fresh_codex_task_session_after_failed_attach(
            AgentProvider::Codex,
            Some("ws"),
            "ws",
            Some("task-42"),
            Some("thread-42"),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 0,
                aborted: 4,
            }),
            Some(CodexSessionTurnMetrics {
                started: 4,
                completed: 0,
                aborted: 4,
            }),
            Some(&CodexThreadStatus::Idle),
        ));
    }

    #[test]
    fn working_codex_task_attach_target_uses_existing_task_thread() {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.task_states.insert(
            "task-42".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("thread-42".to_string()),
                session_status: Some(RootSessionStatus::Busy),
                agent_state: Some(AutomationAgentState::Working),
                ..Default::default()
            },
        );

        let target = working_codex_task_attach_target(
            &started,
            Some("task-42"),
            Some("/tmp/task-42".to_string()),
        )
        .expect("working codex task attach target should be computed");

        assert_eq!(
            target,
            Some(AttachTarget::Codex {
                uri: "ws://127.0.0.1:3456/".to_string(),
                thread_id: Some("thread-42".to_string()),
            })
        );
    }

    #[test]
    fn snapshot_attach_target_for_selection_prefers_task_attach_when_task_session_exists() {
        let mut started = snapshot(true, Some("http://opencode:secret@127.0.0.1:3000/"));
        started.root_session_id = Some("ses-root-1".to_string());
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.task_states.insert(
            "task-42".to_string(),
            multicode_lib::WorkspaceTaskRuntimeSnapshot {
                session_id: Some("ses-task-42".to_string()),
                ..Default::default()
            },
        );

        let target = snapshot_attach_target_for_selection(&started, Some("task-42"))
            .expect("task session should still be preferred");

        assert_eq!(
            target,
            AttachTarget::Opencode {
                uri: "http://127.0.0.1:3000/".to_string(),
                username: "opencode".to_string(),
                password: "secret".to_string(),
                session_id: Some("ses-task-42".to_string()),
            }
        );
    }

    #[test]
    fn attach_cli_args_use_codex_resume_for_codex_target() {
        let target = AttachTarget::Codex {
            uri: "ws://127.0.0.1:3456".to_string(),
            thread_id: Some("thread-123".to_string()),
        };

        assert_eq!(
            attach_cli_args("codex", &target),
            vec![
                "codex".to_string(),
                "resume".to_string(),
                "--remote".to_string(),
                "ws://127.0.0.1:3456".to_string(),
                "thread-123".to_string(),
            ]
        );
    }

    #[test]
    fn attach_cli_args_use_last_for_codex_when_thread_is_unavailable() {
        let target = AttachTarget::Codex {
            uri: "ws://127.0.0.1:3456".to_string(),
            thread_id: None,
        };

        assert_eq!(
            attach_cli_args("codex", &target),
            vec![
                "codex".to_string(),
                "resume".to_string(),
                "--remote".to_string(),
                "ws://127.0.0.1:3456".to_string(),
                "--last".to_string(),
            ]
        );
    }

    #[test]
    fn attach_cli_args_start_fresh_remote_codex_session_in_task_checkout() {
        let target = AttachTarget::CodexNew {
            uri: "ws://127.0.0.1:3456".to_string(),
            cwd: Some("/tmp/task".to_string()),
            prompt: Some("Continue work on this task.".to_string()),
        };

        assert_eq!(
            attach_cli_args("codex", &target),
            vec![
                "codex".to_string(),
                "--remote".to_string(),
                "ws://127.0.0.1:3456".to_string(),
                "-C".to_string(),
                "/tmp/task".to_string(),
                "Continue work on this task.".to_string(),
            ]
        );
    }

    #[test]
    fn tmux_session_command_restores_original_term_inside_session() {
        let command = vec!["opencode".to_string(), "attach".to_string()];

        assert_eq!(
            tmux_session_command(command, Some("screen-256color")),
            vec![
                "env".to_string(),
                "TERM=screen-256color".to_string(),
                "opencode".to_string(),
                "attach".to_string(),
            ]
        );
    }

    #[test]
    fn tmux_session_command_skips_term_override_when_unset() {
        let command = vec!["opencode".to_string(), "attach".to_string()];

        assert_eq!(
            tmux_session_command(command, None),
            vec![
                "env".to_string(),
                "opencode".to_string(),
                "attach".to_string(),
            ]
        );
    }

    #[test]
    fn command_exists_detects_executable_files_on_path() {
        let root = TestDir::new();
        let bin_dir = root.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir should exist");
        let tool_path = bin_dir.join("multicode-test-tool");
        fs::write(&tool_path, "#!/bin/sh\nexit 0\n").expect("tool should be written");
        let mut perms = fs::metadata(&tool_path)
            .expect("tool metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tool_path, perms).expect("tool should be executable");

        let old_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", bin_dir.as_os_str());
        }

        assert!(command_exists("multicode-test-tool"));
        assert!(!command_exists("missing-tool"));

        if let Some(path) = old_path {
            unsafe {
                std::env::set_var("PATH", path);
            }
        } else {
            unsafe {
                std::env::remove_var("PATH");
            }
        }
    }

    #[test]
    fn compare_tool_is_available_uses_configured_command_path() {
        let root = TestDir::new();
        let tool_path = root.path().join("idea");
        fs::write(&tool_path, "#!/bin/sh\nexit 0\n").expect("tool should be written");
        let mut perms = fs::metadata(&tool_path)
            .expect("tool metadata should exist")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tool_path, perms).expect("tool should be executable");

        assert!(compare_tool_is_available(&CompareConfig {
            tool: CompareTool::Intellij,
            command: Some(tool_path.to_string_lossy().into_owned()),
        }));
    }

    #[test]
    fn compare_tool_name_labels_supported_tools() {
        assert_eq!(compare_tool_name(CompareTool::Vscode), "VS Code");
        assert_eq!(compare_tool_name(CompareTool::Intellij), "IntelliJ IDEA");
    }

    #[test]
    fn repository_diff_shell_command_keeps_pager_open_and_shows_status() {
        let command = repository_diff_shell_command();
        assert!(command.contains("git status --short"));
        assert!(command.contains("git --no-pager diff"));
        assert!(command.contains("less -R -X"));
        assert!(!command.contains("less -R -F -X"));
    }

    #[test]
    fn shell_command_in_repo_does_not_prefix_exec() {
        let command = shell_command_in_repo("/tmp/repo", "tmp=1; echo hi");
        assert!(command.contains("cd -- /tmp/repo && "));
        assert!(!command.contains("&& exec "));
    }

    #[test]
    fn tui_cli_args_accept_optional_relay_socket() {
        let parsed = crate::CliArgs::try_parse_from([
            "multicode-tui",
            "config.toml",
            "--relay-socket",
            "/tmp/multicode-relay.sock",
        ])
        .expect("cli args should parse");
        assert_eq!(parsed.config_path, PathBuf::from("config.toml"));
        assert_eq!(
            parsed.relay_socket,
            Some(PathBuf::from("/tmp/multicode-relay.sock"))
        );
    }

    #[test]
    fn tui_cli_args_accept_hidden_recency_scan_flags() {
        let parsed = crate::CliArgs::try_parse_from([
            "multicode-tui",
            "config.toml",
            "--recency-scan-path",
            "/tmp/worktree",
            "--recency-scan-is-dir",
            "--recency-scan-exclude",
            ".multicode/remote",
            "--recency-scan-exclude",
            "node_modules",
        ])
        .expect("hidden recency scan args should parse");
        assert_eq!(parsed.config_path, PathBuf::from("config.toml"));
        assert_eq!(
            parsed.recency_scan_path,
            Some(PathBuf::from("/tmp/worktree"))
        );
        assert!(parsed.recency_scan_is_dir);
        assert_eq!(
            parsed.recency_scan_exclude,
            vec![".multicode/remote".to_string(), "node_modules".to_string()]
        );
    }

    #[test]
    fn workspace_attach_target_requires_username_and_password() {
        let missing_username = snapshot(true, Some("http://:secret@127.0.0.1/"));
        let err = workspace_attach_target(&missing_username)
            .expect_err("URI without username should fail");
        assert!(
            err.to_string()
                .contains("workspace attach URI is missing username credentials")
        );

        let missing_password = snapshot(true, Some("http://opencode@127.0.0.1/"));
        let err = workspace_attach_target(&missing_password)
            .expect_err("URI without password should fail");
        assert!(
            err.to_string()
                .contains("workspace attach URI is missing password credentials")
        );
    }

    #[test]
    fn build_handler_command_replaces_placeholder_argument() {
        let (program, args) = build_handler_command(
            "/usr/bin/firefox {}",
            multicode_lib::HandlerArgumentMode::Argument,
            "https://example.com",
        )
        .expect("handler command should parse");
        assert_eq!(program, "/usr/bin/firefox");
        assert_eq!(args, vec!["https://example.com".to_string()]);
    }

    #[test]
    fn build_handler_command_accepts_review_without_placeholder() {
        let (program, args) = build_handler_command(
            "/usr/bin/smerge",
            multicode_lib::HandlerArgumentMode::Chdir,
            "/tmp/repo",
        )
        .expect("review handler without placeholder should parse");
        assert_eq!(program, "/usr/bin/smerge");
        assert!(args.is_empty());
    }

    #[test]
    fn validate_workspace_link_target_accepts_repo_under_workspace_directory() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_dir = root.path().join("agent-work");
            fs::create_dir_all(&workspace_dir).expect("workspace dir should be created");
            let repo_dir = workspace_dir.join("micronaut-core");
            fs::create_dir_all(&repo_dir).expect("repo dir should be created");
            fs::create_dir(repo_dir.join(".git")).expect(".git folder should be created");

            let link = WorkspaceLink {
                kind: WorkspaceLinkKind::Review,
                value: repo_dir.to_string_lossy().into_owned(),
                source: WorkspaceLinkSource::AgentProvided,
            };
            let validated = validate_workspace_link_target(&link, &workspace_dir)
                .await
                .expect("repo directory under workspace root should be accepted");
            assert!(PathBuf::from(validated).is_dir());
        });
    }

    #[test]
    fn validate_workspace_link_target_rejects_repo_outside_workspace_directory() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_dir = root.path().join("agent-work");
            let outside_dir = root.path().join("outside-repo");
            fs::create_dir_all(&workspace_dir).expect("workspace dir should be created");
            fs::create_dir_all(&outside_dir).expect("outside dir should be created");

            let link = WorkspaceLink {
                kind: WorkspaceLinkKind::Review,
                value: outside_dir.to_string_lossy().into_owned(),
                source: WorkspaceLinkSource::AgentProvided,
            };
            let err = validate_workspace_link_target(&link, &workspace_dir)
                .await
                .expect_err("repo outside workspace root should be rejected");
            let message = err.to_string();
            assert!(
                message.contains("outside workspace directory")
                    || message.contains("must contain a '.git' entry")
            );
        });
    }

    #[test]
    fn validate_workspace_link_target_rejects_repo_without_git_entry() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let workspace_dir = root.path().join("agent-work");
            let repo_dir = workspace_dir.join("core12299").join("micronaut-core");
            fs::create_dir_all(&repo_dir).expect("repo dir should be created");

            let link = WorkspaceLink {
                kind: WorkspaceLinkKind::Review,
                value: repo_dir.to_string_lossy().into_owned(),
                source: WorkspaceLinkSource::AgentProvided,
            };
            let err = validate_workspace_link_target(&link, &workspace_dir)
                .await
                .expect_err("repo without .git entry should be rejected");
            assert!(err.to_string().contains("must contain a '.git' entry"));
        });
    }

    #[test]
    fn validate_workspace_link_target_rejects_non_https_urls() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime should build");

        runtime.block_on(async {
            let root = TestDir::new();
            let link = WorkspaceLink {
                kind: WorkspaceLinkKind::Issue,
                value: "http://example.com/issue/1".to_string(),
                source: WorkspaceLinkSource::AgentProvided,
            };
            let err = validate_workspace_link_target(&link, root.path())
                .await
                .expect_err("non-https URL should be rejected");
            assert!(err.to_string().contains("must use https"));
        });
    }

    #[test]
    fn workspace_links_collect_review_issue_and_pr_entries() {
        let mut started = snapshot(true, Some("http://example"));
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.persistent.agent_provided.repo = vec!["/tmp/repo-a".to_string()];
        started.persistent.agent_provided.issue = vec!["https://example.com/issue/1".to_string()];
        started.persistent.agent_provided.pr = vec!["https://example.com/pull/2".to_string()];

        let links = workspace_links(&started);
        assert_eq!(
            links,
            vec![
                WorkspaceLink {
                    kind: WorkspaceLinkKind::Issue,
                    value: "https://github.com/example/repo/issues/42".to_string(),
                    source: WorkspaceLinkSource::Automation,
                },
                WorkspaceLink {
                    kind: WorkspaceLinkKind::Review,
                    value: "/tmp/repo-a".to_string(),
                    source: WorkspaceLinkSource::AgentProvided,
                },
                WorkspaceLink {
                    kind: WorkspaceLinkKind::Issue,
                    value: "https://example.com/issue/1".to_string(),
                    source: WorkspaceLinkSource::AgentProvided,
                },
                WorkspaceLink {
                    kind: WorkspaceLinkKind::Pr,
                    value: "https://example.com/pull/2".to_string(),
                    source: WorkspaceLinkSource::AgentProvided,
                },
            ]
        );
    }

    #[test]
    fn workspace_links_put_custom_issue_and_pr_before_automatic_links() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.custom_links.issue =
            vec!["https://example.com/custom-issue/1".to_string()];
        started.persistent.custom_links.pr = vec!["https://example.com/custom-pr/2".to_string()];
        started.persistent.agent_provided.issue = vec!["https://example.com/issue/3".to_string()];
        started.persistent.agent_provided.pr = vec!["https://example.com/pull/4".to_string()];

        let links = workspace_links(&started);
        assert_eq!(links[0].kind, WorkspaceLinkKind::Issue);
        assert_eq!(links[0].source, WorkspaceLinkSource::Custom);
        assert_eq!(links[1].kind, WorkspaceLinkKind::Issue);
        assert_eq!(links[1].source, WorkspaceLinkSource::AgentProvided);
        assert_eq!(links[2].kind, WorkspaceLinkKind::Pr);
        assert_eq!(links[2].source, WorkspaceLinkSource::Custom);
        assert_eq!(links[3].kind, WorkspaceLinkKind::Pr);
        assert_eq!(links[3].source, WorkspaceLinkSource::AgentProvided);
    }

    #[test]
    fn next_link_selection_right_cycles_back_to_row_after_last_link() {
        assert_eq!(next_link_selection_right(None, 0), None);
        assert_eq!(next_link_selection_right(None, 3), Some(0));
        assert_eq!(next_link_selection_right(Some(0), 3), Some(1));
        assert_eq!(next_link_selection_right(Some(1), 3), Some(2));
        assert_eq!(next_link_selection_right(Some(2), 3), None);
    }

    #[test]
    fn next_non_stopped_row_skips_stopped_workspaces_in_both_directions() {
        let ordered_keys = vec![
            "alpha".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
            "delta".to_string(),
        ];
        let snapshots = HashMap::from([
            ("alpha".to_string(), snapshot(false, None)),
            ("beta".to_string(), starting_snapshot()),
            ("gamma".to_string(), snapshot(false, None)),
            ("delta".to_string(), snapshot(true, Some("http://example"))),
        ]);

        assert_eq!(
            next_non_stopped_row(0, &ordered_keys, &snapshots, 1),
            Some(2)
        );
        assert_eq!(
            next_non_stopped_row(2, &ordered_keys, &snapshots, 1),
            Some(4)
        );
        assert_eq!(
            next_non_stopped_row(4, &ordered_keys, &snapshots, -1),
            Some(2)
        );
    }

    #[test]
    fn next_non_stopped_row_returns_none_when_no_match_exists() {
        let ordered_keys = vec!["alpha".to_string(), "beta".to_string()];
        let snapshots = HashMap::from([
            ("alpha".to_string(), snapshot(false, None)),
            ("beta".to_string(), snapshot(false, None)),
        ]);

        assert_eq!(next_non_stopped_row(0, &ordered_keys, &snapshots, 1), None);
        assert_eq!(next_non_stopped_row(2, &ordered_keys, &snapshots, -1), None);
    }

    #[test]
    fn visible_workspace_links_only_include_validated_entries() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.agent_provided.repo = vec!["/tmp/repo-a".to_string()];
        started.persistent.agent_provided.issue = vec!["https://example.com/issue/1".to_string()];
        started.persistent.agent_provided.pr = vec!["https://example.com/pull/2".to_string()];

        let all_links = workspace_links(&started);
        let mut validations = HashMap::new();
        validations.insert(
            all_links[0].clone(),
            WorkspaceLinkValidationResult::Valid("/tmp/repo-a".to_string()),
        );
        validations.insert(
            all_links[1].clone(),
            WorkspaceLinkValidationResult::Invalid("rejected".to_string()),
        );

        let visible = visible_workspace_links(&started, &validations);
        assert_eq!(visible[0], all_links[0].clone());
        assert_eq!(visible[1].kind, WorkspaceLinkKind::Issue);
        assert!(visible[1].value.is_empty());
        assert_eq!(visible[2].kind, WorkspaceLinkKind::Pr);
        assert!(visible[2].value.is_empty());
    }

    #[test]
    fn compare_target_path_prefers_validated_review_repo() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.agent_provided.repo = vec!["/tmp/repo-a".to_string()];
        let workspace = TestDir::new();

        let all_links = workspace_links(&started);
        let mut validations = HashMap::new();
        validations.insert(
            all_links[0].clone(),
            WorkspaceLinkValidationResult::Valid("/tmp/repo-a".to_string()),
        );

        assert_eq!(
            compare_target_path(&started, &validations, workspace.path()),
            Some(PathBuf::from("/tmp/repo-a"))
        );
    }

    #[test]
    fn compare_target_path_falls_back_to_issue_worktree() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository =
            Some("micronaut-projects/micronaut-serialization".to_string());
        started.persistent.automation_issue = Some(
            "https://github.com/micronaut-projects/micronaut-serialization/issues/921".to_string(),
        );
        let workspace = TestDir::new();
        let repo_path = workspace.path().join("work/micronaut-serialization-921");
        fs::create_dir_all(&repo_path).expect("issue worktree repo should be created");
        create_fake_git_repo(&repo_path);

        assert_eq!(
            compare_target_path(&started, &HashMap::new(), workspace.path()),
            Some(workspace.path().join("work/micronaut-serialization-921"))
        );
    }

    #[test]
    fn compare_target_path_falls_back_to_assigned_repository_root() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository =
            Some("micronaut-projects/micronaut-serialization".to_string());
        let workspace = TestDir::new();
        create_fake_git_repo(&workspace.path().join("micronaut-serialization"));

        assert_eq!(
            compare_target_path(&started, &HashMap::new(), workspace.path()),
            Some(workspace.path().join("micronaut-serialization"))
        );
    }

    #[test]
    fn compare_target_path_for_task_prefers_runtime_task_checkout_metadata() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository =
            Some("micronaut-projects/micronaut-redis".to_string());
        assign_active_task(
            &mut started,
            "https://github.com/micronaut-projects/micronaut-redis/issues/726",
        );
        let workspace = TestDir::new();
        let repo_root = workspace.path().join("micronaut-redis");
        create_fake_git_repo(&repo_root);
        let worktree = workspace.path().join("work/micronaut-redis-726");
        fs::create_dir_all(&worktree).expect("task worktree should be created");
        create_fake_git_worktree(&worktree, &repo_root.join(".git"));
        started.task_states.insert(
            "task-42".to_string(),
            WorkspaceTaskRuntimeSnapshot {
                repository: vec![
                    repo_root.display().to_string(),
                    worktree.display().to_string(),
                ],
                ..Default::default()
            },
        );

        let task = started
            .task_persistent_snapshot("task-42")
            .expect("task should exist");
        assert_eq!(
            compare_target_path_for_task(
                &started,
                task,
                started.task_states.get("task-42"),
                workspace.path()
            ),
            Some(worktree)
        );
    }

    #[test]
    fn compare_target_path_for_task_skips_broken_worktree_and_falls_back() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository =
            Some("micronaut-projects/micronaut-redis".to_string());
        assign_active_task(
            &mut started,
            "https://github.com/micronaut-projects/micronaut-redis/issues/726",
        );
        let workspace = TestDir::new();
        let repo_root = workspace.path().join("micronaut-redis");
        create_fake_git_repo(&repo_root);
        let worktree = workspace.path().join("work/micronaut-redis-726");
        fs::create_dir_all(&worktree).expect("task worktree should be created");
        fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                repo_root
                    .join(".git/worktrees/micronaut-redis-726")
                    .display()
            ),
        )
        .expect("broken worktree git file should be created");
        started.task_states.insert(
            "task-42".to_string(),
            WorkspaceTaskRuntimeSnapshot {
                repository: vec![
                    worktree.display().to_string(),
                    repo_root.display().to_string(),
                ],
                ..Default::default()
            },
        );

        let task = started
            .task_persistent_snapshot("task-42")
            .expect("task should exist");
        assert_eq!(
            compare_target_path_for_task(
                &started,
                task,
                started.task_states.get("task-42"),
                workspace.path()
            ),
            Some(repo_root)
        );
    }

    #[test]
    fn compare_target_path_is_none_without_review_repo_or_workspace_repo() {
        let started = snapshot(true, Some("http://example"));
        let workspace = TestDir::new();

        assert_eq!(
            compare_target_path(&started, &HashMap::new(), workspace.path()),
            None
        );
    }

    #[test]
    fn snapshot_attach_cwd_for_selection_prefers_task_checkout() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository =
            Some("micronaut-projects/micronaut-serialization".to_string());
        assign_active_task(
            &mut started,
            "https://github.com/micronaut-projects/micronaut-serialization/issues/921",
        );
        let workspace = TestDir::new();
        let repo_root = workspace.path().join("micronaut-serialization");
        create_fake_git_repo(&repo_root);
        let task_checkout = workspace.path().join("work/micronaut-serialization-921");
        fs::create_dir_all(&task_checkout).expect("task checkout should be created");
        create_fake_git_worktree(&task_checkout, &repo_root.join(".git"));

        assert_eq!(
            snapshot_attach_cwd_for_selection(
                &started,
                Some("task-42"),
                &HashMap::new(),
                workspace.path(),
            ),
            Some(task_checkout)
        );
    }

    #[test]
    fn snapshot_attach_cwd_for_selection_falls_back_to_workspace_checkout() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository =
            Some("micronaut-projects/micronaut-serialization".to_string());
        let workspace = TestDir::new();
        create_fake_git_repo(&workspace.path().join("micronaut-serialization"));

        assert_eq!(
            snapshot_attach_cwd_for_selection(&started, None, &HashMap::new(), workspace.path()),
            Some(workspace.path().join("micronaut-serialization"))
        );
    }

    #[test]
    fn selectable_workspace_links_include_custom_issue_and_pr_without_github_status() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.agent_provided.repo = vec!["/tmp/repo-a".to_string()];
        started.persistent.custom_links.issue = vec!["https://example.com/issue/1".to_string()];
        started.persistent.custom_links.pr = vec!["https://example.com/pull/2".to_string()];

        let all_links = workspace_links(&started);
        let mut validations = HashMap::new();
        validations.insert(
            all_links[0].clone(),
            WorkspaceLinkValidationResult::Valid("/tmp/repo-a".to_string()),
        );
        validations.insert(
            all_links[1].clone(),
            WorkspaceLinkValidationResult::Valid("https://example.com/issue/1".to_string()),
        );
        validations.insert(
            all_links[2].clone(),
            WorkspaceLinkValidationResult::Valid("https://example.com/pull/2".to_string()),
        );

        let selectable = selectable_workspace_links(&started, &validations, &HashMap::new());
        assert_eq!(selectable, all_links);
    }

    #[test]
    fn selectable_workspace_links_include_empty_issue_and_pr_slots_for_custom_add_flow() {
        let started = snapshot(true, Some("http://example"));

        let selectable = selectable_workspace_links(&started, &HashMap::new(), &HashMap::new());
        assert_eq!(selectable.len(), 2);
        assert_eq!(selectable[0].kind, WorkspaceLinkKind::Issue);
        assert!(selectable[0].value.is_empty());
        assert_eq!(selectable[1].kind, WorkspaceLinkKind::Pr);
        assert!(selectable[1].value.is_empty());
    }

    #[test]
    fn archived_workspace_links_keep_issue_and_pr_but_hide_review() {
        let mut archived = snapshot(false, None);
        archived.persistent.archived = true;
        archived.persistent.agent_provided.repo = vec!["/tmp/repo-a".to_string()];
        archived.persistent.agent_provided.issue = vec!["https://example.com/issue/1".to_string()];
        archived.persistent.agent_provided.pr = vec!["https://example.com/pull/2".to_string()];

        let all_links = workspace_links(&archived);
        let mut validations = HashMap::new();
        validations.insert(
            all_links[0].clone(),
            WorkspaceLinkValidationResult::Valid("/tmp/repo-a".to_string()),
        );
        validations.insert(
            all_links[1].clone(),
            WorkspaceLinkValidationResult::Valid("https://example.com/issue/1".to_string()),
        );
        validations.insert(
            all_links[2].clone(),
            WorkspaceLinkValidationResult::Valid("https://example.com/pull/2".to_string()),
        );

        let visible = visible_workspace_links(&archived, &validations);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].kind, WorkspaceLinkKind::Issue);
        assert_eq!(visible[1].kind, WorkspaceLinkKind::Pr);
    }

    #[test]
    fn validated_workspace_links_by_kind_keeps_multiple_targets() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.agent_provided.repo = vec![
            "/tmp/repo-a".to_string(),
            "/tmp/repo-b".to_string(),
            "/tmp/repo-c".to_string(),
        ];

        let all_links = workspace_links(&started);
        let mut validations = HashMap::new();
        validations.insert(
            all_links[0].clone(),
            WorkspaceLinkValidationResult::Valid("/tmp/repo-a".to_string()),
        );
        validations.insert(
            all_links[1].clone(),
            WorkspaceLinkValidationResult::Valid("/tmp/repo-b".to_string()),
        );
        validations.insert(
            all_links[2].clone(),
            WorkspaceLinkValidationResult::Invalid("skip".to_string()),
        );

        assert_eq!(
            validated_workspace_links_by_kind(&started, &validations, WorkspaceLinkKind::Review,),
            vec![all_links[0].clone(), all_links[1].clone()]
        );
    }

    #[test]
    fn compact_github_tooltip_target_shortens_issue_and_pr_links() {
        assert_eq!(
            compact_github_tooltip_target(
                "https://github.com/micronaut-projects/micronaut-core/issues/1234"
            ),
            Some("\u{f408} micronaut-projects/micronaut-core#1234".to_string())
        );
        assert_eq!(
            compact_github_tooltip_target(
                "https://github.com/micronaut-projects/micronaut-core/pull/5678"
            ),
            Some("\u{f408} micronaut-projects/micronaut-core#5678".to_string())
        );
    }

    #[test]
    fn compact_github_tooltip_target_leaves_other_urls_unmatched() {
        assert_eq!(
            compact_github_tooltip_target("https://example.com/issues/1234"),
            None
        );
        assert_eq!(
            compact_github_tooltip_target("https://github.com/micronaut-projects/micronaut-core"),
            None
        );
    }

    #[test]
    fn github_repository_url_normalizes_repository_specs() {
        assert_eq!(
            github_repository_url("micronaut-projects/micronaut-core"),
            Some("https://github.com/micronaut-projects/micronaut-core".to_string())
        );
        assert_eq!(
            github_repository_url("https://github.com/micronaut-projects/micronaut-core/issues"),
            Some("https://github.com/micronaut-projects/micronaut-core".to_string())
        );
    }

    #[test]
    fn github_repository_url_rejects_invalid_specs() {
        assert_eq!(github_repository_url(""), None);
        assert_eq!(github_repository_url("owner/repo/extra"), None);
        assert_eq!(
            github_repository_url("https://example.com/owner/repo"),
            None
        );
    }

    #[test]
    fn selected_link_tooltip_area_prefers_space_below_link() {
        let targets = vec![("target".to_string(), false)];
        let area = selected_link_tooltip_area(
            Rect::new(0, 0, 80, 20),
            2,
            WorkspaceLinkKind::Review,
            &targets,
            [10, 10, 5, 5, 5, 2, 2, 2, 2, 2, 2],
        )
        .expect("tooltip area should exist");

        assert_eq!(area.x, 41);
        assert_eq!(area.y, 5);
        assert_eq!(area.height, 3);
    }

    #[test]
    fn selected_link_tooltip_area_flips_above_when_below_space_is_too_small() {
        let targets = vec![
            ("one".to_string(), false),
            ("two".to_string(), false),
            ("three".to_string(), false),
        ];
        let area = selected_link_tooltip_area(
            Rect::new(0, 0, 80, 8),
            4,
            WorkspaceLinkKind::Pr,
            &targets,
            [10, 10, 5, 5, 5, 2, 2, 2, 2, 2, 2],
        )
        .expect("tooltip area should exist");

        assert_eq!(area.x, 50);
        assert_eq!(area.y, 1);
        assert_eq!(area.height, 5);
    }

    #[test]
    fn description_line_does_not_embed_agent_links() {
        let mut started = snapshot(true, Some("http://example"));
        started.root_session_title = Some("Root session title".to_string());
        started.persistent.agent_provided.repo = vec!["/tmp/repo-a".to_string()];

        let line = description_line(&started, "Custom description", false);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "Custom description · Root session title");
    }

    #[test]
    fn workspace_link_kind_uses_short_labels() {
        assert_eq!(WorkspaceLinkKind::Review.short_label(), "RE");
        assert_eq!(WorkspaceLinkKind::Issue.short_label(), "IS");
        assert_eq!(WorkspaceLinkKind::Pr.short_label(), "PR");
    }

    #[test]
    fn archived_icon_cells_use_dimmed_foreground_color() {
        let style = Style::default().fg(Color::DarkGray).bg(Color::Reset);
        assert_eq!(style.fg, Some(Color::DarkGray));
        assert_eq!(style.bg, Some(Color::Reset));
    }

    #[test]
    fn every_status_icon_kind_has_a_nerd_font_glyph() {
        for kind in [
            StatusIconKind::Eye,
            StatusIconKind::Server,
            StatusIconKind::FileDiff,
            StatusIconKind::Bug,
            StatusIconKind::Docs,
            StatusIconKind::Enhancement,
            StatusIconKind::Improvement,
            StatusIconKind::Regression,
            StatusIconKind::DependencyUpgrade,
            StatusIconKind::GitPullRequest,
            StatusIconKind::GitPullRequestDraft,
            StatusIconKind::GitPullRequestClosed,
            StatusIconKind::GitMerge,
            StatusIconKind::IssueOpened,
            StatusIconKind::IssueClosed,
        ] {
            let glyph = icon_glyph(kind);
            assert!(!glyph.is_empty());
            assert_eq!(glyph.chars().count(), 1);
        }
    }

    #[test]
    fn status_icon_mappings_use_expected_glyph_kinds() {
        assert_eq!(
            issue_icon_kind_and_color(GithubIssueState::Open),
            (StatusIconKind::IssueOpened, Color::Green)
        );
        assert_eq!(
            issue_icon_kind_and_color(GithubIssueState::Closed),
            (StatusIconKind::IssueClosed, Color::Magenta)
        );

        assert_eq!(
            pr_icon_kind_and_color(GithubPrStatus {
                state: GithubPrState::Open,
                build: GithubPrBuildState::Succeeded,
                review: GithubPrReviewState::Accepted,
                is_draft: false,
                fetched_at: SystemTime::UNIX_EPOCH,
            }),
            (StatusIconKind::GitPullRequest, Color::Green)
        );
        assert_eq!(
            pr_icon_kind_and_color(GithubPrStatus {
                state: GithubPrState::Open,
                build: GithubPrBuildState::Succeeded,
                review: GithubPrReviewState::Accepted,
                is_draft: true,
                fetched_at: SystemTime::UNIX_EPOCH,
            }),
            (StatusIconKind::GitPullRequestDraft, Color::DarkGray)
        );
        assert_eq!(
            pr_icon_kind_and_color(GithubPrStatus {
                state: GithubPrState::Rejected,
                build: GithubPrBuildState::Succeeded,
                review: GithubPrReviewState::Accepted,
                is_draft: false,
                fetched_at: SystemTime::UNIX_EPOCH,
            }),
            (StatusIconKind::GitPullRequestClosed, Color::Red)
        );
        assert_eq!(
            pr_icon_kind_and_color(GithubPrStatus {
                state: GithubPrState::Merged,
                build: GithubPrBuildState::Succeeded,
                review: GithubPrReviewState::Accepted,
                is_draft: false,
                fetched_at: SystemTime::UNIX_EPOCH,
            }),
            (StatusIconKind::GitMerge, Color::Magenta)
        );

        let open_pr = GithubPrStatus {
            state: GithubPrState::Open,
            build: GithubPrBuildState::Succeeded,
            review: GithubPrReviewState::Accepted,
            is_draft: false,
            fetched_at: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(pr_build_icon_color(open_pr), Some(Color::Green));
        assert_eq!(pr_review_icon_color(open_pr), Some(Color::Green));

        let unreviewed_pr = GithubPrStatus {
            state: GithubPrState::Open,
            build: GithubPrBuildState::Succeeded,
            review: GithubPrReviewState::None,
            is_draft: false,
            fetched_at: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(pr_review_icon_color(unreviewed_pr), Some(Color::DarkGray));

        let pending_review_pr = GithubPrStatus {
            state: GithubPrState::Open,
            build: GithubPrBuildState::Succeeded,
            review: GithubPrReviewState::Outstanding,
            is_draft: false,
            fetched_at: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(pr_review_icon_color(pending_review_pr), Some(Color::Yellow));

        let rejected_review_pr = GithubPrStatus {
            state: GithubPrState::Open,
            build: GithubPrBuildState::Succeeded,
            review: GithubPrReviewState::Rejected,
            is_draft: false,
            fetched_at: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(pr_review_icon_color(rejected_review_pr), Some(Color::Red));

        let merged_pr = GithubPrStatus {
            state: GithubPrState::Merged,
            build: GithubPrBuildState::Succeeded,
            review: GithubPrReviewState::Accepted,
            is_draft: false,
            fetched_at: SystemTime::UNIX_EPOCH,
        };
        assert_eq!(pr_build_icon_color(merged_pr), None);
        assert_eq!(pr_review_icon_color(merged_pr), None);
    }

    #[test]
    fn help_line_shows_link_navigation_when_workspace_has_agent_links() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.agent_provided.issue = vec!["https://example.com/issue/1".to_string()];

        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            1,
            Some(0),
            false,
            false,
            Some(WorkspaceLinkKind::Issue),
            true,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("←/→ select link"));
        assert!(text.contains("Enter open link"));
        assert!(text.contains("o open GitHub"));
        assert!(text.contains("↑/↓ select target"));
        assert!(text.contains("a add link"));
        assert!(text.contains("Esc row focus"));
        assert!(!text.contains("edit description"));
        assert!(!text.contains("archive"));
        assert!(!text.contains("recheck GH status"));
    }

    #[test]
    fn help_line_shows_edit_delete_for_selected_custom_link() {
        let started = snapshot(true, Some("http://example"));

        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            1,
            Some(0),
            true,
            false,
            Some(WorkspaceLinkKind::Issue),
            true,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("d edit/delete link"));
    }

    #[test]
    fn help_line_shows_add_only_for_empty_issue_placeholder() {
        let started = snapshot(true, Some("http://example"));

        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            1,
            Some(0),
            true,
            true,
            Some(WorkspaceLinkKind::Issue),
            true,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Enter add issue link"));
        assert!(!text.contains("a add link"));
        assert!(!text.contains("d edit/delete link"));
    }

    #[test]
    fn help_line_shows_recheck_hotkey_only_when_refreshable_github_links_exist() {
        let started = snapshot(true, Some("http://example"));

        let enabled_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            1,
            Some(0),
            false,
            false,
            None,
            true,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let enabled_text = enabled_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!enabled_text.contains("r recheck GH status"));

        let disabled_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            1,
            Some(0),
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let disabled_text = disabled_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!disabled_text.contains("r recheck GH status"));
    }

    #[test]
    fn help_line_shows_only_create_actions_on_create_row() {
        let line = help_line(
            UiMode::Normal,
            0,
            0,
            None,
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Enter create"));
        assert!(!text.contains("archive"));
        assert!(!text.contains("start"));
    }

    #[test]
    fn help_line_shows_attach_only_for_started_workspace() {
        let started = snapshot(true, Some("http://example"));
        let started_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let started_text = started_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(started_text.contains("Enter attach"));

        let stopped = snapshot(false, None);
        let stopped_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&stopped),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let stopped_text = stopped_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!stopped_text.contains("Enter attach"));
        assert!(stopped_text.contains("Enter start+attach"));
    }

    #[test]
    fn help_line_shows_pause_or_resume_only_for_started_automation_workspace() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository = Some("example/repo".to_string());
        let started_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            true,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let started_text = started_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(started_text.contains("p pause"));

        started.persistent.automation_paused = true;
        let paused_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            true,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let paused_text = paused_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(paused_text.contains("p resume"));

        let stopped = snapshot(false, None);
        let stopped_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&stopped),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let stopped_text = stopped_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!stopped_text.contains("p pause"));
        assert!(!stopped_text.contains("p resume"));
    }

    #[test]
    fn help_line_shows_issue_hotkey_for_workspace_with_assigned_repository() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository = Some("example/repo".to_string());
        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            true,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("o open repo"));
        assert!(text.contains("i issue"));
        assert!(text.contains("n queue next"));
    }

    #[test]
    fn help_line_hides_issue_hotkey_without_assigned_repository() {
        let started = snapshot(true, Some("http://example"));
        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(!text.contains("i issue"));
        assert!(!text.contains("n queue next"));
    }

    #[test]
    fn help_line_shows_delete_hotkey_for_workspace_row_focus() {
        let started = snapshot(true, Some("http://example"));
        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            true,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("x delete"));
    }

    #[test]
    fn help_line_limits_actions_for_task_row_focus() {
        let started = snapshot(true, Some("http://example"));
        let line = help_line(
            UiMode::Normal,
            2,
            2,
            Some(&started),
            true,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            true,
            true,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Enter attach"));
        assert!(text.contains("c compare"));
        assert!(text.contains("e edit"));
        assert!(text.contains("x remove issue"));
        assert!(!text.contains("a approve"));
        assert!(!text.contains("i issue"));
        assert!(!text.contains("d edit description"));
        assert!(!text.contains("a archive"));
        assert!(!text.contains("r recheck GH status"));
    }

    #[test]
    fn help_line_shows_approve_hotkey_for_codex_task_row_focus() {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        started.root_session_id = Some("thread-root".to_string());
        let line = help_line(
            UiMode::Normal,
            2,
            2,
            Some(&started),
            true,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            true,
            true,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("a approve"));
        assert!(!text.contains("a archive"));
    }

    #[test]
    fn help_line_shows_fix_ci_hotkey_for_failing_task_row() {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        started.root_session_id = Some("thread-root".to_string());
        let line = help_line_with_task_fix(
            UiMode::Normal,
            2,
            2,
            Some(&started),
            true,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            true,
            true,
            true,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("a approve"));
        assert!(text.contains("f fix CI"));
        assert!(text.contains("o open GitHub"));
    }

    #[test]
    fn help_line_shows_open_github_for_task_row_focus_without_selected_link() {
        let started = snapshot(true, Some("http://example"));
        let line = help_line(
            UiMode::Normal,
            2,
            2,
            Some(&started),
            true,
            2,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("o open GitHub"));
        assert!(text.contains("x remove issue"));
    }

    #[test]
    fn should_offer_codex_ci_fix_requires_pr_link_and_open_non_green_pr() {
        assert!(should_offer_codex_ci_fix(
            true,
            Some(GithubPrStatus {
                state: GithubPrState::Open,
                build: GithubPrBuildState::Failed,
                review: GithubPrReviewState::Outstanding,
                is_draft: false,
                fetched_at: UNIX_EPOCH,
            })
        ));
        assert!(should_offer_codex_ci_fix(
            true,
            Some(GithubPrStatus {
                state: GithubPrState::Open,
                build: GithubPrBuildState::Building,
                review: GithubPrReviewState::Outstanding,
                is_draft: false,
                fetched_at: UNIX_EPOCH,
            })
        ));
        assert!(!should_offer_codex_ci_fix(
            true,
            Some(GithubPrStatus {
                state: GithubPrState::Open,
                build: GithubPrBuildState::Succeeded,
                review: GithubPrReviewState::Outstanding,
                is_draft: false,
                fetched_at: UNIX_EPOCH,
            })
        ));
        assert!(!should_offer_codex_ci_fix(
            true,
            Some(GithubPrStatus {
                state: GithubPrState::Merged,
                build: GithubPrBuildState::Failed,
                review: GithubPrReviewState::Outstanding,
                is_draft: false,
                fetched_at: UNIX_EPOCH,
            })
        ));
        assert!(should_offer_codex_ci_fix(true, None));
        assert!(!should_offer_codex_ci_fix(false, None));
    }

    #[test]
    fn build_codex_fix_ci_prompt_includes_autonomous_context_and_ci_instructions() {
        let prompt = build_codex_fix_ci_prompt(
            "micronaut-projects/micronaut-kafka",
            "https://github.com/micronaut-projects/micronaut-kafka/issues/873",
            Some("https://github.com/micronaut-projects/micronaut-kafka/pull/1308"),
            std::path::Path::new("/tmp/work/micronaut-kafka-873"),
            std::path::Path::new("/tmp/state/task-873.state"),
            Some("thread-task-873"),
        );

        assert!(prompt.contains(
            "You are operating in an autonomous multicode workspace for repository micronaut-projects/micronaut-kafka."
        ));
        assert!(prompt.contains("Continue autonomously from where you left off."));
        assert!(prompt.contains(
            "Start from the existing checkout for GitHub issue https://github.com/micronaut-projects/micronaut-kafka/issues/873."
        ));
        assert!(prompt.contains("Primary checkout for this task: /tmp/work/micronaut-kafka-873"));
        assert!(prompt.contains("write autonomous state updates to `/tmp/state/task-873.state`"));
        assert!(
            prompt
                .contains("write autonomous state updates in the format `<state>:thread-task-873`")
        );
        assert!(prompt.contains("Use the existing pull request https://github.com/micronaut-projects/micronaut-kafka/pull/1308."));
        assert!(prompt.contains("`machine-readable-pr`"));
        assert!(prompt.contains("`autonomous-state`"));
        assert!(prompt.contains(
            "Run repository commands, builds, Gradle tasks, focused tests, git commits, branch pushes, and pull request updates as needed without asking for permission."
        ));
        assert!(prompt.contains(
            "Existing tests must never be changed just to satisfy failing checks or to mask regressions; preserve the intended existing behavior."
        ));
        assert!(prompt.contains("Do not create a new pull request."));
        assert!(prompt.contains("Do not merge the pull request."));
    }

    #[test]
    fn github_repository_spec_accepts_repo_and_github_urls() {
        assert_eq!(
            github_repository_spec("micronaut-projects/micronaut-kafka").as_deref(),
            Some("micronaut-projects/micronaut-kafka")
        );
        assert_eq!(
            github_repository_spec("https://github.com/micronaut-projects/micronaut-kafka")
                .as_deref(),
            Some("micronaut-projects/micronaut-kafka")
        );
        assert_eq!(
            github_repository_spec(
                "https://github.com/micronaut-projects/micronaut-kafka/issues/873"
            )
            .as_deref(),
            Some("micronaut-projects/micronaut-kafka")
        );
        assert_eq!(
            github_repository_spec(
                "https://github.com/micronaut-projects/micronaut-kafka/pull/1308"
            )
            .as_deref(),
            Some("micronaut-projects/micronaut-kafka")
        );
        assert!(github_repository_spec("https://example.com/not-github/repo").is_none());
    }

    #[test]
    fn task_repository_spec_falls_back_to_task_issue_when_workspace_repo_missing() {
        let mut started = snapshot(true, Some("ws://127.0.0.1:3456/"));
        started.root_session_id = Some("thread-root".to_string());
        let issue_url = "https://github.com/micronaut-projects/micronaut-kafka/issues/873";
        let task_id = "task-873".to_string();
        started
            .persistent
            .tasks
            .push(multicode_lib::WorkspaceTaskPersistentSnapshot::new(
                task_id.clone(),
                issue_url.to_string(),
                multicode_lib::WorkspaceTaskSource::Manual,
            ));
        started.active_task_id = Some(task_id.clone());

        assert_eq!(
            task_repository_spec(&started, &task_id).as_deref(),
            Some("micronaut-projects/micronaut-kafka")
        );
    }

    #[test]
    fn help_line_shows_open_github_for_selected_issue_on_task_row() {
        let started = snapshot(true, Some("http://example"));
        let line = help_line(
            UiMode::Normal,
            2,
            2,
            Some(&started),
            true,
            2,
            Some(0),
            false,
            false,
            Some(WorkspaceLinkKind::Issue),
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Enter open link"));
        assert!(text.contains("o open GitHub"));
        assert!(text.contains("Esc row focus"));
        assert!(!text.contains("x remove issue"));
    }

    #[test]
    fn help_line_hides_open_github_for_selected_review_on_task_row() {
        let started = snapshot(true, Some("http://example"));
        let line = help_line(
            UiMode::Normal,
            2,
            2,
            Some(&started),
            true,
            2,
            Some(0),
            false,
            false,
            Some(WorkspaceLinkKind::Review),
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Enter open link"));
        assert!(!text.contains("o open GitHub"));
        assert!(text.contains("Esc row focus"));
    }

    #[test]
    fn help_line_shows_compare_hotkey_only_when_enabled() {
        let started = snapshot(true, Some("http://example"));
        let enabled_line = help_line(
            UiMode::Normal,
            2,
            1,
            Some(&started),
            true,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            true,
            true,
            false,
            no_tool_hotkeys(),
            "",
        );
        let enabled_text = enabled_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(enabled_text.contains("c compare"));
        assert!(enabled_text.contains("e edit"));

        let disabled_line = help_line(
            UiMode::Normal,
            2,
            1,
            Some(&started),
            true,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let disabled_text = disabled_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!disabled_text.contains("c compare"));
        assert!(!disabled_text.contains("e edit"));
    }

    #[test]
    fn help_line_shows_starting_message_in_starting_modal() {
        let line = help_line(
            UiMode::StartingModal,
            1,
            1,
            Some(&snapshot(false, None)),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Starting workspace and waiting for server readiness"));
    }

    #[test]
    fn help_line_shows_wait_message_for_non_cancellable_tool_progress_modal() {
        let line = help_line(
            UiMode::ToolProgressModal,
            1,
            1,
            Some(&snapshot(false, None)),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Operation is running in the selected workspace"));
        assert!(text.contains("Waiting for it to finish"));
        assert!(!text.contains("Esc cancel"));
    }

    #[test]
    fn help_line_shows_confirm_delete_message() {
        let line = help_line(
            UiMode::ConfirmDelete,
            1,
            1,
            Some(&snapshot(false, None)),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Delete item:"));
        assert!(text.contains("Enter"));
    }

    #[test]
    fn help_line_shows_confirm_task_removal_message() {
        let line = help_line(
            UiMode::ConfirmTaskRemoval,
            1,
            1,
            Some(&snapshot(false, None)),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Remove issue:"));
        assert!(text.contains("←/→"));
        assert!(text.contains("Enter"));
    }

    #[test]
    fn starting_modal_closes_when_workspace_returns_to_stopped() {
        let mut mode = UiMode::StartingModal;
        let mut starting_workspace_key = Some("alpha".to_string());
        let mut started_wait_since = Some(Instant::now());
        let mut status = String::new();
        let mut failed_snapshot = WorkspaceSnapshot::default();
        failed_snapshot.automation_status =
            Some("Start failed alpha: workspace start failed".to_string());

        let starting_state = Some(WorkspaceUiState::Stopped);
        match starting_state {
            Some(WorkspaceUiState::Starting) => {}
            Some(WorkspaceUiState::Started) => {}
            Some(WorkspaceUiState::Stopped) => {
                if let Some(key) = starting_workspace_key.as_deref() {
                    status = starting_modal_failure_status(key, Some(&failed_snapshot));
                }
                mode = UiMode::Normal;
                starting_workspace_key = None;
                started_wait_since = None;
            }
            None => {
                mode = UiMode::Normal;
                starting_workspace_key = None;
                started_wait_since = None;
            }
        }

        assert_eq!(mode, UiMode::Normal);
        assert!(starting_workspace_key.is_none());
        assert!(started_wait_since.is_none());
        assert!(status.contains("Start failed alpha"));
    }

    #[test]
    fn starting_modal_failure_status_prefers_workspace_automation_status() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.automation_status =
            Some("Start failed serialization: Apple container allocator exhausted".to_string());

        assert_eq!(
            starting_modal_failure_status("serialization", Some(&snapshot)),
            "Workspace 'serialization' failed to start: Start failed serialization: Apple container allocator exhausted"
        );
    }

    #[test]
    fn help_line_shows_tool_hotkeys_for_usable_tools() {
        let started = snapshot(true, Some("http://example"));
        let tool_hotkeys: Vec<(String, String)> = vec![("x".to_string(), "Shell".to_string())];
        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            true,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            &tool_hotkeys,
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("x Shell"));
    }

    #[test]
    fn contextual_tool_hotkeys_hides_prompt_tool_until_workspace_is_started() {
        let tools = vec![
            ToolConfig {
                type_: ToolType::Exec,
                name: "Shell".to_string(),
                key: "x".to_string(),
                exec: Some("/bin/bash".to_string()),
                prompt: None,
            },
            ToolConfig {
                type_: ToolType::Prompt,
                name: "Smerge".to_string(),
                key: "m".to_string(),
                exec: None,
                prompt: Some("Start /usr/bin/smerge in the repository".to_string()),
            },
        ];

        let stopped = snapshot(false, None);
        assert_eq!(
            contextual_tool_hotkeys(&tools, Some(&stopped)),
            vec![("x".to_string(), "Shell".to_string())]
        );

        let started = snapshot(true, Some("http://example"));
        assert_eq!(
            contextual_tool_hotkeys(&tools, Some(&started)),
            vec![
                ("x".to_string(), "Shell".to_string()),
                ("m".to_string(), "Smerge".to_string())
            ]
        );
    }

    #[test]
    fn session_wait_state_defaults_missing_status_to_idle() {
        assert!(matches!(
            session_wait_state_for_entry(None),
            SessionWaitState::Idle
        ));
    }

    #[test]
    fn session_wait_state_treats_busy_and_retry_as_busy() {
        assert!(matches!(
            session_wait_state_for_entry(Some(&opencode::client::types::SessionStatus::Busy)),
            SessionWaitState::Busy
        ));
        assert!(matches!(
            session_wait_state_for_entry(Some(&opencode::client::types::SessionStatus::Retry {
                attempt: 1.0,
                message: "retry".to_string(),
                next: 0.1,
            })),
            SessionWaitState::Busy
        ));
    }

    #[test]
    fn centered_rect_fixed_uses_requested_size_when_it_fits() {
        let rect = centered_rect_fixed(20, 7, Rect::new(0, 0, 80, 24));
        assert_eq!(rect.width, 20);
        assert_eq!(rect.height, 7);
    }

    #[test]
    fn centered_rect_fixed_clamps_to_available_area() {
        let rect = centered_rect_fixed(99, 99, Rect::new(0, 0, 30, 10));
        assert_eq!(rect.width, 30);
        assert_eq!(rect.height, 10);
    }

    #[test]
    fn tmux_status_left_includes_workspace_key_and_custom_description() {
        assert_eq!(
            tmux_status_left("core12439", "backend service"),
            "core12439 | backend service | "
        );
        assert_eq!(tmux_status_left("core12439", "   "), "core12439 | ");
        assert_eq!(
            tmux_status_left("core#12439", "desc #1"),
            "core##12439 | desc ##1 | "
        );
    }

    #[test]
    fn started_workspace_attach_ready_waits_for_root_session_after_start() {
        let started = snapshot(true, Some("http://example"));
        let now = Instant::now();

        let (ready, wait_since) = started_workspace_attach_ready(&started, None, now);

        assert!(!ready);
        assert_eq!(wait_since, Some(now));
    }

    #[test]
    fn started_workspace_attach_ready_attaches_immediately_when_root_session_exists() {
        let mut started = snapshot(true, Some("http://example"));
        started.root_session_id = Some("ses-root-latest".to_string());
        let now = Instant::now();

        let (ready, wait_since) = started_workspace_attach_ready(&started, None, now);

        assert!(ready);
        assert_eq!(wait_since, None);
    }

    #[test]
    fn started_workspace_attach_ready_falls_back_after_timeout_without_root_session() {
        let started = snapshot(true, Some("http://example"));
        let now = Instant::now();
        let timed_out_since = now - ROOT_SESSION_ATTACH_WAIT_TIMEOUT - Duration::from_millis(1);

        let (ready, wait_since) =
            started_workspace_attach_ready(&started, Some(timed_out_since), now);

        assert!(ready);
        assert_eq!(wait_since, None);
    }

    #[test]
    fn started_workspace_attach_ready_resets_wait_while_not_started() {
        let not_started = snapshot(false, Some("http://example"));
        let now = Instant::now();

        let (ready, wait_since) = started_workspace_attach_ready(&not_started, Some(now), now);

        assert!(!ready);
        assert_eq!(wait_since, None);
    }

    #[test]
    fn help_line_shows_contextual_archive_and_unarchive_action() {
        let mut active = snapshot(false, None);
        active.persistent.archived = false;
        let active_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&active),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let active_text = active_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(active_text.contains("a archive"));

        let mut archived = snapshot(false, None);
        archived.persistent.archived = true;
        let archived_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&archived),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let archived_text = archived_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(archived_text.contains("a unarchive"));
    }

    #[test]
    fn server_cell_label_is_empty_for_stopped_workspace() {
        let stopped = snapshot(false, None);
        assert_eq!(server_cell_label(&stopped), "");
    }

    #[test]
    fn server_cell_label_defaults_unknown_session_status_to_idle() {
        let started = snapshot(true, Some("http://example"));
        assert_eq!(server_cell_label(&started), "Idle");
    }

    #[test]
    fn server_cell_label_shows_busy_for_busy_session_status() {
        let mut started = snapshot(true, Some("http://example"));
        started.root_session_status = Some(RootSessionStatus::Busy);
        assert_eq!(server_cell_label(&started), "Busy");
    }

    #[test]
    fn server_cell_label_shows_question_for_question_session_status() {
        let mut started = snapshot(true, Some("http://example"));
        started.root_session_status = Some(RootSessionStatus::Question);
        assert_eq!(server_cell_label(&started), "Question");
    }

    #[test]
    fn server_cell_label_uses_automation_question_state() {
        let mut started = snapshot(true, Some("http://example"));
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.automation_agent_state = Some(AutomationAgentState::Question);

        assert_eq!(server_cell_label(&started), "Question");
    }

    #[test]
    fn server_cell_label_uses_automation_review_state_as_idle() {
        let mut started = snapshot(true, Some("http://example"));
        assign_active_task(&mut started, "https://github.com/example/repo/issues/42");
        started.automation_agent_state = Some(AutomationAgentState::Review);

        assert_eq!(server_cell_label(&started), "Idle");
    }

    #[test]
    fn description_cell_text_appends_root_session_title_after_description() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.description = "Custom description".to_string();
        started.root_session_title = Some("Root session title".to_string());

        assert_eq!(
            description_cell_text(&started, &started.persistent.description),
            "Custom description · Root session title"
        );
    }

    #[test]
    fn description_cell_text_includes_automation_status_before_root_session_title() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.description = "Custom description".to_string();
        started.automation_status = Some("Working on example/repo#42".to_string());
        started.root_session_title = Some("Root session title".to_string());

        assert_eq!(
            description_cell_text(&started, &started.persistent.description),
            "Custom description · Working on example/repo#42 · Root session title"
        );
    }

    #[test]
    fn description_line_styles_custom_description_cyan() {
        let mut started = snapshot(true, Some("http://example"));
        started.root_session_title = Some("Root session title".to_string());
        let line = description_line(&started, "Custom description", false);

        assert_eq!(line.spans[0].content, "Custom description");
        assert_eq!(line.spans[0].style.fg, Some(DESCRIPTION_COLOR));
        assert_eq!(line.spans[2].content, "Root session title");
    }

    #[test]
    fn description_line_does_not_prefix_spinner_for_failure_status() {
        let mut started = snapshot(true, Some("http://example"));
        started.automation_status = Some("Start failed repo: workspace start failed".to_string());

        let line = description_line(&started, "", false);

        assert_eq!(
            line.spans[0].content,
            "Start failed repo: workspace start failed"
        );
    }

    #[test]
    fn description_line_shows_bold_red_oom_prefix_before_description() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.description = "Custom description".to_string();
        started.oom_kill_count = Some(2);

        let line = description_line(&started, &started.persistent.description, false);

        assert_eq!(line.spans[0].content, "OOM ");
        assert_eq!(line.spans[0].style.fg, Some(OOM_COLOR));
        assert!(line.spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(line.spans[1].content, "Custom description");
    }

    #[test]
    fn server_cell_style_uses_idle_busy_and_question_colors() {
        let idle = snapshot(true, Some("http://example"));
        assert_eq!(server_cell_style(&idle, false).fg, Some(IDLE_COLOR));

        let mut busy = snapshot(true, Some("http://example"));
        busy.root_session_status = Some(RootSessionStatus::Busy);
        assert_eq!(server_cell_style(&busy, false).fg, Some(BUSY_COLOR));

        let mut waiting = snapshot(true, Some("http://example"));
        waiting.root_session_status = Some(RootSessionStatus::Question);
        assert_eq!(
            server_cell_style(&waiting, false).fg,
            Some(WAITING_FOR_INPUT_COLOR)
        );

        let stopped = snapshot(false, None);
        assert_eq!(server_cell_style(&stopped, false).fg, None);
    }

    #[test]
    fn cost_cell_label_prefers_price_and_falls_back_to_tokens() {
        let mut snapshot = snapshot(true, Some("http://example"));
        snapshot.usage_total_tokens = Some(1_234_567);
        snapshot.usage_total_cost = Some(2.5);
        assert_eq!(cost_cell_label(&snapshot), "$2.50");

        snapshot.usage_total_cost = Some(0.0);
        assert_eq!(cost_cell_label(&snapshot), "1234k");

        snapshot.usage_total_cost = None;
        assert_eq!(cost_cell_label(&snapshot), "1234k");
    }

    #[test]
    fn task_cost_cell_label_uses_compact_task_tokens() {
        let task_state = WorkspaceTaskRuntimeSnapshot {
            usage_total_tokens: Some(987_654),
            ..Default::default()
        };

        assert_eq!(task_cost_cell_label(Some(&task_state)), "987k");
        assert_eq!(task_cost_cell_label(None), "");
    }

    #[test]
    fn workspace_cost_cell_label_sums_task_tokens_before_workspace_usage() {
        let mut snapshot = snapshot(true, Some("http://example"));
        snapshot.usage_total_cost = Some(2.5);
        snapshot.usage_total_tokens = Some(1_234_567);
        snapshot.task_states.insert(
            "task-1".to_string(),
            WorkspaceTaskRuntimeSnapshot {
                usage_total_tokens: Some(111),
                ..Default::default()
            },
        );
        snapshot.task_states.insert(
            "task-2".to_string(),
            WorkspaceTaskRuntimeSnapshot {
                usage_total_tokens: Some(222),
                ..Default::default()
            },
        );

        assert_eq!(cost_cell_label(&snapshot), "333");
    }

    #[test]
    fn cpu_and_ram_cell_labels_format_usage_values() {
        let mut snapshot = snapshot(true, Some("http://example"));
        snapshot.usage_cpu_percent = Some(7);
        snapshot.usage_ram_bytes = Some(1536);

        assert_eq!(cpu_cell_label(&snapshot), "7%");
        assert!(!ram_cell_label(&snapshot).is_empty());
    }

    #[test]
    fn ram_cell_style_turns_red_at_limit_minus_512_mib() {
        let mut snapshot = snapshot(true, Some("http://example"));
        let memory_high = 2 * 1024 * 1024 * 1024_u64;

        snapshot.usage_ram_bytes = Some(memory_high - RAM_LIMIT_WARNING_HEADROOM_BYTES - 1);
        assert_eq!(ram_cell_style(&snapshot, Some(memory_high), false).fg, None);

        snapshot.usage_ram_bytes = Some(memory_high - RAM_LIMIT_WARNING_HEADROOM_BYTES);
        assert_eq!(
            ram_cell_style(&snapshot, Some(memory_high), false).fg,
            Some(OOM_COLOR)
        );
    }

    #[test]
    fn parse_optional_size_bytes_handles_present_and_missing_values() {
        assert_eq!(
            multicode_lib::services::parse_optional_size_bytes(Some("2GB"), "memory_max")
                .expect("size should parse"),
            Some(
                size::Size::from_str("2GB")
                    .expect("crate should parse size")
                    .bytes() as u64
            )
        );
        assert_eq!(
            multicode_lib::services::parse_optional_size_bytes(Some("2 GiB"), "memory_max")
                .expect("size should parse"),
            Some(
                size::Size::from_str("2 GiB")
                    .expect("crate should parse size")
                    .bytes() as u64
            )
        );
        assert_eq!(
            multicode_lib::services::parse_optional_size_bytes(None, "memory_max")
                .expect("missing size should be allowed"),
            None
        );
    }

    #[test]
    fn parse_proc_cpu_totals_reads_aggregate_cpu_fields() {
        let totals = parse_proc_cpu_totals(
            "cpu  100 20 40 300 10 5 6 7 0 0\ncpu0 50 10 20 150 5 2 3 4 0 0\n",
        )
        .expect("aggregate cpu line should parse");
        assert_eq!(totals.active, 100 + 20 + 40 + 5 + 6 + 7);
        assert_eq!(totals.total, 100 + 20 + 40 + 300 + 10 + 5 + 6 + 7);
    }

    #[test]
    fn machine_cpu_percent_scales_with_logical_cpu_count() {
        let previous = ProcCpuTotals {
            active: 1_000,
            total: 2_000,
        };
        let current = ProcCpuTotals {
            active: 1_500,
            total: 3_000,
        };

        assert_eq!(machine_cpu_percent(previous, current, 8), Some(400));
    }

    #[test]
    fn parse_proc_meminfo_used_ram_bytes_uses_memavailable() {
        let used = parse_proc_meminfo_used_ram_bytes(
            "MemTotal:       8192000 kB\nMemFree:         512000 kB\nMemAvailable:    2048000 kB\nCached:          1024000 kB\n",
        )
        .expect("meminfo should parse");
        assert_eq!(used, (8_192_000_u64 - 2_048_000_u64) * 1024);
    }

    #[test]
    fn parse_proc_meminfo_total_ram_bytes_reads_memtotal() {
        let total = parse_proc_meminfo_total_ram_bytes(
            "MemTotal:       8192000 kB\nMemFree:         512000 kB\nMemAvailable:    2048000 kB\nCached:          1024000 kB\n",
        )
        .expect("meminfo should parse");
        assert_eq!(total, 8_192_000_u64 * 1024);
    }

    #[test]
    fn disk_usage_from_statvfs_calculates_free_and_total_bytes() {
        assert_eq!(
            disk_usage_from_statvfs(1_000, 250, 4_096),
            Some(DiskUsage {
                free_bytes: 250_u64 * 4_096,
                total_bytes: 1_000_u64 * 4_096,
            })
        );
    }

    #[test]
    fn disk_usage_from_statvfs_rejects_zero_fragment_size() {
        assert_eq!(disk_usage_from_statvfs(1_000, 250, 0), None);
    }

    #[test]
    fn machine_description_includes_cpu_ram_and_disk_details() {
        assert_eq!(
            machine_description(
                16,
                Some(64 * 1024 * 1024 * 1024),
                Some(DiskUsage {
                    free_bytes: 300 * 1024 * 1024 * 1024,
                    total_bytes: 500 * 1024 * 1024 * 1024,
                })
            ),
            format!(
                "CPUs: 16 · Total RAM: {} · Free disk: {}",
                format_ram_bytes(64 * 1024 * 1024 * 1024),
                format_ram_bytes(300 * 1024 * 1024 * 1024)
            )
        );
    }

    #[test]
    fn right_align_cell_text_pads_to_requested_width() {
        assert_eq!(right_align_cell_text("7%", 4), "  7%");
        assert_eq!(right_align_cell_text("100%", 4), "100%");
        assert_eq!(
            right_align_cell_text("64 MiB", RAM_COLUMN_WIDTH),
            "    64 MiB"
        );
    }

    #[test]
    fn help_line_renders_hotkeys_in_bold() {
        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&snapshot(true, Some("http://example"))),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let enter = line
            .spans
            .iter()
            .find(|span| span.content == "Enter")
            .expect("Enter hotkey span should exist");
        assert!(enter.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn help_line_hides_unusable_direction_keys_at_boundaries() {
        let first_line = help_line(
            UiMode::Normal,
            0,
            2,
            None,
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let first_text = first_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(!first_text.contains("↑ move"));
        assert!(first_text.contains("↓ move"));

        let last_workspace = snapshot(false, None);
        let last_line = help_line(
            UiMode::Normal,
            2,
            2,
            Some(&last_workspace),
            false,
            0,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            false,
            false,
            no_tool_hotkeys(),
            "",
        );
        let last_text = last_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(last_text.contains("↑ move"));
        assert!(!last_text.contains("↓ move"));
    }

    #[test]
    fn table_column_widths_auto_size_workspace_and_server_columns() {
        let mut snapshots = HashMap::new();
        let mut busy = snapshot(true, Some("http://example"));
        busy.root_session_status = Some(RootSessionStatus::Busy);
        busy.usage_total_cost = Some(12.34);
        busy.usage_cpu_percent = Some(3);
        busy.usage_ram_bytes = Some(64 * 1024 * 1024);
        snapshots.insert("wksp-short".to_string(), snapshot(false, None));
        snapshots.insert("wksp-longer-name".to_string(), busy);
        let ordered_keys = vec!["wksp-short".to_string(), "wksp-longer-name".to_string()];

        let (
            workspace_width,
            server_width,
            cpu_width,
            ram_width,
            cost_width,
            re_width,
            is_width,
            t_width,
            pr_width,
            build_width,
            review_width,
        ) = table_column_widths(
            &ordered_keys,
            &snapshots,
            "Machine:",
            "2200%",
            &machine_ram_cell_label(Some(31.0_f64.mul_add(1024.0 * 1024.0 * 1024.0, 0.0) as u64)),
        );

        assert!(workspace_width >= content_width(CREATE_ROW_LABEL));
        assert!(workspace_width >= content_width("wksp-longer-name"));
        assert!(server_width >= content_width("Server"));
        assert!(server_width >= content_width("Machine:"));
        assert!(cpu_width >= content_width("2200%"));
        assert_eq!(
            ram_width,
            RAM_COLUMN_WIDTH
                .max(content_width("RAM"))
                .max(content_width(&machine_ram_cell_label(Some(
                    31.0_f64.mul_add(1024.0 * 1024.0 * 1024.0, 0.0) as u64,
                ))))
        );
        assert!(cost_width >= content_width("Cost"));
        assert!(cost_width >= content_width("$12.34"));
        assert_eq!(re_width, content_width("RE").max(LINK_COLUMN_WIDTH));
        assert_eq!(is_width, content_width("IS").max(LINK_COLUMN_WIDTH));
        assert_eq!(t_width, content_width("T").max(TYPE_COLUMN_WIDTH));
        assert_eq!(pr_width, content_width("PR").max(LINK_COLUMN_WIDTH));
        assert_eq!(build_width, content_width("B").max(STATUS_COLUMN_WIDTH));
        assert_eq!(
            review_width,
            content_width("R").max(REVIEW_STATUS_COLUMN_WIDTH)
        );
    }

    #[test]
    fn table_column_widths_include_task_cost_and_server_labels() {
        let mut workspace = snapshot(true, Some("http://example"));
        workspace
            .persistent
            .tasks
            .push(multicode_lib::WorkspaceTaskPersistentSnapshot::new(
                "task-39".to_string(),
                "https://github.com/graemerocher/multicode-test/issues/39".to_string(),
                multicode_lib::WorkspaceTaskSource::Scan,
            ));
        workspace.task_states.insert(
            "task-39".to_string(),
            WorkspaceTaskRuntimeSnapshot {
                usage_total_tokens: Some(123_456_789),
                waiting_on_vm: true,
                ..Default::default()
            },
        );
        let mut snapshots = HashMap::new();
        snapshots.insert("e2e-test".to_string(), workspace);
        let ordered_keys = vec!["e2e-test".to_string()];

        let (_, server_width, _, _, cost_width, _, _, _, _, _, _) = table_column_widths(
            &ordered_keys,
            &snapshots,
            "Machine:",
            "2200%",
            &machine_ram_cell_label(Some(0)),
        );

        assert!(server_width >= content_width("Waiting on VM"));
        assert!(cost_width >= content_width("123456k"));
    }

    #[test]
    fn approve_restarts_codex_task_when_runtime_is_not_loaded() {
        let task_state = WorkspaceTaskRuntimeSnapshot {
            session_id: Some("session-1".to_string()),
            agent_state: Some(AutomationAgentState::Review),
            session_status: Some(RootSessionStatus::Idle),
            status: Some("NotLoaded".to_string()),
            ..Default::default()
        };

        assert!(should_restart_codex_task_for_pr_request(Some(&task_state)));
    }

    #[test]
    fn approve_keeps_existing_codex_task_when_review_session_is_idle() {
        let task_state = WorkspaceTaskRuntimeSnapshot {
            session_id: Some("session-1".to_string()),
            agent_state: Some(AutomationAgentState::Review),
            session_status: Some(RootSessionStatus::Idle),
            status: Some("Idle".to_string()),
            ..Default::default()
        };

        assert!(!should_restart_codex_task_for_pr_request(Some(&task_state)));
    }

    #[test]
    fn ci_fix_restarts_idle_review_task_session() {
        let task_state = WorkspaceTaskRuntimeSnapshot {
            session_id: Some("session-1".to_string()),
            agent_state: Some(AutomationAgentState::Review),
            session_status: Some(RootSessionStatus::Idle),
            status: Some("Idle".to_string()),
            ..Default::default()
        };

        assert!(should_restart_codex_task_for_ci_fix(Some(&task_state)));
    }

    #[test]
    fn ci_fix_keeps_busy_working_task_session() {
        let task_state = WorkspaceTaskRuntimeSnapshot {
            session_id: Some("session-1".to_string()),
            agent_state: Some(AutomationAgentState::Working),
            session_status: Some(RootSessionStatus::Busy),
            status: Some("Active".to_string()),
            ..Default::default()
        };

        assert!(!should_restart_codex_task_for_ci_fix(Some(&task_state)));
    }

    #[test]
    fn ci_fix_restarts_question_task_session() {
        let task_state = WorkspaceTaskRuntimeSnapshot {
            session_id: Some("session-1".to_string()),
            agent_state: Some(AutomationAgentState::Review),
            session_status: Some(RootSessionStatus::Question),
            status: Some("Idle".to_string()),
            ..Default::default()
        };

        assert!(should_restart_codex_task_for_ci_fix(Some(&task_state)));
    }

    #[test]
    fn workspace_ordering_keeps_archived_last_and_newest_first() {
        let mut snapshots = HashMap::new();

        let mut oldest = snapshot(false, None);
        oldest.persistent.created_at = Some(UNIX_EPOCH + Duration::from_secs(1));

        let mut newest = snapshot(false, None);
        newest.persistent.created_at = Some(UNIX_EPOCH + Duration::from_secs(3));

        let mut no_timestamp = snapshot(false, None);
        no_timestamp.persistent.created_at = None;

        let mut archived_newest = snapshot(false, None);
        archived_newest.persistent.archived = true;
        archived_newest.persistent.created_at = Some(UNIX_EPOCH + Duration::from_secs(4));

        snapshots.insert("oldest".to_string(), oldest);
        snapshots.insert("newest".to_string(), newest);
        snapshots.insert("no-timestamp".to_string(), no_timestamp);
        snapshots.insert("archived-newest".to_string(), archived_newest);

        let mut keys = vec![
            "oldest".to_string(),
            "newest".to_string(),
            "no-timestamp".to_string(),
            "archived-newest".to_string(),
        ];
        keys.sort_by(|left, right| workspace_ordering(left, right, &snapshots));

        assert_eq!(
            keys,
            vec![
                "newest".to_string(),
                "oldest".to_string(),
                "no-timestamp".to_string(),
                "archived-newest".to_string(),
            ]
        );
    }

    #[test]
    fn handler_config_defaults_review_pty_to_false() {
        let handler = HandlerConfig::default();
        assert!(!handler.review_pty);
    }

    #[test]
    fn handler_config_parses_review_pty_from_toml() {
        let handler: HandlerConfig = toml::from_str(
            "review = \"/usr/bin/smerge\"\nreview-pty = true\nweb = \"/usr/bin/firefox {}\"\n",
        )
        .expect("handler config should parse");
        assert!(handler.review_pty);
    }
}
