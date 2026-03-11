use std::{
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::SystemTime,
};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{error, info, warn};

use crate::config::{ConfigError, ServiceConfig};

#[derive(Clone, Debug)]
pub struct WorkflowDefinition {
    pub config: Map<String, Value>,
    pub prompt_template: String,
}

#[derive(Clone, Debug)]
pub struct LoadedWorkflow {
    pub definition: WorkflowDefinition,
    pub config: ServiceConfig,
}

#[derive(Clone, Debug, Default)]
struct WorkflowState {
    last_good: Option<LoadedWorkflow>,
    validation_error: Option<String>,
    fingerprint: Option<WorkflowFingerprint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WorkflowFingerprint {
    modified: Option<SystemTime>,
    len: u64,
    content_hash: u64,
}

#[derive(Clone)]
pub struct WorkflowStore {
    path: PathBuf,
    state: Arc<RwLock<WorkflowState>>,
}

pub struct WorkflowWatcher {
    pub join: JoinHandle<()>,
}

#[derive(Debug, Error, Clone)]
pub enum WorkflowError {
    #[error("missing_workflow_file path={path} reason={reason}")]
    MissingWorkflowFile { path: PathBuf, reason: String },
    #[error("workflow_front_matter_not_a_map")]
    FrontMatterNotMap,
    #[error("workflow_parse_error {0}")]
    Parse(String),
    #[error("{0}")]
    InvalidConfig(String),
}

impl From<ConfigError> for WorkflowError {
    fn from(value: ConfigError) -> Self {
        Self::InvalidConfig(value.to_string())
    }
}

impl WorkflowStore {
    pub fn new(path: PathBuf) -> Result<Self, WorkflowError> {
        let loaded = Self::load_path(&path)?;
        let fingerprint = fingerprint_for(&path)?;
        let state = WorkflowState {
            last_good: Some(loaded),
            validation_error: None,
            fingerprint: Some(fingerprint),
        };
        Ok(Self {
            path,
            state: Arc::new(RwLock::new(state)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn effective(&self) -> LoadedWorkflow {
        self.state
            .read()
            .expect("workflow store poisoned")
            .last_good
            .clone()
            .expect("workflow store always has a last known good workflow")
    }

    pub fn validation_error(&self) -> Option<String> {
        self.state
            .read()
            .expect("workflow store poisoned")
            .validation_error
            .clone()
    }

    pub fn refresh_if_changed(&self) {
        match fingerprint_for(&self.path) {
            Ok(next) => {
                let current = self
                    .state
                    .read()
                    .expect("workflow store poisoned")
                    .fingerprint;
                if current == Some(next) {
                    return;
                }
                self.reload();
            }
            Err(error) => self.update_error(error),
        }
    }

    pub fn reload(&self) {
        match Self::load_path(&self.path) {
            Ok(loaded) => match fingerprint_for(&self.path) {
                Ok(fingerprint) => {
                    let mut state = self.state.write().expect("workflow store poisoned");
                    state.last_good = Some(loaded);
                    state.validation_error = None;
                    state.fingerprint = Some(fingerprint);
                    info!("workflow=reload status=ok path={}", self.path.display());
                }
                Err(error) => self.update_error(error),
            },
            Err(error) => self.update_error(error),
        }
    }

    pub fn start_watcher(&self) -> WorkflowWatcher {
        let path = self.path.clone();
        let store = self.clone();
        let join = tokio::spawn(async move {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut watcher = match watcher_for(path.clone(), tx) {
                Ok(watcher) => watcher,
                Err(error) => {
                    error!(
                        "workflow=watch status=failed path={} error={error}",
                        path.display()
                    );
                    return;
                }
            };

            if let Err(error) = watcher.watch(&path, RecursiveMode::NonRecursive) {
                error!(
                    "workflow=watch status=failed path={} error={error}",
                    path.display()
                );
                return;
            }

            while rx.recv().await.is_some() {
                store.reload();
            }
        });
        WorkflowWatcher { join }
    }

    fn update_error(&self, error: WorkflowError) {
        let message = error.to_string();
        warn!(
            "workflow=reload status=invalid path={} error={message}",
            self.path.display()
        );
        let mut state = self.state.write().expect("workflow store poisoned");
        state.validation_error = Some(message);
    }

    pub fn load_path(path: &Path) -> Result<LoadedWorkflow, WorkflowError> {
        let raw = fs::read_to_string(path).map_err(|error| WorkflowError::MissingWorkflowFile {
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
        let definition = parse_workflow(&raw)?;
        let config = ServiceConfig::from_map(&definition.config)?;
        Ok(LoadedWorkflow { definition, config })
    }
}

fn watcher_for(path: PathBuf, tx: mpsc::UnboundedSender<()>) -> notify::Result<RecommendedWatcher> {
    notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
        Ok(event) => {
            if is_relevant_event(&event.kind)
                && event.paths.iter().any(|candidate| candidate == &path)
            {
                let _ = tx.send(());
            }
        }
        Err(error) => {
            warn!(
                "workflow=watch status=event_error path={} error={error}",
                path.display()
            );
        }
    })
}

fn is_relevant_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    )
}

fn fingerprint_for(path: &Path) -> Result<WorkflowFingerprint, WorkflowError> {
    let metadata = fs::metadata(path).map_err(|error| WorkflowError::MissingWorkflowFile {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let content = fs::read(path).map_err(|error| WorkflowError::MissingWorkflowFile {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    Ok(WorkflowFingerprint {
        modified: metadata.modified().ok(),
        len: metadata.len(),
        content_hash: hasher.finish(),
    })
}

fn parse_workflow(raw: &str) -> Result<WorkflowDefinition, WorkflowError> {
    let lines: Vec<&str> = raw.lines().collect();
    let (front_matter, prompt_lines) = if lines.first().copied() == Some("---") {
        let rest = &lines[1..];
        let end = rest.iter().position(|line| *line == "---");
        match end {
            Some(index) => (&rest[..index], &rest[index + 1..]),
            None => (rest, &[][..]),
        }
    } else {
        (&[][..], lines.as_slice())
    };

    let yaml = front_matter.join("\n");
    let config = if yaml.trim().is_empty() {
        Map::new()
    } else {
        let value: serde_yaml::Value =
            serde_yaml::from_str(&yaml).map_err(|error| WorkflowError::Parse(error.to_string()))?;
        if !matches!(value, serde_yaml::Value::Mapping(_)) {
            return Err(WorkflowError::FrontMatterNotMap);
        }
        match serde_json::to_value(value)
            .map_err(|error| WorkflowError::Parse(error.to_string()))?
        {
            Value::Object(map) => map,
            _ => return Err(WorkflowError::FrontMatterNotMap),
        }
    };

    let prompt_template = prompt_lines.join("\n").trim().to_string();
    Ok(WorkflowDefinition {
        config,
        prompt_template,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        sync::{Mutex, OnceLock},
    };

    use serde_json::Value;
    use tempfile::tempdir;

    use super::{WorkflowError, fingerprint_for, parse_workflow};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn parses_prompt_only_workflows() {
        let parsed = parse_workflow("Prompt only\n").expect("prompt-only workflow should parse");
        assert!(parsed.config.is_empty());
        assert_eq!(parsed.prompt_template, "Prompt only");
    }

    #[test]
    fn parses_unterminated_front_matter() {
        let parsed = parse_workflow("---\ntracker:\n  kind: linear\n")
            .expect("unterminated front matter should parse");
        assert_eq!(parsed.prompt_template, "");
        assert_eq!(
            parsed
                .config
                .get("tracker")
                .and_then(|value| value.get("kind"))
                .and_then(Value::as_str),
            Some("linear")
        );
    }

    #[test]
    fn rejects_non_map_front_matter() {
        let error = parse_workflow("---\n- nope\n---\nPrompt\n").unwrap_err();
        assert!(matches!(error, WorkflowError::FrontMatterNotMap));
    }

    #[test]
    fn store_keeps_last_good_when_reload_is_invalid() {
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        fs::write(
            &workflow_path,
            "---\ntracker:\n  kind: linear\n  api_key: token\n  project_slug: proj\n---\nhi",
        )
        .expect("write workflow");

        let store = super::WorkflowStore::new(workflow_path.clone()).expect("workflow store");
        assert!(store.validation_error().is_none());

        fs::write(&workflow_path, "---\n- nope\n---\n").expect("write invalid workflow");
        store.reload();

        assert!(store.validation_error().is_some());
        assert_eq!(store.effective().definition.prompt_template, "hi");
    }

    #[test]
    fn fingerprint_changes_for_same_size_content_edits() {
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        fs::write(&workflow_path, "abc").expect("write workflow");
        let first = fingerprint_for(&workflow_path).expect("fingerprint");

        fs::write(&workflow_path, "xyz").expect("rewrite workflow");
        let second = fingerprint_for(&workflow_path).expect("fingerprint");

        assert_ne!(first.content_hash, second.content_hash);
        assert_eq!(first.len, second.len);
    }

    #[test]
    fn bundled_workflows_load_successfully() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for relative_path in [
            "WORKFLOW.md",
            "WORKFLOW.local.md",
            "WORKFLOW.smoke.full.md",
            "WORKFLOW.smoke.minimal.md",
        ] {
            let workflow_path = manifest_dir.join(relative_path);
            let loaded = super::WorkflowStore::load_path(&workflow_path).unwrap_or_else(|error| {
                panic!("failed to load {}: {error}", workflow_path.display())
            });
            assert!(
                !loaded.definition.prompt_template.is_empty(),
                "expected non-empty prompt in {}",
                workflow_path.display()
            );
        }
    }

    #[test]
    fn bundled_workflows_are_dispatch_ready_with_test_env() {
        let _guard = env_lock().lock().expect("env lock");
        let workspace_root = tempdir().expect("tempdir");
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

        unsafe {
            env::set_var("LINEAR_API_KEY", "token");
            env::set_var("SYMPHONY_WORKSPACE_ROOT", workspace_root.path());
            env::set_var("SYMPHONY_SMOKE_PROJECT_SLUG", "smoke-proj");
        }

        for relative_path in [
            "WORKFLOW.md",
            "WORKFLOW.local.md",
            "WORKFLOW.smoke.full.md",
            "WORKFLOW.smoke.minimal.md",
        ] {
            let workflow_path = manifest_dir.join(relative_path);
            let loaded = super::WorkflowStore::load_path(&workflow_path).unwrap_or_else(|error| {
                panic!("failed to load {}: {error}", workflow_path.display())
            });
            loaded
                .config
                .validate_dispatch_ready()
                .unwrap_or_else(|error| {
                    panic!(
                        "{} should be dispatch-ready: {error}",
                        workflow_path.display()
                    )
                });
        }

        unsafe {
            env::remove_var("LINEAR_API_KEY");
            env::remove_var("SYMPHONY_WORKSPACE_ROOT");
            env::remove_var("SYMPHONY_SMOKE_PROJECT_SLUG");
        }
    }
}
