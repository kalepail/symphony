---
# Edit these values for your local live run.
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY
  project_slug: "ec9cafa08222" # Change to your Linear project slug.
  active_states:
    - Todo
    - In Progress
    - Merging
    - Rework
  terminal_states:
    - Closed
    - Cancelled
    - Canceled
    - Duplicate
    - Done
polling:
  interval_ms: 5000
workspace:
  root: /Users/kalepail/tmp/elixir-symphony-workspaces # Change if you want a different workspace root.
hooks:
  after_create: |
    git clone --depth 1 https://github.com/kalepail/symphony-smoke-lab.git . # Change repo URL if needed.
  before_remove: |
    cd /Users/kalepail/Desktop/symphony/elixir && mise exec -- mix workspace.before_remove --repo kalepail/symphony-smoke-lab # Change owner/repo if needed.
agent:
  max_concurrent_agents: 10
  max_turns: 20
codex:
  command: codex --config shell_environment_policy.inherit=all --config model_reasoning_effort=medium --model gpt-5.4 app-server
  approval_policy: never
  thread_sandbox: workspace-write
  turn_sandbox_policy:
    type: workspaceWrite
server:
  port: 3001 # Change if you want the dashboard on a different port.
---

You are working on a Linear ticket `{{ issue.identifier }}`

{% if attempt %}
Continuation context:

- This is retry attempt #{{ attempt }} because the ticket is still in an active state.
- Resume from the current workspace state instead of restarting from scratch.
- Do not repeat already-completed investigation or validation unless needed for new code changes.
- Do not end the turn while the issue remains in an active state unless you are blocked by missing required permissions/secrets.
{% endif %}

Issue context:
Identifier: {{ issue.identifier }}
Title: {{ issue.title }}
Current status: {{ issue.state }}
Labels: {{ issue.labels }}
URL: {{ issue.url }}

Description:
{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}

Instructions:

1. This is an unattended orchestration session. Never ask a human to perform follow-up actions.
2. Only stop early for a true blocker (missing required auth/permissions/secrets). If blocked, record it in the workpad and move the issue according to workflow.
3. Final message must report completed actions and blockers only. Do not include "next steps for user".

Work only in the provided repository copy. Do not touch any other path.

## Live workflow intent

- This workflow targets `kalepail/symphony-smoke-lab`.
- Default mutation target: `SMOKE_TARGET.md`.
- Review or rework target when explicitly requested: `smoke/review-target.md`.
- Default validation command for repo mutations: `sh scripts/validate-smoke-repo.sh`.
- Pull request titles should begin with `[live]`.
- Do not edit CI or workflow files in the smoke repo unless the issue explicitly asks for that path.

## Prerequisite: Linear MCP or `linear_graphql` tool is available

The agent should be able to talk to Linear, either via a configured Linear MCP server or injected `linear_graphql` tool. If none are present, stop and ask the user to configure Linear.

## Default posture

- Start by determining the ticket's current status, then follow the matching flow for that status.
- Start every task by opening the tracking workpad comment and bringing it up to date before doing new implementation work.
- Reproduce first: always confirm the current behavior or issue signal before changing code so the fix target is explicit.
- Treat a single persistent Linear comment as the source of truth for progress.
- Keep ticket metadata current.
- Move status only when the matching quality bar is met.
- Operate autonomously end-to-end unless blocked by missing requirements, secrets, or permissions.

## Related skills

- `linear`: interact with Linear.
- `commit`: produce clean, logical commits during implementation.
- `push`: keep remote branch current and publish updates.
- `pull`: keep branch updated with latest `origin/main` before handoff.
- `land`: when ticket reaches `Merging`, use the runtime-provided `land` skill instructions.

## Status map

- `Backlog` -> out of scope for this workflow; do not modify.
- `Todo` -> queued; immediately transition to `In Progress` before active work.
- `In Progress` -> implementation actively underway.
- `Human Review` -> PR is attached and validated; waiting on human approval.
- `Merging` -> approved by human; execute the `land` skill flow.
- `Rework` -> reviewer requested changes; planning plus implementation required.
- `Done` -> terminal state; no further action required.
