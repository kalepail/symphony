# Elixir Observability Audit for Rust Todoist Observer

Audit date: 2026-03-13

## Summary

This audit compares the Elixir CLI/TUI observability stack against the Rust Todoist observer, with emphasis on operator-facing terminal and dashboard behavior plus the plumbing that makes those surfaces trustworthy.

The current state is not "Rust is missing observability." Rust already matches or exceeds Elixir in several important areas:

- terminal rendering safety is stronger in Rust, including alternate-screen isolation and explicit viewport clipping
- the web dashboard transport is stronger in Rust because it has SSE plus polling fallback instead of a LiveView-only path
- the Rust dashboard and issue payloads expose more operator context, including Todoist budget data and richer tracked-task detail
- Rust has comparable message humanization and test coverage for Codex event summarization

There are still two concrete Elixir advantages worth adopting in Rust:

1. Elixir preserves `snapshot_timeout` versus `snapshot_unavailable` all the way to operator-facing payloads.
2. Elixir keeps `/` usable during degraded startup by rendering an HTML dashboard error state instead of returning a JSON 503 body.

One planned follow-up item is already satisfied and should not be treated as a gap: Rust already has a logging conventions document at `rust-todoist/docs/logging.md`.

## Parity Already Achieved

### Terminal render throttling and event summarization

What Elixir does:

- Elixir coalesces rapid dashboard updates and locks this behavior down with tests in `elixir/test/symphony_elixir/orchestrator_status_test.exs:1127`.
- Elixir also has broad Codex event humanization coverage in `elixir/test/symphony_elixir/orchestrator_status_test.exs:1360`.

What Rust does today:

- Rust also coalesces rapid terminal updates and tests it in `rust-todoist/src/observability.rs:3535`.
- Rust has equivalent humanization coverage for Codex events, wrapper events, nested envelopes, auto-approval, and auto-answering in `rust-todoist/src/observability.rs:3137` and `rust-todoist/src/observability.rs:3286`.

Operator impact:

- No meaningful parity gap here. Both runtimes protect the terminal from event storms and keep the last-activity column readable.

Should Rust adopt the Elixir behavior?

- Already adopted.

Recommended follow-up:

- Keep this area stable and avoid reducing test depth during refactors.

### ANSI and control-byte sanitization

What Elixir does:

- Elixir strips ANSI and control bytes from last-message content and tests it in `elixir/test/symphony_elixir/orchestrator_status_test.exs:1301`.

What Rust does today:

- Rust strips ANSI escape sequences, OSC hyperlink escapes, and control bytes and tests it in `rust-todoist/src/observability.rs:3130` and `rust-todoist/src/observability.rs:3426`.

Operator impact:

- No current gap. Rust meets the same operator goal and handles hyperlink-style escape sequences explicitly.

Should Rust adopt the Elixir behavior?

- Already adopted.

Recommended follow-up:

- None beyond keeping the sanitization coverage intact.

### Logging conventions and file rotation

What Elixir does:

- Elixir documents its logging contract in `elixir/docs/logging.md:1`.
- Elixir configures a rotating disk logger and tests that handler in `elixir/test/symphony_elixir/orchestrator_status_test.exs:1264`.

What Rust does today:

- Rust configures rotating file logs in `rust-todoist/src/logging.rs:19`.
- Rust already has a logging guide in `rust-todoist/docs/logging.md:1`, and it is at least as specific as the Elixir version because it also covers `thread_id`, `turn_id`, `worker_host`, and `workspace`.

Operator impact:

- The expected "add a Rust logging conventions doc" follow-up is obsolete. Rust already has the doc and the runtime support.

Should Rust adopt the Elixir behavior?

- Already adopted, and in some respects extended.

Recommended follow-up:

- No new doc needed. Keep the Rust guide in sync with actual log wording as the runtime evolves.

## Places Where Rust Is Better

### Terminal isolation and viewport handling

What Elixir does:

- Elixir redraws the terminal dashboard and tests width behavior in `elixir/test/symphony_elixir/orchestrator_status_test.exs:1330`.

What Rust does today:

- Rust explicitly enters and leaves the alternate screen and hides/restores the cursor, with test coverage in `rust-todoist/src/observability.rs:3589`.
- Rust clips rendered output to the actual viewport width and height, with tests in `rust-todoist/src/observability.rs:3000` and `rust-todoist/src/observability.rs:3020`.

Operator impact:

- Rust is safer in cramped panes and avoids polluting normal shell scrollback.

Should Rust adopt the Elixir behavior?

- No. This is an area where Elixir should be treated as the baseline and Rust as the stronger implementation.

Recommended follow-up:

- Preserve alternate-screen rendering and viewport clipping as non-regression requirements.

### Web transport and richer payloads

What Elixir does:

- Elixir serves a LiveView dashboard and verifies refresh behavior through PubSub in `elixir/test/symphony_elixir/extensions_test.exs:523`.

What Rust does today:

- Rust serves `/api/v1/stream` as SSE and tests both initial delivery and follow-up refreshes in `rust-todoist/src/http.rs:290` and `rust-todoist/src/http.rs:327`.
- Rust’s dashboard and sample payloads include Todoist budget state and richer tracked-task metadata in `rust-todoist/src/observability.rs:2998`, `rust-todoist/src/observability.rs:3040`, and `rust-todoist/src/observability.rs:3742`.

Operator impact:

- Rust gives operators a more direct machine-readable stream and more Todoist-specific runtime context.

Should Rust adopt the Elixir behavior?

- No. Rust already exceeds Elixir here and should not regress.

Recommended follow-up:

- Keep SSE, polling fallback, Todoist budget visibility, and richer issue metadata as explicit advantages in future docs and tests.

## Concrete Elixir Advantages Rust Should Add

### Finding 1: Preserve `snapshot_timeout` versus `snapshot_unavailable`

What Elixir does:

- Elixir distinguishes snapshot timeout from general unavailability in the presenter: `snapshot_timeout` and `snapshot_unavailable` are different operator-facing codes in `elixir/lib/symphony_elixir_web/presenter.ex:12`.
- Elixir locks that behavior down in tests at `elixir/test/symphony_elixir/extensions_test.exs:466`.

What Rust does today:

- Rust already distinguishes `TimedOut` and `Unavailable` at the orchestrator handle layer in `rust-todoist/src/orchestrator.rs:49` and `rust-todoist/src/orchestrator.rs:479`.
- That distinction is lost at the HTTP boundary because `dashboard()` and `api_state()` collapse all snapshot failures into a single `snapshot_unavailable` path in `rust-todoist/src/http.rs:120`.
- The terminal degraded path also treats any snapshot error as generic unavailability in `rust-todoist/src/observability.rs:979`.

Operator impact:

- Operators cannot tell whether the runtime is down versus temporarily slow or blocked.
- This makes first-response diagnosis worse and weakens confidence in the web and terminal observability surfaces during incidents.

Should Rust adopt the Elixir behavior?

- Yes.

Recommended follow-up:

- Preserve timeout versus unavailable all the way through the Rust observability presenter and HTTP responses.
- Add explicit degraded-state modeling rather than collapsing snapshot fetches into `Option<StatePayload>`.
- Update `/api/v1/state` to emit distinct error codes for timeout and unavailability.
- Extend the terminal degraded renderer so timeout can be surfaced differently from full unavailability.

### Finding 2: Keep `/` usable during degraded startup

What Elixir does:

- Elixir keeps the dashboard route usable even when the snapshot path is degraded. Its LiveView renders an unavailable-state HTML page, and this is tested in `elixir/test/symphony_elixir/extensions_test.exs:599`.

What Rust does today:

- Rust has a useful degraded terminal/dashboard renderer in `rust-todoist/src/observability.rs:775`.
- Rust does not use an equivalent HTML degraded page on first-load web requests. The `/` route returns JSON 503 when `present_state()` fails in `rust-todoist/src/http.rs:120`.

Operator impact:

- On first load, the browser loses the dashboard shell entirely and shows a raw JSON error instead of a purpose-built operator page.
- This is a worse failure mode than Elixir even though Rust has stronger steady-state transport.

Should Rust adopt the Elixir behavior?

- Yes.

Recommended follow-up:

- Make `/` render a degraded HTML dashboard shell instead of a JSON 503 body when the snapshot fetch fails.
- Reuse the existing degraded-copy ideas already present in `render_snapshot_unavailable_dashboard()` so the web path retains links and refresh context.
- Add an HTTP test that asserts `/` stays HTML under snapshot failure.

## Supporting Observability Notes

### Issue/detail API usefulness

What Elixir does:

- Elixir issue payloads are useful but intentionally slim and only preserve the latest recent event in the presenter path at `elixir/lib/symphony_elixir_web/presenter.ex:63`.

What Rust does today:

- Rust issue detail preserves a broader recent-event list and richer tracked issue metadata in `rust-todoist/src/orchestrator.rs:1773`, `rust-todoist/src/orchestrator.rs:1866`, and `rust-todoist/src/observability.rs:3065`.

Operator impact:

- Rust is already better for deep-dive debugging from the issue detail endpoint.

Should Rust adopt the Elixir behavior?

- No.

Recommended follow-up:

- Keep the richer issue detail payloads and mention them accurately in operator docs.

### Documentation drift

What Elixir does:

- Elixir does not claim to exceed Rust on web observability; it simply verifies the behavior it has.

What Rust does today:

- The Rust README says "Rust now covers both observability surfaces that mattered in Elixir and exceeds them on the web path" in `rust-todoist/README.md:149`.
- That claim is too strong while the two concrete gaps above remain.

Operator impact:

- This creates the wrong expectation for operators and maintainers reading the runtime docs.

Should Rust adopt the Elixir behavior?

- Yes, in the sense that the docs should describe actual behavior rather than aspirational parity.

Recommended follow-up:

- Soften the README parity claim until timeout semantics and degraded HTML handling are fixed.
- After those fixes land, re-evaluate whether "exceeds Elixir on the web path" is still the right phrasing.

## Recommended Backlog

1. Preserve `snapshot_timeout` versus `snapshot_unavailable` in Rust operator surfaces.
2. Make `/` render degraded HTML instead of JSON on first-load snapshot failure.
3. Update Rust observability docs and comments to describe the above degraded/error behavior precisely once it changes.
4. Soften the current README parity claim until the operator-facing gaps are closed.

## Bottom Line

Rust observability is already strong. The remaining work is not a broad parity port; it is a focused cleanup of degraded-mode semantics and operator messaging.

Rust should not copy Elixir wholesale. It should keep its current advantages:

- alternate-screen terminal rendering
- viewport clipping
- SSE stream support
- Todoist rate-budget visibility
- richer issue/detail payloads
- broader recent-event history

The correct next step is to add the two missing degraded-mode behaviors from Elixir while preserving the Rust-specific improvements.
