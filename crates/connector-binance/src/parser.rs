use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
use event_envelope::{
    BookDelta, BookLevel, BookSnapshot, EventEnvelope, QualityFlags, SnapshotKind,
    UncheckedEventMetadata, UncheckedEventPayload,
};
use fixed_decimal::{DecimalError, FixedDecimal, Price, Quantity};
use serde::Deserialize;
use thiserror::Error;

pub const EXPECTED_SOURCE: &str = "binance-fixture";
pub const EXPECTED_SYMBOL: &str = "BTCUSDT";
pub const EXPECTED_GENERATION: u32 = 1;

const VENUE: &str = "binance";
const PARSER_VERSION: &str = "binance-fixture-parser-v1";
const NORMALIZER_VERSION: &str = "binance-fixture-v1";
const INGESTION_INSTANCE: &str = "offline-fixture";

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed fixture JSON")]
    MalformedJson,
    #[error("fixture identity does not match BTCUSDT generation 1")]
    Identity,
    #[error("fixture sequence is invalid")]
    Sequence,
    #[error("fixture timestamp is invalid")]
    Timestamp,
    #[error("fixture monetary value is invalid")]
    Decimal,
    #[error("normalized fixture event is invalid")]
    Event,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireMessage {
    Snapshot {
        source: String,
        symbol: String,
        generation: u32,
        event_unix_nanos: i64,
        sequence: u64,
        bids: Vec<[String; 2]>,
        asks: Vec<[String; 2]>,
    },
    Delta {
        source: String,
        symbol: String,
        generation: u32,
        event_unix_nanos: i64,
        first_sequence: u64,
        last_sequence: u64,
        bids: Vec<[String; 2]>,
        asks: Vec<[String; 2]>,
    },
}

pub fn parse_fixture_line(raw: &[u8]) -> Result<EventEnvelope, ParseError> {
    let wire = serde_json::from_slice::<WireMessage>(raw).map_err(|_| ParseError::MalformedJson)?;
    match wire {
        WireMessage::Snapshot {
            source,
            symbol,
            generation,
            event_unix_nanos,
            sequence,
            bids,
            asks,
        } => {
            validate_identity(&source, &symbol, generation)?;
            if sequence == 0 {
                return Err(ParseError::Sequence);
            }
            let payload = UncheckedEventPayload::BookSnapshot(BookSnapshot {
                bids: parse_levels(bids)?,
                asks: parse_levels(asks)?,
                last_sequence: sequence,
            });
            build_envelope(
                raw,
                event_unix_nanos,
                sequence,
                SnapshotKind::Snapshot,
                payload,
            )
        }
        WireMessage::Delta {
            source,
            symbol,
            generation,
            event_unix_nanos,
            first_sequence,
            last_sequence,
            bids,
            asks,
        } => {
            validate_identity(&source, &symbol, generation)?;
            if first_sequence == 0
                || last_sequence == 0
                || first_sequence > last_sequence
                || bids.is_empty() && asks.is_empty()
            {
                return Err(ParseError::Sequence);
            }
            let payload = UncheckedEventPayload::BookDelta(BookDelta {
                bids: parse_levels(bids)?,
                asks: parse_levels(asks)?,
                first_sequence,
                last_sequence,
            });
            build_envelope(
                raw,
                event_unix_nanos,
                last_sequence,
                SnapshotKind::Delta,
                payload,
            )
        }
    }
}

fn validate_identity(source: &str, symbol: &str, generation: u32) -> Result<(), ParseError> {
    if source != EXPECTED_SOURCE || symbol != EXPECTED_SYMBOL || generation != EXPECTED_GENERATION {
        Err(ParseError::Identity)
    } else {
        Ok(())
    }
}

fn parse_levels(levels: Vec<[String; 2]>) -> Result<Vec<BookLevel>, ParseError> {
    levels
        .into_iter()
        .map(|[price, quantity]| {
            let price = Price::new(parse_decimal(&price)?).map_err(map_decimal)?;
            let quantity = Quantity::new(parse_decimal(&quantity)?).map_err(map_decimal)?;
            Ok(BookLevel {
                price,
                quantity,
                order_count: None,
            })
        })
        .collect()
}

fn parse_decimal(value: &str) -> Result<FixedDecimal, ParseError> {
    FixedDecimal::parse_canonical(value).map_err(map_decimal)
}

fn map_decimal(_error: DecimalError) -> ParseError {
    ParseError::Decimal
}

fn build_envelope(
    raw: &[u8],
    event_unix_nanos: i64,
    sequence: u64,
    snapshot_kind: SnapshotKind,
    payload: UncheckedEventPayload,
) -> Result<EventEnvelope, ParseError> {
    if event_unix_nanos <= 0 {
        return Err(ParseError::Timestamp);
    }
    let venue = VenueId::new(VENUE).map_err(|_| ParseError::Identity)?;
    let source = SourceId::new(SourceKind::Exchange, EXPECTED_SOURCE, EXPECTED_GENERATION)
        .map_err(|_| ParseError::Identity)?;
    let instrument = InstrumentId::new(venue.clone(), EXPECTED_SYMBOL, EXPECTED_GENERATION)
        .map_err(|_| ParseError::Identity)?;
    let timestamp = UnixNanos::new(event_unix_nanos);
    let previous_sequence_number = match &payload {
        UncheckedEventPayload::BookDelta(delta) => delta.first_sequence.checked_sub(1),
        _ => None,
    };
    let metadata = UncheckedEventMetadata {
        schema_version: 1,
        source,
        venue: Some(venue),
        instrument_id: Some(instrument),
        exchange_timestamp: Some(timestamp),
        exchange_transaction_timestamp: None,
        receive_wall_timestamp: timestamp,
        receive_monotonic_ns: sequence,
        normalization_timestamp: timestamp,
        connection_started_at: timestamp,
        sequence_number: Some(sequence),
        previous_sequence_number,
        connection_epoch: 1,
        subscription_epoch: 1,
        snapshot_kind,
        source_checksum: None,
        raw_payload_hash: *blake3::hash(raw).as_bytes(),
        parser_version: PARSER_VERSION.to_owned(),
        normalizer_version: NORMALIZER_VERSION.to_owned(),
        ingestion_instance: INGESTION_INSTANCE.to_owned(),
        quality_score_ppm: 1_000_000,
        quality_flags: QualityFlags::NONE,
    };
    EventEnvelope::new(metadata, payload).map_err(|_| ParseError::Event)
}
