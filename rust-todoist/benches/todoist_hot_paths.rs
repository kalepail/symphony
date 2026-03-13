use std::{collections::HashMap, fs, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State as AxumState},
    routing::{get, post},
};
use criterion::{
    BenchmarkId, Criterion, SamplingMode, Throughput, criterion_group, criterion_main,
};
use serde_json::{Value, json};
use symphony_rust_todoist::{
    tracker::{TrackerClient, todoist::TodoistTracker},
    workflow::WorkflowStore,
};
use tempfile::tempdir;
use tokio::{net::TcpListener, runtime::Runtime, task::JoinHandle};

#[derive(Clone)]
struct BenchTaskSet {
    candidate_tasks: Arc<Vec<Value>>,
    tasks_by_id: Arc<HashMap<String, Value>>,
}

struct TodoistBenchServer {
    base_url: String,
    join: JoinHandle<()>,
}

impl Drop for TodoistBenchServer {
    fn drop(&mut self) {
        self.join.abort();
    }
}

fn benchmark_config() -> Criterion {
    Criterion::default()
        .sample_size(20)
        .measurement_time(Duration::from_secs(10))
}

fn candidate_tasks() -> Vec<Value> {
    (0..100)
        .map(|index| {
            let state_section = if index % 2 == 0 {
                "sec-todo"
            } else {
                "sec-progress"
            };
            json!({
                "id": format!("task-{index}"),
                "content": format!("Task {index}"),
                "project_id": "proj",
                "section_id": state_section,
                "labels": ["runtime-owned"],
                "is_completed": false,
                "comment_count": 0,
                "created_at": "2026-03-13T00:00:00Z",
                "url": format!("https://app.todoist.com/app/task/task-{index}")
            })
        })
        .collect()
}

fn build_task_map() -> HashMap<String, Value> {
    candidate_tasks()
        .into_iter()
        .map(|task| {
            let id = task["id"].as_str().expect("task id").to_string();
            (id, task)
        })
        .collect()
}

async fn spawn_todoist_bench_server(tasks: BenchTaskSet) -> TodoistBenchServer {
    async fn get_project(Path(_project_id): Path<String>) -> Json<Value> {
        Json(json!({
            "id": "proj",
            "is_shared": false,
            "can_assign_tasks": false
        }))
    }

    async fn list_sections(Query(_query): Query<HashMap<String, String>>) -> Json<Value> {
        Json(json!({
            "results": [
                {"id": "sec-todo", "project_id": "proj", "name": "Todo"},
                {"id": "sec-progress", "project_id": "proj", "name": "In Progress"},
                {"id": "sec-done", "project_id": "proj", "name": "Done"}
            ],
            "next_cursor": null
        }))
    }

    async fn sync() -> Json<Value> {
        Json(json!({
            "user_plan_limits": {
                "current": {
                    "comments": true
                }
            }
        }))
    }

    async fn list_tasks(
        AxumState(tasks): AxumState<BenchTaskSet>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let results = if let Some(ids) = query.get("ids") {
            ids.split(',')
                .filter_map(|id| tasks.tasks_by_id.get(id).cloned())
                .collect::<Vec<_>>()
        } else {
            tasks.candidate_tasks.as_ref().clone()
        };
        Json(json!({
            "results": results,
            "next_cursor": null
        }))
    }

    let app = Router::new()
        .route("/projects/{project_id}", get(get_project))
        .route("/sections", get(list_sections))
        .route("/sync", post(sync))
        .route("/tasks", get(list_tasks))
        .with_state(tasks);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let join = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    TodoistBenchServer {
        base_url: format!("http://{address}"),
        join,
    }
}

fn workflow_config(base_url: &str) -> symphony_rust_todoist::config::ServiceConfig {
    let dir = tempdir().expect("tempdir");
    let workflow_path = dir.path().join("WORKFLOW.md");
    fs::write(
        &workflow_path,
        format!(
            r#"---
tracker:
  kind: todoist
  base_url: {base_url}
  api_key: token
  project_id: proj
  label: runtime-owned
  active_states:
    - Todo
    - In Progress
  terminal_states:
    - Done
workspace:
  root: {}
---

benchmark
"#,
            dir.path().join("workspaces").display()
        ),
    )
    .expect("write workflow");
    WorkflowStore::new(workflow_path)
        .expect("workflow store")
        .effective()
        .config
}

fn bench_fetch_candidate_issues_warm(c: &mut Criterion) {
    let runtime = Runtime::new().expect("tokio runtime");
    let tasks = BenchTaskSet {
        candidate_tasks: Arc::new(candidate_tasks()),
        tasks_by_id: Arc::new(build_task_map()),
    };
    let server = runtime.block_on(spawn_todoist_bench_server(tasks));
    let tracker = TodoistTracker::new(workflow_config(&server.base_url));

    runtime.block_on(async {
        tracker
            .fetch_candidate_issues()
            .await
            .expect("warm candidate fetch");
    });

    let mut group = c.benchmark_group("todoist/fetch_candidate_issues");
    group.sampling_mode(SamplingMode::Flat);
    group.bench_function("warm_100_tasks", |b| {
        b.to_async(&runtime).iter(|| async {
            let issues = tracker
                .fetch_candidate_issues()
                .await
                .expect("candidate issues");
            criterion::black_box(issues)
        });
    });
    group.finish();
}

fn bench_fetch_issue_states_by_ids_warm(c: &mut Criterion) {
    let runtime = Runtime::new().expect("tokio runtime");
    let tasks = BenchTaskSet {
        candidate_tasks: Arc::new(candidate_tasks()),
        tasks_by_id: Arc::new(build_task_map()),
    };
    let server = runtime.block_on(spawn_todoist_bench_server(tasks));
    let tracker = TodoistTracker::new(workflow_config(&server.base_url));
    let mut group = c.benchmark_group("todoist/fetch_issue_states_by_ids");
    group.sampling_mode(SamplingMode::Flat);

    for size in [1usize, 10, 100] {
        let issue_ids = (0..size)
            .map(|index| format!("task-{index}"))
            .collect::<Vec<_>>();
        runtime.block_on(async {
            tracker
                .fetch_issue_states_by_ids(&issue_ids)
                .await
                .expect("warm refresh fetch");
        });

        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size),
            &issue_ids,
            |b, issue_ids| {
                b.to_async(&runtime).iter(|| async {
                    let issues = tracker
                        .fetch_issue_states_by_ids(issue_ids)
                        .await
                        .expect("refresh issues");
                    criterion::black_box(issues)
                });
            },
        );
    }

    group.finish();
}

criterion_group! {
    name = todoist_hot_paths;
    config = benchmark_config();
    targets = bench_fetch_candidate_issues_warm, bench_fetch_issue_states_by_ids_warm
}
criterion_main!(todoist_hot_paths);
