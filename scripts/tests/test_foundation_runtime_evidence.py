#!/usr/bin/env python3
import copy
import importlib.util
import json
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
WRAPPER = ROOT / "scripts" / "verify-foundation-runtime-twice.py"
COMMIT = "a" * 40
SHA = "b" * 64


def load_wrapper_module():
    spec = importlib.util.spec_from_file_location("foundation_runtime_twice", WRAPPER)
    if spec is None or spec.loader is None:
        raise RuntimeError("two-run verifier wrapper could not be imported")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def native_summary(total: int) -> dict:
    return {
        "result": "Passed",
        "totalTestCount": total,
        "passedTests": total,
        "failedTests": 0,
        "skippedTests": 0,
        "expectedFailures": 0,
        "devicesAndConfigurations": [
            {
                "passedTests": total,
                "failedTests": 0,
                "skippedTests": 0,
                "expectedFailures": 0,
            }
        ],
    }


def valid_single_evidence() -> dict:
    labels = [
        "cargo-build",
        "cargo-msrv",
        "cargo-format",
        "cargo-clippy",
        "cargo-tests",
        "verifier-shellcheck",
        "verifier-helper-tests",
        "buf-lint",
        "buf-build",
        "proto-generation",
        "config-generation",
        "xcode-generation",
        "swift-package-tests",
        "native-app-build",
        "app-controlled-controlled-quit",
        "app-relaunch-controlled-quit",
        "unified-log-inspection",
        "unified-log-policy",
        "native-unit-tests",
        "native-ui-tests",
        "static-audit",
        "security-audit",
        "git-diff-check",
    ]
    snapshot = {
        "source": "binance-fixture",
        "symbol": "BTCUSDT",
        "generation": 1,
        "sequence": 102,
        "best_bid": "60000.1",
        "best_ask": "60000.2",
        "health": 1,
        "event_unix_nanos": 1_700_000_000_200_000_000,
        "receive_unix_nanos": 1_700_000_000_200_000_000,
        "freshness_millis": 0,
    }
    runs = [
        {
            "run": number,
            "snapshot_digest": SHA,
            "snapshot": copy.deepcopy(snapshot),
            "shutdown_exit_code": 0,
            "stopped_pid": 4_200 + number,
            "pid_absent_after_shutdown": True,
            "wal_size_bytes": 658,
            "wal_sha256": SHA,
        }
        for number in (1, 2)
    ]
    records = [
        {
            "label": label,
            "termination": termination,
            "app_pid": 5_000 + index,
            "app_exit_code": exit_code,
            "daemon_pid": 6_000 + index,
            "watcher_pid": 7_000 + index,
            "owned_processes_gone": True,
            "daemon_parent_was_app": True,
            "watcher_parent_was_daemon": True,
            "cleanup_required_escalation": False,
        }
        for index, (label, termination, exit_code) in enumerate(
            [
                ("app-controlled", "controlled", 0),
                ("app-supervisor-loss", "SIGKILL", 137),
                ("app-relaunch", "controlled", 0),
            ],
            start=1,
        )
    ]
    artifacts = {
        name: SHA
        for name in [
            "Cargo.lock",
            "configs/schema.json",
            "fixtures/binance/btcusdt-book-v1.jsonl",
            "proto/buf.lock",
            "apps/macos/CuspObservatory.xcodeproj/project.pbxproj",
            "apps/macos/Packages/TransitionClient/Package.resolved",
            "target/debug/cryptoriskd",
            "target/debug/foundation-runtime-probe",
            "/private/tmp/run/CuspObservatory.app/Contents/MacOS/CuspObservatory",
            "/private/tmp/run/CuspObservatory.app/Contents/MacOS/cryptoriskd",
        ]
    }
    return {
        "schema_version": 2,
        "generated_at": "2026-07-28T12:00:00+00:00",
        "verified_commit": COMMIT,
        "tree_clean_at_start": True,
        "subgate_status": "passed",
        "full_gate_invocations_observed": 1,
        "two_clean_full_gate_runs_observed": False,
        "two_run_receipt": None,
        "provenance_invariant": "exact-clean-commit evidence",
        "status": "blocked",
        "readiness_label": "blocked:verification-incomplete",
        "target_readiness_label": "runtime-proven foundation slice",
        "blockers": ["two-clean-full-gate-runs-not-observed"],
        "tool_versions": {
            label: "version"
            for label in [
                "bash",
                "buf",
                "cargo",
                "developer-mode",
                "git",
                "protoc",
                "rustc",
                "rustc-msrv",
                "shellcheck",
                "swift",
                "xcodebuild",
                "xcodegen",
            ]
        },
        "commands": [
            {
                "label": label,
                "command": f"command-{label}",
                "exit_code": 0,
                "started_unix_nanos": index,
                "ended_unix_nanos": index + 1,
                "output_sha256": SHA,
                "test_count_matches": [],
            }
            for index, label in enumerate(labels, start=1)
        ],
        "artifact_sha256": artifacts,
        "process_and_stream_inspection_sha256": {
            f"daemon-{number}.{suffix}": SHA
            for number in (1, 2)
            for suffix in ("lsof", "process", "stderr", "stdout")
        },
        "runtime": {
            "healthy_run_count": 2,
            "runs": runs,
            "healthy_digests_identical": True,
            "deliberate_authentication_failure": True,
            "authentication_failure": {
                "authentication_failure": "rejected",
                "code": "Unauthenticated",
            },
            "graceful_shutdown_observed": True,
            "wal_recovery_observed": True,
            "secret_scans": [
                {"secret_material_findings": []},
                {"secret_material_findings": []},
            ],
            "warning_findings": {},
            "stale_process_findings": {},
            "unified_log_findings": [],
        },
        "native_app_runtime": {
            "records": records,
            "controlled_quit_observed": True,
            "forced_supervisor_loss_observed": True,
            "relaunch_observed": True,
            "exact_watcher_topology_observed": True,
            "wal_recovery": {
                "before_relaunch_size_bytes": 658,
                "before_relaunch_sha256": SHA,
                "after_relaunch_size_bytes": 658,
                "after_relaunch_sha256": SHA,
                "identical": True,
            },
            "log_findings": {},
        },
        "native_test_summaries": {
            "native-unit-summary.json": native_summary(35),
            "native-ui-summary.json": native_summary(3),
        },
        "static_audit_historical_baseline_error_count": 384,
        "static_audit_current_error_count": 0,
        "security_audit_current_counts": {
            "critical": 0,
            "high": 0,
            "medium": 0,
            "low": 0,
        },
        "tree_mutation_checks": {
            "start-tree-status.txt": [],
            "pre-audit-tree-status.txt": [],
            "post-audit-tree-status.txt": [],
            "pre-evidence-tree-status.txt": [],
        },
        "limitations": [
            "This is not model, live-source, App Store, notarization, or release readiness."
        ],
    }


def encoded(evidence: dict) -> bytes:
    return (json.dumps(evidence, sort_keys=True) + "\n").encode()


class FoundationRuntimeEvidenceTests(unittest.TestCase):
    def test_exact_valid_single_evidence_is_accepted(self) -> None:
        module = load_wrapper_module()
        parsed = module.validate_single_evidence(
            encoded(valid_single_evidence()), COMMIT
        )
        self.assertEqual(parsed["verified_commit"], COMMIT)

    def test_summary_boolean_cannot_hide_a_forged_authentication_record(self) -> None:
        module = load_wrapper_module()
        evidence = valid_single_evidence()
        evidence["runtime"]["authentication_failure"]["code"] = "OK"
        with self.assertRaises(module.EvidenceValidationError):
            module.validate_single_evidence(encoded(evidence), COMMIT)

    def test_command_inventory_cannot_be_duplicated_or_omitted(self) -> None:
        module = load_wrapper_module()
        evidence = valid_single_evidence()
        evidence["commands"][1]["label"] = evidence["commands"][0]["label"]
        with self.assertRaises(module.EvidenceValidationError):
            module.validate_single_evidence(encoded(evidence), COMMIT)

    def test_summary_boolean_cannot_hide_forged_runtime_details(self) -> None:
        module = load_wrapper_module()
        evidence = valid_single_evidence()
        evidence["runtime"]["runs"][1]["snapshot_digest"] = "c" * 64
        with self.assertRaises(module.EvidenceValidationError):
            module.validate_single_evidence(encoded(evidence), COMMIT)

    def test_native_unit_summary_must_be_exactly_35_of_35(self) -> None:
        module = load_wrapper_module()
        evidence = valid_single_evidence()
        evidence["native_test_summaries"]["native-unit-summary.json"][
            "totalTestCount"
        ] = 34
        with self.assertRaises(module.EvidenceValidationError):
            module.validate_single_evidence(encoded(evidence), COMMIT)

    def test_artifact_hashes_must_cover_the_exact_inventory(self) -> None:
        module = load_wrapper_module()
        evidence = valid_single_evidence()
        evidence["artifact_sha256"]["Cargo.lock"] = "not-a-sha256"
        with self.assertRaises(module.EvidenceValidationError):
            module.validate_single_evidence(encoded(evidence), COMMIT)

    def test_private_anchor_rejects_first_evidence_tampering(self) -> None:
        module = load_wrapper_module()
        with tempfile.TemporaryDirectory() as directory:
            evidence_path = pathlib.Path(directory) / "first.json"
            anchor_path = pathlib.Path(directory) / "first.sha256"
            raw = encoded(valid_single_evidence())
            evidence_path.write_bytes(raw)
            module.write_private_anchor(anchor_path, raw)
            evidence_path.write_bytes(raw + b" ")
            with self.assertRaises(module.EvidenceValidationError):
                module.read_anchored_evidence(evidence_path, anchor_path)

    def test_two_valid_distinct_runs_produce_a_valid_promotion_receipt(self) -> None:
        module = load_wrapper_module()
        first = valid_single_evidence()
        second = valid_single_evidence()
        second["generated_at"] = "2026-07-28T12:01:00+00:00"
        second["commands"][0]["started_unix_nanos"] += 100
        second["commands"][0]["ended_unix_nanos"] += 100
        combined = module.combine_validated_evidence(
            encoded(first), encoded(second), COMMIT
        )
        self.assertEqual(combined["status"], "passed")
        self.assertEqual(combined["full_gate_invocations_observed"], 2)
        self.assertTrue(combined["two_clean_full_gate_runs_observed"])
        self.assertEqual(combined["blockers"], [])
        self.assertEqual(
            len(combined["two_run_receipt"]["invocations"]),
            2,
        )

    def test_single_verifier_rejects_the_retired_prior_evidence_option(self) -> None:
        result = subprocess.run(
            [
                str(ROOT / "scripts" / "verify-foundation-runtime.sh"),
                "--prior-evidence",
                "/tmp/forged.json",
            ],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage:", result.stderr)


if __name__ == "__main__":
    unittest.main()
