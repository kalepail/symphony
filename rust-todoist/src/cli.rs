use std::{env, future::Future, path::PathBuf};

use clap::Parser;
use tracing::{error, info};

use crate::{
    config::ServiceConfig,
    http::HttpServer,
    logging,
    observability::TerminalDashboard,
    orchestrator::Orchestrator,
    workflow::{WorkflowError, WorkflowStore},
};

const ACKNOWLEDGEMENT_FLAG: &str =
    "i-understand-that-this-will-be-running-without-the-usual-guardrails";

#[derive(Debug, Parser)]
#[command(name = "symphony")]
#[command(about = "Rust implementation of the Symphony orchestrator")]
pub struct Args {
    #[arg(long = ACKNOWLEDGEMENT_FLAG)]
    pub acknowledge_preview: bool,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub logs_root: Option<PathBuf>,
    #[arg(value_name = "WORKFLOW.md")]
    pub workflow_path: Option<PathBuf>,
}

pub async fn run() -> Result<(), String> {
    let args = Args::parse();
    run_with_args(args).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ShutdownReason {
    Interrupt,
    Terminate,
    InterruptListenerFailed(String),
    OrchestratorExited(String),
    HttpServerExited(String),
    WorkflowWatcherExited(String),
}

impl ShutdownReason {
    fn is_unexpected(&self) -> bool {
        !matches!(self, Self::Interrupt | Self::Terminate)
    }

    fn component(&self) -> &'static str {
        match self {
            Self::Interrupt => "signal",
            Self::Terminate => "signal",
            Self::InterruptListenerFailed(_) => "signal",
            Self::OrchestratorExited(_) => "orchestrator",
            Self::HttpServerExited(_) => "http_server",
            Self::WorkflowWatcherExited(_) => "workflow_watcher",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Interrupt => "received_interrupt",
            Self::Terminate => "received_sigterm",
            Self::InterruptListenerFailed(detail)
            | Self::OrchestratorExited(detail)
            | Self::HttpServerExited(detail)
            | Self::WorkflowWatcherExited(detail) => detail,
        }
    }

    fn user_message(&self) -> Option<String> {
        match self {
            Self::Interrupt => None,
            Self::Terminate => None,
            Self::InterruptListenerFailed(detail) => Some(format!(
                "Symphony stopped unexpectedly: interrupt listener failed: {detail}"
            )),
            Self::OrchestratorExited(detail) => Some(format!(
                "Symphony stopped unexpectedly: orchestrator exited: {detail}"
            )),
            Self::HttpServerExited(detail) => Some(format!(
                "Symphony stopped unexpectedly: HTTP server exited: {detail}"
            )),
            Self::WorkflowWatcherExited(detail) => Some(format!(
                "Symphony stopped unexpectedly: workflow watcher exited: {detail}"
            )),
        }
    }
}

async fn run_with_args(args: Args) -> Result<(), String> {
    require_guardrails_acknowledgement(&args)?;
    let log_file = logging::init(args.logs_root.as_deref())?;
    info!("logging=status=initialized file={}", log_file.display());
    let workflow_path = args.workflow_path.unwrap_or_else(default_workflow_path);

    let workflow_store =
        WorkflowStore::new(workflow_path.clone()).map_err(format_workflow_error)?;
    let mut watcher = workflow_store.start_watcher();
    let mut orchestrator = Orchestrator::start(workflow_store.clone()).await?;
    let handle = orchestrator.handle();
    let startup_config = workflow_store.effective().config.clone();
    let requested_port = args.port.or(startup_config.server.port);
    let host = args
        .host
        .or_else(|| startup_config.server.host.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    info!(
        "{}",
        startup_summary(&workflow_path, &startup_config, &host, requested_port)
    );

    let mut http = match requested_port {
        Some(port) => {
            let server =
                HttpServer::start(handle.clone(), workflow_store.clone(), &host, port).await?;
            info!("http_server=status=started addr={}", server.local_addr());
            Some(server)
        }
        None => None,
    };
    let terminal_dashboard = TerminalDashboard::start(
        handle.clone(),
        workflow_store.clone(),
        startup_config.observability.clone(),
        http.as_ref().map(HttpServer::local_addr),
    );

    let shutdown_reason =
        wait_for_shutdown_reason(&mut orchestrator, http.as_mut(), &mut watcher).await;
    log_shutdown_reason(&shutdown_reason);

    run_shutdown_sequence(
        move || async move {
            terminal_dashboard.shutdown().await;
        },
        move || async move {
            if let Some(server) = http {
                server.shutdown().await;
            }
        },
        move || async move {
            watcher.shutdown().await;
        },
        move || async move {
            orchestrator.shutdown().await;
        },
    )
    .await;

    match shutdown_reason.user_message() {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

async fn wait_for_shutdown_reason(
    orchestrator: &mut Orchestrator,
    http: Option<&mut HttpServer>,
    watcher: &mut crate::workflow::WorkflowWatcher,
) -> ShutdownReason {
    let orchestrator_finished = orchestrator.is_finished();
    let http_finished = http.as_ref().is_some_and(|server| server.is_finished());
    let watcher_finished = watcher.is_finished();

    select_shutdown_reason(
        wait_for_user_shutdown_signal(),
        async move {
            if orchestrator_finished {
                std::future::pending().await
            } else {
                ShutdownReason::OrchestratorExited(component_exit_detail(
                    "orchestrator",
                    orchestrator.wait_for_exit().await,
                ))
            }
        },
        async move {
            if http_finished {
                std::future::pending().await
            } else {
                wait_for_http_exit(http).await
            }
        },
        async move {
            if watcher_finished {
                std::future::pending().await
            } else {
                ShutdownReason::WorkflowWatcherExited(component_exit_detail(
                    "workflow watcher",
                    watcher.wait_for_exit().await,
                ))
            }
        },
    )
    .await
}

async fn wait_for_user_shutdown_signal() -> ShutdownReason {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        match result {
                            Ok(()) => ShutdownReason::Interrupt,
                            Err(error) => ShutdownReason::InterruptListenerFailed(error.to_string()),
                        }
                    }
                    _ = sigterm.recv() => ShutdownReason::Terminate,
                }
            }
            Err(error) => ShutdownReason::InterruptListenerFailed(error.to_string()),
        }
    }

    #[cfg(not(unix))]
    {
        match tokio::signal::ctrl_c().await {
            Ok(()) => ShutdownReason::Interrupt,
            Err(error) => ShutdownReason::InterruptListenerFailed(error.to_string()),
        }
    }
}

async fn wait_for_http_exit(http: Option<&mut HttpServer>) -> ShutdownReason {
    match http {
        Some(server) => ShutdownReason::HttpServerExited(component_exit_detail(
            "HTTP server",
            server.wait_for_exit().await,
        )),
        None => std::future::pending().await,
    }
}

async fn select_shutdown_reason<I, O, H, W>(
    interrupt: I,
    orchestrator_exit: O,
    http_exit: H,
    workflow_watcher_exit: W,
) -> ShutdownReason
where
    I: Future<Output = ShutdownReason>,
    O: Future<Output = ShutdownReason>,
    H: Future<Output = ShutdownReason>,
    W: Future<Output = ShutdownReason>,
{
    tokio::select! {
        reason = interrupt => reason,
        reason = orchestrator_exit => reason,
        reason = http_exit => reason,
        reason = workflow_watcher_exit => reason,
    }
}

fn component_exit_detail(component: &str, result: Result<(), String>) -> String {
    match result {
        Ok(()) => format!("{component} stopped without an explicit shutdown request"),
        Err(error) => error,
    }
}

fn log_shutdown_reason(reason: &ShutdownReason) {
    if reason.is_unexpected() {
        error!(
            "runtime_shutdown status=unexpected component={} detail={}",
            reason.component(),
            reason.detail()
        );
    } else {
        info!(
            "runtime_shutdown status=requested component={} detail={}",
            reason.component(),
            reason.detail()
        );
    }
}

async fn run_shutdown_sequence<T, H, W, O, TFut, HFut, WFut, OFut>(
    shutdown_terminal: T,
    shutdown_http: H,
    shutdown_watcher: W,
    shutdown_orchestrator: O,
) where
    T: FnOnce() -> TFut,
    H: FnOnce() -> HFut,
    W: FnOnce() -> WFut,
    O: FnOnce() -> OFut,
    TFut: Future<Output = ()>,
    HFut: Future<Output = ()>,
    WFut: Future<Output = ()>,
    OFut: Future<Output = ()>,
{
    shutdown_terminal().await;
    shutdown_http().await;
    shutdown_watcher().await;
    shutdown_orchestrator().await;
}

fn default_workflow_path() -> PathBuf {
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("WORKFLOW.md")
}

fn format_workflow_error(error: WorkflowError) -> String {
    error.to_string()
}

fn require_guardrails_acknowledgement(args: &Args) -> Result<(), String> {
    if args.acknowledge_preview {
        Ok(())
    } else {
        Err(acknowledgement_banner())
    }
}

fn acknowledgement_banner() -> String {
    let lines = [
        "This Symphony implementation is a low key engineering preview.",
        "Codex will run without any guardrails.",
        "Symphony Rust is not a supported product and is presented as-is.",
        &format!("To proceed, start with `--{ACKNOWLEDGEMENT_FLAG}` CLI argument"),
    ];

    let width = lines.iter().map(|line| line.len()).max().unwrap_or(0);
    let border = "─".repeat(width + 2);
    let top = format!("╭{border}╮");
    let bottom = format!("╰{border}╯");
    let spacer = format!("│ {} │", " ".repeat(width));

    let mut content = vec![top, spacer.clone()];
    content.extend(
        lines
            .iter()
            .map(|line| format!("│ {line:<width$} │", width = width)),
    );
    content.push(spacer);
    content.push(bottom);
    content.join("\n")
}

fn startup_summary(
    workflow_path: &std::path::Path,
    config: &ServiceConfig,
    host: &str,
    port: Option<u16>,
) -> String {
    let tracker_kind = config.tracker.kind.as_deref().unwrap_or("unknown");
    let project = config.tracker.project_id.as_deref().unwrap_or("n/a");
    let label = config.tracker.label.as_deref().unwrap_or("none");
    let assignee = config.tracker.assignee.as_deref().unwrap_or("none");
    let port = port
        .map(|value| value.to_string())
        .unwrap_or_else(|| "disabled".to_string());
    let worker_hosts = if config.worker.ssh_hosts.is_empty() {
        "local".to_string()
    } else {
        config.worker.ssh_hosts.join("|")
    };

    format!(
        "runtime_start status=initialized workflow_path={} tracker_kind={} tracker_project_id={} tracker_label={} tracker_assignee={} active_states={} terminal_states={} workspace_root={} polling_interval_ms={} observability_refresh_ms={} observability_render_interval_ms={} host={} port={} worker_hosts={} max_concurrent_agents={}",
        workflow_path.display(),
        tracker_kind,
        project,
        label,
        assignee,
        config.tracker.active_states.join("|"),
        config.tracker.terminal_states.join("|"),
        config.workspace.root.display(),
        config.polling.interval_ms,
        config.observability.refresh_ms,
        config.observability.render_interval_ms,
        host,
        port,
        worker_hosts,
        config.agent.max_concurrent_agents,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use clap::Parser;
    use tempfile::tempdir;
    use tokio::time::sleep;

    use super::{
        ACKNOWLEDGEMENT_FLAG, Args, acknowledgement_banner, default_workflow_path,
        require_guardrails_acknowledgement, run_shutdown_sequence, run_with_args,
        select_shutdown_reason, startup_summary,
    };
    use crate::{config::ServiceConfig, runtime_env};

    #[test]
    fn acknowledgement_flag_is_required() {
        let args = Args::try_parse_from(["symphony"]).expect("args");
        let error = require_guardrails_acknowledgement(&args).expect_err("ack required");
        assert!(error.contains("Codex will run without any guardrails."));
        assert!(error.contains(ACKNOWLEDGEMENT_FLAG));
    }

    #[test]
    fn acknowledgement_flag_allows_startup() {
        let args = Args::try_parse_from([
            "symphony",
            "--i-understand-that-this-will-be-running-without-the-usual-guardrails",
            "--logs-root",
            "/tmp/logs",
            "./WORKFLOW.md",
        ])
        .expect("args");
        assert!(require_guardrails_acknowledgement(&args).is_ok());
        assert_eq!(
            args.logs_root.as_deref().and_then(|path| path.to_str()),
            Some("/tmp/logs")
        );
    }

    #[test]
    fn acknowledgement_banner_mentions_flag() {
        let banner = acknowledgement_banner();
        assert!(banner.contains("Symphony Rust is not a supported product"));
        assert!(banner.contains(ACKNOWLEDGEMENT_FLAG));
    }

    #[test]
    fn default_workflow_path_uses_current_directory() {
        let _guard = runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let previous = env::current_dir().expect("cwd");
        env::set_current_dir(dir.path()).expect("set cwd");
        let cwd = env::current_dir().expect("resolved cwd");

        let workflow_path = default_workflow_path();

        env::set_current_dir(previous).expect("restore cwd");
        assert_eq!(workflow_path, cwd.join("WORKFLOW.md"));
    }

    #[test]
    fn missing_default_workflow_file_fails_startup() {
        let _guard = runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let previous = env::current_dir().expect("cwd");
        env::set_current_dir(dir.path()).expect("set cwd");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let result = runtime.block_on(run_with_args(Args {
            acknowledge_preview: true,
            port: None,
            host: None,
            logs_root: Some(dir.path().join("logs")),
            workflow_path: None,
        }));

        env::set_current_dir(previous).expect("restore cwd");
        let error = result.expect_err("missing workflow");
        assert!(error.contains("missing_workflow_file"));
    }

    #[test]
    fn loads_dotenv_from_workflow_directory() {
        let _guard = runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        let dotenv_path = dir.path().join(".env");
        std::fs::write(&workflow_path, "---\ntracker:\n  kind: todoist\n---\n").expect("workflow");
        std::fs::write(
            &dotenv_path,
            "TODOIST_API_TOKEN=dotenv-token\nSYMPHONY_WORKSPACE_ROOT=/tmp/dotenv-workspaces\n",
        )
        .expect("dotenv");
        unsafe {
            env::remove_var("TODOIST_API_TOKEN");
            env::remove_var("SYMPHONY_WORKSPACE_ROOT");
        }
        runtime_env::clear_for_tests();

        runtime_env::load_dotenv_for_workflow(&workflow_path).expect("dotenv");

        assert_eq!(
            runtime_env::get("TODOIST_API_TOKEN").as_deref(),
            Some("dotenv-token")
        );
        assert_eq!(
            runtime_env::get("SYMPHONY_WORKSPACE_ROOT").as_deref(),
            Some("/tmp/dotenv-workspaces")
        );
        assert!(env::var("TODOIST_API_TOKEN").is_err());
        assert!(env::var("SYMPHONY_WORKSPACE_ROOT").is_err());

        runtime_env::clear_for_tests();
    }

    #[test]
    fn dotenv_local_overrides_dotenv_when_shell_env_is_absent() {
        let _guard = runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        std::fs::write(&workflow_path, "---\ntracker:\n  kind: todoist\n---\n").expect("workflow");
        std::fs::write(dir.path().join(".env"), "TODOIST_API_TOKEN=base-token\n").expect("env");
        std::fs::write(
            dir.path().join(".env.local"),
            "TODOIST_API_TOKEN=local-token\n",
        )
        .expect("env local");
        unsafe {
            env::remove_var("TODOIST_API_TOKEN");
        }
        runtime_env::clear_for_tests();

        runtime_env::load_dotenv_for_workflow(&workflow_path).expect("dotenv");

        assert_eq!(
            runtime_env::get("TODOIST_API_TOKEN").as_deref(),
            Some("local-token")
        );
        assert!(env::var("TODOIST_API_TOKEN").is_err());
        runtime_env::clear_for_tests();
    }

    #[test]
    fn existing_shell_env_wins_over_dotenv_files() {
        let _guard = runtime_env::test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dir = tempdir().expect("tempdir");
        let workflow_path = dir.path().join("WORKFLOW.md");
        std::fs::write(&workflow_path, "---\ntracker:\n  kind: todoist\n---\n").expect("workflow");
        std::fs::write(dir.path().join(".env"), "TODOIST_API_TOKEN=dotenv-token\n").expect("env");
        unsafe {
            env::set_var("TODOIST_API_TOKEN", "shell-token");
        }
        runtime_env::clear_for_tests();

        runtime_env::load_dotenv_for_workflow(&workflow_path).expect("dotenv");

        assert_eq!(
            runtime_env::get("TODOIST_API_TOKEN").as_deref(),
            Some("shell-token")
        );

        unsafe {
            env::remove_var("TODOIST_API_TOKEN");
        }
        runtime_env::clear_for_tests();
    }

    #[tokio::test]
    async fn select_shutdown_reason_returns_interrupt_when_signal_wins() {
        let reason = select_shutdown_reason(
            async {
                sleep(Duration::from_millis(5)).await;
                super::ShutdownReason::Interrupt
            },
            async {
                sleep(Duration::from_millis(50)).await;
                super::ShutdownReason::OrchestratorExited("late".to_string())
            },
            async {
                sleep(Duration::from_millis(50)).await;
                super::ShutdownReason::HttpServerExited("late".to_string())
            },
            async {
                sleep(Duration::from_millis(50)).await;
                super::ShutdownReason::WorkflowWatcherExited("late".to_string())
            },
        )
        .await;

        assert_eq!(reason, super::ShutdownReason::Interrupt);
    }

    #[tokio::test]
    async fn select_shutdown_reason_surfaces_component_failure() {
        let reason = select_shutdown_reason(
            async {
                sleep(Duration::from_millis(50)).await;
                super::ShutdownReason::Interrupt
            },
            async {
                sleep(Duration::from_millis(5)).await;
                super::ShutdownReason::OrchestratorExited("boom".to_string())
            },
            async {
                sleep(Duration::from_millis(50)).await;
                super::ShutdownReason::HttpServerExited("late".to_string())
            },
            async {
                sleep(Duration::from_millis(50)).await;
                super::ShutdownReason::WorkflowWatcherExited("late".to_string())
            },
        )
        .await;

        assert_eq!(
            reason,
            super::ShutdownReason::OrchestratorExited("boom".to_string())
        );
    }

    #[tokio::test]
    async fn shutdown_sequence_runs_in_expected_order() {
        let events = Arc::new(Mutex::new(Vec::new()));

        run_shutdown_sequence(
            {
                let events = Arc::clone(&events);
                move || async move {
                    events.lock().expect("events").push("terminal".to_string());
                }
            },
            {
                let events = Arc::clone(&events);
                move || async move {
                    events.lock().expect("events").push("http".to_string());
                }
            },
            {
                let events = Arc::clone(&events);
                move || async move {
                    events.lock().expect("events").push("watcher".to_string());
                }
            },
            {
                let events = Arc::clone(&events);
                move || async move {
                    events
                        .lock()
                        .expect("events")
                        .push("orchestrator".to_string());
                }
            },
        )
        .await;

        assert_eq!(
            *events.lock().expect("events"),
            vec![
                "terminal".to_string(),
                "http".to_string(),
                "watcher".to_string(),
                "orchestrator".to_string()
            ]
        );
    }

    #[test]
    fn startup_summary_includes_runtime_context() {
        let config = ServiceConfig::from_map(
            serde_json::json!({
                "tracker": {
                    "kind": "todoist",
                    "api_key": "token",
                    "project_id": "proj-1",
                    "label": "symphony-live",
                    "assignee": "user-1",
                    "active_states": ["Todo", "In Progress"],
                    "terminal_states": ["Canceled"]
                },
                "polling": {
                    "interval_ms": 15000
                },
                "observability": {
                    "refresh_ms": 1000,
                    "render_interval_ms": 16
                },
                "workspace": {
                    "root": "/tmp/symphony-workspaces"
                },
                "worker": {
                    "ssh_hosts": ["worker-a", "worker-b"]
                }
            })
            .as_object()
            .expect("config object"),
        )
        .expect("config");

        let summary = startup_summary(
            std::path::Path::new("/tmp/symphony/WORKFLOW.live.md"),
            &config,
            "127.0.0.1",
            Some(3000),
        );

        assert!(summary.contains("runtime_start status=initialized"));
        assert!(summary.contains("workflow_path=/tmp/symphony/WORKFLOW.live.md"));
        assert!(summary.contains("tracker_project_id=proj-1"));
        assert!(summary.contains("tracker_label=symphony-live"));
        assert!(summary.contains("active_states=Todo|In Progress"));
        assert!(summary.contains("terminal_states=Canceled"));
        assert!(summary.contains("worker_hosts=worker-a|worker-b"));
        assert!(summary.contains("port=3000"));
    }
}
