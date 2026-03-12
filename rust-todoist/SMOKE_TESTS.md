# Symphony Smoke Tests

This document defines the live smoke matrix for the Rust runtime against the dedicated GitHub smoke repository [kalepail/symphony-smoke-lab](https://github.com/kalepail/symphony-smoke-lab).

## Repository

- Repo: `kalepail/symphony-smoke-lab`
- Default branch: `main`
- Canonical mutation target: `SMOKE_TARGET.md`
- Review/rework target: `smoke/review-target.md`
- Validation command: `sh scripts/validate-smoke-repo.sh`
- CI workflow: `.github/workflows/ci.yml`
- PR template: `.github/pull_request_template.md`
- Required PR label: `symphony`

## Required Environment

- `TODOIST_API_TOKEN`
- `SYMPHONY_WORKSPACE_ROOT`
- `SYMPHONY_SMOKE_PROJECT_ID`
- If `tracker.assignee` will be set, a Todoist project that supports assignment and exposes the intended assignee as a collaborator
- GitHub CLI authenticated with `repo` scope and `gh auth setup-git` already applied on the host
- For the direct GitHub REST fallback, either `GH_TOKEN` / `GITHUB_TOKEN` is exported or `gh auth token` succeeds on the host
- File logs are written to `log/symphony.log` or the chosen `--logs-root`; Symphony lifecycle lines stay visible there even if the shell exports `RUST_LOG=warn`

`SYMPHONY_SMOKE_PROJECT_ID` should point at a dedicated Todoist project reserved for Symphony smoke runs.
Seed minimal smoke tasks with the label `symphony-smoke-minimal` and full/rework/merge smoke tasks with the label `symphony-smoke-full`.
Todoist comments must be available on the connected account or plan because the persistent Symphony workpad is a task-scoped comment and startup now validates that capability.

## Canonical Todoist Sections

For exact parity with the original Elixir workflow and [SPEC.md](../SPEC.md), the dedicated smoke Todoist setup now uses this section organization:

- Visible columns: `Backlog`, `Todo`, `In Progress`, `Human Review`
- Hidden columns: `Rework`, `Merging`, `Done`, `Canceled`, `Duplicate`

Smoke parity runs and production workflows should use the canonical names above directly.
`Human Review` is a human handoff column, not an active dispatch state. Symphony resumes only after a human moves the task to `Rework` or `Merging`.
For workpad validation, treat task-scoped comments and their `item_id` field as the canonical Todoist comment surface for a task. Full workflow runs should leave exactly one surviving task-scoped workpad comment marked by both `## Codex Workpad` and `<!-- symphony:workpad -->`.

## Operator Preflight

Before any PR-oriented live smoke (`smoke-pr`, `smoke-rework`, `smoke-merge`, or `parity-smoke`), confirm the local GitHub CLI can see the smoke repo and required label:

```bash
sh scripts/github_publish_preflight.sh --repo kalepail/symphony-smoke-lab --label symphony
```

This verifies both the `gh` path and the direct GitHub REST fallback against the target repo and required label before the live run starts.

## Workflow Files

- Minimal live smoke: [WORKFLOW.smoke.minimal.md](./WORKFLOW.smoke.minimal.md)
- Full live smoke: [WORKFLOW.smoke.full.md](./WORKFLOW.smoke.full.md)
- Repo-owned live E2E harness: [tests/live_e2e.rs](./tests/live_e2e.rs)

## Observability Evidence

Each live smoke should capture both operator surfaces while the run is active:

- Terminal dashboard evidence:
  - header with agents/max, runtime, tokens, project URL, and next refresh
  - running or backoff row for the active smoke issue
- Web dashboard evidence:
  - live status badge connected to `/api/v1/stream`
  - runtime and throughput cards
  - running session row with JSON details link

If the stream degrades, also capture the polling-fallback badge state. For one unavailable-path check, capture the terminal offline frame and the web fallback/offline badge behavior.

## Smoke Matrix

1. `smoke-minimal`
   - Workflow: `WORKFLOW.smoke.minimal.md`
   - Seed task label: `symphony-smoke-minimal`
   - Proves: live Todoist polling, workspace bootstrap, Codex turn execution, repo mutation, validation command execution, single-workpad comment discipline, a bounded non-terminal state transition back to `Backlog`, and terminal/web observability during a live run
   - Expected repo effect: one appended bullet in `SMOKE_TARGET.md`

2. `smoke-pr`
   - Workflow: `WORKFLOW.smoke.full.md`
   - Seed the issue in `Todo`
   - Seed task label: `symphony-smoke-full`
   - Task body should instruct the agent to update `SMOKE_TARGET.md`, run `sh scripts/validate-smoke-repo.sh`, commit, push, open a PR, label it `symphony`, and attach the PR details to the Todoist task
   - Expected outcome: issue reaches `Human Review` with a green PR
   - Expected observability: SSE dashboard stays live during publish; terminal dashboard shows live activity and any retry pressure without flooding

3. `smoke-rework`
   - Start from a `smoke-pr` PR in the team's review handoff state
   - Add at least one actionable PR review comment that requires a concrete repo change
   - Move the issue to `Rework`
   - Expected outcome: the agent addresses or explicitly replies to the feedback, reruns validation, updates the PR, and returns the issue to the team's review handoff state

4. `smoke-merge`
   - Start from a reviewed and approved `smoke-pr`
   - Move the issue to `Merging`
   - Expected outcome: the `land` flow merges the PR, moves the issue to `Done`, and cleans up the workspace

5. `parity-smoke`
   - Run the same seeded scenario once with Rust and once with Elixir
   - Compare external artifacts only:
   - Todoist section and completion transitions
   - workpad comment behavior
   - PR creation and metadata
   - review feedback handling
   - merge outcome
   - cleanup behavior

## Suggested Issue Template For `smoke-pr`

Use this structure in the Todoist task description:

```md
## Goal

Run a high-fidelity Symphony smoke test against the dedicated smoke repository.

## Requirements

- Update `SMOKE_TARGET.md` by appending one dated bullet under `## Change Log`
- Mention the issue identifier in the new bullet
- Do not modify any other repo file
- Run `sh scripts/validate-smoke-repo.sh`
- Commit, push, open a PR, and attach it to this issue

## Validation

- [ ] `sh scripts/validate-smoke-repo.sh`
- [ ] GitHub Actions `smoke-ci` passes on the PR
```

## Operational Notes

- Keep this repo disposable. Branch churn, PR churn, and squash merges are expected.
- Prefer bounded smoke issues with explicit acceptance criteria so failures are attributable.
- When a smoke run fails, capture the issue identifier, PR URL if one exists, and the relevant `log/symphony.log` slice before retrying.
- Capture `/api/v1/state` and at least one `/api/v1/stream` event payload during full observability parity runs so API, web, and terminal evidence line up.
- Treat one-off GitHub transport, DNS, or temporary `403` failures as transient smoke noise first; retry the failing operation, then try Symphony's host-side `github_api` tool when available, then the direct GitHub REST fallback before classifying the run as blocked.
- Prefer Symphony's host-side `github_api` tool not only for PR creation but also for post-publish metadata writes such as adding the `symphony` label, because in-session `gh auth token` can be unavailable even when host GitHub auth is healthy.
- Do not let a lower-privilege fallback interface redefine the primary GitHub path as permanently blocked. If `gh` has valid repo access and the failure is transient transport noise, leave the issue active and let the next continuation turn retry publish work.
- Once the required retry/fallback evidence is captured for a transient publish failure, prefer ending the current turn and letting Symphony schedule a continuation turn rather than burning tokens in prolonged same-turn reasoning.
