//! Validated market events with complete, deterministic BLAKE3 identities.

mod payload;
mod quality;

pub use payload::*;
pub use quality::{MAX_QUALITY_SCORE_PPM, QualityFlags};

use domain::{InstrumentDefinition, InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use serde::{Deserialize, Serialize, de::Error as _, ser::SerializeStruct as _};
use std::{fmt, io};
use thiserror::Error;

const EVENT_DOMAIN: &[u8] = b"cmti:event:v1\0";
const MAX_BOOK_LEVELS: usize = 100_000;
const MAX_METADATA_TEXT: usize = 4_096;
/// Maximum serialized event size accepted before any untrusted serde allocation.
///
/// This matches the raw-WAL frame boundary used by the production ingestion
/// path. Untrusted JSON callers must use [`EventEnvelope::from_json_slice`].
pub const MAX_EVENT_WIRE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EventError {
    #[error("invalid event metadata: {0}")]
    InvalidMetadata(&'static str),
    #[error("invalid event payload: {0}")]
    InvalidPayload(&'static str),
    #[error("event metadata sequence does not match payload")]
    SequenceMismatch,
    #[error("event identity does not match metadata and payload")]
    IdentityMismatch,
    #[error("event type does not match payload")]
    EventTypeMismatch,
    #[error("canonical event field is too large")]
    CanonicalFieldTooLarge,
    #[error("serialized event exceeds the bounded wire size")]
    WireTooLarge,
    #[error("serialized event is invalid")]
    InvalidWire,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SnapshotKind {
    NotApplicable = 0,
    Snapshot = 1,
    Delta = 2,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UncheckedEventMetadata {
    pub schema_version: u32,
    pub source: SourceId,
    pub venue: Option<VenueId>,
    pub instrument_id: Option<InstrumentId>,
    #[serde(rename = "exchange_event_time_ns")]
    pub exchange_timestamp: Option<UnixNanos>,
    #[serde(rename = "exchange_transaction_time_ns")]
    pub exchange_transaction_timestamp: Option<UnixNanos>,
    #[serde(rename = "receive_wall_time_ns")]
    pub receive_wall_timestamp: UnixNanos,
    #[serde(rename = "receive_monotonic_time_ns")]
    pub receive_monotonic_ns: u64,
    #[serde(rename = "normalization_time_ns")]
    pub normalization_timestamp: UnixNanos,
    pub connection_started_at: UnixNanos,
    pub sequence_number: Option<u64>,
    pub previous_sequence_number: Option<u64>,
    pub connection_epoch: u64,
    pub subscription_epoch: u64,
    #[serde(rename = "snapshot_or_delta")]
    pub snapshot_kind: SnapshotKind,
    pub source_checksum: Option<String>,
    pub raw_payload_hash: [u8; 32],
    pub parser_version: String,
    pub normalizer_version: String,
    pub ingestion_instance: String,
    pub quality_score_ppm: u32,
    pub quality_flags: QualityFlags,
}

impl UncheckedEventMetadata {
    fn validate(&self) -> Result<(), EventError> {
        if self.schema_version == 0 {
            return Err(EventError::InvalidMetadata("schema_version"));
        }
        if self.connection_epoch == 0 {
            return Err(EventError::InvalidMetadata("connection_epoch"));
        }
        if self.subscription_epoch == 0 {
            return Err(EventError::InvalidMetadata("subscription_epoch"));
        }
        if self.receive_wall_timestamp < self.connection_started_at {
            return Err(EventError::InvalidMetadata("receive_wall_timestamp"));
        }
        if self.normalization_timestamp < self.receive_wall_timestamp {
            return Err(EventError::InvalidMetadata("normalization_timestamp"));
        }
        if self
            .previous_sequence_number
            .zip(self.sequence_number)
            .is_some_and(|(previous, current)| previous >= current)
            || (self.previous_sequence_number.is_some() && self.sequence_number.is_none())
        {
            return Err(EventError::InvalidMetadata("sequence_number"));
        }
        if self.raw_payload_hash == [0; 32] {
            return Err(EventError::InvalidMetadata("raw_payload_hash"));
        }
        if !valid_metadata_text(&self.parser_version) {
            return Err(EventError::InvalidMetadata("parser_version"));
        }
        if !valid_metadata_text(&self.normalizer_version) {
            return Err(EventError::InvalidMetadata("normalizer_version"));
        }
        if !valid_metadata_text(&self.ingestion_instance) {
            return Err(EventError::InvalidMetadata("ingestion_instance"));
        }
        if self
            .source_checksum
            .as_deref()
            .is_some_and(|value| !valid_metadata_text(value))
        {
            return Err(EventError::InvalidMetadata("source_checksum"));
        }
        if self.quality_score_ppm > MAX_QUALITY_SCORE_PPM {
            return Err(EventError::InvalidMetadata("quality_score_ppm"));
        }
        if self.source.kind() == SourceKind::Exchange && self.venue.is_none() {
            return Err(EventError::InvalidMetadata("venue"));
        }
        if let Some(instrument) = &self.instrument_id
            && self.venue.as_ref() != Some(instrument.venue())
        {
            return Err(EventError::InvalidMetadata("instrument_id"));
        }
        Ok(())
    }
}

/// Event metadata that has passed the complete envelope contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct EventMetadata(UncheckedEventMetadata);

impl EventMetadata {
    pub const fn as_unchecked(&self) -> &UncheckedEventMetadata {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Side {
    Buy = 0,
    Sell = 1,
    Unknown = 2,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum EventType {
    Trade = 0,
    TopOfBook = 1,
    BookSnapshot = 2,
    BookDelta = 3,
    InstrumentDefinition = 4,
    FundingObservation = 5,
    OpenInterestObservation = 6,
    LiquidationObservation = 7,
    MarkIndexObservation = 8,
    FutureBasisObservation = 9,
    OptionTicker = 10,
    OptionTrade = 11,
    VenueStatus = 12,
    ChainBlock = 13,
    ChainTransactionAggregate = 14,
    ChainMempoolObservation = 15,
    ChainMetric = 16,
    ExternalMarketObservation = 17,
    StructuredEvent = 18,
    DataQualityObservation = 19,
    PredictionRecord = 20,
    AlertEvent = 21,
}

impl EventType {
    pub const ALL: [Self; 22] = [
        Self::Trade,
        Self::TopOfBook,
        Self::BookSnapshot,
        Self::BookDelta,
        Self::InstrumentDefinition,
        Self::FundingObservation,
        Self::OpenInterestObservation,
        Self::LiquidationObservation,
        Self::MarkIndexObservation,
        Self::FutureBasisObservation,
        Self::OptionTicker,
        Self::OptionTrade,
        Self::VenueStatus,
        Self::ChainBlock,
        Self::ChainTransactionAggregate,
        Self::ChainMempoolObservation,
        Self::ChainMetric,
        Self::ExternalMarketObservation,
        Self::StructuredEvent,
        Self::DataQualityObservation,
        Self::PredictionRecord,
        Self::AlertEvent,
    ];
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookLevel {
    pub price: Price,
    pub quantity: Quantity,
    pub order_count: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trade {
    pub trade_id: String,
    pub price: Price,
    pub quantity: Quantity,
    pub side: Side,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookSnapshot {
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub last_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookDelta {
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub first_sequence: u64,
    pub last_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum UncheckedEventPayload {
    Trade(Trade),
    TopOfBook(TopOfBook),
    BookSnapshot(BookSnapshot),
    BookDelta(BookDelta),
    InstrumentDefinition(InstrumentDefinition),
    FundingObservation(FundingObservation),
    OpenInterestObservation(OpenInterestObservation),
    LiquidationObservation(LiquidationObservation),
    MarkIndexObservation(MarkIndexObservation),
    FutureBasisObservation(FutureBasisObservation),
    OptionTicker(OptionTicker),
    OptionTrade(OptionTrade),
    VenueStatus(VenueStatus),
    ChainBlock(ChainBlock),
    ChainTransactionAggregate(ChainTransactionAggregate),
    ChainMempoolObservation(ChainMempoolObservation),
    ChainMetric(ChainMetric),
    ExternalMarketObservation(ExternalMarketObservation),
    StructuredEvent(StructuredEvent),
    DataQualityObservation(DataQualityObservation),
    PredictionRecord(PredictionRecord),
    AlertEvent(AlertEvent),
}

impl UncheckedEventPayload {
    pub const fn event_type(&self) -> EventType {
        match self {
            Self::Trade(_) => EventType::Trade,
            Self::TopOfBook(_) => EventType::TopOfBook,
            Self::BookSnapshot(_) => EventType::BookSnapshot,
            Self::BookDelta(_) => EventType::BookDelta,
            Self::InstrumentDefinition(_) => EventType::InstrumentDefinition,
            Self::FundingObservation(_) => EventType::FundingObservation,
            Self::OpenInterestObservation(_) => EventType::OpenInterestObservation,
            Self::LiquidationObservation(_) => EventType::LiquidationObservation,
            Self::MarkIndexObservation(_) => EventType::MarkIndexObservation,
            Self::FutureBasisObservation(_) => EventType::FutureBasisObservation,
            Self::OptionTicker(_) => EventType::OptionTicker,
            Self::OptionTrade(_) => EventType::OptionTrade,
            Self::VenueStatus(_) => EventType::VenueStatus,
            Self::ChainBlock(_) => EventType::ChainBlock,
            Self::ChainTransactionAggregate(_) => EventType::ChainTransactionAggregate,
            Self::ChainMempoolObservation(_) => EventType::ChainMempoolObservation,
            Self::ChainMetric(_) => EventType::ChainMetric,
            Self::ExternalMarketObservation(_) => EventType::ExternalMarketObservation,
            Self::StructuredEvent(_) => EventType::StructuredEvent,
            Self::DataQualityObservation(_) => EventType::DataQualityObservation,
            Self::PredictionRecord(_) => EventType::PredictionRecord,
            Self::AlertEvent(_) => EventType::AlertEvent,
        }
    }

    fn validate(&self, metadata: &UncheckedEventMetadata) -> Result<(), EventError> {
        metadata.validate()?;
        if self.requires_instrument()
            && (metadata.venue.is_none() || metadata.instrument_id.is_none())
        {
            return Err(EventError::InvalidMetadata("instrument identity"));
        }

        match self {
            Self::Trade(trade) => {
                if !valid_metadata_text(&trade.trade_id) || trade.quantity.value().is_zero() {
                    return Err(EventError::InvalidPayload("trade"));
                }
                if metadata.snapshot_kind != SnapshotKind::NotApplicable {
                    return Err(EventError::InvalidMetadata("snapshot_kind"));
                }
            }
            Self::TopOfBook(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
            Self::BookSnapshot(snapshot) => {
                validate_book(&snapshot.bids, &snapshot.asks, false, true)?;
                if metadata.snapshot_kind != SnapshotKind::Snapshot
                    || metadata.sequence_number != Some(snapshot.last_sequence)
                {
                    return Err(EventError::SequenceMismatch);
                }
            }
            Self::BookDelta(delta) => {
                validate_book(&delta.bids, &delta.asks, true, false)?;
                if delta.first_sequence > delta.last_sequence
                    || metadata.snapshot_kind != SnapshotKind::Delta
                    || metadata.sequence_number != Some(delta.last_sequence)
                    || metadata.previous_sequence_number != delta.first_sequence.checked_sub(1)
                {
                    return Err(EventError::SequenceMismatch);
                }
            }
            Self::InstrumentDefinition(definition) => {
                require_observation(metadata)?;
                if metadata.instrument_id.as_ref() != Some(definition.id()) {
                    return Err(EventError::InvalidMetadata("instrument definition"));
                }
            }
            Self::FundingObservation(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
            Self::OpenInterestObservation(_) | Self::MarkIndexObservation(_) => {
                require_observation(metadata)?;
            }
            Self::LiquidationObservation(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
            Self::FutureBasisObservation(_) => require_observation(metadata)?,
            Self::OptionTicker(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
            Self::OptionTrade(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
            Self::VenueStatus(value) => {
                value.validate()?;
                require_observation(metadata)?;
                if metadata.venue.is_none() {
                    return Err(EventError::InvalidMetadata("venue"));
                }
            }
            Self::ChainBlock(value) => {
                value.validate()?;
                require_observation(metadata)?;
                require_source_kind(metadata, SourceKind::Chain)?;
            }
            Self::ChainTransactionAggregate(value) => {
                value.validate()?;
                require_observation(metadata)?;
                require_source_kind(metadata, SourceKind::Chain)?;
            }
            Self::ChainMempoolObservation(_) | Self::ChainMetric(_) => {
                require_observation(metadata)?;
                require_source_kind(metadata, SourceKind::Chain)?;
                if let Self::ChainMetric(value) = self {
                    value.validate()?;
                }
            }
            Self::ExternalMarketObservation(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
            Self::StructuredEvent(value) => {
                value.validate()?;
                require_observation(metadata)?;
                if metadata.source != value.source_identity {
                    return Err(EventError::InvalidMetadata("structured event source"));
                }
            }
            Self::DataQualityObservation(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
            Self::PredictionRecord(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
            Self::AlertEvent(value) => {
                value.validate()?;
                require_observation(metadata)?;
            }
        }
        Ok(())
    }

    const fn requires_instrument(&self) -> bool {
        matches!(
            self,
            Self::Trade(_)
                | Self::TopOfBook(_)
                | Self::BookSnapshot(_)
                | Self::BookDelta(_)
                | Self::InstrumentDefinition(_)
                | Self::FundingObservation(_)
                | Self::OpenInterestObservation(_)
                | Self::LiquidationObservation(_)
                | Self::MarkIndexObservation(_)
                | Self::FutureBasisObservation(_)
                | Self::OptionTicker(_)
                | Self::OptionTrade(_)
        )
    }
}

fn valid_metadata_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_METADATA_TEXT
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn require_observation(metadata: &UncheckedEventMetadata) -> Result<(), EventError> {
    if metadata.snapshot_kind == SnapshotKind::NotApplicable {
        Ok(())
    } else {
        Err(EventError::InvalidMetadata("snapshot_kind"))
    }
}

fn require_source_kind(
    metadata: &UncheckedEventMetadata,
    expected: SourceKind,
) -> Result<(), EventError> {
    if metadata.source.kind() == expected {
        Ok(())
    } else {
        Err(EventError::InvalidMetadata("source kind"))
    }
}

/// Event payload that has been validated together with its metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct EventPayload(UncheckedEventPayload);

impl EventPayload {
    pub const fn as_unchecked(&self) -> &UncheckedEventPayload {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EventId([u8; 32]);

impl EventId {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

impl Serialize for EventId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&hex::encode(self.0))
    }
}

impl<'de> Deserialize<'de> for EventId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() != 64 {
            return Err(D::Error::custom("event id must be 32-byte lowercase hex"));
        }
        let mut bytes = [0_u8; 32];
        hex::decode_to_slice(&encoded, &mut bytes)
            .map_err(|_| D::Error::custom("event id must be 32-byte lowercase hex"))?;
        if hex::encode(bytes) != encoded {
            return Err(D::Error::custom("event id must be 32-byte lowercase hex"));
        }
        Ok(Self(bytes))
    }
}

/// A validated event with a canonical source-event identity.
///
/// Direct serde deserialization is intended only for trusted or already
/// size-bounded formats. Use [`Self::from_json_slice`] for untrusted JSON so
/// the total record is rejected before strings or collections are allocated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventEnvelope {
    id: EventId,
    event_type: EventType,
    metadata: EventMetadata,
    payload: EventPayload,
}

impl EventEnvelope {
    pub fn new(
        metadata: UncheckedEventMetadata,
        payload: UncheckedEventPayload,
    ) -> Result<Self, EventError> {
        payload.validate(&metadata)?;
        let id = canonical_event_id_unchecked(&metadata, &payload)?;
        let event_type = payload.event_type();
        let envelope = Self {
            id,
            event_type,
            metadata: EventMetadata(metadata),
            payload: EventPayload(payload),
        };
        envelope.ensure_wire_size()?;
        Ok(envelope)
    }

    pub const fn id(&self) -> EventId {
        self.id
    }

    pub const fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }

    pub const fn payload(&self) -> &EventPayload {
        &self.payload
    }

    pub const fn event_type(&self) -> EventType {
        self.event_type
    }

    pub fn verify(&self) -> Result<(), EventError> {
        if canonical_event_id_unchecked(&self.metadata.0, &self.payload.0)? == self.id {
            Ok(())
        } else {
            Err(EventError::IdentityMismatch)
        }
    }

    pub fn from_json_slice(input: &[u8]) -> Result<Self, EventError> {
        if input.len() > MAX_EVENT_WIRE_BYTES {
            return Err(EventError::WireTooLarge);
        }
        serde_json::from_slice(input).map_err(|_| EventError::InvalidWire)
    }

    fn ensure_wire_size(&self) -> Result<(), EventError> {
        let mut writer = BoundedWireCounter::default();
        match serde_json::to_writer(&mut writer, self) {
            Ok(()) => Ok(()),
            Err(_) if writer.exceeded => Err(EventError::WireTooLarge),
            Err(_) => Err(EventError::InvalidWire),
        }
    }
}

#[derive(Default)]
struct BoundedWireCounter {
    length: usize,
    exceeded: bool,
}

impl io::Write for BoundedWireCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_EVENT_WIRE_BYTES.saturating_sub(self.length) {
            self.exceeded = true;
            return Err(io::Error::other("event wire size exceeded"));
        }
        self.length += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Serialize for EventEnvelope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let metadata = &self.metadata.0;
        let mut wire = serializer.serialize_struct("EventEnvelope", 25)?;
        wire.serialize_field("event_id", &self.id)?;
        wire.serialize_field("event_type", &self.event_type)?;
        wire.serialize_field("schema_version", &metadata.schema_version)?;
        wire.serialize_field("source", &metadata.source)?;
        wire.serialize_field("venue", &metadata.venue)?;
        wire.serialize_field("instrument_id", &metadata.instrument_id)?;
        wire.serialize_field("exchange_event_time_ns", &metadata.exchange_timestamp)?;
        wire.serialize_field(
            "exchange_transaction_time_ns",
            &metadata.exchange_transaction_timestamp,
        )?;
        wire.serialize_field("receive_wall_time_ns", &metadata.receive_wall_timestamp)?;
        wire.serialize_field("receive_monotonic_time_ns", &metadata.receive_monotonic_ns)?;
        wire.serialize_field("normalization_time_ns", &metadata.normalization_timestamp)?;
        wire.serialize_field("connection_started_at", &metadata.connection_started_at)?;
        wire.serialize_field("sequence_number", &metadata.sequence_number)?;
        wire.serialize_field(
            "previous_sequence_number",
            &metadata.previous_sequence_number,
        )?;
        wire.serialize_field("connection_epoch", &metadata.connection_epoch)?;
        wire.serialize_field("subscription_epoch", &metadata.subscription_epoch)?;
        wire.serialize_field("snapshot_or_delta", &metadata.snapshot_kind)?;
        wire.serialize_field("source_checksum", &metadata.source_checksum)?;
        wire.serialize_field("raw_payload_hash", &metadata.raw_payload_hash)?;
        wire.serialize_field("parser_version", &metadata.parser_version)?;
        wire.serialize_field("normalizer_version", &metadata.normalizer_version)?;
        wire.serialize_field("ingestion_instance", &metadata.ingestion_instance)?;
        wire.serialize_field("quality_score_ppm", &metadata.quality_score_ppm)?;
        wire.serialize_field("quality_flags", &metadata.quality_flags)?;
        wire.serialize_field("payload", &self.payload.0)?;
        wire.end()
    }
}

impl<'de> Deserialize<'de> for EventEnvelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireEnvelope {
            event_id: EventId,
            event_type: EventType,
            schema_version: u32,
            source: SourceId,
            venue: Option<VenueId>,
            instrument_id: Option<InstrumentId>,
            exchange_event_time_ns: Option<UnixNanos>,
            exchange_transaction_time_ns: Option<UnixNanos>,
            receive_wall_time_ns: UnixNanos,
            receive_monotonic_time_ns: u64,
            normalization_time_ns: UnixNanos,
            connection_started_at: UnixNanos,
            sequence_number: Option<u64>,
            previous_sequence_number: Option<u64>,
            connection_epoch: u64,
            subscription_epoch: u64,
            snapshot_or_delta: SnapshotKind,
            source_checksum: Option<String>,
            raw_payload_hash: [u8; 32],
            parser_version: String,
            normalizer_version: String,
            ingestion_instance: String,
            quality_score_ppm: u32,
            quality_flags: QualityFlags,
            payload: UncheckedEventPayload,
        }

        let wire = WireEnvelope::deserialize(deserializer)?;
        let metadata = UncheckedEventMetadata {
            schema_version: wire.schema_version,
            source: wire.source,
            venue: wire.venue,
            instrument_id: wire.instrument_id,
            exchange_timestamp: wire.exchange_event_time_ns,
            exchange_transaction_timestamp: wire.exchange_transaction_time_ns,
            receive_wall_timestamp: wire.receive_wall_time_ns,
            receive_monotonic_ns: wire.receive_monotonic_time_ns,
            normalization_timestamp: wire.normalization_time_ns,
            connection_started_at: wire.connection_started_at,
            sequence_number: wire.sequence_number,
            previous_sequence_number: wire.previous_sequence_number,
            connection_epoch: wire.connection_epoch,
            subscription_epoch: wire.subscription_epoch,
            snapshot_kind: wire.snapshot_or_delta,
            source_checksum: wire.source_checksum,
            raw_payload_hash: wire.raw_payload_hash,
            parser_version: wire.parser_version,
            normalizer_version: wire.normalizer_version,
            ingestion_instance: wire.ingestion_instance,
            quality_score_ppm: wire.quality_score_ppm,
            quality_flags: wire.quality_flags,
        };
        let envelope = Self::new(metadata, wire.payload).map_err(D::Error::custom)?;
        if envelope.event_type != wire.event_type {
            return Err(D::Error::custom(EventError::EventTypeMismatch));
        }
        if envelope.id != wire.event_id {
            return Err(D::Error::custom(EventError::IdentityMismatch));
        }
        Ok(envelope)
    }
}

/// Computes the versioned canonical source-event identity.
///
/// Identity includes source and instrument identity, source event/transaction
/// times, sequence/session semantics, event type, and the complete canonical
/// payload. Local receive/normalization times, parser lineage, raw storage
/// evidence, and quality annotations are deliberately excluded so replaying
/// the same source event does not fragment deduplication. This accepts
/// unchecked wire components so identity behavior can be audited; runtime code
/// should construct [`EventEnvelope`] instead.
pub fn canonical_event_id_unchecked(
    metadata: &UncheckedEventMetadata,
    payload: &UncheckedEventPayload,
) -> Result<EventId, EventError> {
    let mut writer = CanonicalWriter::new();
    encode_identity_metadata(&mut writer, metadata)?;
    encode_payload(&mut writer, payload)?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(EVENT_DOMAIN);
    hasher.update(writer.as_bytes());
    Ok(EventId(*hasher.finalize().as_bytes()))
}

fn validate_book(
    bids: &[BookLevel],
    asks: &[BookLevel],
    allow_zero_quantity: bool,
    enforce_uncrossed: bool,
) -> Result<(), EventError> {
    if bids.len().saturating_add(asks.len()) > MAX_BOOK_LEVELS {
        return Err(EventError::InvalidPayload("book level capacity"));
    }
    if !allow_zero_quantity
        && bids
            .iter()
            .chain(asks)
            .any(|level| level.quantity.value().is_zero())
    {
        return Err(EventError::InvalidPayload("zero snapshot quantity"));
    }
    if !bids.windows(2).all(|pair| pair[0].price > pair[1].price)
        || !asks.windows(2).all(|pair| pair[0].price < pair[1].price)
    {
        return Err(EventError::InvalidPayload("book ordering"));
    }
    if enforce_uncrossed
        && bids
            .first()
            .zip(asks.first())
            .is_some_and(|(bid, ask)| bid.price >= ask.price)
    {
        return Err(EventError::InvalidPayload("crossed book"));
    }
    Ok(())
}

fn encode_identity_metadata(
    writer: &mut CanonicalWriter,
    metadata: &UncheckedEventMetadata,
) -> Result<(), EventError> {
    writer.u8(metadata.source.kind() as u8);
    writer.string(metadata.source.name())?;
    writer.u32(metadata.source.generation());
    writer.optional(metadata.venue.as_ref(), |writer, venue| {
        writer.string(venue.as_str())
    })?;
    writer.optional(metadata.instrument_id.as_ref(), |writer, instrument| {
        writer.string(instrument.venue().as_str())?;
        writer.string(instrument.venue_symbol())?;
        writer.u32(instrument.generation());
        Ok(())
    })?;
    writer.optional(metadata.exchange_timestamp, |writer, timestamp| {
        writer.i64(timestamp.value());
        Ok(())
    })?;
    writer.optional(
        metadata.exchange_transaction_timestamp,
        |writer, timestamp| {
            writer.i64(timestamp.value());
            Ok(())
        },
    )?;
    writer.optional(metadata.sequence_number, |writer, sequence| {
        writer.u64(sequence);
        Ok(())
    })?;
    Ok(())
}

fn encode_payload(
    writer: &mut CanonicalWriter,
    payload: &UncheckedEventPayload,
) -> Result<(), EventError> {
    match payload {
        UncheckedEventPayload::Trade(trade) => {
            writer.u8(0);
            writer.string(&trade.trade_id)?;
            writer.decimal(trade.price.value())?;
            writer.decimal(trade.quantity.value())?;
            writer.u8(trade.side as u8);
        }
        UncheckedEventPayload::TopOfBook(value) => {
            writer.u8(1);
            writer.optional(value.bid.as_ref(), CanonicalWriter::level)?;
            writer.optional(value.ask.as_ref(), CanonicalWriter::level)?;
        }
        UncheckedEventPayload::BookSnapshot(snapshot) => {
            writer.u8(2);
            writer.levels(&snapshot.bids)?;
            writer.levels(&snapshot.asks)?;
            writer.u64(snapshot.last_sequence);
        }
        UncheckedEventPayload::BookDelta(delta) => {
            writer.u8(3);
            writer.levels(&delta.bids)?;
            writer.levels(&delta.asks)?;
            writer.u64(delta.first_sequence);
            writer.u64(delta.last_sequence);
        }
        UncheckedEventPayload::InstrumentDefinition(value) => {
            writer.u8(4);
            writer.instrument_definition(value)?;
        }
        UncheckedEventPayload::FundingObservation(value) => {
            writer.u8(5);
            writer.decimal(value.funding_rate.value())?;
            writer.i64(value.observed_at.value());
            writer.optional(value.next_funding_time, |writer, time| {
                writer.i64(time.value());
                Ok(())
            })?;
        }
        UncheckedEventPayload::OpenInterestObservation(value) => {
            writer.u8(6);
            writer.decimal(value.quantity.value())?;
            writer.optional(value.quote_notional, |writer, notional| {
                writer.decimal(notional.value())
            })?;
            writer.i64(value.observed_at.value());
        }
        UncheckedEventPayload::LiquidationObservation(value) => {
            writer.u8(7);
            writer.string(&value.liquidation_id)?;
            writer.decimal(value.price.value())?;
            writer.decimal(value.quantity.value())?;
            writer.u8(value.side as u8);
        }
        UncheckedEventPayload::MarkIndexObservation(value) => {
            writer.u8(8);
            writer.decimal(value.mark_price.value())?;
            writer.decimal(value.index_price.value())?;
        }
        UncheckedEventPayload::FutureBasisObservation(value) => {
            writer.u8(9);
            writer.decimal(value.future_price.value())?;
            writer.decimal(value.reference_price.value())?;
            writer.decimal(value.basis_rate.value())?;
            writer.optional(value.annualized_basis_rate, |writer, rate| {
                writer.decimal(rate.value())
            })?;
        }
        UncheckedEventPayload::OptionTicker(value) => {
            writer.u8(10);
            writer.optional(value.bid_price, |writer, price| {
                writer.decimal(price.value())
            })?;
            writer.optional(value.ask_price, |writer, price| {
                writer.decimal(price.value())
            })?;
            writer.decimal(value.mark_price.value())?;
            writer.decimal(value.mark_iv.value())?;
            writer.decimal(value.delta.value())?;
            writer.decimal(value.open_interest.value())?;
        }
        UncheckedEventPayload::OptionTrade(value) => {
            writer.u8(11);
            writer.string(&value.trade_id)?;
            writer.decimal(value.price.value())?;
            writer.decimal(value.quantity.value())?;
            writer.u8(value.side as u8);
            writer.optional(value.implied_volatility, |writer, rate| {
                writer.decimal(rate.value())
            })?;
        }
        UncheckedEventPayload::VenueStatus(value) => {
            writer.u8(12);
            writer.u8(value.state as u8);
            writer.i64(value.observed_at.value());
            writer.optional(value.message.as_deref(), |writer, message| {
                writer.string(message)
            })?;
        }
        UncheckedEventPayload::ChainBlock(value) => {
            writer.u8(13);
            writer.asset(&value.chain)?;
            writer.string(&value.block_hash)?;
            writer.optional(value.parent_hash.as_deref(), |writer, hash| {
                writer.string(hash)
            })?;
            writer.u64(value.height);
            writer.i64(value.block_time.value());
            writer.u64(value.confirmation_depth);
            writer.u8(value.finality as u8);
            writer.u64(value.transaction_count);
        }
        UncheckedEventPayload::ChainTransactionAggregate(value) => {
            writer.u8(14);
            writer.asset(&value.chain)?;
            writer.i64(value.interval_start.value());
            writer.i64(value.interval_end.value());
            writer.u64(value.transaction_count);
            writer.optional(value.transferred_value, |writer, notional| {
                writer.decimal(notional.value())
            })?;
            writer.optional(value.fee_total, |writer, notional| {
                writer.decimal(notional.value())
            })?;
        }
        UncheckedEventPayload::ChainMempoolObservation(value) => {
            writer.u8(15);
            writer.asset(&value.chain)?;
            writer.i64(value.observed_at.value());
            writer.u64(value.transaction_count);
            writer.u64(value.virtual_size);
            writer.optional(value.total_fees, |writer, notional| {
                writer.decimal(notional.value())
            })?;
        }
        UncheckedEventPayload::ChainMetric(value) => {
            writer.u8(16);
            writer.asset(&value.chain)?;
            writer.string(&value.metric_name)?;
            writer.decimal(value.value)?;
            writer.i64(value.observed_at.value());
        }
        UncheckedEventPayload::ExternalMarketObservation(value) => {
            writer.u8(17);
            writer.string(&value.market_id)?;
            writer.decimal(value.value)?;
            writer.i64(value.observed_at.value());
            writer.optional(value.source_native_id.as_deref(), |writer, id| {
                writer.string(id)
            })?;
        }
        UncheckedEventPayload::StructuredEvent(value) => {
            writer.u8(18);
            writer.string(&value.structured_event_id)?;
            writer.string(&value.event_type)?;
            writer.assets(&value.affected_assets)?;
            writer.venues(&value.affected_venues)?;
            writer.strings(&value.affected_protocols)?;
            writer.source(&value.source_identity)?;
            writer.u32(value.source_reliability_ppm);
            writer.i64(value.publication_time.value());
            writer.optional(value.effective_time, |writer, time| {
                writer.i64(time.value());
                Ok(())
            })?;
            writer.optional(value.expected_end_time, |writer, time| {
                writer.i64(time.value());
                Ok(())
            })?;
            writer.decimal(value.direction_prior.value())?;
            writer.u32(value.severity_ppm);
            writer.u32(value.extraction_confidence_ppm);
            writer.boolean(value.human_verified);
            writer.bytes(&value.source_document_hash)?;
            writer.string(&value.extractor_model_id)?;
            writer.string(&value.extractor_prompt_version)?;
            writer.u32(
                u32::try_from(value.extracted_facts.len())
                    .map_err(|_| EventError::CanonicalFieldTooLarge)?,
            );
            for fact in &value.extracted_facts {
                writer.string(&fact.key)?;
                writer.string(&fact.value)?;
            }
        }
        UncheckedEventPayload::DataQualityObservation(value) => {
            writer.u8(19);
            writer.string(&value.component)?;
            writer.i64(value.observed_at.value());
            writer.u32(value.score_ppm);
            writer.u64(value.flags.bits());
            writer.optional(value.details.as_deref(), |writer, details| {
                writer.string(details)
            })?;
        }
        UncheckedEventPayload::PredictionRecord(value) => {
            writer.u8(20);
            writer.string(&value.forecast_id)?;
            writer.i64(value.issued_at.value());
            writer.string(&value.entity_id)?;
            writer.string(&value.model_package_id)?;
            writer.u32(value.forecast_schema_version);
            writer.string(&value.prediction_event_type)?;
            writer.u64(value.horizon_seconds);
            writer.u32(value.calibrated_probability_ppm);
            writer.decimal(value.raw_score.value())?;
            writer.u32(value.base_rate_ppm);
            writer.u32(value.lower_uncertainty_ppm);
            writer.u32(value.upper_uncertainty_ppm);
            writer.u8(value.availability as u8);
            writer.u32(value.quality_score_ppm);
            writer.u32(value.applicability_score_ppm);
            writer.string(&value.scenario_summary_id)?;
            writer.string(&value.evidence_bundle_id)?;
            writer.optional(value.supersedes_forecast_id.as_deref(), |writer, id| {
                writer.string(id)
            })?;
        }
        UncheckedEventPayload::AlertEvent(value) => {
            writer.u8(21);
            writer.string(&value.alert_id)?;
            writer.string(&value.rule_id)?;
            writer.u8(value.status as u8);
            writer.u8(value.priority);
            writer.i64(value.triggered_at.value());
            writer.optional(value.recovered_at, |writer, time| {
                writer.i64(time.value());
                Ok(())
            })?;
            writer.optional(value.forecast_id.as_deref(), |writer, id| writer.string(id))?;
            writer.string(&value.message)?;
        }
    }
    Ok(())
}

struct CanonicalWriter {
    bytes: Vec<u8>,
}

impl CanonicalWriter {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn boolean(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), EventError> {
        self.u32(u32::try_from(value.len()).map_err(|_| EventError::CanonicalFieldTooLarge)?);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn string(&mut self, value: &str) -> Result<(), EventError> {
        self.bytes(value.as_bytes())
    }

    fn decimal(&mut self, value: FixedDecimal) -> Result<(), EventError> {
        self.string(&value.to_string())
    }

    fn source(&mut self, value: &SourceId) -> Result<(), EventError> {
        self.u8(value.kind() as u8);
        self.string(value.name())?;
        self.u32(value.generation());
        Ok(())
    }

    fn venue(&mut self, value: &VenueId) -> Result<(), EventError> {
        self.string(value.as_str())
    }

    fn asset(&mut self, value: &domain::AssetId) -> Result<(), EventError> {
        self.u8(value.namespace() as u8);
        self.string(value.chain_id())?;
        self.string(value.contract_or_mint())?;
        self.string(value.canonical_symbol())?;
        self.u32(value.generation());
        Ok(())
    }

    fn instrument_definition(&mut self, value: &InstrumentDefinition) -> Result<(), EventError> {
        self.venue(value.id().venue())?;
        self.string(value.id().venue_symbol())?;
        self.u32(value.id().generation());
        self.u8(value.product_type() as u8);
        self.asset(value.base_asset())?;
        self.asset(value.quote_asset())?;
        self.asset(value.settlement_asset())?;
        self.decimal(value.contract_multiplier())?;
        self.u8(value.contract_value_unit() as u8);
        self.u8(value.contract_kind() as u8);
        self.optional(value.expiry_time(), |writer, time| {
            writer.i64(time.value());
            Ok(())
        })?;
        self.optional(value.strike(), |writer, price| {
            writer.decimal(price.value())
        })?;
        self.optional(value.option_side(), |writer, side| {
            writer.u8(side as u8);
            Ok(())
        })?;
        self.decimal(value.price_tick().value())?;
        self.decimal(value.quantity_step().value())?;
        self.i64(value.listing_time().value());
        self.optional(value.delisting_time(), |writer, time| {
            writer.i64(time.value());
            Ok(())
        })
    }

    fn assets(&mut self, values: &[domain::AssetId]) -> Result<(), EventError> {
        self.u32(u32::try_from(values.len()).map_err(|_| EventError::CanonicalFieldTooLarge)?);
        for value in values {
            self.asset(value)?;
        }
        Ok(())
    }

    fn venues(&mut self, values: &[VenueId]) -> Result<(), EventError> {
        self.u32(u32::try_from(values.len()).map_err(|_| EventError::CanonicalFieldTooLarge)?);
        for value in values {
            self.venue(value)?;
        }
        Ok(())
    }

    fn strings(&mut self, values: &[String]) -> Result<(), EventError> {
        self.u32(u32::try_from(values.len()).map_err(|_| EventError::CanonicalFieldTooLarge)?);
        for value in values {
            self.string(value)?;
        }
        Ok(())
    }

    fn optional<T>(
        &mut self,
        value: Option<T>,
        encode: impl FnOnce(&mut Self, T) -> Result<(), EventError>,
    ) -> Result<(), EventError> {
        match value {
            Some(value) => {
                self.u8(1);
                encode(self, value)
            }
            None => {
                self.u8(0);
                Ok(())
            }
        }
    }

    fn levels(&mut self, levels: &[BookLevel]) -> Result<(), EventError> {
        self.u32(u32::try_from(levels.len()).map_err(|_| EventError::CanonicalFieldTooLarge)?);
        for level in levels {
            self.decimal(level.price.value())?;
            self.decimal(level.quantity.value())?;
            self.optional(level.order_count, |writer, count| {
                writer.u32(count);
                Ok(())
            })?;
        }
        Ok(())
    }

    fn level(&mut self, level: &BookLevel) -> Result<(), EventError> {
        self.decimal(level.price.value())?;
        self.decimal(level.quantity.value())?;
        self.optional(level.order_count, |writer, count| {
            writer.u32(count);
            Ok(())
        })
    }
}
