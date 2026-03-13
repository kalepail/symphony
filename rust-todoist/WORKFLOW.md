---
tracker:
  kind: todoist
  api_key: $TODOIST_API_TOKEN
  # Set to a real Todoist project id before running.
  project_id: $SYMPHONY_TODOIST_PROJECT_ID
  # Optional: set `label` when one runtime should own only part of a shared project.
  # label: symphony-runtime
  # Optional: leave unset for the common personal-project case.
  # Only set this when the Todoist project is shared and assignment-based routing is intentional.
  # assignee: me
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
  root: $SYMPHONY_WORKSPACE_ROOT
hooks:
  after_create: |
    git clone --depth 1 ${SOURCE_REPO_URL:-git@github.com:your-org/your-repo.git} .
  before_remove: |
    if [ -f scripts/workspace_before_remove.sh ]; then
      sh scripts/workspace_before_remove.sh
    elif [ -f rust-todoist/scripts/workspace_before_remove.sh ]; then
      sh rust-todoist/scripts/workspace_before_remove.sh
    fi
agent:
  max_concurrent_agents: 10
  max_turns: 20
codex:
  command: ${CODEX_BIN:-codex} --config shell_environment_policy.inherit=core --config model_reasoning_effort=high --model gpt-5.4 app-server
  approval_policy: never
  thread_sandbox: workspace-write
server:
  port: 3000
---

You are working on a Todoist task `{{ issue.identifier }}`

This file is a reusable template, not a repo-specific smoke workflow.

Before using it for a live project, make sure:

- `tracker.project_id` points at the intended Todoist project.
- `hooks.after_create` clones the intended repository.
- the Todoist project contains the canonical sections needed by your workflow.
- any repo-specific validation commands, publish rules, and cleanup logic are added below.

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
Due: {% if issue.due is defined and issue.due %}{{ issue.due }}{% else %}none{% endif %}
Deadline: {% if issue.deadline is defined and issue.deadline %}{{ issue.deadline }}{% else %}none{% endif %}

Description:
{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}

Todoist review comments:
{% if issue.todoist_comments is defined and issue.todoist_comments %}
{% for comment in issue.todoist_comments %}
- {% if comment.posted_at %}[{{ comment.posted_at }}] {% endif %}{% if comment.author_id %}{{ comment.author_id }}{% else %}unknown author{% endif %}
  {{ comment.content }}
  {% if comment.attachment_name or comment.attachment_url %}Attachment: {% if comment.attachment_name %}{{ comment.attachment_name }}{% else %}resource{% endif %}{% if comment.attachment_url %} ({{ comment.attachment_url }}){% endif %}{% endif %}
{% endfor %}
{% if issue.todoist_comments_truncated is defined and issue.todoist_comments_truncated %}
Some older or oversized Todoist comments were compacted before prompt rendering. Use `todoist.list_comments` if full history is needed.
{% endif %}
{% else %}
No Todoist task comments provided.
{% endif %}

Instructions:

1. This is an unattended orchestration session. Never ask a human to perform follow-up actions.
2. Only stop early for a true blocker (missing required auth/permissions/secrets). If blocked, record it in the workpad and move the task according to workflow.
3. Final message must report completed actions and blockers only. Do not include "next steps for user".

Work only in the provided repository copy. Do not touch any other path.

## Critical completion rule

- Do not complete the task directly from `Todo`, `In Progress`, `Human Review`, or `Rework`.
- Task completion is allowed only after the task has reached `Merging` and the associated PR merge has actually completed.
- If publish, review, or merge work is incomplete, keep the task active and record status in the workpad instead of closing it.

## Prerequisite: Todoist `todoist` tool is available

The agent should be able to talk to Todoist through the injected `todoist` tool. If it is unavailable, stop and ask the user to configure the Todoist-backed runtime correctly.

## Todoist tool quick recipes

When the session includes `todoist`, prefer these exact narrow operations instead of exploratory searches:

- Fetch the current task directly with `{"action":"get_task","task_id":"<task-id>"}`.
- Use `list_comments` to read human-authored task comments and attached resources on the Todoist task itself. Treat the `## Codex Workpad` comment as agent-owned and exclude it from human review intake.
- Manage the persistent task workpad with `get_workpad`, `upsert_workpad`, and `delete_workpad`. When `get_workpad` returns a `comment_id`, pass that hint back into later `upsert_workpad` calls.
- Use `create_project_comment` only for true project-level comments. Agent-owned task comments must use the single workpad via `upsert_workpad`.
- Todoist comment responses identify task comments with `item_id`.
- Resolve workflow state by listing sections for the configured project and matching by section name.
- Move the task between active states with `move_task` and `section_id`, then use `close_task` only after the task is in `Merging` and the workpad links a PR that is actually merged.
- `create_task` automatically inherits `tracker.label` when runtime label scoping is configured.
- Top-level `create_task` calls default into the project's `Todo` section when no `section_id` is provided. Use `parent_id` only for true subtasks.
- Use `list_tasks` with `parent_id` when the current task already has subtasks or you need to inspect child work.
- Use `list_activities` when task history, reviewer timing, or comment provenance is unclear.
- Keep each tool call to a single operation and avoid broad listing calls unless these direct recipes fail.

## Default posture

- Start by determining the ticket's current status, then follow the matching flow for that status.
- For `Todo`, `In Progress`, and `Rework`, start by opening the tracking workpad comment and bringing it up to date before doing new implementation work.
- For `Todo`, `In Progress`, and `Rework`, spend extra effort up front on planning and verification design before implementation.
- For `Todo`, `In Progress`, and `Rework`, reproduce first: always confirm the current behavior/problem signal before changing code so the fix target is explicit.
- Keep ticket metadata current (state, checklist, acceptance criteria, links).
- Treat a single persistent Todoist comment as the source of truth for progress.
- Use that single workpad comment for all progress and handoff notes; do not post separate "done"/summary comments.
- After the workpad exists, keep it current with `upsert_workpad`; do not use `create_project_comment` or raw `update_comment` for workpad writes.
- Persist the workpad `comment_id` returned by `get_workpad`/`upsert_workpad` and pass it back into later `upsert_workpad` calls so Todoist rewrites stay single-request.
- Keep the workpad on the task itself. Do not fall back to project comments for task execution state.
- Treat non-workpad Todoist task comments as first-class human review input. They may contain general instructions, resources, or follow-up direction that must be carried into planning, validation, and rework just like GitHub PR feedback.
- For `Merging`, skip the generic execution bootstrap. Do not start with repo-wide planning, reproduction, pull-sync, or routine workpad reconciliation. Treat `Merging` as a fast path: identify the surviving PR, verify it is merge-ready, run the `land` flow, then guarded `close_task`.
- Treat any ticket-authored `Validation`, `Test Plan`, or `Testing` section as non-negotiable acceptance input: mirror it in the workpad and execute it before considering the work complete.
- When meaningful out-of-scope improvements are discovered during execution,
  create a separate Todoist task instead of expanding scope. The follow-up task
  must include a clear title, description, and acceptance criteria, be placed in
  the same project as the current task, and reference the current task in the
  description because Todoist does not provide native related/blocker links.
- Use subtasks only when the work is truly a child deliverable of the current task. Use a new top-level task for independent follow-up work.
- Runtime-scoped follow-up tasks should rely on the tracker defaults for project, label, and Todo-lane placement unless the workflow explicitly needs a different target.
- Move status only when the matching quality bar is met.
- Keep shell invocations atomic. Prefer one command or tool call per step so retries and failure evidence stay readable.
- If Todoist comment writes start failing or rate limiting, batch workpad edits and avoid rewriting the whole comment for checklist-only churn.
- Operate autonomously end-to-end unless blocked by missing requirements, secrets, or permissions.
- Use the blocked-access escape hatch only for true external blockers (missing required tools/auth) after exhausting documented fallbacks.

## Related skills

- `todoist`: interact with Todoist through the injected dynamic tool.
- `commit`: produce clean, logical commits during implementation.
- `push`: keep remote branch current and publish updates.
- `pull`: keep branch updated with latest `origin/main` before handoff.
- `land`: when ticket reaches `Merging`, use the runtime-provided `land` skill instructions. If the current checkout does not include the skill file, rely on the injected skill rather than a repo-local path.

## Status map

- `Backlog` -> out of scope for this workflow; do not modify.
- `Todo` -> queued; immediately transition to `In Progress` before active work.
  - Special case: if a PR is already attached, treat as feedback/rework loop (run full PR feedback sweep, address or explicitly push back, revalidate, return to `Human Review`).
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
     - If PR is already attached, start by reviewing all open PR comments and deciding required changes vs explicit pushback responses.
   - `In Progress` -> continue execution flow from current scratchpad comment.
   - `Human Review` -> human handoff state. Do not make changes, and expect Symphony to stay idle until a human moves the task to `Rework` or `Merging`.
   - `Merging` -> skip Steps 1 and 2. On entry, go directly to the merge fast path in Step 3; do not call `gh pr merge` directly.
   - `Rework` -> run rework flow.
   - `Done` -> do nothing and shut down.
4. Check whether a PR already exists for the current branch and whether it is closed.
   - If a branch PR exists and is `CLOSED` or `MERGED`, treat prior branch work as non-reusable for this run.
   - Create a fresh branch from `origin/main` and restart execution flow as a new attempt.
   - Skip this branch-reset logic while in `Merging`; the merge fast path must operate on the surviving review PR instead of creating new implementation work.
5. For `Todo` tickets, do startup sequencing in this exact order:
   - resolve the `In Progress` section id with `list_sections`
   - `move_task(..., section_id: "<in-progress-section-id>")`
   - find/create the bootstrap workpad with `get_workpad` and `upsert_workpad`
   - only then begin analysis/planning/implementation work.
6. For `Merging`, do not create or rewrite the workpad before the merge decision unless the surviving PR is ambiguous or a blocker must be recorded.
7. Add a short comment if state and task content are inconsistent, then proceed with the safest flow.

## Step 1: Start/continue execution (Todo, In Progress, or Rework only)

Do not run this step while the task is in `Merging`.

1.  Find or create a single persistent scratchpad comment for the current task:
    - Use `{"action":"get_workpad","task_id":"<task-id>"}` to discover the active workpad.
    - The workpad is identified by the visible header `## Codex Workpad` and the hidden marker `<!-- symphony:workpad -->`.
    - If found, reuse that comment; do not create a new workpad comment.
    - If not found, create one with `{"action":"upsert_workpad","task_id":"<task-id>","content":"..."}` and use it for all updates.
    - Persist the workpad comment ID and pass it back as `comment_id` on later `upsert_workpad` calls.
2.  If arriving from `Todo`, do not delay on additional status transitions: the task should already be `In Progress` before this step begins.
3.  Immediately reconcile the workpad before new edits:
    - Check off items that are already done.
    - Expand/fix the plan so it is comprehensive for current scope.
    - Ensure `Acceptance Criteria` and `Validation` are current and still make sense for the task.
4.  Start work by writing/updating a hierarchical plan in the workpad comment.
5.  Ensure the workpad includes a compact environment stamp at the top as a code fence line:
    - Format: `<host>:<abs-workdir>@<short-sha>`
    - Example: `devbox-01:/home/dev-user/code/symphony-workspaces/MT-32@7bdde33bc`
    - Do not include metadata already inferable from Todoist task fields (`task ID`, `status`, `branch`).
6.  Add explicit acceptance criteria and TODOs in checklist form in the same comment.
    - If changes are user-facing, include a UI walkthrough acceptance criterion that describes the end-to-end user path to validate.
    - If changes touch app files or app behavior, add explicit app-specific flow checks to `Acceptance Criteria` in the workpad (for example: launch path, changed interaction path, and expected result path).
    - If the ticket description/comment context includes `Validation`, `Test Plan`, or `Testing` sections, copy those requirements into the workpad `Acceptance Criteria` and `Validation` sections as required checkboxes (no optional downgrade).
7.  Run a principal-style self-review of the plan and refine it in the comment.
8.  Before implementing, capture a concrete reproduction signal and record it in the workpad `Notes` section (command/output, screenshot, or deterministic UI behavior).
9.  Run the `pull` skill to sync with latest `origin/main` before any code edits, then record the pull/sync result in the workpad `Notes`.
    - Include a `pull skill evidence` note with:
      - merge source(s),
      - result (`clean` or `conflicts resolved`),
      - resulting `HEAD` short SHA.
10. Compact context and proceed to execution.

## PR feedback sweep protocol (required)

When a ticket has an attached PR, run this protocol before moving to the team's review handoff state:

1. Identify the PR number from the task-scoped workpad comment. If native task-link metadata is present, use it as supplemental confirmation rather than the source of truth.
2. Gather feedback from all channels:
   - Todoist task comments other than the `## Codex Workpad` comment, including any attached resources or general review instructions.
   - Top-level PR comments (`gh pr view --comments`).
   - Inline review comments (`gh api repos/<owner>/<repo>/pulls/<pr>/comments`).
   - Review summaries/states (`gh pr view --json reviews`).
3. Treat every actionable reviewer comment from Todoist or GitHub (human or bot), including inline review comments, as blocking until one of these is true:
   - code/test/docs updated to address it, or
   - explicit, justified pushback reply is posted on the relevant GitHub thread, or recorded in the workpad when the feedback came from a Todoist task comment.
4. Update the workpad plan/checklist to include each feedback item and its resolution status.
5. Re-run validation after feedback-driven changes and push updates.
6. Repeat this sweep until there are no outstanding actionable comments.

## Blocked-access escape hatch (required behavior)

Use this only when completion is blocked by missing required tools or missing auth/permissions that cannot be resolved in-session.

- GitHub is **not** a valid blocker by default. Always try fallback strategies first (alternate remote/auth mode, then continue publish/review flow).
- Treat GitHub DNS failures, connection resets, TLS/transport failures, HTTP 5xx, rate limits, and temporary `403`/abuse-protection responses as transient unless repeated evidence proves otherwise.
- Retry transient GitHub operations at least twice with a short backoff before escalating, then retry through an alternate interface (`gh`, Symphony's host-side `github_api` tool when available, direct GitHub REST via `curl`, GitHub MCP, or direct git remote flow) before concluding the operation is blocked.
- If `gh auth status -h github.com` is healthy and repo read operations succeed, treat `gh` as the primary GitHub interface for blocker classification.
- If Symphony exposes a `github_api` tool, prefer it as the next publish fallback after transient `gh` transport failures because it runs host-side and avoids in-session DNS/auth drift.
- If Symphony exposes a `github_api` tool, prefer it for post-publish GitHub metadata writes as well (labels, PR metadata refresh, and other REST mutations) because host-side auth is often available even when in-session `gh auth token` is not.
- If `gh` transport is flaky but the `github_api` tool is unavailable, or `curl https://api.github.com/repos/<owner>/<repo>` succeeds with `GH_TOKEN`/`GITHUB_TOKEN` (or a token obtained from `gh auth token`), treat direct GitHub REST as the next publish fallback before GitHub MCP.
- If the primary `gh` path only fails with transient transport errors, keep the ticket in its active state and let a continuation turn retry publish/review work. Do not hand the ticket off to review solely because a lower-privilege fallback interface also failed.
- After documenting a transient GitHub publish failure and exhausting the required retry/fallback sequence, end the current turn promptly while keeping the ticket active so the next continuation turn can retry from the saved workpad state.
- Treat GitHub MCP `403 Resource not accessible by personal access token` responses as evidence about that fallback interface only; they do not prove the primary `gh` path lacks the required permissions.
- Never classify a GitHub blocker from a single failed command. Record the exact failing command and stderr/API response in the workpad first.
- Do not move to `Human Review` for GitHub access/auth until all fallback strategies have been attempted and documented in the workpad, and the remaining blocker is a durable auth/permission problem rather than a transient transport failure.
- If a non-GitHub required tool is missing, or required non-GitHub auth is unavailable, move the ticket to `Human Review` with a short blocker brief in the workpad that includes:
  - what is missing,
  - why it blocks required acceptance/validation,
  - exact human action needed to unblock.
- Keep the brief concise and action-oriented; do not add extra top-level comments outside the workpad.

## Step 2: Execution phase (Todo/In Progress/Rework -> Human Review)

Do not run this step while the task is in `Merging`.

1.  Determine current repo state (`branch`, `git status`, `HEAD`) and verify the kickoff `pull` sync result is already recorded in the workpad before implementation continues.
2.  If current task state is `Todo`, move it to `In Progress`; otherwise leave the current state unchanged.
3.  Load the existing workpad comment and treat it as the active execution checklist.
    - Edit it liberally whenever reality changes (scope, risks, validation approach, discovered tasks).
4.  Implement against the hierarchical TODOs and keep the comment current:
    - Check off completed items.
    - Add newly discovered items in the appropriate section.
    - Keep parent/child structure intact as scope evolves.
    - Update the workpad at milestone boundaries (for example: reproduction complete, code change landed, validation run, review feedback addressed), batching nearby checklist-only edits into the next reviewer-facing rewrite.
    - Never leave completed work unchecked in the plan.
    - For tickets that started as `Todo` with an attached PR, run the full PR feedback sweep protocol immediately after kickoff and before new feature work.
5.  Run validation/tests required for the scope.
    - Mandatory gate: execute all ticket-provided `Validation`/`Test Plan`/`Testing` requirements when present; treat unmet items as incomplete work.
    - Prefer a targeted proof that directly demonstrates the behavior you changed.
    - You may make temporary local proof edits to validate assumptions when this increases confidence.
    - Revert every temporary proof edit before commit/push.
    - Document these temporary proof steps and outcomes in the workpad `Validation`/`Notes` sections so reviewers can follow the evidence.
    - If app-touching, run runtime validation and capture supporting media when the necessary tooling is available before handoff.
6.  Re-check all acceptance criteria and close any gaps.
7.  Before every `git push` attempt, run the required validation for your scope and confirm it passes; if it fails, address issues and rerun until green, then commit and push changes.
8.  Record the surviving PR URL in the task-scoped workpad comment. If the runtime later exposes native task-link metadata, keep that metadata aligned as supplemental evidence.
    - As soon as the surviving PR exists, do one workpad rewrite that records the PR URL and remember the returned `comment_id` for the remaining handoff steps.
    - If `gh pr create` fails with transient transport noise after the branch is already pushed, create the PR through Symphony's `github_api` tool when available; otherwise use direct GitHub REST before falling back to GitHub MCP.
    - `github_api` publish fallback:
      - create the PR with `POST /repos/<owner>/<repo>/pulls` and JSON body fields `title`, `head`, `base`, and `body`.
      - add the `symphony` label with `POST /repos/<owner>/<repo>/issues/<pr-number>/labels` and JSON body `{ "labels": ["symphony"] }`.
      - re-read the PR with `GET /repos/<owner>/<repo>/pulls/<pr-number>` and check status with `GET /repos/<owner>/<repo>/commits/<head-sha>/check-runs`.
    - Direct REST publish fallback:
      - obtain a token from `GH_TOKEN` or `GITHUB_TOKEN`; if neither is set but `gh auth token` works, export one of them and continue.
      - create the PR with `curl` to `POST https://api.github.com/repos/<owner>/<repo>/pulls` using JSON body fields `title`, `head`, `base`, and `body`.
      - add the `symphony` label with `POST https://api.github.com/repos/<owner>/<repo>/issues/<pr-number>/labels`.
      - re-read the PR with `GET https://api.github.com/repos/<owner>/<repo>/pulls/<pr-number>` and check status with `GET https://api.github.com/repos/<owner>/<repo>/commits/<head-sha>/check-runs`.
    - Ensure the GitHub PR has label `symphony` (add it if missing).
    - For label writes, try Symphony's host-side `github_api` tool first with `POST /repos/<owner>/<repo>/issues/<pr-number>/labels` and JSON body `{ "labels": ["symphony"] }`.
    - If the host-side `github_api` tool is unavailable or the label write fails with durable host-side auth/permission evidence, retry with `gh pr edit <pr-number> --add-label symphony`, then `gh issue edit <pr-number> --add-label symphony` because the PR is also an issue.
    - Do not treat an in-session `gh auth token` failure by itself as proof that labeling is blocked when the host-side `github_api` tool is available.
    - Re-read PR metadata after each label attempt; only treat labeling as blocked after the retry/fallback sequence fails with documented output.
9.  Merge latest `origin/main` into branch, resolve conflicts, and rerun checks.
10. Update the workpad comment with final checklist status and validation notes.
    - Mark completed plan/acceptance/validation checklist items as checked.
    - Add final handoff notes (commit + validation summary) in the same workpad comment.
    - Keep the current surviving PR URL in the workpad comment handoff notes so the task retains a canonical PR reference inside Todoist.
    - If the current workpad already contains the surviving PR URL and latest reviewer-facing validation evidence, do not rewrite it again just to restate passing checks.
    - Add a short `### Confusions` section at the bottom when any part of task execution was unclear/confusing, with concise bullets.
    - Do not post any additional completion summary comment.
11. Before moving to `Human Review`, poll PR feedback and checks:
    - Read the PR `Manual QA Plan` comment (when present) and use it to sharpen UI/runtime test coverage for the current change.
    - Run the full PR feedback sweep protocol.
    - Confirm PR checks are passing (green) after the latest changes.
    - Confirm every required ticket-provided validation/test-plan item is explicitly marked complete in the workpad.
    - Repeat this check-address-verify loop until no outstanding comments remain and checks are fully passing.
    - Only perform a final `upsert_workpad` before state transition if `Plan`, `Acceptance Criteria`, or `Validation` changed materially since the last rewrite. If they are already current, move directly to `Human Review`.
    - A pure "all checks are green and there were no actionable review comments" delta is not material by itself when the existing workpad already includes the surviving PR URL, the required validation command results, and the current acceptance checklist context. Do not rewrite the workpad only to flip those last handoff boxes.
    - If the only remaining delta is a non-essential handoff polish and the current workpad already contains the surviving PR URL plus materially current validation evidence, do not stay active just to rewrite the workpad again.
    - If Todoist rate-limits that non-essential final rewrite, do not burn the turn waiting on repeated retries. Keep the existing PR-linked workpad, then move directly to `Human Review`.
    - Verify there is exactly one surviving open PR for the current rerun branch before handoff. If no open PR exists, or only closed PRs are attached, stay active and recreate/reopen the correct PR instead of handing off.
12. Only then move the task to `Human Review`.
    - Exception: if blocked by missing required non-GitHub tools/auth per the blocked-access escape hatch, move to `Human Review` with the blocker brief and explicit unblock actions.
    - Exception: if GitHub publish work is blocked only by transient transport failures, keep the issue active, document the evidence in the workpad, and allow the next continuation turn to retry instead of handing off early.
13. For `Todo` tickets that already had a PR attached at kickoff:
    - Ensure all existing PR feedback was reviewed and resolved, including inline review comments (code changes or explicit, justified pushback response).
    - Ensure branch was pushed with any required updates.
    - Then move to `Human Review`.

## Step 3: Human Review and merge handling

1. When the task is in `Human Review`, do not code or change task content. This is an operator handoff state, so no new Symphony turn should run until a human moves the task to `Rework` or `Merging`.
2. Poll for updates as needed, including non-workpad Todoist task comments plus GitHub PR review comments from humans and bots.
3. If review feedback requires changes, move the task to `Rework` and follow the rework flow.
4. If approved, human moves the task to `Merging`.
5. When the task is in `Merging`, use this fast path:
   - identify the single surviving PR from the current workpad, branch, or task-linked metadata without doing a full execution bootstrap.
   - refresh non-workpad Todoist task comments with `{"action":"list_comments","task_id":"<task-id>"}` instead of relying only on the initial prompt snapshot.
   - treat actionable non-workpad Todoist task comments the same way as actionable non-bot GitHub feedback during merge preflight.
   - confirm the PR is still open, checks are green, and there are no unresolved actionable human comments from Todoist or GitHub. Bot comments may provide helpful merge-time signal, but they are advisory unless a human comment or explicit workflow instruction says otherwise.
   - if fresh human review feedback appears and code or docs must change, move the task to `Rework` instead of merging.
   - if merge readiness is blocked for another reason, record only the minimum blocker evidence needed in the workpad and keep the task active.
6. Once the surviving PR is confirmed merge-ready, run the `land` skill in a loop until the PR is merged. If the checkout does not contain the skill file, use the runtime-provided `land` skill guidance instead of a repo-local path. Do not call `gh pr merge` directly.
7. After merge is complete, do at most one final workpad refresh if the merged PR URL or blocker resolution evidence is missing, then immediately call `{"action":"close_task","task_id":"<task-id>"}` for the current task while it is still in `Merging`.
   - If the pre-merge workpad already linked the surviving PR and the merge outcome is otherwise clear from the current turn, do not block completion on a cosmetic post-merge workpad rewrite.
   - If Todoist rate-limits that optional final rewrite, skip it and proceed with guarded `close_task`.
8. Do not end the turn after merge until guarded `close_task` succeeds or a durable blocker is recorded in the workpad.

## Step 4: Rework handling

1. Treat `Rework` as a planning reset, not a license to discard already-accepted implementation.
2. Re-read the full task description, refresh all non-workpad Todoist task comments with `{"action":"list_comments","task_id":"<task-id>"}`, review the latest surviving PR diff, and gather all GitHub human comments; treat the Todoist comments as the same class of human review input as non-bot GitHub feedback, then explicitly identify what will be done differently this attempt.
3. Close only PRs that are truly superseded by the new rerun attempt.
   - At most one PR may survive a rerun handoff.
   - Preserve the newest valid open PR once it has the intended diff, passing checks, and no actionable feedback.
   - Do not close the final good PR just because earlier rerun PRs were retired.
4. Before any branch reset or force-push, record the surviving PR head SHA and summarize the already-accepted diff in the workpad.
5. Default rework base is the surviving PR branch/HEAD, not `origin/main`.
   - Layer the requested follow-up changes on top of the current surviving diff.
   - Keep the original task scope plus the rework request in the same PR unless the PR is invalid or explicitly superseded.
6. Only restart from `origin/main` when the current PR/branch is unusable (for example: closed, merged, corrupted, or intentionally replaced).
   - If you must restart from `origin/main`, you must reapply both the original intended task diff and the newly requested rework before handing back to review.
   - Never hand off a rerun PR that fixes the review comment but drops the original accepted change.
7. Remove the existing workpad comment from the task with `{"action":"delete_workpad","task_id":"<task-id>"}`.
8. Create a new bootstrap workpad with `upsert_workpad` and explicitly carry forward the union of:
   - original acceptance criteria, and
   - review/rework acceptance criteria.
9. Before moving back to `Human Review`, verify the surviving PR diff still contains the original intended change set plus the new rework change set.

## Completion bar before review handoff

- Step 1/2 checklist is fully complete and accurately reflected in the single workpad comment.
- Acceptance criteria and required ticket-provided validation items are complete.
- Validation/tests are green for the latest commit.
- PR feedback sweep is complete and no actionable comments remain.
- PR checks are green, branch is pushed, and exactly one open PR for the current rerun is referenced in the workpad comment.
- Required PR metadata is present (`symphony` label).
- For `Rework`, the surviving PR still contains the original intended task diff in addition to the follow-up fix.
- If app-touching, runtime validation/media requirements from Step 2 are complete.

## Guardrails

- If the branch PR is already closed/merged, do not reuse that branch or prior implementation state for continuation.
- For closed/merged branch PRs, create a new branch from `origin/main` and restart from reproduction/planning as if starting fresh.
- Never leave a task in `Human Review` or `Merging` without an open PR that matches the current rerun branch. If the only linked PRs are closed, reopen the correct PR or create a fresh replacement before handoff.
- In `Merging`, default to the surviving review PR. Do not create a fresh branch, rerun pull sync, or restart implementation unless merge readiness is genuinely impossible to recover.
- If task state is `Backlog`, do not modify it; wait for human to move to `Todo`.
- Do not edit the task description for planning or progress tracking.
- Use exactly one persistent workpad comment (`## Codex Workpad` + `<!-- symphony:workpad -->`) per task, and keep it as a task comment on the active Todoist task.
- Do not edit or delete human-authored Todoist task comments while managing the workpad. Only the single workpad comment is agent-owned.
- If comment editing is unavailable in-session, fall back to the Todoist `todoist` tool comment actions; only report blocked if those actions are unavailable for the configured account.
- Temporary proof edits are allowed only for local verification and must be reverted before commit.
- If out-of-scope improvements are found, create a separate Backlog task rather than expanding current scope.
- Default to a new top-level task in the same Todoist project.
- Only create the follow-up as a subtask when it is clearly a child step of the current task and should stay subordinate to it.
- Include a clear title, description, acceptance criteria, and a short back-reference to the current `TD-...` task in the follow-up description.
- Todoist has no native `related` or `blockedBy` graph in v1, so record dependency notes in the task description instead of inventing structured relation fields.
- Do not move to `Human Review` unless the `Completion bar before review handoff` is satisfied.
- In `Human Review`, do not make changes. A human must move the task to `Rework` or `Merging` before Symphony runs again.
- If state is terminal (including implicit completed state `Done`), do nothing and shut down.
- Keep task text concise, specific, and reviewer-oriented.
- If blocked and no workpad exists yet, add one blocker comment describing blocker, impact, and next unblock action.

## Workpad template

Use this exact structure for the persistent workpad comment and keep it updated in place throughout execution:

````md
## Codex Workpad

<!-- symphony:workpad -->

```text
<hostname>:<abs-path>@<short-sha>
```

### Plan

- [ ] 1\. Parent task
  - [ ] 1.1 Child task
  - [ ] 1.2 Child task
- [ ] 2\. Parent task

### Acceptance Criteria

- [ ] Criterion 1
- [ ] Criterion 2

### Validation

- [ ] targeted tests: `<command>`

### Notes

- <short progress note with timestamp>

### Confusions

- <only include when something was confusing during execution>
````
