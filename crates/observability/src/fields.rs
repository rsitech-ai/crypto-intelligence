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
    InstrumentId,
    ConnectionId,
    EventId,
    ForecastId,
    ReplayId,
    IncidentId,
    Sequence,
    QueueDepth,
    DurationMicros,
}

impl FieldName {
    pub const ALL: [Self; 17] = [
        Self::Level,
        Self::Event,
        Self::Target,
        Self::Spans,
        Self::Message,
        Self::Component,
        Self::Outcome,
        Self::SourceId,
        Self::InstrumentId,
        Self::ConnectionId,
        Self::EventId,
        Self::ForecastId,
        Self::ReplayId,
        Self::IncidentId,
        Self::Sequence,
        Self::QueueDepth,
        Self::DurationMicros,
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
            Self::InstrumentId => "instrument_id",
            Self::ConnectionId => "connection_id",
            Self::EventId => "event_id",
            Self::ForecastId => "forecast_id",
            Self::ReplayId => "replay_id",
            Self::IncidentId => "incident_id",
            Self::Sequence => "sequence",
            Self::QueueDepth => "queue_depth",
            Self::DurationMicros => "duration_micros",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.as_str() == value)
    }
}
