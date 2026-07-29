use std::num::NonZeroU64;

use domain::{SourceId, SourceKind, UnixNanos};
use quality::{QualityCause, QualityError, SourceHealthState, SourceHealthTracker};

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "deribit", 1).expect("source")
}

#[test]
fn quality_transitions_require_compatible_evidence_and_consecutive_commits() {
    let mut tracker = SourceHealthTracker::new(source(), NonZeroU64::MIN);
    assert_eq!(tracker.state(), SourceHealthState::Recovering);

    let recovered = tracker
        .prepare(
            SourceHealthState::Healthy,
            QualityCause::RecoveryVerified,
            UnixNanos::new(1),
        )
        .expect("recovery evidence");
    let stale = recovered.event().clone();
    tracker.commit(recovered).expect("commit healthy");

    assert_eq!(tracker.state(), SourceHealthState::Healthy);
    assert_eq!(stale.sequence(), NonZeroU64::MIN);
    assert_eq!(
        tracker.prepare(
            SourceHealthState::Quarantined,
            QualityCause::FreshnessRestored,
            UnixNanos::new(2),
        ),
        Err(QualityError::IncompatibleCause)
    );

    let degraded = tracker
        .prepare(
            SourceHealthState::Degraded,
            QualityCause::CapacityPressure,
            UnixNanos::new(3),
        )
        .expect("capacity degradation");
    let duplicate = degraded.clone();
    tracker.commit(degraded).expect("commit degraded");
    assert_eq!(
        tracker.commit(duplicate),
        Err(QualityError::StaleTransition)
    );
}

#[test]
fn integrity_failure_cannot_return_healthy_without_recovery() {
    let mut tracker = SourceHealthTracker::new(source(), NonZeroU64::MIN);
    tracker
        .commit(
            tracker
                .prepare(
                    SourceHealthState::Healthy,
                    QualityCause::RecoveryVerified,
                    UnixNanos::new(1),
                )
                .expect("healthy"),
        )
        .expect("commit");
    tracker
        .commit(
            tracker
                .prepare(
                    SourceHealthState::Unhealthy,
                    QualityCause::SequenceIntegrityFailed,
                    UnixNanos::new(2),
                )
                .expect("integrity failure"),
        )
        .expect("commit");

    assert_eq!(
        tracker.prepare(
            SourceHealthState::Healthy,
            QualityCause::FreshnessRestored,
            UnixNanos::new(3),
        ),
        Err(QualityError::InvalidTransition {
            from: SourceHealthState::Unhealthy,
            to: SourceHealthState::Healthy,
        })
    );
    tracker
        .commit(
            tracker
                .prepare(
                    SourceHealthState::Recovering,
                    QualityCause::RecoveryStarted,
                    UnixNanos::new(4),
                )
                .expect("recovery start"),
        )
        .expect("commit");
}

#[test]
fn event_time_must_advance_and_quarantine_requires_policy_evidence() {
    let mut tracker = SourceHealthTracker::new(source(), NonZeroU64::new(7).expect("epoch"));
    tracker
        .commit(
            tracker
                .prepare(
                    SourceHealthState::Unhealthy,
                    QualityCause::SchemaValidityFailed,
                    UnixNanos::new(10),
                )
                .expect("schema failure"),
        )
        .expect("commit");

    assert_eq!(
        tracker.prepare(
            SourceHealthState::Quarantined,
            QualityCause::RetryBudgetExhausted,
            UnixNanos::new(10),
        ),
        Err(QualityError::NonMonotonicObservationTime)
    );
    let quarantined = tracker
        .prepare(
            SourceHealthState::Quarantined,
            QualityCause::RetryBudgetExhausted,
            UnixNanos::new(11),
        )
        .expect("budget exhaustion");
    assert_eq!(quarantined.event().connection_epoch().get(), 7);
}

#[test]
fn every_allowed_state_transition_has_a_compatible_typed_cause() {
    for from in SourceHealthState::ALL {
        for to in SourceHealthState::ALL {
            let transition_allowed = from.transition_to(to).is_ok();
            let compatible_causes = QualityCause::ALL
                .into_iter()
                .filter(|cause| cause.is_compatible(from, to))
                .count();
            assert_eq!(
                transition_allowed,
                compatible_causes > 0,
                "{from:?} -> {to:?} cause coverage"
            );
        }
    }
}
