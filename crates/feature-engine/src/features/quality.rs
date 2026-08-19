//! Operational-quality features and separately typed gating fields.

use collector_runtime::OperationalQualityReceipt;
use connector_core::{Completeness, StreamClass};
use quality::SourceHealthState;

use super::FeatureComputationError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationalUncertaintyFields {
    source_latency_ns: u64,
    feed_jitter_ns: u64,
    clock_skew_estimate_ns: i64,
    event_age_ns: u64,
    stale_quote_duration_ns: u64,
    sequence_gap_count: u64,
    checksum_failure_count: u64,
    reconnect_count: u64,
    recovery_count: u64,
    correction_count: u64,
    revision_count: u64,
    raw_to_normalized_rejection_count: u64,
    source_health: SourceHealthState,
    stream: StreamClass,
    completeness: Completeness,
}

impl OperationalUncertaintyFields {
    pub const fn source_latency_ns(self) -> u64 {
        self.source_latency_ns
    }

    pub const fn event_age_ns(self) -> u64 {
        self.event_age_ns
    }

    pub const fn feed_jitter_ns(self) -> u64 {
        self.feed_jitter_ns
    }

    pub const fn clock_skew_estimate_ns(self) -> i64 {
        self.clock_skew_estimate_ns
    }

    pub const fn stale_quote_duration_ns(self) -> u64 {
        self.stale_quote_duration_ns
    }

    pub const fn sequence_gap_count(self) -> u64 {
        self.sequence_gap_count
    }

    pub const fn stale(self) -> bool {
        self.stale_quote_duration_ns > 0
    }

    pub const fn checksum_failed(self) -> bool {
        self.checksum_failure_count > 0
    }

    pub const fn checksum_failure_count(self) -> u64 {
        self.checksum_failure_count
    }

    pub const fn reconnect_count(self) -> u64 {
        self.reconnect_count
    }

    pub const fn recovery_count(self) -> u64 {
        self.recovery_count
    }

    pub const fn correction_count(self) -> u64 {
        self.correction_count
    }

    pub const fn revision_count(self) -> u64 {
        self.revision_count
    }

    pub const fn raw_to_normalized_rejection_count(self) -> u64 {
        self.raw_to_normalized_rejection_count
    }

    pub const fn source_health(self) -> SourceHealthState {
        self.source_health
    }

    pub const fn stream(self) -> StreamClass {
        self.stream
    }

    pub const fn completeness(self) -> Completeness {
        self.completeness
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationalGatingFields {
    authority_available: bool,
    eligible: bool,
    complete_for_cascade: bool,
    source_health: SourceHealthState,
}

impl OperationalGatingFields {
    pub const fn authority_available(self) -> bool {
        self.authority_available
    }

    pub const fn eligible(self) -> bool {
        self.eligible
    }

    pub const fn complete_for_cascade(self) -> bool {
        self.complete_for_cascade
    }

    pub const fn source_health(self) -> SourceHealthState {
        self.source_health
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationalQualitySnapshot {
    uncertainty_fields: OperationalUncertaintyFields,
    gating_fields: OperationalGatingFields,
}

impl OperationalQualitySnapshot {
    pub const fn uncertainty_fields(&self) -> &OperationalUncertaintyFields {
        &self.uncertainty_fields
    }

    pub const fn gating_fields(&self) -> &OperationalGatingFields {
        &self.gating_fields
    }
}

pub fn operational_quality_snapshot(
    receipt: &OperationalQualityReceipt,
) -> Result<OperationalQualitySnapshot, FeatureComputationError> {
    let input = receipt.sample();
    let source_latency_ns = input
        .receive_wall_time
        .value()
        .checked_sub(input.last_trusted_event_time.value())
        .and_then(|latency| u64::try_from(latency).ok())
        .ok_or(FeatureComputationError::InvalidInput)?;
    let event_age_ns = input
        .as_known_at
        .value()
        .checked_sub(input.last_trusted_event_time.value())
        .and_then(|age| u64::try_from(age).ok())
        .ok_or(FeatureComputationError::InvalidInput)?;
    let stale_quote_duration_ns = if event_age_ns > input.stale_after_ns {
        event_age_ns
    } else {
        0
    };

    let complete_for_cascade = receipt.sequence_gap_count() == 0
        && receipt.checksum_failure_count() == 0
        && receipt.stream() == StreamClass::Liquidations
        && matches!(
            receipt.completeness(),
            Completeness::VenueReportedComplete { .. } | Completeness::VenueReportedAll { .. }
        );
    let eligible =
        receipt.source_health() == SourceHealthState::Healthy && stale_quote_duration_ns == 0;
    Ok(OperationalQualitySnapshot {
        uncertainty_fields: OperationalUncertaintyFields {
            source_latency_ns,
            feed_jitter_ns: input.feed_jitter_ns,
            clock_skew_estimate_ns: input.clock_skew_estimate_ns,
            event_age_ns,
            stale_quote_duration_ns,
            sequence_gap_count: receipt.sequence_gap_count(),
            checksum_failure_count: receipt.checksum_failure_count(),
            reconnect_count: receipt.reconnect_count(),
            recovery_count: receipt.recovery_count(),
            correction_count: receipt.correction_count(),
            revision_count: receipt.revision_count(),
            raw_to_normalized_rejection_count: receipt.raw_to_normalized_rejection_count(),
            source_health: receipt.source_health(),
            stream: receipt.stream(),
            completeness: receipt.completeness(),
        },
        gating_fields: OperationalGatingFields {
            authority_available: true,
            eligible,
            complete_for_cascade,
            source_health: receipt.source_health(),
        },
    })
}
