# Linear to Todoist Migration Spec

Status: proposed  
Last updated: March 11, 2026  
Baseline reviewed against trunk: `72d6fce`

## Overview

This spec defines the implementation work required to migrate Symphony away from Linear by adding a
Todoist-backed sibling runtime at `rust-todoist/`.

This is the final implementation spec for the migration work. It is intentionally opinionated. It
locks the migration posture, the API strategy, the runtime boundaries, and the feature-parity bar
that must be met before any later consolidation discussion.

## Scope

In scope:

- build `rust-todoist/` beside [`rust/`](../rust/README.md)
- preserve [`rust/`](../rust/README.md) as the Linear reference runtime during migration
- preserve the current service skeleton: CLI, workflow reload, orchestrator, workspace manager,
  Codex app-server integration, logging, terminal dashboard, HTTP dashboard, and SSE stream
- preserve the deterministic `memory` tracker path for local and CI coverage
- replace Linear-specific live tracker behavior with Todoist-native behavior
- replace `linear_graphql` with a structured `todoist` tool
- rewrite workflow, prompt, smoke, and operator docs for Todoist
- reach feature-for-feature parity against the current Rust runtime where Todoist has an equivalent

Out of scope:

- rewriting [`rust/`](../rust/README.md) in place
- deleting the Linear runtime before Todoist parity is proven
- porting Todoist into [`elixir/`](../elixir/README.md)
- making webhooks the v1 source of truth
- depending on Todoist completed-task archive access for runtime correctness
- preserving Linear-only concepts with no Todoist equivalent, such as blocker graphs, issue
  relations, or branch metadata

## Source Hierarchy

This spec is based on the following source hierarchy.

Primary external sources:

- [Todoist API v1](https://developer.todoist.com/api/v1/)
- [Todoist API token guide](https://www.todoist.com/help/articles/find-your-api-token-Jpzx9IIlB)
- [Todoist usage limits](https://www.todoist.com/help/articles/usage-limits-in-todoist-e5rcSY)
- [Introduction to tasks](https://www.todoist.com/help/articles/introduction-to-tasks-080OAXric)
- [Introduction to sub-tasks](https://www.todoist.com/help/articles/introduction-to-sub-tasks-kMamDo)
- [Introduction to sections](https://www.todoist.com/help/articles/introduction-to-sections-rOrK0aEn)
- [Use the board layout in Todoist](https://www.todoist.com/help/articles/use-the-board-layout-in-todoist-AiAVsyEI)
- [Introduction to comments and file uploads](https://www.todoist.com/help/articles/introduction-to-comments-and-file-uploads-CwiA50)
- [Introduction to labels](https://www.todoist.com/help/articles/introduction-to-labels-dSo2eE)
- [What are reminders?](https://www.todoist.com/help/articles/what-are-reminders-Mn0qQ0hh)
- [Collaborate with friends or family in Todoist](https://www.todoist.com/help/articles/collaborate-with-friends-or-family-in-todoist-tzkGUy)
- [Customize views in Todoist](https://www.todoist.com/help/articles/customize-views-in-todoist-AoHhBxFdZ)
- [Official Todoist Python SDK docs](https://doist.github.io/todoist-api-python/)
- [Official Todoist TypeScript SDK docs](https://doist.github.io/todoist-api-typescript/)
- [Doist/todoist-api-python](https://github.com/Doist/todoist-api-python)
- [Doist/todoist-api-typescript](https://github.com/Doist/todoist-api-typescript)

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
- [rust/src/observability.rs](../rust/src/observability.rs)
- [elixir/README.md](../elixir/README.md)

Research note:

- Perplexity and parallel MCPs were used for triangulation and freshness checks.
- Official Todoist docs and official SDK repos are treated as the source of truth where Perplexity
  was incomplete or inconsistent.

## Locked Decisions

The following decisions are now locked for implementation.

### 1. Sibling Runtime, Not In-Place Replacement

Do not replace `rust/` in place.

Do this instead:

1. keep [`rust/`](../rust/README.md) as the working Linear reference
2. create `rust-todoist/` beside it
3. port the proven runtime skeleton first
4. adapt only the tracker-shaped, workflow-shaped, and observability wording surfaces
5. defer any shared-library extraction until after parity is proven

### 2. Native Rust HTTP Client, Not Embedded Foreign SDKs

`rust-todoist/` will use a native Rust HTTP client built on `reqwest`.

It will not embed the official Python SDK or the official TypeScript SDK as runtime dependencies.

Reasons:

- Todoist does not provide an official Rust SDK
- the current Rust runtime already uses direct HTTP clients successfully
- direct Rust control is better for retries, backoff, pagination, logging, and error mapping
- embedding Python or Node would increase operational complexity and reduce parity with the current
  Rust runtime architecture

The official Python and TypeScript SDKs are still valuable, but only as:

- reference implementations for endpoint semantics
- smoke/debug tools during development
- evidence of current supported API behavior

### 3. Resource Endpoints First, `/sync` Only Where It Earns Its Keep

`rust-todoist/` will use Todoist API v1 resource endpoints as the primary operational surface.

Primary resource endpoints:

- `/api/v1/tasks`
- `/api/v1/tasks/{task_id}`
- `/api/v1/tasks/{task_id}/close`
- `/api/v1/tasks/{task_id}/reopen`
- `/api/v1/comments`
- `/api/v1/comments/{comment_id}`
- `/api/v1/sections`
- `/api/v1/projects/{project_id}`
- `/api/v1/projects/{project_id}/collaborators`
- `/api/v1/user`

`/api/v1/sync` will be used only for targeted bootstrap reads where it materially reduces guesswork
or startup probing. In v1, that means:

- reading `user_plan_limits`
- optionally reading `collaborators` or `sections` only if later implementation evidence shows
  resource endpoints are insufficient

It will not be the primary polling loop.

### 4. Polling Remains the Source of Truth

Todoist webhooks exist, but v1 correctness will remain poll-driven.

Webhooks are explicitly future work only. They may later be used to shorten reaction time, but the
runtime must remain correct when webhooks are absent, delayed, duplicated, reordered, or disabled.

### 5. Preserve `memory` and `github_api`

The new runtime must preserve:

- the `memory` tracker for deterministic local and CI coverage
- the host-side `github_api` tool path already present in Rust trunk

Replacing `linear_graphql` with `todoist` is tracker work. Removing `github_api` would be a
regression and is not part of this migration.

### 6. Feature Parity Is Measured Against Current Rust, Not Elixir

Elixir remains useful as historical context, but the migration acceptance bar is parity with the
current Rust runtime on trunk, including:

- workflow reload behavior
- stale retry suppression
- poll coalescing
- workspace path confinement
- thread and turn sandbox-policy passthrough
- terminal dashboard
- web dashboard and `/api/v1/stream`
- preserved host-side GitHub fallback tool

## Official Todoist API and SDK Assessment

The following external facts materially shape the implementation.

### Current API Surface

Official Todoist docs currently describe a unified API under:

- `https://api.todoist.com/api/v1`

The docs site is hosted on `developer.todoist.com`, but the runtime must call the API host
`api.todoist.com`.

Key official API facts:

- Todoist API v1 combines former REST and Sync surfaces under a unified documentation set
- relevant resource endpoints use cursor-based pagination
- paginated responses use `results` and `next_cursor`
- default page size is `50`
- maximum page size is `200`
- paginated endpoints include tasks, comments, sections, projects, project collaborators, and
  activities
- POST requests have a documented `1 MiB` request-body limit
- rate-limit and other errors can include `retry_after` in `error_extra`

### Relevant Hard Product Limits

Official Todoist help and plan docs currently expose limits that matter for runtime design:

- `300` active tasks per project
- `20` sections per project
- `15,000` characters per comment

These limits are materially helpful for implementation:

- full-project polling is bounded and cheap for Symphony's intended project-per-workflow model
- section resolution is small enough to cache aggressively
- workpad comments must be compacted rather than grown without bound

### Plan-Gated Capabilities

Official Todoist docs currently expose `user_plan_limits.current` through `/sync`.

Important fields for Symphony:

- `comments`
- `completed_tasks`
- `activity_log`
- `activity_log_limit`
- `max_sections`
- `max_tasks`
- `upload_limit_mb`

Implications:

- comment support must be validated at startup
- completed-task archive access cannot be assumed
- activity log is optional and cannot be made a correctness dependency

### Official SDK Availability

Official SDKs exist for:

- Python
- TypeScript

No official Rust SDK was identified in current Todoist official sources.

Official SDK freshness reviewed on March 11, 2026:

- `Doist/todoist-api-typescript`
  - latest release: `v6.10.0` published February 27, 2026
  - latest visible main-branch commit in review: March 11, 2026
  - docs explicitly state migration to Todoist API v1
  - supports custom fetch and preserved retry/error behavior
- `Doist/todoist-api-python`
  - latest visible tags in review: `v3.2.1`, `v3.2.0`
  - latest visible main-branch commit in review: March 6, 2026
  - README documents `TodoistAPIAsync` using `httpx.AsyncClient`
  - docs and README show paginated comments iteration

### Official MCP

Todoist now publishes an official hosted MCP endpoint at:

- [ai.todoist.net/mcp](https://ai.todoist.net/mcp)

This is useful as optional background context for human users or external tools, but it is not the
core runtime contract for Symphony. Symphony must own its own tracker tool and runtime behavior.

### `/sync` Caveats

Official `/sync` documentation exposes caveats that affect the migration:

- `/sync` is still a first-party-favored endpoint for efficient sync
- `/sync` uses legacy naming conventions such as `items` and `notes`
- initial full sync may be stale for big accounts
- docs explicitly recommend an immediate incremental sync when `full_sync_date_utc` is lagging
- `workspace_users` are not returned in full sync and only appear in incremental sync

These caveats are the main reason the migration will not use `/sync` as the primary operational
surface.

## Todoist-Native Domain Model

The migration target is not "Linear fields backed by Todoist storage". It is a Todoist-first model
that preserves Symphony's orchestration goals while using Todoist's native concepts correctly.

### Primary Work Item: Top-Level Task

The primary Symphony unit of work is a Todoist task with `parent_id = null`.

Use top-level tasks for:

- independently triaged work items
- tasks that should move through workflow sections
- tasks that should own a PR, workpad, and review/merge lifecycle

Dispatch rule for v1:

- the orchestrator auto-dispatches top-level tasks by default
- subtasks remain visible to the tool and UI, but are not auto-dispatched unless a later explicit
  configuration adds subtask execution

### Workflow Stages: Sections

Sections are the native Todoist mechanism for organizing project phases, and Todoist's board view
renders sections as columns. That maps well to Symphony workflow states.

Use sections for:

- `Todo`
- `In Progress`
- `Human Review`
- `Rework`
- `Merging`
- optional open terminal lanes such as `Cancelled` or `Duplicate`

Do not use labels or comments as the primary workflow-state representation.

Operator recommendation:

- shared Symphony Todoist projects should default to Todoist board layout so section-based
  workflow states are visible as columns

### Child Steps and Delegation: Subtasks

Todoist subtasks are native child work items, not a workaround.

Use subtasks when:

- the current task should be decomposed into explicit child steps
- responsibility needs to be split inside one parent deliverable
- a follow-up is subordinate to the same parent outcome rather than a separate backlog item

Do not use subtasks by default for every follow-up. A new top-level task remains the default for
independent out-of-scope work.

Implementation rule:

- `create_task` with `parent_id` creates a subtask
- `move_task` with `parent_id` reparents an existing task
- subtasks are visible through tool actions, but the orchestrator does not auto-dispatch them in v1

### Metadata and Planning: Labels, Due Dates, Deadlines, and Reminders

Todoist labels are orthogonal categorization, not workflow state.

Use labels for:

- repo or product area tagging
- bug or feature classification
- risk or waiting markers
- human routing hints that are not equivalent to ownership

Todoist due dates, deadlines, and reminders are also first-class native features.

Use them for:

- scheduling expectations
- reminder notifications
- human planning metadata
- escalation or SLA-style context

Do not overload sections with these meanings.

### Ownership and Collaboration: Assignee and Collaborators

Assignment in Todoist is meaningful in shared projects. Todoist supports one assignee per task and
encourages clear ownership.

Use:

- collaborator membership to validate assignable users
- `assignee_id` for the single responsible individual
- comments and subtasks for broader collaboration or delegated child work

### Discussion Surfaces: Task Comments vs Project Comments

Todoist has both task comments and project comments. They are not interchangeable.

Use task comments for:

- the Symphony workpad
- task-specific human or agent handoff
- PR links, review notes, and execution history for a single work item

Use project comments only for:

- project-level operator notes
- cross-task administrative discussion

Hard rule:

- Symphony's persistent `## Codex Workpad` must always be a task comment addressed by `task_id`,
  never a project comment addressed by `project_id`
- until Symphony grows a first-class Todoist-native external-link field, the surviving PR URL in
  that task-scoped workpad comment is the canonical tracker-side PR reference

### Optional Diagnostics: Activity Log

Todoist activity is useful for audit and debugging, but it is not the source of truth for the
orchestrator. It remains optional and plan-gated.

## Current Trunk Baseline

The current Rust runtime already provides the service shape that `rust-todoist/` must preserve:

- CLI startup and acknowledgement flow in [`rust/src/cli.rs`](../rust/src/cli.rs)
- workflow loading and hot reload in [`rust/src/workflow.rs`](../rust/src/workflow.rs)
- typed config parsing in [`rust/src/config.rs`](../rust/src/config.rs)
- issue normalization in [`rust/src/issue.rs`](../rust/src/issue.rs)
- tracker boundary in [`rust/src/tracker/mod.rs`](../rust/src/tracker/mod.rs)
- deterministic fixture-backed tracker support in [`rust/src/tracker/memory.rs`](../rust/src/tracker/memory.rs)
- dynamic-tool execution in [`rust/src/dynamic_tool.rs`](../rust/src/dynamic_tool.rs)
- Codex app-server transport in [`rust/src/codex/mod.rs`](../rust/src/codex/mod.rs)
- shared presentation and terminal dashboard logic in [`rust/src/observability.rs`](../rust/src/observability.rs)
- orchestrator lifecycle in [`rust/src/orchestrator.rs`](../rust/src/orchestrator.rs)
- workspace management in [`rust/src/workspace.rs`](../rust/src/workspace.rs)
- HTTP observability in [`rust/src/http.rs`](../rust/src/http.rs)
- live web stream endpoint `/api/v1/stream`
- host-side `github_api` dynamic tool alongside the tracker tool

The migration target is therefore not "invent a new service." It is "preserve this runtime shape,
replace the Linear coupling, and keep the proven operational behavior."

## Proposed Runtime Layout

`rust-todoist/` should start as a close sibling of `rust/`:

```text
rust-todoist/
  Cargo.toml
  README.md
  WORKFLOW.md
  WORKFLOW.smoke.minimal.md
  WORKFLOW.smoke.full.md
  SMOKE_TESTS.md
  scripts/
    workspace_before_remove.sh
    github_publish_preflight.sh
  src/
    main.rs
    lib.rs
    cli.rs
    workflow.rs
    logging.rs
    observability.rs
    config.rs
    issue.rs
    prompt.rs
    dynamic_tool.rs
    orchestrator.rs
    http.rs
    workspace.rs
    codex/
      mod.rs
    tracker/
      mod.rs
      todoist.rs
      memory.rs
```

Phase 1 should copy and adapt. Do not prematurely abstract shared code into a common crate.

## Reuse Plan

| Current `rust/` surface | `rust-todoist/` action |
| --- | --- |
| [`rust/src/cli.rs`](../rust/src/cli.rs) | copy with minimal naming changes |
| [`rust/src/workflow.rs`](../rust/src/workflow.rs) | copy and preserve content-hash reload and last-known-good fallback |
| [`rust/src/logging.rs`](../rust/src/logging.rs) | copy unchanged |
| [`rust/src/workspace.rs`](../rust/src/workspace.rs) | copy unchanged; preserve path safety and hooks |
| [`rust/src/codex/mod.rs`](../rust/src/codex/mod.rs) | copy; only update tracker tool names and tests |
| [`rust/src/observability.rs`](../rust/src/observability.rs) | copy; update tracker wording, links, and identifiers |
| [`rust/src/http.rs`](../rust/src/http.rs) | copy; preserve routes and SSE behavior |
| [`rust/src/config.rs`](../rust/src/config.rs) | copy, then adapt tracker config from Linear to Todoist |
| [`rust/src/issue.rs`](../rust/src/issue.rs) | copy, then make unsupported fields explicit compatibility shims |
| [`rust/src/tracker/linear.rs`](../rust/src/tracker/linear.rs) | use only as HTTP-client and pagination pattern reference |
| [`rust/src/tracker/memory.rs`](../rust/src/tracker/memory.rs) | copy, then expand fixture model and structured tool support |
| [`rust/src/dynamic_tool.rs`](../rust/src/dynamic_tool.rs) | preserve `github_api`; replace `linear_graphql` with structured `todoist` actions |
| [`rust/src/orchestrator.rs`](../rust/src/orchestrator.rs) | copy, then change startup cleanup, reconciliation, blocker behavior, and tracker refresh semantics |
| [`rust/WORKFLOW.md`](../rust/WORKFLOW.md) | rewrite for Todoist while preserving unattended discipline |
| [`rust/SMOKE_TESTS.md`](../rust/SMOKE_TESTS.md) and smoke workflows | create Todoist equivalents in `rust-todoist/` |

## Public Interface Changes

The following runtime interface changes are intentional.

### Workflow Front Matter

`rust-todoist/WORKFLOW.md` must support:

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
workspace:
  root: ~/code/symphony-workspaces
codex:
  command: codex app-server
```

### Tracker Kinds

Supported tracker kinds in `rust-todoist/`:

- `todoist`
- `memory`

`linear` should not be a supported live kind inside `rust-todoist/`.

### Defaults and Compatibility

- `tracker.base_url` defaults to `https://api.todoist.com/api/v1`
- `tracker.api_key` falls back to `TODOIST_API_TOKEN`
- `tracker.assignee` may also fall back to `TODOIST_ASSIGNEE`
- `active_states` and `terminal_states` continue to accept YAML lists or comma-separated strings
- `observability.terminal_enabled`, `observability.refresh_ms`, and
  `observability.render_interval_ms` preserve the current Rust defaults and meaning
- `workspace.root`, hook semantics, approval behavior, and sandbox passthrough preserve the
  current Rust behavior

### Validation Errors

Add Todoist-specific config and runtime errors:

- `missing_todoist_api_token`
- `missing_todoist_project_id`
- `todoist_project_not_found`
- `todoist_missing_current_user`
- `todoist_assignee_not_resolvable`
- `todoist_missing_required_section`
- `todoist_comments_unavailable`
- `todoist_comment_too_large`
- `todoist_api_request`
- `todoist_api_status`
- `todoist_rate_limited`
- `todoist_unknown_payload`

Retire Linear-only operator errors from the forward path in `rust-todoist/`.

## Tracker Boundary Refactor

The current Rust tracker trait includes `raw_graphql`, which is Linear-specific.

That method must not survive in the Todoist forward path.

### Required Change

Replace the raw GraphQL escape hatch with a typed Todoist tracker/tool boundary.

Two acceptable implementations:

1. extend `TrackerClient` with typed tool-facing methods
2. split the tracker boundary into:
   - orchestrator-facing tracker methods
   - structured tool-facing tracker methods

The implementation choice is flexible, but the forward path must become typed and action-based.

### Minimum Forward Interface

The live Todoist implementation must expose enough functionality to serve both orchestrator needs
and dynamic-tool needs:

- fetch candidate issues
- fetch issues by states
- fetch issue states by IDs
- list projects
- get current user
- list collaborators
- list sections
- list labels
- get task
- list comments
- create comment
- update comment
- move task
- update task
- close task
- reopen task
- create task
- list reminders
- create reminder
- update reminder
- delete reminder

`create_task` is required because the current default workflow expects follow-up work to be filed
instead of silently dropped.

## Data Model

### Tracker-Neutral Issue Shape

`rust-todoist/src/issue.rs` can remain structurally close to the current Rust model, but Todoist
semantics must be explicit.

Required fields:

- `id: String`
- `identifier: String`
- `title: String`
- `description: Option<String>`
- `priority: Option<i64>`
- `state: String`
- `url: Option<String>`
- `labels: Vec<String>`
- `created_at: Option<DateTime<Utc>>`
- `updated_at: Option<DateTime<Utc>>`
- `assignee_id: Option<String>`
- `assigned_to_worker: bool`

Todoist-native enrichment fields:

- `project_id: String`
- `section_id: Option<String>`
- `parent_id: Option<String>`
- `is_subtask: bool`
- `due: Option<TodoistDue>`
- `deadline: Option<TodoistDeadline>`

Compatibility-only fields:

- `branch_name: Option<String>` always `None` for Todoist
- `blocked_by: Vec<BlockerRef>` always empty for Todoist v1

### Synthetic Identifier

Generate:

- `TD-<task_id>`

This synthetic identifier must be used for:

- workspace directory names after sanitization
- logs
- `/api/v1/{identifier}`
- detail pages and status views

### Todoist State Model

Todoist state is section-based for open work and task-close based for completion.

Canonical mapping:

- `Todo`, `In Progress`, `Human Review`, `Merging`, `Rework`
  - represented by Todoist section names
- `Done`
  - represented by closing the task
- `Cancelled`, `Duplicate`
  - represented by explicit open-task sections if the workflow needs them

In v1, do not add a second terminal representation such as label-based terminal routing. Keep the
state model section-based plus close/reopen.

### Priority Normalization

Normalize Todoist priority values into Symphony's current sort shape:

- Todoist `4` -> Symphony `1`
- Todoist `3` -> Symphony `2`
- Todoist `2` -> Symphony `3`
- Todoist `1` -> Symphony `4`
- unset -> `None`

This preserves current Rust ordering where smaller numbers sort earlier.

### Assignment Normalization

Todoist assignment semantics differ across surfaces:

- resource task update docs expose `assignee_id`
- `/sync` task objects expose `responsible_uid`
- `/sync` also exposes `assigned_by_uid`
- assignment is meaningful only for shared projects

The runtime must normalize this rather than leaking upstream field-name differences.

Implementation rule:

- use resource endpoints for active runtime operations and dynamic tool writes
- normalize assignment internally to `assignee_id: Option<String>`
- treat missing or invalid assignment on non-shared projects as "unassigned"

## Todoist API Strategy

Implementation must use the official Todoist API v1 with:

- base URL `https://api.todoist.com/api/v1`
- auth header `Authorization: Bearer <token>`

### Startup Bootstrap

On startup, the Todoist tracker must perform the following bootstrap sequence:

1. validate token presence
2. fetch project metadata for `tracker.project_id`
3. fetch project sections
4. resolve all configured `active_states` to section IDs
5. resolve any configured `terminal_states` to section IDs if those states are used as open
   sections
6. fetch current user if `tracker.assignee == "me"`
7. fetch project collaborators if an explicit assignee is configured or shared-project assignment
   validation is required
8. fetch `user_plan_limits` from `/sync` and validate that comments are available
9. record whether reminders, deadlines, and activity are available so the tool surface can
   advertise only supported features

Bootstrapping must fail fast if:

- the project does not exist
- required active sections are missing
- `assignee: me` cannot resolve a current user
- an explicit assignee cannot be resolved for a shared project workflow
- comments are unavailable and the workflow requires workpad comments

### Why `/sync` Is Used Only at Bootstrap

`/sync` earns its complexity only for capability and account-level metadata that is otherwise
awkward to infer:

- `user_plan_limits`
- optional future incremental capability checks

It does not earn primary-loop ownership because:

- it retains legacy naming
- it requires freshness caveats such as `full_sync_date_utc`
- it is broader than Symphony needs for the core orchestration loop

### Candidate Polling

Candidate polling will use paginated resource endpoints, not `/sync`.

Algorithm:

1. list sections for the configured project and build a `section_id -> section_name` map
2. list tasks for the configured project with `limit=200`
3. continue until `next_cursor` is null
4. filter tasks client-side to sections whose names are in `active_states`
5. apply assignee filtering client-side
6. normalize into the issue model
7. sort by normalized priority, then creation time, then identifier

Why this is acceptable:

- Todoist officially caps active tasks per project at `300`
- Todoist officially caps sections per project at `20`
- in Symphony's intended deployment model, a project scan is bounded and operationally simple

### State Refresh by IDs

Current Rust refresh logic relies on Linear batch reads by ID. Todoist does not expose an obvious
equivalent batch active-task read by ID in the resource surface.

Required refresh algorithm:

1. dedupe requested task IDs
2. fetch each task by ID using the task-get endpoint
3. run requests with bounded concurrency
4. classify results by task ID

Result handling:

- `200` -> active task snapshot
- `404` or Todoist `NOT_FOUND` payload -> treat as missing
- other transport or auth failures -> refresh failure

Missing-ID rule:

- any requested task ID absent from refresh results must be treated as no longer active

This rule is mandatory because completed Todoist tasks disappear from active task reads.

### Startup Cleanup

Current Rust startup cleanup queries terminal Linear states and removes matching workspaces.

That must not be ported directly.

Replace it with:

1. enumerate existing workspace directories under `workspace.root`
2. fetch all open tasks in the configured Todoist project
3. derive the set of live synthetic identifiers `TD-<task_id>`
4. remove workspaces whose identifiers are not in the live set
5. optionally remove workspaces for open tasks sitting in configured open terminal sections

Do not require completed-task archive access for startup cleanup correctness.

### Comments and Workpad

Todoist comments are required for the current Symphony workpad model.

Official constraints:

- comments are plan-gated
- comment content is limited to `15,000` characters
- comment listing requires exactly one of `task_id` or `project_id`

Task-comment rule:

- the Symphony workpad always uses `task_id`
- project comments are reserved for project-level operator notes and are not valid workpad targets

Workpad algorithm:

1. list comments for `task_id`
2. paginate until the workpad marker is found or no comments remain
3. reuse the newest matching workpad comment when found
4. create a new workpad comment when missing
5. update that same comment for planning, progress, validation, and handoff

Required markers:

- visible marker: `## Codex Workpad`
- hidden marker: `<!-- symphony:workpad -->`

### Workpad Compaction

Workpad content must be bounded.

Required compaction behavior:

- keep current plan, status, blockers, validation, and handoff sections
- compress older history into a short summary
- never allow unbounded append-only comment growth
- validate serialized comment size before write
- fail with a deterministic `todoist_comment_too_large` error only if compaction still cannot fit

### Task State Updates

Map workflow actions like this:

- move within active workflow
  - update task `section_id`
- close for `Done`
  - call close-task endpoint
- reopen from closed state
  - call reopen-task endpoint
  - then move to the requested section
- update task metadata
  - use task update endpoint for assignee, labels, priority, content, and description

### Follow-Up Task Creation

The current workflow expects out-of-scope follow-up work to be filed. Todoist has no Linear-style
issue relation graph, but Symphony should still preserve the ability to record follow-up work.

Required behavior:

- `todoist.create_task` is a first-class tool action
- created follow-up work goes into the same Todoist project unless the workflow explicitly chooses a
  different project
- independent follow-up work defaults to a new top-level task in the configured `Todo` section when
  available
- follow-up work may be created as a subtask only when it is clearly a child step of the current
  task and should remain subordinate to it
- the follow-up description must include a short back-reference to the originating `TD-<task_id>`
- no blocker or related graph is required in v1

### Labels, Due Dates, Deadlines, and Reminders

Todoist-native metadata must remain usable, not merely preserved.

Rules:

- labels are available to the tool and preserved in the normalized model
- labels must not be used as the primary workflow-state mechanism
- due dates and deadlines should be preserved on reads and writable on task mutations
- reminders should be exposed only when the account plan supports them
- reminder failures or lack of reminder support must not block core orchestration

### Projects and Collaborators

The Todoist runtime is project-scoped for orchestration, but the tool surface should still expose
enough project context to operate natively.

Rules:

- project metadata is part of startup bootstrap and observability linking
- collaborator listing is required for shared-project assignment validation
- project comments remain available to operators, but not as the workpad substrate

### Activities and Completed Tasks

Todoist exposes:

- paginated activity logs
- completed-task endpoints

Both are useful, but neither is required for v1 correctness.

Decision:

- do not make activity logs a correctness dependency
- do not make completed-task archive access a correctness dependency
- use active-task reads plus missing-ID reconciliation for correctness
- treat activities and completed-task archive reads as optional future diagnostics

### Webhooks

Official webhook facts that matter:

- only HTTPS callback URLs are allowed
- requests can be delayed, duplicated, reordered, or lost
- verification uses `X-Todoist-Hmac-SHA256`
- dedupe uses `X-Todoist-Delivery-ID`
- failed deliveries are retried

Decision:

- do not use webhooks in v1
- document them as future acceleration only

## Dynamic Tool Contract

The Todoist runtime keeps the host-side `github_api` tool and replaces the tracker-specific
`linear_graphql` tool with a structured `todoist` tool.

### Tool Name

- `todoist`

### Supported Actions

Required actions:

- `list_projects`
- `get_project`
- `get_current_user`
- `list_collaborators`
- `list_tasks`
- `get_task`
- `list_sections`
- `get_section`
- `list_labels`
- `list_comments`
- `create_comment`
- `update_comment`
- `move_task`
- `update_task`
- `close_task`
- `reopen_task`
- `create_task`
- `list_reminders`
- `create_reminder`
- `update_reminder`
- `delete_reminder`

### Design Rules

- `action` is required
- each action has strict required keys
- unknown keys are rejected
- no raw HTTP passthrough
- no raw `/sync` command execution
- tool output keeps the current Rust content-item envelope shape

### Action Semantics

`list_tasks`

- defaults to the configured project unless explicitly overridden
- supports pagination parameters where useful
- returns normalized Todoist task data
- may include subtasks in tool reads; orchestrator candidate selection remains top-level only

`get_task`

- returns a single active-task snapshot
- surfaces not-found clearly

`list_sections`

- defaults to the configured project

`list_labels`

- returns personal labels and preserves label names as Todoist-native metadata

`list_collaborators`

- defaults to the configured project
- is required for shared-project assignment inspection

`list_comments`

- requires exactly one of `task_id` or `project_id`
- task comments are the required workpad surface
- project comments are operator-only and must not be used for the persistent workpad

`create_comment`

- requires `content`
- requires exactly one of `task_id` or `project_id`
- rejects content over `15,000` characters
- workpad creation must target `task_id`

`update_comment`

- requires `comment_id`
- enforces the same content-size bound

`move_task`

- supports moving a task to a different `section_id`
- supports changing `parent_id` when explicitly reparenting into or out of a subtask relationship

`update_task`

- supports content, description, section, priority, assignee, labels, due data, and deadlines where
  the API or account permits

`close_task`

- closes the task and returns success even when the response body is null

`reopen_task`

- reopens the task

`create_task`

- supports at minimum:
  - `content`
  - `description`
  - `project_id`
  - `section_id`
  - `parent_id`
  - `priority`
  - `labels`
  - `assignee_id`
  - `due`
  - `deadline`

`list_reminders`

- optionally filters by `task_id`
- is advertised only when reminder support is available for the configured account

`create_reminder`, `update_reminder`, `delete_reminder`

- are available only when reminder support is available
- must not be required for core orchestration correctness

### Error Mapping

The tool must return structured JSON errors with:

- a stable machine-readable code
- a human-readable message
- preserved HTTP status when present
- preserved `retry_after` when present
- payload excerpts where useful for recovery

## `memory` Tracker Requirements

The `memory` tracker cannot remain issue-array only if the forward workflow depends on the
structured `todoist` tool.

### Required Change

Expand the `memory` fixture model beyond:

- `issues`

to support at minimum:

- `issues`
- `sections`
- `comments`
- `current_user`
- `collaborators`
- `user_plan_limits`
- `projects`
- `labels`
- `reminders`

### Required Behavior

The `memory` tracker must support the same structured tool actions as the live Todoist tracker,
subject to deterministic fixture-backed rules.

At minimum:

- list/get tasks
- list/get sections
- list projects
- list collaborators
- list labels
- list/create/update comments
- move task
- update task
- close task
- reopen task
- create task
- get current user
- list/create/update/delete reminders where enabled

This is required so:

- workflow rendering remains realistic
- system tests can execute tool-driven workpad updates
- CI does not become live-Todoist-dependent

## Feature Parity Matrix

| Capability | `rust/` baseline | `rust-todoist/` target |
| --- | --- | --- |
| CLI and acknowledgement | explicit acknowledgement flag, workflow path, optional port | same behavior |
| Workflow loading | front matter parsing, content-hash reload, last-known-good fallback | same behavior |
| Config parsing | typed config, env resolution, path normalization | same behavior plus Todoist fields |
| Tracker kinds | `linear` plus `memory` | `todoist` plus `memory` |
| Candidate polling | active Linear issues by project/state | active Todoist tasks by project/section |
| Manual refresh | coalesced refresh endpoint | same behavior |
| Retry queue | exponential backoff with stale-retry suppression | same behavior |
| Workspace lifecycle | create/reuse/hooks/cleanup/path safety | same behavior |
| Dynamic tool registration | `linear_graphql` plus `github_api` | `todoist` plus `github_api` |
| App-server transport | tool calls, approval handling, requestUserInput auto-answer | same behavior |
| Thread sandbox passthrough | explicit thread and turn sandbox values | same behavior |
| Issue normalization | Linear-shaped issue model | tracker-neutral issue model with `TD-<task_id>` |
| Assignment routing | viewer or explicit Linear assignee | current user or explicit Todoist collaborator ID |
| Priority sorting | Linear priority ordering | normalized Todoist priority ordering |
| Startup cleanup | terminal-state query | open-project enumeration plus missing-ID logic |
| Running reconciliation | refresh returned rows only | refresh-by-ID with missing-ID handling |
| Blocker gating | Todo blocked-by-non-terminal rule | removed by default |
| Workpad persistence | single Linear comment reused | single Todoist comment reused |
| Terminal dashboard | ANSI dashboard | same behavior |
| Web dashboard | JSON plus SSE | same endpoints with Todoist identifiers and links |
| Logging | rotating file logs | same behavior |
| Smoke workflows | Linear-specific | Todoist-specific equivalents |
| Deterministic tracker-tool coverage | `memory` can skip `raw_graphql` | `memory` must support structured Todoist actions |

## Orchestrator Changes

### Dispatch Eligibility

Preserve:

- active-state filtering
- assignment filtering
- bounded concurrency
- per-state concurrency limits
- priority sorting

Change:

- remove default blocker gating for Todoist
- do not require meaningful `blocked_by` data

### Running Reconciliation

Current Rust reconciliation iterates over returned refresh rows only. That is not sufficient for
Todoist.

Required logic:

1. request refresh for all running IDs
2. build a returned-ID set
3. for any requested ID missing from the returned set:
   - terminate or avoid continuing the run
   - release the claim
   - clean up the workspace when the task is no longer live

### Retry Handling

Preserve:

- exponential retry backoff
- stale-retry suppression
- explicit retry metadata

Change:

- if a retry target is missing from refresh results, treat it as non-active rather than waiting for
  an explicit terminal row

### Continuation Behavior

Preserve:

- reuse of workspace and thread state after clean worker exit
- continuation turns while the task remains active
- explicit max-turn handling

Only tracker wording, active-state semantics, and refresh behavior should change.

## Workflow and Prompt Migration

The workflow migration is not a search-and-replace exercise.

### Required Prompt Changes

Replace:

- "Linear issue" or "Linear ticket" wording
- Linear-specific prerequisite sections
- `linear_graphql` references
- guidance that depends on blocker graphs, issue relations, or Linear state lookups

Preserve:

- unattended posture
- no-human-follow-up instruction
- pull-before-edit discipline
- PR feedback sweep expectations
- validation-first handoff bar
- single persistent workpad comment discipline
- continuation guidance
- blocked-access escape hatch
- `github_api` fallback guidance

### Follow-Up Work Policy

Because the current workflow expects out-of-scope follow-up work to be filed, `rust-todoist`
should keep that behavior through `todoist.create_task`.

What changes:

- no `related` graph
- no `blockedBy` graph
- follow-up linkage becomes textual and workflow-local

### Workpad Format Rule

Because Todoist comments are capped at `15,000` characters, the workflow renderer must define a
bounded workpad shape. The implementation should:

- keep current plan, progress, blockers, validation, and handoff state
- compress older narrative detail into a short history summary
- refuse unbounded append-only growth

## Observability and Operator Surfaces

Preserve the existing surfaces:

- terminal dashboard
- `/`
- `/api/v1/state`
- `/api/v1/stream`
- `/api/v1/{identifier}`
- `/api/v1/refresh`

Required changes:

- identifiers use `TD-<task_id>`
- payloads include Todoist task and project links where available
- operator copy stops referring to `project_slug`, GraphQL, or Linear states
- tracker-specific error labels are Todoist-specific
- SSE and JSON payloads remain shape-compatible with current dashboard code where practical

## Testing and Validation Matrix

### Config

- `tracker.kind: todoist` parses
- `tracker.kind: memory` still parses
- `tracker.project_id` is required
- `tracker.base_url` default and override work
- `TODOIST_API_TOKEN` fallback resolves
- `TODOIST_ASSIGNEE` fallback works if retained
- YAML-list and CSV state parsing both work
- explicit turn sandbox policy still passes through unchanged

### Todoist Client

- project lookup
- section pagination
- task pagination
- top-level task vs subtask normalization
- comment pagination
- current-user lookup
- collaborator lookup
- label listing
- plan-limit bootstrap via `/sync`
- assignee filtering
- per-ID refresh with bounded concurrency
- missing-ID refresh behavior
- comment-size enforcement
- task-comment vs project-comment targeting
- due, deadline, and reminder handling
- rate-limit and `retry_after` handling

### Dynamic Tool

- tool advertisement
- strict action validation
- transport error mapping
- structured JSON output
- project, collaborator, and label actions
- `create_task` behavior
- `create_task` with `parent_id` behavior
- `move_task` reparent behavior
- reminder action gating
- preserved `github_api` behavior alongside the tracker tool

### `memory` Tracker

- expanded fixture parsing
- structured Todoist action support
- deterministic comment mutation behavior
- deterministic task-comment vs project-comment targeting
- deterministic section move behavior
- deterministic parent/subtask behavior
- deterministic close/reopen behavior
- deterministic current-user and plan-limit behavior

### Orchestrator

- candidate dispatch from active sections
- candidate dispatch excludes subtasks by default
- no blocker gating by default
- startup cleanup using open project items
- running reconciliation for:
  - completed tasks
  - deleted tasks
  - tasks moved out of active sections
  - tasks missing from refresh results
- retry handling for missing items
- stale-retry suppression
- poll coalescing

### Workspace and Codex

- canonicalized path validation
- symlink-escape rejection
- deterministic synthetic identifier naming
- hook timeout and failure handling
- requestUserInput auto-answer behavior
- explicit thread and turn sandbox passthrough
- dynamic tool calls within the app-server session

### Workflow and Prompt

- Todoist workflow sample renders correctly
- continuation copy is tracker-neutral or Todoist-first
- workpad marker reuse works with Todoist task comments
- default workflow explains when to create a top-level task vs a subtask
- workpad compaction prevents oversized comments
- default workflow no longer contains unsupported tracker steps

### Observability

- state snapshot endpoint
- SSE stream endpoint
- item detail endpoint by synthetic identifier
- refresh endpoint
- Todoist task and project links
- rendered metadata includes labels and due/deadline context where available
- Todoist-specific blocking reasons

### Smoke and CI

- `rust-todoist` CI is green
- deterministic `memory` tests are green
- Todoist live-smoke workflow exists
- legacy Linear smoke docs remain clearly labeled if retained

## Implementation Phases

### Phase 1. Freeze Baseline and Scaffold

1. freeze the current `rust/` runtime as the parity reference
2. create `rust-todoist/` by copying the shared runtime skeleton
3. keep `rust/` untouched except for doc references

### Phase 2. Config and Issue Typing

1. adapt tracker config from Linear to Todoist
2. implement `project_id`, `base_url`, and Todoist env fallbacks
3. make issue typing tracker-neutral
4. introduce synthetic identifiers

### Phase 3. Todoist Tracker Client

1. add `tracker/todoist.rs`
2. implement project, section, task, comment, collaborator, label, reminder, and user reads
3. implement `/sync` bootstrap for `user_plan_limits`
4. implement update, move, close, reopen, reminder, and create-task mutations
5. implement bounded per-ID refresh

### Phase 4. Dynamic Tool

1. replace `linear_graphql` with structured `todoist`
2. preserve `github_api`
3. add strict action validation and Todoist error mapping

### Phase 5. Orchestrator Behavior

1. remove blocker gating
2. add open-project startup cleanup
3. add missing-ID reconciliation
4. preserve poll coalescing, retry suppression, and continuation behavior

### Phase 6. `memory` Tracker Expansion

1. expand fixture shape
2. implement structured tool actions
3. update deterministic tests and workflow fixtures

### Phase 7. Workflow, Docs, and Smoke

1. rewrite `rust-todoist/WORKFLOW.md`
2. rewrite `rust-todoist/README.md`
3. add Todoist smoke workflows and smoke docs
4. update root docs to point the forward path at Todoist

### Phase 8. Validation and Cutover Readiness

1. run unit and integration tests
2. run memory-backed end-to-end system tests
3. run Todoist live smoke against a dedicated sandbox project
4. confirm feature parity against the matrix in this document

## Release Checklist

- `rust-todoist/` starts with `tracker.kind: todoist`
- `rust-todoist/` also supports `tracker.kind: memory`
- `rust/` remains available as the legacy Linear reference during cutover
- the `todoist` tool fully replaces `linear_graphql` for the forward workflow
- the host-side `github_api` tool remains available
- startup comment-capability validation works
- workpad comment reuse works
- workpad compaction handles large tasks safely
- missing-ID reconciliation is implemented
- open-project cleanup is implemented
- no blocker gating remains in the Todoist path
- status APIs expose Todoist identifiers and links
- Todoist smoke docs and workflows exist
- CI is green

## Post-Parity Follow-Up

Only after parity is proven should the repo decide whether to:

- keep both runtimes long-term
- retire `rust/`
- rename `rust-todoist/` back to `rust/`
- extract a shared crate for duplicated runtime code

That decision is intentionally out of scope for this spec.
