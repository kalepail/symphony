#!/usr/bin/env python3

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import sys
from collections import Counter
from pathlib import Path
from typing import Any


KV_RE = re.compile(r"([A-Za-z0-9_]+)=([^\s]+)")
TS_RE = re.compile(r"^(\d{4}-\d{2}-\d{2}T[^\s]+)")
TURN_RE = re.compile(r"turn=(\d+)/(\d+)")

RUST_RUN_MARKERS = (
    "dispatch=status=started",
    "worker=status=starting",
    "worker=status=completed",
    "worker=status=quiesced",
    "worker=status=failed",
    "codex_context=status=turn_start",
    "codex_turn_summary",
    "retry=status=queued",
    "reconcile=status=stalled",
    "dynamic_tool_call",
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compare Symphony runtime logs for Rust Todoist and Elixir baselines."
    )
    parser.add_argument("--rust-log", required=True, help="Path to the Rust runtime log file.")
    parser.add_argument(
        "--elixir-log", required=True, help="Path to the Elixir runtime log file."
    )
    parser.add_argument(
        "--rust-label", default="Rust Todoist", help="Display label for the Rust run."
    )
    parser.add_argument(
        "--elixir-label", default="Elixir baseline", help="Display label for the Elixir run."
    )
    parser.add_argument(
        "--rust-run-id",
        help="Explicit Rust run_id to analyze when the Rust log contains multiple runs.",
    )
    parser.add_argument(
        "--elixir-run-id",
        help="Explicit Elixir run_id to analyze if the Elixir log emits one.",
    )
    return parser.parse_args()


def build_run(
    label: str,
    path: Path,
    runtime: str,
    *,
    run_id: str | None,
    supports_structured_metrics: bool,
) -> dict[str, Any]:
    return {
        "label": label,
        "path": str(path),
        "runtime": runtime,
        "run_id": run_id,
        "supports_structured_metrics": supports_structured_metrics,
        "start_at": None,
        "end_at": None,
        "outcome": "unknown",
        "continuations": 0,
        "turns": [],
        "tool_counts": Counter(),
        "tool_status_counts": Counter(),
        "action_counts": Counter(),
        "service_retries": Counter(),
        "blocked_events": [],
        "token_updates": [],
        "token_total": 0,
        "token_source": None,
        "summary_statuses": [],
        "notes": [],
    }


def build_turn(turn_number: int, *, kind: str | None = None) -> dict[str, Any]:
    return {
        "turn_number": turn_number,
        "kind": kind,
        "continuation_reason": None,
        "prompt_bytes": None,
        "workflow_template_bytes": None,
        "issue_context_bytes": None,
        "preloaded_comment_bytes": None,
        "preloaded_comment_count": None,
        "comment_preload_truncated": None,
        "dynamic_tool_bytes": None,
        "started_at": None,
        "summary_status": None,
        "tool_calls_by_tool": {},
        "tool_calls_by_action": {},
        "repeated_actions": [],
        "command_execution_count": None,
        "error_kind": None,
        "http_status": None,
        "retry_after_secs": None,
        "upstream_service": None,
        "token_total": None,
        "token_delta": None,
        "token_source": None,
    }


def parse_timestamp(line: str) -> dt.datetime | None:
    match = TS_RE.search(line)
    if not match:
        return None
    raw = match.group(1)
    try:
        return dt.datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError:
        return None


def parse_kv_pairs(line: str) -> dict[str, str]:
    pairs: dict[str, str] = {}
    for key, value in KV_RE.findall(line):
        pairs[key] = value.strip().strip('"')
    return pairs


def parse_json_fragment(line: str) -> Any | None:
    start = line.find("{")
    if start < 0:
        return None
    fragment = line[start:].strip()
    try:
        return json.loads(fragment)
    except json.JSONDecodeError:
        return None


def stringify(value: Any) -> str:
    if value is None:
        return "n/a"
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def parse_compact_map(raw: str | None) -> dict[str, int]:
    if not raw or raw in {"n/a", "none", "{}"}:
        return {}
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    if not isinstance(data, dict):
        return {}

    result: dict[str, int] = {}
    for key, value in data.items():
        try:
            result[str(key)] = int(value)
        except (TypeError, ValueError):
            continue
    return result


def parse_token_usage_payload(payload: Any) -> dict[str, Any] | None:
    if not isinstance(payload, dict):
        return None

    method = payload.get("method")
    if method == "thread/tokenUsage/updated":
        total = payload.get("params", {}).get("tokenUsage", {}).get("total", {})
        if isinstance(total, dict):
            return {
                "source": "thread/tokenUsage/updated.total",
                "input_tokens": int(
                    total.get("inputTokens")
                    or total.get("input_tokens")
                    or total.get("prompt_tokens")
                    or 0
                ),
                "output_tokens": int(
                    total.get("outputTokens")
                    or total.get("output_tokens")
                    or total.get("completion_tokens")
                    or 0
                ),
                "total_tokens": int(
                    total.get("totalTokens") or total.get("total_tokens") or total.get("total") or 0
                ),
            }

    if method == "turn/completed":
        usage = payload.get("params", {}).get("usage") or payload.get("usage")
        if isinstance(usage, dict):
            return {
                "source": "turn/completed.usage",
                "input_tokens": int(usage.get("inputTokens") or usage.get("input_tokens") or 0),
                "output_tokens": int(
                    usage.get("outputTokens") or usage.get("output_tokens") or 0
                ),
                "total_tokens": int(usage.get("totalTokens") or usage.get("total_tokens") or 0),
            }
    return None


def ensure_turn(
    run: dict[str, Any],
    turn_by_number: dict[int, dict[str, Any]],
    turn_number: int,
    *,
    kind: str | None = None,
) -> dict[str, Any]:
    turn = turn_by_number.get(turn_number)
    if turn is None:
        turn = build_turn(turn_number, kind=kind)
        run["turns"].append(turn)
        turn_by_number[turn_number] = turn
    elif kind and not turn.get("kind"):
        turn["kind"] = kind
    return turn


def parse_rust_log(path: Path, label: str, selected_run_id: str | None) -> dict[str, Any]:
    runs_by_run_id: dict[str, dict[str, Any]] = {}
    turns_by_run_id: dict[str, dict[int, dict[str, Any]]] = {}

    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        timestamp = parse_timestamp(line)
        kv = parse_kv_pairs(line)
        payload = parse_json_fragment(line)
        token_usage = parse_token_usage_payload(payload)
        line_run_id = kv.get("run_id")

        if line_run_id:
            run = runs_by_run_id.setdefault(
                line_run_id,
                build_run(
                    label,
                    path,
                    "rust",
                    run_id=line_run_id,
                    supports_structured_metrics=True,
                ),
            )
        elif selected_run_id and selected_run_id in runs_by_run_id:
            run = runs_by_run_id[selected_run_id]
        elif len(runs_by_run_id) == 1:
            run = next(iter(runs_by_run_id.values()))
        else:
            run = None

        if run is None:
            if any(marker in line for marker in RUST_RUN_MARKERS) and len(runs_by_run_id) > 1:
                raise ValueError(
                    f"multiple Rust runs found in {path}; rerun with --rust-run-id"
                )
            continue

        turn_by_number = turns_by_run_id.setdefault(run["run_id"] or "unknown", {})

        if "worker=status=starting" in line:
            run["start_at"] = run["start_at"] or timestamp

        if "worker=status=completed" in line:
            run["end_at"] = timestamp
            run["outcome"] = (
                "continued" if kv.get("continuation") == "queued" else "completed"
            )
        elif "worker=status=quiesced" in line:
            run["end_at"] = timestamp
            run["outcome"] = "completed"
        elif "worker=status=failed" in line:
            run["end_at"] = timestamp
            run["outcome"] = "failed"

        if "reconcile=status=stalled" in line:
            run["blocked_events"].append(
                f"{timestamp.isoformat() if timestamp else 'n/a'} stalled"
            )
            run["outcome"] = "stalled"

        if "dispatch=status=blocked" in line or "todoist_sync=status=dispatch_blocked" in line:
            run["blocked_events"].append(line.strip())

        if "retry=status=queued" in line:
            run["service_retries"]["orchestrator"] += 1

        if "github_api status=retry" in line:
            run["service_retries"]["github"] += 1
        if (
            "Todoist request was rate limited" in line
            or "retry_after" in line
            and "todoist" in line.lower()
        ):
            run["service_retries"]["todoist"] += 1

        if "codex_context=status=turn_start" in line:
            turn_number = int(kv.get("turn", "0") or "0")
            turn = ensure_turn(run, turn_by_number, turn_number, kind=kv.get("kind"))
            turn["continuation_reason"] = kv.get("continuation_reason")
            turn["prompt_bytes"] = int(kv.get("prompt_bytes", "0") or "0")
            turn["workflow_template_bytes"] = kv.get("workflow_template_bytes")
            turn["issue_context_bytes"] = kv.get("issue_context_bytes")
            turn["preloaded_comment_bytes"] = kv.get("preloaded_comment_bytes")
            turn["preloaded_comment_count"] = kv.get("preloaded_comment_count")
            turn["comment_preload_truncated"] = kv.get("comment_preload_truncated")
            turn["dynamic_tool_bytes"] = kv.get("dynamic_tool_bytes")
            turn["started_at"] = timestamp
            if kv.get("kind") == "continuation":
                run["continuations"] += 1

        if "codex_turn_summary" in line:
            turn_number = int(kv.get("turn", "0") or "0")
            turn = ensure_turn(run, turn_by_number, turn_number)
            turn["summary_status"] = kv.get("status")
            turn["tool_calls_by_tool"] = parse_compact_map(kv.get("tool_calls_by_tool"))
            turn["tool_calls_by_action"] = parse_compact_map(kv.get("tool_calls_by_action"))
            repeated = kv.get("repeated_actions")
            turn["repeated_actions"] = (
                [] if not repeated or repeated == "none" else [item.strip() for item in repeated.split("|")]
            )
            turn["command_execution_count"] = kv.get("command_execution_count")
            turn["error_kind"] = kv.get("last_error_kind")
            turn["http_status"] = kv.get("http_status")
            turn["retry_after_secs"] = kv.get("retry_after_secs")
            turn["upstream_service"] = kv.get("upstream_service")
            run["summary_statuses"].append(kv.get("status", "unknown"))

        if "dynamic_tool_call" in line:
            tool = kv.get("tool", "unknown")
            action = kv.get("action", "unknown")
            status = kv.get("status", "unknown")
            if action == "unknown" and tool != "unknown":
                action = tool
            run["tool_counts"][tool] += 1
            run["tool_status_counts"][f"{tool}:{status}"] += 1
            if action != "unknown":
                run["action_counts"][action] += 1

        if token_usage:
            run["token_updates"].append(
                {
                    "timestamp": timestamp,
                    "source": token_usage["source"],
                    "total_tokens": token_usage["total_tokens"],
                }
            )
            if token_usage["total_tokens"] >= run["token_total"]:
                run["token_total"] = token_usage["total_tokens"]
                run["token_source"] = token_usage["source"]

    if selected_run_id:
        if selected_run_id not in runs_by_run_id:
            raise ValueError(f"Rust run_id `{selected_run_id}` was not found in {path}")
        run = runs_by_run_id[selected_run_id]
    elif not runs_by_run_id:
        run = build_run(
            label,
            path,
            "rust",
            run_id=None,
            supports_structured_metrics=True,
        )
        run["notes"].append("No Rust run_id-marked lifecycle lines were found in the log.")
    elif len(runs_by_run_id) == 1:
        run = next(iter(runs_by_run_id.values()))
    else:
        raise ValueError(f"multiple Rust runs found in {path}; rerun with --rust-run-id")

    assign_tokens_to_turns(run)
    infer_outcome(run)
    return run


def parse_elixir_log(path: Path, label: str, selected_run_id: str | None) -> dict[str, Any]:
    run = build_run(
        label,
        path,
        "elixir",
        run_id=None,
        supports_structured_metrics=False,
    )
    turn_by_number: dict[int, dict[str, Any]] = {}
    run["notes"].append(
        "Elixir file logs do not emit Rust-equivalent prompt and tool telemetry; unsupported fields are reported as n/a."
    )

    if selected_run_id:
        run["notes"].append(
            "Elixir run_id selection was requested, but current Elixir file logs do not emit run_id markers."
        )

    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        timestamp = parse_timestamp(line)
        payload = parse_json_fragment(line)
        token_usage = parse_token_usage_payload(payload)

        if "Dispatching issue to agent:" in line or "Codex session started for" in line:
            run["start_at"] = run["start_at"] or timestamp

        if "Agent task completed for issue_id=" in line:
            run["end_at"] = timestamp
            run["outcome"] = "completed"
        elif "Agent task exited for issue_id=" in line:
            run["end_at"] = timestamp
            run["outcome"] = "failed"
        elif "Agent task finished for issue_id=" in line:
            run["end_at"] = timestamp
            run["outcome"] = "completed" if "reason=:normal" in line else "failed"
        elif "Codex session ended with error for" in line:
            run["end_at"] = timestamp
            run["outcome"] = "failed"

        if "Issue stalled:" in line:
            run["blocked_events"].append(
                f"{timestamp.isoformat() if timestamp else 'n/a'} stalled"
            )
            run["outcome"] = "stalled"

        if "Retrying issue_id=" in line:
            run["service_retries"]["orchestrator"] += 1

        turn_match = TURN_RE.search(line)
        if turn_match:
            turn_number = int(turn_match.group(1))
            kind = "initial" if turn_number == 1 else "continuation"
            turn = ensure_turn(run, turn_by_number, turn_number, kind=kind)
            if turn["started_at"] is None:
                turn["started_at"] = timestamp
            if "Continuing agent run for" in line:
                turn["continuation_reason"] = "issue_still_active_after_turn"
                run["continuations"] = max(run["continuations"], turn_number - 1)
            elif "Completed agent run for" in line:
                turn["summary_status"] = "completed"

        if token_usage:
            run["token_updates"].append(
                {
                    "timestamp": timestamp,
                    "source": token_usage["source"],
                    "total_tokens": token_usage["total_tokens"],
                }
            )
            if token_usage["total_tokens"] >= run["token_total"]:
                run["token_total"] = token_usage["total_tokens"]
                run["token_source"] = token_usage["source"]

    assign_tokens_to_turns(run)
    infer_outcome(run)
    return run


def assign_tokens_to_turns(run: dict[str, Any]) -> None:
    turns = run["turns"]
    updates = run["token_updates"]
    if not turns or not updates:
        return

    previous_total = 0
    for index, turn in enumerate(turns):
        start = turn.get("started_at")
        end = turns[index + 1].get("started_at") if index + 1 < len(turns) else None
        candidates = []
        for update in updates:
            timestamp = update["timestamp"]
            if timestamp is None:
                continue
            if start and timestamp < start:
                continue
            if end and timestamp >= end:
                continue
            candidates.append(update)
        if not candidates:
            continue
        last = candidates[-1]
        turn["token_total"] = last["total_tokens"]
        turn["token_delta"] = last["total_tokens"] - previous_total
        turn["token_source"] = last["source"]
        previous_total = last["total_tokens"]


def infer_outcome(run: dict[str, Any]) -> None:
    if run["outcome"] != "unknown":
        return
    if run["blocked_events"]:
        run["outcome"] = "api-blocked"
    elif run["turns"]:
        run["outcome"] = "running"


def duration_minutes(run: dict[str, Any]) -> str:
    start = run.get("start_at")
    end = run.get("end_at")
    if not start or not end:
        return "n/a"
    seconds = max((end - start).total_seconds(), 0.0)
    return f"{seconds / 60.0:.1f}"


def most_common_item(counter: Counter[str]) -> tuple[str | None, int]:
    filtered = Counter({key: count for key, count in counter.items() if key != "unknown"})
    if not filtered:
        return None, 0
    item, count = filtered.most_common(1)[0]
    return item, count


def metric_value(run: dict[str, Any], value: int) -> str:
    if not run["supports_structured_metrics"]:
        return "n/a"
    return str(value)


def hypothesis_rows(rust: dict[str, Any], elixir: dict[str, Any]) -> list[tuple[str, str]]:
    rows: list[tuple[str, str]] = []

    if rust["token_total"] and elixir["token_total"] and rust["token_total"] > elixir["token_total"]:
        ratio = rust["token_total"] / max(elixir["token_total"], 1)
        rows.append(
            (
                "Prompt or continuation inflation in Rust",
                f"Rust ended at {rust['token_total']:,} tokens versus {elixir['token_total']:,} for Elixir ({ratio:.1f}x).",
            )
        )

    if rust["supports_structured_metrics"]:
        rust_comment_bytes = sum(
            int(turn["preloaded_comment_bytes"])
            for turn in rust["turns"]
            if stringify(turn.get("preloaded_comment_bytes")).isdigit()
        )
        if rust_comment_bytes > 0:
            rows.append(
                (
                    "Todoist preload volume is visible in Rust",
                    f"Rust logged {rust_comment_bytes:,} preloaded comment bytes across parsed turn starts.",
                )
            )

        rust_top_action, rust_top_count = most_common_item(rust["action_counts"])
        if rust_top_action and rust_top_count > 1:
            rows.append(
                (
                    "Rust repeated the same external action",
                    f"Top repeated Rust action: `{rust_top_action}` x{rust_top_count}.",
                )
            )

    if rust["service_retries"]:
        retry_summary = ", ".join(
            f"{service}={count}" for service, count in rust["service_retries"].most_common()
        )
        rows.append(
            (
                "External API churn contributed to Rust runtime length",
                f"Rust logged retry activity: {retry_summary}.",
            )
        )

    if not elixir["supports_structured_metrics"]:
        rows.append(
            (
                "Elixir file logs do not support structured prompt/tool parity",
                "Use observability snapshots or additional Elixir instrumentation before drawing per-tool or per-context conclusions.",
            )
        )

    if not rows:
        rows.append(
            (
                "Comparison harness needs more paired-run data",
                "The parsed logs did not expose enough signal to rank causes confidently.",
            )
        )
    return rows


def render_table(headers: list[str], rows: list[list[str]]) -> str:
    lines = [
        "| " + " | ".join(headers) + " |",
        "| " + " | ".join("---" for _ in headers) + " |",
    ]
    for row in rows:
        lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


def render_notes(run: dict[str, Any]) -> list[str]:
    notes = [f"- {note}" for note in run["notes"]]
    if not notes:
        notes.append("- none")
    return notes


def render_report(rust: dict[str, Any], elixir: dict[str, Any]) -> str:
    timeline_rows = [
        [
            rust["label"],
            stringify(rust["run_id"]),
            stringify(rust["start_at"].isoformat() if rust["start_at"] else None),
            stringify(rust["end_at"].isoformat() if rust["end_at"] else None),
            duration_minutes(rust),
            str(len(rust["turns"])),
            rust["outcome"],
            f"{rust['token_total']:,}" if rust["token_total"] else "n/a",
            stringify(rust["token_source"]),
        ],
        [
            elixir["label"],
            stringify(elixir["run_id"]),
            stringify(elixir["start_at"].isoformat() if elixir["start_at"] else None),
            stringify(elixir["end_at"].isoformat() if elixir["end_at"] else None),
            duration_minutes(elixir),
            str(len(elixir["turns"])),
            elixir["outcome"],
            f"{elixir['token_total']:,}" if elixir["token_total"] else "n/a",
            stringify(elixir["token_source"]),
        ],
    ]

    token_rows: list[list[str]] = []
    for run in [rust, elixir]:
        if not run["turns"]:
            token_rows.append([run["label"], "-", "-", "-", "-", "-", "-"])
            continue
        for turn in run["turns"]:
            token_rows.append(
                [
                    run["label"],
                    str(turn["turn_number"]),
                    stringify(turn.get("kind")),
                    stringify(turn.get("continuation_reason")),
                    stringify(turn.get("token_total")),
                    stringify(turn.get("token_delta")),
                    stringify(turn.get("token_source")),
                ]
            )

    tool_keys = sorted(set(rust["tool_counts"]) | set(elixir["tool_counts"]))
    if not tool_keys:
        tool_rows = [["(no structured tool data)", "n/a", "n/a", "n/a", "n/a"]]
    else:
        tool_rows = [
            [
                key,
                metric_value(rust, rust["tool_counts"].get(key, 0)),
                metric_value(elixir, elixir["tool_counts"].get(key, 0)),
                metric_value(rust, rust["tool_status_counts"].get(f"{key}:failed", 0)),
                metric_value(elixir, elixir["tool_status_counts"].get(f"{key}:failed", 0)),
            ]
            for key in tool_keys
        ]

    hypotheses = hypothesis_rows(rust, elixir)
    hypothesis_lines = [
        f"{index + 1}. {title}: {evidence}"
        for index, (title, evidence) in enumerate(hypotheses)
    ]

    confirmed: list[str] = []
    if rust["continuations"] > elixir["continuations"]:
        confirmed.append(
            f"Rust used more continuation turns ({rust['continuations']}) than {elixir['label']} ({elixir['continuations']})."
        )
    action, count = most_common_item(rust["action_counts"])
    if action and count > 1:
        confirmed.append(f"Rust repeated `{action}` {count} times in the parsed log slice.")
    if rust["blocked_events"]:
        confirmed.append("Rust encountered tracker or dispatch blocking during the run.")
    if not confirmed:
        confirmed.append("No confirmed cause yet; the harness captured baseline metrics only.")

    open_questions = []
    if not rust["token_updates"] or not elixir["token_updates"]:
        open_questions.append(
            "At least one run did not emit parseable absolute token updates in the selected log slice; confirm raw `thread/tokenUsage/updated` visibility."
        )
    open_questions.append(
        "Validate whether Todoist preload/workpad reads are larger than Linear context using observability snapshots, not file logs alone."
    )
    open_questions.append(
        "Compare repeated GitHub and tracker actions under the same model and reasoning-effort settings."
    )

    return "\n".join(
        [
            "# Runtime Comparison Report",
            "",
            "## Inputs",
            f"- Rust log: `{rust['path']}`",
            f"- Elixir log: `{elixir['path']}`",
            "",
            "## Notes",
            *render_notes(rust),
            *render_notes(elixir),
            "",
            "## Paired-Run Timeline",
            render_table(
                [
                    "Run",
                    "Run ID",
                    "Start",
                    "End",
                    "Runtime (min)",
                    "Turns",
                    "Outcome",
                    "Final token total",
                    "Final token source",
                ],
                timeline_rows,
            ),
            "",
            "## Per-Turn Tokens",
            render_table(
                [
                    "Run",
                    "Turn",
                    "Kind",
                    "Continuation reason",
                    "Token total",
                    "Token delta",
                    "Token source",
                ],
                token_rows,
            ),
            "",
            "## Tool Frequency",
            render_table(
                [
                    "Tool",
                    f"{rust['label']} calls",
                    f"{elixir['label']} calls",
                    f"{rust['label']} failed",
                    f"{elixir['label']} failed",
                ],
                tool_rows,
            ),
            "",
            "## Ranked Hypotheses",
            *hypothesis_lines,
            "",
            "## Confirmed Causes",
            *[f"- {item}" for item in confirmed],
            "",
            "## Still Open",
            *[f"- {item}" for item in open_questions],
            "",
        ]
    )


def main() -> int:
    args = parse_args()
    try:
        rust = parse_rust_log(Path(args.rust_log), args.rust_label, args.rust_run_id)
        elixir = parse_elixir_log(Path(args.elixir_log), args.elixir_label, args.elixir_run_id)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    print(render_report(rust, elixir))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
