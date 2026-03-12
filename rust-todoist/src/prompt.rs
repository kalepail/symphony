use minijinja::{Environment, UndefinedBehavior, value::Value as MiniValue};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{issue::Issue, workflow::WorkflowDefinition};

const DEFAULT_PROMPT: &str = r#"You are working on a Todoist task.

Identifier: {{ issue.identifier }}
Title: {{ issue.title }}

Body:
{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}"#;

#[derive(Debug, Error)]
pub enum PromptError {
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
    let template_source = if workflow.prompt_template.trim().is_empty() {
        DEFAULT_PROMPT
    } else {
        workflow.prompt_template.as_str()
    };

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    let template = env
        .template_from_str(template_source)
        .map_err(|error| PromptError::TemplateParse(error.to_string()))?;

    let issue_json = serde_json::to_value(issue)
        .map_err(|error| PromptError::TemplateRender(error.to_string()))?;
    let context = MiniValue::from_serialize(build_context(issue_json, attempt));

    template
        .render(context)
        .map_err(|error| PromptError::TemplateRender(error.to_string()))
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{issue::Issue, workflow::WorkflowDefinition};

    use super::{PromptError, build_issue_prompt};

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
}
