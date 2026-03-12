use std::{
    path::{Component, Path, PathBuf},
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

#[derive(Clone, Debug)]
pub struct Workspace {
    pub path: PathBuf,
    pub workspace_key: String,
    pub created_now: bool,
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
        Self::create_for_identifier(config, &issue.identifier, Some(issue), true).await
    }

    pub async fn create_for_identifier(
        config: &ServiceConfig,
        identifier: &str,
        issue: Option<&Issue>,
        run_after_create_hook: bool,
    ) -> Result<Self, WorkspaceError> {
        let workspace_key = sanitize_identifier(identifier);
        let workspace_path = config.workspace.root.join(&workspace_key);
        validate_workspace_path(&config.workspace.root, &workspace_path)?;

        let created_now = ensure_workspace(&workspace_path).await?;
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
            )
            .await?;
        }

        Ok(Self {
            path: workspace_path,
            workspace_key,
            created_now,
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
        let workspace_key = sanitize_identifier(identifier);
        let workspace_path = config.workspace.root.join(workspace_key);
        Self::remove_path(config, &workspace_path, identifier).await
    }

    pub async fn remove_path(
        config: &ServiceConfig,
        path: &Path,
        identifier: &str,
    ) -> Result<(), WorkspaceError> {
        if !path.exists() {
            return Ok(());
        }

        validate_workspace_path(&config.workspace.root, path)?;
        if path.is_dir()
            && let Some(script) = config.hooks.before_remove.as_deref()
            && let Err(error) = run_hook(
                script,
                "before_remove",
                path,
                &config.hooks,
                None,
                identifier,
            )
            .await
        {
            warn!(
                "workspace_hook=before_remove status=ignored issue_identifier={} error={error}",
                identifier
            );
        }

        if path.is_dir() {
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))
        } else {
            tokio::fs::remove_file(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))
        }
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
    config.workspace.root.join(sanitize_identifier(identifier))
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

async fn ensure_workspace(path: &Path) -> Result<bool, WorkspaceError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => {
            clean_tmp_artifacts(path).await?;
            Ok(false)
        }
        Ok(_) => {
            tokio::fs::remove_file(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            tokio::fs::create_dir_all(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            Ok(true)
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

    let shell = shell_binary();
    let mut command = Command::new(shell);
    command
        .arg("-lc")
        .arg(script)
        .current_dir(workspace)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let output = timeout(Duration::from_millis(timeout_ms), command.output())
        .await
        .map_err(|_| WorkspaceError::HookTimeout {
            hook: hook_name.to_string(),
            timeout_ms,
        })?
        .map_err(|error| WorkspaceError::Io(error.to_string()))?;

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
    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        Workspace, WorkspaceError, sanitize_identifier, validate_workspace_path,
        workspace_path_for_identifier,
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
}
