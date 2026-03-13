use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::mpsc,
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::{
    config::ServiceConfig,
    dynamic_tool,
    issue::Issue,
    logging,
    tracker::TrackerClient,
    workspace::{
        remote_shell_assign, ssh_args, ssh_executable, validate_workspace_path_for_worker,
    },
};

const NON_INTERACTIVE_ANSWER: &str =
    "This is a non-interactive session. Operator input is unavailable.";
const APP_SERVER_STOP_TIMEOUT_SECS: u64 = 5;

#[derive(Clone, Debug)]
pub struct CodexEvent {
    pub event: String,
    pub timestamp: DateTime<Utc>,
    pub worker_host: Option<String>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<u32>,
    pub usage: Option<Value>,
    pub rate_limits: Option<Value>,
    pub payload: Option<Value>,
    pub raw: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Error)]
pub enum CodexError {
    #[error("codex_not_found {0}")]
    CodexNotFound(String),
    #[error("invalid_workspace_cwd {0}")]
    InvalidWorkspaceCwd(String),
    #[error("io_error {0}")]
    Io(String),
    #[error("response_timeout")]
    ResponseTimeout,
    #[error("turn_timeout")]
    TurnTimeout,
    #[error("port_exit {0}")]
    PortExit(i32),
    #[error("response_error {0}")]
    ResponseError(String),
    #[error("turn_failed {0}")]
    TurnFailed(String),
    #[error("turn_cancelled {0}")]
    TurnCancelled(String),
    #[error("turn_input_required {0}")]
    TurnInputRequired(String),
    #[error("approval_required {0}")]
    ApprovalRequired(String),
}

pub struct AppServerClient {
    config: ServiceConfig,
    tracker: Arc<dyn TrackerClient>,
}

pub struct AppServerSession {
    config: ServiceConfig,
    tracker: Arc<dyn TrackerClient>,
    workspace: PathBuf,
    worker_host: Option<String>,
    child: Child,
    stdin: ChildStdin,
    stdout_rx: mpsc::UnboundedReceiver<RawLine>,
    thread_id: String,
    dynamic_tool_payloads: Vec<dynamic_tool::ToolPayloadMetric>,
    next_request_id: u64,
    codex_app_server_pid: Option<u32>,
}

pub struct TurnSession {
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: String,
}

struct TurnContext<'a> {
    pid: Option<u32>,
    worker_host: Option<String>,
    issue_id: &'a str,
    issue_identifier: &'a str,
    session_id: &'a str,
    thread_id: String,
    turn_id: &'a str,
    workspace: String,
    raw: &'a str,
    cancellation: &'a CancellationToken,
}

struct ToolInputReply {
    answers: Value,
    event: &'static str,
    message: String,
}

#[derive(Clone, Default)]
struct EventIdentity {
    pid: Option<u32>,
    worker_host: Option<String>,
    session_id: Option<String>,
    thread_id: Option<String>,
    turn_id: Option<String>,
}

impl EventIdentity {
    fn new(pid: Option<u32>, worker_host: Option<String>) -> Self {
        Self {
            pid,
            worker_host,
            ..Self::default()
        }
    }

    fn with_turn(mut self, session_id: String, thread_id: String, turn_id: String) -> Self {
        self.session_id = Some(session_id);
        self.thread_id = Some(thread_id);
        self.turn_id = Some(turn_id);
        self
    }
}

#[derive(Debug)]
enum RawLine {
    Stdout(String),
    StdoutMalformed(String),
    StdoutClosed,
}

impl AppServerClient {
    pub fn new(config: ServiceConfig, tracker: Arc<dyn TrackerClient>) -> Self {
        Self { config, tracker }
    }

    pub async fn start_session(&self, workspace: &Path) -> Result<AppServerSession, CodexError> {
        self.start_session_on_host(workspace, None).await
    }

    pub async fn start_session_on_host(
        &self,
        workspace: &Path,
        worker_host: Option<&str>,
    ) -> Result<AppServerSession, CodexError> {
        validate_workspace_path_for_worker(&self.config, workspace, worker_host)
            .map_err(|error| CodexError::InvalidWorkspaceCwd(error.to_string()))?;

        let workspace = workspace.to_path_buf();
        let worker_host = worker_host.map(ToOwned::to_owned);
        let mut child = spawn_app_server_process(&self.config, &workspace, worker_host.as_deref())?;

        let codex_app_server_pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CodexError::Io("child stdin unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CodexError::Io("child stdout unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| CodexError::Io("child stderr unavailable".to_string()))?;

        let stdout_rx = spawn_stdout_task(stdout);
        spawn_stderr_task(stderr, codex_app_server_pid, worker_host.clone());

        let mut session = AppServerSession {
            config: self.config.clone(),
            tracker: Arc::clone(&self.tracker),
            workspace,
            worker_host,
            child,
            stdin,
            stdout_rx,
            thread_id: String::new(),
            dynamic_tool_payloads: Vec::new(),
            next_request_id: 10,
            codex_app_server_pid,
        };

        session.initialize().await?;
        session.start_thread().await?;
        Ok(session)
    }
}

impl AppServerSession {
    fn event_identity(&self) -> EventIdentity {
        EventIdentity::new(self.codex_app_server_pid, self.worker_host.clone())
    }

    pub async fn run_turn<F>(
        &mut self,
        prompt: &str,
        issue: &Issue,
        cancellation: &CancellationToken,
        mut on_event: F,
    ) -> Result<TurnSession, CodexError>
    where
        F: FnMut(CodexEvent),
    {
        info!(
            "codex_turn=status=starting {} worker_host={} workspace={} thread_id={} pid={}",
            logging::issue_context(issue),
            logging::worker_host_for_log(self.worker_host.as_deref()),
            self.workspace.display(),
            self.thread_id,
            self.codex_app_server_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "n/a".to_string()),
        );
        let request_id = self.take_request_id();
        let payload = json!({
            "id": request_id,
            "method": "turn/start",
            "params": {
                "threadId": self.thread_id,
                "input": [
                    {
                        "type": "text",
                        "text": prompt
                    }
                ],
                "cwd": self.workspace,
                "title": format!("{}: {}", issue.identifier, issue.title),
                "approvalPolicy": self.config.codex.approval_policy,
                "sandboxPolicy": turn_sandbox_policy_for_workspace(&self.config, &self.workspace)
            }
        });
        self.send_json(&payload).await?;
        let response = self
            .await_response(request_id, None)
            .await
            .inspect_err(|error| {
                warn!(
                    "codex_turn=status=startup_failed {} worker_host={} workspace={} thread_id={} error={}",
                    logging::issue_context(issue),
                    logging::worker_host_for_log(self.worker_host.as_deref()),
                    self.workspace.display(),
                    self.thread_id,
                    error,
                );
                on_event(event_with(
                    "startup_failed",
                    self.event_identity(),
                    Some(error.to_string()),
                    None,
                ));
            })?;
        let turn_id = response
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| CodexError::ResponseError(response.to_string()))?
            .to_string();
        let session_id = format!("{}-{turn_id}", self.thread_id);
        let turn_identity = self.event_identity().with_turn(
            session_id.clone(),
            self.thread_id.clone(),
            turn_id.clone(),
        );
        info!(
            "codex_turn=status=started {} worker_host={} workspace={} pid={}",
            logging::codex_session_context(
                &issue.id,
                &issue.identifier,
                &session_id,
                &self.thread_id,
                &turn_id,
            ),
            logging::worker_host_for_log(self.worker_host.as_deref()),
            self.workspace.display(),
            self.codex_app_server_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "n/a".to_string()),
        );
        on_event(event_with(
            "session_started",
            turn_identity.clone(),
            None,
            None,
        ));

        let deadline = Instant::now() + Duration::from_millis(self.config.codex.turn_timeout_ms);
        loop {
            if cancellation.is_cancelled() {
                return Err(CodexError::TurnCancelled(
                    "cancelled by orchestrator".to_string(),
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CodexError::TurnTimeout);
            }
            let line = tokio::select! {
                _ = cancellation.cancelled() => {
                    return Err(CodexError::TurnCancelled(
                        "cancelled by orchestrator".to_string(),
                    ));
                }
                line = timeout(remaining, self.stdout_rx.recv()) => {
                    line.map_err(|_| CodexError::TurnTimeout)?
                }
            };
            match line {
                Some(RawLine::Stdout(raw)) => match serde_json::from_str::<Value>(&raw) {
                    Ok(message) => {
                        if let Some(method) = message.get("method").and_then(Value::as_str) {
                            let turn = TurnContext {
                                pid: self.codex_app_server_pid,
                                worker_host: self.worker_host.clone(),
                                issue_id: &issue.id,
                                issue_identifier: &issue.identifier,
                                session_id: &session_id,
                                thread_id: self.thread_id.clone(),
                                turn_id: &turn_id,
                                workspace: self.workspace.display().to_string(),
                                raw: &raw,
                                cancellation,
                            };
                            if let Some(error) = self
                                .handle_turn_message(method, &message, &turn, &mut on_event)
                                .await?
                            {
                                return Err(error);
                            }

                            if method == "turn/completed" {
                                return Ok(TurnSession {
                                    session_id,
                                    thread_id: self.thread_id.clone(),
                                    turn_id,
                                });
                            }
                        }
                    }
                    Err(_) => {
                        on_event(event_with(
                            "malformed",
                            turn_identity.clone(),
                            Some(raw.clone()),
                            Some(raw),
                        ));
                    }
                },
                Some(RawLine::StdoutMalformed(raw)) => {
                    on_event(event_with(
                        "malformed",
                        turn_identity.clone(),
                        Some(raw.clone()),
                        Some(raw),
                    ));
                }
                Some(RawLine::StdoutClosed) | None => return Err(self.port_exit_error().await),
            }
        }
    }

    pub async fn stop(&mut self) {
        let pid = self.child.id().or(self.codex_app_server_pid);
        let _ = self.child.start_kill();
        match timeout(
            Duration::from_secs(APP_SERVER_STOP_TIMEOUT_SECS),
            self.child.wait(),
        )
        .await
        {
            Ok(_) => {}
            Err(_) => {
                warn!(
                    "app_server_shutdown=status=timeout worker_host={} workspace={} pid={pid:?}",
                    logging::worker_host_for_log(self.worker_host.as_deref()),
                    self.workspace.display(),
                );
                let _ = self.child.kill().await;
                let _ = timeout(Duration::from_secs(1), self.child.wait()).await;
            }
        }
    }

    async fn initialize(&mut self) -> Result<(), CodexError> {
        let payload = json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "symphony-rust",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }
        });
        self.send_json(&payload).await?;
        self.await_response(1, None).await?;
        self.send_json(&json!({
            "method": "initialized",
            "params": {}
        }))
        .await
    }

    async fn start_thread(&mut self) -> Result<(), CodexError> {
        let request_id = 2;
        let dynamic_tools =
            dynamic_tool::tool_specs_with_tracker(&self.config, self.tracker.as_ref()).await;
        let dynamic_tool_payloads = dynamic_tool::tool_payload_metrics(&dynamic_tools);
        let payload = json!({
            "id": request_id,
            "method": "thread/start",
            "params": {
                "approvalPolicy": self.config.codex.approval_policy,
                "sandbox": self.config.codex.thread_sandbox,
                "cwd": self.workspace,
                "dynamicTools": dynamic_tools
            }
        });
        self.send_json(&payload).await?;
        let response = self.await_response(request_id, None).await?;
        self.dynamic_tool_payloads = dynamic_tool_payloads;
        self.thread_id = response
            .get("thread")
            .and_then(|thread| thread.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| CodexError::ResponseError(response.to_string()))?
            .to_string();
        info!(
            "codex_thread=status=started thread_id={} worker_host={} workspace={} pid={} dynamic_tools={} dynamic_tool_bytes={}",
            self.thread_id,
            logging::worker_host_for_log(self.worker_host.as_deref()),
            self.workspace.display(),
            self.codex_app_server_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "n/a".to_string()),
            dynamic_tools.len(),
            dynamic_tool::total_tool_payload_bytes(&self.dynamic_tool_payloads),
        );
        Ok(())
    }

    pub fn dynamic_tool_payloads(&self) -> &[dynamic_tool::ToolPayloadMetric] {
        &self.dynamic_tool_payloads
    }

    async fn handle_turn_message<F>(
        &mut self,
        method: &str,
        message: &Value,
        turn: &TurnContext<'_>,
        on_event: &mut F,
    ) -> Result<Option<CodexError>, CodexError>
    where
        F: FnMut(CodexEvent),
    {
        let usage = extract_usage(message);
        let rate_limits = extract_rate_limits(message);

        if method == "turn/completed" {
            info!(
                "codex_turn=status=completed {} worker_host={} workspace={} pid={}",
                turn_log_context(turn),
                logging::worker_host_for_log(turn.worker_host.as_deref()),
                turn.workspace,
                turn.pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
            );
            on_event(event(
                turn,
                "turn_completed",
                usage,
                rate_limits,
                message.clone(),
                Some(turn.raw.to_string()),
            ));
            return Ok(None);
        }

        if method == "turn/failed" {
            warn!(
                "codex_turn=status=failed {} worker_host={} workspace={} pid={} error={}",
                turn_log_context(turn),
                logging::worker_host_for_log(turn.worker_host.as_deref()),
                turn.workspace,
                turn.pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
                inline_log_value(&message.to_string()),
            );
            on_event(event(
                turn,
                "turn_failed",
                usage,
                rate_limits,
                message.clone(),
                Some(turn.raw.to_string()),
            ));
            return Ok(Some(CodexError::TurnFailed(message.to_string())));
        }

        if method == "turn/cancelled" {
            warn!(
                "codex_turn=status=cancelled {} worker_host={} workspace={} pid={}",
                turn_log_context(turn),
                logging::worker_host_for_log(turn.worker_host.as_deref()),
                turn.workspace,
                turn.pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
            );
            on_event(event(
                turn,
                "turn_cancelled",
                usage,
                rate_limits,
                message.clone(),
                Some(turn.raw.to_string()),
            ));
            return Ok(Some(CodexError::TurnCancelled(message.to_string())));
        }

        if message.get("id").is_some()
            && message.get("params").is_some()
            && let Some(result) = self.handle_server_request(message, turn, on_event).await?
        {
            return Ok(Some(result));
        }

        if input_required_method(method, message) {
            on_event(event(
                turn,
                "turn_input_required",
                usage,
                rate_limits,
                message.clone(),
                Some(turn.raw.to_string()),
            ));
            turn.cancellation.cancel();
            return Ok(Some(CodexError::TurnInputRequired(method.to_string())));
        }

        on_event(event(
            turn,
            "notification",
            usage,
            rate_limits,
            message.clone(),
            Some(turn.raw.to_string()),
        ));
        Ok(None)
    }

    async fn handle_server_request<F>(
        &mut self,
        message: &Value,
        turn: &TurnContext<'_>,
        on_event: &mut F,
    ) -> Result<Option<CodexError>, CodexError>
    where
        F: FnMut(CodexEvent),
    {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| CodexError::ResponseError(message.to_string()))?;
        let request_id = message
            .get("id")
            .cloned()
            .ok_or_else(|| CodexError::ResponseError(message.to_string()))?;

        match method {
            "item/commandExecution/requestApproval" => {
                self.handle_approval_request(
                    message,
                    turn,
                    on_event,
                    request_id,
                    "acceptForSession",
                    method,
                )
                .await
            }
            "item/fileChange/requestApproval" => {
                self.handle_approval_request(
                    message,
                    turn,
                    on_event,
                    request_id,
                    "acceptForSession",
                    method,
                )
                .await
            }
            "execCommandApproval" => {
                self.handle_approval_request(
                    message,
                    turn,
                    on_event,
                    request_id,
                    "approved_for_session",
                    method,
                )
                .await
            }
            "applyPatchApproval" => {
                self.handle_approval_request(
                    message,
                    turn,
                    on_event,
                    request_id,
                    "approved_for_session",
                    method,
                )
                .await
            }
            "item/tool/call" => {
                let params = message.get("params").cloned().unwrap_or_default();
                let tool = tool_call_name(&params).unwrap_or_default();
                let arguments = tool_call_arguments(&params);
                let result =
                    dynamic_tool::execute(&self.config, self.tracker.as_ref(), &tool, arguments)
                        .await;
                let event_payload = tool_event_payload(message, &result);
                self.send_json(&json!({
                    "id": request_id,
                    "result": result
                }))
                .await?;
                let event_name = if result
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    "tool_call_completed"
                } else if matches!(
                    tool.as_str(),
                    dynamic_tool::TODOIST_TOOL | dynamic_tool::GITHUB_API_TOOL
                ) {
                    "tool_call_failed"
                } else {
                    "unsupported_tool_call"
                };
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or("n/a");
                let tool_status = if event_name == "tool_call_completed" {
                    "completed"
                } else {
                    "failed"
                };
                let success = result
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let error_summary = result
                    .get("error")
                    .map(|error| inline_log_value(&error.to_string()))
                    .unwrap_or_else(|| "none".to_string());
                info!(
                    "dynamic_tool_call status={} {} tool={} call_id={} success={} error={}",
                    tool_status,
                    turn_log_context(turn),
                    tool,
                    call_id,
                    success,
                    error_summary,
                );
                on_event(event(turn, event_name, None, None, event_payload, None));
                Ok(None)
            }
            "item/tool/requestUserInput" => {
                let auto_approve = auto_approve_requests(&self.config.codex.approval_policy);
                if let Some(reply) = build_tool_input_reply(message.get("params"), auto_approve) {
                    self.send_json(&json!({
                        "id": request_id,
                        "result": { "answers": reply.answers }
                    }))
                    .await?;
                    let mut response_event = event(
                        turn,
                        reply.event,
                        extract_usage(message),
                        extract_rate_limits(message),
                        message.clone(),
                        None,
                    );
                    response_event.message = Some(reply.message);
                    on_event(response_event);
                    Ok(None)
                } else {
                    on_event(event(
                        turn,
                        "turn_input_required",
                        None,
                        None,
                        message.clone(),
                        None,
                    ));
                    Ok(Some(CodexError::TurnInputRequired(method.to_string())))
                }
            }
            "item/permissions/requestApproval" => {
                self.send_json(&json!({
                    "id": request_id,
                    "result": { "permissions": {} }
                }))
                .await?;
                on_event(event(
                    turn,
                    "approval_required",
                    None,
                    None,
                    message.clone(),
                    None,
                ));
                Ok(Some(CodexError::ApprovalRequired(method.to_string())))
            }
            "mcpServer/elicitation/request" => {
                self.send_json(&json!({
                    "id": request_id,
                    "result": { "action": "decline" }
                }))
                .await?;
                on_event(event(
                    turn,
                    "turn_input_required",
                    None,
                    None,
                    message.clone(),
                    None,
                ));
                Ok(Some(CodexError::TurnInputRequired(method.to_string())))
            }
            _ => Ok(None),
        }
    }

    async fn handle_approval_request<F>(
        &mut self,
        message: &Value,
        turn: &TurnContext<'_>,
        on_event: &mut F,
        request_id: Value,
        decision: &str,
        method: &str,
    ) -> Result<Option<CodexError>, CodexError>
    where
        F: FnMut(CodexEvent),
    {
        if auto_approve_requests(&self.config.codex.approval_policy) {
            self.send_json(&json!({
                "id": request_id,
                "result": { "decision": decision }
            }))
            .await?;
            on_event(event(
                turn,
                "approval_auto_approved",
                extract_usage(message),
                extract_rate_limits(message),
                message.clone(),
                None,
            ));
            Ok(None)
        } else {
            on_event(event(
                turn,
                "approval_required",
                extract_usage(message),
                extract_rate_limits(message),
                message.clone(),
                None,
            ));
            Ok(Some(CodexError::ApprovalRequired(method.to_string())))
        }
    }

    async fn await_response(
        &mut self,
        request_id: u64,
        deadline: Option<Instant>,
    ) -> Result<Value, CodexError> {
        loop {
            let timeout_duration = deadline
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|| Duration::from_millis(self.config.codex.read_timeout_ms));
            if timeout_duration.is_zero() {
                return Err(CodexError::ResponseTimeout);
            }
            let next = timeout(timeout_duration, self.stdout_rx.recv())
                .await
                .map_err(|_| CodexError::ResponseTimeout)?;
            match next {
                Some(RawLine::Stdout(raw)) => match serde_json::from_str::<Value>(&raw) {
                    Ok(message) => {
                        if message.get("id").and_then(Value::as_u64) == Some(request_id) {
                            if let Some(result) = message.get("result") {
                                return Ok(result.clone());
                            }
                            if let Some(error) = message.get("error") {
                                return Err(CodexError::ResponseError(error.to_string()));
                            }
                            return Err(CodexError::ResponseError(message.to_string()));
                        }
                    }
                    Err(_) => {
                        debug!("codex_stream=response status=ignored raw={raw}");
                    }
                },
                Some(RawLine::StdoutMalformed(raw)) => {
                    debug!("codex_stream=response status=malformed raw={raw}");
                }
                Some(RawLine::StdoutClosed) | None => return Err(self.port_exit_error().await),
            }
        }
    }

    async fn send_json(&mut self, payload: &Value) -> Result<(), CodexError> {
        let encoded =
            serde_json::to_vec(payload).map_err(|error| CodexError::Io(error.to_string()))?;
        self.stdin
            .write_all(&encoded)
            .await
            .map_err(|error| CodexError::Io(error.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|error| CodexError::Io(error.to_string()))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| CodexError::Io(error.to_string()))
    }

    fn take_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    async fn port_exit_error(&mut self) -> CodexError {
        match self.child.wait().await {
            Ok(status) => CodexError::PortExit(status.code().unwrap_or(-1)),
            Err(error) => CodexError::Io(error.to_string()),
        }
    }
}

impl Drop for AppServerSession {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn spawn_app_server_process(
    config: &ServiceConfig,
    workspace: &Path,
    worker_host: Option<&str>,
) -> Result<Child, CodexError> {
    if let Some(worker_host) = worker_host {
        let executable = ssh_executable().map_err(|error| CodexError::Io(error.to_string()))?;
        let remote_command = format!(
            "{}\ncd \"$workspace\" && exec {}",
            remote_shell_assign("workspace", &workspace.to_string_lossy()),
            config.codex.command
        );
        let mut child = Command::new(executable);
        child
            .args(ssh_args(worker_host, &remote_command))
            .env_remove("TODOIST_API_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child.spawn().map_err(spawn_error)
    } else {
        let shell =
            app_server_shell().ok_or_else(|| CodexError::CodexNotFound("no_shell_found".into()))?;
        Command::new(shell)
            .arg("-lc")
            .arg(&config.codex.command)
            .current_dir(workspace)
            .env_remove("TODOIST_API_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn_error)
    }
}

fn spawn_error(error: std::io::Error) -> CodexError {
    if error.kind() == std::io::ErrorKind::NotFound {
        CodexError::CodexNotFound(error.to_string())
    } else {
        CodexError::Io(error.to_string())
    }
}

fn turn_sandbox_policy_for_workspace(config: &ServiceConfig, workspace: &Path) -> Value {
    let mut policy = config.codex.turn_sandbox_policy.clone();
    if let Some(policy_map) = policy.as_object_mut()
        && policy_map.contains_key("writableRoots")
    {
        policy_map.insert(
            "writableRoots".to_string(),
            json!([workspace.to_string_lossy().to_string()]),
        );
    }
    policy
}

fn spawn_stdout_task(stdout: ChildStdout) -> mpsc::UnboundedReceiver<RawLine> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut buffer = Vec::new();
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) => {
                    let _ = tx.send(RawLine::StdoutClosed);
                    return;
                }
                Ok(_) => {
                    while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                        buffer.pop();
                    }
                    match String::from_utf8(buffer) {
                        Ok(line) => {
                            let _ = tx.send(RawLine::Stdout(line));
                        }
                        Err(error) => {
                            let _ = tx.send(RawLine::StdoutMalformed(error.to_string()));
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(RawLine::StdoutMalformed(error.to_string()));
                    let _ = tx.send(RawLine::StdoutClosed);
                    return;
                }
            }
        }
    });
    rx
}

fn spawn_stderr_task(stderr: ChildStderr, pid: Option<u32>, worker_host: Option<String>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let lowered = line.to_ascii_lowercase();
            if lowered.contains("error")
                || lowered.contains("warn")
                || lowered.contains("failed")
                || lowered.contains("panic")
            {
                warn!(
                    "codex_stream=stderr status=warning pid={} worker_host={} line={}",
                    pid.map(|value| value.to_string())
                        .unwrap_or_else(|| "n/a".to_string()),
                    logging::worker_host_for_log(worker_host.as_deref()),
                    line,
                );
            } else {
                debug!(
                    "codex_stream=stderr status=debug pid={} worker_host={} line={}",
                    pid.map(|value| value.to_string())
                        .unwrap_or_else(|| "n/a".to_string()),
                    logging::worker_host_for_log(worker_host.as_deref()),
                    line,
                );
            }
        }
    });
}

fn turn_log_context(turn: &TurnContext<'_>) -> String {
    logging::codex_session_context(
        turn.issue_id,
        turn.issue_identifier,
        turn.session_id,
        &turn.thread_id,
        turn.turn_id,
    )
}

fn inline_log_value(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn app_server_shell() -> Option<&'static str> {
    if Path::new("/bin/bash").exists() {
        Some("/bin/bash")
    } else if Path::new("/bin/sh").exists() {
        Some("/bin/sh")
    } else {
        None
    }
}

fn auto_approve_requests(approval_policy: &Value) -> bool {
    approval_policy.as_str() == Some("never")
}

fn build_tool_input_reply(params: Option<&Value>, auto_approve: bool) -> Option<ToolInputReply> {
    if auto_approve && let Some((answers, decision)) = build_approval_answers(params) {
        return Some(ToolInputReply {
            answers,
            event: "approval_auto_approved",
            message: decision,
        });
    }

    build_unavailable_answers(params).map(|answers| ToolInputReply {
        answers,
        event: "tool_input_auto_answered",
        message: NON_INTERACTIVE_ANSWER.to_string(),
    })
}

fn build_approval_answers(params: Option<&Value>) -> Option<(Value, String)> {
    let questions = params
        .and_then(|params| params.get("questions"))
        .and_then(Value::as_array)?;
    let mut answers = serde_json::Map::new();
    let mut last_label = None;
    for question in questions {
        let question_id = question.get("id").and_then(Value::as_str)?;
        let options = question.get("options").and_then(Value::as_array)?;
        let label = approval_option_label(options)?;
        last_label = Some(label.clone());
        answers.insert(question_id.to_string(), json!({ "answers": [label] }));
    }
    match (answers.is_empty(), last_label) {
        (false, Some(label)) => Some((Value::Object(answers), label)),
        _ => None,
    }
}

fn build_unavailable_answers(params: Option<&Value>) -> Option<Value> {
    let questions = params
        .and_then(|params| params.get("questions"))
        .and_then(Value::as_array)?;
    let mut answers = serde_json::Map::new();
    for question in questions {
        let question_id = question.get("id").and_then(Value::as_str)?;
        answers.insert(
            question_id.to_string(),
            json!({
                "answers": [NON_INTERACTIVE_ANSWER]
            }),
        );
    }
    (!answers.is_empty()).then_some(Value::Object(answers))
}

fn approval_option_label(options: &[Value]) -> Option<String> {
    let labels: Vec<&str> = options
        .iter()
        .filter_map(|option| option.get("label").and_then(Value::as_str))
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .collect();

    labels
        .iter()
        .find(|label| **label == "Approve this Session")
        .or_else(|| labels.iter().find(|label| **label == "Approve Once"))
        .or_else(|| {
            labels.iter().find(|label| {
                let normalized = label.to_ascii_lowercase();
                normalized.starts_with("approve") || normalized.starts_with("allow")
            })
        })
        .map(|label| (*label).to_string())
}

fn tool_call_name(params: &Value) -> Option<String> {
    params
        .as_object()
        .and_then(|params| {
            params
                .get("tool")
                .or_else(|| params.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn tool_call_arguments(params: &Value) -> Value {
    params
        .as_object()
        .and_then(|params| params.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn tool_event_payload(message: &Value, result: &Value) -> Value {
    let mut payload = message.as_object().cloned().unwrap_or_default();
    payload.insert("toolResult".to_string(), result.clone());
    Value::Object(payload)
}

fn input_required_method(method: &str, message: &Value) -> bool {
    matches!(
        method,
        "turn/input_required"
            | "turn/needs_input"
            | "turn/need_input"
            | "turn/request_input"
            | "turn/request_response"
            | "turn/provide_input"
            | "turn/approval_required"
    ) || message
        .get("params")
        .and_then(Value::as_object)
        .and_then(|params| params.get("needsInput"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn event(
    turn: &TurnContext<'_>,
    event: &str,
    usage: Option<Value>,
    rate_limits: Option<Value>,
    payload: Value,
    raw: Option<String>,
) -> CodexEvent {
    CodexEvent {
        event: event.to_string(),
        timestamp: Utc::now(),
        worker_host: turn.worker_host.clone(),
        session_id: Some(turn.session_id.to_string()),
        thread_id: Some(turn.thread_id.clone()),
        turn_id: Some(turn.turn_id.to_string()),
        codex_app_server_pid: turn.pid,
        usage,
        rate_limits,
        payload: Some(payload),
        raw,
        message: None,
    }
}

fn event_with(
    event: &str,
    identity: EventIdentity,
    message: Option<String>,
    raw: Option<String>,
) -> CodexEvent {
    CodexEvent {
        event: event.to_string(),
        timestamp: Utc::now(),
        worker_host: identity.worker_host,
        session_id: identity.session_id,
        thread_id: identity.thread_id,
        turn_id: identity.turn_id,
        codex_app_server_pid: identity.pid,
        usage: None,
        rate_limits: None,
        payload: None,
        raw,
        message,
    }
}

fn extract_usage(message: &Value) -> Option<Value> {
    absolute_usage(message).or_else(|| turn_completed_usage(message))
}

fn absolute_usage(message: &Value) -> Option<Value> {
    [
        &["params", "msg", "payload", "info", "total_token_usage"][..],
        &["params", "msg", "info", "total_token_usage"][..],
        &["params", "tokenUsage", "total"][..],
        &["tokenUsage", "total"][..],
    ]
    .iter()
    .find_map(|path| lookup_path(message, path))
    .map(normalize_usage_map)
}

fn turn_completed_usage(message: &Value) -> Option<Value> {
    if message.get("method").and_then(Value::as_str) != Some("turn/completed") {
        return None;
    }

    [&["params", "usage"][..], &["usage"][..]]
        .iter()
        .find_map(|path| lookup_path(message, path))
        .map(normalize_usage_map)
}

fn extract_rate_limits(message: &Value) -> Option<Value> {
    rate_limits_from_payload(message)
}

fn normalize_usage_map(value: &Value) -> Value {
    json!({
        "input_tokens": extract_integer(value, &["inputTokens", "input_tokens"]).unwrap_or(0),
        "output_tokens": extract_integer(value, &["outputTokens", "output_tokens"]).unwrap_or(0),
        "total_tokens": extract_integer(value, &["totalTokens", "total_tokens"]).unwrap_or(0),
    })
}

fn extract_integer(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().map(|value| value as i64))
        })
}

fn lookup_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, segment| current.get(*segment))
}

fn rate_limits_from_payload(payload: &Value) -> Option<Value> {
    match payload {
        Value::Object(map) => {
            let direct = map
                .get("rateLimits")
                .or_else(|| map.get("rate_limits"))
                .cloned();

            if direct.as_ref().is_some_and(rate_limits_map) {
                return direct;
            }
            if rate_limits_map(payload) {
                return Some(payload.clone());
            }

            map.values().find_map(rate_limits_from_payload)
        }
        Value::Array(values) => values.iter().find_map(rate_limits_from_payload),
        _ => None,
    }
}

fn rate_limits_map(payload: &Value) -> bool {
    let Some(map) = payload.as_object() else {
        return false;
    };

    ["primary", "secondary", "credits"]
        .iter()
        .any(|key| map.contains_key(*key))
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use crate::{
        config::ServiceConfig,
        issue::Issue,
        tracker::{TrackerClient, TrackerError},
    };

    use super::{AppServerClient, CodexError, CodexEvent, extract_rate_limits, extract_usage};

    struct StubTracker {
        tool_result: Result<Value, TrackerError>,
    }

    #[async_trait]
    impl TrackerClient for StubTracker {
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

        async fn get_current_user(&self) -> Result<Value, TrackerError> {
            self.tool_result.clone()
        }
    }

    #[test]
    fn extracts_absolute_token_usage() {
        let usage = extract_usage(&json!({
            "method": "thread/tokenUsage/updated",
            "params": {
                "tokenUsage": {
                    "total": {
                        "inputTokens": 10,
                        "outputTokens": 20,
                        "totalTokens": 30
                    }
                }
            }
        }))
        .expect("usage");
        assert_eq!(
            usage
                .get("input_tokens")
                .and_then(serde_json::Value::as_i64),
            Some(10)
        );
        assert_eq!(
            usage
                .get("total_tokens")
                .and_then(serde_json::Value::as_i64),
            Some(30)
        );
    }

    #[tokio::test]
    async fn app_server_rejects_workspace_root_and_outside_workspace_root() {
        let dir = tempdir().expect("tempdir");
        let workspace_root = dir.path().join("workspaces");
        let outside_workspace = dir.path().join("outside");
        fs::create_dir_all(&workspace_root).expect("workspace root");
        fs::create_dir_all(&outside_workspace).expect("outside workspace");
        let script = dir.path().join("fake-codex.sh");
        write_executable(&script, "#!/bin/sh\nexit 0\n");

        let config = sample_config(&workspace_root.join("MT-ROOT"), &script, None);
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );

        let root_error = match app.start_session(&workspace_root).await {
            Ok(_) => panic!("expected workspace root to be rejected"),
            Err(error) => error,
        };
        assert!(matches!(root_error, CodexError::InvalidWorkspaceCwd(_)));

        let outside_error = match app.start_session(&outside_workspace).await {
            Ok(_) => panic!("expected outside workspace to be rejected"),
            Err(error) => error,
        };
        assert!(matches!(outside_error, CodexError::InvalidWorkspaceCwd(_)));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn start_session_strips_todoist_token_from_child_environment() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let trace = dir.path().join("token.log");
        let script = dir.path().join("fake-codex.sh");
        write_executable(
            &script,
            &format!(
                r#"#!/bin/sh
trace_file="{}"
printf '%s\n' "${{TODOIST_API_TOKEN:-missing}}" > "$trace_file"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-env"}}}}}}'
      exit 0
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
                trace.display()
            ),
        );

        unsafe {
            std::env::set_var("TODOIST_API_TOKEN", "secret-token");
        }

        let config = sample_config(&workspace, &script, None);
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );

        let mut session = app.start_session(&workspace).await.expect("session");
        session.stop().await;

        let captured = fs::read_to_string(trace).expect("trace");
        assert_eq!(captured.trim(), "missing");

        unsafe {
            std::env::remove_var("TODOIST_API_TOKEN");
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn start_session_on_host_uses_ssh_launch_and_emits_worker_host_metadata() {
        let _guard = crate::runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let remote_workspace = home.join("remote-workspaces").join("MT-REMOTE");
        fs::create_dir_all(&remote_workspace).expect("workspace");
        let trace = dir.path().join("remote-trace.log");
        let ssh = dir.path().join("ssh");
        write_executable(
            &ssh,
            r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  case "$1" in
    -F|-p)
      shift 2
      ;;
    -T)
      shift
      ;;
    *)
      shift
      break
      ;;
  esac
done
exec /bin/sh -lc "$1"
"#,
        );
        let script = dir.path().join("fake-codex.sh");
        write_executable(
            &script,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$(pwd)" > "{}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-remote"}}}}}}'
      ;;
    4)
      printf '%s\n' '{{"id":10,"result":{{"turn":{{"id":"turn-remote"}}}}}}'
      printf '%s\n' '{{"method":"turn/completed"}}'
      exit 0
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
                trace.display()
            ),
        );

        unsafe {
            std::env::set_var("SYMPHONY_SSH_BIN", &ssh);
            std::env::set_var("HOME", &home);
        }

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj"
                },
                "workspace": {
                    "root": "~/remote-workspaces"
                },
                "codex": {
                    "command": script.display().to_string()
                }
            })
            .as_object()
            .expect("config object"),
        )
        .expect("config");
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );

        let mut session = app
            .start_session_on_host(
                Path::new("~/remote-workspaces/MT-REMOTE"),
                Some("builder-1"),
            )
            .await
            .expect("session");
        let events = Arc::new(Mutex::new(Vec::<CodexEvent>::new()));
        let captured = Arc::clone(&events);
        session
            .run_turn(
                "Remote turn",
                &sample_issue(),
                &CancellationToken::new(),
                move |event| captured.lock().expect("events").push(event),
            )
            .await
            .expect("turn");
        session.stop().await;

        assert_eq!(
            fs::read_to_string(&trace).expect("trace").trim(),
            remote_workspace.display().to_string()
        );
        let events = events.lock().expect("events");
        assert!(
            events
                .iter()
                .all(|event| event.worker_host.as_deref() == Some("builder-1"))
        );

        unsafe {
            std::env::remove_var("SYMPHONY_SSH_BIN");
            std::env::remove_var("HOME");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn app_server_rejects_symlink_escape_workspace_paths() {
        let dir = tempdir().expect("tempdir");
        let workspace_root = dir.path().join("workspaces");
        let outside_workspace = dir.path().join("outside");
        let symlink_workspace = workspace_root.join("MT-SYM");
        fs::create_dir_all(&workspace_root).expect("workspace root");
        fs::create_dir_all(&outside_workspace).expect("outside workspace");
        std::os::unix::fs::symlink(&outside_workspace, &symlink_workspace).expect("symlink");
        let script = dir.path().join("fake-codex.sh");
        write_executable(&script, "#!/bin/sh\nexit 0\n");

        let config = sample_config(&workspace_root.join("MT-SYM"), &script, None);
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );

        let error = match app.start_session(&symlink_workspace).await {
            Ok(_) => panic!("expected symlink escape to be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, CodexError::InvalidWorkspaceCwd(_)));
    }

    #[test]
    fn ignores_unscoped_generic_usage_payloads() {
        let usage = extract_usage(&json!({
            "method": "notification",
            "params": {
                "usage": {
                    "inputTokens": 10,
                    "outputTokens": 20,
                    "totalTokens": 30
                }
            }
        }));
        assert!(usage.is_none());
    }

    #[test]
    fn extracts_turn_completed_usage_payload() {
        let usage = extract_usage(&json!({
            "method": "turn/completed",
            "usage": {
                "inputTokens": 5,
                "outputTokens": 7,
                "totalTokens": 12
            }
        }))
        .expect("usage");

        assert_eq!(
            usage
                .get("input_tokens")
                .and_then(serde_json::Value::as_i64),
            Some(5)
        );
        assert_eq!(
            usage
                .get("total_tokens")
                .and_then(serde_json::Value::as_i64),
            Some(12)
        );
    }

    #[test]
    fn extracts_rate_limits_payload() {
        let rate_limits = extract_rate_limits(&json!({
            "method": "account/rateLimits/updated",
            "params": {
                "rateLimits": {
                    "primary": { "usedPercent": 75 }
                }
            }
        }))
        .expect("rate limits");
        assert_eq!(
            rate_limits
                .get("primary")
                .and_then(|value| value.get("usedPercent"))
                .and_then(serde_json::Value::as_i64),
            Some(75)
        );
    }

    #[test]
    fn extracts_nested_rate_limits_payload() {
        let rate_limits = extract_rate_limits(&json!({
            "payload": {
                "wrapper": {
                    "rate_limits": {
                        "limit_id": "requests",
                        "primary": { "usedPercent": 10 }
                    }
                }
            }
        }))
        .expect("rate limits");

        assert_eq!(
            rate_limits
                .get("primary")
                .and_then(|value| value.get("usedPercent"))
                .and_then(serde_json::Value::as_i64),
            Some(10)
        );
    }

    #[tokio::test]
    async fn requires_approval_when_policy_is_not_never() {
        let dir = tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let script = dir.path().join("fake-codex.sh");
        write_executable(
            &script,
            r#"#!/bin/sh
count=0
while IFS= read -r line; do
  count=$((count + 1))
  case "$count" in
    1)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-approval"}}}'
      ;;
    4)
      printf '%s\n' '{"id":10,"result":{"turn":{"id":"turn-approval"}}}'
      printf '%s\n' '{"id":99,"method":"item/commandExecution/requestApproval","params":{"command":"gh pr view","cwd":"/tmp","reason":"need approval"}}'
      ;;
    *)
      sleep 1
      ;;
  esac
done
"#,
        );

        let config = sample_config(&workspace, &script, None);
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );
        let mut session = app.start_session(&workspace).await.expect("session");
        let issue = sample_issue();

        let result = session
            .run_turn(
                "Handle approval request",
                &issue,
                &CancellationToken::new(),
                |_| {},
            )
            .await;

        session.stop().await;
        assert!(matches!(result, Err(CodexError::ApprovalRequired(_))));
    }

    #[tokio::test]
    async fn run_turn_honors_cancellation_while_waiting_for_stream_output() {
        let dir = tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let script = dir.path().join("fake-codex.sh");
        write_executable(
            &script,
            r#"#!/bin/sh
count=0
while IFS= read -r line; do
  count=$((count + 1))
  case "$count" in
    1)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"thread-cancel"}}}'
      ;;
    4)
      printf '%s\n' '{"id":10,"result":{"turn":{"id":"turn-cancel"}}}'
      sleep 30
      ;;
    *)
      sleep 30
      ;;
  esac
done
"#,
        );

        let config = sample_config(&workspace, &script, Some(json!("never")));
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );
        let mut session = app.start_session(&workspace).await.expect("session");
        let cancellation = CancellationToken::new();
        let cancel_clone = cancellation.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            session.run_turn(
                "Wait for cancellation",
                &sample_issue(),
                &cancellation,
                |_| {},
            ),
        )
        .await
        .expect("run_turn should cancel promptly");

        session.stop().await;
        assert!(matches!(result, Err(CodexError::TurnCancelled(_))));
    }

    #[tokio::test]
    async fn auto_approves_command_execution_when_policy_is_never() {
        let dir = tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let trace = dir.path().join("trace.log");
        let script = dir.path().join("fake-codex.sh");
        write_executable(
            &script,
            &format!(
                r#"#!/bin/sh
trace_file="{}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-auto"}}}}}}'
      ;;
    4)
      printf '%s\n' '{{"id":10,"result":{{"turn":{{"id":"turn-auto"}}}}}}'
      printf '%s\n' '{{"id":99,"method":"item/commandExecution/requestApproval","params":{{"command":"gh pr view","cwd":"/tmp","reason":"need approval"}}}}'
      ;;
    5)
      printf '%s\n' '{{"method":"turn/completed"}}'
      exit 0
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
                trace.display()
            ),
        );

        let config = sample_config(&workspace, &script, Some(json!("never")));
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );
        let mut session = app.start_session(&workspace).await.expect("session");

        let result = session
            .run_turn(
                "Handle approval request",
                &sample_issue(),
                &CancellationToken::new(),
                |_| {},
            )
            .await;

        session.stop().await;
        assert!(result.is_ok());

        let trace = fs::read_to_string(trace).expect("trace");
        assert!(trace.contains(r#""id":99,"result":{"decision":"acceptForSession"}"#));
    }

    #[tokio::test]
    async fn auto_answers_tool_input_and_continues() {
        let dir = tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let trace = dir.path().join("trace.log");
        let script = dir.path().join("fake-codex.sh");
        write_executable(
            &script,
            &format!(
                r#"#!/bin/sh
trace_file="{}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-input"}}}}}}'
      ;;
    4)
      printf '%s\n' '{{"id":10,"result":{{"turn":{{"id":"turn-input"}}}}}}'
      printf '%s\n' '{{"id":111,"method":"item/tool/requestUserInput","params":{{"itemId":"call-1","questions":[{{"header":"Provide context","id":"freeform-1","isOther":false,"isSecret":false,"options":null,"question":"What should I post back?"}}],"threadId":"thread-input","turnId":"turn-input"}}}}'
      ;;
    5)
      printf '%s\n' '{{"method":"turn/completed"}}'
      exit 0
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
                trace.display()
            ),
        );

        let config = sample_config(&workspace, &script, None);
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );
        let mut session = app.start_session(&workspace).await.expect("session");
        let events = Arc::new(Mutex::new(Vec::<CodexEvent>::new()));
        let captured = Arc::clone(&events);

        let result = session
            .run_turn(
                "Handle generic tool input",
                &sample_issue(),
                &CancellationToken::new(),
                move |event| captured.lock().expect("events").push(event),
            )
            .await;

        session.stop().await;
        assert!(result.is_ok());

        let events = events.lock().expect("events");
        let answered = events
            .iter()
            .find(|event| event.event == "tool_input_auto_answered")
            .expect("tool input auto answered event");
        assert_eq!(
            answered.message.as_deref(),
            Some(super::NON_INTERACTIVE_ANSWER)
        );

        let trace = fs::read_to_string(trace).expect("trace");
        assert!(trace.contains(super::NON_INTERACTIVE_ANSWER));
    }

    #[tokio::test]
    async fn auto_approves_tool_user_input_when_policy_is_never() {
        let dir = tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let trace = dir.path().join("trace.log");
        let script = dir.path().join("fake-codex.sh");
        write_executable(
            &script,
            &format!(
                r#"#!/bin/sh
trace_file="{}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-tool-approval"}}}}}}'
      ;;
    4)
      printf '%s\n' '{{"id":10,"result":{{"turn":{{"id":"turn-tool-approval"}}}}}}'
      printf '%s\n' '{{"id":110,"method":"item/tool/requestUserInput","params":{{"itemId":"call-approval","questions":[{{"header":"Approve app tool call?","id":"mcp_tool_call_approval_call-approval","isOther":false,"isSecret":false,"options":[{{"description":"Run once","label":"Approve Once"}},{{"description":"Run and remember","label":"Approve this Session"}},{{"description":"Decline","label":"Deny"}}],"question":"Allow this action?"}}],"threadId":"thread-tool-approval","turnId":"turn-tool-approval"}}}}'
      ;;
    5)
      printf '%s\n' '{{"method":"turn/completed"}}'
      exit 0
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
                trace.display()
            ),
        );

        let config = sample_config(&workspace, &script, Some(json!("never")));
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Ok(json!({ "data": {} })),
            }),
        );
        let mut session = app.start_session(&workspace).await.expect("session");
        let events = Arc::new(Mutex::new(Vec::<CodexEvent>::new()));
        let captured = Arc::clone(&events);

        let result = session
            .run_turn(
                "Handle approval prompt",
                &sample_issue(),
                &CancellationToken::new(),
                move |event| captured.lock().expect("events").push(event),
            )
            .await;

        session.stop().await;
        assert!(result.is_ok());

        let events = events.lock().expect("events");
        let approved = events
            .iter()
            .find(|event| event.event == "approval_auto_approved")
            .expect("approval event");
        assert_eq!(approved.message.as_deref(), Some("Approve this Session"));

        let trace = fs::read_to_string(trace).expect("trace");
        assert!(trace.contains(
            r#""mcp_tool_call_approval_call-approval":{"answers":["Approve this Session"]}"#
        ));
    }

    #[tokio::test]
    async fn emits_tool_call_failed_for_failed_dynamic_tool_results() {
        let dir = tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).expect("workspace");
        let trace = dir.path().join("trace.log");
        let script = dir.path().join("fake-codex.sh");
        write_executable(
            &script,
            &format!(
                r#"#!/bin/sh
trace_file="{}"
count=0
while IFS= read -r line; do
  count=$((count + 1))
  printf 'JSON:%s\n' "$line" >> "$trace_file"
  case "$count" in
    1)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    2)
      ;;
    3)
      printf '%s\n' '{{"id":2,"result":{{"thread":{{"id":"thread-tool"}}}}}}'
      ;;
    4)
      printf '%s\n' '{{"id":10,"result":{{"turn":{{"id":"turn-tool"}}}}}}'
      printf '%s\n' '{{"id":103,"method":"item/tool/call","params":{{"name":"todoist","callId":"call-1","threadId":"thread-tool","turnId":"turn-tool","arguments":{{"action":"get_current_user"}}}}}}'
      ;;
    5)
      printf '%s\n' '{{"method":"turn/completed"}}'
      exit 0
      ;;
    *)
      exit 0
      ;;
  esac
done
"#,
                trace.display()
            ),
        );

        let config = sample_config(&workspace, &script, Some(json!("never")));
        let app = AppServerClient::new(
            config,
            Arc::new(StubTracker {
                tool_result: Err(TrackerError::TodoistApiRequest("boom".to_string())),
            }),
        );
        let mut session = app.start_session(&workspace).await.expect("session");
        let events = Arc::new(Mutex::new(Vec::<CodexEvent>::new()));
        let captured = Arc::clone(&events);

        let result = session
            .run_turn(
                "Handle failed tool call",
                &sample_issue(),
                &CancellationToken::new(),
                move |event| captured.lock().expect("events").push(event),
            )
            .await;

        session.stop().await;
        assert!(result.is_ok());

        let events = events.lock().expect("events");
        assert!(events.iter().any(|event| event.event == "tool_call_failed"));

        let trace = fs::read_to_string(trace).expect("trace");
        assert!(trace.contains(r#""id":103"#));
        assert!(trace.contains(r#""success":false"#));
    }

    fn sample_issue() -> Issue {
        Issue {
            id: "issue-1".to_string(),
            identifier: "MT-1".to_string(),
            title: "Title".to_string(),
            state: "In Progress".to_string(),
            ..Issue::default()
        }
    }

    fn sample_config(
        workspace: &Path,
        script: &Path,
        approval_policy: Option<Value>,
    ) -> ServiceConfig {
        let workspace_root = workspace.parent().expect("workspace parent");
        let mut config = json!({
            "tracker": {
                "kind": "todoist",
                "api_key": "token",
                "project_id": "proj"
            },
            "workspace": {
                "root": workspace_root
            },
            "codex": {
                "command": script.display().to_string()
            }
        });

        if let Some(policy) = approval_policy {
            config
                .as_object_mut()
                .expect("config object")
                .get_mut("codex")
                .and_then(Value::as_object_mut)
                .expect("codex object")
                .insert("approval_policy".to_string(), policy);
        }

        ServiceConfig::from_map(config.as_object().expect("config object")).expect("config")
    }

    fn write_executable(path: &Path, script: &str) {
        fs::write(path, script).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut permissions = fs::metadata(path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).expect("chmod");
        }
    }
}
