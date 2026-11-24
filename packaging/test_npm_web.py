"""packaging/npm/web.py turns wasm-bindgen's output into the @chanto/web package."""

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "packaging" / "npm" / "web.py"
BUILT = ["chanto_web.js", "chanto_web.d.ts", "chanto_web_bg.wasm", "chanto_web_bg.wasm.d.ts"]


class WebPackageTests(unittest.TestCase):
    def setUp(self):
        self.pkg = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.pkg)
        for name in BUILT:
            (self.pkg / name).write_text("built")
        # The release downloads this next to LICENSE before packing.
        self.licenses = ROOT / "THIRD_PARTY_LICENSES.txt"
        self.made_licenses = not self.licenses.exists()
        if self.made_licenses:
            self.licenses.write_text("licenses")
            self.addCleanup(self.licenses.unlink)

    def run_script(self, tag):
        return subprocess.run([sys.executable, str(SCRIPT), tag, str(self.pkg)],
                              capture_output=True, text=True)

    def test_writes_a_versioned_package_with_its_notices(self):
        self.assertEqual(self.run_script("v1.2.3").returncode, 0)
        meta = json.loads((self.pkg / "package.json").read_text())
        self.assertEqual((meta["name"], meta["version"]), ("@chanto/web", "1.2.3"))
        self.assertEqual(meta["types"], "chanto_web.d.ts")
        for name in meta["files"]:
            self.assertTrue((self.pkg / name).is_file(), name)
        self.assertIn("THIRD_PARTY_LICENSES.txt", meta["files"])

    def test_refuses_a_bad_tag_or_a_missing_build(self):
        self.assertNotEqual(self.run_script("1.2.3").returncode, 0)
        (self.pkg / "chanto_web_bg.wasm").unlink()
        result = self.run_script("v1.2.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("chanto_web_bg.wasm", result.stderr)


if __name__ == "__main__":
    unittest.main()
