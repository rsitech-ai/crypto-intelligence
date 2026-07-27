from __future__ import annotations

import json
import pathlib
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "security-audit.py"


class SecurityAuditTests(unittest.TestCase):
    def run_audit(self, files: dict[str, str]) -> dict:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            scripts = root / "scripts"
            scripts.mkdir()
            copied_scanner = scripts / "security-audit.py"
            shutil.copy2(SCRIPT, copied_scanner)
            for relative_path, content in files.items():
                path = root / relative_path
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")

            subprocess.run(
                ["python3", str(copied_scanner)],
                cwd=root,
                check=False,
                capture_output=True,
                text=True,
            )
            report = json.loads(
                (root / "release/evidence/security-audit.json").read_text(
                    encoding="utf-8"
                )
            )

        return report

    def test_scanner_does_not_report_its_own_signatures(self) -> None:
        report = self.run_audit({})

        self.assertNotIn(
            "scripts/security-audit.py",
            {finding["path"] for finding in report["findings"]},
        )

    def test_secret_redaction_code_is_not_a_hard_coded_secret(self) -> None:
        report = self.run_audit(
            {
                "crates/observability/src/lib.rs": (
                    'fn sensitive(v: &str) -> bool { '
                    'v.contains("password=") || v.contains("api_key=") }\n'
                )
            }
        )

        self.assertNotIn(
            "crates/observability/src/lib.rs",
            {finding["path"] for finding in report["findings"]},
        )

    def test_actual_password_assignment_is_reported(self) -> None:
        secret_assignment = "pass" + 'word = "not-a-real-secret"\n'
        report = self.run_audit(
            {"config/default.toml": secret_assignment}
        )

        self.assertIn(
            "config/default.toml",
            {finding["path"] for finding in report["findings"]},
        )

    def test_report_discloses_narrow_scan_scope(self) -> None:
        report = self.run_audit({})

        self.assertEqual(
            report["scope"],
            "secret-pattern and required-boundary-presence scan",
        )
        self.assertIn(
            "does not verify security implementation or runtime behavior",
            report["limitations"],
        )


if __name__ == "__main__":
    unittest.main()
