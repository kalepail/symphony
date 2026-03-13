use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{
        Html, IntoResponse,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::stream;
use serde_json::json;
use tokio::net::TcpListener;

use crate::{
    observability::{Presenter, SnapshotStatus, StatePayload, render_dashboard_html},
    orchestrator::{OrchestratorHandle, OrchestratorHandleError},
    workflow::WorkflowStore,
};

const ISSUE_ROUTE: &str = "/api/v1/{identifier}";

#[derive(Clone)]
struct AppState {
    orchestrator: OrchestratorHandle,
    workflow_store: WorkflowStore,
    dashboard_addr: SocketAddr,
    presenter: Arc<Mutex<Presenter>>,
}

pub struct HttpServer {
    join: Option<tokio::task::JoinHandle<Result<(), String>>>,
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
            dashboard_addr: local_addr,
            presenter: Arc::new(Mutex::new(Presenter::default())),
        });

        let join = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .map_err(|error| error.to_string())
        });
        Ok(Self {
            join: Some(join),
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn is_finished(&self) -> bool {
        self.join
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    }

    pub async fn wait_for_exit(&mut self) -> Result<(), String> {
        match self.join.take() {
            Some(join) => match join.await {
                Ok(result) => result,
                Err(error) => Err(error.to_string()),
            },
            None => Ok(()),
        }
    }

    pub async fn shutdown(mut self) {
        if let Some(join) = self.join.take() {
            join.abort();
            let _ = join.await;
        }
    }
}

fn app_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard).fallback(api_method_not_allowed))
        .route(
            "/api/v1/state",
            get(api_state).fallback(api_method_not_allowed),
        )
        .route(
            "/api/v1/stream",
            get(api_stream).fallback(api_method_not_allowed),
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
    let payload = present_state(&state).await;
    (
        state_payload_status(&payload),
        Html(render_dashboard_html(&payload)),
    )
        .into_response()
}

async fn api_state(State(state): State<AppState>) -> impl IntoResponse {
    let payload = present_state(&state).await;
    (state_payload_status(&payload), Json(payload)).into_response()
}

async fn api_stream(
    State(state): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let updates = state.orchestrator.subscribe_observability();
    let stream = stream::unfold(
        (state, updates, false),
        |(state, mut updates, mut sent_initial)| async move {
            if sent_initial {
                if updates.changed().await.is_err() {
                    return None;
                }
            } else {
                sent_initial = true;
            }

            let payload = present_state(&state).await;
            let event = Event::default()
                .event("state")
                .data(serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string()));
            Some((Ok(event), (state, updates, sent_initial)))
        },
    );

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text("keep-alive"),
    )
}

async fn api_issue(
    State(state): State<AppState>,
    AxumPath(identifier): AxumPath<String>,
) -> impl IntoResponse {
    match state.orchestrator.issue_detail(identifier).await {
        Ok(Some(detail)) => Json(Presenter::present_issue_detail(detail)).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "code": "issue_not_found", "message": "issue not found" } })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "code": "issue_unavailable", "message": "issue detail unavailable" } })),
        )
            .into_response(),
    }
}

async fn api_refresh(State(state): State<AppState>) -> impl IntoResponse {
    match state.orchestrator.refresh().await {
        Ok(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        Err(_) => (
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

async fn present_state(state: &AppState) -> StatePayload {
    let mut presenter = state
        .presenter
        .lock()
        .expect("observability presenter poisoned");
    match state.orchestrator.cached_snapshot() {
        Ok(cached) => presenter.present_cached_state(
            cached,
            &state.workflow_store,
            Some(state.dashboard_addr),
        ),
        Err(error) => present_snapshot_failure(
            &mut presenter,
            &state.workflow_store,
            state.dashboard_addr,
            error,
        ),
    }
}

fn present_snapshot_failure(
    presenter: &mut Presenter,
    workflow_store: &WorkflowStore,
    dashboard_addr: SocketAddr,
    error: OrchestratorHandleError,
) -> StatePayload {
    presenter.present_snapshot_failure(error, workflow_store, Some(dashboard_addr))
}

fn state_payload_status(payload: &StatePayload) -> StatusCode {
    if payload.snapshot_status == SnapshotStatus::Unavailable {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header},
        routing::get,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::{
        observability::{Presenter, SnapshotStatus},
        orchestrator::Orchestrator,
        workflow::WorkflowStore,
    };

    use super::{AppState, ISSUE_ROUTE, app_router};

    #[test]
    fn issue_route_uses_supported_capture_syntax() {
        let _app: Router<()> = Router::new().route(ISSUE_ROUTE, get(|| async {}));
    }

    #[tokio::test]
    async fn api_state_includes_observability_links_and_throughput() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let app = app_router(AppState {
            orchestrator: orchestrator.handle(),
            workflow_store: workflow_store.clone(),
            dashboard_addr: "127.0.0.1:4000".parse().expect("addr"),
            presenter: Arc::new(Mutex::new(Presenter::default())),
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
        assert_eq!(
            payload["links"]["project_url"],
            "https://app.todoist.com/app/project/proj"
        );
        assert_eq!(payload["links"]["dashboard_url"], "http://127.0.0.1:4000/");
        assert!(payload["throughput"]["graph_10m"].is_string());
        assert!(payload["throughput"]["tps_5s"].is_number());

        orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn api_state_preserves_snapshot_unavailable_error_payload() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let handle = orchestrator.handle();
        orchestrator.shutdown().await;
        let app = app_router(AppState {
            orchestrator: handle,
            workflow_store: workflow_store.clone(),
            dashboard_addr: "127.0.0.1:4000".parse().expect("addr"),
            presenter: Arc::new(Mutex::new(Presenter::default())),
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

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(payload["error"]["code"], "snapshot_unavailable");
        assert_eq!(
            payload["error"]["message"],
            "Orchestrator snapshot unavailable"
        );
    }

    #[tokio::test]
    async fn dashboard_renders_html_shell_when_snapshot_is_unavailable() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let handle = orchestrator.handle();
        orchestrator.shutdown().await;
        let app = app_router(AppState {
            orchestrator: handle,
            workflow_store: workflow_store.clone(),
            dashboard_addr: "127.0.0.1:4000".parse().expect("addr"),
            presenter: Arc::new(Mutex::new(Presenter::default())),
        });

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/html; charset=utf-8")
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let html = String::from_utf8(body.to_vec()).expect("utf8");
        assert!(html.contains("id=\"error-card\""));
        assert!(html.contains("snapshot_unavailable"));
        assert!(html.contains("Todoist Operations Dashboard"));
    }

    #[test]
    fn state_payload_status_keeps_stale_snapshots_on_http_200() {
        let payload = crate::observability::StatePayload {
            generated_at: chrono::Utc::now(),
            snapshot_status: SnapshotStatus::Stale,
            snapshot_age_ms: Some(8_000),
            error: Some(crate::observability::ErrorPayload {
                code: "snapshot_stale",
                message: "Using stale orchestrator snapshot",
            }),
            counts: crate::orchestrator::SnapshotCounts {
                running: 0,
                retrying: 0,
            },
            agent_limits: crate::observability::AgentLimitsPayload {
                max_concurrent_agents: 1,
            },
            running: Vec::new(),
            retrying: Vec::new(),
            codex_totals: crate::observability::CodexTotalsPayload {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                seconds_running: 0.0,
            },
            rate_limits: None,
            todoist_rate_budget: None,
            polling: crate::orchestrator::PollingSnapshot {
                checking: true,
                next_poll_in_ms: None,
                poll_interval_ms: 5_000,
            },
            workflow: crate::observability::WorkflowPayload {
                path: "WORKFLOW.md".to_string(),
                dispatch_status: "ready",
                blocking_reason: None,
                using_last_good: false,
            },
            links: crate::observability::LinksPayload {
                project_url: None,
                dashboard_url: None,
            },
            throughput: crate::observability::ThroughputPayload {
                tps_5s: 0.0,
                graph_10m: String::new(),
            },
        };

        assert_eq!(super::state_payload_status(&payload), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_stream_returns_sse_content_type_and_initial_event() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let app = app_router(AppState {
            orchestrator: orchestrator.handle(),
            workflow_store: workflow_store.clone(),
            dashboard_addr: "127.0.0.1:4000".parse().expect("addr"),
            presenter: Arc::new(Mutex::new(Presenter::default())),
        });

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let mut body = response.into_body();
        let text = read_sse_event(&mut body).await;
        assert!(text.contains("data:"));
        assert!(text.contains("\"workflow\""));

        orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn api_stream_emits_follow_up_after_refresh() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let handle = orchestrator.handle();
        let app = app_router(AppState {
            orchestrator: handle.clone(),
            workflow_store: workflow_store.clone(),
            dashboard_addr: "127.0.0.1:4000".parse().expect("addr"),
            presenter: Arc::new(Mutex::new(Presenter::default())),
        });

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/stream")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        let mut body = response.into_body();
        let _first = read_sse_event(&mut body).await;

        let refresh_response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("refresh response");
        assert_eq!(refresh_response.status(), StatusCode::ACCEPTED);

        let text = read_sse_event(&mut body).await;
        assert!(text.contains("data:"));
        assert!(text.contains("\"generated_at\""));

        orchestrator.shutdown().await;
    }

    #[tokio::test]
    async fn api_refresh_rejects_get_with_method_not_allowed_payload() {
        let (orchestrator, workflow_store) = test_runtime(false).await;
        let app = app_router(AppState {
            orchestrator: orchestrator.handle(),
            workflow_store: workflow_store.clone(),
            dashboard_addr: "127.0.0.1:4000".parse().expect("addr"),
            presenter: Arc::new(Mutex::new(Presenter::default())),
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
            dashboard_addr: "127.0.0.1:4000".parse().expect("addr"),
            presenter: Arc::new(Mutex::new(Presenter::default())),
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

    async fn test_runtime(invalidate_workflow: bool) -> (Orchestrator, WorkflowStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture_path = dir.path().join("memory.json");
        std::fs::write(
            &fixture_path,
            r#"{
  "tasks": [],
  "sections": [
    {"id":"sec-todo","project_id":"proj","name":"Todo"},
    {"id":"sec-in-progress","project_id":"proj","name":"In Progress"}
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
workspace:
  root: /tmp/symphony-http-tests
---

test
"#,
                fixture_path.display()
            ),
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

    async fn read_sse_event(body: &mut Body) -> String {
        let mut bytes = Vec::new();
        loop {
            let frame = body.frame().await.expect("frame").expect("body frame");
            if let Ok(data) = frame.into_data() {
                bytes.extend_from_slice(&data);
                if bytes.ends_with(b"\n\n") || bytes.ends_with(b"\r\n\r\n") {
                    return String::from_utf8(bytes).expect("utf8");
                }
            }
        }
    }
}
