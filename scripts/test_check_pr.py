import runpy
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
check = runpy.run_path(str(Path(__file__).with_name("check-pr.py")), run_name="check_pr")
problems = check["problems"]

FILLED = """## What this changes

It does the thing.

## Testing

Ran the tests.
"""


class CheckPr(unittest.TestCase):
    def test_a_filled_description_passes(self):
        self.assertEqual(problems(FILLED), [])

    def test_the_unfilled_template_fails(self):
        template = (ROOT / ".github/pull_request_template.md").read_text()
        found = problems(template)
        self.assertIn("'## What this changes'", found[0])
        self.assertIn("'## Testing'", found[1])

    def test_missing_sections_fail(self):
        found = problems("## What this changes\n\nOnly one section.\n")
        self.assertEqual(len(found), 1)
        self.assertIn("'## Testing'", found[0])

    def test_comments_do_not_count_as_text(self):
        body = "## What this changes\n\n<!-- required -->\n\n## Testing\n\n<!-- how -->\n"
        self.assertEqual(len(problems(body)), 2)

    def test_headings_inside_a_section_do_not_break_it(self):
        body = FILLED + "\n### Notes\n\nMore detail.\n"
        self.assertEqual(problems(body), [])

    def test_ai_attribution_is_refused(self):
        for credit in (
            "Generated with [Claude Code]",
            "Co-Authored-By: Claude <noreply@anthropic.com>",
            "Written with GitHub Copilot.",
            "\U0001F916",
        ):
            self.assertTrue(problems(FILLED + credit), credit)


if __name__ == "__main__":
    unittest.main()
