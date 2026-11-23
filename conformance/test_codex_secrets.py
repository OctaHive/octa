"""Safe diagnostics at the native CLI conformance boundary."""

from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import codex_secrets


class SecretConformanceDiagnosticsTests(unittest.TestCase):
    def test_failed_cli_retains_bounded_redacted_fixture_diagnostics(self):
        secret = "synthetic-conformance-credential"
        stderr = ("padding" * 1000 + secret + "\x1b[31mfixture failure marker\x1b[0m").encode()
        completion = subprocess.CompletedProcess([], 1, b"", stderr)
        with mock.patch.object(codex_secrets.subprocess, "run", return_value=completion):
            with self.assertRaises(AssertionError) as rejected:
                codex_secrets.run_cli(Path("octa"), Path("workspace"), {}, secret)
        diagnostic = str(rejected.exception)
        self.assertIn("fixture failure marker", diagnostic)
        self.assertIn("code 1", diagnostic)
        self.assertNotIn(secret, diagnostic)
        self.assertNotIn("\x1b", diagnostic)
        self.assertLessEqual(len(diagnostic), 2200)

    def test_failed_cli_retains_redacted_stdout_diagnostics_too(self):
        secret = "synthetic-conformance-credential"
        completion = subprocess.CompletedProcess([], 1,
            ("padding" * 1000 + secret + "\x1b[31mnative fixture error marker\x1b[0m").encode(),
            ("stderr" * 1000 + secret + "task failed").encode())
        with mock.patch.object(codex_secrets.subprocess, "run", return_value=completion):
            with self.assertRaises(AssertionError) as rejected:
                codex_secrets.run_cli(Path("octa"), Path("workspace"), {}, secret)
        diagnostic = str(rejected.exception)
        self.assertIn("native fixture error marker", diagnostic)
        self.assertIn("task failed", diagnostic)
        self.assertNotIn(secret, diagnostic)
        self.assertNotIn("\x1b", diagnostic)
        self.assertLessEqual(len(diagnostic), 2200)


if __name__ == "__main__":
    unittest.main()
