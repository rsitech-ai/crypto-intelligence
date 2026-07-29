use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn run_replay(manifest: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crypto-replay"))
        .args(["--manifest", &manifest.display().to_string(), "--verify"])
        .current_dir(workspace_root())
        .output()
        .expect("run crypto-replay")
}

fn run_feature_replay(manifest: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crypto-replay"))
        .args([
            "--manifest",
            &manifest.display().to_string(),
            "--verify-features",
        ])
        .current_dir(workspace_root())
        .output()
        .expect("run feature replay")
}

#[test]
fn verify_emits_one_stable_machine_readable_summary() {
    let root = workspace_root();
    let output = run_replay(&root.join("fixtures/golden-replays/market-foundation/manifest.toml"));

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).expect("JSON summary");
    assert_eq!(summary["schema_version"], 1);
    assert_eq!(summary["status"], "pass");
    assert_eq!(summary["replayed_records"], 3);
    assert_eq!(summary["normalized_events"], 3);
    assert_eq!(summary["quality_incidents"], 0);
    assert_eq!(summary["silent_integrity_failures"], 0);
    assert_eq!(summary["logical_elapsed_ns"], 20);
    assert_eq!(
        summary["normalized_event_hash"],
        "bff5485b87ed0d85053aa6cb4be4888e34eb18175b25077567ceacb2de902fc7"
    );
    assert_eq!(
        summary["book_state_hash"],
        "0df4cebb4f07b01926a89952e826e55e412820e1927253ca47730e1cb7875e72"
    );
    assert_eq!(
        summary["quality_incident_hash"],
        "29c1739deeec06d20afc3bdb2bb0b4e0141ceec7f443d13dca422d206660b684"
    );
}

#[test]
fn verify_returns_nonzero_without_stdout_when_a_digest_mismatches() {
    let root = workspace_root();
    let output =
        run_replay(&root.join("fixtures/golden-replays/market-foundation/digest-mismatch.toml"));

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(error.starts_with("crypto-replay: FAIL: digest mismatch:"));
    assert!(
        error.contains("events=bff5485b87ed0d85053aa6cb4be4888e34eb18175b25077567ceacb2de902fc7")
    );
}

#[test]
fn verify_features_proves_live_wal_and_parquet_parity() {
    let root = workspace_root();
    let output =
        run_feature_replay(&root.join("fixtures/golden-replays/features-v1/manifest.toml"));

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).expect("JSON summary");
    assert_eq!(summary["schema_version"], 1);
    assert_eq!(summary["status"], "pass");
    assert_eq!(summary["replayed_feature_rows"], 3);
    assert_eq!(summary["float_max_abs_error"], 0.0);
    for field in [
        "fixed_point_hash",
        "lineage_hash",
        "finality_quality_hash",
        "wal_segment_blake3",
        "parquet_manifest_blake3",
    ] {
        assert_eq!(
            summary[field].as_str().map(str::len),
            Some(64),
            "missing stable digest field {field}"
        );
        assert_ne!(summary[field], "0".repeat(64));
    }
}
