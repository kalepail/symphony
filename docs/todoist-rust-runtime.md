# Todoist Runtime Research Brief

Status: research brief and compact handoff  
Reviewed: March 11, 2026  
Baseline reviewed against trunk: `72d6fce`

For the fuller delivery plan and implementation checklist, see:

- [Linear to Todoist migration PRD](./linear-to-todoist-prd.md)
- [Linear to Todoist migration technical spec](./linear-to-todoist-spec.md)

## Purpose

This document is the compact baseline for the Todoist migration.

It reflects two current realities:

- trunk already contains a real Rust runtime in [`rust/`](../rust/README.md)
- the safest migration posture is to build `rust-todoist/` beside `rust/`, not replace `rust/`
  immediately

Use this brief when starting a fresh review pass so the core repo facts and Todoist constraints do
not need to be rediscovered from scratch.

## Source Material

Repository baseline:

- [README.md](../README.md)
- [SPEC.md](../SPEC.md)
- [rust/README.md](../rust/README.md)
- [rust/WORKFLOW.md](../rust/WORKFLOW.md)
- [rust/src/config.rs](../rust/src/config.rs)
- [rust/src/issue.rs](../rust/src/issue.rs)
- [rust/src/tracker/mod.rs](../rust/src/tracker/mod.rs)
- [rust/src/tracker/linear.rs](../rust/src/tracker/linear.rs)
- [rust/src/tracker/memory.rs](../rust/src/tracker/memory.rs)
- [rust/src/dynamic_tool.rs](../rust/src/dynamic_tool.rs)
- [rust/src/orchestrator.rs](../rust/src/orchestrator.rs)
- [rust/src/http.rs](../rust/src/http.rs)
- [elixir/README.md](../elixir/README.md)

Official Todoist references:

- [Todoist API v1](https://developer.todoist.com/api/v1/)
- [Find your API token](https://www.todoist.com/help/articles/find-your-api-token-Jpzx9IIlB)
- [Usage limits in Todoist](https://www.todoist.com/help/articles/usage-limits-in-todoist-e5rcSY)
- [Official Todoist Python SDK docs](https://doist.github.io/todoist-api-python/)
- [Official Todoist TypeScript SDK docs](https://doist.github.io/todoist-api-typescript/)

## Baseline Summary

Latest trunk now has:

- a real Rust runtime with CLI, workflow reload, tracker boundary, orchestrator, workspace
  manager, Codex app-server client, HTTP status routes, and logging
- a deterministic `memory` tracker for local and CI tests
- tracked smoke workflows and Rust CI
- that runtime is still fully Linear-backed for live work

The migration target is therefore:

- preserve the current Rust runtime as a known reference
- build `rust-todoist/` with the same service skeleton
- replace only the Linear-coupled tracker and workflow assumptions

## What We Can Reuse From `rust/`

The existing Rust runtime already gives us the hard non-tracker service shape:

- CLI startup and acknowledgement
- `WORKFLOW.md` loading and reload
- typed config parsing with env and path resolution
- terminal and web observability config plus shared presentation logic
- orchestrator loop with retries and continuation
- workspace lifecycle hooks and path confinement
- Codex app-server JSON-RPC over stdio
- request-user-input auto-answer handling
- explicit thread and turn sandbox-policy passthrough
- HTTP observability routes and SSE stream
- ANSI terminal dashboard rendering
- rotating file logs

Those pieces should be copied into `rust-todoist/` with minimal change first, then adapted only
where Todoist semantics actually require it.

## Current Linear Coupling To Replace

The current Rust runtime is Linear-coupled in these specific ways:

- live tracker config only supports `tracker.kind=linear`
- tracker config is `project_slug`, `LINEAR_API_KEY`, `LINEAR_ASSIGNEE`, and a GraphQL endpoint
- the tracker trait still exposes `raw_graphql`
- the tracker-specific live dynamic tool is `linear_graphql`
- the runtime also exposes a host-side `github_api` tool when host GitHub auth is available
- the default prompt and continuation copy say "Linear issue"
- the default workflow depends on Linear comments, Linear MCP or `linear_graphql`, and Linear
  follow-up issue creation
- startup cleanup assumes terminal-state queries
- dispatch eligibility still applies blocker gating
- smoke workflows and smoke docs are Linear-specific

## Todoist Facts That Change The Design

Official Todoist docs currently establish the following facts:

- the current unified Todoist API lives at `https://api.todoist.com/api/v1`
- relevant list endpoints are cursor-paginated with `results` and `next_cursor`
- the default page size is 50 and the maximum is 200
- tasks, comments, sections, projects, collaborators, and activities are paginated
- comments are plan-gated through `user_plan_limits.current.comments`
- completed-task archive access is plan-gated through `user_plan_limits.current.completed_tasks`
- comments are capped at 15,000 characters
- Todoist allows 300 active tasks per project and 20 sections per project
- task assignment is meaningful for shared projects only
- `retry_after` is documented in error payloads for rate-limited requests
- Todoist hosts an official MCP endpoint at [ai.todoist.net/mcp](https://ai.todoist.net/mcp)
- official SDKs exist for Python and TypeScript, but no official Rust SDK was identified

The final spec now locks the implementation choice:

- `rust-todoist/` should use a native Rust HTTP client against Todoist API v1
- the official Python and TypeScript SDKs are reference implementations, not runtime dependencies

## Todoist-Native Runtime Model

### Tracker Contract

`rust-todoist/WORKFLOW.md` should support:

```yaml
tracker:
  kind: todoist
  base_url: https://api.todoist.com/api/v1
  api_key: $TODOIST_API_TOKEN
  project_id: "6XGgm6PHrGgMpCFX"
  assignee: me
  active_states:
    - Todo
    - In Progress
    - Human Review
    - Merging
    - Rework
  terminal_states:
    - Cancelled
    - Duplicate
```

### State Model

- active workflow states -> Todoist section names
- `Done` -> close task
- `Cancelled` / `Duplicate` -> explicit open-task sections

### Identifier Model

Use:

- `TD-<task_id>`

This replaces Linear ticket keys for workspace naming, logs, and status routes.

### Workpad Model

Preserve the single persistent comment model with:

- marker header: `## Codex Workpad`
- optional secondary marker: `<!-- symphony:workpad -->`

Because Todoist comments are limited to 15,000 characters, `rust-todoist/` must use bounded
workpad compaction rather than append-only growth.

## Non-Parity Gaps To Accept Explicitly

Todoist does not provide direct equivalents for:

- blocker graphs
- short human ticket keys like `ABC-123`
- branch metadata

Therefore:

- `blocked_by` is not ported as a functional dispatch rule
- `branch_name` remains empty in the Todoist runtime
- synthetic identifiers replace Linear ticket keys

## Required Runtime Changes

### Cleanup

Do not port the current Rust startup cleanup as-is. Todoist completed tasks disappear from active
task reads.

Required replacement:

- derive stale workspaces from the difference between workspace directories and all open project
  items

### Refresh Reconciliation

Current Rust refresh only processes returned rows. That is insufficient for Todoist.

Required replacement:

- treat any requested task ID that is missing from refresh results as no longer active

### Assignment

`tracker.assignee` must be validated against Todoist current-user and collaborator data because
assignee semantics differ on shared vs non-shared projects.

### Dynamic Tool

Preserve `github_api` and replace `linear_graphql` with a structured `todoist` tool covering:

- get current user
- list/get tasks
- list/get sections
- list/create/update comments
- update task
- close task
- reopen task
- create task for follow-up work

## Recommended Migration Shape

The pragmatic implementation sequence is:

1. create `rust-todoist/` by copying the current Rust service skeleton
2. preserve `memory` tracker support inside the new runtime
3. preserve the current `observability` config and terminal/SSE surfaces
4. implement `tracker.kind: todoist` and `tracker.project_id`
5. add `tracker/todoist.rs`
6. replace `linear_graphql` with structured Todoist actions while keeping `github_api`
7. update cleanup and missing-ID reconciliation for Todoist semantics
8. rewrite the workflow, smoke docs, and operator surfaces for Todoist
9. validate feature parity against `rust/`

## Conclusion

The Todoist migration is best understood as:

- a sibling runtime build, not a wholesale replacement of `rust/`
- a feature-parity exercise against the current Rust runtime
- a tracker-model conversion that must explicitly account for Todoist differences in comments,
  assignment, completion, pagination, and missing active-task visibility

That is the baseline assumed by the PRD and technical spec.
