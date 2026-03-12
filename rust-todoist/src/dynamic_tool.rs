use regex::Regex;
use reqwest::Method;
use serde_json::{Value, json};
use std::{process::Command as StdCommand, sync::OnceLock};
use tokio::process::Command;
use tracing::info;

use crate::{
    config::ServiceConfig,
    issue::normalize_state_name,
    runtime_env,
    tracker::{TrackerClient, TrackerError},
};

pub const GITHUB_API_TOOL: &str = "github_api";
pub const TODOIST_TOOL: &str = "todoist";
const GITHUB_API_BASE_URL_ENV: &str = "SYMPHONY_GITHUB_API_URL";
const GITHUB_API_DEFAULT_BASE_URL: &str = "https://api.github.com";
const WORKPAD_HEADER: &str = "## Codex Workpad";
const WORKPAD_MARKER: &str = "<!-- symphony:workpad -->";
const GITHUB_API_TOOL_DESCRIPTION: &str = concat!(
    "Execute a GitHub REST API request using host-side auth from Symphony's runtime.\n",
    "Use this when `gh` transport is flaky inside Codex sessions but the host can still reach GitHub.\n",
    "Create pull requests with POST `/repos/{owner}/{repo}/pulls` and JSON body fields `title`, `head`, `base`, `body`.\n",
    "Add labels with POST `/repos/{owner}/{repo}/issues/{number}/labels` and body `{ \"labels\": [\"symphony\"] }`.\n",
    "Read pull requests with GET `/repos/{owner}/{repo}/pulls/{number}`.\n",
    "Read CI checks with GET `/repos/{owner}/{repo}/commits/{sha}/check-runs`."
);
const TODOIST_TOOL_DESCRIPTION: &str = concat!(
    "Execute a structured Todoist API action using Symphony's configured tracker auth.\n",
    "Supported actions: list_projects, get_project, get_current_user, list_collaborators, ",
    "list_tasks, get_task, list_sections, get_section, list_labels, get_comment, list_comments, ",
    "create_project_comment, update_comment, delete_comment, get_workpad, upsert_workpad, delete_workpad, ",
    "update_task, move_task, close_task, reopen_task, create_task, list_reminders, list_activities, ",
    "create_reminder, update_reminder, delete_reminder.\n",
    "Use this tool instead of raw HTTP. Keep each call narrow and specific.\n",
    "Task comments require `task_id` only; project comments require `project_id` only. ",
    "Todoist comment responses identify task comments with `item_id`.\n",
    "Use `create_project_comment` for project comments and `upsert_workpad` for the single persistent task workpad.\n",
    "When `tracker.label` is configured, `create_task` automatically inherits that label. ",
    "Top-level `create_task` calls also default into the project's `Todo` section when no section is supplied.\n",
    "`close_task` is guarded: the task must already be in `Merging`, the workpad must contain a linked GitHub PR URL, ",
    "and Symphony verifies that the PR is actually merged before completing the task.\n",
    "For Symphony's persistent task workpad, prefer `get_workpad`, `upsert_workpad`, and `delete_workpad` ",
    "instead of raw comment mutations."
);

pub fn tool_specs(config: &ServiceConfig) -> Vec<Value> {
    let mut specs = Vec::new();

    if matches!(config.tracker.kind.as_deref(), Some("todoist" | "memory")) {
        specs.push(json!({
            "name": TODOIST_TOOL,
            "description": TODOIST_TOOL_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "additionalProperties": true,
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "Todoist action to execute."
                    },
                    "content": {
                        "type": ["string", "null"],
                        "description": "Primary content for create_task, create_project_comment, update_comment, or upsert_workpad."
                    },
                    "description": {
                        "type": ["string", "null"],
                        "description": "Todoist task description for create_task or update_task."
                    },
                    "labels": {
                        "description": "Todoist label names for create_task or update_task."
                    },
                    "assignee_id": {
                        "type": ["string", "number", "null"],
                        "description": "Todoist assignee id for create_task or update_task."
                    },
                    "priority": {
                        "type": ["integer", "null"],
                        "description": "Todoist task priority from 1 (normal) to 4 (urgent)."
                    },
                    "due": {
                        "description": "Todoist due object for create_task, update_task, create_reminder, or update_reminder."
                    },
                    "deadline": {
                        "description": "Todoist deadline object for create_task or update_task."
                    },
                    "cursor": {
                        "type": ["string", "null"],
                        "description": "Cursor for paginated list actions."
                    },
                    "limit": {
                        "type": ["integer", "null"],
                        "description": "Requested page size for paginated list actions."
                    },
                    "task_id": {
                        "type": ["string", "number", "null"],
                        "description": "Task identifier for get/update/move/close/reopen task actions."
                    },
                    "section_id": {
                        "type": ["string", "number", "null"],
                        "description": "Section identifier for get/update section-based actions."
                    },
                    "parent_id": {
                        "type": ["string", "number", "null"],
                        "description": "Parent task identifier for subtask listing, creation, or task reparenting."
                    },
                    "label": {
                        "type": ["string", "null"],
                        "description": "Todoist label name for list_tasks filtering."
                    },
                    "ids": {
                        "description": "Comma-separated task ids or an array of task ids for list_tasks."
                    },
                    "filter": {
                        "type": ["string", "null"],
                        "description": "Todoist filter query for list_tasks. When provided, Symphony uses `/tasks/filter`."
                    },
                    "lang": {
                        "type": ["string", "null"],
                        "description": "IETF language tag for Todoist filter queries."
                    },
                    "comment_id": {
                        "type": ["string", "number", "null"],
                        "description": "Comment identifier for get_comment, update_comment, or delete_comment."
                    },
                    "project_id": {
                        "type": ["string", "number", "null"],
                        "description": "Optional project identifier; defaults to tracker.project_id where applicable. Do not send this for task comment actions."
                    },
                    "attachment": {
                        "description": "Optional Todoist comment attachment object."
                    },
                    "uids_to_notify": {
                        "description": "Optional list of Todoist user ids to notify for comment creation."
                    },
                    "reminder_id": {
                        "type": ["string", "number", "null"],
                        "description": "Reminder identifier for update_reminder and delete_reminder."
                    },
                    "object_type": {
                        "type": ["string", "null"],
                        "description": "Activity-log object type such as `project`, `item`, or `note`."
                    },
                    "object_id": {
                        "type": ["string", "number", "null"],
                        "description": "Activity-log object identifier, used with object_type."
                    },
                    "event_type": {
                        "type": ["string", "null"],
                        "description": "Todoist activity event type such as `added`, `updated`, `completed`, or `moved`."
                    },
                    "object_event_types": {
                        "description": "Single value or array using Todoist `object_type:event_type` syntax, for example `item:completed`."
                    },
                    "parent_project_id": {
                        "type": ["string", "number", "null"],
                        "description": "Activity-log parent project identifier."
                    },
                    "parent_item_id": {
                        "type": ["string", "number", "null"],
                        "description": "Activity-log parent task identifier."
                    },
                    "date_from": {
                        "type": ["string", "null"],
                        "description": "RFC3339 lower bound for list_activities."
                    },
                    "date_to": {
                        "type": ["string", "null"],
                        "description": "RFC3339 exclusive upper bound for list_activities."
                    },
                    "initiator_id_null": {
                        "type": ["boolean", "null"],
                        "description": "Filter activities by whether the initiator is absent (`true`) or present (`false`)."
                    }
                }
            }
        }));
    }

    if github_api_available() {
        specs.push(json!({
            "name": GITHUB_API_TOOL,
            "description": GITHUB_API_TOOL_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["method", "path"],
                "properties": {
                    "method": {
                        "type": "string",
                        "description": "HTTP method to send to the GitHub REST API (GET, POST, PATCH, PUT, DELETE)."
                    },
                    "path": {
                        "type": "string",
                        "description": "API path beginning with `/`, for example `/repos/owner/repo/pulls`."
                    },
                    "body": {
                        "description": "Optional JSON body for POST/PATCH/PUT requests."
                    }
                }
            }
        }));
    }

    specs
}

pub async fn execute(
    config: &ServiceConfig,
    tracker: &dyn TrackerClient,
    tool: &str,
    arguments: Value,
) -> Value {
    match tool {
        TODOIST_TOOL => {
            if !matches!(config.tracker.kind.as_deref(), Some("todoist" | "memory")) {
                return failure_payload(json!({
                    "error": {
                        "message": "Symphony is not configured with tracker.kind=todoist or tracker.kind=memory for this session."
                    }
                }));
            }

            match execute_todoist(config, tracker, arguments).await {
                Ok(body) => success_payload(body),
                Err(error) => failure_payload(tool_error_payload(error)),
            }
        }
        GITHUB_API_TOOL => match normalize_github_arguments(arguments) {
            Ok((method, path, body)) => match execute_github_api(method, path, body).await {
                Ok(body) => success_payload(body),
                Err(error) => failure_payload(error),
            },
            Err(error) => failure_payload(error),
        },
        _ => failure_payload(json!({
            "error": {
                "message": format!("Unsupported dynamic tool: {tool}."),
                "supportedTools": [TODOIST_TOOL, GITHUB_API_TOOL]
            }
        })),
    }
}

async fn execute_todoist(
    config: &ServiceConfig,
    tracker: &dyn TrackerClient,
    arguments: Value,
) -> Result<Value, TrackerError> {
    let args = arguments.as_object().cloned().ok_or_else(|| {
        TrackerError::TrackerOperationUnsupported(
            "`todoist` expects an object with an `action` field".to_string(),
        )
    })?;
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported("`todoist.action` is required".to_string())
        })?;
    info!(tool = "todoist", action);

    match action {
        "list_projects" => tracker.list_projects(Value::Object(args)).await,
        "get_project" => {
            tracker
                .get_project(&required_id(&args, "project_id")?)
                .await
        }
        "get_current_user" => tracker.get_current_user().await,
        "list_collaborators" => tracker.list_collaborators(Value::Object(args)).await,
        "list_tasks" => tracker.list_tasks(Value::Object(args)).await,
        "get_task" => tracker.get_task(&required_id(&args, "task_id")?).await,
        "list_sections" => tracker.list_sections(Value::Object(args)).await,
        "get_section" => {
            tracker
                .get_section(&required_id(&args, "section_id")?)
                .await
        }
        "list_labels" => tracker.list_labels(Value::Object(args)).await,
        "get_comment" => {
            tracker
                .get_comment(&required_id(&args, "comment_id")?)
                .await
        }
        "delete_comment" => {
            tracker
                .delete_comment(&required_id(&args, "comment_id")?)
                .await
        }
        "list_comments" => tracker.list_comments(Value::Object(args)).await,
        "get_workpad" => get_workpad(tracker, &required_id(&args, "task_id")?).await,
        "upsert_workpad" => {
            let task_id = required_id(&args, "task_id")?;
            let content = required_string(&args, "content")?;
            upsert_workpad(tracker, &task_id, &content).await
        }
        "delete_workpad" => delete_workpad(tracker, &required_id(&args, "task_id")?).await,
        "create_project_comment" => create_project_comment(tracker, args).await,
        "create_comment" => reject_generic_comment_creation(),
        "update_comment" => {
            let comment_id = required_id(&args, "comment_id")?;
            tracker
                .update_comment(&comment_id, Value::Object(args))
                .await
        }
        "update_task" => {
            let task_id = required_id(&args, "task_id")?;
            tracker.update_task(&task_id, Value::Object(args)).await
        }
        "move_task" => {
            let task_id = required_id(&args, "task_id")?;
            tracker.move_task(&task_id, Value::Object(args)).await
        }
        "close_task" => close_task_guarded(config, tracker, &required_id(&args, "task_id")?).await,
        "reopen_task" => tracker.reopen_task(&required_id(&args, "task_id")?).await,
        "create_task" => tracker.create_task(Value::Object(args)).await,
        "list_reminders" => tracker.list_reminders(Value::Object(args)).await,
        "list_activities" => tracker.list_activities(Value::Object(args)).await,
        "create_reminder" => tracker.create_reminder(Value::Object(args)).await,
        "update_reminder" => {
            let reminder_id = required_id(&args, "reminder_id")?;
            tracker
                .update_reminder(&reminder_id, Value::Object(args))
                .await
        }
        "delete_reminder" => {
            tracker
                .delete_reminder(&required_id(&args, "reminder_id")?)
                .await
        }
        other => Err(TrackerError::TrackerOperationUnsupported(format!(
            "unsupported todoist action `{other}`"
        ))),
    }
}

fn required_id(map: &serde_json::Map<String, Value>, key: &str) -> Result<String, TrackerError> {
    match map.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        Some(Value::Number(value)) => Ok(value.to_string()),
        _ => Err(TrackerError::TrackerOperationUnsupported(format!(
            "`todoist.{key}` is required"
        ))),
    }
}

fn required_string(
    map: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, TrackerError> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(format!("`todoist.{key}` is required"))
        })
}

fn reject_generic_comment_creation() -> Result<Value, TrackerError> {
    Err(TrackerError::TrackerOperationUnsupported(
        "Use `upsert_workpad` for task-scoped Symphony comments or `create_project_comment` for project comments.".to_string(),
    ))
}

async fn create_project_comment(
    tracker: &dyn TrackerClient,
    mut args: serde_json::Map<String, Value>,
) -> Result<Value, TrackerError> {
    let project_id = required_id(&args, "project_id")?;
    args.remove("task_id");
    args.insert("project_id".to_string(), Value::String(project_id));
    tracker.create_comment(Value::Object(args)).await
}

async fn get_workpad(tracker: &dyn TrackerClient, task_id: &str) -> Result<Value, TrackerError> {
    let comment = find_workpad_comment(tracker, task_id).await?;
    Ok(json!({
        "task_id": task_id,
        "found": comment.is_some(),
        "comment_id": comment.as_ref().and_then(|value| value.get("id")).cloned().unwrap_or(Value::Null),
        "comment": comment
    }))
}

async fn upsert_workpad(
    tracker: &dyn TrackerClient,
    task_id: &str,
    content: &str,
) -> Result<Value, TrackerError> {
    let content = normalize_workpad_content(content);
    if let Some(existing) = find_workpad_comment(tracker, task_id).await? {
        let comment_id = existing.get("id").and_then(Value::as_str).ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(
                "existing workpad comment is missing an `id`".to_string(),
            )
        })?;
        let comment = tracker
            .update_comment(comment_id, json!({ "content": content }))
            .await?;
        return Ok(json!({
            "task_id": task_id,
            "created": false,
            "comment_id": comment.get("id").cloned().unwrap_or(Value::Null),
            "comment": comment
        }));
    }

    let comment = tracker
        .create_comment(json!({ "task_id": task_id, "content": content }))
        .await?;
    Ok(json!({
        "task_id": task_id,
        "created": true,
        "comment_id": comment.get("id").cloned().unwrap_or(Value::Null),
        "comment": comment
    }))
}

async fn delete_workpad(tracker: &dyn TrackerClient, task_id: &str) -> Result<Value, TrackerError> {
    let Some(comment) = find_workpad_comment(tracker, task_id).await? else {
        return Ok(json!({
            "task_id": task_id,
            "deleted": false,
            "comment_id": Value::Null
        }));
    };
    let comment_id = comment.get("id").and_then(Value::as_str).ok_or_else(|| {
        TrackerError::TrackerOperationUnsupported(
            "existing workpad comment is missing an `id`".to_string(),
        )
    })?;
    tracker.delete_comment(comment_id).await?;
    Ok(json!({
        "task_id": task_id,
        "deleted": true,
        "comment_id": comment_id
    }))
}

async fn close_task_guarded(
    _config: &ServiceConfig,
    tracker: &dyn TrackerClient,
    task_id: &str,
) -> Result<Value, TrackerError> {
    let issue = tracker
        .fetch_issue_states_by_ids(&[task_id.to_string()])
        .await?
        .into_iter()
        .find(|issue| issue.id == task_id)
        .ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(format!(
                "`close_task` could not load Todoist task `{task_id}`"
            ))
        })?;

    if normalize_state_name(&issue.state) != "merging" {
        return Err(TrackerError::TrackerOperationUnsupported(format!(
            "`close_task` is only allowed from `Merging`. Current state is `{}`.",
            issue.state
        )));
    }

    let workpad = find_workpad_comment(tracker, task_id)
        .await?
        .ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(
                "`close_task` requires the persistent task workpad comment.".to_string(),
            )
        })?;
    let workpad_content = workpad
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(
                "`close_task` requires readable workpad content.".to_string(),
            )
        })?;
    let pr_url = extract_github_pr_url(workpad_content).ok_or_else(|| {
        TrackerError::TrackerOperationUnsupported(
            "`close_task` requires a linked GitHub PR URL in the workpad comment.".to_string(),
        )
    })?;
    verify_pull_request_merged(&pr_url).await?;
    info!(tool = "todoist", action = "close_task", task_id, pr_url);
    tracker.close_task(task_id).await?;
    Ok(json!({
        "closed": true,
        "task_id": task_id,
        "pr_url": pr_url,
        "state": issue.state
    }))
}

async fn find_workpad_comment(
    tracker: &dyn TrackerClient,
    task_id: &str,
) -> Result<Option<Value>, TrackerError> {
    let mut cursor: Option<String> = None;
    let mut best: Option<Value> = None;

    loop {
        let mut arguments = json!({ "task_id": task_id });
        if let Some(next_cursor) = cursor.as_ref() {
            arguments["cursor"] = Value::String(next_cursor.clone());
        }
        let page = tracker.list_comments(arguments).await?;
        let results = page
            .get("results")
            .and_then(Value::as_array)
            .ok_or(TrackerError::TodoistUnknownPayload)?;
        for comment in results {
            if !is_workpad_comment(comment) {
                continue;
            }
            let replace = best
                .as_ref()
                .is_none_or(|current| workpad_sort_key(comment) >= workpad_sort_key(current));
            if replace {
                best = Some(comment.clone());
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

    Ok(best)
}

fn normalize_workpad_content(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return format!("{WORKPAD_HEADER}\n\n{WORKPAD_MARKER}");
    }
    if trimmed.contains(WORKPAD_MARKER) {
        return trimmed.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix(WORKPAD_HEADER) {
        return format!("{WORKPAD_HEADER}\n\n{WORKPAD_MARKER}{}", rest);
    }
    format!("{WORKPAD_HEADER}\n\n{WORKPAD_MARKER}\n\n{trimmed}")
}

fn is_workpad_comment(comment: &Value) -> bool {
    comment
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(|content| {
            content.contains(WORKPAD_MARKER) || content.trim_start().starts_with(WORKPAD_HEADER)
        })
}

fn workpad_sort_key(comment: &Value) -> &str {
    comment
        .get("updated_at")
        .and_then(Value::as_str)
        .or_else(|| comment.get("posted_at").and_then(Value::as_str))
        .unwrap_or("")
}

fn extract_github_pr_url(content: &str) -> Option<String> {
    static PR_URL_RE: OnceLock<Regex> = OnceLock::new();
    let regex = PR_URL_RE.get_or_init(|| {
        Regex::new(r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/pull/\d+")
            .expect("valid github pr regex")
    });
    regex
        .find(content)
        .map(|matched| matched.as_str().to_string())
}

async fn verify_pull_request_merged(pr_url: &str) -> Result<(), TrackerError> {
    let output = Command::new("gh")
        .args(["pr", "view", pr_url, "--json", "url,state,mergedAt,isDraft"])
        .output()
        .await
        .map_err(|error| {
            TrackerError::TrackerOperationUnsupported(format!(
                "`close_task` could not run `gh pr view`: {error}"
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr
        };
        return Err(TrackerError::TrackerOperationUnsupported(format!(
            "`close_task` could not verify GitHub PR merge status: {detail}"
        )));
    }

    let payload: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        TrackerError::TrackerOperationUnsupported(format!(
            "`close_task` could not parse `gh pr view` output: {error}"
        ))
    })?;
    let merged_at = payload.get("mergedAt");
    if merged_at.is_none() || merged_at.is_some_and(Value::is_null) {
        let state = payload
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("OPEN");
        return Err(TrackerError::TrackerOperationUnsupported(format!(
            "`close_task` requires the linked PR to be merged. Current PR state is `{state}`."
        )));
    }

    Ok(())
}

fn success_payload(body: Value) -> Value {
    json!({
        "success": true,
        "contentItems": [
            {
                "type": "inputText",
                "text": serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
            }
        ]
    })
}

fn failure_payload(body: Value) -> Value {
    json!({
        "success": false,
        "contentItems": [
            {
                "type": "inputText",
                "text": serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
            }
        ]
    })
}

fn github_api_available() -> bool {
    github_token_from_env().is_some() || gh_cli_available()
}

fn gh_cli_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        StdCommand::new("gh")
            .arg("--version")
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

fn github_api_base_url() -> String {
    runtime_env::get(GITHUB_API_BASE_URL_ENV)
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| GITHUB_API_DEFAULT_BASE_URL.to_string())
}

fn github_token_from_env() -> Option<String> {
    runtime_env::get("GH_TOKEN")
        .or_else(|| runtime_env::get("GITHUB_TOKEN"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn github_auth_token() -> Result<String, Value> {
    if let Some(token) = github_token_from_env() {
        return Ok(token);
    }

    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
        .map_err(|error| {
            json!({
                "error": {
                    "message": "GitHub API tool is unavailable because no auth token could be found.",
                    "reason": error.to_string()
                }
            })
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(json!({
            "error": {
                "message": "GitHub API tool could not retrieve a host auth token from `gh auth token`.",
                "reason": stderr
            }
        }));
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(json!({
            "error": {
                "message": "GitHub API tool received an empty auth token."
            }
        }));
    }

    Ok(token)
}

fn normalize_github_arguments(arguments: Value) -> Result<(Method, String, Option<Value>), Value> {
    let map = arguments.as_object().ok_or_else(|| {
        json!({
            "error": {
                "message": "`github_api` expects an object with `method`, `path`, and optional `body`."
            }
        })
    })?;

    let method = map
        .get("method")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            json!({
                "error": {
                    "message": "`github_api.method` is required."
                }
            })
        })?;
    let method = Method::from_bytes(method.to_ascii_uppercase().as_bytes()).map_err(|error| {
        json!({
            "error": {
                "message": "`github_api.method` must be a valid HTTP method.",
                "reason": error.to_string()
            }
        })
    })?;

    let path = map
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            json!({
                "error": {
                    "message": "`github_api.path` is required."
                }
            })
        })?;
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    let body = match map.get("body") {
        Some(Value::Null) | None => None,
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                Some(parsed)
            } else {
                Some(Value::String(raw.clone()))
            }
        }
        Some(value) => Some(value.clone()),
    };

    Ok((method, path, body))
}

async fn execute_github_api(
    method: Method,
    path: String,
    body: Option<Value>,
) -> Result<Value, Value> {
    let token = github_auth_token().await?;
    let url = format!("{}{}", github_api_base_url(), path);
    let client = reqwest::Client::builder()
        .user_agent("symphony-rust-todoist/github_api")
        .build()
        .map_err(|error| json!({ "error": { "message": error.to_string() } }))?;

    let mut request = client
        .request(method.clone(), &url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(body) = body {
        request = request.json(&body);
    }

    let response = request.send().await.map_err(|error| {
        json!({
            "error": {
                "message": "GitHub API request failed before receiving a response.",
                "reason": error.to_string(),
                "method": method.as_str(),
                "path": path
            }
        })
    })?;
    let status = response.status();
    let body_text = response.text().await.map_err(|error| {
        json!({
            "error": {
                "message": "GitHub API response body could not be read.",
                "reason": error.to_string(),
                "method": method.as_str(),
                "path": path
            }
        })
    })?;
    if body_text.trim().is_empty() {
        if status.is_success() {
            return Ok(json!({ "status": status.as_u16() }));
        }
        return Err(json!({
            "error": {
                "message": "GitHub API request returned a non-success status with an empty body.",
                "status": status.as_u16(),
                "method": method.as_str(),
                "path": path
            }
        }));
    }

    let parsed_body = parse_error_body(&body_text);
    if status.is_success() {
        Ok(parsed_body)
    } else {
        Err(json!({
            "error": {
                "message": format!("GitHub API request failed with HTTP {}.", status.as_u16()),
                "status": status.as_u16(),
                "method": method.as_str(),
                "path": path,
                "response": parsed_body
            }
        }))
    }
}

fn parse_error_body(body: &str) -> Value {
    if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!({ "raw": body }))
    }
}

fn tool_error_payload(error: TrackerError) -> Value {
    match error {
        TrackerError::MissingTrackerApiKey => json!({
            "error": {
                "message": "Symphony is missing Todoist auth. Set tracker.api_key in WORKFLOW.md or export TODOIST_API_TOKEN."
            }
        }),
        TrackerError::MissingTrackerProjectId => json!({
            "error": {
                "message": "Symphony is missing tracker.project_id."
            }
        }),
        TrackerError::TodoistProjectNotFound(project_id) => json!({
            "error": {
                "message": format!("Todoist project `{project_id}` was not found.")
            }
        }),
        TrackerError::TodoistMissingRequiredSection(section) => json!({
            "error": {
                "message": format!("Todoist section `{section}` is required by WORKFLOW.md but was not found.")
            }
        }),
        TrackerError::MissingTodoistCurrentUser => json!({
            "error": {
                "message": "Todoist current user could not be resolved."
            }
        }),
        TrackerError::TodoistCommentsUnavailable => json!({
            "error": {
                "message": "Todoist comments are unavailable for this account or plan."
            }
        }),
        TrackerError::TodoistRemindersUnavailable => json!({
            "error": {
                "message": "Todoist reminders are unavailable for this account or plan."
            }
        }),
        TrackerError::TodoistActivityLogUnavailable => json!({
            "error": {
                "message": "Todoist activity log is unavailable for this account or plan."
            }
        }),
        TrackerError::TodoistCommentTooLarge { limit, actual } => json!({
            "error": {
                "message": format!("Todoist comment content exceeds the {limit}-character limit."),
                "actual": actual,
                "limit": limit
            }
        }),
        TrackerError::TodoistRateLimited { retry_after } => json!({
            "error": {
                "message": "Todoist request was rate limited.",
                "retry_after": retry_after
            }
        }),
        TrackerError::TodoistApiStatus { status, body } => json!({
            "error": {
                "message": format!("Todoist API request failed with HTTP {status}."),
                "status": status,
                "body": body
            }
        }),
        TrackerError::TodoistApiRequest(reason) => json!({
            "error": {
                "message": "Todoist API request failed before receiving a successful response.",
                "reason": reason
            }
        }),
        TrackerError::TodoistUnknownPayload => json!({
            "error": {
                "message": "Todoist API returned a payload Symphony could not decode."
            }
        }),
        TrackerError::TrackerOperationUnsupported(reason) => json!({
            "error": {
                "message": reason
            }
        }),
        TrackerError::UnsupportedTrackerKind(kind) => json!({
            "error": {
                "message": format!("Unsupported tracker kind `{kind}`.")
            }
        }),
        TrackerError::MissingTrackerFixturePath => json!({
            "error": {
                "message": "The memory tracker requires tracker.fixture_path."
            }
        }),
        TrackerError::MemoryFixtureIo { path, error } => json!({
            "error": {
                "message": format!("Failed to read memory fixture `{path}`."),
                "reason": error
            }
        }),
        TrackerError::MemoryFixtureParse { path, error } => json!({
            "error": {
                "message": format!("Failed to parse memory fixture `{path}`."),
                "reason": error
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use std::{
        env, fs,
        sync::{Arc, Mutex},
    };
    use tempfile::tempdir;

    use crate::{
        config::ServiceConfig,
        issue::Issue,
        tracker::{TrackerClient, TrackerError},
    };

    use super::{TODOIST_TOOL, execute, extract_github_pr_url, tool_specs};

    #[derive(Clone, Default)]
    struct StubTodoistTracker {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl StubTodoistTracker {
        fn record(&self, call: impl Into<String>) {
            self.calls.lock().expect("calls").push(call.into());
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

    #[async_trait]
    impl TrackerClient for StubTodoistTracker {
        async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
            unreachable!()
        }

        async fn fetch_issues_by_states(
            &self,
            _states: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            unreachable!()
        }

        async fn fetch_issue_states_by_ids(
            &self,
            _issue_ids: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            unreachable!()
        }

        async fn list_projects(&self, _arguments: Value) -> Result<Value, TrackerError> {
            Ok(json!({ "results": [{"id": "proj-1"}], "next_cursor": null }))
        }

        async fn get_project(&self, project_id: &str) -> Result<Value, TrackerError> {
            Ok(json!({ "id": project_id, "name": "Project" }))
        }

        async fn get_comment(&self, comment_id: &str) -> Result<Value, TrackerError> {
            Ok(json!({ "id": comment_id, "item_id": "task-1", "content": "## Codex Workpad" }))
        }

        async fn delete_comment(&self, comment_id: &str) -> Result<Value, TrackerError> {
            self.record(format!("delete_comment:{comment_id}"));
            Ok(Value::Null)
        }

        async fn list_comments(&self, arguments: Value) -> Result<Value, TrackerError> {
            let task_id = arguments
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let results = match task_id {
                "task-workpad" => vec![json!({
                    "id": "comment-workpad",
                    "item_id": "task-workpad",
                    "content": "## Codex Workpad\n\nExisting",
                    "posted_at": "2026-03-11T20:00:00Z"
                })],
                _ => Vec::new(),
            };
            Ok(json!({ "results": results, "next_cursor": null }))
        }

        async fn create_comment(&self, arguments: Value) -> Result<Value, TrackerError> {
            let task_id = arguments
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let content = arguments
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.record(format!("create_comment:{task_id}"));
            Ok(json!({
                "id": "comment-created",
                "item_id": task_id,
                "content": content
            }))
        }

        async fn update_comment(
            &self,
            comment_id: &str,
            arguments: Value,
        ) -> Result<Value, TrackerError> {
            let content = arguments
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.record(format!("update_comment:{comment_id}"));
            Ok(json!({
                "id": comment_id,
                "item_id": "task-workpad",
                "content": content
            }))
        }

        async fn create_reminder(&self, arguments: Value) -> Result<Value, TrackerError> {
            Ok(json!({ "created": true, "task_id": arguments["task_id"] }))
        }

        async fn list_activities(&self, _arguments: Value) -> Result<Value, TrackerError> {
            Ok(json!({
                "events": [{"id": 1, "object_type": "item", "event_type": "updated"}],
                "next_cursor": null
            }))
        }

        async fn update_reminder(
            &self,
            reminder_id: &str,
            arguments: Value,
        ) -> Result<Value, TrackerError> {
            Ok(
                json!({ "updated": true, "id": reminder_id, "minute_offset": arguments["minute_offset"] }),
            )
        }

        async fn delete_reminder(&self, reminder_id: &str) -> Result<Value, TrackerError> {
            Ok(json!({ "deleted": true, "id": reminder_id }))
        }
    }

    struct ReminderErrorTracker;

    struct CloseGuardTracker {
        issue: Issue,
        workpad_content: String,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl CloseGuardTracker {
        fn new(state: &str, workpad_content: &str) -> Self {
            Self {
                issue: Issue {
                    id: "task-close".to_string(),
                    identifier: "TD-task-close".to_string(),
                    title: "Close guard".to_string(),
                    state: state.to_string(),
                    ..Issue::default()
                },
                workpad_content: workpad_content.to_string(),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

    #[async_trait]
    impl TrackerClient for ReminderErrorTracker {
        async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
            unreachable!()
        }

        async fn fetch_issues_by_states(
            &self,
            _states: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            unreachable!()
        }

        async fn fetch_issue_states_by_ids(
            &self,
            _issue_ids: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            unreachable!()
        }

        async fn create_reminder(&self, _arguments: Value) -> Result<Value, TrackerError> {
            Err(TrackerError::TodoistRemindersUnavailable)
        }

        async fn list_activities(&self, _arguments: Value) -> Result<Value, TrackerError> {
            Err(TrackerError::TodoistActivityLogUnavailable)
        }
    }

    #[async_trait]
    impl TrackerClient for CloseGuardTracker {
        async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
            unreachable!()
        }

        async fn fetch_issues_by_states(
            &self,
            _states: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            unreachable!()
        }

        async fn fetch_issue_states_by_ids(
            &self,
            _issue_ids: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            Ok(vec![self.issue.clone()])
        }

        async fn list_comments(&self, _arguments: Value) -> Result<Value, TrackerError> {
            Ok(json!({
                "results": [{
                    "id": "comment-workpad",
                    "item_id": "task-close",
                    "content": self.workpad_content,
                    "posted_at": "2026-03-12T04:00:00Z"
                }],
                "next_cursor": null
            }))
        }

        async fn close_task(&self, task_id: &str) -> Result<Value, TrackerError> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("close_task:{task_id}"));
            Ok(Value::Null)
        }
    }

    fn test_config() -> ServiceConfig {
        ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "memory"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config")
    }

    fn payload_body(result: &Value) -> Value {
        let text = result["contentItems"][0]["text"].as_str().expect("text");
        serde_json::from_str(text).expect("json body")
    }

    #[tokio::test]
    async fn tool_specs_describe_expanded_todoist_surface() {
        let specs = tool_specs(&test_config());
        let todoist = specs
            .into_iter()
            .find(|spec| spec["name"] == TODOIST_TOOL)
            .expect("todoist tool");
        let description = todoist["description"].as_str().expect("description");

        assert!(description.contains("list_projects"));
        assert!(description.contains("list_collaborators"));
        assert!(description.contains("list_labels"));
        assert!(description.contains("get_comment"));
        assert!(description.contains("delete_comment"));
        assert!(description.contains("upsert_workpad"));
        assert!(description.contains("create_project_comment"));
        assert!(description.contains("create_reminder"));
        assert!(description.contains("list_activities"));
    }

    #[tokio::test]
    async fn execute_todoist_routes_project_and_reminder_actions() {
        let config = test_config();
        let tracker = StubTodoistTracker::default();

        let projects = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "list_projects"}),
        )
        .await;
        assert!(projects["success"].as_bool().expect("success"));
        assert_eq!(payload_body(&projects)["results"][0]["id"], "proj-1");

        let project = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "get_project", "project_id": "proj-9"}),
        )
        .await;
        assert_eq!(payload_body(&project)["id"], "proj-9");

        let comment = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "get_comment", "comment_id": "comment-7"}),
        )
        .await;
        assert_eq!(payload_body(&comment)["item_id"], "task-1");

        let project_comment = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "create_project_comment", "project_id": "proj-9", "content": "Project note"}),
        )
        .await;
        assert_eq!(payload_body(&project_comment)["content"], "Project note");

        let reminder = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "update_reminder", "reminder_id": "rem-1", "minute_offset": 15}),
        )
        .await;
        assert_eq!(payload_body(&reminder)["id"], "rem-1");

        let activities = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "list_activities", "object_type": "item", "object_id": "task-1"}),
        )
        .await;
        assert_eq!(
            payload_body(&activities)["events"][0]["event_type"],
            "updated"
        );

        let deleted = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "delete_reminder", "reminder_id": "rem-2"}),
        )
        .await;
        assert_eq!(payload_body(&deleted)["deleted"], true);
    }

    #[tokio::test]
    async fn execute_todoist_workpad_actions_reuse_single_comment() {
        let config = test_config();
        let tracker = StubTodoistTracker::default();

        let existing = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "get_workpad", "task_id": "task-workpad"}),
        )
        .await;
        assert_eq!(payload_body(&existing)["found"], true);
        assert_eq!(payload_body(&existing)["comment_id"], "comment-workpad");

        let updated = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "upsert_workpad", "task_id": "task-workpad", "content": "## Codex Workpad\n\nBody"}),
        )
        .await;
        assert_eq!(payload_body(&updated)["created"], false);
        assert!(
            payload_body(&updated)["comment"]["content"]
                .as_str()
                .expect("content")
                .contains("<!-- symphony:workpad -->")
        );

        let created = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "upsert_workpad", "task_id": "task-empty", "content": "Body"}),
        )
        .await;
        assert_eq!(payload_body(&created)["created"], true);
        assert!(
            payload_body(&created)["comment"]["content"]
                .as_str()
                .expect("content")
                .starts_with("## Codex Workpad")
        );

        let deleted = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "delete_workpad", "task_id": "task-workpad"}),
        )
        .await;
        assert_eq!(payload_body(&deleted)["deleted"], true);

        assert_eq!(
            tracker.calls(),
            vec![
                "update_comment:comment-workpad".to_string(),
                "create_comment:task-empty".to_string(),
                "delete_comment:comment-workpad".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn execute_todoist_rejects_generic_comment_creation() {
        let config = test_config();
        let tracker = StubTodoistTracker::default();

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "create_comment", "task_id": "task-1", "content": "summary"}),
        )
        .await;

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "Use `upsert_workpad` for task-scoped Symphony comments or `create_project_comment` for project comments."
        );
        assert!(tracker.calls().is_empty());
    }

    #[tokio::test]
    async fn close_task_requires_merging_state() {
        let config = test_config();
        let tracker = CloseGuardTracker::new(
            "In Progress",
            "## Codex Workpad\n\n<!-- symphony:workpad -->\n\nPR: https://github.com/example/repo/pull/1",
        );

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "close_task", "task_id": "task-close"}),
        )
        .await;

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "`close_task` is only allowed from `Merging`. Current state is `In Progress`."
        );
        assert!(tracker.calls().is_empty());
    }

    #[tokio::test]
    async fn close_task_requires_merged_pr_in_workpad() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");
        let gh_path = bin_dir.join("gh");
        fs::write(
            &gh_path,
            "#!/bin/sh\nprintf '%s\\n' '{\"url\":\"https://github.com/example/repo/pull/1\",\"state\":\"OPEN\",\"mergedAt\":null,\"isDraft\":false}'\n",
        )
        .expect("fake gh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(&gh_path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&gh_path, permissions).expect("chmod");
        }

        let original_path = env::var("PATH").unwrap_or_default();
        unsafe {
            env::set_var("PATH", format!("{}:{}", bin_dir.display(), original_path));
        }

        let config = test_config();
        let tracker = CloseGuardTracker::new(
            "Merging",
            "## Codex Workpad\n\n<!-- symphony:workpad -->\n\nPR: https://github.com/example/repo/pull/1",
        );

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "close_task", "task_id": "task-close"}),
        )
        .await;

        unsafe {
            env::set_var("PATH", original_path);
        }

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "`close_task` requires the linked PR to be merged. Current PR state is `OPEN`."
        );
        assert!(tracker.calls().is_empty());
    }

    #[test]
    fn extracts_github_pr_urls_from_workpad_content() {
        let content = "## Codex Workpad\n\n<!-- symphony:workpad -->\n\nTracking https://github.com/example/repo/pull/42";
        assert_eq!(
            extract_github_pr_url(content).as_deref(),
            Some("https://github.com/example/repo/pull/42")
        );
    }

    #[tokio::test]
    async fn execute_todoist_maps_reminder_availability_errors() {
        let config = test_config();
        let tracker = ReminderErrorTracker;

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "create_reminder", "task_id": "task-1"}),
        )
        .await;

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "Todoist reminders are unavailable for this account or plan."
        );
    }

    #[tokio::test]
    async fn execute_todoist_maps_activity_log_availability_errors() {
        let config = test_config();
        let tracker = ReminderErrorTracker;

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "list_activities", "object_type": "item", "object_id": "task-1"}),
        )
        .await;

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "Todoist activity log is unavailable for this account or plan."
        );
    }

    #[tokio::test]
    async fn execute_todoist_requires_reminder_id_for_update() {
        let config = test_config();
        let tracker = StubTodoistTracker::default();

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "update_reminder", "minute_offset": 10}),
        )
        .await;

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "`todoist.reminder_id` is required"
        );
    }
}
