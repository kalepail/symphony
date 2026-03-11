use std::{env, path::PathBuf};

use clap::Parser;
use tracing::info;

use crate::{
    http::HttpServer,
    logging,
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

async fn run_with_args(args: Args) -> Result<(), String> {
    require_guardrails_acknowledgement(&args)?;
    let log_file = logging::init(args.logs_root.as_deref())?;
    info!("logging=status=initialized file={}", log_file.display());
    let workflow_path = args.workflow_path.unwrap_or_else(default_workflow_path);

    let workflow_store =
        WorkflowStore::new(workflow_path.clone()).map_err(format_workflow_error)?;
    let watcher = workflow_store.start_watcher();
    let orchestrator = Orchestrator::start(workflow_store.clone()).await?;
    let handle = orchestrator.handle();
    let startup_config = workflow_store.effective().config.clone();
    let host = args
        .host
        .or_else(|| startup_config.server.host.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let http = match args.port.or(startup_config.server.port) {
        Some(port) => {
            let server =
                HttpServer::start(handle.clone(), workflow_store.clone(), &host, port).await?;
            info!("http_server=status=started addr={}", server.local_addr());
            Some(server)
        }
        None => None,
    };

    tokio::signal::ctrl_c()
        .await
        .map_err(|error| error.to_string())?;

    if let Some(server) = http {
        server.abort();
    }
    watcher.join.abort();
    orchestrator.shutdown().await;
    Ok(())
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        ACKNOWLEDGEMENT_FLAG, Args, acknowledgement_banner, require_guardrails_acknowledgement,
    };

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
}
