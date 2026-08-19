from __future__ import annotations

import importlib.util
import pathlib
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "static-audit.py"


def load_static_audit():
    spec = importlib.util.spec_from_file_location("static_audit", SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError("unable to load static audit")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class PlaceholderDetectionTests(unittest.TestCase):
    def test_generated_build_paths_are_ignored(self) -> None:
        audit = load_static_audit()

        self.assertTrue(
            audit.ignored_generated_path(
                pathlib.Path("target/debug/.fingerprint/generated.json")
            )
        )
        self.assertTrue(
            audit.ignored_generated_path(
                pathlib.Path(".build/checkouts/generated/Cargo.toml")
            )
        )
        self.assertFalse(
            audit.ignored_generated_path(pathlib.Path("crates/domain/Cargo.toml"))
        )

    def test_rejects_tautological_planned_contract_test(self) -> None:
        audit = load_static_audit()

        reason = audit.placeholder_reason(
            pathlib.Path("crates/cusp/tests/equilibria_barrier.rs"),
            "#[test] fn equilibria_barrier_planned_contract(){assert_eq!(1_u32,1_u32);}",
        )

        self.assertEqual(reason, "tautological planned-contract test")

    def test_rejects_comment_only_implementation_boundary(self) -> None:
        audit = load_static_audit()

        reason = audit.placeholder_reason(
            pathlib.Path("crates/cusp/src/barrier.rs"),
            "//! cusp::barrier implementation boundary.\n",
        )

        self.assertEqual(reason, "comment-only implementation boundary")

    def test_rejects_manifest_without_package_or_workspace(self) -> None:
        audit = load_static_audit()
        with tempfile.TemporaryDirectory() as directory:
            manifest = pathlib.Path(directory) / "Cargo.toml"
            manifest.write_text(
                'schema_version = 1\nstatus = "implemented"\n',
                encoding="utf-8",
            )

            reason = audit.cargo_manifest_reason(manifest)

        self.assertEqual(reason, "missing [package] or [workspace] table")

    def test_rejects_non_xcode_project_file(self) -> None:
        audit = load_static_audit()
        with tempfile.TemporaryDirectory() as directory:
            project = pathlib.Path(directory) / "project.pbxproj"
            project.write_text(
                "schema_version = 1\nstatus = implemented\n",
                encoding="utf-8",
            )

            reason = audit.xcode_project_reason(project)

        self.assertEqual(reason, "missing Xcode project structure")

    def test_rejects_generic_rust_contract_module(self) -> None:
        audit = load_static_audit()

        reason = audit.placeholder_reason(
            pathlib.Path("crates/connector-binance/src/parser.rs"),
            "//! connector-binance::parser implementation boundary.\n"
            "#[derive(Clone,Debug,Eq,PartialEq)] "
            "pub struct ParserContract { pub schema_version:u32, pub identifier:String }\n"
            "impl ParserContract { pub fn new(identifier:impl Into<String>)->"
            "Result<Self,&'static str>{Ok(Self{schema_version:1,identifier:identifier.into()})} }\n",
        )

        self.assertEqual(reason, "generic identifier contract module")

    def test_rejects_contract_only_swift_source(self) -> None:
        audit = load_static_audit()

        reason = audit.placeholder_reason(
            pathlib.Path("apps/macos/CuspObservatory/App/AppModel.swift"),
            "import Foundation\n"
            "public enum AppmodelContract:Sendable"
            "{public static let schemaVersion:UInt32=1}\n",
        )

        self.assertEqual(reason, "contract-only Swift source")

    def test_rejects_path_only_rust_source(self) -> None:
        audit = load_static_audit()

        reason = audit.placeholder_reason(
            pathlib.Path("apps/cryptoriskd/src/startup.rs"),
            "apps/cryptoriskd/src/startup.rs\n",
        )

        self.assertEqual(reason, "path-only source placeholder")

    def test_rejects_installed_only_shell_script(self) -> None:
        audit = load_static_audit()

        reason = audit.placeholder_reason(
            pathlib.Path("scripts/run-soak.sh"),
            "#!/usr/bin/env bash\n"
            "set -euo pipefail\n"
            "printf 'scripts/run-soak.sh: installed\\n'\n",
        )

        self.assertEqual(reason, "installed-only shell placeholder")

    def test_rejects_workflow_without_triggers_or_jobs(self) -> None:
        audit = load_static_audit()
        with tempfile.TemporaryDirectory() as directory:
            workflow = pathlib.Path(directory) / "ci.yml"
            workflow.write_text(
                "schema_version: 1\n"
                'artifact_id: ".github/workflows/ci.yml"\n'
                "status: implemented\n"
                "local_only: true\n",
                encoding="utf-8",
            )

            reason = audit.workflow_reason(workflow)

        self.assertEqual(reason, "missing workflow triggers or jobs")

    def test_secret_scan_ignores_redaction_patterns(self) -> None:
        audit = load_static_audit()

        self.assertFalse(
            audit.contains_hard_coded_secret(
                'value.contains("password=") || value.contains("api_key=")'
            )
        )

    def test_secret_scan_rejects_assignment(self) -> None:
        audit = load_static_audit()
        assignment = "pass" + 'word = "not-a-real-secret"'

        self.assertTrue(audit.contains_hard_coded_secret(assignment))


if __name__ == "__main__":
    unittest.main()
