pub mod memory;
pub mod todoist;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use thiserror::Error;
use tokio::task_local;

use crate::config::ServiceConfig;
use crate::issue::Issue;

pub const TODOIST_COMMENT_SIZE_LIMIT: usize = 15_000;

task_local! {
    static REQUEST_LOG_HANDLE: RequestLogHandle;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RequestLogContext {
    pub source: String,
    pub issue_id: Option<String>,
    pub issue_identifier: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_action: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RequestRetrySummary {
    pub retry_count: u32,
    pub total_wait_secs: u64,
}

#[derive(Clone, Debug)]
pub struct RequestLogHandle {
    inner: Arc<Mutex<RequestLogState>>,
}

#[derive(Clone, Debug)]
struct RequestLogState {
    context: RequestLogContext,
    retry_summary: RequestRetrySummary,
}

impl RequestLogHandle {
    pub fn new(context: RequestLogContext) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RequestLogState {
                context,
                retry_summary: RequestRetrySummary::default(),
            })),
        }
    }

    pub fn context(&self) -> RequestLogContext {
        self.inner
            .lock()
            .expect("request log state poisoned")
            .context
            .clone()
    }

    pub fn record_retry(&self, delay_secs: u64) {
        let mut state = self.inner.lock().expect("request log state poisoned");
        state.retry_summary.retry_count = state.retry_summary.retry_count.saturating_add(1);
        state.retry_summary.total_wait_secs = state
            .retry_summary
            .total_wait_secs
            .saturating_add(delay_secs);
    }

    pub fn retry_summary(&self) -> RequestRetrySummary {
        self.inner
            .lock()
            .expect("request log state poisoned")
            .retry_summary
            .clone()
    }
}

pub async fn with_request_log_handle<F, T>(handle: RequestLogHandle, future: F) -> T
where
    F: Future<Output = T>,
{
    REQUEST_LOG_HANDLE.scope(handle, future).await
}

pub fn current_request_log_handle() -> Option<RequestLogHandle> {
    REQUEST_LOG_HANDLE.try_with(Clone::clone).ok()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrackerCapabilities {
    pub comments: bool,
    pub reminders: bool,
    pub activity_log: bool,
}

impl TrackerCapabilities {
    pub const fn full() -> Self {
        Self {
            comments: true,
            reminders: true,
            activity_log: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TrackerRateBudget {
    pub service: String,
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset_at: Option<DateTime<Utc>>,
    pub reset_in_seconds: Option<u64>,
    pub retry_after_seconds: Option<u64>,
    pub throttled_until: Option<DateTime<Utc>>,
    pub throttled_for_seconds: Option<u64>,
    pub next_request_at: Option<DateTime<Utc>>,
    pub next_request_in_seconds: Option<u64>,
    pub observed_at: Option<DateTime<Utc>>,
}

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
    #[error("todoist_assignee_not_resolvable assignee={assignee} project_id={project_id}")]
    TodoistAssigneeNotResolvable {
        assignee: String,
        project_id: String,
    },
    #[error("todoist_comments_unavailable")]
    TodoistCommentsUnavailable,
    #[error("todoist_reminders_unavailable")]
    TodoistRemindersUnavailable,
    #[error("todoist_activity_log_unavailable")]
    TodoistActivityLogUnavailable,
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
    async fn capabilities(&self) -> Result<TrackerCapabilities, TrackerError> {
        Ok(TrackerCapabilities::full())
    }
    async fn rate_budget(&self) -> Option<TrackerRateBudget> {
        None
    }
    async fn validate_startup(&self) -> Result<(), TrackerError> {
        Ok(())
    }
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
    async fn restore_active_issue(&self, _issue: &Issue) -> Result<(), TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "restore_active_issue".to_string(),
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
    async fn get_comment(&self, _comment_id: &str) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "get_comment".to_string(),
        ))
    }
    async fn delete_comment(&self, _comment_id: &str) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "delete_comment".to_string(),
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
    async fn list_activities(&self, _arguments: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "list_activities".to_string(),
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

#[cfg(test)]
mod tests {
    use super::{
        RequestLogContext, RequestLogHandle, current_request_log_handle, with_request_log_handle,
    };

    #[tokio::test]
    async fn request_log_handle_scopes_context_and_retry_summary() {
        let handle = RequestLogHandle::new(RequestLogContext {
            source: "tool_call".to_string(),
            issue_id: Some("task-1".to_string()),
            issue_identifier: Some("TD-task-1".to_string()),
            run_id: Some("run-1".to_string()),
            session_id: Some("session-1".to_string()),
            thread_id: Some("thread-1".to_string()),
            turn_id: Some("turn-1".to_string()),
            tool_name: Some("todoist".to_string()),
            tool_action: Some("todoist:get_task:task_id=task-1".to_string()),
        });

        let summary = with_request_log_handle(handle.clone(), async {
            let scoped = current_request_log_handle().expect("scoped request log");
            assert_eq!(scoped.context().source, "tool_call");
            scoped.record_retry(2);
            scoped.record_retry(4);
            scoped.retry_summary()
        })
        .await;

        assert_eq!(summary.retry_count, 2);
        assert_eq!(summary.total_wait_secs, 6);
        assert!(current_request_log_handle().is_none());
    }
}
