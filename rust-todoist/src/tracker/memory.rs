use std::{fs, path::Path};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    config::ServiceConfig,
    issue::{Issue, normalize_state_name},
    tracker::{TrackerClient, TrackerError},
};

#[derive(Clone)]
pub struct MemoryTracker {
    config: ServiceConfig,
}

impl MemoryTracker {
    pub fn new(config: ServiceConfig) -> Self {
        Self { config }
    }

    fn fixture_path(&self) -> Result<&Path, TrackerError> {
        self.config
            .tracker
            .fixture_path
            .as_deref()
            .ok_or(TrackerError::MissingTrackerFixturePath)
    }

    fn load_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let path = self.fixture_path()?;
        let raw = fs::read_to_string(path).map_err(|error| TrackerError::MemoryFixtureIo {
            path: path.display().to_string(),
            error: error.to_string(),
        })?;

        parse_fixture(path, &raw)
    }
}

#[async_trait]
impl TrackerClient for MemoryTracker {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let active_states: std::collections::BTreeSet<String> = self
            .config
            .tracker
            .active_states
            .iter()
            .map(|state| normalize_state_name(state))
            .collect();

        Ok(self
            .load_issues()?
            .into_iter()
            .filter(|issue| active_states.contains(&issue.state_key()))
            .collect())
    }

    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: std::collections::BTreeSet<String> = states
            .iter()
            .map(|state| normalize_state_name(state))
            .collect();

        Ok(self
            .load_issues()?
            .into_iter()
            .filter(|issue| wanted.contains(&issue.state_key()))
            .collect())
    }

    async fn fetch_issue_states_by_ids(
        &self,
        issue_ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        if issue_ids.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: std::collections::BTreeSet<&str> =
            issue_ids.iter().map(String::as_str).collect();

        Ok(self
            .load_issues()?
            .into_iter()
            .filter(|issue| wanted.contains(issue.id.as_str()))
            .collect())
    }

    async fn fetch_open_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        self.load_issues()
    }

    async fn raw_graphql(&self, _query: &str, _variables: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "memory tracker does not support raw_graphql".to_string(),
        ))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MemoryFixture {
    Issues(Vec<Issue>),
    Envelope { issues: Vec<Issue> },
}

fn parse_fixture(path: &Path, raw: &str) -> Result<Vec<Issue>, TrackerError> {
    let fixture = match path.extension().and_then(|ext| ext.to_str()) {
        Some("yaml" | "yml") => serde_yaml::from_str::<MemoryFixture>(raw).map_err(|error| {
            TrackerError::MemoryFixtureParse {
                path: path.display().to_string(),
                error: error.to_string(),
            }
        })?,
        _ => serde_json::from_str::<MemoryFixture>(raw).map_err(|error| {
            TrackerError::MemoryFixtureParse {
                path: path.display().to_string(),
                error: error.to_string(),
            }
        })?,
    };

    Ok(match fixture {
        MemoryFixture::Issues(issues) => issues,
        MemoryFixture::Envelope { issues } => issues,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::{config::ServiceConfig, tracker::TrackerClient};

    use super::MemoryTracker;

    #[tokio::test]
    async fn fetch_candidate_issues_filters_by_active_states() {
        let dir = tempdir().expect("tempdir");
        let fixture = dir.path().join("issues.json");
        std::fs::write(
            &fixture,
            r#"[
  {"id":"1","identifier":"MEM-1","title":"Todo","state":"Todo"},
  {"id":"2","identifier":"MEM-2","title":"Done","state":"Done"}
]"#,
        )
        .expect("fixture");

        let config = ServiceConfig::from_map(
            serde_json::json!({
                "tracker": {
                    "kind": "memory",
                    "fixture_path": fixture
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let tracker = MemoryTracker::new(config);
        let issues = tracker.fetch_candidate_issues().await.expect("issues");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "MEM-1");
    }

    #[tokio::test]
    async fn fetch_issue_states_by_ids_supports_yaml_envelope() {
        let dir = tempdir().expect("tempdir");
        let fixture = dir.path().join("issues.yaml");
        std::fs::write(
            &fixture,
            r#"issues:
  - id: "1"
    identifier: "MEM-1"
    title: "Todo"
    state: "In Progress"
    labels: ["backend"]
  - id: "2"
    identifier: "MEM-2"
    title: "Done"
    state: "Done"
"#,
        )
        .expect("fixture");

        let config = ServiceConfig::from_map(
            serde_json::json!({
                "tracker": {
                    "kind": "memory",
                    "fixture_path": fixture
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let tracker = MemoryTracker::new(config);
        let issues = tracker
            .fetch_issue_states_by_ids(&["1".to_string()])
            .await
            .expect("issues");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "MEM-1");
        assert_eq!(issues[0].labels, vec!["backend"]);
    }
}
