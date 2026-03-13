use std::{
    collections::VecDeque,
    env,
    hash::{Hash, Hasher},
    io::{self, IsTerminal, Write},
    iter::Peekable,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    task::JoinHandle,
    time::{MissedTickBehavior, interval, sleep_until},
};

use crate::{
    config::ObservabilityConfig,
    issue::Issue,
    orchestrator::{
        IssueDetail, OrchestratorHandle, OrchestratorHandleError, PollingSnapshot, RecentEvent,
        RetrySnapshot, Snapshot, SnapshotCounts, TokenSnapshot,
    },
    tracker::TrackerRateBudget,
    workflow::WorkflowStore,
};

const THROUGHPUT_WINDOW_MS: i64 = 5_000;
const THROUGHPUT_GRAPH_WINDOW_MS: i64 = 10 * 60 * 1_000;
const THROUGHPUT_GRAPH_COLUMNS: usize = 24;
const SPARKLINE_BLOCKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const MINIMUM_IDLE_RERENDER_MS: u64 = 1_000;
const DEFAULT_TERMINAL_COLUMNS: usize = 132;
const DEFAULT_TERMINAL_ROWS: usize = 24;
const RUNNING_STATUS_WIDTH: usize = 1;
const RUNNING_ID_WIDTH: usize = 10;
const RUNNING_STATE_WIDTH: usize = 14;
const RUNNING_SESSION_WIDTH: usize = 14;
const RUNNING_PID_WIDTH: usize = 8;
const RUNNING_RUNTIME_WIDTH: usize = 14;
const RUNNING_TOKENS_WIDTH: usize = 10;
const RUNNING_ROW_CHROME_WIDTH: usize = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalViewport {
    columns: usize,
    rows: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatePayload {
    pub generated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
    pub counts: SnapshotCounts,
    pub agent_limits: AgentLimitsPayload,
    pub running: Vec<RunningEntryPayload>,
    pub retrying: Vec<RetrySnapshot>,
    pub codex_totals: CodexTotalsPayload,
    pub rate_limits: Option<Value>,
    pub todoist_rate_budget: Option<TrackerRateBudget>,
    pub polling: PollingSnapshot,
    pub workflow: WorkflowPayload,
    pub links: LinksPayload,
    pub throughput: ThroughputPayload,
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorPayload {
    pub code: &'static str,
    pub message: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentLimitsPayload {
    pub max_concurrent_agents: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunningEntryPayload {
    pub issue_id: String,
    pub issue_identifier: String,
    pub title: String,
    pub state: String,
    pub url: Option<String>,
    pub project_url: Option<String>,
    pub labels: Vec<String>,
    pub due: Option<Value>,
    pub deadline: Option<Value>,
    pub worker_host: Option<String>,
    pub session_id: Option<String>,
    pub app_server_pid: Option<u32>,
    pub turn_count: u32,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub runtime_seconds: f64,
    pub workspace: String,
    pub tokens: TokenSnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodexTotalsPayload {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub seconds_running: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkflowPayload {
    pub path: String,
    pub dispatch_status: &'static str,
    pub blocking_reason: Option<String>,
    pub using_last_good: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LinksPayload {
    pub project_url: Option<String>,
    pub dashboard_url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ThroughputPayload {
    pub tps_5s: f64,
    pub graph_10m: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssuePayload {
    pub issue_identifier: String,
    pub issue_id: Option<String>,
    pub status: String,
    pub workspace: IssueWorkspacePayload,
    pub attempts: IssueAttemptsPayload,
    pub running: Option<IssueRunningPayload>,
    pub retry: Option<IssueRetryPayload>,
    pub logs: IssueLogsPayload,
    pub recent_events: Vec<RecentEventPayload>,
    pub last_error: Option<String>,
    pub tracked: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssueWorkspacePayload {
    pub path: String,
    pub worker_host: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssueAttemptsPayload {
    pub restart_count: u32,
    pub current_retry_attempt: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssueRunningPayload {
    pub worker_host: Option<String>,
    pub session_id: Option<String>,
    pub app_server_pid: Option<u32>,
    pub turn_count: u32,
    pub state: String,
    pub started_at: DateTime<Utc>,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub runtime_seconds: f64,
    pub tokens: TokenSnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssueRetryPayload {
    pub attempt: u32,
    pub due_at: DateTime<Utc>,
    pub worker_host: Option<String>,
    pub workspace_location: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssueLogsPayload {
    pub codex_session_logs: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecentEventPayload {
    pub at: DateTime<Utc>,
    pub event: String,
    pub message: Option<String>,
}

#[derive(Clone, Default)]
pub struct Presenter {
    token_samples: VecDeque<(i64, u64)>,
    last_tps_second: Option<i64>,
    last_tps_value: f64,
}

impl Presenter {
    pub fn present_state(
        &mut self,
        snapshot: Snapshot,
        workflow_store: &WorkflowStore,
        dashboard_addr: Option<SocketAddr>,
    ) -> StatePayload {
        let validation_error = workflow_store.validation_error();
        let effective = workflow_store.effective();
        let blocking_reason = validation_error.clone().or_else(|| {
            effective
                .config
                .validate_dispatch_ready()
                .err()
                .map(|error| error.to_string())
        });
        let project_url = effective
            .config
            .tracker
            .project_id
            .as_deref()
            .map(todoist_project_url);
        let dashboard_url = dashboard_url(
            effective
                .config
                .server
                .host
                .as_deref()
                .unwrap_or("127.0.0.1"),
            effective.config.server.port,
            dashboard_addr.map(|addr| addr.port()),
        );
        let now_ms = snapshot.generated_at.timestamp_millis();
        let total_tokens = snapshot.codex_totals.total_tokens;
        self.capture_token_sample(now_ms, total_tokens);
        let tps_5s = self.throttled_tps(now_ms, total_tokens);
        let graph_10m = self.tps_graph(now_ms, total_tokens);

        StatePayload {
            generated_at: snapshot.generated_at,
            error: None,
            counts: snapshot.counts,
            agent_limits: AgentLimitsPayload {
                max_concurrent_agents: effective.config.agent.max_concurrent_agents,
            },
            running: snapshot
                .running
                .into_iter()
                .map(|entry| {
                    let last_event = entry.last_event.clone();
                    RunningEntryPayload {
                        issue_id: entry.issue_id,
                        issue_identifier: entry.issue_identifier,
                        title: entry.title,
                        state: entry.state,
                        url: entry.url,
                        project_url: entry.project_id.as_deref().map(todoist_project_url),
                        labels: entry.labels,
                        due: entry.due,
                        deadline: entry.deadline,
                        worker_host: entry.worker_host,
                        session_id: entry.session_id,
                        app_server_pid: entry.codex_app_server_pid,
                        turn_count: entry.turn_count,
                        last_event: last_event.clone(),
                        last_message: summarize_codex_message(
                            last_event.as_deref(),
                            entry.last_message.as_deref(),
                        ),
                        started_at: entry.started_at,
                        last_event_at: entry.last_event_at,
                        runtime_seconds: runtime_seconds(snapshot.generated_at, entry.started_at),
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
            todoist_rate_budget: snapshot.todoist_rate_budget,
            polling: snapshot.polling,
            workflow: WorkflowPayload {
                path: workflow_store.path().display().to_string(),
                dispatch_status: if blocking_reason.is_none() {
                    "ready"
                } else {
                    "blocked"
                },
                blocking_reason,
                using_last_good: validation_error.is_some(),
            },
            links: LinksPayload {
                project_url,
                dashboard_url,
            },
            throughput: ThroughputPayload { tps_5s, graph_10m },
        }
    }

    pub fn present_snapshot_failure(
        &self,
        error: OrchestratorHandleError,
        workflow_store: &WorkflowStore,
        dashboard_addr: Option<SocketAddr>,
    ) -> StatePayload {
        let validation_error = workflow_store.validation_error();
        let effective = workflow_store.effective();
        let blocking_reason = validation_error.clone().or_else(|| {
            effective
                .config
                .validate_dispatch_ready()
                .err()
                .map(|error| error.to_string())
        });
        let project_url = effective
            .config
            .tracker
            .project_id
            .as_deref()
            .map(todoist_project_url);
        let dashboard_url = dashboard_url(
            effective
                .config
                .server
                .host
                .as_deref()
                .unwrap_or("127.0.0.1"),
            effective.config.server.port,
            dashboard_addr.map(|addr| addr.port()),
        );

        StatePayload {
            generated_at: Utc::now(),
            error: Some(snapshot_error_payload(error)),
            counts: SnapshotCounts {
                running: 0,
                retrying: 0,
            },
            agent_limits: AgentLimitsPayload {
                max_concurrent_agents: effective.config.agent.max_concurrent_agents,
            },
            running: Vec::new(),
            retrying: Vec::new(),
            codex_totals: CodexTotalsPayload {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                seconds_running: 0.0,
            },
            rate_limits: None,
            todoist_rate_budget: None,
            polling: PollingSnapshot {
                checking: false,
                next_poll_in_ms: None,
                poll_interval_ms: effective.config.polling.interval_ms,
            },
            workflow: WorkflowPayload {
                path: workflow_store.path().display().to_string(),
                dispatch_status: if blocking_reason.is_none() {
                    "ready"
                } else {
                    "blocked"
                },
                blocking_reason,
                using_last_good: validation_error.is_some(),
            },
            links: LinksPayload {
                project_url,
                dashboard_url,
            },
            throughput: ThroughputPayload {
                tps_5s: self.last_tps_value,
                graph_10m: String::new(),
            },
        }
    }

    pub fn present_issue_detail(detail: IssueDetail) -> IssuePayload {
        IssuePayload {
            issue_identifier: detail.issue_identifier,
            issue_id: detail.issue_id,
            status: detail.status,
            workspace: IssueWorkspacePayload {
                path: detail.workspace.path,
                worker_host: detail.workspace.worker_host,
            },
            attempts: IssueAttemptsPayload {
                restart_count: detail.attempts.restart_count,
                current_retry_attempt: detail.attempts.current_retry_attempt,
            },
            running: detail.running.map(|running| {
                let last_event = running.last_event.clone();
                IssueRunningPayload {
                    worker_host: running.worker_host,
                    session_id: running.session_id,
                    app_server_pid: running.codex_app_server_pid,
                    turn_count: running.turn_count,
                    state: running.state,
                    started_at: running.started_at,
                    last_event: last_event.clone(),
                    last_message: summarize_codex_message(
                        last_event.as_deref(),
                        running.last_message.as_deref(),
                    ),
                    last_event_at: running.last_event_at,
                    runtime_seconds: runtime_seconds(Utc::now(), running.started_at),
                    tokens: running.tokens,
                }
            }),
            retry: detail.retry.map(|retry| IssueRetryPayload {
                attempt: retry.attempt,
                due_at: retry.due_at,
                worker_host: retry.worker_host,
                workspace_location: retry.workspace_location,
                error: retry.error,
            }),
            logs: IssueLogsPayload {
                codex_session_logs: Vec::new(),
            },
            recent_events: detail
                .recent_events
                .into_iter()
                .map(Self::present_recent_event)
                .collect(),
            last_error: detail.last_error,
            tracked: tracked_issue_payload(detail.tracked_issue.as_ref()),
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

    fn capture_token_sample(&mut self, now_ms: i64, total_tokens: u64) {
        self.token_samples.push_front((now_ms, total_tokens));
        self.prune_graph_samples(now_ms);
    }

    fn prune_graph_samples(&mut self, now_ms: i64) {
        let min_timestamp = now_ms - THROUGHPUT_GRAPH_WINDOW_MS.max(THROUGHPUT_WINDOW_MS);
        while self
            .token_samples
            .back()
            .is_some_and(|(timestamp, _)| *timestamp < min_timestamp)
        {
            self.token_samples.pop_back();
        }
    }

    fn rolling_tps(&self, now_ms: i64, current_tokens: u64) -> f64 {
        let min_timestamp = now_ms - THROUGHPUT_WINDOW_MS;
        let mut samples = self
            .token_samples
            .iter()
            .copied()
            .filter(|(timestamp, _)| *timestamp >= min_timestamp)
            .collect::<Vec<_>>();
        samples.push((now_ms, current_tokens));

        match samples.last().copied().zip(samples.first().copied()) {
            Some(((end_ms, end_tokens), (start_ms, start_tokens))) => {
                let elapsed_ms = end_ms - start_ms;
                let delta_tokens = end_tokens.saturating_sub(start_tokens);
                if elapsed_ms <= 0 {
                    0.0
                } else {
                    delta_tokens as f64 / (elapsed_ms as f64 / 1_000.0)
                }
            }
            None => 0.0,
        }
    }

    fn throttled_tps(&mut self, now_ms: i64, current_tokens: u64) -> f64 {
        let second = now_ms / 1_000;
        if self.last_tps_second == Some(second) {
            self.last_tps_value
        } else {
            let next = self.rolling_tps(now_ms, current_tokens);
            self.last_tps_second = Some(second);
            self.last_tps_value = next;
            next
        }
    }

    fn tps_graph(&self, now_ms: i64, current_tokens: u64) -> String {
        if THROUGHPUT_GRAPH_COLUMNS == 0 {
            return String::new();
        }

        let bucket_ms = THROUGHPUT_GRAPH_WINDOW_MS / THROUGHPUT_GRAPH_COLUMNS as i64;
        let active_bucket_start = now_ms - (now_ms % bucket_ms);
        let graph_window_start =
            active_bucket_start - ((THROUGHPUT_GRAPH_COLUMNS as i64 - 1) * bucket_ms);

        let mut samples = self.token_samples.iter().copied().collect::<Vec<_>>();
        samples.push((now_ms, current_tokens));
        samples.sort_by_key(|(timestamp, _)| *timestamp);

        let rates = samples
            .windows(2)
            .map(|window| {
                let (start_ms, start_tokens) = window[0];
                let (end_ms, end_tokens) = window[1];
                let elapsed_ms = end_ms - start_ms;
                let delta_tokens = end_tokens.saturating_sub(start_tokens);
                let tps = if elapsed_ms <= 0 {
                    0.0
                } else {
                    delta_tokens as f64 / (elapsed_ms as f64 / 1_000.0)
                };
                (end_ms, tps)
            })
            .collect::<Vec<_>>();

        let mut bucket_tps = Vec::with_capacity(THROUGHPUT_GRAPH_COLUMNS);
        for bucket_index in 0..THROUGHPUT_GRAPH_COLUMNS {
            let bucket_start = graph_window_start + bucket_index as i64 * bucket_ms;
            let bucket_end = bucket_start + bucket_ms;
            let last_bucket = bucket_index == THROUGHPUT_GRAPH_COLUMNS - 1;
            let values = rates
                .iter()
                .filter_map(|(timestamp, tps)| {
                    let in_bucket = if last_bucket {
                        *timestamp >= bucket_start && *timestamp <= bucket_end
                    } else {
                        *timestamp >= bucket_start && *timestamp < bucket_end
                    };
                    in_bucket.then_some(*tps)
                })
                .collect::<Vec<_>>();
            let tps = if values.is_empty() {
                0.0
            } else {
                values.iter().sum::<f64>() / values.len() as f64
            };
            bucket_tps.push(tps);
        }

        let max_tps = bucket_tps.iter().copied().fold(0.0f64, f64::max).max(1.0);

        bucket_tps
            .into_iter()
            .map(|value| {
                let index =
                    ((value / max_tps) * (SPARKLINE_BLOCKS.len() as f64 - 1.0)).round() as usize;
                SPARKLINE_BLOCKS
                    .get(index)
                    .copied()
                    .unwrap_or(SPARKLINE_BLOCKS[0])
            })
            .collect()
    }
}

pub fn render_dashboard_html(payload: &StatePayload) -> String {
    let initial_state = escape_json_for_script_tag(
        &serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string()),
    );
    format!(
        "<!doctype html>\
        <html lang=\"en\">\
          <head>\
            <meta charset=\"utf-8\">\
            <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
            <title>Symphony Todoist Observability</title>\
            <style>{}</style>\
          </head>\
          <body>\
            <div class=\"shell\">\
              <section class=\"hero\">\
                <div class=\"card\">\
                  <p class=\"eyebrow\">Symphony Observability</p>\
                  <h1>Todoist Operations Dashboard</h1>\
                  <p class=\"hero-copy\">Live Todoist-native orchestration state, task metadata, retries, throughput, workflow health, and recent Codex activity for unattended runs.</p>\
                  <p class=\"meta\">Generated at <span id=\"generated-at\"></span></p>\
                  <div class=\"link-row\">\
                    <a id=\"project-link\" class=\"pill pill-link\" target=\"_blank\" rel=\"noreferrer noopener\"></a>\
                    <a id=\"dashboard-link\" class=\"pill pill-link\" target=\"_blank\" rel=\"noreferrer noopener\"></a>\
                  </div>\
                </div>\
                <div class=\"card\">\
                  <div class=\"section-head\">\
                    <div>\
                      <p class=\"eyebrow\">Workflow Health</p>\
                      <div id=\"workflow-pill\" class=\"pill\"></div>\
                    </div>\
                    <div class=\"status-stack\">\
                      <span id=\"runtime-status\" class=\"status-chip status-chip-checking\">\
                        <span class=\"status-chip-dot\"></span>\
                        <span id=\"runtime-status-label\">Connecting</span>\
                      </span>\
                      <div class=\"actions\">\
                        <button id=\"refresh-now\" type=\"button\">Refresh now</button>\
                      </div>\
                    </div>\
                  </div>\
                  <div id=\"workflow-note\" class=\"workflow-note\"></div>\
                  <div id=\"workflow-path\" class=\"workflow-note mono\"></div>\
                  <div id=\"workflow-last-good\" class=\"workflow-note\"></div>\
                  <div id=\"runtime-status-copy\" class=\"workflow-note\"></div>\
                </div>\
              </section>\
              <section id=\"error-card\" class=\"card section error-card\" hidden>\
                <div class=\"section-head\">\
                  <div>\
                    <h2 id=\"error-title\">Observability degraded</h2>\
                    <p id=\"error-copy\" class=\"muted\"></p>\
                  </div>\
                </div>\
              </section>\
              <section class=\"grid\">\
                <div class=\"card metric\">\
                  <div class=\"label\">Running</div>\
                  <div id=\"metric-running\" class=\"value\"></div>\
                  <div class=\"muted\">Active issue sessions.</div>\
                </div>\
                <div class=\"card metric\">\
                  <div class=\"label\">Retrying</div>\
                  <div id=\"metric-retrying\" class=\"value\"></div>\
                  <div class=\"muted\">Issues queued for continuation or backoff.</div>\
                </div>\
                <div class=\"card metric\">\
                  <div class=\"label\">Total Tokens</div>\
                  <div id=\"metric-total-tokens\" class=\"value\"></div>\
                  <div id=\"metric-token-breakdown\" class=\"muted\"></div>\
                </div>\
                <div class=\"card metric\">\
                  <div class=\"label\">Runtime</div>\
                  <div id=\"metric-runtime\" class=\"value\" data-base-runtime=\"0\"></div>\
                  <div id=\"metric-polling\" class=\"muted\"></div>\
                </div>\
              </section>\
              <section class=\"grid throughput-grid\">\
                <div class=\"card metric\">\
                  <div class=\"label\">Throughput</div>\
                  <div id=\"metric-tps\" class=\"value\"></div>\
                  <div class=\"muted\">Rolling 5-second tokens/sec.</div>\
                </div>\
                <div class=\"card metric throughput-graph\">\
                  <div class=\"label\">10-Minute Graph</div>\
                  <div id=\"metric-graph\" class=\"graph mono\"></div>\
                  <div class=\"muted\">Live throughput sparkline.</div>\
                </div>\
              </section>\
              <section class=\"card section\">\
                <div class=\"section-head\">\
                  <div>\
                    <h2>Running Sessions</h2>\
                    <p class=\"muted\">Active issues, worker host, last known Codex activity, token usage, and workspace location.</p>\
                  </div>\
                </div>\
                <table>\
                  <thead>\
                    <tr><th>Task</th><th>State</th><th>Session</th><th>Runtime / turns</th><th>Last Activity</th><th>Tokens</th><th>Workspace</th></tr>\
                  </thead>\
                  <tbody id=\"running-body\"></tbody>\
                </table>\
              </section>\
              <section class=\"card section\">\
                <div class=\"section-head\">\
                  <div>\
                    <h2>Retry Queue</h2>\
                    <p class=\"muted\">Pending retries with preferred worker host, workspace affinity, due time, and last known error.</p>\
                  </div>\
                </div>\
                <table>\
                  <thead>\
                    <tr><th>Issue</th><th>Attempt</th><th>Due At</th><th>Error</th></tr>\
                  </thead>\
                  <tbody id=\"retry-body\"></tbody>\
                </table>\
              </section>\
              <section class=\"meta-grid\">\
                <div class=\"card section\">\
                  <h2>Codex Rate Limits</h2>\
                  <pre id=\"rate-limits-json\"></pre>\
                </div>\
                <div class=\"card section\">\
                  <h2>Todoist Budget</h2>\
                  <pre id=\"todoist-rate-budget-json\"></pre>\
                </div>\
                <div class=\"card section\">\
                  <h2>Polling State</h2>\
                  <pre id=\"polling-json\"></pre>\
                </div>\
              </section>\
            </div>\
            <script id=\"initial-state\" type=\"application/json\">{}</script>\
            <script>{}</script>\
          </body>\
        </html>",
        DASHBOARD_CSS, initial_state, DASHBOARD_JS
    )
}

fn render_terminal_dashboard(
    payload: &StatePayload,
    terminal_viewport: Option<TerminalViewport>,
) -> String {
    let viewport = terminal_viewport.unwrap_or(TerminalViewport {
        columns: DEFAULT_TERMINAL_COLUMNS,
        rows: DEFAULT_TERMINAL_ROWS,
    });
    let columns = viewport.columns.max(1);
    let running_event_width = columns.saturating_sub(
        RUNNING_STATUS_WIDTH
            + RUNNING_ID_WIDTH
            + RUNNING_STATE_WIDTH
            + RUNNING_SESSION_WIDTH
            + RUNNING_PID_WIDTH
            + RUNNING_RUNTIME_WIDTH
            + RUNNING_TOKENS_WIDTH
            + RUNNING_ROW_CHROME_WIDTH,
    );

    let mut lines = vec![
        colorize("╭─ SYMPHONY STATUS", ANSI_BOLD),
        format!(
            "{}{}{}{}{}",
            colorize("│ Agents: ", ANSI_BOLD),
            colorize(&payload.counts.running.to_string(), ANSI_GREEN),
            colorize("/", ANSI_GRAY),
            colorize(
                &payload.agent_limits.max_concurrent_agents.to_string(),
                ANSI_GRAY
            ),
            ""
        ),
        format!(
            "{}{}",
            colorize("│ Throughput: ", ANSI_BOLD),
            colorize(&format!("{:.1} tps", payload.throughput.tps_5s), ANSI_CYAN)
        ),
        format!(
            "{}{}",
            colorize("│ Runtime: ", ANSI_BOLD),
            colorize(
                &format_duration(payload.codex_totals.seconds_running),
                ANSI_MAGENTA
            )
        ),
        format!(
            "{}{}{}{}{}{}",
            colorize("│ Tokens: ", ANSI_BOLD),
            colorize(
                &format!("in {}", format_int(payload.codex_totals.input_tokens)),
                ANSI_YELLOW
            ),
            colorize(" | ", ANSI_GRAY),
            colorize(
                &format!("out {}", format_int(payload.codex_totals.output_tokens)),
                ANSI_YELLOW
            ),
            colorize(" | ", ANSI_GRAY),
            colorize(
                &format!("total {}", format_int(payload.codex_totals.total_tokens)),
                ANSI_YELLOW
            )
        ),
        format!(
            "{}{}",
            colorize("│ Codex Limits: ", ANSI_BOLD),
            format_rate_limits(
                payload
                    .rate_limits
                    .as_ref()
                    .map(format_rate_limits_summary)
                    .as_deref(),
            )
        ),
        format!(
            "{}{}",
            colorize("│ Todoist Budget: ", ANSI_BOLD),
            format_rate_limits(
                payload
                    .todoist_rate_budget
                    .as_ref()
                    .map(format_todoist_rate_budget_summary)
                    .as_deref(),
            )
        ),
    ];

    if let Some(project_url) = payload.links.project_url.as_deref() {
        lines.push(format!(
            "{}{}",
            colorize("│ Project: ", ANSI_BOLD),
            colorize(project_url, ANSI_CYAN)
        ));
    }
    if let Some(dashboard_url) = payload.links.dashboard_url.as_deref() {
        lines.push(format!(
            "{}{}",
            colorize("│ Dashboard: ", ANSI_BOLD),
            colorize(dashboard_url, ANSI_CYAN)
        ));
    }
    lines.push(format!(
        "{}{}",
        colorize("│ Next refresh: ", ANSI_BOLD),
        colorize(&format_polling_status(&payload.polling), ANSI_CYAN)
    ));
    lines.push(format!(
        "{}{}",
        colorize("│ Graph: ", ANSI_BOLD),
        colorize(&payload.throughput.graph_10m, ANSI_CYAN)
    ));
    lines.push(colorize("├─ Running", ANSI_BOLD));
    lines.push("│".to_string());
    lines.push(running_table_header_row(running_event_width));
    lines.push(running_table_separator_row(running_event_width));
    if payload.running.is_empty() {
        lines.push(format!("│  {}", colorize("No active agents", ANSI_GRAY)));
        lines.push("│".to_string());
    } else {
        for entry in &payload.running {
            lines.push(format_running_row(entry, running_event_width));
        }
        lines.push("│".to_string());
    }
    lines.push(colorize("├─ Backoff queue", ANSI_BOLD));
    lines.push("│".to_string());
    if payload.retrying.is_empty() {
        lines.push(format!(
            "│  {}",
            colorize("Retry queue is empty", ANSI_GRAY)
        ));
    } else {
        for entry in &payload.retrying {
            lines.push(format_retry_row(entry));
        }
    }
    lines.push("╰─".to_string());
    fit_terminal_frame(lines, viewport).join("\n")
}

pub fn render_offline_status() -> String {
    [
        colorize("╭─ SYMPHONY STATUS", ANSI_BOLD),
        colorize("│ app_status=offline", ANSI_RED),
        "╰─".to_string(),
    ]
    .join("\n")
}

pub fn render_snapshot_timed_out_status() -> String {
    [
        colorize("╭─ SYMPHONY STATUS", ANSI_BOLD),
        colorize("│ Orchestrator snapshot timed out", ANSI_RED),
        "╰─".to_string(),
    ]
    .join("\n")
}

pub fn render_snapshot_unavailable_status() -> String {
    [
        colorize("╭─ SYMPHONY STATUS", ANSI_BOLD),
        colorize("│ Orchestrator snapshot unavailable", ANSI_RED),
        "╰─".to_string(),
    ]
    .join("\n")
}

fn render_snapshot_failure_dashboard(
    error: OrchestratorHandleError,
    workflow_store: &WorkflowStore,
    dashboard_addr: Option<SocketAddr>,
    tps_5s: f64,
    polling: Option<&PollingSnapshot>,
) -> String {
    let effective = workflow_store.effective();
    let project_url = effective
        .config
        .tracker
        .project_id
        .as_deref()
        .map(todoist_project_url);
    let dashboard_url = dashboard_url(
        effective
            .config
            .server
            .host
            .as_deref()
            .unwrap_or("127.0.0.1"),
        effective.config.server.port,
        dashboard_addr.map(|addr| addr.port()),
    );

    let mut lines = vec![
        colorize("╭─ SYMPHONY STATUS", ANSI_BOLD),
        colorize(
            &format!("│ {}", snapshot_error_payload(error).message),
            ANSI_RED,
        ),
        format!(
            "{}{}",
            colorize("│ Throughput: ", ANSI_BOLD),
            colorize(&format!("{tps_5s:.1} tps"), ANSI_CYAN)
        ),
    ];

    if let Some(project_url) = project_url.as_deref() {
        lines.push(format!(
            "{}{}",
            colorize("│ Project: ", ANSI_BOLD),
            colorize(project_url, ANSI_CYAN)
        ));
    }
    if let Some(dashboard_url) = dashboard_url.as_deref() {
        lines.push(format!(
            "{}{}",
            colorize("│ Dashboard: ", ANSI_BOLD),
            colorize(dashboard_url, ANSI_CYAN)
        ));
    }

    let next_refresh = polling
        .map(format_polling_status)
        .unwrap_or_else(|| "n/a".to_string());
    lines.push(format!(
        "{}{}",
        colorize("│ Next refresh: ", ANSI_BOLD),
        colorize(&next_refresh, ANSI_CYAN)
    ));
    lines.push("╰─".to_string());
    lines.join("\n")
}

#[cfg(test)]
fn render_snapshot_unavailable_dashboard(
    workflow_store: &WorkflowStore,
    dashboard_addr: Option<SocketAddr>,
    tps_5s: f64,
    polling: Option<&PollingSnapshot>,
) -> String {
    render_snapshot_failure_dashboard(
        OrchestratorHandleError::Unavailable,
        workflow_store,
        dashboard_addr,
        tps_5s,
        polling,
    )
}

pub struct TerminalDashboard {
    enabled: bool,
    shutdown: Arc<dyn Fn() + Send + Sync>,
    join: Option<JoinHandle<()>>,
}

struct TerminalRenderer {
    init: Arc<dyn Fn() + Send + Sync>,
    render: Arc<dyn Fn(String) + Send + Sync>,
    shutdown: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Default)]
struct TerminalRenderState {
    last_snapshot_fingerprint: Option<u64>,
    last_rendered_content: Option<String>,
    last_rendered_at: Option<Instant>,
    pending_content: Option<String>,
    flush_due_at: Option<Instant>,
    snapshot_due_at: Option<Instant>,
    last_polling: Option<PollingSnapshot>,
}

impl TerminalDashboard {
    pub fn start(
        orchestrator: OrchestratorHandle,
        workflow_store: WorkflowStore,
        observability: ObservabilityConfig,
        dashboard_addr: Option<SocketAddr>,
    ) -> Self {
        let renderer = terminal_renderer();
        Self::start_with_renderer(
            orchestrator,
            workflow_store,
            observability,
            dashboard_addr,
            renderer,
            io::stdout().is_terminal(),
        )
    }

    fn start_with_renderer(
        orchestrator: OrchestratorHandle,
        workflow_store: WorkflowStore,
        observability: ObservabilityConfig,
        dashboard_addr: Option<SocketAddr>,
        renderer: TerminalRenderer,
        interactive_terminal: bool,
    ) -> Self {
        if !observability.terminal_enabled || !interactive_terminal {
            return Self {
                enabled: false,
                shutdown: renderer.shutdown,
                join: None,
            };
        }

        (renderer.init)();
        let task_renderer = Arc::clone(&renderer.render);
        let join = tokio::spawn(async move {
            let mut presenter = Presenter::default();
            let mut updates = orchestrator.subscribe_observability();
            let mut refresh_tick = interval(Duration::from_millis(observability.refresh_ms));
            refresh_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut render_state = TerminalRenderState::default();

            loop {
                tokio::select! {
                    changed = updates.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        schedule_terminal_snapshot(
                            &mut render_state,
                            observability.render_interval_ms,
                            false,
                        );
                    }
                    _ = refresh_tick.tick() => {
                        schedule_terminal_snapshot(
                            &mut render_state,
                            observability.render_interval_ms,
                            false,
                        );
                    }
                    _ = wait_for_terminal_snapshot(render_state.snapshot_due_at),
                        if render_state.snapshot_due_at.is_some() => {
                        render_state.snapshot_due_at = None;
                        refresh_terminal_dashboard(
                            &orchestrator,
                            &workflow_store,
                            dashboard_addr,
                            &mut presenter,
                            &mut render_state,
                            observability.render_interval_ms,
                            &task_renderer,
                        ).await;
                    }
                    _ = wait_for_terminal_flush(render_state.flush_due_at),
                        if render_state.flush_due_at.is_some() => {
                        flush_pending_terminal_frame(&task_renderer, &mut render_state);
                    }
                }
            }
        });

        Self {
            enabled: true,
            shutdown: renderer.shutdown,
            join: Some(join),
        }
    }

    pub async fn shutdown(mut self) {
        if self.enabled {
            (self.shutdown)();
        }
        if let Some(join) = self.join.take() {
            join.abort();
            let _ = join.await;
        }
    }
}

fn periodic_rerender_due(last_rendered_at: Option<Instant>) -> bool {
    match last_rendered_at {
        None => true,
        Some(last_rendered_at) => {
            last_rendered_at.elapsed().as_millis() as u64 >= MINIMUM_IDLE_RERENDER_MS
        }
    }
}

fn schedule_terminal_snapshot(
    render_state: &mut TerminalRenderState,
    render_interval_ms: u64,
    force_now: bool,
) {
    let now = Instant::now();
    let due_at = if force_now || render_now(render_state.last_rendered_at, render_interval_ms, now)
    {
        now
    } else {
        schedule_flush_due_at(render_state.last_rendered_at, render_interval_ms, now)
    };

    render_state.snapshot_due_at = Some(
        render_state
            .snapshot_due_at
            .map_or(due_at, |existing| existing.min(due_at)),
    );
}

async fn refresh_terminal_dashboard(
    orchestrator: &OrchestratorHandle,
    workflow_store: &WorkflowStore,
    dashboard_addr: Option<SocketAddr>,
    presenter: &mut Presenter,
    render_state: &mut TerminalRenderState,
    render_interval_ms: u64,
    task_renderer: &Arc<dyn Fn(String) + Send + Sync>,
) {
    let next = match orchestrator.snapshot().await {
        Ok(snapshot) => {
            let payload = presenter.present_state(snapshot, workflow_store, dashboard_addr);
            render_state.last_polling = Some(payload.polling.clone());
            render_terminal_dashboard(&payload, Some(current_terminal_viewport()))
        }
        Err(error) => render_snapshot_failure_dashboard(
            error,
            workflow_store,
            dashboard_addr,
            presenter.last_tps_value,
            render_state.last_polling.as_ref(),
        ),
    };

    let fingerprint = fingerprint_terminal_content(&next);
    let rerender_due = periodic_rerender_due(render_state.last_rendered_at);
    if render_state.last_snapshot_fingerprint == Some(fingerprint) && !rerender_due {
        return;
    }

    maybe_enqueue_terminal_frame(
        render_state,
        next,
        fingerprint,
        render_interval_ms,
        task_renderer,
    );
}

fn maybe_enqueue_terminal_frame(
    render_state: &mut TerminalRenderState,
    content: String,
    fingerprint: u64,
    render_interval_ms: u64,
    task_renderer: &Arc<dyn Fn(String) + Send + Sync>,
) {
    render_state.last_snapshot_fingerprint = Some(fingerprint);

    if render_state.last_rendered_content.as_deref() == Some(content.as_str()) {
        return;
    }

    let now = Instant::now();
    if render_now(render_state.last_rendered_at, render_interval_ms, now)
        && render_state.flush_due_at.is_none()
    {
        task_renderer(content.clone());
        render_state.last_rendered_content = Some(content);
        render_state.last_rendered_at = Some(now);
        render_state.pending_content = None;
        render_state.flush_due_at = None;
        return;
    }

    render_state.pending_content = Some(content);
    if render_state.flush_due_at.is_none() {
        render_state.flush_due_at = Some(schedule_flush_due_at(
            render_state.last_rendered_at,
            render_interval_ms,
            now,
        ));
    }
}

fn flush_pending_terminal_frame(
    task_renderer: &Arc<dyn Fn(String) + Send + Sync>,
    render_state: &mut TerminalRenderState,
) {
    render_state.flush_due_at = None;
    if let Some(content) = render_state.pending_content.take() {
        task_renderer(content.clone());
        render_state.last_rendered_content = Some(content);
        render_state.last_rendered_at = Some(Instant::now());
    }
}

fn render_now(last_rendered_at: Option<Instant>, render_interval_ms: u64, now: Instant) -> bool {
    match last_rendered_at {
        None => true,
        Some(last_rendered_at) => {
            now.duration_since(last_rendered_at).as_millis() as u64 >= render_interval_ms
        }
    }
}

fn schedule_flush_due_at(
    last_rendered_at: Option<Instant>,
    render_interval_ms: u64,
    now: Instant,
) -> Instant {
    match last_rendered_at {
        None => now,
        Some(last_rendered_at) => {
            let elapsed_ms = now.duration_since(last_rendered_at).as_millis() as u64;
            let remaining_ms = render_interval_ms.saturating_sub(elapsed_ms).max(1);
            now + Duration::from_millis(remaining_ms)
        }
    }
}

async fn wait_for_terminal_flush(flush_due_at: Option<Instant>) {
    match flush_due_at {
        Some(flush_due_at) => sleep_until(tokio::time::Instant::from_std(flush_due_at)).await,
        None => std::future::pending().await,
    }
}

async fn wait_for_terminal_snapshot(snapshot_due_at: Option<Instant>) {
    match snapshot_due_at {
        Some(snapshot_due_at) => sleep_until(tokio::time::Instant::from_std(snapshot_due_at)).await,
        None => std::future::pending().await,
    }
}

fn fingerprint_terminal_content(content: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

fn terminal_renderer() -> TerminalRenderer {
    TerminalRenderer {
        init: Arc::new(default_alternate_terminal_init),
        render: Arc::new(default_terminal_renderer),
        shutdown: Arc::new(default_alternate_terminal_shutdown),
    }
}

fn default_terminal_renderer(content: String) {
    let mut stdout = io::stdout();
    let _ = render_terminal_frame_into(&mut stdout, &content);
}

fn default_alternate_terminal_init() {
    let mut stdout = io::stdout();
    let _ = enter_alternate_screen_into(&mut stdout);
}

fn default_alternate_terminal_shutdown() {
    let mut stdout = io::stdout();
    let _ = leave_alternate_screen_into(&mut stdout);
}

fn render_terminal_frame_into<W: Write>(writer: &mut W, content: &str) -> io::Result<()> {
    writer.write_all(ANSI_HOME.as_bytes())?;
    if !content.is_empty() {
        for (index, line) in content.split('\n').enumerate() {
            if index > 0 {
                writer.write_all(b"\n")?;
            }
            writer.write_all(line.as_bytes())?;
            writer.write_all(ANSI_CLEAR_TO_EOL.as_bytes())?;
        }
    }
    writer.write_all(ANSI_CLEAR_DOWN.as_bytes())?;
    writer.flush()
}

fn enter_alternate_screen_into<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(
        format!(
            "{}{}{}{}",
            ANSI_ENTER_ALTERNATE_SCREEN, ANSI_HIDE_CURSOR, ANSI_HOME, ANSI_CLEAR_DOWN
        )
        .as_bytes(),
    )?;
    writer.flush()
}

fn leave_alternate_screen_into<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(format!("{}{}", ANSI_SHOW_CURSOR, ANSI_LEAVE_ALTERNATE_SCREEN).as_bytes())?;
    writer.flush()
}

fn running_table_header_row(running_event_width: usize) -> String {
    format!(
        "│ {} {} {} {} {} {} {} {}",
        format_cell("", RUNNING_STATUS_WIDTH),
        format_cell("TASK", RUNNING_ID_WIDTH),
        format_cell("STATE", RUNNING_STATE_WIDTH),
        format_cell("SESSION", RUNNING_SESSION_WIDTH),
        format_cell("PID", RUNNING_PID_WIDTH),
        format_cell("RUNTIME/TURNS", RUNNING_RUNTIME_WIDTH),
        format_cell("EVENT", running_event_width),
        format_cell("TOKENS", RUNNING_TOKENS_WIDTH)
    )
}

fn running_table_separator_row(running_event_width: usize) -> String {
    format!(
        "│ {} {} {} {} {} {} {} {}",
        "─",
        "─".repeat(RUNNING_ID_WIDTH),
        "─".repeat(RUNNING_STATE_WIDTH),
        "─".repeat(RUNNING_SESSION_WIDTH),
        "─".repeat(RUNNING_PID_WIDTH),
        "─".repeat(RUNNING_RUNTIME_WIDTH),
        "─".repeat(running_event_width),
        "─".repeat(RUNNING_TOKENS_WIDTH)
    )
}

fn format_running_row(entry: &RunningEntryPayload, running_event_width: usize) -> String {
    let dot = colorize(
        &format_cell("●", RUNNING_STATUS_WIDTH),
        running_row_dot_color(entry),
    );
    let issue = colorize(
        &format_cell(&entry.issue_identifier, RUNNING_ID_WIDTH),
        ANSI_CYAN,
    );
    let state = colorize(
        &format_cell(&humanize_status(&entry.state), RUNNING_STATE_WIDTH),
        state_color(&entry.state),
    );
    let session = format_cell(
        &entry
            .session_id
            .as_deref()
            .map(short_id)
            .unwrap_or_else(|| "n/a".to_string()),
        RUNNING_SESSION_WIDTH,
    );
    let pid = format_cell(
        &entry
            .app_server_pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "n/a".to_string()),
        RUNNING_PID_WIDTH,
    );
    let runtime_turns = format_cell(
        &format!(
            "{} / {}t",
            format_duration(entry.runtime_seconds),
            entry.turn_count
        ),
        RUNNING_RUNTIME_WIDTH,
    );
    let event_text = entry
        .worker_host
        .as_deref()
        .map(|worker_host| {
            format!(
                "{}: {}",
                worker_host,
                entry
                    .last_message
                    .as_deref()
                    .unwrap_or("No Codex message yet.")
            )
        })
        .unwrap_or_else(|| {
            entry
                .last_message
                .as_deref()
                .unwrap_or("No Codex message yet.")
                .to_string()
        });
    let event = format_cell(&sanitize_display_text(&event_text), running_event_width);
    let tokens = format_cell_right(&format_int(entry.tokens.total_tokens), RUNNING_TOKENS_WIDTH);
    format!("│ {dot} {issue} {state} {session} {pid} {runtime_turns} {event} {tokens}")
}

fn format_retry_row(entry: &RetrySnapshot) -> String {
    let due_in = entry
        .due_at
        .signed_duration_since(Utc::now())
        .num_milliseconds()
        .max(0);
    let due_in_seconds = ((due_in + 999) / 1_000).max(0);
    format!(
        "│  {} attempt={} worker={} due_in={}s error={}",
        colorize(&entry.issue_identifier, ANSI_CYAN),
        entry.attempt,
        inline_text(entry.worker_host.as_deref().unwrap_or("local")),
        due_in_seconds,
        inline_text(entry.error.as_deref().unwrap_or("none"))
    )
}

fn state_color(state: &str) -> &'static str {
    match slugify(state).as_str() {
        "in-progress" | "merging" | "todo" => ANSI_GREEN,
        "rework" => ANSI_YELLOW,
        "human-review" | "done" => ANSI_MAGENTA,
        _ => ANSI_BLUE,
    }
}

fn running_row_dot_color(entry: &RunningEntryPayload) -> &'static str {
    match entry.last_event.as_deref() {
        Some("turn_completed") => ANSI_MAGENTA,
        Some("codex/event/task_started") => ANSI_GREEN,
        Some("codex/event/token_count") => ANSI_YELLOW,
        Some(_) => ANSI_BLUE,
        None => state_color(&entry.state),
    }
}

fn format_rate_limits(summary: Option<&str>) -> String {
    match summary {
        Some(summary) if !summary.is_empty() => colorize(summary, ANSI_CYAN),
        _ => colorize("n/a", ANSI_GRAY),
    }
}

fn runtime_seconds(now: DateTime<Utc>, started_at: DateTime<Utc>) -> f64 {
    now.signed_duration_since(started_at)
        .num_milliseconds()
        .max(0) as f64
        / 1_000.0
}

pub fn summarize_codex_message(event: Option<&str>, message: Option<&str>) -> Option<String> {
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
    match (
        payload.and_then(dynamic_tool_name),
        payload.and_then(dynamic_tool_action),
    ) {
        (Some(tool), Some(action)) => format!("{base} ({tool}:{action})"),
        (Some(tool), None) => format!("{base} ({tool})"),
        _ => base.to_string(),
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

fn dynamic_tool_action(payload: &Value) -> Option<String> {
    json_text(
        payload,
        &[
            &["params", "arguments", "action"],
            &["toolResult", "output", "action"],
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
        "agent_reasoning" => humanize_streaming_event("reasoning update", payload),
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
            &["params", "summaryText"],
            &["params", "textDelta"],
            &["params", "msg", "delta"],
            &["params", "msg", "text"],
            &["params", "msg", "content"],
            &["params", "msg", "payload", "delta"],
            &["params", "msg", "payload", "text"],
            &["params", "msg", "payload", "content"],
            &["params", "msg", "payload", "summaryText"],
            &["params", "msg", "payload", "textDelta"],
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

pub fn format_rate_limits_summary(rate_limits: &Value) -> String {
    let primary =
        json_value(rate_limits, &[&["primary"]]).and_then(format_rate_limit_bucket_summary);
    let secondary =
        json_value(rate_limits, &[&["secondary"]]).and_then(format_rate_limit_bucket_summary);

    let credits = json_value(rate_limits, &[&["credits"]]).and_then(format_credits_summary);

    let mut parts = Vec::new();
    if let Some(primary) = primary {
        parts.push(format!("primary {primary}"));
    }
    if let Some(secondary) = secondary {
        parts.push(format!("secondary {secondary}"));
    }
    if let Some(credits) = credits {
        parts.push(credits);
    }

    if parts.is_empty() {
        "n/a".to_string()
    } else {
        parts.join("; ")
    }
}

pub fn format_todoist_rate_budget_summary(budget: &TrackerRateBudget) -> String {
    let mut parts = Vec::new();

    if let (Some(remaining), Some(limit)) = (budget.remaining, budget.limit) {
        parts.push(format!("{remaining}/{limit} remaining"));
    } else if let Some(limit) = budget.limit {
        parts.push(format!("limit {limit}"));
    }

    if let Some(reset_in_seconds) = budget.reset_in_seconds {
        parts.push(format!("reset {reset_in_seconds}s"));
    }

    if let Some(throttled_for_seconds) = budget.throttled_for_seconds {
        parts.push(format!("throttled {throttled_for_seconds}s"));
    } else if let Some(retry_after_seconds) = budget.retry_after_seconds {
        parts.push(format!("retry_after {retry_after_seconds}s"));
    }

    if let Some(next_request_in_seconds) = budget.next_request_in_seconds {
        parts.push(format!("next slot {next_request_in_seconds}s"));
    }

    if parts.is_empty() {
        "n/a".to_string()
    } else {
        parts.join("; ")
    }
}

fn format_rate_limit_bucket_summary(bucket: &Value) -> Option<String> {
    let used_percent = json_number(bucket, &[&["usedPercent"], &["used_percent"]]);
    let remaining = parse_integer(json_value(bucket, &[&["remaining"]]));
    let limit = parse_integer(json_value(bucket, &[&["limit"]]));
    let reset_in_seconds = parse_integer(json_value(
        bucket,
        &[&["resetInSeconds"], &["reset_in_seconds"]],
    ));

    let usage = match (used_percent, remaining, limit) {
        (Some(used_percent), _, _) => format!("{used_percent}% used"),
        (None, Some(remaining), Some(limit)) => format!("{remaining}/{limit} remaining"),
        _ => return None,
    };

    match reset_in_seconds {
        Some(seconds) => Some(format!("{usage}, reset {seconds}s")),
        None => Some(usage),
    }
}

fn format_credits_summary(credits: &Value) -> Option<String> {
    if json_value(credits, &[&["unlimited"]])
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Some("credits unlimited".to_string())
    } else if json_value(credits, &[&["has_credits"]])
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        json_number(credits, &[&["balance"]]).map(|balance| format!("credits {:.1}", balance))
    } else if json_value(credits, &[&["has_credits"]])
        .and_then(Value::as_bool)
        .is_some()
    {
        Some("credits exhausted".to_string())
    } else {
        None
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
        .map(title_case_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_case_word(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => {
            let mut rendered = first.to_uppercase().collect::<String>();
            rendered.push_str(&chars.as_str().to_ascii_lowercase());
            rendered
        }
        None => String::new(),
    }
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

fn snapshot_error_payload(error: OrchestratorHandleError) -> ErrorPayload {
    match error {
        OrchestratorHandleError::TimedOut => ErrorPayload {
            code: "snapshot_timeout",
            message: "Orchestrator snapshot timed out",
        },
        OrchestratorHandleError::Unavailable => ErrorPayload {
            code: "snapshot_unavailable",
            message: "Orchestrator snapshot unavailable",
        },
    }
}

fn inline_text(value: &str) -> String {
    let collapsed = sanitize_display_text(value);
    truncate_chars(&collapsed, 80)
}

fn sanitize_display_text(value: &str) -> String {
    let stripped = strip_ansi_sequences(value);
    let cleaned: String = stripped
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t' || *ch == '\r')
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_ansi_sequences(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\u{1b}' => match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    consume_csi_sequence(&mut chars);
                }
                Some(']') => {
                    chars.next();
                    consume_osc_sequence(&mut chars);
                }
                Some('P' | '^' | '_') => {
                    chars.next();
                    consume_st_terminated_sequence(&mut chars);
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\u{9b}' => consume_csi_sequence(&mut chars),
            _ => output.push(ch),
        }
    }

    output
}

fn consume_csi_sequence<I>(chars: &mut Peekable<I>)
where
    I: Iterator<Item = char>,
{
    loop {
        let Some(next) = chars.next() else {
            break;
        };
        if ('@'..='~').contains(&next) {
            break;
        }
    }
}

fn consume_osc_sequence<I>(chars: &mut Peekable<I>)
where
    I: Iterator<Item = char>,
{
    loop {
        let Some(next) = chars.next() else {
            break;
        };
        match next {
            '\u{7}' => break,
            '\u{1b}' if matches!(chars.peek().copied(), Some('\\')) => {
                chars.next();
                break;
            }
            _ => {}
        }
    }
}

fn consume_st_terminated_sequence<I>(chars: &mut Peekable<I>)
where
    I: Iterator<Item = char>,
{
    loop {
        let Some(next) = chars.next() else {
            break;
        };
        if next == '\u{1b}' && matches!(chars.peek().copied(), Some('\\')) {
            chars.next();
            break;
        }
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return value.chars().take(max_chars).collect();
    }

    let truncated: String = value.chars().take(max_chars - 3).collect();
    format!("{truncated}...")
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

pub fn format_int(value: u64) -> String {
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

fn pretty_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
}

fn slugify(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn escape_json_for_script_tag(value: &str) -> String {
    value
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

fn tracked_issue_payload(issue: Option<&Issue>) -> Value {
    let Some(issue) = issue else {
        return json!({});
    };

    let mut payload = serde_json::to_value(issue)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(project_id) = issue.project_id.as_deref() {
        payload.insert(
            "project_url".to_string(),
            Value::String(todoist_project_url(project_id)),
        );
    }
    Value::Object(payload)
}

pub fn humanize_blocking_reason(value: &str) -> &str {
    if value.starts_with("todoist_project_not_found ") {
        "configured Todoist project was not found"
    } else if value.starts_with("todoist_missing_required_section ") {
        "Todoist workflow is missing a required section"
    } else if value.starts_with("todoist_comment_too_large") {
        "Todoist workpad comment exceeds the 15,000 character limit"
    } else if value.starts_with("todoist_rate_limited") {
        "Todoist rate limit reached; retrying after the server hint"
    } else if value.starts_with("todoist_api_status ") {
        "Todoist API returned a non-success status"
    } else if value.starts_with("todoist_api_request ") {
        "Todoist API request failed before a response was received"
    } else {
        match value {
            "workflow_front_matter_not_a_map" => "workflow front matter must decode to a map",
            "missing_tracker_api_key" => "Todoist API token is missing",
            "missing_tracker_project_id" => "Todoist project id is missing",
            "missing_tracker_kind" => "tracker kind is missing",
            "missing_codex_command" => "codex command is missing",
            "todoist_missing_current_user" => "Todoist current user lookup failed",
            "todoist_comments_unavailable" => "Todoist comments are unavailable on this account",
            "todoist_reminders_unavailable" => "Todoist reminders are unavailable on this account",
            "todoist_unknown_payload" => "Todoist returned an unexpected payload shape",
            other => other,
        }
    }
}

pub fn todoist_project_url(project_id: &str) -> String {
    format!("https://app.todoist.com/app/project/{project_id}")
}

pub fn dashboard_url(
    host: &str,
    configured_port: Option<u16>,
    bound_port: Option<u16>,
) -> Option<String> {
    let port = bound_port.or(configured_port)?;
    if port == 0 {
        return None;
    }

    Some(format!("http://{}:{port}/", dashboard_url_host(host)))
}

fn dashboard_url_host(host: &str) -> String {
    let trimmed_host = host.trim();
    match trimmed_host {
        "" | "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        _ if trimmed_host.starts_with('[') && trimmed_host.ends_with(']') => {
            trimmed_host.to_string()
        }
        _ if trimmed_host.contains(':') => format!("[{trimmed_host}]"),
        _ => trimmed_host.to_string(),
    }
}

fn format_duration(seconds: f64) -> String {
    let total_seconds = seconds.max(0.0).round() as u64;
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let secs = total_seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

fn format_polling_status(polling: &PollingSnapshot) -> String {
    if polling.checking {
        "checking now…".to_string()
    } else if let Some(next_poll_in_ms) = polling.next_poll_in_ms {
        format!("{}s", next_poll_in_ms.div_ceil(1_000))
    } else {
        "n/a".to_string()
    }
}

fn format_cell(value: &str, width: usize) -> String {
    format_cell_aligned(value, width, false)
}

fn format_cell_right(value: &str, width: usize) -> String {
    format_cell_aligned(value, width, true)
}

fn format_cell_aligned(value: &str, width: usize, right_align: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let truncated = truncate_chars(value, width);
    let len = truncated.chars().count();
    if len >= width {
        truncated
    } else if right_align {
        format!("{truncated:>width$}")
    } else {
        format!("{truncated:<width$}")
    }
}

const ANSI_HOME: &str = "\u{1b}[H";
const ANSI_CLEAR_TO_EOL: &str = "\u{1b}[K";
const ANSI_CLEAR_DOWN: &str = "\u{1b}[J";
const ANSI_ENTER_ALTERNATE_SCREEN: &str = "\u{1b}[?1049h";
const ANSI_LEAVE_ALTERNATE_SCREEN: &str = "\u{1b}[?1049l";
const ANSI_HIDE_CURSOR: &str = "\u{1b}[?25l";
const ANSI_SHOW_CURSOR: &str = "\u{1b}[?25h";
const ANSI_RESET: &str = "\u{1b}[0m";
const ANSI_BOLD: &str = "\u{1b}[1m";
const ANSI_BLUE: &str = "\u{1b}[34m";
const ANSI_CYAN: &str = "\u{1b}[36m";
const ANSI_GREEN: &str = "\u{1b}[32m";
const ANSI_MAGENTA: &str = "\u{1b}[35m";
const ANSI_RED: &str = "\u{1b}[31m";
const ANSI_YELLOW: &str = "\u{1b}[33m";
const ANSI_GRAY: &str = "\u{1b}[90m";

fn colorize(value: &str, ansi: &str) -> String {
    format!("{ansi}{value}{ANSI_RESET}")
}

fn fit_terminal_frame(lines: Vec<String>, viewport: TerminalViewport) -> Vec<String> {
    let columns = viewport.columns.max(1);
    let rows = viewport.rows.max(1);
    let total_lines = lines.len();
    let mut fitted = lines
        .into_iter()
        .map(|line| fit_terminal_line(&line, columns))
        .collect::<Vec<_>>();
    if fitted.len() > rows {
        fitted.truncate(rows.saturating_sub(1));
        let hidden = total_lines.saturating_sub(rows.saturating_sub(1));
        fitted.push(fit_terminal_line(
            &format!("╰─ {hidden} more lines hidden; enlarge terminal"),
            columns,
        ));
    }
    fitted
}

fn fit_terminal_line(line: &str, columns: usize) -> String {
    if columns == 0 {
        return String::new();
    }

    let plain = strip_ansi_sequences(line);
    if plain.chars().count() <= columns {
        return line.to_string();
    }

    truncate_plain_line(&plain, columns)
}

fn truncate_plain_line(value: &str, columns: usize) -> String {
    if columns == 0 {
        return String::new();
    }
    if columns == 1 {
        return "…".to_string();
    }
    let visible_columns = columns.saturating_sub(1);
    let mut chars = value.chars();
    let mut truncated = String::with_capacity(columns);
    for _ in 0..visible_columns {
        let Some(ch) = chars.next() else {
            return value.to_string();
        };
        truncated.push(ch);
    }
    if chars.next().is_some() {
        truncated.push('…');
        truncated
    } else {
        value.to_string()
    }
}

fn current_terminal_viewport() -> TerminalViewport {
    terminal_viewport_from_stdout()
        .or_else(terminal_viewport_from_env)
        .unwrap_or(TerminalViewport {
            columns: DEFAULT_TERMINAL_COLUMNS,
            rows: DEFAULT_TERMINAL_ROWS,
        })
}

fn terminal_viewport_from_env() -> Option<TerminalViewport> {
    let columns = env::var("COLUMNS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0);
    let rows = env::var("LINES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0);

    match (columns, rows) {
        (Some(columns), Some(rows)) => Some(TerminalViewport { columns, rows }),
        _ => None,
    }
}

#[cfg(unix)]
fn terminal_viewport_from_stdout() -> Option<TerminalViewport> {
    use std::os::fd::AsRawFd;

    let stdout = io::stdout();
    let fd = stdout.as_raw_fd();
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    let status = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if status != 0 {
        return None;
    }

    let size = unsafe { size.assume_init() };
    let columns = usize::from(size.ws_col);
    let rows = usize::from(size.ws_row);
    if columns == 0 || rows == 0 {
        None
    } else {
        Some(TerminalViewport { columns, rows })
    }
}

#[cfg(not(unix))]
fn terminal_viewport_from_stdout() -> Option<TerminalViewport> {
    None
}

const DASHBOARD_CSS: &str = r#"
:root {
  --bg: #f3efe6;
  --panel: #fffaf2;
  --panel-strong: #fffdf8;
  --ink: #1f1b16;
  --muted: #6a6155;
  --accent: #1e6b52;
  --accent-soft: #d9efe6;
  --warn: #9d3d12;
  --warn-soft: #f6dfd4;
  --line: rgba(31, 27, 22, 0.12);
  --shadow: 0 18px 48px rgba(78, 59, 31, 0.08);
}
* { box-sizing: border-box; }
body { margin: 0; font-family: "Avenir Next", "Segoe UI", sans-serif; background: radial-gradient(circle at top left, #fff7e4 0, var(--bg) 42%, #efe6d7 100%); color: var(--ink); }
.shell { max-width: 1320px; margin: 0 auto; padding: 32px 20px 48px; }
.hero { display: grid; grid-template-columns: 1.5fr 1fr; gap: 18px; margin-bottom: 18px; }
.card { background: var(--panel); border: 1px solid var(--line); border-radius: 22px; padding: 22px; box-shadow: var(--shadow); }
.hero h1, h2 { margin: 0 0 8px; font-family: "Iowan Old Style", "Palatino Linotype", Georgia, serif; }
.hero h1 { font-size: 2.2rem; line-height: 1.05; }
.eyebrow { text-transform: uppercase; letter-spacing: 0.12em; font-size: 0.74rem; color: var(--muted); margin: 0 0 12px; }
.hero-copy, .meta, .muted { color: var(--muted); }
.pill { display: inline-flex; align-items: center; gap: 8px; border-radius: 999px; padding: 8px 12px; font-weight: 600; font-size: 0.9rem; }
.pill-ready { background: var(--accent-soft); color: var(--accent); }
.pill-blocked { background: var(--warn-soft); color: var(--warn); }
.pill-link { background: #ece4d4; color: var(--ink); text-decoration: none; }
.status-stack { display: grid; gap: 10px; justify-items: end; }
.status-chip { display: inline-flex; align-items: center; gap: 8px; border-radius: 999px; padding: 8px 12px; font-weight: 600; font-size: 0.9rem; }
.status-chip-dot { width: 10px; height: 10px; border-radius: 999px; background: currentColor; opacity: 0.85; }
.status-chip-online { background: var(--accent-soft); color: var(--accent); }
.status-chip-offline { background: var(--warn-soft); color: var(--warn); }
.status-chip-degraded { background: #f4e6d6; color: #8f4e17; }
.status-chip-fallback { background: #ece4d4; color: var(--ink); }
.status-chip-checking { background: #ece4d4; color: var(--ink); }
.error-card { border: 1px solid #e6c7a5; background: #fff6ed; }
.grid { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 14px; margin-bottom: 18px; }
.throughput-grid { grid-template-columns: 1fr 3fr; }
.metric .value { font-size: 2rem; font-weight: 700; margin: 10px 0 6px; }
.metric .label { color: var(--muted); font-size: 0.88rem; text-transform: uppercase; letter-spacing: 0.08em; }
.graph { font-size: 1.8rem; line-height: 1; letter-spacing: 0.08em; }
.section { margin-bottom: 18px; }
.section-head { display: flex; justify-content: space-between; gap: 12px; align-items: flex-start; margin-bottom: 14px; }
.actions button, .subtle-button { border: none; border-radius: 999px; padding: 10px 14px; background: var(--ink); color: white; cursor: pointer; font: inherit; }
.subtle-button { padding: 6px 10px; font-size: 0.85rem; }
table { width: 100%; border-collapse: collapse; }
th, td { text-align: left; padding: 12px 10px; border-top: 1px solid var(--line); vertical-align: top; }
th { font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.08em; color: var(--muted); border-top: none; }
.state { display: inline-block; border-radius: 999px; padding: 5px 10px; font-size: 0.82rem; font-weight: 600; background: #ece4d4; }
.state-in-progress, .state-merging, .state-todo { background: #e8efe9; color: #255b43; }
.state-rework { background: #fff2d6; color: #946200; }
.state-human-review, .state-done { background: #f0e6ff; color: #5f4b8b; }
.activity-event { font-weight: 600; }
.activity-message { margin-top: 4px; color: var(--muted); max-width: 40ch; }
.activity-time { margin-top: 6px; color: var(--muted); font-size: 0.82rem; }
.empty { color: var(--muted); padding: 18px 10px; }
.mono, pre { font-family: "SFMono-Regular", Consolas, "Liberation Mono", Menlo, monospace; }
pre { margin: 0; white-space: pre-wrap; word-break: break-word; background: #1f1b16; color: #f8f2e7; border-radius: 16px; padding: 16px; overflow-x: auto; }
.workflow-note { margin-top: 10px; color: var(--muted); font-size: 0.92rem; }
.meta-grid { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px; }
.link-row { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 16px; }
.issue-stack, .detail-stack, .token-stack { display: grid; gap: 4px; }
.issue-title { color: var(--muted); max-width: 34ch; }
.issue-links, .issue-meta { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
.issue-link { color: var(--accent); text-decoration: none; font-size: 0.88rem; }
.meta-chip { display: inline-flex; align-items: center; gap: 6px; border-radius: 999px; padding: 4px 10px; font-size: 0.8rem; background: #efe6d7; color: var(--ink); }
.meta-chip-muted { background: #f4ede1; color: var(--muted); }
.numeric { font-variant-numeric: tabular-nums; }
@media (max-width: 960px) {
  .hero, .grid, .meta-grid, .throughput-grid { grid-template-columns: 1fr; }
  .section-head { flex-direction: column; }
}
"#;

const DASHBOARD_JS: &str = r#"
(() => {
  const initialNode = document.getElementById('initial-state');
  let currentState = initialNode ? JSON.parse(initialNode.textContent || '{}') : null;
  let stream;
  let fallbackPolling;
  let clockInterval;

  function escapeHtml(value) {
    return String(value ?? '')
      .replaceAll('&', '&amp;')
      .replaceAll('<', '&lt;')
      .replaceAll('>', '&gt;')
      .replaceAll('"', '&quot;')
      .replaceAll("'", '&#39;');
  }

  function formatInt(value) {
    return Number(value || 0).toLocaleString('en-US');
  }

  function formatDuration(seconds) {
    const totalSeconds = Math.max(0, Math.round(Number(seconds || 0)));
    const hours = Math.floor(totalSeconds / 3600);
    const minutes = Math.floor((totalSeconds % 3600) / 60);
    const secs = totalSeconds % 60;
    if (hours > 0) {
      return `${String(hours).padStart(2, '0')}:${String(minutes).padStart(2, '0')}:${String(secs).padStart(2, '0')}`;
    }
    return `${String(minutes).padStart(2, '0')}:${String(secs).padStart(2, '0')}`;
  }

  function slugify(value) {
    return String(value || '').trim().toLowerCase().replace(/[^a-z0-9]+/g, '-');
  }

  function formatTodoistDate(value) {
    if (!value) return '';
    if (typeof value === 'string') return value;
    if (typeof value === 'object') {
      if (value.date && value.string && value.string !== value.date) {
        return `${value.string} (${value.date})`;
      }
      if (value.date) return value.date;
      if (value.string) return value.string;
      if (value.datetime) return value.datetime;
    }
    return '';
  }

  function renderTaskMeta(entry) {
    const parts = [];
    (entry.labels || []).forEach((label) => {
      parts.push(`<span class="meta-chip">${escapeHtml(label)}</span>`);
    });

    const due = formatTodoistDate(entry.due);
    if (due) {
      parts.push(`<span class="meta-chip meta-chip-muted">Due ${escapeHtml(due)}</span>`);
    }

    const deadline = formatTodoistDate(entry.deadline);
    if (deadline) {
      parts.push(`<span class="meta-chip meta-chip-muted">Deadline ${escapeHtml(deadline)}</span>`);
    }

    return parts.length > 0
      ? `<div class="issue-meta">${parts.join('')}</div>`
      : '';
  }

  function updateStatus(mode, label, copy) {
    const badge = document.getElementById('runtime-status');
    const labelNode = document.getElementById('runtime-status-label');
    const copyNode = document.getElementById('runtime-status-copy');
    if (!badge || !labelNode || !copyNode) return;
    badge.className = `status-chip status-chip-${mode}`;
    labelNode.textContent = label;
    copyNode.textContent = copy;
  }

  function renderRunningRows(payload) {
    if (!payload.running || payload.running.length === 0) {
      return '<tr><td colspan="7" class="empty">No active task sessions.</td></tr>';
    }
    return payload.running.map((entry) => {
      const runtime = formatDuration(entry.runtime_seconds);
      const startedAt = entry.started_at || '';
      const lastEventAt = entry.last_event_at || 'n/a';
      const links = [
        `<a class="issue-link" href="/api/v1/${encodeURIComponent(entry.issue_identifier)}">Run JSON</a>`
      ];
      if (entry.url) {
        links.push(`<a class="issue-link" href="${escapeHtml(entry.url)}" target="_blank" rel="noreferrer noopener">Open task</a>`);
      }
      if (entry.project_url) {
        links.push(`<a class="issue-link" href="${escapeHtml(entry.project_url)}" target="_blank" rel="noreferrer noopener">Project board</a>`);
      }
      const sessionButton = entry.session_id
        ? `<button type="button" class="subtle-button" data-copy="${escapeHtml(entry.session_id)}">Copy ID</button>`
        : '<span class="muted">n/a</span>';
      return `<tr>
        <td>
          <div class="issue-stack">
            <span><strong>${escapeHtml(entry.issue_identifier)}</strong></span>
            <span class="issue-title">${escapeHtml(entry.title || 'Untitled task')}</span>
            <span class="muted">Worker: ${escapeHtml(entry.worker_host || 'local')}</span>
            <div class="issue-links">${links.join('')}</div>
            ${renderTaskMeta(entry)}
          </div>
        </td>
        <td><span class="state state-${slugify(entry.state)}">${escapeHtml(entry.state)}</span></td>
        <td>${sessionButton}</td>
        <td class="numeric">
          <span class="runtime-value" data-base-runtime="${Number(entry.runtime_seconds || 0)}" data-started-at="${escapeHtml(startedAt)}">${runtime}</span>
          <div class="muted">${entry.turn_count} turns</div>
        </td>
        <td>
          <div class="detail-stack">
            <span class="activity-event">${escapeHtml(entry.last_event || 'n/a')}</span>
            <span class="activity-message">${escapeHtml(entry.last_message || 'No Codex message yet.')}</span>
            <span class="activity-time mono">${escapeHtml(lastEventAt)}</span>
          </div>
        </td>
        <td class="numeric">
          <div class="token-stack">
            <span>Total: ${formatInt(entry.tokens?.total_tokens)}</span>
            <span class="muted">In ${formatInt(entry.tokens?.input_tokens)} / Out ${formatInt(entry.tokens?.output_tokens)}</span>
          </div>
        </td>
        <td class="mono"><div>${escapeHtml(entry.workspace)}</div><div class="muted">${escapeHtml(entry.worker_host || 'local')}</div></td>
      </tr>`;
    }).join('');
  }

  function renderRetryRows(payload) {
    if (!payload.retrying || payload.retrying.length === 0) {
      return '<tr><td colspan="4" class="empty">Retry queue is empty.</td></tr>';
    }
    return payload.retrying.map((entry) => `<tr>
      <td><strong>${escapeHtml(entry.issue_identifier)}</strong><div class="muted">Worker: ${escapeHtml(entry.worker_host || 'local')}</div>${entry.workspace_location ? `<div class="mono muted">${escapeHtml(entry.workspace_location)}</div>` : ''}</td>
      <td>${entry.attempt}</td>
      <td class="mono">${escapeHtml(entry.due_at)}</td>
      <td>${escapeHtml(entry.error || 'none')}</td>
    </tr>`).join('');
  }

  function applyLinks(payload) {
    const projectLink = document.getElementById('project-link');
    const dashboardLink = document.getElementById('dashboard-link');
    if (projectLink) {
      if (payload.links?.project_url) {
        projectLink.href = payload.links.project_url;
        projectLink.textContent = 'Todoist Board';
        projectLink.style.display = 'inline-flex';
      } else {
        projectLink.style.display = 'none';
      }
    }
    if (dashboardLink) {
      if (payload.links?.dashboard_url) {
        dashboardLink.href = payload.links.dashboard_url;
        dashboardLink.textContent = payload.links.dashboard_url;
        dashboardLink.style.display = 'inline-flex';
      } else {
        dashboardLink.style.display = 'none';
      }
    }
  }

  function applyState(payload) {
    currentState = payload;
    const errorCard = document.getElementById('error-card');
    const errorTitle = document.getElementById('error-title');
    const errorCopy = document.getElementById('error-copy');
    document.getElementById('generated-at').textContent = payload.generated_at || 'n/a';
    document.getElementById('metric-running').textContent = payload.counts?.running ?? 0;
    document.getElementById('metric-retrying').textContent = payload.counts?.retrying ?? 0;
    document.getElementById('metric-total-tokens').textContent = formatInt(payload.codex_totals?.total_tokens);
    document.getElementById('metric-token-breakdown').textContent =
      `${formatInt(payload.codex_totals?.input_tokens)} input / ${formatInt(payload.codex_totals?.output_tokens)} output`;
    const runtime = document.getElementById('metric-runtime');
    runtime.dataset.baseRuntime = String(Number(payload.codex_totals?.seconds_running || 0));
    runtime.textContent = formatDuration(payload.codex_totals?.seconds_running || 0);
    document.getElementById('metric-polling').textContent =
      payload.polling?.checking
        ? 'Checking now…'
        : `Next poll in ${payload.polling?.next_poll_in_ms != null ? Math.ceil(payload.polling.next_poll_in_ms / 1000) + 's' : 'n/a'}`;
    document.getElementById('metric-tps').textContent = `${Number(payload.throughput?.tps_5s || 0).toFixed(1)} tps`;
    document.getElementById('metric-graph').textContent = payload.throughput?.graph_10m || '';
    document.getElementById('running-body').innerHTML = renderRunningRows(payload);
    document.getElementById('retry-body').innerHTML = renderRetryRows(payload);
    document.getElementById('rate-limits-json').textContent = JSON.stringify(payload.rate_limits ?? 'No rate-limit payload observed yet.', null, 2);
    document.getElementById('todoist-rate-budget-json').textContent = JSON.stringify(payload.todoist_rate_budget ?? 'No Todoist budget observed yet.', null, 2);
    document.getElementById('polling-json').textContent = JSON.stringify(payload.polling ?? {}, null, 2);

    const workflowNote = document.getElementById('workflow-note');
    const workflowPath = document.getElementById('workflow-path');
    const workflowLastGood = document.getElementById('workflow-last-good');
    const workflowPill = document.getElementById('workflow-pill');
    if (workflowPill) {
      const blocked = payload.workflow?.dispatch_status !== 'ready';
      workflowPill.className = `pill ${blocked ? 'pill-blocked' : 'pill-ready'}`;
      workflowPill.textContent = `Dispatch ${payload.workflow?.dispatch_status || 'unknown'}`;
    }
    if (workflowNote) {
      workflowNote.textContent = payload.workflow?.blocking_reason || 'Workflow and dispatch configuration are healthy.';
    }
    if (workflowPath) {
      workflowPath.textContent = payload.workflow?.path || 'n/a';
    }
    if (workflowLastGood) {
      workflowLastGood.textContent = `Using last good config: ${payload.workflow?.using_last_good ? 'yes' : 'no'}`;
    }
    if (errorCard && errorTitle && errorCopy) {
      if (payload.error) {
        errorCard.hidden = false;
        errorTitle.textContent = payload.error.message || 'Observability degraded';
        errorCopy.textContent = payload.error.code || 'unknown_error';
      } else {
        errorCard.hidden = true;
        errorTitle.textContent = 'Observability degraded';
        errorCopy.textContent = '';
      }
    }
    applyLinks(payload);
    bindCopyButtons();
    updateRuntimeClocks();
  }

  function updateRuntimeClocks() {
    if (!currentState?.generated_at) return;
    const generatedAtMs = new Date(currentState.generated_at).getTime();
    const elapsedSeconds = Math.max(0, (Date.now() - generatedAtMs) / 1000);
    const runtime = document.getElementById('metric-runtime');
    if (runtime) {
      const base = Number(runtime.dataset.baseRuntime || '0');
      runtime.textContent = formatDuration(base + elapsedSeconds * Number(currentState.counts?.running || 0));
    }
    document.querySelectorAll('.runtime-value').forEach((node) => {
      const base = Number(node.dataset.baseRuntime || '0');
      node.textContent = formatDuration(base + elapsedSeconds);
    });
  }

  function bindCopyButtons() {
    document.querySelectorAll('[data-copy]').forEach((button) => {
      if (button.dataset.bound === 'true') return;
      button.dataset.bound = 'true';
      button.addEventListener('click', () => {
        navigator.clipboard.writeText(button.dataset.copy || '');
        const label = button.textContent;
        button.textContent = 'Copied';
        window.setTimeout(() => {
          button.textContent = label;
        }, 1200);
      });
    });
  }

  async function fetchState() {
    const response = await fetch('/api/v1/state', { cache: 'no-store' });
    const payload = await response.json();
    applyState(payload);
    if (payload.error) {
      updateStatus('degraded', 'Degraded', `${payload.error.message}. Polling /api/v1/state every 5s.`);
      return;
    }
    if (!response.ok) throw new Error(`status ${response.status}`);
    updateStatus('fallback', 'Polling', 'Streaming unavailable. Falling back to /api/v1/state every 5s.');
  }

  function startFallbackPolling() {
    if (fallbackPolling) return;
    fetchState().catch(() => {
      updateStatus('offline', 'Offline', 'Unable to reach /api/v1/state. Retrying every 5s.');
    });
    fallbackPolling = window.setInterval(() => {
      fetchState().catch(() => {
        updateStatus('offline', 'Offline', 'Unable to reach /api/v1/state. Retrying every 5s.');
      });
    }, 5000);
  }

  function stopFallbackPolling() {
    if (!fallbackPolling) return;
    window.clearInterval(fallbackPolling);
    fallbackPolling = null;
  }

  function handleStreamPayload(event) {
    const payload = JSON.parse(event.data);
    applyState(payload);
    if (payload.error) {
      updateStatus('degraded', 'Degraded', `${payload.error.message} via /api/v1/stream.`);
      return;
    }
    updateStatus('online', 'Live', 'Connected to /api/v1/stream for live updates.');
  }

  function connectStream() {
    if (!window.EventSource) {
      startFallbackPolling();
      return;
    }
    stream = new EventSource('/api/v1/stream');
    stream.onopen = () => {
      stopFallbackPolling();
      updateStatus('online', 'Live', 'Connected to /api/v1/stream for live updates.');
    };
    stream.onmessage = handleStreamPayload;
    stream.addEventListener('state', handleStreamPayload);
    stream.onerror = () => {
      if (stream) {
        stream.close();
        stream = null;
      }
      startFallbackPolling();
    };
  }

  document.getElementById('refresh-now')?.addEventListener('click', async () => {
    try {
      await fetch('/api/v1/refresh', { method: 'POST' });
    } catch (_error) {
      updateStatus('offline', 'Offline', 'Refresh failed. Retrying via live updates.');
    }
  });

  if (currentState) {
    applyState(currentState);
  }
  updateStatus('checking', 'Connecting', 'Connecting to /api/v1/stream…');
  connectStream();
  clockInterval = window.setInterval(updateRuntimeClocks, 1000);
})();
"#;

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Instant,
    };

    use chrono::{TimeZone, Utc};
    use serde_json::{Value, json};
    use tokio::time::{Duration, sleep};

    use crate::{
        config::ObservabilityConfig,
        issue::Issue,
        orchestrator::{IssueDetail, Orchestrator, PollingSnapshot, SnapshotCounts, TokenSnapshot},
        tracker::TrackerRateBudget,
        workflow::WorkflowStore,
    };

    use super::{
        CodexTotalsPayload, RunningEntryPayload, StatePayload, TerminalDashboard, TerminalRenderer,
        WorkflowPayload, dashboard_url, humanize_blocking_reason, render_dashboard_html,
        render_offline_status, render_terminal_dashboard, summarize_codex_message,
    };

    #[test]
    fn dashboard_url_normalizes_wildcard_hosts() {
        assert_eq!(
            dashboard_url("0.0.0.0", Some(0), Some(43_123)).as_deref(),
            Some("http://127.0.0.1:43123/")
        );
        assert_eq!(
            dashboard_url("::1", Some(4_000), None).as_deref(),
            Some("http://[::1]:4000/")
        );
    }

    #[test]
    fn render_offline_status_marks_app_offline() {
        let rendered = render_offline_status();
        assert!(rendered.contains("app_status=offline"));
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
    fn terminal_dashboard_renders_live_sections() {
        let payload = sample_payload();
        let rendered = render_terminal_dashboard(
            &payload,
            Some(super::TerminalViewport {
                columns: 132,
                rows: 32,
            }),
        );
        assert!(rendered.contains("SYMPHONY STATUS"));
        assert!(rendered.contains("Graph: "));
        assert!(rendered.contains("ABC-123"));
        assert!(rendered.contains("Backoff queue"));
        assert!(rendered.contains("Todoist Budget"));
        assert!(rendered.contains("24/300 remaining"));
    }

    #[test]
    fn terminal_dashboard_respects_viewport_width() {
        let payload = sample_payload();
        let rendered = render_terminal_dashboard(
            &payload,
            Some(super::TerminalViewport {
                columns: 72,
                rows: 32,
            }),
        );

        for line in rendered.lines() {
            assert!(
                super::strip_ansi_sequences(line).chars().count() <= 72,
                "line exceeded viewport width: {:?}",
                super::strip_ansi_sequences(line)
            );
        }
    }

    #[test]
    fn terminal_dashboard_clips_to_viewport_height() {
        let payload = sample_payload();
        let rendered = render_terminal_dashboard(
            &payload,
            Some(super::TerminalViewport {
                columns: 132,
                rows: 8,
            }),
        );

        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 8);
        assert!(
            lines
                .last()
                .is_some_and(|line| line.contains("more lines hidden"))
        );
    }

    #[test]
    fn dashboard_html_bootstraps_sse_without_page_reload() {
        let payload = sample_payload();
        let html = render_dashboard_html(&payload);
        assert!(html.contains("new EventSource('/api/v1/stream')"));
        assert!(html.contains("stream.addEventListener('state', handleStreamPayload);"));
        assert!(html.contains("\"generated_at\":\"2026-03-11T00:00:00Z\""));
        assert!(!html.contains("&quot;generated_at&quot;"));
        assert!(html.contains("window.setInterval(updateRuntimeClocks, 1000)"));
        assert!(!html.contains("window.location.reload"));
        assert!(html.contains("Todoist Budget"));
        assert!(html.contains("todoist-rate-budget-json"));
    }

    #[test]
    fn dashboard_html_mentions_todoist_task_links_and_metadata() {
        let html = render_dashboard_html(&sample_payload());
        assert!(html.contains("Open task"));
        assert!(html.contains("Project board"));
        assert!(html.contains("\"worker_host\":\"ssh-a\""));
        assert!(html.contains("\"workspace_location\":\"/srv/symphony/ABC-456\""));
        assert!(html.contains("Due "));
        assert!(html.contains("Deadline "));
    }

    #[test]
    fn dashboard_html_includes_error_card_shell() {
        let html = render_dashboard_html(&sample_payload());
        assert!(html.contains("id=\"error-card\""));
        assert!(html.contains("id=\"error-title\""));
        assert!(html.contains("id=\"error-copy\""));
    }

    #[test]
    fn present_issue_detail_includes_todoist_tracked_metadata() {
        let payload = super::Presenter::present_issue_detail(IssueDetail {
            issue_identifier: "TD-123".to_string(),
            issue_id: Some("123".to_string()),
            status: "running".to_string(),
            tracked_issue: Some(Issue {
                id: "123".to_string(),
                identifier: "TD-123".to_string(),
                title: "Ship Todoist-native dashboard".to_string(),
                state: "In Progress".to_string(),
                url: Some("https://app.todoist.com/app/task/123".to_string()),
                labels: vec!["frontend".to_string(), "todoist".to_string()],
                project_id: Some("proj-1".to_string()),
                due: Some(json!({"date": "2026-03-12", "string": "tomorrow"})),
                deadline: Some(json!({"date": "2026-03-14"})),
                ..Issue::default()
            }),
            workspace: crate::orchestrator::WorkspaceDetail {
                path: "/tmp/symphony/TD-123".to_string(),
                worker_host: Some("ssh-a".to_string()),
            },
            attempts: crate::orchestrator::AttemptDetail {
                restart_count: 0,
                current_retry_attempt: None,
            },
            running: None,
            retry: None,
            recent_events: Vec::new(),
            last_error: None,
            last_error_stage: None,
            last_error_kind: None,
        });

        assert_eq!(payload.tracked["title"], "Ship Todoist-native dashboard");
        assert_eq!(
            payload.tracked["project_url"],
            "https://app.todoist.com/app/project/proj-1"
        );
        assert_eq!(payload.tracked["labels"][0], "frontend");
        assert_eq!(payload.tracked["due"]["date"], "2026-03-12");
        assert_eq!(payload.workspace.worker_host.as_deref(), Some("ssh-a"));
    }

    #[test]
    fn summarize_codex_message_strips_ansi_and_control_bytes() {
        let payload = concat!(
            "cmd: \u{1b}[31mRED\u{1b}[0m ",
            "\u{1b}]8;;https://example.com\u{1b}\\Open PR\u{1b}]8;;\u{1b}\\",
            "\u{0} after\nline"
        );
        assert_eq!(
            summarize_codex_message(Some("notification"), Some(payload)).as_deref(),
            Some("cmd: RED Open PR after line")
        );
    }

    #[test]
    fn strip_ansi_sequences_removes_csi_osc_and_single_char_escapes() {
        let payload = concat!(
            "before ",
            "\u{1b}[31mRED\u{1b}[0m ",
            "\u{1b}]0;Symphony Dashboard\u{7}",
            "\u{1b}]8;;https://example.com\u{1b}\\Open PR\u{1b}]8;;\u{1b}\\ ",
            "\u{1b}7",
            "after"
        );

        assert_eq!(
            super::strip_ansi_sequences(payload),
            "before RED Open PR after"
        );
    }

    #[test]
    fn humanizes_full_codex_event_set() {
        let event_cases = [
            (
                "turn/started",
                json!({"method": "turn/started", "params": {"turn": {"id": "turn-1"}}}),
                "turn started",
            ),
            (
                "turn/completed",
                json!({"method": "turn/completed", "params": {"turn": {"status": "completed"}}}),
                "turn completed",
            ),
            (
                "turn/diff/updated",
                json!({"method": "turn/diff/updated", "params": {"diff": "line1\nline2"}}),
                "turn diff updated",
            ),
            (
                "turn/plan/updated",
                json!({"method": "turn/plan/updated", "params": {"plan": [{"step":"a"},{"step":"b"}]}}),
                "plan updated",
            ),
            (
                "thread/tokenUsage/updated",
                json!({"method": "thread/tokenUsage/updated", "params": {"usage": {"input_tokens": 8, "output_tokens": 3, "total_tokens": 11}}}),
                "thread token usage updated",
            ),
            (
                "item/started",
                json!({"method": "item/started", "params": {"item": {"type": "commandExecution", "status": "running"}}}),
                "item started: command execution",
            ),
            (
                "item/completed",
                json!({"method": "item/completed", "params": {"item": {"type": "fileChange", "status": "completed"}}}),
                "item completed: file change",
            ),
            (
                "item/agentMessage/delta",
                json!({"method": "item/agentMessage/delta", "params": {"delta": "hello"}}),
                "agent message streaming",
            ),
            (
                "item/plan/delta",
                json!({"method": "item/plan/delta", "params": {"delta": "step"}}),
                "plan streaming",
            ),
            (
                "item/reasoning/summaryTextDelta",
                json!({"method": "item/reasoning/summaryTextDelta", "params": {"summaryText": "thinking"}}),
                "reasoning summary streaming",
            ),
            (
                "item/reasoning/summaryPartAdded",
                json!({"method": "item/reasoning/summaryPartAdded", "params": {"summaryText": "section"}}),
                "reasoning summary section added",
            ),
            (
                "item/reasoning/textDelta",
                json!({"method": "item/reasoning/textDelta", "params": {"textDelta": "reason"}}),
                "reasoning text streaming",
            ),
            (
                "item/commandExecution/outputDelta",
                json!({"method": "item/commandExecution/outputDelta", "params": {"outputDelta": "ok"}}),
                "command output streaming",
            ),
            (
                "item/fileChange/outputDelta",
                json!({"method": "item/fileChange/outputDelta", "params": {"outputDelta": "changed"}}),
                "file change output streaming",
            ),
            (
                "item/commandExecution/requestApproval",
                json!({"method": "item/commandExecution/requestApproval", "params": {"parsedCmd": "git status"}}),
                "command approval requested",
            ),
            (
                "item/fileChange/requestApproval",
                json!({"method": "item/fileChange/requestApproval", "params": {"fileChangeCount": 2}}),
                "file change approval requested",
            ),
            (
                "item/tool/call",
                json!({"method": "item/tool/call", "params": {"tool": "todoist"}}),
                "dynamic tool call requested",
            ),
            (
                "item/tool/requestUserInput",
                json!({"method": "item/tool/requestUserInput", "params": {"question": "Continue?"}}),
                "tool requires user input: Continue?",
            ),
            (
                "codex/event/exec_command_begin",
                json!({"method": "codex/event/exec_command_begin", "params": {"msg": {"command": "git status --short"}}}),
                "git status --short",
            ),
            (
                "codex/event/agent_message_content_delta",
                json!({"method": "codex/event/agent_message_content_delta", "params": {"msg": {"content": "structured update"}}}),
                "agent message content streaming",
            ),
        ];

        for (_method, payload, expected_fragment) in event_cases {
            let text = summarize_codex_message(None, Some(&payload.to_string())).expect("summary");
            assert!(
                text.contains(expected_fragment),
                "expected {expected_fragment:?} in {text:?}"
            );
        }
    }

    #[test]
    fn humanizes_dynamic_tool_wrapper_events() {
        let completed = json!({
            "event": "tool_call_completed",
            "message": {
                "payload": {"method": "item/tool/call", "params": {"name": "todoist", "arguments": {"action": "move_task"}}}
            }
        });
        let failed = json!({
            "event": "tool_call_failed",
            "message": {
                "payload": {"method": "item/tool/call", "params": {"tool": "todoist", "arguments": {"action": "close_task"}}}
            }
        });
        let unsupported = json!({
            "event": "unsupported_tool_call",
            "message": {
                "payload": {"method": "item/tool/call", "params": {"tool": "unknown_tool"}}
            }
        });

        assert_eq!(
            summarize_message_payload(&completed).as_deref(),
            Some("dynamic tool call completed (todoist:move_task)")
        );
        assert_eq!(
            summarize_message_payload(&failed).as_deref(),
            Some("dynamic tool call failed (todoist:close_task)")
        );
        assert_eq!(
            summarize_message_payload(&unsupported).as_deref(),
            Some("unsupported dynamic tool call rejected (unknown_tool)")
        );
    }

    #[test]
    fn humanizes_nested_codex_payload_envelopes() {
        let wrapped = json!({
            "event": "notification",
            "message": {
                "payload": {
                    "method": "turn/completed",
                    "params": {
                        "turn": {"status": "completed"},
                        "usage": {"input_tokens": "10", "output_tokens": 2, "total_tokens": 12}
                    }
                },
                "raw": "{\"method\":\"turn/completed\"}"
            }
        });

        let text = summarize_message_payload(&wrapped).expect("summary");
        assert!(text.contains("turn completed"));
        assert!(text.contains("in 10"));
    }

    #[test]
    fn uses_shell_command_line_as_exec_command_status_text() {
        let payload = json!({
            "event": "notification",
            "message": {
                "method": "codex/event/exec_command_begin",
                "params": {"msg": {"command": "git status --short"}}
            }
        });

        assert_eq!(
            summarize_message_payload(&payload).as_deref(),
            Some("git status --short")
        );
    }

    #[test]
    fn formats_auto_approval_updates_from_codex() {
        let payload = json!({
            "event": "approval_auto_approved",
            "message": {
                "payload": {
                    "method": "item/commandExecution/requestApproval",
                    "params": {"parsedCmd": "cargo test"}
                },
                "decision": "acceptForSession"
            }
        });

        let text = summarize_message_payload(&payload).expect("summary");
        assert!(text.contains("command approval requested"));
        assert!(text.contains("auto-approved"));
    }

    #[test]
    fn formats_auto_answered_tool_input_updates_from_codex() {
        let payload = json!({
            "event": "tool_input_auto_answered",
            "message": {
                "payload": {
                    "method": "item/tool/requestUserInput",
                    "params": {"question": "Continue?"}
                },
                "answer": "This is a non-interactive session. Operator input is unavailable."
            }
        });

        let text = summarize_message_payload(&payload).expect("summary");
        assert!(text.contains("tool requires user input"));
        assert!(text.contains("auto-answered"));
    }

    #[test]
    fn enriches_wrapper_reasoning_and_message_streaming_events_with_payload_context() {
        let reasoning = json!({
            "event": "notification",
            "message": {
                "method": "codex/event/agent_reasoning",
                "params": {
                    "msg": {
                        "payload": {"summaryText": "compare Todoist activity history against workpad state"}
                    }
                }
            }
        });
        let message_delta = json!({
            "event": "notification",
            "message": {
                "method": "codex/event/agent_message_delta",
                "params": {
                    "msg": {
                        "payload": {"delta": "refreshing Todoist workpad comment"}
                    }
                }
            }
        });
        let fallback_reasoning = json!({
            "event": "notification",
            "message": {
                "method": "codex/event/agent_reasoning",
                "params": { "msg": {"payload": {}} }
            }
        });

        let reasoning_text = summarize_message_payload(&reasoning).expect("summary");
        let message_text = summarize_message_payload(&message_delta).expect("summary");
        let fallback_text = summarize_message_payload(&fallback_reasoning).expect("summary");

        assert!(
            reasoning_text.contains(
                "reasoning update: compare Todoist activity history against workpad state"
            )
        );
        assert!(
            message_text.contains("agent message streaming: refreshing Todoist workpad comment")
        );
        assert_eq!(fallback_text, "reasoning update");
    }

    #[test]
    fn running_row_expands_to_requested_terminal_width() {
        let entry = sample_payload().running.into_iter().next().expect("entry");
        let running_event_width = 52;
        let row = super::format_running_row(&entry, running_event_width);
        let plain = super::strip_ansi_sequences(&row);
        let expected_width = 9
            + super::RUNNING_STATUS_WIDTH
            + super::RUNNING_ID_WIDTH
            + super::RUNNING_STATE_WIDTH
            + super::RUNNING_SESSION_WIDTH
            + super::RUNNING_PID_WIDTH
            + super::RUNNING_RUNTIME_WIDTH
            + running_event_width
            + super::RUNNING_TOKENS_WIDTH;

        assert_eq!(plain.chars().count(), expected_width);
        assert!(plain.contains("tool requires user input"));
    }

    #[test]
    fn running_row_sanitizes_escape_sequences_in_last_message() {
        let mut entry = sample_payload().running.into_iter().next().expect("entry");
        entry.last_message = Some(
            concat!(
                "cmd: \u{1b}[31mRED\u{1b}[0m ",
                "\u{1b}]8;;https://example.com\u{1b}\\Open PR\u{1b}]8;;\u{1b}\\",
                "\u{0} after\nline"
            )
            .to_string(),
        );

        let row = super::format_running_row(&entry, 52);
        let plain = super::strip_ansi_sequences(&row);

        assert!(plain.contains("cmd: RED Open PR after line"));
        assert!(!plain.contains('\u{1b}'));
        assert!(!plain.contains('\u{0}'));
    }

    #[test]
    fn tps_graph_matches_steady_throughput_snapshot() {
        let mut presenter = super::Presenter::default();
        let now_ms = 600_000;
        let current_tokens = 6_000;
        for timestamp in (0..=575_000).rev().step_by(25_000) {
            presenter.capture_token_sample(timestamp, (timestamp / 100) as u64);
        }

        assert_eq!(
            presenter.tps_graph(now_ms, current_tokens),
            "████████████████████████"
        );
    }

    #[test]
    fn computes_rolling_tps_and_stable_graph() {
        let mut presenter = super::Presenter::default();
        presenter.capture_token_sample(9_000, 20);
        presenter.capture_token_sample(9_500, 30);
        assert_eq!(presenter.rolling_tps(10_000, 40), 20.0);

        let mut graph_presenter = super::Presenter::default();
        let now_ms = 600_000;
        let mut current_tokens = 0u64;
        for timestamp in (0..=now_ms).step_by(25_000) {
            current_tokens += 500;
            graph_presenter.capture_token_sample(timestamp, current_tokens);
        }
        let first = graph_presenter.tps_graph(now_ms, current_tokens);
        let second = graph_presenter.tps_graph(now_ms + 1_000, current_tokens + 20);
        assert_eq!(first.chars().count(), 24);
        assert_eq!(second.chars().count(), 24);
        assert_eq!(
            first.chars().take(23).collect::<String>(),
            second.chars().take(23).collect::<String>()
        );
    }

    #[test]
    fn humanizes_known_blocking_reasons() {
        assert_eq!(
            humanize_blocking_reason("workflow_front_matter_not_a_map"),
            "workflow front matter must decode to a map"
        );
        assert_eq!(
            humanize_blocking_reason("todoist_comments_unavailable"),
            "Todoist comments are unavailable on this account"
        );
        assert_eq!(
            humanize_blocking_reason("todoist_project_not_found proj-123"),
            "configured Todoist project was not found"
        );
    }

    #[test]
    fn render_snapshot_timeout_status_marks_snapshot_timeout() {
        assert!(super::render_snapshot_timed_out_status().contains("snapshot timed out"));
    }

    #[tokio::test]
    async fn presenter_snapshot_failure_preserves_distinct_error_codes() {
        let (orchestrator, workflow_store) = test_runtime().await;
        let presenter = super::Presenter::default();
        let timeout_payload = presenter.present_snapshot_failure(
            super::OrchestratorHandleError::TimedOut,
            &workflow_store,
            Some("127.0.0.1:4000".parse().expect("addr")),
        );
        let unavailable_payload = presenter.present_snapshot_failure(
            super::OrchestratorHandleError::Unavailable,
            &workflow_store,
            Some("127.0.0.1:4000".parse().expect("addr")),
        );

        orchestrator.shutdown().await;

        assert_eq!(
            timeout_payload.error.as_ref().map(|error| error.code),
            Some("snapshot_timeout")
        );
        assert_eq!(
            timeout_payload.error.as_ref().map(|error| error.message),
            Some("Orchestrator snapshot timed out")
        );
        assert_eq!(
            unavailable_payload.error.as_ref().map(|error| error.code),
            Some("snapshot_unavailable")
        );
        assert_eq!(
            unavailable_payload
                .error
                .as_ref()
                .map(|error| error.message),
            Some("Orchestrator snapshot unavailable")
        );
    }

    #[tokio::test]
    async fn terminal_dashboard_skips_non_interactive_sessions() {
        let (orchestrator, workflow_store) = test_runtime().await;
        let renders = Arc::new(AtomicUsize::new(0));
        let renderer = {
            let renders = Arc::clone(&renders);
            TerminalRenderer {
                init: Arc::new(|| {}),
                render: Arc::new(move |_content: String| {
                    renders.fetch_add(1, Ordering::SeqCst);
                }),
                shutdown: Arc::new(|| {}),
            }
        };

        let dashboard = TerminalDashboard::start_with_renderer(
            orchestrator.handle(),
            workflow_store.clone(),
            ObservabilityConfig {
                terminal_enabled: true,
                refresh_ms: 25,
                render_interval_ms: 10,
            },
            Some("127.0.0.1:4000".parse().expect("addr")),
            renderer,
            false,
        );

        sleep(Duration::from_millis(40)).await;
        dashboard.shutdown().await;
        orchestrator.shutdown().await;
        assert_eq!(renders.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn terminal_dashboard_coalesces_rapid_updates() {
        let (orchestrator, workflow_store) = test_runtime().await;
        let renders = Arc::new(AtomicUsize::new(0));
        let renderer = {
            let renders = Arc::clone(&renders);
            TerminalRenderer {
                init: Arc::new(|| {}),
                render: Arc::new(move |_content: String| {
                    renders.fetch_add(1, Ordering::SeqCst);
                }),
                shutdown: Arc::new(|| {}),
            }
        };

        let handle = orchestrator.handle();
        let dashboard = TerminalDashboard::start_with_renderer(
            handle.clone(),
            workflow_store.clone(),
            ObservabilityConfig {
                terminal_enabled: true,
                refresh_ms: 10_000,
                render_interval_ms: 25,
            },
            Some("127.0.0.1:4000".parse().expect("addr")),
            renderer,
            true,
        );

        sleep(Duration::from_millis(35)).await;
        let initial = renders.load(Ordering::SeqCst);
        assert!(initial >= 1);

        let _ = handle.refresh().await;
        let _ = handle.refresh().await;
        let _ = handle.refresh().await;
        sleep(Duration::from_millis(50)).await;

        dashboard.shutdown().await;
        orchestrator.shutdown().await;
        assert!(renders.load(Ordering::SeqCst) <= initial + 2);
    }

    #[test]
    fn render_terminal_frame_clears_without_trailing_newline() {
        let mut bytes = Vec::new();
        super::render_terminal_frame_into(&mut bytes, "line 1\nline 2").expect("frame");
        let rendered = String::from_utf8(bytes).expect("utf8");
        assert!(rendered.starts_with(super::ANSI_HOME));
        assert!(rendered.contains(super::ANSI_HOME));
        assert!(rendered.contains("line 1"));
        assert!(rendered.contains("line 2"));
        assert!(rendered.contains(super::ANSI_CLEAR_TO_EOL));
        assert!(rendered.ends_with(super::ANSI_CLEAR_DOWN));
        assert!(!rendered.ends_with("\n"));
    }

    #[test]
    fn alternate_screen_init_and_shutdown_emit_expected_sequences() {
        let mut init = Vec::new();
        let mut shutdown = Vec::new();
        super::enter_alternate_screen_into(&mut init).expect("init");
        super::leave_alternate_screen_into(&mut shutdown).expect("shutdown");

        let init = String::from_utf8(init).expect("utf8");
        let shutdown = String::from_utf8(shutdown).expect("utf8");

        assert_eq!(
            init,
            format!(
                "{}{}{}{}",
                super::ANSI_ENTER_ALTERNATE_SCREEN,
                super::ANSI_HIDE_CURSOR,
                super::ANSI_HOME,
                super::ANSI_CLEAR_DOWN
            )
        );
        assert_eq!(
            shutdown,
            format!(
                "{}{}",
                super::ANSI_SHOW_CURSOR,
                super::ANSI_LEAVE_ALTERNATE_SCREEN
            )
        );
    }

    #[tokio::test]
    async fn degraded_snapshot_dashboard_includes_context() {
        let (orchestrator, workflow_store) = test_runtime().await;
        let rendered = super::render_snapshot_unavailable_dashboard(
            &workflow_store,
            Some("127.0.0.1:4000".parse().expect("addr")),
            12.5,
            Some(&PollingSnapshot {
                checking: false,
                next_poll_in_ms: Some(2_000),
                poll_interval_ms: 5_000,
            }),
        );

        orchestrator.shutdown().await;
        let plain = super::strip_ansi_sequences(&rendered);
        assert!(plain.contains("Orchestrator snapshot unavailable"));
        assert!(plain.contains("Throughput: 12.5 tps"));
        assert!(plain.contains("Project: https://app.todoist.com/app/project/proj"));
        assert!(plain.contains("Dashboard: http://127.0.0.1:4000/"));
        assert!(plain.contains("Next refresh: 2s"));
    }

    #[tokio::test]
    async fn degraded_snapshot_dashboard_marks_timeout_context() {
        let (orchestrator, workflow_store) = test_runtime().await;
        let rendered = super::render_snapshot_failure_dashboard(
            super::OrchestratorHandleError::TimedOut,
            &workflow_store,
            Some("127.0.0.1:4000".parse().expect("addr")),
            4.2,
            None,
        );

        orchestrator.shutdown().await;
        let plain = super::strip_ansi_sequences(&rendered);
        assert!(plain.contains("Orchestrator snapshot timed out"));
        assert!(plain.contains("Throughput: 4.2 tps"));
        assert!(plain.contains("Next refresh: n/a"));
    }

    #[test]
    fn pending_terminal_flush_is_reused_until_fire_time() {
        let renders = Arc::new(AtomicUsize::new(0));
        let renderer: Arc<dyn Fn(String) + Send + Sync> = {
            let renders = Arc::clone(&renders);
            Arc::new(move |_content: String| {
                renders.fetch_add(1, Ordering::SeqCst);
            })
        };
        let now = Instant::now();
        let due_at = now + Duration::from_millis(20);
        let mut render_state = super::TerminalRenderState {
            last_rendered_at: Some(now),
            pending_content: Some("old pending".to_string()),
            flush_due_at: Some(due_at),
            ..Default::default()
        };

        super::maybe_enqueue_terminal_frame(
            &mut render_state,
            "new pending".to_string(),
            42,
            50,
            &renderer,
        );

        assert_eq!(renders.load(Ordering::SeqCst), 0);
        assert_eq!(render_state.pending_content.as_deref(), Some("new pending"));
        assert_eq!(render_state.flush_due_at, Some(due_at));
    }

    #[test]
    fn terminal_snapshot_schedule_reuses_earliest_due_time() {
        let now = Instant::now();
        let existing_due_at = now + Duration::from_millis(20);
        let mut render_state = super::TerminalRenderState {
            last_rendered_at: Some(now),
            snapshot_due_at: Some(existing_due_at),
            ..Default::default()
        };

        super::schedule_terminal_snapshot(&mut render_state, 50, false);

        assert_eq!(render_state.snapshot_due_at, Some(existing_due_at));
    }

    #[test]
    fn terminal_snapshot_schedule_forces_immediate_refresh_when_requested() {
        let now = Instant::now();
        let mut render_state = super::TerminalRenderState {
            last_rendered_at: Some(now),
            ..Default::default()
        };

        super::schedule_terminal_snapshot(&mut render_state, 50, true);

        assert!(render_state.snapshot_due_at.is_some());
        assert!(render_state.snapshot_due_at.expect("due_at") <= Instant::now());
    }

    fn sample_payload() -> StatePayload {
        StatePayload {
            generated_at: Utc.with_ymd_and_hms(2026, 3, 11, 0, 0, 0).unwrap(),
            error: None,
            counts: SnapshotCounts {
                running: 1,
                retrying: 1,
            },
            agent_limits: super::AgentLimitsPayload {
                max_concurrent_agents: 10,
            },
            running: vec![RunningEntryPayload {
                issue_id: "issue-123".to_string(),
                issue_identifier: "ABC-123".to_string(),
                title: "Review Todoist-native observability".to_string(),
                state: "In Progress".to_string(),
                url: Some("https://app.todoist.com/app/task/issue-123".to_string()),
                project_url: Some("https://app.todoist.com/app/project/proj".to_string()),
                labels: vec!["frontend".to_string(), "todoist".to_string()],
                due: Some(json!({"date": "2026-03-12", "string": "tomorrow"})),
                deadline: Some(json!({"date": "2026-03-14"})),
                worker_host: Some("ssh-a".to_string()),
                session_id: Some("sess-123".to_string()),
                app_server_pid: Some(4242),
                turn_count: 2,
                last_event: Some("item/tool/requestUserInput".to_string()),
                last_message: Some("tool requires user input: Need approval?".to_string()),
                started_at: Utc.with_ymd_and_hms(2026, 3, 10, 23, 59, 0).unwrap(),
                last_event_at: Some(Utc.with_ymd_and_hms(2026, 3, 11, 0, 0, 0).unwrap()),
                runtime_seconds: 60.0,
                workspace: "/tmp/symphony-http-tests/ABC-123".to_string(),
                tokens: TokenSnapshot {
                    input_tokens: 11,
                    output_tokens: 7,
                    total_tokens: 18,
                },
            }],
            retrying: vec![crate::orchestrator::RetrySnapshot {
                issue_id: "issue-456".to_string(),
                issue_identifier: "ABC-456".to_string(),
                attempt: 2,
                due_at: Utc.with_ymd_and_hms(2026, 3, 11, 0, 1, 0).unwrap(),
                worker_host: Some("ssh-b".to_string()),
                workspace_location: Some("/srv/symphony/ABC-456".to_string()),
                error: Some("boom".to_string()),
                error_stage: None,
                error_kind: None,
            }],
            codex_totals: CodexTotalsPayload {
                input_tokens: 11,
                output_tokens: 7,
                total_tokens: 18,
                seconds_running: 12.5,
            },
            rate_limits: Some(json!({
                "primary": { "remaining": 10, "limit": 100, "reset_in_seconds": 30 },
                "secondary": { "remaining": 40, "limit": 50, "reset_in_seconds": 12 },
                "credits": { "has_credits": true, "balance": 42.0 }
            })),
            todoist_rate_budget: Some(TrackerRateBudget {
                service: "todoist".to_string(),
                limit: Some(300),
                remaining: Some(24),
                reset_at: Some(Utc.with_ymd_and_hms(2026, 3, 11, 0, 5, 0).unwrap()),
                reset_in_seconds: Some(300),
                retry_after_seconds: None,
                throttled_until: None,
                throttled_for_seconds: None,
                next_request_at: Some(Utc.with_ymd_and_hms(2026, 3, 11, 0, 0, 1).unwrap()),
                next_request_in_seconds: Some(1),
                observed_at: Some(Utc.with_ymd_and_hms(2026, 3, 11, 0, 0, 0).unwrap()),
            }),
            polling: PollingSnapshot {
                checking: false,
                next_poll_in_ms: Some(5_000),
                poll_interval_ms: 5_000,
            },
            workflow: WorkflowPayload {
                path: "/tmp/symphony-http-tests/WORKFLOW.md".to_string(),
                dispatch_status: "ready",
                blocking_reason: None,
                using_last_good: false,
            },
            links: super::LinksPayload {
                project_url: Some("https://app.todoist.com/app/project/proj".to_string()),
                dashboard_url: Some("http://127.0.0.1:4000/".to_string()),
            },
            throughput: super::ThroughputPayload {
                tps_5s: 18.2,
                graph_10m: "▁▂▃▄▅▆▇█".to_string(),
            },
        }
    }

    async fn test_runtime() -> (Orchestrator, WorkflowStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture_path = dir.path().join("memory.json");
        std::fs::write(
            &fixture_path,
            r#"{
  "tasks": [],
  "sections": [
    {"id":"sec-todo","project_id":"proj","name":"Todo"},
    {"id":"sec-in-progress","project_id":"proj","name":"In Progress"},
    {"id":"sec-done","project_id":"proj","name":"Done"}
  ],
  "user_plan_limits": {"comments": true}
}"#,
        )
        .expect("fixture");
        let workflow_path = dir.path().join("WORKFLOW.md");
        std::fs::write(
            &workflow_path,
            format!(
                r#"---
tracker:
  kind: memory
  fixture_path: {}
  project_id: proj
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
workspace:
  root: /tmp/symphony-observability-tests
observability:
  terminal_enabled: true
  refresh_ms: 25
  render_interval_ms: 10
---

test
"#,
                fixture_path.display()
            ),
        )
        .expect("workflow");
        let workflow_store = WorkflowStore::new(workflow_path.clone()).expect("store");
        let orchestrator = Orchestrator::start(workflow_store.clone())
            .await
            .expect("orchestrator");
        (orchestrator, workflow_store)
    }

    fn summarize_message_payload(payload: &Value) -> Option<String> {
        let event = payload.get("event").and_then(Value::as_str);
        let message = payload.get("message").map(Value::to_string);
        summarize_codex_message(event, message.as_deref())
    }
}
