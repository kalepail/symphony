use regex::Regex;
use reqwest::{
    Method, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, process::Command as StdCommand, sync::OnceLock, time::Duration};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{
    config::{ServiceConfig, TodoistToolSurface},
    issue::normalize_state_name,
    runtime_env,
    tracker::{TODOIST_COMMENT_SIZE_LIMIT, TrackerCapabilities, TrackerClient, TrackerError},
};

pub const GITHUB_API_TOOL: &str = "github_api";
pub const TODOIST_TOOL: &str = "todoist";
const GITHUB_API_BASE_URL_ENV: &str = "SYMPHONY_GITHUB_API_URL";
const GITHUB_API_DEFAULT_BASE_URL: &str = "https://api.github.com";
const GITHUB_API_MAX_RETRIES: usize = 2;
const GITHUB_API_DEFAULT_DELAY_SECS: u64 = 2;
const GITHUB_API_MAX_DELAY_SECS: u64 = 60;
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
const TODOIST_CURATED_ACTIONS: &[&str] = &[
    "list_tasks",
    "get_task",
    "list_sections",
    "list_comments",
    "get_workpad",
    "upsert_workpad",
    "delete_workpad",
    "update_task",
    "move_task",
    "close_task",
    "reopen_task",
    "create_task",
];
const TODOIST_EXTENDED_ACTIONS: &[&str] = &[
    "list_projects",
    "get_project",
    "get_current_user",
    "list_collaborators",
    "get_section",
    "list_labels",
    "get_comment",
    "create_project_comment",
    "update_comment",
    "delete_comment",
];
const TODOIST_ACTIVITY_ACTIONS: &[&str] = &["list_activities"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ToolPayloadMetric {
    pub name: String,
    pub bytes: usize,
}

pub fn tool_specs(config: &ServiceConfig) -> Vec<Value> {
    tool_specs_with_capabilities(config, TrackerCapabilities::full())
}

pub async fn tool_specs_with_tracker(
    config: &ServiceConfig,
    tracker: &dyn TrackerClient,
) -> Vec<Value> {
    let capabilities = tracker
        .capabilities()
        .await
        .unwrap_or_else(|_| TrackerCapabilities::full());
    tool_specs_with_capabilities(config, capabilities)
}

fn tool_specs_with_capabilities(
    config: &ServiceConfig,
    capabilities: TrackerCapabilities,
) -> Vec<Value> {
    let mut specs = Vec::new();

    if matches!(config.tracker.kind.as_deref(), Some("todoist" | "memory")) {
        specs.push(json!({
            "name": TODOIST_TOOL,
            "description": todoist_tool_description(config, capabilities),
            "inputSchema": todoist_input_schema(config, capabilities)
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

pub fn tool_payload_metrics(specs: &[Value]) -> Vec<ToolPayloadMetric> {
    specs
        .iter()
        .filter_map(|spec| {
            Some(ToolPayloadMetric {
                name: spec.get("name")?.as_str()?.to_string(),
                bytes: serde_json::to_vec(spec).ok()?.len(),
            })
        })
        .collect()
}

pub fn total_tool_payload_bytes(metrics: &[ToolPayloadMetric]) -> usize {
    metrics.iter().map(|metric| metric.bytes).sum()
}

fn todoist_tool_description(config: &ServiceConfig, capabilities: TrackerCapabilities) -> String {
    let actions = todoist_supported_actions(config, capabilities).join(", ");
    format!(
        concat!(
            "Run one structured Todoist API v1 action using Symphony's configured tracker auth.\n",
            "Supported actions for this session: {}.\n",
            "Guardrails:\n",
            "- Use `get_workpad`, `upsert_workpad`, and `delete_workpad` for the single task workpad.\n",
            "- Do not use generic task-comment creation for Symphony notes. Project comments, when exposed, are for true project-level notes only.\n",
            "- `close_task` is guarded and normally only succeeds from `Merging` after the linked GitHub PR is confirmed merged.\n",
            "- Keep calls narrow: one Todoist action per tool call."
        ),
        actions,
    )
}

fn todoist_supported_actions(
    config: &ServiceConfig,
    capabilities: TrackerCapabilities,
) -> Vec<&'static str> {
    let mut actions = TODOIST_CURATED_ACTIONS.to_vec();
    if config.tracker.todoist_tool_surface == TodoistToolSurface::Extended {
        actions.extend_from_slice(TODOIST_EXTENDED_ACTIONS);
    }
    if capabilities.activity_log {
        actions.extend_from_slice(TODOIST_ACTIVITY_ACTIONS);
    }
    actions
}

fn todoist_input_schema(config: &ServiceConfig, capabilities: TrackerCapabilities) -> Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    let mut schema = SCHEMA
        .get_or_init(|| {
            serde_json::from_str(
                r#"{
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["action"],
                    "properties": {
                        "action": { "type": "string", "description": "Todoist action to execute." },
                        "content": { "type": ["string", "null"], "description": "Primary content for create_task, create_project_comment, update_comment, or upsert_workpad." },
                        "description": { "type": ["string", "null"], "description": "Todoist task description for create_task or update_task." },
                        "labels": { "description": "Todoist label names for create_task or update_task." },
                        "assignee_id": { "type": ["string", "number", "null"], "description": "Todoist assignee id for create_task or update_task." },
                        "priority": { "type": ["integer", "null"], "description": "Todoist task priority from 1 (normal) to 4 (urgent)." },
                        "due": { "description": "Todoist due object for create_task or update_task." },
                        "deadline": { "description": "Todoist deadline object for create_task or update_task." },
                        "cursor": { "type": ["string", "null"], "description": "Cursor for paginated list actions." },
                        "limit": { "type": ["integer", "null"], "description": "Requested page size for paginated list actions." },
                        "task_id": { "type": ["string", "number", "null"], "description": "Task identifier for get/update/move/close/reopen task actions." },
                        "section_id": { "type": ["string", "number", "null"], "description": "Section identifier for get/update section-based actions." },
                        "parent_id": { "type": ["string", "number", "null"], "description": "Parent task identifier for subtask listing, creation, or task reparenting." },
                        "origin_task_id": { "type": ["string", "number", "null"], "description": "Optional source task id for follow-up task creation; Symphony records it as `TD-<task_id>` in the new task description." },
                        "label": { "type": ["string", "null"], "description": "Todoist label name for list_tasks filtering." },
                        "ids": { "description": "Comma-separated task ids or an array of task ids for list_tasks." },
                        "filter": { "type": ["string", "null"], "description": "Todoist filter query for list_tasks. When provided, Symphony uses `/tasks/filter`." },
                        "lang": { "type": ["string", "null"], "description": "IETF language tag for Todoist filter queries." },
                        "comment_id": { "type": ["string", "number", "null"], "description": "Comment identifier for get_comment, update_comment, delete_comment, or as an optional workpad hint for upsert_workpad." },
                        "project_id": { "type": ["string", "number", "null"], "description": "Optional project identifier; defaults to tracker.project_id where applicable. Do not send this for task comment actions." },
                        "attachment": { "description": "Optional Todoist comment attachment object." },
                        "uids_to_notify": { "description": "Optional list of Todoist user ids to notify for comment creation." },
                        "object_type": { "type": ["string", "null"], "description": "Activity-log object type such as `project`, `item`, or `note`." },
                        "object_id": { "type": ["string", "number", "null"], "description": "Activity-log object identifier, used with object_type." },
                        "event_type": { "type": ["string", "null"], "description": "Todoist activity event type such as `added`, `updated`, `completed`, or `moved`." },
                        "object_event_types": { "description": "Single value or array using Todoist `object_type:event_type` syntax, for example `item:completed`." },
                        "parent_project_id": { "type": ["string", "number", "null"], "description": "Activity-log parent project identifier." },
                        "parent_item_id": { "type": ["string", "number", "null"], "description": "Activity-log parent task identifier." },
                        "date_from": { "type": ["string", "null"], "description": "RFC3339 lower bound for list_activities." },
                        "date_to": { "type": ["string", "null"], "description": "RFC3339 exclusive upper bound for list_activities." },
                        "include_parent_object": { "type": ["boolean", "null"], "description": "Whether Todoist should expand the parent object in activity results." },
                        "include_child_objects": { "type": ["boolean", "null"], "description": "Whether Todoist should expand child objects in activity results." },
                        "annotate_notes": { "type": ["boolean", "null"], "description": "Whether Todoist should annotate note activity results." },
                        "annotate_parents": { "type": ["boolean", "null"], "description": "Whether Todoist should annotate parent objects in activity results." },
                        "initiator_id_null": { "type": ["boolean", "null"], "description": "Filter activities by whether the initiator is absent (`true`) or present (`false`)." },
                        "initiator_id": { "description": "Single initiator id or array of initiator ids for list_activities." },
                        "workspace_id": { "description": "Optional Todoist workspace id filter for list_activities." }
                    }
                }"#,
            )
            .expect("valid todoist input schema")
        })
        .clone();

    let supported_actions = todoist_supported_actions(config, capabilities);
    let supported_action_set = supported_actions.iter().copied().collect::<BTreeSet<_>>();
    let mut allowed_fields = BTreeSet::new();
    allowed_fields.insert("action");
    for action in &supported_actions {
        let (allowed, _) = todoist_action_contract(action).expect("supported action contract");
        allowed_fields.extend(allowed.iter().copied());
    }

    let properties = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .expect("todoist schema properties");
    properties.retain(|key, _| allowed_fields.contains(key.as_str()));
    if let Some(action_property) = properties.get_mut("action").and_then(Value::as_object_mut) {
        action_property.insert("enum".to_string(), json!(supported_action_set));
        action_property.insert(
            "description".to_string(),
            Value::String("Todoist action to execute for this session.".to_string()),
        );
    }

    schema
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
    validate_todoist_action_arguments(&args, action)?;
    let capabilities = tracker
        .capabilities()
        .await
        .unwrap_or_else(|_| TrackerCapabilities::full());
    ensure_action_capability(action, capabilities)?;
    info!(
        "tool=todoist status=started action={} {}",
        action,
        todoist_log_scope(&args)
    );

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
            delete_project_comment(tracker, &required_id(&args, "comment_id")?).await
        }
        "list_comments" => tracker.list_comments(Value::Object(args)).await,
        "get_workpad" => get_workpad(tracker, &required_id(&args, "task_id")?).await,
        "upsert_workpad" => {
            let task_id = required_id(&args, "task_id")?;
            let content = required_string(&args, "content")?;
            let comment_id = optional_id(&args, "comment_id");
            upsert_workpad(tracker, &task_id, &content, comment_id.as_deref()).await
        }
        "delete_workpad" => delete_workpad(tracker, &required_id(&args, "task_id")?).await,
        "create_project_comment" => create_project_comment(tracker, args).await,
        "create_comment" => reject_generic_comment_creation(),
        "update_comment" => update_project_comment(tracker, args).await,
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
        "list_activities" => tracker.list_activities(Value::Object(args)).await,
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

fn todoist_log_scope(args: &serde_json::Map<String, Value>) -> String {
    const LOG_KEYS: &[&str] = &[
        "task_id",
        "project_id",
        "section_id",
        "comment_id",
        "object_id",
        "origin_task_id",
    ];

    let mut parts = Vec::new();
    for key in LOG_KEYS {
        if let Some(value) = args.get(*key) {
            let value = match value {
                Value::String(text) => text.trim().to_string(),
                Value::Number(number) => number.to_string(),
                _ => String::new(),
            };
            if !value.is_empty() {
                parts.push(format!("{key}={value}"));
            }
        }
    }

    if parts.is_empty() {
        "scope=none".to_string()
    } else {
        parts.join(" ")
    }
}

fn optional_id(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.trim().to_string()),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
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

fn validate_todoist_action_arguments(
    args: &serde_json::Map<String, Value>,
    action: &str,
) -> Result<(), TrackerError> {
    let (allowed, required) = todoist_action_contract(action)?;
    reject_unknown_keys(args, allowed)?;
    for field in required {
        if !args.contains_key(*field) {
            return Err(TrackerError::TrackerOperationUnsupported(format!(
                "`todoist.{field}` is required"
            )));
        }
    }

    if action == "list_comments" {
        require_exactly_one_comment_target(args)?;
    }

    Ok(())
}

fn todoist_action_contract(
    action: &str,
) -> Result<(&'static [&'static str], &'static [&'static str]), TrackerError> {
    match action {
        "list_projects" | "get_current_user" | "list_labels" => {
            Ok((&["action", "cursor", "limit"], &[]))
        }
        "get_project" => Ok((&["action", "project_id"], &["project_id"])),
        "list_collaborators" | "list_sections" => {
            Ok((&["action", "project_id", "cursor", "limit"], &[]))
        }
        "list_tasks" => Ok((
            &[
                "action",
                "project_id",
                "section_id",
                "parent_id",
                "label",
                "ids",
                "filter",
                "lang",
                "cursor",
                "limit",
            ],
            &[],
        )),
        "get_task" => Ok((&["action", "task_id"], &["task_id"])),
        "get_section" => Ok((&["action", "section_id"], &["section_id"])),
        "get_comment" | "delete_comment" => Ok((&["action", "comment_id"], &["comment_id"])),
        "list_comments" => Ok((&["action", "task_id", "project_id", "cursor", "limit"], &[])),
        "create_comment" => Ok((
            &[
                "action",
                "task_id",
                "project_id",
                "content",
                "attachment",
                "uids_to_notify",
            ],
            &["content"],
        )),
        "get_workpad" | "delete_workpad" | "close_task" | "reopen_task" => {
            Ok((&["action", "task_id"], &["task_id"]))
        }
        "upsert_workpad" => Ok((
            &["action", "task_id", "comment_id", "content"],
            &["task_id", "content"],
        )),
        "create_project_comment" => Ok((
            &[
                "action",
                "project_id",
                "content",
                "attachment",
                "uids_to_notify",
            ],
            &["project_id", "content"],
        )),
        "update_comment" => Ok((
            &["action", "comment_id", "content"],
            &["comment_id", "content"],
        )),
        "update_task" => Ok((
            &[
                "action",
                "task_id",
                "content",
                "description",
                "labels",
                "assignee_id",
                "priority",
                "due",
                "deadline",
                "section_id",
                "project_id",
            ],
            &["task_id"],
        )),
        "move_task" => Ok((
            &["action", "task_id", "project_id", "section_id", "parent_id"],
            &["task_id"],
        )),
        "create_task" => Ok((
            &[
                "action",
                "content",
                "description",
                "labels",
                "assignee_id",
                "priority",
                "due",
                "deadline",
                "project_id",
                "section_id",
                "parent_id",
                "origin_task_id",
            ],
            &["content"],
        )),
        "list_activities" => Ok((
            &[
                "action",
                "object_type",
                "object_id",
                "event_type",
                "object_event_types",
                "parent_project_id",
                "parent_item_id",
                "date_from",
                "date_to",
                "include_parent_object",
                "include_child_objects",
                "annotate_notes",
                "annotate_parents",
                "initiator_id_null",
                "initiator_id",
                "workspace_id",
                "cursor",
                "limit",
            ],
            &[],
        )),
        other => Err(TrackerError::TrackerOperationUnsupported(format!(
            "unsupported todoist action `{other}`"
        ))),
    }
}

fn reject_unknown_keys(
    args: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), TrackerError> {
    let unknown = args
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return Ok(());
    }

    Err(TrackerError::TrackerOperationUnsupported(format!(
        "unsupported keys for `todoist.{}`: {}",
        args.get("action")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        unknown.join(", ")
    )))
}

fn require_exactly_one_comment_target(
    args: &serde_json::Map<String, Value>,
) -> Result<(), TrackerError> {
    let task_id = args.get("task_id").is_some();
    let project_id = args.get("project_id").is_some();
    match (task_id, project_id) {
        (true, false) | (false, true) => Ok(()),
        _ => Err(TrackerError::TrackerOperationUnsupported(
            "exactly one of `todoist.task_id` or `todoist.project_id` is required".to_string(),
        )),
    }
}

fn ensure_action_capability(
    action: &str,
    capabilities: TrackerCapabilities,
) -> Result<(), TrackerError> {
    match action {
        "get_comment"
        | "list_comments"
        | "create_project_comment"
        | "update_comment"
        | "delete_comment"
        | "get_workpad"
        | "upsert_workpad"
        | "delete_workpad"
            if !capabilities.comments =>
        {
            Err(TrackerError::TodoistCommentsUnavailable)
        }
        "list_activities" if !capabilities.activity_log => {
            Err(TrackerError::TodoistActivityLogUnavailable)
        }
        _ => Ok(()),
    }
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

async fn update_project_comment(
    tracker: &dyn TrackerClient,
    args: serde_json::Map<String, Value>,
) -> Result<Value, TrackerError> {
    let comment_id = required_id(&args, "comment_id")?;
    ensure_comment_is_project_scoped(tracker, &comment_id).await?;
    tracker
        .update_comment(&comment_id, Value::Object(args))
        .await
}

async fn delete_project_comment(
    tracker: &dyn TrackerClient,
    comment_id: &str,
) -> Result<Value, TrackerError> {
    ensure_comment_is_project_scoped(tracker, comment_id).await?;
    tracker.delete_comment(comment_id).await
}

async fn ensure_comment_is_project_scoped(
    tracker: &dyn TrackerClient,
    comment_id: &str,
) -> Result<(), TrackerError> {
    let comment = tracker.get_comment(comment_id).await?;
    if id_string(comment.get("item_id")).is_some() {
        return Err(TrackerError::TrackerOperationUnsupported(
            "Use `get_workpad`, `upsert_workpad`, or `delete_workpad` for task-scoped Symphony comments.".to_string(),
        ));
    }
    Ok(())
}

async fn get_workpad(tracker: &dyn TrackerClient, task_id: &str) -> Result<Value, TrackerError> {
    let resolved = resolve_workpad_comment(tracker, task_id, None, true).await?;
    let comment = resolved.comment;
    Ok(json!({
        "task_id": task_id,
        "found": comment.is_some(),
        "comment_id": comment.as_ref().and_then(|value| value.get("id")).cloned().unwrap_or(Value::Null),
        "comment": comment,
        "repaired_duplicates": resolved.repaired_duplicates
    }))
}

async fn upsert_workpad(
    tracker: &dyn TrackerClient,
    task_id: &str,
    content: &str,
    comment_id: Option<&str>,
) -> Result<Value, TrackerError> {
    let content = normalize_workpad_content(content);
    let resolved = resolve_workpad_comment(tracker, task_id, comment_id, true).await?;

    if let Some(existing) = resolved.comment {
        if existing
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            == Some(content.as_str())
        {
            return Ok(json!({
                "task_id": task_id,
                "created": false,
                "updated": false,
                "comment_id": existing.get("id").cloned().unwrap_or(Value::Null),
                "comment": existing,
                "repaired_duplicates": resolved.repaired_duplicates
            }));
        }

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
            "comment": comment,
            "repaired_duplicates": resolved.repaired_duplicates
        }));
    }

    let comment = tracker
        .create_comment(json!({ "task_id": task_id, "content": content }))
        .await?;
    Ok(json!({
        "task_id": task_id,
        "created": true,
        "comment_id": comment.get("id").cloned().unwrap_or(Value::Null),
        "comment": comment,
        "repaired_duplicates": resolved.repaired_duplicates
    }))
}

async fn delete_workpad(tracker: &dyn TrackerClient, task_id: &str) -> Result<Value, TrackerError> {
    let resolved = resolve_workpad_comment(tracker, task_id, None, true).await?;
    let Some(comment) = resolved.comment else {
        return Ok(json!({
            "task_id": task_id,
            "deleted": false,
            "comment_id": Value::Null,
            "repaired_duplicates": resolved.repaired_duplicates
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
        "comment_id": comment_id,
        "repaired_duplicates": resolved.repaired_duplicates
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

    let workpad = resolve_workpad_comment(tracker, task_id, None, true)
        .await?
        .comment
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

struct WorkpadCommentResolution {
    comment: Option<Value>,
    repaired_duplicates: bool,
}

async fn resolve_workpad_comment(
    tracker: &dyn TrackerClient,
    task_id: &str,
    comment_hint: Option<&str>,
    repair_duplicates: bool,
) -> Result<WorkpadCommentResolution, TrackerError> {
    if let Some(comment_hint) = comment_hint
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        match tracker.get_comment(comment_hint).await {
            Ok(comment) => {
                if !is_workpad_comment(&comment) {
                    return Err(TrackerError::TrackerOperationUnsupported(format!(
                        "comment_id `{comment_hint}` does not reference a Symphony workpad comment."
                    )));
                }

                let hinted_task_id = id_string(comment.get("item_id")).ok_or_else(|| {
                    TrackerError::TrackerOperationUnsupported(format!(
                        "workpad comment `{comment_hint}` is missing `item_id`."
                    ))
                })?;
                if hinted_task_id != task_id {
                    return Err(TrackerError::TrackerOperationUnsupported(format!(
                        "workpad comment `{comment_hint}` belongs to task `{hinted_task_id}`, not `{task_id}`."
                    )));
                }

                return Ok(WorkpadCommentResolution {
                    comment: Some(comment),
                    repaired_duplicates: false,
                });
            }
            Err(TrackerError::TodoistApiStatus { status: 404, .. }) => {}
            Err(error) => return Err(error),
        }
    }

    let mut cursor: Option<String> = None;
    let mut matches = Vec::new();

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
            if is_workpad_comment(comment) {
                matches.push(comment.clone());
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

    if matches.is_empty() {
        return Ok(WorkpadCommentResolution {
            comment: None,
            repaired_duplicates: false,
        });
    }

    let canonical = choose_canonical_workpad_comment(&matches, comment_hint)?;
    let repaired_duplicates = if repair_duplicates && matches.len() > 1 {
        for duplicate in matches
            .iter()
            .filter(|comment| workpad_comment_id(comment).as_deref() != Some(canonical.as_str()))
        {
            let duplicate_id = workpad_comment_id(duplicate).ok_or_else(|| {
                TrackerError::TrackerOperationUnsupported(
                    "duplicate workpad comment is missing an `id`".to_string(),
                )
            })?;
            tracker.delete_comment(&duplicate_id).await?;
        }
        true
    } else {
        false
    };

    let comment = matches
        .into_iter()
        .find(|comment| workpad_comment_id(comment).as_deref() == Some(canonical.as_str()));

    Ok(WorkpadCommentResolution {
        comment,
        repaired_duplicates,
    })
}

fn normalize_workpad_content(content: &str) -> String {
    let trimmed = content.trim();
    let normalized = if trimmed.is_empty() {
        format!("{WORKPAD_HEADER}\n\n{WORKPAD_MARKER}")
    } else if trimmed.contains(WORKPAD_MARKER) {
        trimmed.to_string()
    } else if let Some(rest) = trimmed.strip_prefix(WORKPAD_HEADER) {
        format!("{WORKPAD_HEADER}\n\n{WORKPAD_MARKER}{}", rest)
    } else {
        format!("{WORKPAD_HEADER}\n\n{WORKPAD_MARKER}\n\n{trimmed}")
    };
    compact_workpad_content(&normalized)
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

fn workpad_comment_id(comment: &Value) -> Option<String> {
    id_string(comment.get("id"))
}

fn choose_canonical_workpad_comment(
    comments: &[Value],
    comment_hint: Option<&str>,
) -> Result<String, TrackerError> {
    if comments.len() == 1 {
        return workpad_comment_id(&comments[0]).ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(
                "existing workpad comment is missing an `id`".to_string(),
            )
        });
    }

    if let Some(comment_hint) = comment_hint
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && comments
            .iter()
            .any(|comment| workpad_comment_id(comment).as_deref() == Some(comment_hint))
    {
        return Ok(comment_hint.to_string());
    }

    let normalized_contents = comments
        .iter()
        .map(|comment| {
            comment
                .get("content")
                .and_then(Value::as_str)
                .map(normalize_workpad_content)
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let all_equivalent = normalized_contents
        .windows(2)
        .all(|pair| pair[0] == pair[1]);
    if !all_equivalent {
        let duplicate_ids = comments
            .iter()
            .filter_map(workpad_comment_id)
            .collect::<Vec<_>>();
        return Err(TrackerError::TrackerOperationUnsupported(format!(
            "multiple workpad comments exist for task and automatic repair is unsafe. Duplicate comment ids: {}",
            duplicate_ids.join(", ")
        )));
    }

    comments
        .iter()
        .max_by_key(|comment| workpad_sort_key(comment))
        .and_then(workpad_comment_id)
        .ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(
                "existing workpad comment is missing an `id`".to_string(),
            )
        })
}

fn compact_workpad_content(content: &str) -> String {
    if content.len() <= TODOIST_COMMENT_SIZE_LIMIT {
        return content.to_string();
    }

    let preferred_sections = [
        "Plan",
        "Acceptance Criteria",
        "Status",
        "Blockers",
        "Validation",
        "Notes",
        "Handoff",
        "Confusions",
    ];
    let pr_url = extract_github_pr_url(content);
    let sections = parse_workpad_sections(content);
    let mut rebuilt = vec![
        WORKPAD_HEADER.to_string(),
        String::new(),
        WORKPAD_MARKER.to_string(),
    ];
    let mut preserved = 0usize;

    for preferred in preferred_sections {
        if let Some((_, body)) = sections
            .iter()
            .find(|(heading, _)| heading.eq_ignore_ascii_case(preferred))
        {
            rebuilt.push(String::new());
            rebuilt.push(format!("## {preferred}"));
            rebuilt.push(String::new());
            rebuilt.push(truncate_workpad_text(body.trim(), 2_200));
            preserved += 1;
        }
    }

    let history = sections
        .iter()
        .filter(|(heading, _)| {
            !preferred_sections
                .iter()
                .any(|preferred| heading.eq_ignore_ascii_case(preferred))
        })
        .map(|(heading, body)| {
            let excerpt = truncate_workpad_text(body.trim(), 160);
            format!("- {heading}: {excerpt}")
        })
        .collect::<Vec<_>>();

    if let Some(pr_url) = pr_url.as_ref()
        && !rebuilt.iter().any(|segment| segment.contains(pr_url))
    {
        rebuilt.push(String::new());
        rebuilt.push("## Validation".to_string());
        rebuilt.push(String::new());
        rebuilt.push(format!("Linked PR: {pr_url}"));
        preserved += 1;
    }

    if !history.is_empty() {
        rebuilt.push(String::new());
        rebuilt.push("## Compacted History".to_string());
        rebuilt.push(String::new());
        rebuilt.push(history.join("\n"));
    } else if preserved == 0 {
        let excerpt = truncate_workpad_text(strip_workpad_prefix(content).trim(), 3_000);
        rebuilt.push(String::new());
        rebuilt.push("## Status".to_string());
        rebuilt.push(String::new());
        rebuilt.push(excerpt);
    }

    let mut compacted = rebuilt.join("\n");
    if compacted.len() > TODOIST_COMMENT_SIZE_LIMIT {
        compacted = truncate_workpad_to_limit(&compacted, pr_url.as_deref());
    }
    compacted
}

fn strip_workpad_prefix(content: &str) -> String {
    let mut body = Vec::new();
    let mut skipped_header = false;
    let mut skipped_marker = false;
    for line in content.lines() {
        if !skipped_header && line.trim() == WORKPAD_HEADER {
            skipped_header = true;
            continue;
        }
        if skipped_header && !skipped_marker && line.trim() == WORKPAD_MARKER {
            skipped_marker = true;
            continue;
        }
        body.push(line);
    }
    body.join("\n")
}

fn parse_workpad_sections(content: &str) -> Vec<(String, String)> {
    let mut sections = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_body = Vec::new();
    for line in strip_workpad_prefix(content).lines() {
        if let Some(rest) = line.trim_start().strip_prefix("## ") {
            if let Some(heading) = current_heading.take() {
                sections.push((heading, current_body.join("\n").trim().to_string()));
                current_body.clear();
            }
            current_heading = Some(rest.trim().to_string());
        } else {
            current_body.push(line.to_string());
        }
    }
    if let Some(heading) = current_heading {
        sections.push((heading, current_body.join("\n").trim().to_string()));
    }
    sections
}

fn truncate_workpad_text(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text[..max_chars.min(text.len())].trim_end().to_string();
    truncated.push_str(" ...");
    truncated
}

fn truncate_workpad_to_limit(content: &str, pr_url: Option<&str>) -> String {
    if content.len() <= TODOIST_COMMENT_SIZE_LIMIT {
        return content.to_string();
    }

    if let Some(pr_url) = pr_url {
        let suffix = format!("\n\nLinked PR: {pr_url}");
        let budget = TODOIST_COMMENT_SIZE_LIMIT.saturating_sub(suffix.len());
        let mut truncated = truncate_workpad_text(content, budget);
        truncated.push_str(&suffix);
        return truncated;
    }

    truncate_workpad_text(content, TODOIST_COMMENT_SIZE_LIMIT)
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

fn id_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.trim().to_string()),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn github_pr_coordinates(pr_url: &str) -> Option<(String, String, String)> {
    static PR_COORDINATES_RE: OnceLock<Regex> = OnceLock::new();
    let regex = PR_COORDINATES_RE.get_or_init(|| {
        Regex::new(r"^https://github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/pull/(\d+)$")
            .expect("valid github pr coordinates regex")
    });

    regex.captures(pr_url).map(|captures| {
        (
            captures[1].to_string(),
            captures[2].to_string(),
            captures[3].to_string(),
        )
    })
}

async fn verify_pull_request_merged(pr_url: &str) -> Result<(), TrackerError> {
    let Some((owner, repo, number)) = github_pr_coordinates(pr_url) else {
        return Err(TrackerError::TrackerOperationUnsupported(format!(
            "`close_task` requires a valid GitHub PR URL in the workpad comment: {pr_url}"
        )));
    };
    let path = format!("/repos/{owner}/{repo}/pulls/{number}");

    match github_api_request(Method::GET, path.clone(), None).await {
        Ok(payload) => verify_pull_request_merged_payload(&payload),
        Err(api_error) => verify_pull_request_merged_with_gh(pr_url)
            .await
            .map_err(|gh_error| {
                TrackerError::TrackerOperationUnsupported(format!(
                    "`close_task` could not verify GitHub PR merge status via runtime API ({}) or `gh pr view` ({gh_error})",
                    github_error_summary(&api_error)
                ))
            }),
    }
}

fn verify_pull_request_merged_payload(payload: &Value) -> Result<(), TrackerError> {
    let merged_at = payload.get("merged_at").or_else(|| payload.get("mergedAt"));
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

async fn verify_pull_request_merged_with_gh(pr_url: &str) -> Result<(), String> {
    let output = Command::new("gh")
        .args(["pr", "view", pr_url, "--json", "url,state,mergedAt,isDraft"])
        .output()
        .await
        .map_err(|error| format!("could not run `gh pr view`: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr
        });
    }

    let payload: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("could not parse `gh pr view` output: {error}"))?;
    verify_pull_request_merged_payload(&payload).map_err(|error| error.to_string())
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
    github_token_from_env().is_some() || gh_auth_available()
}

fn gh_auth_available() -> bool {
    match StdCommand::new("gh").args(["auth", "token"]).output() {
        Ok(output) if output.status.success() => {
            !String::from_utf8_lossy(&output.stdout).trim().is_empty()
        }
        _ => false,
    }
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
    github_api_request(method, path, body).await
}

async fn github_api_request(
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

    let mut waited_secs = 0u64;
    for attempt in 0..=GITHUB_API_MAX_RETRIES {
        let mut request = client
            .request(method.clone(), &url)
            .bearer_auth(&token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(body) = body.as_ref() {
            request = request.json(body);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                if let Some(delay_secs) = github_transport_retry_delay_seconds(attempt, waited_secs)
                {
                    warn!(
                        "github_api status=retry method={} path={} attempt={} delay_secs={} reason={}",
                        method.as_str(),
                        path,
                        attempt + 1,
                        delay_secs,
                        error
                    );
                    waited_secs = waited_secs.saturating_add(delay_secs);
                    sleep(Duration::from_secs(delay_secs)).await;
                    continue;
                }

                return Err(json!({
                    "error": {
                        "message": "GitHub API request failed before receiving a response.",
                        "reason": error.to_string(),
                        "method": method.as_str(),
                        "path": path
                    }
                }));
            }
        };

        let status = response.status();
        let headers = response.headers().clone();
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
        let parsed_body = parse_error_body(&body_text);
        let response_summary = github_response_summary(&parsed_body);

        if status.is_success() {
            if body_text.trim().is_empty() {
                return Ok(json!({ "status": status.as_u16() }));
            }
            return Ok(parsed_body);
        }

        if let Some(delay_secs) =
            github_status_retry_delay_seconds(status, &headers, attempt, waited_secs)
        {
            let retry_after_secs = github_retry_after_seconds(&headers);
            let response_summary_token = response_summary
                .as_deref()
                .map(inline_log_value)
                .unwrap_or_else(|| "none".to_string());
            warn!(
                "github_api status=retry method={} path={} attempt={} delay_secs={} http_status={} retry_after_secs={} reason={} error={}",
                method.as_str(),
                path,
                attempt + 1,
                delay_secs,
                status.as_u16(),
                retry_after_secs
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
                github_retry_reason(status, retry_after_secs),
                response_summary_token,
            );
            waited_secs = waited_secs.saturating_add(delay_secs);
            sleep(Duration::from_secs(delay_secs)).await;
            continue;
        }

        return Err(json!({
            "error": {
                "message": github_error_message(status, response_summary.as_deref()),
                "status": status.as_u16(),
                "method": method.as_str(),
                "path": path,
                "response": parsed_body,
                "response_summary": response_summary
            }
        }));
    }

    Err(json!({
        "error": {
            "message": "GitHub API request exhausted retry budget.",
            "method": method.as_str(),
            "path": path
        }
    }))
}

fn github_status_retry_delay_seconds(
    status: StatusCode,
    headers: &HeaderMap,
    attempt: usize,
    waited_secs: u64,
) -> Option<u64> {
    if attempt >= GITHUB_API_MAX_RETRIES || !github_status_is_transient(status) {
        return None;
    }

    let retry_after = github_retry_after_seconds(headers);
    let delay_secs = retry_after
        .unwrap_or_else(|| github_default_retry_delay_seconds(attempt))
        .min(GITHUB_API_MAX_DELAY_SECS);

    if waited_secs.saturating_add(delay_secs)
        > GITHUB_API_MAX_DELAY_SECS.saturating_mul((GITHUB_API_MAX_RETRIES + 1) as u64)
    {
        return None;
    }

    Some(delay_secs.max(1))
}

fn github_transport_retry_delay_seconds(attempt: usize, waited_secs: u64) -> Option<u64> {
    if attempt >= GITHUB_API_MAX_RETRIES {
        return None;
    }

    let delay_secs = github_default_retry_delay_seconds(attempt);
    if waited_secs.saturating_add(delay_secs)
        > GITHUB_API_MAX_DELAY_SECS.saturating_mul((GITHUB_API_MAX_RETRIES + 1) as u64)
    {
        return None;
    }

    Some(delay_secs)
}

fn github_default_retry_delay_seconds(attempt: usize) -> u64 {
    let shift = attempt.min(5) as u32;
    GITHUB_API_DEFAULT_DELAY_SECS
        .saturating_mul(1u64 << shift)
        .min(GITHUB_API_MAX_DELAY_SECS)
}

fn github_status_is_transient(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn github_retry_after_seconds(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn github_retry_reason(status: StatusCode, retry_after_secs: Option<u64>) -> &'static str {
    if status == StatusCode::TOO_MANY_REQUESTS || retry_after_secs.is_some() {
        "rate_limited"
    } else {
        "transient_http_error"
    }
}

fn github_error_summary(error: &Value) -> String {
    let Some(details) = error.get("error") else {
        return error.to_string();
    };

    let message = details
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown GitHub API error");
    let reason = details.get("reason").and_then(Value::as_str);
    match reason {
        Some(reason) if !reason.is_empty() => format!("{message}: {reason}"),
        _ => message.to_string(),
    }
}

fn parse_error_body(body: &str) -> Value {
    if body.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!({ "raw": body }))
    }
}

fn github_error_message(status: StatusCode, response_summary: Option<&str>) -> String {
    match response_summary.filter(|value| !value.is_empty()) {
        Some(summary) => format!(
            "GitHub API request failed with HTTP {}: {summary}",
            status.as_u16()
        ),
        None => format!("GitHub API request failed with HTTP {}.", status.as_u16()),
    }
}

fn github_response_summary(response: &Value) -> Option<String> {
    let mut parts = Vec::new();

    if let Some(message) = response
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        parts.push(message.to_string());
    }

    if let Some(errors) = response.get("errors").and_then(Value::as_array) {
        for error in errors.iter().take(2) {
            if let Some(summary) = github_error_entry_summary(error) {
                parts.push(summary);
            }
        }
        if errors.len() > 2 {
            parts.push(format!("{} more error(s)", errors.len() - 2));
        }
    }

    if parts.is_empty()
        && let Some(raw) = response
            .get("raw")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        parts.push(truncate_log_detail(raw, 240));
    }

    if parts.is_empty() {
        None
    } else {
        Some(truncate_log_detail(&parts.join("; "), 240))
    }
}

fn inline_log_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "none".to_string();
    }

    trimmed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("|")
        .chars()
        .take(240)
        .collect()
}

fn github_error_entry_summary(error: &Value) -> Option<String> {
    match error {
        Value::String(value) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| truncate_log_detail(trimmed, 120))
        }
        Value::Object(map) => {
            let resource = map.get("resource").and_then(Value::as_str);
            let field = map.get("field").and_then(Value::as_str);
            let code = map.get("code").and_then(Value::as_str);
            let message = map.get("message").and_then(Value::as_str);
            let mut parts = Vec::new();
            if let Some(resource) = resource.filter(|value| !value.trim().is_empty()) {
                parts.push(resource.trim().to_string());
            }
            if let Some(field) = field.filter(|value| !value.trim().is_empty()) {
                parts.push(field.trim().to_string());
            }
            if let Some(code) = code.filter(|value| !value.trim().is_empty()) {
                parts.push(code.trim().to_string());
            }
            if let Some(message) = message.filter(|value| !value.trim().is_empty()) {
                parts.push(message.trim().to_string());
            }
            (!parts.is_empty()).then(|| truncate_log_detail(&parts.join(":"), 120))
        }
        _ => None,
    }
}

fn truncate_log_detail(value: &str, max_chars: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
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
        TrackerError::TodoistAssigneeNotResolvable {
            assignee,
            project_id,
        } => json!({
            "error": {
                "message": format!(
                    "Todoist assignee `{assignee}` is not valid for project `{project_id}`."
                )
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
#[allow(clippy::await_holding_lock)]
mod tests {
    use async_trait::async_trait;
    use axum::{Json, Router, http::StatusCode as HttpStatusCode, routing::get};
    use serde_json::{Value, json};
    use std::{
        env, fs,
        sync::{Arc, Mutex},
    };
    use tempfile::tempdir;
    use tokio::{net::TcpListener, task::JoinHandle};

    use crate::{
        config::{ServiceConfig, TodoistToolSurface},
        issue::Issue,
        tracker::{TODOIST_COMMENT_SIZE_LIMIT, TrackerCapabilities, TrackerClient, TrackerError},
    };

    use super::{
        GITHUB_API_TOOL, TODOIST_TOOL, compact_workpad_content, execute, extract_github_pr_url,
        tool_payload_metrics, tool_specs, tool_specs_with_tracker,
    };

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
            let item_id = match comment_id {
                "comment-workpad" => "task-workpad",
                _ => "task-1",
            };
            Ok(json!({ "id": comment_id, "item_id": item_id, "content": "## Codex Workpad" }))
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
                    "content": "## Codex Workpad\n\n<!-- symphony:workpad -->\n\nExisting",
                    "posted_at": "2026-03-11T20:00:00Z"
                })],
                _ => Vec::new(),
            };
            self.record(format!("list_comments:{task_id}"));
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

        async fn list_activities(&self, _arguments: Value) -> Result<Value, TrackerError> {
            Ok(json!({
                "events": [{"id": 1, "object_type": "item", "event_type": "updated"}],
                "next_cursor": null
            }))
        }
    }

    struct CapabilityTracker {
        capabilities: TrackerCapabilities,
    }

    struct ScopedCommentTracker {
        comment: Value,
    }

    #[derive(Clone)]
    struct DuplicateWorkpadTracker {
        calls: Arc<Mutex<Vec<String>>>,
        comments: Vec<Value>,
    }

    impl DuplicateWorkpadTracker {
        fn new(comments: Vec<Value>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                comments,
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls").clone()
        }
    }

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

    fn set_env_var(key: &str, value: &str) {
        unsafe {
            env::set_var(key, value);
        }
    }

    fn remove_env_var(key: &str) {
        unsafe {
            env::remove_var(key);
        }
    }

    async fn spawn_github_pr_server(payload: Value) -> (String, JoinHandle<()>) {
        async fn pull_response(
            axum::extract::State(payload): axum::extract::State<Value>,
        ) -> (HttpStatusCode, Json<Value>) {
            (HttpStatusCode::OK, Json(payload))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move {
            let app = Router::new()
                .route("/repos/{owner}/{repo}/pulls/{number}", get(pull_response))
                .with_state(payload);
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{address}"), join)
    }

    #[async_trait]
    impl TrackerClient for CapabilityTracker {
        async fn capabilities(&self) -> Result<TrackerCapabilities, TrackerError> {
            Ok(self.capabilities)
        }

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
    }

    #[async_trait]
    impl TrackerClient for ScopedCommentTracker {
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

        async fn get_comment(&self, _comment_id: &str) -> Result<Value, TrackerError> {
            Ok(self.comment.clone())
        }
    }

    #[async_trait]
    impl TrackerClient for DuplicateWorkpadTracker {
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

        async fn list_comments(&self, arguments: Value) -> Result<Value, TrackerError> {
            let task_id = arguments
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.calls
                .lock()
                .expect("calls")
                .push(format!("list_comments:{task_id}"));
            Ok(json!({
                "results": self.comments,
                "next_cursor": null
            }))
        }

        async fn delete_comment(&self, comment_id: &str) -> Result<Value, TrackerError> {
            self.calls
                .lock()
                .expect("calls")
                .push(format!("delete_comment:{comment_id}"));
            Ok(Value::Null)
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

    fn extended_test_config() -> ServiceConfig {
        let mut config = test_config();
        config.tracker.todoist_tool_surface = TodoistToolSurface::Extended;
        config
    }

    fn payload_body(result: &Value) -> Value {
        let text = result["contentItems"][0]["text"].as_str().expect("text");
        serde_json::from_str(text).expect("json body")
    }

    #[tokio::test]
    async fn tool_specs_default_to_curated_todoist_surface() {
        let specs = tool_specs(&test_config());
        let todoist = specs
            .into_iter()
            .find(|spec| spec["name"] == TODOIST_TOOL)
            .expect("todoist tool");
        let description = todoist["description"].as_str().expect("description");
        let schema = todoist["inputSchema"].as_object().expect("schema");
        let properties = schema["properties"].as_object().expect("properties");
        let action_enum = properties["action"]["enum"]
            .as_array()
            .expect("action enum");

        assert!(description.contains("upsert_workpad"));
        assert!(description.contains("list_activities"));
        assert!(!description.contains("list_projects"));
        assert!(!description.contains("create_project_comment"));
        assert!(!description.contains("create_reminder"));
        assert!(!properties.contains_key("reminder_id"));
        assert!(!properties.contains_key("uids_to_notify"));
        assert!(action_enum.iter().all(|value| value != "list_projects"));
        assert!(action_enum.iter().any(|value| value == "get_workpad"));
    }

    #[tokio::test]
    async fn tool_specs_extended_surface_restores_non_default_actions() {
        let specs = tool_specs(&extended_test_config());
        let todoist = specs
            .into_iter()
            .find(|spec| spec["name"] == TODOIST_TOOL)
            .expect("todoist tool");
        let description = todoist["description"].as_str().expect("description");
        let properties = todoist["inputSchema"]["properties"]
            .as_object()
            .expect("properties");

        assert!(description.contains("list_projects"));
        assert!(description.contains("create_project_comment"));
        assert!(description.contains("list_activities"));
        assert!(properties.contains_key("uids_to_notify"));
        assert!(description.contains("list_activities"));
        assert!(!description.contains("create_reminder"));
        assert!(!properties.contains_key("reminder_id"));
    }

    #[tokio::test]
    async fn tool_specs_hide_optional_capability_actions_when_tracker_capabilities_disable_them() {
        let tracker = CapabilityTracker {
            capabilities: TrackerCapabilities {
                comments: true,
                reminders: false,
                activity_log: false,
            },
        };

        let specs = tool_specs_with_tracker(&extended_test_config(), &tracker).await;
        let todoist = specs
            .into_iter()
            .find(|spec| spec["name"] == TODOIST_TOOL)
            .expect("todoist tool");
        let description = todoist["description"].as_str().expect("description");
        let properties = todoist["inputSchema"]["properties"]
            .as_object()
            .expect("properties");

        assert!(description.contains("list_projects"));
        assert!(description.contains("get_workpad"));
        assert!(!description.contains("list_activities"));
        assert!(!properties.contains_key("reminder_id"));
        assert!(!properties.contains_key("date_from"));
    }

    #[test]
    fn tool_payload_metrics_report_serialized_tool_sizes() {
        let specs = tool_specs(&test_config());
        let metrics = tool_payload_metrics(&specs);
        let todoist = metrics
            .iter()
            .find(|metric| metric.name == TODOIST_TOOL)
            .expect("todoist metrics");

        assert!(todoist.bytes > 0);
    }

    #[test]
    fn tool_specs_hide_github_api_when_gh_is_installed_but_unauthenticated() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");
        let gh_path = bin_dir.join("gh");
        fs::write(
            &gh_path,
            "#!/bin/sh\nif [ \"$1\" = \"auth\" ] && [ \"$2\" = \"token\" ]; then\n  printf 'not logged in\\n' >&2\n  exit 1\nfi\nif [ \"$1\" = \"--version\" ]; then\n  printf 'gh version test\\n'\n  exit 0\nfi\nprintf 'unsupported gh call\\n' >&2\nexit 1\n",
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
        let original_gh_token = env::var("GH_TOKEN").ok();
        let original_github_token = env::var("GITHUB_TOKEN").ok();
        set_env_var("PATH", &format!("{}:{}", bin_dir.display(), original_path));
        remove_env_var("GH_TOKEN");
        remove_env_var("GITHUB_TOKEN");

        let specs = tool_specs(&test_config());

        set_env_var("PATH", &original_path);
        match original_gh_token {
            Some(value) => set_env_var("GH_TOKEN", &value),
            None => remove_env_var("GH_TOKEN"),
        }
        match original_github_token {
            Some(value) => set_env_var("GITHUB_TOKEN", &value),
            None => remove_env_var("GITHUB_TOKEN"),
        }

        assert!(specs.iter().all(|spec| spec["name"] != GITHUB_API_TOOL));
    }

    #[tokio::test]
    async fn execute_todoist_routes_project_and_activity_actions() {
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
            json!({"action": "upsert_workpad", "task_id": "task-workpad", "comment_id": "comment-workpad", "content": "## Codex Workpad\n\nBody"}),
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
                "list_comments:task-workpad".to_string(),
                "update_comment:comment-workpad".to_string(),
                "list_comments:task-empty".to_string(),
                "create_comment:task-empty".to_string(),
                "list_comments:task-workpad".to_string(),
                "delete_comment:comment-workpad".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn execute_todoist_workpad_noops_when_normalized_content_is_unchanged() {
        let config = test_config();
        let tracker = StubTodoistTracker::default();

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({
                "action": "upsert_workpad",
                "task_id": "task-workpad",
                "content": "## Codex Workpad\n\n<!-- symphony:workpad -->\n\nExisting"
            }),
        )
        .await;

        assert!(result["success"].as_bool().expect("success"));
        assert_eq!(payload_body(&result)["created"], false);
        assert_eq!(payload_body(&result)["updated"], false);
        assert_eq!(
            tracker.calls(),
            vec!["list_comments:task-workpad".to_string()]
        );
    }

    #[tokio::test]
    async fn execute_todoist_rejects_unknown_keys_per_action_contract() {
        let config = test_config();
        let tracker = StubTodoistTracker::default();

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "list_projects", "bogus": true}),
        )
        .await;

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "unsupported keys for `todoist.list_projects`: bogus"
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
    async fn execute_todoist_rejects_task_scoped_project_comment_mutations() {
        let config = test_config();
        let tracker = ScopedCommentTracker {
            comment: json!({
                "id": "comment-7",
                "item_id": 42,
                "content": "## Codex Workpad\n\n<!-- symphony:workpad -->"
            }),
        };

        let update = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "update_comment", "comment_id": "comment-7", "content": "new"}),
        )
        .await;
        assert!(!update["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&update)["error"]["message"],
            "Use `get_workpad`, `upsert_workpad`, or `delete_workpad` for task-scoped Symphony comments."
        );

        let delete = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "delete_comment", "comment_id": "comment-7"}),
        )
        .await;
        assert!(!delete["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&delete)["error"]["message"],
            "Use `get_workpad`, `upsert_workpad`, or `delete_workpad` for task-scoped Symphony comments."
        );
    }

    #[tokio::test]
    async fn get_workpad_repairs_equivalent_duplicates() {
        let config = test_config();
        let tracker = DuplicateWorkpadTracker::new(vec![
            json!({
                "id": "comment-old",
                "item_id": "task-dupe",
                "content": "## Codex Workpad\n\n<!-- symphony:workpad -->\n\n## Status\n\nReady",
                "posted_at": "2026-03-10T20:00:00Z"
            }),
            json!({
                "id": "comment-new",
                "item_id": "task-dupe",
                "content": "## Codex Workpad\n\n<!-- symphony:workpad -->\n\n## Status\n\nReady",
                "posted_at": "2026-03-11T20:00:00Z"
            }),
        ]);

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "get_workpad", "task_id": "task-dupe"}),
        )
        .await;

        assert!(result["success"].as_bool().expect("success"));
        assert_eq!(payload_body(&result)["comment_id"], "comment-new");
        assert_eq!(payload_body(&result)["repaired_duplicates"], true);
        assert_eq!(
            tracker.calls(),
            vec![
                "list_comments:task-dupe".to_string(),
                "delete_comment:comment-old".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn get_workpad_fails_when_duplicate_contents_conflict() {
        let config = test_config();
        let tracker = DuplicateWorkpadTracker::new(vec![
            json!({
                "id": "comment-a",
                "item_id": "task-dupe",
                "content": "## Codex Workpad\n\n<!-- symphony:workpad -->\n\n## Status\n\nOne",
                "posted_at": "2026-03-10T20:00:00Z"
            }),
            json!({
                "id": "comment-b",
                "item_id": "task-dupe",
                "content": "## Codex Workpad\n\n<!-- symphony:workpad -->\n\n## Status\n\nTwo",
                "posted_at": "2026-03-11T20:00:00Z"
            }),
        ]);

        let result = execute(
            &config,
            &tracker,
            TODOIST_TOOL,
            json!({"action": "get_workpad", "task_id": "task-dupe"}),
        )
        .await;

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "multiple workpad comments exist for task and automatic repair is unsafe. Duplicate comment ids: comment-a, comment-b"
        );
        assert_eq!(tracker.calls(), vec!["list_comments:task-dupe".to_string()]);
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
    #[allow(clippy::await_holding_lock)]
    async fn close_task_requires_merged_pr_in_workpad() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let original_github_api_url = env::var("SYMPHONY_GITHUB_API_URL").ok();
        let original_gh_token = env::var("GH_TOKEN").ok();
        let (github_api_url, join) =
            spawn_github_pr_server(json!({"state": "open", "merged_at": null})).await;
        set_env_var("SYMPHONY_GITHUB_API_URL", &github_api_url);
        set_env_var("GH_TOKEN", "test-token");

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

        join.abort();
        match original_github_api_url {
            Some(value) => set_env_var("SYMPHONY_GITHUB_API_URL", &value),
            None => remove_env_var("SYMPHONY_GITHUB_API_URL"),
        }
        match original_gh_token {
            Some(value) => set_env_var("GH_TOKEN", &value),
            None => remove_env_var("GH_TOKEN"),
        }

        assert!(!result["success"].as_bool().expect("success"));
        assert_eq!(
            payload_body(&result)["error"]["message"],
            "`close_task` requires the linked PR to be merged. Current PR state is `open`."
        );
        assert!(tracker.calls().is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn close_task_accepts_merged_pr_verified_via_runtime_github_api() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let original_github_api_url = env::var("SYMPHONY_GITHUB_API_URL").ok();
        let original_gh_token = env::var("GH_TOKEN").ok();
        let (github_api_url, join) =
            spawn_github_pr_server(json!({"state": "closed", "merged_at": "2026-03-12T14:00:00Z"}))
                .await;
        set_env_var("SYMPHONY_GITHUB_API_URL", &github_api_url);
        set_env_var("GH_TOKEN", "test-token");

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

        join.abort();
        match original_github_api_url {
            Some(value) => set_env_var("SYMPHONY_GITHUB_API_URL", &value),
            None => remove_env_var("SYMPHONY_GITHUB_API_URL"),
        }
        match original_gh_token {
            Some(value) => set_env_var("GH_TOKEN", &value),
            None => remove_env_var("GH_TOKEN"),
        }

        assert!(result["success"].as_bool().expect("success"));
        assert_eq!(payload_body(&result)["closed"], true);
        assert_eq!(tracker.calls(), vec!["close_task:task-close".to_string()]);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn close_task_falls_back_to_gh_when_runtime_github_api_is_unreachable() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).expect("bin dir");
        let gh_path = bin_dir.join("gh");
        fs::write(
            &gh_path,
            "#!/bin/sh\nif [ \"$1\" = \"pr\" ] && [ \"$2\" = \"view\" ]; then\n  printf '%s\\n' '{\"url\":\"https://github.com/example/repo/pull/1\",\"state\":\"MERGED\",\"mergedAt\":\"2026-03-12T14:00:00Z\",\"isDraft\":false}'\n  exit 0\nfi\nprintf 'unsupported gh call\\n' >&2\nexit 1\n",
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
        let original_github_api_url = env::var("SYMPHONY_GITHUB_API_URL").ok();
        let original_gh_token = env::var("GH_TOKEN").ok();
        set_env_var("PATH", &format!("{}:{}", bin_dir.display(), original_path));
        set_env_var("SYMPHONY_GITHUB_API_URL", "http://127.0.0.1:1");
        set_env_var("GH_TOKEN", "test-token");

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

        set_env_var("PATH", &original_path);
        match original_github_api_url {
            Some(value) => set_env_var("SYMPHONY_GITHUB_API_URL", &value),
            None => remove_env_var("SYMPHONY_GITHUB_API_URL"),
        }
        match original_gh_token {
            Some(value) => set_env_var("GH_TOKEN", &value),
            None => remove_env_var("GH_TOKEN"),
        }

        assert!(result["success"].as_bool().expect("success"));
        assert_eq!(payload_body(&result)["closed"], true);
        assert_eq!(tracker.calls(), vec!["close_task:task-close".to_string()]);
    }

    #[test]
    fn extracts_github_pr_urls_from_workpad_content() {
        let content = "## Codex Workpad\n\n<!-- symphony:workpad -->\n\nTracking https://github.com/example/repo/pull/42";
        assert_eq!(
            extract_github_pr_url(content).as_deref(),
            Some("https://github.com/example/repo/pull/42")
        );
    }

    #[test]
    fn compact_workpad_content_preserves_key_sections_within_comment_limit() {
        let repeated = "x".repeat(TODOIST_COMMENT_SIZE_LIMIT / 3);
        let content = format!(
            "## Codex Workpad\n\n<!-- symphony:workpad -->\n\n## Plan\n\n{repeated}\n\n## Acceptance Criteria\n\n{repeated}\n\n## Validation\n\n{repeated}\n\n## Notes\n\n{repeated}\n\n## Confusions\n\n{repeated}\n\n## Scratch\n\n{repeated}\n\nLinked PR: https://github.com/example/repo/pull/42"
        );

        let compacted = compact_workpad_content(&content);

        assert!(compacted.len() <= TODOIST_COMMENT_SIZE_LIMIT);
        assert!(compacted.contains("## Plan"));
        assert!(compacted.contains("## Acceptance Criteria"));
        assert!(compacted.contains("## Validation"));
        assert!(compacted.contains("## Notes"));
        assert!(compacted.contains("## Confusions"));
        assert!(compacted.contains("## Compacted History"));
        assert!(compacted.contains("https://github.com/example/repo/pull/42"));
    }

    #[test]
    fn github_response_summary_includes_message_and_field_errors() {
        let summary = super::github_response_summary(&json!({
            "message": "Invalid request.\n\nCheck the payload.",
            "errors": [
                {"resource": "Commit", "field": "content", "code": "invalid"},
                {"message": "path must not be empty"}
            ]
        }))
        .expect("summary");

        assert_eq!(
            summary,
            "Invalid request. Check the payload.; Commit:content:invalid; path must not be empty"
        );
    }

    #[test]
    fn github_error_message_uses_response_summary_when_available() {
        let message = super::github_error_message(
            reqwest::StatusCode::BAD_REQUEST,
            Some("Invalid request.; Commit:content:invalid"),
        );

        assert_eq!(
            message,
            "GitHub API request failed with HTTP 400: Invalid request.; Commit:content:invalid"
        );
    }

    #[test]
    fn inline_log_value_emits_whitespace_free_token() {
        assert_eq!(
            super::inline_log_value(" Invalid request.\n\nCheck the payload. "),
            "Invalid|request.|Check|the|payload."
        );
    }

    #[tokio::test]
    async fn execute_todoist_maps_activity_log_availability_errors() {
        let config = test_config();
        let tracker = CapabilityTracker {
            capabilities: TrackerCapabilities {
                comments: true,
                reminders: false,
                activity_log: false,
            },
        };

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
}
