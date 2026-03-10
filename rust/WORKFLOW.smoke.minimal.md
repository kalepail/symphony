---
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY
  project_slug: $SYMPHONY_SMOKE_PROJECT_SLUG
  assignee: me
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
  command: codex app-server
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
   - the Linear issue identifier
   - the Linear issue title
   - the current UTC date in `YYYY-MM-DD` form
   - one short phrase saying it was created by a Symphony minimal smoke test
5. Run `sh scripts/validate-smoke-repo.sh`.
6. Use the `linear_graphql` tool to add a short comment to the issue summarizing the exact file change you made.
7. Use the `linear_graphql` tool to move the issue to `Done`.
8. Stop after that.

Linear tool notes:
- Query the current issue with `issue(id: $id)` using the human identifier directly.
- Add the comment with `commentCreate(input: { issueId: $issueId, body: $body })`.
- Move to `Done` by querying `issue(id: $id) { team { states { nodes { id name type } } } }`, selecting the `Done` state id, then calling `issueUpdate(id: $id, input: { stateId: $stateId })`.
- Keep each `linear_graphql` call to one narrow operation.

Constraints:
- Do not edit any other files.
- Do not open a pull request.
- Do not commit or push changes.
- If the repository is already present, reuse it.
- If `SMOKE_TARGET.md` already exists, update only that file.
