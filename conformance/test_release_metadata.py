"""Release artifact contracts exercised through the actual packaging commands."""

import json
import os
import re
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ReleaseMetadataTests(unittest.TestCase):
    def test_release_packages_match_the_documented_version(self):
        expected = re.search(r"^## ([0-9]+\.[0-9]+\.[0-9]+) -", (ROOT / "Changelog.md").read_text()).group(1)
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--offline", "--locked", "--format-version", "1", "--no-deps"], cwd=ROOT, text=True))
        packages = {package["name"]: package for package in metadata["packages"]}
        for identity in metadata["workspace_members"]:
            package = next(p for p in packages.values() if p["id"] == identity)
            if package["name"] == "octa-cache-test-support":
                continue
            with self.subTest(package=package["name"]):
                self.assertEqual(package["version"], expected)
                for dependency in package["dependencies"]:
                    if dependency.get("path") and dependency["name"] in packages and dependency["name"] != "octa-cache-test-support":
                        self.assertEqual(dependency["req"], f"^{expected}")

    @unittest.skipIf(os.name == "nt", "the manifest shell is exercised on Unix CI hosts")
    def test_codex_release_advertises_qualified_observations(self):
        workflow = (ROOT / ".github/workflows/build.yml").read_text()
        statements = [line.strip() for line in workflow.splitlines()
                      if line.strip().startswith("printf 'manifest_version:")
                      and line.strip().endswith("> ./release/plugins/codex.plugin.yml")]
        self.assertEqual(len(statements), 1, "one actual Codex packaging statement")
        statement = statements[0].replace(
            "${{ matrix.platform.runtime_platform }}", "$RELEASE_PLATFORM")
        expected = ["codex.blocking-pre-tool-authorization.v1",
                    "public-command-file-observations.v1",
                    "codex.task-runtime-settings.v1", "model-catalog.v1"]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "release/plugins").mkdir(parents=True)
            for platform, entry in [("linux-x86_64", "octa_plugin_codex"),
                                    ("macos-arm64", "octa_plugin_codex"),
                                    ("windows-x86_64", "octa_plugin_codex.exe")]:
                with self.subTest(platform=platform):
                    subprocess.run(["bash", "-c", statement], cwd=root, check=True,
                                   env={**os.environ, "VERSION": "0.6.0", "PLUGIN_PROTOCOL": "2",
                                        "RELEASE_PLATFORM": platform, "CODEX_ENTRY": entry,
                                        "CODEX_DIGEST": "ab" * 32})
                    manifest = (root / "release/plugins/codex.plugin.yml").read_text()
                    capabilities = [line.strip()[2:] for line in manifest.split("capabilities:\n", 1)[1].splitlines()]
                    self.assertEqual(capabilities, expected)
                    self.assertIn(f"entrypoint: {entry}\n", manifest)
            output = root / "codex-compatibility.json"
            subprocess.run([sys.executable, str(ROOT / "conformance/codex_release_metadata.py"),
                            "--source", str(ROOT / "crates/octa_plugin_codex/src/invocation/executable.rs"),
                            "--plugin-version", "0.6.0", "--plugin-protocol", "2", "--output", str(output)], check=True)
            self.assertEqual(json.loads(output.read_text())["plugin"]["capabilities"], expected)


if __name__ == "__main__":
    unittest.main()
