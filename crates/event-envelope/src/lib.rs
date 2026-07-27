//! Validated market events with complete, deterministic BLAKE3 identities.

use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

const EVENT_DOMAIN: &[u8] = b"cmti:event:v1\0";
const MAX_BOOK_LEVELS: usize = 100_000;

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
    #[error("canonical event field is too large")]
    CanonicalFieldTooLarge,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    NotApplicable,
    Snapshot,
    Delta,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QualityFlags(u64);

impl QualityFlags {
    pub const NONE: Self = Self(0);
    pub const STALE: Self = Self(1 << 0);
    pub const PARTIAL: Self = Self(1 << 1);
    pub const CLOCK_UNCERTAIN: Self = Self(1 << 2);
    pub const SOURCE_DEGRADED: Self = Self(1 << 3);

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventMetadata {
    pub schema_version: u32,
    pub source: SourceId,
    pub venue: Option<VenueId>,
    pub instrument_id: Option<InstrumentId>,
    pub exchange_timestamp: Option<UnixNanos>,
    pub receive_wall_timestamp: UnixNanos,
    pub receive_monotonic_ns: u64,
    pub connection_started_at: UnixNanos,
    pub sequence_number: Option<u64>,
    pub connection_epoch: u64,
    pub snapshot_kind: SnapshotKind,
    pub source_checksum: Option<String>,
    pub raw_payload_hash: [u8; 32],
    pub normalizer_version: String,
    pub ingestion_instance: String,
    pub quality_score_ppm: u32,
    pub quality_flags: QualityFlags,
}

impl EventMetadata {
    pub fn validate(&self) -> Result<(), EventError> {
        if self.schema_version == 0 {
            return Err(EventError::InvalidMetadata("schema_version"));
        }
        if self.connection_epoch == 0 {
            return Err(EventError::InvalidMetadata("connection_epoch"));
        }
        if self.receive_wall_timestamp < self.connection_started_at {
            return Err(EventError::InvalidMetadata("receive_wall_timestamp"));
        }
        if self.raw_payload_hash == [0; 32] {
            return Err(EventError::InvalidMetadata("raw_payload_hash"));
        }
        if self.normalizer_version.is_empty() {
            return Err(EventError::InvalidMetadata("normalizer_version"));
        }
        if self.ingestion_instance.is_empty() {
            return Err(EventError::InvalidMetadata("ingestion_instance"));
        }
        if self.quality_score_ppm > 1_000_000 {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
    Unknown,
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
pub enum EventPayload {
    Trade(Trade),
    BookSnapshot(BookSnapshot),
    BookDelta(BookDelta),
}

impl EventPayload {
    fn validate(&self, metadata: &EventMetadata) -> Result<(), EventError> {
        metadata.validate()?;
        if metadata.venue.is_none() || metadata.instrument_id.is_none() {
            return Err(EventError::InvalidMetadata("instrument identity"));
        }

        match self {
            Self::Trade(trade) => {
                if trade.trade_id.is_empty() || trade.quantity.value().is_zero() {
                    return Err(EventError::InvalidPayload("trade"));
                }
                if metadata.snapshot_kind != SnapshotKind::NotApplicable {
                    return Err(EventError::InvalidMetadata("snapshot_kind"));
                }
            }
            Self::BookSnapshot(snapshot) => {
                validate_book(&snapshot.bids, &snapshot.asks, false)?;
                if metadata.snapshot_kind != SnapshotKind::Snapshot
                    || metadata.sequence_number != Some(snapshot.last_sequence)
                {
                    return Err(EventError::SequenceMismatch);
                }
            }
            Self::BookDelta(delta) => {
                validate_book(&delta.bids, &delta.asks, true)?;
                if delta.first_sequence > delta.last_sequence
                    || metadata.snapshot_kind != SnapshotKind::Delta
                    || metadata.sequence_number != Some(delta.last_sequence)
                {
                    return Err(EventError::SequenceMismatch);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct EventEnvelope {
    id: EventId,
    metadata: EventMetadata,
    payload: EventPayload,
}

impl EventEnvelope {
    pub fn new(metadata: EventMetadata, payload: EventPayload) -> Result<Self, EventError> {
        payload.validate(&metadata)?;
        let id = canonical_event_id(&metadata, &payload)?;
        Ok(Self {
            id,
            metadata,
            payload,
        })
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

    pub fn verify(&self) -> Result<(), EventError> {
        if canonical_event_id(&self.metadata, &self.payload)? == self.id {
            Ok(())
        } else {
            Err(EventError::IdentityMismatch)
        }
    }
}

/// Computes the versioned canonical identity over every metadata and payload
/// field. Validation is separate so mutation-sensitivity can be tested field by
/// field, including mutations that would make an envelope invalid.
pub fn canonical_event_id(
    metadata: &EventMetadata,
    payload: &EventPayload,
) -> Result<EventId, EventError> {
    let mut writer = CanonicalWriter::new();
    encode_metadata(&mut writer, metadata)?;
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
    if bids
        .first()
        .zip(asks.first())
        .is_some_and(|(bid, ask)| bid.price >= ask.price)
    {
        return Err(EventError::InvalidPayload("crossed book"));
    }
    Ok(())
}

fn encode_metadata(
    writer: &mut CanonicalWriter,
    metadata: &EventMetadata,
) -> Result<(), EventError> {
    writer.u32(metadata.schema_version);
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
    writer.i64(metadata.receive_wall_timestamp.value());
    writer.u64(metadata.receive_monotonic_ns);
    writer.i64(metadata.connection_started_at.value());
    writer.optional(metadata.sequence_number, |writer, sequence| {
        writer.u64(sequence);
        Ok(())
    })?;
    writer.u64(metadata.connection_epoch);
    writer.u8(metadata.snapshot_kind as u8);
    writer.optional(metadata.source_checksum.as_deref(), |writer, checksum| {
        writer.string(checksum)
    })?;
    writer.bytes(&metadata.raw_payload_hash)?;
    writer.string(&metadata.normalizer_version)?;
    writer.string(&metadata.ingestion_instance)?;
    writer.u32(metadata.quality_score_ppm);
    writer.u64(metadata.quality_flags.bits());
    Ok(())
}

fn encode_payload(writer: &mut CanonicalWriter, payload: &EventPayload) -> Result<(), EventError> {
    match payload {
        EventPayload::Trade(trade) => {
            writer.u8(0);
            writer.string(&trade.trade_id)?;
            writer.decimal(trade.price.value())?;
            writer.decimal(trade.quantity.value())?;
            writer.u8(trade.side as u8);
        }
        EventPayload::BookSnapshot(snapshot) => {
            writer.u8(1);
            writer.levels(&snapshot.bids)?;
            writer.levels(&snapshot.asks)?;
            writer.u64(snapshot.last_sequence);
        }
        EventPayload::BookDelta(delta) => {
            writer.u8(2);
            writer.levels(&delta.bids)?;
            writer.levels(&delta.asks)?;
            writer.u64(delta.first_sequence);
            writer.u64(delta.last_sequence);
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
}
