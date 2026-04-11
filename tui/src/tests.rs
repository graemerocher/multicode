use crate::*;

#[cfg(test)]
mod tests {
    use multicode_lib::{AutomationAgentState, services::HandlerConfig};

    use super::*;
    use crate::app::{
        compact_github_tooltip_target, should_auto_resume_autonomous_codex_after_attach,
        starting_modal_failure_status,
    };
    use crate::icons::{
        icon_glyph, issue_icon_kind_and_color, pr_build_icon_color, pr_icon_kind_and_color,
        pr_review_icon_color,
    };
    use crate::ops::{
        SessionWaitState, attach_cli_args, build_handler_command, command_exists,
        session_wait_state_for_entry, tmux_session_command, tmux_status_left,
        validate_workspace_link_target, workspace_attach_target, workspace_ordering,
    };
    use crate::render::selected_link_tooltip_area;
    use crate::system::{
        centered_rect_fixed, disk_usage_from_statvfs, machine_cpu_percent, parse_proc_cpu_totals,
        parse_proc_meminfo_total_ram_bytes, parse_proc_meminfo_used_ram_bytes,
        started_workspace_attach_ready,
    };
    use multicode_lib::{
        PersistentWorkspaceSnapshot, RuntimeBackend, RuntimeHandleSnapshot,
        TransientWorkspaceSnapshot,
    };
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::broadcast;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "multicode-tui-test-{}-{}",
                std::process::id(),
                unique
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

    #[test]
    fn should_request_autonomous_issue_scan_for_assigned_workspace_without_active_issue() {
        let mut stopped = WorkspaceSnapshot::default();
        stopped.persistent.assigned_repository =
            Some("micronaut-projects/micronaut-serialization".to_string());
        assert!(crate::app::should_request_autonomous_issue_scan(&stopped));

        stopped.persistent.automation_issue = Some(
            "https://github.com/micronaut-projects/micronaut-serialization/issues/989".to_string(),
        );
        assert!(!crate::app::should_request_autonomous_issue_scan(&stopped));

        let unassigned = WorkspaceSnapshot::default();
        assert!(!crate::app::should_request_autonomous_issue_scan(
            &unassigned
        ));
    }

    #[test]
    fn auto_resume_after_attach_requires_autonomous_issue_workspace() {
        let mut snapshot = WorkspaceSnapshot::default();
        snapshot.persistent.assigned_repository = Some("example/repo".to_string());
        snapshot.persistent.automation_issue =
            Some("https://github.com/example/repo/issues/42".to_string());
        snapshot.root_session_status = Some(RootSessionStatus::Question);

        assert!(should_auto_resume_autonomous_codex_after_attach(&snapshot));

        snapshot.root_session_status = Some(RootSessionStatus::Idle);
        assert!(should_auto_resume_autonomous_codex_after_attach(&snapshot));

        snapshot.root_session_status = Some(RootSessionStatus::Busy);
        assert!(should_auto_resume_autonomous_codex_after_attach(&snapshot));

        snapshot.root_session_status = Some(RootSessionStatus::Question);
        snapshot.persistent.automation_issue = None;
        assert!(!should_auto_resume_autonomous_codex_after_attach(&snapshot));
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
                uri: "ws://127.0.0.1:3456".to_string(),
                thread_id: Some("thread-123".to_string()),
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
                "--last".to_string(),
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
            let repo_dir = workspace_dir.join("core12299").join("micronaut-core");
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
            assert!(err.to_string().contains("outside workspace directory"));
        });
    }

    #[test]
    fn validate_workspace_link_target_rejects_repo_without_git_folder() {
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
                .expect_err("repo without .git folder should be rejected");
            assert!(err.to_string().contains("must contain a '.git' folder"));
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
        started.persistent.agent_provided.repo = vec!["/tmp/repo-a".to_string()];
        started.persistent.agent_provided.issue = vec!["https://example.com/issue/1".to_string()];
        started.persistent.agent_provided.pr = vec!["https://example.com/pull/2".to_string()];

        let links = workspace_links(&started);
        assert_eq!(
            links,
            vec![
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
    fn selected_link_tooltip_area_prefers_space_below_link() {
        let targets = vec![("target".to_string(), false)];
        let area = selected_link_tooltip_area(
            Rect::new(0, 0, 80, 20),
            2,
            WorkspaceLinkKind::Review,
            &targets,
            [10, 10, 5, 5, 5, 2, 2, 2, 2, 2],
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
            [10, 10, 5, 5, 5, 2, 2, 2, 2, 2],
        )
        .expect("tooltip area should exist");

        assert_eq!(area.x, 47);
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
            1,
            Some(0),
            false,
            false,
            Some(WorkspaceLinkKind::Issue),
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

        assert!(text.contains("←/→ select link"));
        assert!(text.contains("Enter open link"));
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
            1,
            Some(0),
            true,
            false,
            Some(WorkspaceLinkKind::Issue),
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
            1,
            Some(0),
            true,
            true,
            Some(WorkspaceLinkKind::Issue),
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
            1,
            Some(0),
            false,
            false,
            None,
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
        assert!(!enabled_text.contains("r recheck GH status"));

        let disabled_line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            1,
            Some(0),
            false,
            false,
            None,
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
            0,
            None,
            false,
            false,
            None,
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
            0,
            None,
            false,
            false,
            None,
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
            0,
            None,
            false,
            false,
            None,
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
    fn help_line_shows_issue_hotkey_for_workspace_with_assigned_repository() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.assigned_repository = Some("example/repo".to_string());
        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            0,
            None,
            false,
            false,
            None,
            false,
            true,
            no_tool_hotkeys(),
            "",
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("i issue"));
    }

    #[test]
    fn help_line_hides_issue_hotkey_without_assigned_repository() {
        let started = snapshot(true, Some("http://example"));
        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            0,
            None,
            false,
            false,
            None,
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
    }

    #[test]
    fn help_line_shows_delete_hotkey_for_workspace_row_focus() {
        let started = snapshot(true, Some("http://example"));
        let line = help_line(
            UiMode::Normal,
            1,
            1,
            Some(&started),
            0,
            None,
            false,
            false,
            None,
            false,
            true,
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
    fn help_line_shows_starting_message_in_starting_modal() {
        let line = help_line(
            UiMode::StartingModal,
            1,
            1,
            Some(&snapshot(false, None)),
            0,
            None,
            false,
            false,
            None,
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
    fn help_line_shows_confirm_delete_message() {
        let line = help_line(
            UiMode::ConfirmDelete,
            1,
            1,
            Some(&snapshot(false, None)),
            0,
            None,
            false,
            false,
            None,
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

        assert!(text.contains("Delete workspace:"));
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
            0,
            None,
            false,
            false,
            None,
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
            0,
            None,
            false,
            false,
            None,
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
            0,
            None,
            false,
            false,
            None,
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
        started.persistent.automation_issue =
            Some("https://github.com/example/repo/issues/42".to_string());
        started.automation_agent_state = Some(AutomationAgentState::Question);

        assert_eq!(server_cell_label(&started), "Question");
    }

    #[test]
    fn server_cell_label_uses_automation_review_state_as_idle() {
        let mut started = snapshot(true, Some("http://example"));
        started.persistent.automation_issue =
            Some("https://github.com/example/repo/issues/42".to_string());
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
        assert_eq!(cost_cell_label(&snapshot), "1 234 567");

        snapshot.usage_total_cost = None;
        assert_eq!(cost_cell_label(&snapshot), "1 234 567");
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
            0,
            None,
            false,
            false,
            None,
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
            0,
            None,
            false,
            false,
            None,
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
            0,
            None,
            false,
            false,
            None,
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
        assert_eq!(pr_width, content_width("PR").max(LINK_COLUMN_WIDTH));
        assert_eq!(build_width, content_width("B").max(STATUS_COLUMN_WIDTH));
        assert_eq!(
            review_width,
            content_width("R").max(REVIEW_STATUS_COLUMN_WIDTH)
        );
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
