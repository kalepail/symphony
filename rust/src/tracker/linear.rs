use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tracing::warn;

use crate::{
    config::ServiceConfig,
    issue::{BlockerRef, Issue},
    tracker::{TrackerClient, TrackerError},
};

const PAGE_SIZE: usize = 50;
const MAX_ERROR_BODY_LOG_BYTES: usize = 1_000;

const CANDIDATE_QUERY: &str = r#"
query SymphonyLinearPoll($projectSlug: String!, $stateNames: [String!]!, $first: Int!, $relationFirst: Int!, $after: String) {
  issues(filter: {project: {slugId: {eq: $projectSlug}}, state: {name: {in: $stateNames}}}, first: $first, after: $after) {
    nodes {
      id
      identifier
      title
      description
      priority
      state { name }
      branchName
      url
      assignee { id }
      labels { nodes { name } }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue {
            id
            identifier
            state { name }
          }
        }
      }
      createdAt
      updatedAt
    }
    pageInfo {
      hasNextPage
      endCursor
    }
  }
}
"#;

const QUERY_BY_IDS: &str = r#"
query SymphonyLinearIssuesById($ids: [ID!]!, $first: Int!, $relationFirst: Int!) {
  issues(filter: {id: {in: $ids}}, first: $first) {
    nodes {
      id
      identifier
      title
      description
      priority
      state { name }
      branchName
      url
      assignee { id }
      labels { nodes { name } }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue {
            id
            identifier
            state { name }
          }
        }
      }
      createdAt
      updatedAt
    }
  }
}
"#;

const VIEWER_QUERY: &str = r#"
query SymphonyLinearViewer {
  viewer { id }
}
"#;

#[derive(Clone)]
pub struct LinearTracker {
    client: Client,
    config: ServiceConfig,
}

impl LinearTracker {
    pub fn new(config: ServiceConfig) -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_millis(30_000))
            .build()
            .expect("reqwest client");
        Self { client, config }
    }

    async fn fetch_by_states(
        &self,
        state_names: &[String],
        assignee_filter: Option<AssigneeFilter>,
    ) -> Result<Vec<Issue>, TrackerError> {
        let project_slug = self
            .config
            .tracker
            .project_slug
            .clone()
            .ok_or(TrackerError::MissingTrackerProjectSlug)?;

        let mut after: Option<String> = None;
        let mut issues = Vec::new();

        loop {
            let variables = json!({
                "projectSlug": project_slug,
                "stateNames": state_names,
                "first": PAGE_SIZE,
                "relationFirst": PAGE_SIZE,
                "after": after,
            });
            let body = self.raw_graphql(CANDIDATE_QUERY, variables).await?;
            let issues_obj = body
                .get("data")
                .and_then(|data| data.get("issues"))
                .ok_or(TrackerError::LinearUnknownPayload)?;
            let page_nodes = issues_obj
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or(TrackerError::LinearUnknownPayload)?;

            for node in page_nodes {
                if let Some(issue) = normalize_issue(node, assignee_filter.as_ref()) {
                    issues.push(issue);
                }
            }

            let has_next = issues_obj
                .get("pageInfo")
                .and_then(|info| info.get("hasNextPage"))
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if !has_next {
                break;
            }

            let next = issues_obj
                .get("pageInfo")
                .and_then(|info| info.get("endCursor"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .ok_or(TrackerError::LinearMissingEndCursor)?;
            after = Some(next);
        }

        Ok(issues)
    }

    async fn resolve_assignee_filter(&self) -> Result<Option<AssigneeFilter>, TrackerError> {
        let assignee = match self.config.tracker.assignee.clone() {
            Some(value) if !value.trim().is_empty() => value,
            _ => return Ok(None),
        };

        if assignee.trim() == "me" {
            let body = self.raw_graphql(VIEWER_QUERY, json!({})).await?;
            let id = body
                .get("data")
                .and_then(|data| data.get("viewer"))
                .and_then(|viewer| viewer.get("id"))
                .and_then(Value::as_str)
                .ok_or(TrackerError::MissingViewerIdentity)?;
            return Ok(Some(AssigneeFilter {
                match_value: id.to_string(),
            }));
        }

        Ok(Some(AssigneeFilter {
            match_value: assignee,
        }))
    }
}

#[async_trait]
impl TrackerClient for LinearTracker {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let api_key = self.config.tracker.api_key.clone();
        if api_key.is_none() {
            return Err(TrackerError::MissingTrackerApiKey);
        }
        let states = self.config.tracker.active_states.clone();
        let assignee_filter = self.resolve_assignee_filter().await?;
        self.fetch_by_states(&states, assignee_filter).await
    }

    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch_by_states(states, None).await
    }

    async fn fetch_issue_states_by_ids(
        &self,
        issue_ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        if issue_ids.is_empty() {
            return Ok(Vec::new());
        }
        let assignee_filter = self.resolve_assignee_filter().await?;
        let variables = json!({
            "ids": issue_ids,
            "first": issue_ids.len().min(PAGE_SIZE),
            "relationFirst": PAGE_SIZE
        });
        let body = self.raw_graphql(QUERY_BY_IDS, variables).await?;
        let nodes = body
            .get("data")
            .and_then(|data| data.get("issues"))
            .and_then(|issues| issues.get("nodes"))
            .and_then(Value::as_array)
            .ok_or(TrackerError::LinearUnknownPayload)?;

        Ok(nodes
            .iter()
            .filter_map(|node| normalize_issue(node, assignee_filter.as_ref()))
            .collect())
    }

    async fn raw_graphql(&self, query: &str, variables: Value) -> Result<Value, TrackerError> {
        let token = self
            .config
            .tracker
            .api_key
            .clone()
            .ok_or(TrackerError::MissingTrackerApiKey)?;

        let response = self
            .client
            .post(&self.config.tracker.endpoint)
            .header("Authorization", token)
            .header("Content-Type", "application/json")
            .json(&json!({
                "query": query,
                "variables": variables
            }))
            .send()
            .await
            .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;

        let status = response.status();
        let body_text = response
            .text()
            .await
            .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;

        if status != StatusCode::OK {
            warn!(
                "linear_graphql=status={} body={}",
                status.as_u16(),
                truncate_error_body(&body_text)
            );
            return Err(TrackerError::LinearApiStatus {
                status: status.as_u16(),
                body: body_text,
            });
        }

        let body = serde_json::from_str::<Value>(&body_text)
            .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;

        if let Some(errors) = body.get("errors") {
            return Err(TrackerError::LinearGraphqlErrors(errors.to_string()));
        }

        Ok(body)
    }
}

fn truncate_error_body(body: &str) -> String {
    let sanitized = body.replace(char::is_control, " ");
    if sanitized.len() > MAX_ERROR_BODY_LOG_BYTES {
        format!("{}...<truncated>", &sanitized[..MAX_ERROR_BODY_LOG_BYTES])
    } else {
        sanitized
    }
}

#[derive(Clone, Debug)]
struct AssigneeFilter {
    match_value: String,
}

fn normalize_issue(value: &Value, assignee_filter: Option<&AssigneeFilter>) -> Option<Issue> {
    let issue = value.as_object()?;
    let assignee_id = issue
        .get("assignee")
        .and_then(Value::as_object)
        .and_then(|assignee| assignee.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let assigned_to_worker = assignee_filter
        .is_none_or(|filter| assignee_id.as_deref() == Some(filter.match_value.as_str()));

    Some(Issue {
        id: issue.get("id")?.as_str()?.to_string(),
        identifier: issue.get("identifier")?.as_str()?.to_string(),
        title: issue.get("title")?.as_str()?.to_string(),
        description: issue
            .get("description")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        priority: issue.get("priority").and_then(Value::as_i64),
        state: issue
            .get("state")
            .and_then(Value::as_object)
            .and_then(|state| state.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        branch_name: issue
            .get("branchName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        url: issue
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        labels: extract_labels(issue),
        blocked_by: extract_blockers(issue),
        created_at: parse_timestamp(issue.get("createdAt")),
        updated_at: parse_timestamp(issue.get("updatedAt")),
        assignee_id,
        assigned_to_worker,
    })
}

fn extract_labels(issue: &serde_json::Map<String, Value>) -> Vec<String> {
    issue
        .get("labels")
        .and_then(Value::as_object)
        .and_then(|labels| labels.get("nodes"))
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| node.get("name").and_then(Value::as_str))
                .map(|name| name.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default()
}

fn extract_blockers(issue: &serde_json::Map<String, Value>) -> Vec<BlockerRef> {
    issue
        .get("inverseRelations")
        .and_then(Value::as_object)
        .and_then(|relations| relations.get("nodes"))
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| {
                    let relation_type = node.get("type")?.as_str()?;
                    if relation_type.trim().eq_ignore_ascii_case("blocks") {
                        let blocker = node.get("issue")?.as_object()?;
                        Some(BlockerRef {
                            id: blocker
                                .get("id")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            identifier: blocker
                                .get("identifier")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            state: blocker
                                .get("state")
                                .and_then(Value::as_object)
                                .and_then(|state| state.get("name"))
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                        })
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_timestamp(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|value| value.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{config::ServiceConfig, tracker::TrackerClient};

    use super::{AssigneeFilter, LinearTracker, extract_blockers, normalize_issue};

    #[test]
    fn normalizes_blockers_and_labels() {
        let issue = normalize_issue(
            &json!({
                "id": "1",
                "identifier": "ABC-1",
                "title": "Title",
                "description": "Desc",
                "priority": 2,
                "state": { "name": "Todo" },
                "labels": { "nodes": [{ "name": "Backend" }] },
                "inverseRelations": {
                    "nodes": [
                        {
                            "type": "blocks",
                            "issue": {
                                "id": "2",
                                "identifier": "ABC-2",
                                "state": { "name": "In Progress" }
                            }
                        },
                        {
                            "type": "relatesTo",
                            "issue": { "id": "3" }
                        }
                    ]
                }
            }),
            Some(&AssigneeFilter {
                match_value: "user-1".to_string(),
            }),
        )
        .expect("issue");

        assert_eq!(issue.labels, vec!["backend"]);
        assert_eq!(issue.blocked_by.len(), 1);
        assert!(!issue.assigned_to_worker);
    }

    #[test]
    fn extracts_blocker_relation_only() {
        let blockers = extract_blockers(
            json!({
                "inverseRelations": {
                    "nodes": [
                        {
                            "type": "blocks",
                            "issue": { "id": "2", "identifier": "ABC-2", "state": { "name": "Done" } }
                        }
                    ]
                }
            })
            .as_object()
            .expect("object"),
        );
        assert_eq!(blockers.len(), 1);
    }

    #[tokio::test]
    async fn empty_state_fetch_returns_empty_without_request() {
        let config = ServiceConfig::from_map(
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
        .expect("config");
        let tracker = LinearTracker::new(config);
        let empty: Vec<String> = Vec::new();
        let result = tracker
            .fetch_issues_by_states(&empty)
            .await
            .expect("empty result");
        assert!(result.is_empty());
    }
}
