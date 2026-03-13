use std::{
    collections::HashMap,
    env, fs,
    net::TcpListener as StdTcpListener,
    path::{Path, PathBuf},
    process::Command as StdCommand,
    sync::OnceLock,
    time::Duration,
};

use chrono::Utc;
use regex::Regex;
use reqwest::{
    Client, Method, StatusCode,
    header::{ACCEPT, AUTHORIZATION, HeaderMap, RETRY_AFTER, USER_AGENT},
};
use serde_json::{Value, json};
use symphony_rust_todoist::{
    orchestrator::{Orchestrator, OrchestratorHandle, OrchestratorHandleError},
    runtime_env,
    workflow::WorkflowStore,
    workspace::{workspace_path_for_identifier, workspace_path_for_identifier_on_host},
};
use tempfile::tempdir;
use tokio::{sync::Mutex, time::sleep};

const RESULT_FILE: &str = "LIVE_E2E_RESULT.txt";
const LIVE_E2E_PROJECT_PREFIX: &str = "Symphony Rust Todoist Live E2E";
const LIVE_MINIMAL_SMOKE_PROJECT_PREFIX: &str = "Symphony Rust Todoist Minimal Smoke E2E";
const LIVE_FULL_SMOKE_PROJECT_PREFIX: &str = "Symphony Rust Todoist Smoke E2E";
const LIVE_DOCKER_WORKER_COUNT: usize = 2;
const LIVE_E2E_TODO_SECTION: &str = "Todo";
const LIVE_E2E_REVIEW_SECTION: &str = "Human Review";
const LIVE_E2E_MERGING_SECTION: &str = "Merging";
const LIVE_MINIMAL_SMOKE_LABEL: &str = "symphony-smoke-minimal";
const LIVE_FULL_SMOKE_LABEL: &str = "symphony-smoke-full";
const TODOIST_WORKPAD_MARKER: &str = "<!-- symphony:workpad -->";
const TODOIST_WORKPAD_HEADER: &str = "## Codex Workpad";
const LIVE_HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
const LIVE_HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;
const TODOIST_RATE_LIMIT_MAX_RETRIES: usize = 4;
const TODOIST_RATE_LIMIT_DEFAULT_DELAY_SECS: u64 = 2;
const TODOIST_RATE_LIMIT_MAX_DELAY_SECS: u64 = 60;
const TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS: u64 = 300;
const LIVE_E2E_CLEANUP_TIMEOUT_SECS: u64 = 60;
const ORCHESTRATOR_START_MAX_RETRIES: usize = 2;
const FULL_SMOKE_HUMAN_REVIEW_TIMEOUT_SECS: u64 = 900;
const FULL_SMOKE_TODOIST_WRITE_GRACE_SECS: u64 = TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS + 30;
const FULL_SMOKE_READY_PR_HANDOFF_GRACE_SECS: u64 = FULL_SMOKE_TODOIST_WRITE_GRACE_SECS;
const FULL_SMOKE_MERGE_TIMEOUT_SECS: u64 = 600;
const FULL_SMOKE_POST_MERGE_CLOSE_GRACE_SECS: u64 = FULL_SMOKE_TODOIST_WRITE_GRACE_SECS;
const FULL_SMOKE_CONTROL_PLANE_GRACE_SECS: u64 = TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS + 30;
const FULL_SMOKE_HANDOFF_QUIESCE_TIMEOUT_SECS: u64 = FULL_SMOKE_CONTROL_PLANE_GRACE_SECS;
const FULL_SMOKE_MERGING_DISPATCH_TIMEOUT_SECS: u64 = FULL_SMOKE_CONTROL_PLANE_GRACE_SECS;
const TODOIST_PROJECT_VISIBILITY_TIMEOUT_SECS: u64 = 15;
const GITHUB_API_DEFAULT_BASE_URL: &str = "https://api.github.com";
const GITHUB_API_VERSION: &str = "2022-11-28";
const GITHUB_USER_AGENT: &str = "symphony-rust-todoist/live_e2e";
const GITHUB_REQUEST_MAX_RETRIES: usize = 4;
const GITHUB_REQUEST_DEFAULT_DELAY_SECS: u64 = 2;
const GITHUB_REQUEST_MAX_DELAY_SECS: u64 = 60;
const GITHUB_REQUEST_MAX_TOTAL_WAIT_SECS: u64 = 300;
const GITHUB_COPILOT_REVIEW_AUTHOR_PREFIX: &str = "copilot-pull-request-reviewer";
const SMOKE_REPO_OWNER: &str = "kalepail";
const SMOKE_REPO_NAME: &str = "symphony-smoke-lab";
const SMOKE_REPO_BRANCH: &str = "main";
const SMOKE_TARGET_FILE: &str = "SMOKE_TARGET.md";
const SMOKE_PR_LABEL: &str = "symphony";
const POLL_INTERVAL_SECS: u64 = 5;
const CANONICAL_SMOKE_SECTIONS: &[&str] = &[
    "Backlog",
    "Todo",
    "In Progress",
    "Human Review",
    "Rework",
    "Merging",
    "Canceled",
    "Duplicate",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveWorkerBackend {
    Local,
    Ssh,
}

#[derive(Debug)]
struct LiveWorkerSetup {
    ssh_worker_hosts: Vec<String>,
    workspace_root: String,
    remote_test_root: Option<String>,
    docker_cleanup: Option<LiveDockerCleanup>,
}

#[derive(Debug)]
struct LiveDockerCleanup {
    project_name: String,
    env: Vec<(String, String)>,
    previous_ssh_config: Option<String>,
}

impl LiveWorkerSetup {
    fn worker_yaml(&self) -> String {
        if self.ssh_worker_hosts.is_empty() {
            return String::new();
        }

        let hosts = self
            .ssh_worker_hosts
            .iter()
            .map(|host| format!("    - {}", yaml_single_quoted(host)))
            .collect::<Vec<_>>()
            .join("\n");

        format!("worker:\n  ssh_hosts:\n{hosts}\n  max_concurrent_agents_per_host: 1\n")
    }
}

fn live_http_client() -> Client {
    Client::builder()
        .connect_timeout(Duration::from_secs(LIVE_HTTP_CONNECT_TIMEOUT_SECS))
        .read_timeout(Duration::from_secs(LIVE_HTTP_REQUEST_TIMEOUT_SECS))
        .timeout(Duration::from_secs(LIVE_HTTP_REQUEST_TIMEOUT_SECS))
        .build()
        .expect("live e2e HTTP client")
}

#[tokio::test]
#[ignore = "requires live Todoist and Codex access"]
async fn completes_a_real_todoist_task_end_to_end() {
    run_live_todoist_task_end_to_end(LiveWorkerBackend::Local).await;
}

#[tokio::test]
#[ignore = "requires live Todoist, Codex, and SSH worker access"]
async fn completes_a_real_todoist_task_end_to_end_with_ssh_worker() {
    run_live_todoist_task_end_to_end(LiveWorkerBackend::Ssh).await;
}

async fn run_live_todoist_task_end_to_end(backend: LiveWorkerBackend) {
    let _guard = live_e2e_lock().lock().await;
    let manifest_workflow = manifest_workflow_path("WORKFLOW.md");
    runtime_env::load_dotenv_for_workflow(&manifest_workflow).expect("dotenv");

    if runtime_env::get("SYMPHONY_RUN_LIVE_E2E").as_deref() != Some("1") {
        eprintln!(
            "skipping live e2e; set SYMPHONY_RUN_LIVE_E2E=1 in rust-todoist/.env.local or export it"
        );
        return;
    }

    let token = match runtime_env::get("TODOIST_API_TOKEN") {
        Some(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("skipping live e2e; TODOIST_API_TOKEN is missing");
            return;
        }
    };

    assert!(
        command_is_ready("codex"),
        "`codex` must be on PATH for rust-todoist live e2e"
    );

    let client = live_http_client();
    let base_url = todoist_base_url();
    let backend_label = match backend {
        LiveWorkerBackend::Local => "local",
        LiveWorkerBackend::Ssh => "ssh",
    };
    let run_id = format!(
        "symphony-rust-todoist-live-e2e-{backend_label}-{}",
        Utc::now().timestamp_millis()
    );
    let project = create_project(
        &client,
        &base_url,
        &token,
        &format!("{LIVE_E2E_PROJECT_PREFIX} {run_id}"),
    )
    .await
    .expect("create Todoist project");
    let project_id = json_id(&project);

    let outcome = async {
        wait_for_todoist_project_visibility(&client, &base_url, &token, &project_id).await?;
        let todo_section = create_section(
            &client,
            &base_url,
            &token,
            &project_id,
            LIVE_E2E_TODO_SECTION,
        )
        .await?;
        let todo_section_id = json_id(&todo_section);
        let review_section = create_section(
            &client,
            &base_url,
            &token,
            &project_id,
            LIVE_E2E_REVIEW_SECTION,
        )
        .await?;
        let review_section_id = json_id(&review_section);
        let task = create_task_with_body(
            &client,
            &base_url,
            &token,
            &project_id,
            &todo_section_id,
            &format!("Symphony rust-todoist live e2e {run_id}"),
            &format!("Run id: {run_id}"),
            &[],
        )
        .await?;
        let task_id = json_id(&task);
        let identifier = format!("TD-{task_id}");
        let expected_result = format!("identifier={identifier}\nproject_id={project_id}\n");
        let expected_workpad_marker =
            format!("## Live E2E Marker\nidentifier={identifier}\nproject_id={project_id}");

        let dir = tempdir().map_err(|error| format!("tempdir failed: {error}"))?;
        let worker_setup = live_worker_setup(backend, &run_id, dir.path())?;
        let workflow_path = dir.path().join("WORKFLOW.md");
        write_live_issue_workflow(
            &workflow_path,
            &token,
            &base_url,
            &project_id,
            &worker_setup,
            &expected_workpad_marker,
        )?;

        let workflow_store =
            WorkflowStore::new(workflow_path.clone()).map_err(|error| error.to_string())?;
        let orchestrator = start_orchestrator_with_retry(workflow_store.clone()).await?;
        let orchestrator_handle = orchestrator.handle();
        let local_workspace_path =
            workspace_path_for_identifier(&workflow_store.effective().config, &identifier);

        let result = async {
            let started = tokio::time::Instant::now();
            let timeout = Duration::from_secs(300);
            let mut observed_comment = false;
            let mut observed_result = false;
            let mut completed = false;
            let mut remote_worker_host = None::<String>;
            let mut remote_workspace_path = None::<PathBuf>;
            let mut observed_remote_worker = false;

            while started.elapsed() < timeout {
                if !observed_result {
                    observed_result = if worker_setup.ssh_worker_hosts.is_empty() {
                        fs::read_to_string(local_workspace_path.join(RESULT_FILE))
                            .ok()
                            .is_some_and(|content| content == expected_result)
                    } else if let (Some(worker_host), Some(workspace_path)) = (
                        remote_worker_host.as_deref(),
                        remote_workspace_path.as_ref(),
                    ) {
                        read_remote_file(worker_host, &workspace_path.join(RESULT_FILE))
                            .ok()
                            .is_some_and(|content| content == expected_result)
                    } else {
                        false
                    };
                }
                if !observed_comment {
                    observed_comment = task_comments_contain(
                        &client,
                        &base_url,
                        &token,
                        &task_id,
                        &expected_workpad_marker,
                    )
                    .await?;
                }
                if !completed {
                    completed = task_is_in_section(
                        &client,
                        &base_url,
                        &token,
                        &task_id,
                        &review_section_id,
                    )
                    .await?;
                }
                if !worker_setup.ssh_worker_hosts.is_empty()
                    && (remote_worker_host.is_none() || remote_workspace_path.is_none())
                    && let Ok(Some(detail)) =
                        orchestrator_handle.issue_detail(identifier.clone()).await
                    && let Some(worker_host) = detail.workspace.worker_host
                {
                    observed_remote_worker = true;
                    remote_workspace_path = Some(workspace_path_for_identifier_on_host(
                        &workflow_store.effective().config,
                        &identifier,
                        Some(worker_host.as_str()),
                    ));
                    remote_worker_host = Some(worker_host);
                }
                if observed_result && observed_comment && completed {
                    break;
                }
                sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
            }

            if !observed_result {
                return Err("expected result file was not produced".to_string());
            }
            if !observed_comment {
                return Err("expected Todoist workpad marker was not observed".to_string());
            }
            if !completed {
                return Err("expected Todoist task to reach the Human Review section".to_string());
            }
            if !worker_setup.ssh_worker_hosts.is_empty() && !observed_remote_worker {
                return Err("expected live e2e task to dispatch onto an SSH worker".to_string());
            }

            Ok::<(), String>(())
        }
        .await;

        orchestrator.shutdown().await;
        match cleanup_live_worker_setup(worker_setup) {
            Some(error) if result.is_ok() => Err(error),
            Some(error) => Err(format!("{}; {error}", result.unwrap_err())),
            None => result,
        }
    }
    .await;

    let project_cleanup = cleanup_with_timeout(
        "Todoist live e2e project cleanup",
        delete_project(&client, &base_url, &token, &project_id),
    )
    .await;
    finalize_live_e2e(
        outcome,
        vec![
            project_cleanup
                .err()
                .map(|error| format!("Todoist live e2e cleanup failed: {error}")),
            runtime_env::load_dotenv_for_workflow(&manifest_workflow)
                .err()
                .map(|error| format!("failed to restore dotenv state: {error}")),
        ],
    );
}

#[tokio::test]
#[ignore = "requires live Todoist and Codex access"]
async fn completes_a_repo_backed_minimal_smoke_workflow_end_to_end() {
    run_minimal_smoke_repo_workflow_end_to_end(LiveWorkerBackend::Local).await;
}

#[tokio::test]
#[ignore = "requires live Todoist, Codex, and SSH worker access"]
async fn completes_a_repo_backed_minimal_smoke_workflow_end_to_end_with_ssh_worker() {
    run_minimal_smoke_repo_workflow_end_to_end(LiveWorkerBackend::Ssh).await;
}

async fn run_minimal_smoke_repo_workflow_end_to_end(backend: LiveWorkerBackend) {
    let _guard = live_e2e_lock().lock().await;
    let manifest_workflow = manifest_workflow_path("WORKFLOW.md");
    runtime_env::load_dotenv_for_workflow(&manifest_workflow).expect("dotenv");

    if runtime_env::get("SYMPHONY_RUN_LIVE_E2E").as_deref() != Some("1") {
        eprintln!(
            "skipping minimal smoke live e2e; set SYMPHONY_RUN_LIVE_E2E=1 in rust-todoist/.env.local or export it"
        );
        return;
    }

    let token = match runtime_env::get("TODOIST_API_TOKEN") {
        Some(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("skipping minimal smoke live e2e; TODOIST_API_TOKEN is missing");
            return;
        }
    };

    assert!(
        command_is_ready("codex"),
        "`codex` must be on PATH for rust-todoist minimal smoke live e2e"
    );

    let client = live_http_client();
    let base_url = todoist_base_url();
    let backend_label = match backend {
        LiveWorkerBackend::Local => "local",
        LiveWorkerBackend::Ssh => "ssh",
    };
    let run_id = format!(
        "symphony-rust-todoist-minimal-smoke-e2e-{backend_label}-{}",
        Utc::now().timestamp_millis()
    );
    let project = create_project(
        &client,
        &base_url,
        &token,
        &format!("{LIVE_MINIMAL_SMOKE_PROJECT_PREFIX} {run_id}"),
    )
    .await
    .expect("create Todoist project");
    let project_id = json_id(&project);

    let outcome = async {
        wait_for_todoist_project_visibility(&client, &base_url, &token, &project_id).await?;
        let sections =
            create_canonical_smoke_sections(&client, &base_url, &token, &project_id).await?;
        let backlog_section_id = sections
            .get("Backlog")
            .cloned()
            .ok_or_else(|| "missing Backlog section in canonical smoke setup".to_string())?;
        let todo_section_id = sections
            .get(LIVE_E2E_TODO_SECTION)
            .cloned()
            .ok_or_else(|| "missing Todo section in canonical smoke setup".to_string())?;

        let task_title = format!("Symphony rust-todoist minimal smoke {run_id}");
        let task = create_task_with_body(
            &client,
            &base_url,
            &token,
            &project_id,
            &todo_section_id,
            &task_title,
            &format!("Run a repo-backed minimal Symphony smoke cycle for {run_id}."),
            &[LIVE_MINIMAL_SMOKE_LABEL],
        )
        .await?;
        let task_id = json_id(&task);
        let identifier = format!("TD-{task_id}");

        let dir = tempdir().map_err(|error| format!("tempdir failed: {error}"))?;
        let worker_setup = live_worker_setup(backend, &run_id, dir.path())?;
        let workflow_path = write_workflow_from_template(
            dir.path(),
            "WORKFLOW.smoke.minimal.md",
            &token,
            &project_id,
            &worker_setup,
        )?;
        let workflow_store =
            WorkflowStore::new(workflow_path).map_err(|error| error.to_string())?;
        let orchestrator = start_orchestrator_with_retry(workflow_store.clone()).await?;
        let orchestrator_handle = orchestrator.handle();
        let local_workspace_path =
            workspace_path_for_identifier(&workflow_store.effective().config, &identifier);

        let result = async {
            let started = tokio::time::Instant::now();
            let timeout = Duration::from_secs(300);
            let mut observed_file_update = false;
            let mut observed_workpad = false;
            let mut returned_to_backlog = false;
            let mut remote_worker_host = None::<String>;
            let mut remote_workspace_path = None::<PathBuf>;
            let mut observed_remote_worker = false;

            while started.elapsed() < timeout {
                if !worker_setup.ssh_worker_hosts.is_empty()
                    && (remote_worker_host.is_none() || remote_workspace_path.is_none())
                    && let Ok(Some(detail)) =
                        orchestrator_handle.issue_detail(identifier.clone()).await
                    && let Some(worker_host) = detail.workspace.worker_host
                {
                    observed_remote_worker = true;
                    remote_workspace_path = Some(workspace_path_for_identifier_on_host(
                        &workflow_store.effective().config,
                        &identifier,
                        Some(worker_host.as_str()),
                    ));
                    remote_worker_host = Some(worker_host);
                }

                if !observed_file_update {
                    observed_file_update = if worker_setup.ssh_worker_hosts.is_empty() {
                        fs::read_to_string(local_workspace_path.join(SMOKE_TARGET_FILE))
                            .ok()
                            .is_some_and(|content| {
                                minimal_smoke_target_contains(&content, &identifier, &task_title)
                            })
                    } else if let (Some(worker_host), Some(workspace_path)) = (
                        remote_worker_host.as_deref(),
                        remote_workspace_path.as_ref(),
                    ) {
                        read_remote_file(worker_host, &workspace_path.join(SMOKE_TARGET_FILE))
                            .ok()
                            .is_some_and(|content| {
                                minimal_smoke_target_contains(&content, &identifier, &task_title)
                            })
                    } else {
                        false
                    };
                }

                if !observed_workpad {
                    let comments = task_comments(&client, &base_url, &token, &task_id).await?;
                    let workpads = workpad_comments(&comments);
                    observed_workpad = workpads.len() == 1
                        && workpads[0]
                            .get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|content| {
                                content.contains("SMOKE_TARGET.md")
                                    && content.contains(TODOIST_WORKPAD_HEADER)
                            });
                }

                if !returned_to_backlog {
                    returned_to_backlog = task_is_in_section(
                        &client,
                        &base_url,
                        &token,
                        &task_id,
                        &backlog_section_id,
                    )
                    .await?;
                }

                if observed_file_update && observed_workpad && returned_to_backlog {
                    break;
                }

                sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
            }

            if !observed_file_update {
                return Err(
                    "expected minimal smoke workspace to update `SMOKE_TARGET.md`".to_string(),
                );
            }
            if !observed_workpad {
                return Err(
                    "expected exactly one persistent workpad comment for minimal smoke".to_string(),
                );
            }
            if !returned_to_backlog {
                return Err("expected minimal smoke task to return to `Backlog`".to_string());
            }
            if !worker_setup.ssh_worker_hosts.is_empty() && !observed_remote_worker {
                return Err(
                    "expected minimal smoke task to dispatch onto an SSH worker".to_string()
                );
            }

            Ok::<(), String>(())
        }
        .await;

        orchestrator.shutdown().await;
        match cleanup_live_worker_setup(worker_setup) {
            Some(error) if result.is_ok() => Err(error),
            Some(error) => Err(format!("{}; {error}", result.unwrap_err())),
            None => result,
        }
    }
    .await;

    let project_cleanup = cleanup_with_timeout(
        "minimal smoke live e2e project cleanup",
        delete_project(&client, &base_url, &token, &project_id),
    )
    .await;
    finalize_live_e2e(
        outcome,
        vec![
            project_cleanup
                .err()
                .map(|error| format!("minimal smoke live e2e cleanup failed: {error}")),
            runtime_env::load_dotenv_for_workflow(&manifest_workflow)
                .err()
                .map(|error| format!("failed to restore dotenv state: {error}")),
        ],
    );
}

#[tokio::test]
#[ignore = "requires live Todoist, Codex, and GitHub access"]
async fn completes_a_full_smoke_repo_workflow_end_to_end() {
    run_full_smoke_repo_workflow_end_to_end(LiveWorkerBackend::Local).await;
}

#[tokio::test]
#[ignore = "requires live Todoist, Codex, GitHub, and SSH worker access"]
async fn completes_a_full_smoke_repo_workflow_end_to_end_with_ssh_worker() {
    run_full_smoke_repo_workflow_end_to_end(LiveWorkerBackend::Ssh).await;
}

async fn run_full_smoke_repo_workflow_end_to_end(backend: LiveWorkerBackend) {
    let _guard = live_e2e_lock().lock().await;
    let manifest_workflow = manifest_workflow_path("WORKFLOW.md");
    runtime_env::load_dotenv_for_workflow(&manifest_workflow).expect("dotenv");

    if runtime_env::get("SYMPHONY_RUN_LIVE_E2E").as_deref() != Some("1") {
        eprintln!(
            "skipping live smoke parity e2e; set SYMPHONY_RUN_LIVE_E2E=1 in rust-todoist/.env.local or export it"
        );
        return;
    }

    let token = match runtime_env::get("TODOIST_API_TOKEN") {
        Some(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("skipping live smoke parity e2e; TODOIST_API_TOKEN is missing");
            return;
        }
    };

    assert!(
        command_is_ready("codex"),
        "`codex` must be on PATH for rust-todoist live smoke parity e2e"
    );
    assert!(
        command_is_ready("gh"),
        "`gh` must be on PATH for rust-todoist live smoke parity e2e"
    );

    preflight_smoke_repo().expect("GitHub smoke repo preflight");
    reset_shared_smoke_state().expect("reset shared smoke state");

    let github_token = github_api_token().expect("GitHub auth token");
    let client = live_http_client();
    let base_url = todoist_base_url();
    let backend_label = match backend {
        LiveWorkerBackend::Local => "local",
        LiveWorkerBackend::Ssh => "ssh",
    };
    let run_id = format!(
        "symphony-rust-todoist-smoke-e2e-{backend_label}-{}",
        Utc::now().timestamp_millis()
    );
    let project = create_project(
        &client,
        &base_url,
        &token,
        &format!("{LIVE_FULL_SMOKE_PROJECT_PREFIX} {run_id}"),
    )
    .await
    .expect("create Todoist project");
    let project_id = json_id(&project);

    let outcome = async {
        wait_for_todoist_project_visibility(&client, &base_url, &token, &project_id).await?;
        let sections =
            create_canonical_smoke_sections(&client, &base_url, &token, &project_id).await?;
        let todo_section_id = sections
            .get(LIVE_E2E_TODO_SECTION)
            .cloned()
            .ok_or_else(|| "missing Todo section in canonical smoke setup".to_string())?;
        let review_section_id = sections
            .get(LIVE_E2E_REVIEW_SECTION)
            .cloned()
            .ok_or_else(|| "missing Human Review section in canonical smoke setup".to_string())?;
        let merging_section_id = sections
            .get(LIVE_E2E_MERGING_SECTION)
            .cloned()
            .ok_or_else(|| "missing Merging section in canonical smoke setup".to_string())?;

        let task = create_task_with_body(
            &client,
            &base_url,
            &token,
            &project_id,
            &todo_section_id,
            &format!("Symphony rust-todoist smoke parity {run_id}"),
            &smoke_task_description(&run_id),
            &[LIVE_FULL_SMOKE_LABEL],
        )
        .await?;
        let task_id = json_id(&task);
        let identifier = format!("TD-{task_id}");

        let dir = tempdir().map_err(|error| format!("tempdir failed: {error}"))?;
        let worker_setup = live_worker_setup(backend, &run_id, dir.path())?;
        let workflow_path = write_workflow_from_template(
            dir.path(),
            "WORKFLOW.smoke.full.md",
            &token,
            &project_id,
            &worker_setup,
        )?;
        let workflow_store =
            WorkflowStore::new(workflow_path).map_err(|error| error.to_string())?;
        let orchestrator = start_orchestrator_with_retry(workflow_store.clone()).await?;
        let orchestrator_handle = orchestrator.handle();

        let result = async {
            let review_started = tokio::time::Instant::now();
            let review_timeout = Duration::from_secs(FULL_SMOKE_HUMAN_REVIEW_TIMEOUT_SECS);
            let mut pr_url = None;
            let mut last_review_error = None;
            let mut ready_pr_without_handoff_since = None;
            let mut remote_worker_host = worker_setup.ssh_worker_hosts.first().cloned();
            let mut observed_remote_worker = false;

            while review_started.elapsed() < review_timeout {
                if !worker_setup.ssh_worker_hosts.is_empty()
                    && let Ok(Some(detail)) =
                        orchestrator_handle.issue_detail(identifier.clone()).await
                    && let Some(worker_host) = detail.workspace.worker_host
                {
                    observed_remote_worker = true;
                    remote_worker_host = Some(worker_host);
                }
                match poll_full_smoke_handoff(
                    &client,
                    &base_url,
                    &token,
                    &github_token,
                    &project_id,
                    &task_id,
                    &review_section_id,
                )
                .await
                {
                    Ok(Some(url)) => {
                        pr_url = Some(url);
                        break;
                    }
                    Ok(None) => {
                        match poll_full_smoke_ready_pr_without_handoff(
                            &client,
                            &base_url,
                            &token,
                            &github_token,
                            &project_id,
                            &task_id,
                            &identifier,
                        )
                        .await
                        {
                            Ok(Some(url)) => {
                                let observed_at = ready_pr_without_handoff_since
                                    .get_or_insert_with(tokio::time::Instant::now);
                                if observed_at.elapsed()
                                    >= Duration::from_secs(
                                        FULL_SMOKE_READY_PR_HANDOFF_GRACE_SECS,
                                    )
                                {
                                    return Err(format!(
                                        "smoke PR `{url}` was open, labeled, green, and diff-scoped, but the Todoist task never reached `Human Review`; likely blocked during final workpad write or handoff transition"
                                    ));
                                }
                            }
                            Ok(None) => ready_pr_without_handoff_since = None,
                            Err(error) if full_smoke_poll_error_is_fatal(&error) => {
                                return Err(error);
                            }
                            Err(error) => last_review_error = Some(error),
                        }
                    }
                    Err(error) if full_smoke_poll_error_is_fatal(&error) => return Err(error),
                    Err(error) => last_review_error = Some(error),
                }
                sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
            }

            let pr_url = match pr_url {
                Some(value) => value,
                None => {
                    return Err(last_review_error.unwrap_or_else(|| {
                        "expected full smoke task to reach Human Review with a labeled, green PR"
                            .to_string()
                    }));
                }
            };

            auto_approve_full_smoke_review(
                &orchestrator_handle,
                &client,
                &base_url,
                &token,
                &github_token,
                &task_id,
                &identifier,
                &merging_section_id,
                &pr_url,
            )
            .await?;

            let merge_started = tokio::time::Instant::now();
            let merge_timeout = Duration::from_secs(FULL_SMOKE_MERGE_TIMEOUT_SECS);
            let mut merged = false;
            let mut last_merge_error = None;
            let mut merge_observed_at = None;

            while merge_started.elapsed() < merge_timeout {
                if !worker_setup.ssh_worker_hosts.is_empty()
                    && let Ok(Some(detail)) =
                        orchestrator_handle.issue_detail(identifier.clone()).await
                    && let Some(worker_host) = detail.workspace.worker_host
                {
                    observed_remote_worker = true;
                    remote_worker_host = Some(worker_host);
                }
                let workspace_path = workspace_path_for_identifier_on_host(
                    &workflow_store.effective().config,
                    &identifier,
                    remote_worker_host.as_deref(),
                );
                match poll_full_smoke_merge(
                    &client,
                    &base_url,
                    &token,
                    &github_token,
                    &project_id,
                    &task_id,
                    &identifier,
                    &run_id,
                    &pr_url,
                    &workspace_path,
                    remote_worker_host.as_deref(),
                )
                .await
                {
                    Ok(FullSmokeMergeStatus::Complete) => {
                        merged = true;
                        break;
                    }
                    Ok(FullSmokeMergeStatus::MergedAwaitingClose) => {
                        let observed_at =
                            merge_observed_at.get_or_insert_with(tokio::time::Instant::now);
                        if observed_at.elapsed()
                            >= Duration::from_secs(FULL_SMOKE_POST_MERGE_CLOSE_GRACE_SECS)
                        {
                            return Err(
                                "smoke PR merged but the Todoist task remained active; expected guarded `close_task` from `Merging`"
                                    .to_string(),
                            );
                        }
                    }
                    Ok(FullSmokeMergeStatus::Pending) => {}
                    Err(error) if full_smoke_poll_error_is_fatal(&error) => return Err(error),
                    Err(error) => last_merge_error = Some(error),
                }
                sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
            }

            if !worker_setup.ssh_worker_hosts.is_empty() && !observed_remote_worker {
                return Err("expected full smoke run to dispatch onto an SSH worker".to_string());
            }

            if !merged {
                return Err(last_merge_error.unwrap_or_else(|| {
                    "expected merged PR and guarded Todoist completion after Merging".to_string()
                }));
            }

            Ok::<(), String>(())
        }
        .await;

        orchestrator.shutdown().await;
        match cleanup_live_worker_setup(worker_setup) {
            Some(error) if result.is_ok() => Err(error),
            Some(error) => Err(format!("{}; {error}", result.unwrap_err())),
            None => result,
        }
    }
    .await;

    let full_smoke_cleanup = cleanup_option_with_timeout(
        "full smoke parity cleanup",
        cleanup_full_smoke(&client, &base_url, &token, &project_id),
    )
    .await;
    finalize_live_e2e(
        outcome,
        vec![
            match full_smoke_cleanup {
                Ok(Some(error)) => Some(format!("full smoke parity cleanup failed: {error}")),
                Ok(None) => None,
                Err(error) => Some(format!("full smoke parity cleanup failed: {error}")),
            },
            runtime_env::load_dotenv_for_workflow(&manifest_workflow)
                .err()
                .map(|error| format!("failed to restore dotenv state: {error}")),
        ],
    );
}

fn write_live_issue_workflow(
    workflow_path: &Path,
    token: &str,
    base_url: &str,
    project_id: &str,
    worker_setup: &LiveWorkerSetup,
    expected_workpad_marker: &str,
) -> Result<(), String> {
    let worker_yaml = worker_setup.worker_yaml();

    fs::write(
        workflow_path,
        format!(
            r#"---
tracker:
  kind: todoist
  api_key: {api_key}
  base_url: {base_url}
  project_id: {tracker_project_id}
  active_states:
    - Todo
polling:
  interval_ms: 2500
workspace:
  root: {workspace_root}
{worker_yaml}observability:
  terminal_enabled: false
codex:
  approval_policy: never
  turn_timeout_ms: 600000
  stall_timeout_ms: 600000
---
You are running a real Symphony rust-todoist end-to-end test.

The current working directory is the workspace root.

Step 1:
Create a file named {result_file} in the current working directory by running exactly:

```sh
cat > {result_file} <<'EOF'
identifier={{{{ issue.identifier }}}}
project_id={project_id}
EOF
```

Then verify it by running:

```sh
cat {result_file}
```

The file content must be exactly:
identifier={{{{ issue.identifier }}}}
project_id={project_id}

Step 2:
Use the `todoist` tool to read the current task `{{{{ issue.id }}}}` and its task-scoped workpad.
Ensure there is exactly one persistent workpad comment for this task and that its content includes
the exact marker block below somewhere verbatim. If the marker is missing, update or create the
workpad with `todoist.upsert_workpad` so it contains the marker block verbatim in addition to any
other useful notes:

{expected_workpad_marker}

Step 3:
Use the `todoist` tool to list sections for the current project `"{project_id}"`, find the
section named `Human Review`, and move the current task into that section with `todoist.move_task`.

Step 4:
Before stopping, verify all three outcomes:
1. the file exists with the exact contents above
2. the task workpad contains the exact marker block above
3. the current task is in the `Human Review` section

Do not ask for approval.
"#,
            api_key = yaml_single_quoted(token),
            base_url = yaml_single_quoted(base_url),
            tracker_project_id = yaml_single_quoted(project_id),
            workspace_root = yaml_single_quoted(&worker_setup.workspace_root),
            worker_yaml = worker_yaml,
            result_file = RESULT_FILE,
            project_id = project_id,
            expected_workpad_marker = expected_workpad_marker,
        ),
    )
    .map_err(|error| format!("workflow write failed: {error}"))
}

fn live_e2e_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn live_worker_setup(
    backend: LiveWorkerBackend,
    run_id: &str,
    test_root: &Path,
) -> Result<LiveWorkerSetup, String> {
    match backend {
        LiveWorkerBackend::Local => {
            let workspace_root = test_root.join("workspaces");
            fs::create_dir_all(&workspace_root)
                .map_err(|error| format!("workspace root create failed: {error}"))?;
            Ok(LiveWorkerSetup {
                ssh_worker_hosts: Vec::new(),
                workspace_root: workspace_root.display().to_string(),
                remote_test_root: None,
                docker_cleanup: None,
            })
        }
        LiveWorkerBackend::Ssh => live_ssh_worker_setup(run_id, test_root),
    }
}

fn live_ssh_worker_setup(run_id: &str, test_root: &Path) -> Result<LiveWorkerSetup, String> {
    let ssh_worker_hosts = live_ssh_worker_hosts();
    if ssh_worker_hosts.is_empty() {
        live_docker_worker_setup(run_id, test_root)
    } else {
        let remote_test_root = format!("{}/.{run_id}", shared_remote_home(&ssh_worker_hosts)?);
        Ok(LiveWorkerSetup {
            ssh_worker_hosts,
            workspace_root: format!("~/.{run_id}/workspaces"),
            remote_test_root: Some(remote_test_root),
            docker_cleanup: None,
        })
    }
}

fn cleanup_live_worker_setup(worker_setup: LiveWorkerSetup) -> Option<String> {
    let mut errors = Vec::new();

    if let Some(remote_test_root) = worker_setup.remote_test_root.as_deref()
        && let Err(error) =
            cleanup_remote_test_root(remote_test_root, &worker_setup.ssh_worker_hosts)
    {
        errors.push(error);
    }

    if let Some(docker_cleanup) = worker_setup.docker_cleanup {
        restore_env_var(
            "SYMPHONY_SSH_CONFIG",
            docker_cleanup.previous_ssh_config.as_deref(),
        );
        if let Err(error) = docker_compose_down(&docker_cleanup.project_name, &docker_cleanup.env) {
            errors.push(error);
        }
    }

    (!errors.is_empty()).then(|| errors.join("; "))
}

fn live_ssh_worker_hosts() -> Vec<String> {
    runtime_env::get("SYMPHONY_LIVE_SSH_WORKER_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn live_docker_worker_setup(run_id: &str, test_root: &Path) -> Result<LiveWorkerSetup, String> {
    let ssh_root = test_root.join("live-docker-ssh");
    let key_path = ssh_root.join("id_ed25519");
    let config_path = ssh_root.join("config");
    let auth_json_path = default_docker_auth_json();
    let worker_ports = reserve_tcp_ports(LIVE_DOCKER_WORKER_COUNT)?;
    let worker_hosts = worker_ports
        .iter()
        .map(|port| format!("localhost:{port}"))
        .collect::<Vec<_>>();
    let project_name = docker_project_name(run_id);
    let previous_ssh_config = runtime_env::get("SYMPHONY_SSH_CONFIG");
    let docker_env = docker_compose_env(
        &worker_ports,
        &auth_json_path,
        &key_path.with_extension("pub"),
    );

    if !command_is_ready("docker") {
        return Err("docker worker mode requires `docker` on PATH".to_string());
    }
    if !auth_json_path.is_file() {
        return Err(format!(
            "docker worker mode requires Codex auth at {}",
            auth_json_path.display()
        ));
    }

    generate_ssh_keypair(&key_path)?;
    write_docker_ssh_config(&config_path, &key_path)?;
    unsafe {
        env::set_var("SYMPHONY_SSH_CONFIG", &config_path);
    }

    let remote_test_root = match (|| -> Result<String, String> {
        docker_compose_up(&project_name, &docker_env)?;
        wait_for_ssh_hosts(&worker_hosts)?;
        Ok(format!("{}/.{run_id}", shared_remote_home(&worker_hosts)?))
    })() {
        Ok(remote_test_root) => remote_test_root,
        Err(error) => {
            restore_env_var("SYMPHONY_SSH_CONFIG", previous_ssh_config.as_deref());
            let _ = docker_compose_down(&project_name, &docker_env);
            return Err(error);
        }
    };

    Ok(LiveWorkerSetup {
        ssh_worker_hosts: worker_hosts,
        workspace_root: format!("~/.{run_id}/workspaces"),
        remote_test_root: Some(remote_test_root),
        docker_cleanup: Some(LiveDockerCleanup {
            project_name,
            env: docker_env,
            previous_ssh_config,
        }),
    })
}

fn cleanup_remote_test_root(test_root: &str, ssh_worker_hosts: &[String]) -> Result<(), String> {
    let mut errors = Vec::new();
    for worker_host in ssh_worker_hosts {
        if let Err(error) = ssh_run(worker_host, &format!("rm -rf {}", shell_escape(test_root))) {
            errors.push(format!("{worker_host}: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("remote cleanup failed: {}", errors.join("; ")))
    }
}

fn remote_path_exists(worker_host: &str, remote_path: &Path) -> Result<bool, String> {
    let remote_path = remote_path.display().to_string();
    ssh_run(
        worker_host,
        &format!(
            "path={}; case \"$path\" in ~/*) path=\"$HOME/${{path#~/}}\" ;; \"~\") path=\"$HOME\" ;; esac; if [ -e \"$path\" ]; then printf present; fi",
            shell_escape(&remote_path)
        ),
    )
    .map(|output| output.trim() == "present")
}

fn restore_env_var(key: &str, value: Option<&str>) {
    if let Some(value) = value {
        unsafe {
            env::set_var(key, value);
        }
    } else {
        unsafe {
            env::remove_var(key);
        }
    }
}

fn manifest_workflow_path(filename: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(filename)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate should live under repo root")
        .to_path_buf()
}

fn finalize_live_e2e(outcome: Result<(), String>, cleanup_steps: Vec<Option<String>>) {
    let cleanup_errors: Vec<String> = cleanup_steps.into_iter().flatten().collect();

    match (outcome, cleanup_errors.is_empty()) {
        (Ok(()), true) => {}
        (Ok(()), false) => {
            eprintln!("live e2e cleanup warning: {}", cleanup_errors.join("; "));
        }
        (Err(error), true) => panic!("{error}"),
        (Err(error), false) => panic!("{error}; {}", cleanup_errors.join("; ")),
    }
}

async fn cleanup_with_timeout<T, F>(label: &str, future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    tokio::time::timeout(Duration::from_secs(LIVE_E2E_CLEANUP_TIMEOUT_SECS), future)
        .await
        .map_err(|_| {
            format!(
                "{label} exceeded {}s timeout",
                LIVE_E2E_CLEANUP_TIMEOUT_SECS
            )
        })?
}

async fn cleanup_option_with_timeout<T, F>(label: &str, future: F) -> Result<T, String>
where
    F: std::future::Future<Output = T>,
{
    tokio::time::timeout(Duration::from_secs(LIVE_E2E_CLEANUP_TIMEOUT_SECS), future)
        .await
        .map_err(|_| {
            format!(
                "{label} exceeded {}s timeout",
                LIVE_E2E_CLEANUP_TIMEOUT_SECS
            )
        })
}

fn minimal_smoke_target_contains(content: &str, identifier: &str, task_title: &str) -> bool {
    content.contains("## Change Log")
        && content.contains(identifier)
        && content.contains(task_title)
        && content.contains("minimal smoke test")
}

fn full_smoke_poll_error_is_fatal(error: &str) -> bool {
    error.starts_with("Todoist project `")
        || error.starts_with("Todoist task disappeared before PR merge was observed")
        || error.starts_with("Todoist task completed before PR merge was observed")
        || error.starts_with("merged smoke PR did not land `")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TodoistTaskStatus {
    Active,
    Completed,
    Missing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FullSmokeMergeStatus {
    Pending,
    MergedAwaitingClose,
    Complete,
}

fn default_docker_auth_json() -> PathBuf {
    runtime_env::get("SYMPHONY_LIVE_DOCKER_AUTH_JSON")
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(&env::var("HOME").unwrap_or_else(|_| ".".to_string()))
                .join(".codex/auth.json")
        })
}

fn shared_remote_home(worker_hosts: &[String]) -> Result<String, String> {
    let mut homes = worker_hosts
        .iter()
        .map(|worker_host| remote_home(worker_host).map(|home| (worker_host.clone(), home)))
        .collect::<Result<Vec<_>, _>>()?;

    let Some((_, first_home)) = homes.first() else {
        return Err("expected at least one live SSH worker host".to_string());
    };

    if homes.iter().all(|(_, home)| home == first_home) {
        Ok(first_home.clone())
    } else {
        homes.sort_by(|left, right| left.0.cmp(&right.0));
        Err(format!(
            "expected all live SSH workers to share one home directory, got: {homes:?}"
        ))
    }
}

fn remote_home(worker_host: &str) -> Result<String, String> {
    let output = ssh_run(worker_host, "printf '%s\\n' \"$HOME\"")?;
    let home = output.trim().to_string();
    if home.is_empty() {
        Err(format!("expected non-empty remote home for {worker_host}"))
    } else {
        Ok(home)
    }
}

fn reserve_tcp_ports(count: usize) -> Result<Vec<u16>, String> {
    let mut ports = Vec::with_capacity(count);
    while ports.len() < count {
        let listener = StdTcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| format!("failed to reserve TCP port: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| format!("failed to read reserved TCP port: {error}"))?
            .port();
        drop(listener);
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    Ok(ports)
}

fn generate_ssh_keypair(key_path: &Path) -> Result<(), String> {
    let key_dir = key_path
        .parent()
        .ok_or_else(|| "ssh key path had no parent directory".to_string())?;
    fs::create_dir_all(key_dir)
        .map_err(|error| format!("failed to create ssh key directory: {error}"))?;
    let _ = fs::remove_file(key_path);
    let _ = fs::remove_file(key_path.with_extension("pub"));

    let output = StdCommand::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(key_path)
        .output()
        .map_err(|error| format!("failed to launch `ssh-keygen`: {error}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to generate live docker ssh key (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn write_docker_ssh_config(config_path: &Path, key_path: &Path) -> Result<(), String> {
    let config_contents = format!(
        "Host localhost 127.0.0.1\n  User root\n  IdentityFile {}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  LogLevel ERROR\n",
        key_path.display()
    );

    let config_dir = config_path
        .parent()
        .ok_or_else(|| "ssh config path had no parent directory".to_string())?;
    fs::create_dir_all(config_dir)
        .map_err(|error| format!("failed to create ssh config directory: {error}"))?;
    fs::write(config_path, config_contents)
        .map_err(|error| format!("failed to write ssh config: {error}"))
}

fn docker_project_name(run_id: &str) -> String {
    run_id
        .chars()
        .map(|ch| {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_alphanumeric() || matches!(lower, '_' | '-') {
                lower
            } else {
                '-'
            }
        })
        .collect()
}

fn docker_compose_env(
    worker_ports: &[u16],
    auth_json_path: &Path,
    authorized_key_path: &Path,
) -> Vec<(String, String)> {
    vec![
        (
            "SYMPHONY_LIVE_DOCKER_AUTH_JSON".to_string(),
            auth_json_path.display().to_string(),
        ),
        (
            "SYMPHONY_LIVE_DOCKER_AUTHORIZED_KEY".to_string(),
            authorized_key_path.display().to_string(),
        ),
        (
            "SYMPHONY_LIVE_DOCKER_WORKER_1_PORT".to_string(),
            worker_ports
                .first()
                .copied()
                .unwrap_or_default()
                .to_string(),
        ),
        (
            "SYMPHONY_LIVE_DOCKER_WORKER_2_PORT".to_string(),
            worker_ports.get(1).copied().unwrap_or_default().to_string(),
        ),
    ]
}

fn docker_compose_up(project_name: &str, docker_env: &[(String, String)]) -> Result<(), String> {
    let support_dir = repo_root().join("elixir/test/support/live_e2e_docker");
    let compose_file = support_dir.join("docker-compose.yml");
    let output = StdCommand::new("docker")
        .args([
            "compose",
            "-f",
            compose_file.to_str().unwrap_or_default(),
            "-p",
            project_name,
            "up",
            "-d",
            "--build",
        ])
        .current_dir(&support_dir)
        .envs(docker_env.iter().map(|(key, value)| (key, value)))
        .output()
        .map_err(|error| format!("failed to launch docker compose: {error}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to start live docker workers (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }
}

fn docker_compose_down(project_name: &str, docker_env: &[(String, String)]) -> Result<(), String> {
    let support_dir = repo_root().join("elixir/test/support/live_e2e_docker");
    let compose_file = support_dir.join("docker-compose.yml");
    let output = StdCommand::new("docker")
        .args([
            "compose",
            "-f",
            compose_file.to_str().unwrap_or_default(),
            "-p",
            project_name,
            "down",
            "-v",
            "--remove-orphans",
        ])
        .current_dir(&support_dir)
        .envs(docker_env.iter().map(|(key, value)| (key, value)))
        .output()
        .map_err(|error| format!("failed to launch docker compose cleanup: {error}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to stop live docker workers (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim()
        ))
    }
}

fn wait_for_ssh_hosts(worker_hosts: &[String]) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    for worker_host in worker_hosts {
        loop {
            match ssh_run(worker_host, "printf ok") {
                Ok(output) if output.trim() == "ok" => break,
                Ok(_) | Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Ok(output) => {
                    return Err(format!(
                        "timed out waiting for ssh host {worker_host} to return `ok`, got `{}`",
                        output.trim()
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "timed out waiting for ssh host {worker_host}: {error}"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn read_remote_file(worker_host: &str, remote_path: &Path) -> Result<String, String> {
    let remote_path = remote_path.display().to_string();
    ssh_run(
        worker_host,
        &format!(
            "path={}; case \"$path\" in ~/*) path=\"$HOME/${{path#~/}}\" ;; \"~\") path=\"$HOME\" ;; esac; cat \"$path\"",
            shell_escape(&remote_path)
        ),
    )
}

fn ssh_run(worker_host: &str, command: &str) -> Result<String, String> {
    let executable = ssh_executable_for_tests()?;
    let output = StdCommand::new(executable)
        .args(ssh_args_for_tests(worker_host, command))
        .output()
        .map_err(|error| format!("failed to launch ssh for {worker_host}: {error}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let merged = if output.stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            String::from_utf8_lossy(&output.stderr).trim().to_string()
        };
        Err(format!(
            "ssh to {worker_host} failed with status {}: {}",
            output.status, merged
        ))
    }
}

fn ssh_executable_for_tests() -> Result<String, String> {
    if let Some(explicit) = runtime_env::get("SYMPHONY_SSH_BIN").filter(|value| !value.is_empty()) {
        return Ok(explicit);
    }

    if let Some(path) = env::var_os("PATH") {
        for candidate_dir in env::split_paths(&path) {
            let candidate = candidate_dir.join("ssh");
            if candidate.is_file() {
                return Ok(candidate.display().to_string());
            }
        }
    }

    Err("ssh executable not found".to_string())
}

fn ssh_args_for_tests(worker_host: &str, command: &str) -> Vec<String> {
    let (destination, port) = parse_worker_host(worker_host);
    let mut args = Vec::new();
    if let Some(config_path) =
        runtime_env::get("SYMPHONY_SSH_CONFIG").filter(|value| !value.trim().is_empty())
    {
        args.push("-F".to_string());
        args.push(config_path);
    }
    args.push("-T".to_string());
    if let Some(port) = port {
        args.push("-p".to_string());
        args.push(port);
    }
    args.push(destination);
    args.push(remote_shell_command(command));
    args
}

fn parse_worker_host(worker_host: &str) -> (String, Option<String>) {
    let trimmed = worker_host.trim();
    if let Some((destination, port)) = trimmed.rsplit_once(':')
        && !destination.is_empty()
        && port.chars().all(|ch| ch.is_ascii_digit())
        && (!destination.contains(':') || (destination.contains('[') && destination.contains(']')))
    {
        return (destination.to_string(), Some(port.to_string()));
    }
    (trimmed.to_string(), None)
}

fn remote_shell_command(command: &str) -> String {
    format!("sh -lc {}", shell_escape(command))
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn todoist_base_url() -> String {
    runtime_env::get("TODOIST_API_BASE_URL")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://api.todoist.com/api/v1".to_string())
}

fn command_is_ready(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }

    env::var_os("PATH").is_some_and(|search_path| {
        env::split_paths(&search_path).any(|directory| directory.join(command).is_file())
    })
}

fn preflight_smoke_repo() -> Result<(), String> {
    let script_path = manifest_workflow_path("scripts/github_publish_preflight.sh");
    let output = StdCommand::new("sh")
        .arg(&script_path)
        .arg("--repo")
        .arg(format!("{SMOKE_REPO_OWNER}/{SMOKE_REPO_NAME}"))
        .arg("--label")
        .arg(SMOKE_PR_LABEL)
        .output()
        .map_err(|error| format!("failed to launch GitHub smoke repo preflight: {error}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(format!(
        "GitHub smoke repo preflight failed: {}",
        if stderr.is_empty() { stdout } else { stderr }
    ))
}

fn run_shared_smoke_reset(extra_env: &[(&str, &str)]) -> Result<(), String> {
    let script_path = repo_root().join("scripts/reset_smoke_state.py");
    let mut command = StdCommand::new("python3");
    command.arg(&script_path);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command
        .output()
        .map_err(|error| format!("failed to launch shared smoke reset: {error}"))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Err(format!(
        "shared smoke reset failed: {}",
        if stderr.is_empty() { stdout } else { stderr }
    ))
}

fn reset_shared_smoke_state() -> Result<(), String> {
    run_shared_smoke_reset(&[])
}

fn reset_shared_smoke_repo_state() -> Result<(), String> {
    run_shared_smoke_reset(&[
        ("SYMPHONY_SMOKE_SKIP_TODOIST_RESET", "1"),
        ("SYMPHONY_SMOKE_SKIP_LINEAR_RESET", "1"),
    ])
}

async fn cleanup_full_smoke(
    client: &Client,
    base_url: &str,
    token: &str,
    project_id: &str,
) -> Option<String> {
    let mut errors = Vec::new();

    if let Err(error) = delete_project(client, base_url, token, project_id).await {
        errors.push(format!("Todoist project cleanup failed: {error}"));
    }
    if let Err(error) = reset_shared_smoke_repo_state() {
        errors.push(format!("shared smoke repo reset failed: {error}"));
    }

    (!errors.is_empty()).then(|| errors.join("; "))
}

fn smoke_task_description(run_id: &str) -> String {
    format!(
        r#"## Goal

Run a high-fidelity Symphony smoke test against the dedicated smoke repository.

## Requirements

- Update `SMOKE_TARGET.md` by appending one dated bullet under `## Change Log`
- Mention the issue identifier in the new bullet
- Include the live run id `{run_id}` in the new bullet
- Do not modify any other repo file
- Run `sh scripts/validate-smoke-repo.sh`
- Commit, push, open a PR, label it `symphony`, and attach it to this issue

## Validation

- [ ] `sh scripts/validate-smoke-repo.sh`
- [ ] GitHub Actions `smoke-ci` passes on the PR
"#
    )
}

fn write_workflow_from_template(
    workflow_dir: &Path,
    template_name: &str,
    token: &str,
    project_id: &str,
    worker_setup: &LiveWorkerSetup,
) -> Result<PathBuf, String> {
    let template_path = manifest_workflow_path(template_name);
    let mut template = fs::read_to_string(&template_path)
        .map_err(|error| format!("failed to read workflow template: {error}"))?;

    if let Some(base_url) = runtime_env::get("TODOIST_API_BASE_URL")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != "https://api.todoist.com/api/v1")
    {
        template = inject_todoist_base_url(&template, &base_url);
    }
    template = inject_worker_yaml(&template, &worker_setup.worker_yaml());

    let workflow_path = workflow_dir.join("WORKFLOW.md");
    fs::write(&workflow_path, template)
        .map_err(|error| format!("failed to write workflow template copy: {error}"))?;

    if worker_setup.ssh_worker_hosts.is_empty() {
        fs::create_dir_all(Path::new(&worker_setup.workspace_root))
            .map_err(|error| format!("failed to create workflow workspace root: {error}"))?;
    }

    let mut env_lines = vec![
        format!("TODOIST_API_TOKEN={}", dotenv_quoted(token)),
        format!(
            "SYMPHONY_WORKSPACE_ROOT={}",
            dotenv_quoted(&worker_setup.workspace_root)
        ),
        format!("SYMPHONY_SMOKE_PROJECT_ID={}", dotenv_quoted(project_id)),
    ];
    if let Some(base_url) = runtime_env::get("TODOIST_API_BASE_URL")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        env_lines.push(format!("TODOIST_API_BASE_URL={}", dotenv_quoted(&base_url)));
    }

    fs::write(workflow_dir.join(".env.local"), env_lines.join("\n") + "\n")
        .map_err(|error| format!("failed to write workflow dotenv overlay: {error}"))?;

    Ok(workflow_path)
}

fn inject_todoist_base_url(template: &str, base_url: &str) -> String {
    if template.contains("  base_url: ") {
        return template.to_string();
    }

    let needle = "  api_key: $TODOIST_API_TOKEN\n";
    template.replacen(
        needle,
        &format!("{needle}  base_url: {}\n", yaml_single_quoted(base_url)),
        1,
    )
}

fn inject_worker_yaml(template: &str, worker_yaml: &str) -> String {
    if worker_yaml.is_empty() || template.contains("\nworker:\n") {
        return template.to_string();
    }

    let needle = "workspace:\n  root: $SYMPHONY_WORKSPACE_ROOT\n";
    template.replacen(needle, &format!("{needle}{worker_yaml}"), 1)
}

fn dotenv_quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn yaml_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn json_id(value: &Value) -> String {
    value
        .get("id")
        .and_then(json_id_scalar)
        .expect("todoist id")
}

fn json_id_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(json_id_scalar)
}

fn json_id_scalar(value: &Value) -> Option<String> {
    Some(match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn create_task_with_body(
    client: &Client,
    base_url: &str,
    token: &str,
    project_id: &str,
    section_id: &str,
    content: &str,
    description: &str,
    labels: &[&str],
) -> Result<Value, String> {
    let mut body = json!({
        "content": content,
        "description": description,
        "project_id": project_id,
        "section_id": section_id
    });
    if !labels.is_empty() {
        body["labels"] = Value::Array(
            labels
                .iter()
                .map(|label| Value::String((*label).to_string()))
                .collect(),
        );
    }

    todoist_request(
        client,
        base_url,
        token,
        Method::POST,
        "/tasks",
        None,
        Some(body),
    )
    .await
}

async fn create_project(
    client: &Client,
    base_url: &str,
    token: &str,
    name: &str,
) -> Result<Value, String> {
    todoist_request(
        client,
        base_url,
        token,
        Method::POST,
        "/projects",
        None,
        Some(json!({ "name": name })),
    )
    .await
}

async fn create_section(
    client: &Client,
    base_url: &str,
    token: &str,
    project_id: &str,
    name: &str,
) -> Result<Value, String> {
    todoist_request(
        client,
        base_url,
        token,
        Method::POST,
        "/sections",
        None,
        Some(json!({
            "project_id": project_id,
            "name": name
        })),
    )
    .await
}

async fn create_canonical_smoke_sections(
    client: &Client,
    base_url: &str,
    token: &str,
    project_id: &str,
) -> Result<HashMap<String, String>, String> {
    let mut sections = HashMap::new();

    for name in CANONICAL_SMOKE_SECTIONS {
        let section = create_section(client, base_url, token, project_id, name).await?;
        sections.insert((*name).to_string(), json_id(&section));
    }

    Ok(sections)
}

async fn delete_project(
    client: &Client,
    base_url: &str,
    token: &str,
    project_id: &str,
) -> Result<(), String> {
    match todoist_request_optional(
        client,
        base_url,
        token,
        Method::DELETE,
        &format!("/projects/{project_id}"),
        None,
        None,
    )
    .await
    {
        Ok(Some(_)) | Ok(None) => Ok(()),
        Err(error) => Err(error),
    }
}

async fn move_task_to_section(
    client: &Client,
    base_url: &str,
    token: &str,
    task_id: &str,
    section_id: &str,
) -> Result<(), String> {
    todoist_request(
        client,
        base_url,
        token,
        Method::POST,
        &format!("/tasks/{task_id}/move"),
        None,
        Some(json!({ "section_id": section_id })),
    )
    .await
    .map(|_| ())
}

async fn task_comments(
    client: &Client,
    base_url: &str,
    token: &str,
    task_id: &str,
) -> Result<Vec<Value>, String> {
    let comments = todoist_request(
        client,
        base_url,
        token,
        Method::GET,
        "/comments",
        Some(&[
            ("task_id".to_string(), task_id.to_string()),
            ("limit".to_string(), "200".to_string()),
        ]),
        None,
    )
    .await?;

    comments
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "Todoist comments payload did not contain `results`".to_string())
}

async fn task_comments_contain(
    client: &Client,
    base_url: &str,
    token: &str,
    task_id: &str,
    expected_comment: &str,
) -> Result<bool, String> {
    Ok(task_comments(client, base_url, token, task_id)
        .await?
        .iter()
        .any(|comment| {
            comment
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.contains(expected_comment))
        }))
}

fn workpad_comments(comments: &[Value]) -> Vec<&Value> {
    comments
        .iter()
        .filter(|comment| {
            comment
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| {
                    content.contains(TODOIST_WORKPAD_MARKER)
                        || content.trim_start().starts_with(TODOIST_WORKPAD_HEADER)
                })
        })
        .collect()
}

async fn task_is_in_section(
    client: &Client,
    base_url: &str,
    token: &str,
    task_id: &str,
    section_id: &str,
) -> Result<bool, String> {
    Ok(todoist_request_optional(
        client,
        base_url,
        token,
        Method::GET,
        &format!("/tasks/{task_id}"),
        None,
        None,
    )
    .await?
    .is_some_and(|task| json_id_field(&task, "section_id").as_deref() == Some(section_id)))
}

async fn todoist_project_exists(
    client: &Client,
    base_url: &str,
    token: &str,
    project_id: &str,
) -> Result<bool, String> {
    Ok(todoist_request_optional(
        client,
        base_url,
        token,
        Method::GET,
        &format!("/projects/{project_id}"),
        None,
        None,
    )
    .await?
    .is_some())
}

async fn wait_for_todoist_project_visibility(
    client: &Client,
    base_url: &str,
    token: &str,
    project_id: &str,
) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    let timeout = Duration::from_secs(TODOIST_PROJECT_VISIBILITY_TIMEOUT_SECS);

    loop {
        if todoist_project_exists(client, base_url, token, project_id).await? {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(format!(
                "Todoist project `{project_id}` did not become visible within {TODOIST_PROJECT_VISIBILITY_TIMEOUT_SECS}s"
            ));
        }
        sleep(Duration::from_secs(1)).await;
    }
}

async fn todoist_task_status(
    client: &Client,
    base_url: &str,
    token: &str,
    task_id: &str,
) -> Result<TodoistTaskStatus, String> {
    let task = todoist_request_optional(
        client,
        base_url,
        token,
        Method::GET,
        &format!("/tasks/{task_id}"),
        None,
        None,
    )
    .await?;

    Ok(match task {
        Some(task)
            if task
                .get("checked")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || task
                    .get("completed_at")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty()) =>
        {
            TodoistTaskStatus::Completed
        }
        Some(_) => TodoistTaskStatus::Active,
        None => TodoistTaskStatus::Missing,
    })
}

async fn todoist_request(
    client: &Client,
    base_url: &str,
    token: &str,
    method: Method,
    path: &str,
    query: Option<&[(String, String)]>,
    body: Option<Value>,
) -> Result<Value, String> {
    todoist_request_optional(client, base_url, token, method, path, query, body)
        .await?
        .ok_or_else(|| format!("Todoist response missing for {path}"))
}

async fn start_orchestrator_with_retry(
    workflow_store: WorkflowStore,
) -> Result<Orchestrator, String> {
    let mut attempts = 0usize;

    loop {
        match Orchestrator::start(workflow_store.clone()).await {
            Ok(orchestrator) => return Ok(orchestrator),
            Err(error) => {
                if let Some(delay_secs) = orchestrator_retry_delay_seconds(&error, attempts) {
                    attempts += 1;
                    sleep(Duration::from_secs(delay_secs)).await;
                    continue;
                }
                return Err(error);
            }
        }
    }
}

async fn todoist_request_optional(
    client: &Client,
    base_url: &str,
    token: &str,
    method: Method,
    path: &str,
    query: Option<&[(String, String)]>,
    body: Option<Value>,
) -> Result<Option<Value>, String> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let request_id = todoist_request_id(&method);
    let mut attempts = 0usize;
    let mut waited_secs = 0u64;

    loop {
        let mut request = client
            .request(method.clone(), &url)
            .bearer_auth(token)
            .header("Accept", "application/json");

        if let Some(request_id) = request_id.as_deref() {
            request = request.header("X-Request-Id", request_id);
        }

        if let Some(query) = query {
            request = request.query(query);
        }
        if let Some(body) = body.as_ref() {
            request = request.json(body);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                if let Some(delay_secs) =
                    todoist_retry_delay_seconds(None, None, attempts, waited_secs)
                {
                    attempts += 1;
                    waited_secs = waited_secs.saturating_add(delay_secs);
                    sleep(Duration::from_secs(delay_secs)).await;
                    continue;
                }
                return Err(format!("Todoist request failed for {path}: {error}"));
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|error| format!("Todoist body read failed for {path}: {error}"))?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let retry_after = todoist_retry_after_hint(&headers, &text);
        if status == StatusCode::TOO_MANY_REQUESTS
            && retry_after_exceeds_wait_budget(
                retry_after,
                waited_secs,
                TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS,
            )
        {
            return Err(format!(
                "Todoist rate limit retry_after={retry_after:?} exceeds live test wait budget of {TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS}s for {path}: {text}"
            ));
        }
        if let Some(delay_secs) =
            todoist_retry_delay_seconds(retry_after, Some(status), attempts, waited_secs)
        {
            attempts += 1;
            waited_secs = waited_secs.saturating_add(delay_secs);
            sleep(Duration::from_secs(delay_secs)).await;
            continue;
        }
        if !status.is_success() {
            return Err(format!(
                "Todoist request failed with HTTP {} for {}: {}",
                status.as_u16(),
                path,
                text
            ));
        }
        if text.trim().is_empty() {
            return Ok(Some(Value::Null));
        }
        return serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("Todoist JSON parse failed for {path}: {error}"));
    }
}

fn todoist_request_id(method: &Method) -> Option<String> {
    matches!(*method, Method::POST | Method::DELETE).then(|| {
        format!(
            "todoist-live-e2e-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        )
    })
}

fn todoist_retry_after_hint(headers: &HeaderMap, text: &str) -> Option<u64> {
    let header_hint = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let body_hint = serde_json::from_str::<Value>(text).ok().and_then(|body| {
        body.get("error_extra")
            .and_then(|value| value.get("retry_after"))
            .and_then(Value::as_u64)
    });

    match (header_hint, body_hint) {
        (Some(header), Some(body)) => Some(header.max(body)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn retry_after_exceeds_wait_budget(
    retry_after: Option<u64>,
    waited_secs: u64,
    total_wait_budget_secs: u64,
) -> bool {
    retry_after
        .is_some_and(|retry_after| retry_after > total_wait_budget_secs.saturating_sub(waited_secs))
}

fn todoist_retry_delay_seconds(
    retry_after: Option<u64>,
    status: Option<StatusCode>,
    attempts: usize,
    waited_secs: u64,
) -> Option<u64> {
    if attempts >= TODOIST_RATE_LIMIT_MAX_RETRIES {
        return None;
    }

    if matches!(status, Some(StatusCode::TOO_MANY_REQUESTS))
        && retry_after_exceeds_wait_budget(
            retry_after,
            waited_secs,
            TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS,
        )
    {
        return None;
    }

    let delay_secs = match status {
        Some(StatusCode::TOO_MANY_REQUESTS) => retry_after
            .unwrap_or(TODOIST_RATE_LIMIT_DEFAULT_DELAY_SECS)
            .clamp(1, TODOIST_RATE_LIMIT_MAX_DELAY_SECS),
        Some(status) if todoist_status_is_transient(status) => {
            todoist_transient_retry_delay(attempts)
        }
        None => todoist_transient_retry_delay(attempts),
        _ => return None,
    };

    waited_secs
        .saturating_add(delay_secs)
        .le(&TODOIST_RATE_LIMIT_MAX_TOTAL_WAIT_SECS)
        .then_some(delay_secs)
}

fn todoist_transient_retry_delay(attempts: usize) -> u64 {
    let power = (attempts as u32).min(6);
    TODOIST_RATE_LIMIT_DEFAULT_DELAY_SECS
        .saturating_mul(2u64.saturating_pow(power))
        .clamp(1, TODOIST_RATE_LIMIT_MAX_DELAY_SECS)
}

fn todoist_status_is_transient(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

#[allow(clippy::too_many_arguments)]
async fn auto_approve_full_smoke_review(
    orchestrator: &OrchestratorHandle,
    client: &Client,
    base_url: &str,
    token: &str,
    github_token: &str,
    task_id: &str,
    identifier: &str,
    merging_section_id: &str,
    pr_url: &str,
) -> Result<(), String> {
    wait_for_full_smoke_handoff_quiescence(orchestrator, identifier).await?;

    if let Some(reason) =
        github_full_smoke_auto_review_blocker(client, github_token, pr_url).await?
    {
        return Err(format!(
            "automated smoke reviewer would not approve `{pr_url}`: {reason}"
        ));
    }

    move_task_to_section(client, base_url, token, task_id, merging_section_id).await?;
    wait_for_full_smoke_merging_turn(orchestrator, client, base_url, token, task_id, identifier)
        .await
}

fn orchestrator_retry_delay_seconds(error: &str, attempts: usize) -> Option<u64> {
    if attempts >= ORCHESTRATOR_START_MAX_RETRIES {
        return None;
    }

    if error.starts_with("todoist_rate_limited") {
        let retry_after = error
            .split("retry_after=Some(")
            .nth(1)
            .and_then(|suffix| suffix.split(')').next())
            .and_then(|value| value.parse::<u64>().ok());

        return Some(
            retry_after
                .unwrap_or(TODOIST_RATE_LIMIT_DEFAULT_DELAY_SECS)
                .clamp(1, TODOIST_RATE_LIMIT_MAX_DELAY_SECS),
        );
    }

    if error.starts_with("todoist_api_status 5") || error.starts_with("todoist_api_request ") {
        return Some(todoist_transient_retry_delay(attempts));
    }

    None
}

#[test]
fn todoist_retry_delay_seconds_retries_transient_server_errors() {
    assert_eq!(
        todoist_retry_delay_seconds(None, Some(StatusCode::BAD_GATEWAY), 0, 0),
        Some(2)
    );
    assert_eq!(
        todoist_retry_delay_seconds(None, Some(StatusCode::SERVICE_UNAVAILABLE), 1, 2),
        Some(4)
    );
}

#[test]
fn todoist_retry_delay_seconds_retries_transport_failures() {
    assert_eq!(todoist_retry_delay_seconds(None, None, 0, 0), Some(2));
}

#[test]
fn todoist_retry_delay_seconds_caps_large_retry_after_values() {
    assert_eq!(
        todoist_retry_delay_seconds(Some(60), Some(StatusCode::TOO_MANY_REQUESTS), 0, 0),
        Some(60)
    );
    assert_eq!(
        todoist_retry_delay_seconds(Some(1_027), Some(StatusCode::TOO_MANY_REQUESTS), 0, 0),
        None
    );
}

#[test]
fn retry_after_exceeds_wait_budget_detects_oversized_hints() {
    assert!(retry_after_exceeds_wait_budget(Some(301), 0, 300));
    assert!(retry_after_exceeds_wait_budget(Some(51), 250, 300));
    assert!(!retry_after_exceeds_wait_budget(Some(50), 250, 300));
    assert!(!retry_after_exceeds_wait_budget(None, 0, 300));
}

#[test]
fn orchestrator_retry_delay_seconds_retries_transient_todoist_startup_errors() {
    assert_eq!(
        orchestrator_retry_delay_seconds("todoist_api_status 502", 0),
        Some(2)
    );
    assert_eq!(
        orchestrator_retry_delay_seconds("todoist_api_request connection reset", 1),
        Some(4)
    );
    assert_eq!(
        orchestrator_retry_delay_seconds("todoist_rate_limited retry_after=Some(1027)", 0),
        Some(60)
    );
}

#[test]
fn github_pr_is_merged_accepts_rest_and_gh_shapes() {
    assert!(github_pr_is_merged(&json!({
        "merged_at": "2026-03-12T16:00:00Z"
    })));
    assert!(github_pr_is_merged(&json!({
        "mergedAt": "2026-03-12T16:00:00Z"
    })));
    assert!(!github_pr_is_merged(&json!({
        "merged_at": Value::Null
    })));
}

fn github_api_base_url() -> String {
    runtime_env::get("GITHUB_API_BASE_URL")
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| GITHUB_API_DEFAULT_BASE_URL.to_string())
}

fn github_api_token() -> Result<String, String> {
    if let Some(token) = runtime_env::get("GH_TOKEN")
        .or_else(|| runtime_env::get("GITHUB_TOKEN"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(token);
    }

    let output = StdCommand::new("gh")
        .args(["auth", "token"])
        .output()
        .map_err(|error| format!("failed to run `gh auth token`: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("`gh auth token` exited with {}", output.status)
        } else {
            format!("`gh auth token` failed: {stderr}")
        });
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err("`gh auth token` returned an empty token".to_string());
    }

    Ok(token)
}

async fn github_request_json(
    client: &Client,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let url = format!("{}{}", github_api_base_url(), path);
    let mut attempts = 0usize;
    let mut waited_secs = 0u64;

    loop {
        let mut request = client
            .request(method.clone(), &url)
            .header(ACCEPT, "application/vnd.github+json")
            .header(USER_AGENT, GITHUB_USER_AGENT)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION);

        if let Some(body) = body.as_ref() {
            request = request.json(body);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                if let Some(delay_secs) =
                    github_retry_delay_seconds(None, None, None, attempts, waited_secs)
                {
                    attempts += 1;
                    waited_secs = waited_secs.saturating_add(delay_secs);
                    sleep(Duration::from_secs(delay_secs)).await;
                    continue;
                }
                return Err(format!("GitHub request failed for {path}: {error}"));
            }
        };
        let status = response.status();
        let headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|error| format!("GitHub body read failed for {path}: {error}"))?;
        let retry_after = github_retry_after_hint(&headers, &text);

        if github_status_uses_retry_after(status, &text)
            && retry_after_exceeds_wait_budget(
                retry_after,
                waited_secs,
                GITHUB_REQUEST_MAX_TOTAL_WAIT_SECS,
            )
        {
            return Err(format!(
                "GitHub rate limit retry_after={retry_after:?} exceeds live test wait budget of {GITHUB_REQUEST_MAX_TOTAL_WAIT_SECS}s for {path}: {text}"
            ));
        }

        if let Some(delay_secs) = github_retry_delay_seconds(
            retry_after,
            Some(status),
            Some(&text),
            attempts,
            waited_secs,
        ) {
            attempts += 1;
            waited_secs = waited_secs.saturating_add(delay_secs);
            sleep(Duration::from_secs(delay_secs)).await;
            continue;
        }

        if !status.is_success() {
            return Err(format!(
                "GitHub request failed with HTTP {} for {}: {}",
                status.as_u16(),
                path,
                text
            ));
        }

        return serde_json::from_str(&text)
            .map_err(|error| format!("GitHub JSON parse failed for {path}: {error}"));
    }
}

async fn github_request_raw(client: &Client, token: &str, path: &str) -> Result<String, String> {
    let url = format!("{}{}", github_api_base_url(), path);
    let mut attempts = 0usize;
    let mut waited_secs = 0u64;

    loop {
        let response = match client
            .request(Method::GET, &url)
            .header(ACCEPT, "application/vnd.github.raw+json")
            .header(USER_AGENT, GITHUB_USER_AGENT)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if let Some(delay_secs) =
                    github_retry_delay_seconds(None, None, None, attempts, waited_secs)
                {
                    attempts += 1;
                    waited_secs = waited_secs.saturating_add(delay_secs);
                    sleep(Duration::from_secs(delay_secs)).await;
                    continue;
                }
                return Err(format!("GitHub raw request failed for {path}: {error}"));
            }
        };

        let status = response.status();
        let headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|error| format!("GitHub raw body read failed for {path}: {error}"))?;
        let retry_after = github_retry_after_hint(&headers, &text);

        if github_status_uses_retry_after(status, &text)
            && retry_after_exceeds_wait_budget(
                retry_after,
                waited_secs,
                GITHUB_REQUEST_MAX_TOTAL_WAIT_SECS,
            )
        {
            return Err(format!(
                "GitHub rate limit retry_after={retry_after:?} exceeds live test wait budget of {GITHUB_REQUEST_MAX_TOTAL_WAIT_SECS}s for {path}: {text}"
            ));
        }

        if let Some(delay_secs) = github_retry_delay_seconds(
            retry_after,
            Some(status),
            Some(&text),
            attempts,
            waited_secs,
        ) {
            attempts += 1;
            waited_secs = waited_secs.saturating_add(delay_secs);
            sleep(Duration::from_secs(delay_secs)).await;
            continue;
        }

        if !status.is_success() {
            return Err(format!(
                "GitHub raw request failed with HTTP {} for {}: {}",
                status.as_u16(),
                path,
                text
            ));
        }

        return Ok(text);
    }
}

fn github_pr_coordinates(pr_url: &str) -> Result<(String, String, String), String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let regex = RE.get_or_init(|| {
        Regex::new(r"^https://github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)/pull/(\d+)$")
            .expect("valid PR URL regex")
    });

    let captures = regex
        .captures(pr_url)
        .ok_or_else(|| format!("invalid GitHub PR URL in workpad: {pr_url}"))?;

    Ok((
        captures[1].to_string(),
        captures[2].to_string(),
        captures[3].to_string(),
    ))
}

fn extract_github_pr_url(content: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let regex = RE.get_or_init(|| {
        Regex::new(r"https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/pull/\d+")
            .expect("valid PR URL regex")
    });

    regex
        .find(content)
        .map(|matched| matched.as_str().to_string())
}

async fn github_pull_request_details(
    client: &Client,
    token: &str,
    pr_url: &str,
) -> Result<Value, String> {
    let (owner, repo, number) = github_pr_coordinates(pr_url)?;
    github_request_json(
        client,
        token,
        Method::GET,
        &format!("/repos/{owner}/{repo}/pulls/{number}"),
        None,
    )
    .await
}

async fn github_pull_request_comments(
    client: &Client,
    token: &str,
    pr_url: &str,
) -> Result<Vec<Value>, String> {
    let (owner, repo, number) = github_pr_coordinates(pr_url)?;
    let payload = github_request_json(
        client,
        token,
        Method::GET,
        &format!("/repos/{owner}/{repo}/issues/{number}/comments?per_page=100"),
        None,
    )
    .await?;

    payload
        .as_array()
        .cloned()
        .ok_or_else(|| "GitHub PR issue-comments response was not an array".to_string())
}

async fn github_pull_request_review_comments(
    client: &Client,
    token: &str,
    pr_url: &str,
) -> Result<Vec<Value>, String> {
    let (owner, repo, number) = github_pr_coordinates(pr_url)?;
    let payload = github_request_json(
        client,
        token,
        Method::GET,
        &format!("/repos/{owner}/{repo}/pulls/{number}/comments?per_page=100"),
        None,
    )
    .await?;

    payload
        .as_array()
        .cloned()
        .ok_or_else(|| "GitHub PR review-comments response was not an array".to_string())
}

async fn github_pull_request_reviews(
    client: &Client,
    token: &str,
    pr_url: &str,
) -> Result<Vec<Value>, String> {
    let (owner, repo, number) = github_pr_coordinates(pr_url)?;
    let payload = github_request_json(
        client,
        token,
        Method::GET,
        &format!("/repos/{owner}/{repo}/pulls/{number}/reviews?per_page=100"),
        None,
    )
    .await?;

    payload
        .as_array()
        .cloned()
        .ok_or_else(|| "GitHub PR reviews response was not an array".to_string())
}

async fn github_full_smoke_auto_review_blocker(
    client: &Client,
    token: &str,
    pr_url: &str,
) -> Result<Option<String>, String> {
    let issue_comments = github_pull_request_comments(client, token, pr_url).await?;
    let review_comments = github_pull_request_review_comments(client, token, pr_url).await?;
    let reviews = github_pull_request_reviews(client, token, pr_url).await?;

    Ok(full_smoke_auto_review_blocker(
        &issue_comments,
        &review_comments,
        &reviews,
    ))
}

fn github_pr_is_merged(payload: &Value) -> bool {
    payload
        .get("merged_at")
        .or_else(|| payload.get("mergedAt"))
        .is_some_and(|value| !value.is_null())
}

fn full_smoke_auto_review_blocker(
    issue_comments: &[Value],
    review_comments: &[Value],
    reviews: &[Value],
) -> Option<String> {
    for comment in issue_comments {
        let author = github_actor_login(comment);
        let body = github_comment_body(comment);
        if !body.is_empty() && !github_comment_is_ignorable(author) {
            return Some(format!(
                "top-level PR comment from `{}` requires review",
                author.unwrap_or("unknown")
            ));
        }
    }

    for comment in review_comments {
        let author = github_actor_login(comment);
        let body = github_comment_body(comment);
        if !body.is_empty() && !github_comment_is_ignorable(author) {
            return Some(format!(
                "inline review comment from `{}` requires review",
                author.unwrap_or("unknown")
            ));
        }
    }

    for review in reviews {
        let author = github_actor_login(review);
        let state = review
            .get("state")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let body = github_comment_body(review);

        if review_is_ignorable_copilot_overview(author, state, body) {
            continue;
        }

        if matches!(state, "CHANGES_REQUESTED" | "REQUEST_CHANGES") {
            return Some(format!(
                "review state `{state}` from `{}` requires rework",
                author.unwrap_or("unknown")
            ));
        }

        if state == "COMMENTED" && !body.is_empty() {
            return Some(format!(
                "review summary comment from `{}` requires review",
                author.unwrap_or("unknown")
            ));
        }
    }

    None
}

fn github_actor_login(value: &Value) -> Option<&str> {
    value
        .get("user")
        .or_else(|| value.get("author"))
        .and_then(|value| value.get("login"))
        .and_then(Value::as_str)
}

fn github_comment_body(value: &Value) -> &str {
    value
        .get("body")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
}

fn github_comment_is_ignorable(author: Option<&str>) -> bool {
    author.is_some_and(is_copilot_review_author)
}

fn review_is_ignorable_copilot_overview(author: Option<&str>, state: &str, body: &str) -> bool {
    author.is_some_and(is_copilot_review_author) && state == "COMMENTED" && !body.is_empty()
}

fn is_copilot_review_author(author: &str) -> bool {
    author
        .trim()
        .starts_with(GITHUB_COPILOT_REVIEW_AUTHOR_PREFIX)
}

fn github_retry_after_hint(headers: &HeaderMap, text: &str) -> Option<u64> {
    let header_hint = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let body_hint = serde_json::from_str::<Value>(text).ok().and_then(|body| {
        body.get("retry_after")
            .or_else(|| {
                body.get("error_extra")
                    .and_then(|extra| extra.get("retry_after"))
            })
            .and_then(Value::as_u64)
    });

    match (header_hint, body_hint) {
        (Some(header), Some(body)) => Some(header.max(body)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn github_retry_delay_seconds(
    retry_after: Option<u64>,
    status: Option<StatusCode>,
    text: Option<&str>,
    attempts: usize,
    waited_secs: u64,
) -> Option<u64> {
    if attempts >= GITHUB_REQUEST_MAX_RETRIES {
        return None;
    }

    if status.is_some_and(|status| github_status_uses_retry_after(status, text.unwrap_or_default()))
        && retry_after_exceeds_wait_budget(
            retry_after,
            waited_secs,
            GITHUB_REQUEST_MAX_TOTAL_WAIT_SECS,
        )
    {
        return None;
    }

    let delay_secs = match status {
        Some(status) if github_status_uses_retry_after(status, text.unwrap_or_default()) => {
            retry_after
                .unwrap_or(GITHUB_REQUEST_DEFAULT_DELAY_SECS)
                .clamp(1, GITHUB_REQUEST_MAX_DELAY_SECS)
        }
        Some(status) if github_status_is_transient(status, text.unwrap_or_default()) => {
            github_transient_retry_delay(attempts)
        }
        None => github_transient_retry_delay(attempts),
        _ => return None,
    };

    waited_secs
        .saturating_add(delay_secs)
        .le(&GITHUB_REQUEST_MAX_TOTAL_WAIT_SECS)
        .then_some(delay_secs)
}

fn github_status_uses_retry_after(status: StatusCode, text: &str) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || (status == StatusCode::FORBIDDEN && github_error_mentions_rate_limit(text))
}

fn github_status_is_transient(status: StatusCode, text: &str) -> bool {
    github_status_uses_retry_after(status, text)
        || matches!(
            status,
            StatusCode::REQUEST_TIMEOUT
                | StatusCode::INTERNAL_SERVER_ERROR
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
        )
}

fn github_error_mentions_rate_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("rate limit")
        || lower.contains("secondary rate limit")
        || lower.contains("abuse")
}

fn github_transient_retry_delay(attempts: usize) -> u64 {
    let power = (attempts as u32).min(6);
    GITHUB_REQUEST_DEFAULT_DELAY_SECS
        .saturating_mul(2u64.saturating_pow(power))
        .clamp(1, GITHUB_REQUEST_MAX_DELAY_SECS)
}

async fn github_issue_has_label(
    client: &Client,
    token: &str,
    pr_url: &str,
    label: &str,
) -> Result<bool, String> {
    let (owner, repo, number) = github_pr_coordinates(pr_url)?;
    let issue = github_request_json(
        client,
        token,
        Method::GET,
        &format!("/repos/{owner}/{repo}/issues/{number}"),
        None,
    )
    .await?;

    Ok(issue
        .get("labels")
        .and_then(Value::as_array)
        .is_some_and(|labels| {
            labels.iter().any(|entry| {
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name == label)
            })
        }))
}

async fn github_pr_changed_files(
    client: &Client,
    token: &str,
    pr_url: &str,
) -> Result<Vec<String>, String> {
    let (owner, repo, number) = github_pr_coordinates(pr_url)?;
    let payload = github_request_json(
        client,
        token,
        Method::GET,
        &format!("/repos/{owner}/{repo}/pulls/{number}/files?per_page=100"),
        None,
    )
    .await?;

    payload
        .as_array()
        .ok_or_else(|| "GitHub PR files response was not an array".to_string())
        .map(|files| {
            files
                .iter()
                .filter_map(|entry| entry.get("filename").and_then(Value::as_str))
                .map(ToOwned::to_owned)
                .collect()
        })
}

async fn github_pr_checks_green(
    client: &Client,
    token: &str,
    pr_url: &str,
    head_sha: &str,
) -> Result<bool, String> {
    let (owner, repo, _) = github_pr_coordinates(pr_url)?;
    let check_runs_payload = github_request_json(
        client,
        token,
        Method::GET,
        &format!("/repos/{owner}/{repo}/commits/{head_sha}/check-runs"),
        None,
    )
    .await?;

    match github_check_runs_payload_green(&check_runs_payload)? {
        Some(result) => Ok(result),
        None => {
            let status_payload = github_request_json(
                client,
                token,
                Method::GET,
                &format!("/repos/{owner}/{repo}/commits/{head_sha}/status"),
                None,
            )
            .await?;
            github_combined_status_payload_green(&status_payload)
        }
    }
}

fn github_check_runs_payload_green(payload: &Value) -> Result<Option<bool>, String> {
    let runs = payload
        .get("check_runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "GitHub check-runs response did not contain `check_runs`".to_string())?;

    if runs.is_empty() {
        return Ok(None);
    }

    Ok(Some(runs.iter().all(|run| {
        run.get("status").and_then(Value::as_str) == Some("completed")
            && run.get("conclusion").and_then(Value::as_str) == Some("success")
    })))
}

fn github_combined_status_payload_green(payload: &Value) -> Result<bool, String> {
    let state = payload
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub combined-status response did not contain `state`".to_string())?;
    let statuses = payload
        .get("statuses")
        .and_then(Value::as_array)
        .ok_or_else(|| "GitHub combined-status response did not contain `statuses`".to_string())?;
    let total_count = payload
        .get("total_count")
        .and_then(Value::as_u64)
        .unwrap_or(statuses.len() as u64);

    Ok(total_count > 0 && state == "success")
}

fn full_smoke_running_state_matches(
    status: &str,
    running_state: Option<&str>,
    expected_state: &str,
) -> bool {
    status == "running"
        && running_state.is_some_and(|state| state.eq_ignore_ascii_case(expected_state))
}

fn describe_orchestrator_handle_error(error: OrchestratorHandleError) -> &'static str {
    match error {
        OrchestratorHandleError::TimedOut => "timed_out",
        OrchestratorHandleError::Unavailable => "unavailable",
    }
}

async fn wait_for_full_smoke_handoff_quiescence(
    orchestrator: &OrchestratorHandle,
    identifier: &str,
) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    let timeout = Duration::from_secs(FULL_SMOKE_HANDOFF_QUIESCE_TIMEOUT_SECS);
    let mut last_status = "status=`unknown`".to_string();

    while started.elapsed() < timeout {
        match orchestrator.issue_detail(identifier.to_string()).await {
            Ok(None) => return Ok(()),
            Ok(Some(detail)) => {
                last_status = format!(
                    "status=`{}` running_state=`{}` last_error=`{}`",
                    detail.status,
                    detail
                        .running
                        .as_ref()
                        .map(|running| running.state.as_str())
                        .unwrap_or("none"),
                    detail.last_error.as_deref().unwrap_or("none"),
                );
            }
            Err(error) => {
                last_status = format!(
                    "status=`orchestrator_{}`",
                    describe_orchestrator_handle_error(error)
                );
            }
        }
        sleep(Duration::from_secs(1)).await;
    }

    match orchestrator.issue_detail(identifier.to_string()).await {
        Ok(Some(detail)) => Err(format!(
            "full smoke task reached `Human Review` but the prior turn did not quiesce before `Merging`; orchestrator detail status=`{}` running_state=`{}` last_error=`{}`",
            detail.status,
            detail
                .running
                .as_ref()
                .map(|running| running.state.as_str())
                .unwrap_or("none"),
            detail.last_error.as_deref().unwrap_or("none"),
        )),
        Ok(None) => Ok(()),
        Err(_) => Err(format!(
            "full smoke task reached `Human Review` but the prior turn did not quiesce before `Merging`; {}",
            last_status
        )),
    }
}

async fn wait_for_full_smoke_merging_turn(
    orchestrator: &OrchestratorHandle,
    client: &Client,
    base_url: &str,
    token: &str,
    task_id: &str,
    identifier: &str,
) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    let timeout = Duration::from_secs(FULL_SMOKE_MERGING_DISPATCH_TIMEOUT_SECS);
    let mut last_detail =
        "status=`idle` tracked_state=`none` running_state=`none` last_error=`none`".to_string();
    let _ = orchestrator.refresh().await;

    while started.elapsed() < timeout {
        if matches!(
            todoist_task_status(client, base_url, token, task_id).await?,
            TodoistTaskStatus::Completed | TodoistTaskStatus::Missing
        ) {
            return Ok(());
        }

        match orchestrator.issue_detail(identifier.to_string()).await {
            Ok(Some(detail)) => {
                let running_state = detail
                    .running
                    .as_ref()
                    .map(|running| running.state.as_str());
                let tracked_state = detail
                    .tracked_issue
                    .as_ref()
                    .map(|issue| issue.state.as_str());

                if full_smoke_running_state_matches(
                    &detail.status,
                    running_state,
                    LIVE_E2E_MERGING_SECTION,
                ) {
                    return Ok(());
                }

                last_detail = format!(
                    "status=`{}` tracked_state=`{}` running_state=`{}` last_error=`{}`",
                    detail.status,
                    tracked_state.unwrap_or("none"),
                    running_state.unwrap_or("none"),
                    detail.last_error.as_deref().unwrap_or("none"),
                );
            }
            Ok(None) => {}
            Err(error) => {
                last_detail = format!(
                    "status=`orchestrator_{}` tracked_state=`none` running_state=`none` last_error=`none`",
                    describe_orchestrator_handle_error(error)
                );
            }
        }

        sleep(Duration::from_secs(1)).await;
    }

    Err(format!(
        "full smoke task moved to `Merging` but the follow-up turn did not start before timeout; {}",
        last_detail
    ))
}

async fn github_repo_file(
    client: &Client,
    token: &str,
    owner: &str,
    repo: &str,
    reference: &str,
    path: &str,
) -> Result<String, String> {
    github_request_raw(
        client,
        token,
        &format!("/repos/{owner}/{repo}/contents/{path}?ref={reference}"),
    )
    .await
}

async fn poll_full_smoke_handoff(
    client: &Client,
    base_url: &str,
    token: &str,
    github_token: &str,
    project_id: &str,
    task_id: &str,
    review_section_id: &str,
) -> Result<Option<String>, String> {
    if !todoist_project_exists(client, base_url, token, project_id).await? {
        return Err(format!(
            "Todoist project `{project_id}` disappeared before the smoke task reached `Human Review`"
        ));
    }

    if !task_is_in_section(client, base_url, token, task_id, review_section_id).await? {
        return Ok(None);
    }

    let comments = task_comments(client, base_url, token, task_id).await?;
    let workpads = workpad_comments(&comments);
    if workpads.len() != 1 {
        return Err(format!(
            "expected exactly one persistent workpad comment before Human Review, found {}",
            workpads.len()
        ));
    }

    let workpad_content = workpads[0]
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "workpad comment was missing readable content".to_string())?;
    let pr_url = extract_github_pr_url(workpad_content)
        .ok_or_else(|| "workpad comment did not contain a GitHub PR URL".to_string())?;

    let pr = github_pull_request_details(client, github_token, &pr_url).await?;
    let title = pr
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub PR response did not contain a title".to_string())?;
    if !title.starts_with("[smoke]") {
        return Err(format!(
            "expected smoke PR title to start with `[smoke]`, got `{title}`"
        ));
    }
    if pr.get("state").and_then(Value::as_str) != Some("open") {
        return Err(format!(
            "expected smoke PR to still be open at Human Review, got `{}`",
            pr.get("state").and_then(Value::as_str).unwrap_or("unknown")
        ));
    }

    let head_sha = pr
        .get("head")
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub PR response did not contain `head.sha`".to_string())?;

    if !github_issue_has_label(client, github_token, &pr_url, SMOKE_PR_LABEL).await? {
        return Err("expected smoke PR to carry the `symphony` label".to_string());
    }
    if !github_pr_checks_green(client, github_token, &pr_url, head_sha).await? {
        return Ok(None);
    }

    let changed_files = github_pr_changed_files(client, github_token, &pr_url).await?;
    if changed_files != vec![SMOKE_TARGET_FILE.to_string()] {
        return Err(format!(
            "expected smoke PR to only change `{SMOKE_TARGET_FILE}`, changed files were {changed_files:?}"
        ));
    }

    Ok(Some(pr_url))
}

async fn poll_full_smoke_ready_pr_without_handoff(
    client: &Client,
    base_url: &str,
    token: &str,
    github_token: &str,
    project_id: &str,
    task_id: &str,
    identifier: &str,
) -> Result<Option<String>, String> {
    if !todoist_project_exists(client, base_url, token, project_id).await? {
        return Err(format!(
            "Todoist project `{project_id}` disappeared before the smoke task reached `Human Review`"
        ));
    }

    let Some(pr) =
        github_find_open_smoke_pr_for_identifier(client, github_token, identifier).await?
    else {
        return Ok(None);
    };

    let pr_url = pr
        .get("html_url")
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub PR response did not contain `html_url`".to_string())?
        .to_string();

    if pr.get("state").and_then(Value::as_str) != Some("open") {
        return Ok(None);
    }

    if !github_issue_has_label(client, github_token, &pr_url, SMOKE_PR_LABEL).await? {
        return Ok(None);
    }

    let head_sha = pr
        .get("head")
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub PR response did not contain `head.sha`".to_string())?;

    if !github_pr_checks_green(client, github_token, &pr_url, head_sha).await? {
        return Ok(None);
    }

    let changed_files = github_pr_changed_files(client, github_token, &pr_url).await?;
    if changed_files != vec![SMOKE_TARGET_FILE.to_string()] {
        return Ok(None);
    }

    if matches!(
        todoist_task_status(client, base_url, token, task_id).await?,
        TodoistTaskStatus::Completed | TodoistTaskStatus::Missing
    ) {
        return Ok(None);
    }

    Ok(Some(pr_url))
}

#[allow(clippy::too_many_arguments)]
async fn poll_full_smoke_merge(
    client: &Client,
    base_url: &str,
    token: &str,
    github_token: &str,
    project_id: &str,
    task_id: &str,
    identifier: &str,
    run_id: &str,
    pr_url: &str,
    workspace_path: &Path,
    worker_host: Option<&str>,
) -> Result<FullSmokeMergeStatus, String> {
    let pr = github_pull_request_details(client, github_token, pr_url).await?;
    let merged = github_pr_is_merged(&pr);
    if !todoist_project_exists(client, base_url, token, project_id).await? {
        return Err(format!(
            "Todoist project `{project_id}` disappeared before the full smoke run completed"
        ));
    }

    let task_status = todoist_task_status(client, base_url, token, task_id).await?;
    let task_completed = matches!(
        task_status,
        TodoistTaskStatus::Completed | TodoistTaskStatus::Missing
    );

    if matches!(task_status, TodoistTaskStatus::Missing) && !merged {
        return Err("Todoist task disappeared before PR merge was observed".to_string());
    }
    if matches!(task_status, TodoistTaskStatus::Completed) && !merged {
        return Err("Todoist task completed before PR merge was observed".to_string());
    }

    if merged && !task_completed {
        return Ok(FullSmokeMergeStatus::MergedAwaitingClose);
    }

    if !merged || !task_completed {
        return Ok(FullSmokeMergeStatus::Pending);
    }

    let workspace_exists = match worker_host {
        Some(worker_host) => remote_path_exists(worker_host, workspace_path)?,
        None => workspace_path.exists(),
    };

    if workspace_exists {
        return Ok(FullSmokeMergeStatus::Pending);
    }

    let main_content = github_repo_file(
        client,
        github_token,
        SMOKE_REPO_OWNER,
        SMOKE_REPO_NAME,
        SMOKE_REPO_BRANCH,
        SMOKE_TARGET_FILE,
    )
    .await?;

    if !main_content.contains(run_id) || !main_content.contains(identifier) {
        return Err(format!(
            "merged smoke PR did not land `{run_id}` and `{identifier}` in `{SMOKE_TARGET_FILE}` on `{SMOKE_REPO_BRANCH}`"
        ));
    }

    Ok(FullSmokeMergeStatus::Complete)
}

async fn github_find_open_smoke_pr_for_identifier(
    client: &Client,
    token: &str,
    identifier: &str,
) -> Result<Option<Value>, String> {
    let payload = github_request_json(
        client,
        token,
        Method::GET,
        &format!("/repos/{SMOKE_REPO_OWNER}/{SMOKE_REPO_NAME}/pulls?state=open&per_page=100"),
        None,
    )
    .await?;

    let pulls = payload
        .as_array()
        .ok_or_else(|| "GitHub pull list response was not an array".to_string())?;
    let identifier_lower = identifier.to_ascii_lowercase();

    Ok(pulls
        .iter()
        .find(|pull| {
            let title = pull
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let body = pull
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let head_ref = pull
                .get("head")
                .and_then(|value| value.get("ref"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();

            title.starts_with("[smoke]")
                && (title.contains(&identifier_lower)
                    || body.contains(&identifier_lower)
                    || head_ref.contains(&identifier_lower))
        })
        .cloned())
}

#[test]
fn github_check_runs_payload_green_accepts_all_successful_runs() {
    let payload = json!({
        "check_runs": [
            {"status": "completed", "conclusion": "success"},
            {"status": "completed", "conclusion": "success"}
        ]
    });

    assert_eq!(
        github_check_runs_payload_green(&payload).unwrap(),
        Some(true)
    );
}

#[test]
fn github_check_runs_payload_green_falls_back_when_no_runs_exist() {
    let payload = json!({ "check_runs": [] });

    assert_eq!(github_check_runs_payload_green(&payload).unwrap(), None);
}

#[test]
fn github_combined_status_payload_green_accepts_successful_status_contexts() {
    let payload = json!({
        "state": "success",
        "total_count": 1,
        "statuses": [
            {"state": "success", "context": "ci/smoke"}
        ]
    });

    assert!(github_combined_status_payload_green(&payload).unwrap());
}

#[test]
fn github_combined_status_payload_green_rejects_empty_or_non_success_statuses() {
    let empty_payload = json!({
        "state": "success",
        "total_count": 0,
        "statuses": []
    });
    let pending_payload = json!({
        "state": "pending",
        "total_count": 1,
        "statuses": [
            {"state": "pending", "context": "ci/smoke"}
        ]
    });

    assert!(!github_combined_status_payload_green(&empty_payload).unwrap());
    assert!(!github_combined_status_payload_green(&pending_payload).unwrap());
}

#[test]
fn github_status_is_transient_recognizes_rate_limit_responses() {
    assert!(github_status_is_transient(
        StatusCode::FORBIDDEN,
        r#"{"message":"You have exceeded a secondary rate limit."}"#
    ));
    assert!(github_status_is_transient(
        StatusCode::TOO_MANY_REQUESTS,
        r#"{"message":"API rate limit exceeded"}"#
    ));
    assert!(!github_status_is_transient(
        StatusCode::FORBIDDEN,
        r#"{"message":"Resource not accessible by integration"}"#
    ));
}

#[test]
fn github_retry_delay_seconds_retries_and_caps_rate_limit_hints() {
    assert_eq!(
        github_retry_delay_seconds(
            Some(60),
            Some(StatusCode::TOO_MANY_REQUESTS),
            Some("{}"),
            0,
            0,
        ),
        Some(60)
    );
    assert_eq!(
        github_retry_delay_seconds(
            Some(1_024),
            Some(StatusCode::TOO_MANY_REQUESTS),
            Some("{}"),
            0,
            0,
        ),
        None
    );
    assert_eq!(
        github_retry_delay_seconds(None, Some(StatusCode::BAD_GATEWAY), Some("{}"), 1, 2,),
        Some(4)
    );
}

#[test]
fn github_retry_after_hint_prefers_the_larger_header_or_body_value() {
    let mut headers = HeaderMap::new();
    headers.insert(RETRY_AFTER, "12".parse().expect("retry-after header"));

    assert_eq!(
        github_retry_after_hint(
            &headers,
            r#"{"error_extra":{"retry_after":30},"message":"secondary rate limit"}"#
        ),
        Some(30)
    );
}

#[test]
fn full_smoke_running_state_requires_an_active_merging_turn() {
    assert!(full_smoke_running_state_matches(
        "running",
        Some("Merging"),
        LIVE_E2E_MERGING_SECTION,
    ));
    assert!(!full_smoke_running_state_matches(
        "retrying",
        Some("Merging"),
        LIVE_E2E_MERGING_SECTION,
    ));
    assert!(!full_smoke_running_state_matches(
        "running",
        Some("Human Review"),
        LIVE_E2E_MERGING_SECTION,
    ));
}

#[test]
fn full_smoke_auto_review_blocker_ignores_copilot_overview_review() {
    let reviews = vec![json!({
        "author": {"login": "copilot-pull-request-reviewer[bot]"},
        "state": "COMMENTED",
        "body": "## Pull request overview\nLooks good."
    })];

    assert_eq!(full_smoke_auto_review_blocker(&[], &[], &reviews), None);
}

#[test]
fn full_smoke_auto_review_blocker_flags_requested_changes() {
    let reviews = vec![json!({
        "author": {"login": "human-reviewer"},
        "state": "CHANGES_REQUESTED",
        "body": "Please fix the failing edge case."
    })];

    let blocker = full_smoke_auto_review_blocker(&[], &[], &reviews)
        .expect("changes requested review should block auto approval");

    assert!(blocker.contains("CHANGES_REQUESTED"));
    assert!(blocker.contains("human-reviewer"));
}

#[test]
fn full_smoke_auto_review_blocker_flags_inline_review_comments() {
    let review_comments = vec![json!({
        "user": {"login": "human-reviewer"},
        "body": "Please rename this helper before merge."
    })];

    let blocker = full_smoke_auto_review_blocker(&[], &review_comments, &[])
        .expect("inline review comment should block auto approval");

    assert!(blocker.contains("inline review comment"));
    assert!(blocker.contains("human-reviewer"));
}
