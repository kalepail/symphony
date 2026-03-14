# Scripts

This directory intentionally mixes two kinds of checked-in assets:

- Repo-wide tooling used from the main Symphony repository.
- Shared smoke-harness assets used to manage the dedicated smoke repo and live test state.

It is not a language-specific directory.

## Ownership

- [`check_pr_body.py`](./check_pr_body.py) is repo-wide CI/tooling. GitHub Actions uses it to validate pull request descriptions against the repository PR template.
- [`reset_smoke_state.py`](./reset_smoke_state.py) is shared smoke reset tooling. It restores the dedicated smoke repo baseline and cleans up disposable smoke state used by live runs.
- [`smoke_repo_baseline/`](./smoke_repo_baseline) is a checked-in fixture that represents the root of the external smoke repo `kalepail/symphony-smoke-lab`.

## Placement Rules

- Put repo-wide CI or tooling helpers here.
- Put shared smoke-harness helpers and fixtures here.
- Put runtime-specific helpers in the runtime's own `scripts/` directory instead.

For example, [`rust-todoist/scripts/`](../rust-todoist/scripts) is the Rust Todoist runtime-local script directory, while this directory remains the shared root for repo-wide and smoke-harness assets.

## About `smoke_repo_baseline/AGENTS.md`

[`smoke_repo_baseline/AGENTS.md`](./smoke_repo_baseline/AGENTS.md) belongs to the smoke-repo fixture itself. It is not an additional policy layer for the main Symphony repository; it exists because the fixture models another repo's root layout.
