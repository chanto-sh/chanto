import runpy
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CHECK = runpy.run_path(str(ROOT / "scripts/check-advisory-exceptions.py"))


class Exceptions(unittest.TestCase):
    def check(self, version, checksum=None, exception=True, fuzz=False):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            directory = root / "fuzz" if fuzz else root
            directory.mkdir(exist_ok=True)
            (directory / "osv-scanner.toml").write_text(
                f'[[IgnoredVulns]]\nid = "{CHECK["ADVISORY"]}"\n' if exception else ""
            )
            (directory / "Cargo.lock").write_text(
                f'[[package]]\nname = "block-buffer"\nversion = "{version}"\n'
                'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
                f'checksum = "{checksum or CHECK["CHECKSUM_0104"]}"\n'
            )
            return CHECK["problems"](root)

    def test_reviewed_release_and_fixed_versions_pass(self):
        for version in ("0.10.4", "0.12.1", "0.12.2", "0.13.0"):
            with self.subTest(version=version):
                self.assertEqual(self.check(version), [])

    def test_affected_unreviewed_and_prerelease_versions_fail(self):
        for version in ("0.10.3", "0.10.5", "0.11.0", "0.12.0", "0.12.1-rc.1"):
            with self.subTest(version=version):
                self.assertTrue(self.check(version))

    def test_reviewed_version_requires_the_reviewed_checksum(self):
        self.assertTrue(self.check("0.10.4", checksum="different"))

    def test_fuzz_exception_is_also_bounded(self):
        self.assertTrue(self.check("0.11.0", fuzz=True))
        self.assertEqual(self.check("0.10.4", fuzz=True), [])

    def test_no_exception_leaves_detection_to_osv(self):
        self.assertEqual(self.check("0.11.0", exception=False), [])
