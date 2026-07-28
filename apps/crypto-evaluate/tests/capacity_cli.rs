use std::{
    fs,
    os::unix::fs::symlink,
    path::Path,
    process::{Command, Output},
};

use capacity::{CapacityEvidence, CapacityInput, EvidenceLevel, HardwareProfile, WorkloadProfile};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn input(evidence: EvidenceLevel) -> CapacityInput {
    CapacityInput::new(
        HardwareProfile {
            schema_version: 1,
            apple_silicon: true,
            logical_cpus: 12,
            memory_bytes: 64 * GIB,
            free_disk_bytes: 2_000 * GIB,
            sustained_inbound_bytes_per_second: 50 * MIB,
            burst_inbound_bytes_per_second: 250 * MIB,
            sustained_disk_write_bytes_per_second: 500 * MIB,
            burst_disk_write_bytes_per_second: 1_000 * MIB,
            sustained_normalized_events_per_second: 100_000,
            burst_normalized_events_per_second: 400_000,
            evidence: CapacityEvidence {
                schema_version: 1,
                level: evidence,
                capacity_uncertainty_ppm: 100_000,
                sample_count: u32::from(evidence == EvidenceLevel::Measured),
                observed_at_unix_seconds: u64::from(evidence == EvidenceLevel::Measured),
                measurement_blake3: if evidence == EvidenceLevel::Measured {
                    [0x42; 32]
                } else {
                    [0; 32]
                },
            },
        },
        WorkloadProfile {
            schema_version: 1,
            tier_a_instruments: 1,
            tier_b_instruments: 1,
            tier_c_instruments: 0,
            tier_a_retention_days: 14,
            tier_b_retention_days: 14,
            tier_c_retention_days: 0,
            sustained_events_per_second: 1_000,
            burst_events_per_second: 2_000,
            demand_uncertainty_ppm: 1_000_000,
            raw_bytes_per_event: 512,
            normalized_bytes_per_event: 256,
            wal_write_amplification_ppm: 1_100_000,
            parquet_write_amplification_ppm: 1_200_000,
            feature_cpu_nanos_per_event: 50_000,
            tier_a_book_memory_bytes: 256 * MIB,
            tier_b_book_memory_bytes: 64 * MIB,
            tier_c_book_memory_bytes: 16 * MIB,
            feature_memory_bytes_per_instrument: 32 * MIB,
            model_memory_bytes: 2 * GIB,
            ingestion_queue_memory_bytes: 64 * MIB,
            rpc_inflight_memory_bytes: MIB,
            runtime_memory_bytes: 8 * MIB,
            concurrent_replay: false,
            replay_event_count: 0,
            replay_events_per_second: 0,
            replay_cpu_multiplier_ppm: 1_000_000,
        },
    )
}

fn run(input: &Path, format: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crypto-evaluate"))
        .args([
            "capacity",
            "--input",
            input.to_str().expect("test path must be UTF-8"),
            "--format",
            format,
        ])
        .output()
        .expect("capacity CLI must run")
}

#[test]
fn fitting_preview_is_deterministic_and_requires_verified_evidence() {
    let directory = tempfile::tempdir().expect("temporary input root");
    let path = directory.path().join("capacity.json");
    fs::write(
        &path,
        serde_json::to_vec(&input(EvidenceLevel::Measured)).expect("input must encode"),
    )
    .expect("input must write");

    let first = run(&path, "json");
    let second = run(&path, "json");
    assert_eq!(first.status.code(), Some(3));
    assert_eq!(first.stdout, second.stdout);
    assert!(first.stderr.is_empty());
    let decision: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("preview must be JSON");
    assert_eq!(decision["status"], "evidence_required");
    assert_eq!(decision["estimate"]["inbound_bytes_per_second"], 512_000);
}

#[test]
fn evidence_and_resource_refusals_have_distinct_stable_exit_codes() {
    let directory = tempfile::tempdir().expect("temporary input root");
    let evidence_path = directory.path().join("evidence.json");
    fs::write(
        &evidence_path,
        serde_json::to_vec(&input(EvidenceLevel::Measured)).expect("input must encode"),
    )
    .expect("input must write");
    let evidence = run(&evidence_path, "json");
    assert_eq!(evidence.status.code(), Some(3));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&evidence.stdout)
            .expect("evidence decision must be JSON")["status"],
        "evidence_required"
    );

    let mut rejected_input = input(EvidenceLevel::Measured);
    rejected_input.hardware.memory_bytes = GIB;
    let rejected_path = directory.path().join("rejected.json");
    fs::write(
        &rejected_path,
        serde_json::to_vec(&rejected_input).expect("input must encode"),
    )
    .expect("input must write");
    let rejected = run(&rejected_path, "json-pretty");
    assert_eq!(rejected.status.code(), Some(4));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&rejected.stdout)
            .expect("rejected decision must be JSON")["status"],
        "rejected"
    );
}

#[test]
fn forged_certification_is_invalid_and_never_receives_exit_zero() {
    let directory = tempfile::tempdir().expect("temporary input root");
    let path = directory.path().join("forged.json");
    let json = serde_json::to_string(&input(EvidenceLevel::Measured)).expect("input must encode");
    fs::write(&path, json.replace("\"measured\"", "\"certified\"")).expect("input must write");

    let output = run(&path, "json");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_oversized_and_symlink_inputs_fail_without_echoing_contents() {
    let directory = tempfile::tempdir().expect("temporary input root");
    let invalid_path = directory.path().join("invalid.json");
    fs::write(&invalid_path, r#"{"secret":"cli-secret-sentinel"}"#).expect("invalid input");
    let invalid = run(&invalid_path, "json");
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&invalid.stderr).contains("cli-secret-sentinel"));

    let oversized_path = directory.path().join("oversized.json");
    fs::write(&oversized_path, vec![b'x'; 1024 * 1024 + 1]).expect("oversized input");
    let oversized = run(&oversized_path, "json");
    assert_eq!(oversized.status.code(), Some(1));
    assert!(oversized.stdout.is_empty());

    let link_path = directory.path().join("input-link.json");
    symlink(&invalid_path, &link_path).expect("input symlink");
    let linked = run(&link_path, "json");
    assert_eq!(linked.status.code(), Some(1));
    assert!(linked.stdout.is_empty());
}
