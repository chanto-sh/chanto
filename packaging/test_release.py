"""Regression checks for release preflight, using temporary metadata copies."""

from pathlib import Path
import re
import runpy
import shutil
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parent.parent
check = runpy.run_path(str(ROOT / "packaging/check-release.py"))["check"]
changelog = runpy.run_path(str(ROOT / "packaging/changelog.py"))


class ReleaseMetadataTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in (
            "Cargo.toml", "Cargo.lock", "desktop/Cargo.toml",
            "ide/vscode/package.json", "ide/jetbrains/build.gradle.kts",
            "CHANGELOG.md", "install.sh", "install.ps1",
        ):
            target = self.root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        shutil.copytree(ROOT / "src/tui/snapshots/visual", self.root / "src/tui/snapshots/visual")
        self.version = tomllib.loads((self.root / "Cargo.toml").read_text())["package"]["version"]
        self.tag = "v" + self.version

    def replace(self, path, old, new):
        file = self.root / path
        text = file.read_text()
        self.assertIn(old, text)
        file.write_text(text.replace(old, new, 1))

    def test_current_release_passes(self):
        check(self.root, self.tag)

    def test_rejects_wrong_or_malformed_tag(self):
        for tag in ("v99.0.0", "0.1.6", "v1.2.3; echo x"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                check(self.root, tag)

    def test_rejects_editor_version_drift(self):
        self.replace("ide/vscode/package.json", self.version, "99.0.0")
        with self.assertRaisesRegex(ValueError, "VS Code"):
            check(self.root, self.tag)

    def test_rejects_lockfile_version_drift(self):
        self.replace("Cargo.lock", f'name = "chanto"\nversion = "{self.version}"',
                     'name = "chanto"\nversion = "99.0.0"')
        with self.assertRaisesRegex(ValueError, "Cargo.lock"):
            check(self.root, self.tag)

    def test_rejects_undated_notes(self):
        self.replace("CHANGELOG.md", f"## {self.tag} (", "## pending (")
        with self.assertRaisesRegex(ValueError, "dated release"):
            check(self.root, self.tag)

    def test_rejects_installer_floor_drift(self):
        # The floor only moves with a security release, so read it rather than
        # assuming it names this release.
        floor = re.search(
            r"^\$minVersion = '(v[0-9.]+)'$", (self.root / "install.ps1").read_text(), re.MULTILINE
        )[1]
        self.replace("install.sh", f"min_version={floor}", "min_version=v99.0.0")
        with self.assertRaisesRegex(ValueError, "floors disagree"):
            check(self.root, self.tag)

    def test_rejects_stale_welcome(self):
        self.replace("src/tui/snapshots/visual/dark_40x20_welcome.snap", self.version, "0.0.0")
        with self.assertRaisesRegex(ValueError, "stale welcome"):
            check(self.root, self.tag)

    def test_rejects_uncollected_fragments(self):
        (self.root / "changelog.d").mkdir()
        (self.root / "changelog.d/README.md").write_text("how fragments work\n")
        check(self.root, self.tag)
        (self.root / "changelog.d/fix-x.md").write_text("- **Fix.** Something.\n")
        with self.assertRaisesRegex(ValueError, "fix-x.md"):
            check(self.root, self.tag)


class CratesPublishedTests(unittest.TestCase):
    def test_every_workspace_crate_chanto_depends_on_is_published(self):
        """`cargo publish -p chanto` fails on a path dependency crates.io
        doesn't have (v0.1.9 did, on chanto-core): release.yml publishes each."""
        import re
        manifest = (ROOT / "Cargo.toml").read_text()
        deps = manifest.split("[dependencies]", 1)[1].split("\n[", 1)[0]
        path_deps = {
            (m.group(1), m.group(2))
            for m in re.finditer(r'^([A-Za-z0-9_-]+)\s*=\s*\{[^}]*path\s*=\s*"([^"]+)"', deps, re.M)
        }
        self.assertTrue(path_deps, "expected chanto's workspace crates among its dependencies")
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        listed = set(
            (name, path)
            for path, name in re.findall(r"(\w+):(chanto-[\w-]+)", re.search(r'CRATES="([^"]+)"', workflow).group(1))
        )
        self.assertEqual(path_deps, listed)

class ChangelogFragmentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "changelog.d").mkdir()
        (self.root / "changelog.d/README.md").write_text("how fragments work\n")
        (self.root / "CHANGELOG.md").write_text(
            "# Changelog\n\n## Unreleased\n\n- **Old.** Written in place.\n\n"
            "## v0.1.0 (October 1, 2026)\n\n- **First.** Released.\n"
        )

    def fragment(self, name, text):
        (self.root / "changelog.d" / name).write_text(text)

    def test_collects_unreleased_and_fragments(self):
        self.fragment("b.md", "- **B.** Second.\n")
        self.fragment("a.md", "- **A.** First.\n- **A2.** Also.\n")
        changelog["release"](self.root, "v0.2.0", "October 28, 2026")
        self.assertEqual(
            (self.root / "CHANGELOG.md").read_text(),
            "# Changelog\n\n## Unreleased\n\n## v0.2.0 (October 28, 2026)\n\n"
            "- **Old.** Written in place.\n- **A.** First.\n- **A2.** Also.\n- **B.** Second.\n\n"
            "## v0.1.0 (October 1, 2026)\n\n- **First.** Released.\n",
        )
        self.assertEqual([p.name for p in (self.root / "changelog.d").iterdir()], ["README.md"])

    def test_rejects_malformed_fragment_and_keeps_files(self):
        self.fragment("bad.md", "## Heading\nprose\n")
        before = (self.root / "CHANGELOG.md").read_text()
        with self.assertRaisesRegex(ValueError, "bad.md"):
            changelog["release"](self.root, "v0.2.0", "October 28, 2026")
        self.assertEqual((self.root / "CHANGELOG.md").read_text(), before)
        self.assertTrue((self.root / "changelog.d/bad.md").exists())

    def test_rejects_existing_section_and_bad_date(self):
        with self.assertRaisesRegex(ValueError, "already"):
            changelog["release"](self.root, "v0.1.0", "October 28, 2026")
        with self.assertRaises(ValueError):
            changelog["release"](self.root, "v0.2.0", "28/10/2026")

    def test_collected_release_passes_release_check_shape(self):
        self.fragment("a.md", "- **A.** First.\n")
        changelog["release"](self.root, "v0.2.0", "October 28, 2026")
        text = (self.root / "CHANGELOG.md").read_text()
        self.assertRegex(text, r"## Unreleased\n\n## v0.2.0 \(October 28, 2026\)\n")



class RequireFragmentTests(unittest.TestCase):
    def need(self, title, body="", changed=()):
        return changelog["require_fragment"](title, body, list(changed))

    def test_user_visible_types_need_a_fragment(self):
        for title in ["feat: add X", "fix(tui): wrap Y", "perf(http)!: share Z"]:
            self.assertIsNotNone(self.need(title, changed=["src/x.rs"]), title)
            self.assertIsNone(self.need(title, changed=["src/x.rs", "changelog.d/x.md"]), title)

    def test_other_types_do_not(self):
        for title in ["refactor: move X", "ci(perf): tighten Y", "docs: fix a typo",
                      "chore(release): v0.2.0", "test(core): cover Z", "build(deps): bump w"]:
            self.assertIsNone(self.need(title, changed=["src/x.rs"]), title)

    def test_the_readme_is_not_a_fragment(self):
        self.assertIsNotNone(self.need("fix: x", changed=["changelog.d/README.md"]))

    def test_opting_out_takes_a_reason_outside_the_template_comments(self):
        for body in ["Changelog: none (CI only)", "## Notes\nchangelog: none - Windows-only warnings",
                     "Changelog: none: a test fix"]:
            self.assertIsNone(self.need("fix: x", body), body)
        for body in ["Changelog: none", "<!-- Changelog: none (template) -->",
                     "no changelog needed", "Changelog: none ()"]:
            self.assertIsNotNone(self.need("fix: x", body), body)

    def test_the_message_says_how_to_fix_it(self):
        why = self.need("perf: faster", changed=["src/x.rs"])
        self.assertIn("changelog.d/<name>.md", why)
        self.assertIn("Changelog: none", why)

if __name__ == "__main__":
    unittest.main()
