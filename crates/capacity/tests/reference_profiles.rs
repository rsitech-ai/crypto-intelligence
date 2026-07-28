use capacity::{
    AdmissionDecision, AdmissionReason, CapacityError, CapacityEstimate, CapacityEvidence,
    CapacityInput, EvidenceLevel, HardwareProfile, WorkloadProfile, admit,
};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

fn measured_input() -> CapacityInput {
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
                level: EvidenceLevel::Measured,
                capacity_uncertainty_ppm: 100_000,
                sample_count: 240,
                observed_at_unix_seconds: 1_800_000_000,
                measurement_blake3: [0x42; 32],
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

#[test]
fn estimate_uses_exact_checked_integer_units_and_reports_uncertainty() {
    let estimate =
        CapacityEstimate::calculate(&measured_input()).expect("measured input must calculate");

    assert_eq!(estimate.inbound_bytes_per_second, 512_000);
    assert_eq!(estimate.burst_inbound_bytes_per_second, 1_024_000);
    assert_eq!(estimate.raw_bytes_per_day, 44_236_800_000);
    assert_eq!(estimate.normalized_bytes_per_day, 22_118_400_000);
    assert_eq!(estimate.wal_write_bytes_per_second, 563_200);
    assert_eq!(estimate.parquet_write_bytes_per_second, 307_200);
    assert_eq!(estimate.total_write_bytes_per_second, 870_400);
    assert_eq!(estimate.burst_total_write_bytes_per_second, 1_740_800);
    assert_eq!(estimate.feature_cpu_cores_ppm, 50_000);
    assert_eq!(estimate.book_memory_bytes, 320 * MIB);
    assert_eq!(estimate.feature_memory_bytes, 64 * MIB);
    assert_eq!(estimate.peak_memory_bytes, 2_626_682_880);
    assert!(estimate.retention_days_supported >= 14);
    assert_eq!(estimate.capacity_uncertainty_ppm, 100_000);
    assert_eq!(estimate.demand_uncertainty_ppm, 1_000_000);
    assert_eq!(estimate.sustained_event_headroom_ppm, 90_000_000);
    assert_eq!(estimate.burst_event_headroom_ppm, 180_000_000);
}

#[test]
fn fitting_declared_profiles_require_repository_verified_evidence() {
    assert!(matches!(
        admit(&measured_input()).expect("measured input must calculate"),
        AdmissionDecision::EvidenceRequired { .. }
    ));

    let mut conservative = measured_input();
    conservative.hardware.evidence = CapacityEvidence {
        schema_version: 1,
        level: EvidenceLevel::ConservativeDefault,
        capacity_uncertainty_ppm: 500_000,
        sample_count: 0,
        observed_at_unix_seconds: 0,
        measurement_blake3: [0; 32],
    };
    assert!(matches!(
        admit(&conservative).expect("conservative input must calculate"),
        AdmissionDecision::EvidenceRequired { .. }
    ));
}

#[test]
fn caller_cannot_deserialize_a_self_asserted_certification() {
    let json = serde_json::to_string(&measured_input()).expect("fixture must serialize");
    let forged = json.replace("\"measured\"", "\"certified\"");
    assert!(serde_json::from_str::<CapacityInput>(&forged).is_err());
}

#[test]
fn tier_a_failure_is_rejected_without_a_silent_downgrade() {
    let mut input = measured_input();
    input.hardware.memory_bytes = 2 * GIB;
    input.workload.tier_a_instruments = 20;
    input.workload.tier_b_instruments = 100;

    let AdmissionDecision::Rejected {
        reasons, downgrade, ..
    } = admit(&input).expect("oversized input must calculate")
    else {
        panic!("unsafe Tier A workload must be rejected");
    };
    assert!(reasons.contains(&AdmissionReason::MemoryBudgetExceeded));
    assert!(
        downgrade.is_none(),
        "a proposal cannot pretend optional coverage fixes required Tier A"
    );
}

#[test]
fn optional_downgrade_preserves_tier_a_coverage_and_retention() {
    let mut input = measured_input();
    input.hardware.memory_bytes = 8 * GIB;
    input.workload.tier_b_instruments = 100;
    input.workload.tier_c_instruments = 100;
    input.workload.tier_c_retention_days = 14;

    let AdmissionDecision::Rejected {
        reasons,
        downgrade: Some(proposal),
        ..
    } = admit(&input).expect("oversized optional workload must calculate")
    else {
        panic!("optional coverage pressure must return an explicit proposal");
    };
    assert!(reasons.contains(&AdmissionReason::MemoryBudgetExceeded));
    assert_eq!(
        proposal.tier_a_instruments,
        input.workload.tier_a_instruments
    );
    assert_eq!(
        proposal.tier_a_retention_days,
        input.workload.tier_a_retention_days
    );
    assert_eq!(proposal.tier_b_instruments, 0);
    assert_eq!(proposal.tier_c_instruments, 0);
    assert_eq!(
        proposal.quality_impact.tier_a_retention_days_preserved,
        input.workload.tier_a_retention_days
    );
    assert_eq!(
        proposal.quality_impact.removed_tier_b_instruments,
        input.workload.tier_b_instruments
    );
    assert_eq!(
        proposal.quality_impact.removed_tier_c_instruments,
        input.workload.tier_c_instruments
    );
    assert!(!proposal.automatically_applied);
}

#[test]
fn bandwidth_write_and_event_reasons_are_explicit_and_stable() {
    let mut input = measured_input();
    input.hardware.apple_silicon = false;
    input.hardware.memory_bytes = GIB;
    input.hardware.free_disk_bytes = GIB;
    input.hardware.sustained_inbound_bytes_per_second = 1;
    input.hardware.burst_inbound_bytes_per_second = 1;
    input.hardware.sustained_disk_write_bytes_per_second = 1;
    input.hardware.burst_disk_write_bytes_per_second = 1;
    input.hardware.sustained_normalized_events_per_second = 1;
    input.hardware.burst_normalized_events_per_second = 1;

    let first = admit(&input).expect("rejected input must calculate");
    let second = admit(&input).expect("rejected input must be deterministic");
    assert_eq!(
        serde_json::to_vec(&first).expect("decision must serialize"),
        serde_json::to_vec(&second).expect("decision must serialize")
    );
    let AdmissionDecision::Rejected { reasons, .. } = first else {
        panic!("resource-starved input must reject");
    };
    assert!(reasons.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(reasons.contains(&AdmissionReason::SustainedBandwidthHeadroomInsufficient));
    assert!(reasons.contains(&AdmissionReason::BurstBandwidthHeadroomInsufficient));
    assert!(reasons.contains(&AdmissionReason::SustainedWriteHeadroomInsufficient));
    assert!(reasons.contains(&AdmissionReason::BurstWriteHeadroomInsufficient));
}

#[test]
fn concurrent_replay_reports_duration_io_and_cpu_cost() {
    let mut input = measured_input();
    input.workload.concurrent_replay = true;
    input.workload.replay_event_count = 10_000;
    input.workload.replay_events_per_second = 100;
    input.workload.replay_cpu_multiplier_ppm = 2_000_000;

    let estimate = CapacityEstimate::calculate(&input).expect("replay input must calculate");
    assert_eq!(estimate.replay_event_count, 10_000);
    assert_eq!(estimate.replay_duration_seconds, 100);
    assert_eq!(estimate.replay_read_bytes, 7_680_000);
    assert_eq!(estimate.replay_cpu_core_seconds, 1);
    assert_eq!(estimate.replay_cpu_cores_ppm, 10_000);
}

#[test]
fn positive_cpu_demand_rounds_up_and_runtime_reservations_are_counted() {
    let mut input = measured_input();
    input.workload.sustained_events_per_second = 1;
    input.workload.burst_events_per_second = 1;
    input.workload.feature_cpu_nanos_per_event = 1;
    let estimate = CapacityEstimate::calculate(&input).expect("boundary input must calculate");
    assert_eq!(estimate.feature_cpu_cores_ppm, 1);
    assert_eq!(
        estimate.peak_memory_bytes,
        estimate.book_memory_bytes
            + estimate.feature_memory_bytes
            + estimate.model_memory_bytes
            + estimate.ingestion_queue_memory_bytes
            + estimate.rpc_inflight_memory_bytes
            + estimate.runtime_memory_bytes
    );
}

#[test]
fn invalid_and_overflowing_inputs_fail_closed() {
    let mut invalid = measured_input();
    invalid.workload.tier_a_instruments = 0;
    assert!(matches!(
        admit(&invalid),
        Err(CapacityError::InvalidInput { .. })
    ));

    let mut invalid_evidence = measured_input();
    invalid_evidence.hardware.evidence.sample_count = 0;
    assert!(matches!(
        admit(&invalid_evidence),
        Err(CapacityError::InvalidInput { .. })
    ));

    let mut unbounded_default = measured_input();
    unbounded_default.hardware.evidence = CapacityEvidence {
        schema_version: 1,
        level: EvidenceLevel::ConservativeDefault,
        capacity_uncertainty_ppm: 0,
        sample_count: 0,
        observed_at_unix_seconds: 0,
        measurement_blake3: [0; 32],
    };
    assert!(matches!(
        admit(&unbounded_default),
        Err(CapacityError::InvalidInput { .. })
    ));

    let mut overflow = measured_input();
    overflow.workload.sustained_events_per_second = u64::MAX;
    overflow.workload.burst_events_per_second = u64::MAX;
    overflow.workload.raw_bytes_per_event = u64::MAX;
    assert_eq!(admit(&overflow), Err(CapacityError::Overflow));
}
