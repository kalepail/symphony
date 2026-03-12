use reqwest::Method;
use serde_json::{Value, json};
use std::{env, process::Command as StdCommand, sync::OnceLock};
use tokio::process::Command;

use crate::{
    config::ServiceConfig,
    tracker::{TrackerClient, TrackerError},
};

pub const GITHUB_API_TOOL: &str = "github_api";
pub const LINEAR_GRAPHQL_TOOL: &str = "linear_graphql";
const GITHUB_API_BASE_URL_ENV: &str = "SYMPHONY_GITHUB_API_URL";
const GITHUB_API_DEFAULT_BASE_URL: &str = "https://api.github.com";
const GITHUB_API_TOOL_DESCRIPTION: &str = concat!(
    "Execute a GitHub REST API request using host-side auth from Symphony's runtime.\n",
    "Use this when `gh` transport is flaky inside Codex sessions but the host can still reach GitHub.\n",
    "Create pull requests with POST `/repos/{owner}/{repo}/pulls` and JSON body fields `title`, `head`, `base`, `body`.\n",
    "Add labels with POST `/repos/{owner}/{repo}/issues/{number}/labels` and body `{ \"labels\": [\"symphony\"] }`.\n",
    "Read pull requests with GET `/repos/{owner}/{repo}/pulls/{number}`.\n",
    "Read CI checks with GET `/repos/{owner}/{repo}/commits/{sha}/check-runs`."
);
const LINEAR_GRAPHQL_TOOL_DESCRIPTION: &str = concat!(
    "Execute a raw GraphQL query or mutation against Linear using Symphony's configured auth.\n",
    "Use one narrow operation per call.\n",
    "When you have a ticket key like `SDF-6`, query `issue(id: $id)` directly; Linear accepts the human identifier there.\n",
    "Create comments with `commentCreate(input: { issueId: $issueId, body: $body })`.\n",
    "Move state by first querying `issue(id: $id) { team { states { nodes { id name type } } } }`, then calling `issueUpdate(id: $id, input: { stateId: $stateId })`.\n",
    "Avoid broad search or schema introspection unless these direct recipes fail."
);

pub fn tool_specs(config: &ServiceConfig) -> Vec<Value> {
    let mut specs = Vec::new();

    if config.tracker.kind.as_deref() == Some("linear") && config.tracker.api_key.is_some() {
        specs.push(json!({
            "name": LINEAR_GRAPHQL_TOOL,
            "description": LINEAR_GRAPHQL_TOOL_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "GraphQL query or mutation document to execute against Linear."
                    },
                    "variables": {
                        "type": ["object", "null"],
                        "description": "Optional GraphQL variables object.",
                        "additionalProperties": true
                    }
                }
            }
        }));
    }

    if github_api_available() {
        specs.push(json!({
            "name": GITHUB_API_TOOL,
            "description": GITHUB_API_TOOL_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["method", "path"],
                "properties": {
                    "method": {
                        "type": "string",
                        "description": "HTTP method to send to the GitHub REST API (GET, POST, PATCH, PUT, DELETE)."
                    },
                    "path": {
                        "type": "string",
                        "description": "API path beginning with `/`, for example `/repos/owner/repo/pulls`."
                    },
                    "body": {
                        "description": "Optional JSON body for POST/PATCH/PUT requests."
                    }
                }
            }
        }));
    }

    specs
}

pub async fn execute(
    config: &ServiceConfig,
    tracker: &dyn TrackerClient,
    tool: &str,
    arguments: Value,
) -> Value {
    match tool {
        LINEAR_GRAPHQL_TOOL => {
            if config.tracker.kind.as_deref() != Some("linear") {
                return failure_payload(json!({
                    "error": {
                        "message": "Symphony is not configured with tracker.kind=linear for this session."
                    }
                }));
            }

            match normalize_arguments(arguments) {
                Ok((query, variables)) => match tracker.raw_graphql(&query, variables).await {
                    Ok(body) => success_payload(body),
                    Err(error) => failure_payload(tool_error_payload(error)),
                },
                Err(error) => failure_payload(error),
            }
        }
        GITHUB_API_TOOL => match normalize_github_arguments(arguments) {
            Ok((method, path, body)) => match execute_github_api(method, path, body).await {
                Ok(body) => success_payload(body),
                Err(error) => failure_payload(error),
            },
            Err(error) => failure_payload(error),
        },
        _ => failure_payload(json!({
            "error": {
                "message": format!("Unsupported dynamic tool: {tool}."),
                "supportedTools": [LINEAR_GRAPHQL_TOOL, GITHUB_API_TOOL]
            }
        })),
    }
}

fn normalize_arguments(arguments: Value) -> Result<(String, Value), Value> {
    match arguments {
        Value::String(query) => {
            let query = query.trim();
            if query.is_empty() {
                return Err(json!({
                    "error": {
                        "message": "`linear_graphql` requires a non-empty `query` string."
                    }
                }));
            }
            Ok((query.to_string(), json!({})))
        }
        Value::Object(map) => {
            let query = map
                .get("query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    json!({
                        "error": {
                            "message": "`linear_graphql` requires a non-empty `query` string."
                        }
                    })
                })?;
            let variables = map.get("variables").cloned().unwrap_or_else(|| json!({}));
            if !variables.is_object() {
                return Err(json!({
                    "error": {
                        "message": "`linear_graphql.variables` must be a JSON object when provided."
                    }
                }));
            }
            Ok((query.to_string(), variables))
        }
        _ => Err(json!({
            "error": {
                "message": "`linear_graphql` expects a GraphQL string or an object with `query` and optional `variables`."
            }
        })),
    }
}

fn success_payload(body: Value) -> Value {
    let success = body
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| errors.is_empty())
        .unwrap_or(true);

    json!({
        "success": success,
        "contentItems": [
            {
                "type": "inputText",
                "text": serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
            }
        ]
    })
}

fn failure_payload(body: Value) -> Value {
    json!({
        "success": false,
        "contentItems": [
            {
                "type": "inputText",
                "text": serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string())
            }
        ]
    })
}

fn github_api_available() -> bool {
    github_token_from_env().is_some() || gh_cli_available()
}

fn gh_cli_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        StdCommand::new("gh")
            .arg("--version")
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

fn github_api_base_url() -> String {
    env::var(GITHUB_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| GITHUB_API_DEFAULT_BASE_URL.to_string())
}

fn github_token_from_env() -> Option<String> {
    env::var("GH_TOKEN")
        .ok()
        .or_else(|| env::var("GITHUB_TOKEN").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn github_auth_token() -> Result<String, Value> {
    if let Some(token) = github_token_from_env() {
        return Ok(token);
    }

    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
        .map_err(|error| {
            json!({
                "error": {
                    "message": "GitHub API tool is unavailable because no auth token could be found.",
                    "reason": error.to_string()
                }
            })
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(json!({
            "error": {
                "message": "GitHub API tool could not retrieve a host auth token from `gh auth token`.",
                "reason": stderr
            }
        }));
    }

    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(json!({
            "error": {
                "message": "GitHub API tool received an empty auth token."
            }
        }));
    }

    Ok(token)
}

fn normalize_github_arguments(arguments: Value) -> Result<(Method, String, Option<Value>), Value> {
    let map = arguments.as_object().ok_or_else(|| {
        json!({
            "error": {
                "message": "`github_api` expects an object with `method`, `path`, and optional `body`."
            }
        })
    })?;

    let method = map
        .get("method")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            json!({
                "error": {
                    "message": "`github_api.method` is required."
                }
            })
        })?;
    let method = Method::from_bytes(method.to_ascii_uppercase().as_bytes()).map_err(|error| {
        json!({
            "error": {
                "message": "`github_api.method` must be a valid HTTP method.",
                "reason": error.to_string()
            }
        })
    })?;

    let path = map
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            json!({
                "error": {
                    "message": "`github_api.path` is required."
                }
            })
        })?;
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    let body = match map.get("body") {
        Some(Value::Null) | None => None,
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                Some(parsed)
            } else {
                Some(Value::String(raw.clone()))
            }
        }
        Some(value) => Some(value.clone()),
    };

    Ok((method, path, body))
}

async fn execute_github_api(
    method: Method,
    path: String,
    body: Option<Value>,
) -> Result<Value, Value> {
    let token = github_auth_token().await?;
    let url = format!("{}{}", github_api_base_url(), path);
    let client = reqwest::Client::builder()
        .user_agent("symphony-rust/github_api")
        .build()
        .map_err(|error| {
            json!({
                "error": {
                    "message": "GitHub API tool failed to build an HTTP client.",
                    "reason": error.to_string()
                }
            })
        })?;

    let mut request = client
        .request(method.clone(), &url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(body) = body {
        request = request.json(&body);
    }

    let response = request.send().await.map_err(|error| {
        json!({
            "error": {
                "message": "GitHub API request failed before receiving a response.",
                "reason": error.to_string(),
                "method": method.as_str(),
                "path": path
            }
        })
    })?;

    let status = response.status();
    let body_text = response.text().await.map_err(|error| {
        json!({
            "error": {
                "message": "GitHub API response body could not be read.",
                "reason": error.to_string(),
                "method": method.as_str(),
                "path": path
            }
        })
    })?;

    if !status.is_success() {
        return Err(json!({
            "error": {
                "message": format!("GitHub API request failed with HTTP {}.", status.as_u16()),
                "status": status.as_u16(),
                "method": method.as_str(),
                "path": path,
                "response": parse_error_body(&body_text)
            }
        }));
    }

    Ok(parse_error_body(&body_text))
}

fn tool_error_payload(error: TrackerError) -> Value {
    match error {
        TrackerError::MissingTrackerApiKey => json!({
            "error": {
                "message": "Symphony is missing Linear auth. Set tracker.api_key in WORKFLOW.md or export LINEAR_API_KEY."
            }
        }),
        TrackerError::LinearApiStatus { status, body } => {
            let parsed_body = parse_error_body(&body);
            json!({
                "error": {
                    "message": format!("Linear GraphQL request failed with HTTP {status}."),
                    "status": status,
                    "response": parsed_body
                }
            })
        }
        TrackerError::LinearApiRequest(reason) => json!({
            "error": {
                "message": "Linear GraphQL request failed before receiving a successful response.",
                "reason": reason
            }
        }),
        TrackerError::LinearGraphqlErrors(body) => json!({
            "error": {
                "message": "Linear GraphQL request returned GraphQL errors.",
                "response": parse_error_body(&body)
            }
        }),
        other => json!({
            "error": {
                "message": "Linear GraphQL tool execution failed.",
                "reason": other.to_string()
            }
        }),
    }
}

fn parse_error_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|_| Value::String(body.to_string()))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use axum::{
        Json, Router, extract::State, http::StatusCode as HttpStatusCode, response::IntoResponse,
        routing::any,
    };
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        env,
        sync::{Arc, Mutex as StdMutex, OnceLock},
    };
    use tokio::{net::TcpListener, sync::Mutex as AsyncMutex, task::JoinHandle};

    use crate::{
        config::ServiceConfig,
        issue::Issue,
        tracker::{TrackerClient, TrackerError},
    };

    use super::{GITHUB_API_TOOL, LINEAR_GRAPHQL_TOOL, execute, tool_specs};

    struct StubTracker;
    struct ErrorTracker {
        error: TrackerError,
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

        async fn raw_graphql(
            &self,
            _query: &str,
            _variables: serde_json::Value,
        ) -> Result<serde_json::Value, TrackerError> {
            Ok(json!({ "data": { "ok": true } }))
        }
    }

    #[async_trait]
    impl TrackerClient for ErrorTracker {
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

        async fn raw_graphql(
            &self,
            _query: &str,
            _variables: serde_json::Value,
        ) -> Result<serde_json::Value, TrackerError> {
            Err(self.error.clone())
        }
    }

    fn linear_config() -> ServiceConfig {
        ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "api_key": "token",
                    "project_slug": "proj"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config")
    }

    fn env_lock() -> &'static AsyncMutex<()> {
        static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| AsyncMutex::new(()))
    }

    struct ScopedEnvVar {
        key: &'static str,
        original: Option<String>,
    }

    impl ScopedEnvVar {
        fn set(key: &'static str, value: &str) -> Self {
            let original = env::var(key).ok();
            unsafe {
                env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(value) => env::set_var(self.key, value),
                    None => env::remove_var(self.key),
                }
            }
        }
    }

    #[tokio::test]
    async fn accepts_multiple_operations() {
        let config = linear_config();

        let payload = execute(
            &config,
            &StubTracker,
            LINEAR_GRAPHQL_TOOL,
            json!({
                "query": "query A { viewer { id } } query B { viewer { id } }"
            }),
        )
        .await;

        assert_eq!(
            payload.get("success").and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn accepts_raw_query_string_arguments() {
        let config = linear_config();

        let payload = execute(
            &config,
            &StubTracker,
            LINEAR_GRAPHQL_TOOL,
            json!("query Viewer { viewer { id } }"),
        )
        .await;

        assert_eq!(
            payload.get("success").and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn hides_linear_tool_without_linear_auth() {
        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "linear",
                    "project_slug": "proj"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let specs = tool_specs(&config);
        let tool_names = specs
            .iter()
            .filter_map(|spec| spec.get("name").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert!(!tool_names.contains(&LINEAR_GRAPHQL_TOOL));
    }

    #[test]
    fn tool_description_includes_linear_recipes() {
        let config = linear_config();
        let specs = tool_specs(&config);
        let description = specs
            .iter()
            .find(|spec| spec.get("name") == Some(&json!(LINEAR_GRAPHQL_TOOL)))
            .and_then(|spec| spec.get("description"))
            .and_then(serde_json::Value::as_str)
            .expect("description");

        assert!(description.contains("issue(id: $id)"));
        assert!(description.contains("commentCreate"));
        assert!(description.contains("issueUpdate"));
    }

    #[tokio::test]
    async fn returns_http_error_response_body_to_model() {
        let config = linear_config();
        let tracker = ErrorTracker {
            error: TrackerError::LinearApiStatus {
                status: 400,
                body: r#"{"errors":[{"message":"Field \"identifier\" is not defined by type \"IssueFilter\"."}]}"#
                    .to_string(),
            },
        };

        let payload = execute(
            &config,
            &tracker,
            LINEAR_GRAPHQL_TOOL,
            json!({
                "query": "query Broken { issue(id: \"SDF-6\") { id } }"
            }),
        )
        .await;

        let rendered = payload
            .get("contentItems")
            .and_then(serde_json::Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(serde_json::Value::as_str)
            .expect("rendered payload");

        assert_eq!(
            payload.get("success").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(rendered.contains("\"status\": 400"));
        assert!(rendered.contains("IssueFilter"));
    }

    #[tokio::test]
    async fn exposes_github_api_tool_when_token_is_present() {
        let _guard = env_lock().lock().await;
        let _token = ScopedEnvVar::set("GH_TOKEN", "token");

        let config = linear_config();
        let specs = tool_specs(&config);
        let tool_names = specs
            .iter()
            .filter_map(|spec| spec.get("name").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();

        assert!(tool_names.contains(&GITHUB_API_TOOL));
    }

    #[tokio::test]
    async fn github_api_executes_host_side_request() {
        let _guard = env_lock().lock().await;
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            HttpStatusCode::CREATED,
            json!({ "html_url": "https://github.com/kalepail/symphony-smoke-lab/pull/2" }),
        )])
        .await;
        let _token = ScopedEnvVar::set("GH_TOKEN", "token");
        let _base_url = ScopedEnvVar::set("SYMPHONY_GITHUB_API_URL", &server.endpoint());

        let payload = execute(
            &linear_config(),
            &StubTracker,
            GITHUB_API_TOOL,
            json!({
                "method": "POST",
                "path": "/repos/kalepail/symphony-smoke-lab/pulls",
                "body": {
                    "title": "test"
                }
            }),
        )
        .await;

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/repos/kalepail/symphony-smoke-lab/pulls");
        assert_eq!(requests[0].body, json!({ "title": "test" }));
        assert_eq!(requests[0].authorization.as_deref(), Some("Bearer token"));
        assert_eq!(
            payload.get("success").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        server.abort();
    }

    #[tokio::test]
    async fn github_api_returns_error_payload_for_non_success_status() {
        let _guard = env_lock().lock().await;
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            HttpStatusCode::UNPROCESSABLE_ENTITY,
            json!({ "message": "Validation Failed" }),
        )])
        .await;
        let _token = ScopedEnvVar::set("GH_TOKEN", "token");
        let _base_url = ScopedEnvVar::set("SYMPHONY_GITHUB_API_URL", &server.endpoint());

        let payload = execute(
            &linear_config(),
            &StubTracker,
            GITHUB_API_TOOL,
            json!({
                "method": "POST",
                "path": "/repos/kalepail/symphony-smoke-lab/pulls",
                "body": {
                    "title": "test"
                }
            }),
        )
        .await;

        let rendered = payload
            .get("contentItems")
            .and_then(serde_json::Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(serde_json::Value::as_str)
            .expect("rendered payload");

        assert_eq!(
            payload.get("success").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(rendered.contains("\"status\": 422"));
        assert!(rendered.contains("Validation Failed"));
        server.abort();
    }

    #[tokio::test]
    async fn github_api_parses_stringified_json_object_body() {
        let _guard = env_lock().lock().await;
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            HttpStatusCode::OK,
            json!({ "ok": true }),
        )])
        .await;
        let _token = ScopedEnvVar::set("GH_TOKEN", "token");
        let _base_url = ScopedEnvVar::set("SYMPHONY_GITHUB_API_URL", &server.endpoint());

        let payload = execute(
            &linear_config(),
            &StubTracker,
            GITHUB_API_TOOL,
            json!({
                "method": "POST",
                "path": "/repos/kalepail/symphony-smoke-lab/issues/4/labels",
                "body": "{\"labels\":[\"symphony\"]}"
            }),
        )
        .await;

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body, json!({ "labels": ["symphony"] }));
        assert_eq!(
            payload.get("success").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        server.abort();
    }

    #[tokio::test]
    async fn github_api_parses_stringified_json_array_body() {
        let _guard = env_lock().lock().await;
        let server = MockHttpServer::start(vec![MockHttpResponse::json(
            HttpStatusCode::OK,
            json!({ "ok": true }),
        )])
        .await;
        let _token = ScopedEnvVar::set("GH_TOKEN", "token");
        let _base_url = ScopedEnvVar::set("SYMPHONY_GITHUB_API_URL", &server.endpoint());

        let payload = execute(
            &linear_config(),
            &StubTracker,
            GITHUB_API_TOOL,
            json!({
                "method": "POST",
                "path": "/repos/kalepail/symphony-smoke-lab/issues/4/labels",
                "body": "[\"symphony\"]"
            }),
        )
        .await;

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body, json!(["symphony"]));
        assert_eq!(
            payload.get("success").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        server.abort();
    }

    #[derive(Clone)]
    struct MockHttpState {
        requests: Arc<StdMutex<Vec<RecordedRequest>>>,
        responses: Arc<StdMutex<VecDeque<MockHttpResponse>>>,
    }

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        body: Value,
    }

    struct MockHttpServer {
        endpoint: String,
        requests: Arc<StdMutex<Vec<RecordedRequest>>>,
        join: JoinHandle<()>,
    }

    impl MockHttpServer {
        async fn start(responses: Vec<MockHttpResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let app = Router::new()
                .fallback(any(mock_http_handler))
                .with_state(MockHttpState {
                    requests: Arc::clone(&requests),
                    responses: Arc::new(StdMutex::new(responses.into())),
                });
            let join = tokio::spawn(async move {
                axum::serve(listener, app).await.expect("serve");
            });

            Self {
                endpoint: format!("http://{addr}"),
                requests,
                join,
            }
        }

        fn endpoint(&self) -> String {
            self.endpoint.clone()
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().expect("requests").clone()
        }

        fn abort(self) {
            self.join.abort();
        }
    }

    #[derive(Clone)]
    struct MockHttpResponse {
        status: HttpStatusCode,
        body: Value,
    }

    impl MockHttpResponse {
        fn json(status: HttpStatusCode, body: Value) -> Self {
            Self { status, body }
        }
    }

    async fn mock_http_handler(
        State(state): State<MockHttpState>,
        request: axum::extract::Request,
    ) -> impl IntoResponse {
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.expect("body");
        let recorded = RecordedRequest {
            method: parts.method.as_str().to_string(),
            path: parts.uri.path().to_string(),
            authorization: parts
                .headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned),
            body: if bytes.is_empty() {
                json!(null)
            } else {
                serde_json::from_slice(&bytes).expect("json body")
            },
        };
        state.requests.lock().expect("requests").push(recorded);
        let response = state
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .expect("mock response");
        (response.status, Json(response.body)).into_response()
    }
}
