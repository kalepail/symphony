use std::collections::{BTreeSet, HashMap};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};

use crate::{
    config::ServiceConfig,
    issue::{Issue, normalize_state_name},
    tracker::{TrackerClient, TrackerError},
};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_PAGE_SIZE: usize = 200;
const DEFAULT_TOOL_PAGE_SIZE: usize = 50;
const COMMENT_SIZE_LIMIT: usize = 15_000;
const REFRESH_CONCURRENCY: usize = 10;

#[derive(Clone)]
pub struct TodoistTracker {
    client: Client,
    config: ServiceConfig,
}

#[derive(Clone)]
struct AssigneeFilter {
    match_value: String,
}

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
        let mut request = self
            .client
            .request(method, &url)
            .bearer_auth(token)
            .header("Accept", "application/json");

        if let Some(query) = query {
            request = request.query(query);
        }

        if let Some(body) = body {
            request = request.json(&body);
        }

        let response = request
            .send()
            .await
            .map_err(|error| TrackerError::TodoistApiRequest(error.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| TrackerError::TodoistApiRequest(error.to_string()))?;

        if !status.is_success() {
            return Err(map_todoist_status(status, &text));
        }

        if text.trim().is_empty() {
            return Ok(Value::Null);
        }

        serde_json::from_str(&text).map_err(|_| TrackerError::TodoistUnknownPayload)
    }

    async fn sync_json(&self, form: &[(&str, &str)]) -> Result<Value, TrackerError> {
        let token = self.token()?;
        let url = format!("{}/sync", self.base_url());
        let response = self
            .client
            .post(&url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .form(form)
            .send()
            .await
            .map_err(|error| TrackerError::TodoistApiRequest(error.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| TrackerError::TodoistApiRequest(error.to_string()))?;

        if !status.is_success() {
            return Err(map_todoist_status(status, &text));
        }

        serde_json::from_str(&text).map_err(|_| TrackerError::TodoistUnknownPayload)
    }

    async fn get_project(&self, project_id: &str) -> Result<Value, TrackerError> {
        self.request_json(Method::GET, &format!("/projects/{project_id}"), None, None)
            .await
            .map_err(|error| match error {
                TrackerError::TodoistApiStatus { status, .. } if status == 404 => {
                    TrackerError::TodoistProjectNotFound(project_id.to_string())
                }
                other => other,
            })
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
        project_id: &str,
        cursor: Option<&str>,
        limit: usize,
        extra: &[(String, String)],
    ) -> Result<Value, TrackerError> {
        let mut query = vec![
            ("project_id".to_string(), project_id.to_string()),
            ("limit".to_string(), limit.to_string()),
        ];
        if let Some(cursor) = cursor {
            query.push(("cursor".to_string(), cursor.to_string()));
        }
        query.extend(extra.iter().cloned());
        self.request_json(Method::GET, "/tasks", Some(&query), None)
            .await
    }

    async fn get_comments_page(&self, query: &[(String, String)]) -> Result<Value, TrackerError> {
        self.ensure_comments_available().await?;
        self.request_json(Method::GET, "/comments", Some(query), None)
            .await
    }

    async fn section_map(&self) -> Result<HashMap<String, String>, TrackerError> {
        let project_id = self.project_id()?;
        self.get_project(&project_id).await?;
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

    async fn resolve_assignee_filter(&self) -> Result<Option<AssigneeFilter>, TrackerError> {
        let assignee = match self.config.tracker.assignee.clone() {
            Some(value) if !value.trim().is_empty() => value,
            _ => return Ok(None),
        };

        if assignee.trim() == "me" {
            let user = self.get_current_user().await?;
            let id = json_id(user.get("id")).ok_or(TrackerError::MissingTodoistCurrentUser)?;
            return Ok(Some(AssigneeFilter { match_value: id }));
        }

        Ok(Some(AssigneeFilter {
            match_value: assignee.trim().to_string(),
        }))
    }

    async fn fetch_all_project_tasks(&self) -> Result<Vec<Value>, TrackerError> {
        let project_id = self.project_id()?;
        let mut tasks = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self
                .get_tasks_page(&project_id, cursor.as_deref(), MAX_PAGE_SIZE, &[])
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

    async fn ensure_comments_available(&self) -> Result<(), TrackerError> {
        let limits = self
            .sync_json(&[
                ("sync_token", "*"),
                ("resource_types", "[\"user_plan_limits\"]"),
            ])
            .await?;
        let comments_available = limits
            .get("user_plan_limits")
            .and_then(|value| value.get("current"))
            .and_then(|value| value.get("comments"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if comments_available {
            Ok(())
        } else {
            Err(TrackerError::TodoistCommentsUnavailable)
        }
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
        let task_id = map.get("task_id").and_then(json_id_from_value);
        let project_id = map.get("project_id").and_then(json_id_from_value);
        if task_id.is_some() == project_id.is_some() {
            return Err(TrackerError::TrackerOperationUnsupported(
                "exactly one of `task_id` or `project_id` is required".to_string(),
            ));
        }

        let mut query = Vec::new();
        if let Some(task_id) = task_id {
            query.push(("task_id".to_string(), task_id));
        }
        if let Some(project_id) = project_id {
            query.push(("project_id".to_string(), project_id));
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
            .filter(|issue| active_states.contains(&issue.state_key()))
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

    async fn raw_graphql(&self, _query: &str, _variables: Value) -> Result<Value, TrackerError> {
        Err(TrackerError::TrackerOperationUnsupported(
            "raw_graphql".to_string(),
        ))
    }

    async fn get_current_user(&self) -> Result<Value, TrackerError> {
        self.request_json(Method::GET, "/user", None, None).await
    }

    async fn list_tasks(&self, arguments: Value) -> Result<Value, TrackerError> {
        let map = arguments.as_object().cloned().unwrap_or_default();
        let project_id = map
            .get("project_id")
            .and_then(json_id_from_value)
            .or_else(|| self.config.tracker.project_id.clone())
            .ok_or(TrackerError::MissingTrackerProjectId)?;
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
        let mut extra = Vec::new();
        if let Some(section_id) = map.get("section_id").and_then(json_id_from_value) {
            extra.push(("section_id".to_string(), section_id));
        }
        self.get_tasks_page(&project_id, cursor, limit, &extra)
            .await
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

    async fn list_comments(&self, arguments: Value) -> Result<Value, TrackerError> {
        let query = self.comments_query(&arguments).await?;
        self.get_comments_page(&query).await
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
        let body = merge_project_default(
            sanitize_comment_arguments(arguments),
            json!({ "content": content }),
        );
        self.request_json(Method::POST, "/comments", None, Some(body))
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
        let body = sanitize_write_arguments(arguments);
        self.request_json(Method::POST, &format!("/tasks/{task_id}"), None, Some(body))
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
        let mut body = merge_project_default(
            sanitize_write_arguments(arguments),
            json!({ "content": content }),
        );
        if body.get("project_id").is_none()
            && let Some(project_id) = self.config.tracker.project_id.clone()
        {
            body["project_id"] = Value::String(project_id);
        }
        self.request_json(Method::POST, "/tasks", None, Some(body))
            .await
    }
}

fn normalize_task(
    task: &Value,
    sections_by_id: &HashMap<String, String>,
    assignee_filter: Option<&AssigneeFilter>,
    completed_state: &str,
) -> Option<Issue> {
    let id = json_id(task.get("id"))?;
    let identifier = format!("TD-{id}");
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
        branch_name: None,
        url: task
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
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
        blocked_by: Vec::new(),
        created_at: parse_datetime(task.get("created_at").or_else(|| task.get("added_at"))),
        updated_at: parse_datetime(task.get("updated_at")),
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

fn merge_project_default(arguments: Value, defaults: Value) -> Value {
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
    for key in ["action", "task_id", "comment_id"] {
        map.remove(key);
    }
    Value::Object(map)
}

fn sanitize_comment_arguments(arguments: Value) -> Value {
    let mut map = arguments.as_object().cloned().unwrap_or_default();
    for key in ["action", "comment_id"] {
        map.remove(key);
    }
    Value::Object(map)
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

fn map_todoist_status(status: StatusCode, text: &str) -> TrackerError {
    let retry_after = serde_json::from_str::<Value>(text).ok().and_then(|body| {
        body.get("error_extra")
            .and_then(|value| value.get("retry_after"))
            .and_then(Value::as_u64)
    });
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
    use std::collections::HashMap;

    use serde_json::json;

    use super::{
        move_task_body, normalize_task, sanitize_comment_arguments, sanitize_write_arguments,
    };

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
}
