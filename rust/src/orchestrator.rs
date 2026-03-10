use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    codex::{AppServerClient, CodexEvent},
    config::ServiceConfig,
    issue::{Issue, normalize_state_name},
    prompt,
    tracker::{TrackerClient, build_tracker_client},
    workflow::WorkflowStore,
    workspace::{Workspace, workspace_path_for_identifier},
};

#[derive(Clone)]
pub struct OrchestratorHandle {
    tx: mpsc::Sender<Command>,
}

pub struct Orchestrator {
    tx: mpsc::Sender<Command>,
    join: JoinHandle<()>,
}

#[derive(Default)]
struct State {
    poll_interval_ms: u64,
    max_concurrent_agents: usize,
    max_retry_backoff_ms: u64,
    next_poll_due_at: Option<Instant>,
    poll_check_in_progress: bool,
    running: BTreeMap<String, RunningEntry>,
    completed: BTreeSet<String>,
    claimed: BTreeSet<String>,
    retry_attempts: BTreeMap<String, RetryEntry>,
    codex_totals: CodexTotals,
    codex_rate_limits: Option<Value>,
}

struct RunningEntry {
    issue: Issue,
    identifier: String,
    workspace_path: PathBuf,
    cancel: CancellationToken,
    join: JoinHandle<()>,
    session_id: Option<String>,
    last_codex_message: Option<String>,
    last_codex_event: Option<String>,
    last_codex_timestamp: Option<DateTime<Utc>>,
    codex_app_server_pid: Option<u32>,
    codex_input_tokens: u64,
    codex_output_tokens: u64,
    codex_total_tokens: u64,
    last_reported_input_tokens: u64,
    last_reported_output_tokens: u64,
    last_reported_total_tokens: u64,
    turn_count: u32,
    retry_attempt: u32,
    started_at: DateTime<Utc>,
    recent_events: Vec<RecentEvent>,
}

struct RetryEntry {
    attempt: u32,
    due_at: Instant,
    identifier: String,
    error: Option<String>,
    cancel: CancellationToken,
}

#[derive(Clone, Copy, Default)]
struct CodexTotals {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    ended_seconds: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub generated_at: DateTime<Utc>,
    pub counts: SnapshotCounts,
    pub running: Vec<RunningSnapshot>,
    pub retrying: Vec<RetrySnapshot>,
    pub codex_totals: SnapshotTotals,
    pub rate_limits: Option<Value>,
    pub polling: PollingSnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotCounts {
    pub running: usize,
    pub retrying: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunningSnapshot {
    pub issue_id: String,
    pub issue_identifier: String,
    pub state: String,
    pub session_id: Option<String>,
    pub turn_count: u32,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub workspace: String,
    pub tokens: TokenSnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct RetrySnapshot {
    pub issue_id: String,
    pub issue_identifier: String,
    pub attempt: u32,
    pub due_at: DateTime<Utc>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub seconds_running: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TokenSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PollingSnapshot {
    pub checking: bool,
    pub next_poll_in_ms: Option<u64>,
    pub poll_interval_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct IssueDetail {
    pub issue_identifier: String,
    pub issue_id: Option<String>,
    pub status: String,
    pub workspace: WorkspaceDetail,
    pub attempts: AttemptDetail,
    pub running: Option<RunningDetail>,
    pub retry: Option<RetryDetail>,
    pub recent_events: Vec<RecentEvent>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceDetail {
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptDetail {
    pub restart_count: u32,
    pub current_retry_attempt: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunningDetail {
    pub session_id: Option<String>,
    pub turn_count: u32,
    pub state: String,
    pub started_at: DateTime<Utc>,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub tokens: TokenSnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct RetryDetail {
    pub attempt: u32,
    pub due_at: DateTime<Utc>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecentEvent {
    pub at: DateTime<Utc>,
    pub event: String,
    pub message: Option<String>,
}

enum Command {
    Tick,
    WorkerUpdate {
        issue_id: String,
        event: Box<CodexEvent>,
    },
    WorkerExit {
        issue_id: String,
        result: Result<(), String>,
    },
    RetryIssue {
        issue_id: String,
    },
    Snapshot {
        reply: oneshot::Sender<Snapshot>,
    },
    IssueDetail {
        identifier: String,
        reply: oneshot::Sender<Option<IssueDetail>>,
    },
    Refresh {
        reply: oneshot::Sender<RefreshResponse>,
    },
    Shutdown,
}

#[derive(Clone, Debug, Serialize)]
pub struct RefreshResponse {
    pub queued: bool,
    pub coalesced: bool,
    pub requested_at: DateTime<Utc>,
    pub operations: Vec<&'static str>,
}

impl Orchestrator {
    pub async fn start(workflow_store: WorkflowStore) -> Result<Self, String> {
        let effective = workflow_store.effective();
        effective
            .config
            .validate_dispatch_ready()
            .map_err(|error| error.to_string())?;

        let tracker =
            build_tracker_client(effective.config.clone()).map_err(|error| error.to_string())?;
        let (tx, mut rx) = mpsc::channel(256);
        let join_tx = tx.clone();
        let join = tokio::spawn(async move {
            let mut state = State {
                poll_interval_ms: effective.config.polling.interval_ms,
                max_concurrent_agents: effective.config.agent.max_concurrent_agents,
                max_retry_backoff_ms: effective.config.agent.max_retry_backoff_ms,
                ..State::default()
            };

            startup_terminal_cleanup(&workflow_store, tracker.as_ref()).await;
            schedule_tick(&join_tx, 0);

            while let Some(command) = rx.recv().await {
                match command {
                    Command::Tick => {
                        run_tick(&workflow_store, &join_tx, &mut state).await;
                    }
                    Command::WorkerUpdate { issue_id, event } => {
                        integrate_worker_update(&mut state, &issue_id, *event);
                    }
                    Command::WorkerExit { issue_id, result } => {
                        handle_worker_exit(&mut state, &join_tx, &issue_id, result);
                    }
                    Command::RetryIssue { issue_id } => {
                        handle_retry_issue(&workflow_store, &join_tx, &mut state, &issue_id).await;
                    }
                    Command::Snapshot { reply } => {
                        let _ = reply.send(build_snapshot(&state));
                    }
                    Command::IssueDetail { identifier, reply } => {
                        let _ =
                            reply.send(build_issue_detail(&workflow_store, &state, &identifier));
                    }
                    Command::Refresh { reply } => {
                        let now = Instant::now();
                        let already_due = state.next_poll_due_at.is_some_and(|due| due <= now);
                        let coalesced = state.poll_check_in_progress || already_due;
                        if !coalesced {
                            schedule_tick(&join_tx, 0);
                        }
                        let _ = reply.send(RefreshResponse {
                            queued: true,
                            coalesced,
                            requested_at: Utc::now(),
                            operations: vec!["poll", "reconcile"],
                        });
                    }
                    Command::Shutdown => {
                        shutdown_running(state.running.into_values().collect()).await;
                        shutdown_retries(state.retry_attempts.into_values().collect());
                        break;
                    }
                }
            }
        });

        Ok(Self { tx, join })
    }

    pub fn handle(&self) -> OrchestratorHandle {
        OrchestratorHandle {
            tx: self.tx.clone(),
        }
    }

    pub async fn shutdown(self) {
        let _ = self.tx.send(Command::Shutdown).await;
        let _ = self.join.await;
    }
}

impl OrchestratorHandle {
    pub async fn snapshot(&self) -> Option<Snapshot> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Snapshot { reply: reply_tx })
            .await
            .ok()?;
        reply_rx.await.ok()
    }

    pub async fn issue_detail(&self, identifier: String) -> Option<IssueDetail> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::IssueDetail {
                identifier,
                reply: reply_tx,
            })
            .await
            .ok()?;
        reply_rx.await.ok().flatten()
    }

    pub async fn refresh(&self) -> Option<RefreshResponse> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Refresh { reply: reply_tx })
            .await
            .ok()?;
        reply_rx.await.ok()
    }
}

async fn run_tick(workflow_store: &WorkflowStore, tx: &mpsc::Sender<Command>, state: &mut State) {
    workflow_store.refresh_if_changed();
    refresh_runtime_config(workflow_store, state);
    state.poll_check_in_progress = true;
    state.next_poll_due_at = None;

    reconcile_running_issues(workflow_store, tx, state).await;

    match workflow_store.validation_error() {
        Some(error_message) => {
            error!("dispatch=status=blocked reason={error_message}");
        }
        None => {
            let effective = workflow_store.effective();
            match effective.config.validate_dispatch_ready() {
                Ok(()) => {
                    let tracker = match build_tracker_client(effective.config.clone()) {
                        Ok(tracker) => tracker,
                        Err(error) => {
                            error!("dispatch=status=blocked reason={error}");
                            schedule_next_tick(tx, state);
                            state.poll_check_in_progress = false;
                            return;
                        }
                    };
                    match tracker.fetch_candidate_issues().await {
                        Ok(issues) => {
                            let active_states = effective.config.active_state_set();
                            let terminal_states = effective.config.terminal_state_set();
                            let mut sorted = issues;
                            sorted.sort_by(sort_issues_for_dispatch);
                            for issue in sorted {
                                if available_slots(state) == 0 {
                                    break;
                                }
                                if should_dispatch_issue(
                                    state,
                                    &effective.config,
                                    &issue,
                                    &active_states,
                                    &terminal_states,
                                ) {
                                    dispatch_issue(workflow_store, tx, state, issue, None).await;
                                }
                            }
                        }
                        Err(error) => {
                            error!("dispatch=status=skipped reason={error}");
                        }
                    }
                }
                Err(error) => {
                    error!("dispatch=status=blocked reason={error}");
                }
            }
        }
    }

    state.poll_check_in_progress = false;
    schedule_next_tick(tx, state);
}

fn schedule_next_tick(tx: &mpsc::Sender<Command>, state: &mut State) {
    let delay = Duration::from_millis(state.poll_interval_ms);
    state.next_poll_due_at = Some(Instant::now() + delay);
    schedule_tick(tx, state.poll_interval_ms);
}

fn schedule_tick(tx: &mpsc::Sender<Command>, delay_ms: u64) {
    let tx = tx.clone();
    tokio::spawn(async move {
        sleep(Duration::from_millis(delay_ms)).await;
        let _ = tx.send(Command::Tick).await;
    });
}

fn refresh_runtime_config(workflow_store: &WorkflowStore, state: &mut State) {
    let effective = workflow_store.effective();
    state.poll_interval_ms = effective.config.polling.interval_ms;
    state.max_concurrent_agents = effective.config.agent.max_concurrent_agents;
    state.max_retry_backoff_ms = effective.config.agent.max_retry_backoff_ms;
}

async fn startup_terminal_cleanup(workflow_store: &WorkflowStore, tracker: &dyn TrackerClient) {
    let effective = workflow_store.effective();
    match tracker
        .fetch_issues_by_states(&effective.config.tracker.terminal_states)
        .await
    {
        Ok(issues) => {
            for issue in issues {
                if let Err(error) =
                    Workspace::remove_for_identifier(&effective.config, &issue.identifier).await
                {
                    warn!(
                        "workspace_cleanup=startup status=failed issue_id={} issue_identifier={} error={error}",
                        issue.id, issue.identifier
                    );
                }
            }
        }
        Err(error) => {
            warn!("workspace_cleanup=startup status=skipped reason={error}");
        }
    }
}

async fn reconcile_running_issues(
    workflow_store: &WorkflowStore,
    tx: &mpsc::Sender<Command>,
    state: &mut State,
) {
    reconcile_stalled_runs(workflow_store, tx, state).await;
    if state.running.is_empty() {
        return;
    }

    let effective = workflow_store.effective();
    let tracker = match build_tracker_client(effective.config.clone()) {
        Ok(tracker) => tracker,
        Err(error) => {
            warn!("reconcile=status=skipped reason={error}");
            return;
        }
    };
    let running_ids: Vec<String> = state.running.keys().cloned().collect();
    match tracker.fetch_issue_states_by_ids(&running_ids).await {
        Ok(issues) => {
            let active_states = effective.config.active_state_set();
            let terminal_states = effective.config.terminal_state_set();
            for issue in issues {
                if terminal_states.contains(&issue.state_key()) {
                    info!(
                        "reconcile=status=terminal issue_id={} issue_identifier={} state={}",
                        issue.id, issue.identifier, issue.state
                    );
                    terminate_running_issue(state, issue.id.clone(), true, workflow_store).await;
                } else if !issue.assigned_to_worker {
                    info!(
                        "reconcile=status=rerouted issue_id={} issue_identifier={} assignee={}",
                        issue.id,
                        issue.identifier,
                        issue.assignee_id.as_deref().unwrap_or("none")
                    );
                    terminate_running_issue(state, issue.id.clone(), false, workflow_store).await;
                } else if active_states.contains(&issue.state_key()) {
                    if let Some(running) = state.running.get_mut(&issue.id) {
                        running.issue = issue;
                    }
                } else {
                    info!(
                        "reconcile=status=non_active issue_id={} issue_identifier={} state={}",
                        issue.id, issue.identifier, issue.state
                    );
                    terminate_running_issue(state, issue.id.clone(), false, workflow_store).await;
                }
            }
        }
        Err(error) => {
            debug!("reconcile=status=deferred reason={error}");
        }
    }
}

async fn reconcile_stalled_runs(
    workflow_store: &WorkflowStore,
    tx: &mpsc::Sender<Command>,
    state: &mut State,
) {
    let stall_timeout_ms = workflow_store.effective().config.codex.stall_timeout_ms;
    if stall_timeout_ms <= 0 {
        return;
    }
    let now = Utc::now();
    let stalled: Vec<String> = state
        .running
        .iter()
        .filter_map(|(issue_id, running)| {
            let last_activity = running.last_codex_timestamp.unwrap_or(running.started_at);
            let elapsed = now.signed_duration_since(last_activity).num_milliseconds();
            if elapsed > stall_timeout_ms {
                Some(issue_id.clone())
            } else {
                None
            }
        })
        .collect();

    for issue_id in stalled {
        if let Some(running) = state.running.get(&issue_id) {
            let elapsed_ms = now
                .signed_duration_since(running.last_codex_timestamp.unwrap_or(running.started_at))
                .num_milliseconds();
            warn!(
                "reconcile=status=stalled issue_id={} issue_identifier={} elapsed_ms={} session_id={}",
                issue_id,
                running.identifier,
                elapsed_ms,
                running
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "n/a".to_string())
            );
            let next_attempt = if running.retry_attempt > 0 {
                running.retry_attempt + 1
            } else {
                1
            };
            let identifier = running.identifier.clone();
            terminate_running_issue(state, issue_id.clone(), false, workflow_store).await;
            schedule_retry(
                tx,
                state,
                issue_id.clone(),
                next_attempt,
                identifier,
                Some(format!("stalled for {elapsed_ms}ms without codex activity")),
                false,
            );
        }
    }
}

async fn terminate_running_issue(
    state: &mut State,
    issue_id: String,
    cleanup_workspace: bool,
    workflow_store: &WorkflowStore,
) {
    if let Some(running) = state.running.remove(&issue_id) {
        state.claimed.remove(&issue_id);
        state.retry_attempts.remove(&issue_id);
        add_runtime_seconds(state, &running);
        let config = workflow_store.effective().config.clone();
        let workspace_path = running.workspace_path.clone();
        let identifier = running.identifier.clone();
        running.cancel.cancel();
        tokio::spawn(async move {
            let _ = running.join.await;
            if cleanup_workspace {
                let _ = Workspace::remove_path(&config, &workspace_path, &identifier).await;
            }
        });
    } else {
        state.claimed.remove(&issue_id);
    }
}

fn should_dispatch_issue(
    state: &State,
    config: &ServiceConfig,
    issue: &Issue,
    active_states: &std::collections::BTreeSet<String>,
    terminal_states: &std::collections::BTreeSet<String>,
) -> bool {
    candidate_issue(issue, active_states, terminal_states)
        && !todo_issue_blocked_by_non_terminal(issue, terminal_states)
        && issue.assigned_to_worker
        && !state.claimed.contains(&issue.id)
        && !state.running.contains_key(&issue.id)
        && available_slots(state) > 0
        && state_slots_available(state, config, issue)
}

fn retry_candidate_issue(
    issue: &Issue,
    active_states: &std::collections::BTreeSet<String>,
    terminal_states: &std::collections::BTreeSet<String>,
) -> bool {
    candidate_issue(issue, active_states, terminal_states)
        && !todo_issue_blocked_by_non_terminal(issue, terminal_states)
        && issue.assigned_to_worker
}

fn candidate_issue(
    issue: &Issue,
    active_states: &std::collections::BTreeSet<String>,
    terminal_states: &std::collections::BTreeSet<String>,
) -> bool {
    !issue.id.is_empty()
        && !issue.identifier.is_empty()
        && !issue.title.is_empty()
        && !issue.state.is_empty()
        && active_states.contains(&issue.state_key())
        && !terminal_states.contains(&issue.state_key())
}

fn todo_issue_blocked_by_non_terminal(
    issue: &Issue,
    terminal_states: &std::collections::BTreeSet<String>,
) -> bool {
    issue.state_key() == "todo"
        && issue.blocked_by.iter().any(|blocker| {
            blocker
                .state
                .as_deref()
                .map(normalize_state_name)
                .map(|state| !terminal_states.contains(&state))
                .unwrap_or(true)
        })
}

fn state_slots_available(state: &State, config: &ServiceConfig, issue: &Issue) -> bool {
    let limit = config.max_concurrent_agents_for_state(&issue.state);
    let used = state
        .running
        .values()
        .filter(|running| {
            normalize_state_name(&running.issue.state) == normalize_state_name(&issue.state)
        })
        .count();
    used < limit
}

fn sort_issues_for_dispatch(left: &Issue, right: &Issue) -> std::cmp::Ordering {
    let left_created = left
        .created_at
        .map(|value| value.timestamp_micros())
        .unwrap_or(i64::MAX);
    let right_created = right
        .created_at
        .map(|value| value.timestamp_micros())
        .unwrap_or(i64::MAX);
    left.priority
        .unwrap_or(5)
        .cmp(&right.priority.unwrap_or(5))
        .then_with(|| left_created.cmp(&right_created))
        .then_with(|| left.identifier.cmp(&right.identifier))
}

fn available_slots(state: &State) -> usize {
    state
        .max_concurrent_agents
        .saturating_sub(state.running.len())
}

async fn dispatch_issue(
    workflow_store: &WorkflowStore,
    tx: &mpsc::Sender<Command>,
    state: &mut State,
    issue: Issue,
    attempt: Option<u32>,
) {
    let effective = workflow_store.effective();
    let tracker = match build_tracker_client(effective.config.clone()) {
        Ok(tracker) => tracker,
        Err(error) => {
            error!(
                "dispatch=status=skipped issue_id={} issue_identifier={} reason={error}",
                issue.id, issue.identifier
            );
            return;
        }
    };

    match tracker
        .fetch_issue_states_by_ids(std::slice::from_ref(&issue.id))
        .await
    {
        Ok(refreshed) => {
            let Some(issue) = refreshed.into_iter().next() else {
                info!(
                    "dispatch=status=skipped issue_id={} issue_identifier={} reason=missing_after_revalidate",
                    issue.id, issue.identifier
                );
                return;
            };
            let active_states = effective.config.active_state_set();
            let terminal_states = effective.config.terminal_state_set();
            if !retry_candidate_issue(&issue, &active_states, &terminal_states) {
                info!(
                    "dispatch=status=skipped issue_id={} issue_identifier={} state={} reason=stale_candidate",
                    issue.id, issue.identifier, issue.state
                );
                return;
            }

            let workspace_path =
                workspace_path_for_identifier(&effective.config, &issue.identifier);
            let cancel = CancellationToken::new();
            let issue_id = issue.id.clone();
            let issue_identifier = issue.identifier.clone();
            let join_tx = tx.clone();
            let workflow_store = workflow_store.clone();
            let cancel_clone = cancel.clone();
            let retry_attempt = attempt.unwrap_or(0);
            let worker_issue = issue.clone();
            let worker_issue_id = issue_id.clone();
            let join = tokio::spawn(async move {
                let result = run_worker(
                    workflow_store,
                    worker_issue,
                    attempt,
                    cancel_clone,
                    join_tx.clone(),
                )
                .await;
                let _ = join_tx
                    .send(Command::WorkerExit {
                        issue_id: worker_issue_id,
                        result,
                    })
                    .await;
            });

            state.running.insert(
                issue_id.clone(),
                RunningEntry {
                    issue,
                    identifier: issue_identifier.clone(),
                    workspace_path,
                    cancel,
                    join,
                    session_id: None,
                    last_codex_message: None,
                    last_codex_event: None,
                    last_codex_timestamp: None,
                    codex_app_server_pid: None,
                    codex_input_tokens: 0,
                    codex_output_tokens: 0,
                    codex_total_tokens: 0,
                    last_reported_input_tokens: 0,
                    last_reported_output_tokens: 0,
                    last_reported_total_tokens: 0,
                    turn_count: 0,
                    retry_attempt,
                    started_at: Utc::now(),
                    recent_events: Vec::new(),
                },
            );
            state.claimed.insert(issue_id.clone());
            state.retry_attempts.remove(&issue_id);
            info!(
                "dispatch=status=started issue_id={} issue_identifier={} attempt={}",
                issue_id, issue_identifier, retry_attempt
            );
        }
        Err(error) => {
            warn!(
                "dispatch=status=skipped issue_id={} issue_identifier={} reason={error}",
                issue.id, issue.identifier
            );
        }
    }
}

fn handle_worker_exit(
    state: &mut State,
    tx: &mpsc::Sender<Command>,
    issue_id: &str,
    result: Result<(), String>,
) {
    let Some(running) = state.running.remove(issue_id) else {
        return;
    };
    add_runtime_seconds(state, &running);
    let session_id = running
        .session_id
        .clone()
        .unwrap_or_else(|| "n/a".to_string());
    match result {
        Ok(()) => {
            state.completed.insert(issue_id.to_string());
            schedule_retry(
                tx,
                state,
                issue_id.to_string(),
                1,
                running.identifier.clone(),
                None,
                true,
            );
            info!(
                "worker=status=completed issue_id={} issue_identifier={} session_id={}",
                issue_id, running.identifier, session_id
            );
        }
        Err(error) => {
            let attempt = if running.retry_attempt > 0 {
                running.retry_attempt + 1
            } else {
                1
            };
            schedule_retry(
                tx,
                state,
                issue_id.to_string(),
                attempt,
                running.identifier.clone(),
                Some(error.clone()),
                false,
            );
            warn!(
                "worker=status=failed issue_id={} issue_identifier={} session_id={} error={}",
                issue_id, running.identifier, session_id, error
            );
        }
    }
}

async fn handle_retry_issue(
    workflow_store: &WorkflowStore,
    tx: &mpsc::Sender<Command>,
    state: &mut State,
    issue_id: &str,
) {
    let Some(retry_entry) = state.retry_attempts.remove(issue_id) else {
        return;
    };
    let effective = workflow_store.effective();
    let tracker = match build_tracker_client(effective.config.clone()) {
        Ok(tracker) => tracker,
        Err(error) => {
            warn!("retry=status=failed issue_id={} reason={error}", issue_id);
            return;
        }
    };
    handle_retry_issue_with_tracker(
        workflow_store,
        tx,
        state,
        issue_id,
        retry_entry,
        tracker.as_ref(),
    )
    .await;
}

async fn handle_retry_issue_with_tracker(
    workflow_store: &WorkflowStore,
    tx: &mpsc::Sender<Command>,
    state: &mut State,
    issue_id: &str,
    retry_entry: RetryEntry,
    tracker: &dyn TrackerClient,
) {
    let effective = workflow_store.effective();
    let active_states = effective.config.active_state_set();
    let terminal_states = effective.config.terminal_state_set();
    match tracker
        .fetch_issue_states_by_ids(&[issue_id.to_string()])
        .await
    {
        Ok(issues) => {
            if let Some(issue) = issues.into_iter().find(|issue| issue.id == issue_id) {
                if terminal_states.contains(&issue.state_key()) {
                    cleanup_issue_workspace(&effective.config, &issue).await;
                    state.claimed.remove(issue_id);
                } else if retry_candidate_issue(&issue, &active_states, &terminal_states) {
                    if available_slots(state) > 0
                        && state_slots_available(state, &effective.config, &issue)
                    {
                        dispatch_issue(workflow_store, tx, state, issue, Some(retry_entry.attempt))
                            .await;
                    } else {
                        schedule_retry(
                            tx,
                            state,
                            issue.id.clone(),
                            retry_entry.attempt + 1,
                            issue.identifier.clone(),
                            Some("no available orchestrator slots".to_string()),
                            false,
                        );
                    }
                } else {
                    state.claimed.remove(issue_id);
                }
            } else {
                state.claimed.remove(issue_id);
            }
        }
        Err(error) => {
            schedule_retry(
                tx,
                state,
                issue_id.to_string(),
                retry_entry.attempt + 1,
                retry_entry.identifier,
                Some(format!("retry poll failed: {error}")),
                false,
            );
        }
    }
}

async fn cleanup_issue_workspace(config: &ServiceConfig, issue: &Issue) {
    if let Err(error) = Workspace::remove_for_identifier(config, &issue.identifier).await {
        warn!(
            "workspace_cleanup=retry status=failed issue_id={} issue_identifier={} error={error}",
            issue.id, issue.identifier
        );
    }
}

fn schedule_retry(
    tx: &mpsc::Sender<Command>,
    state: &mut State,
    issue_id: String,
    attempt: u32,
    identifier: String,
    error: Option<String>,
    continuation: bool,
) {
    if let Some(existing) = state.retry_attempts.remove(&issue_id) {
        existing.cancel.cancel();
    }
    let delay_ms = if continuation && attempt == 1 {
        1_000
    } else {
        let power = (attempt.saturating_sub(1)).min(10);
        10_000u64.saturating_mul(2u64.saturating_pow(power))
    };
    let delay_ms = if continuation && attempt == 1 {
        1_000
    } else {
        delay_ms.min(state.max_retry_backoff_ms)
    };

    let cancel = CancellationToken::new();
    let issue_id_for_task = issue_id.clone();
    let tx_clone = tx.clone();
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel_clone.cancelled() => {}
            _ = sleep(Duration::from_millis(delay_ms)) => {
                let _ = tx_clone.send(Command::RetryIssue { issue_id: issue_id_for_task }).await;
            }
        }
    });

    state.retry_attempts.insert(
        issue_id.clone(),
        RetryEntry {
            attempt,
            due_at: Instant::now() + Duration::from_millis(delay_ms),
            identifier: identifier.clone(),
            error: error.clone(),
            cancel,
        },
    );
    warn!(
        "retry=status=queued issue_id={} issue_identifier={} attempt={} delay_ms={} error={}",
        issue_id,
        identifier,
        attempt,
        delay_ms,
        error.unwrap_or_else(|| "none".to_string())
    );
}

fn integrate_worker_update(state: &mut State, issue_id: &str, event: CodexEvent) {
    let Some(running) = state.running.get_mut(issue_id) else {
        return;
    };

    if event.event == "session_started" {
        let next_session_id = event.session_id.clone();
        if next_session_id != running.session_id {
            running.turn_count += 1;
        }
        running.session_id = next_session_id;
    }

    if let Some(pid) = event.codex_app_server_pid {
        running.codex_app_server_pid = Some(pid);
    }
    running.last_codex_event = Some(event.event.clone());
    running.last_codex_timestamp = Some(event.timestamp);
    running.last_codex_message = event
        .message
        .clone()
        .or_else(|| event.raw.clone())
        .or_else(|| event.payload.as_ref().map(|payload| payload.to_string()));

    if let Some(usage) = event.usage.as_ref() {
        let input = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total = usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let input_delta = input.saturating_sub(running.last_reported_input_tokens);
        let output_delta = output.saturating_sub(running.last_reported_output_tokens);
        let total_delta = total.saturating_sub(running.last_reported_total_tokens);

        running.last_reported_input_tokens = input.max(running.last_reported_input_tokens);
        running.last_reported_output_tokens = output.max(running.last_reported_output_tokens);
        running.last_reported_total_tokens = total.max(running.last_reported_total_tokens);

        running.codex_input_tokens += input_delta;
        running.codex_output_tokens += output_delta;
        running.codex_total_tokens += total_delta;
        state.codex_totals.input_tokens += input_delta;
        state.codex_totals.output_tokens += output_delta;
        state.codex_totals.total_tokens += total_delta;
    }

    if let Some(rate_limits) = event.rate_limits.clone() {
        state.codex_rate_limits = Some(rate_limits);
    }

    running.recent_events.push(RecentEvent {
        at: event.timestamp,
        event: event.event,
        message: running.last_codex_message.clone(),
    });
    if running.recent_events.len() > 20 {
        let drain = running.recent_events.len() - 20;
        running.recent_events.drain(0..drain);
    }
}

fn add_runtime_seconds(state: &mut State, running: &RunningEntry) {
    let elapsed = Utc::now()
        .signed_duration_since(running.started_at)
        .num_milliseconds()
        .max(0) as f64
        / 1_000.0;
    state.codex_totals.ended_seconds += elapsed;
}

fn build_snapshot(state: &State) -> Snapshot {
    let now = Utc::now();
    let now_instant = Instant::now();
    let live_seconds: f64 = state
        .running
        .values()
        .map(|running| {
            now.signed_duration_since(running.started_at)
                .num_milliseconds()
                .max(0) as f64
                / 1_000.0
        })
        .sum();

    Snapshot {
        generated_at: now,
        counts: SnapshotCounts {
            running: state.running.len(),
            retrying: state.retry_attempts.len(),
        },
        running: state
            .running
            .iter()
            .map(|(issue_id, running)| RunningSnapshot {
                issue_id: issue_id.clone(),
                issue_identifier: running.identifier.clone(),
                state: running.issue.state.clone(),
                session_id: running.session_id.clone(),
                turn_count: running.turn_count,
                last_event: running.last_codex_event.clone(),
                last_message: running.last_codex_message.clone(),
                started_at: running.started_at,
                last_event_at: running.last_codex_timestamp,
                workspace: running.workspace_path.display().to_string(),
                tokens: TokenSnapshot {
                    input_tokens: running.codex_input_tokens,
                    output_tokens: running.codex_output_tokens,
                    total_tokens: running.codex_total_tokens,
                },
            })
            .collect(),
        retrying: state
            .retry_attempts
            .iter()
            .map(|(issue_id, retry)| RetrySnapshot {
                issue_id: issue_id.clone(),
                issue_identifier: retry.identifier.clone(),
                attempt: retry.attempt,
                due_at: now
                    + chrono::Duration::milliseconds(
                        retry
                            .due_at
                            .saturating_duration_since(now_instant)
                            .as_millis() as i64,
                    ),
                error: retry.error.clone(),
            })
            .collect(),
        codex_totals: SnapshotTotals {
            input_tokens: state.codex_totals.input_tokens,
            output_tokens: state.codex_totals.output_tokens,
            total_tokens: state.codex_totals.total_tokens,
            seconds_running: state.codex_totals.ended_seconds + live_seconds,
        },
        rate_limits: state.codex_rate_limits.clone(),
        polling: PollingSnapshot {
            checking: state.poll_check_in_progress,
            next_poll_in_ms: state
                .next_poll_due_at
                .map(|due| due.saturating_duration_since(now_instant).as_millis() as u64),
            poll_interval_ms: state.poll_interval_ms,
        },
    }
}

fn build_issue_detail(
    workflow_store: &WorkflowStore,
    state: &State,
    identifier: &str,
) -> Option<IssueDetail> {
    let running_entry = state
        .running
        .iter()
        .find(|(_, running)| running.identifier == identifier);
    let retry_entry = state
        .retry_attempts
        .iter()
        .find(|(_, retry)| retry.identifier == identifier);

    let running = running_entry.map(|(_, running)| running);
    let retry = retry_entry.map(|(_, retry)| retry);

    if running.is_none() && retry.is_none() {
        return None;
    }

    let workspace = running
        .map(|running| running.workspace_path.clone())
        .unwrap_or_else(|| {
            let effective = workflow_store.effective();
            workspace_path_for_identifier(&effective.config, identifier)
        });

    let issue_id = running
        .map(|running| running.issue.id.clone())
        .or_else(|| retry_entry.map(|(issue_id, _)| issue_id.clone()));
    let status = if running.is_some() {
        "running"
    } else {
        "retrying"
    }
    .to_string();

    Some(IssueDetail {
        issue_identifier: identifier.to_string(),
        issue_id,
        status,
        workspace: WorkspaceDetail {
            path: workspace.display().to_string(),
        },
        attempts: AttemptDetail {
            restart_count: running
                .map(|running| running.turn_count.saturating_sub(1))
                .unwrap_or(0),
            current_retry_attempt: retry.map(|retry| retry.attempt),
        },
        running: running.map(|running| RunningDetail {
            session_id: running.session_id.clone(),
            turn_count: running.turn_count,
            state: running.issue.state.clone(),
            started_at: running.started_at,
            last_event: running.last_codex_event.clone(),
            last_message: running.last_codex_message.clone(),
            last_event_at: running.last_codex_timestamp,
            tokens: TokenSnapshot {
                input_tokens: running.codex_input_tokens,
                output_tokens: running.codex_output_tokens,
                total_tokens: running.codex_total_tokens,
            },
        }),
        retry: retry.map(|retry| RetryDetail {
            attempt: retry.attempt,
            due_at: Utc::now()
                + chrono::Duration::milliseconds(
                    retry
                        .due_at
                        .saturating_duration_since(Instant::now())
                        .as_millis() as i64,
                ),
            error: retry.error.clone(),
        }),
        recent_events: running
            .map(|running| running.recent_events.clone())
            .unwrap_or_default(),
        last_error: retry.and_then(|retry| retry.error.clone()),
    })
}

async fn shutdown_running(entries: Vec<RunningEntry>) {
    for running in entries {
        running.cancel.cancel();
        let _ = running.join.await;
    }
}

fn shutdown_retries(entries: Vec<RetryEntry>) {
    for retry in entries {
        retry.cancel.cancel();
    }
}

async fn run_worker(
    workflow_store: WorkflowStore,
    issue: Issue,
    attempt: Option<u32>,
    cancellation: CancellationToken,
    tx: mpsc::Sender<Command>,
) -> Result<(), String> {
    let effective = workflow_store.effective();
    let workspace = Workspace::create_for_issue(&effective.config, &issue)
        .await
        .map_err(|error| error.to_string())?;
    if let Err(error) = workspace.run_before_run(&effective.config, &issue).await {
        workspace.run_after_run(&effective.config, &issue).await;
        return Err(error.to_string());
    }

    let tracker =
        build_tracker_client(effective.config.clone()).map_err(|error| error.to_string())?;
    let app = AppServerClient::new(effective.config.clone(), tracker.clone());
    let mut session = match app.start_session(&workspace.path).await {
        Ok(session) => session,
        Err(error) => {
            workspace.run_after_run(&effective.config, &issue).await;
            return Err(error.to_string());
        }
    };

    let worker_result = async {
        let mut current_issue = issue.clone();
        let max_turns = workflow_store.effective().config.agent.max_turns;
        for turn_number in 1..=max_turns {
            if cancellation.is_cancelled() {
                return Err("cancelled by orchestrator".to_string());
            }
            let workflow = workflow_store.effective();
            let prompt = if turn_number == 1 {
                prompt::build_issue_prompt(&workflow.definition, &current_issue, attempt)
                    .map_err(|error| error.to_string())?
            } else {
                prompt::continuation_guidance(turn_number, max_turns)
            };

            session
                .run_turn(&prompt, &current_issue, &cancellation, |event| {
                    let issue_id = current_issue.id.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        let _ = tx
                            .send(Command::WorkerUpdate {
                                issue_id,
                                event: Box::new(event),
                            })
                            .await;
                    });
                })
                .await
                .map_err(|error| error.to_string())?;

            let tracker = build_tracker_client(workflow_store.effective().config.clone())
                .map_err(|error| error.to_string())?;
            let refreshed = tracker
                .fetch_issue_states_by_ids(std::slice::from_ref(&current_issue.id))
                .await
                .map_err(|error| error.to_string())?;
            if let Some(next_issue) = refreshed.into_iter().next() {
                current_issue = next_issue;
            }

            let active = workflow_store
                .effective()
                .config
                .active_state_set()
                .contains(&current_issue.state_key());
            if !active {
                break;
            }
            if turn_number >= max_turns {
                break;
            }
        }
        Ok(())
    }
    .await;

    session.stop().await;
    workspace
        .run_after_run(&workflow_store.effective().config, &issue)
        .await;
    worker_result
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, Utc};
    use std::{fs, path::PathBuf, sync::Arc, time::Instant};
    use tempfile::tempdir;
    use tokio::{sync::oneshot, task::JoinHandle};
    use tokio_util::sync::CancellationToken;

    use crate::issue::{BlockerRef, Issue};

    use super::{
        RetryEntry, RunningEntry, State, build_snapshot, candidate_issue,
        handle_retry_issue_with_tracker, sort_issues_for_dispatch,
        todo_issue_blocked_by_non_terminal,
    };
    use crate::{orchestrator::build_issue_detail, workflow::WorkflowStore};

    #[test]
    fn sorts_issues_by_priority_then_created_at() {
        let now = Utc::now();
        let mut issues = [
            Issue {
                id: "2".to_string(),
                identifier: "B".to_string(),
                title: "b".to_string(),
                state: "Todo".to_string(),
                priority: Some(2),
                created_at: Some(now),
                ..Issue::default()
            },
            Issue {
                id: "1".to_string(),
                identifier: "A".to_string(),
                title: "a".to_string(),
                state: "Todo".to_string(),
                priority: Some(1),
                created_at: Some(now + ChronoDuration::seconds(1)),
                ..Issue::default()
            },
        ];

        issues.sort_by(sort_issues_for_dispatch);
        assert_eq!(issues[0].id, "1");
    }

    #[test]
    fn todo_blocked_issue_is_not_candidate() {
        let issue = Issue {
            id: "1".to_string(),
            identifier: "A".to_string(),
            title: "a".to_string(),
            state: "Todo".to_string(),
            blocked_by: vec![BlockerRef {
                id: Some("2".to_string()),
                identifier: Some("B".to_string()),
                state: Some("In Progress".to_string()),
            }],
            ..Issue::default()
        };
        let active: std::collections::BTreeSet<String> = ["todo".to_string()].into_iter().collect();
        let terminal: std::collections::BTreeSet<String> =
            ["done".to_string()].into_iter().collect();

        assert!(candidate_issue(&issue, &active, &terminal));
        assert!(todo_issue_blocked_by_non_terminal(&issue, &terminal));
    }

    #[test]
    fn snapshot_reports_counts() {
        let snapshot = build_snapshot(&State::default());
        assert_eq!(snapshot.counts.running, 0);
        assert_eq!(snapshot.counts.retrying, 0);
    }

    #[tokio::test]
    async fn active_rerouted_issue_stops_running_agent() {
        let workflow_store = sample_workflow_store();
        let mut state = State::default();
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "In Progress".to_string(),
            assigned_to_worker: true,
            ..Issue::default()
        };

        let (join, _release) = parked_join_handle();
        state.running.insert(
            issue.id.clone(),
            RunningEntry {
                issue: issue.clone(),
                identifier: issue.identifier.clone(),
                workspace_path: PathBuf::from("/tmp/workspace"),
                cancel: CancellationToken::new(),
                join,
                session_id: None,
                last_codex_message: None,
                last_codex_event: None,
                last_codex_timestamp: None,
                codex_app_server_pid: None,
                codex_input_tokens: 0,
                codex_output_tokens: 0,
                codex_total_tokens: 0,
                last_reported_input_tokens: 0,
                last_reported_output_tokens: 0,
                last_reported_total_tokens: 0,
                turn_count: 0,
                retry_attempt: 0,
                started_at: Utc::now(),
                recent_events: Vec::new(),
            },
        );
        state.claimed.insert(issue.id.clone());

        let rerouted = Issue {
            assigned_to_worker: false,
            assignee_id: Some("user-2".to_string()),
            ..issue
        };

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let tracker = Arc::new(StaticTracker {
            by_id: vec![rerouted],
            candidates: Vec::new(),
        });
        reconcile_running_issues_with_tracker(&workflow_store, &tx, &mut state, tracker.as_ref())
            .await;

        assert!(state.running.is_empty());
        assert!(!state.claimed.contains("1"));
    }

    #[tokio::test]
    async fn terminal_retry_issue_cleans_up_workspace() {
        let dir = tempdir().expect("tempdir");
        let workflow_store = sample_workflow_store_with_root(dir.path());
        let effective = workflow_store.effective();
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "Done".to_string(),
            assigned_to_worker: true,
            ..Issue::default()
        };
        let workspace_path = effective.config.workspace.root.join("ABC-1");
        fs::create_dir_all(&workspace_path).expect("workspace");
        fs::write(workspace_path.join("marker.txt"), "present").expect("marker");

        let mut state = State::default();
        state.claimed.insert(issue.id.clone());
        state.retry_attempts.insert(
            issue.id.clone(),
            RetryEntry {
                attempt: 1,
                due_at: Instant::now(),
                identifier: issue.identifier.clone(),
                error: None,
                cancel: CancellationToken::new(),
            },
        );

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let tracker = Arc::new(StaticTracker {
            by_id: vec![issue.clone()],
            candidates: Vec::new(),
        });
        let retry_entry = state.retry_attempts.remove(&issue.id).expect("retry entry");

        handle_retry_issue_with_tracker(
            &workflow_store,
            &tx,
            &mut state,
            &issue.id,
            retry_entry,
            tracker.as_ref(),
        )
        .await;

        assert!(!state.claimed.contains(&issue.id));
        assert!(!workspace_path.exists());
    }

    #[test]
    fn issue_detail_includes_retry_issue_id() {
        let workflow_store = sample_workflow_store();
        let mut state = State::default();
        state.retry_attempts.insert(
            "issue-1".to_string(),
            super::RetryEntry {
                attempt: 2,
                due_at: Instant::now(),
                identifier: "ABC-1".to_string(),
                error: Some("boom".to_string()),
                cancel: CancellationToken::new(),
            },
        );

        let detail = build_issue_detail(&workflow_store, &state, "ABC-1").expect("detail");
        assert_eq!(detail.issue_id.as_deref(), Some("issue-1"));
    }

    struct StaticTracker {
        candidates: Vec<Issue>,
        by_id: Vec<Issue>,
    }

    #[async_trait::async_trait]
    impl crate::tracker::TrackerClient for StaticTracker {
        async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, crate::tracker::TrackerError> {
            Ok(self.candidates.clone())
        }

        async fn fetch_issues_by_states(
            &self,
            _states: &[String],
        ) -> Result<Vec<Issue>, crate::tracker::TrackerError> {
            Ok(Vec::new())
        }

        async fn fetch_issue_states_by_ids(
            &self,
            _issue_ids: &[String],
        ) -> Result<Vec<Issue>, crate::tracker::TrackerError> {
            Ok(self.by_id.clone())
        }

        async fn raw_graphql(
            &self,
            _query: &str,
            _variables: serde_json::Value,
        ) -> Result<serde_json::Value, crate::tracker::TrackerError> {
            unreachable!()
        }
    }

    async fn reconcile_running_issues_with_tracker(
        workflow_store: &WorkflowStore,
        tx: &tokio::sync::mpsc::Sender<super::Command>,
        state: &mut State,
        tracker: &dyn crate::tracker::TrackerClient,
    ) {
        super::reconcile_stalled_runs(workflow_store, tx, state).await;
        let effective = workflow_store.effective();
        let active_states = effective.config.active_state_set();
        let terminal_states = effective.config.terminal_state_set();
        let running_ids: Vec<String> = state.running.keys().cloned().collect();
        let issues = tracker
            .fetch_issue_states_by_ids(&running_ids)
            .await
            .expect("issues");

        for issue in issues {
            if terminal_states.contains(&issue.state_key()) {
                super::terminate_running_issue(state, issue.id.clone(), true, workflow_store).await;
            } else if !issue.assigned_to_worker {
                super::terminate_running_issue(state, issue.id.clone(), false, workflow_store)
                    .await;
            } else if active_states.contains(&issue.state_key()) {
                if let Some(running) = state.running.get_mut(&issue.id) {
                    running.issue = issue;
                }
            } else {
                super::terminate_running_issue(state, issue.id.clone(), false, workflow_store)
                    .await;
            }
        }
    }

    fn sample_workflow_store() -> WorkflowStore {
        let root = std::env::temp_dir().join("symphony-tests");
        sample_workflow_store_with_root(&root)
    }

    fn sample_workflow_store_with_root(root: &std::path::Path) -> WorkflowStore {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("WORKFLOW.md");
        fs::write(
            &path,
            format!(
                r#"---
tracker:
  kind: linear
  api_key: token
  project_slug: proj
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
    - Cancelled
workspace:
  root: {}
---

test
"#,
                root.display()
            ),
        )
        .expect("write workflow");
        let persisted = temp_workflow_path(&path);
        WorkflowStore::new(persisted).expect("workflow store")
    }

    fn parked_join_handle() -> (JoinHandle<()>, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel::<()>();
        let join = tokio::spawn(async move {
            let _ = rx.await;
        });
        (join, tx)
    }

    fn temp_workflow_path(source: &std::path::Path) -> PathBuf {
        let unique = format!(
            "symphony-orchestrator-test-{}-{}.md",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let target = std::env::temp_dir().join(unique);
        fs::copy(source, &target).expect("copy workflow");
        target
    }
}
