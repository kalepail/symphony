use minijinja::{Environment, UndefinedBehavior, value::Value as MiniValue};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{issue::Issue, workflow::WorkflowDefinition};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuePromptMetrics {
    pub prompt_bytes: usize,
    pub workflow_template_bytes: usize,
    pub rendered_issue_context_bytes: usize,
    pub preloaded_comment_bytes: usize,
    pub preloaded_comment_count: usize,
    pub comment_preload_truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuePromptBuild {
    pub prompt: String,
    pub metrics: IssuePromptMetrics,
}

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("template_empty")]
    TemplateEmpty,
    #[error("template_parse_error {0}")]
    TemplateParse(String),
    #[error("template_render_error {0}")]
    TemplateRender(String),
}

pub fn build_issue_prompt(
    workflow: &WorkflowDefinition,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    Ok(build_issue_prompt_with_metrics(workflow, issue, attempt)?.prompt)
}

pub fn build_issue_prompt_with_metrics(
    workflow: &WorkflowDefinition,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<IssuePromptBuild, PromptError> {
    let template_source = workflow.prompt_template.trim();
    if template_source.is_empty() {
        return Err(PromptError::TemplateEmpty);
    }

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    let template = env
        .template_from_str(template_source)
        .map_err(|error| PromptError::TemplateParse(error.to_string()))?;

    let issue_json = serde_json::to_value(issue)
        .map_err(|error| PromptError::TemplateRender(error.to_string()))?;
    let prompt = render_prompt(&template, issue_json, attempt)?;

    let mut issue_without_comments = issue.clone();
    issue_without_comments.todoist_comments.clear();
    issue_without_comments.todoist_comments_truncated = false;
    let mut issue_without_comments_json = serde_json::to_value(issue_without_comments)
        .map_err(|error| PromptError::TemplateRender(error.to_string()))?;
    if let Some(issue_without_comments_map) = issue_without_comments_json.as_object_mut() {
        issue_without_comments_map.insert("todoist_comments".to_string(), Value::Array(Vec::new()));
        issue_without_comments_map
            .insert("todoist_comments_truncated".to_string(), Value::Bool(false));
    }
    let prompt_without_comments = render_prompt(&template, issue_without_comments_json, attempt)?;

    Ok(IssuePromptBuild {
        metrics: IssuePromptMetrics {
            prompt_bytes: prompt.len(),
            workflow_template_bytes: template_source.len(),
            rendered_issue_context_bytes: prompt_without_comments.len(),
            preloaded_comment_bytes: prompt.len().saturating_sub(prompt_without_comments.len()),
            preloaded_comment_count: issue.todoist_comments.len(),
            comment_preload_truncated: issue.todoist_comments_truncated,
        },
        prompt,
    })
}

pub fn continuation_guidance(turn_number: usize, max_turns: usize) -> String {
    format!(
        "Continuation guidance:\n\n- The previous Codex turn completed normally, but the Todoist task is still in an active state.\n- This is continuation turn #{turn_number} of {max_turns} for the current agent run.\n- Resume from the current workspace and thread state instead of restarting from scratch.\n- The original task instructions are already in the thread history, so do not restate them before acting.\n- Focus on the remaining task work and do not end the turn while the task stays active unless you are truly blocked."
    )
}

fn build_context(issue: Value, attempt: Option<u32>) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("issue".to_string(), issue);
    map.insert(
        "attempt".to_string(),
        attempt.map(Value::from).unwrap_or(Value::Null),
    );
    map
}

fn render_prompt(
    template: &minijinja::Template<'_, '_>,
    issue_json: Value,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    let context = MiniValue::from_serialize(build_context(issue_json, attempt));
    template
        .render(context)
        .map_err(|error| PromptError::TemplateRender(error.to_string()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{issue::Issue, workflow::WorkflowDefinition};

    use super::{PromptError, build_issue_prompt, build_issue_prompt_with_metrics};

    #[test]
    fn renders_issue_and_attempt() {
        let workflow = WorkflowDefinition {
            config: json!({}).as_object().expect("object").clone(),
            prompt_template: "{{ issue.identifier }} attempt={{ attempt }}".to_string(),
        };
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let prompt = build_issue_prompt(&workflow, &issue, Some(2)).expect("render prompt");
        assert_eq!(prompt, "ABC-1 attempt=2");
    }

    #[test]
    fn fails_unknown_variables_in_strict_mode() {
        let workflow = WorkflowDefinition {
            config: json!({}).as_object().expect("object").clone(),
            prompt_template: "{{ missing }}".to_string(),
        };
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let error = build_issue_prompt(&workflow, &issue, None).unwrap_err();
        assert!(matches!(error, PromptError::TemplateRender(_)));
    }

    #[test]
    fn rejects_empty_templates() {
        let workflow = WorkflowDefinition {
            config: json!({}).as_object().expect("object").clone(),
            prompt_template: "   ".to_string(),
        };
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let error = build_issue_prompt(&workflow, &issue, None).unwrap_err();
        assert!(matches!(error, PromptError::TemplateEmpty));
    }

    #[test]
    fn guarded_optional_issue_fields_render_when_omitted() {
        let workflow = WorkflowDefinition {
            config: json!({}).as_object().expect("object").clone(),
            prompt_template: r#"
URL: {% if issue.url is defined and issue.url %}{{ issue.url }}{% else %}n/a{% endif %}
Assignee: {% if issue.assignee_id is defined and issue.assignee_id %}{{ issue.assignee_id }}{% else %}unassigned{% endif %}
Due: {% if issue.due is defined and issue.due %}{{ issue.due }}{% else %}none{% endif %}
Deadline: {% if issue.deadline is defined and issue.deadline %}{{ issue.deadline }}{% else %}none{% endif %}
"#
            .to_string(),
        };
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "Todo".to_string(),
            ..Issue::default()
        };

        let prompt = build_issue_prompt(&workflow, &issue, None).expect("render prompt");
        assert!(prompt.contains("URL: n/a"));
        assert!(prompt.contains("Assignee: unassigned"));
        assert!(prompt.contains("Due: none"));
        assert!(prompt.contains("Deadline: none"));
    }

    #[test]
    fn renders_preloaded_todoist_comments_when_present() {
        let workflow = WorkflowDefinition {
            config: json!({}).as_object().expect("object").clone(),
            prompt_template:
                "{% for comment in issue.todoist_comments %}{{ comment.author_id }}={{ comment.content }}\n{% endfor %}".to_string(),
        };
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "Rework".to_string(),
            todoist_comments: vec![crate::issue::IssueComment {
                id: "comment-1".to_string(),
                author_id: Some("user-1".to_string()),
                content: "Please keep the original diff and add docs.".to_string(),
                ..crate::issue::IssueComment::default()
            }],
            ..Issue::default()
        };

        let prompt = build_issue_prompt(&workflow, &issue, None).expect("render prompt");
        assert!(prompt.contains("user-1=Please keep the original diff and add docs."));
    }

    #[test]
    fn measures_prompt_comment_contribution() {
        let workflow = WorkflowDefinition {
            config: json!({}).as_object().expect("object").clone(),
            prompt_template: "A={{ issue.identifier }}\n{% for comment in issue.todoist_comments %}C={{ comment.content }}\n{% endfor %}".to_string(),
        };
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "Rework".to_string(),
            todoist_comments: vec![crate::issue::IssueComment {
                id: "comment-1".to_string(),
                content: "Please keep the original diff and add docs.".to_string(),
                ..crate::issue::IssueComment::default()
            }],
            ..Issue::default()
        };

        let built = build_issue_prompt_with_metrics(&workflow, &issue, None).expect("render");
        assert!(built.metrics.prompt_bytes > 0);
        assert!(built.metrics.workflow_template_bytes > 0);
        assert!(built.metrics.rendered_issue_context_bytes > 0);
        assert!(built.metrics.preloaded_comment_bytes > 0);
        assert_eq!(built.metrics.preloaded_comment_count, 1);
        assert!(!built.metrics.comment_preload_truncated);
    }

    #[test]
    fn reports_comment_preload_truncation_flag() {
        let workflow = WorkflowDefinition {
            config: json!({}).as_object().expect("object").clone(),
            prompt_template: "{{ issue.identifier }}".to_string(),
        };
        let issue = Issue {
            id: "1".to_string(),
            identifier: "ABC-1".to_string(),
            title: "Title".to_string(),
            state: "Rework".to_string(),
            todoist_comments_truncated: true,
            ..Issue::default()
        };

        let built = build_issue_prompt_with_metrics(&workflow, &issue, None).expect("render");
        assert!(built.metrics.comment_preload_truncated);
    }
}
