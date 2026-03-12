use std::{env, fs, time::Duration};

use chrono::Utc;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use symphony_rust_todoist::{
    orchestrator::Orchestrator, workflow::WorkflowStore, workspace::workspace_path_for_identifier,
};
use tempfile::tempdir;
use tokio::time::sleep;

const RESULT_FILE: &str = "LIVE_E2E_RESULT.txt";

#[tokio::test]
#[ignore = "requires live Todoist and Codex access"]
async fn completes_a_real_todoist_task_end_to_end() {
    if env::var("SYMPHONY_RUN_LIVE_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping live e2e; set SYMPHONY_RUN_LIVE_E2E=1");
        return;
    }

    let token = match env::var("TODOIST_API_TOKEN") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("skipping live e2e; TODOIST_API_TOKEN is missing");
            return;
        }
    };
    let project_id = match env::var("SYMPHONY_SMOKE_PROJECT_ID") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => {
            eprintln!("skipping live e2e; SYMPHONY_SMOKE_PROJECT_ID is missing");
            return;
        }
    };

    let codex_ready = std::process::Command::new("codex")
        .arg("--version")
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    assert!(
        codex_ready,
        "`codex` must be on PATH for rust-todoist live e2e"
    );

    let client = Client::new();
    let base_url = env::var("TODOIST_API_BASE_URL")
        .unwrap_or_else(|_| "https://api.todoist.com/api/v1".to_string());
    let todo_section_id = todo_section_id(&client, &base_url, &token, &project_id).await;
    let run_id = format!(
        "symphony-rust-todoist-live-e2e-{}",
        Utc::now().timestamp_millis()
    );
    let task = create_task(
        &client,
        &base_url,
        &token,
        &project_id,
        &todo_section_id,
        &run_id,
    )
    .await;
    let task_id = task_id(&task);
    let identifier = format!("TD-{task_id}");
    let expected_result = format!("identifier={identifier}\nproject_id={project_id}\n");
    let expected_comment = format!(
        "Symphony rust-todoist live e2e comment\nidentifier={identifier}\nproject_id={project_id}"
    );

    let dir = tempdir().expect("tempdir");
    let workspace_root = dir.path().join("workspaces");
    fs::create_dir_all(&workspace_root).expect("workspace root");
    let workflow_path = dir.path().join("WORKFLOW.md");
    fs::write(
        &workflow_path,
        format!(
            r#"---
tracker:
  kind: todoist
  api_key: $TODOIST_API_TOKEN
  project_id: $SYMPHONY_SMOKE_PROJECT_ID
  active_states:
    - Todo
  terminal_states:
    - Done
polling:
  interval_ms: 1000
workspace:
  root: {}
observability:
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
Use the `todoist` tool to read the current task `{{{{ issue.id }}}}` and its task comments.
If the exact comment body below is not already present, post exactly one task comment with this exact body:

{expected_comment}

Step 3:
Close the current task with the `todoist` tool.

Step 4:
Before stopping, verify all three outcomes:
1. the file exists with the exact contents above
2. the exact Todoist task comment exists
3. the current task has been closed

Do not ask for approval.
"#,
            workspace_root.display(),
            result_file = RESULT_FILE,
            project_id = project_id,
            expected_comment = expected_comment,
        ),
    )
    .expect("workflow");

    let workflow_store = WorkflowStore::new(workflow_path.clone()).expect("workflow store");
    let workspace_path =
        workspace_path_for_identifier(&workflow_store.effective().config, &identifier);
    let orchestrator = Orchestrator::start(workflow_store.clone())
        .await
        .expect("orchestrator");

    let started = tokio::time::Instant::now();
    let timeout = Duration::from_secs(300);
    let mut observed_comment = false;
    let mut observed_result = false;
    let mut completed = false;

    while started.elapsed() < timeout {
        if !observed_result {
            observed_result = fs::read_to_string(workspace_path.join(RESULT_FILE))
                .ok()
                .is_some_and(|content| content == expected_result);
        }
        if !observed_comment {
            observed_comment =
                task_comments_contain(&client, &base_url, &token, &task_id, &expected_comment)
                    .await;
        }
        if !completed {
            completed = task_is_closed(&client, &base_url, &token, &task_id).await;
        }
        if observed_result && observed_comment && completed {
            break;
        }
        sleep(Duration::from_secs(2)).await;
    }

    orchestrator.shutdown().await;

    if !completed {
        let _ = close_task(&client, &base_url, &token, &task_id).await;
    }

    assert!(observed_result, "expected result file was not produced");
    assert!(
        observed_comment,
        "expected Todoist task comment was not observed"
    );
    assert!(completed, "expected Todoist task to be closed");
}

fn task_id(task: &Value) -> String {
    task.get("id")
        .and_then(|value| match value {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
        .expect("todoist task id")
}

async fn todo_section_id(client: &Client, base_url: &str, token: &str, project_id: &str) -> String {
    let sections = todoist_request(
        client,
        base_url,
        token,
        Method::GET,
        "/sections",
        Some(&[
            ("project_id".to_string(), project_id.to_string()),
            ("limit".to_string(), "200".to_string()),
        ]),
        None,
    )
    .await;
    sections
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| {
            results.iter().find_map(|section| {
                let name = section.get("name").and_then(Value::as_str)?;
                if name.eq_ignore_ascii_case("Todo") {
                    section.get("id").and_then(|value| match value {
                        Value::String(text) => Some(text.clone()),
                        Value::Number(number) => Some(number.to_string()),
                        _ => None,
                    })
                } else {
                    None
                }
            })
        })
        .expect("Todo section must exist in the live Todoist smoke project")
}

async fn create_task(
    client: &Client,
    base_url: &str,
    token: &str,
    project_id: &str,
    section_id: &str,
    run_id: &str,
) -> Value {
    todoist_request(
        client,
        base_url,
        token,
        Method::POST,
        "/tasks",
        None,
        Some(json!({
            "content": format!("Symphony rust-todoist live e2e {run_id}"),
            "description": format!("Run id: {run_id}"),
            "project_id": project_id,
            "section_id": section_id
        })),
    )
    .await
}

async fn task_comments_contain(
    client: &Client,
    base_url: &str,
    token: &str,
    task_id: &str,
    expected_comment: &str,
) -> bool {
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
    .await;
    comments
        .get("results")
        .and_then(Value::as_array)
        .is_some_and(|results| {
            results.iter().any(|comment| {
                comment.get("content").and_then(Value::as_str) == Some(expected_comment)
            })
        })
}

async fn task_is_closed(client: &Client, base_url: &str, token: &str, task_id: &str) -> bool {
    todoist_request_optional(
        client,
        base_url,
        token,
        Method::GET,
        &format!("/tasks/{task_id}"),
        None,
        None,
    )
    .await
    .is_none()
}

async fn close_task(client: &Client, base_url: &str, token: &str, task_id: &str) -> Option<Value> {
    todoist_request_optional(
        client,
        base_url,
        token,
        Method::POST,
        &format!("/tasks/{task_id}/close"),
        None,
        None,
    )
    .await
}

async fn todoist_request(
    client: &Client,
    base_url: &str,
    token: &str,
    method: Method,
    path: &str,
    query: Option<&[(String, String)]>,
    body: Option<Value>,
) -> Value {
    todoist_request_optional(client, base_url, token, method, path, query, body)
        .await
        .expect("todoist response")
}

async fn todoist_request_optional(
    client: &Client,
    base_url: &str,
    token: &str,
    method: Method,
    path: &str,
    query: Option<&[(String, String)]>,
    body: Option<Value>,
) -> Option<Value> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    let mut request = client
        .request(method, &url)
        .bearer_auth(token)
        .header("Accept", "application/json");

    if let Some(query) = query {
        request = request.query(query);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }

    let response = request.send().await.expect("todoist request");
    let status = response.status();
    let text = response.text().await.expect("todoist body");
    if status == StatusCode::NOT_FOUND {
        return None;
    }
    if !status.is_success() {
        panic!(
            "Todoist request failed with HTTP {}: {}",
            status.as_u16(),
            text
        );
    }
    if text.trim().is_empty() {
        Some(Value::Null)
    } else {
        Some(serde_json::from_str(&text).expect("todoist json"))
    }
}
