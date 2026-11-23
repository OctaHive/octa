"""GitHub release bodies contain only the version selected by the tag."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class ReleaseNotesTests(unittest.TestCase):
    def test_selects_only_the_requested_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            changelog = root / "Changelog.md"
            changelog.write_text("# Changelog\n\n## 0.10.1 - future\n\nNewer fix.\n\n"
                                 "## 0.10.0 - today\n\n### Added\n\nCurrent change.\n\n"
                                 "## 0.9.0 - yesterday\n\nHistorical fix.\n", encoding="utf-8")
            output = root / "notes.md"
            subprocess.run([sys.executable, str(ROOT / "conformance/release_notes.py"),
                            "--version", "0.10.0", "--changelog", str(changelog),
                            "--output", str(output)], check=True)
            self.assertEqual(output.read_text(encoding="utf-8"),
                             "## 0.10.0 - today\n\n### Added\n\nCurrent change.\n")

    def test_missing_release_fails_without_publishing_other_versions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            changelog = root / "Changelog.md"
            changelog.write_text("## 0.9.0 - yesterday\n\nHistorical fix.\n", encoding="utf-8")
            output = root / "notes.md"
            result = subprocess.run([sys.executable, str(ROOT / "conformance/release_notes.py"),
                                     "--version", "0.10.0", "--changelog", str(changelog),
                                     "--output", str(output)], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("0.10.0", result.stderr)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
