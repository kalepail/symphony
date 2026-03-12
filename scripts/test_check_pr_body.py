from __future__ import annotations

import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check_pr_body.py")

TEMPLATE = textwrap.dedent(
    """\
    #### Context

    <!-- Why is this change needed? -->

    #### TL;DR

    *<!-- A short summary -->*

    #### Summary

    - <!-- Summary bullet -->

    #### Alternatives

    - <!-- Alternative bullet -->

    #### Test Plan

    - [ ] <!-- Test checkbox -->
    """
)

VALID_BODY = textwrap.dedent(
    """\
    #### Context

    Context text.

    #### TL;DR

    Short summary.

    #### Summary

    - First change.

    #### Alternatives

    - Alternative considered.

    #### Test Plan

    - [x] Ran targeted checks.
    """
)


class CheckPrBodyTest(unittest.TestCase):
    def run_checker(self, repo_root: Path, body: str, template: str | None = TEMPLATE):
        if template is not None:
            (repo_root / ".github").mkdir(parents=True, exist_ok=True)
            (repo_root / ".github" / "pull_request_template.md").write_text(
                template, encoding="utf-8"
            )
        body_path = repo_root / "body.md"
        body_path.write_text(body, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--file", str(body_path)],
            cwd=repo_root,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_valid_body_passes(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            result = self.run_checker(Path(temp_dir), VALID_BODY)
        self.assertEqual(result.returncode, 0)
        self.assertIn("PR body format OK", result.stdout)

    def test_missing_template_fails(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            result = self.run_checker(Path(temp_dir), VALID_BODY, template=None)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unable to read PR template", result.stderr)

    def test_placeholders_fail(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            result = self.run_checker(Path(temp_dir), TEMPLATE)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("placeholder comments", result.stderr)

    def test_out_of_order_headings_fail(self):
        body = textwrap.dedent(
            """\
            #### TL;DR

            Short summary.

            #### Context

            Context text.

            #### Summary

            - First change.

            #### Alternatives

            - Alternative considered.

            #### Test Plan

            - [x] Ran targeted checks.
            """
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            result = self.run_checker(Path(temp_dir), body)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Required headings are out of order.", result.stderr)

    def test_bullet_and_checkbox_expectations_fail(self):
        body = textwrap.dedent(
            """\
            #### Context

            Context text.

            #### TL;DR

            Short summary.

            #### Summary

            Not a bullet.

            #### Alternatives

            Also not a bullet.

            #### Test Plan

            No checkbox.
            """
        )
        with tempfile.TemporaryDirectory() as temp_dir:
            result = self.run_checker(Path(temp_dir), body)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Section must include at least one bullet item: #### Summary", result.stderr)
        self.assertIn(
            "Section must include at least one checkbox item: #### Test Plan", result.stderr
        )


if __name__ == "__main__":
    unittest.main()
