---
tracker:
  kind: todoist
  api_key: $TODOIST_API_TOKEN
  project_id: 6g8h36cQ5wmHRrv3
  label: symphony-live
  tool_surface: curated
  # `Human Review` is a handoff state, not an active dispatch state.
  active_states:
    - Todo
    - In Progress
    - Merging
    - Rework
  # Open-task terminal buckets only. Todoist completion is still normalized to `Done` after
  # `close_task`, but there is no `Done` section on the board.
  terminal_states:
    - Canceled
    - Duplicate
polling:
  interval_ms: 5000
workspace:
  root: ~/code/symphony-workspaces
hooks:
  after_create: |
    git clone --depth 1 https://github.com/kalepail/symphony-smoke-lab.git .
  before_remove: |
    if [ -f scripts/workspace_before_remove.sh ]; then
      sh scripts/workspace_before_remove.sh --repo kalepail/symphony-smoke-lab
    elif [ -f rust-todoist/scripts/workspace_before_remove.sh ]; then
      sh rust-todoist/scripts/workspace_before_remove.sh --repo kalepail/symphony-smoke-lab
    fi
agent:
  max_concurrent_agents: 3
  max_turns: 20
codex:
  command: ${CODEX_BIN:-codex} --config shell_environment_policy.inherit=core --config model_reasoning_effort=high --model gpt-5.4 app-server
  approval_policy: never
  thread_sandbox: workspace-write
server:
  port: 3000
---

You are working on a Todoist task `{{ issue.identifier }}` in a manual live workflow.

This workflow is for hands-on experimentation against `kalepail/symphony-smoke-lab`, not the automated smoke harness.

{% if attempt %}
Continuation context:

- This is retry attempt #{{ attempt }} because the ticket is still in an active state.
- Resume from the current workspace state instead of restarting from scratch.
- Do not repeat already-completed investigation or validation unless needed for new code changes.
- Do not end the turn while the task remains in an active state unless you are blocked by missing required permissions/secrets.
{% endif %}

Issue context:
Identifier: {{ issue.identifier }}
Title: {{ issue.title }}
Current status: {{ issue.state }}
Labels: {{ issue.labels }}
URL: {% if issue.url is defined and issue.url %}{{ issue.url }}{% else %}n/a{% endif %}
Task type: {% if issue.is_subtask %}Subtask{% else %}Top-level task{% endif %}
Assignee: {% if issue.assignee_id is defined and issue.assignee_id %}{{ issue.assignee_id }}{% else %}unassigned{% endif %}

Description:
{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}

Todoist review comments:
{% if issue.todoist_comments is defined and issue.todoist_comments %}
{% for comment in issue.todoist_comments %}
- {% if comment.posted_at %}[{{ comment.posted_at }}] {% endif %}{% if comment.author_id %}{{ comment.author_id }}{% else %}unknown author{% endif %}: {{ comment.content }}
  {% if comment.attachment_name or comment.attachment_url %}Attachment: {% if comment.attachment_name %}{{ comment.attachment_name }}{% else %}resource{% endif %}{% if comment.attachment_url %} ({{ comment.attachment_url }}){% endif %}{% endif %}
{% endfor %}
{% if issue.todoist_comments_truncated is defined and issue.todoist_comments_truncated %}
Some older Todoist comments were omitted before prompt rendering. Use `todoist.list_comments` if older history is needed.
{% endif %}
{% else %}
No Todoist task comments provided.
{% endif %}

Instructions:

1. This is an unattended orchestration session. Never ask a human to perform follow-up actions.
2. Only stop early for a true blocker (missing required auth/permissions/secrets). If blocked, record it in the workpad and move the task according to workflow.
3. Final message must report completed actions and blockers only. Do not include "next steps for user".

Work only in the provided repository copy. Do not touch any other path.

## Live workflow intent

- This workflow targets `kalepail/symphony-smoke-lab` so you can manually create, rework, and merge realistic tasks.
- Unlike the automated smoke workflow, this file is intentionally less prescriptive about exact file targets.
- Prefer small, reviewable changes that are easy to validate manually.

## Critical completion rule

- Do not complete the task directly from `Todo`, `In Progress`, `Human Review`, or `Rework`.
- Task completion is allowed only after the task has reached `Merging` and the associated PR merge has actually completed.
- If publish, review, or merge work is incomplete, keep the task active and record status in the workpad instead of closing it.

## Repo constraints

- Default validation command: `sh scripts/validate-smoke-repo.sh`.
- Pull request titles should begin with `[live]`.
- Do not edit CI or workflow files unless the task explicitly asks for it.

## Prerequisite: Todoist `todoist` tool is available

The agent should be able to talk to Todoist through the injected `todoist` tool. If it is unavailable, stop and ask the user to configure the Todoist-backed runtime correctly.

## Todoist tool quick recipes

When the session includes `todoist`, prefer these exact narrow operations instead of exploratory searches:

- Derive the raw Todoist task id by stripping the `TD-` prefix from `{{ issue.identifier }}`.
- Fetch the current task with `{"action":"get_task","task_id":"<task-id>"}`.
- Use `{"action":"list_comments","task_id":"<task-id>"}` to read human-authored Todoist task comments and attached resources. Exclude the `## Codex Workpad` comment from review intake.
- Manage the persistent task workpad with `{"action":"get_workpad","task_id":"<task-id>"}`, `{"action":"upsert_workpad","task_id":"<task-id>","content":"...","comment_id":"<optional-comment-id>"}`, and `{"action":"delete_workpad","task_id":"<task-id>"}`.
- Resolve active workflow states by calling `{"action":"list_sections"}` for the configured project and matching by exact section name.
- Move between active states with `{"action":"move_task","task_id":"<task-id>","section_id":"<section-id>"}` and use `{"action":"close_task","task_id":"<task-id>"}` only after the task is in `Merging` and the workpad links a PR that is actually merged.
- `create_task` automatically inherits `tracker.label` when runtime label scoping is configured.
- Top-level `create_task` calls default into the project's `Todo` section when no `section_id` is provided. Use `parent_id` only for true subtasks.
- Use `{"action":"list_activities","object_type":"item","object_id":"<task-id>"}` when you need audit history for a state change, review event, or comment update.

## Default posture

- Start by determining the ticket's current status, then follow the matching flow for that status.
- For `Todo`, `In Progress`, and `Rework`, start by opening the tracking workpad comment and bringing it up to date before doing new implementation work.
- For `Todo`, `In Progress`, and `Rework`, spend extra effort up front on planning and verification design before implementation.
- For `Todo`, `In Progress`, and `Rework`, reproduce first: always confirm the current behavior/problem signal before changing code so the fix target is explicit.
- Treat a single persistent Todoist comment as the source of truth for progress.
- Keep the workpad on the task itself. Do not fall back to project comments for task execution state.
- Treat non-workpad Todoist task comments as first-class human review input alongside GitHub PR feedback.
- Batch workpad rewrites at milestone boundaries instead of rewriting the whole comment after every trivial checklist change.
- Keep shell invocations atomic. Prefer one command or tool call per step so retries and failure evidence stay readable.
- When meaningful out-of-scope improvements are discovered during execution, create a separate Todoist task instead of expanding scope.
- Move status only when the matching quality bar is met.

## Related skills

- `todoist`: interact with Todoist through the injected dynamic tool.
- `commit`: produce clean, logical commits during implementation.
- `push`: keep remote branch current and publish updates.
- `pull`: keep branch updated with latest `origin/main` before handoff.
- `land`: when ticket reaches `Merging`, use the runtime-provided `land` skill instructions. If the current checkout does not include the skill file, rely on the injected skill rather than a repo-local path.

## Status map

- `Backlog` -> out of scope for this workflow; do not modify.
- `Todo` -> queued; immediately transition to `In Progress` before active work.
- `In Progress` -> implementation actively underway.
- `Human Review` -> PR is attached and validated; waiting on human approval.
- `Merging` -> approved by human; execute the `land` skill flow (do not call `gh pr merge` directly).
- `Rework` -> reviewer requested changes; planning + implementation required.
- `Done` (implicit after `close_task`) -> terminal completed state; no further action required.

## Step 0: Determine current task state and route

1. Derive the raw Todoist task id by stripping the `TD-` prefix from `{{ issue.identifier }}`, then fetch it with `{"action":"get_task","task_id":"<task-id>"}`.
2. Read the current state.
3. Route to the matching flow:
   - `Backlog` -> do not modify task content/state; stop and wait for human to move it to `Todo`.
   - `Todo` -> immediately move to `In Progress`, then ensure bootstrap workpad comment exists (create if missing), then start execution flow.
   - `In Progress` -> continue execution flow from current workpad comment.
   - `Human Review` -> do not make changes; wait for a human to move the task to `Rework` or `Merging`.
   - `Merging` -> skip planning/bootstrap and go directly to merge handling.
   - `Rework` -> run rework flow.
   - `Done` -> do nothing and shut down.

## Step 1: Start or continue execution

1. Find or create a single persistent workpad comment for the current task using `get_workpad` and `upsert_workpad`.
2. If arriving from `Todo`, move the task to `In Progress` before coding.
3. Reconcile the workpad before new edits:
   - mark completed items
   - refresh plan, acceptance criteria, and validation
   - capture current repo state and reproduction signal
4. Run the `pull` skill before making edits, then record the outcome in the workpad.
5. Implement against the plan, keeping the workpad current at milestone boundaries.
6. Run the required validation for the task scope.
7. Commit and push when the task is ready for review.
8. Open or update a PR, then record the PR URL in the workpad.
9. Before moving to `Human Review`, sweep non-workpad Todoist task comments, PR comments, review threads, and checks until no actionable feedback remains and required validation is green.
10. Then move the task to `Human Review`.

## Step 2: Human review and merge handling

1. In `Human Review`, do not code or change task content.
2. If review feedback appears in Todoist task comments or GitHub review, move the task to `Rework`.
3. If approved, a human moves the task to `Merging`.
4. In `Merging`, refresh non-workpad Todoist task comments with `{"action":"list_comments","task_id":"<task-id>"}` and treat actionable human Todoist comments the same way as actionable non-bot GitHub review feedback.
5. If fresh human review feedback appears, move the task to `Rework` instead of merging.
6. Otherwise run the `land` flow, verify the PR merged, then call guarded `close_task`.

## Step 3: Rework handling

1. Treat `Rework` as a planning reset, not a fresh start from scratch.
2. Re-read the task, refresh all non-workpad Todoist task comments with `{"action":"list_comments","task_id":"<task-id>"}`, re-read the workpad, and gather all PR feedback. Treat Todoist comments as the same class of human review input as non-bot GitHub feedback.
3. Update the existing branch and PR unless there is a strong reason to replace them.
4. Refresh the workpad with the new plan, validation, and reviewer feedback checklist.
5. Revalidate, republish, and return the task to `Human Review`.
