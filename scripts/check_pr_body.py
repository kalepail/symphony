#!/usr/bin/env python3

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


DEFAULT_TEMPLATE_PATHS = (
    Path(".github/pull_request_template.md"),
    Path("../.github/pull_request_template.md"),
)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Validate a PR description markdown file against the repository PR template.",
    )
    parser.add_argument("--file", required=True, help="Path to the PR body markdown file")
    parser.add_argument(
        "--template",
        help="Optional path to the PR template. Defaults to .github/pull_request_template.md",
    )
    return parser.parse_args(argv)


def read_template(path: str | None) -> tuple[Path, str]:
    candidates = [Path(path)] if path else list(DEFAULT_TEMPLATE_PATHS)
    for candidate in candidates:
        try:
            return candidate, candidate.read_text(encoding="utf-8")
        except FileNotFoundError:
            continue
    joined = ", ".join(str(path) for path in candidates)
    raise ValidationError(f"Unable to read PR template from any of: {joined}")


def read_file(path: str) -> str:
    file_path = Path(path)
    try:
        return file_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise ValidationError(f"Unable to read {path}: {exc}") from exc


def extract_template_headings(template: str, template_path: Path) -> list[str]:
    headings = re.findall(r"^#{4,6}\s+.+$", template, flags=re.MULTILINE)
    if not headings:
        raise ValidationError(f"No markdown headings found in {template_path}")
    return headings


def lint(template: str, body: str, headings: list[str]) -> list[str]:
    errors: list[str] = []
    errors.extend(check_required_headings(body, headings))
    errors.extend(check_order(body, headings))
    errors.extend(check_no_placeholders(body))
    errors.extend(check_sections_from_template(template, body, headings))
    return errors


def check_required_headings(body: str, headings: list[str]) -> list[str]:
    missing = [heading for heading in headings if heading_position(body, heading) is None]
    return [f"Missing required heading: {heading}" for heading in missing]


def check_order(body: str, headings: list[str]) -> list[str]:
    positions = [
        position
        for heading in headings
        if (position := heading_position(body, heading)) is not None
    ]
    if positions == sorted(positions):
        return []
    return ["Required headings are out of order."]


def check_no_placeholders(body: str) -> list[str]:
    if "<!--" in body:
        return ["PR description still contains template placeholder comments (<!-- ... -->)."]
    return []


def check_sections_from_template(template: str, body: str, headings: list[str]) -> list[str]:
    errors: list[str] = []
    for heading in headings:
        template_section = capture_heading_section(template, heading, headings)
        body_section = capture_heading_section(body, heading, headings)
        if body_section is None:
            continue
        if body_section.strip() == "":
            errors.append(f"Section cannot be empty: {heading}")
            continue

        requires_bullets = bool(re.search(r"^- ", template_section or "", flags=re.MULTILINE))
        if requires_bullets and not re.search(r"^- ", body_section, flags=re.MULTILINE):
            errors.append(f"Section must include at least one bullet item: {heading}")

        requires_checkboxes = bool(
            re.search(r"^- \[ \] ", template_section or "", flags=re.MULTILINE)
        )
        if requires_checkboxes and not re.search(
            r"^- \[[ xX]\] ", body_section, flags=re.MULTILINE
        ):
            errors.append(f"Section must include at least one checkbox item: {heading}")
    return errors


def heading_position(body: str, heading: str) -> int | None:
    index = body.find(heading)
    return None if index < 0 else index


def capture_heading_section(doc: str, heading: str, headings: list[str]) -> str | None:
    heading_index = doc.find(heading)
    if heading_index < 0:
        return None
    section_start = heading_index + len(heading)
    if section_start + 2 > len(doc):
        return ""
    if doc[section_start : section_start + 2] != "\n\n":
        return None

    content = doc[section_start + 2 :]
    offsets = [
        offset
        for marker in headings_after(heading, headings)
        if (offset := content.find(marker)) >= 0
    ]
    if not offsets:
        return content
    return content[: min(offsets)]


def headings_after(current_heading: str, headings: list[str]) -> list[str]:
    return [f"\n{heading}" for heading in headings if heading != current_heading]


class ValidationError(RuntimeError):
    pass


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    try:
        template_path, template = read_template(args.template)
        body = read_file(args.file)
        headings = extract_template_headings(template, template_path)
        errors = lint(template, body, headings)
    except ValidationError as exc:
        print(str(exc), file=sys.stderr)
        return 1

    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        print(
            f"PR body format invalid. Read `{template_path}` and follow it precisely.",
            file=sys.stderr,
        )
        return 1

    print("PR body format OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
