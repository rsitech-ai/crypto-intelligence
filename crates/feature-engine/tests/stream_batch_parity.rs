use std::{fs, num::NonZeroU64};

use domain::{SourceId, SourceKind, UnixNanos};
use feature_engine::{
    ClockBasis, CountWindow, EmissionAction, EventCountState, Finalization, HalfLifeEwmaState,
    LogicalClock, MaterializationError, MaterializationMode, Materializer, PartitionConfig,
    PartitionId, PointInTimeFeatureSet, PointInTimeTrainingPlan, RecordedClock, RecordedTimestamp,
    ThresholdKind, ThresholdWindow, ThresholdWindowState, TimeWindow, TimeWindowSpec, TimerId,
    WatermarkKey, WatermarkTracker, WatermarkUpdate, WindowLifecycle,
};
use feature_registry::{DurationNanos, WindowDefinition, WindowId, WindowKind};
use fixed_decimal::FixedDecimal;
use quality::SourceHealthState;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RecordedInput {
    event_time: i64,
    receive_time: i64,
    watermark: i64,
    value: f64,
    amount: String,
    correction: bool,
}

#[derive(Debug, PartialEq)]
struct EngineOutput {
    memberships: Vec<Vec<Vec<TimeWindow>>>,
    finality: Vec<Finalization>,
    emissions: Vec<Option<EmissionAction>>,
    timer_batches: Vec<Vec<TimerId>>,
    ewma_bits: Vec<u64>,
    count_windows: Vec<Option<CountWindow>>,
    threshold_windows: Vec<Option<ThresholdWindow>>,
    volume_windows: Vec<Option<ThresholdWindow>>,
}

fn window_id(value: &str) -> WindowId {
    WindowId::new(value).expect("test window ID should be valid")
}

fn recorded_sequence() -> Vec<RecordedInput> {
    vec![
        RecordedInput {
            event_time: 5,
            receive_time: 6,
            watermark: 5,
            value: 10.0,
            amount: "40".into(),
            correction: false,
        },
        RecordedInput {
            event_time: 15,
            receive_time: 16,
            watermark: 15,
            value: 14.0,
            amount: "60".into(),
            correction: false,
        },
        RecordedInput {
            event_time: 9,
            receive_time: 17,
            watermark: 15,
            value: 12.0,
            amount: "10".into(),
            correction: true,
        },
        RecordedInput {
            event_time: 25,
            receive_time: 26,
            watermark: 25,
            value: 18.0,
            amount: "90".into(),
            correction: false,
        },
    ]
}

fn run_engine(inputs: impl IntoIterator<Item = RecordedInput>) -> EngineOutput {
    let time_definition = WindowDefinition::try_new_time(
        window_id("ten-nanos"),
        WindowKind::Tumbling,
        DurationNanos::new(10),
        None,
    )
    .expect("time definition should be valid");
    let sliding_definition = WindowDefinition::try_new_time(
        window_id("sliding-ten"),
        WindowKind::Sliding,
        DurationNanos::new(10),
        Some(DurationNanos::new(5)),
    )
    .expect("sliding definition should be valid");
    let session_definition = WindowDefinition::try_new_session_aligned(
        window_id("session-ten"),
        DurationNanos::new(10),
        UnixNanos::new(10),
    )
    .expect("session definition should be valid");
    let time_specs = [
        TimeWindowSpec::try_from_definition(&time_definition)
            .expect("time runtime should match its registry definition"),
        TimeWindowSpec::try_from_definition(&sliding_definition)
            .expect("sliding runtime should match its registry definition"),
        TimeWindowSpec::try_from_definition(&session_definition)
            .expect("session runtime should match its registry definition"),
    ];
    let ewma_definition = WindowDefinition::try_new_exponentially_weighted(
        window_id("half-life-ten"),
        DurationNanos::new(10),
    )
    .expect("EWMA definition should be valid");
    let count_definition = WindowDefinition::try_new_event_count(
        window_id("count-two"),
        NonZeroU64::new(2).expect("count should be nonzero"),
        None,
    )
    .expect("count definition should be valid");
    let threshold_definition = WindowDefinition::try_new_threshold(
        window_id("notional-hundred"),
        WindowKind::Notional,
        FixedDecimal::parse_canonical("100").expect("threshold should be valid"),
    )
    .expect("threshold definition should be valid");
    let volume_definition = WindowDefinition::try_new_threshold(
        window_id("volume-fifty"),
        WindowKind::Volume,
        FixedDecimal::parse_canonical("50").expect("threshold should be valid"),
    )
    .expect("volume definition should be valid");

    let source =
        SourceId::new(SourceKind::Exchange, "binance", 1).expect("test source should be valid");
    let key = WatermarkKey::new(
        source,
        PartitionId::new("trades").expect("partition should be valid"),
    );
    let mut tracker = WatermarkTracker::try_new(
        vec![PartitionConfig::required(key.clone())],
        DurationNanos::new(0),
        vec![SourceHealthState::Healthy],
    )
    .expect("tracker should be valid");
    let tracked_window = TimeWindow::try_new(UnixNanos::new(0), UnixNanos::new(10))
        .expect("tracked window should be valid");
    let mut lifecycle =
        WindowLifecycle::try_new(&tracker, 4, 8).expect("lifecycle should be valid");
    let mut clock = RecordedClock::try_new(
        LogicalClock::try_new(UnixNanos::new(1), 2).expect("clock should be valid"),
        ClockBasis::EventTime,
    );
    clock
        .schedule(
            TimerId::new(1).expect("timer ID should be valid"),
            UnixNanos::new(15),
        )
        .expect("timer should schedule");
    clock
        .schedule(
            TimerId::new(2).expect("timer ID should be valid"),
            UnixNanos::new(25),
        )
        .expect("timer should schedule");
    let mut count = EventCountState::try_from_definition(&count_definition)
        .expect("count state should be valid");
    let mut threshold = ThresholdWindowState::try_from_definition(&threshold_definition)
        .expect("threshold state should be valid");
    let mut volume = ThresholdWindowState::try_from_definition(&volume_definition)
        .expect("volume state should be valid");
    let mut ewma = HalfLifeEwmaState::try_from_definition(&ewma_definition, 16, &tracker)
        .expect("EWMA state should be valid");

    let mut memberships = Vec::new();
    let mut finality = Vec::new();
    let mut emissions = Vec::new();
    let mut timer_batches = Vec::new();
    let mut ewma_bits = Vec::new();
    let mut count_windows = Vec::new();
    let mut threshold_windows = Vec::new();
    let mut volume_windows = Vec::new();
    for input in inputs {
        let event_time = UnixNanos::new(input.event_time);
        memberships.push(
            time_specs
                .iter()
                .map(|spec| {
                    spec.windows_for(event_time)
                        .expect("window membership should resolve")
                })
                .collect(),
        );
        timer_batches.push(
            clock
                .advance(
                    RecordedTimestamp::try_new(event_time, UnixNanos::new(input.receive_time))
                        .expect("recorded timestamp should be valid"),
                )
                .expect("recorded clock should advance monotonically"),
        );
        tracker
            .advance(
                &key,
                WatermarkUpdate::new(
                    UnixNanos::new(input.watermark),
                    UnixNanos::new(input.watermark + 1),
                    SourceHealthState::Healthy,
                ),
            )
            .expect("watermark should advance monotonically");
        let decision = tracker.decision(tracked_window);
        finality.push(decision.state());
        let action = if input.correction {
            let correction = tracker
                .correction(&key, tracked_window, event_time)
                .expect("late input should produce exact-window evidence");
            Some(
                lifecycle
                    .correct(correction)
                    .expect("late input should append a correction"),
            )
        } else if lifecycle.history(tracked_window).is_none()
            || lifecycle
                .history(tracked_window)
                .and_then(|history| history.last())
                .is_some_and(|emission| {
                    emission.finality() == feature_registry::FinalityState::Provisional
                })
        {
            Some(
                lifecycle
                    .apply(decision)
                    .expect("watermark decision should apply"),
            )
        } else {
            None
        };
        emissions.push(action);

        ewma_bits.push(
            ewma.update_at(event_time, input.value)
                .expect("bounded EWMA replay should be valid")
                .to_bits(),
        );

        count_windows.push(count.observe().expect("count should advance"));
        threshold_windows.push(
            threshold
                .observe(
                    FixedDecimal::parse_canonical(&input.amount)
                        .expect("amount should be canonical"),
                )
                .expect("amount should apply"),
        );
        volume_windows.push(
            volume
                .observe(
                    FixedDecimal::parse_canonical(&input.amount)
                        .expect("amount should be canonical"),
                )
                .expect("volume should apply"),
        );
    }

    EngineOutput {
        memberships,
        finality,
        emissions,
        timer_batches,
        ewma_bits,
        count_windows,
        threshold_windows,
        volume_windows,
    }
}

#[derive(Serialize)]
struct CanonicalOutput {
    memberships: Vec<Vec<Vec<(i64, i64)>>>,
    finality: Vec<u8>,
    emissions: Vec<Option<(u8, u32, u32)>>,
    timer_batches: Vec<Vec<u64>>,
    ewma_bits: Vec<u64>,
    count_windows: Vec<Option<(u64, u64, u64)>>,
    threshold_windows: Vec<Option<(u8, u64, String)>>,
    volume_windows: Vec<Option<(u8, u64, String)>>,
}

fn digest(output: &EngineOutput) -> blake3::Hash {
    let canonical = CanonicalOutput {
        memberships: output
            .memberships
            .iter()
            .map(|family| {
                family
                    .iter()
                    .map(|windows| {
                        windows
                            .iter()
                            .map(|window| (window.start().value(), window.end().value()))
                            .collect()
                    })
                    .collect()
            })
            .collect(),
        finality: output
            .finality
            .iter()
            .map(|state| match state {
                Finalization::Provisional => 1,
                Finalization::Final => 2,
                Finalization::Invalid => 3,
            })
            .collect(),
        emissions: output
            .emissions
            .iter()
            .map(|action| {
                action.map(|action| match action {
                    EmissionAction::Provisional { revision } => (1, 0, revision),
                    EmissionAction::RevisedProvisional {
                        replaces_revision,
                        revision,
                    } => (2, replaces_revision, revision),
                    EmissionAction::Final { revision } => (3, 0, revision),
                    EmissionAction::Corrected {
                        replaces_revision,
                        revision,
                    } => (4, replaces_revision, revision),
                    EmissionAction::Invalid { revision } => (5, 0, revision),
                })
            })
            .collect(),
        timer_batches: output
            .timer_batches
            .iter()
            .map(|batch| batch.iter().map(|timer| timer.value()).collect())
            .collect(),
        ewma_bits: output.ewma_bits.clone(),
        count_windows: output
            .count_windows
            .iter()
            .map(|window| window.map(|window| (window.index, window.start_event, window.end_event)))
            .collect(),
        threshold_windows: canonical_thresholds(&output.threshold_windows),
        volume_windows: canonical_thresholds(&output.volume_windows),
    };
    let bytes = serde_json::to_vec(&canonical).expect("canonical output should encode");
    blake3::hash(&bytes)
}

fn canonical_thresholds(windows: &[Option<ThresholdWindow>]) -> Vec<Option<(u8, u64, String)>> {
    windows
        .iter()
        .map(|window| {
            window.map(|window| {
                (
                    match window.kind {
                        ThresholdKind::Volume => 1,
                        ThresholdKind::Notional => 2,
                    },
                    window.index,
                    window.total.to_string(),
                )
            })
        })
        .collect()
}

#[test]
fn live_recorded_replay_and_batch_drivers_produce_identical_task_two_outputs() {
    let recorded = recorded_sequence();
    let live = run_engine(recorded.clone());

    let replay_bytes = serde_json::to_vec(&recorded).expect("recorded fixture should encode");
    let replay_inputs: Vec<RecordedInput> =
        serde_json::from_slice(&replay_bytes).expect("recorded fixture should decode");
    let replay = run_engine(replay_inputs);

    let batch_inputs = recorded
        .chunks(2)
        .flat_map(|batch| batch.iter().cloned())
        .collect::<Vec<_>>();
    let batch = run_engine(batch_inputs);

    assert_eq!(live, replay);
    assert_eq!(live, batch);
    assert_eq!(digest(&live), digest(&replay));
    assert_eq!(digest(&live), digest(&batch));
    assert_eq!(
        live.timer_batches,
        vec![
            vec![],
            vec![TimerId::new(1).expect("timer ID should be valid")],
            vec![],
            vec![TimerId::new(2).expect("timer ID should be valid")],
        ]
    );
    assert!(matches!(
        live.emissions[2],
        Some(EmissionAction::Corrected { .. })
    ));
}

fn private_materialization_root(label: &str) -> tempfile::TempDir {
    let root = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("materialization root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
            .expect("private materialization root");
    }
    root
}

#[test]
fn live_wal_and_real_parquet_paths_produce_identical_feature_rows() {
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/golden-replays/features-v1/manifest.toml"
    );
    let live_root = private_materialization_root("cmti-features-live-");
    let wal_root = private_materialization_root("cmti-features-wal-");
    let parquet_root = private_materialization_root("cmti-features-parquet-");

    let live = Materializer::run_fixture(
        manifest,
        MaterializationMode::LiveSimulation,
        live_root.path(),
    )
    .expect("live fixture materialization");
    let wal = Materializer::run_fixture(manifest, MaterializationMode::WalReplay, wal_root.path())
        .expect("verified WAL fixture materialization");
    let parquet = Materializer::run_fixture(
        manifest,
        MaterializationMode::ParquetReplay,
        parquet_root.path(),
    )
    .expect("verified Parquet fixture materialization");

    assert_eq!(live.rows(), wal.rows());
    assert_eq!(wal.rows(), parquet.rows());
    assert_eq!(live.fixed_point_digest(), wal.fixed_point_digest());
    assert_eq!(wal.fixed_point_digest(), parquet.fixed_point_digest());
    assert_eq!(live.lineage_digest(), parquet.lineage_digest());
    assert_eq!(
        live.finality_quality_digest(),
        parquet.finality_quality_digest()
    );
    assert!(
        live.float_max_abs_error(&parquet)
            .expect("same float shape")
            <= 1e-12
    );

    assert_eq!(live.rows().len(), 3);
    assert_eq!(live.rows()[0].feature_id(), "consolidated_price");
    assert_eq!(live.rows()[0].datum_json(), r#""101.25""#);
    assert_eq!(live.rows()[1].feature_id(), "realized_volatility");
    assert_eq!(live.rows()[1].float_value(), Some(0.125));
    assert_eq!(live.rows()[2].quality_millionths(), 800_000);

    assert!(live.wal_segment_blake3().is_none());
    assert!(live.sealed_manifest_blake3().is_none());
    assert!(
        wal.wal_segment_blake3()
            .is_some_and(|digest| digest != [0; 32])
    );
    assert!(wal.sealed_manifest_blake3().is_none());
    assert!(parquet.wal_segment_blake3().is_none());
    assert!(
        parquet
            .sealed_manifest_blake3()
            .is_some_and(|digest| digest != [0; 32])
    );
}

#[test]
fn corrected_dataset_versions_remain_immutable_and_queries_are_point_in_time() {
    let base_manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/golden-replays/features-v1/manifest.toml"
    );
    let correction_manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/golden-replays/features-v1/correction-v2.toml"
    );
    let root = private_materialization_root("cmti-features-correction-");

    let base = Materializer::run_fixture(
        base_manifest,
        MaterializationMode::ParquetReplay,
        root.path(),
    )
    .expect("sealed base feature dataset");
    let base_hash = base
        .sealed_manifest_blake3()
        .expect("base manifest hash must be durable");
    let correction =
        Materializer::run_correction_fixture(correction_manifest, root.path(), base_hash)
            .expect("sealed correction dataset");
    let correction_hash = correction
        .sealed_manifest_blake3()
        .expect("correction manifest hash must be durable");

    assert_ne!(base_hash, correction_hash);
    assert_eq!(
        Materializer::verify_fixture_dataset(base_manifest, root.path())
            .expect("base version remains independently verifiable")
            .sealed_manifest_blake3(),
        Some(base_hash)
    );
    assert_eq!(
        Materializer::verify_fixture_dataset(correction_manifest, root.path())
            .expect("correction version and its chain verify")
            .sealed_manifest_blake3(),
        Some(correction_hash)
    );

    let history =
        PointInTimeFeatureSet::try_new(vec![base.rows()[0].clone(), correction.rows()[0].clone()])
            .expect("consecutive immutable correction history");
    assert!(
        history
            .as_known_at(base.rows()[0].as_known_at_ns() - 1)
            .is_empty()
    );
    let before_correction = history.as_known_at(base.rows()[0].computed_at_ns());
    assert_eq!(before_correction.len(), 1);
    assert_eq!(before_correction[0].datum_json(), r#""101.25""#);
    assert_eq!(before_correction[0].revision(), 1);

    let after_correction = history.as_known_at(correction.rows()[0].computed_at_ns());
    assert_eq!(after_correction.len(), 1);
    assert_eq!(after_correction[0].datum_json(), r#""101.5""#);
    assert_eq!(after_correction[0].revision(), 2);
}

#[test]
fn missing_float_features_round_trip_without_inventing_zero_or_float_bits() {
    let manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/golden-replays/features-v1/manifest.toml"
    );
    let root = private_materialization_root("cmti-features-missing-");
    let source = fs::read_to_string(manifest).expect("base manifest");
    let missing = source.replace(
        "value_type = \"float64\"\nvalue = \"0.125\"",
        "value_type = \"float64\"\nmissingness_reason = \"insufficient_history\"",
    );
    assert_ne!(source, missing, "fixture mutation must apply");
    let missing_manifest = root.path().join("missing.toml");
    fs::write(&missing_manifest, missing).expect("write missingness fixture");

    let report = Materializer::run_fixture(
        &missing_manifest,
        MaterializationMode::ParquetReplay,
        root.path(),
    )
    .expect("explicit missing feature must round trip");
    let volatility = report
        .rows()
        .iter()
        .find(|row| row.feature_id() == "realized_volatility")
        .expect("volatility row");
    assert_eq!(
        volatility.datum_json(),
        r#"{"missing":"insufficient_history"}"#
    );
    assert_eq!(volatility.float_value(), None);
}

#[test]
fn correction_publication_rejects_unversioned_changes_to_base_rows() {
    let base_manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/golden-replays/features-v1/manifest.toml"
    );
    let correction_manifest = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/golden-replays/features-v1/correction-v2.toml"
    );
    let root = private_materialization_root("cmti-features-tampered-correction-");
    let base = Materializer::run_fixture(
        base_manifest,
        MaterializationMode::ParquetReplay,
        root.path(),
    )
    .expect("base publication");
    let source = fs::read_to_string(correction_manifest).expect("correction manifest");
    let tampered = source.replace("value = \"0.125\"", "value = \"0.5\"");
    assert_ne!(source, tampered, "fixture mutation must apply");
    let tampered_manifest = root.path().join("tampered-correction.toml");
    fs::write(&tampered_manifest, tampered).expect("write tampered correction");

    assert!(matches!(
        Materializer::run_correction_fixture(
            &tampered_manifest,
            root.path(),
            base.sealed_manifest_blake3().expect("base hash"),
        ),
        Err(MaterializationError::InvalidCorrectionHistory)
    ));
}

#[test]
fn generated_training_plan_carries_all_point_in_time_guards() {
    let plan = PointInTimeTrainingPlan::try_new("predictions", "materialized_features")
        .expect("safe table identifiers");
    let sql = plan.sql();

    assert!(sql.contains("f.as_known_at_ns <= p.prediction_time_ns"));
    assert!(sql.contains("f.computed_at_ns <= p.prediction_time_ns"));
    assert!(sql.contains("f.finality_as_known_at_ns <= p.prediction_time_ns"));
    assert!(sql.contains("f.event_time_end_ns <= p.prediction_time_ns"));
    assert!(sql.contains("ORDER BY f.revision DESC"));
    assert!(PointInTimeTrainingPlan::try_new("predictions; DROP TABLE x", "features").is_err());
}
