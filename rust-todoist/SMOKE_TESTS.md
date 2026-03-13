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
- For automated remote-worker live E2E, either set `SYMPHONY_LIVE_SSH_WORKER_HOSTS` to reachable SSH targets with the same repo/bootstrap prerequisites available on those hosts, or leave it unset and let the harness provision Docker-backed SSH workers using Codex auth from `SYMPHONY_LIVE_DOCKER_AUTH_JSON` or `~/.codex/auth.json`
- If `tracker.assignee` will be set, a Todoist project that supports assignment and exposes the intended assignee as a collaborator
- GitHub CLI authenticated with `repo` scope and `gh auth setup-git` already applied on the host
- For the direct GitHub REST fallback, either `GH_TOKEN` / `GITHUB_TOKEN` is exported or `gh auth token` succeeds on the host
- Prefer `gh auth login` or a classic PAT with `repo` scope over a fine-grained PAT. Symphony publish/merge flows read check runs and mutate PR metadata, and smoke helpers also update repository contents and refs.
- File logs are written to `log/symphony.log` or the chosen `--logs-root`; Symphony lifecycle lines stay visible there even if the shell exports `RUST_LOG=warn`

`SYMPHONY_SMOKE_PROJECT_ID` should point at a dedicated Todoist project reserved for Symphony smoke runs.
Use a dedicated Todoist user for smoke whenever possible. Todoist applies rate limits per user, so separate API tokens on the same account still contend for the same upstream budget.
Seed minimal smoke tasks with the label `symphony-smoke-minimal` and full/rework/merge smoke tasks with the label `symphony-smoke-full`.
Todoist comments must be available on the connected account or plan because the persistent Symphony workpad is a task-scoped comment and startup now validates that capability.

## Canonical Todoist Sections

For exact parity with the original Elixir workflow and [SPEC.md](../SPEC.md), the dedicated smoke Todoist setup now uses this section organization:

- Visible columns: `Backlog`, `Todo`, `In Progress`, `Human Review`
- Hidden columns: `Rework`, `Merging`, `Canceled`, `Duplicate`

There is no `Done` section in the canonical Todoist board. A completed task disappears from the
active board and remains associated with the section it was completed from when operators choose to
show completed tasks.

Smoke parity runs and production workflows should use the canonical names above directly.
`Human Review` is a human handoff column, not an active dispatch state. Symphony resumes only after a human moves the task to `Rework` or `Merging`.
For automated smoke and live E2E runs, the Rust repo-owned harness simulates that human approval externally after verifying the PR is green and review-ready, then moves the task to `Merging`. Production runs still require a real human to perform that transition.
For workpad validation, treat task-scoped comments and their `item_id` field as the canonical Todoist comment surface for a task. Full workflow runs should reach `Human Review` with exactly one persistent task-scoped workpad comment marked by both `## Codex Workpad` and `<!-- symphony:workpad -->`.

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
  - Includes dedicated local and SSH-worker variants for the lightweight disposable Todoist handoff smoke, the repo-backed minimal smoke workflow, and the full parity smoke that exercises PR creation, automated `Human Review` approval, `Merging`, verified merge, and guarded `todoist.close_task`.
- Shared-state cleanup helper: [../scripts/reset_smoke_state.py](../scripts/reset_smoke_state.py)

## Observability Evidence

Each live smoke should capture both operator surfaces while the run is active:

- Terminal dashboard evidence:
  - header with agents/max, runtime, tokens, project URL, and next refresh
  - distinct `Codex Limits` and `Todoist Budget` lines when either payload has been observed
  - running or backoff row for the active smoke issue
- Web dashboard evidence:
  - live status badge connected to `/api/v1/stream`
  - runtime and throughput cards
  - running session row with JSON details link
  - separate `Codex Rate Limits` and `Todoist Budget` JSON panes
- When the run is remote, also capture the worker host and remote workspace location shown in the running session row and issue detail view

If the stream degrades, also capture the polling-fallback badge state. For one unavailable-path check, capture the terminal offline frame and the web fallback/offline badge behavior.

## Smoke Matrix

1. `smoke-minimal`
   - Workflow: `WORKFLOW.smoke.minimal.md`
   - Seed task label: `symphony-smoke-minimal`
   - Automated coverage: `completes_a_repo_backed_minimal_smoke_workflow_end_to_end` and `completes_a_repo_backed_minimal_smoke_workflow_end_to_end_with_ssh_worker` in `tests/live_e2e.rs`
   - Proves: live Todoist polling, workspace bootstrap, Codex turn execution, repo mutation, validation command execution, single-workpad comment discipline, a bounded non-terminal state transition back to `Backlog`, and terminal/web observability during a live run
   - Expected repo effect: one appended bullet in `SMOKE_TARGET.md`

2. `smoke-pr`
   - Workflow: `WORKFLOW.smoke.full.md`
   - Seed the issue in `Todo`
   - Seed task label: `symphony-smoke-full`
   - Automated coverage: the first half of `completes_a_full_smoke_repo_workflow_end_to_end` and `completes_a_full_smoke_repo_workflow_end_to_end_with_ssh_worker`
   - Task body should instruct the agent to update `SMOKE_TARGET.md`, run `sh scripts/validate-smoke-repo.sh`, commit, push, open a PR, label it `symphony`, and attach the PR details to the Todoist task
   - Expected outcome: issue reaches `Human Review` with a green PR
   - Expected observability: SSE dashboard stays live during publish; terminal dashboard shows live activity and any retry pressure without flooding

3. `smoke-rework`
    - Start from a `smoke-pr` PR in the team's review handoff state
    - Add at least one actionable PR review comment that requires a concrete repo change
    - Move the issue to `Rework`
    - Automation status: workflow contract is implemented in `WORKFLOW.smoke.full.md`, but `tests/live_e2e.rs` does not yet auto-drive reviewer feedback through a full rework cycle
    - Expected outcome: the agent addresses or explicitly replies to the feedback, reruns validation, updates the PR, and returns the issue to the team's review handoff state

4. `smoke-merge`
   - Start from a reviewed and approved `smoke-pr`
   - Move the issue to `Merging`
   - Automated coverage: the second half of `completes_a_full_smoke_repo_workflow_end_to_end` and `completes_a_full_smoke_repo_workflow_end_to_end_with_ssh_worker`
   - Expected outcome: the `land` flow merges the PR, guarded `todoist.close_task` completes the task, and the workspace is cleaned up

5. `parity-smoke`
   - Run equivalent seeded scenarios once with Rust Todoist and once with the Elixir Linear service
   - Automation status: manual artifact comparison only; Elixir's live harness is still Linear-native, so there is no cross-runtime Todoist automation target
   - Compare external artifacts only:
   - Todoist section and completion transitions
   - workpad comment behavior
   - PR creation and metadata
   - review feedback handling
   - merge outcome
   - cleanup behavior, including workspace removal and disposable smoke-branch pruning

6. `smoke-remote-worker`
   - Run `WORKFLOW.smoke.full.md` or an equivalent bounded scenario with `worker.ssh_hosts` configured
   - The repo-owned `tests/live_e2e.rs` harness now includes dedicated ignored SSH tests for the minimal handoff flow, the repo-backed minimal smoke flow, and the full parity smoke flow
   - Expected outcome: dispatch lands on one remote worker, observability shows the selected `worker_host`, retries preserve host affinity when they occur, and remote workspace cleanup removes the remote directory after completion
   - Expected observability: `/api/v1/state`, `/api/v1/<issue_identifier>`, and the dashboard running row all show the same worker host and logical remote workspace path

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
- Reset the shared smoke environment before a fresh smoke round with `python3 ../scripts/reset_smoke_state.py`.
  - This restores the smoke repo baseline, deletes disposable smoke branches, removes shared smoke tasks, and deletes disposable Rust Todoist live-E2E projects left behind by aborted runs while skipping any project IDs currently registered as active smoke runs.
- Prefer bounded smoke issues with explicit acceptance criteria so failures are attributable.
- When a smoke run fails, capture the issue identifier, PR URL if one exists, and the relevant `log/symphony.log` slice before retrying.
- Capture `/api/v1/state` and at least one `/api/v1/stream` event payload during full observability parity runs so API, web, and terminal evidence line up.
- For remote-worker smoke, capture the selected worker host in the dashboard plus the corresponding remote workspace path before cleanup and confirm that the directory is gone after the run finishes.
- Treat one-off GitHub transport, DNS, or temporary `403` failures as transient smoke noise first; retry the failing operation, then try Symphony's host-side `github_api` tool when available, then the direct GitHub REST fallback before classifying the run as blocked.
- Prefer Symphony's host-side `github_api` tool not only for PR creation but also for post-publish metadata writes such as adding the `symphony` label, because in-session `gh auth token` can be unavailable even when host GitHub auth is healthy.
- Do not let a lower-privilege fallback interface redefine the primary GitHub path as permanently blocked. If `gh` has valid repo access and the failure is transient transport noise, leave the issue active and let the next continuation turn retry publish work.
- Once the required retry/fallback evidence is captured for a transient publish failure, prefer ending the current turn and letting Symphony schedule a continuation turn rather than burning tokens in prolonged same-turn reasoning.
