use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    process::Command as StdCommand,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    codex::{AppServerClient, CodexError, CodexEvent},
    config::ServiceConfig,
    issue::{Issue, normalize_state_name},
    prompt,
    tracker::{TrackerClient, TrackerError, TrackerRateBudget, build_tracker_client},
    workflow::WorkflowStore,
    workspace::{
        Workspace, WorkspaceError, sanitize_identifier, ssh_args, ssh_executable,
        workspace_path_for_identifier_on_host,
    },
};

const RUNNING_SHUTDOWN_TIMEOUT_SECS: u64 = 10;
const ORCHESTRATOR_SHUTDOWN_TIMEOUT_SECS: u64 = 15;
const STARTUP_WORKSPACE_CLEANUP_TIMEOUT_SECS: u64 = 15;
const HANDLE_COMMAND_TIMEOUT_SECS: u64 = 5;

#[derive(Clone)]
pub struct OrchestratorHandle {
    tx: mpsc::Sender<Command>,
    updates: watch::Receiver<u64>,
}

pub struct Orchestrator {
    tx: mpsc::Sender<Command>,
    updates: watch::Receiver<u64>,
    join: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OrchestratorHandleError {
    TimedOut,
    Unavailable,
}

#[derive(Default)]
struct State {
    poll_interval_ms: u64,
    max_concurrent_agents: usize,
    max_retry_backoff_ms: u64,
    worker_runtime: WorkerRuntimeConfig,
    validated_startup_config: Option<ServiceConfig>,
    next_poll_due_at: Option<Instant>,
    poll_check_in_progress: bool,
    running: BTreeMap<String, RunningEntry>,
    completed: BTreeSet<String>,
    claimed: BTreeSet<String>,
    retry_attempts: BTreeMap<String, RetryEntry>,
    codex_totals: CodexTotals,
    codex_rate_limits: Option<Value>,
    todoist_rate_budget: Option<TrackerRateBudget>,
}

struct RunningEntry {
    issue: Issue,
    identifier: String,
    workspace_path: PathBuf,
    workspace_location: String,
    worker_host: Option<String>,
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
    terminal_transition_permitted: bool,
    recent_events: Vec<RecentEvent>,
}

struct RetryEntry {
    attempt: u32,
    due_at: Instant,
    identifier: String,
    tracked_issue: Option<Issue>,
    worker_host: Option<String>,
    workspace_location: Option<String>,
    error: Option<String>,
    error_stage: Option<String>,
    error_kind: Option<String>,
    cancel: CancellationToken,
}

struct RetryRequest {
    issue_id: String,
    attempt: u32,
    identifier: String,
    tracked_issue: Option<Issue>,
    worker_host: Option<String>,
    workspace_location: Option<String>,
    error: Option<String>,
    error_stage: Option<String>,
    error_kind: Option<String>,
    continuation: bool,
}

#[derive(Clone, Debug, Default)]
struct WorkerRuntimeConfig {
    ssh_hosts: Vec<String>,
    max_concurrent_agents_per_host: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkerSelection {
    Local,
    Remote(String),
    NoCapacity,
}

#[derive(Clone, Copy, Default)]
struct CodexTotals {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    ended_seconds: f64,
}

enum WorkerDisposition {
    Continue { issue: Issue },
    Quiesced { issue: Option<Issue> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkerErrorStage {
    Startup,
    WorkspaceCreate,
    BeforeRun,
    TrackerBuild,
    SessionStart,
    TurnRun,
    StateRefresh,
    Cleanup,
}

impl WorkerErrorStage {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::WorkspaceCreate => "workspace_create",
            Self::BeforeRun => "before_run",
            Self::TrackerBuild => "tracker_build",
            Self::SessionStart => "session_start",
            Self::TurnRun => "turn_run",
            Self::StateRefresh => "state_refresh",
            Self::Cleanup => "cleanup",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WorkerErrorKind {
    StartupFailoverExhausted,
    WorkspaceFailure,
    HookFailure,
    TrackerFailure,
    SessionFailure,
    TurnFailure,
    ApprovalRequired,
    InputRequired,
    Cancellation,
}

impl WorkerErrorKind {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::StartupFailoverExhausted => "startup_failover_exhausted",
            Self::WorkspaceFailure => "workspace_failure",
            Self::HookFailure => "hook_failure",
            Self::TrackerFailure => "tracker_failure",
            Self::SessionFailure => "session_failure",
            Self::TurnFailure => "turn_failure",
            Self::ApprovalRequired => "approval_required",
            Self::InputRequired => "input_required",
            Self::Cancellation => "cancellation",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkerError {
    stage: WorkerErrorStage,
    kind: WorkerErrorKind,
    worker_host: Option<String>,
    message: String,
}

impl WorkerError {
    fn new(
        stage: WorkerErrorStage,
        kind: WorkerErrorKind,
        worker_host: Option<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            stage,
            kind,
            worker_host,
            message: message.into(),
        }
    }

    fn startup_failover_eligible(&self) -> bool {
        matches!(
            self.stage,
            WorkerErrorStage::WorkspaceCreate
                | WorkerErrorStage::BeforeRun
                | WorkerErrorStage::SessionStart
        )
    }

    fn stage_name(&self) -> String {
        self.stage.as_str().to_string()
    }

    fn kind_name(&self) -> String {
        self.kind.as_str().to_string()
    }

    fn startup_failover_exhausted(errors: Vec<Self>) -> Self {
        let worker_host = errors.last().and_then(|error| error.worker_host.clone());
        let message = errors
            .iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        Self::new(
            WorkerErrorStage::Startup,
            WorkerErrorKind::StartupFailoverExhausted,
            worker_host,
            if message.is_empty() {
                "all worker startup attempts failed".to_string()
            } else {
                format!("all worker startup attempts failed: {message}")
            },
        )
    }
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "worker_stage={} worker_kind={} worker_host={} {}",
            self.stage.as_str(),
            self.kind.as_str(),
            self.worker_host.as_deref().unwrap_or("local"),
            self.message
        )
    }
}

impl std::error::Error for WorkerError {}

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub generated_at: DateTime<Utc>,
    pub counts: SnapshotCounts,
    pub running: Vec<RunningSnapshot>,
    pub retrying: Vec<RetrySnapshot>,
    pub codex_totals: SnapshotTotals,
    pub rate_limits: Option<Value>,
    pub todoist_rate_budget: Option<TrackerRateBudget>,
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
    pub title: String,
    pub state: String,
    pub url: Option<String>,
    pub project_id: Option<String>,
    pub labels: Vec<String>,
    pub due: Option<Value>,
    pub deadline: Option<Value>,
    pub worker_host: Option<String>,
    pub session_id: Option<String>,
    pub codex_app_server_pid: Option<u32>,
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
    pub worker_host: Option<String>,
    pub workspace_location: Option<String>,
    pub error: Option<String>,
    pub error_stage: Option<String>,
    pub error_kind: Option<String>,
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
    pub tracked_issue: Option<Issue>,
    pub workspace: WorkspaceDetail,
    pub attempts: AttemptDetail,
    pub running: Option<RunningDetail>,
    pub retry: Option<RetryDetail>,
    pub recent_events: Vec<RecentEvent>,
    pub last_error: Option<String>,
    pub last_error_stage: Option<String>,
    pub last_error_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceDetail {
    pub path: String,
    pub worker_host: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AttemptDetail {
    pub restart_count: u32,
    pub current_retry_attempt: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunningDetail {
    pub worker_host: Option<String>,
    pub session_id: Option<String>,
    pub codex_app_server_pid: Option<u32>,
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
    pub worker_host: Option<String>,
    pub workspace_location: Option<String>,
    pub error: Option<String>,
    pub error_stage: Option<String>,
    pub error_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RecentEvent {
    pub at: DateTime<Utc>,
    pub event: String,
    pub message: Option<String>,
}

enum Command {
    Tick,
    WorkerRuntimeInfo {
        issue_id: String,
        worker_host: Option<String>,
        workspace_path: PathBuf,
        workspace_location: String,
    },
    WorkerUpdate {
        issue_id: String,
        event: Box<CodexEvent>,
    },
    WorkerExit {
        issue_id: String,
        result: Box<Result<WorkerDisposition, WorkerError>>,
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
        tracker
            .validate_startup()
            .await
            .map_err(|error| error.to_string())?;
        let initial_todoist_rate_budget = tracker.rate_budget().await;
        let (tx, mut rx) = mpsc::channel(256);
        let (updates_tx, updates_rx) = watch::channel(0u64);
        let join_tx = tx.clone();
        let join = tokio::spawn(async move {
            let mut state = State {
                poll_interval_ms: effective.config.polling.interval_ms,
                max_concurrent_agents: effective.config.agent.max_concurrent_agents,
                max_retry_backoff_ms: effective.config.agent.max_retry_backoff_ms,
                todoist_rate_budget: initial_todoist_rate_budget,
                validated_startup_config: Some(effective.config.clone()),
                ..State::default()
            };
            let mut update_version = 0u64;

            tokio::spawn(run_startup_terminal_cleanup(
                workflow_store.clone(),
                tracker.clone(),
                Duration::from_secs(STARTUP_WORKSPACE_CLEANUP_TIMEOUT_SECS),
            ));
            schedule_tick(&join_tx, 0);

            while let Some(command) = rx.recv().await {
                match command {
                    Command::Tick => {
                        run_tick(
                            &workflow_store,
                            &join_tx,
                            &mut state,
                            &updates_tx,
                            &mut update_version,
                        )
                        .await;
                    }
                    Command::WorkerRuntimeInfo {
                        issue_id,
                        worker_host,
                        workspace_path,
                        workspace_location,
                    } => {
                        integrate_worker_runtime_info(
                            &mut state,
                            &issue_id,
                            worker_host,
                            workspace_path,
                            workspace_location,
                        );
                        notify_observers(&updates_tx, &mut update_version);
                    }
                    Command::WorkerUpdate { issue_id, event } => {
                        integrate_worker_update(&mut state, &issue_id, *event);
                        notify_observers(&updates_tx, &mut update_version);
                    }
                    Command::WorkerExit { issue_id, result } => {
                        handle_worker_exit(&mut state, &join_tx, &issue_id, *result);
                        notify_observers(&updates_tx, &mut update_version);
                    }
                    Command::RetryIssue { issue_id } => {
                        handle_retry_issue(&workflow_store, &join_tx, &mut state, &issue_id).await;
                        notify_observers(&updates_tx, &mut update_version);
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
                        notify_observers(&updates_tx, &mut update_version);
                    }
                    Command::Shutdown => {
                        shutdown_running(state.running.into_values().collect()).await;
                        shutdown_retries(state.retry_attempts.into_values().collect());
                        notify_observers(&updates_tx, &mut update_version);
                        break;
                    }
                }
            }
        });

        Ok(Self {
            tx,
            updates: updates_rx,
            join: Some(join),
        })
    }

    pub fn handle(&self) -> OrchestratorHandle {
        OrchestratorHandle {
            tx: self.tx.clone(),
            updates: self.updates.clone(),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.join.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub async fn wait_for_exit(&mut self) -> Result<(), String> {
        match self.join.take() {
            Some(join) => match join.await {
                Ok(()) => Ok(()),
                Err(error) => Err(error.to_string()),
            },
            None => Ok(()),
        }
    }

    pub async fn shutdown(mut self) {
        let tx = self.tx;
        let mut join = self.join.take();

        match timeout(
            Duration::from_secs(ORCHESTRATOR_SHUTDOWN_TIMEOUT_SECS),
            tx.send(Command::Shutdown),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                debug!("orchestrator_shutdown=status=channel_closed error={error}");
            }
            Err(_) => {
                warn!(
                    "orchestrator_shutdown=status=send_timeout timeout_secs={}",
                    ORCHESTRATOR_SHUTDOWN_TIMEOUT_SECS
                );
            }
        }

        if let Some(join_handle) = join.as_mut() {
            match timeout(
                Duration::from_secs(ORCHESTRATOR_SHUTDOWN_TIMEOUT_SECS),
                join_handle,
            )
            .await
            {
                Ok(_) => {}
                Err(_) => {
                    warn!(
                        "orchestrator_shutdown=status=join_timeout timeout_secs={}",
                        ORCHESTRATOR_SHUTDOWN_TIMEOUT_SECS
                    );
                    if let Some(join_handle) = join.take() {
                        join_handle.abort();
                        let _ = join_handle.await;
                    }
                }
            }
        }
    }
}

impl OrchestratorHandle {
    pub fn subscribe_observability(&self) -> watch::Receiver<u64> {
        self.updates.clone()
    }

    pub async fn snapshot(&self) -> Result<Snapshot, OrchestratorHandleError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        timeout(
            Duration::from_secs(HANDLE_COMMAND_TIMEOUT_SECS),
            self.tx.send(Command::Snapshot { reply: reply_tx }),
        )
        .await
        .map_err(|_| OrchestratorHandleError::TimedOut)?
        .map_err(|_| OrchestratorHandleError::Unavailable)?;
        timeout(Duration::from_secs(HANDLE_COMMAND_TIMEOUT_SECS), reply_rx)
            .await
            .map_err(|_| OrchestratorHandleError::TimedOut)?
            .map_err(|_| OrchestratorHandleError::Unavailable)
    }

    pub async fn issue_detail(
        &self,
        identifier: String,
    ) -> Result<Option<IssueDetail>, OrchestratorHandleError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        timeout(
            Duration::from_secs(HANDLE_COMMAND_TIMEOUT_SECS),
            self.tx.send(Command::IssueDetail {
                identifier,
                reply: reply_tx,
            }),
        )
        .await
        .map_err(|_| OrchestratorHandleError::TimedOut)?
        .map_err(|_| OrchestratorHandleError::Unavailable)?;
        timeout(Duration::from_secs(HANDLE_COMMAND_TIMEOUT_SECS), reply_rx)
            .await
            .map_err(|_| OrchestratorHandleError::TimedOut)?
            .map_err(|_| OrchestratorHandleError::Unavailable)
    }

    pub async fn refresh(&self) -> Result<RefreshResponse, OrchestratorHandleError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        timeout(
            Duration::from_secs(HANDLE_COMMAND_TIMEOUT_SECS),
            self.tx.send(Command::Refresh { reply: reply_tx }),
        )
        .await
        .map_err(|_| OrchestratorHandleError::TimedOut)?
        .map_err(|_| OrchestratorHandleError::Unavailable)?;
        timeout(Duration::from_secs(HANDLE_COMMAND_TIMEOUT_SECS), reply_rx)
            .await
            .map_err(|_| OrchestratorHandleError::TimedOut)?
            .map_err(|_| OrchestratorHandleError::Unavailable)
    }
}

async fn run_tick(
    workflow_store: &WorkflowStore,
    tx: &mpsc::Sender<Command>,
    state: &mut State,
    updates: &watch::Sender<u64>,
    update_version: &mut u64,
) {
    workflow_store.refresh_if_changed();
    refresh_runtime_config(workflow_store, state);
    state.poll_check_in_progress = true;
    state.next_poll_due_at = None;
    notify_observers(updates, update_version);

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
                            notify_observers(updates, update_version);
                            return;
                        }
                    };
                    if startup_validation_needed(state, &effective.config) {
                        let validate_result = tracker.validate_startup().await;
                        capture_tracker_rate_budget(state, tracker.as_ref()).await;
                        if let Err(error) = validate_result {
                            error!("dispatch=status=blocked reason={error}");
                            schedule_next_tick(tx, state);
                            state.poll_check_in_progress = false;
                            notify_observers(updates, update_version);
                            return;
                        }
                        mark_startup_validated(state, &effective.config);
                    }
                    let issues_result = tracker.fetch_candidate_issues().await;
                    capture_tracker_rate_budget(state, tracker.as_ref()).await;
                    match issues_result {
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
                                    dispatch_issue(workflow_store, tx, state, issue, None, None)
                                        .await;
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
    notify_observers(updates, update_version);
}

fn notify_observers(updates: &watch::Sender<u64>, update_version: &mut u64) {
    *update_version = update_version.wrapping_add(1);
    let _ = updates.send(*update_version);
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
    state.worker_runtime = worker_runtime_from_config(&effective.config);
}

async fn capture_tracker_rate_budget(state: &mut State, tracker: &dyn TrackerClient) {
    state.todoist_rate_budget = tracker.rate_budget().await;
}

fn startup_validation_needed(state: &State, config: &ServiceConfig) -> bool {
    state.validated_startup_config.as_ref() != Some(config)
}

fn mark_startup_validated(state: &mut State, config: &ServiceConfig) {
    state.validated_startup_config = Some(config.clone());
}

fn worker_runtime_from_config(config: &ServiceConfig) -> WorkerRuntimeConfig {
    WorkerRuntimeConfig {
        ssh_hosts: config.worker.ssh_hosts.clone(),
        max_concurrent_agents_per_host: config.worker.max_concurrent_agents_per_host,
    }
}

async fn startup_terminal_cleanup(workflow_store: &WorkflowStore, tracker: &dyn TrackerClient) {
    let effective = workflow_store.effective();
    let worker_runtime = worker_runtime_from_config(&effective.config);

    match tracker.fetch_open_issues().await {
        Ok(issues) => {
            let live_workspaces: BTreeSet<String> = issues
                .iter()
                .map(|issue| sanitize_identifier(&issue.identifier))
                .collect();
            if worker_runtime.ssh_hosts.is_empty() {
                match Workspace::sweep_stale_workspaces(&effective.config, &live_workspaces, None)
                    .await
                {
                    Ok(removed) => {
                        for path in removed {
                            info!(
                                "workspace_cleanup=startup status=removed path={}",
                                path.display()
                            );
                        }
                    }
                    Err(error) => warn!("workspace_cleanup=startup status=skipped reason={error}"),
                }
            } else {
                for worker_host in &worker_runtime.ssh_hosts {
                    match Workspace::sweep_stale_workspaces(
                        &effective.config,
                        &live_workspaces,
                        Some(worker_host.as_str()),
                    )
                    .await
                    {
                        Ok(removed) => {
                            for path in removed {
                                info!(
                                    "workspace_cleanup=startup status=removed worker_host={} path={}",
                                    worker_host,
                                    path.display()
                                );
                            }
                        }
                        Err(error) => warn!(
                            "workspace_cleanup=startup status=skipped worker_host={} reason={error}",
                            worker_host
                        ),
                    }
                }
            }
        }
        Err(error) => {
            warn!("workspace_cleanup=startup status=skipped reason={error}");
        }
    }
}

async fn run_startup_terminal_cleanup(
    workflow_store: WorkflowStore,
    tracker: std::sync::Arc<dyn TrackerClient>,
    timeout_duration: Duration,
) {
    info!(
        "workspace_cleanup=startup status=started root={}",
        workflow_store.effective().config.workspace.root.display()
    );

    match timeout(
        timeout_duration,
        startup_terminal_cleanup(&workflow_store, tracker.as_ref()),
    )
    .await
    {
        Ok(()) => {
            info!(
                "workspace_cleanup=startup status=completed root={}",
                workflow_store.effective().config.workspace.root.display()
            );
        }
        Err(_) => {
            warn!(
                "workspace_cleanup=startup status=timeout timeout_secs={} root={}",
                timeout_duration.as_secs(),
                workflow_store.effective().config.workspace.root.display()
            );
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
    let issues_result = tracker.fetch_issue_states_by_ids(&running_ids).await;
    capture_tracker_rate_budget(state, tracker.as_ref()).await;
    match issues_result {
        Ok(issues) => {
            let returned_ids: BTreeSet<String> =
                issues.iter().map(|issue| issue.id.clone()).collect();
            let active_states = effective.config.active_state_set();
            let terminal_states = effective.config.terminal_state_set();
            for missing_id in running_ids
                .iter()
                .filter(|issue_id| !returned_ids.contains(*issue_id))
                .cloned()
                .collect::<Vec<_>>()
            {
                let maybe_running = state.running.get(&missing_id).map(|running| {
                    (
                        running.identifier.clone(),
                        running.issue.clone(),
                        running.terminal_transition_permitted,
                    )
                });
                let identifier = maybe_running
                    .as_ref()
                    .map(|(identifier, _, _)| identifier.clone())
                    .unwrap_or_else(|| missing_id.clone());
                if let Some((_, previous_issue, permitted)) = maybe_running
                    && !permitted
                    && !guarded_close_transition_may_be_in_flight(&previous_issue)
                {
                    warn!(
                        "reconcile=status=missing_unexpected issue_id={} issue_identifier={} reason=not_returned_by_tracker previous_state={}",
                        missing_id, identifier, previous_issue.state
                    );
                    match tracker.restore_active_issue(&previous_issue).await {
                        Ok(()) => {
                            info!(
                                "reconcile=status=restored issue_id={} issue_identifier={} state={}",
                                missing_id, identifier, previous_issue.state
                            );
                            if let Some(running) = state.running.get_mut(&missing_id) {
                                running.issue = previous_issue;
                                running.terminal_transition_permitted = false;
                            }
                            continue;
                        }
                        Err(error) => {
                            warn!(
                                "reconcile=status=restore_failed issue_id={} issue_identifier={} reason=not_returned_by_tracker error={}",
                                missing_id, identifier, error
                            );
                        }
                    }
                }
                info!(
                    "reconcile=status=missing issue_id={} issue_identifier={} reason=not_returned_by_tracker",
                    missing_id, identifier
                );
                terminate_running_issue(state, missing_id, true, workflow_store).await;
            }

            for issue in issues {
                if terminal_states.contains(&issue.state_key()) {
                    let unexpected_terminal = state.running.get(&issue.id).is_some_and(|running| {
                        !running.terminal_transition_permitted
                            && !guarded_close_transition_may_be_in_flight(&running.issue)
                    });
                    if unexpected_terminal {
                        let previous_issue = state
                            .running
                            .get(&issue.id)
                            .map(|running| running.issue.clone());
                        if let Some(previous_issue) = previous_issue {
                            warn!(
                                "reconcile=status=unexpected_terminal issue_id={} issue_identifier={} state={} previous_state={}",
                                issue.id, issue.identifier, issue.state, previous_issue.state
                            );
                            match tracker.restore_active_issue(&previous_issue).await {
                                Ok(()) => {
                                    info!(
                                        "reconcile=status=restored issue_id={} issue_identifier={} state={}",
                                        issue.id, issue.identifier, previous_issue.state
                                    );
                                    if let Some(running) = state.running.get_mut(&issue.id) {
                                        running.issue = previous_issue;
                                        running.terminal_transition_permitted = false;
                                    }
                                    continue;
                                }
                                Err(error) => {
                                    warn!(
                                        "reconcile=status=restore_failed issue_id={} issue_identifier={} state={} error={}",
                                        issue.id, issue.identifier, issue.state, error
                                    );
                                }
                            }
                        }
                    }
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
            let last_activity_at = running.last_codex_timestamp.unwrap_or(running.started_at);
            let elapsed_ms = now
                .signed_duration_since(last_activity_at)
                .num_milliseconds();
            let session_id = running
                .session_id
                .clone()
                .unwrap_or_else(|| "n/a".to_string());
            let identifier = running.identifier.clone();
            let tracked_issue = running.issue.clone();
            let worker_host = running.worker_host.clone();
            let workspace_location = running.workspace_location.clone();
            warn!(
                "reconcile=status=stalled issue_id={} issue_identifier={} elapsed_ms={} session_id={}",
                issue_id, identifier, elapsed_ms, session_id
            );
            let next_attempt = if running.retry_attempt > 0 {
                running.retry_attempt + 1
            } else {
                1
            };
            terminate_running_issue(state, issue_id.clone(), false, workflow_store).await;
            schedule_retry(
                tx,
                state,
                RetryRequest {
                    issue_id: issue_id.clone(),
                    attempt: next_attempt,
                    identifier,
                    tracked_issue: Some(tracked_issue),
                    worker_host,
                    workspace_location: Some(workspace_location),
                    error: Some(format!("stalled for {elapsed_ms}ms without codex activity")),
                    error_stage: None,
                    error_kind: None,
                    continuation: false,
                },
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
        let worker_host = running.worker_host.clone();
        tokio::spawn(async move {
            stop_running_entry(running).await;
            if cleanup_workspace {
                let _ = Workspace::remove_path_on_host(
                    &config,
                    &workspace_path,
                    &identifier,
                    worker_host.as_deref(),
                )
                .await;
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
        && issue.assigned_to_worker
        && !state.claimed.contains(&issue.id)
        && !state.running.contains_key(&issue.id)
        && available_slots(state) > 0
        && state_slots_available(state, config, issue)
        && worker_slots_available(state, None)
}

fn retry_candidate_issue(
    issue: &Issue,
    active_states: &std::collections::BTreeSet<String>,
    terminal_states: &std::collections::BTreeSet<String>,
) -> bool {
    candidate_issue(issue, active_states, terminal_states) && issue.assigned_to_worker
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

fn worker_slots_available(state: &State, preferred_worker_host: Option<&str>) -> bool {
    !matches!(
        select_worker_host(state, preferred_worker_host),
        WorkerSelection::NoCapacity
    )
}

fn select_worker_host(state: &State, preferred_worker_host: Option<&str>) -> WorkerSelection {
    if state.worker_runtime.ssh_hosts.is_empty() {
        return WorkerSelection::Local;
    }

    let available_hosts = state
        .worker_runtime
        .ssh_hosts
        .iter()
        .filter(|host| worker_host_slots_available(state, host))
        .cloned()
        .collect::<Vec<_>>();

    if available_hosts.is_empty() {
        return WorkerSelection::NoCapacity;
    }

    if let Some(preferred_worker_host) = preferred_worker_host
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && available_hosts
            .iter()
            .any(|host| host == preferred_worker_host)
    {
        return WorkerSelection::Remote(preferred_worker_host.to_string());
    }

    available_hosts
        .iter()
        .enumerate()
        .min_by_key(|(index, host)| (running_worker_host_count(state, host), *index))
        .map(|(_, host)| WorkerSelection::Remote(host.clone()))
        .unwrap_or(WorkerSelection::NoCapacity)
}

fn worker_host_slots_available(state: &State, worker_host: &str) -> bool {
    match state.worker_runtime.max_concurrent_agents_per_host {
        Some(limit) => running_worker_host_count(state, worker_host) < limit,
        None => true,
    }
}

fn running_worker_host_count(state: &State, worker_host: &str) -> usize {
    state
        .running
        .values()
        .filter(|running| running.worker_host.as_deref() == Some(worker_host))
        .count()
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
    preferred_worker_host: Option<String>,
) {
    let clear_claim = |state: &mut State| {
        state.claimed.remove(&issue.id);
    };
    let effective = workflow_store.effective();
    let tracker = match build_tracker_client(effective.config.clone()) {
        Ok(tracker) => tracker,
        Err(error) => {
            error!(
                "dispatch=status=skipped issue_id={} issue_identifier={} reason={error}",
                issue.id, issue.identifier
            );
            clear_claim(state);
            return;
        }
    };

    let refreshed = tracker
        .fetch_issue_states_by_ids(std::slice::from_ref(&issue.id))
        .await;
    capture_tracker_rate_budget(state, tracker.as_ref()).await;
    match refreshed {
        Ok(refreshed) => {
            let Some(issue) = refreshed.into_iter().next() else {
                info!(
                    "dispatch=status=skipped issue_id={} issue_identifier={} reason=missing_after_revalidate",
                    issue.id, issue.identifier
                );
                clear_claim(state);
                return;
            };
            let active_states = effective.config.active_state_set();
            let terminal_states = effective.config.terminal_state_set();
            if !retry_candidate_issue(&issue, &active_states, &terminal_states) {
                info!(
                    "dispatch=status=skipped issue_id={} issue_identifier={} state={} reason=stale_candidate",
                    issue.id, issue.identifier, issue.state
                );
                clear_claim(state);
                return;
            }

            let worker_host = match select_worker_host(state, preferred_worker_host.as_deref()) {
                WorkerSelection::Local => None,
                WorkerSelection::Remote(host) => Some(host),
                WorkerSelection::NoCapacity => {
                    info!(
                        "dispatch=status=skipped issue_id={} issue_identifier={} reason=no_worker_capacity preferred_worker_host={}",
                        issue.id,
                        issue.identifier,
                        preferred_worker_host.as_deref().unwrap_or("local")
                    );
                    clear_claim(state);
                    return;
                }
            };
            let workspace_path = workspace_path_for_identifier_on_host(
                &effective.config,
                &issue.identifier,
                worker_host.as_deref(),
            );
            let workspace_location = workspace_path.display().to_string();
            let cancel = CancellationToken::new();
            let issue_id = issue.id.clone();
            let issue_identifier = issue.identifier.clone();
            let join_tx = tx.clone();
            let workflow_store = workflow_store.clone();
            let cancel_clone = cancel.clone();
            let retry_attempt = attempt.unwrap_or(0);
            let worker_issue = issue.clone();
            let worker_issue_id = issue_id.clone();
            let worker_host_for_task = worker_host.clone();
            let join = tokio::spawn(async move {
                let result = run_worker(
                    workflow_store,
                    worker_issue,
                    attempt,
                    worker_host_for_task,
                    cancel_clone,
                    join_tx.clone(),
                )
                .await;
                let _ = join_tx
                    .send(Command::WorkerExit {
                        issue_id: worker_issue_id,
                        result: Box::new(result),
                    })
                    .await;
            });

            state.running.insert(
                issue_id.clone(),
                RunningEntry {
                    issue,
                    identifier: issue_identifier.clone(),
                    workspace_path,
                    workspace_location,
                    worker_host: worker_host.clone(),
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
                    terminal_transition_permitted: false,
                    recent_events: Vec::new(),
                },
            );
            state.claimed.insert(issue_id.clone());
            state.retry_attempts.remove(&issue_id);
            info!(
                "dispatch=status=started issue_id={} issue_identifier={} attempt={} worker_host={}",
                issue_id,
                issue_identifier,
                retry_attempt,
                worker_host.as_deref().unwrap_or("local")
            );
        }
        Err(error) => {
            warn!(
                "dispatch=status=skipped issue_id={} issue_identifier={} reason={error}",
                issue.id, issue.identifier
            );
            clear_claim(state);
        }
    }
}

fn handle_worker_exit(
    state: &mut State,
    tx: &mpsc::Sender<Command>,
    issue_id: &str,
    result: Result<WorkerDisposition, WorkerError>,
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
        Ok(WorkerDisposition::Continue { issue }) => {
            state.completed.insert(issue_id.to_string());
            schedule_retry(
                tx,
                state,
                RetryRequest {
                    issue_id: issue_id.to_string(),
                    attempt: 1,
                    identifier: running.identifier.clone(),
                    tracked_issue: Some(issue.clone()),
                    worker_host: running.worker_host.clone(),
                    workspace_location: Some(running.workspace_location.clone()),
                    error: None,
                    error_stage: None,
                    error_kind: None,
                    continuation: true,
                },
            );
            info!(
                "worker=status=completed issue_id={} issue_identifier={} session_id={} continuation=queued next_state={}",
                issue_id, running.identifier, session_id, issue.state
            );
        }
        Ok(WorkerDisposition::Quiesced { issue }) => {
            state.claimed.remove(issue_id);
            state.retry_attempts.remove(issue_id);
            info!(
                "worker=status=quiesced issue_id={} issue_identifier={} session_id={} final_state={}",
                issue_id,
                running.identifier,
                session_id,
                issue
                    .as_ref()
                    .map(|issue| issue.state.as_str())
                    .unwrap_or("missing")
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
                RetryRequest {
                    issue_id: issue_id.to_string(),
                    attempt,
                    identifier: running.identifier.clone(),
                    tracked_issue: Some(running.issue.clone()),
                    worker_host: running.worker_host.clone(),
                    workspace_location: Some(running.workspace_location.clone()),
                    error: Some(error.to_string()),
                    error_stage: Some(error.stage_name()),
                    error_kind: Some(error.kind_name()),
                    continuation: false,
                },
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
    let issues_result = tracker
        .fetch_issue_states_by_ids(&[issue_id.to_string()])
        .await;
    capture_tracker_rate_budget(state, tracker).await;
    match issues_result {
        Ok(issues) => {
            if let Some(issue) = issues.into_iter().find(|issue| issue.id == issue_id) {
                if terminal_states.contains(&issue.state_key()) {
                    cleanup_issue_workspace(
                        &effective.config,
                        &issue,
                        retry_entry.worker_host.as_deref(),
                        !state.worker_runtime.ssh_hosts.is_empty(),
                    )
                    .await;
                    state.claimed.remove(issue_id);
                } else if retry_candidate_issue(&issue, &active_states, &terminal_states) {
                    if available_slots(state) > 0
                        && state_slots_available(state, &effective.config, &issue)
                        && worker_slots_available(state, retry_entry.worker_host.as_deref())
                    {
                        dispatch_issue(
                            workflow_store,
                            tx,
                            state,
                            issue,
                            Some(retry_entry.attempt),
                            retry_entry.worker_host.clone(),
                        )
                        .await;
                    } else {
                        schedule_retry(
                            tx,
                            state,
                            RetryRequest {
                                issue_id: issue.id.clone(),
                                attempt: retry_entry.attempt + 1,
                                identifier: issue.identifier.clone(),
                                tracked_issue: Some(issue.clone()),
                                worker_host: retry_entry.worker_host.clone(),
                                workspace_location: retry_entry.workspace_location.clone(),
                                error: Some("no available orchestrator slots".to_string()),
                                error_stage: retry_entry.error_stage.clone(),
                                error_kind: retry_entry.error_kind.clone(),
                                continuation: false,
                            },
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
                RetryRequest {
                    issue_id: issue_id.to_string(),
                    attempt: retry_entry.attempt + 1,
                    identifier: retry_entry.identifier,
                    tracked_issue: retry_entry.tracked_issue.clone(),
                    worker_host: retry_entry.worker_host.clone(),
                    workspace_location: retry_entry.workspace_location.clone(),
                    error: Some(format!("retry poll failed: {error}")),
                    error_stage: retry_entry.error_stage.clone(),
                    error_kind: retry_entry.error_kind.clone(),
                    continuation: false,
                },
            );
        }
    }
}

async fn cleanup_issue_workspace(
    config: &ServiceConfig,
    issue: &Issue,
    worker_host: Option<&str>,
    remote_workers_configured: bool,
) {
    if let Some(worker_host) = worker_host {
        if let Err(error) =
            Workspace::remove_for_identifier_on_host(config, &issue.identifier, Some(worker_host))
                .await
        {
            warn!(
                "workspace_cleanup=retry status=failed issue_id={} issue_identifier={} worker_host={} error={error}",
                issue.id, issue.identifier, worker_host
            );
        }
        return;
    }

    if remote_workers_configured {
        for worker_host in &config.worker.ssh_hosts {
            if let Err(error) = Workspace::remove_for_identifier_on_host(
                config,
                &issue.identifier,
                Some(worker_host.as_str()),
            )
            .await
            {
                warn!(
                    "workspace_cleanup=retry status=failed issue_id={} issue_identifier={} worker_host={} error={error}",
                    issue.id, issue.identifier, worker_host
                );
            }
        }
    } else if let Err(error) = Workspace::remove_for_identifier(config, &issue.identifier).await {
        warn!(
            "workspace_cleanup=retry status=failed issue_id={} issue_identifier={} error={error}",
            issue.id, issue.identifier
        );
    }
}

fn should_cleanup_terminal_workspace_after_guarded_close(
    guarded_close_observed: bool,
    issue: Option<&Issue>,
    terminal_states: &std::collections::BTreeSet<String>,
) -> bool {
    guarded_close_observed && issue.is_none_or(|issue| terminal_states.contains(&issue.state_key()))
}

fn guarded_close_transition_may_be_in_flight(issue: &Issue) -> bool {
    issue.state_key() == "merging"
}

fn schedule_retry(tx: &mpsc::Sender<Command>, state: &mut State, request: RetryRequest) {
    let RetryRequest {
        issue_id,
        attempt,
        identifier,
        tracked_issue,
        worker_host,
        workspace_location,
        error,
        error_stage,
        error_kind,
        continuation,
    } = request;
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
            tracked_issue,
            worker_host: worker_host.clone(),
            workspace_location,
            error: error.clone(),
            error_stage: error_stage.clone(),
            error_kind: error_kind.clone(),
            cancel,
        },
    );
    warn!(
        "retry=status=queued issue_id={} issue_identifier={} attempt={} delay_ms={} worker_host={} error={}",
        issue_id,
        identifier,
        attempt,
        delay_ms,
        worker_host.as_deref().unwrap_or("local"),
        error.unwrap_or_else(|| "none".to_string())
    );
}

fn integrate_worker_runtime_info(
    state: &mut State,
    issue_id: &str,
    worker_host: Option<String>,
    workspace_path: PathBuf,
    workspace_location: String,
) {
    let Some(running) = state.running.get_mut(issue_id) else {
        return;
    };
    running.worker_host = worker_host;
    running.workspace_path = workspace_path;
    running.workspace_location = workspace_location;
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
    if let Some(worker_host) = event.worker_host.clone() {
        running.worker_host = Some(worker_host);
    }
    if todoist_close_task_succeeded(event.payload.as_ref(), issue_id) {
        running.terminal_transition_permitted = true;
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
    if running.recent_events.len() > 100 {
        let drain = running.recent_events.len() - 100;
        running.recent_events.drain(0..drain);
    }
}

fn todoist_close_task_succeeded(payload: Option<&Value>, issue_id: &str) -> bool {
    let Some(payload) = payload else {
        return false;
    };

    let tool = payload
        .get("params")
        .and_then(|params| params.get("tool").or_else(|| params.get("name")))
        .and_then(Value::as_str);
    let action = payload
        .get("params")
        .and_then(|params| params.get("arguments"))
        .and_then(|arguments| arguments.get("action"))
        .and_then(Value::as_str);
    let task_id = payload
        .get("params")
        .and_then(|params| params.get("arguments"))
        .and_then(|arguments| arguments.get("task_id"))
        .and_then(json_id_string);
    let success = payload
        .get("toolResult")
        .and_then(|value| value.get("success"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    success
        && tool == Some("todoist")
        && action == Some("close_task")
        && task_id.as_deref() == Some(issue_id)
}

fn json_id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
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
                title: running.issue.title.clone(),
                state: running.issue.state.clone(),
                url: running.issue.url.clone(),
                project_id: running.issue.project_id.clone(),
                labels: running.issue.labels.clone(),
                due: running.issue.due.clone(),
                deadline: running.issue.deadline.clone(),
                worker_host: running.worker_host.clone(),
                session_id: running.session_id.clone(),
                codex_app_server_pid: running.codex_app_server_pid,
                turn_count: running.turn_count,
                last_event: running.last_codex_event.clone(),
                last_message: running.last_codex_message.clone(),
                started_at: running.started_at,
                last_event_at: running.last_codex_timestamp,
                workspace: running.workspace_location.clone(),
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
                worker_host: retry.worker_host.clone(),
                workspace_location: retry.workspace_location.clone(),
                error: retry.error.clone(),
                error_stage: retry.error_stage.clone(),
                error_kind: retry.error_kind.clone(),
            })
            .collect(),
        codex_totals: SnapshotTotals {
            input_tokens: state.codex_totals.input_tokens,
            output_tokens: state.codex_totals.output_tokens,
            total_tokens: state.codex_totals.total_tokens,
            seconds_running: state.codex_totals.ended_seconds + live_seconds,
        },
        rate_limits: state.codex_rate_limits.clone(),
        todoist_rate_budget: state.todoist_rate_budget.clone(),
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
        .map(|running| running.workspace_location.clone())
        .or_else(|| retry.and_then(|retry| retry.workspace_location.clone()))
        .unwrap_or_else(|| {
            let effective = workflow_store.effective();
            let worker_host = running
                .and_then(|running| running.worker_host.as_deref())
                .or_else(|| retry.and_then(|retry| retry.worker_host.as_deref()));
            workspace_path_for_identifier_on_host(&effective.config, identifier, worker_host)
                .display()
                .to_string()
        });

    let issue_id = running
        .map(|running| running.issue.id.clone())
        .or_else(|| retry_entry.map(|(issue_id, _)| issue_id.clone()));
    let tracked_issue = running
        .map(|running| running.issue.clone())
        .or_else(|| retry.and_then(|retry| retry.tracked_issue.clone()));
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
        tracked_issue,
        workspace: WorkspaceDetail {
            path: workspace,
            worker_host: running
                .and_then(|running| running.worker_host.clone())
                .or_else(|| retry.and_then(|retry| retry.worker_host.clone())),
        },
        attempts: AttemptDetail {
            restart_count: running
                .map(|running| running.turn_count.saturating_sub(1))
                .unwrap_or(0),
            current_retry_attempt: retry.map(|retry| retry.attempt),
        },
        running: running.map(|running| RunningDetail {
            worker_host: running.worker_host.clone(),
            session_id: running.session_id.clone(),
            codex_app_server_pid: running.codex_app_server_pid,
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
            worker_host: retry.worker_host.clone(),
            workspace_location: retry.workspace_location.clone(),
            error: retry.error.clone(),
            error_stage: retry.error_stage.clone(),
            error_kind: retry.error_kind.clone(),
        }),
        recent_events: running
            .map(|running| running.recent_events.clone())
            .unwrap_or_default(),
        last_error: retry.and_then(|retry| retry.error.clone()),
        last_error_stage: retry.and_then(|retry| retry.error_stage.clone()),
        last_error_kind: retry.and_then(|retry| retry.error_kind.clone()),
    })
}

async fn shutdown_running(entries: Vec<RunningEntry>) {
    for running in entries {
        stop_running_entry(running).await;
    }
}

async fn stop_running_entry(mut running: RunningEntry) {
    running.cancel.cancel();
    let issue_id = running.issue.id.clone();
    let identifier = running.identifier.clone();
    let pid = running.codex_app_server_pid;

    match timeout(
        Duration::from_secs(RUNNING_SHUTDOWN_TIMEOUT_SECS),
        &mut running.join,
    )
    .await
    {
        Ok(_) => {}
        Err(_) => {
            warn!(
                "worker_shutdown=status=timeout issue_id={} issue_identifier={} pid={:?}",
                issue_id, identifier, pid
            );
            force_kill_process(pid, running.worker_host.as_deref());
            running.join.abort();
            let _ = running.join.await;
        }
    }
}

#[cfg(unix)]
fn force_kill_process(pid: Option<u32>, worker_host: Option<&str>) {
    if let Some(pid) = pid {
        if let Some(worker_host) = worker_host {
            if let Ok(executable) = ssh_executable() {
                let _ = StdCommand::new(executable)
                    .args(ssh_args(worker_host, &format!("kill -9 {pid}")))
                    .status();
            }
        } else {
            let _ = StdCommand::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
    }
}

#[cfg(not(unix))]
fn force_kill_process(_pid: Option<u32>, _worker_host: Option<&str>) {}

fn shutdown_retries(entries: Vec<RetryEntry>) {
    for retry in entries {
        retry.cancel.cancel();
    }
}

fn candidate_worker_hosts(
    config: &ServiceConfig,
    preferred_worker_host: Option<&str>,
) -> Vec<Option<String>> {
    let mut seen = BTreeSet::new();
    let hosts = config
        .worker
        .ssh_hosts
        .iter()
        .map(|host| host.trim())
        .filter(|host| !host.is_empty())
        .filter(|host| seen.insert((*host).to_string()))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    match preferred_worker_host
        .map(str::trim)
        .filter(|host| !host.is_empty())
    {
        Some(preferred) => {
            let mut candidates = vec![Some(preferred.to_string())];
            for host in hosts {
                if host != preferred {
                    candidates.push(Some(host));
                }
            }
            candidates
        }
        None if hosts.is_empty() => vec![None],
        None => hosts.into_iter().map(Some).collect(),
    }
}

fn workspace_worker_error(
    stage: WorkerErrorStage,
    worker_host: Option<&str>,
    error: WorkspaceError,
) -> WorkerError {
    let kind = match stage {
        WorkerErrorStage::BeforeRun => WorkerErrorKind::HookFailure,
        _ => WorkerErrorKind::WorkspaceFailure,
    };
    WorkerError::new(
        stage,
        kind,
        worker_host.map(ToOwned::to_owned),
        error.to_string(),
    )
}

fn tracker_worker_error(
    stage: WorkerErrorStage,
    worker_host: Option<&str>,
    error: TrackerError,
) -> WorkerError {
    WorkerError::new(
        stage,
        WorkerErrorKind::TrackerFailure,
        worker_host.map(ToOwned::to_owned),
        error.to_string(),
    )
}

fn codex_worker_error(
    stage: WorkerErrorStage,
    worker_host: Option<&str>,
    error: CodexError,
) -> WorkerError {
    let kind = match error {
        CodexError::ApprovalRequired(_) => WorkerErrorKind::ApprovalRequired,
        CodexError::TurnInputRequired(_) => WorkerErrorKind::InputRequired,
        CodexError::TurnCancelled(_) => WorkerErrorKind::Cancellation,
        CodexError::TurnFailed(_) => WorkerErrorKind::TurnFailure,
        _ if stage == WorkerErrorStage::TurnRun => WorkerErrorKind::TurnFailure,
        _ => WorkerErrorKind::SessionFailure,
    };
    WorkerError::new(
        stage,
        kind,
        worker_host.map(ToOwned::to_owned),
        error.to_string(),
    )
}

async fn start_worker_on_candidate_hosts<T, F, Fut>(
    candidate_hosts: Vec<Option<String>>,
    mut start: F,
) -> Result<(Option<String>, T), WorkerError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<T, WorkerError>>,
{
    let total_hosts = candidate_hosts.len();
    let mut startup_failures = Vec::new();

    for worker_host in candidate_hosts {
        match start(worker_host.clone()).await {
            Ok(started) => return Ok((worker_host, started)),
            Err(error)
                if error.startup_failover_eligible()
                    && startup_failures.len() + 1 < total_hosts =>
            {
                warn!(
                    "worker_startup=status=failed worker_host={} error={}; trying next worker host",
                    worker_host.as_deref().unwrap_or("local"),
                    error
                );
                startup_failures.push(error);
            }
            Err(error) => {
                startup_failures.push(error);
                return Err(WorkerError::startup_failover_exhausted(startup_failures));
            }
        }
    }

    Err(WorkerError::startup_failover_exhausted(startup_failures))
}

async fn run_worker(
    workflow_store: WorkflowStore,
    issue: Issue,
    attempt: Option<u32>,
    worker_host: Option<String>,
    cancellation: CancellationToken,
    tx: mpsc::Sender<Command>,
) -> Result<WorkerDisposition, WorkerError> {
    let effective = workflow_store.effective();
    info!(
        "worker=status=starting issue_id={} issue_identifier={} worker_host={}",
        issue.id,
        issue.identifier,
        worker_host.as_deref().unwrap_or("local")
    );

    let tracker = build_tracker_client(effective.config.clone())
        .map_err(|error| tracker_worker_error(WorkerErrorStage::TrackerBuild, None, error))?;
    let app = AppServerClient::new(effective.config.clone(), tracker.clone());
    let mut refresh_tracker = tracker.clone();
    let mut refresh_tracker_config = effective.config.clone();
    let candidate_hosts = candidate_worker_hosts(&effective.config, worker_host.as_deref());
    let (worker_host, (workspace, mut session)) =
        start_worker_on_candidate_hosts(candidate_hosts, |candidate_worker_host| {
            let app = &app;
            let config = &effective.config;
            let issue = &issue;
            async move {
                let workspace = Workspace::create_for_issue_on_host(
                    config,
                    issue,
                    candidate_worker_host.as_deref(),
                )
                .await
                .map_err(|error| {
                    workspace_worker_error(
                        WorkerErrorStage::WorkspaceCreate,
                        candidate_worker_host.as_deref(),
                        error,
                    )
                })?;
                if let Err(error) = workspace.run_before_run(config, issue).await {
                    workspace.run_after_run(config, issue).await;
                    return Err(workspace_worker_error(
                        WorkerErrorStage::BeforeRun,
                        candidate_worker_host.as_deref(),
                        error,
                    ));
                }

                match app
                    .start_session_on_host(&workspace.path, candidate_worker_host.as_deref())
                    .await
                {
                    Ok(session) => Ok((workspace, session)),
                    Err(error) => {
                        workspace.run_after_run(config, issue).await;
                        Err(codex_worker_error(
                            WorkerErrorStage::SessionStart,
                            candidate_worker_host.as_deref(),
                            error,
                        ))
                    }
                }
            }
        })
        .await?;

    let _ = tx
        .send(Command::WorkerRuntimeInfo {
            issue_id: issue.id.clone(),
            worker_host: worker_host.clone(),
            workspace_path: workspace.path.clone(),
            workspace_location: workspace.path.display().to_string(),
        })
        .await;

    let worker_result = async {
        let mut current_issue = tracker
            .fetch_issue_states_by_ids(std::slice::from_ref(&issue.id))
            .await
            .map_err(|error| {
                WorkerError::new(
                    WorkerErrorStage::StateRefresh,
                    WorkerErrorKind::TrackerFailure,
                    worker_host.clone(),
                    error.to_string(),
                )
            })?
            .into_iter()
            .next()
            .unwrap_or_else(|| issue.clone());
        let max_turns = workflow_store.effective().config.agent.max_turns;
        let terminal_states = workflow_store.effective().config.terminal_state_set();
        let mut guarded_close_observed = false;
        for turn_number in 1..=max_turns {
            if cancellation.is_cancelled() {
                return Err(WorkerError::new(
                    WorkerErrorStage::TurnRun,
                    WorkerErrorKind::Cancellation,
                    worker_host.clone(),
                    "cancelled by orchestrator",
                ));
            }
            let workflow = workflow_store.effective();
            let prompt = if turn_number == 1 {
                prompt::build_issue_prompt(&workflow.definition, &current_issue, attempt).map_err(
                    |error| {
                        WorkerError::new(
                            WorkerErrorStage::TurnRun,
                            WorkerErrorKind::TurnFailure,
                            worker_host.clone(),
                            error.to_string(),
                        )
                    },
                )?
            } else {
                prompt::continuation_guidance(turn_number, max_turns)
            };
            let guarded_close_for_turn =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let guarded_close_events = std::sync::Arc::clone(&guarded_close_for_turn);
            let issue_id_for_events = current_issue.id.clone();

            session
                .run_turn(&prompt, &current_issue, &cancellation, |event| {
                    if todoist_close_task_succeeded(event.payload.as_ref(), &issue_id_for_events) {
                        guarded_close_events.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
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
                .map_err(|error| {
                    codex_worker_error(WorkerErrorStage::TurnRun, worker_host.as_deref(), error)
                })?;
            guarded_close_observed |=
                guarded_close_for_turn.load(std::sync::atomic::Ordering::Relaxed);

            let refresh_config = workflow_store.effective().config.clone();
            if refresh_config != refresh_tracker_config {
                refresh_tracker =
                    build_tracker_client(refresh_config.clone()).map_err(|error| {
                        tracker_worker_error(
                            WorkerErrorStage::TrackerBuild,
                            worker_host.as_deref(),
                            error,
                        )
                    })?;
                refresh_tracker_config = refresh_config;
            }
            let refreshed = refresh_tracker
                .fetch_issue_states_by_ids(std::slice::from_ref(&current_issue.id))
                .await
                .map_err(|error| {
                    tracker_worker_error(
                        WorkerErrorStage::StateRefresh,
                        worker_host.as_deref(),
                        error,
                    )
                })?;
            if let Some(next_issue) = refreshed.into_iter().next() {
                current_issue = next_issue;
            } else if should_cleanup_terminal_workspace_after_guarded_close(
                guarded_close_observed,
                None,
                &terminal_states,
            ) {
                return Ok((WorkerDisposition::Quiesced { issue: None }, true));
            }

            let active = workflow_store
                .effective()
                .config
                .active_state_set()
                .contains(&current_issue.state_key());
            if !active {
                let cleanup_terminal_workspace =
                    should_cleanup_terminal_workspace_after_guarded_close(
                        guarded_close_observed,
                        Some(&current_issue),
                        &terminal_states,
                    );
                return Ok((
                    WorkerDisposition::Quiesced {
                        issue: Some(current_issue),
                    },
                    cleanup_terminal_workspace,
                ));
            }
            if turn_number >= max_turns {
                break;
            }
        }
        Ok((
            WorkerDisposition::Continue {
                issue: current_issue,
            },
            false,
        ))
    }
    .await;

    let cleanup_terminal_workspace = matches!(worker_result.as_ref(), Ok((_, true)));
    session.stop().await;
    workspace
        .run_after_run(&workflow_store.effective().config, &issue)
        .await;
    if cleanup_terminal_workspace {
        Workspace::remove_path_on_host(
            &workflow_store.effective().config,
            &workspace.path,
            &issue.identifier,
            worker_host.as_deref(),
        )
        .await
        .map_err(|error| {
            workspace_worker_error(WorkerErrorStage::Cleanup, worker_host.as_deref(), error)
        })?;
    }
    worker_result.map(|(result, _)| result)
}

#[cfg(test)]
mod tests {
    use axum::{
        Json, Router,
        extract::{Path, Query, State as AxumState},
        routing::{get, post},
    };
    use chrono::{Duration as ChronoDuration, Utc};
    use serde_json::{Value, json};
    use std::{
        collections::HashMap,
        fs,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };
    use tempfile::tempdir;
    use tokio::{
        net::TcpListener,
        sync::{oneshot, watch},
        task::JoinHandle,
    };
    use tokio_util::sync::CancellationToken;

    use crate::issue::Issue;

    use super::{
        Orchestrator, OrchestratorHandle, OrchestratorHandleError, RetryEntry, RunningEntry, State,
        WorkerDisposition, WorkerError, WorkerErrorKind, WorkerErrorStage, WorkerRuntimeConfig,
        WorkerSelection, build_snapshot, candidate_issue, dispatch_issue,
        guarded_close_transition_may_be_in_flight, handle_retry_issue_with_tracker,
        handle_worker_exit, run_startup_terminal_cleanup, select_worker_host,
        should_cleanup_terminal_workspace_after_guarded_close, sort_issues_for_dispatch,
        todoist_close_task_succeeded,
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
    fn todo_issue_with_required_fields_is_a_candidate() {
        let issue = Issue {
            id: "1".to_string(),
            identifier: "A".to_string(),
            title: "a".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };
        let active: std::collections::BTreeSet<String> = ["todo".to_string()].into_iter().collect();
        let terminal: std::collections::BTreeSet<String> =
            ["done".to_string()].into_iter().collect();

        assert!(candidate_issue(&issue, &active, &terminal));
    }

    #[test]
    fn snapshot_reports_counts() {
        let snapshot = build_snapshot(&State::default());
        assert_eq!(snapshot.counts.running, 0);
        assert_eq!(snapshot.counts.retrying, 0);
    }

    #[tokio::test]
    async fn worker_host_selection_prefers_least_loaded_host_and_honors_affinity() {
        let mut state = State {
            worker_runtime: WorkerRuntimeConfig {
                ssh_hosts: vec!["ssh-a".to_string(), "ssh-b".to_string()],
                max_concurrent_agents_per_host: Some(2),
            },
            ..State::default()
        };
        state.running.insert(
            "issue-1".to_string(),
            RunningEntry {
                issue: Issue {
                    id: "issue-1".to_string(),
                    identifier: "ABC-1".to_string(),
                    title: "one".to_string(),
                    state: "In Progress".to_string(),
                    ..Issue::default()
                },
                identifier: "ABC-1".to_string(),
                workspace_path: PathBuf::from("/tmp/workspace-a"),
                workspace_location: "/tmp/workspace-a".to_string(),
                worker_host: Some("ssh-a".to_string()),
                cancel: CancellationToken::new(),
                join: tokio::spawn(async {}),
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
                terminal_transition_permitted: false,
                recent_events: Vec::new(),
            },
        );

        assert_eq!(
            select_worker_host(&state, None),
            WorkerSelection::Remote("ssh-b".to_string())
        );
        assert_eq!(
            select_worker_host(&state, Some("ssh-a")),
            WorkerSelection::Remote("ssh-a".to_string())
        );

        state.running.insert(
            "issue-2".to_string(),
            RunningEntry {
                issue: Issue {
                    id: "issue-2".to_string(),
                    identifier: "ABC-2".to_string(),
                    title: "two".to_string(),
                    state: "In Progress".to_string(),
                    ..Issue::default()
                },
                identifier: "ABC-2".to_string(),
                workspace_path: PathBuf::from("/tmp/workspace-a-2"),
                workspace_location: "/tmp/workspace-a-2".to_string(),
                worker_host: Some("ssh-a".to_string()),
                cancel: CancellationToken::new(),
                join: tokio::spawn(async {}),
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
                terminal_transition_permitted: false,
                recent_events: Vec::new(),
            },
        );

        assert_eq!(
            select_worker_host(&state, Some("ssh-a")),
            WorkerSelection::Remote("ssh-b".to_string())
        );
    }

    #[test]
    fn candidate_worker_hosts_prefers_affinity_then_remaining_hosts() {
        let workflow_store = sample_workflow_store_with_worker_hosts(
            std::env::temp_dir().join("symphony-workers").as_path(),
            &["ssh-a", "ssh-b", "ssh-a"],
        );
        let config = workflow_store.effective().config;

        assert_eq!(
            super::candidate_worker_hosts(&config, Some("ssh-b")),
            vec![Some("ssh-b".to_string()), Some("ssh-a".to_string())]
        );
        assert_eq!(
            super::candidate_worker_hosts(&config, None),
            vec![Some("ssh-a".to_string()), Some("ssh-b".to_string())]
        );
    }

    #[tokio::test]
    async fn startup_host_failover_tries_next_worker_host() {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let calls = attempts.clone();
        let result = super::start_worker_on_candidate_hosts(
            vec![Some("ssh-a".to_string()), Some("ssh-b".to_string())],
            move |worker_host| {
                let calls = calls.clone();
                async move {
                    calls
                        .lock()
                        .expect("attempts")
                        .push(worker_host.as_deref().unwrap_or("local").to_string());
                    if worker_host.as_deref() == Some("ssh-a") {
                        Err(WorkerError::new(
                            WorkerErrorStage::SessionStart,
                            WorkerErrorKind::SessionFailure,
                            Some("ssh-a".to_string()),
                            "ssh-a unavailable",
                        ))
                    } else {
                        Ok("started")
                    }
                }
            },
        )
        .await
        .expect("fallback should succeed");

        assert_eq!(result.0.as_deref(), Some("ssh-b"));
        assert_eq!(
            attempts.lock().expect("attempts").as_slice(),
            ["ssh-a".to_string(), "ssh-b".to_string()]
        );
    }

    #[tokio::test]
    async fn integrate_worker_runtime_info_updates_host_and_workspace() {
        let mut state = State::default();
        state.running.insert(
            "issue-1".to_string(),
            RunningEntry {
                issue: Issue {
                    id: "issue-1".to_string(),
                    identifier: "ABC-1".to_string(),
                    title: "one".to_string(),
                    state: "In Progress".to_string(),
                    ..Issue::default()
                },
                identifier: "ABC-1".to_string(),
                workspace_path: PathBuf::from("/tmp/original"),
                workspace_location: "/tmp/original".to_string(),
                worker_host: Some("ssh-a".to_string()),
                cancel: CancellationToken::new(),
                join: tokio::spawn(async {}),
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
                terminal_transition_permitted: false,
                recent_events: Vec::new(),
            },
        );

        super::integrate_worker_runtime_info(
            &mut state,
            "issue-1",
            Some("ssh-b".to_string()),
            PathBuf::from("/srv/symphony/ABC-1"),
            "/srv/symphony/ABC-1".to_string(),
        );

        let running = state.running.get("issue-1").expect("running entry");
        assert_eq!(running.worker_host.as_deref(), Some("ssh-b"));
        assert_eq!(running.workspace_path, PathBuf::from("/srv/symphony/ABC-1"));
        assert_eq!(running.workspace_location, "/srv/symphony/ABC-1");
    }

    #[tokio::test]
    async fn snapshot_includes_worker_host_and_workspace_affinity() {
        let mut state = State::default();
        state.running.insert(
            "issue-1".to_string(),
            RunningEntry {
                issue: Issue {
                    id: "issue-1".to_string(),
                    identifier: "ABC-1".to_string(),
                    title: "one".to_string(),
                    state: "In Progress".to_string(),
                    ..Issue::default()
                },
                identifier: "ABC-1".to_string(),
                workspace_path: PathBuf::from("/tmp/workspace-a"),
                workspace_location: "/srv/symphony/ABC-1".to_string(),
                worker_host: Some("ssh-a".to_string()),
                cancel: CancellationToken::new(),
                join: tokio::spawn(async {}),
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
                terminal_transition_permitted: false,
                recent_events: Vec::new(),
            },
        );
        state.retry_attempts.insert(
            "issue-2".to_string(),
            RetryEntry {
                attempt: 2,
                due_at: Instant::now() + Duration::from_secs(30),
                identifier: "ABC-2".to_string(),
                tracked_issue: None,
                worker_host: Some("ssh-b".to_string()),
                workspace_location: Some("/srv/symphony/ABC-2".to_string()),
                error: Some("boom".to_string()),
                error_stage: None,
                error_kind: None,
                cancel: CancellationToken::new(),
            },
        );

        let snapshot = build_snapshot(&state);
        assert_eq!(snapshot.running[0].worker_host.as_deref(), Some("ssh-a"));
        assert_eq!(snapshot.running[0].workspace, "/srv/symphony/ABC-1");
        assert_eq!(snapshot.retrying[0].worker_host.as_deref(), Some("ssh-b"));
        assert_eq!(
            snapshot.retrying[0].workspace_location.as_deref(),
            Some("/srv/symphony/ABC-2")
        );
    }

    #[test]
    fn detects_guarded_todoist_close_task_events() {
        let payload = json!({
            "params": {
                "name": "todoist",
                "arguments": {
                    "action": "close_task",
                    "task_id": "task-1"
                }
            },
            "toolResult": {
                "success": true
            }
        });

        assert!(todoist_close_task_succeeded(Some(&payload), "task-1"));
        assert!(!todoist_close_task_succeeded(Some(&payload), "task-2"));
    }

    #[test]
    fn guarded_close_cleanup_only_triggers_for_terminal_or_missing_issue_state() {
        let terminal_states: std::collections::BTreeSet<String> =
            ["done".to_string()].into_iter().collect();
        let done_issue = Issue {
            state: "Done".to_string(),
            ..Issue::default()
        };
        let merging_issue = Issue {
            state: "Merging".to_string(),
            ..Issue::default()
        };

        assert!(should_cleanup_terminal_workspace_after_guarded_close(
            true,
            Some(&done_issue),
            &terminal_states,
        ));
        assert!(should_cleanup_terminal_workspace_after_guarded_close(
            true,
            None,
            &terminal_states,
        ));
        assert!(!should_cleanup_terminal_workspace_after_guarded_close(
            true,
            Some(&merging_issue),
            &terminal_states,
        ));
        assert!(!should_cleanup_terminal_workspace_after_guarded_close(
            false,
            Some(&done_issue),
            &terminal_states,
        ));
    }

    #[test]
    fn merging_state_is_treated_as_a_possible_guarded_close_in_flight() {
        let merging_issue = Issue {
            state: "Merging".to_string(),
            ..Issue::default()
        };
        let review_issue = Issue {
            state: "Human Review".to_string(),
            ..Issue::default()
        };

        assert!(guarded_close_transition_may_be_in_flight(&merging_issue));
        assert!(!guarded_close_transition_may_be_in_flight(&review_issue));
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
                workspace_location: "/tmp/workspace".to_string(),
                worker_host: None,
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
                terminal_transition_permitted: false,
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
                tracked_issue: Some(issue.clone()),
                worker_host: None,
                workspace_location: Some(workspace_path.display().to_string()),
                error: None,
                error_stage: None,
                error_kind: None,
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
                tracked_issue: Some(Issue {
                    id: "issue-1".to_string(),
                    identifier: "ABC-1".to_string(),
                    title: "Retry me".to_string(),
                    state: "Rework".to_string(),
                    labels: vec!["backend".to_string()],
                    url: Some("https://app.todoist.com/app/task/issue-1".to_string()),
                    project_id: Some("proj-1".to_string()),
                    ..Issue::default()
                }),
                worker_host: Some("ssh-a".to_string()),
                workspace_location: Some("/srv/symphony/ABC-1".to_string()),
                error: Some("boom".to_string()),
                error_stage: Some("session_start".to_string()),
                error_kind: Some("session_failure".to_string()),
                cancel: CancellationToken::new(),
            },
        );

        let detail = build_issue_detail(&workflow_store, &state, "ABC-1").expect("detail");
        assert_eq!(detail.issue_id.as_deref(), Some("issue-1"));
        assert_eq!(
            detail
                .tracked_issue
                .as_ref()
                .and_then(|issue| issue.project_id.as_deref()),
            Some("proj-1")
        );
        assert_eq!(detail.workspace.worker_host.as_deref(), Some("ssh-a"));
        assert_eq!(detail.workspace.path, "/srv/symphony/ABC-1");
        assert_eq!(detail.last_error_stage.as_deref(), Some("session_start"));
        assert_eq!(detail.last_error_kind.as_deref(), Some("session_failure"));
    }

    #[tokio::test]
    async fn handle_issue_detail_times_out_when_command_loop_is_unresponsive() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (_updates_tx, updates_rx) = watch::channel(0u64);
        let handle = OrchestratorHandle {
            tx,
            updates: updates_rx,
        };

        assert!(matches!(
            handle.issue_detail("ABC-1".to_string()).await,
            Err(OrchestratorHandleError::TimedOut)
        ));
    }

    #[tokio::test]
    async fn quiesced_worker_exit_clears_claim_without_retry() {
        let issue = Issue {
            id: "issue-1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Review ready".to_string(),
            state: "In Progress".to_string(),
            assigned_to_worker: true,
            ..Issue::default()
        };
        let mut state = State::default();
        state.running.insert(
            issue.id.clone(),
            RunningEntry {
                issue: issue.clone(),
                identifier: issue.identifier.clone(),
                workspace_path: PathBuf::from("/tmp/workspace"),
                workspace_location: "/tmp/workspace".to_string(),
                worker_host: None,
                cancel: CancellationToken::new(),
                join: tokio::spawn(async {}),
                session_id: Some("session-1".to_string()),
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
                turn_count: 1,
                retry_attempt: 0,
                started_at: Utc::now(),
                terminal_transition_permitted: false,
                recent_events: Vec::new(),
            },
        );
        state.claimed.insert(issue.id.clone());

        let quiesced_issue = Issue {
            state: "Human Review".to_string(),
            ..issue.clone()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(1);

        handle_worker_exit(
            &mut state,
            &tx,
            &issue.id,
            Ok(WorkerDisposition::Quiesced {
                issue: Some(quiesced_issue),
            }),
        );

        assert!(!state.claimed.contains(&issue.id));
        assert!(!state.retry_attempts.contains_key(&issue.id));
        assert!(state.running.is_empty());
    }

    #[tokio::test]
    async fn continuation_retry_tracks_latest_issue_state() {
        let issue = Issue {
            id: "issue-1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Merge ready".to_string(),
            state: "In Progress".to_string(),
            assigned_to_worker: true,
            ..Issue::default()
        };
        let mut state = State {
            max_retry_backoff_ms: 60_000,
            ..State::default()
        };
        state.running.insert(
            issue.id.clone(),
            RunningEntry {
                issue: issue.clone(),
                identifier: issue.identifier.clone(),
                workspace_path: PathBuf::from("/tmp/workspace"),
                workspace_location: "/tmp/workspace".to_string(),
                worker_host: Some("ssh-a".to_string()),
                cancel: CancellationToken::new(),
                join: tokio::spawn(async {}),
                session_id: Some("session-1".to_string()),
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
                turn_count: 1,
                retry_attempt: 0,
                started_at: Utc::now(),
                terminal_transition_permitted: false,
                recent_events: Vec::new(),
            },
        );
        state.claimed.insert(issue.id.clone());

        let continued_issue = Issue {
            state: "Merging".to_string(),
            ..issue.clone()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel(1);

        handle_worker_exit(
            &mut state,
            &tx,
            &issue.id,
            Ok(WorkerDisposition::Continue {
                issue: continued_issue,
            }),
        );

        let retry = state
            .retry_attempts
            .get(&issue.id)
            .expect("continuation retry should be queued");
        assert_eq!(
            retry
                .tracked_issue
                .as_ref()
                .map(|issue| issue.state.as_str()),
            Some("Merging")
        );
        assert_eq!(retry.worker_host.as_deref(), Some("ssh-a"));
    }

    #[tokio::test]
    async fn worker_failure_retry_captures_structured_error_fields() {
        let issue = Issue {
            id: "issue-1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Retry me".to_string(),
            state: "In Progress".to_string(),
            assigned_to_worker: true,
            ..Issue::default()
        };
        let mut state = State {
            max_retry_backoff_ms: 60_000,
            ..State::default()
        };
        state.running.insert(
            issue.id.clone(),
            RunningEntry {
                issue: issue.clone(),
                identifier: issue.identifier.clone(),
                workspace_path: PathBuf::from("/tmp/workspace"),
                workspace_location: "/tmp/workspace".to_string(),
                worker_host: Some("ssh-a".to_string()),
                cancel: CancellationToken::new(),
                join: tokio::spawn(async {}),
                session_id: Some("session-1".to_string()),
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
                turn_count: 1,
                retry_attempt: 0,
                started_at: Utc::now(),
                terminal_transition_permitted: false,
                recent_events: Vec::new(),
            },
        );

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        handle_worker_exit(
            &mut state,
            &tx,
            &issue.id,
            Err(WorkerError::new(
                WorkerErrorStage::SessionStart,
                WorkerErrorKind::SessionFailure,
                Some("ssh-a".to_string()),
                "ssh died",
            )),
        );

        let retry = state
            .retry_attempts
            .get(&issue.id)
            .expect("retry should be queued");
        assert_eq!(retry.error_stage.as_deref(), Some("session_start"));
        assert_eq!(retry.error_kind.as_deref(), Some("session_failure"));
        assert!(
            retry
                .error
                .as_deref()
                .is_some_and(|error| error.contains("ssh died"))
        );
    }

    #[tokio::test]
    async fn dispatch_issue_clears_claim_when_revalidate_fails_before_start() {
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        fs::write(
            &workflow_path,
            format!(
                r#"---
tracker:
  kind: todoist
workspace:
  root: {}
---

test
"#,
                dir.path().join("workspaces").display()
            ),
        )
        .expect("workflow");

        let workflow_store = WorkflowStore::new(workflow_path).expect("workflow store");
        let issue = Issue {
            id: "issue-1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Retry me".to_string(),
            state: "Merging".to_string(),
            assigned_to_worker: true,
            ..Issue::default()
        };
        let mut state = State::default();
        state.claimed.insert(issue.id.clone());

        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        dispatch_issue(
            &workflow_store,
            &tx,
            &mut state,
            issue.clone(),
            Some(1),
            None,
        )
        .await;

        assert!(
            !state.claimed.contains(&issue.id),
            "dispatch should release the claim when it cannot start a worker"
        );
        assert!(!state.running.contains_key(&issue.id));
    }

    #[tokio::test]
    async fn orchestrator_start_fails_when_todoist_comments_are_unavailable() {
        let dir = tempdir().expect("tempdir");
        let fixture_path = dir.path().join("memory.json");
        fs::write(
            &fixture_path,
            r#"{
  "tasks": [],
  "sections": [{"id":"sec-todo","project_id":"proj","name":"Todo"}],
  "user_plan_limits": {"comments": false}
}"#,
        )
        .expect("fixture");
        let workflow_path = dir.path().join("WORKFLOW.md");
        fs::write(
            &workflow_path,
            format!(
                r#"---
tracker:
  kind: memory
  fixture_path: {}
  project_id: proj
workspace:
  root: {}
---

test
"#,
                fixture_path.display(),
                dir.path().join("workspaces").display()
            ),
        )
        .expect("workflow");

        let workflow_store = WorkflowStore::new(workflow_path).expect("workflow store");
        let error = match Orchestrator::start(workflow_store).await {
            Ok(_) => panic!("startup should fail"),
            Err(error) => error,
        };

        assert!(error.contains("todoist_comments_unavailable"));
    }

    #[tokio::test]
    async fn startup_cleanup_timeout_returns_when_fetch_open_issues_hangs() {
        let workflow_store = sample_workflow_store();
        let (started_tx, started_rx) = oneshot::channel();
        let tracker = Arc::new(HangingOpenIssuesTracker {
            started: Mutex::new(Some(started_tx)),
        });

        let cleanup = tokio::spawn(run_startup_terminal_cleanup(
            workflow_store,
            tracker,
            Duration::from_millis(20),
        ));

        started_rx.await.expect("cleanup should start");
        tokio::time::timeout(Duration::from_secs(1), cleanup)
            .await
            .expect("cleanup task should finish after timeout")
            .expect("cleanup task should not panic");
    }

    #[tokio::test]
    async fn startup_cleanup_skips_local_workspace_sweep_when_remote_workers_are_configured() {
        let dir = tempdir().expect("tempdir");
        let workflow_store =
            sample_workflow_store_with_worker_hosts(dir.path(), &["ssh-a", "ssh-b"]);
        let workspace_path = workflow_store
            .effective()
            .config
            .workspace
            .root
            .join("ABC-1");
        fs::create_dir_all(&workspace_path).expect("workspace");

        run_startup_terminal_cleanup(
            workflow_store,
            Arc::new(StaticTracker {
                candidates: Vec::new(),
                by_id: Vec::new(),
            }),
            Duration::from_secs(1),
        )
        .await;

        assert!(workspace_path.exists());
    }

    #[derive(Default)]
    struct TodoistTickCounts {
        project: AtomicUsize,
        sections: AtomicUsize,
        plan_limits: AtomicUsize,
        tasks: AtomicUsize,
    }

    struct TodoistTickServer {
        base_url: String,
        join: JoinHandle<()>,
    }

    impl Drop for TodoistTickServer {
        fn drop(&mut self) {
            self.join.abort();
        }
    }

    async fn spawn_counting_todoist_tick_server(
        counts: Arc<TodoistTickCounts>,
    ) -> TodoistTickServer {
        async fn get_project(
            AxumState(counts): AxumState<Arc<TodoistTickCounts>>,
            Path(_project_id): Path<String>,
        ) -> Json<Value> {
            counts.project.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "id": "proj",
                "is_shared": false,
                "can_assign_tasks": false
            }))
        }

        async fn list_sections(
            AxumState(counts): AxumState<Arc<TodoistTickCounts>>,
            Query(_query): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            counts.sections.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "results": [
                    {"id": "sec-todo", "project_id": "proj", "name": "Todo"},
                    {"id": "sec-progress", "project_id": "proj", "name": "In Progress"}
                ],
                "next_cursor": null
            }))
        }

        async fn sync(AxumState(counts): AxumState<Arc<TodoistTickCounts>>) -> Json<Value> {
            counts.plan_limits.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "user_plan_limits": {
                    "current": {
                        "comments": true
                    }
                }
            }))
        }

        async fn list_tasks(
            AxumState(counts): AxumState<Arc<TodoistTickCounts>>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            counts.tasks.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "results": if !query.contains_key("label") {
                    vec![json!({
                        "id": "task-1",
                        "content": "Cache me",
                        "project_id": "proj",
                        "section_id": "sec-todo",
                        "labels": []
                    })]
                } else {
                    Vec::new()
                },
                "next_cursor": null
            }))
        }

        let app = Router::new()
            .route("/projects/{project_id}", get(get_project))
            .route("/sections", get(list_sections))
            .route("/sync", post(sync))
            .route("/tasks", get(list_tasks))
            .with_state(counts);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("local addr");
        let join = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        TodoistTickServer {
            base_url: format!("http://{address}"),
            join,
        }
    }

    #[tokio::test]
    async fn run_tick_reuses_startup_validation_and_shared_tracker_metadata_when_config_is_unchanged()
     {
        let counts = Arc::new(TodoistTickCounts::default());
        let server = spawn_counting_todoist_tick_server(counts.clone()).await;
        let dir = tempdir().expect("tempdir");
        let workflow_store = todoist_workflow_store_with_base_url(dir.path(), &server.base_url);
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let (updates_tx, _updates_rx) = watch::channel(0u64);
        let mut state = State::default();
        let mut version = 0u64;

        super::run_tick(&workflow_store, &tx, &mut state, &updates_tx, &mut version).await;
        super::run_tick(&workflow_store, &tx, &mut state, &updates_tx, &mut version).await;

        assert_eq!(counts.project.load(Ordering::SeqCst), 1);
        assert_eq!(counts.sections.load(Ordering::SeqCst), 1);
        assert_eq!(counts.plan_limits.load(Ordering::SeqCst), 1);
        assert_eq!(counts.tasks.load(Ordering::SeqCst), 2);
        assert_eq!(
            state.validated_startup_config.as_ref(),
            Some(&workflow_store.effective().config)
        );
    }

    #[tokio::test]
    async fn workflow_reload_updates_validated_config_without_refetching_unrelated_metadata() {
        let counts = Arc::new(TodoistTickCounts::default());
        let server = spawn_counting_todoist_tick_server(counts.clone()).await;
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        write_todoist_workflow_with_base_url_label_and_api_key(
            &workflow_path,
            dir.path(),
            &server.base_url,
            "runtime-owned",
            "token",
        );
        let workflow_store = WorkflowStore::new(workflow_path.clone()).expect("workflow store");
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let (updates_tx, _updates_rx) = watch::channel(0u64);
        let mut state = State::default();
        let mut version = 0u64;

        super::run_tick(&workflow_store, &tx, &mut state, &updates_tx, &mut version).await;
        super::run_tick(&workflow_store, &tx, &mut state, &updates_tx, &mut version).await;
        assert_eq!(counts.project.load(Ordering::SeqCst), 1);
        assert_eq!(counts.sections.load(Ordering::SeqCst), 1);
        assert_eq!(counts.plan_limits.load(Ordering::SeqCst), 1);
        assert_eq!(counts.tasks.load(Ordering::SeqCst), 2);

        write_todoist_workflow_with_base_url_label_and_api_key(
            &workflow_path,
            dir.path(),
            &server.base_url,
            "runtime-owned-v2",
            "token",
        );
        workflow_store.reload();

        super::run_tick(&workflow_store, &tx, &mut state, &updates_tx, &mut version).await;

        assert_eq!(counts.project.load(Ordering::SeqCst), 1);
        assert_eq!(counts.sections.load(Ordering::SeqCst), 1);
        assert_eq!(counts.plan_limits.load(Ordering::SeqCst), 1);
        assert_eq!(counts.tasks.load(Ordering::SeqCst), 3);
        assert_eq!(
            state.validated_startup_config.as_ref(),
            Some(&workflow_store.effective().config)
        );
        assert_eq!(
            workflow_store.effective().config.tracker.label.as_deref(),
            Some("runtime-owned-v2")
        );
    }

    #[tokio::test]
    async fn workflow_reload_refetches_tracker_metadata_when_cache_key_changes() {
        let counts = Arc::new(TodoistTickCounts::default());
        let server = spawn_counting_todoist_tick_server(counts.clone()).await;
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        write_todoist_workflow_with_base_url_label_and_api_key(
            &workflow_path,
            dir.path(),
            &server.base_url,
            "runtime-owned",
            "token-a",
        );
        let workflow_store = WorkflowStore::new(workflow_path.clone()).expect("workflow store");
        let (tx, _rx) = tokio::sync::mpsc::channel(4);
        let (updates_tx, _updates_rx) = watch::channel(0u64);
        let mut state = State::default();
        let mut version = 0u64;

        super::run_tick(&workflow_store, &tx, &mut state, &updates_tx, &mut version).await;
        super::run_tick(&workflow_store, &tx, &mut state, &updates_tx, &mut version).await;
        assert_eq!(counts.project.load(Ordering::SeqCst), 1);
        assert_eq!(counts.sections.load(Ordering::SeqCst), 1);
        assert_eq!(counts.plan_limits.load(Ordering::SeqCst), 1);
        assert_eq!(counts.tasks.load(Ordering::SeqCst), 2);

        write_todoist_workflow_with_base_url_label_and_api_key(
            &workflow_path,
            dir.path(),
            &server.base_url,
            "runtime-owned",
            "token-b",
        );
        workflow_store.reload();

        super::run_tick(&workflow_store, &tx, &mut state, &updates_tx, &mut version).await;

        assert_eq!(counts.project.load(Ordering::SeqCst), 2);
        assert_eq!(counts.sections.load(Ordering::SeqCst), 2);
        assert_eq!(counts.plan_limits.load(Ordering::SeqCst), 2);
        assert_eq!(counts.tasks.load(Ordering::SeqCst), 3);
        assert_eq!(
            state.validated_startup_config.as_ref(),
            Some(&workflow_store.effective().config)
        );
        assert_eq!(
            workflow_store.effective().config.tracker.label.as_deref(),
            Some("runtime-owned")
        );
    }

    struct StaticTracker {
        candidates: Vec<Issue>,
        by_id: Vec<Issue>,
    }

    struct HangingOpenIssuesTracker {
        started: Mutex<Option<oneshot::Sender<()>>>,
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
    }

    #[async_trait::async_trait]
    impl crate::tracker::TrackerClient for HangingOpenIssuesTracker {
        async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, crate::tracker::TrackerError> {
            Ok(Vec::new())
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
            Ok(Vec::new())
        }

        async fn fetch_open_issues(&self) -> Result<Vec<Issue>, crate::tracker::TrackerError> {
            if let Some(tx) = self.started.lock().expect("mutex poisoned").take() {
                let _ = tx.send(());
            }
            std::future::pending::<Result<Vec<Issue>, crate::tracker::TrackerError>>().await
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
        sample_workflow_store_with_worker_hosts(root, &[])
    }

    fn sample_workflow_store_with_worker_hosts(
        root: &std::path::Path,
        worker_hosts: &[&str],
    ) -> WorkflowStore {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("WORKFLOW.md");
        let worker_yaml = if worker_hosts.is_empty() {
            String::new()
        } else {
            format!(
                "worker:\n  ssh_hosts:\n{}\n  max_concurrent_agents_per_host: 2\n",
                worker_hosts
                    .iter()
                    .map(|host| format!("    - {host}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        fs::write(
            &path,
            format!(
                r#"---
tracker:
  kind: todoist
  api_key: token
  project_id: proj
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Cancelled
workspace:
  root: {}
{}
---

test
"#,
                root.display(),
                worker_yaml
            ),
        )
        .expect("write workflow");
        let persisted = temp_workflow_path(&path);
        WorkflowStore::new(persisted).expect("workflow store")
    }

    fn todoist_workflow_store_with_base_url(
        root: &std::path::Path,
        base_url: &str,
    ) -> WorkflowStore {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("WORKFLOW.md");
        write_todoist_workflow_with_base_url_label_and_api_key(
            &path,
            root,
            base_url,
            "runtime-owned",
            "token",
        );
        let persisted = temp_workflow_path(&path);
        WorkflowStore::new(persisted).expect("workflow store")
    }

    fn write_todoist_workflow_with_base_url_label_and_api_key(
        path: &std::path::Path,
        root: &std::path::Path,
        base_url: &str,
        label: &str,
        api_key: &str,
    ) {
        fs::write(
            path,
            format!(
                r#"---
tracker:
  kind: todoist
  base_url: {base_url}
  api_key: {api_key}
  project_id: proj
  label: {label}
  active_states:
    - Todo
    - In Progress
workspace:
  root: {}
---

test
"#,
                root.display()
            ),
        )
        .expect("write workflow");
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
