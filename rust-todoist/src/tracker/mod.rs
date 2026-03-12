pub mod memory;
pub mod todoist;

use async_trait::async_trait;
use serde_json::Value;
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
    #[error("missing_tracker_project_id")]
    MissingTrackerProjectId,
    #[error("missing_tracker_fixture_path")]
    MissingTrackerFixturePath,
    #[error("todoist_api_request {0}")]
    TodoistApiRequest(String),
    #[error("todoist_api_status {status}")]
    TodoistApiStatus { status: u16, body: String },
    #[error("todoist_unknown_payload")]
    TodoistUnknownPayload,
    #[error("todoist_missing_current_user")]
    MissingTodoistCurrentUser,
    #[error("todoist_project_not_found {0}")]
    TodoistProjectNotFound(String),
    #[error("todoist_missing_required_section {0}")]
    TodoistMissingRequiredSection(String),
    #[error("todoist_comments_unavailable")]
    TodoistCommentsUnavailable,
    #[error("todoist_reminders_unavailable")]
    TodoistRemindersUnavailable,
    #[error("todoist_comment_too_large limit={limit} actual={actual}")]
    TodoistCommentTooLarge { limit: usize, actual: usize },
    #[error("todoist_rate_limited retry_after={retry_after:?}")]
    TodoistRateLimited { retry_after: Option<u64> },
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
    async fn fetch_open_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "fetch_open_issues".to_string(),
        ))
    }
    async fn get_current_user(&self) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "get_current_user".to_string(),
        ))
    }
    async fn list_projects(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "list_projects".to_string(),
        ))
    }
    async fn get_project(&self, _project_id: &str) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "get_project".to_string(),
        ))
    }
    async fn list_collaborators(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "list_collaborators".to_string(),
        ))
    }
    async fn list_tasks(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "list_tasks".to_string(),
        ))
    }
    async fn get_task(&self, _task_id: &str) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "get_task".to_string(),
        ))
    }
    async fn list_sections(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "list_sections".to_string(),
        ))
    }
    async fn get_section(&self, _section_id: &str) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "get_section".to_string(),
        ))
    }
    async fn list_labels(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "list_labels".to_string(),
        ))
    }
    async fn list_comments(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "list_comments".to_string(),
        ))
    }
    async fn create_comment(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "create_comment".to_string(),
        ))
    }
    async fn update_comment(
        &self,
        _comment_id: &str,
        _arguments: Value,
    ) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "update_comment".to_string(),
        ))
    }
    async fn update_task(&self, _task_id: &str, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "update_task".to_string(),
        ))
    }
    async fn move_task(&self, _task_id: &str, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "move_task".to_string(),
        ))
    }
    async fn close_task(&self, _task_id: &str) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "close_task".to_string(),
        ))
    }
    async fn reopen_task(&self, _task_id: &str) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "reopen_task".to_string(),
        ))
    }
    async fn create_task(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "create_task".to_string(),
        ))
    }
    async fn list_reminders(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "list_reminders".to_string(),
        ))
    }
    async fn create_reminder(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "create_reminder".to_string(),
        ))
    }
    async fn update_reminder(
        &self,
        _reminder_id: &str,
        _arguments: Value,
    ) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "update_reminder".to_string(),
        ))
    }
    async fn delete_reminder(&self, _reminder_id: &str) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "delete_reminder".to_string(),
        ))
    }
}

pub fn build_tracker_client(config: ServiceConfig) -> Result<Arc<dyn TrackerClient>, TrackerError> {
    match config.tracker.kind.as_deref() {
        Some("todoist") => Ok(Arc::new(todoist::TodoistTracker::new(config))),
        Some("memory") => Ok(Arc::new(memory::MemoryTracker::new(config))),
        Some(other) => Err(TrackerError::UnsupportedTrackerKind(other.to_string())),
        None => Err(TrackerError::UnsupportedTrackerKind(
            "<missing>".to_string(),
        )),
    }
}
