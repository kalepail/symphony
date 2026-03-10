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

`SYMPHONY_SMOKE_PROJECT_SLUG` should point at a dedicated Linear project reserved for Symphony smoke runs.

## Workflow Files

- Minimal live smoke: [WORKFLOW.smoke.minimal.md](./WORKFLOW.smoke.minimal.md)
- Full live smoke: [WORKFLOW.smoke.full.md](./WORKFLOW.smoke.full.md)

## Smoke Matrix

1. `smoke-minimal`
   - Workflow: `WORKFLOW.smoke.minimal.md`
   - Proves: live Linear polling, workspace bootstrap, Codex turn execution, repo mutation, validation command execution, `linear_graphql` comment, `Done` transition, workspace cleanup
   - Expected repo effect: one appended bullet in `SMOKE_TARGET.md`

2. `smoke-pr`
   - Workflow: `WORKFLOW.smoke.full.md`
   - Seed the issue in `Todo`
   - Issue body should instruct the agent to update `SMOKE_TARGET.md`, run `sh scripts/validate-smoke-repo.sh`, commit, push, open a PR, label it `symphony`, and attach the PR to the Linear issue
   - Expected outcome: issue reaches `Human Review` with a green PR

3. `smoke-rework`
   - Start from a `smoke-pr` PR in `Human Review`
   - Add at least one actionable PR review comment that requires a concrete repo change
   - Move the issue to `Rework`
   - Expected outcome: the agent addresses or explicitly replies to the feedback, reruns validation, updates the PR, and returns the issue to `Human Review`

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
