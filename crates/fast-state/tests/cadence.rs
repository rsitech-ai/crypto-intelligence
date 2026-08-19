use std::{collections::BTreeSet, sync::Arc};

use domain::{AssetId, AssetNamespace, SourceId, SourceKind, UnixNanos};
use fast_state::{
    CoalescingSnapshotSlot, EventTimeliness, FastStateError, FastStateScheduler, FastStateSnapshot,
    FastStateSnapshotInput, SnapshotHealth, TickKind,
};
use feature_registry::{
    CodeRevision, DurationNanos, EntityScope, EventTimePolicy, FeatureConsumptionRole,
    FeatureDatum, FeatureDefinition, FeatureDefinitionInput, FeatureDocumentation, FeatureEntity,
    FeatureId, FeatureObservation, FeatureObservationInput, FeatureRegistry, FeatureStatus,
    FeatureValue, FeatureValueType, FinalityState, FiniteF64, FormulaHash, InputRequirement,
    LineageHash, MissingnessPolicy, MissingnessReason, NormalizationKind, NormalizationPolicy,
    ObservationRevision, QualityRequirement, QualityScore, RegistryError, SourceCoverage,
    SourceCoverageEntry, WindowDefinition, WindowId, WindowKind,
};
use quality::SourceHealthState;
use semver::Version;

const INTERNAL_TICK_NS: i64 = 100_000_000;
const PUBLIC_TICK_NS: i64 = 1_000_000_000;

#[test]
fn replay_emits_exact_internal_and_public_tick_counts() {
    let mut scheduler = FastStateScheduler::try_new(0, 64).expect("scheduler");

    scheduler
        .advance_to(5 * PUBLIC_TICK_NS)
        .expect("five-second replay");

    assert_eq!(scheduler.count(TickKind::Internal100Ms), 50);
    assert_eq!(scheduler.count(TickKind::Published1S), 5);
    assert_eq!(scheduler.pending_len(), 55);
}

#[test]
fn arbitrary_start_time_preserves_epoch_congruent_boundaries() {
    let mut scheduler = FastStateScheduler::try_new(50_000_000, 16).expect("scheduler");
    scheduler.advance_to(PUBLIC_TICK_NS).expect("advance");

    let ticks = scheduler.drain_ticks(16);
    assert_eq!(ticks.len(), 11);
    assert_eq!(ticks[0].scheduled_at_ns(), INTERNAL_TICK_NS);
    assert!(ticks.iter().all(|tick| match tick.kind() {
        TickKind::Internal100Ms => tick.scheduled_at_ns() % INTERNAL_TICK_NS == 0,
        TickKind::Published1S => tick.scheduled_at_ns() % PUBLIC_TICK_NS == 0,
    }));
}

#[test]
fn coincident_ticks_run_internal_evaluation_before_publication() {
    let mut scheduler = FastStateScheduler::try_new(0, 16).expect("scheduler");
    scheduler
        .advance_to(PUBLIC_TICK_NS)
        .expect("one-second replay");

    let ticks = scheduler.drain_ticks(16);
    assert_eq!(ticks.len(), 11);
    assert_eq!(ticks[9].scheduled_at_ns(), PUBLIC_TICK_NS);
    assert_eq!(ticks[9].kind(), TickKind::Internal100Ms);
    assert_eq!(ticks[10].scheduled_at_ns(), PUBLIC_TICK_NS);
    assert_eq!(ticks[10].kind(), TickKind::Published1S);
    assert!(
        ticks
            .windows(2)
            .all(|pair| pair[0].sequence() < pair[1].sequence())
    );
}

#[test]
fn replay_chunk_size_does_not_change_tick_sequence() {
    let mut single = FastStateScheduler::try_new(50_000_000, 64).expect("single scheduler");
    single
        .advance_to(5 * PUBLIC_TICK_NS)
        .expect("single advance");
    let expected = single.drain_ticks(64);

    let mut chunked = FastStateScheduler::try_new(50_000_000, 16).expect("chunked scheduler");
    let mut actual = Vec::new();
    for second in 1..=5 {
        chunked
            .advance_to(second * PUBLIC_TICK_NS)
            .expect("chunked advance");
        actual.extend(chunked.drain_ticks(16));
    }

    assert_eq!(actual, expected);
}

#[test]
fn backlog_exhaustion_is_transactional_and_never_skips_ticks() {
    let mut scheduler = FastStateScheduler::try_new(0, 10).expect("scheduler");

    assert_eq!(
        scheduler.advance_to(PUBLIC_TICK_NS),
        Err(FastStateError::TickBacklogCapacity)
    );
    assert_eq!(scheduler.now_ns(), 0);
    assert_eq!(scheduler.pending_len(), 0);
    assert_eq!(scheduler.count(TickKind::Internal100Ms), 0);
    assert_eq!(scheduler.count(TickKind::Published1S), 0);

    scheduler
        .advance_to(9 * INTERNAL_TICK_NS)
        .expect("bounded catch-up");
    assert_eq!(scheduler.pending_len(), 9);
}

#[test]
fn clock_regression_cancellation_and_future_event_time_fail_closed() {
    assert_eq!(
        FastStateScheduler::try_new(-1, 1),
        Err(FastStateError::InvalidScheduler)
    );
    assert_eq!(
        FastStateScheduler::try_new(0, 0),
        Err(FastStateError::InvalidScheduler)
    );
    assert_eq!(
        FastStateScheduler::try_new(0, 65_537),
        Err(FastStateError::InvalidScheduler)
    );

    let mut scheduler = FastStateScheduler::try_new(1_000, 16).expect("scheduler");
    assert_eq!(
        scheduler.advance_to(999),
        Err(FastStateError::ClockRegression {
            current_ns: 1_000,
            attempted_ns: 999,
        })
    );

    scheduler.advance_to(2_000).expect("advance");
    assert_eq!(
        scheduler
            .classify_event_time(1_500)
            .expect("late event is inspectable"),
        EventTimeliness::Late
    );
    assert_eq!(
        scheduler.classify_event_time(2_000).expect("on-time event"),
        EventTimeliness::OnTime
    );
    assert_eq!(
        scheduler.classify_event_time(2_001),
        Err(FastStateError::EventAheadOfScheduler)
    );
    assert_eq!(
        scheduler.classify_event_time(-1),
        Err(FastStateError::InvalidEventTime)
    );

    scheduler.cancel();
    assert!(scheduler.is_cancelled());
    assert_eq!(scheduler.advance_to(3_000), Err(FastStateError::Cancelled));
}

#[test]
fn snapshot_derives_missingness_quality_and_identity_from_validated_observations() {
    let (registry, input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![
            FeatureDatum::Present(FeatureValue::Float64(FiniteF64::new(1.5).expect("finite"))),
            FeatureDatum::Missing(MissingnessReason::InsufficientHistory),
            FeatureDatum::Present(FeatureValue::Float64(FiniteF64::new(-0.0).expect("finite"))),
        ],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x20,
    );

    let first = build_snapshot(&registry, input.clone()).expect("first snapshot");
    let second = build_snapshot(&registry, input).expect("second snapshot");

    assert_eq!(first.health(), SnapshotHealth::Degraded);
    assert_eq!(first.feature_count(), 3);
    assert_eq!(first.missing_feature_count(), 1);
    assert_eq!(first.feature(0), Some(Some(1.5)));
    assert_eq!(first.feature(1), Some(None));
    assert_eq!(first.feature(2), Some(Some(0.0)));
    assert_eq!(first.feature(3), None);
    assert_eq!(
        first.missingness_reason(1),
        Some(Some(MissingnessReason::InsufficientHistory))
    );
    assert_eq!(first.missingness_reason(0), Some(None));
    assert_eq!(
        first.feature_vector().iter().collect::<Vec<_>>(),
        vec![Some(1.5), None, Some(0.0)]
    );
    assert_eq!(first.minimum_source_coverage_millionths(), 1_000_000);
    assert_eq!(first.minimum_quality_score_millionths(), 950_000);
    assert_eq!(first.evidence_hash(), second.evidence_hash());
    assert!(first.evidence_hash().iter().any(|byte| *byte != 0));
}

#[test]
fn snapshot_health_cannot_hide_degraded_or_unhealthy_observations() {
    let threshold_healthy = snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        nine_of_ten_coverage(),
        900_000,
    );
    assert_eq!(threshold_healthy.health(), SnapshotHealth::Healthy);

    let low_quality = snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        899_999,
    );
    assert_eq!(low_quality.health(), SnapshotHealth::Degraded);

    let unavailable_quality = snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        699_999,
    );
    assert_eq!(unavailable_quality.health(), SnapshotHealth::Unavailable);

    let all_missing = snapshot_with(
        PUBLIC_TICK_NS,
        vec![FeatureDatum::Missing(
            MissingnessReason::InsufficientHistory,
        )],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
    );
    assert_eq!(all_missing.health(), SnapshotHealth::Unavailable);

    let stale_missing = snapshot_with(
        PUBLIC_TICK_NS,
        vec![FeatureDatum::Missing(MissingnessReason::Stale)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
    );
    assert_eq!(stale_missing.health(), SnapshotHealth::Unavailable);

    let (stale_registry, mut stale_input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x20,
    );
    stale_input.as_of_event_time_ns = 61 * PUBLIC_TICK_NS;
    let ttl_boundary = build_snapshot(&stale_registry, stale_input.clone()).expect("TTL boundary");
    assert_eq!(ttl_boundary.health(), SnapshotHealth::Healthy);

    stale_input.as_of_event_time_ns = 62 * PUBLIC_TICK_NS;
    let stale_snapshot = build_snapshot(&stale_registry, stale_input).expect("stale evidence");
    assert_eq!(stale_snapshot.health(), SnapshotHealth::Unavailable);

    let degraded = snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Degraded),
        950_000,
    );
    assert_eq!(degraded.health(), SnapshotHealth::Degraded);

    let low_coverage = snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        partial_coverage(),
        950_000,
    );
    assert_eq!(low_coverage.health(), SnapshotHealth::Unavailable);

    let unhealthy = snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Unhealthy),
        950_000,
    );
    assert_eq!(unhealthy.health(), SnapshotHealth::Unavailable);
}

#[test]
fn snapshot_rejects_bad_time_count_duplicates_and_unregistered_observations() {
    let (registry, mut input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x20,
    );
    input.as_of_event_time_ns -= 1;
    assert_eq!(
        FastStateSnapshot::try_for_published_tick(
            &registry,
            published_tick(PUBLIC_TICK_NS),
            input.clone()
        ),
        Err(FastStateError::PublicationTimeMismatch)
    );

    let (_, future) = snapshot_input(
        2 * PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x20,
    );
    input = future;
    input.as_of_event_time_ns = PUBLIC_TICK_NS;
    assert_eq!(
        FastStateSnapshot::try_for_published_tick(
            &registry,
            published_tick(PUBLIC_TICK_NS),
            input.clone()
        ),
        Err(FastStateError::InvalidSnapshotTime),
        "an observation from the future must not enter an earlier snapshot"
    );

    input.as_of_event_time_ns = PUBLIC_TICK_NS;
    input.observations.clear();
    assert_eq!(
        build_snapshot(&registry, input.clone()),
        Err(FastStateError::InvalidFeatureCount)
    );

    let (_, one) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x20,
    );
    input.observations = vec![one.observations[0].clone(); 4_097];
    assert_eq!(
        build_snapshot(&registry, input.clone()),
        Err(FastStateError::InvalidFeatureCount)
    );

    input.observations.truncate(2);
    assert_eq!(
        build_snapshot(&registry, input.clone()),
        Err(FastStateError::DuplicateFeatureObservation)
    );

    input.observations.truncate(1);
    assert_eq!(
        build_snapshot(&FeatureRegistry::new(), input),
        Err(FastStateError::InvalidFeatureObservation(
            RegistryError::UnknownDefinition
        ))
    );

    let (mixed_registry, mut mixed) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(1.0), present(2.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x20,
    );
    mixed.observations[1] = observation(
        1,
        FeatureEntity::Asset(
            AssetId::new(AssetNamespace::Native, "ethereum", "", "ETH", 1).expect("other asset"),
        ),
        present(2.0),
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        ObservationEvidence {
            event_time_end_ns: PUBLIC_TICK_NS,
            known_offset_ns: 100,
            lineage_seed: 0x20,
        },
    );
    assert_eq!(
        build_snapshot(&mixed_registry, mixed),
        Err(FastStateError::MixedSnapshotEntity)
    );

    let mut integer_registry = FeatureRegistry::new();
    integer_registry
        .register(definition_with_value_type(0, FeatureValueType::Integer))
        .expect("integer definition");
    let integer_observation = observation(
        0,
        FeatureEntity::Asset(
            AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("asset"),
        ),
        FeatureDatum::Present(FeatureValue::Integer(1)),
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        ObservationEvidence {
            event_time_end_ns: PUBLIC_TICK_NS,
            known_offset_ns: 100,
            lineage_seed: 0x20,
        },
    );
    assert_eq!(
        build_snapshot(
            &integer_registry,
            FastStateSnapshotInput {
                as_of_event_time_ns: PUBLIC_TICK_NS,
                observations: vec![integer_observation],
            },
        ),
        Err(FastStateError::UnsupportedFeatureValue)
    );
}

#[test]
fn snapshot_evidence_commits_semantics_without_unrelated_registry_coupling() {
    let coverage = full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy);
    let (mut registry, input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(1.0), present(0.0)],
        coverage.clone(),
        950_000,
        100,
        0x20,
    );
    let baseline_snapshot = build_snapshot(&registry, input.clone()).expect("baseline snapshot");
    let baseline = baseline_snapshot.evidence_hash();

    let mut reversed = input.clone();
    reversed.observations.reverse();
    assert_eq!(
        build_snapshot(&registry, reversed)
            .expect("reordered")
            .evidence_hash(),
        baseline
    );

    let variants = [
        snapshot_hash(
            vec![present(2.0), present(0.0)],
            coverage.clone(),
            950_000,
            100,
            0x20,
        ),
        snapshot_hash(
            vec![
                present(1.0),
                FeatureDatum::Missing(MissingnessReason::Stale),
            ],
            coverage.clone(),
            950_000,
            100,
            0x20,
        ),
        snapshot_hash(
            vec![present(1.0), present(0.0)],
            partial_coverage(),
            950_000,
            100,
            0x20,
        ),
        snapshot_hash(
            vec![present(1.0), present(0.0)],
            coverage.clone(),
            949_999,
            100,
            0x20,
        ),
        snapshot_hash(
            vec![present(1.0), present(0.0)],
            coverage.clone(),
            950_000,
            101,
            0x20,
        ),
        snapshot_hash(
            vec![present(1.0), present(0.0)],
            coverage,
            950_000,
            100,
            0x40,
        ),
    ];
    assert_eq!(variants.into_iter().collect::<BTreeSet<_>>().len(), 6);
    assert!(!variants.contains(&baseline));

    registry
        .register(definition(99))
        .expect("unrelated registered feature");
    let with_unrelated_definition =
        build_snapshot(&registry, input).expect("same selected feature schema");
    assert_eq!(
        with_unrelated_definition.feature_schema_hash(),
        baseline_snapshot.feature_schema_hash(),
        "unselected registry entries must not invalidate a selected vector schema"
    );
    assert_eq!(with_unrelated_definition.evidence_hash(), baseline);

    assert_eq!(
        snapshot_hash(
            vec![present(1.0), present(-0.0)],
            full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
            950_000,
            100,
            0x20,
        ),
        baseline,
        "negative zero must canonicalize before evidence hashing"
    );
}

#[test]
fn ui_slot_coalesces_corrections_without_accepting_regression_or_entity_mix() {
    let first = Arc::new(snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
    ));
    let correction = snapshot_correction(&first, 2.0, 200, 2).expect("first correction");
    let stale_later_known = snapshot_correction(&first, 3.0, 300, 1);
    let older = Arc::new(snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
    ));
    let other_entity = Arc::new(snapshot_for_entity(
        AssetId::new(AssetNamespace::Native, "ethereum", "", "ETH", 1).expect("other asset"),
    ));
    let mut slot = CoalescingSnapshotSlot::new();

    assert_eq!(slot.publish(Arc::clone(&first)), Ok(1));
    assert_eq!(slot.publish(Arc::clone(&correction)), Ok(2));
    assert_eq!(slot.coalesced_total(), 1);
    assert_eq!(
        stale_later_known,
        Err(FastStateError::SnapshotCorrectionMismatch)
    );
    assert_eq!(slot.publish(older), Err(FastStateError::SnapshotRegression));
    assert_eq!(
        slot.publish(other_entity),
        Err(FastStateError::SnapshotEntityMismatch)
    );

    let delivered = slot.take_latest().expect("latest correction");
    assert_eq!(delivered.sequence(), 2);
    assert_eq!(delivered.coalesced_before_delivery(), 1);
    assert_eq!(delivered.snapshot().feature(0), Some(Some(2.0)));

    let next_correction = snapshot_correction(&correction, 4.0, 400, 3).expect("next correction");
    assert_eq!(
        slot.publish(Arc::clone(&next_correction)),
        Ok(3),
        "revision validation must survive delivery of the previous snapshot"
    );
    assert_eq!(
        slot.take_latest()
            .expect("post-delivery correction")
            .snapshot()
            .evidence_hash(),
        next_correction.evidence_hash()
    );
}

#[test]
fn published_ticks_build_authoritative_snapshots_while_slow_ui_coalesces() {
    let mut scheduler = FastStateScheduler::try_new(0, 16).expect("scheduler");
    scheduler.advance_to(PUBLIC_TICK_NS).expect("first second");
    let mut first_ticks = scheduler.drain_ticks(16);
    let internal_index = first_ticks
        .iter()
        .position(|tick| tick.kind() == TickKind::Internal100Ms)
        .expect("internal tick");
    let first_internal = first_ticks.remove(internal_index);
    let published_index = first_ticks
        .iter()
        .position(|tick| tick.kind() == TickKind::Published1S)
        .expect("published tick");
    let first_published = first_ticks.remove(published_index);
    let (first_registry, first_input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x20,
    );

    assert_eq!(
        FastStateSnapshot::try_for_published_tick(
            &first_registry,
            first_internal,
            first_input.clone(),
        ),
        Err(FastStateError::PublicationRequiresPublishedTick)
    );
    let mut mismatched = first_input.clone();
    mismatched.as_of_event_time_ns = 2 * PUBLIC_TICK_NS;
    assert_eq!(
        FastStateSnapshot::try_for_published_tick(
            &first_registry,
            published_tick(PUBLIC_TICK_NS),
            mismatched,
        ),
        Err(FastStateError::PublicationTimeMismatch)
    );

    let first = Arc::new(
        FastStateSnapshot::try_for_published_tick(&first_registry, first_published, first_input)
            .expect("first authoritative snapshot"),
    );
    let mut ui = CoalescingSnapshotSlot::new();
    ui.publish(Arc::clone(&first)).expect("first UI state");

    scheduler
        .advance_to(2 * PUBLIC_TICK_NS)
        .expect("authoritative scheduler is independent from full UI slot");
    let second_published = scheduler
        .drain_ticks(16)
        .into_iter()
        .find(|tick| tick.kind() == TickKind::Published1S)
        .expect("second published tick");
    let (second_registry, second_input) = snapshot_input(
        2 * PUBLIC_TICK_NS,
        vec![present(2.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x30,
    );
    let second = Arc::new(
        FastStateSnapshot::try_for_published_tick(&second_registry, second_published, second_input)
            .expect("second authoritative snapshot"),
    );
    ui.publish(Arc::clone(&second))
        .expect("slow UI coalesces instead of blocking");

    assert_eq!(scheduler.count(TickKind::Internal100Ms), 20);
    assert_eq!(scheduler.count(TickKind::Published1S), 2);
    assert_eq!(ui.coalesced_total(), 1);
    let delivered = ui.take_latest().expect("latest UI snapshot");
    assert_eq!(delivered.snapshot().evidence_hash(), second.evidence_hash());
    assert_ne!(first.evidence_hash(), second.evidence_hash());
}

#[test]
fn ui_slot_rejects_unrevisioned_changes_inside_a_partial_correction() {
    let first = Arc::new(snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0), present(10.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
    ));
    let (registry, mut corrected_input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(2.0), present(999.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        200,
        0x30,
    );
    let mut unrevisioned = corrected_input.observations.remove(1).into_input();
    unrevisioned.finality_state = FinalityState::Provisional;
    unrevisioned.revision = ObservationRevision::new(1).expect("original revision");
    corrected_input.observations.push(
        FeatureObservation::try_new(unrevisioned).expect("valid but semantically changed revision"),
    );
    assert_eq!(
        FastStateSnapshot::try_correction(&registry, &first, corrected_input),
        Err(FastStateError::SnapshotCorrectionMismatch)
    );
}

#[test]
fn correction_constructor_rejects_hidden_observation_time_regressions() {
    let first = snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0), present(10.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
    );
    let (registry, candidate) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(2.0), present(20.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        300,
        0x30,
    );

    let mut knowledge_regression = candidate.clone();
    let mut regressed = knowledge_regression.observations.remove(0).into_input();
    regressed.as_known_at = UnixNanos::new(PUBLIC_TICK_NS + 50);
    regressed.computed_at = UnixNanos::new(PUBLIC_TICK_NS + 51);
    regressed.finality_as_known_at = UnixNanos::new(PUBLIC_TICK_NS + 50);
    knowledge_regression
        .observations
        .push(FeatureObservation::try_new(regressed).expect("locally valid regression"));
    assert_eq!(
        FastStateSnapshot::try_correction(&registry, &first, knowledge_regression),
        Err(FastStateError::SnapshotCorrectionMismatch),
        "a second feature's later timestamp must not conceal a per-feature regression"
    );

    let mut shifted_window = candidate.clone();
    let mut shifted = shifted_window.observations.remove(0).into_input();
    shifted.event_time_start = UnixNanos::new(8 * INTERNAL_TICK_NS);
    shifted.event_time_end = UnixNanos::new(9 * INTERNAL_TICK_NS);
    shifted.watermark = Some(UnixNanos::new(9 * INTERNAL_TICK_NS));
    shifted_window
        .observations
        .push(FeatureObservation::try_new(shifted).expect("valid shifted event window"));
    assert_eq!(
        FastStateSnapshot::try_correction(&registry, &first, shifted_window),
        Err(FastStateError::SnapshotCorrectionMismatch),
        "a correction must not replace a different economic event-time slice"
    );

    let mut finality_regression = candidate;
    let mut regressed = finality_regression.observations.remove(0).into_input();
    regressed.finality_as_known_at = UnixNanos::new(PUBLIC_TICK_NS + 50);
    finality_regression
        .observations
        .push(FeatureObservation::try_new(regressed).expect("locally valid finality regression"));
    assert_eq!(
        FastStateSnapshot::try_correction(&registry, &first, finality_regression),
        Err(FastStateError::SnapshotCorrectionMismatch)
    );
}

#[test]
fn snapshot_evidence_binds_the_consumed_publication_sequence() {
    let (registry, input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x20,
    );
    let canonical = build_snapshot(&registry, input.clone()).expect("canonical publication");

    let mut late_scheduler =
        FastStateScheduler::try_new(9 * INTERNAL_TICK_NS, 16).expect("late scheduler");
    late_scheduler
        .advance_to(PUBLIC_TICK_NS)
        .expect("late publication");
    let late_tick = late_scheduler
        .drain_ticks(16)
        .into_iter()
        .find(|tick| tick.kind() == TickKind::Published1S)
        .expect("late scheduler publication");
    let late = FastStateSnapshot::try_for_published_tick(&registry, late_tick, input)
        .expect("sequence-bound snapshot");

    assert_eq!(canonical.publication_sequence(), 11);
    assert_eq!(late.publication_sequence(), 2);
    assert_ne!(canonical.evidence_hash(), late.evidence_hash());
}

#[test]
fn ui_slot_rejects_global_knowledge_time_regression_across_event_times() {
    let (first_registry, first_input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        2 * PUBLIC_TICK_NS,
        0x20,
    );
    let first =
        Arc::new(build_snapshot(&first_registry, first_input).expect("later-known first event"));
    let second = Arc::new(snapshot_with(
        2 * PUBLIC_TICK_NS,
        vec![present(2.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
    ));
    let mut slot = CoalescingSnapshotSlot::new();

    slot.publish(first).expect("first event");
    assert_eq!(
        slot.publish(second),
        Err(FastStateError::SnapshotRegression)
    );
}

#[test]
fn ui_slot_orders_new_events_by_time_across_scheduler_restarts() {
    let first = Arc::new(snapshot_with(
        PUBLIC_TICK_NS,
        vec![present(1.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
    ));
    let (registry, input) = snapshot_input(
        2 * PUBLIC_TICK_NS,
        vec![present(2.0)],
        full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy),
        950_000,
        100,
        0x30,
    );
    let mut restarted =
        FastStateScheduler::try_new(19 * INTERNAL_TICK_NS, 16).expect("restarted scheduler");
    restarted
        .advance_to(2 * PUBLIC_TICK_NS)
        .expect("next publication");
    let tick = restarted
        .drain_ticks(16)
        .into_iter()
        .find(|tick| tick.kind() == TickKind::Published1S)
        .expect("restarted publication");
    let second = Arc::new(
        FastStateSnapshot::try_for_published_tick(&registry, tick, input)
            .expect("restart-bound snapshot"),
    );
    assert!(second.publication_sequence() < first.publication_sequence());

    let mut slot = CoalescingSnapshotSlot::new();
    slot.publish(first).expect("first event");
    assert_eq!(slot.publish(second), Ok(2));
}

fn snapshot_hash(
    data: Vec<FeatureDatum>,
    coverage: SourceCoverage,
    quality: u32,
    known_offset: i64,
    lineage_seed: u8,
) -> [u8; 32] {
    let (registry, input) = snapshot_input(
        PUBLIC_TICK_NS,
        data,
        coverage,
        quality,
        known_offset,
        lineage_seed,
    );
    build_snapshot(&registry, input)
        .expect("snapshot variant")
        .evidence_hash()
}

fn snapshot_with(
    as_of_event_time_ns: i64,
    data: Vec<FeatureDatum>,
    coverage: SourceCoverage,
    quality: u32,
) -> FastStateSnapshot {
    let (registry, input) = snapshot_input(as_of_event_time_ns, data, coverage, quality, 100, 0x20);
    build_snapshot(&registry, input).expect("valid snapshot")
}

fn snapshot_for_entity(asset: AssetId) -> FastStateSnapshot {
    let coverage = full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy);
    let mut registry = FeatureRegistry::new();
    registry.register(definition(0)).expect("definition");
    let observation = observation(
        0,
        FeatureEntity::Asset(asset),
        present(1.0),
        coverage,
        950_000,
        ObservationEvidence {
            event_time_end_ns: PUBLIC_TICK_NS,
            known_offset_ns: 100,
            lineage_seed: 0x20,
        },
    );
    build_snapshot(
        &registry,
        FastStateSnapshotInput {
            as_of_event_time_ns: PUBLIC_TICK_NS,
            observations: vec![observation],
        },
    )
    .expect("other entity snapshot")
}

fn snapshot_correction(
    previous: &FastStateSnapshot,
    value: f64,
    known_offset: i64,
    revision: u32,
) -> Result<Arc<FastStateSnapshot>, FastStateError> {
    let coverage = full_coverage(SourceHealthState::Healthy, SourceHealthState::Healthy);
    let (registry, mut input) = snapshot_input(
        PUBLIC_TICK_NS,
        vec![present(value)],
        coverage,
        950_000,
        known_offset,
        0x30,
    );
    let mut observation = input.observations.remove(0).into_input();
    observation.finality_state = if revision == 1 {
        FinalityState::Provisional
    } else {
        FinalityState::Corrected
    };
    observation.revision = ObservationRevision::new(revision).expect("revision");
    input.observations =
        vec![FeatureObservation::try_new(observation).expect("revised observation")];
    FastStateSnapshot::try_correction(&registry, previous, input).map(Arc::new)
}

fn build_snapshot(
    registry: &FeatureRegistry,
    input: FastStateSnapshotInput,
) -> Result<FastStateSnapshot, FastStateError> {
    let tick = published_tick(input.as_of_event_time_ns);
    FastStateSnapshot::try_for_published_tick(registry, tick, input)
}

fn published_tick(at_ns: i64) -> fast_state::TickEvent {
    assert!(at_ns > 0 && at_ns % PUBLIC_TICK_NS == 0);
    let mut scheduler = FastStateScheduler::try_new(0, 65_536).expect("test scheduler");
    scheduler.advance_to(at_ns).expect("test publication time");
    scheduler
        .drain_ticks(65_536)
        .into_iter()
        .rev()
        .find(|tick| tick.kind() == TickKind::Published1S)
        .expect("published tick")
}

fn snapshot_input(
    as_of_event_time_ns: i64,
    data: Vec<FeatureDatum>,
    coverage: SourceCoverage,
    quality: u32,
    known_offset: i64,
    lineage_seed: u8,
) -> (FeatureRegistry, FastStateSnapshotInput) {
    let entity = FeatureEntity::Asset(
        AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("asset"),
    );
    let mut registry = FeatureRegistry::new();
    let mut observations = Vec::with_capacity(data.len());
    for (index, datum) in data.into_iter().enumerate() {
        registry.register(definition(index)).expect("definition");
        observations.push(observation(
            index,
            entity.clone(),
            datum,
            coverage.clone(),
            quality,
            ObservationEvidence {
                event_time_end_ns: as_of_event_time_ns,
                known_offset_ns: known_offset,
                lineage_seed,
            },
        ));
    }
    (
        registry,
        FastStateSnapshotInput {
            as_of_event_time_ns,
            observations,
        },
    )
}

fn definition(index: usize) -> FeatureDefinition {
    definition_with_value_type(index, FeatureValueType::Float64)
}

fn definition_with_value_type(index: usize, value_type: FeatureValueType) -> FeatureDefinition {
    let id = feature_id(index);
    FeatureDefinition::try_new(FeatureDefinitionInput {
        id: FeatureId::new(&id).expect("feature id"),
        version: version(),
        status: FeatureStatus::Required,
        consumption_role: FeatureConsumptionRole::ModelEligible,
        value_type,
        entities: EntityScope::Asset,
        required_inputs: vec![InputRequirement::new("normalized.trades").expect("input")],
        event_time_policy: EventTimePolicy::SourceEventTime,
        windows: vec![
            WindowDefinition::try_new_time(
                window_id(),
                WindowKind::Sliding,
                DurationNanos::new(INTERNAL_TICK_NS as u64),
                Some(DurationNanos::new(INTERNAL_TICK_NS as u64)),
            )
            .expect("window"),
        ],
        output_resolution: DurationNanos::new(INTERNAL_TICK_NS as u64),
        allowed_lateness: DurationNanos::new(0),
        time_to_live: DurationNanos::new(60 * PUBLIC_TICK_NS as u64),
        normalization: NormalizationPolicy::try_new(NormalizationKind::None, version())
            .expect("normalization"),
        missingness: MissingnessPolicy::Explicit,
        quality_gate: QualityRequirement::try_new(
            QualityScore::from_millionths(0).expect("minimum quality"),
            QualityScore::from_millionths(0).expect("minimum coverage"),
            SourceHealthState::ALL.to_vec(),
        )
        .expect("quality gate"),
        formula_hash: FormulaHash::new([index as u8 + 1; 32]).expect("formula hash"),
        documentation: FeatureDocumentation::new(format!(
            "docs/data-dictionary/features.md#{}",
            id.replace('_', "-")
        ))
        .expect("documentation"),
    })
    .expect("definition")
}

fn observation(
    index: usize,
    entity: FeatureEntity,
    datum: FeatureDatum,
    coverage: SourceCoverage,
    quality: u32,
    evidence: ObservationEvidence,
) -> FeatureObservation {
    let known = evidence.event_time_end_ns + evidence.known_offset_ns;
    let value_type = datum
        .value()
        .map(FeatureValue::value_type)
        .unwrap_or(FeatureValueType::Float64);
    FeatureObservation::try_new(FeatureObservationInput {
        feature_id: FeatureId::new(feature_id(index)).expect("feature id"),
        feature_version: version(),
        entity,
        window_id: window_id(),
        resolution: DurationNanos::new(INTERNAL_TICK_NS as u64),
        datum,
        value_type,
        event_time_start: UnixNanos::new(evidence.event_time_end_ns - INTERNAL_TICK_NS),
        event_time_end: UnixNanos::new(evidence.event_time_end_ns),
        as_known_at: UnixNanos::new(known),
        computed_at: UnixNanos::new(known + 1),
        watermark: Some(UnixNanos::new(evidence.event_time_end_ns)),
        finality_as_known_at: UnixNanos::new(known),
        finality_state: if evidence.known_offset_ns > 100 {
            FinalityState::Corrected
        } else {
            FinalityState::Provisional
        },
        revision: ObservationRevision::new(if evidence.known_offset_ns > 100 { 2 } else { 1 })
            .expect("revision"),
        source_coverage: coverage,
        quality_score: QualityScore::from_millionths(quality).expect("quality"),
        normalization_version: version(),
        formula_hash: FormulaHash::new([index as u8 + 1; 32]).expect("formula hash"),
        code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567")
            .expect("code revision"),
        lineage_hash: LineageHash::new([evidence.lineage_seed.wrapping_add(index as u8); 32])
            .expect("lineage"),
    })
    .expect("observation")
}

#[derive(Clone, Copy)]
struct ObservationEvidence {
    event_time_end_ns: i64,
    known_offset_ns: i64,
    lineage_seed: u8,
}

fn present(value: f64) -> FeatureDatum {
    FeatureDatum::Present(FeatureValue::Float64(
        FiniteF64::new(value).expect("finite value"),
    ))
}

fn feature_id(index: usize) -> String {
    format!("fast_feature_{index}")
}

fn window_id() -> WindowId {
    WindowId::new("fast_100ms").expect("window id")
}

fn version() -> Version {
    Version::parse("1.0.0").expect("version")
}

fn full_coverage(first: SourceHealthState, second: SourceHealthState) -> SourceCoverage {
    SourceCoverage::try_new(vec![
        SourceCoverageEntry::new(source("binance", 1), first),
        SourceCoverageEntry::new(source("kraken", 1), second),
    ])
    .expect("full source coverage")
}

fn partial_coverage() -> SourceCoverage {
    SourceCoverage::try_new_partial(
        vec![source("binance", 1), source("kraken", 1)],
        vec![SourceCoverageEntry::new(
            source("binance", 1),
            SourceHealthState::Healthy,
        )],
    )
    .expect("partial source coverage")
}

fn nine_of_ten_coverage() -> SourceCoverage {
    let expected = (0..10)
        .map(|index| source(&format!("venue{index}"), 1))
        .collect::<Vec<_>>();
    let observed = expected
        .iter()
        .take(9)
        .cloned()
        .map(|source| SourceCoverageEntry::new(source, SourceHealthState::Healthy))
        .collect();
    SourceCoverage::try_new_partial(expected, observed).expect("90 percent source coverage")
}

fn source(name: &str, generation: u32) -> SourceId {
    SourceId::new(SourceKind::Exchange, name, generation).expect("source identity")
}
