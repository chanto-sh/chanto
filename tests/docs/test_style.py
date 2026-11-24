import importlib.util
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "docs_style", ROOT / "scripts" / "docs_style.py"
)
docs_style = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(docs_style)


class Style(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def page(self, body, name="page.md"):
        path = Path(self.dir.name) / name
        path.write_text(body)
        return path

    def reasons(self, body, name="page.md"):
        return [why for _, why in docs_style.findings(self.page(body, name))]

    def test_flags_a_benefit_tail(self):
        reasons = self.reasons("This works, so it stays fast.\n")
        self.assertTrue(any("'so it'" in r for r in reasons), reasons)

    def test_flags_a_long_sentence(self):
        reasons = self.reasons(" ".join(["word"] * 45) + "\n")
        self.assertTrue(any("45-word sentence" in r for r in reasons), reasons)

    def test_flags_banner_words(self):
        reasons = self.reasons("It is simply seamless, and more.\n")
        for phrase in ("'simply'", "'seamless'", "'and more'"):
            self.assertTrue(any(phrase in r for r in reasons), (phrase, reasons))

    def test_ignores_tables_and_code(self):
        body = (
            "| so it | seamless |\n\n"
            "```\nso it simply works, and more\n```\n\n"
            "Run `so it simply works` here.\n"
        )
        self.assertEqual(self.reasons(body), [])

    def test_the_reference_may_hold_a_long_enumeration(self):
        # A reference lists features; its long sentence is a list, not a pitch.
        body = " ".join(["word"] * 45) + "\n"
        self.assertEqual(self.reasons(body, "reference.md"), [])

    def test_the_pages_the_site_shows_are_clean(self):
        for name in (
            "index.md",
            "quickstart.md",
            "faq.md",
            "models.md",
            "cloud.md",
            "security-model.md",
        ):
            with self.subTest(page=name):
                reasons = [why for _, why in docs_style.findings(docs_style.DOCS / name)]
                self.assertEqual(reasons, [])


if __name__ == "__main__":
    unittest.main()
