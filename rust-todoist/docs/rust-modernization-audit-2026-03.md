# Rust Todoist Audit Against March 2026 Best Practices

Audit date: 2026-03-13

## Executive Summary

`rust-todoist` is in better shape than a typical early Rust service. The codebase is already on Rust 2024 edition, `cargo test` passes, `cargo clippy --all-targets --all-features -- -D warnings` passes, and a live `cargo audit` scan is clean as of this audit date.

The main gaps are not "Rust fundamentals are broken." The gaps are project-policy and scaling gaps:

1. `rust-todoist` is effectively outside the repository's Rust CI enforcement path.
2. The Todoist control plane does extra metadata work on every poll/refresh cycle that will consume rate budget unnecessarily as task volume grows.
3. The crate has no explicit toolchain/MSRV/lint/release-profile policy checked into the package.
4. Supply-chain verification is currently ad hoc rather than encoded in CI and config files.
5. Several core files are too large and too `serde_json::Value`-heavy for long-term maintainability.

Given the current implementation priority, the next pass should focus on control-plane performance first, then CI/toolchain baseline, then module decomposition. Security work should stay close to the upstream repo baseline rather than introducing a lot of package-local policy machinery.

## Implementation Progress

This document is now also the implementation tracker for the follow-on work.

- Done on 2026-03-13: the Todoist tracker now caches configured-project metadata in `src/tracker/todoist.rs` with bounded TTLs for project details, section maps, assignee resolution, collaborator IDs, current-user lookup, and plan limits.
- Done on 2026-03-13: `fetch_issue_states_by_ids()` now batches Todoist refresh lookups through `/tasks?ids=...` instead of issuing one request per task.
- Done on 2026-03-13: added request-count tests proving repeated poll and refresh paths reuse cached metadata instead of re-hitting Todoist control-plane endpoints.
- Done on 2026-03-13: added a dedicated `rust-todoist` CI workflow plus `rust-toolchain.toml`, `rust-version`, conservative lint policy, and an explicit thin-LTO release profile.
- Next: split the largest modules and add fixture-backed performance measurement for the Todoist poll/refresh paths.
- Deferred unless upstream practice changes: `cargo-deny` or `cargo-vet` rollout. `cargo audit` in CI is sufficient for now.

## What I Verified

Local checks run during this audit:

- `cargo test`: passed
  - 219 unit/integration tests in `src`
  - 17 non-ignored tests in `tests/live_e2e.rs`
  - 6 live end-to-end tests remain intentionally ignored unless credentials are provided
- `cargo clippy --all-targets --all-features -- -D warnings`: passed
- `cargo audit -q`: clean on 2026-03-13
- `cargo build --release`: succeeded
  - local build time: about 31s on this machine after enabling thin LTO
  - output binary: about 13 MB at `target/release/symphony-todoist`
  - current release build is warning-clean

## What Is Already Good

### Security and runtime posture

- Child Codex processes strip `TODOIST_API_TOKEN` from their environment before spawn in `src/codex/mod.rs:873`.
- Default Codex approval policy rejects sandbox approvals, rules approvals, and MCP elicitations unless the workflow explicitly overrides it in `src/config.rs:612`.
- Default turn sandbox policy keeps network disabled and narrows writable roots to the workspace in `src/config.rs:645` and `src/codex/mod.rs:917`.
- The project already documents that this runtime is intended for trusted environments rather than pretending to be multi-tenant hardened.

### Async/runtime fundamentals

- The orchestrator uses bounded channels, watch updates, cancellation tokens, and explicit timeouts instead of open-ended task orchestration.
- The Todoist client already has pre-throttling and retry-budget logic, which is a strong foundation for API-facing resilience.
- I did not find evidence of production code holding a mutex guard across `.await` in the main runtime paths. The short `std::sync::Mutex` critical sections in `src/http.rs` and `src/tracker/todoist.rs` are acceptable and should not be "fixed" just to look more async-native.

### Test coverage

- The project has unusually deep test coverage for a service runtime, including:
  - config parsing
  - orchestration behavior
  - dynamic tool behavior
  - HTTP/SSE behavior
  - Todoist rate-limit handling
  - live env-gated E2E flows

## Priority Findings

### P1. `rust-todoist` is outside the repo's Rust CI path

Why it matters:

- This is the largest structural gap in the project today.
- The repository has a Rust CI workflow, but it only watches `rust/**` and runs inside `working-directory: rust`.
- `rust-todoist` lives at `rust-todoist/**`, so changes here can bypass the repo's Rust CI entirely.

Evidence:

- `.github/workflows/rust-ci.yml:5-28`

Recommendation:

- Either broaden the existing workflow to include `rust-todoist/**`, or add a dedicated `rust-todoist-ci.yml`.
- Make CI run, at minimum:
  - `cargo fmt --check`
  - `cargo clippy --all-targets --all-features -- -D warnings`
  - `cargo test`
  - `cargo build --release`
  - `cargo audit`
  - `sh -n scripts/workspace_before_remove.sh`
  - `sh -n scripts/github_publish_preflight.sh`

Success criteria:

- Any PR touching `rust-todoist/**` automatically runs Rust checks for this package.

Implementation progress:

- Implemented on 2026-03-13 via `.github/workflows/rust-todoist-ci.yml`.

### P1. Toolchain and crate policy are underspecified

Why it matters:

- `Cargo.toml` declares `edition = "2024"` but does not declare `rust-version`.
- The project also has no `rust-toolchain.toml`, no package-level `[lints]`, and no explicit `[profile.release]`.
- That makes the project more dependent on local convention than checked-in policy.

Evidence:

- `Cargo.toml:1-38`
- local metadata inspection reported `rust_version: null`
- no package-local `rust-toolchain.toml`, `.cargo/config.toml`, `clippy.toml`, or `deny.toml` was found

Recommendation:

- Decide the support policy explicitly. For an internal service runtime, the pragmatic default is "latest stable only."
- Check in:
  - `rust-toolchain.toml` pinned to the org-approved stable toolchain
  - `rust-version` in `Cargo.toml`
  - package-level `[lints.rust]` and `[lints.clippy]`
- Start with a conservative lint policy. Example direction:
  - `unsafe_code = "warn"` first, then move to `deny` if the remaining unsafe call sites are intentionally isolated
  - `unused_must_use = "deny"`
  - selected clippy policy lints for API correctness and error handling

Notes:

- This crate already behaves like a service binary, not a published compatibility library. That argues for pinning aggressively rather than optimizing for broad compiler compatibility.

Implementation progress:

- Implemented on 2026-03-13 with `rust-version = "1.85"`, a stable toolchain file, conservative lint gates, and `profile.release.lto = "thin"`.

### P1. Supply-chain verification is not encoded in the repo

Why it matters:

- The current lockfile is clean today, but there is no checked-in policy ensuring it stays that way.
- Modern Rust teams treat advisory scanning and dependency-policy checks as normal CI, not as occasional operator commands.

Evidence:

- `cargo audit` is clean on 2026-03-13
- no `deny.toml`
- no supply-chain job in CI covering `rust-todoist`

Recommendation:

- Add `cargo audit` to CI immediately.
- Keep the security baseline close to the rest of the repository for now.
- Defer `cargo-deny` and `cargo-vet` unless the upstream repo adopts them broadly or the dependency surface becomes materially riskier.

Success criteria:

- `cargo audit` runs automatically on every `rust-todoist` PR.

### P1. The Todoist control plane repeats avoidable metadata work

Why it matters:

- The polling and refresh paths repeatedly re-fetch project metadata, sections, current-user data, and collaborator data.
- That increases latency and burns Todoist rate budget without improving correctness on every cycle.
- The current rate limiter mitigates pressure, but it does not remove wasted work.

Evidence:

- `src/tracker/todoist.rs:773-923`
- `src/tracker/todoist.rs:1248-1335`

What is happening now:

- `fetch_candidate_issues()` calls `section_map()` and `resolve_assignee_filter()` before every task fetch.
- `fetch_issue_states_by_ids()` repeats `section_map()` and `resolve_assignee_filter()` again before refreshing issue states.
- `resolve_assignee_filter()` can trigger `get_project`, `get_current_user`, and collaborator pagination.

Recommendation:

- Introduce a small metadata cache for the configured Todoist project with explicit TTLs.
- Good cache candidates:
  - project resource
  - section map
  - assignee resolution
  - collaborator IDs
  - user-plan limits
- Keep task fetches live; cache only the relatively stable control-plane metadata.

Implementation progress:

- Implemented on 2026-03-13 in `src/tracker/todoist.rs`.
- Current shape is intentionally narrow: per-tracker-instance cache with short TTLs, which means cache invalidation naturally happens when config reload constructs a new tracker.
- `fetch_issue_states_by_ids()` now batches refresh lookups via `/tasks?ids=...`, which removes the per-task HTTP fan-out for state reconciliation.
- Added targeted tests that assert repeated poll/refresh flows only hit Todoist metadata endpoints once while still fetching live task payloads each time.

Target outcome:

- Reduce steady-state control-plane requests per poll/refresh cycle by at least 50% without changing task freshness.

### P2. Core modules are too large and too stringly typed

Why it matters:

- Several files are already beyond the size where review and refactoring stay cheap.
- The hottest boundary code still relies heavily on `serde_json::Value` and manual key lookup.
- That is workable now, but it will get more fragile as the Todoist/GitHub surfaces evolve.

Evidence from local scan:

- `src/observability.rs`: 3,833 lines
- `src/tracker/todoist.rs`: 3,666 lines
- `tests/live_e2e.rs`: 3,544 lines
- `src/orchestrator.rs`: 2,992 lines
- `src/dynamic_tool.rs`: 2,813 lines
- `src/tracker/memory.rs`: 2,775 lines
- `src/codex/mod.rs`: 2,173 lines

Recommendation:

- Split large files by responsibility, not by arbitrary line count.
- Highest-value decompositions:
  - `tracker/todoist/{client, rate_limit, paging, dto, validation, mutations}.rs`
  - `dynamic_tool/{todoist, github_api, workpad, schema, errors}.rs`
  - `observability/{presenter, html, tui, formatting}.rs`
  - `tests/live_e2e/{todoist, github, ssh, smoke_repo}.rs`
- Introduce typed DTOs for the hot-path Todoist entities first:
  - project
  - section
  - task
  - comment/workpad
  - plan limits
- Keep raw `Value` only at the outermost compatibility edge where flexibility is genuinely needed.

Expected benefit:

- Better compiler help, fewer string-key mistakes, smaller review units, and lower change risk.

### P2. Release-build policy and benchmark policy are missing

Why it matters:

- The crate has no explicit release profile policy and no benchmark harness.
- The current local release build succeeds, but it inherits Cargo defaults rather than a deliberate service-binary profile.
- A performance audit should not end at "seems fast enough"; it should leave behind measurement points.

Evidence:

- no `[profile.release]` in `Cargo.toml`
- no `benches/` directory or Criterion/Divan-style benchmark harness was found
- local `cargo build --release` took about 26s and produced a 13 MB binary

Recommendation:

- Add a first explicit release profile and measure before/after:
  - start with `lto = "thin"`
  - evaluate lower `codegen-units`
  - evaluate `strip = "debuginfo"` or another org-approved stripping policy
- Add benchmark coverage for the paths that actually matter:
  - poll cycle over large Todoist fixture sets
  - `fetch_issue_states_by_ids()` reconciliation
  - dashboard/state payload rendering
  - workpad compaction and comment normalization
- Add one release-build CI job so warning-free release artifacts are enforced.

### P2. Observability degraded-state handling is still incomplete

Why it matters:

- The handle layer distinguishes timeout from unavailability, but the HTTP layer collapses those cases into `snapshot_unavailable`.
- The web dashboard route still returns a JSON 503 instead of rendering a degraded HTML shell on first load.
- This is more of an operability/structure issue than a raw performance problem, but it is real production ergonomics debt.

Evidence:

- `src/orchestrator.rs:479-527`
- `src/http.rs:120-225`
- `src/observability.rs:942-955`

Recommendation:

- Carry forward the already-identified observability fix:
  - preserve `TimedOut` vs `Unavailable` through the presenter/HTTP surface
  - render degraded HTML on `/` instead of raw JSON
  - wire up the existing failure-dashboard helper or replace it cleanly

## Things I Would Not Change Yet

### Do not replace every `std::sync::Mutex` with `tokio::sync::Mutex`

- The current short critical sections in `src/http.rs` and `src/tracker/todoist.rs` are fine.
- The Tokio guidance is explicit that `std::sync::Mutex` is usually preferred when the lock is short-lived and not held across `.await`.
- Replacing these mechanically would add churn without buying real performance or correctness.

### Do not remove shell-based hook/config execution just for aesthetics

- `src/codex/mod.rs:873-905` and `src/workspace.rs:436-460` intentionally support operator-provided shell commands.
- Given the runtime's documented trusted-environment posture, the right next step is stronger policy/documentation around that boundary, not a rushed redesign into a less flexible command schema.

## Recommended Implementation Order

### Phase 1: Control-plane performance

- Add metadata cache for project/section/assignee/plan-limit lookups
- Add request-count instrumentation or tests around Todoist control-plane calls
- Verify reduced API pressure against representative fixtures

Status:

- Metadata cache and request-count tests are done.
- Wider performance measurement and fixture-based benchmarking still remain.

### Phase 2: CI and crate policy

- Put `rust-todoist` under mandatory CI
- Add `rust-toolchain.toml`
- Add `rust-version`
- Add package-level lint policy
- Add release-build CI
- Make release builds warning-clean

### Phase 3: Structural decomposition

- Split `tracker/todoist.rs`
- Split `dynamic_tool.rs`
- Split `observability.rs`
- Type the hot-path DTOs

### Phase 4: Minimal security baseline

- Add `cargo audit` to CI
- Keep additional dependency policy aligned with upstream repo-wide practice

### Phase 5: Operability polish

- Distinguish timeout vs unavailable in HTTP/dashboard responses
- Render degraded HTML on `/`
- Keep release builds warning-free

## Reference Sources

Official or primary guidance used for this audit:

- Rust 1.94.0 release announcement: <https://blog.rust-lang.org/2026/03/05/Rust-1.94.0/>
- Cargo `rust-version` guidance: <https://doc.rust-lang.org/cargo/reference/rust-version.html>
- Cargo manifest lint configuration: <https://doc.rust-lang.org/cargo/reference/manifest.html#the-lints-section>
- Cargo profile settings: <https://doc.rust-lang.org/cargo/reference/profiles.html>
- Tokio shared-state guidance: <https://tokio.rs/tokio/tutorial/shared-state>
- RustSec advisory database overview: <https://rustsec.org/>
- `cargo-vet` book: <https://mozilla.github.io/cargo-vet/>
