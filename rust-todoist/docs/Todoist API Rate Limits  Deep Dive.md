# Todoist API Rate Limits: A Complete Developer Guide

## Overview

Todoist enforces per-user rate limits across its APIs to keep the platform stable. The documented limit is **1,000 requests per 15 minutes per user** for the REST API, though some sources report a lower effective limit of ~450 requests per 15 minutes. This discrepancy, combined with the ongoing migration from the legacy REST v2 / Sync v9 APIs to the new unified API v1.0, makes it easy to accidentally burn through your quota — especially if you have multiple integrations sharing the same user token.[^1][^2][^3]

## Documented Rate Limits

### REST API (v2 / new v1)

The primary documented limit for the REST API is **1,000 requests per user per 15-minute window**. Each individual API call (GET, POST, DELETE, etc.) counts as one request against this quota. Several third-party integration tools, however, implement a more conservative practical limit of approximately **300–450 requests per 15 minutes**, suggesting that the effective ceiling may be lower in practice or that Todoist applies additional undocumented throttling.[^4][^5][^6][^2][^7][^1]

### Sync API (v9)

The Sync API has a split rate-limiting scheme:[^1]

| Sync Type | Limit per 15 Minutes |
|-----------|---------------------|
| Partial (incremental) sync | 1,000 requests |
| Full sync (`sync_token = "*"`) | 100 requests |

Full syncs are dramatically more expensive — both in rate-limit cost and server load. Todoist explicitly recommends performing a full sync only on the initial request, then using incremental syncs with the returned `sync_token` for all subsequent calls.[^1]

### Batch Commands (Sync API)

The Sync API allows batching **up to 100 commands per single request**, and that entire batch counts as just one request toward the rate limit. This is a critical optimization if you're performing bulk operations like creating, updating, or completing many tasks at once.[^1]

## The New Unified API v1.0

Todoist launched their unified API v1.0 in April 2025, merging the REST and Sync APIs into a single API surface. The legacy REST v2 and Sync v9 APIs were projected for deprecation in Q4 2025, with shutdown in early 2026. Key changes include faster endpoints, pagination support, and improved SDKs for Python and JavaScript.[^8][^9][^10][^11]

If you're still hitting the old REST v2 or Sync v9 endpoints, be aware that they may behave differently or have reduced support going forward. Migrating to the unified v1 API is strongly recommended.[^9][^10]

## Why You're Likely Hitting Rate Limits

Based on common patterns reported by developers, there are several likely culprits:

### Polling Too Frequently

The most common cause is aggressive polling intervals. If you're checking for task updates every few seconds, you'll burn through your 15-minute budget quickly. An Obsidian Todoist plugin user resolved constant 429 errors by increasing their auto-refresh interval from 60 seconds to 600 seconds. Even at 60-second polling, a single query endpoint might seem fine, but it adds up fast if you're calling multiple endpoints per refresh cycle.[^12]

**Math check:** At a 10-second polling interval, you'd hit 90 requests in 15 minutes from just one endpoint. If your app checks tasks, projects, labels, and comments, that's 360 requests — already pushing close to the effective limit some integrations observe.

### Multiple Integrations Sharing a Token

Rate limits are **per user**, not per application. Every integration, plugin, MCP server, or script using the same Todoist account shares the same rate-limit bucket. If you have an Obsidian plugin, an MCP agent, a custom sync script, and a Power Automate flow all hitting the API simultaneously, their requests are cumulative. One poorly behaved integration can starve the others.[^13][^14][^1]

### Full Syncs Instead of Incremental

If your code uses `sync_token = "*"` on every call (a full sync), you're limited to just 100 requests per 15 minutes instead of 1,000. This is a common mistake when developers don't properly store and reuse the `sync_token` returned from previous sync responses.[^1]

### Unbatched Write Operations

Creating or updating tasks one-at-a-time rather than batching them wastes requests. If you're adding 50 tasks individually, that's 50 requests. Using the Sync API's batch capability, those same 50 operations could be a single request.[^1]

### Retry Storms

When you hit a 429 error and immediately retry without respecting the `Retry-After` header, you create a retry storm that keeps you rate-limited indefinitely. This is especially common in scripts without proper backoff logic.[^15][^16]

### Possible Undocumented Limits

At least one developer has reported what appears to be a daily or hourly limit beyond the documented 1,000-per-15-minutes cap. While this hasn't been officially confirmed by Doist, it's worth considering if you're staying under the 15-minute limit but still getting 429s.[^17]

## Handling 429 Responses

When you hit the rate limit, Todoist returns an HTTP `429 Too Many Requests` status. The response headers typically include information about when you can retry.[^15]

### Key Response Headers

Standard rate-limit headers to look for:[^18][^19]

| Header | Description |
|--------|-------------|
| `X-RateLimit-Limit` | Maximum requests allowed in the window |
| `X-RateLimit-Remaining` | Requests remaining in the current window |
| `X-RateLimit-Reset` | UTC epoch timestamp when the window resets |
| `Retry-After` | Seconds to wait before retrying (on 429 responses) |

Always parse `Retry-After` when you receive a 429 and wait at least that long before retrying.[^20]

### Exponential Backoff Implementation

A robust retry strategy uses exponential backoff with jitter. Here's the pattern from the official Rust Todoist API wrapper:[^21]

```rust
async fn get_tasks_with_retry(todoist: &TodoistWrapper) -> TodoistResult<PaginatedResponse<Task>> {
    let mut attempts = 0;
    let max_attempts = 3;
    loop {
        attempts += 1;
        match todoist.get_tasks(None, None).await {
            Ok(response) => return Ok(response),
            Err(TodoistError::RateLimited { retry_after, message }) if attempts < max_attempts => {
                let delay = retry_after.unwrap_or(60);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
            Err(e) => return Err(e),
        }
    }
}
```

For the Python SDK, since Doist declined to add built-in backoff, you can pass a custom `session` with a retry adapter:[^16]

```python
from requests.adapters import HTTPAdapter
from urllib3.util.retry import Retry
from todoist_api_python.api import TodoistAPI

session = requests.Session()
retries = Retry(total=3, backoff_factor=1, status_forcelist=[429, 500, 502, 503])
session.mount("https://", HTTPAdapter(max_retries=retries))

api = TodoistAPI("your-token", session=session)
```

## Best Practices to Stay Under the Limit

### Monitor Your Remaining Budget

Read `X-RateLimit-Remaining` from every API response. Track it in your application state and throttle proactively when it drops below 20% of `X-RateLimit-Limit`.[^19][^14]

### Use Incremental Syncs

Always store the `sync_token` from Sync API responses and pass it back on subsequent calls. This converts full syncs (100/15min limit) into partial syncs (1,000/15min limit) and also returns less data per response.[^1]

### Batch Write Operations

Use the Sync API's command batching to group up to 100 operations per request. This is the single most impactful optimization for write-heavy workloads.[^1]

### Use Webhooks Instead of Polling

Rather than polling for changes, register webhooks to receive push notifications when tasks, projects, or comments change. This eliminates the need for periodic API calls entirely, though webhook delivery can sometimes be delayed.[^22][^1]

### Scope Your Integrations

If you run multiple tools against the same Todoist account, consider these strategies:

- **Audit all integrations**: List every app, plugin, script, and automation that uses your Todoist API token. The combined load across all of them shares the same rate budget.[^14]
- **Stagger polling intervals**: If multiple integrations poll on timers, offset them so they don't all fire at the same time.
- **Cache aggressively**: Store task/project data locally and only refresh when necessary. If data hasn't changed (use `sync_token` to detect this), skip the API call.[^14]
- **Use separate tokens for debugging**: During development, use a test account rather than your main account to avoid impacting your production integrations.

### Implement a Local Rate Limiter

The Todoist MCP Server implements a token-bucket rate limiter that pre-throttles requests before they ever hit the API:[^4]

- REST API: 300 requests/minute (token bucket: 300 capacity, 5 tokens/sec refill)
- Sync API: 50 requests/minute (token bucket: 50 capacity, ~0.83 tokens/sec refill)

Implementing a similar client-side limiter — particularly a token-bucket or leaky-bucket algorithm — prevents bursts from ever triggering a 429.[^23][^4]

## Quick Diagnostic Checklist

If you're hitting 429s unexpectedly, work through this checklist:

1. **Check polling frequency** — Are you hitting the API more often than every 15–30 seconds?
2. **Count total integrations** — How many apps/scripts/plugins share your Todoist token?
3. **Inspect sync type** — Are you accidentally doing full syncs (`sync_token = "*"`) instead of incremental?
4. **Look for retry storms** — Is your error handling immediately retrying 429s without backoff?
5. **Log `X-RateLimit-Remaining`** — Add logging to see how fast your budget depletes per 15-minute window.
6. **Check for unbatched writes** — Are you making individual API calls for bulk operations?
7. **Verify API version** — Are you on the new unified v1 API, or still on the legacy REST v2/Sync v9 that may be deprecated?[^8][^9]

---

## References

1. [Todoist API Essential Guide - Rollout](https://rollout.com/integration-guides/todoist/api-essentials) - An essential reference guide to the Todoist API

2. [Todoist - Numerics | Cynapse](https://cynapse.com/numerics/docs/todoist) - Numerics documentation for the Todoist Integration

3. [Todoist Plugin error, please help: "Request failed, status 429"](https://forum.obsidian.md/t/todoist-plugin-error-please-help-request-failed-status-429/52556) - Things I have tried Reloading obsidian. (no change) Checking Todoist to see that it works (it does) ...

4. [Todoist MCP Server - Glama](https://glama.ai/mcp/servers/@shayonpal/mcp-todoist) - MCP Server to create and manage tasks, projects, labels and more in Todoist, using their unified v1 ...

5. [Using the Todoist API to Create Tasks (with PHP examples) - Endgrate](https://endgrate.com/blog/using-the-todoist-api-to-create-tasks-(with-php-examples)) - Learn to integrate Todoist API with PHP for task creation and management.

6. [Todoist MCP Server - Assay](https://assay.tools/packages/todoist-mcp-server) - Todoist MCP Server — Todoist MCP server enabling AI agents to interact with Todoist's task managemen...

7. [Todoist - AnyQuery](https://anyquery.dev/integrations/todoist) - The plugin is subject to the rate limits of the Todoist API (1000 requests per 15 minutes). If you h...

8. [Todoist API changes - Drafts Community](https://forums.getdrafts.com/t/todoist-api-changes/16352) - Todoist has a new, integrated API that supercedes both their previous “REST” and “SYNC” APIs. At the...

9. [Todoist API Updates (deadline Febrary 2026) - Drafts Community](https://forums.getdrafts.com/t/todoist-api-updates-deadline-febrary-2026/16403) - Drafts has long supported integration with Todoist, the online task manager, through actions that wo...

10. [Todoist API v1.0](https://groups.google.com/a/doist.com/g/todoist-api/c/LKz0K5TRQ9Q)

11. [How to stop getting rate limited by APIs - Merge](https://www.merge.dev/blog/how-to-stop-being-rate-limited-best-practices-for-making-api-calls-at-scale) - Merge engineer David Donnelly walks readers through the best practices and approaches for implementi...

12. [Unknown Todoist Plugin error, please help: "Request failed, status 429"](https://www.reddit.com/r/ObsidianMD/comments/10ei7hx/unknown_todoist_plugin_error_please_help_request/)

13. [Todoist MCP Server by Shockedrope - Glama](https://glama.ai/mcp/servers/@Shockedrope/todoist-mcp-server) - Enables AI assistants to manage Todoist tasks, projects, labels, sections, and comments through natu...

14. [API Rate Limit Exceeded: Causes, Fixes, and Best Practices - DigitalAPIwww.digitalapi.ai › blogs › api-rate-limit-exceeded](https://www.digitalapi.ai/blogs/api-rate-limit-exceeded) - Hit an API Rate Limit Exceeded error? Learn what triggers it, how to fix it, and best practices to a...

15. [Looks like the sync API is down.](https://www.reddit.com/r/todoist/comments/10v3o5y/looks_like_the_sync_api_is_down/)

16. [Automatically backoff when rate limited · Issue #38](https://github.com/Doist/todoist-api-python/issues/38) - Enhancement description Add option to automatically use a backoff when you've hit a rate limit. The ...

17. [Daily rate limit? · Issue #155 · Doist/todoist-api-python - GitHub](https://github.com/Doist/todoist-api-python/issues/155) - The official documentation states 1k requests per 15m. It seems as though there is some sort of dail...

18. [Examples of HTTP API Rate Limiting HTTP Response headers](https://stackoverflow.com/questions/16022624/examples-of-http-api-rate-limiting-http-response-headers) - One of the Additional HTTP Status Codes (RFC6585) is 429 Too Many Requests Where can I find examples...

19. [How to Implement API Rate Limit Headers - OneUptime](https://oneuptime.com/blog/post/2026-01-30-api-rate-limit-headers/view) - A practical guide to implementing standardized API rate limit headers that help clients understand t...

20. [Retry-After header - HTTP - MDN Web Docs](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Retry-After) - The HTTP Retry-After response header indicates how long the user agent should wait before making a f...

21. [Todoist-api - Lib.rs](https://lib.rs/crates/todoist-api) - A Rust wrapper for the Todoist Unified API v1

22. [Webhook (rate) limits?](https://www.reddit.com/r/todoist/comments/13oysis/webhook_rate_limits/)

23. [API Rate Limiting Cheat Sheet](https://dev.to/colinmcdermott/api-rate-limiting-cheat-sheet-409f) - Jump to a section: Gateway-level rate limiting Token bucket algorithm Leaky bucket...

