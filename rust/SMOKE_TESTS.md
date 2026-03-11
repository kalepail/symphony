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

- `LINEAR_API_KEY`
- `SYMPHONY_WORKSPACE_ROOT`
- `SYMPHONY_SMOKE_PROJECT_SLUG`
- GitHub CLI authenticated with `repo` scope and `gh auth setup-git` already applied on the host
- For the direct GitHub REST fallback, either `GH_TOKEN` / `GITHUB_TOKEN` is exported or `gh auth token` succeeds on the host
- File logs are written to `log/symphony.log` or the chosen `--logs-root`; Symphony lifecycle lines stay visible there even if the shell exports `RUST_LOG=warn`

`SYMPHONY_SMOKE_PROJECT_SLUG` should point at a dedicated Linear project reserved for Symphony smoke runs.

## Canonical Linear Columns

For exact parity with the original Elixir workflow and [SPEC.md](/Users/kalepail/Desktop/symphony/SPEC.md), the dedicated smoke Linear setup now uses this board organization:

- Visible columns: `Backlog`, `Todo`, `In Progress`, `Human Review`
- Hidden columns: `Rework`, `Merging`, `Done`, `Canceled`, `Duplicate`

Compatibility substitutes such as `In Review` remain supported for non-smoke migrations, but smoke parity runs should use the canonical names above.

## Operator Preflight

Before any PR-oriented live smoke (`smoke-pr`, `smoke-rework`, `smoke-merge`, or `parity-smoke`), confirm the local GitHub CLI can see the smoke repo and required label:

```bash
sh scripts/github_publish_preflight.sh --repo kalepail/symphony-smoke-lab --label symphony
```

This verifies both the `gh` path and the direct GitHub REST fallback against the target repo and required label before the live run starts.

## Workflow Files

- Minimal live smoke: [WORKFLOW.smoke.minimal.md](./WORKFLOW.smoke.minimal.md)
- Full live smoke: [WORKFLOW.smoke.full.md](./WORKFLOW.smoke.full.md)

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
   - Proves: live Linear polling, workspace bootstrap, Codex turn execution, repo mutation, validation command execution, `linear_graphql` comment, `Done` transition, workspace cleanup, terminal/web observability during a live run
   - Expected repo effect: one appended bullet in `SMOKE_TARGET.md`

2. `smoke-pr`
   - Workflow: `WORKFLOW.smoke.full.md`
   - Seed the issue in `Todo`
   - Issue body should instruct the agent to update `SMOKE_TARGET.md`, run `sh scripts/validate-smoke-repo.sh`, commit, push, open a PR, label it `symphony`, and attach the PR to the Linear issue
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
   - Linear state transitions
   - workpad comment behavior
   - PR creation and metadata
   - review feedback handling
   - merge outcome
   - cleanup behavior

## Suggested Issue Template For `smoke-pr`

Use this structure in the Linear issue body:

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
