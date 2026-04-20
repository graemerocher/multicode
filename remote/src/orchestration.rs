use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use multicode_lib::{RemoteActionRequest, decode_remote_action_request};
use multicode_lib::{
    database::Database,
    logging,
    services::{
        GithubStatusService,
        config::{Config, RemoteSyncMappingConfig},
    },
    tree_scan,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt},
    net::UnixListener,
    process::Command,
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(name = "multicode-remote")]
pub struct CliArgs {
    pub config_path: PathBuf,
    #[arg(long = "ssh")]
    pub ssh_uri: String,
}

#[derive(Debug, Clone)]
pub struct RemoteCliOptions {
    pub ssh_port: Option<u16>,
    pub ssh_identity_file: Option<PathBuf>,
    pub ssh_known_hosts_file: Option<PathBuf>,
    pub ssh_strict_host_key_checking: bool,
    pub remote_tui_sanity_check: bool,
}

impl Default for RemoteCliOptions {
    fn default() -> Self {
        Self {
            ssh_port: None,
            ssh_identity_file: None,
            ssh_known_hosts_file: None,
            ssh_strict_host_key_checking: true,
            remote_tui_sanity_check: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteCliDependencies {
    pub local_tui_binary_override: Option<PathBuf>,
    pub local_tui_stage_root_override: Option<PathBuf>,
}

impl Default for RemoteCliDependencies {
    fn default() -> Self {
        Self {
            local_tui_binary_override: None,
            local_tui_stage_root_override: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteIntegrationResult {
    pub remote_root: PathBuf,
    pub remote_tui_path: PathBuf,
    pub remote_config_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRuntimeConfig {
    remote_workspace_directory: PathBuf,
    remote_home_directory: PathBuf,
    remote_architecture: RemoteArchitecture,
    sync_interval: Duration,
    sync_up: Vec<ResolvedSyncPathMapping>,
    sync_bidi: Vec<ResolvedSyncPathMapping>,
    config_support_sync_up: Vec<ResolvedSyncPathMapping>,
    rewritten_remote_config: Config,
    install_command: String,
    github_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteArchitecture {
    X86_64,
    Aarch64,
}

impl RemoteArchitecture {
    fn target_triple(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-linux-gnu",
            Self::Aarch64 => "aarch64-unknown-linux-gnu",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectorySyncTarget {
    ExactDestination,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedSyncPathMapping {
    local: PathBuf,
    remote: PathBuf,
    exclude: Vec<String>,
    dereference_symlinks: bool,
    local_is_dir: bool,
    directory_sync_target: DirectorySyncTarget,
}

pub async fn run_remote_cli(
    args: CliArgs,
    options: RemoteCliOptions,
    dependencies: RemoteCliDependencies,
) -> io::Result<RemoteIntegrationResult> {
    logging::init_stdout_logging();
    info!(config_path = %args.config_path.display(), ssh_uri = %args.ssh_uri, "starting multicode-remote");
    info!("loading remote config");
    let config = read_remote_config(&args.config_path).await?;
    info!("resolving remote runtime config");
    let remote_home_directory = fetch_remote_home_directory(&args, &options).await?;
    info!(remote_home = %remote_home_directory.display(), "resolved remote home directory");
    let remote_architecture = fetch_remote_architecture(&args, &options).await?;
    info!(remote_architecture = ?remote_architecture, "resolved remote architecture");
    let remote_config = resolve_runtime_config(
        &config,
        Some(&args.config_path),
        &remote_home_directory,
        remote_architecture,
    )
    .await?;
    let result = run_remote_session(&args, &remote_config, &options, &dependencies).await?;
    Ok(result)
}

async fn run_remote_session(
    args: &CliArgs,
    config: &ResolvedRuntimeConfig,
    options: &RemoteCliOptions,
    dependencies: &RemoteCliDependencies,
) -> io::Result<RemoteIntegrationResult> {
    info!("running remote install command");
    run_ssh_command(args, options, &build_install_command(config)).await?;
    info!("ensuring remote runtime directories exist");
    ensure_remote_runtime_directories(args, config, options).await?;
    info!("syncing multicode-tui binary");
    sync_remote_tui_binary(args, config, options, dependencies).await?;
    info!(
        mapping_count = config.config_support_sync_up.len(),
        "syncing config support paths"
    );
    sync_skill_mappings_up(args, &config.config_support_sync_up, options).await?;
    info!("syncing rewritten remote config file");
    sync_remote_config_file(args, config, options).await?;
    sync_skill_mappings_up(args, &config.config_support_sync_up, options).await?;
    info!(
        mapping_count = config.sync_up.len(),
        "syncing upload-only paths"
    );
    sync_mappings_up(args, &config.sync_up, options).await?;
    info!(
        mapping_count = config.sync_bidi.len(),
        "syncing bidirectional paths up"
    );
    let layout = remote_layout(config)?;
    sync_bidi_mappings_up_if_local_is_not_older(args, &layout, &config.sync_bidi, options).await?;

    info!("preparing relay socket forwarding");
    let relay = prepare_relay(args, config, options).await?;
    info!(local_socket = %relay.local_socket_path.display(), remote_socket = %relay.remote_socket_path.display(), "starting relay listener");
    let relay_handle = spawn_relay_listener(
        relay.local_socket_path.clone(),
        args.config_path.clone(),
        config.sync_bidi.clone(),
    );

    let bidi_handle = if config.sync_bidi.is_empty() {
        None
    } else {
        info!(
            interval_seconds = config.sync_interval.as_secs(),
            "starting bidirectional download sync loop"
        );
        Some(spawn_bidi_sync_loop(
            args.clone(),
            config.clone(),
            options.clone(),
        ))
    };

    info!("launching remote multicode-tui session");
    let stdout_logging_guard = logging::suppress_stdout_logging();
    let status = Command::new("ssh")
        .args(build_ssh_interactive_args_with_forwarding(
            &args.ssh_uri,
            &[(&relay.local_socket_path, &relay.remote_socket_path)],
            options,
        ))
        .arg(build_remote_tui_command_with_relay_and_options(
            config,
            Some(relay.remote_socket_path.as_path()),
            options,
        ))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;
    drop(stdout_logging_guard);

    info!(status = %status, "remote multicode-tui session finished");
    if let Some(handle) = bidi_handle {
        handle.abort();
        let _ = handle.await;
    }
    relay_handle.abort();
    let _ = relay_handle.await;
    let _ = tokio::fs::remove_file(&relay.local_socket_path).await;

    info!(
        mapping_count = config.sync_bidi.len(),
        "syncing bidirectional paths down"
    );
    sync_mappings_down(args, &config.sync_bidi, options, true).await?;

    if status.success() {
        let layout = remote_layout(config)?;
        Ok(RemoteIntegrationResult {
            remote_root: layout.root,
            remote_tui_path: layout.remote_tui_path,
            remote_config_path: layout.remote_config_path,
        })
    } else {
        Err(io::Error::other(format!(
            "ssh session exited with status {status}"
        )))
    }
}

#[derive(Debug, Clone)]
struct RelayPaths {
    local_socket_path: PathBuf,
    remote_socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteLayout {
    root: PathBuf,
    remote_tui_path: PathBuf,
    remote_config_path: PathBuf,
    remote_relay_dir: PathBuf,
}

async fn prepare_relay(
    args: &CliArgs,
    config: &ResolvedRuntimeConfig,
    options: &RemoteCliOptions,
) -> io::Result<RelayPaths> {
    let layout = remote_layout(config)?;
    let local_socket_path = std::env::temp_dir().join(format!(
        "multicode-remote-{}-{}.sock",
        std::process::id(),
        Uuid::new_v4()
    ));
    let remote_socket_path = layout
        .remote_relay_dir
        .join(format!("{}.sock", Uuid::new_v4()));
    let _ = tokio::fs::remove_file(&local_socket_path).await;
    run_ssh_command(
        args,
        options,
        &format!(
            "rm -f {}",
            shell_single_quote(&remote_socket_path.to_string_lossy())
        ),
    )
    .await?;
    Ok(RelayPaths {
        local_socket_path,
        remote_socket_path,
    })
}

async fn ensure_remote_runtime_directories(
    args: &CliArgs,
    config: &ResolvedRuntimeConfig,
    options: &RemoteCliOptions,
) -> io::Result<()> {
    let layout = remote_layout(config)?;
    info!(
        remote_root = %layout.root.display(),
        remote_relay_dir = %layout.remote_relay_dir.display(),
        remote_workspace_directory = %config.remote_workspace_directory.display(),
        "creating remote runtime directories"
    );
    run_ssh_command(
        args,
        options,
        &format!(
            "mkdir -p {} {} {}",
            shell_single_quote(&layout.root.to_string_lossy()),
            shell_single_quote(&layout.remote_relay_dir.to_string_lossy()),
            shell_single_quote(&config.remote_workspace_directory.to_string_lossy()),
        ),
    )
    .await
}

fn spawn_relay_listener(
    local_socket_path: PathBuf,
    config_path: PathBuf,
    sync_bidi: Vec<ResolvedSyncPathMapping>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let Ok(listener) = UnixListener::bind(&local_socket_path) else {
            return;
        };
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let config_path = config_path.clone();
            let sync_bidi = sync_bidi.clone();
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stream);
                let mut line = String::new();
                if reader
                    .read_line(&mut line)
                    .await
                    .ok()
                    .filter(|n| *n > 0)
                    .is_none()
                {
                    return;
                }
                let Ok(request) = decode_remote_action_request(line.trim()) else {
                    return;
                };
                let _ = execute_relay_request(&config_path, &sync_bidi, &request).await;
            });
        }
    })
}

async fn execute_relay_request(
    config_path: &Path,
    sync_bidi: &[ResolvedSyncPathMapping],
    request: &RemoteActionRequest,
) -> io::Result<()> {
    let raw = tokio::fs::read_to_string(config_path).await?;
    let full: Config =
        toml::from_str(&raw).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let local_argument = match request.action {
        multicode_lib::RemoteAction::Review => {
            rewrite_review_path_to_local_sync_root(sync_bidi, &request.argument)
                .unwrap_or_else(|| request.argument.clone())
        }
        multicode_lib::RemoteAction::Web => request.argument.clone(),
    };
    let handler = match request.action {
        multicode_lib::RemoteAction::Review => full.handler.review,
        multicode_lib::RemoteAction::Web => full.handler.web,
    };
    let argument_mode = match request.action {
        multicode_lib::RemoteAction::Review => multicode_lib::HandlerArgumentMode::Chdir,
        multicode_lib::RemoteAction::Web => multicode_lib::HandlerArgumentMode::Argument,
    };
    let (program, args) = build_handler_command(&handler, argument_mode, &local_argument)?;
    debug!(action = ?request.action, argument = %request.argument, local_argument = %local_argument, program = %program, args = ?args, "executing relayed local handler");
    let mut command = Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if request.action == multicode_lib::RemoteAction::Review {
        command.current_dir(&local_argument);
    }
    command.spawn().map(|_| ()).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to spawn relayed handler program '{}' with args {:?}: {}",
                program, args, err
            ),
        )
    })
}

fn build_handler_command(
    template: &str,
    argument_mode: multicode_lib::HandlerArgumentMode,
    argument: &str,
) -> io::Result<(String, Vec<String>)> {
    multicode_lib::build_handler_command(template, argument_mode, argument)
}

fn rewrite_review_path_to_local_sync_root(
    mappings: &[ResolvedSyncPathMapping],
    argument: &str,
) -> Option<String> {
    let remote = Path::new(argument);
    mappings.iter().find_map(|mapping| {
        let suffix = remote.strip_prefix(&mapping.remote).ok()?;
        Some(mapping.local.join(suffix).to_string_lossy().into_owned())
    })
}

fn spawn_bidi_sync_loop(
    args: CliArgs,
    config: ResolvedRuntimeConfig,
    options: RemoteCliOptions,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(config.sync_interval).await;
            if let Err(err) = sync_mappings_down(&args, &config.sync_bidi, &options, false).await {
                error!(error = %err, "background bidirectional rsync download failed");
            }
        }
    })
}

async fn sync_mappings_up(
    args: &CliArgs,
    mappings: &[ResolvedSyncPathMapping],
    options: &RemoteCliOptions,
) -> io::Result<()> {
    ensure_remote_directory_sync_destinations_exist(args, mappings, options).await?;
    for mapping in mappings {
        ensure_local_sync_source_exists(mapping)?;
        run_rsync_command(
            build_rsync_up_args(&args.ssh_uri, mapping, options, true)?,
            format!("rsync upload for '{}'", mapping.local.display()),
        )
        .await?;
    }
    Ok(())
}

async fn sync_skill_mappings_up(
    args: &CliArgs,
    mappings: &[ResolvedSyncPathMapping],
    options: &RemoteCliOptions,
) -> io::Result<()> {
    sync_mappings_up(args, mappings, options).await
}

fn remote_directory_sync_destination(mapping: &ResolvedSyncPathMapping) -> io::Result<&Path> {
    match mapping.directory_sync_target {
        DirectorySyncTarget::ExactDestination => Ok(mapping.remote.as_path()),
    }
}

async fn ensure_remote_directory_sync_destinations_exist(
    args: &CliArgs,
    mappings: &[ResolvedSyncPathMapping],
    options: &RemoteCliOptions,
) -> io::Result<()> {
    let targets = mappings
        .iter()
        .filter(|mapping| mapping.local_is_dir)
        .map(remote_directory_sync_destination)
        .collect::<io::Result<BTreeSet<_>>>()?;
    for target in targets {
        run_ssh_command(
            args,
            options,
            &format!("mkdir -p {}", shell_single_quote(&target.to_string_lossy())),
        )
        .await?;
    }
    Ok(())
}

fn ensure_local_sync_source_exists(mapping: &ResolvedSyncPathMapping) -> io::Result<()> {
    if mapping.local_is_dir && !mapping.local.exists() {
        std::fs::create_dir_all(&mapping.local)?;
    }
    Ok(())
}

async fn sync_bidi_mappings_up_if_local_is_not_older(
    args: &CliArgs,
    layout: &RemoteLayout,
    mappings: &[ResolvedSyncPathMapping],
    options: &RemoteCliOptions,
) -> io::Result<()> {
    let (local_latest, remote_latest) = tokio::try_join!(
        latest_local_sync_mappings_modified_time(mappings),
        latest_remote_sync_mappings_modified_time(args, layout, mappings, options),
    )?;
    info!(
        mapping_count = mappings.len(),
        local_latest = ?local_latest,
        remote_latest = ?remote_latest,
        "checked bidirectional sync modification times"
    );
    if should_sync_bidirectional_mapping_up(local_latest, remote_latest) {
        sync_mappings_up(args, mappings, options).await?;
    } else {
        warn!(
            mapping_count = mappings.len(),
            local_latest = ?local_latest,
            remote_latest = ?remote_latest,
            "skipping bidirectional sync-up because remote tree is newer"
        );
    }
    Ok(())
}

fn should_sync_bidirectional_mapping_up(
    local_latest: Option<SystemTime>,
    remote_latest: Option<SystemTime>,
) -> bool {
    match (local_latest, remote_latest) {
        (Some(_), Some(_)) => {
            compare_sync_tree_recency(local_latest, remote_latest) != Ordering::Less
        }
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => true,
    }
}

fn compare_sync_tree_recency(
    local_latest: Option<SystemTime>,
    remote_latest: Option<SystemTime>,
) -> Ordering {
    match (local_latest, remote_latest) {
        (Some(local), Some(remote)) => local.cmp(&remote),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

async fn latest_local_sync_mappings_modified_time(
    mappings: &[ResolvedSyncPathMapping],
) -> io::Result<Option<SystemTime>> {
    let mut latest = None;
    for mapping in mappings {
        latest = tree_scan::max_modified_time(latest, latest_local_tree_modified_time(mapping)?);
    }
    Ok(latest)
}

async fn latest_remote_sync_mappings_modified_time(
    args: &CliArgs,
    layout: &RemoteLayout,
    mappings: &[ResolvedSyncPathMapping],
    options: &RemoteCliOptions,
) -> io::Result<Option<SystemTime>> {
    let mut latest = None;
    for mapping in mappings {
        latest = tree_scan::max_modified_time(
            latest,
            latest_remote_tree_modified_time(args, layout, mapping, options).await?,
        );
    }
    Ok(latest)
}

fn latest_local_tree_modified_time(
    mapping: &ResolvedSyncPathMapping,
) -> io::Result<Option<SystemTime>> {
    tree_scan::latest_modified_time(
        &mapping.local,
        mapping.local_is_dir,
        &recency_excludes(mapping),
    )
}

#[cfg(test)]
fn latest_local_path_modified_time(path: &Path, is_dir: bool) -> io::Result<Option<SystemTime>> {
    tree_scan::latest_modified_time(path, is_dir, &[])
}

async fn latest_remote_tree_modified_time(
    args: &CliArgs,
    layout: &RemoteLayout,
    mapping: &ResolvedSyncPathMapping,
    options: &RemoteCliOptions,
) -> io::Result<Option<SystemTime>> {
    let command = build_remote_latest_timestamp_command(layout, mapping);
    let output = Command::new("ssh")
        .args(build_ssh_base_args(&args.ssh_uri, options))
        .arg(command)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .await?;
    if !output.status.success() {
        if remote_timestamp_probe_failed_for_missing_path(&output.stderr) {
            return Ok(None);
        }
        return Err(io::Error::other(format!(
            "failed to resolve latest remote modification timestamp for '{}' with status {}",
            mapping.remote.display(),
            output.status
        )));
    }
    parse_remote_latest_timestamp_output(&output.stdout)
}

fn remote_timestamp_probe_failed_for_missing_path(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr);
    stderr.contains("No such file or directory")
}

fn build_remote_latest_timestamp_command(
    layout: &RemoteLayout,
    mapping: &ResolvedSyncPathMapping,
) -> String {
    let remote_tui_path = shell_single_quote(&layout.remote_tui_path.to_string_lossy());
    let remote_config_path = shell_single_quote(&layout.remote_config_path.to_string_lossy());
    let remote_path = shell_single_quote(&mapping.remote.to_string_lossy());
    let excludes = recency_excludes(mapping)
        .into_iter()
        .map(|exclude| format!("--recency-scan-exclude {}", shell_single_quote(&exclude)))
        .collect::<Vec<_>>()
        .join(" ");
    let is_dir = if mapping.local_is_dir {
        "--recency-scan-is-dir"
    } else {
        ""
    };
    format!(
        "{remote_tui_path} {remote_config_path} --recency-scan-path {remote_path} {is_dir} {excludes}",
    )
}

fn parse_remote_latest_timestamp_output(stdout: &[u8]) -> io::Result<Option<SystemTime>> {
    let raw = String::from_utf8(stdout.to_vec())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "none" {
        return Ok(None);
    }
    let seconds = trimmed.parse::<u64>().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid remote timestamp '{trimmed}': {err}"),
        )
    })?;
    Ok(Some(UNIX_EPOCH + Duration::from_secs(seconds)))
}

fn recency_excludes(mapping: &ResolvedSyncPathMapping) -> Vec<String> {
    let mut exclude = BTreeSet::new();
    for pattern in &mapping.exclude {
        exclude.insert(pattern.trim_matches('/').to_string());
    }
    if mapping.local_is_dir {
        exclude.insert(".multicode/remote".to_string());
    }
    exclude
        .into_iter()
        .filter(|pattern| !pattern.is_empty())
        .collect()
}

async fn run_rsync_command(args: Vec<String>, context: String) -> io::Result<()> {
    let mut child = Command::new("rsync")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture rsync stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("failed to capture rsync stderr"))?;

    let stdout_task = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut writer = logging::stdout_writer();
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = stdout.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read])?;
        }
        writer.flush()
    });
    let stderr_task = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut buffer = Vec::new();
        stderr.read_to_end(&mut buffer).await.map(|_| buffer)
    });

    let status = child.wait().await?;
    stdout_task
        .await
        .map_err(|err| io::Error::other(format!("failed to join rsync stdout task: {err}")))??;
    let stderr = stderr_task
        .await
        .map_err(|err| io::Error::other(format!("failed to join rsync stderr task: {err}")))??;
    let stderr = String::from_utf8_lossy(&stderr);
    let visible_stderr = filter_rsync_stderr(&stderr);
    if !status.success() {
        let trimmed = visible_stderr.trim();
        if trimmed.is_empty() {
            return Err(io::Error::other(format!(
                "{context} exited with status {}",
                status
            )));
        }
        return Err(io::Error::other(format!(
            "{context} exited with status {}: {trimmed}",
            status
        )));
    }
    Ok(())
}

fn filter_rsync_stderr(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|line| {
            !line.contains("file has vanished:")
                && !line.contains("directory has vanished:")
                && !line.contains("some files vanished before they could be transferred")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn sync_remote_tui_binary(
    args: &CliArgs,
    config: &ResolvedRuntimeConfig,
    options: &RemoteCliOptions,
    dependencies: &RemoteCliDependencies,
) -> io::Result<()> {
    let layout = remote_layout(config)?;
    let mapping = remote_tui_sync_mapping(config, dependencies)?;
    run_rsync_command(
        build_rsync_file_up_args(
            &args.ssh_uri,
            &mapping,
            &layout.remote_tui_path,
            options,
            true,
        ),
        "rsync upload for multicode-tui binary".to_string(),
    )
    .await
}

async fn sync_remote_config_file(
    args: &CliArgs,
    config: &ResolvedRuntimeConfig,
    options: &RemoteCliOptions,
) -> io::Result<()> {
    let layout = remote_layout(config)?;
    let temp_root = std::env::temp_dir().join(format!(
        "multicode-remote-config-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    fs::create_dir_all(&temp_root)?;
    let local_remote_config = temp_root.join("config.toml");
    let serialized = toml::to_string(&config.rewritten_remote_config)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    tokio::fs::write(&local_remote_config, serialized).await?;
    let result = run_rsync_command(
        build_rsync_file_up_args(
            &args.ssh_uri,
            &local_remote_config,
            &layout.remote_config_path,
            options,
            true,
        ),
        "rsync upload for config.toml".to_string(),
    )
    .await;
    let _ = tokio::fs::remove_file(&local_remote_config).await;
    let _ = tokio::fs::remove_dir_all(&temp_root).await;
    result
}

async fn sync_mappings_down(
    args: &CliArgs,
    mappings: &[ResolvedSyncPathMapping],
    options: &RemoteCliOptions,
    show_progress: bool,
) -> io::Result<()> {
    for mapping in mappings {
        run_rsync_command(
            build_rsync_down_args(&args.ssh_uri, mapping, options, show_progress),
            format!("rsync download for '{}'", mapping.remote.display()),
        )
        .await?;
    }
    Ok(())
}

async fn run_ssh_command(
    args: &CliArgs,
    options: &RemoteCliOptions,
    command: &str,
) -> io::Result<()> {
    let status = Command::new("ssh")
        .args(build_ssh_base_args(&args.ssh_uri, options))
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "ssh command '{command}' exited with status {status}"
        )))
    }
}

async fn read_remote_config(path: &Path) -> io::Result<Config> {
    let raw = tokio::fs::read_to_string(path).await?;
    let full: Config =
        toml::from_str(&raw).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if full.remote.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "config.toml must contain a [remote] section",
        ));
    }
    Ok(full)
}

async fn resolve_github_token(config: &Config) -> io::Result<Option<String>> {
    let temp_db_path = std::env::temp_dir().join(format!(
        "multicode-remote-github-token-{}-{}.sqlite",
        std::process::id(),
        Uuid::new_v4()
    ));
    let github_status_service = GithubStatusService::new(
        Database::open_path(&temp_db_path).await.map_err(|err| {
            io::Error::other(format!(
                "failed to open temporary database for GitHub token resolution: {err:?}"
            ))
        })?,
        config.github.token.clone(),
    )
    .await
    .map_err(|err| {
        io::Error::other(format!("failed to initialize GitHub token resolver: {err}"))
    })?;
    let result = match config.github.token.as_ref() {
        Some(_) => github_status_service
            .resolved_github_token()
            .await
            .map(Some)
            .map_err(|err| io::Error::other(format!("failed to resolve GitHub token: {err}"))),
        None => Ok(None),
    };
    let _ = tokio::fs::remove_file(&temp_db_path).await;
    result
}

async fn resolve_runtime_config(
    full_config: &Config,
    config_path: Option<&Path>,
    remote_home_directory: &Path,
    remote_architecture: RemoteArchitecture,
) -> io::Result<ResolvedRuntimeConfig> {
    let config = full_config
        .remote
        .as_ref()
        .expect("remote config should exist after read_remote_config");
    validate_non_empty("workspace_directory", &full_config.workspace_directory)?;
    validate_non_empty("remote.install.command", &config.install.command)?;
    let remote_workspace_directory =
        expand_remote_path(&full_config.workspace_directory, remote_home_directory)?;
    let config_support_sync_up = resolve_added_skill_sync_mappings(
        &full_config.isolation.add_skills_from,
        config_path,
        &remote_workspace_directory,
    )?;
    Ok(ResolvedRuntimeConfig {
        remote_workspace_directory: remote_workspace_directory.clone(),
        remote_home_directory: remote_home_directory.to_path_buf(),
        remote_architecture,
        sync_interval: Duration::from_secs(config.sync_interval_seconds.max(1)),
        sync_up: resolve_sync_mappings(&config.sync_up, "remote.sync_up", remote_home_directory)?,
        sync_bidi: resolve_sync_mappings(
            &config.sync_bidi,
            "remote.sync_bidi",
            remote_home_directory,
        )?,
        rewritten_remote_config: rewrite_remote_config(
            full_config,
            &config_support_sync_up,
            &remote_workspace_directory,
        )?,
        config_support_sync_up,
        install_command: config.install.command.trim().to_string(),
        github_token: resolve_github_token(full_config).await?,
    })
}

fn resolve_sync_mappings(
    mappings: &[RemoteSyncMappingConfig],
    field: &str,
    remote_home_directory: &Path,
) -> io::Result<Vec<ResolvedSyncPathMapping>> {
    mappings
        .iter()
        .enumerate()
        .map(|(index, mapping)| {
            validate_non_empty(&format!("{field}[{index}].local"), &mapping.local)?;
            validate_non_empty(&format!("{field}[{index}].remote"), &mapping.remote)?;
            let local = expand_path(&mapping.local)?;
            let local_is_dir = std::fs::metadata(&local)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(true);
            Ok(ResolvedSyncPathMapping {
                local,
                remote: expand_remote_path(&mapping.remote, remote_home_directory)?,
                exclude: mapping.exclude.clone(),
                dereference_symlinks: mapping.dereference_symlinks,
                local_is_dir,
                directory_sync_target: DirectorySyncTarget::ExactDestination,
            })
        })
        .collect()
}

fn resolve_added_skill_sync_mappings(
    paths: &[String],
    config_path: Option<&Path>,
    remote_workspace_directory: &Path,
) -> io::Result<Vec<ResolvedSyncPathMapping>> {
    let Some(config_path) = config_path else {
        return Ok(Vec::new());
    };
    let config_directory = config_path.parent().unwrap_or_else(|| Path::new("."));
    let remote_skills_root = remote_workspace_directory
        .join(".multicode")
        .join("remote")
        .join("added-skills");
    let mut mappings = BTreeMap::<PathBuf, PathBuf>::new();

    for relative_dir in paths {
        let source_root = std::fs::canonicalize(config_directory.join(relative_dir))?;
        let remote_relative_dir = Path::new(relative_dir);
        for entry in std::fs::read_dir(&source_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            mappings.insert(
                std::fs::canonicalize(entry.path())?,
                remote_skills_root
                    .join(remote_relative_dir)
                    .join(entry.file_name()),
            );
        }
    }

    Ok(mappings
        .into_iter()
        .map(|(local, remote)| ResolvedSyncPathMapping {
            local,
            remote,
            exclude: Vec::new(),
            dereference_symlinks: false,
            local_is_dir: true,
            directory_sync_target: DirectorySyncTarget::ExactDestination,
        })
        .collect())
}

fn rewrite_remote_config(
    full_config: &Config,
    config_support_sync_up: &[ResolvedSyncPathMapping],
    remote_workspace_directory: &Path,
) -> io::Result<Config> {
    let mut rewritten = full_config.clone();
    let remote_support_root = remote_workspace_directory.join(".multicode").join("remote");
    let rewritten_paths = config_support_sync_up
        .iter()
        .map(|mapping| {
            mapping
                .remote
                .parent()
                .and_then(|parent| parent.strip_prefix(&remote_support_root).ok())
                .map(normalize_relative_remote_support_path)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "remote added-skill path '{}' is outside support root '{}'",
                        mapping.remote.display(),
                        remote_support_root.display()
                    ))
                })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut rewritten_paths = rewritten_paths;
    rewritten_paths.sort();
    rewritten_paths.dedup();
    rewritten.isolation.add_skills_from = rewritten_paths;
    Ok(rewritten)
}

fn normalize_relative_remote_support_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn build_ssh_base_args(ssh_uri: &str, options: &RemoteCliOptions) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(port) = options.ssh_port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    if let Some(identity_file) = &options.ssh_identity_file {
        args.push("-i".to_string());
        args.push(identity_file.to_string_lossy().into_owned());
    }
    if let Some(known_hosts_file) = &options.ssh_known_hosts_file {
        args.push("-o".to_string());
        args.push(format!(
            "UserKnownHostsFile={}",
            known_hosts_file.to_string_lossy()
        ));
    }
    args.push("-o".to_string());
    args.push(format!(
        "StrictHostKeyChecking={}",
        if options.ssh_strict_host_key_checking {
            "yes"
        } else {
            "no"
        }
    ));
    args.push(ssh_uri.to_string());
    args
}

fn build_ssh_args_with_forwarding(
    ssh_uri: &str,
    socket_forwards: &[(&Path, &Path)],
    options: &RemoteCliOptions,
) -> Vec<String> {
    let mut args = build_ssh_base_args(ssh_uri, options);
    let destination = args.pop().expect("ssh destination should exist");
    args.push("-o".to_string());
    args.push("ExitOnForwardFailure=yes".to_string());
    args.push("-o".to_string());
    args.push("StreamLocalBindUnlink=yes".to_string());
    args.push("-o".to_string());
    args.push("StreamLocalBindMask=0177".to_string());
    for (local_socket_path, remote_socket_path) in socket_forwards {
        args.push("-R".to_string());
        args.push(format!(
            "{}:{}",
            remote_socket_path.to_string_lossy(),
            local_socket_path.to_string_lossy()
        ));
    }
    args.push(destination);
    args
}

fn build_ssh_interactive_args_with_forwarding(
    ssh_uri: &str,
    socket_forwards: &[(&Path, &Path)],
    options: &RemoteCliOptions,
) -> Vec<String> {
    let mut args = build_ssh_args_with_forwarding(ssh_uri, socket_forwards, options);
    let destination = args.pop().expect("ssh destination should exist");
    args.push("-tt".to_string());
    args.push(destination);
    args
}

fn build_install_command(config: &ResolvedRuntimeConfig) -> String {
    let layout =
        remote_layout(config).expect("resolved runtime config should produce remote layout");
    format!(
        "mkdir -p {} && cd {} && {}",
        shell_single_quote(&layout.root.to_string_lossy()),
        shell_single_quote(&layout.root.to_string_lossy()),
        config.install_command
    )
}

fn build_remote_tui_command_with_relay_and_options(
    config: &ResolvedRuntimeConfig,
    relay_socket: Option<&Path>,
    options: &RemoteCliOptions,
) -> String {
    let layout =
        remote_layout(config).expect("resolved runtime config should produce remote layout");
    let mut argv = vec![
        shell_single_quote(&layout.remote_tui_path.to_string_lossy()),
        shell_single_quote(&layout.remote_config_path.to_string_lossy()),
    ];
    if let Some(relay_socket) = relay_socket {
        argv.push(shell_single_quote("--relay-socket"));
        argv.push(shell_single_quote(&relay_socket.to_string_lossy()));
    }
    if cfg!(test) {
        argv.push(shell_single_quote("--remote-sanity-check"));
    }
    if options.remote_tui_sanity_check {
        argv.push(shell_single_quote("--remote-sanity-check"));
    }
    if config.github_token.is_some() {
        argv.push(shell_single_quote("--github-token-env"));
        argv.push(shell_single_quote("GITHUB_MCP_TOKEN"));
    }
    let mut command = format!("exec {}", argv.join(" "));
    if let Some(github_token) = &config.github_token {
        command = format!(
            "export GITHUB_MCP_TOKEN={} && {}",
            shell_single_quote(github_token),
            command
        );
    }
    if options.remote_tui_sanity_check {
        format!(
            "cd {} && printf 'pwd=%s\\nargv=%s\\n' \"$(pwd)\" {} > launch-wrapper.log && sh -lc {} > launch.stdout 2> launch.stderr",
            shell_single_quote(&layout.root.to_string_lossy()),
            shell_single_quote(&argv.join(" ")),
            shell_single_quote(&command),
        )
    } else {
        format!(
            "cd {} && {}",
            shell_single_quote(&layout.root.to_string_lossy()),
            command
        )
    }
}

fn build_rsync_up_args(
    ssh_uri: &str,
    mapping: &ResolvedSyncPathMapping,
    options: &RemoteCliOptions,
    show_progress: bool,
) -> io::Result<Vec<String>> {
    let mut args = build_rsync_mapping_args(ssh_uri, mapping, options, show_progress);
    args.push("--update".to_string());
    args.push("--delete".to_string());
    for exclude in &mapping.exclude {
        args.push("--exclude".to_string());
        args.push(exclude.clone());
    }
    if mapping.local_is_dir {
        args.push(ensure_local_dir_suffix(&mapping.local));
        args.push(format!(
            "{ssh_uri}:{}",
            ensure_remote_dir_suffix(remote_directory_sync_destination(mapping)?)
        ));
    } else {
        args.push("--mkpath".to_string());
        args.push(mapping.local.to_string_lossy().into_owned());
        args.push(format!("{ssh_uri}:{}", mapping.remote.to_string_lossy()));
    }
    Ok(args)
}

fn build_rsync_file_up_args(
    ssh_uri: &str,
    local_binary_path: &Path,
    remote_binary_path: &Path,
    options: &RemoteCliOptions,
    show_progress: bool,
) -> Vec<String> {
    let mut args = build_rsync_base_args(ssh_uri, options, show_progress);
    args.push("--mkpath".to_string());
    args.push(local_binary_path.to_string_lossy().into_owned());
    args.push(format!(
        "{ssh_uri}:{}",
        remote_binary_path.to_string_lossy()
    ));
    args
}

fn build_rsync_mapping_args(
    _ssh_uri: &str,
    mapping: &ResolvedSyncPathMapping,
    options: &RemoteCliOptions,
    show_progress: bool,
) -> Vec<String> {
    let mut args = build_rsync_common_args(options, show_progress);
    args.push(if mapping.dereference_symlinks {
        "--copy-links".to_string()
    } else {
        "--links".to_string()
    });
    args.push("-e".to_string());
    args.push(build_rsync_ssh_command(options));
    args
}

fn build_rsync_base_args(
    _ssh_uri: &str,
    options: &RemoteCliOptions,
    show_progress: bool,
) -> Vec<String> {
    let mut args = build_rsync_common_args(options, show_progress);
    args.push("--links".to_string());
    args.push("-e".to_string());
    args.push(build_rsync_ssh_command(options));
    args
}

fn build_rsync_common_args(_options: &RemoteCliOptions, show_progress: bool) -> Vec<String> {
    let mut args = vec![
        "--recursive".to_string(),
        "--perms".to_string(),
        "--times".to_string(),
        "--devices".to_string(),
        "--compress".to_string(),
    ];
    if show_progress {
        args.push("--no-inc-recursive".to_string());
        args.push("--info=progress2".to_string());
        args.push("--human-readable".to_string());
    }
    args
}

fn build_rsync_ssh_command(options: &RemoteCliOptions) -> String {
    let mut ssh_command = vec!["ssh".to_string()];
    if let Some(port) = options.ssh_port {
        ssh_command.push("-p".to_string());
        ssh_command.push(port.to_string());
    }
    if let Some(identity_file) = &options.ssh_identity_file {
        ssh_command.push("-i".to_string());
        ssh_command.push(identity_file.to_string_lossy().into_owned());
    }
    if let Some(known_hosts_file) = &options.ssh_known_hosts_file {
        ssh_command.push("-o".to_string());
        ssh_command.push(format!(
            "UserKnownHostsFile={}",
            known_hosts_file.to_string_lossy()
        ));
    }
    ssh_command.push("-o".to_string());
    ssh_command.push(format!(
        "StrictHostKeyChecking={}",
        if options.ssh_strict_host_key_checking {
            "yes"
        } else {
            "no"
        }
    ));
    ssh_command.join(" ")
}

fn remote_layout(config: &ResolvedRuntimeConfig) -> io::Result<RemoteLayout> {
    let root = expand_remote_path(
        &format!(
            "{}/.multicode/remote",
            config.remote_workspace_directory.display()
        ),
        &config.remote_home_directory,
    )?;
    Ok(RemoteLayout {
        remote_tui_path: root.join("multicode-tui"),
        remote_config_path: root.join("config.toml"),
        remote_relay_dir: root.join("relay"),
        root,
    })
}

fn build_rsync_down_args(
    ssh_uri: &str,
    mapping: &ResolvedSyncPathMapping,
    options: &RemoteCliOptions,
    show_progress: bool,
) -> Vec<String> {
    let mut args = build_rsync_mapping_args(ssh_uri, mapping, options, show_progress);
    args.push("--delete".to_string());
    for exclude in &mapping.exclude {
        args.push("--exclude".to_string());
        args.push(exclude.clone());
    }
    args.push(format!(
        "{ssh_uri}:{}",
        ensure_remote_dir_suffix(&mapping.remote)
    ));
    args.push(ensure_local_dir_suffix(&mapping.local));
    args
}

fn ensure_local_dir_suffix(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.ends_with('/') {
        text.into_owned()
    } else {
        format!("{text}/")
    }
}

fn ensure_remote_dir_suffix(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.ends_with('/') {
        text.into_owned()
    } else {
        format!("{text}/")
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn expand_path(value: &str) -> io::Result<PathBuf> {
    let expanded = shellexpand::full(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
    Ok(PathBuf::from(expanded.into_owned()))
}

fn expand_remote_path(value: &str, remote_home_directory: &Path) -> io::Result<PathBuf> {
    let home = remote_home_directory.to_string_lossy().into_owned();
    let trimmed = value.trim();
    let expanded = if trimmed == "~" || trimmed.starts_with("~/") {
        trimmed.replacen('~', &home, 1)
    } else {
        shellexpand::env_with_context_no_errors(trimmed, |name: &str| {
            if name == "HOME" {
                Some(home.clone())
            } else {
                None
            }
        })
        .into_owned()
    };
    Ok(PathBuf::from(expanded))
}

fn validate_non_empty(field: &str, value: &str) -> io::Result<()> {
    if value.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{field} must not be empty"),
        ));
    }
    Ok(())
}

fn remote_tui_sync_mapping(
    config: &ResolvedRuntimeConfig,
    dependencies: &RemoteCliDependencies,
) -> io::Result<PathBuf> {
    if let Some(path) = &dependencies.local_tui_binary_override {
        return Ok(path.clone());
    }
    let stage_root = if let Some(path) = &dependencies.local_tui_stage_root_override {
        path.clone()
    } else {
        let current_exe = std::env::current_exe()?;
        let target_dir = current_exe
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| io::Error::other("current executable has no target directory parent for locating staged multicode-tui binaries"))?;
        target_dir.join("multicode-remote").join("tui")
    };
    let path = stage_root.join(format!(
        "{}-multicode-tui",
        config.remote_architecture.target_triple()
    ));
    if path.is_file() {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no staged multicode-tui binary for target {} at {}",
                config.remote_architecture.target_triple(),
                path.display()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multicode_lib::services::config::{AgentConfig, IsolationConfig, RemoteConfig};
    use std::fs;

    #[test]
    fn parse_remote_architecture_maps_supported_values() {
        assert_eq!(
            parse_remote_architecture("x86_64\n").expect("x86_64 should map"),
            RemoteArchitecture::X86_64
        );
        assert_eq!(
            parse_remote_architecture("amd64\n").expect("amd64 should map"),
            RemoteArchitecture::X86_64
        );
        assert_eq!(
            parse_remote_architecture("aarch64\n").expect("aarch64 should map"),
            RemoteArchitecture::Aarch64
        );
        assert_eq!(
            parse_remote_architecture("arm64\n").expect("arm64 should map"),
            RemoteArchitecture::Aarch64
        );
        assert!(parse_remote_architecture("riscv64\n").is_err());
    }

    #[tokio::test]
    async fn remote_tui_sync_mapping_uses_staged_arch_specific_binary() {
        let temp_root = std::env::temp_dir().join(format!(
            "multicode-remote-stage-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_root);
        fs::create_dir_all(&temp_root).expect("temp stage root should be created");
        let staged = temp_root.join("aarch64-unknown-linux-gnu-multicode-tui");
        fs::write(&staged, b"binary").expect("staged binary should be written");

        let resolved = resolve_runtime_config(
            &sample_full_config(),
            None,
            Path::new("/home/alice"),
            RemoteArchitecture::Aarch64,
        )
        .await
        .expect("config should resolve");
        let dependencies = RemoteCliDependencies {
            local_tui_binary_override: None,
            local_tui_stage_root_override: Some(temp_root.clone()),
        };

        let actual = remote_tui_sync_mapping(&resolved, &dependencies)
            .expect("staged binary should resolve");
        assert_eq!(actual, staged);

        let _ = fs::remove_dir_all(&temp_root);
    }

    fn sample_remote_config() -> RemoteConfig {
        RemoteConfig {
            sync_interval_seconds: 2,
            sync_up: vec![RemoteSyncMappingConfig {
                local: "/tmp/local-bin".to_string(),
                remote: "/srv/multicode/bin".to_string(),
                exclude: vec!["target".to_string(), "tmp".to_string()],
                dereference_symlinks: false,
            }],
            sync_bidi: vec![RemoteSyncMappingConfig {
                local: "/tmp/agent-work".to_string(),
                remote: "~/dev/agent-work".to_string(),
                exclude: vec![".git".to_string()],
                dereference_symlinks: false,
            }],
            install: multicode_lib::services::config::RemoteInstallConfig {
                command: "./install-deps.sh".to_string(),
            },
        }
    }

    fn sample_full_config() -> Config {
        Config {
            workspace_directory: "~/dev/agent-work".to_string(),
            isolation: Default::default(),
            runtime: Default::default(),
            autonomous: Default::default(),
            agent: AgentConfig::default(),
            opencode: vec!["opencode-cli".to_string()],
            compare: Default::default(),
            tool: Vec::new(),
            handler: Default::default(),
            remote: Some(sample_remote_config()),
            github: Default::default(),
        }
    }

    fn sample_full_config_with_github_token() -> Config {
        let mut config = sample_full_config();
        config.github.token = Some(multicode_lib::services::GithubTokenConfig {
            env: Some("MULTICODE_TEST_GITHUB_TOKEN".to_string()),
            command: None,
        });
        config
    }

    #[tokio::test]
    async fn resolve_runtime_config_resolves_github_token_once() {
        unsafe {
            std::env::set_var("MULTICODE_TEST_GITHUB_TOKEN", "token-from-env");
        }

        let resolved = resolve_runtime_config(
            &sample_full_config_with_github_token(),
            None,
            Path::new("/home/alice"),
            RemoteArchitecture::X86_64,
        )
        .await
        .expect("config should resolve");

        assert_eq!(resolved.github_token.as_deref(), Some("token-from-env"));

        unsafe {
            std::env::remove_var("MULTICODE_TEST_GITHUB_TOKEN");
        }
    }

    #[test]
    fn build_remote_tui_command_exports_github_token_when_resolved() {
        let mut resolved = ResolvedRuntimeConfig {
            remote_workspace_directory: PathBuf::from("/home/alice/dev/agent-work"),
            remote_home_directory: PathBuf::from("/home/alice"),
            remote_architecture: RemoteArchitecture::X86_64,
            sync_interval: Duration::from_secs(2),
            sync_up: Vec::new(),
            sync_bidi: Vec::new(),
            config_support_sync_up: Vec::new(),
            rewritten_remote_config: sample_full_config(),
            install_command: "echo hi".to_string(),
            github_token: Some("abc'123".to_string()),
        };

        let command = build_remote_tui_command_with_relay_and_options(
            &resolved,
            None,
            &RemoteCliOptions::default(),
        );
        assert!(command.contains("export GITHUB_MCP_TOKEN='abc'\\''123' && exec"));
        assert!(command.contains("'--github-token-env' 'GITHUB_MCP_TOKEN'"));

        resolved.github_token = None;
        let command_without_token = build_remote_tui_command_with_relay_and_options(
            &resolved,
            None,
            &RemoteCliOptions::default(),
        );
        assert!(!command_without_token.contains("GITHUB_MCP_TOKEN"));
    }

    #[test]
    fn parse_cli_args_requires_config_and_ssh_flag() {
        let parsed = CliArgs::try_parse_from([
            "multicode-remote",
            "config.toml",
            "--ssh",
            "alice@example.com",
        ])
        .expect("cli args should parse");
        assert_eq!(parsed.config_path, PathBuf::from("config.toml"));
        assert_eq!(parsed.ssh_uri, "alice@example.com");
        assert!(CliArgs::try_parse_from(["multicode-remote"]).is_err());
    }

    #[test]
    fn build_ssh_args_with_forwarding_includes_reverse_socket_mapping() {
        assert_eq!(
            build_ssh_args_with_forwarding(
                "alice@example.com",
                &[(Path::new("/tmp/local.sock"), Path::new("/srv/remote.sock"))],
                &RemoteCliOptions::default(),
            ),
            vec![
                "-o".to_string(),
                "StrictHostKeyChecking=yes".to_string(),
                "-o".to_string(),
                "ExitOnForwardFailure=yes".to_string(),
                "-o".to_string(),
                "StreamLocalBindUnlink=yes".to_string(),
                "-o".to_string(),
                "StreamLocalBindMask=0177".to_string(),
                "-R".to_string(),
                "/srv/remote.sock:/tmp/local.sock".to_string(),
                "alice@example.com".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_interactive_args_forces_tty_allocation() {
        assert_eq!(
            build_ssh_interactive_args_with_forwarding(
                "alice@example.com",
                &[(Path::new("/tmp/local.sock"), Path::new("/srv/remote.sock"))],
                &RemoteCliOptions::default(),
            ),
            vec![
                "-o".to_string(),
                "StrictHostKeyChecking=yes".to_string(),
                "-o".to_string(),
                "ExitOnForwardFailure=yes".to_string(),
                "-o".to_string(),
                "StreamLocalBindUnlink=yes".to_string(),
                "-o".to_string(),
                "StreamLocalBindMask=0177".to_string(),
                "-R".to_string(),
                "/srv/remote.sock:/tmp/local.sock".to_string(),
                "-tt".to_string(),
                "alice@example.com".to_string(),
            ]
        );
    }

    #[test]
    fn build_ssh_interactive_with_forwarding_args_combines_tty_and_reverse_socket_mapping() {
        assert_eq!(
            build_ssh_interactive_args_with_forwarding(
                "alice@example.com",
                &[(Path::new("/tmp/local.sock"), Path::new("/srv/remote.sock"))],
                &RemoteCliOptions::default(),
            ),
            vec![
                "-o".to_string(),
                "StrictHostKeyChecking=yes".to_string(),
                "-o".to_string(),
                "ExitOnForwardFailure=yes".to_string(),
                "-o".to_string(),
                "StreamLocalBindUnlink=yes".to_string(),
                "-o".to_string(),
                "StreamLocalBindMask=0177".to_string(),
                "-R".to_string(),
                "/srv/remote.sock:/tmp/local.sock".to_string(),
                "-tt".to_string(),
                "alice@example.com".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn resolve_runtime_config_validates_and_expands_sync_mappings() {
        let config = sample_remote_config();
        let resolved = resolve_runtime_config(
            &Config {
                workspace_directory: "~/dev/agent-work".to_string(),
                isolation: Default::default(),
                runtime: Default::default(),
                autonomous: Default::default(),
                agent: AgentConfig::default(),
                opencode: vec!["opencode-cli".to_string()],
                compare: Default::default(),
                tool: Vec::new(),
                handler: Default::default(),
                remote: Some(config),
                github: Default::default(),
            },
            None,
            Path::new("/home/alice"),
            RemoteArchitecture::X86_64,
        )
        .await
        .expect("config should resolve");
        assert_eq!(
            resolved.remote_workspace_directory,
            PathBuf::from("/home/alice/dev/agent-work")
        );
        assert_eq!(resolved.remote_home_directory, PathBuf::from("/home/alice"));
        assert_eq!(resolved.sync_interval, Duration::from_secs(2));
        assert_eq!(
            resolved.sync_up[0].remote,
            PathBuf::from("/srv/multicode/bin")
        );
        assert_eq!(resolved.sync_up[0].exclude, vec!["target", "tmp"]);
        assert_eq!(
            resolved.sync_bidi[0].remote,
            PathBuf::from("/home/alice/dev/agent-work")
        );
        assert_eq!(resolved.sync_bidi[0].exclude, vec![".git"]);
    }

    #[tokio::test]
    async fn resolve_runtime_config_adds_skill_sync_mappings_from_config_directory() {
        let temp_root = std::env::temp_dir().join(format!(
            "multicode-remote-skill-sync-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let config_dir = temp_root.join("config-dir");
        let skill_root = config_dir.join("extra-skills");
        let skill_dir = skill_root.join("sample-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill dir should be created");
        std::fs::write(skill_dir.join("SKILL.md"), "# sample")
            .expect("skill file should be written");
        let config_path = config_dir.join("config.toml");

        let resolved = resolve_runtime_config(
            &Config {
                workspace_directory: "~/dev/agent-work".to_string(),
                isolation: IsolationConfig {
                    add_skills_from: vec!["extra-skills".to_string()],
                    ..Default::default()
                },
                runtime: Default::default(),
                autonomous: Default::default(),
                agent: AgentConfig::default(),
                opencode: vec!["opencode-cli".to_string()],
                compare: Default::default(),
                tool: Vec::new(),
                handler: Default::default(),
                remote: Some(sample_remote_config()),
                github: Default::default(),
            },
            Some(&config_path),
            Path::new("/home/alice"),
            RemoteArchitecture::X86_64,
        )
        .await
        .expect("config should resolve");

        assert_eq!(resolved.config_support_sync_up.len(), 1);
        assert_eq!(
            resolved.config_support_sync_up[0].local,
            std::fs::canonicalize(&skill_dir).expect("skill dir should canonicalize")
        );
        assert_eq!(
            resolved.config_support_sync_up[0].remote,
            PathBuf::from(
                "/home/alice/dev/agent-work/.multicode/remote/added-skills/extra-skills/sample-skill"
            )
        );

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn rewrite_remote_config_rewrites_add_skills_from_to_remote_support_paths() {
        let remote_workspace_directory = PathBuf::from("/home/alice/dev/agent-work");
        let rewritten = rewrite_remote_config(
            &Config {
                workspace_directory: "~/dev/agent-work".to_string(),
                isolation: IsolationConfig {
                    add_skills_from: vec!["workspace-skills".to_string()],
                    ..Default::default()
                },
                runtime: Default::default(),
                autonomous: Default::default(),
                agent: AgentConfig::default(),
                opencode: vec!["opencode-cli".to_string()],
                compare: Default::default(),
                tool: Vec::new(),
                handler: Default::default(),
                remote: Some(sample_remote_config()),
                github: Default::default(),
            },
            &[ResolvedSyncPathMapping {
                local: PathBuf::from("/tmp/local-skills/sample-skill"),
                remote: PathBuf::from(
                    "/home/alice/dev/agent-work/.multicode/remote/added-skills/workspace-skills/sample-skill",
                ),
                exclude: Vec::new(),
                dereference_symlinks: false,
                local_is_dir: true,
                directory_sync_target: DirectorySyncTarget::ExactDestination,
            }],
            &remote_workspace_directory,
        )
        .expect("config should rewrite");

        assert_eq!(
            rewritten.isolation.add_skills_from,
            vec!["added-skills/workspace-skills".to_string()]
        );
    }

    #[tokio::test]
    async fn build_remote_commands_match_expected_shape() {
        let resolved = resolve_runtime_config(
            &sample_full_config(),
            None,
            Path::new("/home/alice"),
            RemoteArchitecture::X86_64,
        )
        .await
        .expect("config should resolve");
        let layout = remote_layout(&resolved).expect("layout should derive from workspace mapping");
        assert!(build_install_command(&resolved).contains("./install-deps.sh"));
        assert_eq!(
            layout.root,
            PathBuf::from("/home/alice/dev/agent-work/.multicode/remote")
        );
        assert_eq!(
            layout.remote_tui_path,
            PathBuf::from("/home/alice/dev/agent-work/.multicode/remote/multicode-tui")
        );
        assert_eq!(
            layout.remote_config_path,
            PathBuf::from("/home/alice/dev/agent-work/.multicode/remote/config.toml")
        );
        assert_eq!(
            layout.remote_relay_dir,
            PathBuf::from("/home/alice/dev/agent-work/.multicode/remote/relay")
        );
        let remote_tui_command = build_remote_tui_command_with_relay_and_options(
            &resolved,
            None,
            &RemoteCliOptions::default(),
        );
        assert!(
            remote_tui_command
                .contains("cd '/home/alice/dev/agent-work/.multicode/remote' && exec")
                || remote_tui_command.contains(
                    "cd '/home/alice/dev/agent-work/.multicode/remote' && export GITHUB_MCP_TOKEN="
                )
        );
        assert!(
            remote_tui_command
                .contains("'/home/alice/dev/agent-work/.multicode/remote/multicode-tui'")
        );
        assert!(
            remote_tui_command
                .contains("'/home/alice/dev/agent-work/.multicode/remote/config.toml'")
        );
        let remote_tui_command_with_relay = build_remote_tui_command_with_relay_and_options(
            &resolved,
            Some(Path::new(
                "/home/alice/dev/agent-work/.multicode/remote/relay/123.sock",
            )),
            &RemoteCliOptions::default(),
        );
        assert!(remote_tui_command_with_relay.contains("'--relay-socket'"));
        assert!(
            remote_tui_command_with_relay
                .contains("'/home/alice/dev/agent-work/.multicode/remote/relay/123.sock'")
        );
        assert_eq!(
            build_rsync_file_up_args(
                "alice@example.com",
                Path::new("/tmp/config.toml"),
                &layout.remote_config_path,
                &RemoteCliOptions::default(),
                false,
            ),
            vec![
                "--recursive".to_string(),
                "--perms".to_string(),
                "--times".to_string(),
                "--devices".to_string(),
                "--compress".to_string(),
                "--links".to_string(),
                "-e".to_string(),
                "ssh -o StrictHostKeyChecking=yes".to_string(),
                "--mkpath".to_string(),
                "/tmp/config.toml".to_string(),
                "alice@example.com:/home/alice/dev/agent-work/.multicode/remote/config.toml"
                    .to_string(),
            ]
        );
        assert_eq!(
            build_rsync_up_args(
                "alice@example.com",
                &resolved.sync_up[0],
                &RemoteCliOptions::default(),
                false
            )
            .expect("directory sync args should build"),
            vec![
                "--recursive".to_string(),
                "--perms".to_string(),
                "--times".to_string(),
                "--devices".to_string(),
                "--compress".to_string(),
                "--links".to_string(),
                "-e".to_string(),
                "ssh -o StrictHostKeyChecking=yes".to_string(),
                "--update".to_string(),
                "--delete".to_string(),
                "--exclude".to_string(),
                "target".to_string(),
                "--exclude".to_string(),
                "tmp".to_string(),
                "/tmp/local-bin/".to_string(),
                "alice@example.com:/srv/multicode/bin/".to_string(),
            ]
        );
        assert_eq!(
            build_rsync_file_up_args(
                "alice@example.com",
                Path::new("/tmp/multicode-tui"),
                &layout.remote_tui_path,
                &RemoteCliOptions::default(),
                false,
            ),
            vec![
                "--recursive".to_string(),
                "--perms".to_string(),
                "--times".to_string(),
                "--devices".to_string(),
                "--compress".to_string(),
                "--links".to_string(),
                "-e".to_string(),
                "ssh -o StrictHostKeyChecking=yes".to_string(),
                "--mkpath".to_string(),
                "/tmp/multicode-tui".to_string(),
                "alice@example.com:/home/alice/dev/agent-work/.multicode/remote/multicode-tui"
                    .to_string(),
            ]
        );
        assert_eq!(
            build_rsync_down_args(
                "alice@example.com",
                &resolved.sync_bidi[0],
                &RemoteCliOptions::default(),
                false
            ),
            vec![
                "--recursive".to_string(),
                "--perms".to_string(),
                "--times".to_string(),
                "--devices".to_string(),
                "--compress".to_string(),
                "--links".to_string(),
                "-e".to_string(),
                "ssh -o StrictHostKeyChecking=yes".to_string(),
                "--delete".to_string(),
                "--exclude".to_string(),
                ".git".to_string(),
                "alice@example.com:/home/alice/dev/agent-work/".to_string(),
                "/tmp/agent-work/".to_string(),
            ]
        );
    }

    #[test]
    fn compare_sync_tree_recency_orders_missing_and_present_timestamps() {
        let now = SystemTime::now();
        let earlier = now
            .checked_sub(Duration::from_secs(1))
            .expect("earlier time should exist");
        let later = now
            .checked_add(Duration::from_secs(1))
            .expect("later time should exist");

        assert_eq!(
            compare_sync_tree_recency(Some(later), Some(earlier)),
            Ordering::Greater
        );
        assert_eq!(
            compare_sync_tree_recency(Some(now), Some(now)),
            Ordering::Equal
        );
        assert_eq!(
            compare_sync_tree_recency(Some(now), None),
            Ordering::Greater
        );
        assert_eq!(compare_sync_tree_recency(None, None), Ordering::Equal);
        assert_eq!(
            compare_sync_tree_recency(Some(earlier), Some(later)),
            Ordering::Less
        );
        assert_eq!(compare_sync_tree_recency(None, Some(now)), Ordering::Less);
    }

    #[test]
    fn should_sync_bidirectional_mapping_up_only_when_remote_is_not_newer() {
        let now = SystemTime::now();
        let earlier = now
            .checked_sub(Duration::from_secs(1))
            .expect("earlier time should exist");
        let later = now
            .checked_add(Duration::from_secs(1))
            .expect("later time should exist");

        assert!(should_sync_bidirectional_mapping_up(
            Some(later),
            Some(earlier)
        ));
        assert!(should_sync_bidirectional_mapping_up(Some(now), Some(now)));
        assert!(should_sync_bidirectional_mapping_up(Some(now), None));
        assert!(should_sync_bidirectional_mapping_up(None, None));
        assert!(!should_sync_bidirectional_mapping_up(
            Some(earlier),
            Some(later)
        ));
        assert!(!should_sync_bidirectional_mapping_up(None, Some(now)));
    }

    #[test]
    fn ensure_local_sync_source_exists_creates_missing_directory_sources() {
        let temp_root = std::env::temp_dir().join(format!(
            "multicode-remote-missing-sync-source-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let missing = temp_root.join("nested").join("workspace");

        ensure_local_sync_source_exists(&ResolvedSyncPathMapping {
            local: missing.clone(),
            remote: PathBuf::from("/remote/workspace"),
            exclude: Vec::new(),
            dereference_symlinks: false,
            local_is_dir: true,
            directory_sync_target: DirectorySyncTarget::ExactDestination,
        })
        .expect("missing directory source should be created");

        assert!(missing.is_dir());

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[tokio::test]
    async fn latest_local_path_modified_time_tracks_newest_nested_entry() {
        let temp_root = std::env::temp_dir().join(format!(
            "multicode-remote-local-mtime-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(temp_root.join("nested")).expect("temp tree should be created");
        let old_file = temp_root.join("old.txt");
        let new_file = temp_root.join("nested").join("new.txt");
        std::fs::write(&old_file, "old").expect("old file should be written");
        tokio::time::sleep(Duration::from_secs(1)).await;
        std::fs::write(&new_file, "new").expect("new file should be written");

        let latest = latest_local_path_modified_time(&temp_root, true)
            .expect("latest mtime should be read")
            .expect("directory tree should have an mtime");
        let newest_file_mtime = std::fs::metadata(&new_file)
            .expect("new file metadata should exist")
            .modified()
            .expect("new file mtime should exist");
        assert!(latest >= newest_file_mtime);

        std::fs::remove_dir_all(&temp_root).expect("temp root should be removed");
    }

    #[tokio::test]
    async fn latest_local_path_modified_time_returns_none_for_missing_path() {
        let missing = std::env::temp_dir().join(format!(
            "multicode-remote-local-mtime-missing-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        assert_eq!(
            latest_local_path_modified_time(&missing, true)
                .expect("missing path lookup should succeed"),
            None
        );
    }

    #[test]
    fn remote_timestamp_probe_failed_for_missing_path_detects_missing_directory_error() {
        assert!(remote_timestamp_probe_failed_for_missing_path(
            b"Error: Custom { kind: NotFound, error: \"/root/agent-work: No such file or directory\" }\n"
        ));
        assert!(!remote_timestamp_probe_failed_for_missing_path(
            b"Error: permission denied\n"
        ));
    }

    #[test]
    fn parse_remote_latest_timestamp_output_handles_none_and_epoch_seconds() {
        assert_eq!(
            parse_remote_latest_timestamp_output(
                b"none
"
            )
            .expect("none should parse"),
            None
        );
        assert_eq!(
            parse_remote_latest_timestamp_output(
                b"42
"
            )
            .expect("epoch seconds should parse"),
            Some(UNIX_EPOCH + Duration::from_secs(42))
        );
    }

    #[test]
    fn recency_excludes_adds_remote_runtime_subtree_for_directory_mappings() {
        let mapping = ResolvedSyncPathMapping {
            local: PathBuf::from("/tmp/local"),
            remote: PathBuf::from("/tmp/remote"),
            exclude: vec!["custom/skip".to_string()],
            dereference_symlinks: false,
            local_is_dir: true,
            directory_sync_target: DirectorySyncTarget::ExactDestination,
        };
        assert_eq!(
            recency_excludes(&mapping),
            vec![".multicode/remote".to_string(), "custom/skip".to_string()]
        );
    }

    #[test]
    fn recency_excludes_do_not_add_runtime_subtree_for_file_mappings() {
        let mapping = ResolvedSyncPathMapping {
            local: PathBuf::from("/tmp/local-file"),
            remote: PathBuf::from("/tmp/remote-file"),
            exclude: vec!["custom/skip".to_string()],
            dereference_symlinks: false,
            local_is_dir: false,
            directory_sync_target: DirectorySyncTarget::ExactDestination,
        };
        assert_eq!(recency_excludes(&mapping), vec!["custom/skip".to_string()]);
    }

    #[test]
    fn is_excluded_relative_path_matches_exact_paths_and_descendants() {
        let exclude = vec![".multicode/remote".to_string()];
        assert!(tree_scan::is_excluded_relative_path(
            Path::new(".multicode/remote"),
            &exclude
        ));
        assert!(tree_scan::is_excluded_relative_path(
            Path::new(".multicode/remote/relay/file.sock"),
            &exclude
        ));
        assert!(tree_scan::is_excluded_relative_path(
            Path::new(".multicode"),
            &exclude
        ));
        assert!(!tree_scan::is_excluded_relative_path(
            Path::new("workspace/file.txt"),
            &exclude
        ));
    }

    #[tokio::test]
    async fn build_rsync_up_args_preserves_file_destinations_and_uses_mkpath() {
        let temp_root = std::env::temp_dir().join(format!(
            "multicode-remote-file-sync-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&temp_root).expect("temp root should be created");
        let local_file = temp_root.join("auth.json");
        std::fs::write(&local_file, "token").expect("local file should be written");
        let mapping = ResolvedSyncPathMapping {
            local: local_file.clone(),
            remote: PathBuf::from("/home/alice/.local/share/opencode/auth.json"),
            exclude: Vec::new(),
            dereference_symlinks: false,
            local_is_dir: false,
            directory_sync_target: DirectorySyncTarget::ExactDestination,
        };

        let args = build_rsync_up_args(
            "alice@example.com",
            &mapping,
            &RemoteCliOptions::default(),
            false,
        )
        .expect("file sync args should build");

        assert!(args.iter().any(|arg| arg == "--update"));
        assert!(args.iter().any(|arg| arg == "--mkpath"));
        assert!(args.iter().any(|arg| arg == &local_file.to_string_lossy()));
        assert!(
            args.iter()
                .any(|arg| arg == "alice@example.com:/home/alice/.local/share/opencode/auth.json")
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == &format!("{}/", local_file.to_string_lossy()))
        );

        std::fs::remove_dir_all(&temp_root).expect("temp root should be removed");
    }

    #[tokio::test]
    async fn build_rsync_up_args_syncs_directories_into_exact_destination() {
        let temp_root = std::env::temp_dir().join(format!(
            "multicode-remote-dir-sync-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let local_dir = temp_root.join("skill-alpha");
        std::fs::create_dir_all(&local_dir).expect("local dir should be created");
        let mapping = ResolvedSyncPathMapping {
            local: local_dir.clone(),
            remote: PathBuf::from(
                "/home/alice/dev/agent-work/.multicode/remote/added-skills/workspace-skills/skill-alpha",
            ),
            exclude: Vec::new(),
            dereference_symlinks: false,
            local_is_dir: true,
            directory_sync_target: DirectorySyncTarget::ExactDestination,
        };

        let args = build_rsync_up_args(
            "alice@example.com",
            &mapping,
            &RemoteCliOptions::default(),
            false,
        )
        .expect("directory sync args should build");

        assert!(
            args.iter()
                .any(|arg| arg == &format!("{}/", local_dir.to_string_lossy()))
        );
        assert!(!args.iter().any(|arg| arg == "--mkpath"));
        assert!(args.iter().any(|arg| {
            arg == "alice@example.com:/home/alice/dev/agent-work/.multicode/remote/added-skills/workspace-skills/skill-alpha/"
        }));

        std::fs::remove_dir_all(&temp_root).expect("temp root should be removed");
    }

    #[test]
    fn ensure_remote_directory_sync_destinations_exist_deduplicates_targets() {
        let mappings = vec![
            ResolvedSyncPathMapping {
                local: PathBuf::from("/tmp/skill-alpha"),
                remote: PathBuf::from(
                    "/home/alice/dev/agent-work/.multicode/remote/added-skills/workspace-skills/skill-alpha",
                ),
                exclude: Vec::new(),
                dereference_symlinks: false,
                local_is_dir: true,
                directory_sync_target: DirectorySyncTarget::ExactDestination,
            },
            ResolvedSyncPathMapping {
                local: PathBuf::from("/tmp/skill-beta"),
                remote: PathBuf::from(
                    "/home/alice/dev/agent-work/.multicode/remote/added-skills/workspace-skills/skill-beta",
                ),
                exclude: Vec::new(),
                dereference_symlinks: false,
                local_is_dir: true,
                directory_sync_target: DirectorySyncTarget::ExactDestination,
            },
        ];

        let command = mappings
            .iter()
            .filter(|mapping| mapping.local_is_dir)
            .map(|mapping| mapping.remote.parent().expect("remote parent should exist"))
            .collect::<BTreeSet<_>>();
        assert_eq!(command.len(), 1);
    }

    #[test]
    fn ensure_remote_directory_mapping_targets_exist_deduplicates_targets() {
        let mappings = vec![
            ResolvedSyncPathMapping {
                local: PathBuf::from("/tmp/skill-alpha"),
                remote: PathBuf::from(
                    "/home/alice/dev/agent-work/.multicode/remote/added-skills/workspace-skills/skill-alpha",
                ),
                exclude: Vec::new(),
                dereference_symlinks: false,
                local_is_dir: true,
                directory_sync_target: DirectorySyncTarget::ExactDestination,
            },
            ResolvedSyncPathMapping {
                local: PathBuf::from("/tmp/skill-alpha-copy"),
                remote: PathBuf::from(
                    "/home/alice/dev/agent-work/.multicode/remote/added-skills/workspace-skills/skill-alpha",
                ),
                exclude: Vec::new(),
                dereference_symlinks: false,
                local_is_dir: true,
                directory_sync_target: DirectorySyncTarget::ExactDestination,
            },
        ];

        let command = mappings
            .iter()
            .filter(|mapping| mapping.local_is_dir)
            .map(|mapping| mapping.remote.as_path())
            .collect::<BTreeSet<_>>();
        assert_eq!(command.len(), 1);
    }

    #[tokio::test]
    async fn build_rsync_args_include_progress_flags_when_enabled() {
        let resolved = resolve_runtime_config(
            &sample_full_config(),
            None,
            Path::new("/home/alice"),
            RemoteArchitecture::X86_64,
        )
        .await
        .expect("config should resolve");

        let args = build_rsync_down_args(
            "alice@example.com",
            &resolved.sync_bidi[0],
            &RemoteCliOptions::default(),
            true,
        );

        assert!(args.iter().any(|arg| arg == "--no-inc-recursive"));
        assert!(args.iter().any(|arg| arg == "--info=progress2"));
        assert!(args.iter().any(|arg| arg == "--human-readable"));
    }

    #[tokio::test]
    async fn build_rsync_mapping_args_use_copy_links_when_dereferencing_symlinks() {
        let mut config = sample_remote_config();
        config.sync_bidi[0].dereference_symlinks = true;
        let resolved = resolve_runtime_config(
            &Config {
                workspace_directory: "~/dev/agent-work".to_string(),
                isolation: Default::default(),
                runtime: Default::default(),
                autonomous: Default::default(),
                agent: AgentConfig::default(),
                opencode: vec!["opencode-cli".to_string()],
                compare: Default::default(),
                tool: Vec::new(),
                handler: Default::default(),
                remote: Some(config),
                github: Default::default(),
            },
            None,
            Path::new("/home/alice"),
            RemoteArchitecture::X86_64,
        )
        .await
        .expect("config should resolve");

        let args = build_rsync_down_args(
            "alice@example.com",
            &resolved.sync_bidi[0],
            &RemoteCliOptions::default(),
            false,
        );

        assert!(args.iter().any(|arg| arg == "--copy-links"));
        assert!(!args.iter().any(|arg| arg == "--links"));
    }

    #[tokio::test]
    async fn build_rsync_bidi_args_include_delete_for_both_directions() {
        let resolved = resolve_runtime_config(
            &sample_full_config(),
            None,
            Path::new("/home/alice"),
            RemoteArchitecture::X86_64,
        )
        .await
        .expect("config should resolve");

        let up_args = build_rsync_up_args(
            "alice@example.com",
            &resolved.sync_bidi[0],
            &RemoteCliOptions::default(),
            false,
        )
        .expect("bidi upload args should build");
        let down_args = build_rsync_down_args(
            "alice@example.com",
            &resolved.sync_bidi[0],
            &RemoteCliOptions::default(),
            false,
        );

        assert!(up_args.iter().any(|arg| arg == "--update"));
        assert!(up_args.iter().any(|arg| arg == "--delete"));
        assert!(down_args.iter().any(|arg| arg == "--delete"));
        assert!(!down_args.iter().any(|arg| arg == "--update"));
    }

    #[tokio::test]
    async fn build_remote_tui_command_with_relay_keeps_relay_for_remote_review_fallbacks() {
        let resolved = resolve_runtime_config(
            &sample_full_config(),
            None,
            Path::new("/home/alice"),
            RemoteArchitecture::X86_64,
        )
        .await
        .expect("config should resolve");

        let remote_tui_command_with_relay = build_remote_tui_command_with_relay_and_options(
            &resolved,
            Some(Path::new(
                "/home/alice/dev/agent-work/.multicode/remote/relay/123.sock",
            )),
            &RemoteCliOptions::default(),
        );

        assert!(remote_tui_command_with_relay.contains("'--relay-socket'"));
    }

    #[test]
    fn rewrite_review_path_to_local_sync_root_maps_remote_bidi_path() {
        let mappings = vec![ResolvedSyncPathMapping {
            local: PathBuf::from("/tmp/agent-work"),
            remote: PathBuf::from("/remote-home/dev/agent-work"),
            exclude: Vec::new(),
            dereference_symlinks: false,
            local_is_dir: true,
            directory_sync_target: DirectorySyncTarget::ExactDestination,
        }];

        let actual = rewrite_review_path_to_local_sync_root(
            &mappings,
            "/remote-home/dev/agent-work/core11241/repo",
        );

        assert_eq!(actual, Some("/tmp/agent-work/core11241/repo".to_string()));
    }

    #[test]
    fn rewrite_review_path_to_local_sync_root_leaves_unmapped_path_untranslated() {
        let mappings = vec![ResolvedSyncPathMapping {
            local: PathBuf::from("/tmp/agent-work"),
            remote: PathBuf::from("/remote-home/dev/agent-work"),
            exclude: Vec::new(),
            dereference_symlinks: false,
            local_is_dir: true,
            directory_sync_target: DirectorySyncTarget::ExactDestination,
        }];

        let actual = rewrite_review_path_to_local_sync_root(&mappings, "/srv/other/path/repo");

        assert_eq!(actual, None);
    }

    #[test]
    fn filter_rsync_stderr_suppresses_vanished_warnings() {
        let stderr = concat!(
            "rsync warning: some files vanished before they could be transferred (code 24) at main.c(1338) [sender=3.2.7]
",
            "file has vanished: \"/tmp/example\"
",
            "directory has vanished: \"/tmp/retry\"
",
        );

        assert_eq!(filter_rsync_stderr(stderr), "");
    }

    #[test]
    fn filter_rsync_stderr_keeps_real_errors() {
        let stderr = concat!(
            "Permission denied (publickey).
",
            "rsync error: unexplained error (code 255) at io.c(232) [sender=3.2.7]
",
        );

        assert_eq!(
            filter_rsync_stderr(stderr),
            "Permission denied (publickey).
rsync error: unexplained error (code 255) at io.c(232) [sender=3.2.7]"
        );
    }
}

async fn fetch_remote_home_directory(
    args: &CliArgs,
    options: &RemoteCliOptions,
) -> io::Result<PathBuf> {
    let output = Command::new("ssh")
        .args(build_ssh_base_args(&args.ssh_uri, options))
        .arg("printf %s \"$HOME\"")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .await?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "failed to resolve remote $HOME with status {}",
            output.status
        )));
    }
    let home = String::from_utf8(output.stdout)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    if home.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote $HOME resolved to an empty string",
        ));
    }
    Ok(PathBuf::from(home.trim()))
}

async fn fetch_remote_architecture(
    args: &CliArgs,
    options: &RemoteCliOptions,
) -> io::Result<RemoteArchitecture> {
    let output = Command::new("ssh")
        .args(build_ssh_base_args(&args.ssh_uri, options))
        .arg("uname -m")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .await?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "failed to resolve remote architecture with status {}",
            output.status
        )));
    }
    let architecture = String::from_utf8(output.stdout)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    parse_remote_architecture(&architecture)
}

fn parse_remote_architecture(value: &str) -> io::Result<RemoteArchitecture> {
    match value.trim() {
        "x86_64" | "amd64" => Ok(RemoteArchitecture::X86_64),
        "aarch64" | "arm64" => Ok(RemoteArchitecture::Aarch64),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported remote architecture '{other}'"),
        )),
    }
}
