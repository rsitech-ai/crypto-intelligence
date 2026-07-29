//! Stable structured-log field names.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldName {
    Level,
    Event,
    Target,
    Spans,
    Message,
    Component,
    Outcome,
    SourceId,
    SourceGeneration,
    InstrumentId,
    ConnectionId,
    ConnectionEpoch,
    EventId,
    ForecastId,
    ReplayId,
    IncidentId,
    Sequence,
    QualitySequence,
    ObservedAtUnixNanos,
    QualityFrom,
    QualityTo,
    QualityCause,
    QueueDepth,
    DurationMicros,
    PendingWalCompressionJobs,
}

impl FieldName {
    pub const ALL: [Self; 25] = [
        Self::Level,
        Self::Event,
        Self::Target,
        Self::Spans,
        Self::Message,
        Self::Component,
        Self::Outcome,
        Self::SourceId,
        Self::SourceGeneration,
        Self::InstrumentId,
        Self::ConnectionId,
        Self::ConnectionEpoch,
        Self::EventId,
        Self::ForecastId,
        Self::ReplayId,
        Self::IncidentId,
        Self::Sequence,
        Self::QualitySequence,
        Self::ObservedAtUnixNanos,
        Self::QualityFrom,
        Self::QualityTo,
        Self::QualityCause,
        Self::QueueDepth,
        Self::DurationMicros,
        Self::PendingWalCompressionJobs,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Level => "level",
            Self::Event => "event",
            Self::Target => "target",
            Self::Spans => "spans",
            Self::Message => "message",
            Self::Component => "component",
            Self::Outcome => "outcome",
            Self::SourceId => "source_id",
            Self::SourceGeneration => "source_generation",
            Self::InstrumentId => "instrument_id",
            Self::ConnectionId => "connection_id",
            Self::ConnectionEpoch => "connection_epoch",
            Self::EventId => "event_id",
            Self::ForecastId => "forecast_id",
            Self::ReplayId => "replay_id",
            Self::IncidentId => "incident_id",
            Self::Sequence => "sequence",
            Self::QualitySequence => "quality_sequence",
            Self::ObservedAtUnixNanos => "observed_at_unix_nanos",
            Self::QualityFrom => "quality_from",
            Self::QualityTo => "quality_to",
            Self::QualityCause => "quality_cause",
            Self::QueueDepth => "queue_depth",
            Self::DurationMicros => "duration_micros",
            Self::PendingWalCompressionJobs => "pending_wal_compression_jobs",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.as_str() == value)
    }
}
