# Logging Best Practices

This guide defines the Rust Todoist logging contract for Symphony. It keeps the
operator-facing wording stable enough for grep/debug workflows while preserving
enough structured context to explain what the runtime was doing.

## Log Location And Rotation

Rust writes file logs relative to the process current working directory unless
`--logs-root` is provided.

- Launch from `rust-todoist/`: `rust-todoist/log/symphony.log`
- Launch from repo root: `log/symphony.log`
- Launch from `elixir/`: `elixir/log/symphony.log`
- Override with `--logs-root /path/to/root`: `/path/to/root/log/symphony.log`

Rotation is size-based, not daily:

- Active file: `symphony.log`
- Archives: `symphony.log.1` through `symphony.log.5`
- Rotation rule: rotate before a write would exceed 10 MB
- Retention rule: delete the oldest archive beyond `.5`

Worst-case retained log data is roughly 60 MB: the current file plus 5
archives.

## Goals

- Make logs searchable by task, run, and session.
- Preserve stable lifecycle wording for dashboards and grep-based debugging.
- Keep high-signal operator context in file logs without dumping large payloads.

## Required Context Fields

When a log line is tied to a tracked task, include:

- `issue_id`: stable Todoist task id
- `issue_identifier`: Symphony synthetic identifier such as `TD-123456`

When a log line is tied to a runtime attempt, include:

- `run_id`: stable per-run correlation key for one orchestrator attempt

When a log line is tied to a Codex turn, include:

- `session_id`
- `thread_id`
- `turn_id`

When execution location matters, include:

- `worker_host`
- `workspace`

## Message Design

- Use explicit `key=value` pairs in the message text for durable grepability.
- Prefer deterministic lifecycle verbs such as `starting`, `started`,
  `completed`, `failed`, `queued`, `timeout`, `stalled`, and `skipped`.
- Include a concise `error=` or `reason=` field when something fails.
- Avoid logging large raw JSON blobs by default.

## Scope Guidance

- `cli`: log `runtime_start`, component startup, and `runtime_shutdown`.
- `orchestrator`: log dispatch, retry, reconciliation, worker
  start/completion/failure, and cleanup with task context and `run_id`.
- `codex`: log session/thread/turn start, completion, failure, timeout, and
  stderr warnings with session and worker context.
- `dynamic_tool`: log tool/action completion with the narrowest available
  Todoist object scope such as `task_id`, `project_id`, or `comment_id`.
- `tracker`: log Todoist retry warnings with stable retry buckets plus enough
  detail to distinguish timeouts, 429s, and transient 5xx/transport failures.

## Rust Error Taxonomy

Rust uses a few operator-facing error buckets in stable places:

- Tracker retry warnings:
  - `reason=transport_error`
  - `reason=rate_limited`
  - `reason=server_error`
- Dynamic tool telemetry fields:
  - `tool`
  - `action`
  - `upstream_service`
  - `error_kind`
  - `http_status`
  - `retry_after_secs`
  - `retry_count`
  - `retry_wait_secs`
- Codex lifecycle failures:
  - `response_timeout`
  - `turn_timeout`
  - `stalled`

Codex turn summaries and stalled-run logs also carry:

- `last_tool`: most recent tool action seen in the turn
- `error`: the latest concise upstream/tool failure detail when available

Tracker retry warnings also include:

- `source`: `poller`, `tool_call`, `startup`, or `sync`
- `request_lane`: `rest` or `sync`
- `detail`: concise raw failure detail for transport/status errors

When the retry happens inside an active tool call, the warning should also carry
the available task/session correlation keys such as `issue_id`,
`issue_identifier`, `run_id`, and `session_id`.

## Operator Guide

Use the fields below to distinguish common failure classes quickly:

- Todoist poller instability:
  - tracker warning with `source=poller`
  - usually no `session_id`
  - often appears during fetch/sync outside an active Codex turn
- Shared Todoist rate-limit wait:
  - `todoist_rate_budget status=throttled source=...`
  - includes `retry_after_secs`, `throttled_for_secs`, and `next_request_in_secs`
  - later recovery emits `todoist_rate_budget status=cleared ...`
- Todoist tool-call retries:
  - tracker warning with `source=tool_call`
  - includes task/run/session keys when available
  - matching `dynamic_tool_call` and `codex_turn_summary` lines should expose
    `retry_count` and `retry_wait_secs`
- Codex app-server stall or timeout:
  - session/turn lifecycle lines show `response_timeout`, `turn_timeout`, or
    `reconcile=status=stalled`
  - tracker retry warnings are absent or secondary
  - `codex_turn_summary` and `reconcile=status=stalled` should expose the last
    tool action plus the last concise error detail
- GitHub publish-path failures:
  - `dynamic_tool_call ... tool=github_api ... status=failed`
  - `http_status`, `retry_after_secs`, and `error=` should indicate whether the
    runtime hit rate limiting, abuse throttling, or request validation failures
  - the matching `codex_turn_summary` or stalled-run log should preserve the
    last `github_api` action so operators can tell which publish step wedged
- Tracker blocking on long `retry_after`:
  - tracker warning shows `reason=rate_limited`
  - `retry_after_secs` or `detail=retry_after=...`
  - no app-server timeout is implied by itself

## Checklist For New Logs

- Is this event tied to a task? Include `issue_id` and `issue_identifier`.
- Is this event tied to one runtime attempt? Include `run_id`.
- Is this event tied to a Codex session? Include `session_id`, `thread_id`, and
  `turn_id`.
- Is the execution location relevant? Include `worker_host` and `workspace`.
- Is the wording stable enough to search and alert on later?
