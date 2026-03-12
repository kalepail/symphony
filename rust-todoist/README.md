# Symphony Rust Todoist

This directory contains a Rust implementation of the Symphony service specification in [`../SPEC.md`](../SPEC.md).

This sibling runtime adapts the Rust orchestration model to Todoist-backed workflows.

It currently follows the same high-level operating model:

1. Poll Todoist for active work
2. Create or reuse one isolated workspace per task
3. Launch Codex in app-server mode inside that workspace
4. Render the repo-owned `WORKFLOW.md` prompt for the task
5. Keep running continuation turns until the task leaves an active state or the run hits `agent.max_turns`

## Trust And Safety Posture

This Rust port is intended for trusted environments.

- Codex is launched only inside sanitized per-task workspaces.
- Workspace paths are validated to stay under `workspace.root`.
- Command and file-change approvals are only auto-approved when `codex.approval_policy: never`.
- Tool `requestUserInput` prompts are answered non-interactively so unattended runs continue, while broader MCP elicitations and extra permission requests still fail hard.
- The structured `todoist` dynamic tool is exposed for `tracker.kind == "todoist"` and `tracker.kind == "memory"`.

If you need a stricter posture, tighten the Codex approval and sandbox settings in `WORKFLOW.md` and run Symphony inside an external sandbox.

## Features

- `WORKFLOW.md` loader with YAML front matter, content-hash reload detection, and last-known-good fallback
- Typed config layer with defaults, `$VAR` resolution, path normalization, and stricter validation for state limits and hook timeouts
- Todoist polling, pagination, task-state refresh, and open-project startup cleanup
- Optional deterministic `memory` tracker for local/system tests without live Todoist
- Workspace hooks: `after_create`, `before_run`, `after_run`, `before_remove`
- Codex app-server client over stdio with:
  - `initialize` / `initialized` / `thread/start` / `turn/start`
  - separate stdout protocol parsing and stderr diagnostics
  - command/file-change approval handling
  - structured `todoist` dynamic tool execution
  - token and rate-limit extraction
- Single-authority orchestrator with:
  - bounded concurrency
  - retry queue and exponential backoff
  - continuation retries after clean worker exit
  - reconciliation for stall detection and tracker state changes
- Live observability surfaces:
  - ANSI terminal dashboard for interactive operators
  - SSE-backed web dashboard with JSON API
  - `/`
  - `/api/v1/state`
  - `/api/v1/stream`
  - `/api/v1/<issue_identifier>`
  - `/api/v1/refresh`

## Run

```bash
cd rust-todoist
cargo run -- --i-understand-that-this-will-be-running-without-the-usual-guardrails ./WORKFLOW.md --port 3000
```

If no path is provided, Symphony uses `./WORKFLOW.md` from the current working directory.

Press `Ctrl+C` to stop the service.

Environment files:

- `rust-todoist/` now loads `.env` and `.env.local` from the workflow directory before parsing `WORKFLOW.md`.
- Existing shell environment variables still win over file-loaded values.
- `.env.local` overrides `.env` for keys that are not already exported in the shell.
- The ignored live E2E harness reads `SYMPHONY_RUN_LIVE_E2E`, `TODOIST_API_TOKEN`, and `SYMPHONY_SMOKE_PROJECT_ID` from the same files, so you do not need separate shell exports.
- Copy [`.env.example`](./.env.example) to `.env` for local setup.

## Configuration

Minimal example:

```md
---
tracker:
  kind: todoist
  api_key: $TODOIST_API_TOKEN
  project_id: your-project-id
workspace:
  root: ~/code/symphony-workspaces
observability:
  terminal_enabled: true
  refresh_ms: 1000
  render_interval_ms: 250
hooks:
  after_create: |
    git clone --depth 1 git@github.com:your-org/your-repo.git .
agent:
  max_concurrent_agents: 10
  max_turns: 20
codex:
  command: codex app-server
---

You are working on Todoist task {{ issue.identifier }}.

Title: {{ issue.title }}

{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}
```

Notes:

- The bundled [`WORKFLOW.md`](./WORKFLOW.md) now carries the stronger unattended workflow contract used by Symphony: explicit task-scoped workpad discipline, PR feedback sweeps, Todoist-native rework handling, and a completion bar before the canonical review handoff state `Human Review`.
- `tracker.kind` supports `todoist` for live runs and `memory` for deterministic local/system tests backed by a fixture file.
- `tracker.label` is an optional Todoist-native routing filter when one runtime should own only a subset of a shared project.
- If `tracker.assignee` is configured, the project must support assignment and any explicit assignee id must be a valid Todoist collaborator for that shared project.
- When `tracker.label` is configured, `todoist.create_task` automatically inherits that label so follow-up tasks stay inside the same runtime ownership boundary.
- Top-level `todoist.create_task` calls default into the project's `Todo` section when no `section_id` is supplied. Use `parent_id` only for true subtasks.
- Todoist comments must be available on the connected account or plan. Symphony validates that at startup because task-scoped workpad comments are part of the core runtime contract.
- `tracker.active_states` and `tracker.terminal_states` accept either YAML lists or comma-separated strings.
- `tracker.fixture_path` is used when `tracker.kind: memory` and may point to either JSON or YAML containing an issue array or `{ issues: [...] }` envelope.
- `observability.terminal_enabled` defaults to `true`, while terminal rendering only activates on interactive TTYs. `observability.refresh_ms` defaults to `1000` and `observability.render_interval_ms` defaults to `250`.
- `workspace.root` supports `~` and `$VAR`. Bare path names such as `workspaces` remain relative.
- `codex.command` is preserved as a shell command string and is launched via a POSIX shell (`bash -lc` when available, otherwise `sh -lc`).
- Prompt rendering uses strict template behavior. Unknown variables or filters fail the affected run attempt.
- The Rust implementation watches `WORKFLOW.md` and reloads the last good config without restart. Invalid reloads are logged and block new dispatches until fixed.
- `todoist` exposes structured actions such as `get_task`, `list_sections`, `list_comments`, `create_project_comment`, `delete_comment`, `get_workpad`, `upsert_workpad`, `delete_workpad`, `list_tasks`, `list_activities`, `update_task`, `move_task`, and `close_task`, and preserves Todoist HTTP error payloads in tool output so Codex can recover from validation failures.
  Task comment requests use `task_id`, while Todoist comment responses identify those same comments with `item_id`. The default workflow uses the dedicated workpad actions so the persistent `## Codex Workpad` stays on one task comment instead of drifting across multiple comments. `close_task` is guarded by Symphony and only succeeds from `Merging` after the linked GitHub PR is verified as merged.
- Startup now requires the same explicit acknowledgement flag as Elixir: `--i-understand-that-this-will-be-running-without-the-usual-guardrails`.
- Optional web observability can be enabled via CLI `--port` or `server.port` in `WORKFLOW.md`. `server.host` is also supported; the default bind host remains loopback (`127.0.0.1`). The dashboard now uses live SSE updates, keeps runtime clocks moving client-side, and falls back to `/api/v1/state` polling if the stream is unavailable. The terminal dashboard is enabled independently through `observability.terminal_enabled`.
- Logs now default to `./log/symphony.log` relative to the current working directory, with size-based rotation at 10 MB and retention for 5 archived files. Override the root with `--logs-root /path/to/root`, which writes to `/path/to/root/log/symphony.log`. Symphony's own lifecycle targets remain at `info` in the file log even when the surrounding shell uses a stricter `RUST_LOG` value such as `warn`.
- The sample `before_remove` hook first checks for `./scripts/workspace_before_remove.sh` in the target repo, then falls back to the bundled [`rust-todoist/scripts/workspace_before_remove.sh`](./scripts/workspace_before_remove.sh) path when the workspace itself is this repository. If you copy the workflow into another repo, either copy that script too or replace the hook with your own cleanup.
- [`rust-todoist/scripts/github_publish_preflight.sh`](./scripts/github_publish_preflight.sh) provides a fast operator preflight for both `gh` and direct GitHub REST access, repo visibility, PR listing, and required label presence before launching a live PR-oriented smoke run.
- When Symphony exposes the host-side `github_api` tool, prefer it for both PR creation and post-publish metadata writes such as applying the `symphony` label. This avoids the in-session `gh auth token` drift that can appear even when host GitHub auth is healthy.
- The smoke workflows are label-scoped on purpose: `symphony-smoke-minimal` and `symphony-smoke-full` let multiple Todoist runtimes share one smoke project without stealing each other's tasks.

## Operator Surface

Rust now covers both observability surfaces that mattered in Elixir and exceeds them on the web path:

| Surface | Elixir | Rust |
| --- | --- | --- |
| Terminal | ANSI status dashboard | ANSI status dashboard with shared event humanization |
| Web dashboard | LiveView | SSE live updates with polling fallback |
| JSON API | `/api/v1/*` | `/api/v1/*` plus `/api/v1/stream` |
| Runtime links | Project + dashboard URL | Project + dashboard URL |
| Live activity | Humanized Codex messages | Shared humanized Codex messages across terminal, web, and JSON |
| Throughput graph | Terminal only | Terminal and web |

### Captured Evidence: Terminal

```text
╭─ SYMPHONY STATUS
│ Agents: 1/10
│ Throughput: 18.2 tps
│ Runtime: 00:12
│ Tokens: in 11 | out 7 | total 18
│ Project: https://app.todoist.com/app/project/example-123
│ Dashboard: http://127.0.0.1:4000/
│ Next refresh: 5s
│ Graph: ▁▂▃▄▅▆▇█
├─ Running
```

### Captured Evidence: Web

```text
Todoist Operations Dashboard
- Live SSE status badge with polling fallback
- Running / Retrying / Total Tokens / Runtime cards
- Throughput card and 10-minute graph card
- Running sessions table with JSON details and Copy ID affordance
- Retry queue, rate-limit JSON, and polling JSON
```

## Canonical Todoist Workflow

For parity with the original Elixir implementation and the root [SPEC.md](../SPEC.md), the dedicated smoke workflow now uses this Todoist section layout:

- Visible columns: `Backlog`, `Todo`, `In Progress`, `Human Review`
- Hidden columns: `Rework`, `Merging`, `Done`, `Canceled`, `Duplicate`

`Human Review`, `Rework`, and `Merging` are first-class workflow states in the Todoist runtime. Smoke and production workflows should use those canonical names directly, and rework should preserve the surviving PR diff instead of restarting blindly from `origin/main`. `Human Review` is intentionally a handoff column rather than an active dispatch state, so Symphony resumes only after a human moves the task to `Rework` or `Merging`.

## Testing

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --test live_e2e -- --ignored --nocapture
sh -n scripts/workspace_before_remove.sh
sh -n scripts/github_publish_preflight.sh
```

## Live Smoke Workflows

Tracked live-smoke workflow files now live alongside the main workflow:

- [WORKFLOW.smoke.minimal.md](./WORKFLOW.smoke.minimal.md) exercises the smallest safe live path against the dedicated smoke repo.
- [WORKFLOW.smoke.full.md](./WORKFLOW.smoke.full.md) targets the full branch, PR, review, and merge contract against the same repo.
- [SMOKE_TESTS.md](./SMOKE_TESTS.md) documents the smoke matrix, required environment, expected dashboard evidence, and the dedicated repo `kalepail/symphony-smoke-lab`.
- [tests/live_e2e.rs](./tests/live_e2e.rs) is the env-gated real Todoist/Codex integration test modeled after Elixir’s live E2E harness.
