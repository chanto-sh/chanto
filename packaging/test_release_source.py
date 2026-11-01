"""What counts as a release coming from this repository."""

import base64
import json
from pathlib import Path
import re
import runpy
import unittest

ROOT = Path(__file__).resolve().parent.parent
sdk_check = runpy.run_path(str(ROOT / "packaging/sdk_check.py"))
RELEASE_WORKFLOWS = ["release", "npm", "container", "sdk-publish", "tag-release"]


class SourceTests(unittest.TestCase):
    def test_npm_source_reads_the_provenance_workflow(self):
        statement = {"predicate": {"buildDefinition": {"externalParameters": {"workflow": {
            "repository": "https://github.com/lookalike/chanto", "path": ".github/workflows/x.yml", "ref": "refs/tags/v1",
        }}}}}
        payload = base64.b64encode(json.dumps(statement).encode()).decode()
        got = sdk_check["npm_source"]({"attestations": [
            {"predicateType": "https://github.com/npm/attestation/tree/main/specs/publish/v0.1", "bundle": {}},
            {"predicateType": "https://slsa.dev/provenance/v1", "bundle": {"dsseEnvelope": {"payload": payload}}},
        ]})
        self.assertEqual(got, ("https://github.com/lookalike/chanto", ".github/workflows/x.yml", "refs/tags/v1"))

    def test_npm_without_provenance_fails(self):
        with self.assertRaises(SystemExit):
            sdk_check["npm_source"]({"attestations": [{"predicateType": "other", "bundle": {}}]})

    def test_pypi_source_reads_the_publisher(self):
        got = sdk_check["pypi_source"]({"attestation_bundles": [{"publisher": {"repository": "a/b", "workflow": "c.yml"}}]})
        self.assertEqual(got, ("a/b", "c.yml"))


class ReleaseWorkflowTests(unittest.TestCase):
    def test_releases_build_only_this_repository(self):
        """A release that checks out another repository still gets a clean
        attestation naming this one, so nothing later would notice."""
        for name in RELEASE_WORKFLOWS:
            text = (ROOT / f".github/workflows/{name}.yml").read_text()
            self.assertIsNone(re.search(r"^\s+repository:", text, re.M), name)


if __name__ == "__main__":
    unittest.main()
