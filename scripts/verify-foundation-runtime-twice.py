#!/usr/bin/env python3
"""Run and attest two exact foundation verifier invocations.

The single-run verifier deliberately cannot consume earlier evidence. This
parent owns both private outputs and the first-run digest, validates detailed
records from each run, and is the only path that emits a two-run promotion.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
from datetime import datetime
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[1]
VERIFIER = ROOT / "scripts" / "verify-foundation-runtime.sh"
DEFAULT_OUTPUT = ROOT / "release" / "evidence" / "foundation-runtime-verification.json"
TARGET_LABEL = "runtime-proven foundation slice"
INCOMPLETE_BLOCKER = "two-clean-full-gate-runs-not-observed"
POLICY = "foundation-runtime-two-run/v1"
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
COMMIT = re.compile(r"[0-9a-f]{40,64}\Z")

COMMAND_LABELS = [
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
TOOL_LABELS = {
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
}
FIXED_ARTIFACTS = {
    "Cargo.lock",
    "configs/schema.json",
    "fixtures/binance/btcusdt-book-v1.jsonl",
    "proto/baselines/cmti-v1.binpb",
    "apps/macos/CuspObservatory.xcodeproj/project.pbxproj",
    "apps/macos/Packages/TransitionClient/Package.resolved",
    "target/debug/cryptoriskd",
    "target/debug/foundation-runtime-probe",
}
INSPECTION_ARTIFACTS = {
    f"daemon-{number}.{suffix}"
    for number in (1, 2)
    for suffix in ("lsof", "process", "stderr", "stdout")
}
TREE_CHECKS = {
    "start-tree-status.txt",
    "pre-audit-tree-status.txt",
    "post-audit-tree-status.txt",
    "pre-evidence-tree-status.txt",
}
TOP_LEVEL_KEYS = {
    "artifact_sha256",
    "blockers",
    "commands",
    "full_gate_invocations_observed",
    "generated_at",
    "limitations",
    "native_app_runtime",
    "native_test_summaries",
    "process_and_stream_inspection_sha256",
    "provenance_invariant",
    "readiness_label",
    "runtime",
    "schema_version",
    "security_audit_current_counts",
    "static_audit_current_error_count",
    "static_audit_historical_baseline_error_count",
    "status",
    "subgate_status",
    "target_readiness_label",
    "tool_versions",
    "tree_clean_at_start",
    "tree_mutation_checks",
    "two_clean_full_gate_runs_observed",
    "two_run_receipt",
    "verified_commit",
}


class EvidenceValidationError(ValueError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise EvidenceValidationError(message)


def exact_keys(value: Any, expected: set[str], label: str) -> dict[str, Any]:
    require(isinstance(value, dict), f"{label} must be an object")
    require(set(value) == expected, f"{label} has an unexpected schema")
    return value


def is_sha256(value: Any) -> bool:
    return isinstance(value, str) and SHA256.fullmatch(value) is not None


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise EvidenceValidationError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def parse_json(raw: bytes) -> dict[str, Any]:
    try:
        parsed = json.loads(raw, object_pairs_hook=reject_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise EvidenceValidationError("evidence is not valid JSON") from error
    require(isinstance(parsed, dict), "evidence root must be an object")
    return parsed


def validate_commands(commands: Any) -> None:
    require(isinstance(commands, list), "commands must be an array")
    require(
        [command.get("label") for command in commands if isinstance(command, dict)]
        == COMMAND_LABELS,
        "command labels or order do not match the exact gate inventory",
    )
    expected_keys = {
        "command",
        "ended_unix_nanos",
        "exit_code",
        "label",
        "output_sha256",
        "started_unix_nanos",
        "test_count_matches",
    }
    for index, command in enumerate(commands):
        exact_keys(command, expected_keys, f"commands[{index}]")
        require(command["exit_code"] == 0, f"{command['label']} did not pass")
        require(
            isinstance(command["command"], str) and command["command"],
            f"{command['label']} command is missing",
        )
        require(
            isinstance(command["started_unix_nanos"], int)
            and isinstance(command["ended_unix_nanos"], int)
            and command["started_unix_nanos"] >= 0
            and command["ended_unix_nanos"] >= command["started_unix_nanos"],
            f"{command['label']} timing is invalid",
        )
        require(
            is_sha256(command["output_sha256"]),
            f"{command['label']} output hash is invalid",
        )
        require(
            isinstance(command["test_count_matches"], list),
            f"{command['label']} test counts are invalid",
        )


def validate_artifacts(artifacts: Any) -> None:
    require(isinstance(artifacts, dict), "artifact hashes must be an object")
    require(len(artifacts) == 10, "artifact inventory must contain exactly ten files")
    require(
        FIXED_ARTIFACTS.issubset(artifacts),
        "fixed artifact inventory is incomplete",
    )
    dynamic = set(artifacts) - FIXED_ARTIFACTS
    suffixes = {
        "/CuspObservatory.app/Contents/MacOS/CuspObservatory",
        "/CuspObservatory.app/Contents/MacOS/cryptoriskd",
    }
    require(
        len(dynamic) == 2
        and all(
            isinstance(path, str)
            and path.startswith("/")
            and sum(path.endswith(suffix) for suffix in suffixes) == 1
            for path in dynamic
        )
        and all(sum(path.endswith(suffix) for path in dynamic) == 1 for suffix in suffixes),
        "native app artifact inventory is invalid",
    )
    require(
        all(is_sha256(digest) for digest in artifacts.values()),
        "artifact inventory contains an invalid SHA-256",
    )


SNAPSHOT = {
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


def validate_runtime(runtime: Any) -> None:
    exact_keys(
        runtime,
        {
            "authentication_failure",
            "deliberate_authentication_failure",
            "graceful_shutdown_observed",
            "healthy_digests_identical",
            "healthy_run_count",
            "runs",
            "secret_scans",
            "stale_process_findings",
            "unified_log_findings",
            "wal_recovery_observed",
            "warning_findings",
        },
        "runtime",
    )
    require(runtime["healthy_run_count"] == 2, "exactly two daemon runs are required")
    runs = runtime["runs"]
    require(isinstance(runs, list) and len(runs) == 2, "daemon records are incomplete")
    run_keys = {
        "pid_absent_after_shutdown",
        "run",
        "shutdown_exit_code",
        "snapshot",
        "snapshot_digest",
        "stopped_pid",
        "wal_sha256",
        "wal_size_bytes",
    }
    for index, run in enumerate(runs, start=1):
        exact_keys(run, run_keys, f"runtime run {index}")
        require(run["run"] == index, "daemon run ordinals are invalid")
        require(run["snapshot"] == SNAPSHOT, f"daemon run {index} snapshot is invalid")
        require(is_sha256(run["snapshot_digest"]), "snapshot digest is invalid")
        require(is_sha256(run["wal_sha256"]), "WAL hash is invalid")
        require(
            run["shutdown_exit_code"] == 0
            and run["pid_absent_after_shutdown"] is True
            and isinstance(run["stopped_pid"], int)
            and run["stopped_pid"] > 0
            and isinstance(run["wal_size_bytes"], int)
            and run["wal_size_bytes"] > 0,
            f"daemon run {index} shutdown/WAL record is invalid",
        )
    digests_identical = runs[0]["snapshot_digest"] == runs[1]["snapshot_digest"]
    wal_identical = (
        runs[0]["wal_sha256"] == runs[1]["wal_sha256"]
        and runs[0]["wal_size_bytes"] == runs[1]["wal_size_bytes"]
    )
    graceful = all(
        run["shutdown_exit_code"] == 0 and run["pid_absent_after_shutdown"] is True
        for run in runs
    )
    authentication = {
        "authentication_failure": "rejected",
        "code": "Unauthenticated",
    }
    require(
        runtime["authentication_failure"] == authentication,
        "authentication failure detail is invalid",
    )
    require(
        runtime["healthy_digests_identical"] is digests_identical
        and digests_identical,
        "snapshot summary does not match daemon records",
    )
    require(
        runtime["wal_recovery_observed"] is wal_identical and wal_identical,
        "WAL summary does not match daemon records",
    )
    require(
        runtime["graceful_shutdown_observed"] is graceful and graceful,
        "shutdown summary does not match daemon records",
    )
    require(
        runtime["deliberate_authentication_failure"] is True,
        "authentication rejection was not observed",
    )
    require(
        runtime["secret_scans"]
        == [
            {"secret_material_findings": []},
            {"secret_material_findings": []},
        ],
        "secret scan records are incomplete or non-empty",
    )
    require(runtime["warning_findings"] == {}, "warning findings are not empty")
    require(runtime["stale_process_findings"] == {}, "stale process findings are not empty")
    require(runtime["unified_log_findings"] == [], "unified log findings are not empty")


def validate_native_runtime(native: Any) -> None:
    exact_keys(
        native,
        {
            "controlled_quit_observed",
            "exact_watcher_topology_observed",
            "forced_supervisor_loss_observed",
            "log_findings",
            "records",
            "relaunch_observed",
            "wal_recovery",
        },
        "native_app_runtime",
    )
    records = native["records"]
    require(isinstance(records, list) and len(records) == 3, "native records are incomplete")
    record_keys = {
        "app_exit_code",
        "app_pid",
        "cleanup_required_escalation",
        "daemon_parent_was_app",
        "daemon_pid",
        "label",
        "owned_processes_gone",
        "termination",
        "watcher_parent_was_daemon",
        "watcher_pid",
    }
    expected = [
        ("app-controlled", "controlled", 0),
        ("app-supervisor-loss", "SIGKILL", (9, 137)),
        ("app-relaunch", "controlled", 0),
    ]
    for index, (record, (label, termination, exit_code)) in enumerate(
        zip(records, expected, strict=True)
    ):
        exact_keys(record, record_keys, f"native record {index}")
        accepted_exits = exit_code if isinstance(exit_code, tuple) else (exit_code,)
        require(
            record["label"] == label
            and record["termination"] == termination
            and record["app_exit_code"] in accepted_exits
            and record["owned_processes_gone"] is True
            and record["daemon_parent_was_app"] is True
            and record["watcher_parent_was_daemon"] is True
            and record["cleanup_required_escalation"] is False
            and all(
                isinstance(record[field], int) and record[field] > 0
                for field in ("app_pid", "daemon_pid", "watcher_pid")
            ),
            f"native record {label} is invalid",
        )
    require(
        native["controlled_quit_observed"] is True
        and native["forced_supervisor_loss_observed"] is True
        and native["relaunch_observed"] is True
        and native["exact_watcher_topology_observed"] is True,
        "native summary flags do not match the exact records",
    )
    wal = exact_keys(
        native["wal_recovery"],
        {
            "after_relaunch_sha256",
            "after_relaunch_size_bytes",
            "before_relaunch_sha256",
            "before_relaunch_size_bytes",
            "identical",
        },
        "native WAL recovery",
    )
    identical = (
        wal["before_relaunch_sha256"] == wal["after_relaunch_sha256"]
        and wal["before_relaunch_size_bytes"] == wal["after_relaunch_size_bytes"]
    )
    require(
        is_sha256(wal["before_relaunch_sha256"])
        and is_sha256(wal["after_relaunch_sha256"])
        and isinstance(wal["before_relaunch_size_bytes"], int)
        and wal["before_relaunch_size_bytes"] > 0
        and wal["identical"] is identical
        and identical,
        "native WAL recovery detail is invalid",
    )
    require(native["log_findings"] == {}, "native log findings are not empty")


def validate_summary(summary: Any, expected_total: int | None, label: str) -> int:
    require(isinstance(summary, dict), f"{label} summary must be an object")
    required = {
        "result",
        "totalTestCount",
        "passedTests",
        "failedTests",
        "skippedTests",
        "expectedFailures",
        "devicesAndConfigurations",
    }
    require(required.issubset(summary), f"{label} summary schema is incomplete")
    total = summary["totalTestCount"]
    require(
        isinstance(total, int)
        and total > 0
        and summary["result"] == "Passed"
        and summary["passedTests"] == total
        and summary["failedTests"] == 0
        and summary["skippedTests"] == 0
        and summary["expectedFailures"] == 0
        and (expected_total is None or total == expected_total),
        f"{label} result counts are invalid",
    )
    configurations = summary["devicesAndConfigurations"]
    require(
        isinstance(configurations, list)
        and len(configurations) == 1
        and configurations[0].get("passedTests") == total
        and configurations[0].get("failedTests") == 0
        and configurations[0].get("skippedTests") == 0
        and configurations[0].get("expectedFailures") == 0,
        f"{label} device result counts are invalid",
    )
    return total


def validate_single_evidence(raw: bytes, expected_commit: str) -> dict[str, Any]:
    evidence = parse_json(raw)
    exact_keys(evidence, TOP_LEVEL_KEYS, "evidence")
    require(evidence["schema_version"] == 2, "unsupported evidence schema")
    require(COMMIT.fullmatch(expected_commit) is not None, "expected commit is invalid")
    require(evidence["verified_commit"] == expected_commit, "verified commit mismatch")
    try:
        datetime.fromisoformat(evidence["generated_at"])
    except (TypeError, ValueError) as error:
        raise EvidenceValidationError("generated_at is invalid") from error
    require(evidence["tree_clean_at_start"] is True, "start tree was not clean")
    require(evidence["subgate_status"] == "passed", "single-run subgates did not pass")
    require(evidence["full_gate_invocations_observed"] == 1, "single-run count is invalid")
    require(evidence["two_clean_full_gate_runs_observed"] is False, "single run claims two runs")
    require(evidence["two_run_receipt"] is None, "single run contains a promotion receipt")
    require(evidence["status"] == "blocked", "single run status must be blocked")
    require(
        evidence["readiness_label"] == "blocked:verification-incomplete"
        and evidence["target_readiness_label"] == TARGET_LABEL,
        "single-run readiness labels are invalid",
    )
    require(evidence["blockers"] == [INCOMPLETE_BLOCKER], "single-run blockers are invalid")
    require(
        isinstance(evidence["provenance_invariant"], str)
        and evidence["provenance_invariant"],
        "provenance invariant is missing",
    )
    exact_keys(evidence["tool_versions"], TOOL_LABELS, "tool_versions")
    require(
        all(
            isinstance(version, str) and version
            for version in evidence["tool_versions"].values()
        ),
        "tool version is missing",
    )
    validate_commands(evidence["commands"])
    validate_artifacts(evidence["artifact_sha256"])
    exact_keys(
        evidence["process_and_stream_inspection_sha256"],
        INSPECTION_ARTIFACTS,
        "process inspection hashes",
    )
    require(
        all(
            is_sha256(digest)
            for digest in evidence["process_and_stream_inspection_sha256"].values()
        ),
        "process inspection hash is invalid",
    )
    validate_runtime(evidence["runtime"])
    validate_native_runtime(evidence["native_app_runtime"])
    summaries = exact_keys(
        evidence["native_test_summaries"],
        {"native-unit-summary.json", "native-ui-summary.json"},
        "native test summaries",
    )
    validate_summary(summaries["native-unit-summary.json"], 35, "native unit")
    validate_summary(summaries["native-ui-summary.json"], None, "native UI")
    require(
        evidence["static_audit_historical_baseline_error_count"] == 384
        and evidence["static_audit_current_error_count"] == 0,
        "static audit counts are invalid",
    )
    exact_keys(
        evidence["security_audit_current_counts"],
        {"critical", "high", "medium", "low"},
        "security audit counts",
    )
    require(
        all(count == 0 for count in evidence["security_audit_current_counts"].values()),
        "security audit findings remain",
    )
    checks = exact_keys(evidence["tree_mutation_checks"], TREE_CHECKS, "tree checks")
    require(all(value == [] for value in checks.values()), "tree mutation was observed")
    require(
        evidence["limitations"]
        == ["This is not model, live-source, App Store, notarization, or release readiness."],
        "otherwise-green limitations are invalid",
    )
    return evidence


def write_private_anchor(path: pathlib.Path, raw: bytes) -> None:
    digest = hashlib.sha256(raw).hexdigest().encode() + b"\n"
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(descriptor, digest)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def read_anchored_evidence(path: pathlib.Path, anchor_path: pathlib.Path) -> bytes:
    try:
        raw = path.read_bytes()
        anchor = anchor_path.read_text().strip()
    except OSError as error:
        raise EvidenceValidationError("private evidence anchor is unavailable") from error
    require(is_sha256(anchor), "private evidence anchor is invalid")
    require(
        hashlib.sha256(raw).hexdigest() == anchor,
        "first evidence changed after its private anchor was recorded",
    )
    return raw


def invocation_receipt(ordinal: int, raw: bytes, evidence: dict[str, Any]) -> dict[str, Any]:
    return {
        "ordinal": ordinal,
        "evidence_sha256": hashlib.sha256(raw).hexdigest(),
        "generated_at": evidence["generated_at"],
        "command_output_sha256": {
            command["label"]: command["output_sha256"] for command in evidence["commands"]
        },
        "artifact_sha256": evidence["artifact_sha256"],
        "runtime_snapshot_sha256": [
            run["snapshot_digest"] for run in evidence["runtime"]["runs"]
        ],
        "runtime_wal_sha256": [run["wal_sha256"] for run in evidence["runtime"]["runs"]],
        "native_test_totals": {
            name: summary["totalTestCount"]
            for name, summary in evidence["native_test_summaries"].items()
        },
    }


def combine_validated_evidence(
    first_raw: bytes, second_raw: bytes, expected_commit: str
) -> dict[str, Any]:
    first = validate_single_evidence(first_raw, expected_commit)
    second = validate_single_evidence(second_raw, expected_commit)
    first_digest = hashlib.sha256(first_raw).hexdigest()
    second_digest = hashlib.sha256(second_raw).hexdigest()
    require(first_digest != second_digest, "two byte-identical evidence files are not distinct runs")
    require(
        datetime.fromisoformat(second["generated_at"])
        > datetime.fromisoformat(first["generated_at"]),
        "second evidence timestamp does not follow the first",
    )
    combined = copy.deepcopy(second)
    combined["status"] = "passed"
    combined["readiness_label"] = TARGET_LABEL
    combined["blockers"] = []
    combined["full_gate_invocations_observed"] = 2
    combined["two_clean_full_gate_runs_observed"] = True
    combined["two_run_receipt"] = {
        "policy": POLICY,
        "verified_commit": expected_commit,
        "invocations": [
            invocation_receipt(1, first_raw, first),
            invocation_receipt(2, second_raw, second),
        ],
    }
    return combined


def atomic_write(path: pathlib.Path, raw: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    temporary_path = pathlib.Path(temporary)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(raw)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary_path, path)
    finally:
        temporary_path.unlink(missing_ok=True)


def run_single(output: pathlib.Path, stdout: pathlib.Path, stderr: pathlib.Path) -> int:
    with stdout.open("wb") as standard_output, stderr.open("wb") as standard_error:
        result = subprocess.run(
            [str(VERIFIER), "--evidence-output", str(output)],
            cwd=ROOT,
            stdout=standard_output,
            stderr=standard_error,
            check=False,
        )
    return result.returncode


def publish_blocked(source: pathlib.Path, destination: pathlib.Path) -> None:
    if source.is_file():
        atomic_write(destination, source.read_bytes())


def current_commit_and_clean_tree() -> str:
    status = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=ROOT,
        capture_output=True,
        check=True,
    )
    require(status.stdout == b"", "two-run verification requires an exact clean commit")
    commit = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    require(COMMIT.fullmatch(commit) is not None, "repository commit is invalid")
    return commit


def parse_arguments(arguments: list[str]) -> pathlib.Path:
    parser = argparse.ArgumentParser()
    parser.add_argument("--evidence-output", type=pathlib.Path, default=DEFAULT_OUTPUT)
    parsed = parser.parse_args(arguments)
    return parsed.evidence_output.resolve()


def main(arguments: list[str] | None = None) -> int:
    output = parse_arguments(sys.argv[1:] if arguments is None else arguments)
    try:
        commit = current_commit_and_clean_tree()
        with tempfile.TemporaryDirectory(prefix="cmti-foundation-two-run.") as directory:
            private_root = pathlib.Path(directory)
            private_root.chmod(0o700)
            first_path = private_root / "first.json"
            first_status = run_single(
                first_path, private_root / "first.stdout", private_root / "first.stderr"
            )
            first_raw = first_path.read_bytes() if first_path.is_file() else b""
            try:
                validate_single_evidence(first_raw, commit)
                require(first_status == 1, "single verifier returned an unexpected status")
            except EvidenceValidationError:
                publish_blocked(first_path, output)
                raise
            anchor_path = private_root / "first.sha256"
            write_private_anchor(anchor_path, first_raw)

            second_path = private_root / "second.json"
            second_status = run_single(
                second_path, private_root / "second.stdout", private_root / "second.stderr"
            )
            anchored_first = read_anchored_evidence(first_path, anchor_path)
            second_raw = second_path.read_bytes() if second_path.is_file() else b""
            try:
                validate_single_evidence(second_raw, commit)
                require(second_status == 1, "single verifier returned an unexpected status")
                combined = combine_validated_evidence(anchored_first, second_raw, commit)
            except EvidenceValidationError:
                publish_blocked(second_path, output)
                raise
            atomic_write(
                output,
                (json.dumps(combined, indent=2, sort_keys=True) + "\n").encode(),
            )
    except (EvidenceValidationError, OSError, subprocess.SubprocessError) as error:
        print(f"foundation two-run verification: BLOCKED ({error})", file=sys.stderr)
        return 1
    print(f"foundation two-run verification: PASS ({TARGET_LABEL})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
