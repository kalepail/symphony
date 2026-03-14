# Todoist Rate Limit Notes

This note captures the official Todoist rate-limit guidance that matters for the
Rust runtime, plus the specific log lines to look for when a run appears stuck.

## Official Limits

As of 2026-03-13, Todoist's official developer docs describe:

- API v1 rate limiting:
  - partial sync requests: 1000 requests per 15 minutes
  - full sync requests: 100 requests per 15 minutes
  - source: <https://developer.todoist.com/api/v1/#rate-limiting>
- REST API v1 rate limiting:
  - 450 requests per 15 minutes
  - source: <https://developer.todoist.com/rest/v1/#rate-limiting>
- Processing timeout:
  - Todoist may return a timeout error when request processing exceeds 15
    seconds
  - source: <https://developer.todoist.com/api/v1/#processing-timeout>

## What Symphony Uses

The Rust runtime defaults `tracker.base_url` to
`https://api.todoist.com/api/v1` and uses:

- REST-style endpoints such as `/tasks`, `/sections`, `/comments`, and
  `/projects`
- the `/sync` endpoint for sync-style operations

That means the API v1 sync limits are directly relevant, and the REST v1 limits
are still useful operator context for the resource-style endpoints we call.

Todoist rate limits are effectively shared at the Todoist account level, so
smoke, local development, and production traffic on the same user can interfere
with each other.

## Runtime Behavior

Symphony currently:

- parses `Retry-After` hints from response headers and Todoist error payloads
- tracks upstream budget metadata including `X-RateLimit-*`
- fails fast when Todoist asks for a retry delay beyond the runtime wait budget
- carries retry counts and total wait time into tool and turn telemetry when a
  tool call eventually succeeds after retries

The current retry policy caps normal Todoist request waiting at 300 seconds in
aggregate before failing the request.

## Logs To Check

When a run looks stuck, inspect these signals in order:

- shared upstream throttling:
  - `todoist_rate_budget status=throttled source=... retry_after_secs=...`
- tracker retry warnings:
  - `reason=rate_limited`
  - `source=poller|tool_call|startup|sync`
  - `detail=retry_after=...`
- tool-level failure or success telemetry:
  - `dynamic_tool_call ... error_kind=rate_limited retry_after_secs=...`
  - `dynamic_tool_call ... retry_count=... retry_wait_secs=...`
- turn-level rollup:
  - `codex_turn_summary ... retry_after_secs=...`
  - `codex_turn_summary ... tool_retry_count_total=... tool_retry_wait_secs_total=...`

If those rate-limit markers are absent and the session still goes quiet, the run
is more likely stalled in Codex turn processing, stream handling, or stall
reconciliation than blocked on Todoist backoff.
