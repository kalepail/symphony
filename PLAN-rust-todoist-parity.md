# Rust Todoist Parity Plan

Status: proposed execution plan
Last updated: March 12, 2026
Scope: bring `rust-todoist/` to comprehensive operational parity with the Elixir service while keeping Todoist as the tracker and replacing Linear-specific behavior with correct Todoist-native behavior

## Purpose

This plan turns the current review findings into an execution document we can work through iteratively.

The target is not just "Todoist support exists." The target is:

- `rust-todoist/` matches the proven runtime qualities of `elixir/`
- `rust-todoist/` preserves the proven Rust runtime skeleton and operator ergonomics
- Linear-specific assumptions are removed or replaced with explicit Todoist-native equivalents
- the remaining correctness, safety, and confidence gaps are closed with code, tests, smoke coverage, and operator docs

## Source Baseline

Primary repository references:

- `elixir/lib/symphony_elixir/orchestrator.ex`
- `elixir/lib/symphony_elixir/workspace.ex`
- `elixir/lib/symphony_elixir/codex/app_server.ex`
- `elixir/lib/symphony_elixir/codex/dynamic_tool.ex`
- `elixir/lib/symphony_elixir/config/schema.ex`
- `rust/src/orchestrator.rs`
- `rust/src/tracker/linear.rs`
- `rust-todoist/src/orchestrator.rs`
- `rust-todoist/src/tracker/todoist.rs`
- `rust-todoist/src/tracker/mod.rs`
- `rust-todoist/src/dynamic_tool.rs`
- `rust-todoist/src/workspace.rs`
- `rust-todoist/src/http.rs`
- `rust-todoist/src/observability.rs`
- `docs/linear-to-todoist-prd.md`
- `docs/linear-to-todoist-spec.md`
- `docs/todoist-rust-runtime.md`

Primary external references:

- official Todoist API v1 docs
- official Todoist help docs for limits and product semantics
- official Todoist SDK docs for Python and TypeScript as behavior references only

## Executive Summary

`rust-todoist/` is already strong in three areas:

- Todoist-native modeling and structured tracker actions
- richer operator-facing observability than Elixir in some surfaces
- a clearer forward runtime contract than raw Linear GraphQL tool usage

But it is not yet complete. The biggest remaining gaps are:

1. no SSH remote worker support or distributed execution parity with Elixir
2. incomplete hardening around workpad lifecycle, task scoping, and capability gating
3. insufficient parity-confidence coverage for orchestrator, Codex stream handling, and live smoke flows

This plan closes those gaps in a deliberate order.

## Locked Goals

The implementation work should preserve these goals:

- keep `rust-todoist/` as the forward Todoist runtime
- keep `rust/` as the current Linear reference until parity is complete
- measure runtime-quality parity primarily against actual behavior in `elixir/` and `rust/`
- keep Todoist-native modeling instead of back-porting fake Linear semantics
- preserve `github_api` as a host-side fallback surface
- preserve deterministic `memory` coverage and expand it where needed
- do not treat webhooks as the correctness source of truth in v1

## Non-Goals

- porting Todoist into `elixir/`
- replacing `rust/` in place before parity is proven
- inventing a fake blocker graph in Todoist
- introducing a shared crate before the implementation is stable
- making completed-task archive access a hard dependency for correctness

## Parity Definition

`rust-todoist/` reaches comprehensive parity when all of the following are true:

- orchestration, retry, continuation, cleanup, hooks, and observability match the operational quality of `elixir/`
- distributed execution support is present through SSH worker hosts
- Todoist tracker behavior is fully native and correctly bounded by project, section, label, assignee, and plan capability
- workpad behavior is safe, compact, deterministic, and reusable across retries and continuations
- structured tracker tooling is safe enough for unattended use
- deterministic tests and live smoke flows cover the critical happy paths and edge cases

## Current State Assessment

### Areas where `rust-todoist/` is already strong

- structured Todoist tool surface in `rust-todoist/src/dynamic_tool.rs`
- broad Todoist tracker surface in `rust-todoist/src/tracker/todoist.rs`
- Todoist-native issue fields in `rust-todoist/src/issue.rs`
- good HTTP/SSE/dashboard surfaces in `rust-todoist/src/http.rs` and `rust-todoist/src/observability.rs`
- stronger startup validation for Todoist account and project constraints

### Highest-impact missing capabilities

- SSH worker execution parity
- remote workspace lifecycle parity
- workpad compaction and duplicate-repair semantics
- strict mutation scoping to configured project and runtime slice
- stronger capability-shaped tool advertisement
- full parity-confidence tests around orchestrator and app-server edge cases

## Core Design Decisions

### 1. Keep Todoist Native

Map concepts like this:

- work item -> top-level Todoist task
- active workflow state -> Todoist section name
- done -> task close
- reopen/rework -> reopen + section move
- tracker comment substrate -> task comment only
- task identifier -> `TD-<task_id>`
- follow-up backlog item -> top-level task by default, subtask only when truly subordinate

Do not reintroduce Linear-only concepts as fake Todoist state.

### 2. Preserve the Runtime Skeleton

Retain and extend the existing Rust Todoist structure:

- CLI
- workflow loader and reload
- config layer
- orchestrator
- workspace manager
- Codex app-server integration
- observability presenter and HTTP server
- `memory` tracker

### 3. Prefer Explicit Tracker-Safe Actions

Continue the current direction of typed Todoist actions rather than raw escape hatches.

### 4. Add Distributed Execution as the Biggest Remaining Operational Gap

The most important parity lift from Elixir is SSH worker support.

## Workstreams

## Workstream 1: SSH Worker and Distributed Execution Parity

### Objective

Bring `rust-todoist/` up to Elixir parity for remote worker execution, host scheduling, per-host caps, retry affinity, and remote workspace handling.

### Reference behavior

- `elixir/lib/symphony_elixir/orchestrator.ex`
- `elixir/lib/symphony_elixir/workspace.ex`
- `elixir/lib/symphony_elixir/codex/app_server.ex`
- `elixir/lib/symphony_elixir/config/schema.ex`

### Deliverables

- worker config in `rust-todoist/src/config.rs`
- worker selection and per-host concurrency logic in `rust-todoist/src/orchestrator.rs`
- local and SSH workspace backends in `rust-todoist/src/workspace.rs`
- remote Codex app-server launch support in `rust-todoist/src/codex/mod.rs`
- worker metadata surfaced in `rust-todoist/src/observability.rs` and `rust-todoist/src/http.rs`

### Required config additions

- `worker.ssh_hosts`
- `worker.max_concurrent_agents_per_host`
- optional preferred-host retry affinity metadata

### Tasks

1. extend config parsing and validation for worker settings
2. add a worker selection layer that supports:
   - local worker when no SSH hosts exist
   - least-loaded host selection
   - per-host cap enforcement
   - preferred-host reuse on retries where safe
3. refactor workspace code to support local and remote execution backends
4. add remote hook execution semantics for `after_create`, `before_run`, `after_run`, `before_remove`
5. add remote workspace cleanup and stale workspace sweeping
6. add worker host metadata to snapshots, detail views, and retry entries
7. ensure path validation remains safe for remote `~` semantics and remote path confinement

### Acceptance criteria

- tasks can execute on configured SSH workers
- host capacity prevents over-dispatch
- retry entries preserve preferred host when appropriate
- remote workspaces are created, reused, and cleaned safely
- operator surfaces show worker host and remote workspace location

## Workstream 2: Todoist Contract Hardening

### Objective

Close the remaining correctness and safety gaps in the Todoist tracker and dynamic tool layers.

### Reference behavior

- `rust-todoist/src/tracker/todoist.rs`
- `rust-todoist/src/dynamic_tool.rs`
- `docs/linear-to-todoist-spec.md`

### Deliverables

- bounded workpad compaction
- canonical single-workpad enforcement or duplicate repair
- stricter task mutation scoping
- clearer capability gating
- stricter action schema validation

### Tasks

#### 2.1 Workpad lifecycle hardening

Implement in `rust-todoist/src/tracker/todoist.rs` and `rust-todoist/src/dynamic_tool.rs`:

- compact workpad content before write when approaching Todoist's `15000`-character comment limit
- preserve current sections like plan, status, blockers, validation, and handoff while compressing older history
- detect duplicate workpad comments and either:
  - repair to one canonical workpad and remove/archive the others, or
  - fail deterministically with a visible operator-facing error if automatic repair is unsafe
- ensure workpad APIs are the only path for task-scoped Symphony workpad comment mutation

#### 2.2 Project and runtime-slice scoping

Implement in `rust-todoist/src/tracker/todoist.rs`:

- verify mutated tasks belong to the configured `project_id`
- when `tracker.label` is configured, enforce runtime ownership on direct task mutation paths
- prevent known foreign tasks from being moved, updated, reopened, or closed accidentally

#### 2.3 Action schema tightening

Implement in `rust-todoist/src/dynamic_tool.rs`:

- reject unknown keys for structured Todoist actions
- keep strict required keys per action
- restrict raw task comment mutation to project comments only where intended
- ensure task comments for Symphony flow only through `get_workpad`, `upsert_workpad`, and `delete_workpad`

#### 2.4 Capability gating

Implement in `rust-todoist/src/tracker/todoist.rs` and `rust-todoist/src/dynamic_tool.rs`:

- advertise reminders only when supported by current account plan
- advertise activity log access only when supported
- validate comments support at startup and keep that as a hard requirement for Symphony workpad usage
- validate open terminal sections when workflow config expects them

#### 2.5 Follow-up task safety

Implement in `rust-todoist/src/tracker/todoist.rs`:

- auto-add origin back-reference like `TD-<task_id>` to follow-up descriptions
- default top-level follow-up tasks into the `Todo` section when available
- keep subtask creation explicit and intentional through `parent_id`

#### 2.6 Merge verification fallback

Implement in `rust-todoist/src/dynamic_tool.rs`:

- preserve current guarded `close_task` behavior
- add `github_api` fallback when `gh` is unavailable or insufficient
- keep merged-PR verification mandatory before closing from `Merging`

### Acceptance criteria

- workpad updates do not fail merely due to unbounded growth
- one canonical workpad comment exists per task
- direct mutation cannot escape the configured project or runtime slice
- unsupported reminder/activity features are not falsely advertised as normal actions
- follow-up tasks preserve origin traceability

## Workstream 3: Orchestrator Semantic Parity and Robustness

### Objective

Tighten runtime behavior around refresh, retry, continuation, cleanup, and Todoist-specific disappearance semantics.

### Reference behavior

- `elixir/lib/symphony_elixir/orchestrator.ex`
- `rust/src/orchestrator.rs`
- `rust-todoist/src/orchestrator.rs`

### Tasks

1. preserve the existing Todoist-native no-blocker default
2. review and harden missing-ID reconciliation for all running issues
3. ensure retry suppression correctly ignores stale retry tokens and timers
4. ensure normal worker exit schedules continuation correctly
5. ensure abnormal exit increases backoff and caps cleanly
6. ensure startup cleanup remains based on open-project enumeration, not terminal query semantics
7. ensure close/reopen/move flows are reflected accurately in runtime snapshots and cleanup decisions
8. ensure distributed-worker state integrates cleanly with retry, continuation, and cleanup logic

### Acceptance criteria

- running tasks do not survive after they disappear from active Todoist visibility
- retries do not revive stale state incorrectly
- continuations reuse workspace and thread state correctly
- startup cleanup removes stale local and remote workspaces safely

## Workstream 4: Observability and Operator Surface Completion

### Objective

Keep current Rust Todoist observability strengths and extend them to cover distributed-worker parity and remaining operator blind spots.

### Reference behavior

- `rust-todoist/src/observability.rs`
- `rust-todoist/src/http.rs`
- `elixir/lib/symphony_elixir_web/presenter.ex`

### Tasks

1. expose worker host and workspace location in snapshot payloads
2. expose retry affinity and host load where useful
3. ensure blocking reasons remain Todoist-specific and actionable
4. ensure detail views include enough tracker metadata for troubleshooting:
   - project
   - section
   - labels
   - due/deadline
   - workpad and close-task failures
5. ensure SSE continues to work with remote worker state transitions

### Acceptance criteria

- operators can answer where work is running, why it is blocked, and what will happen next without reading logs directly

## Workstream 5: `memory` Tracker and Deterministic Parity Coverage

### Objective

Make the `memory` tracker a real parity harness for the structured Todoist runtime, not just a thin fake.

### Reference behavior

- `rust-todoist/src/tracker/memory.rs`
- `docs/linear-to-todoist-spec.md`

### Tasks

1. verify the fixture model supports:
   - issues/tasks
   - sections
   - comments
   - current user
   - collaborators
   - labels
   - user plan limits
   - reminders
   - projects
2. ensure structured tool actions behave deterministically:
   - workpad retrieval and upsert
   - move/update/close/reopen/create task
   - project comment creation
   - reminder CRUD where enabled
3. add fixtures for duplicate workpads, oversized workpads, unsupported reminders, unsupported activity log, and scoped-ownership cases

### Acceptance criteria

- CI can exercise the hardening behavior without live Todoist access
- the memory tracker reflects the real structured tool contract well enough to prevent drift

## Workstream 6: Test and Validation Expansion

### Objective

Close the parity-confidence gap through targeted unit, integration, system, and live smoke coverage.

### Priority P0

#### 6.1 Orchestrator lifecycle tests

Add or expand tests around `rust-todoist/src/orchestrator.rs` for:

- active issue refresh without teardown
- missing running issue stops worker correctly
- normal exit schedules short continuation retry
- abnormal exit increases backoff progressively and caps it
- stale retry suppression
- manual refresh coalescing
- immediate poll after startup
- stall detection and retry reflection in snapshot
- local and remote worker variants

#### 6.2 Codex app-server and stream behavior

Add or expand tests around `rust-todoist/src/codex/mod.rs` for:

- partial JSON line buffering
- stderr diagnostics capture
- unsupported dynamic tool failure path
- successful Todoist tool call path
- approval and request-user-input variants
- remote app-server startup and teardown behavior

#### 6.3 Todoist tracker/tool guardrails

Add or expand tests around `rust-todoist/src/tracker/todoist.rs` and `rust-todoist/src/dynamic_tool.rs` for:

- close-task success from `Merging` with merged PR
- failure when workpad missing
- failure when PR URL missing
- failure when PR not merged
- `github_api` fallback path
- duplicate workpad handling
- workpad compaction behavior
- required section validation
- assignment filtering and validation
- rate-limit mapping
- oversized-comment handling
- project-scope and label-scope mutation blocking

### Priority P1

#### 6.4 Prompt, workflow, and env-path tests

Add or expand tests for:

- prompt fallback behavior
- blank prompt behavior
- due/deadline rendering
- continuation prompt rendering
- dotenv edge cases
- workflow reload error handling and last-known-good fallback

#### 6.5 Observability snapshot tests

Add or expand tests for:

- idle
- busy
- backoff queue
- remote worker running state
- remote worker failure state
- offline/unavailable snapshot handling
- SSE behavior with updated state payloads

### Priority P2

#### 6.6 Logging and lower-risk hardening

Add targeted tests for:

- log rotation boundaries
- invalid log filter fallbacks
- concurrent append behavior where relevant

### Acceptance criteria

- parity-critical runtime behavior is covered by deterministic tests
- the most dangerous unattended failure modes have explicit regression coverage

## Workstream 7: Live Smoke Matrix and Cutover Confidence

### Objective

Move from one live E2E to a meaningful live smoke matrix.

### Reference behavior

- `rust-todoist/tests/live_e2e.rs`
- `rust-todoist/SMOKE_TESTS.md`

### Proposed live smoke coverage

1. repo-backed minimal smoke flow that returns to `Backlog`
2. publish/PR flow
3. human review to rework flow
4. merge and guarded close flow
5. remote-worker smoke through the SSH/Docker-backed live harness

### Tasks

1. split or extend live smoke coverage to make the scenarios explicit
2. ensure smoke docs match the actual runtime contract
3. ensure one persistent workpad comment survives the full lifecycle
4. ensure close only occurs from `Merging` after verified merge
5. ensure follow-up tasks and subtasks behave as documented
6. add a remote-worker live smoke scenario when feasible

### Acceptance criteria

- live smoke proves the full unattended loop, not just a happy-path minimal run

## Workstream 8: Documentation and Operator Guidance

### Objective

Keep forward-path docs aligned with the runtime as the implementation evolves.

### Tasks

1. update `rust-todoist/README.md` once SSH support and hardening land
2. update `rust-todoist/SMOKE_TESTS.md` to reflect actual automation and runtime guardrails
3. update `docs/todoist-rust-runtime.md` and `docs/linear-to-todoist-spec.md` when implementation choices sharpen
4. document remote worker setup and failure modes
5. document workpad compaction and duplicate-handling behavior
6. document project-scope and label-scope enforcement so operators understand runtime ownership rules

### Acceptance criteria

- the docs describe the runtime that actually exists
- operators can configure and debug the system without source-diving

## Sequenced Execution Plan

Recommended order:

### Phase 1: SSH Worker Parity

- config
- worker selection
- remote workspace lifecycle
- remote Codex startup
- observability fields

### Phase 2: Todoist Hardening

- workpad compaction
- duplicate workpad handling
- strict task/project/label scoping
- capability gating
- schema tightening
- `github_api` close-task fallback

### Phase 3: Orchestrator Tightening

- missing-ID reconciliation review
- retry and continuation hardening
- startup cleanup review
- distributed-worker integration behavior

### Phase 4: Deterministic Test Expansion

- orchestrator tests
- Codex tests
- tracker and dynamic-tool tests
- memory tracker fixture additions
- observability tests

### Phase 5: Live Smoke Expansion

- minimal
- review/rework
- publish/merge
- remote-worker smoke

### Phase 6: Doc Sync and Cutover Readiness

- README and smoke docs
- final parity checklist
- cutover recommendation

## Backlog Checklist

## P0

- [ ] add SSH worker config to `rust-todoist/src/config.rs`
- [ ] add SSH worker scheduler to `rust-todoist/src/orchestrator.rs`
- [ ] add local and SSH workspace backends to `rust-todoist/src/workspace.rs`
- [ ] add remote Codex app-server launch support to `rust-todoist/src/codex/mod.rs`
- [ ] surface worker metadata in `rust-todoist/src/observability.rs` and `rust-todoist/src/http.rs`
- [ ] implement workpad compaction in `rust-todoist/src/tracker/todoist.rs`
- [ ] implement duplicate workpad detection and repair policy
- [ ] restrict task workpad mutation to dedicated workpad actions
- [ ] enforce configured project scope on direct task mutation paths
- [ ] enforce runtime label scope where configured
- [ ] tighten Todoist action schemas in `rust-todoist/src/dynamic_tool.rs`
- [ ] add `github_api` fallback for merge verification during guarded close
- [ ] expand orchestrator parity tests
- [ ] expand Codex stream/protocol tests
- [ ] expand Todoist tracker/dynamic-tool guardrail tests

## P1

- [ ] validate `terminal_states` open-section expectations at startup
- [ ] capability-shape reminder/activity tool advertisement
- [ ] auto-add origin back-reference on follow-up task creation
- [ ] expand memory tracker fixtures for hardening scenarios
- [ ] add remote-worker observability snapshot tests
- [ ] expand workflow/prompt/runtime-env reload edge-case tests

## P2

- [ ] expand logging validation depth
- [ ] refine lower-priority operator copy and dashboard polish
- [ ] revisit whether any shared abstractions should be extracted after parity is proven

## Release Gates

Do not call the plan complete until all of the following are true:

- `rust-todoist/` supports local and SSH workers safely
- remote workspace lifecycle and hooks work correctly
- one bounded canonical workpad exists per task
- structured Todoist actions reject invalid and out-of-scope mutations
- close-task guard uses reliable merged-PR verification with fallback support
- missing-ID reconciliation and startup cleanup are correct
- deterministic parity-critical tests are green
- live smoke matrix is green
- operator docs match the runtime

## Suggested Working Loop

Use this loop for execution:

1. pick the next unchecked P0 or P1 item
2. identify exact touched files before editing
3. implement the smallest end-to-end slice that produces real user-visible parity
4. add or expand deterministic tests for that slice
5. run the smallest relevant test set first, then broader validation
6. update this plan file as items move from pending to done
7. only move to the next workstream once the current slice is stable

## First Recommended Slice

Start here:

1. add worker config and worker metadata plumbing
2. add SSH workspace backend support
3. add remote Codex launch path
4. add orchestrator scheduling and retry affinity for workers
5. add focused tests for local vs remote scheduling and cleanup

Reason: this is the single biggest remaining parity gap versus Elixir, and it affects orchestrator behavior, workspace management, observability, and operator confidence all at once.

## Completion Definition

This plan is complete when `rust-todoist/` is the clear forward runtime for unattended operation because it combines:

- the runtime strengths of Elixir
- the operator-friendly observability of modern Rust Symphony
- the correct Todoist-native tracker contract
- the safety rails and test confidence needed for real unattended use
