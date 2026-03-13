use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::TodoistPromptCommentLimits;

const TODOIST_WORKPAD_HEADER: &str = "## Codex Workpad";
const TODOIST_WORKPAD_MARKER: &str = "<!-- symphony:workpad -->";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueComment {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub posted_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_url: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub state: String,
    pub url: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub is_subtask: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee_id: Option<String>,
    #[serde(default = "default_assigned_to_worker")]
    pub assigned_to_worker: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todoist_comments: Vec<IssueComment>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub todoist_comments_truncated: bool,
}

fn default_assigned_to_worker() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Issue {
    pub fn state_key(&self) -> String {
        normalize_state_name(&self.state)
    }
}

pub fn normalize_state_name(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

pub fn todoist_review_comments_from_values(
    values: &[Value],
    limits: &TodoistPromptCommentLimits,
) -> (Vec<IssueComment>, bool) {
    let mut comments = values
        .iter()
        .filter_map(todoist_review_comment_from_value)
        .collect::<Vec<_>>();
    comments.sort_by(|left, right| {
        left.posted_at
            .as_deref()
            .unwrap_or_default()
            .cmp(right.posted_at.as_deref().unwrap_or_default())
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut truncated = false;
    if comments.len() > limits.max_comments {
        let split_at = comments.len() - limits.max_comments;
        comments = comments.split_off(split_at);
        truncated = true;
    }

    let mut total_chars = 0usize;
    let mut selected = Vec::new();
    for mut comment in comments.into_iter().rev() {
        if comment.content.chars().count() > limits.max_comment_chars {
            comment.content = truncate_text(&comment.content, limits.max_comment_chars);
            truncated = true;
        }

        let comment_chars = comment.content.chars().count()
            + comment
                .attachment_name
                .as_deref()
                .map(|value| value.chars().count())
                .unwrap_or(0)
            + comment
                .attachment_url
                .as_deref()
                .map(|value| value.chars().count())
                .unwrap_or(0);

        if !selected.is_empty()
            && total_chars.saturating_add(comment_chars) > limits.max_total_chars
        {
            truncated = true;
            continue;
        }

        total_chars = total_chars.saturating_add(comment_chars);
        selected.push(comment);
    }

    selected.reverse();
    (selected, truncated)
}

fn todoist_review_comment_from_value(value: &Value) -> Option<IssueComment> {
    if value.get("is_deleted").and_then(Value::as_bool) == Some(true) {
        return None;
    }

    let content = value
        .get("content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if is_todoist_workpad_content(content) {
        return None;
    }

    let attachment = value.get("file_attachment");
    Some(IssueComment {
        id: json_id_string(value.get("id"))?,
        author_id: json_id_string(value.get("posted_uid")),
        posted_at: optional_trimmed_string(value.get("posted_at")),
        updated_at: optional_trimmed_string(value.get("updated_at")),
        content: content.to_string(),
        attachment_name: attachment.and_then(|value| {
            optional_trimmed_string(value.get("file_name"))
                .or_else(|| optional_trimmed_string(value.get("name")))
        }),
        attachment_url: attachment.and_then(|value| {
            optional_trimmed_string(value.get("file_url"))
                .or_else(|| optional_trimmed_string(value.get("download_url")))
                .or_else(|| optional_trimmed_string(value.get("url")))
        }),
    })
}

fn is_todoist_workpad_content(content: &str) -> bool {
    content.contains(TODOIST_WORKPAD_MARKER)
        || content.trim_start().starts_with(TODOIST_WORKPAD_HEADER)
}

fn optional_trimmed_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn json_id_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(value)) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    if truncated.chars().count() != value.chars().count() {
        truncated.push_str("...");
    }
    truncated
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::TodoistPromptCommentLimits;

    use super::todoist_review_comments_from_values;

    #[test]
    fn todoist_review_comments_exclude_workpad_comments() {
        let values = vec![
            json!({
                "id": "comment-human",
                "posted_uid": "user-1",
                "posted_at": "2026-03-13T12:00:00Z",
                "content": "Please keep the original diff."
            }),
            json!({
                "id": "comment-workpad",
                "item_id": "task-1",
                "content": "## Codex Workpad\n\n<!-- symphony:workpad -->\n\nTracked state"
            }),
        ];

        let (comments, truncated) =
            todoist_review_comments_from_values(&values, &TodoistPromptCommentLimits::default());
        assert!(!truncated);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, "comment-human");
        assert_eq!(comments[0].content, "Please keep the original diff.");
    }

    #[test]
    fn todoist_review_comments_capture_attachment_metadata() {
        let values = vec![json!({
            "id": "comment-attachment",
            "posted_uid": "user-2",
            "content": "Reference this spec.",
            "file_attachment": {
                "file_name": "spec.md",
                "file_url": "https://files.example/spec.md"
            }
        })];

        let (comments, truncated) =
            todoist_review_comments_from_values(&values, &TodoistPromptCommentLimits::default());
        assert!(!truncated);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].attachment_name.as_deref(), Some("spec.md"));
        assert_eq!(
            comments[0].attachment_url.as_deref(),
            Some("https://files.example/spec.md")
        );
    }

    #[test]
    fn todoist_review_comments_respect_configured_limits() {
        let values = vec![
            json!({
                "id": "comment-1",
                "posted_uid": "user-1",
                "posted_at": "2026-03-13T12:00:00Z",
                "content": "abcdef"
            }),
            json!({
                "id": "comment-2",
                "posted_uid": "user-2",
                "posted_at": "2026-03-13T13:00:00Z",
                "content": "ghijkl"
            }),
        ];

        let (comments, truncated) = todoist_review_comments_from_values(
            &values,
            &TodoistPromptCommentLimits {
                max_comments: 1,
                max_comment_chars: 4,
                max_total_chars: 4,
            },
        );

        assert!(truncated);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, "comment-2");
        assert_eq!(comments[0].content, "ghij...");
    }
}
