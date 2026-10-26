"""Regression checks for release preflight, using temporary metadata copies."""

from pathlib import Path
import runpy
import shutil
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parent.parent
check = runpy.run_path(str(ROOT / "packaging/check-release.py"))["check"]


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
        self.replace("install.ps1", "$minVersion = '" + self.tag + "'", "$minVersion = 'v99.0.0'")
        with self.assertRaisesRegex(ValueError, "floors disagree"):
            check(self.root, self.tag)

    def test_rejects_stale_welcome(self):
        self.replace("src/tui/snapshots/visual/dark_40x20_welcome.snap", self.version, "0.0.0")
        with self.assertRaisesRegex(ValueError, "stale welcome"):
            check(self.root, self.tag)


if __name__ == "__main__":
    unittest.main()
