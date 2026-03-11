use chrono::{DateTime, Utc};
use std::net::SocketAddr;

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, task::JoinHandle};

use crate::{
    orchestrator::{
        IssueDetail, OrchestratorHandle, PollingSnapshot, RecentEvent, RetrySnapshot, Snapshot,
        SnapshotCounts, TokenSnapshot,
    },
    workflow::WorkflowStore,
};

const ISSUE_ROUTE: &str = "/api/v1/{identifier}";

#[derive(Clone)]
struct AppState {
    orchestrator: OrchestratorHandle,
    workflow_store: WorkflowStore,
}

#[derive(Clone, Debug, Serialize)]
struct StatePayload {
    generated_at: DateTime<Utc>,
    counts: SnapshotCounts,
    running: Vec<RunningEntryPayload>,
    retrying: Vec<RetrySnapshot>,
    codex_totals: CodexTotalsPayload,
    rate_limits: Option<serde_json::Value>,
    polling: PollingSnapshot,
    workflow: WorkflowPayload,
}

#[derive(Clone, Debug, Serialize)]
struct RunningEntryPayload {
    issue_id: String,
    issue_identifier: String,
    state: String,
    session_id: Option<String>,
    turn_count: u32,
    last_event: Option<String>,
    last_message: Option<String>,
    started_at: DateTime<Utc>,
    last_event_at: Option<DateTime<Utc>>,
    workspace: String,
    tokens: TokenSnapshot,
}

#[derive(Clone, Debug, Serialize)]
struct CodexTotalsPayload {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    seconds_running: f64,
}

#[derive(Clone, Debug, Serialize)]
struct WorkflowPayload {
    path: String,
    dispatch_status: &'static str,
    blocking_reason: Option<String>,
    using_last_good: bool,
}

#[derive(Clone, Debug, Serialize)]
struct IssuePayload {
    issue_identifier: String,
    issue_id: Option<String>,
    status: String,
    workspace: IssueWorkspacePayload,
    attempts: IssueAttemptsPayload,
    running: Option<IssueRunningPayload>,
    retry: Option<IssueRetryPayload>,
    logs: IssueLogsPayload,
    recent_events: Vec<RecentEventPayload>,
    last_error: Option<String>,
    tracked: Value,
}

#[derive(Clone, Debug, Serialize)]
struct IssueWorkspacePayload {
    path: String,
}

#[derive(Clone, Debug, Serialize)]
struct IssueAttemptsPayload {
    restart_count: u32,
    current_retry_attempt: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
struct IssueRunningPayload {
    session_id: Option<String>,
    turn_count: u32,
    state: String,
    started_at: DateTime<Utc>,
    last_event: Option<String>,
    last_message: Option<String>,
    last_event_at: Option<DateTime<Utc>>,
    tokens: TokenSnapshot,
}

#[derive(Clone, Debug, Serialize)]
struct IssueRetryPayload {
    attempt: u32,
    due_at: DateTime<Utc>,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct IssueLogsPayload {
    codex_session_logs: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
struct RecentEventPayload {
    at: DateTime<Utc>,
    event: String,
    message: Option<String>,
}

pub struct HttpServer {
    join: JoinHandle<()>,
    local_addr: SocketAddr,
}

impl HttpServer {
    pub async fn start(
        orchestrator: OrchestratorHandle,
        workflow_store: WorkflowStore,
        host: &str,
        port: u16,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind(format!("{host}:{port}"))
            .await
            .map_err(|error| error.to_string())?;
        let local_addr = listener.local_addr().map_err(|error| error.to_string())?;
        let app = app_router(AppState {
            orchestrator,
            workflow_store,
        });

        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { join, local_addr })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn abort(self) {
        self.join.abort();
    }
}

fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard).fallback(api_method_not_allowed))
        .route(
            "/api/v1/state",
            get(api_state).fallback(api_method_not_allowed),
        )
        .route(ISSUE_ROUTE, get(api_issue).fallback(api_method_not_allowed))
        .route(
            "/api/v1/refresh",
            post(api_refresh).fallback(api_method_not_allowed),
        )
        .fallback(api_not_found)
        .with_state(state)
}

async fn dashboard(State(state): State<AppState>) -> impl IntoResponse {
    match state.orchestrator.snapshot().await {
        Some(snapshot) => {
            let payload = state_payload(snapshot, &state.workflow_store);
            Html(render_dashboard(&payload)).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "code": "snapshot_unavailable", "message": "snapshot unavailable" } })),
        )
            .into_response(),
    }
}

async fn api_state(State(state): State<AppState>) -> impl IntoResponse {
    match state.orchestrator.snapshot().await {
        Some(snapshot) => Json(state_payload(snapshot, &state.workflow_store)).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "code": "snapshot_unavailable", "message": "snapshot unavailable" } })),
        )
            .into_response(),
    }
}

async fn api_issue(
    State(state): State<AppState>,
    AxumPath(identifier): AxumPath<String>,
) -> impl IntoResponse {
    match state.orchestrator.issue_detail(identifier).await {
        Some(detail) => Json(present_issue_detail(detail)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "code": "issue_not_found", "message": "issue not found" } })),
        )
            .into_response(),
    }
}

async fn api_refresh(State(state): State<AppState>) -> impl IntoResponse {
    match state.orchestrator.refresh().await {
        Some(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "code": "refresh_unavailable", "message": "refresh unavailable" } })),
        )
            .into_response(),
    }
}

async fn api_method_not_allowed() -> impl IntoResponse {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({ "error": { "code": "method_not_allowed", "message": "Method not allowed" } })),
    )
        .into_response()
}

async fn api_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "code": "not_found", "message": "Route not found" } })),
    )
        .into_response()
}

fn state_payload(snapshot: Snapshot, workflow_store: &WorkflowStore) -> StatePayload {
    let validation_error = workflow_store.validation_error();
    let blocking_reason = validation_error.clone().or_else(|| {
        workflow_store
            .effective()
            .config
            .validate_dispatch_ready()
            .err()
            .map(|error| error.to_string())
    });

    StatePayload {
        generated_at: snapshot.generated_at,
        counts: snapshot.counts,
        running: snapshot
            .running
            .into_iter()
            .map(|entry| {
                let last_event = entry.last_event.clone();
                RunningEntryPayload {
                    issue_id: entry.issue_id,
                    issue_identifier: entry.issue_identifier,
                    state: entry.state,
                    session_id: entry.session_id,
                    turn_count: entry.turn_count,
                    last_event,
                    last_message: summarize_codex_message(
                        entry.last_event.as_deref(),
                        entry.last_message.as_deref(),
                    ),
                    started_at: entry.started_at,
                    last_event_at: entry.last_event_at,
                    workspace: entry.workspace,
                    tokens: entry.tokens,
                }
            })
            .collect(),
        retrying: snapshot.retrying,
        codex_totals: CodexTotalsPayload {
            input_tokens: snapshot.codex_totals.input_tokens,
            output_tokens: snapshot.codex_totals.output_tokens,
            total_tokens: snapshot.codex_totals.total_tokens,
            seconds_running: snapshot.codex_totals.seconds_running,
        },
        rate_limits: snapshot.rate_limits,
        polling: snapshot.polling,
        workflow: WorkflowPayload {
            path: workflow_store.path().display().to_string(),
            dispatch_status: if blocking_reason.is_some() {
                "blocked"
            } else {
                "ready"
            },
            blocking_reason,
            using_last_good: validation_error.is_some(),
        },
    }
}

fn render_dashboard(payload: &StatePayload) -> String {
    let workflow_tone = if payload.workflow.dispatch_status == "ready" {
        "ok"
    } else {
        "blocked"
    };
    let workflow_reason = payload
        .workflow
        .blocking_reason
        .as_deref()
        .map(humanize_blocking_reason)
        .unwrap_or("Workflow and dispatch configuration are healthy.");
    let rate_limits = payload
        .rate_limits
        .as_ref()
        .map(pretty_json)
        .unwrap_or_else(|| "No rate-limit payload observed yet.".to_string());
    let running_rows = if payload.running.is_empty() {
        "<tr><td colspan=\"7\" class=\"empty\">No active sessions.</td></tr>".to_string()
    } else {
        payload
            .running
            .iter()
            .map(|entry| {
                format!(
                    "<tr>\
                        <td><strong>{}</strong></td>\
                        <td><span class=\"state state-{}\">{}</span></td>\
                        <td>{}</td>\
                        <td>{}</td>\
                        <td>{}</td>\
                        <td>{}</td>\
                        <td class=\"mono\">{}</td>\
                    </tr>",
                    escape_html(&entry.issue_identifier),
                    slugify(&entry.state),
                    escape_html(&entry.state),
                    escape_html(entry.session_id.as_deref().unwrap_or("n/a")),
                    entry.turn_count,
                    render_last_activity(entry),
                    escape_html(&format!(
                        "{} total / {} in / {} out",
                        format_int(entry.tokens.total_tokens),
                        format_int(entry.tokens.input_tokens),
                        format_int(entry.tokens.output_tokens)
                    ),),
                    escape_html(&entry.workspace),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };
    let retry_rows = if payload.retrying.is_empty() {
        "<tr><td colspan=\"4\" class=\"empty\">Retry queue is empty.</td></tr>".to_string()
    } else {
        payload
            .retrying
            .iter()
            .map(|entry| {
                format!(
                    "<tr>\
                        <td><strong>{}</strong></td>\
                        <td>{}</td>\
                        <td>{}</td>\
                        <td>{}</td>\
                    </tr>",
                    escape_html(&entry.issue_identifier),
                    entry.attempt,
                    escape_html(&entry.due_at.to_rfc3339()),
                    escape_html(entry.error.as_deref().unwrap_or("none")),
                )
            })
            .collect::<Vec<_>>()
            .join("")
    };

    format!(
        "<!doctype html>\
        <html lang=\"en\">\
          <head>\
            <meta charset=\"utf-8\">\
            <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
            <title>Symphony Observability</title>\
            <style>\
              :root {{\
                --bg: #f3efe6;\
                --panel: #fffaf2;\
                --panel-strong: #fffdf8;\
                --ink: #1f1b16;\
                --muted: #6a6155;\
                --accent: #1e6b52;\
                --accent-soft: #d9efe6;\
                --warn: #9d3d12;\
                --warn-soft: #f6dfd4;\
                --line: rgba(31, 27, 22, 0.12);\
                --shadow: 0 18px 48px rgba(78, 59, 31, 0.08);\
              }}\
              * {{ box-sizing: border-box; }}\
              body {{ margin: 0; font-family: \"Avenir Next\", \"Segoe UI\", sans-serif; background: radial-gradient(circle at top left, #fff7e4 0, var(--bg) 42%, #efe6d7 100%); color: var(--ink); }}\
              .shell {{ max-width: 1280px; margin: 0 auto; padding: 32px 20px 48px; }}\
              .hero {{ display: grid; grid-template-columns: 1.5fr 1fr; gap: 18px; margin-bottom: 18px; }}\
              .card {{ background: var(--panel); border: 1px solid var(--line); border-radius: 22px; padding: 22px; box-shadow: var(--shadow); }}\
              .hero h1 {{ margin: 0 0 8px; font-family: \"Iowan Old Style\", \"Palatino Linotype\", Georgia, serif; font-size: 2.2rem; line-height: 1.05; }}\
              .eyebrow {{ text-transform: uppercase; letter-spacing: 0.12em; font-size: 0.74rem; color: var(--muted); margin: 0 0 12px; }}\
              .hero p, .meta, .muted {{ color: var(--muted); }}\
              .pill {{ display: inline-flex; align-items: center; gap: 8px; border-radius: 999px; padding: 8px 12px; font-weight: 600; font-size: 0.9rem; }}\
              .pill.ok {{ background: var(--accent-soft); color: var(--accent); }}\
              .pill.blocked {{ background: var(--warn-soft); color: var(--warn); }}\
              .status-stack {{ display: grid; gap: 10px; justify-items: end; }}\
              .status-chip {{ display: inline-flex; align-items: center; gap: 8px; border-radius: 999px; padding: 8px 12px; font-weight: 600; font-size: 0.9rem; }}\
              .status-chip-dot {{ width: 10px; height: 10px; border-radius: 999px; background: currentColor; opacity: 0.85; }}\
              .status-chip-online {{ background: var(--accent-soft); color: var(--accent); }}\
              .status-chip-offline {{ background: var(--warn-soft); color: var(--warn); }}\
              .status-chip-checking {{ background: #ece4d4; color: var(--ink); }}\
              .grid {{ display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 14px; margin-bottom: 18px; }}\
              .metric {{ background: var(--panel-strong); }}\
              .metric .value {{ font-size: 2rem; font-weight: 700; margin: 10px 0 6px; }}\
              .metric .label {{ color: var(--muted); font-size: 0.88rem; text-transform: uppercase; letter-spacing: 0.08em; }}\
              .section {{ margin-bottom: 18px; }}\
              .section h2 {{ margin: 0 0 8px; font-family: \"Iowan Old Style\", \"Palatino Linotype\", Georgia, serif; font-size: 1.4rem; }}\
              .section-head {{ display: flex; justify-content: space-between; gap: 12px; align-items: flex-start; margin-bottom: 14px; }}\
              .actions button {{ border: none; border-radius: 999px; padding: 10px 14px; background: var(--ink); color: white; cursor: pointer; font: inherit; }}\
              table {{ width: 100%; border-collapse: collapse; }}\
              th, td {{ text-align: left; padding: 12px 10px; border-top: 1px solid var(--line); vertical-align: top; }}\
              th {{ font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.08em; color: var(--muted); border-top: none; }}\
              .state {{ display: inline-block; border-radius: 999px; padding: 5px 10px; font-size: 0.82rem; font-weight: 600; background: #ece4d4; }}\
              .state-in-progress, .state-merging, .state-rework, .state-todo {{ background: #e8efe9; color: #255b43; }}\
              .activity-event {{ font-weight: 600; }}\
              .activity-message {{ margin-top: 4px; color: var(--muted); max-width: 34ch; }}\
              .activity-time {{ margin-top: 6px; color: var(--muted); font-size: 0.82rem; }}\
              .empty {{ color: var(--muted); padding: 18px 10px; }}\
              .mono, pre {{ font-family: \"SFMono-Regular\", Consolas, \"Liberation Mono\", Menlo, monospace; }}\
              pre {{ margin: 0; white-space: pre-wrap; word-break: break-word; background: #1f1b16; color: #f8f2e7; border-radius: 16px; padding: 16px; overflow-x: auto; }}\
              .workflow-note {{ margin-top: 10px; color: var(--muted); font-size: 0.92rem; }}\
              .meta-grid {{ display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }}\
              @media (max-width: 960px) {{\
                .hero, .grid, .meta-grid {{ grid-template-columns: 1fr; }}\
                .section-head {{ flex-direction: column; }}\
              }}\
            </style>\
          </head>\
          <body>\
            <div class=\"shell\">\
              <section class=\"hero\">\
                <div class=\"card\">\
                  <p class=\"eyebrow\">Symphony Observability</p>\
                  <h1>Rust Runtime Dashboard</h1>\
                  <p>Active orchestration state, retry pressure, workflow health, and recent Codex activity for unattended runs.</p>\
                  <p class=\"meta\">Generated at {}</p>\
                </div>\
                <div class=\"card\">\
                  <div class=\"section-head\">\
                    <div>\
                      <p class=\"eyebrow\">Workflow Health</p>\
                      <div class=\"pill {}\">Dispatch {}</div>\
                    </div>\
                    <div class=\"status-stack\">\
                      <span id=\"runtime-status\" class=\"status-chip status-chip-checking\">\
                        <span class=\"status-chip-dot\"></span>\
                        <span id=\"runtime-status-label\">Checking</span>\
                      </span>\
                      <div class=\"actions\">\
                        <button type=\"button\" onclick=\"fetch('/api/v1/refresh', {{ method: 'POST' }}).then(() => location.reload())\">Refresh Now</button>\
                      </div>\
                    </div>\
                  </div>\
                  <div class=\"workflow-note\">{}</div>\
                  <div class=\"workflow-note mono\">{}</div>\
                  <div class=\"workflow-note\">Using last good config: {}</div>\
                  <div class=\"workflow-note\" id=\"runtime-status-copy\">Polling <span class=\"mono\">/api/v1/state</span> every 5s and refreshing every 15s while reachable.</div>\
                </div>\
              </section>\
              <section class=\"grid\">\
                <div class=\"card metric\">\
                  <div class=\"label\">Running</div>\
                  <div class=\"value\">{}</div>\
                  <div class=\"muted\">Active issue sessions.</div>\
                </div>\
                <div class=\"card metric\">\
                  <div class=\"label\">Retrying</div>\
                  <div class=\"value\">{}</div>\
                  <div class=\"muted\">Issues queued for continuation or backoff.</div>\
                </div>\
                <div class=\"card metric\">\
                  <div class=\"label\">Total Tokens</div>\
                  <div class=\"value\">{}</div>\
                  <div class=\"muted\">{} input / {} output</div>\
                </div>\
                <div class=\"card metric\">\
                  <div class=\"label\">Polling</div>\
                  <div class=\"value\">{}</div>\
                  <div class=\"muted\">Next poll in {}</div>\
                </div>\
              </section>\
              <section class=\"card section\">\
                <div class=\"section-head\">\
                  <div>\
                    <h2>Running Sessions</h2>\
                    <p class=\"muted\">Current issue threads, turns, last activity summary, and workspace path.</p>\
                  </div>\
                </div>\
                <table>\
                  <thead>\
                    <tr><th>Issue</th><th>State</th><th>Session</th><th>Turns</th><th>Last Activity</th><th>Tokens</th><th>Workspace</th></tr>\
                  </thead>\
                  <tbody>{}</tbody>\
                </table>\
              </section>\
              <section class=\"card section\">\
                <div class=\"section-head\">\
                  <div>\
                    <h2>Retry Queue</h2>\
                    <p class=\"muted\">Pending retries with due time and last known error.</p>\
                  </div>\
                </div>\
                <table>\
                  <thead>\
                    <tr><th>Issue</th><th>Attempt</th><th>Due At</th><th>Error</th></tr>\
                  </thead>\
                  <tbody>{}</tbody>\
                </table>\
              </section>\
              <section class=\"meta-grid\">\
                <div class=\"card section\">\
                  <h2>Rate Limits</h2>\
                  <pre>{}</pre>\
                </div>\
                <div class=\"card section\">\
                  <h2>Polling State</h2>\
                  <pre>{}</pre>\
                </div>\
              </section>\
            </div>\
            <script>\
              (() => {{\
                const badge = document.getElementById('runtime-status');\
                const label = document.getElementById('runtime-status-label');\
                const copy = document.getElementById('runtime-status-copy');\
                let online = false;\
                async function checkState() {{\
                  try {{\
                    const response = await fetch('/api/v1/state', {{ cache: 'no-store' }});\
                    if (!response.ok) throw new Error(`status ${{response.status}}`);\
                    online = true;\
                    badge.className = 'status-chip status-chip-online';\
                    label.textContent = 'Live';\
                    copy.innerHTML = 'Polling <span class=\"mono\">/api/v1/state</span> every 5s and refreshing every 15s while reachable.';\
                  }} catch (_error) {{\
                    online = false;\
                    badge.className = 'status-chip status-chip-offline';\
                    label.textContent = 'Offline';\
                    copy.innerHTML = 'Unable to reach <span class=\"mono\">/api/v1/state</span>. Retrying every 5s.';\
                  }}\
                }}\
                checkState();\
                window.setInterval(checkState, 5000);\
                window.setInterval(() => {{\
                  if (online) {{\
                    window.location.reload();\
                  }}\
                }}, 15000);\
              }})();\
            </script>\
          </body>\
        </html>",
        escape_html(&payload.generated_at.to_rfc3339()),
        workflow_tone,
        escape_html(payload.workflow.dispatch_status),
        escape_html(workflow_reason),
        escape_html(&payload.workflow.path),
        if payload.workflow.using_last_good {
            "yes"
        } else {
            "no"
        },
        payload.counts.running,
        payload.counts.retrying,
        format_int(payload.codex_totals.total_tokens),
        format_int(payload.codex_totals.input_tokens),
        format_int(payload.codex_totals.output_tokens),
        if payload.polling.checking {
            "Checking now".to_string()
        } else {
            format!("every {} ms", format_int(payload.polling.poll_interval_ms))
        },
        payload
            .polling
            .next_poll_in_ms
            .map(|value| format!("{} ms", format_int(value)))
            .unwrap_or_else(|| "n/a".to_string()),
        running_rows,
        retry_rows,
        escape_html(&rate_limits),
        escape_html(&pretty_json(&payload.polling)),
    )
}

fn pretty_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
}

fn present_issue_detail(detail: IssueDetail) -> IssuePayload {
    IssuePayload {
        issue_identifier: detail.issue_identifier,
        issue_id: detail.issue_id,
        status: detail.status,
        workspace: IssueWorkspacePayload {
            path: detail.workspace.path,
        },
        attempts: IssueAttemptsPayload {
            restart_count: detail.attempts.restart_count,
            current_retry_attempt: detail.attempts.current_retry_attempt,
        },
        running: detail.running.map(|running| {
            let last_event = running.last_event.clone();
            IssueRunningPayload {
                session_id: running.session_id,
                turn_count: running.turn_count,
                state: running.state,
                started_at: running.started_at,
                last_event: last_event.clone(),
                last_message: summarize_codex_message(
                    last_event.as_deref(),
                    running.last_message.as_deref(),
                ),
                last_event_at: running.last_event_at,
                tokens: running.tokens,
            }
        }),
        retry: detail.retry.map(|retry| IssueRetryPayload {
            attempt: retry.attempt,
            due_at: retry.due_at,
            error: retry.error,
        }),
        logs: IssueLogsPayload {
            codex_session_logs: Vec::new(),
        },
        recent_events: detail
            .recent_events
            .into_iter()
            .map(present_recent_event)
            .collect(),
        last_error: detail.last_error,
        tracked: json!({}),
    }
}

fn present_recent_event(event: RecentEvent) -> RecentEventPayload {
    let recent_event = event.event;
    RecentEventPayload {
        at: event.at,
        event: recent_event.clone(),
        message: summarize_codex_message(Some(&recent_event), event.message.as_deref()),
    }
}

fn render_last_activity(entry: &RunningEntryPayload) -> String {
    let event = escape_html(entry.last_event.as_deref().unwrap_or("n/a"));
    let message = escape_html(
        entry
            .last_message
            .as_deref()
            .unwrap_or("No Codex message yet."),
    );
    let at = entry
        .last_event_at
        .map(|timestamp| escape_html(&timestamp.to_rfc3339()))
        .unwrap_or_else(|| "n/a".to_string());

    format!(
        "<div class=\"activity-event\">{event}</div>\
         <div class=\"activity-message\">{message}</div>\
         <div class=\"activity-time mono\">{at}</div>"
    )
}

fn summarize_codex_message(event: Option<&str>, message: Option<&str>) -> Option<String> {
    if event.is_none() && message.is_none() {
        return None;
    }

    let parsed = message.and_then(parse_json_message);
    let payload = parsed.as_ref().map(unwrap_codex_message_payload);

    let summary = event
        .and_then(|event_name| humanize_codex_event(event_name, parsed.as_ref(), payload))
        .or_else(|| payload.map(humanize_codex_payload))
        .or_else(|| message.map(inline_text))
        .or_else(|| event.map(humanize_event_name));

    summary.map(|value| truncate_chars(&value, 140))
}

fn parse_json_message(message: &str) -> Option<Value> {
    serde_json::from_str(message).ok()
}

fn unwrap_codex_message_payload(value: &Value) -> &Value {
    if value.as_object().is_some_and(|map| {
        map.contains_key("method") || map.contains_key("session_id") || map.contains_key("reason")
    }) {
        value
    } else {
        value.get("payload").unwrap_or(value)
    }
}

fn humanize_codex_event(
    event: &str,
    message: Option<&Value>,
    payload: Option<&Value>,
) -> Option<String> {
    let payload = payload.or(message);
    match event {
        "session_started" => {
            let session_id =
                payload.and_then(|value| json_text(value, &[&["session_id"], &["sessionId"]]));
            Some(match session_id {
                Some(session_id) => format!("session started ({session_id})"),
                None => "session started".to_string(),
            })
        }
        "turn_input_required" => Some("turn blocked: waiting for user input".to_string()),
        "approval_auto_approved" => {
            let method = payload.and_then(|value| json_text(value, &[&["method"]]));
            let decision = message.and_then(|value| json_text(value, &[&["decision"]]));
            let base = method
                .map(|method| format!("{} (auto-approved)", humanize_codex_method(method, payload)))
                .unwrap_or_else(|| "approval request auto-approved".to_string());
            Some(match decision {
                Some(decision) => format!("{base}: {}", inline_text(decision)),
                None => base,
            })
        }
        "tool_input_auto_answered" => {
            let answer = message.and_then(|value| json_text(value, &[&["answer"]]));
            let base = format!(
                "{} (auto-answered)",
                humanize_codex_method("item/tool/requestUserInput", payload)
            );
            Some(match answer {
                Some(answer) => format!("{base}: {}", inline_text(answer)),
                None => base,
            })
        }
        "tool_call_completed" => Some(humanize_dynamic_tool_event(
            "dynamic tool call completed",
            payload,
        )),
        "tool_call_failed" => Some(humanize_dynamic_tool_event(
            "dynamic tool call failed",
            payload,
        )),
        "unsupported_tool_call" => Some(humanize_dynamic_tool_event(
            "unsupported dynamic tool call rejected",
            payload,
        )),
        "turn_ended_with_error" => Some(format!(
            "turn ended with error: {}",
            format_reason(message, payload)
        )),
        "startup_failed" => Some(format!(
            "startup failed: {}",
            format_reason(message, payload)
        )),
        "turn_failed" => Some(humanize_codex_method("turn/failed", payload)),
        "turn_cancelled" => Some("turn cancelled".to_string()),
        "malformed" => Some("malformed JSON event from codex".to_string()),
        _ => None,
    }
}

fn humanize_codex_payload(payload: &Value) -> String {
    if let Some(method) = json_text(payload, &[&["method"]]) {
        humanize_codex_method(method, Some(payload))
    } else if let Some(session_id) = json_text(payload, &[&["session_id"], &["sessionId"]]) {
        format!("session started ({session_id})")
    } else if let Some(error) = payload.get("error") {
        format!("error: {}", format_error_value(error))
    } else if let Some(text) = payload.as_str() {
        inline_text(text)
    } else {
        truncate_chars(&inline_text(&pretty_json(payload)), 140)
    }
}

fn humanize_codex_method(method: &str, payload: Option<&Value>) -> String {
    match method {
        "thread/started" => match payload.and_then(|value| {
            json_text(
                value,
                &[
                    &["params", "thread", "id"],
                    &["params", "threadId"],
                    &["thread_id"],
                ],
            )
        }) {
            Some(thread_id) => format!("thread started ({thread_id})"),
            None => "thread started".to_string(),
        },
        "turn/started" => match payload.and_then(|value| {
            json_text(
                value,
                &[
                    &["params", "turn", "id"],
                    &["params", "turnId"],
                    &["turn_id"],
                ],
            )
        }) {
            Some(turn_id) => format!("turn started ({turn_id})"),
            None => "turn started".to_string(),
        },
        "turn/completed" => {
            let status = payload
                .and_then(|value| {
                    json_text(
                        value,
                        &[
                            &["params", "turn", "status"],
                            &["params", "status"],
                            &["status"],
                        ],
                    )
                })
                .unwrap_or("completed");
            let usage_suffix = payload
                .and_then(|value| {
                    json_value(
                        value,
                        &[
                            &["params", "usage"],
                            &["params", "tokenUsage"],
                            &["usage"],
                            &["tokenUsage"],
                        ],
                    )
                })
                .and_then(format_usage_counts)
                .map(|usage| format!(" ({usage})"))
                .unwrap_or_default();
            format!("turn completed ({status}){usage_suffix}")
        }
        "turn/failed" => match payload.and_then(|value| {
            json_text(
                value,
                &[&["params", "error", "message"], &["error", "message"]],
            )
        }) {
            Some(error) => format!("turn failed: {}", inline_text(error)),
            None => "turn failed".to_string(),
        },
        "turn/cancelled" => "turn cancelled".to_string(),
        "turn/diff/updated" => {
            let line_count = payload
                .and_then(|value| json_text(value, &[&["params", "diff"], &["diff"]]))
                .map(|diff| diff.lines().filter(|line| !line.trim().is_empty()).count())
                .unwrap_or(0);
            if line_count > 0 {
                format!("turn diff updated ({line_count} lines)")
            } else {
                "turn diff updated".to_string()
            }
        }
        "turn/plan/updated" => {
            let plan_len = payload
                .and_then(|value| {
                    json_value(
                        value,
                        &[
                            &["params", "plan"],
                            &["params", "steps"],
                            &["params", "items"],
                            &["plan"],
                            &["steps"],
                            &["items"],
                        ],
                    )
                })
                .and_then(Value::as_array)
                .map(|entries| entries.len());
            match plan_len {
                Some(length) => format!("plan updated ({length} steps)"),
                None => "plan updated".to_string(),
            }
        }
        "thread/tokenUsage/updated" => {
            let usage = payload.and_then(|value| {
                json_value(
                    value,
                    &[
                        &["params", "tokenUsage", "total"],
                        &["params", "tokenUsage"],
                        &["tokenUsage", "total"],
                        &["tokenUsage"],
                        &["usage"],
                    ],
                )
            });
            match usage.and_then(format_usage_counts) {
                Some(usage) => format!("thread token usage updated ({usage})"),
                None => "thread token usage updated".to_string(),
            }
        }
        "item/started" => humanize_item_lifecycle("started", payload),
        "item/completed" => humanize_item_lifecycle("completed", payload),
        "item/agentMessage/delta" => humanize_streaming_event("agent message streaming", payload),
        "item/plan/delta" => humanize_streaming_event("plan streaming", payload),
        "item/reasoning/summaryTextDelta" => {
            humanize_streaming_event("reasoning summary streaming", payload)
        }
        "item/reasoning/summaryPartAdded" => {
            humanize_streaming_event("reasoning summary section added", payload)
        }
        "item/reasoning/textDelta" => humanize_streaming_event("reasoning text streaming", payload),
        "item/commandExecution/outputDelta" => {
            humanize_streaming_event("command output streaming", payload)
        }
        "item/fileChange/outputDelta" => {
            humanize_streaming_event("file change output streaming", payload)
        }
        "item/commandExecution/requestApproval" => match payload.and_then(extract_command) {
            Some(command) => format!("command approval requested ({command})"),
            None => "command approval requested".to_string(),
        },
        "item/fileChange/requestApproval" => {
            let change_count = payload
                .and_then(|value| {
                    json_integer(
                        value,
                        &[&["params", "fileChangeCount"], &["params", "changeCount"]],
                    )
                })
                .unwrap_or(0);
            if change_count > 0 {
                format!("file change approval requested ({change_count} files)")
            } else {
                "file change approval requested".to_string()
            }
        }
        "item/tool/requestUserInput" | "tool/requestUserInput" => {
            let question = payload.and_then(|value| {
                json_text(
                    value,
                    &[
                        &["params", "question"],
                        &["params", "prompt"],
                        &["question"],
                        &["prompt"],
                    ],
                )
            });
            match question {
                Some(question) => format!("tool requires user input: {}", inline_text(question)),
                None => "tool requires user input".to_string(),
            }
        }
        "account/updated" => {
            let auth_mode = payload
                .and_then(|value| json_text(value, &[&["params", "authMode"], &["authMode"]]))
                .unwrap_or("unknown");
            format!("account updated (auth {auth_mode})")
        }
        "account/rateLimits/updated" => {
            let rate_limits = payload
                .and_then(|value| json_value(value, &[&["params", "rateLimits"], &["rateLimits"]]));
            format!(
                "rate limits updated: {}",
                rate_limits
                    .map(format_rate_limits_summary)
                    .unwrap_or_else(|| "n/a".to_string())
            )
        }
        "account/chatgptAuthTokens/refresh" => "account auth token refresh requested".to_string(),
        "item/tool/call" => match payload.and_then(dynamic_tool_name) {
            Some(tool) => format!("dynamic tool call requested ({tool})"),
            None => "dynamic tool call requested".to_string(),
        },
        other if other.starts_with("codex/event/") => {
            humanize_codex_wrapper_event(other.trim_start_matches("codex/event/"), payload)
        }
        other => match payload.and_then(|value| json_text(value, &[&["params", "msg", "type"]])) {
            Some(msg_type) => format!("{other} ({msg_type})"),
            None => other.to_string(),
        },
    }
}

fn humanize_dynamic_tool_event(base: &str, payload: Option<&Value>) -> String {
    match payload.and_then(dynamic_tool_name) {
        Some(tool) => format!("{base} ({tool})"),
        None => base.to_string(),
    }
}

fn dynamic_tool_name(payload: &Value) -> Option<String> {
    json_text(
        payload,
        &[
            &["params", "tool"],
            &["params", "name"],
            &["tool"],
            &["name"],
        ],
    )
    .map(inline_text)
    .filter(|value| !value.is_empty())
}

fn humanize_item_lifecycle(state: &str, payload: Option<&Value>) -> String {
    let item = payload.and_then(|value| json_value(value, &[&["params", "item"], &["item"]]));
    let item_type = item
        .and_then(|value| json_text(value, &[&["type"]]))
        .map(humanize_item_type)
        .unwrap_or_else(|| "item".to_string());
    let item_status = item
        .and_then(|value| json_text(value, &[&["status"]]))
        .map(humanize_status);
    let item_id = item
        .and_then(|value| json_text(value, &[&["id"]]))
        .map(short_id);

    let mut details = Vec::new();
    if let Some(item_id) = item_id {
        details.push(item_id);
    }
    if let Some(item_status) = item_status {
        details.push(item_status);
    }

    if details.is_empty() {
        format!("item {state}: {item_type}")
    } else {
        format!("item {state}: {item_type} ({})", details.join(", "))
    }
}

fn humanize_codex_wrapper_event(event: &str, payload: Option<&Value>) -> String {
    match event {
        "mcp_startup_update" => {
            let server = payload
                .and_then(|value| json_text(value, &[&["params", "msg", "server"]]))
                .unwrap_or("mcp");
            let state = payload
                .and_then(|value| json_text(value, &[&["params", "msg", "status", "state"]]))
                .unwrap_or("updated");
            format!("mcp startup: {server} {state}")
        }
        "mcp_startup_complete" => "mcp startup complete".to_string(),
        "task_started" => "task started".to_string(),
        "user_message" => "user message received".to_string(),
        "item_started" => match payload.and_then(wrapper_payload_type) {
            Some(kind) => format!("item started ({})", humanize_item_type(&kind)),
            None => "item started".to_string(),
        },
        "item_completed" => match payload.and_then(wrapper_payload_type) {
            Some(kind) => format!("item completed ({})", humanize_item_type(&kind)),
            None => "item completed".to_string(),
        },
        "agent_message_delta" => humanize_streaming_event("agent message streaming", payload),
        "agent_message_content_delta" => {
            humanize_streaming_event("agent message content streaming", payload)
        }
        "agent_reasoning_delta" => humanize_streaming_event("reasoning streaming", payload),
        "reasoning_content_delta" => {
            humanize_streaming_event("reasoning content streaming", payload)
        }
        "agent_reasoning_section_break" => "reasoning section break".to_string(),
        "turn_diff" => "turn diff updated".to_string(),
        "exec_command_begin" => payload
            .and_then(extract_command)
            .unwrap_or_else(|| "command started".to_string()),
        "exec_command_end" => {
            let exit_code = payload.and_then(|value| {
                json_integer(
                    value,
                    &[
                        &["params", "msg", "exit_code"],
                        &["params", "msg", "exitCode"],
                        &["exit_code"],
                        &["exitCode"],
                    ],
                )
            });
            match exit_code {
                Some(code) => format!("command completed (exit {code})"),
                None => "command completed".to_string(),
            }
        }
        "exec_command_output_delta" => "command output streaming".to_string(),
        "mcp_tool_call_begin" => "mcp tool call started".to_string(),
        "mcp_tool_call_end" => "mcp tool call completed".to_string(),
        "token_count" => {
            let usage = payload.and_then(|value| {
                json_value(
                    value,
                    &[
                        &["params", "msg", "payload", "info", "total_token_usage"],
                        &["params", "msg", "info", "total_token_usage"],
                        &["params", "tokenUsage", "total"],
                        &["tokenUsage", "total"],
                    ],
                )
            });
            match usage.and_then(format_usage_counts) {
                Some(usage) => format!("token count update ({usage})"),
                None => "token count update".to_string(),
            }
        }
        other => match payload.and_then(|value| json_text(value, &[&["params", "msg", "type"]])) {
            Some(msg_type) => format!("{other} ({msg_type})"),
            None => other.to_string(),
        },
    }
}

fn humanize_streaming_event(label: &str, payload: Option<&Value>) -> String {
    match payload.and_then(extract_delta_preview) {
        Some(preview) => format!("{label}: {preview}"),
        None => label.to_string(),
    }
}

fn extract_delta_preview(payload: &Value) -> Option<String> {
    json_text(
        payload,
        &[
            &["params", "delta"],
            &["params", "text"],
            &["params", "chunk"],
            &["params", "msg", "delta"],
            &["params", "msg", "text"],
            &["params", "msg", "content"],
            &["params", "msg", "payload", "delta"],
            &["params", "msg", "payload", "text"],
        ],
    )
    .map(inline_text)
    .filter(|value| !value.is_empty())
}

fn extract_command(payload: &Value) -> Option<String> {
    json_value(
        payload,
        &[
            &["params", "parsedCmd"],
            &["params", "command"],
            &["params", "cmd"],
            &["params", "argv"],
            &["params", "args"],
            &["params", "msg", "command"],
            &["params", "msg", "parsed_cmd"],
            &["params", "msg", "parsedCmd"],
        ],
    )
    .and_then(normalize_command)
}

fn normalize_command(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let command = inline_text(text);
            if command.is_empty() {
                None
            } else {
                Some(command)
            }
        }
        Value::Array(items) => {
            let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if parts.len() == items.len() && !parts.is_empty() {
                Some(inline_text(&parts.join(" ")))
            } else {
                None
            }
        }
        Value::Object(_) => {
            let binary = json_text(value, &[&["parsedCmd"], &["command"], &["cmd"]]);
            let args = json_value(value, &[&["args"], &["argv"]]).and_then(Value::as_array);
            match (binary, args) {
                (Some(binary), Some(args)) => {
                    let mut parts = vec![binary.to_string()];
                    for arg in args {
                        parts.push(arg.as_str()?.to_string());
                    }
                    Some(inline_text(&parts.join(" ")))
                }
                (Some(binary), None) => Some(inline_text(binary)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn wrapper_payload_type(payload: &Value) -> Option<String> {
    json_text(payload, &[&["params", "msg", "payload", "type"]]).map(|value| value.to_string())
}

fn format_usage_counts(usage: &Value) -> Option<String> {
    let input = parse_integer(json_value(
        usage,
        &[
            &["input_tokens"],
            &["prompt_tokens"],
            &["inputTokens"],
            &["promptTokens"],
        ],
    ));
    let output = parse_integer(json_value(
        usage,
        &[
            &["output_tokens"],
            &["completion_tokens"],
            &["outputTokens"],
            &["completionTokens"],
        ],
    ));
    let total = parse_integer(json_value(
        usage,
        &[&["total_tokens"], &["total"], &["totalTokens"]],
    ));

    let mut parts = Vec::new();
    append_usage_part(&mut parts, "in", input);
    append_usage_part(&mut parts, "out", output);
    append_usage_part(&mut parts, "total", total);

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn append_usage_part(parts: &mut Vec<String>, label: &str, value: Option<u64>) {
    if let Some(value) = value {
        parts.push(format!("{label} {}", format_int(value)));
    }
}

fn format_rate_limits_summary(rate_limits: &Value) -> String {
    let primary =
        json_value(rate_limits, &[&["primary"]]).and_then(format_rate_limit_bucket_summary);
    let secondary =
        json_value(rate_limits, &[&["secondary"]]).and_then(format_rate_limit_bucket_summary);

    match (primary, secondary) {
        (Some(primary), Some(secondary)) => format!("primary {primary}; secondary {secondary}"),
        (Some(primary), None) => format!("primary {primary}"),
        (None, Some(secondary)) => format!("secondary {secondary}"),
        (None, None) => "n/a".to_string(),
    }
}

fn format_rate_limit_bucket_summary(bucket: &Value) -> Option<String> {
    let used_percent = json_number(bucket, &[&["usedPercent"], &["used_percent"]]);
    let window_mins = json_integer(
        bucket,
        &[&["windowDurationMins"], &["window_duration_mins"]],
    );

    match (used_percent, window_mins) {
        (Some(used_percent), Some(window_mins)) => {
            Some(format!("{used_percent}% / {window_mins}m"))
        }
        (Some(used_percent), None) => Some(format!("{used_percent}% used")),
        _ => None,
    }
}

fn format_error_value(error: &Value) -> String {
    if let Some(message) = json_text(error, &[&["message"]]) {
        inline_text(message)
    } else if let Some(text) = error.as_str() {
        inline_text(text)
    } else {
        truncate_chars(&inline_text(&pretty_json(error)), 96)
    }
}

fn format_reason(message: Option<&Value>, payload: Option<&Value>) -> String {
    if let Some(reason) = message.and_then(|value| json_value(value, &[&["reason"]])) {
        format_error_value(reason)
    } else if let Some(reason) = payload.and_then(|value| json_value(value, &[&["reason"]])) {
        format_error_value(reason)
    } else if let Some(message) = message {
        truncate_chars(&inline_text(&pretty_json(message)), 96)
    } else {
        "unknown error".to_string()
    }
}

fn humanize_item_type(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    let mut previous_was_alnum = false;
    for ch in value.chars() {
        if ch == '_' || ch == '/' || ch == '-' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            previous_was_alnum = false;
        } else if ch.is_ascii_uppercase() && previous_was_alnum {
            out.push(' ');
            out.push(ch.to_ascii_lowercase());
            previous_was_alnum = true;
        } else {
            out.push(ch.to_ascii_lowercase());
            previous_was_alnum = ch.is_ascii_alphanumeric();
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn humanize_status(value: &str) -> String {
    value
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn short_id(value: &str) -> String {
    let short: String = value.chars().take(12).collect();
    if value.chars().count() > 12 {
        short
    } else {
        value.to_string()
    }
}

fn humanize_event_name(event: &str) -> String {
    event.replace('_', " ")
}

fn inline_text(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t' || *ch == '\r')
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, 80)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        value.to_string()
    }
}

fn parse_integer(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    if let Some(value) = value.as_u64() {
        Some(value)
    } else if let Some(value) = value.as_i64() {
        u64::try_from(value).ok()
    } else if let Some(value) = value.as_str() {
        value.trim().parse::<u64>().ok()
    } else {
        None
    }
}

fn json_integer(value: &Value, paths: &[&[&str]]) -> Option<i64> {
    json_value(value, paths).and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
            .or_else(|| {
                value
                    .as_str()
                    .and_then(|text| text.trim().parse::<i64>().ok())
            })
    })
}

fn json_number(value: &Value, paths: &[&[&str]]) -> Option<f64> {
    json_value(value, paths).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_i64().map(|value| value as f64))
            .or_else(|| value.as_u64().map(|value| value as f64))
            .or_else(|| {
                value
                    .as_str()
                    .and_then(|text| text.trim().parse::<f64>().ok())
            })
    })
}

fn json_text<'a>(value: &'a Value, paths: &[&[&str]]) -> Option<&'a str> {
    json_value(value, paths).and_then(Value::as_str)
}

fn json_value<'a>(value: &'a Value, paths: &[&[&str]]) -> Option<&'a Value> {
    paths.iter().find_map(|path| json_path(value, path))
}

fn json_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, segment| json_key(current, segment))
}

fn json_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Object(map) = value else {
        return None;
    };

    map.get(key).or_else(|| {
        let alternate = alternate_key(key);
        if alternate == key {
            None
        } else {
            map.get(alternate.as_str())
        }
    })
}

fn alternate_key(key: &str) -> String {
    if key.contains('_') {
        snake_to_camel(key)
    } else if key.chars().any(|ch| ch.is_ascii_uppercase()) {
        camel_to_snake(key)
    } else {
        key.to_string()
    }
}

fn snake_to_camel(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut uppercase_next = false;
    for ch in value.chars() {
        if ch == '_' {
            uppercase_next = true;
        } else if uppercase_next {
            out.push(ch.to_ascii_uppercase());
            uppercase_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn camel_to_snake(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for (index, ch) in value.chars().enumerate() {
        if ch.is_ascii_uppercase() && index > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

fn format_int(value: u64) -> String {
    let raw = value.to_string();
    let mut out = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().rev().enumerate() {
        if index != 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn slugify(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn humanize_blocking_reason(value: &str) -> &str {
    match value {
        "workflow_front_matter_not_a_map" => "workflow front matter must decode to a map",
        "missing_tracker_api_key" => "tracker api key is missing",
        "missing_tracker_project_slug" => "tracker project slug is missing",
        "missing_tracker_kind" => "tracker kind is missing",
        "missing_codex_command" => "codex command is missing",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use chrono::Utc;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    use crate::{
        orchestrator::{
            AttemptDetail, IssueDetail, Orchestrator, PollingSnapshot, RecentEvent, RetryDetail,
            RunningDetail, SnapshotCounts, TokenSnapshot, WorkspaceDetail,
        },
        workflow::WorkflowStore,
    };

    use super::{
        AppState, CodexTotalsPayload, ISSUE_ROUTE, RunningEntryPayload, StatePayload,
        WorkflowPayload, app_router, present_issue_detail, render_dashboard,
        summarize_codex_message,
    };

    #[test]
    fn issue_route_uses_supported_capture_syntax() {
        let _app: Router<()> = Router::new().route(ISSUE_ROUTE, get(|| async {}));
    }

    #[tokio::test]
    async fn api_state_includes_workflow_payload() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let app = app_router(AppState {
            orchestrator: orchestrator.handle(),
            workflow_store: workflow_store.clone(),
        });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["workflow"]["dispatch_status"], "ready");
        assert_eq!(payload["workflow"]["using_last_good"], false);

        orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn dashboard_shows_workflow_blocking_reason_when_invalid() {
        let (orchestrator, workflow_store) = test_runtime(true).await;
        let app = app_router(AppState {
            orchestrator: orchestrator.handle(),
            workflow_store: workflow_store.clone(),
        });

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(html.contains("Dispatch blocked"));
        assert!(html.contains("workflow front matter must decode to a map"));

        orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn api_refresh_returns_accepted_payload() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let app = app_router(AppState {
            orchestrator: orchestrator.handle(),
            workflow_store: workflow_store.clone(),
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["queued"], true);
        assert_eq!(payload["operations"], json!(["poll", "reconcile"]));

        orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn api_refresh_rejects_get_with_method_not_allowed_payload() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let app = app_router(AppState {
            orchestrator: orchestrator.handle(),
            workflow_store: workflow_store.clone(),
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["error"]["code"], "method_not_allowed");

        orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn api_unknown_route_returns_not_found_payload() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let app = app_router(AppState {
            orchestrator: orchestrator.handle(),
            workflow_store: workflow_store.clone(),
        });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/does/not/exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["error"]["code"], "not_found");

        orchestrator.shutdown().await;
    }

    #[test]
    fn summarize_codex_message_humanizes_turn_completed_usage() {
        let message = json!({
            "method": "turn/completed",
            "params": {
                "turn": { "status": "completed" },
                "usage": {
                    "inputTokens": 12,
                    "outputTokens": 3,
                    "totalTokens": 15
                }
            }
        })
        .to_string();

        assert_eq!(
            summarize_codex_message(None, Some(&message)).as_deref(),
            Some("turn completed (completed) (in 12, out 3, total 15)")
        );
    }

    #[test]
    fn present_issue_detail_matches_elixir_shape() {
        let now = Utc::now();
        let detail = IssueDetail {
            issue_identifier: "ABC-123".to_string(),
            issue_id: Some("issue-123".to_string()),
            status: "running".to_string(),
            workspace: WorkspaceDetail {
                path: "/tmp/symphony-http-tests/ABC-123".to_string(),
            },
            attempts: AttemptDetail {
                restart_count: 1,
                current_retry_attempt: Some(2),
            },
            running: Some(RunningDetail {
                session_id: Some("sess-123".to_string()),
                turn_count: 2,
                state: "In Progress".to_string(),
                started_at: now,
                last_event: Some("session_started".to_string()),
                last_message: Some(json!({ "session_id": "sess-123" }).to_string()),
                last_event_at: Some(now),
                tokens: TokenSnapshot {
                    input_tokens: 11,
                    output_tokens: 7,
                    total_tokens: 18,
                },
            }),
            retry: Some(RetryDetail {
                attempt: 2,
                due_at: now,
                error: Some("boom".to_string()),
            }),
            recent_events: vec![RecentEvent {
                at: now,
                event: "notification".to_string(),
                message: Some(
                    json!({
                        "method": "item/tool/requestUserInput",
                        "params": { "question": "Need approval?" }
                    })
                    .to_string(),
                ),
            }],
            last_error: Some("boom".to_string()),
        };

        let payload = serde_json::to_value(present_issue_detail(detail)).expect("json");
        assert_eq!(payload["logs"]["codex_session_logs"], json!([]));
        assert_eq!(payload["tracked"], json!({}));
        assert_eq!(
            payload["running"]["last_message"],
            "session started (sess-123)"
        );
        assert_eq!(
            payload["recent_events"][0]["message"],
            "tool requires user input: Need approval?"
        );
    }

    #[test]
    fn dashboard_renders_humanized_last_activity() {
        let payload = StatePayload {
            generated_at: Utc::now(),
            counts: SnapshotCounts {
                running: 1,
                retrying: 0,
            },
            running: vec![RunningEntryPayload {
                issue_id: "issue-123".to_string(),
                issue_identifier: "ABC-123".to_string(),
                state: "In Progress".to_string(),
                session_id: Some("sess-123".to_string()),
                turn_count: 2,
                last_event: Some("item/tool/requestUserInput".to_string()),
                last_message: Some("tool requires user input: Need approval?".to_string()),
                started_at: Utc::now(),
                last_event_at: Some(Utc::now()),
                workspace: "/tmp/symphony-http-tests/ABC-123".to_string(),
                tokens: TokenSnapshot {
                    input_tokens: 11,
                    output_tokens: 7,
                    total_tokens: 18,
                },
            }],
            retrying: Vec::new(),
            codex_totals: CodexTotalsPayload {
                input_tokens: 11,
                output_tokens: 7,
                total_tokens: 18,
                seconds_running: 12.5,
            },
            rate_limits: None,
            polling: PollingSnapshot {
                checking: false,
                next_poll_in_ms: Some(5000),
                poll_interval_ms: 5000,
            },
            workflow: WorkflowPayload {
                path: "/tmp/symphony-http-tests/WORKFLOW.md".to_string(),
                dispatch_status: "ready",
                blocking_reason: None,
                using_last_good: false,
            },
        };

        let html = render_dashboard(&payload);
        assert!(html.contains("Last Activity"));
        assert!(html.contains("tool requires user input: Need approval?"));
        assert!(html.contains("id=\"runtime-status\""));
        assert!(html.contains("window.setInterval(checkState, 5000)"));
    }

    async fn test_runtime(invalidate_workflow: bool) -> (Orchestrator, WorkflowStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        std::fs::write(
            &workflow_path,
            r#"---
tracker:
  kind: linear
  endpoint: "http://127.0.0.1:9/graphql"
  api_key: token
  project_slug: proj
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
workspace:
  root: /tmp/symphony-http-tests
---

test
"#,
        )
        .expect("workflow");
        let workflow_store = WorkflowStore::new(workflow_path.clone()).expect("store");
        if invalidate_workflow {
            std::fs::write(&workflow_path, "---\n- nope\n---\n").expect("invalid workflow");
            workflow_store.reload();
        }
        let orchestrator = Orchestrator::start(workflow_store.clone())
            .await
            .expect("orchestrator");
        (orchestrator, workflow_store)
    }
}
