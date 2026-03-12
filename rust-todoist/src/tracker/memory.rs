use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    config::ServiceConfig,
    issue::{Issue, normalize_state_name},
    tracker::{
        TrackerClient, TrackerError,
        todoist::{normalize_task, todoist_task_url},
    },
};

#[derive(Clone)]
pub struct MemoryTracker {
    config: ServiceConfig,
    state: Arc<Mutex<MemoryState>>,
}

#[derive(Clone, Debug, Default)]
struct MemoryState {
    tasks: Vec<Value>,
    sections: Vec<Value>,
    comments: Vec<Value>,
    current_user: Value,
    collaborators: Vec<Value>,
    projects: Vec<Value>,
    labels: Vec<Value>,
    reminders: Vec<Value>,
    user_plan_limits: Value,
    next_task_id: u64,
    next_comment_id: u64,
    next_reminder_id: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct MemoryFixtureEnvelope {
    #[serde(default)]
    issues: Vec<Issue>,
    #[serde(default)]
    tasks: Vec<Value>,
    #[serde(default)]
    sections: Vec<Value>,
    #[serde(default)]
    comments: Vec<Value>,
    #[serde(default)]
    current_user: Value,
    #[serde(default)]
    collaborators: Vec<Value>,
    #[serde(default)]
    projects: Vec<Value>,
    #[serde(default)]
    labels: Vec<Value>,
    #[serde(default)]
    reminders: Vec<Value>,
    #[serde(default)]
    user_plan_limits: Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MemoryFixture {
    Issues(Vec<Issue>),
    Tasks(Vec<Value>),
    Envelope(MemoryFixtureEnvelope),
}

impl MemoryTracker {
    pub fn new(config: ServiceConfig) -> Self {
        let state = load_state(&config).unwrap_or_else(|error| {
            panic!("failed to initialize memory tracker state: {error}");
        });
        Self {
            config,
            state: Arc::new(Mutex::new(state)),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state.lock().expect("memory tracker state poisoned")
    }

    fn current_project_id(&self) -> String {
        self.config
            .tracker
            .project_id
            .clone()
            .unwrap_or_else(|| "memory-project".to_string())
    }

    fn sections_by_id(state: &MemoryState) -> HashMap<String, String> {
        state
            .sections
            .iter()
            .filter_map(|section| {
                Some((
                    json_id(section.get("id"))?,
                    section
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("Unnamed Section")
                        .to_string(),
                ))
            })
            .collect()
    }

    fn completed_state_label(&self) -> String {
        self.config
            .tracker
            .terminal_states
            .iter()
            .find(|value| normalize_state_name(value) == "done")
            .cloned()
            .unwrap_or_else(|| "Done".to_string())
    }

    fn assignee_filter_value(&self, state: &MemoryState) -> Option<String> {
        match self.config.tracker.assignee.as_deref() {
            Some("me") => json_id(state.current_user.get("id")),
            Some(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
            _ => None,
        }
    }

    fn issues_from_state(&self, state: &MemoryState) -> Vec<Issue> {
        let sections = Self::sections_by_id(state);
        let completed_state = self.completed_state_label();
        let assignee_filter = self.assignee_filter_value(state);
        state
            .tasks
            .iter()
            .filter_map(|task| normalize_task(task, &sections, None, completed_state.as_str()))
            .map(|mut issue| {
                issue.assigned_to_worker = assignee_filter
                    .as_ref()
                    .is_none_or(|filter| issue.assignee_id.as_deref() == Some(filter.as_str()));
                issue
            })
            .collect()
    }

    fn ensure_comments_available(&self, state: &MemoryState) -> Result<(), TrackerError> {
        if state
            .user_plan_limits
            .get("comments")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            Ok(())
        } else {
            Err(TrackerError::TodoistCommentsUnavailable)
        }
    }

    fn ensure_reminders_available(&self, state: &MemoryState) -> Result<(), TrackerError> {
        if state
            .user_plan_limits
            .get("reminders")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            Ok(())
        } else {
            Err(TrackerError::TodoistRemindersUnavailable)
        }
    }
}

#[async_trait]
impl TrackerClient for MemoryTracker {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let active_states: BTreeSet<String> = self
            .config
            .tracker
            .active_states
            .iter()
            .map(|state| normalize_state_name(state))
            .collect();
        let issues = self.issues_from_state(&self.state());

        Ok(issues
            .into_iter()
            .filter(|issue| !issue.is_subtask && active_states.contains(&issue.state_key()))
            .collect())
    }

    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: BTreeSet<String> = states
            .iter()
            .map(|state| normalize_state_name(state))
            .collect();

        Ok(self
            .issues_from_state(&self.state())
            .into_iter()
            .filter(|issue| wanted.contains(&issue.state_key()))
            .collect())
    }

    async fn fetch_issue_states_by_ids(
        &self,
        issue_ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        if issue_ids.is_empty() {
            return Ok(Vec::new());
        }
        let wanted: BTreeSet<&str> = issue_ids.iter().map(String::as_str).collect();

        Ok(self
            .issues_from_state(&self.state())
            .into_iter()
            .filter(|issue| wanted.contains(issue.id.as_str()))
            .collect())
    }

    async fn fetch_open_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        Ok(self.issues_from_state(&self.state()))
    }

    async fn get_current_user(&self) -> Result<Value, TrackerError> {
        Ok(self.state().current_user.clone())
    }

    async fn list_projects(&self, arguments: Value) -> Result<Value, TrackerError> {
        let state = self.state();
        Ok(paginate_collection(
            state.projects.clone(),
            arguments.get("cursor").and_then(Value::as_str),
            read_limit(&arguments),
        ))
    }

    async fn get_project(&self, project_id: &str) -> Result<Value, TrackerError> {
        let state = self.state();
        state
            .projects
            .iter()
            .find(|project| json_id(project.get("id")).as_deref() == Some(project_id))
            .cloned()
            .ok_or_else(|| not_found("project", project_id))
    }

    async fn list_collaborators(&self, arguments: Value) -> Result<Value, TrackerError> {
        let project_id = arguments
            .get("project_id")
            .and_then(json_id_from_value)
            .unwrap_or_else(|| self.current_project_id());
        let state = self.state();
        let collaborators = state
            .collaborators
            .iter()
            .filter(|collaborator| {
                collaborator
                    .get("project_id")
                    .and_then(json_id_from_value)
                    .is_none_or(|value| value == project_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        Ok(paginate_collection(
            collaborators,
            arguments.get("cursor").and_then(Value::as_str),
            read_limit(&arguments),
        ))
    }

    async fn list_tasks(&self, arguments: Value) -> Result<Value, TrackerError> {
        let project_id = arguments
            .get("project_id")
            .and_then(json_id_from_value)
            .unwrap_or_else(|| self.current_project_id());
        let section_id = arguments.get("section_id").and_then(json_id_from_value);
        let state = self.state();
        let tasks = state
            .tasks
            .iter()
            .filter(|task| {
                task.get("project_id")
                    .and_then(json_id_from_value)
                    .is_none_or(|value| value == project_id)
            })
            .filter(|task| {
                section_id.as_ref().is_none_or(|section_id| {
                    task.get("section_id")
                        .and_then(json_id_from_value)
                        .as_deref()
                        == Some(section_id.as_str())
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        Ok(paginate_collection(
            tasks,
            arguments.get("cursor").and_then(Value::as_str),
            read_limit(&arguments),
        ))
    }

    async fn get_task(&self, task_id: &str) -> Result<Value, TrackerError> {
        let state = self.state();
        state
            .tasks
            .iter()
            .find(|task| json_id(task.get("id")).as_deref() == Some(task_id))
            .cloned()
            .ok_or_else(|| not_found("task", task_id))
    }

    async fn list_sections(&self, arguments: Value) -> Result<Value, TrackerError> {
        let project_id = arguments
            .get("project_id")
            .and_then(json_id_from_value)
            .unwrap_or_else(|| self.current_project_id());
        let state = self.state();
        let sections = state
            .sections
            .iter()
            .filter(|section| {
                section
                    .get("project_id")
                    .and_then(json_id_from_value)
                    .is_none_or(|value| value == project_id)
            })
            .cloned()
            .collect::<Vec<_>>();
        Ok(paginate_collection(
            sections,
            arguments.get("cursor").and_then(Value::as_str),
            read_limit(&arguments),
        ))
    }

    async fn get_section(&self, section_id: &str) -> Result<Value, TrackerError> {
        let state = self.state();
        state
            .sections
            .iter()
            .find(|section| json_id(section.get("id")).as_deref() == Some(section_id))
            .cloned()
            .ok_or_else(|| not_found("section", section_id))
    }

    async fn list_labels(&self, arguments: Value) -> Result<Value, TrackerError> {
        let state = self.state();
        Ok(paginate_collection(
            state.labels.clone(),
            arguments.get("cursor").and_then(Value::as_str),
            read_limit(&arguments),
        ))
    }

    async fn list_comments(&self, arguments: Value) -> Result<Value, TrackerError> {
        let target = comment_target(&arguments)?;
        let state = self.state();
        let comments = state
            .comments
            .iter()
            .filter(|comment| match target {
                "task_id" => {
                    comment.get("task_id").and_then(json_id_from_value)
                        == arguments.get("task_id").and_then(json_id_from_value)
                }
                "project_id" => {
                    comment.get("project_id").and_then(json_id_from_value)
                        == arguments.get("project_id").and_then(json_id_from_value)
                }
                _ => false,
            })
            .cloned()
            .collect::<Vec<_>>();
        Ok(paginate_collection(
            comments,
            arguments.get("cursor").and_then(Value::as_str),
            read_limit(&arguments),
        ))
    }

    async fn create_comment(&self, arguments: Value) -> Result<Value, TrackerError> {
        let mut state = self.state();
        self.ensure_comments_available(&state)?;
        let content = required_string(&arguments, "content", "create_comment")?;
        if content.len() > 15_000 {
            return Err(TrackerError::TodoistCommentTooLarge {
                limit: 15_000,
                actual: content.len(),
            });
        }
        let mut comment = Map::new();
        comment.insert(
            "id".to_string(),
            Value::String(next_id(&mut state.next_comment_id, "comment")),
        );
        comment.insert("content".to_string(), Value::String(content));
        comment.insert(
            "posted_at".to_string(),
            Value::String(Utc::now().to_rfc3339()),
        );
        copy_if_present(&arguments, &mut comment, "task_id");
        copy_if_present(&arguments, &mut comment, "project_id");
        let comment = Value::Object(comment);
        state.comments.push(comment.clone());
        Ok(comment)
    }

    async fn update_comment(
        &self,
        comment_id: &str,
        arguments: Value,
    ) -> Result<Value, TrackerError> {
        let mut state = self.state();
        self.ensure_comments_available(&state)?;
        let content = required_string(&arguments, "content", "update_comment")?;
        if content.len() > 15_000 {
            return Err(TrackerError::TodoistCommentTooLarge {
                limit: 15_000,
                actual: content.len(),
            });
        }
        let comment =
            value_object_mut(find_by_id_mut(&mut state.comments, comment_id, "comment")?)?;
        comment.insert("content".to_string(), Value::String(content));
        comment.insert(
            "updated_at".to_string(),
            Value::String(Utc::now().to_rfc3339()),
        );
        Ok(Value::Object(comment.clone()))
    }

    async fn update_task(&self, task_id: &str, arguments: Value) -> Result<Value, TrackerError> {
        let mut state = self.state();
        let task_value = {
            let task = value_object_mut(find_by_id_mut(&mut state.tasks, task_id, "task")?)?;
            let updates = sanitize_action_arguments(
                arguments,
                &[
                    "content",
                    "description",
                    "priority",
                    "labels",
                    "assignee_id",
                    "due",
                    "deadline",
                    "section_id",
                ],
            );
            if let Some(updates) = updates.as_object() {
                for (key, value) in updates {
                    task.insert(key.clone(), value.clone());
                }
            }
            Value::Object(task.clone())
        };
        sync_labels_from_names(&mut state, &label_names(&task_value));
        Ok(task_value)
    }

    async fn move_task(&self, task_id: &str, arguments: Value) -> Result<Value, TrackerError> {
        let mut state = self.state();
        let task = value_object_mut(find_by_id_mut(&mut state.tasks, task_id, "task")?)?;
        let updates =
            sanitize_action_arguments(arguments, &["project_id", "section_id", "parent_id"]);
        let Some(updates) = updates.as_object() else {
            return Err(TrackerError::TrackerOperationUnsupported(
                "`section_id`, `project_id`, or `parent_id` is required for move_task".to_string(),
            ));
        };
        if updates.is_empty() {
            return Err(TrackerError::TrackerOperationUnsupported(
                "`section_id`, `project_id`, or `parent_id` is required for move_task".to_string(),
            ));
        }
        for (key, value) in updates {
            task.insert(key.clone(), value.clone());
        }
        Ok(Value::Object(task.clone()))
    }

    async fn close_task(&self, task_id: &str) -> Result<Value, TrackerError> {
        let mut state = self.state();
        let task = value_object_mut(find_by_id_mut(&mut state.tasks, task_id, "task")?)?;
        task.insert("checked".to_string(), Value::Bool(true));
        task.insert("is_completed".to_string(), Value::Bool(true));
        task.insert(
            "completed_at".to_string(),
            Value::String(Utc::now().to_rfc3339()),
        );
        Ok(Value::Null)
    }

    async fn reopen_task(&self, task_id: &str) -> Result<Value, TrackerError> {
        let mut state = self.state();
        let task = value_object_mut(find_by_id_mut(&mut state.tasks, task_id, "task")?)?;
        task.insert("checked".to_string(), Value::Bool(false));
        task.insert("is_completed".to_string(), Value::Bool(false));
        task.remove("completed_at");
        Ok(Value::Null)
    }

    async fn create_task(&self, arguments: Value) -> Result<Value, TrackerError> {
        let mut state = self.state();
        let content = required_string(&arguments, "content", "create_task")?;
        let id = next_id(&mut state.next_task_id, "task");
        let project_id = arguments
            .get("project_id")
            .and_then(json_id_from_value)
            .unwrap_or_else(|| self.current_project_id());
        let mut task = Map::new();
        task.insert("id".to_string(), Value::String(id.clone()));
        task.insert("content".to_string(), Value::String(content));
        task.insert("project_id".to_string(), Value::String(project_id));
        task.insert("url".to_string(), Value::String(todoist_task_url(&id)));
        task.insert(
            "created_at".to_string(),
            Value::String(Utc::now().to_rfc3339()),
        );
        task.insert("checked".to_string(), Value::Bool(false));
        task.insert("is_completed".to_string(), Value::Bool(false));
        if let Some(updates) = sanitize_action_arguments(
            arguments,
            &[
                "description",
                "section_id",
                "parent_id",
                "priority",
                "labels",
                "assignee_id",
                "due",
                "deadline",
            ],
        )
        .as_object()
        {
            for (key, value) in updates {
                task.insert(key.clone(), value.clone());
            }
        }
        let task = Value::Object(task);
        sync_labels_from_names(&mut state, &label_names(&task));
        state.tasks.push(task.clone());
        Ok(task)
    }

    async fn list_reminders(&self, arguments: Value) -> Result<Value, TrackerError> {
        let state = self.state();
        self.ensure_reminders_available(&state)?;
        let task_id = arguments.get("task_id").and_then(json_id_from_value);
        let reminders = state
            .reminders
            .iter()
            .filter(|reminder| {
                task_id.as_ref().is_none_or(|task_id| {
                    reminder
                        .get("item_id")
                        .and_then(json_id_from_value)
                        .as_deref()
                        == Some(task_id.as_str())
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        Ok(paginate_collection(
            reminders,
            arguments.get("cursor").and_then(Value::as_str),
            read_limit(&arguments),
        ))
    }

    async fn create_reminder(&self, arguments: Value) -> Result<Value, TrackerError> {
        let mut state = self.state();
        self.ensure_reminders_available(&state)?;
        let task_id = arguments
            .get("task_id")
            .and_then(json_id_from_value)
            .ok_or_else(|| {
                TrackerError::TrackerOperationUnsupported(
                    "`task_id` is required for create_reminder".to_string(),
                )
            })?;
        let mut reminder = Map::new();
        reminder.insert(
            "id".to_string(),
            Value::String(next_id(&mut state.next_reminder_id, "reminder")),
        );
        reminder.insert("item_id".to_string(), Value::String(task_id));
        if let Some(updates) = sanitize_action_arguments(
            arguments,
            &[
                "type",
                "minute_offset",
                "due",
                "service",
                "name",
                "loc_lat",
                "loc_long",
                "loc_trigger",
                "radius",
            ],
        )
        .as_object()
        {
            for (key, value) in updates {
                reminder.insert(key.clone(), value.clone());
            }
        }
        let reminder = Value::Object(reminder);
        state.reminders.push(reminder.clone());
        Ok(reminder)
    }

    async fn update_reminder(
        &self,
        reminder_id: &str,
        arguments: Value,
    ) -> Result<Value, TrackerError> {
        let mut state = self.state();
        self.ensure_reminders_available(&state)?;
        let reminder = value_object_mut(find_by_id_mut(
            &mut state.reminders,
            reminder_id,
            "reminder",
        )?)?;
        let updates = sanitize_action_arguments(
            arguments,
            &[
                "type",
                "minute_offset",
                "due",
                "service",
                "name",
                "loc_lat",
                "loc_long",
                "loc_trigger",
                "radius",
            ],
        );
        let Some(updates) = updates.as_object() else {
            return Err(TrackerError::TrackerOperationUnsupported(
                "at least one reminder field is required for update_reminder".to_string(),
            ));
        };
        if updates.is_empty() {
            return Err(TrackerError::TrackerOperationUnsupported(
                "at least one reminder field is required for update_reminder".to_string(),
            ));
        }
        for (key, value) in updates {
            reminder.insert(key.clone(), value.clone());
        }
        Ok(Value::Object(reminder.clone()))
    }

    async fn delete_reminder(&self, reminder_id: &str) -> Result<Value, TrackerError> {
        let mut state = self.state();
        self.ensure_reminders_available(&state)?;
        let before = state.reminders.len();
        state
            .reminders
            .retain(|reminder| json_id(reminder.get("id")).as_deref() != Some(reminder_id));
        if state.reminders.len() == before {
            return Err(not_found("reminder", reminder_id));
        }
        Ok(json!({ "status": "ok", "id": reminder_id, "deleted": true }))
    }
}

fn load_state(config: &ServiceConfig) -> Result<MemoryState, TrackerError> {
    let path = config
        .tracker
        .fixture_path
        .as_deref()
        .ok_or(TrackerError::MissingTrackerFixturePath)?;
    let raw = fs::read_to_string(path).map_err(|error| TrackerError::MemoryFixtureIo {
        path: path.display().to_string(),
        error: error.to_string(),
    })?;
    parse_fixture(config, path, &raw)
}

fn parse_fixture(
    config: &ServiceConfig,
    path: &Path,
    raw: &str,
) -> Result<MemoryState, TrackerError> {
    let fixture = match path.extension().and_then(|ext| ext.to_str()) {
        Some("yaml" | "yml") => serde_yaml::from_str::<MemoryFixture>(raw).map_err(|error| {
            TrackerError::MemoryFixtureParse {
                path: path.display().to_string(),
                error: error.to_string(),
            }
        })?,
        _ => serde_json::from_str::<MemoryFixture>(raw).map_err(|error| {
            TrackerError::MemoryFixtureParse {
                path: path.display().to_string(),
                error: error.to_string(),
            }
        })?,
    };

    Ok(match fixture {
        MemoryFixture::Issues(issues) => state_from_envelope(
            config,
            MemoryFixtureEnvelope {
                issues,
                ..MemoryFixtureEnvelope::default()
            },
        ),
        MemoryFixture::Tasks(tasks) => state_from_envelope(
            config,
            MemoryFixtureEnvelope {
                tasks,
                ..MemoryFixtureEnvelope::default()
            },
        ),
        MemoryFixture::Envelope(envelope) => state_from_envelope(config, envelope),
    })
}

fn state_from_envelope(config: &ServiceConfig, envelope: MemoryFixtureEnvelope) -> MemoryState {
    let project_id = config
        .tracker
        .project_id
        .clone()
        .unwrap_or_else(|| "memory-project".to_string());
    let mut tasks = if envelope.tasks.is_empty() {
        legacy_issues_to_tasks(&envelope.issues, &project_id)
    } else {
        envelope.tasks
    };
    let mut sections = if envelope.sections.is_empty() {
        derive_sections(&tasks, &envelope.issues, &project_id)
    } else {
        envelope.sections
    };
    let mut projects = if envelope.projects.is_empty() {
        vec![json!({
            "id": project_id,
            "name": "Memory Project",
            "is_shared": !envelope.collaborators.is_empty()
        })]
    } else {
        envelope.projects
    };
    let current_user = if envelope.current_user.is_null() {
        json!({ "id": "memory-user", "name": "Memory User" })
    } else {
        envelope.current_user
    };
    let mut labels = if envelope.labels.is_empty() {
        derive_labels(&tasks)
    } else {
        envelope.labels
    };
    let user_plan_limits = normalize_plan_limits(envelope.user_plan_limits);

    ensure_task_defaults(&mut tasks, &project_id);
    ensure_section_defaults(&mut sections, &project_id);
    ensure_project_defaults(&mut projects, &project_id);
    sync_labels_from_tasks(&tasks, &mut labels);

    MemoryState {
        next_task_id: next_counter(&tasks, 10_000),
        next_comment_id: next_counter(&envelope.comments, 20_000),
        next_reminder_id: next_counter(&envelope.reminders, 30_000),
        tasks,
        sections,
        comments: envelope.comments,
        current_user,
        collaborators: envelope.collaborators,
        projects,
        labels,
        reminders: envelope.reminders,
        user_plan_limits,
    }
}

fn normalize_plan_limits(value: Value) -> Value {
    if let Some(current) = value.get("current") {
        current.clone()
    } else if value.is_object() {
        value
    } else {
        json!({
            "comments": true,
            "reminders": true,
            "deadlines": true,
            "activity_log": true
        })
    }
}

fn legacy_issues_to_tasks(issues: &[Issue], project_id: &str) -> Vec<Value> {
    let now = Utc::now().to_rfc3339();
    issues
        .iter()
        .map(|issue| {
            let completed = issue.state_key() == "done";
            let section_id =
                (!completed).then(|| format!("section-{}", sanitize_state_key(&issue.state)));
            json!({
                "id": issue.id,
                "content": issue.title,
                "description": issue.description.clone(),
                "priority": issue.priority.map(denormalize_priority).unwrap_or(1),
                "project_id": issue.project_id.clone().unwrap_or_else(|| project_id.to_string()),
                "section_id": section_id.or(issue.section_id.clone()),
                "parent_id": issue.parent_id.clone(),
                "labels": issue.labels.clone(),
                "assignee_id": issue.assignee_id.clone(),
                "checked": completed,
                "is_completed": completed,
                "completed_at": completed.then(|| now.clone()),
                "created_at": issue.created_at.map(|value| value.to_rfc3339()),
                "updated_at": issue.updated_at.map(|value| value.to_rfc3339()),
                "url": issue.url.clone().unwrap_or_else(|| todoist_task_url(&issue.id)),
                "due": issue.due.clone(),
                "deadline": issue.deadline.clone()
            })
        })
        .collect()
}

fn derive_sections(tasks: &[Value], issues: &[Issue], project_id: &str) -> Vec<Value> {
    let mut names = BTreeMap::new();
    for issue in issues {
        if issue.state_key() != "done" && !issue.state.trim().is_empty() {
            names
                .entry(issue.state_key())
                .or_insert(issue.state.clone());
        }
    }
    for task in tasks {
        if let Some(section_id) = task.get("section_id").and_then(json_id_from_value) {
            let name = task
                .get("section_name")
                .and_then(Value::as_str)
                .unwrap_or(section_id.as_str())
                .to_string();
            names.entry(sanitize_state_key(&name)).or_insert(name);
        }
    }
    names
        .into_iter()
        .map(|(key, name)| {
            json!({
                "id": format!("section-{key}"),
                "project_id": project_id,
                "name": name
            })
        })
        .collect()
}

fn derive_labels(tasks: &[Value]) -> Vec<Value> {
    let mut labels = BTreeSet::new();
    for task in tasks {
        for label in task
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            labels.insert(label.to_string());
        }
    }
    labels
        .into_iter()
        .enumerate()
        .map(|(index, name)| json!({ "id": format!("label-{}", index + 1), "name": name }))
        .collect()
}

fn ensure_task_defaults(tasks: &mut [Value], project_id: &str) {
    for task in tasks {
        let Some(map) = task.as_object_mut() else {
            continue;
        };
        if map.get("project_id").and_then(json_id_from_value).is_none() {
            map.insert(
                "project_id".to_string(),
                Value::String(project_id.to_string()),
            );
        }
        if map.get("url").and_then(Value::as_str).is_none()
            && let Some(id) = map.get("id").and_then(json_id_from_value)
        {
            map.insert("url".to_string(), Value::String(todoist_task_url(&id)));
        }
        map.entry("labels".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
    }
}

fn ensure_section_defaults(sections: &mut [Value], project_id: &str) {
    for section in sections {
        let Some(map) = section.as_object_mut() else {
            continue;
        };
        if map.get("project_id").and_then(json_id_from_value).is_none() {
            map.insert(
                "project_id".to_string(),
                Value::String(project_id.to_string()),
            );
        }
    }
}

fn ensure_project_defaults(projects: &mut [Value], project_id: &str) {
    for project in projects {
        let Some(map) = project.as_object_mut() else {
            continue;
        };
        if map.get("id").and_then(json_id_from_value).is_none() {
            map.insert("id".to_string(), Value::String(project_id.to_string()));
        }
    }
}

fn sync_labels_from_tasks(tasks: &[Value], labels: &mut Vec<Value>) {
    let existing = labels
        .iter()
        .filter_map(|label| label.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let mut missing = tasks
        .iter()
        .flat_map(|task| {
            task.get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|name| !existing.contains(name))
        .collect::<BTreeSet<_>>();
    let start = labels.len();
    for (index, name) in missing.iter().enumerate() {
        labels.push(json!({
            "id": format!("label-{}", start + index + 1),
            "name": name
        }));
    }
    missing.clear();
}

fn sync_labels_from_names(state: &mut MemoryState, names: &[String]) {
    let existing = state
        .labels
        .iter()
        .filter_map(|label| label.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    for label in names
        .iter()
        .filter(|label| !existing.contains(label.as_str()))
    {
        state.labels.push(json!({
            "id": format!("label-{}", state.labels.len() + 1),
            "name": label
        }));
    }
}

fn label_names(task: &Value) -> Vec<String> {
    task.get("labels")
        .and_then(Value::as_array)
        .map(|labels| {
            labels
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn next_counter(values: &[Value], fallback: u64) -> u64 {
    values
        .iter()
        .filter_map(|value| value.get("id"))
        .filter_map(json_id_from_value)
        .filter_map(|value| {
            value
                .rsplit('-')
                .next()
                .and_then(|part| part.parse::<u64>().ok())
        })
        .max()
        .map(|value| value + 1)
        .unwrap_or(fallback)
}

fn next_id(counter: &mut u64, prefix: &str) -> String {
    let id = format!("{prefix}-{}", *counter);
    *counter += 1;
    id
}

fn paginate_collection(values: Vec<Value>, cursor: Option<&str>, limit: usize) -> Value {
    let start = cursor
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_default();
    let end = start.saturating_add(limit).min(values.len());
    json!({
        "results": values[start..end].to_vec(),
        "next_cursor": (end < values.len()).then(|| end.to_string())
    })
}

fn read_limit(arguments: &Value) -> usize {
    arguments
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value.min(200) as usize)
        .unwrap_or(50)
}

fn sanitize_action_arguments(arguments: Value, allowed_keys: &[&str]) -> Value {
    let mut map = arguments.as_object().cloned().unwrap_or_default();
    map.retain(|key, _| allowed_keys.contains(&key.as_str()));
    Value::Object(map)
}

fn comment_target(arguments: &Value) -> Result<&'static str, TrackerError> {
    let has_task_id = arguments
        .get("task_id")
        .and_then(json_id_from_value)
        .is_some();
    let has_project_id = arguments
        .get("project_id")
        .and_then(json_id_from_value)
        .is_some();
    match (has_task_id, has_project_id) {
        (true, false) => Ok("task_id"),
        (false, true) => Ok("project_id"),
        _ => Err(TrackerError::TrackerOperationUnsupported(
            "exactly one of `task_id` or `project_id` is required".to_string(),
        )),
    }
}

fn required_string(arguments: &Value, key: &str, action: &str) -> Result<String, TrackerError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            TrackerError::TrackerOperationUnsupported(format!("`{key}` is required for {action}"))
        })
}

fn copy_if_present(arguments: &Value, map: &mut Map<String, Value>, key: &str) {
    if let Some(value) = arguments.get(key) {
        map.insert(key.to_string(), value.clone());
    }
}

fn find_by_id_mut<'a>(
    values: &'a mut [Value],
    id: &str,
    resource: &str,
) -> Result<&'a mut Value, TrackerError> {
    values
        .iter_mut()
        .find(|value| json_id(value.get("id")).as_deref() == Some(id))
        .ok_or_else(|| not_found(resource, id))
}

fn value_object_mut(value: &mut Value) -> Result<&mut Map<String, Value>, TrackerError> {
    value.as_object_mut().ok_or_else(|| {
        TrackerError::TrackerOperationUnsupported("fixture value must be an object".to_string())
    })
}

fn json_id(value: Option<&Value>) -> Option<String> {
    value.and_then(json_id_from_value)
}

fn json_id_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn not_found(resource: &str, id: &str) -> TrackerError {
    TrackerError::TodoistApiStatus {
        status: 404,
        body: format!("{resource} `{id}` not found"),
    }
}

fn sanitize_state_key(state: &str) -> String {
    normalize_state_name(state).replace(' ', "-")
}

fn denormalize_priority(priority: i64) -> i64 {
    match priority {
        1 => 4,
        2 => 3,
        3 => 2,
        4 => 1,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use crate::{config::ServiceConfig, tracker::TrackerClient};

    use super::MemoryTracker;

    #[tokio::test]
    async fn fetch_candidate_issues_filters_by_active_states_and_excludes_subtasks() {
        let dir = tempdir().expect("tempdir");
        let fixture = dir.path().join("tasks.json");
        std::fs::write(
            &fixture,
            r#"{
  "tasks": [
    {"id":"1","content":"Todo","project_id":"proj","section_id":"sec-todo","labels":[]},
    {"id":"2","content":"Child","project_id":"proj","section_id":"sec-todo","parent_id":"1","labels":[]},
    {"id":"3","content":"Done","project_id":"proj","checked":true,"labels":[]}
  ],
  "sections": [
    {"id":"sec-todo","project_id":"proj","name":"Todo"}
  ]
}"#,
        )
        .expect("fixture");

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "memory",
                    "fixture_path": fixture,
                    "project_id": "proj"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let tracker = MemoryTracker::new(config);
        let issues = tracker.fetch_candidate_issues().await.expect("issues");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "TD-1");
    }

    #[tokio::test]
    async fn fetch_issue_states_by_ids_supports_yaml_envelope() {
        let dir = tempdir().expect("tempdir");
        let fixture = dir.path().join("issues.yaml");
        std::fs::write(
            &fixture,
            r#"issues:
  - id: "1"
    identifier: "MEM-1"
    title: "Todo"
    state: "In Progress"
    labels: ["backend"]
  - id: "2"
    identifier: "MEM-2"
    title: "Done"
    state: "Done"
"#,
        )
        .expect("fixture");

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "memory",
                    "fixture_path": fixture,
                    "project_id": "proj"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let tracker = MemoryTracker::new(config);
        let issues = tracker
            .fetch_issue_states_by_ids(&["1".to_string()])
            .await
            .expect("issues");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "1");
        assert_eq!(issues[0].labels, vec!["backend"]);
    }

    #[tokio::test]
    async fn memory_tracker_supports_todoist_tool_mutations() {
        let dir = tempdir().expect("tempdir");
        let fixture = dir.path().join("state.json");
        std::fs::write(
            &fixture,
            r#"{
  "tasks": [
    {"id":"task-1","content":"Parent","project_id":"proj","section_id":"sec-todo","labels":["backend"],"priority":4}
  ],
  "sections": [
    {"id":"sec-todo","project_id":"proj","name":"Todo"},
    {"id":"sec-progress","project_id":"proj","name":"In Progress"}
  ],
  "current_user": {"id":"user-1","name":"Memory User"},
  "projects": [{"id":"proj","name":"Memory Project","is_shared":true}],
  "collaborators": [{"id":"user-1","project_id":"proj","name":"Memory User"}],
  "labels": [{"id":"label-1","name":"backend"}],
  "user_plan_limits": {"comments": true, "reminders": true}
}"#,
        )
        .expect("fixture");

        let config = ServiceConfig::from_map(
            json!({
                "tracker": {
                    "kind": "memory",
                    "fixture_path": fixture,
                    "project_id": "proj",
                    "assignee": "me"
                }
            })
            .as_object()
            .expect("object"),
        )
        .expect("config");

        let tracker = MemoryTracker::new(config);
        let projects = tracker.list_projects(json!({})).await.expect("projects");
        assert_eq!(projects["results"].as_array().expect("results").len(), 1);

        let labels = tracker.list_labels(json!({})).await.expect("labels");
        assert_eq!(labels["results"].as_array().expect("results").len(), 1);

        let comment = tracker
            .create_comment(json!({"task_id": "task-1", "content": "## Codex Workpad"}))
            .await
            .expect("comment");
        let comment_id = comment["id"].as_str().expect("id");
        let updated = tracker
            .update_comment(comment_id, json!({"content": "updated"}))
            .await
            .expect("updated");
        assert_eq!(updated["content"], "updated");

        let moved = tracker
            .move_task("task-1", json!({"section_id": "sec-progress"}))
            .await
            .expect("moved");
        assert_eq!(moved["section_id"], "sec-progress");

        let subtask = tracker
            .create_task(json!({"content": "Child", "parent_id": "task-1", "labels": ["backend", "frontend"]}))
            .await
            .expect("subtask");
        assert_eq!(subtask["parent_id"], "task-1");

        let reminder = tracker
            .create_reminder(json!({"task_id": "task-1", "type": "relative", "minute_offset": 30}))
            .await
            .expect("reminder");
        let reminder_id = reminder["id"].as_str().expect("reminder id");
        tracker
            .update_reminder(reminder_id, json!({"minute_offset": 15}))
            .await
            .expect("update reminder");
        let reminders = tracker
            .list_reminders(json!({"task_id": "task-1"}))
            .await
            .expect("reminders");
        assert_eq!(reminders["results"].as_array().expect("results").len(), 1);
        tracker
            .delete_reminder(reminder_id)
            .await
            .expect("delete reminder");

        tracker.close_task("task-1").await.expect("close");
        tracker.reopen_task("task-1").await.expect("reopen");

        let issues = tracker.fetch_candidate_issues().await.expect("issues");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].assignee_id.as_deref(), None);
    }
}
