use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::runtime_env;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceConfig {
    pub tracker: TrackerConfig,
    pub polling: PollingConfig,
    pub observability: ObservabilityConfig,
    pub workspace: WorkspaceConfig,
    pub hooks: HookConfig,
    pub agent: AgentConfig,
    pub codex: CodexConfig,
    pub server: ServerConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackerConfig {
    pub kind: Option<String>,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub project_slug: Option<String>,
    pub fixture_path: Option<PathBuf>,
    pub assignee: Option<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PollingConfig {
    pub interval_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    pub terminal_enabled: bool,
    pub refresh_ms: u64,
    pub render_interval_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookConfig {
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub max_concurrent_agents: usize,
    pub max_turns: usize,
    pub max_retry_backoff_ms: u64,
    pub max_concurrent_agents_by_state: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodexConfig {
    pub command: String,
    pub approval_policy: Value,
    pub thread_sandbox: Value,
    pub turn_sandbox_policy: Value,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Error, Clone)]
pub enum ConfigError {
    #[error("invalid_workflow_config {0}")]
    Invalid(String),
    #[error("missing_tracker_kind")]
    MissingTrackerKind,
    #[error("unsupported_tracker_kind {0}")]
    UnsupportedTrackerKind(String),
    #[error("missing_tracker_api_key")]
    MissingTrackerApiKey,
    #[error("missing_tracker_project_slug")]
    MissingTrackerProjectSlug,
    #[error("missing_tracker_fixture_path")]
    MissingTrackerFixturePath,
    #[error("missing_codex_command")]
    MissingCodexCommand,
}

impl ServiceConfig {
    pub fn from_map(config: &Map<String, Value>) -> Result<Self, ConfigError> {
        let tracker = parse_tracker_config(config.get("tracker"))?;
        let polling = parse_polling_config(config.get("polling"))?;
        let observability = parse_observability_config(config.get("observability"))?;
        let workspace = parse_workspace_config(config.get("workspace"))?;
        let hooks = parse_hook_config(config.get("hooks"))?;
        let agent = parse_agent_config(config.get("agent"))?;
        let codex = parse_codex_config(config.get("codex"), &workspace.root)?;
        let server = parse_server_config(config.get("server"))?;

        Ok(Self {
            tracker,
            polling,
            observability,
            workspace,
            hooks,
            agent,
            codex,
            server,
        })
    }

    pub fn validate_dispatch_ready(&self) -> Result<(), ConfigError> {
        let kind = self
            .tracker
            .kind
            .as_deref()
            .ok_or(ConfigError::MissingTrackerKind)?;

        match kind {
            "linear" => {
                if self.tracker.api_key.is_none() {
                    return Err(ConfigError::MissingTrackerApiKey);
                }
                if self.tracker.project_slug.is_none() {
                    return Err(ConfigError::MissingTrackerProjectSlug);
                }
            }
            "memory" => {
                if self.tracker.fixture_path.is_none() {
                    return Err(ConfigError::MissingTrackerFixturePath);
                }
            }
            other => return Err(ConfigError::UnsupportedTrackerKind(other.to_string())),
        }
        if self.codex.command.is_empty() {
            return Err(ConfigError::MissingCodexCommand);
        }
        Ok(())
    }

    pub fn active_state_set(&self) -> BTreeSet<String> {
        self.tracker
            .active_states
            .iter()
            .map(|value| normalize_state_key(value))
            .filter(|value| !value.is_empty())
            .collect()
    }

    pub fn terminal_state_set(&self) -> BTreeSet<String> {
        self.tracker
            .terminal_states
            .iter()
            .map(|value| normalize_state_key(value))
            .filter(|value| !value.is_empty())
            .collect()
    }

    pub fn max_concurrent_agents_for_state(&self, state: &str) -> usize {
        let state_key = normalize_state_key(state);
        self.agent
            .max_concurrent_agents_by_state
            .get(&state_key)
            .copied()
            .unwrap_or(self.agent.max_concurrent_agents)
    }
}

fn parse_tracker_config(value: Option<&Value>) -> Result<TrackerConfig, ConfigError> {
    let map = as_object(value, "tracker")?;
    let api_key = resolve_secret(map.get("api_key"), "LINEAR_API_KEY")?;
    let assignee = resolve_secret(map.get("assignee"), "LINEAR_ASSIGNEE")?;

    Ok(TrackerConfig {
        kind: optional_string(map.get("kind"), "tracker.kind")?,
        endpoint: optional_string(map.get("endpoint"), "tracker.endpoint")?
            .unwrap_or_else(|| "https://api.linear.app/graphql".to_string()),
        api_key,
        project_slug: resolve_env_string(map.get("project_slug"), "tracker.project_slug")?,
        fixture_path: optional_string(map.get("fixture_path"), "tracker.fixture_path")?.map(
            |path| resolve_path_value(Some(path.as_str()), PathBuf::from("memory_issues.json")),
        ),
        assignee,
        active_states: parse_string_list(
            map.get("active_states"),
            "tracker.active_states",
            &["Todo", "In Progress"],
        )?,
        terminal_states: parse_string_list(
            map.get("terminal_states"),
            "tracker.terminal_states",
            &["Closed", "Cancelled", "Canceled", "Duplicate", "Done"],
        )?,
    })
}

fn parse_polling_config(value: Option<&Value>) -> Result<PollingConfig, ConfigError> {
    let map = as_object(value, "polling")?;
    Ok(PollingConfig {
        interval_ms: parse_positive_u64(map.get("interval_ms"), "polling.interval_ms")?
            .unwrap_or(30_000),
    })
}

fn parse_observability_config(value: Option<&Value>) -> Result<ObservabilityConfig, ConfigError> {
    let map = as_object(value, "observability")?;
    let terminal_enabled = parse_bool(
        map.get("terminal_enabled"),
        "observability.terminal_enabled",
    )?;
    let legacy_dashboard_enabled = parse_bool(
        map.get("dashboard_enabled"),
        "observability.dashboard_enabled",
    )?;

    Ok(ObservabilityConfig {
        // Accept Elixir's legacy `dashboard_enabled` key so migrated workflows keep
        // their operator-surface intent without renaming the field immediately.
        terminal_enabled: terminal_enabled
            .or(legacy_dashboard_enabled)
            .unwrap_or(true),
        refresh_ms: parse_positive_u64(map.get("refresh_ms"), "observability.refresh_ms")?
            .unwrap_or(1_000),
        render_interval_ms: parse_positive_u64(
            map.get("render_interval_ms"),
            "observability.render_interval_ms",
        )?
        .unwrap_or(250),
    })
}

fn parse_workspace_config(value: Option<&Value>) -> Result<WorkspaceConfig, ConfigError> {
    let map = as_object(value, "workspace")?;
    let root = optional_string(map.get("root"), "workspace.root")?;
    Ok(WorkspaceConfig {
        root: resolve_path_value(root.as_deref(), default_workspace_root()),
    })
}

fn parse_hook_config(value: Option<&Value>) -> Result<HookConfig, ConfigError> {
    let map = as_object(value, "hooks")?;
    Ok(HookConfig {
        after_create: map
            .get("after_create")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        before_run: map
            .get("before_run")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        after_run: map
            .get("after_run")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        before_remove: map
            .get("before_remove")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        timeout_ms: parse_positive_u64(map.get("timeout_ms"), "hooks.timeout_ms")?
            .unwrap_or(60_000),
    })
}

fn parse_agent_config(value: Option<&Value>) -> Result<AgentConfig, ConfigError> {
    let map = as_object(value, "agent")?;
    Ok(AgentConfig {
        max_concurrent_agents: parse_positive_u64(
            map.get("max_concurrent_agents"),
            "agent.max_concurrent_agents",
        )?
        .unwrap_or(10) as usize,
        max_turns: parse_positive_u64(map.get("max_turns"), "agent.max_turns")?.unwrap_or(20)
            as usize,
        max_retry_backoff_ms: parse_positive_u64(
            map.get("max_retry_backoff_ms"),
            "agent.max_retry_backoff_ms",
        )?
        .unwrap_or(300_000),
        max_concurrent_agents_by_state: parse_state_limits(
            map.get("max_concurrent_agents_by_state"),
        )?,
    })
}

fn parse_codex_config(
    value: Option<&Value>,
    workspace_root: &Path,
) -> Result<CodexConfig, ConfigError> {
    let map = as_object(value, "codex")?;
    Ok(CodexConfig {
        command: optional_string(map.get("command"), "codex.command")?
            .unwrap_or_else(|| "codex app-server".to_string()),
        approval_policy: map
            .get("approval_policy")
            .cloned()
            .unwrap_or_else(default_approval_policy),
        thread_sandbox: map
            .get("thread_sandbox")
            .cloned()
            .unwrap_or_else(|| Value::String("workspace-write".to_string())),
        turn_sandbox_policy: map
            .get("turn_sandbox_policy")
            .cloned()
            .unwrap_or_else(|| default_turn_sandbox_policy(workspace_root)),
        turn_timeout_ms: parse_positive_u64(map.get("turn_timeout_ms"), "codex.turn_timeout_ms")?
            .unwrap_or(3_600_000),
        read_timeout_ms: parse_positive_u64(map.get("read_timeout_ms"), "codex.read_timeout_ms")?
            .unwrap_or(5_000),
        stall_timeout_ms: parse_i64(map.get("stall_timeout_ms"), "codex.stall_timeout_ms")?
            .unwrap_or(300_000),
    })
}

fn parse_server_config(value: Option<&Value>) -> Result<ServerConfig, ConfigError> {
    let map = as_object(value, "server")?;
    let port = parse_non_negative_u64(map.get("port"), "server.port")?.map(|value| value as u16);
    Ok(ServerConfig {
        host: optional_string(map.get("host"), "server.host")?,
        port,
    })
}

fn parse_state_limits(value: Option<&Value>) -> Result<BTreeMap<String, usize>, ConfigError> {
    let map = match value {
        None | Some(Value::Null) => return Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map,
        _ => {
            return Err(ConfigError::Invalid(
                "agent.max_concurrent_agents_by_state must be an object".to_string(),
            ));
        }
    };

    let mut parsed = BTreeMap::new();
    for (state, raw_limit) in map {
        let normalized_state = normalize_state_key(state);
        if normalized_state.is_empty() {
            return Err(ConfigError::Invalid(
                "agent.max_concurrent_agents_by_state state names must not be blank".to_string(),
            ));
        }

        let limit = parse_positive_u64(Some(raw_limit), "agent.max_concurrent_agents_by_state")?
            .ok_or_else(|| {
                ConfigError::Invalid(
                    "agent.max_concurrent_agents_by_state limits must be positive integers"
                        .to_string(),
                )
            })?;

        parsed.insert(normalized_state, limit as usize);
    }

    Ok(parsed)
}

fn parse_string_list(
    value: Option<&Value>,
    field: &str,
    default: &[&str],
) -> Result<Vec<String>, ConfigError> {
    match value {
        None | Some(Value::Null) => Ok(default.iter().map(|value| (*value).to_string()).collect()),
        Some(Value::String(raw)) => Ok(raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| match value {
                Value::String(text) => Ok(text.clone()),
                _ => Err(ConfigError::Invalid(format!(
                    "{field} must be a string list"
                ))),
            })
            .collect(),
        _ => Err(ConfigError::Invalid(format!(
            "{field} must be a string list"
        ))),
    }
}

fn as_object<'a>(
    value: Option<&'a Value>,
    field: &str,
) -> Result<&'a Map<String, Value>, ConfigError> {
    match value {
        None | Some(Value::Null) => Ok(empty_object()),
        Some(Value::Object(map)) => Ok(map),
        _ => Err(ConfigError::Invalid(format!("{field} must be an object"))),
    }
}

fn empty_object() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

fn optional_string(value: Option<&Value>, field: &str) -> Result<Option<String>, ConfigError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.to_string())),
        _ => Err(ConfigError::Invalid(format!("{field} must be a string"))),
    }
}

fn parse_bool(value: Option<&Value>, field: &str) -> Result<Option<bool>, ConfigError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(ConfigError::Invalid(format!("{field} must be a boolean"))),
        },
        _ => Err(ConfigError::Invalid(format!("{field} must be a boolean"))),
    }
}

fn parse_positive_u64(value: Option<&Value>, field: &str) -> Result<Option<u64>, ConfigError> {
    let parsed = parse_non_negative_u64(value, field)?;
    match parsed {
        Some(0) => Err(ConfigError::Invalid(format!(
            "{field} must be greater than 0"
        ))),
        other => Ok(other),
    }
}

fn parse_non_negative_u64(value: Option<&Value>, field: &str) -> Result<Option<u64>, ConfigError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| ConfigError::Invalid(format!("{field} must be a non-negative integer")))
            .map(Some),
        Some(Value::String(text)) => {
            text.trim().parse::<u64>().map(Some).map_err(|_| {
                ConfigError::Invalid(format!("{field} must be a non-negative integer"))
            })
        }
        _ => Err(ConfigError::Invalid(format!(
            "{field} must be a non-negative integer"
        ))),
    }
}

fn parse_i64(value: Option<&Value>, field: &str) -> Result<Option<i64>, ConfigError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .ok_or_else(|| ConfigError::Invalid(format!("{field} must be an integer")))
            .map(Some),
        Some(Value::String(text)) => text
            .trim()
            .parse::<i64>()
            .map(Some)
            .map_err(|_| ConfigError::Invalid(format!("{field} must be an integer"))),
        _ => Err(ConfigError::Invalid(format!("{field} must be an integer"))),
    }
}

fn resolve_secret(value: Option<&Value>, env_name: &str) -> Result<Option<String>, ConfigError> {
    let value = optional_string(value, env_name)?;
    Ok(match value {
        None => normalize_string(runtime_env::get(env_name)),
        Some(value) => {
            if let Some(reference) = env_reference(&value) {
                normalize_string(runtime_env::get(reference))
            } else {
                normalize_string(Some(value))
            }
        }
    })
}

fn resolve_env_string(value: Option<&Value>, field: &str) -> Result<Option<String>, ConfigError> {
    let value = optional_string(value, field)?;
    Ok(match value {
        None => None,
        Some(value) => {
            if let Some(reference) = env_reference(&value) {
                normalize_string(runtime_env::get(reference))
            } else {
                normalize_string(Some(value))
            }
        }
    })
}

fn normalize_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| if value.is_empty() { None } else { Some(value) })
}

fn env_reference(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('$')
        .filter(|name| !name.is_empty())
        .filter(|name| {
            name.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        })
}

fn resolve_path_value(value: Option<&str>, default: PathBuf) -> PathBuf {
    let raw = value
        .and_then(|value| {
            env_reference(value)
                .map(runtime_env::get)
                .unwrap_or_else(|| Some(value.to_string()))
        })
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.display().to_string());

    let expanded = shellexpand::tilde(&raw).to_string();
    let path = PathBuf::from(&expanded);

    if path.is_absolute() {
        return path;
    }
    if raw.contains(std::path::MAIN_SEPARATOR)
        || raw.contains('/')
        || raw.contains('\\')
        || raw.starts_with('~')
    {
        return env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path);
    }
    PathBuf::from(raw)
}

fn default_workspace_root() -> PathBuf {
    env::temp_dir().join("symphony_workspaces")
}

fn default_approval_policy() -> Value {
    serde_json::json!({
        "reject": {
            "sandbox_approval": true,
            "rules": true,
            "mcp_elicitations": true
        }
    })
}

fn default_turn_sandbox_policy(workspace_root: &Path) -> Value {
    serde_json::json!({
        "type": "workspaceWrite",
        "writableRoots": [workspace_root],
        "readOnlyAccess": { "type": "fullAccess" },
        "networkAccess": false,
        "excludeTmpdirEnvVar": false,
        "excludeSlashTmp": false
    })
}

pub fn normalize_state_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::ServiceConfig;
    use crate::runtime_env;
    use serde_json::json;
    use tempfile::tempdir;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn parses_defaults_and_limits() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                },
                "agent": {
                    "max_concurrent_agents_by_state": {
                        "In Progress": 2,
                        "Review": 3
                    }
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(config.polling.interval_ms, 30_000);
        assert_eq!(config.max_concurrent_agents_for_state("in progress"), 2);
        assert_eq!(config.max_concurrent_agents_for_state("review"), 3);
    }

    #[test]
    fn supports_csv_state_config() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj",
                    "active_states": "Todo, In Progress, Review"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(
            config.tracker.active_states,
            vec!["Todo", "In Progress", "Review"]
        );
    }

    #[test]
    fn default_turn_sandbox_policy_matches_current_codex_schema() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(
            config.codex.turn_sandbox_policy.get("networkAccess"),
            Some(&json!(false))
        );
    }

    #[test]
    fn codex_defaults_match_spec() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(
            config.codex.approval_policy,
            json!({
                "reject": {
                    "sandbox_approval": true,
                    "rules": true,
                    "mcp_elicitations": true
                }
            })
        );
        assert_eq!(config.codex.thread_sandbox, json!("workspace-write"));
        assert_eq!(config.codex.turn_timeout_ms, 3_600_000);
        assert_eq!(config.codex.read_timeout_ms, 5_000);
        assert_eq!(config.codex.stall_timeout_ms, 300_000);
        assert!(config.observability.terminal_enabled);
        assert_eq!(config.observability.refresh_ms, 1_000);
        assert_eq!(config.observability.render_interval_ms, 250);
    }

    #[test]
    fn preserves_explicit_observability_values() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                },
                "observability": {
                    "terminal_enabled": false,
                    "refresh_ms": 2_500,
                    "render_interval_ms": 500
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert!(!config.observability.terminal_enabled);
        assert_eq!(config.observability.refresh_ms, 2_500);
        assert_eq!(config.observability.render_interval_ms, 500);
    }

    #[test]
    fn supports_legacy_dashboard_enabled_alias() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                },
                "observability": {
                    "dashboard_enabled": false
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert!(!config.observability.terminal_enabled);
    }

    #[test]
    fn terminal_enabled_takes_precedence_over_legacy_dashboard_enabled() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                },
                "observability": {
                    "terminal_enabled": true,
                    "dashboard_enabled": false
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert!(config.observability.terminal_enabled);
    }

    #[test]
    fn preserves_explicit_codex_passthrough_values() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                },
                "codex": {
                    "approval_policy": "future-policy",
                    "thread_sandbox": "future-sandbox",
                    "turn_sandbox_policy": {
                        "type": "futureSandbox",
                        "flag": true
                    }
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(config.codex.approval_policy, json!("future-policy"));
        assert_eq!(config.codex.thread_sandbox, json!("future-sandbox"));
        assert_eq!(
            config.codex.turn_sandbox_policy,
            json!({
                "type": "futureSandbox",
                "flag": true
            })
        );
    }

    #[test]
    fn memory_tracker_requires_fixture_path() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "memory"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let error = config.validate_dispatch_ready().unwrap_err();
        assert!(matches!(
            error,
            super::ConfigError::MissingTrackerFixturePath
        ));
    }

    #[test]
    fn memory_tracker_accepts_fixture_path() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "memory",
                    "fixture_path": "/tmp/memory-tracker.json"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(
            config.tracker.fixture_path.as_deref(),
            Some(std::path::Path::new("/tmp/memory-tracker.json"))
        );
        config.validate_dispatch_ready().expect("dispatch ready");
    }

    #[test]
    fn config_reads_runtime_env_overlay_without_exporting_process_env() {
        let _guard = env_lock().lock().expect("env lock");
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        std::fs::write(&workflow_path, "---\ntracker:\n  kind: linear\n---\n").expect("workflow");
        std::fs::write(
            dir.path().join(".env"),
            "LINEAR_API_KEY=dotenv-token\nSYMPHONY_WORKSPACE_ROOT=/tmp/overlay-root\n",
        )
        .expect("dotenv");
        unsafe {
            std::env::remove_var("LINEAR_API_KEY");
            std::env::remove_var("SYMPHONY_WORKSPACE_ROOT");
        }
        runtime_env::clear_for_tests();
        runtime_env::load_dotenv_for_workflow(&workflow_path).expect("dotenv");

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "$LINEAR_API_KEY",
                    "project_slug": "proj"
                },
                "workspace": {
                    "root": "$SYMPHONY_WORKSPACE_ROOT"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(config.tracker.api_key.as_deref(), Some("dotenv-token"));
        assert_eq!(
            config.workspace.root,
            std::path::Path::new("/tmp/overlay-root")
        );
        assert!(std::env::var("LINEAR_API_KEY").is_err());
        assert!(std::env::var("SYMPHONY_WORKSPACE_ROOT").is_err());

        runtime_env::clear_for_tests();
    }

    #[test]
    fn rejects_invalid_state_limits() {
        let error = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                },
                "agent": {
                    "max_concurrent_agents_by_state": {
                        "Review": 0
                    }
                }
            })
            .as_object()
            .expect("object"),
        )
        .unwrap_err();

        assert!(matches!(error, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn rejects_invalid_hook_timeout() {
        let error = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                },
                "hooks": {
                    "timeout_ms": 0
                }
            })
            .as_object()
            .expect("object"),
        )
        .unwrap_err();

        assert!(matches!(error, super::ConfigError::Invalid(_)));
    }

    #[test]
    fn resolves_project_slug_from_env_reference() {
        let _guard = env_lock().lock().expect("env lock");
        let env_name = "SYMPHONY_TEST_PROJECT_SLUG";
        unsafe {
            std::env::set_var(env_name, "proj-from-env");
        }

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": format!("${env_name}")
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(
            config.tracker.project_slug.as_deref(),
            Some("proj-from-env")
        );

        unsafe {
            std::env::remove_var(env_name);
        }
    }

    #[test]
    fn resolves_workspace_root_from_env_reference() {
        let _guard = env_lock().lock().expect("env lock");
        let dir = tempdir().expect("tempdir");
        let env_name = "SYMPHONY_TEST_WORKSPACE_ROOT";
        unsafe {
            std::env::set_var(env_name, dir.path());
        }

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                },
                "workspace": {
                    "root": format!("${env_name}")
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(config.workspace.root, dir.path());

        unsafe {
            std::env::remove_var(env_name);
        }
    }
}
