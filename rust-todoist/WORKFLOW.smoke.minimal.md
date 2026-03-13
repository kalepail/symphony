---
tracker:
  kind: todoist
  api_key: $TODOIST_API_TOKEN
  project_id: $SYMPHONY_SMOKE_PROJECT_ID
  label: symphony-smoke-minimal
  active_states:
    - Todo
polling:
  interval_ms: 5000
workspace:
  root: $SYMPHONY_WORKSPACE_ROOT
hooks:
  after_create: |
    git clone --depth 1 https://github.com/kalepail/symphony-smoke-lab.git .
agent:
  max_concurrent_agents: 1
  max_turns: 8
codex:
  command: codex --config shell_environment_policy.inherit=core --config model_reasoning_effort=low --model gpt-5.4 app-server
server:
  port: 3000
---

You are running a bounded Symphony live smoke test in the `kalepail/symphony-smoke-lab` repository.

Issue:
- Identifier: {{ issue.identifier }}
- Title: {{ issue.title }}

Steps:
1. Inspect the repository before editing anything.
2. Create or update a single file named `SMOKE_TARGET.md` at the repository root.
3. Preserve the existing headings in `SMOKE_TARGET.md` and append exactly one new bullet under `## Change Log`.
4. That new bullet must contain:
   - the Todoist task identifier
   - the Todoist task title
   - the current UTC date in `YYYY-MM-DD` form
   - one short phrase saying it was created by a Symphony minimal smoke test
5. Run `sh scripts/validate-smoke-repo.sh`.
6. Use the `todoist` tool to create or update the task workpad with a short summary of the exact file change you made.
7. Resolve the `Backlog` section id and move the task back to `Backlog` instead of completing it.
8. Stop after that.

Todoist tool notes:
- Read the current task with `{"action":"get_task","task_id":"<task-id>"}`.
- Write the task summary with `{"action":"upsert_workpad","task_id":"<task-id>","content":"..."}`.
- The workpad is the single agent-owned task comment, and Todoist comment responses identify that task with `item_id`.
- Resolve sections with `{"action":"list_sections"}` and move the task with `{"action":"move_task","task_id":"<task-id>","section_id":"<backlog-section-id>"}`.
- Keep each `todoist` call to one narrow operation.

Constraints:
- Do not edit any other files.
- Do not open a pull request.
- Do not commit or push changes.
- If the repository is already present, reuse it.
- If `SMOKE_TARGET.md` already exists, update only that file.
