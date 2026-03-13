# Logging Best Practices

This guide defines the Rust Todoist logging contract for Symphony. It follows the same intent as the Elixir logging guidance: make failures diagnosable from one log slice without reruns.

## Goals

- Make logs searchable by issue and session.
- Preserve stable lifecycle wording for dashboards and grep-based debugging.
- Keep high-signal operator context in file logs without dumping large payloads.

## Required Context Fields

When a log line is tied to a tracked task, include both identifiers:

- `issue_id`: stable Todoist task id
- `issue_identifier`: Symphony synthetic identifier such as `TD-123456`

When a log line is tied to a Codex turn, include:

- `session_id`
- `thread_id`
- `turn_id`

When execution location matters, include:

- `worker_host`
- `workspace`

## Message Design

- Use explicit `key=value` pairs in the message text for durable grepability.
- Prefer deterministic lifecycle verbs such as `starting`, `started`, `completed`, `failed`, `queued`, `timeout`, and `skipped`.
- Include a concise `error=` or `reason=` field when something fails.
- Avoid logging large raw JSON blobs by default.

## Scope Guidance

- `cli`: log `runtime_start`, component startup, and `runtime_shutdown`.
- `orchestrator`: log dispatch, retry, reconciliation, worker start/completion/failure, and cleanup with issue context.
- `codex`: log turn start/completion/failure/cancel, session/thread start, and stderr warnings with session and worker context.
- `dynamic_tool`: log tool/action start with the narrowest available Todoist object scope such as `task_id`, `project_id`, or `comment_id`.

## Checklist For New Logs

- Is this event tied to a task? Include `issue_id` and `issue_identifier`.
- Is this event tied to a Codex session? Include `session_id`, `thread_id`, and `turn_id`.
- Is the execution location relevant? Include `worker_host` and `workspace`.
- Is the wording stable enough to search and alert on later?
