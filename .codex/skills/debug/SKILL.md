---
name: debug
description:
  Investigate stuck runs and execution failures by tracing Symphony and Codex
  logs with issue/session identifiers; use when runs stall, retry repeatedly, or
  fail unexpectedly.
---

# Debug

## Goals

- Find why a run is stuck, retrying, or failing.
- Correlate tracker issue or task identity to a Codex session quickly.
- Read the right logs in the right order to isolate root cause.

## Log Sources

`log/symphony.log` is relative to the runtime current working directory unless
`--logs-root` is passed. In this repo that usually means:

- running from repo root: `log/symphony.log`
- running from `rust-todoist/`: `rust-todoist/log/symphony.log`
- running from `elixir/`: `elixir/log/symphony.log`
- overriding with `--logs-root /path`: `/path/log/symphony.log`

Check rotated logs too:

- Rust: `symphony.log.1` through `symphony.log.5`
- Elixir: wrapped disk-log files rooted at `symphony.log`, plus `.idx` and
  `.siz` metadata

## Correlation Keys

- `issue_identifier`: human ticket or task key such as `ABC-123`
- `issue_id`: tracker internal ID
- `run_id`: orchestrator attempt identifier
- `session_id`: Codex session identifier
- `thread_id`: Codex thread identifier
- `turn_id`: Codex turn identifier

Prefer `run_id` as the first Rust correlation key when you have it, then narrow
to `session_id` and `turn_id`.

## Quick Triage

1. Confirm the runtime and the working directory used to launch it.
2. Search the correct `log/symphony.log*` path for `issue_identifier` first.
3. Extract `run_id` and `session_id` from matching lines.
4. Trace that `run_id` or `session_id` across start, stream, retries,
   completion/failure, and stall handling logs.
5. Decide the failure class before reading more widely.

## Commands

```bash
# 1) Narrow by ticket key
rg -n "issue_identifier=<tracker-key>" log/symphony.log*

# 2) If available, narrow by runtime attempt
rg -n "run_id=<run-id>" log/symphony.log*

# 3) If needed, narrow by tracker internal ID
rg -n "issue_id=<tracker-internal-id>" log/symphony.log*

# 4) Pull session IDs seen for that run or ticket
rg -o "session_id=[^ ;]+" log/symphony.log* | sort -u

# 5) Trace one session end-to-end
rg -n "session_id=<session-id>|thread_id=<thread-id>|turn_id=<turn-id>" log/symphony.log*

# 6) Focus on stuck, timeout, and retry signals
rg -n "reconcile=status=stalled|turn_timeout|response_timeout|dynamic_tool_call|todoist .*retry|Issue stalled|Codex session failed|Codex session ended with error" log/symphony.log*
```

## Investigation Flow

1. Locate the ticket slice:
   - Search by `issue_identifier=<KEY>`.
   - If present, pin to one `run_id=<...>`.
2. Establish timeline:
   - Identify dispatch/start lines for that run.
   - Follow with `session_id`, `thread_id`, and `turn_id`.
3. Classify the problem:
   - Todoist poller instability:
     - tracker retry warning with `source=poller`
     - usually outside an active session
   - Todoist tool-call retry:
     - tracker retry warning with `source=tool_call`
     - usually shares `issue_id`, `issue_identifier`, `run_id`, and `session_id`
   - Tracker blocking on long `retry_after`:
     - `reason=rate_limited`
     - `retry_after_secs` or `detail=retry_after=...`
   - Codex timeout or stall:
     - `response_timeout`, `turn_timeout`, or `reconcile=status=stalled`
4. Validate scope:
   - Check whether failures are isolated to one run or repeating across
     multiple tasks.
5. Capture evidence:
   - Save key log lines with timestamps plus the correlation keys you used.
   - Record the exact failing stage, not just the symptom.

## Reading Rust Session Logs

In the Rust runtime, the most useful lifecycle chain is:

1. run and session start lines containing `run_id=...`
2. `dynamic_tool_call ...`
3. `codex_turn_summary ...`
4. terminal lifecycle lines such as completion, `response_timeout`,
   `turn_timeout`, or `reconcile=status=stalled`

When Todoist tool retries occur inside a turn, expect the tracker warning and
the final tool/session summary to line up:

- tracker warning: `reason=... source=tool_call detail=...`
- tool summary: `retry_count=... retry_wait_secs=...`
- turn summary: `tool_retry_count_total=... tool_retry_wait_secs_total=...`

## Reading Elixir Session Logs

In the Elixir runtime, keep the investigation narrower:

1. `issue_identifier=...` and `issue_id=...`
2. `session_id=...`
3. session start/completion/error lines
4. stall recovery such as `Issue stalled ... restarting with backoff`

Focus on lifecycle timing and session state rather than per-tool retry counters.

## Notes

- Prefer `rg` over `grep` for speed on large logs.
- Check rotated logs before concluding the data is missing.
- If a new runtime log omits `run_id`, `issue_id`, `issue_identifier`,
  `session_id`, `thread_id`, or `turn_id` where they should exist, treat that as
  a logging gap worth fixing.
