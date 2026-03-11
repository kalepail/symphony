pub mod linear;
pub mod memory;

use async_trait::async_trait;
use std::sync::Arc;
use thiserror::Error;

use crate::config::ServiceConfig;
use crate::issue::Issue;

#[derive(Debug, Error, Clone)]
pub enum TrackerError {
    #[error("unsupported_tracker_kind {0}")]
    UnsupportedTrackerKind(String),
    #[error("missing_tracker_api_key")]
    MissingTrackerApiKey,
    #[error("missing_tracker_project_slug")]
    MissingTrackerProjectSlug,
    #[error("missing_tracker_fixture_path")]
    MissingTrackerFixturePath,
    #[error("linear_api_request {0}")]
    LinearApiRequest(String),
    #[error("linear_api_status {status}")]
    LinearApiStatus { status: u16, body: String },
    #[error("linear_graphql_errors {0}")]
    LinearGraphqlErrors(String),
    #[error("linear_unknown_payload")]
    LinearUnknownPayload,
    #[error("linear_missing_end_cursor")]
    LinearMissingEndCursor,
    #[error("missing_linear_viewer_identity")]
    MissingViewerIdentity,
    #[error("memory_fixture_io path={path} error={error}")]
    MemoryFixtureIo { path: String, error: String },
    #[error("memory_fixture_parse path={path} error={error}")]
    MemoryFixtureParse { path: String, error: String },
    #[error("tracker_operation_unsupported {0}")]
    TrackerOperationUnsupported(String),
}

#[async_trait]
pub trait TrackerClient: Send + Sync {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError>;
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError>;
    async fn fetch_issue_states_by_ids(
        &self,
        issue_ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError>;
    async fn raw_graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, TrackerError>;
}

pub fn build_tracker_client(config: ServiceConfig) -> Result<Arc<dyn TrackerClient>, TrackerError> {
    match config.tracker.kind.as_deref() {
        Some("linear") => Ok(Arc::new(linear::LinearTracker::new(config))),
        Some("memory") => Ok(Arc::new(memory::MemoryTracker::new(config))),
        Some(other) => Err(TrackerError::UnsupportedTrackerKind(other.to_string())),
        None => Err(TrackerError::UnsupportedTrackerKind(
            "<missing>".to_string(),
        )),
    }
}
