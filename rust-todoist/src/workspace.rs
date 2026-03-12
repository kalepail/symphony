use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    process::Output,
    time::Duration,
};

use tokio::{process::Command, time::timeout};
use tracing::{info, warn};

use crate::{
    config::{HookConfig, ServiceConfig},
    issue::Issue,
};

const TMP_ARTIFACTS: &[&str] = &["tmp", ".elixir_ls"];
const MAX_HOOK_LOG_BYTES: usize = 2_048;
const REMOTE_WORKSPACE_MARKER: &str = "__SYMPHONY_WORKSPACE__";
const REMOTE_LIST_MARKER: &str = "__SYMPHONY_WORKSPACE_LIST__";

#[derive(Clone, Debug)]
pub struct Workspace {
    pub path: PathBuf,
    pub workspace_key: String,
    pub created_now: bool,
    pub worker_host: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace_outside_root workspace={workspace} root={root}")]
    OutsideRoot { workspace: String, root: String },
    #[error("workspace_equals_root workspace={workspace} root={root}")]
    EqualsRoot { workspace: String, root: String },
    #[error("workspace_symlink_escape path={path} root={root}")]
    SymlinkEscape { path: String, root: String },
    #[error("workspace_path_unreadable path={path} reason={reason}")]
    PathUnreadable { path: String, reason: String },
    #[error("workspace_hook_failed hook={hook} status={status} output={output}")]
    HookFailed {
        hook: String,
        status: i32,
        output: String,
    },
    #[error("workspace_hook_timeout hook={hook} timeout_ms={timeout_ms}")]
    HookTimeout { hook: String, timeout_ms: u64 },
    #[error("workspace_io {0}")]
    Io(String),
}

impl Workspace {
    pub async fn create_for_issue(
        config: &ServiceConfig,
        issue: &Issue,
    ) -> Result<Self, WorkspaceError> {
        Self::create_for_issue_on_host(config, issue, None).await
    }

    pub async fn create_for_issue_on_host(
        config: &ServiceConfig,
        issue: &Issue,
        worker_host: Option<&str>,
    ) -> Result<Self, WorkspaceError> {
        Self::create_for_identifier_on_host(
            config,
            &issue.identifier,
            Some(issue),
            true,
            worker_host,
        )
        .await
    }

    pub async fn create_for_identifier(
        config: &ServiceConfig,
        identifier: &str,
        issue: Option<&Issue>,
        run_after_create_hook: bool,
    ) -> Result<Self, WorkspaceError> {
        Self::create_for_identifier_on_host(config, identifier, issue, run_after_create_hook, None)
            .await
    }

    pub async fn create_for_identifier_on_host(
        config: &ServiceConfig,
        identifier: &str,
        issue: Option<&Issue>,
        run_after_create_hook: bool,
        worker_host: Option<&str>,
    ) -> Result<Self, WorkspaceError> {
        let workspace_key = sanitize_identifier(identifier);
        let workspace_path = workspace_path_for_identifier_on_host(config, identifier, worker_host);
        validate_workspace_path_for_worker(config, &workspace_path, worker_host)?;

        let (created_now, resolved_workspace_path) =
            ensure_workspace(config, &workspace_path, worker_host).await?;
        let workspace_path = if worker_host.is_some() {
            workspace_path
        } else {
            resolved_workspace_path.unwrap_or(workspace_path)
        };
        if created_now
            && run_after_create_hook
            && let Some(script) = config.hooks.after_create.as_deref()
        {
            run_hook(
                script,
                "after_create",
                &workspace_path,
                &config.hooks,
                issue,
                identifier,
                worker_host,
            )
            .await?;
        }

        Ok(Self {
            path: workspace_path,
            workspace_key,
            created_now,
            worker_host: worker_host.map(ToOwned::to_owned),
        })
    }

    pub async fn run_before_run(
        &self,
        config: &ServiceConfig,
        issue: &Issue,
    ) -> Result<(), WorkspaceError> {
        if let Some(script) = config.hooks.before_run.as_deref() {
            run_hook(
                script,
                "before_run",
                &self.path,
                &config.hooks,
                Some(issue),
                &issue.identifier,
                self.worker_host.as_deref(),
            )
            .await?;
        }
        Ok(())
    }

    pub async fn run_after_run(&self, config: &ServiceConfig, issue: &Issue) {
        if let Some(script) = config.hooks.after_run.as_deref()
            && let Err(error) = run_hook(
                script,
                "after_run",
                &self.path,
                &config.hooks,
                Some(issue),
                &issue.identifier,
                self.worker_host.as_deref(),
            )
            .await
        {
            warn!(
                "workspace_hook=after_run status=ignored issue_id={} issue_identifier={} error={error}",
                issue.id, issue.identifier
            );
        }
    }

    pub async fn remove_for_identifier(
        config: &ServiceConfig,
        identifier: &str,
    ) -> Result<(), WorkspaceError> {
        Self::remove_for_identifier_on_host(config, identifier, None).await
    }

    pub async fn remove_for_identifier_on_host(
        config: &ServiceConfig,
        identifier: &str,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        let workspace_key = sanitize_identifier(identifier);
        let workspace_path =
            workspace_path_for_identifier_on_host(config, &workspace_key, worker_host);
        Self::remove_path_on_host(config, &workspace_path, identifier, worker_host).await
    }

    pub async fn remove_path(
        config: &ServiceConfig,
        path: &Path,
        identifier: &str,
    ) -> Result<(), WorkspaceError> {
        Self::remove_path_on_host(config, path, identifier, None).await
    }

    pub async fn remove_path_on_host(
        config: &ServiceConfig,
        path: &Path,
        identifier: &str,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        if worker_host.is_none() && !path.exists() {
            return Ok(());
        }

        validate_workspace_path_for_worker(config, path, worker_host)?;
        if workspace_exists(path, worker_host).await?
            && let Some(script) = config.hooks.before_remove.as_deref()
            && let Err(error) = run_hook(
                script,
                "before_remove",
                path,
                &config.hooks,
                None,
                identifier,
                worker_host,
            )
            .await
        {
            warn!(
                "workspace_hook=before_remove status=ignored issue_identifier={} error={error}",
                identifier
            );
        }

        if let Some(worker_host) = worker_host {
            remove_remote_workspace(worker_host, path, config.hooks.timeout_ms).await
        } else if path.is_dir() {
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))
        } else {
            tokio::fs::remove_file(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))
        }
    }

    pub async fn sweep_stale_workspaces(
        config: &ServiceConfig,
        live_workspace_keys: &BTreeSet<String>,
        worker_host: Option<&str>,
    ) -> Result<Vec<PathBuf>, WorkspaceError> {
        let candidates = list_workspace_paths(config, worker_host).await?;
        let mut removed = Vec::new();
        for (workspace_key, path) in candidates {
            if live_workspace_keys.contains(&workspace_key) {
                continue;
            }
            Self::remove_path_on_host(config, &path, &workspace_key, worker_host).await?;
            removed.push(path);
        }
        Ok(removed)
    }
}

pub fn sanitize_identifier(identifier: &str) -> String {
    identifier
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub fn workspace_path_for_identifier(config: &ServiceConfig, identifier: &str) -> PathBuf {
    workspace_path_for_identifier_on_host(config, identifier, None)
}

pub fn workspace_path_for_identifier_on_host(
    config: &ServiceConfig,
    identifier: &str,
    worker_host: Option<&str>,
) -> PathBuf {
    let workspace_key = sanitize_identifier(identifier);
    if worker_host.is_some() {
        PathBuf::from(join_remote_workspace_root(
            &config.workspace.root_raw,
            &workspace_key,
        ))
    } else {
        config.workspace.root.join(workspace_key)
    }
}

pub fn validate_workspace_path_for_worker(
    config: &ServiceConfig,
    workspace: &Path,
    worker_host: Option<&str>,
) -> Result<(), WorkspaceError> {
    if worker_host.is_some() {
        validate_remote_workspace_path(&config.workspace.root_raw, workspace)
    } else {
        validate_workspace_path(&config.workspace.root, workspace)
    }
}

pub fn validate_workspace_path(root: &Path, workspace: &Path) -> Result<(), WorkspaceError> {
    let expanded_root = normalize_absolute_path(root);
    let expanded_workspace = normalize_absolute_path(workspace);

    if expanded_workspace == expanded_root {
        return Err(WorkspaceError::EqualsRoot {
            workspace: expanded_workspace.display().to_string(),
            root: expanded_root.display().to_string(),
        });
    }

    if !expanded_workspace.starts_with(&expanded_root) {
        return Err(WorkspaceError::OutsideRoot {
            workspace: expanded_workspace.display().to_string(),
            root: expanded_root.display().to_string(),
        });
    }

    let relative = expanded_workspace
        .strip_prefix(&expanded_root)
        .map_err(|error| WorkspaceError::Io(error.to_string()))?;
    let mut current = expanded_root.clone();
    for segment in relative.components() {
        current.push(segment);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(WorkspaceError::SymlinkEscape {
                    path: current.display().to_string(),
                    root: expanded_root.display().to_string(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(WorkspaceError::PathUnreadable {
                    path: current.display().to_string(),
                    reason: error.to_string(),
                });
            }
        }
    }

    Ok(())
}

async fn ensure_workspace(
    config: &ServiceConfig,
    path: &Path,
    worker_host: Option<&str>,
) -> Result<(bool, Option<PathBuf>), WorkspaceError> {
    if let Some(worker_host) = worker_host {
        return ensure_remote_workspace(worker_host, path, config.hooks.timeout_ms).await;
    }

    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => {
            clean_tmp_artifacts(path).await?;
            Ok((false, None))
        }
        Ok(_) => {
            tokio::fs::remove_file(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            tokio::fs::create_dir_all(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            Ok((true, None))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            Ok((true, None))
        }
        Err(error) => Err(WorkspaceError::Io(error.to_string())),
    }
}

async fn clean_tmp_artifacts(path: &Path) -> Result<(), WorkspaceError> {
    for entry in TMP_ARTIFACTS {
        let candidate = path.join(entry);
        if tokio::fs::try_exists(&candidate)
            .await
            .map_err(|error| WorkspaceError::Io(error.to_string()))?
        {
            if candidate.is_dir() {
                tokio::fs::remove_dir_all(&candidate)
                    .await
                    .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            } else {
                tokio::fs::remove_file(&candidate)
                    .await
                    .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            }
        }
    }
    Ok(())
}

async fn run_hook(
    script: &str,
    hook_name: &str,
    workspace: &Path,
    hooks: &HookConfig,
    issue: Option<&Issue>,
    identifier: &str,
    worker_host: Option<&str>,
) -> Result<(), WorkspaceError> {
    let timeout_ms = hooks.timeout_ms;
    let issue_context = issue
        .map(|issue| {
            format!(
                "issue_id={} issue_identifier={}",
                issue.id, issue.identifier
            )
        })
        .unwrap_or_else(|| format!("issue_identifier={identifier}"));
    info!(
        "workspace_hook={} status=starting {} workspace={}",
        hook_name,
        issue_context,
        workspace.display()
    );

    let output = if let Some(worker_host) = worker_host {
        run_remote_command(
            worker_host,
            &format!(
                "{}\ncd \"$workspace\" && {}",
                remote_shell_assign("workspace", &workspace.to_string_lossy()),
                script
            ),
            timeout_ms,
            hook_name,
        )
        .await?
    } else {
        let shell = shell_binary();
        let mut command = Command::new(shell);
        command
            .arg("-lc")
            .arg(script)
            .current_dir(workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        timeout(Duration::from_millis(timeout_ms), command.output())
            .await
            .map_err(|_| WorkspaceError::HookTimeout {
                hook: hook_name.to_string(),
                timeout_ms,
            })?
            .map_err(|error| WorkspaceError::Io(error.to_string()))?
    };

    if output.status.success() {
        return Ok(());
    }

    let combined = truncate_output([output.stdout, output.stderr].concat());
    Err(WorkspaceError::HookFailed {
        hook: hook_name.to_string(),
        status: output.status.code().unwrap_or(-1),
        output: combined,
    })
}

fn truncate_output(output: Vec<u8>) -> String {
    let text = String::from_utf8_lossy(&output).replace(char::is_control, " ");
    if text.len() > MAX_HOOK_LOG_BYTES {
        format!("{}...<truncated>", &text[..MAX_HOOK_LOG_BYTES])
    } else {
        text
    }
}

fn shell_binary() -> &'static str {
    if Path::new("/bin/bash").exists() {
        "/bin/bash"
    } else {
        "/bin/sh"
    }
}

fn validate_remote_workspace_path(root_raw: &str, workspace: &Path) -> Result<(), WorkspaceError> {
    let root = normalize_remote_path(root_raw);
    let workspace = normalize_remote_path(&workspace.to_string_lossy());

    if workspace.trim().is_empty() {
        return Err(WorkspaceError::PathUnreadable {
            path: workspace,
            reason: "empty".to_string(),
        });
    }

    if workspace.contains(['\n', '\r', '\0']) {
        return Err(WorkspaceError::PathUnreadable {
            path: workspace,
            reason: "invalid_characters".to_string(),
        });
    }

    if root.is_empty() {
        return Err(WorkspaceError::PathUnreadable {
            path: workspace,
            reason: "empty_root".to_string(),
        });
    }

    if workspace == root {
        return Err(WorkspaceError::EqualsRoot { workspace, root });
    }

    let root_prefix = format!("{root}/");
    let workspace_prefix = format!("{workspace}/");
    if !workspace_prefix.starts_with(&root_prefix) {
        return Err(WorkspaceError::OutsideRoot { workspace, root });
    }

    Ok(())
}

fn normalize_remote_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "/" {
        "/".to_string()
    } else {
        trimmed.trim_end_matches('/').to_string()
    }
}

fn join_remote_workspace_root(root: &str, workspace_key: &str) -> String {
    let root = normalize_remote_path(root);
    if root.is_empty() {
        workspace_key.to_string()
    } else if root == "/" {
        format!("/{workspace_key}")
    } else {
        format!("{root}/{workspace_key}")
    }
}

async fn workspace_exists(path: &Path, worker_host: Option<&str>) -> Result<bool, WorkspaceError> {
    if let Some(worker_host) = worker_host {
        let output = run_remote_command(
            worker_host,
            &format!(
                "{}\nif [ -e \"$workspace\" ]; then printf '1'; else printf '0'; fi",
                remote_shell_assign("workspace", &path.to_string_lossy())
            ),
            5_000,
            "workspace_exists",
        )
        .await?;
        Ok(String::from_utf8_lossy(&output.stdout).trim() == "1")
    } else {
        tokio::fs::try_exists(path)
            .await
            .map_err(|error| WorkspaceError::Io(error.to_string()))
    }
}

async fn ensure_remote_workspace(
    worker_host: &str,
    path: &Path,
    timeout_ms: u64,
) -> Result<(bool, Option<PathBuf>), WorkspaceError> {
    let mut script_lines = vec![
        "set -eu".to_string(),
        remote_shell_assign("workspace", &path.to_string_lossy()),
        "if [ -d \"$workspace\" ]; then".to_string(),
        "  created=0".to_string(),
    ];
    for artifact in TMP_ARTIFACTS {
        script_lines.push(format!("  rm -rf \"$workspace/{artifact}\""));
    }
    script_lines.extend([
        "elif [ -e \"$workspace\" ]; then".to_string(),
        "  rm -rf \"$workspace\"".to_string(),
        "  mkdir -p \"$workspace\"".to_string(),
        "  created=1".to_string(),
        "else".to_string(),
        "  mkdir -p \"$workspace\"".to_string(),
        "  created=1".to_string(),
        "fi".to_string(),
        "cd \"$workspace\"".to_string(),
        format!(
            "printf '%s\\t%s\\t%s\\n' '{}' \"$created\" \"$(pwd -P)\"",
            REMOTE_WORKSPACE_MARKER
        ),
    ]);

    let output = run_remote_command(
        worker_host,
        &script_lines.join("\n"),
        timeout_ms,
        "workspace_prepare",
    )
    .await?;
    if !output.status.success() {
        return Err(WorkspaceError::HookFailed {
            hook: "workspace_prepare".to_string(),
            status: output.status.code().unwrap_or(-1),
            output: truncate_output([output.stdout, output.stderr].concat()),
        });
    }
    parse_remote_workspace_output(&output)
}

async fn remove_remote_workspace(
    worker_host: &str,
    path: &Path,
    timeout_ms: u64,
) -> Result<(), WorkspaceError> {
    let script = format!(
        "{}\nrm -rf \"$workspace\"",
        remote_shell_assign("workspace", &path.to_string_lossy())
    );
    let output = run_remote_command(worker_host, &script, timeout_ms, "before_remove").await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorkspaceError::HookFailed {
            hook: "before_remove".to_string(),
            status: output.status.code().unwrap_or(-1),
            output: truncate_output([output.stdout, output.stderr].concat()),
        })
    }
}

async fn list_workspace_paths(
    config: &ServiceConfig,
    worker_host: Option<&str>,
) -> Result<Vec<(String, PathBuf)>, WorkspaceError> {
    if let Some(worker_host) = worker_host {
        let script = format!(
            "{}\nif [ -d \"$workspace_root\" ]; then\n  cd \"$workspace_root\"\n  root_path=\"$(pwd -P)\"\n  for entry in * .*; do\n    case \"$entry\" in\n      .|..) continue ;;\n    esac\n    [ -d \"$entry\" ] || continue\n    printf '%s\\t%s\\t%s\\n' '{}' \"$entry\" \"$root_path/$entry\"\n  done\nfi",
            remote_shell_assign("workspace_root", &config.workspace.root_raw),
            REMOTE_LIST_MARKER
        );
        let output = run_remote_command(
            worker_host,
            &script,
            config.hooks.timeout_ms,
            "workspace_list",
        )
        .await?;
        if !output.status.success() {
            return Err(WorkspaceError::HookFailed {
                hook: "workspace_list".to_string(),
                status: output.status.code().unwrap_or(-1),
                output: truncate_output([output.stdout, output.stderr].concat()),
            });
        }
        return Ok(parse_remote_workspace_list(&output)?
            .into_iter()
            .map(|(workspace_key, _)| {
                (
                    workspace_key.clone(),
                    workspace_path_for_identifier_on_host(config, &workspace_key, Some(worker_host)),
                )
            })
            .collect());
    }

    let mut entries = match tokio::fs::read_dir(&config.workspace.root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(WorkspaceError::Io(error.to_string())),
    };
    let mut results = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|error| WorkspaceError::Io(error.to_string()))?
    {
        let file_type = entry
            .file_type()
            .await
            .map_err(|error| WorkspaceError::Io(error.to_string()))?;
        if !file_type.is_dir() {
            continue;
        }
        let workspace_key = entry.file_name().to_string_lossy().to_string();
        results.push((workspace_key, entry.path()));
    }
    Ok(results)
}

fn parse_remote_workspace_output(
    output: &Output,
) -> Result<(bool, Option<PathBuf>), WorkspaceError> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() == 3 && parts[0] == REMOTE_WORKSPACE_MARKER {
            return Ok((parts[1] == "1", Some(PathBuf::from(parts[2]))));
        }
    }
    Err(WorkspaceError::Io(format!(
        "remote workspace preparation returned invalid output: {}",
        stdout.trim()
    )))
}

fn parse_remote_workspace_list(output: &Output) -> Result<Vec<(String, PathBuf)>, WorkspaceError> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() == 3 && parts[0] == REMOTE_LIST_MARKER {
            results.push((parts[1].to_string(), PathBuf::from(parts[2])));
        }
    }
    Ok(results)
}

pub(crate) fn remote_shell_assign(variable_name: &str, raw_path: &str) -> String {
    [
        format!("{variable_name}={}", shell_escape(raw_path)),
        format!("case \"${}\" in", variable_name),
        format!("  '~') {variable_name}=\"$HOME\" ;;"),
        format!(
            "  '~/'*) {variable_name}=\"$HOME/${{{}#\\~/}}\" ;;",
            variable_name
        ),
        "esac".to_string(),
    ]
    .join("\n")
}

pub(crate) fn remote_shell_command(command: &str) -> String {
    format!("bash -lc {}", shell_escape(command))
}

pub(crate) fn ssh_args(worker_host: &str, command: &str) -> Vec<String> {
    let (destination, port) = parse_worker_host(worker_host);
    let mut args = Vec::new();
    if let Some(config_path) = std::env::var("SYMPHONY_SSH_CONFIG")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        args.push("-F".to_string());
        args.push(config_path);
    }
    args.push("-T".to_string());
    if let Some(port) = port {
        args.push("-p".to_string());
        args.push(port);
    }
    args.push(destination);
    args.push(remote_shell_command(command));
    args
}

pub(crate) fn ssh_executable() -> Result<String, WorkspaceError> {
    if let Some(explicit) = std::env::var("SYMPHONY_SSH_BIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(explicit);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for candidate_dir in std::env::split_paths(&path) {
            let candidate = candidate_dir.join("ssh");
            if candidate.is_file() {
                return Ok(candidate.display().to_string());
            }
        }
    }
    Err(WorkspaceError::Io("ssh executable not found".to_string()))
}

async fn run_remote_command(
    worker_host: &str,
    script: &str,
    timeout_ms: u64,
    hook_name: &str,
) -> Result<Output, WorkspaceError> {
    let executable = ssh_executable()?;
    let mut command = Command::new(executable);
    command
        .args(ssh_args(worker_host, script))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    timeout(Duration::from_millis(timeout_ms), command.output())
        .await
        .map_err(|_| WorkspaceError::HookTimeout {
            hook: hook_name.to_string(),
            timeout_ms,
        })?
        .map_err(|error| WorkspaceError::Io(error.to_string()))
}

fn parse_worker_host(worker_host: &str) -> (String, Option<String>) {
    let trimmed = worker_host.trim();
    if let Some((destination, port)) = trimmed.rsplit_once(':')
        && !destination.is_empty()
        && port.chars().all(|ch| ch.is_ascii_digit())
        && (!destination.contains(':') || (destination.contains('[') && destination.contains(']')))
    {
        return (destination.to_string(), Some(port.to_string()));
    }
    (trimmed.to_string(), None)
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn normalize_absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }

    normalized
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        Workspace, WorkspaceError, sanitize_identifier, validate_workspace_path,
        workspace_path_for_identifier, workspace_path_for_identifier_on_host,
    };
    use crate::{config::ServiceConfig, issue::Issue};

    #[tokio::test]
    async fn reuses_existing_workspace_without_deleting_local_changes() {
        let dir = tempdir().expect("tempdir");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT/1".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let first = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");
        tokio::fs::write(first.path.join("README.md"), "changed\n")
            .await
            .expect("write");
        tokio::fs::create_dir_all(first.path.join("tmp"))
            .await
            .expect("mkdir");
        tokio::fs::write(first.path.join("tmp").join("scratch.txt"), "remove me")
            .await
            .expect("write scratch");

        let second = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");

        assert_eq!(first.path, second.path);
        assert_eq!(
            tokio::fs::read_to_string(second.path.join("README.md"))
                .await
                .expect("read"),
            "changed\n"
        );
        assert!(!second.path.join("tmp").join("scratch.txt").exists());
    }

    #[tokio::test]
    async fn workspace_path_is_deterministic_per_issue_identifier() {
        let dir = tempdir().expect("tempdir");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT/Det".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let first = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");
        let second = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");

        assert_eq!(first.path, second.path);
        assert_eq!(
            first.path.file_name().and_then(|name| name.to_str()),
            Some("MT_Det")
        );
    }

    #[tokio::test]
    async fn creates_empty_directory_when_no_bootstrap_hook_is_configured() {
        let dir = tempdir().expect("tempdir");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-EMPTY".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let workspace = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");
        let mut entries = tokio::fs::read_dir(&workspace.path)
            .await
            .expect("read dir");

        assert!(entries.next_entry().await.expect("entry").is_none());
    }

    #[tokio::test]
    async fn surfaces_after_create_hook_failures() {
        let dir = tempdir().expect("tempdir");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() },
                "hooks": { "after_create": "echo nope && exit 17" }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-FAIL".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let error = Workspace::create_for_issue(&config, &issue)
            .await
            .unwrap_err();

        match error {
            WorkspaceError::HookFailed {
                hook,
                status,
                output,
            } => {
                assert_eq!(hook, "after_create");
                assert_eq!(status, 17);
                assert!(output.contains("nope"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn surfaces_after_create_hook_timeouts() {
        let dir = tempdir().expect("tempdir");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() },
                "hooks": {
                    "timeout_ms": 10,
                    "after_create": "sleep 1"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-TIMEOUT".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let error = Workspace::create_for_issue(&config, &issue)
            .await
            .unwrap_err();

        match error {
            WorkspaceError::HookTimeout { hook, timeout_ms } => {
                assert_eq!(hook, "after_create");
                assert_eq!(timeout_ms, 10);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn surfaces_before_run_hook_failures() {
        let dir = tempdir().expect("tempdir");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() },
                "hooks": { "before_run": "echo nope && exit 23" }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-BEFORE-RUN-FAIL".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let workspace = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");
        let error = workspace.run_before_run(&config, &issue).await.unwrap_err();

        match error {
            WorkspaceError::HookFailed {
                hook,
                status,
                output,
            } => {
                assert_eq!(hook, "before_run");
                assert_eq!(status, 23);
                assert!(output.contains("nope"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn surfaces_before_run_hook_timeouts() {
        let dir = tempdir().expect("tempdir");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() },
                "hooks": {
                    "timeout_ms": 10,
                    "before_run": "sleep 1"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-BEFORE-RUN-TIMEOUT".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let workspace = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");
        let error = workspace.run_before_run(&config, &issue).await.unwrap_err();

        match error {
            WorkspaceError::HookTimeout { hook, timeout_ms } => {
                assert_eq!(hook, "before_run");
                assert_eq!(timeout_ms, 10);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn after_run_hook_failures_are_ignored() {
        let dir = tempdir().expect("tempdir");
        let marker = dir.path().join("after-run.marker");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() },
                "hooks": {
                    "after_run": format!("printf 'ran' > '{}' && exit 19", marker.display())
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-AFTER-RUN-FAIL".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let workspace = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");
        workspace.run_after_run(&config, &issue).await;

        assert_eq!(
            tokio::fs::read_to_string(&marker).await.expect("marker"),
            "ran"
        );
        assert!(workspace.path.exists());
    }

    #[tokio::test]
    async fn after_run_hook_timeouts_are_ignored() {
        let dir = tempdir().expect("tempdir");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() },
                "hooks": {
                    "timeout_ms": 10,
                    "after_run": "sleep 1"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-AFTER-RUN-TIMEOUT".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let workspace = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");
        workspace.run_after_run(&config, &issue).await;

        assert!(workspace.path.exists());
    }

    #[tokio::test]
    async fn before_remove_hook_failures_do_not_block_workspace_removal() {
        let dir = tempdir().expect("tempdir");
        let marker = dir.path().join("before-remove.marker");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": dir.path() },
                "hooks": {
                    "before_remove": format!("printf 'ran' > '{}' && exit 17", marker.display())
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-BEFORE-REMOVE".to_string(),
            title: "Title".to_string(),
            state: "Done".to_string(),
            ..Issue::default()
        };

        let workspace = Workspace::create_for_issue(&config, &issue)
            .await
            .expect("workspace");
        tokio::fs::write(workspace.path.join("README.md"), "present")
            .await
            .expect("write");

        Workspace::remove_for_identifier(&config, &issue.identifier)
            .await
            .expect("remove");

        assert_eq!(
            tokio::fs::read_to_string(&marker).await.expect("marker"),
            "ran"
        );
        assert!(!workspace.path.exists());
    }

    #[tokio::test]
    async fn rejects_symlink_escape() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("workspaces");
        let outside = dir.path().join("outside");
        tokio::fs::create_dir_all(&root).await.expect("root");
        tokio::fs::create_dir_all(&outside).await.expect("outside");
        std::os::unix::fs::symlink(&outside, root.join("MT-SYM")).expect("symlink");

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": root }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT-SYM".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let error = Workspace::create_for_issue(&config, &issue)
            .await
            .unwrap_err();
        assert!(matches!(error, WorkspaceError::SymlinkEscape { .. }));
    }

    #[tokio::test]
    async fn removes_existing_workspace_file_path() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("workspaces");
        tokio::fs::create_dir_all(&root).await.expect("root");

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": root }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let path = config.workspace.root.join("MT-1");
        tokio::fs::write(&path, "stale file").await.expect("write");
        Workspace::remove_path(&config, &path, "MT-1")
            .await
            .expect("remove path");

        assert!(!path.exists());
    }

    #[test]
    fn rejects_parent_dir_escape() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("workspaces");
        let escape = root.join("..").join("outside");

        let error = validate_workspace_path(&root, &escape).unwrap_err();
        assert!(matches!(error, WorkspaceError::OutsideRoot { .. }));
    }

    #[test]
    fn sanitizes_identifiers_and_paths() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": "workspaces" }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(sanitize_identifier("MT/123"), "MT_123");
        assert_eq!(
            workspace_path_for_identifier(&config, "MT/123"),
            config.workspace.root.join("MT_123")
        );
    }

    #[tokio::test]
    async fn creates_remote_workspace_with_tilde_root_and_runs_remote_hooks() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        fs::create_dir_all(&home).expect("home");
        let ssh = dir.path().join("ssh");
        write_executable(
            &ssh,
            r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  case "$1" in
    -F|-p)
      shift 2
      ;;
    -T)
      shift
      ;;
    *)
      host="$1"
      shift
      break
      ;;
  esac
done
command="$1"
exec /bin/sh -lc "$command"
"#,
        );
        unsafe {
            std::env::set_var("SYMPHONY_SSH_BIN", &ssh);
            std::env::set_var("HOME", &home);
        }

        let marker = home.join("remote-hook.txt");
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": "~/remote-workspaces" },
                "hooks": {
                    "after_create": format!("printf 'created' > '{}'", marker.display())
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");
        let issue = Issue {
            id: "1".to_string(),
            identifier: "MT/REMOTE".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let workspace = Workspace::create_for_issue_on_host(&config, &issue, Some("builder-1"))
            .await
            .expect("workspace");

        assert_eq!(workspace.worker_host.as_deref(), Some("builder-1"));
        assert_eq!(
            workspace.path,
            Path::new("~/remote-workspaces").join("MT_REMOTE")
        );
        assert_eq!(fs::read_to_string(marker).expect("marker"), "created");

        unsafe {
            std::env::remove_var("SYMPHONY_SSH_BIN");
            std::env::remove_var("HOME");
        }
    }

    #[tokio::test]
    async fn sweeps_remote_stale_workspaces() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let remote_root = home.join("remote-workspaces");
        fs::create_dir_all(remote_root.join("MT_LIVE")).expect("live");
        fs::create_dir_all(remote_root.join("MT_STALE")).expect("stale");
        let ssh = dir.path().join("ssh");
        write_executable(
            &ssh,
            r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  case "$1" in
    -F|-p)
      shift 2
      ;;
    -T)
      shift
      ;;
    *)
      shift
      break
      ;;
  esac
done
exec /bin/sh -lc "$1"
"#,
        );
        unsafe {
            std::env::set_var("SYMPHONY_SSH_BIN", &ssh);
            std::env::set_var("HOME", &home);
        }

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": "~/remote-workspaces" }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let removed = Workspace::sweep_stale_workspaces(
            &config,
            &["MT_LIVE".to_string()].into_iter().collect(),
            Some("builder-1"),
        )
        .await
        .expect("sweep");

        assert_eq!(removed.len(), 1);
        assert!(!remote_root.join("MT_STALE").exists());
        assert!(remote_root.join("MT_LIVE").exists());

        unsafe {
            std::env::remove_var("SYMPHONY_SSH_BIN");
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn remote_workspace_paths_preserve_raw_root() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": { "root": "~/remote-workspaces" }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        assert_eq!(
            workspace_path_for_identifier_on_host(&config, "MT/123", Some("builder-1")),
            Path::new("~/remote-workspaces").join("MT_123")
        );
    }

    fn write_executable(path: &Path, script: &str) {
        fs::write(path, script).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("chmod");
        }
    }
}
