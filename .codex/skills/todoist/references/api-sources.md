# Todoist API Sources

Use this file when you need the current official Todoist API source of truth.

## Primary source

Use the unified Todoist API v1 docs first:

- `https://developer.todoist.com/api/v1/`

For this skill, treat that page as the authoritative vendor reference for:

- endpoint and command shape
- id format
- pagination behavior
- reminder and activity availability
- `/api/v1/sync` behavior that still exists inside API v1

## Local runtime sources

For Symphony-specific behavior, use these local files:

- `../../../rust-todoist/src/dynamic_tool.rs`
- `../../../rust-todoist/README.md`
- `../../../rust-todoist/WORKFLOW.md`

These local files define the actual `todoist` tool contract, the workpad rules,
guarded `close_task` semantics, and the workflow expectations that go beyond
upstream Todoist API behavior.

## Practical rule

When sources disagree:

1. trust the live official Todoist API v1 docs first
2. trust the local Rust Todoist runtime contract second for Symphony behavior
3. treat search summaries as hints, not authority

## Legacy note

Todoist still hosts older REST v2 and Sync v9 documentation, but this skill
does not treat them as primary references. Only consult them when you are
explicitly debugging migration or historical naming differences.
