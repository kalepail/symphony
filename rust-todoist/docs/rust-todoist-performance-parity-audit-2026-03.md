# Rust Todoist Performance and Parity Audit

Status: implementation guide with progress markers
Reviewed: March 13, 2026
Focus: performance, error reporting, flow recovery, and Elixir/SPEC parity

This document is the follow-up audit to `docs/rust-modernization-audit-2026-03.md`.

That earlier audit was the broad modernization pass. This one is narrower and more practical:

- prioritize control-plane performance over general hardening
- verify error reporting and recovery behavior against the actual Rust code
- compare remaining runtime gaps against the Elixir implementation
- check whether the current tests are proving meaningful behavior, not just returning green

The user preference for this pass is explicit and reasonable: do not overengineer security. Keep security work close to the current upstream baseline and spend the next implementation budget on performance, runtime recovery, and real parity.

## Executive Summary

`rust-todoist/` is no longer "behind Elixir" in the broad sense.

Current Rust Todoist is already at or ahead of Elixir in several important areas:

- Todoist-native tracker surface and structured tool contract
- degraded observability and operator-visible error reporting
- SSH worker support, remote workspace handling, host caps, and retry affinity
- live smoke coverage breadth

The remaining gaps are now narrower than this audit originally described.

The main runtime issues called out below were addressed on the current branch:

1. Todoist control-plane metadata now survives tracker reconstruction across orchestrator hot paths through a shared cache registry, startup validation is skipped when config is unchanged, and worker continuation refreshes reuse a tracker until the workflow config changes.
2. Rust now performs same-attempt worker-host failover for startup-stage failures instead of immediately backing off to the retry queue.
3. Worker failures now preserve stage/kind structure through retries and issue detail reporting.
4. The test suite now covers the specific cross-tick cache and startup failover behaviors that were previously missing.

The main open item after this pass is measurement depth: request-count regression coverage exists for the hot paths that mattered most, but there is still no dedicated `benches/` harness.

## Implementation Update

Implemented on this branch:

- shared Todoist metadata cache keyed to tracker bootstrap identity, reused across tracker instances
- startup validation only when the effective config changes
- worker continuation refresh tracker reuse until config reload changes the tracker config
- same-attempt worker-host failover for startup-stage failures
- typed `WorkerError` classification propagated into retry state and issue detail
- request-count tests for:
  - unchanged-config poll ticks
  - unrelated workflow reloads that preserve cached metadata
  - cache-key-changing workflow reloads that force metadata refetch
  - startup failover and structured error reporting surfaces

Still open by design:

- no dedicated `benches/` tree yet
- no wall-time benchmark suite beyond targeted request-count regression tests

## Sources Reviewed

Primary repository sources re-reviewed for this pass:

- `SPEC.md`
- `PLAN-rust-todoist-parity.md`
- `rust-todoist/README.md`
- `rust-todoist/docs/rust-modernization-audit-2026-03.md`
- `rust-todoist/docs/todoist-rust-runtime.md`
- `rust-todoist/docs/linear-to-todoist-spec.md`
- `rust-todoist/src/orchestrator.rs`
- `rust-todoist/src/tracker/mod.rs`
- `rust-todoist/src/tracker/todoist.rs`
- `rust-todoist/src/codex/mod.rs`
- `rust-todoist/src/observability.rs`
- `rust-todoist/src/http.rs`
- `rust-todoist/src/workspace.rs`
- `rust-todoist/tests/live_e2e.rs`
- `elixir/lib/symphony_elixir/orchestrator.ex`
- `elixir/lib/symphony_elixir/agent_runner.ex`
- `elixir/lib/symphony_elixir/codex/app_server.ex`
- `elixir/lib/symphony_elixir/tracker.ex`

Validation run on the current branch state:

- `cargo test`
- `cargo test -- --list`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo build --release`

Observed result on March 13, 2026 after implementation work:

- `226` local unit/system tests passed
- `17` non-ignored helper tests in `tests/live_e2e.rs` passed
- `6` env-gated live end-to-end tests remained ignored as expected
- `cargo clippy` passed with `-D warnings`
- `cargo build --release` passed

## Parity Verdict

### Where Rust Todoist is already at parity or ahead

- SSH worker support is present. The earlier parity plan is stale here.
  - Rust has host selection, per-host caps, retry host affinity, remote workspace creation, remote hook execution, remote cleanup, and SSH-backed Codex startup.
- Error reporting is stronger than Elixir on degraded observability paths.
  - Rust preserves distinct `snapshot_timeout` and `snapshot_unavailable` error codes and keeps the HTML dashboard shell alive in degraded mode.
- Flow recovery is already stronger in a few Todoist-specific paths.
  - Rust has `restore_active_issue()` and uses it when reconciliation sees an unexpected missing or terminal task during an active run.
- Tracker and tool parity is not the problem anymore.
  - Rust's `TrackerClient` contract is materially richer than Elixir's tracker boundary, and the structured Todoist tool surface is much safer than the old `linear_graphql` shape.

### What remains after this pass

- explicit performance measurement and regression harnesses beyond request-count coverage

## Findings

### P1. Tracker metadata caching is undermined by repeated tracker reconstruction

Status on current branch: addressed for the main hot paths.

The recent Todoist metadata cache is real, but it is currently scoped to a single `TodoistTracker` instance.

Evidence:

- `TodoistTracker` stores `metadata_cache` per instance in `src/tracker/todoist.rs` and initializes it with a fresh `Arc<Mutex<TodoistMetadataCache>>` in `TodoistTracker::new()` (`src/tracker/todoist.rs:52-56`, `src/tracker/todoist.rs:517-523`).
- The orchestrator rebuilds the tracker repeatedly:
  - startup: `src/orchestrator.rs:318-324`
  - every poll tick before dispatch: `src/orchestrator.rs:554-565`
  - running-issue reconciliation: `src/orchestrator.rs:751-760`
  - dispatch revalidation: `src/orchestrator.rs:1125-1140`
  - retry handling: `src/orchestrator.rs:1346-1378`
  - worker startup: `src/orchestrator.rs:1954-1956`
  - per-turn state refresh inside the worker loop: `src/orchestrator.rs:2010-2015`
- The migration spec frames bootstrap as startup work, not poll-loop work (`docs/linear-to-todoist-spec.md`, "Startup Bootstrap").

Why this matters:

- `validate_startup()` does real control-plane work in Todoist: project lookup, section resolution, assignee resolution, and comment-capability checks.
- Within a single tracker instance the cache works, and the tests prove that.
  - `fetch_candidate_issues_reuses_cached_project_metadata_across_polls` (`src/tracker/todoist.rs:3421-3469`)
  - `fetch_issue_states_batches_requests_and_reuses_cached_current_user_lookup` (`src/tracker/todoist.rs:3472-3545`)
- Across orchestrator ticks and worker turns, that cache is thrown away and rebuilt.
- The rate budget is intentionally shared across tracker instances (`src/tracker/todoist.rs:4193-4228`), but the metadata cache is not. That asymmetry is a useful clue: the performance-sensitive shared state is only partially shared today.

Operational effect:

- higher steady-state request volume to Todoist than the current cache design suggests
- more startup-validation churn on short poll intervals
- more chances for transient `/sync`, `/user`, `/projects/{id}`, or `/sections` failures to block new dispatches
- less benefit from the cache work already landed

Recommendation:

1. Move the live tracker client into orchestrator runtime state and reuse it across poll/reconcile/dispatch/retry paths while the effective config fingerprint is unchanged.
2. Re-run `validate_startup()` only on orchestrator start and when the workflow/config actually changes.
3. Keep the shared rate-budget registry exactly as-is.
4. If a full shared tracker instance is too invasive for the first step, lift metadata caching into a shared registry keyed by base URL, token, and project id.

Acceptance criteria:

- two consecutive poll ticks with unchanged config should not re-fetch project metadata, sections, collaborators/current user, or plan limits
- a worker continuation turn should not rebuild the Todoist control-plane view from scratch
- workflow reload should update validated config, while cache-key changes should trigger fresh metadata fetches

Implemented coverage:

- `run_tick_reuses_startup_validation_and_shared_tracker_metadata_when_config_is_unchanged`
- `workflow_reload_updates_validated_config_without_refetching_unrelated_metadata`
- `workflow_reload_refetches_tracker_metadata_when_cache_key_changes`

### P1. Rust still lacks Elixir's same-attempt worker-host failover

Status on current branch: addressed.

Elixir still has one recovery behavior that Rust Todoist does not: if a chosen worker host fails during startup, Elixir immediately tries the next configured host in the same run attempt.

Evidence:

- Elixir builds an ordered candidate host list and recursively falls through to the next host on failure in `AgentRunner` (`elixir/lib/symphony_elixir/agent_runner.ex:13-19`, `elixir/lib/symphony_elixir/agent_runner.ex:29-36`, `elixir/lib/symphony_elixir/agent_runner.ex:191-201`).
- Rust selects one host in `dispatch_issue()` and starts a single worker task (`src/orchestrator.rs:1137-1168`).
- `run_worker()` returns early on workspace creation failure, `before_run` hook failure, tracker-build failure, or session start failure (`src/orchestrator.rs:1946-1965`).
- `handle_worker_exit()` then schedules a delayed retry instead of trying another available host immediately (`src/orchestrator.rs:1258-1329`).

Why this matters:

- one flaky SSH host currently turns a potentially recoverable dispatch into a backoff cycle
- this directly hurts throughput and latency under partial worker-host failures
- this is exactly the kind of recovery behavior the user cares about for high-likelihood delivery

Recommendation:

Implement same-attempt failover for startup-stage failures only:

1. Build the same ordered candidate host list Elixir uses: preferred host first, then the remaining configured hosts.
2. Treat these as startup-stage failures eligible for immediate fallback:
   - workspace creation
   - `before_run` hook
   - tracker construction for the worker session
   - Codex app-server session start
3. Once a turn is actually running on a host, keep the current retry path. Do not try to migrate a live session.
4. Preserve host affinity on later retries, but allow the same-attempt fallback to escape the preferred host when that host is unhealthy.

Acceptance criteria:

- a failure on `ssh-a` with `ssh-b` healthy should still produce a running worker on the original dispatch attempt
- the retry queue should only be used when every candidate host fails
- observability should show both the failed startup host and the fallback host

Implemented coverage:

- `startup_host_failover_tries_next_worker_host`

### P1. Worker failures lose too much structure too early

Status on current branch: addressed for retry/detail reporting.

Rust has strong operator-facing event humanization once Codex is running, but the worker lifecycle still collapses many failures into plain strings.

Evidence:

- `run_worker()` returns `Result<WorkerDisposition, String>` (`src/orchestrator.rs:1930-2069`)
- failures are stringified at:
  - workspace create (`src/orchestrator.rs:1946-1948`)
  - `before_run` hook (`src/orchestrator.rs:1949-1951`)
  - tracker build (`src/orchestrator.rs:1954-1955`)
  - session start (`src/orchestrator.rs:1957-1964`)
  - prompt build / turn execution / post-turn refresh (`src/orchestrator.rs:1978-2015`)
- by the time `handle_worker_exit()` receives the failure, it only sees `Err(String)` (`src/orchestrator.rs:1258-1329`)

Why this matters:

- the retry system cannot distinguish "rate limited by Todoist", "worker host unreachable", "Codex approval required", "hook timeout", and "state refresh failed"
- observability loses the failure stage unless a Codex event had already emitted one
- the missing structure makes host failover and targeted backoff harder to implement cleanly

This is not a call for more security machinery. It is a call for better runtime classification.

Implemented shape:

- `stage`: `startup`, `workspace_create`, `before_run`, `tracker_build`, `session_start`, `turn_run`, `state_refresh`, `cleanup`
- `kind`: `startup_failover_exhausted`, `workspace_failure`, `hook_failure`, `tracker_failure`, `session_failure`, `turn_failure`, `approval_required`, `input_required`, `cancellation`
- `worker_host`
- `message`

Implemented coverage:

- `worker_failure_retry_captures_structured_error_fields`
- `issue_detail_includes_retry_issue_id`

Positive note:

Rust is already ahead on degraded error presentation once the orchestrator handle path itself fails.

- `startup_failed` is humanized in the presenter (`src/observability.rs:1472-1475`)
- snapshot errors remain distinct (`src/observability.rs:2150-2158`)

The gap is specifically the worker bootstrap path, not the observability layer as a whole.

### P2. The performance audit still lacks a measurement harness

Status on current branch: partially addressed.

The repo now has better control-plane behavior than it did a day ago, but it still has no benchmark harness or fixture-backed steady-state performance suite.

Evidence:

- `cargo test -- --list` reports `0 benchmarks`
- there is still no `benches/` tree in `rust-todoist/`

Why this matters:

- the main remaining performance risks are now orchestration-path regressions, not obvious algorithmic mistakes
- request-count regressions are easy to reintroduce in tracker rebuild paths, config reload logic, or recovery flows
- without explicit measurement, future refactors can quietly undo the recent batching/cache wins

Current state:

- request-count regression coverage now exists for the hot paths that were most at risk
- there is still no `benches/` tree or wall-time benchmark harness

Recommendation:

Keep expanding fixture-backed performance coverage for the paths that actually matter:

1. steady-state poll tick with unchanged config
2. running-issue reconciliation with 1, 10, and 100 active issue ids
3. worker continuation refresh path across multiple turns
4. startup validation plus first candidate poll

The first pass does not need microbenchmarks everywhere.

Preferred measurement style:

- request-count assertions first
- wall-time benchmarks second
- no premature optimization of UI rendering or tiny helper functions

### P2. The test suite is meaningful, but it is missing the exact recovery/performance cases that matter most now

Status on current branch: mostly addressed for the highest-value cases.

The current test story is stronger than "green but shallow." It already proves a lot of real behavior.

What is already meaningfully covered:

- tracker caching and batching inside a tracker instance
  - `fetch_candidate_issues_reuses_cached_project_metadata_across_polls`
  - `fetch_issue_states_batches_requests_and_reuses_cached_current_user_lookup`
  - `fetch_issue_states_splits_large_refresh_sets_into_batches`
- rate-limit behavior
  - pre-throttle
  - retry-after budgets
  - stable request ids on retried writes
- workpad lifecycle and duplicate repair
- SSH workspace creation and remote sweeping
- local and SSH live E2E paths for:
  - real Todoist task flow
  - minimal smoke flow
  - full smoke parity flow

What was added on this branch:

- same-attempt fallback from one worker host to another
- cross-tick tracker reuse at the orchestrator layer
- structured worker bootstrap failure reporting
- workflow reload behavior for both unrelated config changes and cache-key-changing config changes

Recommendation:

Treat any next test additions as targeted parity/performance tests, not generic coverage expansion:

1. same-attempt failover at additional startup stages beyond the current session-start case
2. worker continuation-turn performance coverage with direct request-count assertions
3. optional wall-time benchmark coverage once request-count regressions stop finding issues

### P3. One parity doc is now stale enough to be misleading

`PLAN-rust-todoist-parity.md` still says the biggest gap is "no SSH remote worker support or distributed execution parity with Elixir."

That is no longer true.

Current Rust Todoist already has:

- SSH worker placement
- host selection
- per-host caps
- retry affinity
- remote workspace operations
- SSH-backed live smoke coverage

This matters because stale parity docs waste implementation time on already-closed gaps.

Recommendation:

- update the parity plan so the top open items match this audit:
  - tracker reuse across hot paths
  - same-attempt worker-host failover
  - typed worker failure reporting
  - performance regression harnesses

## SPEC Alignment

Against the language-agnostic `SPEC.md`, current Rust Todoist is in good shape on the big contract items:

- bounded concurrency
- single authoritative orchestrator state
- per-issue workspaces
- retries and continuation
- restart recovery without a database
- operator-visible observability

The remaining gaps are not "missing the spec." They are "the current implementation can still do the spec more efficiently and recover more gracefully under partial failures."

The most important SPEC-aligned improvements are:

1. move Todoist bootstrap work out of the steady-state poll loop when config has not changed
2. make startup-stage worker-host recovery match Elixir's proven behavior
3. preserve structured worker failure causes so recovery policy can be smarter than generic retry

## What Not To Do Next

The next pass should avoid churn that does not buy delivery odds.

Do not prioritize:

- broad new security policy layers beyond current upstream-safe defaults
- replacing short-lived mutex usage just because it looks "more modern"
- switching the core polling loop to `/sync`
- large cosmetic refactors of modules that are not on the hot path
- speculative async/concurrency rewrites without measurement

## Recommended Implementation Order

### Phase 1: Control-plane performance

1. Reuse a live tracker instance across orchestrator hot paths while config is unchanged.
2. Run `validate_startup()` on start and workflow/config change, not every poll tick.
3. Add request-count tests that prove the cache now survives across ticks and continuation turns.

### Phase 2: Recovery parity

1. Add same-attempt worker-host fallback for startup-stage failures.
2. Introduce typed worker failure classification.
3. Surface structured startup failure data in retries and issue detail.

### Phase 3: Measurement and doc cleanup

1. Add fixture-backed control-plane benchmarks or request-count performance tests.
2. Update `PLAN-rust-todoist-parity.md` so it reflects the current runtime, not the pre-SSH state.

## Bottom Line

The branch is in a better place than the stale parity plan implies.

Rust Todoist is already credible on functionality, Todoist-native behavior, operator observability, and live smoke coverage. The next meaningful work is not broad hardening. It is targeted performance and recovery work:

- make the existing metadata cache survive the orchestrator's real hot paths
- match Elixir's startup failover behavior across worker hosts
- preserve structured worker failure causes so recovery and reporting stay precise

If those three areas are addressed carefully, the runtime will have a much stronger chance of delivering the same `SPEC.md` workflow quality that Elixir delivered for Linear, but on Todoist and in Rust.
