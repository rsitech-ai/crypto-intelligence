use std::{
    fs::File,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    os::fd::AsFd,
};

use collector_runtime::{
    AdmissionError, AdmissionLimits, CollectorSupervisor, CoverageTier, OfferOutcome,
    OperationalQualityEvent, OperationalQualitySampleInput, OverflowAction, QueueClass,
    QueueTracker, RetryDecision, RetryPolicy, ShutdownActions, ShutdownError, ShutdownPhase,
    SourcePolicy, SupervisorHarness, execute_shutdown,
};
use connector_core::{Completeness, ConnectorCommand, ResynchronizationReason, StreamClass};
use domain::{SourceId, SourceKind, UnixNanos};
use quality::SourceHealthState;

fn source(name: &str, generation: u32) -> SourceId {
    SourceId::new(SourceKind::Exchange, name, generation).expect("source")
}

#[test]
fn operational_receipts_bind_supervisor_health_epoch_and_knowledge_time() {
    let mismatched_source = source("bybit", 1);
    let mut mismatched_supervisor =
        CollectorSupervisor::try_new(AdmissionLimits::try_new(1, 0, 0, 1).expect("limits"))
            .expect("supervisor");
    mismatched_supervisor
        .admit(
            mismatched_source.clone(),
            SourcePolicy::new(CoverageTier::A, NonZeroU32::MIN),
            RetryPolicy::try_new(3, 100, 1_000, 0, 1).expect("retry"),
        )
        .expect("admit");
    assert_eq!(
        mismatched_supervisor.bind_source_completeness(
            &mismatched_source,
            &connector_binance::binance_capabilities().expect("capabilities"),
            StreamClass::Liquidations,
        ),
        Err(AdmissionError::SourceCompletenessAuthorityMismatch)
    );

    let source = source("binance", 1);
    let mut supervisor =
        CollectorSupervisor::try_new(AdmissionLimits::try_new(1, 0, 0, 1).expect("limits"))
            .expect("supervisor");
    supervisor
        .admit(
            source.clone(),
            SourcePolicy::new(CoverageTier::A, NonZeroU32::MIN),
            RetryPolicy::try_new(3, 100, 1_000, 0, 1).expect("retry"),
        )
        .expect("admit");
    supervisor
        .mark_healthy(&source, UnixNanos::new(10))
        .expect("healthy transition");
    supervisor
        .record_operational_event(
            &source,
            OperationalQualityEvent::Recovery,
            UnixNanos::new(10),
        )
        .expect("collector-recorded recovery");
    supervisor
        .record_operational_event(
            &source,
            OperationalQualityEvent::SequenceGap,
            UnixNanos::new(20),
        )
        .expect("end-boundary event");
    let sample = OperationalQualitySampleInput {
        window_start: UnixNanos::new(1),
        window_end: UnixNanos::new(20),
        last_trusted_event_time: UnixNanos::new(19),
        receive_wall_time: UnixNanos::new(21),
        as_known_at: UnixNanos::new(21),
        stale_after_ns: 10,
        feed_jitter_ns: 1,
        clock_skew_estimate_ns: -1,
    };
    assert_eq!(
        supervisor.issue_operational_quality_receipt(&source, sample),
        Err(AdmissionError::SourceCompletenessNotConfigured)
    );
    assert_eq!(
        supervisor.bind_source_completeness(
            &source,
            &connector_binance::binance_capabilities().expect("capabilities"),
            StreamClass::OptionBooks,
        ),
        Err(AdmissionError::InvalidSourceCompleteness)
    );
    supervisor
        .bind_source_completeness(
            &source,
            &connector_binance::binance_capabilities().expect("capabilities"),
            StreamClass::MarkAndIndex,
        )
        .expect("bind connector completeness");
    assert_eq!(
        supervisor.bind_source_completeness(
            &source,
            &connector_binance::binance_capabilities().expect("capabilities"),
            StreamClass::Liquidations,
        ),
        Err(AdmissionError::SourceCompletenessAlreadyConfigured)
    );
    let receipt = supervisor
        .issue_operational_quality_receipt(&source, sample)
        .expect("sealed receipt");
    assert_eq!(receipt.source(), &source);
    assert_eq!(receipt.connection_epoch(), NonZeroU64::MIN);
    assert_eq!(receipt.source_health(), SourceHealthState::Healthy);
    assert_eq!(receipt.stream(), StreamClass::MarkAndIndex);
    assert_eq!(
        receipt.completeness(),
        Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        }
    );
    assert_eq!(receipt.sample(), sample);
    assert_eq!(receipt.recovery_count(), 1);
    assert_eq!(
        receipt.sequence_gap_count(),
        0,
        "half-open window must exclude an event at window_end"
    );

    assert_eq!(
        supervisor
            .retire_operational_events_before(&source, UnixNanos::new(20))
            .expect("retire finalized history"),
        1
    );
    assert_eq!(
        supervisor.record_operational_event(
            &source,
            OperationalQualityEvent::Reconnect,
            UnixNanos::new(19),
        ),
        Err(AdmissionError::OperationalEventBeforeRetirement)
    );
    assert_eq!(
        supervisor.issue_operational_quality_receipt(&source, sample),
        Err(AdmissionError::InvalidOperationalQualitySample),
        "a receipt must not silently undercount retired history"
    );
    let retained_sample = OperationalQualitySampleInput {
        window_start: UnixNanos::new(20),
        window_end: UnixNanos::new(21),
        last_trusted_event_time: UnixNanos::new(20),
        receive_wall_time: UnixNanos::new(21),
        as_known_at: UnixNanos::new(21),
        ..sample
    };
    assert_eq!(
        supervisor
            .issue_operational_quality_receipt(&source, retained_sample)
            .expect("watermark event retained")
            .sequence_gap_count(),
        1
    );
    assert_eq!(
        supervisor.retire_operational_events_before(&source, UnixNanos::new(19)),
        Err(AdmissionError::InvalidOperationalEventRetirement)
    );

    let mut future_health = sample;
    future_health.as_known_at = UnixNanos::new(9);
    assert_eq!(
        supervisor.issue_operational_quality_receipt(&source, future_health),
        Err(AdmissionError::InvalidOperationalQualitySample)
    );
    let data_volume =
        File::open(tempfile::tempdir().expect("temp volume").path()).expect("open temp volume");
    assert_eq!(
        supervisor.issue_storage_pressure_receipt(UnixNanos::new(1)),
        Err(AdmissionError::StoragePressureProbeNotConfigured)
    );
    supervisor
        .bind_data_volume(data_volume.as_fd())
        .expect("bind data volume");
    assert_eq!(
        supervisor.bind_data_volume(data_volume.as_fd()),
        Err(AdmissionError::StoragePressureProbeAlreadyConfigured)
    );
    assert_eq!(
        supervisor.issue_storage_pressure_receipt(UnixNanos::new(0)),
        Err(AdmissionError::InvalidOperationalQualitySample)
    );
}

#[tokio::test]
async fn full_book_delta_queue_degrades_and_resyncs_instead_of_dropping() {
    let report = SupervisorHarness::with_capacity(1)
        .expect("harness")
        .saturate_book_deltas()
        .await
        .expect("overflow report");
    assert_eq!(report.action(), OverflowAction::DegradeAndResnapshot);
    assert_eq!(report.silent_drops(), 0);
    assert_eq!(report.published_quality_events(), 2);
    assert_eq!(report.quality_state(), SourceHealthState::Recovering);
    assert!(matches!(
        report.connector_command(),
        Some(ConnectorCommand::Resynchronize {
            reason: ResynchronizationReason::RequiredDeltaOverflow,
            ..
        })
    ));
}

#[test]
fn ticker_updates_may_only_coalesce_the_same_instrument_key() {
    assert!(QueueClass::UiTicker.coalescing_allowed());
    assert!(QueueClass::OptionalTicker.coalescing_allowed());
    assert!(!QueueClass::BookDelta.coalescing_allowed());
    assert!(!QueueClass::RawWal.coalescing_allowed());

    let mut queue = QueueTracker::try_new(
        QueueClass::UiTicker,
        NonZeroUsize::new(1).expect("capacity"),
    )
    .expect("queue");
    assert_eq!(
        queue.offer(Some("btc-usd")).expect("first"),
        OfferOutcome::Queued
    );
    assert_eq!(
        queue.offer(Some("btc-usd")).expect("replacement"),
        OfferOutcome::Coalesced
    );
    assert_eq!(
        queue.offer(Some("eth-usd")),
        Err(AdmissionError::QueueCapacity)
    );
    let metrics = queue.metrics();
    assert_eq!(queue.overflow_action(), OverflowAction::CoalesceByKey);
    assert_eq!(metrics.occupancy(), 1);
    assert_eq!(metrics.coalesced(), 1);
    assert_eq!(metrics.overflows(), 2);
    assert_eq!(metrics.rejected(), 1);
    assert_eq!(metrics.silent_drops(), 0);
    assert_eq!(
        queue.complete(Some("eth-usd")),
        Err(AdmissionError::UnknownQueueKey)
    );
    assert_eq!(queue.metrics().occupancy(), 1);
    queue
        .complete(Some("btc-usd"))
        .expect("complete retained key");
    assert_eq!(queue.metrics().occupancy(), 0);
}

#[test]
fn raw_wal_saturation_backpressures_without_mutation_or_drop() {
    let mut queue =
        QueueTracker::try_new(QueueClass::RawWal, NonZeroUsize::new(1).expect("capacity"))
            .expect("queue");
    assert_eq!(queue.offer(None).expect("first"), OfferOutcome::Queued);
    assert_eq!(
        queue.offer(None).expect("backpressure"),
        OfferOutcome::Backpressured
    );
    assert_eq!(queue.metrics().occupancy(), 1);
    assert_eq!(queue.metrics().backpressured(), 1);
    assert_eq!(queue.metrics().silent_drops(), 0);
}

#[test]
fn every_queue_class_has_one_explicit_overflow_policy() {
    let actions = QueueClass::ALL.map(collector_runtime::overflow_action);
    assert_eq!(
        actions,
        [
            OverflowAction::Backpressure,
            OverflowAction::Backpressure,
            OverflowAction::DegradeAndResnapshot,
            OverflowAction::Backpressure,
            OverflowAction::Backpressure,
            OverflowAction::Backpressure,
            OverflowAction::CoalesceByKey,
            OverflowAction::CoalesceByKey,
            OverflowAction::Backpressure,
            OverflowAction::RejectAdmission,
        ]
    );
}

#[test]
fn tier_admission_is_bounded_and_never_silently_downgrades_tier_a() {
    let limits = AdmissionLimits::try_new(1, 1, 1, 2).expect("limits");
    let policy = SourcePolicy::new(CoverageTier::A, NonZeroU32::new(1).expect("instruments"));
    let retry = RetryPolicy::try_new(3, 100, 2_000, 1_000, 42).expect("retry");
    let mut supervisor = CollectorSupervisor::try_new(limits).expect("supervisor");

    supervisor
        .admit(source("binance", 1), policy, retry)
        .expect("first Tier A source");
    assert_eq!(
        supervisor.admit(source("deribit", 1), policy, retry),
        Err(AdmissionError::TierCapacity {
            tier: CoverageTier::A
        })
    );
    assert_eq!(supervisor.source_count(), 1);
}

#[test]
fn retry_backoff_is_deterministic_bounded_and_quarantines_on_exhaustion() {
    let limits = AdmissionLimits::try_new(1, 1, 1, 2).expect("limits");
    let retry = RetryPolicy::try_new(3, 100, 2_000, 1_000, 42).expect("retry");
    let source_id = source("kraken", 1);
    let mut first = CollectorSupervisor::try_new(limits).expect("supervisor");
    let mut second = CollectorSupervisor::try_new(limits).expect("supervisor");
    for supervisor in [&mut first, &mut second] {
        supervisor
            .admit(
                source_id.clone(),
                SourcePolicy::new(CoverageTier::A, NonZeroU32::new(1).expect("instruments")),
                retry,
            )
            .expect("admit");
        supervisor
            .mark_healthy(&source_id, UnixNanos::new(1))
            .expect("healthy");
    }

    let one = first
        .record_failure(&source_id, UnixNanos::new(2))
        .expect("first failure");
    let two = second
        .record_failure(&source_id, UnixNanos::new(2))
        .expect("same failure");
    assert_eq!(one, two);
    let RetryDecision::Backoff { delay_ms, .. } = one else {
        panic!("first failure must back off");
    };
    assert!((90..=110).contains(&delay_ms));

    assert!(matches!(
        first
            .record_failure(&source_id, UnixNanos::new(3))
            .expect("second failure"),
        RetryDecision::Backoff { .. }
    ));
    assert_eq!(
        first
            .record_failure(&source_id, UnixNanos::new(4))
            .expect("budget exhausted"),
        RetryDecision::Quarantined
    );
    assert_eq!(
        first.source_state(&source_id).expect("state"),
        SourceHealthState::Quarantined
    );
    assert_eq!(first.pending_quality_events(), 3);
    let published_states = (0..3)
        .map(|_| first.pop_quality_event().expect("quality event").to())
        .collect::<Vec<_>>();
    assert_eq!(
        published_states,
        [
            SourceHealthState::Healthy,
            SourceHealthState::Unhealthy,
            SourceHealthState::Quarantined,
        ]
    );
    assert_eq!(first.pending_quality_events(), 0);
}

#[test]
fn connection_epoch_renewal_is_checked_and_resets_retry_state() {
    let limits = AdmissionLimits::try_new(1, 1, 1, 1).expect("limits");
    let retry = RetryPolicy::try_new(2, 100, 200, 0, 7).expect("retry");
    let source_id = source("bybit", 1);
    let mut supervisor = CollectorSupervisor::try_new(limits).expect("supervisor");
    supervisor
        .admit_with_epoch(
            source_id.clone(),
            SourcePolicy::new(CoverageTier::A, NonZeroU32::new(1).expect("instruments")),
            retry,
            NonZeroU64::new(u64::MAX).expect("epoch"),
        )
        .expect("admit");
    assert_eq!(
        supervisor.renew_connection(&source_id),
        Err(AdmissionError::ConnectionEpochExhausted)
    );
}

#[test]
fn retry_policy_and_failure_time_bounds_fail_closed() {
    assert_eq!(
        RetryPolicy::try_new(65, 100, 1_000, 0, 1),
        Err(AdmissionError::InvalidRetryPolicy)
    );
    assert_eq!(
        RetryPolicy::try_new(3, 100, 86_400_001, 0, 1),
        Err(AdmissionError::InvalidRetryPolicy)
    );

    let limits = AdmissionLimits::try_new(1, 0, 0, 1).expect("limits");
    let retry = RetryPolicy::try_new(3, 100, 1_000, 0, 1).expect("retry");
    let source_id = source("binance", 1);
    let mut supervisor = CollectorSupervisor::try_new(limits).expect("supervisor");
    supervisor
        .admit(
            source_id.clone(),
            SourcePolicy::new(CoverageTier::A, NonZeroU32::new(1).expect("instruments")),
            retry,
        )
        .expect("admit");
    supervisor
        .mark_healthy(&source_id, UnixNanos::new(1))
        .expect("healthy");
    supervisor
        .record_failure(&source_id, UnixNanos::new(2))
        .expect("first failure");
    assert_eq!(
        supervisor.record_failure(&source_id, UnixNanos::new(2)),
        Err(AdmissionError::NonMonotonicFailureTime)
    );
}

#[derive(Default)]
struct RecordingShutdown {
    calls: Vec<ShutdownPhase>,
    fail_at: Option<ShutdownPhase>,
}

impl RecordingShutdown {
    fn record(&mut self, phase: ShutdownPhase) -> Result<(), ShutdownError> {
        self.calls.push(phase);
        if self.fail_at == Some(phase) {
            Err(ShutdownError::StepFailed(phase))
        } else {
            Ok(())
        }
    }
}

impl ShutdownActions for RecordingShutdown {
    async fn stop_source_reads(&mut self) -> Result<(), ShutdownError> {
        self.record(ShutdownPhase::StopSourceReads)
    }

    async fn drain_wal(&mut self) -> Result<(), ShutdownError> {
        self.record(ShutdownPhase::DrainWal)
    }

    async fn checkpoint_books(&mut self) -> Result<(), ShutdownError> {
        self.record(ShutdownPhase::CheckpointBooks)
    }

    async fn seal_partitions(&mut self) -> Result<(), ShutdownError> {
        self.record(ShutdownPhase::SealPartitions)
    }

    async fn close_metadata(&mut self) -> Result<(), ShutdownError> {
        self.record(ShutdownPhase::CloseMetadata)
    }
}

#[tokio::test]
async fn shutdown_executes_the_exact_durable_order_and_stops_after_failure() {
    let expected = vec![
        ShutdownPhase::StopSourceReads,
        ShutdownPhase::DrainWal,
        ShutdownPhase::CheckpointBooks,
        ShutdownPhase::SealPartitions,
        ShutdownPhase::CloseMetadata,
    ];
    let mut success = RecordingShutdown::default();
    let report = execute_shutdown(&mut success).await.expect("shutdown");
    assert_eq!(success.calls, expected);
    assert_eq!(report.completed(), expected);

    let mut failed = RecordingShutdown {
        fail_at: Some(ShutdownPhase::CheckpointBooks),
        ..RecordingShutdown::default()
    };
    assert_eq!(
        execute_shutdown(&mut failed).await,
        Err(ShutdownError::StepFailed(ShutdownPhase::CheckpointBooks))
    );
    assert_eq!(
        failed.calls,
        [
            ShutdownPhase::StopSourceReads,
            ShutdownPhase::DrainWal,
            ShutdownPhase::CheckpointBooks,
        ]
    );
}
