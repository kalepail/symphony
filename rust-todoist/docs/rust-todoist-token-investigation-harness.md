# Rust Todoist Token Investigation Harness

This harness is the operator path for explaining runtime and token deltas between the Rust Todoist runtime and the Elixir baseline.

## Goal

Use one controlled experiment matrix and one repeatable log-analysis step so a paired run can answer:

- whether the token gap is real or accounting drift
- whether Rust prompt construction is larger than Elixir for the same task intent
- whether Rust is spending extra turns or looping on the same external actions
- whether Todoist-side retries, refreshes, or workpad/comment scans are adding avoidable churn

## Required Run Controls

Keep these fixed across the pair:

- same repo and branch state
- same task intent
- same model
- same reasoning effort
- same workflow revision unless the experiment explicitly isolates prompt differences

The tracker backend is the only intentional baseline difference for the default Rust-vs-Elixir comparison.

## Experiment Matrix

Run these in order:

1. Baseline pair
- Rust Todoist runtime
- Elixir baseline runtime

2. Accounting validation pair
- confirm both runs emit raw `thread/tokenUsage/updated.total` events
- confirm dashboard totals match the parsed token stream totals

3. Context-isolation pair
- Rust with normal Todoist comment/workpad preload
- Rust with minimized preload for the same task

4. Tool-isolation pair
- Rust with normal tool behavior
- Rust with repeated GitHub action diagnosis enabled and the log slice preserved

## Required Log Signals

The Rust runtime now emits these signals for every active turn:

- `codex_context=status=turn_start`
  - prompt byte breakdown
  - comment preload count and truncation flag
  - continuation reason
  - dynamic tool bytes

- `codex_turn_summary`
  - tool call counts by tool and action
  - repeated action summary
  - command execution count
  - last structured tool failure fields

- `dynamic_tool_call`
  - tool
  - action
  - duration
  - upstream service
  - error kind
  - HTTP status
  - retry-after seconds

## Analysis Script

Generate the paired-run report with:

```bash
python rust-todoist/scripts/compare_runtime_logs.py \
  --rust-log rust-todoist/log/symphony.log \
  --elixir-log elixir/log/symphony.log.1 \
  --rust-label "Rust Todoist" \
  --elixir-label "Elixir baseline"
```

The script emits a Markdown report with:

- one paired-run timeline table
- one per-turn token table
- one per-tool-call frequency table
- one ranked hypothesis list with evidence
- one confirmed-causes section
- one still-open section

If the Rust log contains multiple orchestrator runs, pass the explicit Rust `run_id`:

```bash
python rust-todoist/scripts/compare_runtime_logs.py \
  --rust-log rust-todoist/log/symphony.log \
  --elixir-log elixir/log/symphony.log.1 \
  --rust-run-id run-123 \
  --rust-label "Rust Todoist" \
  --elixir-label "Elixir baseline"
```

Without `--rust-run-id`, the script now fails fast on ambiguous Rust logs instead of silently merging multiple runs.

## Known Limits

- Rust file logs now carry `run_id` on the lifecycle lines consumed by the harness. Elixir file logs still do not emit equivalent `run_id`, prompt-byte, tool-count, or repeated-action markers.
- The log-based report therefore treats unsupported Elixir tool and prompt metrics as `n/a`, not `0`.
- For true Rust-vs-Elixir parity on prompt-context and tool-behavior comparisons, prefer observability snapshots or JSON exports over rotating file logs.
- Token accounting policy for `turn/completed.usage` remains a shared Rust-and-Elixir follow-up. The current Rust work records token source explicitly but does not make a Rust-only semantic change there.

## Acceptance

The harness is working when a single paired run can explain most of the observed delta with direct evidence from:

- token source and totals
- prompt byte breakdown
- continuation count
- repeated external actions
- retry and blocking signals
