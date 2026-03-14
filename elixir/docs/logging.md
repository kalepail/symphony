# Logging Best Practices

This guide defines logging conventions for the Elixir runtime so Codex can
diagnose failures quickly and operators know where the file logs actually live.

## Log Location And Rotation

Elixir writes file logs relative to the process current working directory unless
`--logs-root` is provided.

- Launch from `elixir/`: `elixir/log/symphony.log`
- Launch from repo root: `log/symphony.log`
- Launch from `rust-todoist/`: `rust-todoist/log/symphony.log`
- Override with `--logs-root /path/to/root`: `/path/to/root/log/symphony.log`

Rotation is size-based through OTP's `:logger_disk_log_h` wrap mode, not daily:

- Data files: wrapped log files rooted at `symphony.log`
- Metadata files: `symphony.log.idx` and `symphony.log.siz`
- Rotation cap: 5 files at 10 MB each

Treat the total retained log data as bounded to roughly 50 MB plus small
metadata files.

## Goals

- Make logs searchable by issue and session.
- Capture enough execution context to identify root cause without reruns.
- Keep messages stable so dashboards and alerts are reliable.

## Required Context Fields

When logging issue-related work, include:

- `issue_id`: Linear internal UUID
- `issue_identifier`: human ticket key such as `MT-620`

When logging Codex execution lifecycle events, include:

- `session_id`

Include `worker_host` and `workspace` when location is relevant to the failure.

## Message Design

- Use explicit `key=value` pairs in message text for high-signal fields.
- Prefer deterministic lifecycle wording for recurring events.
- Include the action outcome (`completed`, `failed`, `retrying`, `stalled`) and
  the reason or error when available.
- Avoid logging large payloads unless they are required for debugging.

## Scope Guidance

- `AgentRunner`: log start/completion/failure with issue context and
  `session_id` when known.
- `Orchestrator`: log dispatch, retry, terminal/non-active transitions, and
  worker exits with issue context. Include `session_id` whenever running-entry
  data has it.
- `Codex.AppServer`: log session start/completion/error with issue context and
  `session_id`.

## Checklist For New Logs

- Is this event tied to a Linear issue? Include `issue_id` and
  `issue_identifier`.
- Is this event tied to a Codex session? Include `session_id`.
- Is the failure reason present and concise?
- Is the message format consistent with existing lifecycle logs?
