# Todoist API Sources

Use this file when you need the authoritative vendor docs or need to reconcile
versioning confusion.

## Current status

As of March 13, 2026, the official Todoist developer site presents the API
surface this way:

- `https://developer.todoist.com/api/v1/` is the current unified Todoist API
  documentation.
- `https://developer.todoist.com/rest/v2/` is still available, but the page is
  explicitly marked deprecated and points readers to API v1.
- `https://developer.todoist.com/sync/v9` is still available, but the page is
  explicitly marked deprecated and points readers to API v1.

This looks backwards if you remember the earlier REST v1 -> REST v2 migration,
but it is what the current official docs say now.

## Official statements

### Unified API v1

Authoritative URL:

- `https://developer.todoist.com/api/v1/`

Relevant official statements:

- The `Migrating from v9` section says Todoist API v1 is the new API that
  unifies Sync API v9 and REST API v2.
- That same section says the Sync API v9 and REST API v2 documentation remain
  available for reference.

Why it matters for Symphony:

- Prefer API v1 semantics when checking current Todoist behavior.
- Expect opaque string ids, cursor pagination, and renamed objects such as
  tasks, comments, and reminders.
- Do not assume every valid API v1 operation is a pure REST path. The official
  API v1 docs still include `/api/v1/sync` for some resource reads and command
  flows.

### REST API v2

Reference URL:

- `https://developer.todoist.com/rest/v2/`

Relevant official statement:

- The overview page is explicitly marked deprecated and tells readers to refer
  to the new unified Todoist API v1 documentation.

Use it for:

- legacy examples
- migration context
- confirming older SDK or endpoint names when reading existing integrations

Do not treat it as the primary source of truth when it conflicts with API v1.

### Sync API v9

Reference URL:

- `https://developer.todoist.com/sync/v9`

Relevant official statement:

- The overview page is explicitly marked deprecated and tells readers to refer
  to the new unified Todoist API v1 documentation.

Use it for:

- legacy sync semantics
- older note or item naming
- migration context for endpoints still exposed through `/sync`

### Deprecated guides page

Reference URL:

- `https://developer.todoist.com/guides/`

Relevant official statement:

- The guides landing page is explicitly marked deprecated and says the guides
  relate to deprecated Sync API v9 and REST API v2 docs, then points readers to
  the new unified API v1 documentation.

## Supporting operational notice

Status page:

- `https://status.todoist.net/history/1`

Relevant official signal:

- The January 5, 2026 notice refers to scheduled maintenance for legacy API
  endpoints and points affected developers to `api.todoist.com` plus the API v1
  migration docs.

Treat this as supporting context only. The primary source of truth is still the
developer docs above.

## Symphony-local sources

When using the repo-local Todoist skill, vendor docs are only half the story.
Check these local sources too:

- `../../../rust-todoist/src/dynamic_tool.rs`
- `../../../rust-todoist/README.md`
- `../../../rust-todoist/WORKFLOW.md`

These local files define the actual `todoist` tool contract, the workpad rules,
guarded `close_task` semantics, and the workflow-specific expectations that go
beyond upstream Todoist API behavior.

## Rust runtime audit note

The Rust Todoist runtime is already on the API v1 family:

- default base URL: `https://api.todoist.com/api/v1`
- primary reads and writes use API v1 REST-style paths such as `/tasks`,
  `/comments`, `/projects`, `/sections`, `/activities`, `/labels`, and `/user`
- some operations still use `/api/v1/sync`, which is part of the official API
  v1 docs rather than legacy Sync API v9

At the time of this note, the remaining `/api/v1/sync` usage in the runtime is
for user plan limits and reminder-related flows. That is still API v1, not
deprecated `/sync/v9`.

## Practical rule

When official Todoist docs, older community memory, and generated search
summaries disagree:

1. trust the live official Todoist docs first
2. trust Symphony's local Rust Todoist contract second for runtime behavior
3. treat Perplexity or other search summaries as hints, not authority
