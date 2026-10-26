"""Public-channel checks must distinguish accepted publishes from readable packages."""

import json
from pathlib import Path
import runpy
import unittest
import urllib.error


module = runpy.run_path(str(Path(__file__).with_name("check-published.py")))
check = module["check"]
PLATFORMS = module["PLATFORMS"]


class PublishedReleaseTests(unittest.TestCase):
    def setUp(self):
        self.tag = "v1.2.3"
        self.sums = "abc  chanto-v1.2.3-aarch64-apple-darwin.tar.gz\n"
        self.records = {}
        for suffix in ("", *("-" + p for p in PLATFORMS)):
            self.records[f"https://registry.npmjs.org/@chanto%2fcli{suffix}/1.2.3"] = {
                "name": "@chanto/cli" + suffix,
                "version": "1.2.3",
                "dist": {"attestations": {"url": "https://registry.npmjs.org/attestations"}},
                "optionalDependencies": {"@chanto/cli-" + p: "1.2.3" for p in PLATFORMS},
            }
        self.records["https://registry.npmjs.org/@chanto%2fcli/latest"] = {"version": "1.2.3"}
        self.records["https://crates.io/api/v1/crates/chanto/1.2.3"] = {
            "version": {"num": "1.2.3", "yanked": False},
        }
        self.records["https://raw.githubusercontent.com/chanto-sh/homebrew-tap/HEAD/Formula/chanto.rb"] = 'version "1.2.3"\nsha256 "abc"\n'
        self.records["https://ghcr.io/token?service=ghcr.io&scope=repository:chanto-sh/chanto:pull"] = {"token": "public-read-token"}

    def fetch(self, url):
        if url not in self.records:
            raise urllib.error.HTTPError(url, 404, "still processing", None, None)
        value = self.records[url]
        return value if isinstance(value, str) else json.dumps(value)

    def failures(self):
        return check(self.tag, self.sums, self.fetch)

    def test_available_release_passes(self):
        self.assertEqual(self.failures(), [])

    def test_processing_platform_is_not_success(self):
        del self.records["https://registry.npmjs.org/@chanto%2fcli-win32-x64/1.2.3"]
        self.assertTrue(any("404" in f for f in self.failures()))

    def test_missing_npm_provenance_is_rejected(self):
        self.records["https://registry.npmjs.org/@chanto%2fcli/1.2.3"]["dist"] = {}
        self.assertTrue(any("provenance" in f for f in self.failures()))

    def test_stale_npm_latest_is_rejected(self):
        self.records["https://registry.npmjs.org/@chanto%2fcli/latest"]["version"] = "1.2.2"
        self.assertTrue(any("latest" in f for f in self.failures()))

    def test_yanked_crate_is_rejected(self):
        self.records["https://crates.io/api/v1/crates/chanto/1.2.3"]["version"]["yanked"] = True
        self.assertTrue(any("yanked" in f for f in self.failures()))

    def test_homebrew_must_match_signed_checksums(self):
        self.sums = self.sums.replace("abc", "def")
        self.assertTrue(any("checksum mismatch" in f for f in self.failures()))

    def test_private_container_is_not_success(self):
        url = "https://ghcr.io/token?service=ghcr.io&scope=repository:chanto-sh/chanto:pull"
        del self.records[url]
        self.assertEqual(self.failures(), [f"{url}: HTTP Error 404: still processing"])


if __name__ == "__main__":
    unittest.main()
