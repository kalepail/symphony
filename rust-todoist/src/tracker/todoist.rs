use std::collections::{BTreeSet, HashMap};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use reqwest::{
    Client, Method, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde_json::{Map, Value, json};
use tokio::time::sleep;
use tracing::warn;

use crate::{
    config::ServiceConfig,
    issue::{Issue, normalize_state_name},
    tracker::{TrackerClient, TrackerError},
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_PAGE_SIZE: usize = 200;
const MAX_ACTIVITY_PAGE_SIZE: usize = 100;
const DEFAULT_TOOL_PAGE_SIZE: usize = 50;
const COMMENT_SIZE_LIMIT: usize = 15_000;
const REFRESH_CONCURRENCY: usize = 10;
const TODOIST_RATE_LIMIT_MAX_RETRIES: usize = 4;
const TODOIST_RATE_LIMIT_DEFAULT_DELAY_SECS: u64 = 2;
const TODOIST_RATE_LIMIT_MAX_DELAY_SECS: u64 = 60;
const TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS: u64 = 300;
const TODOIST_COMMENT_UPDATE_MAX_RETRIES: usize = 2;
const TODOIST_COMMENT_UPDATE_MAX_DELAY_SECS: u64 = 15;
const TODOIST_COMMENT_UPDATE_MAX_TOTAL_WAIT_SECS: u64 = 30;
const TODOIST_PLAN_LIMITS_MAX_RETRIES: usize = 1;
const TODOIST_PLAN_LIMITS_MAX_DELAY_SECS: u64 = 5;
const TODOIST_PLAN_LIMITS_MAX_TOTAL_WAIT_SECS: u64 = 5;

#[derive(Clone)]
pub struct TodoistTracker {
    client: Client,
    config: ServiceConfig,
}

#[derive(Clone)]
pub(crate) struct AssigneeFilter {
    match_value: String,
}

#[derive(Clone, Copy)]
struct ProjectAssignmentCapabilities {
    is_shared: bool,
    can_assign_tasks: bool,
}

#[derive(Clone, Copy)]
struct TodoistRetryPolicy {
    max_retries: usize,
    default_delay_secs: u64,
    max_delay_secs: u64,
    max_total_wait_secs: u64,
}

const DEFAULT_TODOIST_RETRY_POLICY: TodoistRetryPolicy = TodoistRetryPolicy {
    max_retries: TODOIST_RATE_LIMIT_MAX_RETRIES,
    default_delay_secs: TODOIST_RATE_LIMIT_DEFAULT_DELAY_SECS,
    max_delay_secs: TODOIST_RATE_LIMIT_MAX_DELAY_SECS,
    max_total_wait_secs: TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS,
};

const COMMENT_UPDATE_TODOIST_RETRY_POLICY: TodoistRetryPolicy = TodoistRetryPolicy {
    max_retries: TODOIST_COMMENT_UPDATE_MAX_RETRIES,
    default_delay_secs: TODOIST_RATE_LIMIT_DEFAULT_DELAY_SECS,
    max_delay_secs: TODOIST_COMMENT_UPDATE_MAX_DELAY_SECS,
    max_total_wait_secs: TODOIST_COMMENT_UPDATE_MAX_TOTAL_WAIT_SECS,
};

const PLAN_LIMITS_TODOIST_RETRY_POLICY: TodoistRetryPolicy = TodoistRetryPolicy {
    max_retries: TODOIST_PLAN_LIMITS_MAX_RETRIES,
    default_delay_secs: TODOIST_RATE_LIMIT_DEFAULT_DELAY_SECS,
    max_delay_secs: TODOIST_PLAN_LIMITS_MAX_DELAY_SECS,
    max_total_wait_secs: TODOIST_PLAN_LIMITS_MAX_TOTAL_WAIT_SECS,
};

impl TodoistTracker {
    pub fn new(config: ServiceConfig) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_millis(DEFAULT_TIMEOUT_MS))
            .build()
            .expect("reqwest client");
        Self { client, config }
    }

    fn base_url(&self) -> String {
        self.config
            .tracker
            .base_url
            .trim_end_matches('/')
            .to_string()
    }

    fn token(&self) -> Result<String, TrackerError> {
        self.config
            .tracker
            .api_key
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or(TrackerError::MissingTrackerApiKey)
    }

    fn project_id(&self) -> Result<String, TrackerError> {
        self.config
            .tracker
            .project_id
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or(TrackerError::MissingTrackerProjectId)
    }

    async fn request_json(
        &self,
        method: Method,
        path: &str,
        query: Option<&[(String, String)]>,
        body: Option<Value>,
    ) -> Result<Value, TrackerError> {
        let token = self.token()?;
        let url = format!("{}{}", self.base_url(), path);
        let request_id = todoist_request_id(&method);
        let retry_policy = retry_policy_for_request(&method, path);
        let mut attempts = 0usize;
        let mut waited_secs = 0u64;

        loop {
            let mut request = self
                .client
                .request(method.clone(), &url)
                .bearer_auth(&token)
                .header("Accept", "application/json");

            if let Some(request_id) = request_id.as_deref() {
                request = request.header("X-Request-Id", request_id);
            }

            if let Some(query) = query {
                request = request.query(query);
            }

            if let Some(body) = body.as_ref() {
                request = request.json(body);
            }

            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    let error = TrackerError::TodoistApiRequest(error.to_string());
                    if let Some(delay_secs) =
                        retry_delay_for_attempt(&error, attempts, waited_secs, retry_policy)
                    {
                        warn!(
                            method = method.as_str(),
                            path,
                            attempt = attempts + 1,
                            max_retries = retry_policy.max_retries,
                            delay_secs,
                            total_wait_secs = waited_secs.saturating_add(delay_secs),
                            reason = todoist_retry_reason(&error),
                            "todoist request transient failure; retrying"
                        );
                        attempts += 1;
                        waited_secs = waited_secs.saturating_add(delay_secs);
                        sleep(std::time::Duration::from_secs(delay_secs)).await;
                        continue;
                    }
                    return Err(error);
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            let text = response
                .text()
                .await
                .map_err(|error| TrackerError::TodoistApiRequest(error.to_string()))?;

            if status.is_success() {
                if text.trim().is_empty() {
                    return Ok(Value::Null);
                }

                return serde_json::from_str(&text)
                    .map_err(|_| TrackerError::TodoistUnknownPayload);
            }

            let error = map_todoist_status(status, retry_after_hint(&headers, &text), &text);
            if let Some(delay_secs) =
                retry_delay_for_attempt(&error, attempts, waited_secs, retry_policy)
            {
                warn!(
                    method = method.as_str(),
                    path,
                    attempt = attempts + 1,
                    max_retries = retry_policy.max_retries,
                    delay_secs,
                    total_wait_secs = waited_secs.saturating_add(delay_secs),
                    reason = todoist_retry_reason(&error),
                    "todoist request failed transiently; retrying"
                );
                attempts += 1;
                waited_secs = waited_secs.saturating_add(delay_secs);
                sleep(std::time::Duration::from_secs(delay_secs)).await;
                continue;
            }

            return Err(error);
        }
    }

    async fn sync_json(&self, form: &[(&str, &str)]) -> Result<Value, TrackerError> {
        let token = self.token()?;
        let url = format!("{}/sync", self.base_url());
        let request_id = sync_id("todoist-sync");
        let retry_policy = retry_policy_for_sync_form(form);
        let mut attempts = 0usize;
        let mut waited_secs = 0u64;

        loop {
            let response = match self
                .client
                .post(&url)
                .bearer_auth(&token)
                .header("Accept", "application/json")
                .header("X-Request-Id", &request_id)
                .form(form)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    let error = TrackerError::TodoistApiRequest(error.to_string());
                    if let Some(delay_secs) =
                        retry_delay_for_attempt(&error, attempts, waited_secs, retry_policy)
                    {
                        warn!(
                            path = "/sync",
                            attempt = attempts + 1,
                            max_retries = retry_policy.max_retries,
                            delay_secs,
                            total_wait_secs = waited_secs.saturating_add(delay_secs),
                            reason = todoist_retry_reason(&error),
                            "todoist sync request transient failure; retrying"
                        );
                        attempts += 1;
                        waited_secs = waited_secs.saturating_add(delay_secs);
                        sleep(std::time::Duration::from_secs(delay_secs)).await;
                        continue;
                    }
                    return Err(error);
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            let text = response
                .text()
                .await
                .map_err(|error| TrackerError::TodoistApiRequest(error.to_string()))?;

            if status.is_success() {
                return serde_json::from_str(&text)
                    .map_err(|_| TrackerError::TodoistUnknownPayload);
            }

            let error = map_todoist_status(status, retry_after_hint(&headers, &text), &text);
            if let Some(delay_secs) =
                retry_delay_for_attempt(&error, attempts, waited_secs, retry_policy)
            {
                warn!(
                    path = "/sync",
                    attempt = attempts + 1,
                    max_retries = retry_policy.max_retries,
                    delay_secs,
                    total_wait_secs = waited_secs.saturating_add(delay_secs),
                    reason = todoist_retry_reason(&error),
                    "todoist sync request failed transiently; retrying"
                );
                attempts += 1;
                waited_secs = waited_secs.saturating_add(delay_secs);
                sleep(std::time::Duration::from_secs(delay_secs)).await;
                continue;
            }

            return Err(error);
        }
    }

    async fn sync_commands_json(&self, commands: Value) -> Result<Value, TrackerError> {
        let commands_payload = commands.to_string();
        self.sync_json(&[("commands", commands_payload.as_str())])
            .await
    }

    async fn get_project_resource(&self, project_id: &str) -> Result<Value, TrackerError> {
        self.request_json(Method::GET, &format!("/projects/{project_id}"), None, None)
            .await
            .map_err(|error| match error {
                TrackerError::TodoistApiStatus { status, .. } if status == 404 => {
                    TrackerError::TodoistProjectNotFound(project_id.to_string())
                }
                other => other,
            })
    }

    async fn get_projects_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Value, TrackerError> {
        let mut query = vec![("limit".to_string(), limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        self.request_json(Method::GET, "/projects", Some(&query), None)
            .await
    }

    async fn get_sections_page(
        &self,
        project_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Value, TrackerError> {
        let mut query = vec![
            ("project_id".to_string(), project_id.to_string()),
            ("limit".to_string(), limit.to_string()),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        self.request_json(Method::GET, "/sections", Some(&query), None)
            .await
    }

    async fn get_tasks_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
        extra: &[(String, String)],
    ) -> Result<Value, TrackerError> {
        let mut query = vec![("limit".to_string(), limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        query.extend(extra.iter().cloned());
        self.request_json(Method::GET, "/tasks", Some(&query), None)
            .await
    }

    async fn get_tasks_filter_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
        extra: &[(String, String)],
    ) -> Result<Value, TrackerError> {
        let mut query = vec![("limit".to_string(), limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        query.extend(extra.iter().cloned());
        self.request_json(Method::GET, "/tasks/filter", Some(&query), None)
            .await
    }

    async fn get_comments_page(&self, query: &[(String, String)]) -> Result<Value, TrackerError> {
        self.ensure_comments_available().await?;
        self.request_json(Method::GET, "/comments", Some(query), None)
            .await
    }

    async fn get_collaborators_page(
        &self,
        project_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Value, TrackerError> {
        let mut query = vec![("limit".to_string(), limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        self.request_json(
            Method::GET,
            &format!("/projects/{project_id}/collaborators"),
            Some(&query),
            None,
        )
        .await
    }

    async fn get_labels_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Value, TrackerError> {
        let mut query = vec![("limit".to_string(), limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        self.request_json(Method::GET, "/labels", Some(&query), None)
            .await
    }

    async fn get_activities_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
        extra: &[(String, String)],
    ) -> Result<Value, TrackerError> {
        let mut query = vec![("limit".to_string(), limit.to_string())];
        if let Some(cursor) = cursor {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        query.extend(extra.iter().cloned());
        self.request_json(Method::GET, "/activities", Some(&query), None)
            .await
    }

    async fn section_map_for_project(
        &self,
        project_id: &str,
    ) -> Result<HashMap<String, String>, TrackerError> {
        self.get_project_resource(&project_id).await?;
        let mut sections = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .get_sections_page(&project_id, cursor.as_deref(), MAX_PAGE_SIZE)
                .await?;
            let page_results = page
                .get("results")
                .and_then(Value::as_array)
                .ok_or(TrackerError::TodoistUnknownPayload)?;
            sections.extend(page_results.iter().cloned());
            cursor = page
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }

        let mut map = HashMap::new();
        for section in sections {
            if let Some(id) = json_id(section.get("id")) {
                let name = section
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Unnamed Section")
                    .to_string();
                map.insert(id, name);
            }
        }
        Ok(map)
    }

    async fn section_map(&self) -> Result<HashMap<String, String>, TrackerError> {
        let project_id = self.project_id()?;
        self.section_map_for_project(&project_id).await
    }

    fn project_assignment_capabilities(project: &Value) -> ProjectAssignmentCapabilities {
        ProjectAssignmentCapabilities {
            is_shared: project
                .get("is_shared")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            can_assign_tasks: project
                .get("can_assign_tasks")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    async fn collaborator_ids(&self, project_id: &str) -> Result<BTreeSet<String>, TrackerError> {
        let mut collaborator_ids = BTreeSet::new();
        let mut cursor: Option<String> = None;

        loop {
            let page = self
                .get_collaborators_page(project_id, cursor.as_deref(), MAX_PAGE_SIZE)
                .await?;
            let results = page
                .get("results")
                .and_then(Value::as_array)
                .ok_or(TrackerError::TodoistUnknownPayload)?;

            for collaborator in results {
                if let Some(id) = json_id(collaborator.get("id")) {
                    collaborator_ids.insert(id);
                }
            }

            cursor = page
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }

        Ok(collaborator_ids)
    }

    async fn validate_required_sections(
        &self,
        sections_by_id: &HashMap<String, String>,
        states: &[String],
    ) -> Result<(), TrackerError> {
        let available: BTreeSet<String> = sections_by_id
            .values()
            .map(|value| normalize_state_name(value))
            .collect();
        for state in states {
            let key = normalize_state_name(state);
            if !key.is_empty() && !available.contains(&key) {
                return Err(TrackerError::TodoistMissingRequiredSection(state.clone()));
            }
        }
        Ok(())
    }

    async fn resolve_assignee_filter_for_project(
        &self,
        project_id: &str,
        project: &Value,
    ) -> Result<Option<AssigneeFilter>, TrackerError> {
        let assignee = match self.config.tracker.assignee.clone() {
            Some(value) if !value.trim().is_empty() => value,
            _ => return Ok(None),
        };
        let assignee = assignee.trim().to_string();
        let capabilities = Self::project_assignment_capabilities(project);

        if !capabilities.can_assign_tasks {
            return Err(TrackerError::TodoistAssigneeNotResolvable {
                assignee,
                project_id: project_id.to_string(),
            });
        }

        if assignee == "me" {
            let user = self.get_current_user().await?;
            let id = json_id(user.get("id")).ok_or(TrackerError::MissingTodoistCurrentUser)?;
            return Ok(Some(AssigneeFilter { match_value: id }));
        }

        if capabilities.is_shared {
            let collaborator_ids = self.collaborator_ids(project_id).await?;
            if !collaborator_ids.contains(assignee.as_str()) {
                return Err(TrackerError::TodoistAssigneeNotResolvable {
                    assignee,
                    project_id: project_id.to_string(),
                });
            }
        }

        Ok(Some(AssigneeFilter {
            match_value: assignee,
        }))
    }

    async fn resolve_assignee_filter(&self) -> Result<Option<AssigneeFilter>, TrackerError> {
        let project_id = self.project_id()?;
        let project = self.get_project_resource(&project_id).await?;
        self.resolve_assignee_filter_for_project(&project_id, &project)
            .await
    }

    async fn fetch_all_project_tasks(&self) -> Result<Vec<Value>, TrackerError> {
        let project_id = self.project_id()?;
        let mut extra = vec![("project_id".to_string(), project_id)];
        if let Some(label) = self.runtime_label_filter() {
            extra.push(("label".to_string(), label));
        }
        let mut tasks = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .get_tasks_page(cursor.as_deref(), MAX_PAGE_SIZE, &extra)
                .await?;
            let page_results = page
                .get("results")
                .and_then(Value::as_array)
                .ok_or(TrackerError::TodoistUnknownPayload)?;
            tasks.extend(page_results.iter().cloned());
            cursor = page
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tasks)
    }

    async fn get_task_internal(&self, task_id: &str) -> Result<Option<Value>, TrackerError> {
        match self
            .request_json(Method::GET, &format!("/tasks/{task_id}"), None, None)
            .await
        {
            Ok(task) => Ok(Some(task)),
            Err(TrackerError::TodoistApiStatus { status, .. }) if status == 404 => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn issues_from_tasks(
        &self,
        tasks: Vec<Value>,
        sections_by_id: &HashMap<String, String>,
        assignee_filter: Option<&AssigneeFilter>,
    ) -> Vec<Issue> {
        let completed_state = self.completed_state_label();
        tasks
            .iter()
            .filter_map(|task| {
                normalize_task(
                    task,
                    sections_by_id,
                    assignee_filter,
                    completed_state.as_str(),
                )
            })
            .filter(|issue| !issue.is_subtask)
            .collect()
    }

    fn completed_state_label(&self) -> String {
        self.config
            .tracker
            .terminal_states
            .iter()
            .find(|value| normalize_state_name(value) == "done")
            .cloned()
            .unwrap_or_else(|| "Done".to_string())
    }

    fn runtime_label_filter(&self) -> Option<String> {
        self.config
            .tracker
            .label
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }

    async fn default_todo_section_id_for_project(
        &self,
        project_id: &str,
    ) -> Result<Option<String>, TrackerError> {
        let sections_by_id = self.section_map_for_project(project_id).await?;
        Ok(sections_by_id
            .into_iter()
            .find(|(_, name)| normalize_state_name(name) == "todo")
            .map(|(section_id, _)| section_id))
    }

    async fn ensure_comments_available(&self) -> Result<(), TrackerError> {
        let limits = match self.user_plan_limits().await {
            Ok(limits) => limits,
            Err(error) if comments_plan_probe_is_soft_failure(&error) => {
                warn!(
                    reason = todoist_retry_reason(&error),
                    "todoist comments capability probe unavailable; deferring to comment endpoint"
                );
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let comments_available = limits
            .get("comments")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if comments_available {
            Ok(())
        } else {
            Err(TrackerError::TodoistCommentsUnavailable)
        }
    }

    async fn ensure_reminders_available(&self) -> Result<(), TrackerError> {
        let limits = self.user_plan_limits().await?;
        let reminders_available = limits
            .get("reminders")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if reminders_available {
            Ok(())
        } else {
            Err(TrackerError::TodoistRemindersUnavailable)
        }
    }

    async fn ensure_activity_log_available(&self) -> Result<(), TrackerError> {
        let limits = self.user_plan_limits().await?;
        let activity_log_available = limits
            .get("activity_log")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if activity_log_available {
            Ok(())
        } else {
            Err(TrackerError::TodoistActivityLogUnavailable)
        }
    }

    async fn user_plan_limits(&self) -> Result<Value, TrackerError> {
        let limits = self
            .sync_json(&[
                ("sync_token", "*"),
                ("resource_types", "[\"user_plan_limits\"]"),
            ])
            .await?;
        Ok(limits
            .get("user_plan_limits")
            .and_then(|value| value.get("current"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn validate_startup_config(&self) -> Result<(), TrackerError> {
        let project_id = self.project_id()?;
        let project = self.get_project_resource(&project_id).await?;
        let sections_by_id = self.section_map_for_project(&project_id).await?;
        self.validate_required_sections(&sections_by_id, &self.config.tracker.active_states)
            .await?;
        self.resolve_assignee_filter_for_project(&project_id, &project)
            .await?;
        self.ensure_comments_available().await?;
        Ok(())
    }

    async fn comments_query(
        &self,
        arguments: &Value,
    ) -> Result<Vec<(String, String)>, TrackerError> {
        let map = arguments.as_object().ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(
                "comments arguments must be an object".to_string(),
            )
        })?;
        let target = comment_target(map)?;

        let mut query = Vec::new();
        match target {
            CommentTarget::Task(task_id) => query.push(("task_id".to_string(), task_id)),
            CommentTarget::Project(project_id) => {
                query.push(("project_id".to_string(), project_id))
            }
        }
        let limit = map
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(MAX_PAGE_SIZE as u64) as usize)
            .unwrap_or(DEFAULT_TOOL_PAGE_SIZE);
        query.push(("limit".to_string(), limit.to_string()));
        if let Some(cursor) = map.get("cursor").and_then(Value::as_str).map(str::trim)
            && !cursor.is_empty()
        {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        Ok(query)
    }
}

#[async_trait]
impl TrackerClient for TodoistTracker {
    async fn validate_startup(&self) -> Result<(), TrackerError> {
        self.validate_startup_config().await
    }

    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let sections_by_id = self.section_map().await?;
        self.validate_required_sections(&sections_by_id, &self.config.tracker.active_states)
            .await?;
        let assignee_filter = self.resolve_assignee_filter().await?;
        let active_states: BTreeSet<String> = self
            .config
            .tracker
            .active_states
            .iter()
            .map(|value| normalize_state_name(value))
            .collect();

        let issues = self
            .issues_from_tasks(
                self.fetch_all_project_tasks().await?,
                &sections_by_id,
                assignee_filter.as_ref(),
            )
            .await;

        Ok(issues
            .into_iter()
            .filter(|issue| !issue.is_subtask && active_states.contains(&issue.state_key()))
            .collect())
    }

    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let sections_by_id = self.section_map().await?;
        let wanted: BTreeSet<String> = states
            .iter()
            .map(|value| normalize_state_name(value))
            .collect();
        let issues = self
            .issues_from_tasks(self.fetch_all_project_tasks().await?, &sections_by_id, None)
            .await;
        Ok(issues
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
        let sections_by_id = self.section_map().await?;
        let assignee_filter = self.resolve_assignee_filter().await?;
        let completed_state = self.completed_state_label();
        let unique_ids: Vec<String> = {
            let mut seen = BTreeSet::new();
            issue_ids
                .iter()
                .filter(|id| seen.insert((*id).clone()))
                .cloned()
                .collect()
        };

        let fetched = stream::iter(unique_ids.iter().cloned())
            .map(|task_id| async move { (task_id.clone(), self.get_task_internal(&task_id).await) })
            .buffer_unordered(REFRESH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        let mut by_id = HashMap::new();
        for (task_id, result) in fetched {
            if let Some(task) = result? {
                if let Some(issue) = normalize_task(
                    &task,
                    &sections_by_id,
                    assignee_filter.as_ref(),
                    completed_state.as_str(),
                ) {
                    by_id.insert(task_id, issue);
                }
            }
        }

        Ok(issue_ids
            .iter()
            .filter_map(|task_id| by_id.get(task_id).cloned())
            .collect())
    }

    async fn fetch_open_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let sections_by_id = self.section_map().await?;
        Ok(self
            .issues_from_tasks(self.fetch_all_project_tasks().await?, &sections_by_id, None)
            .await)
    }

    async fn restore_active_issue(&self, issue: &Issue) -> Result<(), TrackerError> {
        match self.reopen_task(&issue.id).await {
            Ok(_) => {}
            Err(TrackerError::TodoistApiStatus { status, .. }) if matches!(status, 400 | 409) => {}
            Err(error) => return Err(error),
        }

        if let Some(section_id) = issue.section_id.as_deref() {
            self.move_task(&issue.id, json!({ "section_id": section_id }))
                .await?;
        }

        Ok(())
    }

    async fn get_current_user(&self) -> Result<Value, TrackerError> {
        self.request_json(Method::GET, "/user", None, None).await
    }

    async fn list_projects(&self, arguments: Value) -> Result<Value, TrackerError> {
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(MAX_PAGE_SIZE as u64) as usize)
            .unwrap_or(DEFAULT_TOOL_PAGE_SIZE);
        let cursor = arguments
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        self.get_projects_page(cursor, limit).await
    }

    async fn get_project(&self, project_id: &str) -> Result<Value, TrackerError> {
        self.get_project_resource(project_id).await
    }

    async fn list_collaborators(&self, arguments: Value) -> Result<Value, TrackerError> {
        let project_id = arguments
            .get("project_id")
            .and_then(json_id_from_value)
            .or_else(|| self.config.tracker.project_id.clone())
            .ok_or(TrackerError::MissingTrackerProjectId)?;
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(MAX_PAGE_SIZE as u64) as usize)
            .unwrap_or(DEFAULT_TOOL_PAGE_SIZE);
        let cursor = arguments
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        self.get_collaborators_page(&project_id, cursor, limit)
            .await
    }

    async fn list_tasks(&self, arguments: Value) -> Result<Value, TrackerError> {
        let map = arguments.as_object().cloned().unwrap_or_default();
        let limit = map
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(MAX_PAGE_SIZE as u64) as usize)
            .unwrap_or(DEFAULT_TOOL_PAGE_SIZE);
        let cursor = map
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let (path, extra) = task_list_request(&map, self.config.tracker.project_id.clone())?;
        match path {
            "/tasks" => self.get_tasks_page(cursor, limit, &extra).await,
            "/tasks/filter" => self.get_tasks_filter_page(cursor, limit, &extra).await,
            _ => unreachable!("unsupported task list path"),
        }
    }

    async fn get_task(&self, task_id: &str) -> Result<Value, TrackerError> {
        self.request_json(Method::GET, &format!("/tasks/{task_id}"), None, None)
            .await
    }

    async fn list_sections(&self, arguments: Value) -> Result<Value, TrackerError> {
        let project_id = arguments
            .get("project_id")
            .and_then(json_id_from_value)
            .or_else(|| self.config.tracker.project_id.clone())
            .ok_or(TrackerError::MissingTrackerProjectId)?;
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(MAX_PAGE_SIZE as u64) as usize)
            .unwrap_or(DEFAULT_TOOL_PAGE_SIZE);
        let cursor = arguments
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        self.get_sections_page(&project_id, cursor, limit).await
    }

    async fn get_section(&self, section_id: &str) -> Result<Value, TrackerError> {
        self.request_json(Method::GET, &format!("/sections/{section_id}"), None, None)
            .await
    }

    async fn list_labels(&self, arguments: Value) -> Result<Value, TrackerError> {
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(MAX_PAGE_SIZE as u64) as usize)
            .unwrap_or(DEFAULT_TOOL_PAGE_SIZE);
        let cursor = arguments
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        self.get_labels_page(cursor, limit).await
    }

    async fn list_comments(&self, arguments: Value) -> Result<Value, TrackerError> {
        let query = self.comments_query(&arguments).await?;
        self.get_comments_page(&query).await
    }

    async fn get_comment(&self, comment_id: &str) -> Result<Value, TrackerError> {
        self.ensure_comments_available().await?;
        self.request_json(Method::GET, &format!("/comments/{comment_id}"), None, None)
            .await
    }

    async fn delete_comment(&self, comment_id: &str) -> Result<Value, TrackerError> {
        self.ensure_comments_available().await?;
        self.request_json(
            Method::DELETE,
            &format!("/comments/{comment_id}"),
            None,
            None,
        )
        .await
    }

    async fn create_comment(&self, arguments: Value) -> Result<Value, TrackerError> {
        self.ensure_comments_available().await?;
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                TrackerError::TrackerOperationUnsupported(
                    "`content` is required for create_comment".to_string(),
                )
            })?;
        if content.len() > COMMENT_SIZE_LIMIT {
            return Err(TrackerError::TodoistCommentTooLarge {
                limit: COMMENT_SIZE_LIMIT,
                actual: content.len(),
            });
        }
        let mut body = comment_target_body(arguments)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        body.insert("content".to_string(), Value::String(content));
        self.request_json(Method::POST, "/comments", None, Some(Value::Object(body)))
            .await
    }

    async fn update_comment(
        &self,
        comment_id: &str,
        arguments: Value,
    ) -> Result<Value, TrackerError> {
        self.ensure_comments_available().await?;
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                TrackerError::TrackerOperationUnsupported(
                    "`content` is required for update_comment".to_string(),
                )
            })?;
        if content.len() > COMMENT_SIZE_LIMIT {
            return Err(TrackerError::TodoistCommentTooLarge {
                limit: COMMENT_SIZE_LIMIT,
                actual: content.len(),
            });
        }
        self.request_json(
            Method::POST,
            &format!("/comments/{comment_id}"),
            None,
            Some(json!({ "content": content })),
        )
        .await
    }

    async fn update_task(&self, task_id: &str, arguments: Value) -> Result<Value, TrackerError> {
        let mut body = sanitize_write_arguments(arguments)
            .as_object()
            .cloned()
            .unwrap_or_default();
        if body.contains_key("labels")
            && let Some(label) = self.runtime_label_filter().as_deref()
        {
            enforce_runtime_label_scope(&mut body, label);
        }
        self.request_json(
            Method::POST,
            &format!("/tasks/{task_id}"),
            None,
            Some(Value::Object(body)),
        )
        .await
    }

    async fn move_task(&self, task_id: &str, arguments: Value) -> Result<Value, TrackerError> {
        let body = move_task_body(arguments)?;
        self.request_json(
            Method::POST,
            &format!("/tasks/{task_id}/move"),
            None,
            Some(body),
        )
        .await
    }

    async fn close_task(&self, task_id: &str) -> Result<Value, TrackerError> {
        self.request_json(Method::POST, &format!("/tasks/{task_id}/close"), None, None)
            .await
    }

    async fn reopen_task(&self, task_id: &str) -> Result<Value, TrackerError> {
        self.request_json(
            Method::POST,
            &format!("/tasks/{task_id}/reopen"),
            None,
            None,
        )
        .await
    }

    async fn create_task(&self, arguments: Value) -> Result<Value, TrackerError> {
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                TrackerError::TrackerOperationUnsupported(
                    "`content` is required for create_task".to_string(),
                )
            })?;
        let mut body = merge_action_defaults(
            sanitize_write_arguments(arguments),
            json!({ "content": content }),
        )
        .as_object()
        .cloned()
        .unwrap_or_default();
        if body.get("project_id").is_none()
            && let Some(project_id) = self.config.tracker.project_id.clone()
        {
            body.insert("project_id".to_string(), Value::String(project_id));
        }
        if let Some(label) = self.runtime_label_filter().as_deref() {
            enforce_runtime_label_scope(&mut body, label);
        }
        if body.get("parent_id").is_none() && body.get("section_id").is_none() {
            if let Some(project_id) = body.get("project_id").and_then(json_id_from_value)
                && let Some(section_id) = self
                    .default_todo_section_id_for_project(&project_id)
                    .await?
            {
                body.insert("section_id".to_string(), Value::String(section_id));
            }
        }
        self.request_json(Method::POST, "/tasks", None, Some(Value::Object(body)))
            .await
    }

    async fn list_reminders(&self, arguments: Value) -> Result<Value, TrackerError> {
        self.ensure_reminders_available().await?;
        let reminders = self
            .sync_json(&[("sync_token", "*"), ("resource_types", "[\"reminders\"]")])
            .await?
            .get("reminders")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let task_id = arguments.get("task_id").and_then(json_id_from_value);
        let filtered: Vec<Value> = reminders
            .into_iter()
            .filter(|reminder| {
                task_id.as_ref().is_none_or(|task_id| {
                    reminder
                        .get("item_id")
                        .and_then(json_id_from_value)
                        .as_deref()
                        == Some(task_id.as_str())
                })
            })
            .collect();
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(MAX_PAGE_SIZE as u64) as usize)
            .unwrap_or(DEFAULT_TOOL_PAGE_SIZE);
        let cursor = arguments
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        Ok(paginate_results(filtered, cursor, limit))
    }

    async fn list_activities(&self, arguments: Value) -> Result<Value, TrackerError> {
        self.ensure_activity_log_available().await?;
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(MAX_ACTIVITY_PAGE_SIZE as u64) as usize)
            .unwrap_or(DEFAULT_TOOL_PAGE_SIZE.min(MAX_ACTIVITY_PAGE_SIZE));
        let cursor = arguments
            .get("cursor")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let mut extra = Vec::new();
        for key in [
            "object_type",
            "object_id",
            "parent_project_id",
            "parent_item_id",
            "event_type",
            "date_from",
            "date_to",
        ] {
            if let Some(value) = scalar_query_value(arguments.get(key)) {
                extra.push((key.to_string(), value));
            }
        }
        for key in [
            "include_parent_object",
            "include_child_objects",
            "annotate_notes",
            "annotate_parents",
            "initiator_id_null",
        ] {
            if let Some(value) = bool_query_value(arguments.get(key)) {
                extra.push((key.to_string(), value));
            }
        }
        for key in ["object_event_types", "initiator_id", "workspace_id"] {
            extend_repeated_query_values(&mut extra, key, arguments.get(key));
        }

        self.get_activities_page(cursor, limit, &extra).await
    }

    async fn create_reminder(&self, arguments: Value) -> Result<Value, TrackerError> {
        self.ensure_reminders_available().await?;
        let body = create_reminder_body(arguments)?;
        let temp_id = sync_id("temp");
        let uuid = sync_id("uuid");
        let response = self
            .sync_commands_json(json!([{
                "type": "reminder_add",
                "temp_id": temp_id,
                "uuid": uuid,
                "args": body
            }]))
            .await?;
        sync_command_status_ok(&response)?;
        let reminder_id = response
            .get("temp_id_mapping")
            .and_then(|value| value.get(&temp_id))
            .and_then(json_id_from_value);
        Ok(json!({
            "status": "ok",
            "id": reminder_id,
            "response": response
        }))
    }

    async fn update_reminder(
        &self,
        reminder_id: &str,
        arguments: Value,
    ) -> Result<Value, TrackerError> {
        self.ensure_reminders_available().await?;
        let body = update_reminder_body(reminder_id, arguments)?;
        let response = self
            .sync_commands_json(json!([{
                "type": "reminder_update",
                "uuid": sync_id("uuid"),
                "args": body
            }]))
            .await?;
        sync_command_status_ok(&response)?;
        Ok(json!({
            "status": "ok",
            "id": reminder_id,
            "response": response
        }))
    }

    async fn delete_reminder(&self, reminder_id: &str) -> Result<Value, TrackerError> {
        self.ensure_reminders_available().await?;
        let response = self
            .sync_commands_json(json!([{
                "type": "reminder_delete",
                "uuid": sync_id("uuid"),
                "args": { "id": reminder_id }
            }]))
            .await?;
        sync_command_status_ok(&response)?;
        Ok(json!({
            "status": "ok",
            "id": reminder_id,
            "deleted": true,
            "response": response
        }))
    }
}

pub(crate) fn normalize_task(
    task: &Value,
    sections_by_id: &HashMap<String, String>,
    assignee_filter: Option<&AssigneeFilter>,
    completed_state: &str,
) -> Option<Issue> {
    let id = json_id(task.get("id"))?;
    let identifier = format!("TD-{id}");
    let task_url = task
        .get("url")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| todoist_task_url(&id));
    let title = task.get("content")?.as_str()?.trim().to_string();
    if title.is_empty() {
        return None;
    }

    let assignee_id = task
        .get("assignee_id")
        .and_then(json_id_from_value)
        .or_else(|| task.get("responsible_uid").and_then(json_id_from_value));
    let assigned_to_worker = assignee_filter
        .map(|filter| assignee_id.as_deref() == Some(filter.match_value.as_str()))
        .unwrap_or(true);
    let section_name = if task_is_completed(task) {
        completed_state.to_string()
    } else {
        task.get("section_id")
            .and_then(json_id_from_value)
            .and_then(|section_id| sections_by_id.get(&section_id).cloned())
            .unwrap_or_else(|| "Unsectioned".to_string())
    };

    Some(Issue {
        id,
        identifier,
        title,
        description: optional_non_empty_string(task.get("description")),
        priority: task
            .get("priority")
            .and_then(Value::as_i64)
            .map(normalize_priority),
        state: section_name,
        url: Some(task_url),
        labels: task
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        created_at: parse_datetime(task.get("created_at").or_else(|| task.get("added_at"))),
        updated_at: parse_datetime(task.get("updated_at")),
        project_id: task.get("project_id").and_then(json_id_from_value),
        section_id: task.get("section_id").and_then(json_id_from_value),
        parent_id: task.get("parent_id").and_then(json_id_from_value),
        is_subtask: task.get("parent_id").and_then(json_id_from_value).is_some(),
        due: task.get("due").cloned(),
        deadline: task.get("deadline").cloned(),
        assignee_id,
        assigned_to_worker,
    })
}

fn task_is_completed(task: &Value) -> bool {
    task.get("checked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || task
            .get("is_completed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || optional_non_empty_string(task.get("completed_at")).is_some()
}

fn optional_non_empty_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_datetime(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn normalize_priority(value: i64) -> i64 {
    match value {
        4 => 1,
        3 => 2,
        2 => 3,
        1 => 4,
        other => other,
    }
}

pub(crate) fn todoist_task_url(task_id: &str) -> String {
    format!("https://app.todoist.com/app/task/{task_id}")
}

fn json_id(value: Option<&Value>) -> Option<String> {
    value.and_then(json_id_from_value)
}

fn json_id_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

enum CommentTarget {
    Task(String),
    Project(String),
}

fn task_list_request(
    arguments: &serde_json::Map<String, Value>,
    default_project_id: Option<String>,
) -> Result<(&'static str, Vec<(String, String)>), TrackerError> {
    let mut query = Vec::new();

    if let Some(filter) = arguments
        .get("filter")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        query.push(("filter".to_string(), filter.to_string()));
        if let Some(lang) = arguments
            .get("lang")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            query.push(("lang".to_string(), lang.to_string()));
        }
        return Ok(("/tasks/filter", query));
    }

    if let Some(ids) = csv_query_value(arguments.get("ids")) {
        query.push(("ids".to_string(), ids));
        return Ok(("/tasks", query));
    }

    if let Some(project_id) = arguments.get("project_id").and_then(json_id_from_value) {
        query.push(("project_id".to_string(), project_id));
    } else if arguments.get("section_id").is_none()
        && arguments.get("parent_id").is_none()
        && arguments.get("label").is_none()
    {
        let project_id = default_project_id.ok_or(TrackerError::MissingTrackerProjectId)?;
        query.push(("project_id".to_string(), project_id));
    }

    if let Some(section_id) = arguments.get("section_id").and_then(json_id_from_value) {
        query.push(("section_id".to_string(), section_id));
    }
    if let Some(parent_id) = arguments.get("parent_id").and_then(json_id_from_value) {
        query.push(("parent_id".to_string(), parent_id));
    }
    if let Some(label) = arguments
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        query.push(("label".to_string(), label.to_string()));
    }

    Ok(("/tasks", query))
}

fn merge_action_defaults(arguments: Value, defaults: Value) -> Value {
    let mut merged = defaults.as_object().cloned().unwrap_or_default();
    if let Some(arguments) = arguments.as_object() {
        for (key, value) in arguments {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

fn sanitize_write_arguments(arguments: Value) -> Value {
    let mut map = arguments.as_object().cloned().unwrap_or_default();
    for key in ["action", "task_id", "comment_id", "reminder_id"] {
        map.remove(key);
    }
    Value::Object(map)
}

fn enforce_runtime_label_scope(body: &mut Map<String, Value>, runtime_label: &str) {
    let runtime_label = runtime_label.trim();
    if runtime_label.is_empty() {
        return;
    }

    let mut labels = body
        .get("labels")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if !labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(runtime_label))
    {
        labels.push(runtime_label.to_string());
    }

    body.insert(
        "labels".to_string(),
        Value::Array(labels.into_iter().map(Value::String).collect()),
    );
}

fn sanitize_comment_arguments(arguments: Value) -> Value {
    let mut map = arguments.as_object().cloned().unwrap_or_default();
    for key in ["action", "comment_id"] {
        map.remove(key);
    }
    Value::Object(map)
}

fn comment_target_body(arguments: Value) -> Result<Value, TrackerError> {
    let mut body = sanitize_comment_arguments(arguments)
        .as_object()
        .cloned()
        .unwrap_or_default();
    match comment_target(&body)? {
        CommentTarget::Task(_) => {
            body.remove("project_id");
        }
        CommentTarget::Project(_) => {
            body.remove("task_id");
        }
    }
    Ok(Value::Object(body))
}

fn comment_target(
    arguments: &serde_json::Map<String, Value>,
) -> Result<CommentTarget, TrackerError> {
    let task_id = arguments.get("task_id").and_then(json_id_from_value);
    let project_id = arguments.get("project_id").and_then(json_id_from_value);
    match (task_id, project_id) {
        (Some(task_id), None) => Ok(CommentTarget::Task(task_id)),
        (None, Some(project_id)) => Ok(CommentTarget::Project(project_id)),
        _ => Err(TrackerError::TrackerOperationUnsupported(
            "exactly one of `task_id` or `project_id` is required".to_string(),
        )),
    }
}

fn move_task_body(arguments: Value) -> Result<Value, TrackerError> {
    let sanitized = sanitize_write_arguments(arguments);
    let mut map = sanitized.as_object().cloned().unwrap_or_default();
    map.retain(|key, _| matches!(key.as_str(), "project_id" | "section_id" | "parent_id"));
    if map.is_empty() {
        return Err(TrackerError::TrackerOperationUnsupported(
            "`section_id`, `project_id`, or `parent_id` is required for move_task".to_string(),
        ));
    }
    Ok(Value::Object(map))
}

fn create_reminder_body(arguments: Value) -> Result<Value, TrackerError> {
    let mut body = arguments.as_object().cloned().unwrap_or_default();
    for key in ["action", "comment_id", "reminder_id"] {
        body.remove(key);
    }
    if let Some(task_id) = body.remove("task_id") {
        body.insert("item_id".to_string(), task_id);
    }
    let has_item_id = body.get("item_id").and_then(json_id_from_value).is_some();
    if !has_item_id {
        return Err(TrackerError::TrackerOperationUnsupported(
            "`task_id` is required for create_reminder".to_string(),
        ));
    }
    Ok(Value::Object(body))
}

fn update_reminder_body(reminder_id: &str, arguments: Value) -> Result<Value, TrackerError> {
    let mut body = sanitize_write_arguments(arguments)
        .as_object()
        .cloned()
        .unwrap_or_default();
    body.remove("task_id");
    body.insert("id".to_string(), Value::String(reminder_id.to_string()));
    if body.len() <= 1 {
        return Err(TrackerError::TrackerOperationUnsupported(
            "at least one reminder field is required for update_reminder".to_string(),
        ));
    }
    Ok(Value::Object(body))
}

fn paginate_results(results: Vec<Value>, cursor: Option<&str>, limit: usize) -> Value {
    let start = cursor
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    let end = start.saturating_add(limit).min(results.len());
    let next_cursor = (end < results.len()).then(|| end.to_string());
    json!({
        "results": results[start..end].to_vec(),
        "next_cursor": next_cursor
    })
}

fn scalar_query_value(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| match value {
        Value::String(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn bool_query_value(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_bool)
        .map(|value| value.to_string())
}

fn csv_query_value(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::Array(values)) => {
            let values = values
                .iter()
                .filter_map(scalar_query_value_from_value)
                .collect::<Vec<_>>();
            (!values.is_empty()).then(|| values.join(","))
        }
        Some(value) => scalar_query_value_from_value(value),
        None => None,
    }
}

fn extend_repeated_query_values(
    query: &mut Vec<(String, String)>,
    key: &str,
    value: Option<&Value>,
) {
    match value {
        Some(Value::Array(values)) => {
            for value in values.iter().filter_map(scalar_query_value_from_value) {
                query.push((key.to_string(), value));
            }
        }
        Some(value) => {
            if let Some(value) = scalar_query_value_from_value(value) {
                query.push((key.to_string(), value));
            }
        }
        None => {}
    }
}

fn scalar_query_value_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn sync_command_status_ok(response: &Value) -> Result<(), TrackerError> {
    let Some(statuses) = response.get("sync_status").and_then(Value::as_object) else {
        return Ok(());
    };

    for status in statuses.values() {
        match status {
            Value::String(value) if value == "ok" => continue,
            other => {
                return Err(TrackerError::TodoistApiRequest(format!(
                    "Todoist sync command failed: {other}"
                )));
            }
        }
    }
    Ok(())
}

fn sync_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{nanos}")
}

fn todoist_request_id(method: &Method) -> Option<String> {
    matches!(*method, Method::POST | Method::DELETE).then(|| sync_id("todoist-request"))
}

fn retry_policy_for_request(method: &Method, path: &str) -> TodoistRetryPolicy {
    if *method == Method::POST && is_comment_update_path(path) {
        COMMENT_UPDATE_TODOIST_RETRY_POLICY
    } else {
        DEFAULT_TODOIST_RETRY_POLICY
    }
}

fn retry_policy_for_sync_form(form: &[(&str, &str)]) -> TodoistRetryPolicy {
    if is_user_plan_limits_sync_form(form) {
        PLAN_LIMITS_TODOIST_RETRY_POLICY
    } else {
        DEFAULT_TODOIST_RETRY_POLICY
    }
}

fn is_comment_update_path(path: &str) -> bool {
    path.strip_prefix("/comments/")
        .is_some_and(|suffix| !suffix.is_empty() && !suffix.contains('/'))
}

fn is_user_plan_limits_sync_form(form: &[(&str, &str)]) -> bool {
    form.iter()
        .any(|(key, value)| *key == "resource_types" && value.trim() == "[\"user_plan_limits\"]")
}

fn retry_after_hint(headers: &HeaderMap, text: &str) -> Option<u64> {
    let header_hint = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let body_hint = serde_json::from_str::<Value>(text).ok().and_then(|body| {
        body.get("error_extra")
            .and_then(|value| value.get("retry_after"))
            .and_then(Value::as_u64)
    });

    match (header_hint, body_hint) {
        (Some(header), Some(body)) => Some(header.max(body)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn retry_delay_for_attempt(
    error: &TrackerError,
    attempts: usize,
    waited_secs: u64,
    policy: TodoistRetryPolicy,
) -> Option<u64> {
    match error {
        TrackerError::TodoistRateLimited { retry_after } if attempts < policy.max_retries => {
            let delay_secs = retry_after
                .unwrap_or(policy.default_delay_secs)
                .clamp(1, policy.max_delay_secs);
            waited_secs
                .saturating_add(delay_secs)
                .le(&policy.max_total_wait_secs)
                .then_some(delay_secs)
        }
        TrackerError::TodoistApiStatus { status, .. }
            if attempts < policy.max_retries && todoist_status_is_transient(*status) =>
        {
            transient_retry_delay_seconds(attempts, waited_secs, policy)
        }
        TrackerError::TodoistApiRequest(_) if attempts < policy.max_retries => {
            transient_retry_delay_seconds(attempts, waited_secs, policy)
        }
        _ => None,
    }
}

fn transient_retry_delay_seconds(
    attempts: usize,
    waited_secs: u64,
    policy: TodoistRetryPolicy,
) -> Option<u64> {
    let power = (attempts as u32).min(6);
    let delay_secs = policy
        .default_delay_secs
        .saturating_mul(2u64.saturating_pow(power))
        .clamp(1, policy.max_delay_secs);
    waited_secs
        .saturating_add(delay_secs)
        .le(&policy.max_total_wait_secs)
        .then_some(delay_secs)
}

fn todoist_status_is_transient(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn comments_plan_probe_is_soft_failure(error: &TrackerError) -> bool {
    match error {
        TrackerError::TodoistRateLimited { .. } => true,
        TrackerError::TodoistApiRequest(_) => true,
        TrackerError::TodoistApiStatus { status, .. } => todoist_status_is_transient(*status),
        _ => false,
    }
}

fn todoist_retry_reason(error: &TrackerError) -> &'static str {
    match error {
        TrackerError::TodoistRateLimited { .. } => "rate_limited",
        TrackerError::TodoistApiStatus { status, .. } if todoist_status_is_transient(*status) => {
            "server_error"
        }
        TrackerError::TodoistApiRequest(_) => "transport_error",
        _ => "non_retryable",
    }
}

fn map_todoist_status(status: StatusCode, retry_after: Option<u64>, text: &str) -> TrackerError {
    if status == StatusCode::TOO_MANY_REQUESTS {
        TrackerError::TodoistRateLimited { retry_after }
    } else {
        TrackerError::TodoistApiStatus {
            status: status.as_u16(),
            body: text.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use axum::{
        Json, Router,
        extract::{Path, Query, State},
        http::{HeaderMap, StatusCode as HttpStatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use reqwest::Method;
    use serde_json::{Value, json};
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::{
        DEFAULT_TODOIST_RETRY_POLICY, RETRY_AFTER, TODOIST_COMMENT_UPDATE_MAX_DELAY_SECS,
        TODOIST_COMMENT_UPDATE_MAX_RETRIES, TODOIST_COMMENT_UPDATE_MAX_TOTAL_WAIT_SECS,
        TODOIST_PLAN_LIMITS_MAX_DELAY_SECS, TODOIST_PLAN_LIMITS_MAX_RETRIES,
        TODOIST_PLAN_LIMITS_MAX_TOTAL_WAIT_SECS, TodoistTracker, comment_target_body,
        create_reminder_body, csv_query_value, enforce_runtime_label_scope,
        extend_repeated_query_values, move_task_body, normalize_task, retry_after_hint,
        retry_delay_for_attempt, retry_policy_for_request, retry_policy_for_sync_form,
        sanitize_comment_arguments, sanitize_write_arguments, task_list_request,
        update_reminder_body,
    };
    use crate::{
        config::ServiceConfig,
        tracker::{TrackerClient, TrackerError},
    };

    #[derive(Clone)]
    struct MockTodoistState {
        project: Value,
        sections: Vec<Value>,
        collaborators: Vec<Value>,
        current_user: Value,
        plan_limits: Value,
    }

    struct MockTodoistServer {
        base_url: String,
        join: JoinHandle<()>,
    }

    impl Drop for MockTodoistServer {
        fn drop(&mut self) {
            self.join.abort();
        }
    }

    async fn spawn_mock_todoist(state: MockTodoistState) -> MockTodoistServer {
        async fn get_project(
            State(state): State<Arc<MockTodoistState>>,
            Path(_project_id): Path<String>,
        ) -> Json<Value> {
            Json(state.project.clone())
        }

        async fn list_sections(
            State(state): State<Arc<MockTodoistState>>,
            Query(_query): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            Json(json!({
                "results": state.sections,
                "next_cursor": null
            }))
        }

        async fn list_collaborators(
            State(state): State<Arc<MockTodoistState>>,
            Path(_project_id): Path<String>,
            Query(_query): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            Json(json!({
                "results": state.collaborators,
                "next_cursor": null
            }))
        }

        async fn get_user(State(state): State<Arc<MockTodoistState>>) -> Json<Value> {
            Json(state.current_user.clone())
        }

        async fn sync(State(state): State<Arc<MockTodoistState>>) -> Json<Value> {
            Json(json!({
                "user_plan_limits": {
                    "current": state.plan_limits
                }
            }))
        }

        let app = Router::new()
            .route("/projects/{project_id}", get(get_project))
            .route("/sections", get(list_sections))
            .route(
                "/projects/{project_id}/collaborators",
                get(list_collaborators),
            )
            .route("/user", get(get_user))
            .route("/sync", post(sync))
            .with_state(Arc::new(state));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        MockTodoistServer {
            base_url: format!("http://{address}"),
            join,
        }
    }

    fn tracker_config(base_url: &str, assignee: Option<&str>) -> ServiceConfig {
        let mut tracker = json!({
            "kind": "todoist",
            "base_url": base_url,
            "api_key": "token",
            "project_id": "proj",
            "active_states": ["Todo", "In Progress"]
        });
        if let Some(assignee) = assignee {
            tracker["assignee"] = json!(assignee);
        }

        ServiceConfig::from_map(
            json!({
                "tracker": tracker
            })
            .as_object()
            .expect("object"),
        )
        .expect("config")
    }

    #[test]
    fn normalize_task_uses_done_for_completed_tasks() {
        let task = json!({
            "id": "123",
            "content": "Ship the thing",
            "section_id": "todo-section",
            "checked": true,
            "completed_at": "2026-03-11T18:30:00Z"
        });
        let sections = HashMap::from([("todo-section".to_string(), "Todo".to_string())]);

        let issue = normalize_task(&task, &sections, None, "Done").expect("issue");

        assert_eq!(issue.identifier, "TD-123");
        assert_eq!(issue.state, "Done");
    }

    #[test]
    fn normalize_task_uses_section_for_open_tasks() {
        let task = json!({
            "id": "123",
            "content": "Ship the thing",
            "section_id": "todo-section",
            "checked": false
        });
        let sections = HashMap::from([("todo-section".to_string(), "Todo".to_string())]);

        let issue = normalize_task(&task, &sections, None, "Done").expect("issue");

        assert_eq!(issue.state, "Todo");
    }

    #[test]
    fn normalize_task_synthesizes_todoist_native_fields() {
        let task = json!({
            "id": "123",
            "content": "Ship the thing",
            "project_id": "proj-1",
            "section_id": "todo-section",
            "parent_id": "parent-1",
            "labels": ["backend"],
            "due": { "date": "2026-03-12" },
            "deadline": { "date": "2026-03-13" }
        });
        let sections = HashMap::from([("todo-section".to_string(), "Todo".to_string())]);

        let issue = normalize_task(&task, &sections, None, "Done").expect("issue");

        assert_eq!(
            issue.url.as_deref(),
            Some("https://app.todoist.com/app/task/123")
        );
        assert_eq!(issue.project_id.as_deref(), Some("proj-1"));
        assert_eq!(issue.parent_id.as_deref(), Some("parent-1"));
        assert!(issue.is_subtask);
        assert_eq!(
            issue
                .due
                .as_ref()
                .and_then(|value| value.get("date"))
                .and_then(|value| value.as_str()),
            Some("2026-03-12")
        );
        assert_eq!(
            issue
                .deadline
                .as_ref()
                .and_then(|value| value.get("date"))
                .and_then(|value| value.as_str()),
            Some("2026-03-13")
        );
    }

    #[test]
    fn task_list_request_keeps_runtime_label_scope_available() {
        let (path, query) = task_list_request(
            json!({
                "project_id": "proj-1",
                "label": "symphony-full-smoke"
            })
            .as_object()
            .expect("object"),
            None,
        )
        .expect("request");

        assert_eq!(path, "/tasks");
        assert_eq!(
            query,
            vec![
                ("project_id".to_string(), "proj-1".to_string()),
                ("label".to_string(), "symphony-full-smoke".to_string())
            ]
        );
    }

    #[test]
    fn sanitize_write_arguments_strips_tool_metadata() {
        let sanitized = sanitize_write_arguments(json!({
            "action": "move_task",
            "task_id": "123",
            "comment_id": "456",
            "section_id": "section-1",
            "content": "keep"
        }));

        assert_eq!(
            sanitized,
            json!({
                "section_id": "section-1",
                "content": "keep"
            })
        );
    }

    #[test]
    fn runtime_label_scope_is_added_for_task_writes() {
        let mut body = json!({
            "content": "Follow-up",
            "labels": ["backend"]
        })
        .as_object()
        .expect("object")
        .clone();

        enforce_runtime_label_scope(&mut body, "symphony-full-smoke");

        assert_eq!(body["labels"], json!(["backend", "symphony-full-smoke"]));
    }

    #[test]
    fn runtime_label_scope_is_not_duplicated_for_task_writes() {
        let mut body = json!({
            "content": "Follow-up",
            "labels": ["symphony-full-smoke", "backend"]
        })
        .as_object()
        .expect("object")
        .clone();

        enforce_runtime_label_scope(&mut body, "SYMPHONY-FULL-SMOKE");

        assert_eq!(body["labels"], json!(["symphony-full-smoke", "backend"]));
    }

    #[test]
    fn sanitize_comment_arguments_keeps_comment_target_ids() {
        let sanitized = sanitize_comment_arguments(json!({
            "action": "create_comment",
            "task_id": "123",
            "project_id": "proj-1",
            "comment_id": "456",
            "content": "keep"
        }));

        assert_eq!(
            sanitized,
            json!({
                "task_id": "123",
                "project_id": "proj-1",
                "content": "keep"
            })
        );
    }

    #[test]
    fn comment_target_body_requires_exactly_one_target() {
        let error = comment_target_body(json!({
            "task_id": "123",
            "project_id": "proj-1",
            "content": "keep"
        }))
        .expect_err("error");

        assert_eq!(
            error.to_string(),
            "tracker_operation_unsupported exactly one of `task_id` or `project_id` is required"
        );
    }

    #[test]
    fn move_task_body_keeps_only_move_fields() {
        let body = move_task_body(json!({
            "action": "move_task",
            "task_id": "123",
            "section_id": "section-1",
            "content": "ignore me"
        }))
        .expect("body");

        assert_eq!(body, json!({ "section_id": "section-1" }));
    }

    #[test]
    fn create_reminder_body_renames_task_id_to_item_id() {
        let body = create_reminder_body(json!({
            "action": "create_reminder",
            "task_id": "123",
            "type": "relative",
            "minute_offset": 30
        }))
        .expect("body");

        assert_eq!(
            body,
            json!({
                "item_id": "123",
                "type": "relative",
                "minute_offset": 30
            })
        );
    }

    #[test]
    fn update_reminder_body_requires_mutation_fields() {
        let error = update_reminder_body(
            "rem-1",
            json!({
                "action": "update_reminder"
            }),
        )
        .expect_err("error");

        assert_eq!(
            error.to_string(),
            "tracker_operation_unsupported at least one reminder field is required for update_reminder"
        );
    }

    #[test]
    fn csv_query_value_supports_arrays() {
        assert_eq!(
            csv_query_value(Some(&json!(["task-1", "task-2"]))),
            Some("task-1,task-2".to_string())
        );
    }

    #[test]
    fn extend_repeated_query_values_supports_arrays() {
        let mut query = Vec::new();
        extend_repeated_query_values(
            &mut query,
            "object_event_types",
            Some(&json!(["item:added", "item:completed"])),
        );

        assert_eq!(
            query,
            vec![
                ("object_event_types".to_string(), "item:added".to_string()),
                (
                    "object_event_types".to_string(),
                    "item:completed".to_string()
                )
            ]
        );
    }

    #[test]
    fn task_list_request_uses_filter_endpoint_when_filter_is_present() {
        let (path, query) = task_list_request(
            json!({
                "filter": "subtask",
                "lang": "en"
            })
            .as_object()
            .expect("object"),
            Some("proj-1".to_string()),
        )
        .expect("request");

        assert_eq!(path, "/tasks/filter");
        assert_eq!(
            query,
            vec![
                ("filter".to_string(), "subtask".to_string()),
                ("lang".to_string(), "en".to_string())
            ]
        );
    }

    #[test]
    fn task_list_request_defaults_to_configured_project_scope() {
        let (path, query) = task_list_request(
            json!({}).as_object().expect("object"),
            Some("proj-1".to_string()),
        )
        .expect("request");

        assert_eq!(path, "/tasks");
        assert_eq!(
            query,
            vec![("project_id".to_string(), "proj-1".to_string())]
        );
    }

    #[test]
    fn task_list_request_requires_scope_when_no_default_project_exists() {
        let error = task_list_request(json!({}).as_object().expect("object"), None)
            .expect_err("missing project");

        assert_eq!(error.to_string(), "missing_tracker_project_id");
    }

    #[tokio::test]
    async fn startup_validation_requires_comment_capability() {
        let server = spawn_mock_todoist(MockTodoistState {
            project: json!({
                "id": "proj",
                "is_shared": false,
                "can_assign_tasks": false
            }),
            sections: vec![
                json!({"id": "sec-todo", "project_id": "proj", "name": "Todo"}),
                json!({"id": "sec-progress", "project_id": "proj", "name": "In Progress"}),
            ],
            collaborators: Vec::new(),
            current_user: json!({"id": "user-1"}),
            plan_limits: json!({"comments": false}),
        })
        .await;

        let tracker = TodoistTracker::new(tracker_config(&server.base_url, None));
        let error = tracker
            .validate_startup()
            .await
            .expect_err("comments required");

        assert_eq!(error.to_string(), "todoist_comments_unavailable");
    }

    #[tokio::test]
    async fn startup_validation_rejects_assignee_outside_shared_project_collaborators() {
        let server = spawn_mock_todoist(MockTodoistState {
            project: json!({
                "id": "proj",
                "is_shared": true,
                "can_assign_tasks": true
            }),
            sections: vec![
                json!({"id": "sec-todo", "project_id": "proj", "name": "Todo"}),
                json!({"id": "sec-progress", "project_id": "proj", "name": "In Progress"}),
            ],
            collaborators: vec![json!({"id": "user-1", "project_id": "proj"})],
            current_user: json!({"id": "user-1"}),
            plan_limits: json!({"comments": true}),
        })
        .await;

        let tracker = TodoistTracker::new(tracker_config(&server.base_url, Some("user-2")));
        let error = tracker
            .validate_startup()
            .await
            .expect_err("shared project collaborator validation");

        assert_eq!(
            error.to_string(),
            "todoist_assignee_not_resolvable assignee=user-2 project_id=proj"
        );
    }

    #[tokio::test]
    async fn startup_validation_accepts_me_for_assignable_project() {
        let server = spawn_mock_todoist(MockTodoistState {
            project: json!({
                "id": "proj",
                "is_shared": true,
                "can_assign_tasks": true
            }),
            sections: vec![
                json!({"id": "sec-todo", "project_id": "proj", "name": "Todo"}),
                json!({"id": "sec-progress", "project_id": "proj", "name": "In Progress"}),
            ],
            collaborators: vec![json!({"id": "user-1", "project_id": "proj"})],
            current_user: json!({"id": "user-1"}),
            plan_limits: json!({"comments": true}),
        })
        .await;

        let tracker = TodoistTracker::new(tracker_config(&server.base_url, Some("me")));

        tracker.validate_startup().await.expect("startup valid");
    }

    #[tokio::test]
    async fn list_projects_retries_rate_limited_reads() {
        async fn list_projects(State(attempts): State<Arc<AtomicUsize>>) -> impl IntoResponse {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                (
                    HttpStatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error_extra": {"retry_after": 0}})),
                )
                    .into_response()
            } else {
                (
                    HttpStatusCode::OK,
                    Json(json!({"results": [{"id": "proj"}], "next_cursor": null})),
                )
                    .into_response()
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move {
            let app = Router::new()
                .route("/projects", get(list_projects))
                .with_state(attempts.clone());
            let _ = axum::serve(listener, app).await;
        });

        let tracker = TodoistTracker::new(tracker_config(&format!("http://{address}"), None));
        let projects = tracker
            .list_projects(json!({}))
            .await
            .expect("projects after retry");
        join.abort();

        assert_eq!(projects["results"][0]["id"], "proj");
    }

    #[tokio::test]
    async fn startup_validation_tolerates_rate_limited_comment_plan_probe() {
        async fn get_project() -> impl IntoResponse {
            Json(json!({
                "id": "proj",
                "is_shared": false,
                "can_assign_tasks": false
            }))
        }

        async fn list_sections() -> impl IntoResponse {
            Json(json!({
                "results": [
                    {"id": "sec-todo", "project_id": "proj", "name": "Todo"},
                    {"id": "sec-progress", "project_id": "proj", "name": "In Progress"}
                ],
                "next_cursor": null
            }))
        }

        async fn sync() -> impl IntoResponse {
            (
                HttpStatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error_extra": {"retry_after": 0}})),
            )
                .into_response()
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move {
            let app = Router::new()
                .route("/projects/{project_id}", get(get_project))
                .route("/sections", get(list_sections))
                .route("/sync", post(sync));
            let _ = axum::serve(listener, app).await;
        });

        let tracker = TodoistTracker::new(tracker_config(&format!("http://{address}"), None));
        tracker
            .validate_startup()
            .await
            .expect("startup should tolerate rate-limited comment plan probe");
        join.abort();
    }

    #[tokio::test]
    async fn list_comments_tolerates_rate_limited_comment_plan_probe() {
        async fn sync() -> impl IntoResponse {
            (
                HttpStatusCode::TOO_MANY_REQUESTS,
                Json(json!({"error_extra": {"retry_after": 0}})),
            )
                .into_response()
        }

        async fn list_comments() -> impl IntoResponse {
            Json(json!({
                "results": [
                    {
                        "id": "comment-1",
                        "item_id": "task-1",
                        "content": "## Codex Workpad\n\n<!-- symphony:workpad -->"
                    }
                ],
                "next_cursor": null
            }))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move {
            let app = Router::new()
                .route("/sync", post(sync))
                .route("/comments", get(list_comments));
            let _ = axum::serve(listener, app).await;
        });

        let tracker = TodoistTracker::new(tracker_config(&format!("http://{address}"), None));
        let comments = tracker
            .list_comments(json!({"task_id": "task-1"}))
            .await
            .expect("comments after soft plan-probe failure");
        join.abort();

        assert_eq!(comments["results"][0]["id"], "comment-1");
    }

    #[tokio::test]
    async fn create_task_retries_rate_limited_writes_with_stable_request_id() {
        async fn create_task(
            State(state): State<(Arc<AtomicUsize>, Arc<Mutex<Vec<Option<String>>>>)>,
            headers: HeaderMap,
        ) -> impl IntoResponse {
            let (attempts, request_ids) = state;
            request_ids.lock().expect("request ids").push(
                headers
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned),
            );
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                (
                    HttpStatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error_extra": {"retry_after": 0}})),
                )
                    .into_response()
            } else {
                (HttpStatusCode::OK, Json(json!({"id": "task-1"}))).into_response()
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let request_ids = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let state = (attempts.clone(), request_ids.clone());
        let join = tokio::spawn(async move {
            let app = Router::new()
                .route("/tasks", post(create_task))
                .with_state(state);
            let _ = axum::serve(listener, app).await;
        });

        let tracker = TodoistTracker::new(tracker_config(&format!("http://{address}"), None));
        let task = tracker
            .create_task(json!({
                "content": "Ship retry handling",
                "project_id": "proj",
                "section_id": "sec-todo"
            }))
            .await
            .expect("task after retry");
        join.abort();

        let request_ids = request_ids.lock().expect("request ids");
        assert_eq!(task["id"], "task-1");
        assert_eq!(request_ids.len(), 2);
        assert!(
            request_ids[0]
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        );
        assert_eq!(request_ids[0], request_ids[1]);
    }

    #[test]
    fn retry_after_hint_uses_longer_body_hint_when_header_is_shorter() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "35".parse().expect("retry-after header"));

        assert_eq!(
            retry_after_hint(&headers, r#"{"error_extra":{"retry_after":129}}"#),
            Some(129)
        );
    }

    #[test]
    fn retry_delay_for_attempt_retries_transient_server_errors() {
        let error = TrackerError::TodoistApiStatus {
            status: 502,
            body: String::new(),
        };

        assert_eq!(
            retry_delay_for_attempt(&error, 0, 0, DEFAULT_TODOIST_RETRY_POLICY),
            Some(2)
        );
        assert_eq!(
            retry_delay_for_attempt(&error, 1, 2, DEFAULT_TODOIST_RETRY_POLICY),
            Some(4)
        );
    }

    #[test]
    fn retry_delay_for_attempt_retries_transport_errors() {
        let error = TrackerError::TodoistApiRequest("connection reset".to_string());

        assert_eq!(
            retry_delay_for_attempt(&error, 0, 0, DEFAULT_TODOIST_RETRY_POLICY),
            Some(2)
        );
    }

    #[test]
    fn retry_delay_for_attempt_honors_total_wait_budget() {
        let error = TrackerError::TodoistRateLimited {
            retry_after: Some(200),
        };

        assert_eq!(
            retry_delay_for_attempt(&error, 0, 0, DEFAULT_TODOIST_RETRY_POLICY),
            Some(60)
        );
        assert_eq!(
            retry_delay_for_attempt(&error, 1, 60, DEFAULT_TODOIST_RETRY_POLICY),
            Some(60)
        );
        assert_eq!(
            retry_delay_for_attempt(&error, 3, 240, DEFAULT_TODOIST_RETRY_POLICY),
            Some(60)
        );
        assert_eq!(
            retry_delay_for_attempt(&error, 4, 240, DEFAULT_TODOIST_RETRY_POLICY),
            None
        );
        assert_eq!(
            retry_delay_for_attempt(&error, 3, 241, DEFAULT_TODOIST_RETRY_POLICY),
            None
        );
    }

    #[test]
    fn retry_delay_for_attempt_caps_large_retry_after_values() {
        let error = TrackerError::TodoistRateLimited {
            retry_after: Some(1_027),
        };

        assert_eq!(
            retry_delay_for_attempt(&error, 0, 0, DEFAULT_TODOIST_RETRY_POLICY),
            Some(60)
        );
    }

    #[test]
    fn comment_update_retry_policy_fails_fast_on_long_rate_limits() {
        let error = TrackerError::TodoistRateLimited {
            retry_after: Some(1_027),
        };
        let policy = retry_policy_for_request(&Method::POST, "/comments/comment-1");

        assert_eq!(policy.max_retries, TODOIST_COMMENT_UPDATE_MAX_RETRIES);
        assert_eq!(
            retry_delay_for_attempt(&error, 0, 0, policy),
            Some(TODOIST_COMMENT_UPDATE_MAX_DELAY_SECS)
        );
        assert_eq!(
            retry_delay_for_attempt(&error, 1, TODOIST_COMMENT_UPDATE_MAX_DELAY_SECS, policy),
            Some(TODOIST_COMMENT_UPDATE_MAX_DELAY_SECS)
        );
        assert_eq!(
            retry_delay_for_attempt(
                &error,
                TODOIST_COMMENT_UPDATE_MAX_RETRIES,
                TODOIST_COMMENT_UPDATE_MAX_TOTAL_WAIT_SECS,
                policy
            ),
            None
        );
    }

    #[test]
    fn user_plan_limits_probe_retry_policy_fails_fast_on_long_rate_limits() {
        let error = TrackerError::TodoistRateLimited {
            retry_after: Some(1_027),
        };
        let policy = retry_policy_for_sync_form(&[
            ("sync_token", "*"),
            ("resource_types", "[\"user_plan_limits\"]"),
        ]);

        assert_eq!(policy.max_retries, TODOIST_PLAN_LIMITS_MAX_RETRIES);
        assert_eq!(
            retry_delay_for_attempt(&error, 0, 0, policy),
            Some(TODOIST_PLAN_LIMITS_MAX_DELAY_SECS)
        );
        assert_eq!(
            retry_delay_for_attempt(
                &error,
                TODOIST_PLAN_LIMITS_MAX_RETRIES,
                TODOIST_PLAN_LIMITS_MAX_TOTAL_WAIT_SECS,
                policy
            ),
            None
        );
    }
}
