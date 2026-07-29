use std::num::NonZeroU64;

use domain::{SourceId, SourceKind, UnixNanos};
use feature_engine::{
    ClockBasis, ClockError, CountWindow, EmissionAction, EventCountState, Finalization,
    HalfLifeEwmaState, LogicalClock, PartitionConfig, PartitionId, RecordedClock,
    RecordedTimestamp, ThresholdKind, ThresholdWindowState, TimeWindow, TimeWindowSpec, TimerId,
    WatermarkError, WatermarkKey, WatermarkTracker, WatermarkUpdate, WindowError, WindowLifecycle,
};
use feature_registry::{DurationNanos, FinalityState, WindowDefinition, WindowId, WindowKind};
use fixed_decimal::FixedDecimal;
use quality::SourceHealthState;

fn source(name: &str) -> SourceId {
    SourceId::new(SourceKind::Exchange, name, 1).expect("test source should be valid")
}

fn key(name: &str, partition: &str) -> WatermarkKey {
    WatermarkKey::new(
        source(name),
        PartitionId::new(partition).expect("test partition should be valid"),
    )
}

fn update(at: i64, health: SourceHealthState) -> WatermarkUpdate {
    WatermarkUpdate::new(UnixNanos::new(at), UnixNanos::new(at), health)
}

fn minute_zero() -> TimeWindow {
    TimeWindow::try_new(UnixNanos::new(0), UnixNanos::new(60_000_000_000))
        .expect("test window should be valid")
}

fn window_id(value: &str) -> WindowId {
    WindowId::new(value).expect("test window ID should be valid")
}

fn time_definition(
    id: &str,
    kind: WindowKind,
    extent: u64,
    advance: Option<u64>,
) -> WindowDefinition {
    WindowDefinition::try_new_time(
        window_id(id),
        kind,
        DurationNanos::new(extent),
        advance.map(DurationNanos::new),
    )
    .expect("test time-window definition should be valid")
}

fn tracker_for(required: WatermarkKey, lateness: u64) -> WatermarkTracker {
    WatermarkTracker::try_new(
        vec![PartitionConfig::required(required)],
        DurationNanos::new(lateness),
        vec![SourceHealthState::Healthy],
    )
    .expect("tracker should be valid")
}

#[test]
fn window_finalizes_only_after_all_required_watermarks_and_lateness() {
    let binance = key("binance", "trades");
    let kraken = key("kraken", "trades");
    let mut tracker = WatermarkTracker::try_new(
        vec![
            PartitionConfig::required(binance.clone()),
            PartitionConfig::required(kraken.clone()),
        ],
        DurationNanos::new(5_000_000_000),
        vec![SourceHealthState::Healthy],
    )
    .expect("tracker should be valid");

    tracker
        .advance(&binance, update(65_000_000_000, SourceHealthState::Healthy))
        .expect("watermark should advance");
    assert_eq!(
        tracker.decision(minute_zero()).state(),
        Finalization::Provisional
    );
    tracker
        .advance(&kraken, update(65_000_000_000, SourceHealthState::Healthy))
        .expect("watermark should advance");
    assert_eq!(tracker.decision(minute_zero()).state(), Finalization::Final);
}

#[test]
fn watermark_regression_and_unknown_partition_fail_without_mutation() {
    let binance = key("binance", "trades");
    let mut tracker = WatermarkTracker::try_new(
        vec![PartitionConfig::required(binance.clone())],
        DurationNanos::new(0),
        vec![SourceHealthState::Healthy],
    )
    .expect("tracker should be valid");
    tracker
        .advance(&binance, update(10, SourceHealthState::Healthy))
        .expect("first watermark should advance");
    tracker
        .advance(
            &binance,
            WatermarkUpdate::new(
                UnixNanos::new(10),
                UnixNanos::new(20),
                SourceHealthState::Healthy,
            ),
        )
        .expect("availability may advance without moving event time");
    let decision_before = tracker.decision(minute_zero());
    assert_eq!(
        tracker.advance(
            &binance,
            WatermarkUpdate::new(
                UnixNanos::new(10),
                UnixNanos::new(19),
                SourceHealthState::Healthy,
            ),
        ),
        Err(WatermarkError::InvalidWatermark)
    );
    assert_eq!(tracker.decision(minute_zero()), decision_before);

    assert!(matches!(
        tracker.advance(&binance, update(9, SourceHealthState::Healthy)),
        Err(WatermarkError::Regression { .. })
    ));
    assert_eq!(tracker.watermark(&binance), Some(UnixNanos::new(10)));
    assert!(matches!(
        tracker.advance(
            &key("kraken", "trades"),
            update(11, SourceHealthState::Healthy)
        ),
        Err(WatermarkError::UnknownPartition)
    ));
}

#[test]
fn optional_partition_never_blocks_and_required_bad_health_invalidates() {
    let required = key("binance", "trades");
    let optional = key("kraken", "trades");
    let mut tracker = WatermarkTracker::try_new(
        vec![
            PartitionConfig::required(required.clone()),
            PartitionConfig::optional(optional),
        ],
        DurationNanos::new(0),
        vec![SourceHealthState::Healthy],
    )
    .expect("tracker should be valid");
    tracker
        .advance(
            &required,
            update(60_000_000_000, SourceHealthState::Healthy),
        )
        .expect("watermark should advance");
    assert_eq!(tracker.decision(minute_zero()).state(), Finalization::Final);

    tracker
        .advance(
            &required,
            update(61_000_000_000, SourceHealthState::Quarantined),
        )
        .expect("quality transition should be recorded");
    assert_eq!(
        tracker.decision(minute_zero()).state(),
        Finalization::Invalid
    );
}

#[test]
fn utc_time_windows_are_exact_and_dst_independent() {
    let hour = DurationNanos::new(3_600_000_000_000);
    let event = UnixNanos::new(1_711_845_000_000_000_000);
    let utc_anchor = UnixNanos::new(1_704_067_200_000_000_000);
    let definition =
        WindowDefinition::try_new_session_aligned(window_id("utc-hour"), hour, utc_anchor)
            .expect("session definition should be valid");
    let spec =
        TimeWindowSpec::try_from_definition(&definition).expect("session window should be valid");

    let windows = spec.windows_for(event).expect("membership should resolve");
    assert_eq!(windows.len(), 1);
    assert_eq!(
        windows[0].end().value() - windows[0].start().value(),
        hour.value() as i64
    );
    assert_eq!(
        windows,
        spec.windows_for(event)
            .expect("timezone-free membership should repeat")
    );
}

#[test]
fn sliding_membership_is_bounded_and_canonical() {
    let definition = time_definition("sliding-ten", WindowKind::Sliding, 10, Some(5));
    let spec =
        TimeWindowSpec::try_from_definition(&definition).expect("sliding window should be valid");

    assert_eq!(
        spec.windows_for(UnixNanos::new(10))
            .expect("membership should resolve"),
        vec![
            TimeWindow::try_new(UnixNanos::new(5), UnixNanos::new(15))
                .expect("window should be valid"),
            TimeWindow::try_new(UnixNanos::new(10), UnixNanos::new(20))
                .expect("window should be valid"),
        ]
    );
    assert!(
        TimeWindowSpec::try_from_definition(&time_definition(
            "fanout-overflow",
            WindowKind::Sliding,
            1_025,
            Some(1),
        ))
        .is_err()
    );
}

#[test]
fn ewma_count_and_notional_windows_are_deterministic() {
    let ewma_key = key("binance", "trades");
    let ewma_tracker = tracker_for(ewma_key.clone(), 2);
    let ewma_definition = WindowDefinition::try_new_exponentially_weighted(
        window_id("ewma-one"),
        DurationNanos::new(10),
    )
    .expect("EWMA definition should be valid");
    let mut left = HalfLifeEwmaState::try_from_definition(&ewma_definition, 16, &ewma_tracker)
        .expect("EWMA should be valid");
    let mut right = left.clone();
    let inputs = [(1, 10.0), (11, 14.0), (21, 18.0)];
    for (at, value) in inputs {
        assert_eq!(
            left.update_at(UnixNanos::new(at), value)
                .expect("EWMA should update")
                .to_bits(),
            right
                .update_at(UnixNanos::new(at), value)
                .expect("EWMA should update")
                .to_bits()
        );
    }
    assert_eq!(left.value(), Some(15.0));
    let late_left = left
        .update_at(UnixNanos::new(6), 99.0)
        .expect("bounded EWMA should replay a late event");
    let late_right = right
        .update_at(UnixNanos::new(6), 99.0)
        .expect("bounded EWMA replay should be deterministic");
    assert_eq!(late_left.to_bits(), late_right.to_bits());
    assert_ne!(left.value(), Some(15.0));

    let mut bounded_tracker = tracker_for(ewma_key.clone(), 0);
    let mut bounded = HalfLifeEwmaState::try_from_definition(&ewma_definition, 1, &bounded_tracker)
        .expect("bounded EWMA should be valid");
    bounded
        .update_at(UnixNanos::new(1), 10.0)
        .expect("first sample should fit");
    let before_capacity_error = bounded.value();
    assert_eq!(
        bounded.update_at(UnixNanos::new(2), 20.0),
        Err(WindowError::EwmaCapacity)
    );
    assert_eq!(bounded.value(), before_capacity_error);
    bounded_tracker
        .advance(&ewma_key, update(1, SourceHealthState::Healthy))
        .expect("watermark should advance");
    bounded
        .advance_watermark(&bounded_tracker)
        .expect("watermark should compact the immutable prefix");
    assert_eq!(bounded.buffered_samples(), 0);
    assert_eq!(bounded.compacted_through(), Some(UnixNanos::new(1)));
    assert_eq!(
        bounded.update_at(UnixNanos::new(1), 99.0),
        Err(WindowError::EwmaBeforeWatermark)
    );
    bounded
        .update_at(UnixNanos::new(2), 20.0)
        .expect("compaction should free bounded replay capacity");

    let mut long_tracker = tracker_for(ewma_key.clone(), 2);
    let mut long_lived = HalfLifeEwmaState::try_from_definition(&ewma_definition, 4, &long_tracker)
        .expect("long-lived EWMA should be valid");
    for at in 1..=10_000 {
        long_lived
            .update_at(UnixNanos::new(at), at as f64)
            .expect("steady state should not exhaust replay capacity");
        long_tracker
            .advance(&ewma_key, update(at, SourceHealthState::Healthy))
            .expect("source watermark should advance");
        long_lived
            .advance_watermark(&long_tracker)
            .expect("each immutable prefix should compact");
    }
    assert_eq!(long_lived.buffered_samples(), 2);
    assert_eq!(long_lived.compacted_through(), Some(UnixNanos::new(9_998)));

    let mut lateness_tracker = tracker_for(ewma_key.clone(), 5);
    let mut lateness_suffix =
        HalfLifeEwmaState::try_from_definition(&ewma_definition, 8, &lateness_tracker)
            .expect("lateness-aware EWMA should be valid");
    for (at, value) in [(1, 10.0), (4, 12.0), (6, 14.0), (9, 16.0)] {
        lateness_suffix
            .update_at(UnixNanos::new(at), value)
            .expect("sample should apply");
    }
    lateness_tracker
        .advance(&ewma_key, update(10, SourceHealthState::Healthy))
        .expect("raw watermark should advance");
    lateness_suffix
        .advance_watermark(&lateness_tracker)
        .expect("effective frontier should compact");
    assert_eq!(lateness_suffix.compacted_through(), Some(UnixNanos::new(5)));
    assert_eq!(lateness_suffix.buffered_samples(), 2);
    assert_eq!(
        lateness_suffix.update_at(UnixNanos::new(5), 99.0),
        Err(WindowError::EwmaBeforeWatermark)
    );
    lateness_suffix
        .update_at(UnixNanos::new(7), 15.0)
        .expect("late event newer than the effective frontier should replay");
    let suffix_before_regression = (
        lateness_suffix.value(),
        lateness_suffix.buffered_samples(),
        lateness_suffix.compacted_through(),
    );
    assert!(matches!(
        lateness_tracker.advance(&ewma_key, update(9, SourceHealthState::Healthy)),
        Err(WatermarkError::Regression { .. })
    ));
    lateness_suffix
        .advance_watermark(&lateness_tracker)
        .expect("unchanged frontier should be idempotent");
    assert_eq!(
        (
            lateness_suffix.value(),
            lateness_suffix.buffered_samples(),
            lateness_suffix.compacted_through(),
        ),
        suffix_before_regression
    );

    let mut checkpoint_tracker = tracker_for(ewma_key.clone(), 0);
    let mut checkpointed =
        HalfLifeEwmaState::try_from_definition(&ewma_definition, 4, &checkpoint_tracker)
            .expect("checkpointed EWMA should be valid");
    checkpointed
        .update_at(UnixNanos::new(1), 10.0)
        .expect("prefix should apply");
    checkpoint_tracker
        .advance(&ewma_key, update(1, SourceHealthState::Healthy))
        .expect("checkpoint watermark should advance");
    checkpointed
        .advance_watermark(&checkpoint_tracker)
        .expect("prefix should compact");
    for (at, value) in [(11, 14.0), (21, 18.0), (16, 99.0)] {
        checkpointed
            .update_at(UnixNanos::new(at), value)
            .expect("bounded suffix should replay");
    }
    let mut full_replay =
        HalfLifeEwmaState::try_from_definition(&ewma_definition, 4, &checkpoint_tracker)
            .expect("full replay EWMA should be valid");
    for (at, value) in [(1, 10.0), (11, 14.0), (16, 99.0), (21, 18.0)] {
        full_replay
            .update_at(UnixNanos::new(at), value)
            .expect("ordered replay should apply");
    }
    assert_eq!(
        checkpointed
            .value()
            .expect("checkpointed value should exist")
            .to_bits(),
        full_replay
            .value()
            .expect("full replay value should exist")
            .to_bits()
    );
    let foreign_tracker = tracker_for(key("kraken", "trades"), 0);
    assert_eq!(
        checkpointed.advance_watermark(&foreign_tracker),
        Err(WindowError::ForeignWatermarkPolicy)
    );

    let count_definition = WindowDefinition::try_new_event_count(
        window_id("count-three"),
        NonZeroU64::new(3).expect("extent should be nonzero"),
        NonZeroU64::new(2),
    )
    .expect("count definition should be valid");
    let mut count = EventCountState::try_from_definition(&count_definition)
        .expect("count state should be valid");
    assert_eq!(count.observe().expect("count should advance"), None);
    assert_eq!(count.observe().expect("count should advance"), None);
    assert_eq!(
        count.observe().expect("count should advance"),
        Some(CountWindow {
            index: 0,
            start_event: 1,
            end_event: 3,
        })
    );
    assert_eq!(count.observe().expect("count should advance"), None);
    assert_eq!(
        count.observe().expect("count should advance"),
        Some(CountWindow {
            index: 1,
            start_event: 3,
            end_event: 5,
        })
    );

    let notional_definition = WindowDefinition::try_new_threshold(
        window_id("notional-hundred"),
        WindowKind::Notional,
        FixedDecimal::parse_canonical("100").expect("threshold should be valid"),
    )
    .expect("notional definition should be valid");
    let mut notional = ThresholdWindowState::try_from_definition(&notional_definition)
        .expect("threshold state should be valid");
    assert!(
        notional
            .observe(FixedDecimal::parse_canonical("40").expect("amount should be valid"))
            .expect("amount should apply")
            .is_none()
    );
    let closed = notional
        .observe(FixedDecimal::parse_canonical("60").expect("amount should be valid"))
        .expect("amount should apply")
        .expect("window should close");
    assert_eq!(closed.kind, ThresholdKind::Notional);
    assert_eq!(
        closed.total,
        FixedDecimal::parse_canonical("100").expect("total should be valid")
    );
}

#[test]
fn registry_window_families_cannot_be_reinterpreted() {
    let time = time_definition("time-only", WindowKind::Tumbling, 10, None);
    let tracker = tracker_for(key("binance", "trades"), 0);
    assert!(HalfLifeEwmaState::try_from_definition(&time, 16, &tracker).is_err());
    assert!(EventCountState::try_from_definition(&time).is_err());
    assert!(ThresholdWindowState::try_from_definition(&time).is_err());

    let ewma = WindowDefinition::try_new_exponentially_weighted(
        window_id("ewma-only"),
        DurationNanos::new(10),
    )
    .expect("EWMA definition should be valid");
    assert!(TimeWindowSpec::try_from_definition(&ewma).is_err());
}

#[test]
fn late_correction_appends_a_new_revision_without_mutating_history() {
    let window = minute_zero();
    let required = key("binance", "trades");
    let mut tracker = tracker_for(required.clone(), 0);
    let mut lifecycle =
        WindowLifecycle::try_new(&tracker, 2, 4).expect("lifecycle should be valid");
    assert_eq!(
        lifecycle
            .apply(tracker.decision(window))
            .expect("provisional should emit"),
        EmissionAction::Provisional { revision: 1 }
    );
    let mut foreign_tracker = tracker_for(required.clone(), 1);
    foreign_tracker
        .advance(
            &required,
            update(60_000_000_001, SourceHealthState::Healthy),
        )
        .expect("foreign watermark should advance");
    assert_eq!(
        lifecycle.apply(foreign_tracker.decision(window)),
        Err(WindowError::ForeignWatermarkPolicy)
    );
    tracker
        .advance(
            &required,
            update(60_000_000_000, SourceHealthState::Healthy),
        )
        .expect("watermark should advance");
    assert_eq!(
        lifecycle
            .apply(tracker.decision(window))
            .expect("final should emit"),
        EmissionAction::Final { revision: 2 }
    );
    let correction = tracker
        .correction(&required, window, UnixNanos::new(30_000_000_000))
        .expect("correction evidence should be valid");
    assert_eq!(correction.source(), &required);
    assert_eq!(
        correction.corrected_event_time(),
        UnixNanos::new(30_000_000_000)
    );
    assert_eq!(
        lifecycle
            .correct(correction.clone())
            .expect("correction should emit"),
        EmissionAction::Corrected {
            replaces_revision: 2,
            revision: 3,
        }
    );
    assert_eq!(
        lifecycle.correct(correction),
        Err(WindowError::StaleFinalizationDecision)
    );

    let history = lifecycle.history(window).expect("history should exist");
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].finality(), FinalityState::Provisional);
    assert_eq!(history[1].finality(), FinalityState::Final);
    assert_eq!(history[2].finality(), FinalityState::Corrected);
}

#[test]
fn logical_timers_are_ordered_cancelable_bounded_and_replay_equivalent() {
    let mut live = RecordedClock::try_new(
        LogicalClock::try_new(UnixNanos::new(1), 3).expect("clock should be valid"),
        ClockBasis::ReceiveTime,
    );
    let mut replay = live.clone();
    for clock in [&mut live, &mut replay] {
        clock
            .schedule(
                TimerId::new(2).expect("timer ID should be valid"),
                UnixNanos::new(20),
            )
            .expect("timer should schedule");
        clock
            .schedule(
                TimerId::new(1).expect("timer ID should be valid"),
                UnixNanos::new(20),
            )
            .expect("timer should schedule");
        clock
            .schedule(
                TimerId::new(3).expect("timer ID should be valid"),
                UnixNanos::new(30),
            )
            .expect("timer should schedule");
        assert!(clock.cancel(TimerId::new(3).expect("timer ID should be valid")));
    }

    let recorded = RecordedTimestamp::try_new(UnixNanos::new(10), UnixNanos::new(20))
        .expect("recorded time should be valid");
    let live_due = live.advance(recorded).expect("live clock should advance");
    let replay_due = replay
        .advance(recorded)
        .expect("replay clock should advance");
    assert_eq!(live_due, replay_due);
    assert_eq!(
        live_due,
        vec![
            TimerId::new(1).expect("timer ID should be valid"),
            TimerId::new(2).expect("timer ID should be valid"),
        ]
    );
    assert!(live.advance(recorded).is_ok());
    assert!(
        live.advance(
            RecordedTimestamp::try_new(UnixNanos::new(9), UnixNanos::new(19))
                .expect("recorded time should be shaped")
        )
        .is_err()
    );
}

#[test]
fn watermark_configuration_is_bounded_unique_and_fail_closed() {
    let required = key("binance", "trades");
    assert!(PartitionId::new("Trades").is_err());
    assert!(
        WatermarkTracker::try_new(
            vec![PartitionConfig::optional(required.clone())],
            DurationNanos::new(0),
            vec![SourceHealthState::Healthy],
        )
        .is_err()
    );
    assert!(matches!(
        WatermarkTracker::try_new(
            vec![
                PartitionConfig::required(required.clone()),
                PartitionConfig::optional(required.clone()),
            ],
            DurationNanos::new(0),
            vec![SourceHealthState::Healthy],
        ),
        Err(WatermarkError::DuplicatePartition)
    ));
    assert!(
        WatermarkTracker::try_new(
            vec![PartitionConfig::required(required.clone())],
            DurationNanos::new(0),
            vec![SourceHealthState::Healthy, SourceHealthState::Healthy],
        )
        .is_err()
    );
    assert!(
        WatermarkTracker::try_new(
            vec![PartitionConfig::required(required)],
            DurationNanos::new(i64::MAX as u64 + 1),
            vec![SourceHealthState::Healthy],
        )
        .is_err()
    );
    assert!(serde_json::from_str::<PartitionId>("\"Trades\"").is_err());
    let oversized = format!("\"{}\"", "a".repeat(97));
    assert!(serde_json::from_str::<PartitionId>(&oversized).is_err());
    let escaped_oversized = format!("\"{}\"", "\\u0061".repeat(97));
    assert!(serde_json::from_str::<PartitionId>(&escaped_oversized).is_err());
}

#[test]
fn semantically_identical_watermark_policies_have_canonical_identity() {
    let binance = key("binance", "trades");
    let kraken = key("kraken", "trades");
    let first = WatermarkTracker::try_new(
        vec![
            PartitionConfig::required(binance.clone()),
            PartitionConfig::optional(kraken.clone()),
        ],
        DurationNanos::new(5),
        vec![SourceHealthState::Recovering, SourceHealthState::Healthy],
    )
    .expect("first tracker should be valid");
    let second = WatermarkTracker::try_new(
        vec![
            PartitionConfig::optional(kraken),
            PartitionConfig::required(binance),
        ],
        DurationNanos::new(5),
        vec![SourceHealthState::Healthy, SourceHealthState::Recovering],
    )
    .expect("reordered tracker should be valid");

    let mut lifecycle = WindowLifecycle::try_new(&first, 1, 2).expect("lifecycle should be valid");
    assert_eq!(
        lifecycle
            .apply(second.decision(minute_zero()))
            .expect("semantic policy identity should ignore input ordering"),
        EmissionAction::Provisional { revision: 1 }
    );
}

#[test]
fn correction_evidence_is_rejected_outside_the_exact_half_open_window() {
    let required = key("binance", "trades");
    let mut tracker = tracker_for(required.clone(), 0);
    let window = minute_zero();
    tracker
        .advance(
            &required,
            update(60_000_000_000, SourceHealthState::Healthy),
        )
        .expect("window should become final");

    assert_eq!(
        tracker.correction(&required, window, UnixNanos::new(-1)),
        Err(WatermarkError::InvalidCorrectionTime)
    );
    assert_eq!(
        tracker.correction(&required, window, window.end()),
        Err(WatermarkError::InvalidCorrectionTime)
    );
    tracker
        .correction(&required, window, window.start())
        .expect("the inclusive start should be valid");
}

#[test]
fn optional_bad_health_is_nonblocking_and_invalid_updates_are_atomic() {
    let required = key("binance", "trades");
    let optional = key("kraken", "trades");
    let mut tracker = WatermarkTracker::try_new(
        vec![
            PartitionConfig::required(required.clone()),
            PartitionConfig::optional(optional.clone()),
        ],
        DurationNanos::new(0),
        vec![SourceHealthState::Healthy],
    )
    .expect("tracker should be valid");
    tracker
        .advance(
            &required,
            update(60_000_000_000, SourceHealthState::Healthy),
        )
        .expect("required watermark should advance");
    tracker
        .advance(&optional, update(1, SourceHealthState::Quarantined))
        .expect("optional quality should be recorded");
    assert_eq!(tracker.decision(minute_zero()).state(), Finalization::Final);

    assert_eq!(
        tracker.advance(&required, update(0, SourceHealthState::Quarantined)),
        Err(WatermarkError::InvalidWatermark)
    );
    assert_eq!(
        tracker.watermark(&required),
        Some(UnixNanos::new(60_000_000_000))
    );
    assert_eq!(tracker.decision(minute_zero()).state(), Finalization::Final);
}

#[test]
fn time_windows_are_half_open_and_reject_overflow() {
    let definition = time_definition("tumbling-ten", WindowKind::Tumbling, 10, None);
    let tumbling = TimeWindowSpec::try_from_definition(&definition)
        .expect("window specification should be valid");
    assert_eq!(
        tumbling
            .windows_for(UnixNanos::new(9))
            .expect("membership should resolve"),
        vec![
            TimeWindow::try_new(UnixNanos::new(0), UnixNanos::new(10))
                .expect("window should be valid")
        ]
    );
    assert_eq!(
        tumbling
            .windows_for(UnixNanos::new(10))
            .expect("boundary membership should resolve"),
        vec![
            TimeWindow::try_new(UnixNanos::new(10), UnixNanos::new(20))
                .expect("window should be valid")
        ]
    );
    let overflowing =
        TimeWindowSpec::try_from_definition(&definition).expect("shape should be valid");
    assert!(matches!(
        overflowing.windows_for(UnixNanos::new(i64::MAX)),
        Err(WindowError::ArithmeticOverflow)
    ));
}

#[test]
fn registry_count_semantics_and_lifecycle_revisions_preserve_history() {
    assert!(
        WindowDefinition::try_new_event_count(
            window_id("gapped-count"),
            NonZeroU64::new(2).expect("extent should be nonzero"),
            NonZeroU64::new(3),
        )
        .is_err()
    );
    let definition = WindowDefinition::try_new_event_count(
        window_id("sliding-count"),
        NonZeroU64::new(2).expect("extent should be nonzero"),
        NonZeroU64::new(1),
    )
    .expect("count definition should be valid");
    let mut count =
        EventCountState::try_from_definition(&definition).expect("count state should be valid");
    assert!(count.observe().expect("count should advance").is_none());
    assert_eq!(
        count.observe().expect("count should advance"),
        Some(CountWindow {
            index: 0,
            start_event: 1,
            end_event: 2,
        })
    );
    assert_eq!(
        count.observe().expect("count should advance"),
        Some(CountWindow {
            index: 1,
            start_event: 2,
            end_event: 3,
        })
    );

    let window = minute_zero();
    let required = key("binance", "trades");
    let mut tracker = tracker_for(required.clone(), 0);
    let mut lifecycle =
        WindowLifecycle::try_new(&tracker, 1, 5).expect("lifecycle should be valid");
    assert_eq!(
        lifecycle
            .apply(tracker.decision(window))
            .expect("provisional should emit"),
        EmissionAction::Provisional { revision: 1 }
    );
    assert_eq!(
        lifecycle.apply(tracker.decision(window)),
        Err(WindowError::StaleFinalizationDecision)
    );
    tracker
        .advance(&required, update(5, SourceHealthState::Healthy))
        .expect("new provisional evidence should advance");
    assert_eq!(
        lifecycle
            .apply(tracker.decision(window))
            .expect("revised provisional should emit"),
        EmissionAction::RevisedProvisional {
            replaces_revision: 1,
            revision: 2,
        }
    );
    let premature_correction = tracker.correction(&required, window, UnixNanos::new(1));
    assert_eq!(
        premature_correction,
        Err(WatermarkError::InvalidCorrectionState)
    );
    assert_eq!(
        lifecycle
            .history(window)
            .expect("history should exist")
            .len(),
        2
    );
    tracker
        .advance(
            &required,
            update(60_000_000_000, SourceHealthState::Healthy),
        )
        .expect("watermark should advance");
    lifecycle
        .apply(tracker.decision(window))
        .expect("final should emit");
    tracker
        .advance(
            &required,
            update(61_000_000_000, SourceHealthState::Quarantined),
        )
        .expect("later health should be recorded");
    assert_eq!(
        lifecycle
            .history(window)
            .expect("history should remain sealed")[2]
            .finality(),
        FinalityState::Final
    );
    assert_eq!(
        tracker.correction(&required, window, UnixNanos::new(2)),
        Err(WatermarkError::InvalidCorrectionState)
    );
    assert_eq!(
        lifecycle
            .apply(tracker.decision(window))
            .expect("invalid revision should be explicit"),
        EmissionAction::Invalid { revision: 4 }
    );
    assert_eq!(
        tracker.correction(&required, window, UnixNanos::new(2)),
        Err(WatermarkError::InvalidCorrectionState)
    );
    tracker
        .advance(
            &required,
            update(62_000_000_000, SourceHealthState::Healthy),
        )
        .expect("source health should recover");
    assert_eq!(tracker.decision(window).state(), Finalization::Final);
    let recovered_correction = tracker
        .correction(&required, window, UnixNanos::new(2))
        .expect("healthy final evidence should permit recovery");
    assert_eq!(
        lifecycle
            .correct(recovered_correction)
            .expect("invalid history should recover append-only"),
        EmissionAction::Corrected {
            replaces_revision: 4,
            revision: 5,
        }
    );
    assert_eq!(
        lifecycle
            .history(window)
            .expect("history should exist")
            .len(),
        5
    );
}

#[test]
fn timer_failures_are_atomic_and_source_clock_skew_is_replayable() {
    let first = TimerId::new(1).expect("timer ID should be valid");
    let second = TimerId::new(2).expect("timer ID should be valid");
    let mut logical = LogicalClock::try_new(UnixNanos::new(10), 1).expect("clock should be valid");
    logical
        .schedule(first, UnixNanos::new(20))
        .expect("timer should schedule");
    assert_eq!(
        logical.schedule(first, UnixNanos::new(30)),
        Err(ClockError::DuplicateTimer)
    );
    assert_eq!(
        logical.schedule(second, UnixNanos::new(30)),
        Err(ClockError::TimerCapacity)
    );
    assert!(matches!(
        logical.advance_to(UnixNanos::new(9)),
        Err(ClockError::Regression { .. })
    ));
    assert_eq!(logical.now(), UnixNanos::new(10));
    assert_eq!(
        logical
            .advance_to(UnixNanos::new(20))
            .expect("clock should advance"),
        vec![first]
    );

    let skewed = RecordedTimestamp::try_new(UnixNanos::new(30), UnixNanos::new(20))
        .expect("independent source and receive clocks should be accepted");
    let mut event_clock = RecordedClock::try_new(
        LogicalClock::try_new(UnixNanos::new(1), 1).expect("clock should be valid"),
        ClockBasis::EventTime,
    );
    event_clock
        .schedule(second, UnixNanos::new(30))
        .expect("timer should schedule");
    assert_eq!(
        event_clock
            .advance(skewed)
            .expect("event clock should advance"),
        vec![second]
    );
    assert!(
        event_clock
            .advance(
                RecordedTimestamp::try_new(UnixNanos::new(20), UnixNanos::new(31))
                    .expect("late event should be shaped")
            )
            .expect("late event must not regress logical time")
            .is_empty()
    );
    assert_eq!(event_clock.now(), UnixNanos::new(30));
}
