use reqwest::Method;
use serde_json::{Value, json};
use std::{env, process::Command as StdCommand, sync::OnceLock};
use tokio::process::Command;

use crate::{
    config::ServiceConfig,
    tracker::{TrackerClient, TrackerError},
};

pub const GITHUB_API_TOOL: &str = "github_api";
pub const TODOIST_TOOL: &str = "todoist";
const GITHUB_API_BASE_URL_ENV: &str = "SYMPHONY_GITHUB_API_URL";
const GITHUB_API_DEFAULT_BASE_URL: &str = "https://api.github.com";
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
    "list_tasks, get_task, list_sections, get_section, list_labels, list_comments, create_comment, ",
    "update_comment, update_task, move_task, close_task, reopen_task, create_task, list_reminders, ",
    "create_reminder, update_reminder, delete_reminder.\n",
    "Use this tool instead of raw HTTP. Keep each call narrow and specific."
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
                    "task_id": {
                        "type": ["string", "number", "null"],
                        "description": "Task identifier for get/update/move/close/reopen task actions."
                    },
                    "section_id": {
                        "type": ["string", "number", "null"],
                        "description": "Section identifier for get/update section-based actions."
                    },
                    "comment_id": {
                        "type": ["string", "number", "null"],
                        "description": "Comment identifier for update_comment."
                    },
                    "project_id": {
                        "type": ["string", "number", "null"],
                        "description": "Optional project identifier; defaults to tracker.project_id where applicable."
                    },
                    "reminder_id": {
                        "type": ["string", "number", "null"],
                        "description": "Reminder identifier for update_reminder and delete_reminder."
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

            match execute_todoist(tracker, arguments).await {
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
        "list_comments" => tracker.list_comments(Value::Object(args)).await,
        "create_comment" => tracker.create_comment(Value::Object(args)).await,
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
        "close_task" => tracker.close_task(&required_id(&args, "task_id")?).await,
        "reopen_task" => tracker.reopen_task(&required_id(&args, "task_id")?).await,
        "create_task" => tracker.create_task(Value::Object(args)).await,
        "list_reminders" => tracker.list_reminders(Value::Object(args)).await,
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
    env::var(GITHUB_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| GITHUB_API_DEFAULT_BASE_URL.to_string())
}

fn github_token_from_env() -> Option<String> {
    env::var("GH_TOKEN")
        .ok()
        .or_else(|| env::var("GITHUB_TOKEN").ok())
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

    use crate::{
        config::ServiceConfig,
        issue::Issue,
        tracker::{TrackerClient, TrackerError},
    };

    use super::{TODOIST_TOOL, execute, tool_specs};

    struct StubTodoistTracker;

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

        async fn create_reminder(&self, arguments: Value) -> Result<Value, TrackerError> {
            Ok(json!({ "created": true, "task_id": arguments["task_id"] }))
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
        assert!(description.contains("create_reminder"));
    }

    #[tokio::test]
    async fn execute_todoist_routes_project_and_reminder_actions() {
        let config = test_config();
        let tracker = StubTodoistTracker;

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

        let reminder = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "update_reminder", "reminder_id": "rem-1", "minute_offset": 15}),
        )
        .await;
        assert_eq!(payload_body(&reminder)["id"], "rem-1");

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
    async fn execute_todoist_requires_reminder_id_for_update() {
        let config = test_config();
        let tracker = StubTodoistTracker;

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
