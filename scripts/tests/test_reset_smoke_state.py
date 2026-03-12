import importlib.util
from pathlib import Path
import unittest


MODULE_PATH = Path(__file__).resolve().parents[1] / "reset_smoke_state.py"
SPEC = importlib.util.spec_from_file_location("reset_smoke_state", MODULE_PATH)
reset_smoke_state = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(reset_smoke_state)


class ResetSmokeStateRetryTests(unittest.TestCase):
    def test_smoke_baseline_files_include_workspace_skills(self):
        expected = {
            "AGENTS.md",
            ".codex/skills/commit/SKILL.md",
            ".codex/skills/pull/SKILL.md",
            ".codex/skills/push/SKILL.md",
            ".codex/skills/land/SKILL.md",
            ".codex/skills/land/land_watch.py",
        }
        self.assertTrue(
            expected.issubset(set(reset_smoke_state.SMOKE_BASELINE_FILES))
        )

    def test_http_status_is_transient_recognizes_rate_limits(self):
        self.assertTrue(
            reset_smoke_state.http_status_is_transient(
                403, '{"message":"You have exceeded a secondary rate limit."}'
            )
        )
        self.assertTrue(
            reset_smoke_state.http_status_is_transient(
                429, '{"message":"API rate limit exceeded"}'
            )
        )
        self.assertFalse(
            reset_smoke_state.http_status_is_transient(
                403, '{"message":"Resource not accessible by integration"}'
            )
        )

    def test_http_retry_delay_seconds_caps_large_retry_after_hints(self):
        self.assertEqual(
            reset_smoke_state.http_retry_delay_seconds(
                429, '{"retry_after":1024}', {}, 0, 0
            ),
            60,
        )
        self.assertEqual(
            reset_smoke_state.http_retry_delay_seconds(502, "{}", {}, 1, 2),
            4,
        )

    def test_http_retry_after_seconds_uses_larger_header_or_body_hint(self):
        headers = {"Retry-After": "12"}
        self.assertEqual(
            reset_smoke_state.http_retry_after_seconds(
                '{"error_extra":{"retry_after":30}}', headers
            ),
            30,
        )


if __name__ == "__main__":
    unittest.main()
