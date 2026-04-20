use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use size::Size;

use super::CombinedServiceError;
use crate::RuntimeBackend;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    #[serde(default = "default_workspace_directory")]
    pub workspace_directory: String,
    pub isolation: IsolationConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub autonomous: AutonomousConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default = "default_opencode_commands")]
    pub opencode: Vec<String>,
    #[serde(default)]
    pub compare: CompareConfig,
    #[serde(default)]
    pub tool: Vec<ToolConfig>,
    #[serde(default)]
    pub handler: HandlerConfig,
    #[serde(default)]
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub github: GithubConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct AutonomousConfig {
    #[serde(
        default = "default_issue_scan_delay_seconds",
        alias = "issue-scan-delay-seconds"
    )]
    pub issue_scan_delay_seconds: u64,
    #[serde(default = "default_max_parallel_issues", alias = "max-parallel-issues")]
    pub max_parallel_issues: usize,
    #[serde(default = "default_scan_on_startup", alias = "scan-on-startup")]
    pub scan_on_startup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct CompareConfig {
    #[serde(default)]
    pub tool: CompareTool,
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CompareTool {
    #[default]
    Vscode,
    Intellij,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgentProvider {
    #[default]
    Opencode,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct AgentConfig {
    #[serde(default)]
    pub provider: AgentProvider,
    #[serde(default)]
    pub opencode: OpencodeAgentConfig,
    #[serde(default)]
    pub codex: CodexAgentConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct OpencodeAgentConfig {
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct CodexAgentConfig {
    #[serde(default = "default_codex_commands")]
    pub commands: Vec<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub approval_policy: CodexApprovalPolicy,
    #[serde(default)]
    pub sandbox_mode: CodexSandboxMode,
    #[serde(default)]
    pub network_access: CodexNetworkAccess,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CodexApprovalPolicy {
    Untrusted,
    OnFailure,
    #[default]
    OnRequest,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CodexSandboxMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
    ExternalSandbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CodexNetworkAccess {
    Restricted,
    #[default]
    Enabled,
}

impl Default for AutonomousConfig {
    fn default() -> Self {
        Self {
            issue_scan_delay_seconds: default_issue_scan_delay_seconds(),
            max_parallel_issues: default_max_parallel_issues(),
            scan_on_startup: default_scan_on_startup(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct RuntimeConfig {
    #[serde(default)]
    pub backend: RuntimeBackend,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub opencode_image: Option<String>,
    #[serde(default)]
    pub codex_image: Option<String>,
}

impl RuntimeConfig {
    pub fn resolved_image(&self, provider: AgentProvider) -> Option<&str> {
        self.image.as_deref().or(match provider {
            AgentProvider::Opencode => self.opencode_image.as_deref(),
            AgentProvider::Codex => self.codex_image.as_deref(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct GithubConfig {
    #[serde(default)]
    pub token: Option<GithubTokenConfig>,
    #[serde(default)]
    pub populate_git_credentials: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct GithubTokenConfig {
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub keychain_service: Option<String>,
    #[serde(default)]
    pub keychain_account: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct RemoteConfig {
    #[serde(default = "default_remote_sync_interval_seconds")]
    pub sync_interval_seconds: u64,
    #[serde(default)]
    pub sync_up: Vec<RemoteSyncMappingConfig>,
    #[serde(default)]
    pub sync_bidi: Vec<RemoteSyncMappingConfig>,
    pub install: RemoteInstallConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct RemoteSyncMappingConfig {
    pub local: String,
    pub remote: String,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub dereference_symlinks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct RemoteInstallConfig {
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct HandlerConfig {
    #[serde(default = "default_handler_review")]
    pub review: String,
    #[serde(default)]
    pub review_pty: bool,
    #[serde(default = "default_handler_web")]
    pub web: String,
}

impl Default for HandlerConfig {
    fn default() -> Self {
        Self {
            review: default_handler_review(),
            review_pty: false,
            web: default_handler_web(),
        }
    }
}

fn default_opencode_commands() -> Vec<String> {
    vec!["opencode-cli".to_string(), "opencode".to_string()]
}

fn default_workspace_directory() -> String {
    "~/dev/multicode-workspaces".to_string()
}

fn default_codex_commands() -> Vec<String> {
    vec!["codex".to_string()]
}

fn default_remote_sync_interval_seconds() -> u64 {
    2
}

fn default_issue_scan_delay_seconds() -> u64 {
    15 * 60
}

fn default_max_parallel_issues() -> usize {
    5
}

fn default_scan_on_startup() -> bool {
    true
}

fn default_handler_review() -> String {
    "/usr/bin/smerge".to_string()
}

fn default_handler_web() -> String {
    "/usr/bin/firefox {}".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    Exec,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ToolConfig {
    #[serde(rename = "type")]
    pub type_: ToolType,
    pub name: String,
    pub key: String,
    #[serde(default)]
    pub exec: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct IsolationConfig {
    #[serde(default)]
    pub readable: Vec<String>,
    #[serde(default)]
    pub writable: Vec<String>,
    #[serde(default)]
    pub isolated: Vec<String>,
    #[serde(default)]
    pub tmpfs: Vec<String>,
    #[serde(default, alias = "add-skills-from")]
    pub add_skills_from: Vec<String>,
    #[serde(default, alias = "inherit-env")]
    pub inherit_env: Vec<String>,
    #[serde(default, alias = "memory-high")]
    pub memory_high: Option<String>,
    #[serde(default, alias = "memory-max")]
    pub memory_max: Option<String>,
    #[serde(default)]
    pub cpu: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedSkillMount {
    pub source: PathBuf,
    pub target: PathBuf,
}

#[derive(Debug, Clone)]
pub(super) struct ExpandedIsolationConfig {
    pub(super) readable: Vec<PathBuf>,
    pub(super) writable: Vec<PathBuf>,
    pub(super) isolated: Vec<PathBuf>,
    pub(super) tmpfs: Vec<PathBuf>,
    pub(super) added_skills: Vec<AddedSkillMount>,
    pub(super) inherit_env: Vec<String>,
    pub(super) memory_high_bytes: Option<u64>,
    pub(super) memory_max_bytes: Option<u64>,
    pub(super) cpu: Option<String>,
}

impl ExpandedIsolationConfig {
    pub(super) fn from_config(
        config: &IsolationConfig,
        config_path: Option<&Path>,
    ) -> Result<Self, CombinedServiceError> {
        Ok(Self {
            readable: expand_isolation_paths(&config.readable, "readable")?,
            writable: expand_isolation_paths(&config.writable, "writable")?,
            isolated: expand_isolation_paths(&config.isolated, "isolated")?,
            tmpfs: expand_isolation_paths(&config.tmpfs, "tmpfs")?,
            added_skills: resolve_added_skill_mounts(&config.add_skills_from, config_path)?,
            inherit_env: config.inherit_env.clone(),
            memory_high_bytes: parse_optional_size_bytes(
                config.memory_high.as_deref(),
                "memory_high",
            )?,
            memory_max_bytes: parse_optional_size_bytes(
                config.memory_max.as_deref(),
                "memory_max",
            )?,
            cpu: config.cpu.clone(),
        })
    }
}

pub fn parse_optional_size_bytes(
    raw: Option<&str>,
    field: &str,
) -> Result<Option<u64>, CombinedServiceError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };

    if let Some(value) = parse_decimal_size_bytes(raw, "GB", 1_000_000_000) {
        return value
            .map(Some)
            .map_err(|message| CombinedServiceError::InvalidIsolationSize {
                field: field.to_string(),
                value: raw.to_string(),
                message,
            });
    }
    if let Some(value) = parse_decimal_size_bytes(raw, "MB", 1_000_000) {
        return value
            .map(Some)
            .map_err(|message| CombinedServiceError::InvalidIsolationSize {
                field: field.to_string(),
                value: raw.to_string(),
                message,
            });
    }

    Size::from_str(raw)
        .map(|size| Some(size.bytes() as u64))
        .map_err(|err| CombinedServiceError::InvalidIsolationSize {
            field: field.to_string(),
            value: raw.to_string(),
            message: err.to_string(),
        })
}

fn parse_decimal_size_bytes(
    raw: &str,
    suffix: &str,
    multiplier: u64,
) -> Option<Result<u64, String>> {
    let number = raw.strip_suffix(suffix)?.trim();
    if number.is_empty() {
        return Some(Err(format!("missing numeric value before {suffix}")));
    }

    let value = match number.parse::<u64>() {
        Ok(value) => value,
        Err(err) => return Some(Err(err.to_string())),
    };

    Some(
        value
            .checked_mul(multiplier)
            .ok_or_else(|| format!("value too large for {suffix}")),
    )
}

pub async fn read_config(config_path: &Path) -> Result<Config, CombinedServiceError> {
    let raw = tokio::fs::read_to_string(config_path).await?;
    Ok(toml::from_str(&raw)?)
}

pub(super) fn resolve_agent_command(candidates: &[String]) -> Result<String, CombinedServiceError> {
    let normalized = candidates
        .iter()
        .map(|candidate| candidate.trim())
        .filter(|candidate| !candidate.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    for candidate in &normalized {
        if let Some(path) = resolve_executable(candidate) {
            return Ok(path.to_string_lossy().into_owned());
        }
    }

    Err(CombinedServiceError::AgentCommandNotFound {
        candidates: normalized,
    })
}

fn resolve_executable(candidate: &str) -> Option<PathBuf> {
    let candidate_path = Path::new(candidate);
    if candidate_path.components().count() > 1 {
        return is_executable_file(candidate_path).then(|| candidate_path.to_path_buf());
    }

    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(candidate))
        .find(|path| is_executable_file(path))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

pub(super) fn path_looks_like_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            let trimmed = name.trim_start_matches('.');
            !trimmed.is_empty() && trimmed.contains('.')
        })
}

pub(super) fn validate_workspace_key(key: &str) -> Result<String, CombinedServiceError> {
    let key = key.trim();
    if key.is_empty() || key.contains('/') || key.contains('\\') {
        return Err(CombinedServiceError::InvalidWorkspaceKey(key.to_string()));
    }
    Ok(key.to_string())
}

pub(super) fn expand_shell_path(value: &str) -> Result<PathBuf, CombinedServiceError> {
    let expanded = shellexpand::full_with_context(
        value,
        || env::var("HOME").ok(),
        |name| match env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(synthesized_env_value(name)),
            Err(err) => Err(err.to_string()),
        },
    )
    .map_err(|err| CombinedServiceError::ShellExpand(err.to_string()))?;
    Ok(PathBuf::from(expanded.into_owned()))
}

pub(super) fn inherited_env_value(name: &str) -> Option<String> {
    env::var(name).ok().or_else(|| synthesized_env_value(name))
}

pub(super) fn synthesized_env_value(name: &str) -> Option<String> {
    match name {
        "XDG_RUNTIME_DIR" => {
            synthesized_xdg_runtime_dir().map(|path| path.to_string_lossy().into_owned())
        }
        _ => None,
    }
}

pub(super) fn synthesized_xdg_runtime_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(env::temp_dir().join("multicode-runtime"))
    }

    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn expand_isolation_paths(
    paths: &[String],
    field: &str,
) -> Result<Vec<PathBuf>, CombinedServiceError> {
    paths
        .iter()
        .map(|path| {
            let path = expand_shell_path(path)?;
            if !path.is_absolute() {
                return Err(CombinedServiceError::InvalidIsolationPath {
                    field: field.to_string(),
                    path,
                });
            }
            Ok(path)
        })
        .collect()
}

fn resolve_added_skill_mounts(
    paths: &[String],
    config_path: Option<&Path>,
) -> Result<Vec<AddedSkillMount>, CombinedServiceError> {
    let Some(config_path) = config_path else {
        return Ok(Vec::new());
    };
    let config_directory = config_path.parent().unwrap_or_else(|| Path::new("."));
    let skills_root = expand_shell_path("~/.config/opencode/skills")?;
    let mut mounts = BTreeMap::<PathBuf, PathBuf>::new();

    for relative_dir in paths {
        let source_root = fs::canonicalize(config_directory.join(relative_dir))?;
        for entry in fs::read_dir(&source_root)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_dir() {
                continue;
            }
            mounts.insert(
                fs::canonicalize(entry.path())?,
                skills_root.join(entry.file_name()),
            );
        }
    }

    Ok(mounts
        .into_iter()
        .map(|(source, target)| AddedSkillMount { source, target })
        .collect())
}

pub(super) fn validate_tool_config_entries(
    tools: &[ToolConfig],
) -> Result<(), CombinedServiceError> {
    let mut seen_keys = HashSet::new();
    let reserved = ['q', 'a', 'd', 'f', 's'];

    for (index, tool) in tools.iter().enumerate() {
        if tool.name.trim().is_empty() {
            return Err(CombinedServiceError::InvalidToolConfig {
                index,
                message: "tool name must not be empty".to_string(),
            });
        }

        let mut key_chars = tool.key.chars();
        let Some(key_char) = key_chars.next() else {
            return Err(CombinedServiceError::InvalidToolConfig {
                index,
                message: "tool key must not be empty".to_string(),
            });
        };
        if key_chars.next().is_some() {
            return Err(CombinedServiceError::InvalidToolConfig {
                index,
                message: "tool key must be a single character".to_string(),
            });
        }
        if reserved.contains(&key_char) {
            return Err(CombinedServiceError::InvalidToolConfig {
                index,
                message: format!("tool key '{key_char}' is reserved"),
            });
        }
        if !seen_keys.insert(key_char) {
            return Err(CombinedServiceError::InvalidToolConfig {
                index,
                message: format!("duplicate tool key '{key_char}'"),
            });
        }

        match tool.type_ {
            ToolType::Exec => {
                if tool
                    .exec
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .is_empty()
                {
                    return Err(CombinedServiceError::InvalidToolConfig {
                        index,
                        message: "exec tool requires non-empty `exec` field".to_string(),
                    });
                }
                if tool
                    .prompt
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|prompt| !prompt.is_empty())
                {
                    return Err(CombinedServiceError::InvalidToolConfig {
                        index,
                        message: "exec tool must not set `prompt`".to_string(),
                    });
                }
            }
            ToolType::Prompt => {
                if tool
                    .prompt
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or_default()
                    .is_empty()
                {
                    return Err(CombinedServiceError::InvalidToolConfig {
                        index,
                        message: "prompt tool requires non-empty `prompt` field".to_string(),
                    });
                }
                if tool
                    .exec
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|exec| !exec.is_empty())
                {
                    return Err(CombinedServiceError::InvalidToolConfig {
                        index,
                        message: "prompt tool must not set `exec`".to_string(),
                    });
                }
            }
        }
    }

    Ok(())
}

pub(super) fn validate_handler_config(handler: &HandlerConfig) -> Result<(), CombinedServiceError> {
    validate_handler_template("review", &handler.review, false)?;
    validate_handler_template("web", &handler.web, true)?;
    Ok(())
}

pub(super) fn validate_remote_config(
    remote: Option<&RemoteConfig>,
) -> Result<(), CombinedServiceError> {
    let Some(remote) = remote else {
        return Ok(());
    };

    validate_non_empty_remote_field("remote.install.command", &remote.install.command)?;
    validate_remote_sync_mappings("remote.sync_up", &remote.sync_up)?;
    validate_remote_sync_mappings("remote.sync_bidi", &remote.sync_bidi)?;
    Ok(())
}

fn validate_remote_sync_mappings(
    field: &str,
    mappings: &[RemoteSyncMappingConfig],
) -> Result<(), CombinedServiceError> {
    for (index, mapping) in mappings.iter().enumerate() {
        validate_non_empty_remote_field(&format!("{field}[{index}].local"), &mapping.local)?;
        validate_non_empty_remote_field(&format!("{field}[{index}].remote"), &mapping.remote)?;
        for (exclude_index, exclude) in mapping.exclude.iter().enumerate() {
            validate_non_empty_remote_field(
                &format!("{field}[{index}].exclude[{exclude_index}]"),
                exclude,
            )?;
        }
    }
    Ok(())
}

fn validate_non_empty_remote_field(field: &str, value: &str) -> Result<(), CombinedServiceError> {
    if value.trim().is_empty() {
        return Err(CombinedServiceError::InvalidRemoteConfig {
            field: field.to_string(),
            message: format!("{field} must not be empty"),
        });
    }
    Ok(())
}

fn validate_handler_template(
    field: &str,
    template: &str,
    requires_argument_placeholder: bool,
) -> Result<(), CombinedServiceError> {
    let trimmed = template.trim();
    if trimmed.is_empty() {
        return Err(CombinedServiceError::InvalidHandlerConfig {
            field: field.to_string(),
            message: "handler command template must not be empty".to_string(),
        });
    }

    let placeholder_count = trimmed.matches("{}").count();
    if requires_argument_placeholder && placeholder_count != 1 {
        return Err(CombinedServiceError::InvalidHandlerConfig {
            field: field.to_string(),
            message: "handler command template must contain exactly one '{}' placeholder"
                .to_string(),
        });
    }
    if !requires_argument_placeholder && placeholder_count != 0 {
        return Err(CombinedServiceError::InvalidHandlerConfig {
            field: field.to_string(),
            message: "handler command template must not contain '{}' placeholder".to_string(),
        });
    }

    let Some(program) = trimmed.split_whitespace().next() else {
        return Err(CombinedServiceError::InvalidHandlerConfig {
            field: field.to_string(),
            message: "handler command template must include an executable".to_string(),
        });
    };
    if !Path::new(program).is_absolute() {
        return Err(CombinedServiceError::InvalidHandlerConfig {
            field: field.to_string(),
            message: "handler executable must be an absolute path".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    static ENV_VAR_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        old_value: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let old_value = env::var_os(key);
            unsafe {
                env::set_var(key, value);
            }
            Self { key, old_value }
        }

        fn remove(key: &'static str) -> Self {
            let old_value = env::var_os(key);
            unsafe {
                env::remove_var(key);
            }
            Self { key, old_value }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.old_value {
                unsafe {
                    env::set_var(self.key, value);
                }
            } else {
                unsafe {
                    env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn expand_shell_path_expands_existing_environment_variables() {
        let _env_lock = ENV_VAR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let runtime_dir = env::temp_dir().join(format!("multicode-config-test-{unique}"));
        let _guard = EnvVarGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        let path =
            expand_shell_path("$XDG_RUNTIME_DIR/opencode").expect("runtime dir should expand");

        assert_eq!(path, runtime_dir.join("opencode"));
    }

    #[test]
    fn config_defaults_workspace_directory_when_omitted() {
        let config: Config = toml::from_str("[isolation]\n")
            .expect("config without workspace-directory should parse");

        assert_eq!(config.workspace_directory, "~/dev/multicode-workspaces");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn expand_shell_path_synthesizes_xdg_runtime_dir_on_macos() {
        let _env_lock = ENV_VAR_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _guard = EnvVarGuard::remove("XDG_RUNTIME_DIR");

        let path =
            expand_shell_path("$XDG_RUNTIME_DIR/opencode").expect("runtime dir should expand");

        assert_eq!(
            path,
            synthesized_xdg_runtime_dir()
                .expect("macOS should synthesize XDG runtime dir")
                .join("opencode")
        );
        assert_eq!(
            inherited_env_value("XDG_RUNTIME_DIR"),
            synthesized_xdg_runtime_dir().map(|path| path.to_string_lossy().into_owned())
        );
    }
}
