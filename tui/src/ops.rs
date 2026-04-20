use crate::*;
use multicode_lib::services::{CompareConfig, CompareTool};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn vscode_command_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from(
        "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
    )];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home)
                .join("Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"),
        );
    }
    candidates
}

fn intellij_command_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/Applications/IntelliJ IDEA.app/Contents/MacOS/idea"),
        PathBuf::from("/Applications/IntelliJ IDEA Ultimate.app/Contents/MacOS/idea"),
        PathBuf::from("/Applications/IntelliJ IDEA CE.app/Contents/MacOS/idea"),
        PathBuf::from("/Applications/IntelliJ IDEA Community Edition.app/Contents/MacOS/idea"),
        PathBuf::from("/usr/local/bin/idea"),
        PathBuf::from("/opt/idea/bin/idea.sh"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates
            .push(PathBuf::from(&home).join("Applications/IntelliJ IDEA.app/Contents/MacOS/idea"));
        candidates.push(
            PathBuf::from(&home)
                .join("Applications/IntelliJ IDEA Ultimate.app/Contents/MacOS/idea"),
        );
        candidates.push(
            PathBuf::from(&home).join("Applications/IntelliJ IDEA CE.app/Contents/MacOS/idea"),
        );
        candidates.push(
            PathBuf::from(&home)
                .join("Applications/IntelliJ IDEA Community Edition.app/Contents/MacOS/idea"),
        );
        candidates.push(
            PathBuf::from(&home).join("Library/Application Support/JetBrains/Toolbox/scripts/idea"),
        );
    }
    candidates
}

fn compare_command_candidates(tool: CompareTool) -> Vec<PathBuf> {
    match tool {
        CompareTool::Vscode => {
            let mut candidates = vec![PathBuf::from("code")];
            candidates.extend(vscode_command_candidates());
            candidates
        }
        CompareTool::Intellij => {
            let mut candidates = vec![PathBuf::from("idea")];
            candidates.extend(intellij_command_candidates());
            candidates
        }
    }
}

pub(crate) fn compare_tool_name(tool: CompareTool) -> &'static str {
    match tool {
        CompareTool::Vscode => "VS Code",
        CompareTool::Intellij => "IntelliJ IDEA",
    }
}

fn compare_command_path(config: &CompareConfig) -> Option<PathBuf> {
    if let Some(command) = config.command.as_deref().map(str::trim)
        && !command.is_empty()
        && command_exists(command)
    {
        return Some(PathBuf::from(command));
    }

    compare_command_candidates(config.tool)
        .into_iter()
        .find(|candidate| command_exists(candidate.to_string_lossy().as_ref()))
}

pub(crate) fn compare_tool_is_available(config: &CompareConfig) -> bool {
    compare_command_path(config).is_some()
}

fn compare_open_command(
    config: &CompareConfig,
    paths: &[PathBuf],
) -> io::Result<(String, Vec<String>)> {
    let program = compare_command_path(config).ok_or_else(|| {
        let configured = config.command.as_deref().map(str::trim).unwrap_or_default();
        if configured.is_empty() {
            io::Error::other(format!(
                "{} is not installed or its CLI launcher is unavailable",
                compare_tool_name(config.tool)
            ))
        } else {
            io::Error::other(format!(
                "{} compare command '{}' is unavailable",
                compare_tool_name(config.tool),
                configured
            ))
        }
    })?;

    let mut args = Vec::new();
    if matches!(config.tool, CompareTool::Vscode) {
        args.push("--reuse-window".to_string());
    }
    args.extend(paths.iter().map(|path| path.to_string_lossy().into_owned()));

    Ok((program.to_string_lossy().into_owned(), args))
}

pub(crate) fn compare_open_repo_command(
    compare: &CompareConfig,
    repo_path: &Path,
) -> io::Result<(String, Vec<String>)> {
    compare_open_command(compare, &[repo_path.to_path_buf()])
}

pub(crate) fn shell_escape_arg(arg: &str) -> String {
    if arg.is_empty() {
        "''".to_string()
    } else if arg
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | ':' | '_' | '-' | '.' | '='))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

pub(crate) fn format_command_line(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(shell_escape_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn workspace_attach_target(snapshot: &WorkspaceSnapshot) -> io::Result<AttachTarget> {
    if workspace_state(snapshot) != WorkspaceUiState::Started {
        return Err(io::Error::other(
            "workspace must be in Started state before attaching",
        ));
    }

    let uri = snapshot
        .transient
        .as_ref()
        .map(|transient| transient.uri.as_str())
        .ok_or_else(|| io::Error::other("workspace is missing transient attach URI"))?;

    let mut parsed = Url::parse(uri)
        .map_err(|err| io::Error::other(format!("workspace attach URI is invalid: {err}")))?;

    if matches!(parsed.scheme(), "ws" | "wss") {
        return Ok(AttachTarget::Codex {
            uri: parsed.to_string(),
            thread_id: snapshot.root_session_id.clone(),
        });
    }

    let username = parsed.username().to_string();
    if username.is_empty() {
        return Err(io::Error::other(
            "workspace attach URI is missing username credentials",
        ));
    }
    let password = parsed
        .password()
        .map(str::to_string)
        .ok_or_else(|| io::Error::other("workspace attach URI is missing password credentials"))?;

    parsed
        .set_username("")
        .map_err(|_| io::Error::other("failed to sanitize workspace attach URI username"))?;
    parsed
        .set_password(None)
        .map_err(|_| io::Error::other("failed to sanitize workspace attach URI password"))?;

    Ok(AttachTarget::Opencode {
        uri: parsed.to_string(),
        username,
        password,
        session_id: snapshot.root_session_id.clone(),
    })
}

pub(crate) fn task_attach_target(
    snapshot: &WorkspaceSnapshot,
    task_state: &multicode_lib::WorkspaceTaskRuntimeSnapshot,
) -> io::Result<AttachTarget> {
    if workspace_state(snapshot) != WorkspaceUiState::Started {
        return Err(io::Error::other(
            "workspace must be in Started state before attaching",
        ));
    }

    let uri = snapshot
        .transient
        .as_ref()
        .map(|transient| transient.uri.as_str())
        .ok_or_else(|| io::Error::other("workspace is missing transient attach URI"))?;

    let mut parsed = Url::parse(uri)
        .map_err(|err| io::Error::other(format!("workspace attach URI is invalid: {err}")))?;

    let session_id = task_state
        .session_id
        .clone()
        .ok_or_else(|| io::Error::other("task does not have a resumable session yet"))?;

    if matches!(parsed.scheme(), "ws" | "wss") {
        return Ok(AttachTarget::Codex {
            uri: parsed.to_string(),
            thread_id: Some(session_id),
        });
    }

    let username = parsed.username().to_string();
    if username.is_empty() {
        return Err(io::Error::other(
            "workspace attach URI is missing username credentials",
        ));
    }
    let password = parsed
        .password()
        .map(str::to_string)
        .ok_or_else(|| io::Error::other("workspace attach URI is missing password credentials"))?;

    parsed
        .set_username("")
        .map_err(|_| io::Error::other("failed to sanitize workspace attach URI username"))?;
    parsed
        .set_password(None)
        .map_err(|_| io::Error::other("failed to sanitize workspace attach URI password"))?;

    Ok(AttachTarget::Opencode {
        uri: parsed.to_string(),
        username,
        password,
        session_id: Some(session_id),
    })
}

pub(crate) fn build_handler_command(
    template: &str,
    argument_mode: multicode_lib::HandlerArgumentMode,
    argument: &str,
) -> io::Result<(String, Vec<String>)> {
    multicode_lib::build_handler_command(template.trim(), argument_mode, argument)
}

pub(crate) async fn validate_workspace_link_target(
    link: &WorkspaceLink,
    workspace_dir: &Path,
) -> io::Result<String> {
    match link.kind {
        WorkspaceLinkKind::Review => {
            let repo_path = PathBuf::from(link.value.trim());
            if !repo_path.is_absolute() {
                return Err(io::Error::other("review link must be an absolute path"));
            }

            let workspace_root = tokio::fs::canonicalize(workspace_dir)
                .await
                .map_err(|err| {
                    io::Error::other(format!(
                        "failed to resolve workspace directory '{}': {err}",
                        workspace_dir.display()
                    ))
                })?;
            let repo_path = tokio::fs::canonicalize(&repo_path).await.map_err(|err| {
                io::Error::other(format!(
                    "review path '{}' is not accessible: {err}",
                    link.value
                ))
            })?;

            if !repo_path.starts_with(&workspace_root) {
                return Err(io::Error::other(format!(
                    "review path '{}' is outside workspace directory '{}'",
                    repo_path.display(),
                    workspace_root.display()
                )));
            }
            let metadata = tokio::fs::metadata(&repo_path).await.map_err(|err| {
                io::Error::other(format!(
                    "failed to inspect review path '{}': {err}",
                    repo_path.display()
                ))
            })?;
            if !metadata.is_dir() {
                return Err(io::Error::other(format!(
                    "review path '{}' is not a directory",
                    repo_path.display()
                )));
            }

            let git_dir = repo_path.join(".git");
            tokio::fs::symlink_metadata(&git_dir).await.map_err(|err| {
                io::Error::other(format!(
                    "review path '{}' must contain a '.git' entry: {err}",
                    repo_path.display()
                ))
            })?;

            Ok(repo_path.to_string_lossy().into_owned())
        }
        WorkspaceLinkKind::Issue | WorkspaceLinkKind::Pr => {
            let url = Url::parse(link.value.trim()).map_err(|err| {
                io::Error::other(format!("web link '{}' is invalid: {err}", link.value))
            })?;
            if url.scheme() != "https" {
                return Err(io::Error::other(format!(
                    "web link '{}' must use https",
                    link.value
                )));
            }
            if url.host_str().is_none() {
                return Err(io::Error::other(format!(
                    "web link '{}' must include a host",
                    link.value
                )));
            }
            Ok(url.to_string())
        }
    }
}

pub(crate) fn attach_cli_args(agent_command: &str, target: &AttachTarget) -> Vec<String> {
    match target {
        AttachTarget::Opencode {
            uri, session_id, ..
        } => {
            let mut args = vec![agent_command.to_string(), "attach".to_string()];
            if let Some(session_id) = session_id.as_deref() {
                args.push("--session".to_string());
                args.push(session_id.to_string());
            }
            args.push(uri.clone());
            args
        }
        AttachTarget::Codex { uri, thread_id } => {
            let mut args = vec![
                agent_command.to_string(),
                "resume".to_string(),
                "--remote".to_string(),
                uri.clone(),
            ];
            if let Some(thread_id) = thread_id.as_deref() {
                args.push(thread_id.to_string());
            } else {
                args.push("--last".to_string());
            }
            args
        }
        AttachTarget::CodexNew { uri, cwd, prompt } => {
            let mut args = vec![
                agent_command.to_string(),
                "--remote".to_string(),
                uri.clone(),
            ];
            if let Some(cwd) = cwd.as_deref() {
                args.push("-C".to_string());
                args.push(cwd.to_string());
            }
            if let Some(prompt) = prompt.as_deref() {
                args.push(prompt.to_string());
            }
            args
        }
    }
}

pub(crate) fn tmux_session_command(
    command: Vec<String>,
    original_term: Option<&str>,
) -> Vec<String> {
    let mut tmux_command = Vec::with_capacity(command.len() + 4);
    tmux_command.push("env".to_string());
    if let Some(term) = original_term {
        tmux_command.push(format!("TERM={term}"));
    }
    tmux_command.extend(command);
    tmux_command
}

pub(crate) async fn attach_in_tmux(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agent_command: &str,
    target: &AttachTarget,
    cwd: Option<&Path>,
    extra_env: &[(String, String)],
    workspace_key: &str,
    custom_description: &str,
) -> io::Result<()> {
    let original_term = std::env::var("TERM").ok();
    let mut session_env = extra_env.to_vec();
    if let Some(term) = original_term.as_deref() {
        session_env.push(("TERM".to_string(), term.to_string()));
    }
    let attach_env = extra_env
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>();
    if let AttachTarget::Opencode {
        username, password, ..
    } = target
    {
        session_env.push(("OPENCODE_SERVER_USERNAME".to_string(), username.to_string()));
        session_env.push(("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string()));
    }
    let mut attach_command = tmux_session_command(attach_env, None);
    attach_command.extend(attach_cli_args(agent_command, target));
    run_tmux_new_session_command(
        terminal,
        &session_env,
        attach_command,
        cwd,
        workspace_key,
        custom_description,
    )
    .await
}

pub(crate) async fn run_tmux_new_session_command(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    env: &[(String, String)],
    command: Vec<String>,
    cwd: Option<&Path>,
    workspace_key: &str,
    custom_description: &str,
) -> io::Result<()> {
    if !command_exists("tmux") {
        let debug_command = command
            .split_first()
            .map(|(program, args)| format_command_line(program, args))
            .unwrap_or_else(|| "<empty command>".to_string());
        tracing::info!(
            command = %debug_command,
            "tmux unavailable; running interactive command directly"
        );
        return run_interactive_command(terminal, env, &command, cwd).await;
    }

    restore_terminal(terminal)?;

    let session_name = generate_tmux_session_name(workspace_key);
    let status_left = tmux_status_left(workspace_key, custom_description);
    let status_left_length = status_left.chars().count().max(32).min(1024).to_string();

    let mut run_error = None;

    let mut create_command = vec![
        "new-session".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        session_name.clone(),
        "env".to_string(),
    ];
    create_command.extend(env.iter().map(|(name, value)| format!("{name}={value}")));
    create_command.extend(command.clone());
    tracing::info!(
        command = %format_command_line("tmux", &create_command),
        "starting application via tmux new-session"
    );

    let mut create_process = Command::new("tmux");
    create_process.env("TERM", "xterm-256color");
    create_process
        .arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(&session_name);
    if let Some(cwd) = cwd {
        create_process.arg("-c").arg(cwd);
    }
    let create_result = create_process
        .arg("env")
        .args(env.iter().map(|(name, value)| format!("{name}={value}")))
        .args(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await;

    match create_result {
        Ok(status) if status.success() => {}
        Ok(status) => {
            run_error = Some(io::Error::other(format!(
                "tmux new-session exited with status {status}"
            )));
        }
        Err(err) => {
            run_error = Some(err);
        }
    }

    if run_error.is_none() {
        if let Err(err) =
            set_tmux_session_option(&session_name, "status-left", status_left.as_str()).await
        {
            run_error = Some(err);
        }
    }

    if run_error.is_none() {
        if let Err(err) = set_tmux_session_option(
            &session_name,
            "status-left-length",
            status_left_length.as_str(),
        )
        .await
        {
            run_error = Some(err);
        }
    }

    if run_error.is_none() {
        tracing::info!(
            command = %format_command_line("tmux", &[
                "attach-session".to_string(),
                "-t".to_string(),
                session_name.clone(),
            ]),
            "starting application via tmux attach-session"
        );
        let mut attach_process = Command::new("tmux");
        attach_process.env("TERM", "xterm-256color");
        match attach_process
            .arg("attach-session")
            .arg("-t")
            .arg(&session_name)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
        {
            Ok(status) if status.success() => {}
            Ok(status) => {
                if tmux_session_exists(&session_name).await? {
                    run_error = Some(io::Error::other(format!(
                        "tmux attach-session exited with status {status}"
                    )));
                }
            }
            Err(err) => {
                run_error = Some(err);
            }
        }
    }

    let setup_result = setup_terminal().map(|new_terminal| {
        *terminal = new_terminal;
    });

    match (run_error, setup_result) {
        (_, Err(err)) => Err(err),
        (Some(err), Ok(())) => Err(err),
        (None, Ok(())) => Ok(()),
    }
}

pub(crate) fn command_exists(command: &str) -> bool {
    if command.contains('/') {
        return is_executable_file(Path::new(command));
    }

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .map(|directory| directory.join(command))
        .any(|candidate| is_executable_file(&candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

async fn run_interactive_command(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    env: &[(String, String)],
    command: &[String],
    cwd: Option<&Path>,
) -> io::Result<()> {
    let Some((program, args)) = command.split_first() else {
        return Err(io::Error::other("interactive command must not be empty"));
    };

    restore_terminal(terminal)?;
    let mut process = Command::new(program);
    process
        .args(args)
        .envs(env.iter().cloned())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    let status = process.status().await;
    let setup_result = setup_terminal().map(|new_terminal| {
        *terminal = new_terminal;
    });

    match (status, setup_result) {
        (_, Err(err)) => Err(err),
        (Ok(status), Ok(())) if status.success() => Ok(()),
        (Ok(status), Ok(())) => Err(io::Error::other(format!(
            "interactive command exited with status {status}"
        ))),
        (Err(err), Ok(())) => Err(err),
    }
}

pub(crate) async fn set_tmux_session_option(
    session_name: &str,
    option: &str,
    value: &str,
) -> io::Result<()> {
    tracing::info!(
        command = %format_command_line("tmux", &[
            "set-option".to_string(),
            "-t".to_string(),
            session_name.to_string(),
            option.to_string(),
            value.to_string(),
        ]),
        "starting application via tmux set-option"
    );
    let status = Command::new("tmux")
        .arg("set-option")
        .arg("-t")
        .arg(session_name)
        .arg(option)
        .arg(value)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;

    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "tmux set-option {option} exited with status {status}"
        )))
    }
}

async fn tmux_session_exists(session_name: &str) -> io::Result<bool> {
    let status = Command::new("tmux")
        .arg("has-session")
        .arg("-t")
        .arg(session_name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;
    Ok(status.success())
}

pub(crate) fn generate_tmux_session_name(workspace_key: &str) -> String {
    let sanitized: String = workspace_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("multicode-{sanitized}-{suffix}")
}

pub(crate) fn tmux_status_left(workspace_key: &str, custom_description: &str) -> String {
    let workspace_key = workspace_key.replace('#', "##");
    let custom_description = custom_description.trim();
    if custom_description.is_empty() {
        format!("{workspace_key} | ")
    } else {
        let custom_description = custom_description.replace('#', "##");
        format!("{workspace_key} | {custom_description} | ")
    }
}

pub(crate) async fn run_prompt_tool_workflow(
    client: std::sync::Arc<opencode::client::Client>,
    events: tokio::sync::broadcast::Sender<opencode::client::types::GlobalEvent>,
    root_session_id: String,
    prompt: String,
    progress_tx: watch::Sender<String>,
) -> Result<(), String> {
    let _ = progress_tx.send("Forking root session...".to_string());
    let root_session_id =
        opencode::client::types::SessionForkSessionId::try_from(root_session_id.as_str())
            .map_err(|err| format!("invalid root session id: {err}"))?;
    let temp_session_id = client
        .session_fork(
            &root_session_id,
            None,
            None,
            &opencode::client::types::SessionForkBody::default(),
        )
        .await
        .map_err(|err| format!("failed to fork root session: {err}"))?
        .into_inner()
        .id
        .to_string();

    let run_result = async {
        let _ = progress_tx.send("Sending tool prompt...".to_string());
        let prompt_body = opencode::client::types::SessionPromptBody {
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
            tools: HashMap::new(),
            variant: None,
        };
        let temp_session_id = temp_session_id
            .parse::<opencode::client::types::SessionPromptSessionId>()
            .map_err(|error| {
                format!("temporary session ID '{temp_session_id}' is invalid: {error}")
            })?;
        client
            .session_prompt(&temp_session_id, None, None, &prompt_body)
            .await
            .map_err(|err| format!("failed to send tool prompt: {err}"))?;

        let _ = progress_tx.send("Waiting for temporary session to become idle...".to_string());
        wait_until_session_idle(client.as_ref(), &events, &temp_session_id).await
    }
    .await;

    let _ = progress_tx.send("Deleting temporary session...".to_string());
    let delete_result = delete_session(client.as_ref(), &temp_session_id).await;

    match (run_result, delete_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(err)) => Err(err),
        (Err(err), Ok(())) => Err(err),
        (Err(run_err), Err(delete_err)) => Err(format!("{run_err}; cleanup failed: {delete_err}")),
    }
}

pub(crate) async fn wait_until_session_idle(
    client: &opencode::client::Client,
    events: &tokio::sync::broadcast::Sender<opencode::client::types::GlobalEvent>,
    session_id: &str,
) -> Result<(), String> {
    let mut event_rx = events.subscribe();
    let wait_started_at = Instant::now();

    match session_wait_state(client, session_id).await {
        Ok(SessionWaitState::Idle) => return Ok(()),
        Ok(SessionWaitState::Busy) => {}
        Err(err) => {
            return Err(format!("failed to query session status: {err}"));
        }
    }

    loop {
        let Some(wait_remaining) = PROMPT_TOOL_IDLE_TIMEOUT.checked_sub(wait_started_at.elapsed())
        else {
            return Err(format!(
                "timed out waiting for temporary session '{session_id}' to become idle"
            ));
        };

        match tokio::time::timeout(wait_remaining, event_rx.recv()).await {
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                match session_wait_state(client, session_id).await {
                    Ok(SessionWaitState::Idle) => return Ok(()),
                    Ok(SessionWaitState::Busy) => {}
                    Err(err) => {
                        return Err(format!("failed to query session status: {err}"));
                    }
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                return Err(
                    "opencode event stream closed while waiting for prompt tool".to_string()
                );
            }
            Err(_) => {
                return Err(format!(
                    "timed out waiting for temporary session '{session_id}' to become idle"
                ));
            }
        }
    }
}

pub(crate) enum SessionWaitState {
    Idle,
    Busy,
}

pub(crate) fn session_wait_state_for_entry(
    status: Option<&opencode::client::types::SessionStatus>,
) -> SessionWaitState {
    match status {
        Some(opencode::client::types::SessionStatus::Busy)
        | Some(opencode::client::types::SessionStatus::Retry { .. }) => SessionWaitState::Busy,
        Some(opencode::client::types::SessionStatus::Idle) | None => SessionWaitState::Idle,
    }
}

pub(crate) async fn session_wait_state(
    client: &opencode::client::Client,
    session_id: &str,
) -> Result<SessionWaitState, opencode::client::Error<opencode::client::types::BadRequestError>> {
    let status = client.session_status(None, None).await?.into_inner();
    Ok(session_wait_state_for_entry(status.get(session_id)))
}

pub(crate) async fn delete_session(
    client: &opencode::client::Client,
    session_id: &str,
) -> Result<(), String> {
    let session_id = opencode::client::types::SessionDeleteSessionId::try_from(session_id)
        .map_err(|err| format!("invalid temporary session id: {err}"))?;
    client
        .session_delete(&session_id, None, None)
        .await
        .map_err(|err| format!("failed to delete temporary session: {err}"))?;
    Ok(())
}

pub(crate) fn created_at_sort_key(created_at: Option<SystemTime>) -> u128 {
    created_at
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

pub(crate) fn workspace_ordering(
    left_key: &str,
    right_key: &str,
    snapshots: &HashMap<String, WorkspaceSnapshot>,
) -> std::cmp::Ordering {
    let left = snapshots.get(left_key);
    let right = snapshots.get(right_key);
    let left_archived = left.map(|s| s.persistent.archived).unwrap_or(false);
    let right_archived = right.map(|s| s.persistent.archived).unwrap_or(false);
    let left_created_at = left.and_then(|s| s.persistent.created_at);
    let right_created_at = right.and_then(|s| s.persistent.created_at);

    left_archived
        .cmp(&right_archived)
        .then_with(|| {
            created_at_sort_key(right_created_at).cmp(&created_at_sort_key(left_created_at))
        })
        .then_with(|| left_key.cmp(right_key))
}
