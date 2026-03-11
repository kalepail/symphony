# Linear to Todoist Migration PRD

Status: proposed  
Last updated: March 10, 2026  
Baseline reviewed against trunk: `ad9849a`

## Summary

Symphony should migrate from Linear to Todoist by building a sibling runtime at `rust-todoist/`
adjacent to the existing [`rust/`](../rust/README.md) runtime, not by rewriting or replacing
`rust/` in place.

That posture matches the current repo reality:

- [`rust/`](../rust/README.md) is now a real Linear-backed runtime with a working service
  skeleton, a deterministic `memory` tracker, smoke workflows, and CI.
- [`elixir/`](../elixir/README.md) remains a useful behavioral reference, but it is no longer the
  only implementation baseline.
- Todoist's tracker model differs enough from Linear that a safe migration needs controlled
  parallelism: preserve the current Rust runtime as reference while building and validating the
  Todoist runtime beside it.

This is still not a thin adapter swap. The new runtime must preserve the current Rust operating
quality bar feature-for-feature while replacing the tracker contract, dynamic tool, workflow copy,
smoke surfaces, and operator guidance with Todoist-native behavior.

## Problem

Symphony's current runtime and public contract are still Linear-first:

- root docs describe Symphony as polling a Linear board
- [`rust/WORKFLOW.md`](../rust/WORKFLOW.md) and the smoke workflows assume Linear states, comments,
  and `linear_graphql`
- [`rust/src/config.rs`](../rust/src/config.rs), [`rust/src/tracker/mod.rs`](../rust/src/tracker/mod.rs),
  [`rust/src/tracker/linear.rs`](../rust/src/tracker/linear.rs), and
  [`rust/src/dynamic_tool.rs`](../rust/src/dynamic_tool.rs) only support a live Linear tracker
- [`rust/src/orchestrator.rs`](../rust/src/orchestrator.rs) still depends on Linear-specific
  behaviors such as blocker metadata and terminal-state cleanup queries

At the same time, trunk already contains significant Rust work that should not be thrown away:

- workflow loading and last-known-good reload
- workspace lifecycle and path confinement
- Codex app-server integration
- request-user-input auto-answer handling
- explicit sandbox-policy passthrough
- terminal and web observability surfaces
- bounded concurrency, retry/backoff, continuation turns, and poll coalescing
- HTTP observability, logging, smoke docs, and Rust CI

Replacing `rust/` wholesale would turn the migration into an avoidable high-risk rewrite. The
better approach is to keep `rust/` intact as the known reference while building `rust-todoist/`
with feature-for-feature parity and Todoist-native semantics.

## Product Goal

Ship a Todoist-backed Symphony runtime at `rust-todoist/` that matches the current Rust runtime's
operational capabilities while making Todoist the primary documented forward path for live tracker
execution.

## Target Audience

- platform engineers implementing and maintaining Symphony
- operators running Symphony unattended against a tracker project
- workflow authors who need a stable tracker-side workpad and mutation contract
- reviewers who need a clear, testable migration plan from Linear to Todoist

## User Jobs

### Operator

Needs to configure Symphony against a Todoist project and have it:

- discover eligible work
- honor assignment rules
- create or reuse isolated workspaces
- continue unfinished work across turns
- stop or clean up correctly when tasks leave the active workflow

### Runtime Maintainer

Needs a migration plan that:

- preserves the existing Rust runtime as a reference implementation
- reuses the proven Rust service skeleton instead of discarding it
- turns Todoist differences into explicit design decisions instead of hidden edge cases

### Workflow Author

Needs a default workflow and tool surface that:

- remain unattended-first
- preserve the single tracker-side workpad comment model
- replace Linear-only tool usage with bounded, structured Todoist actions

## Goals

### Primary Goals

- build a sibling runtime at `rust-todoist/` rather than converting `rust/` in place
- preserve `rust/` as the Linear-plus-memory reference during migration
- preserve feature parity with the current Rust runtime for:
  - CLI startup and acknowledgement flow
  - workflow loading and hot reload
  - typed config parsing and env resolution
  - observability config, terminal dashboard, and SSE-backed web dashboard
  - workspace lifecycle and path safety
  - Codex app-server execution and continuation behavior
  - bounded concurrency, retry/backoff, stale-retry suppression, and poll coalescing
  - HTTP observability and logging
  - deterministic `memory` tracker coverage
- make Todoist the native live tracker model in `rust-todoist/`
- replace `linear_graphql` with a structured `todoist` dynamic tool
- rewrite the runtime-facing workflow, docs, and smoke surfaces so the forward path is Todoist

### Secondary Goals

- keep the Todoist migration isolated enough that regressions can be measured directly against
  `rust/`
- leave open the option to extract shared code later, after parity is proven
- document Todoist's hosted MCP as optional context, not a runtime dependency

## Non-Goals

- deleting or replacing the current [`rust/`](../rust/README.md) runtime in this tranche
- porting Todoist into the Elixir runtime
- preserving full Linear parity for blocker graphs, short issue keys, or branch metadata
- building a generic multi-tracker abstraction layer before the Todoist runtime exists
- making Todoist webhooks the source of truth in v1
- migrating historical Linear issues into Todoist

## Product Decisions

The following decisions are locked for this migration:

- The implementation target is a sibling runtime, `rust-todoist/`, built adjacent to `rust/`.
- The existing `rust/` runtime remains the known-good Linear reference during development and
  validation.
- The existing `memory` tracker behavior must be preserved inside `rust-todoist/` for
  deterministic tests.
- Todoist is modeled natively:
  - `tracker.kind: todoist`
  - `tracker.base_url` defaulting to `https://api.todoist.com/api/v1`
  - `tracker.project_id`
  - `TODOIST_API_TOKEN`
  - `tracker.assignee` supporting `me`, explicit Todoist user IDs, or unset
- Todoist section names represent active workflow states.
- Final `Done` is represented by closing the task.
- `Cancelled` and `Duplicate` remain explicit open-task workflow buckets if the workflow wants
  them; they are not close reasons.
- Symphony keeps the single persistent tracker-side workpad comment model.
- The default workpad marker remains `## Codex Workpad`.
- Symphony owns tracker mutations through a native `todoist` dynamic tool.
- Todoist's hosted MCP at [ai.todoist.net/mcp](https://ai.todoist.net/mcp) is optional background
  context only.
- Phase 1 does not extract a shared library from `rust/` and `rust-todoist/`; reuse may begin as
  targeted copy-and-adapt.

## Functional Requirements

### 1. Runtime Topology

- The repo must gain a new top-level `rust-todoist/` runtime.
- `rust-todoist/` must preserve the proven service shape from `rust/`:
  - CLI
  - workflow loader
  - config layer
  - observability presenter and terminal dashboard
  - tracker boundary
  - orchestrator
  - workspace manager
  - Codex app-server client
  - HTTP server
  - logging
- `rust/` must remain runnable and reviewable as the legacy Linear reference during migration.

### 2. Feature Parity

- `rust-todoist/` must match the current Rust runtime's operational behaviors before it is treated
  as the primary runtime.
- Parity is required across:
  - candidate polling and refresh
  - bounded dispatch and per-state concurrency
  - retry/backoff and continuation
  - startup cleanup and workspace cleanup
  - status API and dashboard detail
  - deterministic test support via `memory`
  - smoke-run documentation and CI coverage

### 3. Todoist Tracker Model

- candidate work must be fetched from a Todoist project by `project_id`
- configured active states must be interpreted as Todoist section names
- state transitions inside the active workflow must be represented by section moves
- final completion must be represented by task close
- rework after close must reopen the task and move it back to the requested section
- synthetic identifiers must use `TD-<task_id>`

### 4. Workpad and Comments

- each Todoist task must keep exactly one Symphony workpad comment
- the runtime must find and reuse that same comment across retries and continuation turns
- startup must block clearly when comments are unavailable for the configured Todoist account plan
- workpad updates must respect Todoist's documented comment-size limit of 15,000 characters

### 5. Assignment and Eligibility

- `tracker.assignee: me` must resolve through Todoist's current-user endpoint
- explicit assignee IDs must be validated against project collaborators when assignment routing is
  enabled
- the runtime must treat shared-project assignment semantics as a startup or dispatch concern, not
  a silent filtering failure
- blocker gating from Linear must not survive as default dispatch logic

### 6. Tooling

- `rust-todoist/` must preserve the host-side `github_api` tool behavior from the current Rust
  runtime.
- `rust-todoist/` must add a structured `todoist` tracker tool in place of the Linear-specific
  `linear_graphql` tool.
- the tool must cover the Todoist operations required by the unattended workflow:
  - get current user
  - list/get tasks
  - list/get sections
  - list/create/update comments
  - update task
  - close task
  - reopen task
- if the default workflow still expects filing follow-up tracker items, the tool must also provide
  `create_task`

### 7. Observability and Operator Experience

- the existing HTTP surfaces must remain present:
  - terminal dashboard
  - `/`
  - `/api/v1/state`
  - `/api/v1/stream`
  - `/api/v1/{identifier}`
  - `/api/v1/refresh`
- the current `observability` config surface must remain present for terminal/web rendering
  behavior
- operator-facing status must use synthetic Todoist identifiers and Todoist links
- blocking reasons must become Todoist-specific and actionable

### 8. Documentation and Smoke Coverage

- the forward-path docs must point users at `rust-todoist/`, not `rust/`, for live Todoist runs
- the repo must retain a clear legacy story for `rust/` while the migration is in progress
- `rust-todoist/` must gain its own workflow sample, smoke docs, and validation story
- the migration is not complete until Todoist live-smoke coverage exists

## Non-Functional Requirements

### Reliability

- no regression in stale-retry suppression, poll coalescing, continuation turns, or workspace
  safety
- paginated Todoist reads must not silently truncate tasks, comments, sections, or collaborators
- missing task IDs during refresh must be handled as a first-class reconciliation case

### Safety

- workspace path canonicalization and symlink-escape rejection must remain intact
- dynamic tool actions must be schema-validated before hitting Todoist
- the existing host-side GitHub publish path must remain available even as the tracker tool
  changes
- startup validation must fail fast for missing auth, missing project configuration, missing active
  sections, or unusable comment/assignee configuration

### Operability

- clear logs and blocking reasons must remain available without a debugger
- the migration docs must be detailed enough to serve as implementation and review checklists

## Dependencies and Preconditions

- a Todoist project with the expected section layout must exist
- if assignment routing is required, the project must be shared and the assignee must be a valid
  collaborator
- the configured Todoist plan must support comments for the workpad model to function
- a valid API token must be available through `TODOIST_API_TOKEN` or inline config

## Workstreams

### Workstream 1: Baseline Freeze and Sibling Runtime Bootstrap

Deliverables:

- `rust/` documented as the legacy Linear reference
- `rust-todoist/` scaffolded from the current Rust service shape
- initial parity checklist tied to the `rust/` baseline

Exit criteria:

- engineers can review `rust-todoist/` against `rust/` module-by-module

### Workstream 2: Todoist Tracker and Dynamic Tool

Deliverables:

- Todoist config contract
- Todoist client implementation
- structured `todoist` dynamic tool while preserving `github_api`
- preserved `memory` tracker inside `rust-todoist/`

Exit criteria:

- a `rust-todoist/` run can poll Todoist, resolve assignee context, and maintain a workpad comment

### Workstream 3: Orchestrator Semantic Parity

Deliverables:

- startup cleanup based on open project items
- running-item reconciliation for missing, completed, rerouted, or deactivated tasks
- retry behavior that respects Todoist completion semantics
- removal of default blocker gating

Exit criteria:

- the Todoist runtime stops and cleans up correctly across active, retrying, and completed work

### Workstream 4: Workflow, Prompt, and Operator Surface Migration

Deliverables:

- Todoist-first workflow sample
- Todoist-first prompt copy and continuation guidance
- tracker-neutral cleanup scripts
- Todoist-aware HTTP status copy

Exit criteria:

- the new runtime no longer depends on Linear wording or `linear_graphql`

### Workstream 5: Validation, Smoke, and Cutover Readiness

Deliverables:

- unit and integration coverage for Todoist behavior
- deterministic `memory` coverage preserved
- Todoist live-smoke docs and workflow files
- CI for `rust-todoist/`
- clear release gates for promoting Todoist to the primary documented runtime

Exit criteria:

- the Todoist runtime has feature-for-feature coverage against the current Rust runtime

## Risks and Mitigations

### Risk: The new runtime drops existing Rust guarantees

Impact:

- the Todoist runtime works superficially but regresses retries, continuation, cleanup, or safety

Mitigation:

- make feature parity with `rust/` a release gate and keep `rust/` intact as the comparison target

### Risk: Todoist comments are plan-gated and size-limited

Impact:

- the single-comment workpad model can fail even if task polling works

Mitigation:

- validate comment capability at startup and design bounded workpad updates under the 15,000
  character limit

### Risk: Assignment semantics differ from Linear

Impact:

- `tracker.assignee` silently filters out work or behaves differently on private vs shared projects

Mitigation:

- validate `me` and explicit assignee IDs against Todoist project/collaborator context

### Risk: Todoist completion behavior leaves stale workers or workspaces

Impact:

- completed tasks disappear from active reads, so stale work may continue

Mitigation:

- redesign refresh reconciliation and startup cleanup around open-project enumeration and
  missing-ID handling

### Risk: Repo guidance stays split between Linear and Todoist

Impact:

- engineers keep following Linear docs while Todoist code lands elsewhere

Mitigation:

- make docs, smoke coverage, and operator guidance part of the release gates, not follow-up work

## Release Gates

The migration is not complete until all of the following are true:

- `rust-todoist/` exists and is runnable end to end against Todoist
- `rust/` remains available as the legacy Linear reference during the cutover period
- `rust-todoist/` preserves deterministic `memory` tracker coverage
- `linear_graphql` is no longer required by the forward-path workflow
- the Todoist runtime maintains a single reusable workpad comment per task
- missing-ID reconciliation and startup cleanup are correct for Todoist completion semantics
- Todoist live-smoke coverage exists
- the feature-parity matrix in the technical spec is fully satisfied
- the repo's forward-path docs route operators to Todoist rather than Linear

## Success Criteria

### Product Success

- an operator can configure Symphony against Todoist without touching Linear config or tools
- a Todoist task can move through the unattended workflow autonomously
- reviewers retain the visibility and safety signals they currently get from the Rust runtime

### Engineering Success

- the migration reuses the current Rust runtime's proven service architecture instead of discarding
  it
- the repo has a low-risk path to compare `rust-todoist/` behavior directly against `rust/`
- the Todoist runtime becomes detailed enough to support implementation, code review, smoke
  validation, and eventual promotion to the primary runtime

## Related Documents

- [Linear to Todoist migration technical spec](./linear-to-todoist-spec.md)
- [Todoist runtime research brief and compact handoff](./todoist-rust-runtime.md)
