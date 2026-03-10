use serde_json::{Value, json};

use crate::{
    config::ServiceConfig,
    tracker::{TrackerClient, TrackerError},
};

pub const LINEAR_GRAPHQL_TOOL: &str = "linear_graphql";
const LINEAR_GRAPHQL_TOOL_DESCRIPTION: &str = concat!(
    "Execute a raw GraphQL query or mutation against Linear using Symphony's configured auth.\n",
    "Use one narrow operation per call.\n",
    "When you have a ticket key like `SDF-6`, query `issue(id: $id)` directly; Linear accepts the human identifier there.\n",
    "Create comments with `commentCreate(input: { issueId: $issueId, body: $body })`.\n",
    "Move state by first querying `issue(id: $id) { team { states { nodes { id name type } } } }`, then calling `issueUpdate(id: $id, input: { stateId: $stateId })`.\n",
    "Avoid broad search or schema introspection unless these direct recipes fail."
);

pub fn tool_specs(config: &ServiceConfig) -> Vec<Value> {
    if config.tracker.kind.as_deref() != Some("linear") || config.tracker.api_key.is_none() {
        return Vec::new();
    }

    vec![json!({
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
    })]
}

pub async fn execute(
    config: &ServiceConfig,
    tracker: &dyn TrackerClient,
    tool: &str,
    arguments: Value,
) -> Value {
    if tool != LINEAR_GRAPHQL_TOOL {
        return failure_payload(json!({
            "error": {
                "message": format!("Unsupported dynamic tool: {tool}."),
                "supportedTools": [LINEAR_GRAPHQL_TOOL]
            }
        }));
    }

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
    use serde_json::json;

    use crate::{
        config::ServiceConfig,
        issue::Issue,
        tracker::{TrackerClient, TrackerError},
    };

    use super::{LINEAR_GRAPHQL_TOOL, execute, tool_specs};

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
    fn hides_tool_without_linear_auth() {
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

        assert!(tool_specs(&config).is_empty());
    }

    #[test]
    fn tool_description_includes_linear_recipes() {
        let config = linear_config();
        let specs = tool_specs(&config);
        let description = specs[0]
            .get("description")
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
}
