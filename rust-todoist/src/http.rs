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
    observability::{Presenter, StatePayload, render_dashboard_html},
    orchestrator::OrchestratorHandle,
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
    join: tokio::task::JoinHandle<()>,
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
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { join, local_addr })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn abort(self) {
        self.join.abort();
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
    match present_state(&state).await {
        Some(payload) => Html(render_dashboard_html(&payload)).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "code": "snapshot_unavailable", "message": "snapshot unavailable" } })),
        )
            .into_response(),
    }
}

async fn api_state(State(state): State<AppState>) -> impl IntoResponse {
    match present_state(&state).await {
        Some(payload) => Json(payload).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": { "code": "snapshot_unavailable", "message": "snapshot unavailable" } })),
        )
            .into_response(),
    }
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

            let payload = present_state(&state).await?;
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
        Some(detail) => Json(Presenter::present_issue_detail(detail)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "code": "issue_not_found", "message": "issue not found" } })),
        )
            .into_response(),
    }
}

async fn api_refresh(State(state): State<AppState>) -> impl IntoResponse {
    match state.orchestrator.refresh().await {
        Some(response) => (StatusCode::ACCEPTED, Json(response)).into_response(),
        None => (
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

async fn present_state(state: &AppState) -> Option<StatePayload> {
    let snapshot = state.orchestrator.snapshot().await?;
    let mut presenter = state
        .presenter
        .lock()
        .expect("observability presenter poisoned");
    Some(presenter.present_state(snapshot, &state.workflow_store, Some(state.dashboard_addr)))
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

    use crate::{observability::Presenter, orchestrator::Orchestrator, workflow::WorkflowStore};

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
  "sections": [{"id":"sec-todo","project_id":"proj","name":"Todo"}],
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
