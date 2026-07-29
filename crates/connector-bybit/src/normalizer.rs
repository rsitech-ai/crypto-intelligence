use std::cmp::Reverse;
use std::num::NonZeroU64;

use connector_core::DurableRawReference;
use domain::{InstrumentId, ProductType, SourceKind, UnixNanos, VenueId};
use event_envelope::{
    BookDelta, BookLevel, BookSnapshot, EventEnvelope, EventError, FundingObservation,
    LiquidationObservation, MarkIndexObservation, OpenInterestObservation, QualityFlags,
    SnapshotKind, Trade, UncheckedEventMetadata, UncheckedEventPayload,
};
use fixed_decimal::Notional;
use instrument_registry::{CatalogSnapshot, ResolveError};
use thiserror::Error;

use crate::{
    AllLiquidation, BookMessageKind, BybitMarket, BybitMessage, DurableBybitMessage, LinearTicker,
    NativeBookLevel, OrderBookMessage, PublicTrade,
};

const EVENT_SCHEMA_VERSION: u32 = 3;
const PARSER_VERSION: &str = "bybit-v5-native-v1";
const NORMALIZER_VERSION: &str = "bybit-v5-normalizer-v1";
const BYBIT_VENUE: &str = "bybit";
const MAX_INGESTION_INSTANCE_BYTES: usize = 256;
const LIQUIDATION_ID_DOMAIN: &[u8] = b"cmti:bybit:all-liquidation:v1\0";

#[derive(Clone, Copy, Debug)]
pub struct NormalizationContext<'a> {
    catalog: &'a CatalogSnapshot,
    raw: &'a DurableRawReference,
    receive_wall_timestamp: UnixNanos,
    receive_monotonic_ns: u64,
    normalization_timestamp: UnixNanos,
    connection_started_at: UnixNanos,
    subscription_epoch: NonZeroU64,
    ingestion_instance: &'a str,
}

impl<'a> NormalizationContext<'a> {
    /// Builds replay-stable event lineage from a verified raw WAL reference.
    pub fn try_new(
        catalog: &'a CatalogSnapshot,
        raw: &'a DurableRawReference,
        normalization_timestamp: UnixNanos,
        connection_started_at: UnixNanos,
        subscription_epoch: NonZeroU64,
        ingestion_instance: &'a str,
    ) -> Result<Self, NormalizationError> {
        if raw.source().kind() != SourceKind::Exchange || raw.source().name() != BYBIT_VENUE {
            return Err(NormalizationError::InvalidContext("raw source"));
        }
        let receive_wall_timestamp = raw.receive_wall_time();
        let receive_monotonic_ns = raw.receive_monotonic_ns();
        if receive_wall_timestamp < connection_started_at {
            return Err(NormalizationError::InvalidContext(
                "connection start ordering",
            ));
        }
        if normalization_timestamp < receive_wall_timestamp {
            return Err(NormalizationError::InvalidContext(
                "normalization time ordering",
            ));
        }
        if ingestion_instance.is_empty()
            || ingestion_instance.len() > MAX_INGESTION_INSTANCE_BYTES
            || ingestion_instance.trim() != ingestion_instance
            || ingestion_instance.chars().any(char::is_control)
        {
            return Err(NormalizationError::InvalidContext("ingestion instance"));
        }
        if !catalog.verify_integrity() {
            return Err(NormalizationError::InvalidContext("catalog integrity"));
        }
        Ok(Self {
            catalog,
            raw,
            receive_wall_timestamp,
            receive_monotonic_ns,
            normalization_timestamp,
            connection_started_at,
            subscription_epoch,
            ingestion_instance,
        })
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum NormalizationError {
    #[error("invalid normalization context: {0}")]
    InvalidContext(&'static str),
    #[error("instrument resolution failed: {0}")]
    InstrumentResolution(#[from] ResolveError),
    #[error("book update contains duplicate price levels")]
    DuplicateBookLevel,
    #[error("event envelope validation failed: {0}")]
    Event(#[from] EventError),
    #[error("parsed message does not match the normalization raw reference")]
    RawPayloadMismatch,
    #[error("open-interest quote notional is invalid")]
    InvalidOpenInterestNotional,
}

pub fn normalize_native_message(
    message: DurableBybitMessage,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let (message, raw_payload_hash) = message.into_parts();
    if &raw_payload_hash != context.raw.payload_hash() {
        return Err(NormalizationError::RawPayloadMismatch);
    }
    match message {
        BybitMessage::OrderBook(value) => normalize_orderbook(value, context),
        BybitMessage::PublicTrades(values) => normalize_trades(values, context),
        BybitMessage::LinearTicker(value) => normalize_linear_ticker(value, context),
        BybitMessage::AllLiquidations(values) => normalize_liquidations(values, context),
    }
}

fn normalize_orderbook(
    value: OrderBookMessage,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let instrument = resolve_instrument(
        context,
        &value.symbol,
        product_type(value.market),
        value.event_time,
    )?;
    let snapshot_kind = match value.kind {
        BookMessageKind::Snapshot => SnapshotKind::Snapshot,
        BookMessageKind::Delta => SnapshotKind::Delta,
    };
    let metadata = metadata(
        context,
        instrument,
        Some(value.event_time),
        Some(value.matching_engine_time),
        Some(value.update_id),
        match value.kind {
            BookMessageKind::Snapshot => None,
            BookMessageKind::Delta => value.update_id.checked_sub(1),
        },
        snapshot_kind,
        QualityFlags::NONE,
    );
    let bids = normalized_levels(value.bids, true)?;
    let asks = normalized_levels(value.asks, false)?;
    let payload = match value.kind {
        BookMessageKind::Snapshot => UncheckedEventPayload::BookSnapshot(BookSnapshot {
            bids,
            asks,
            last_sequence: value.update_id,
        }),
        BookMessageKind::Delta => UncheckedEventPayload::BookDelta(BookDelta {
            bids,
            asks,
            first_sequence: value.update_id,
            last_sequence: value.update_id,
        }),
    };
    Ok(vec![EventEnvelope::new(metadata, payload)?])
}

fn normalize_trades(
    values: Vec<PublicTrade>,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    values
        .into_iter()
        .map(|value| {
            let instrument = resolve_instrument(
                context,
                &value.symbol,
                product_type(value.market),
                value.event_time,
            )?;
            let metadata = metadata(
                context,
                instrument,
                Some(value.event_time),
                Some(value.trade_time),
                Some(value.cross_sequence),
                None,
                SnapshotKind::NotApplicable,
                QualityFlags::NONE,
            );
            let payload = UncheckedEventPayload::Trade(Trade {
                trade_id: value.trade_id,
                price: value.price,
                quantity: value.quantity,
                side: value.side,
            });
            Ok(EventEnvelope::new(metadata, payload)?)
        })
        .collect()
}

fn normalize_linear_ticker(
    value: LinearTicker,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let instrument = resolve_instrument(
        context,
        &value.symbol,
        ProductType::Perpetual,
        value.event_time,
    )?;
    let base = metadata(
        context,
        instrument,
        Some(value.event_time),
        None,
        Some(value.cross_sequence),
        None,
        SnapshotKind::NotApplicable,
        QualityFlags::NONE,
    );
    let mark = EventEnvelope::new(
        base.clone(),
        UncheckedEventPayload::MarkIndexObservation(MarkIndexObservation {
            mark_price: value.mark_price,
            index_price: value.index_price,
        }),
    )?;
    let funding = EventEnvelope::new(
        base.clone(),
        UncheckedEventPayload::FundingObservation(FundingObservation {
            funding_rate: value.funding_rate,
            observed_at: value.event_time,
            next_funding_time: Some(value.next_funding_time),
        }),
    )?;
    let quote_notional = Notional::new(value.open_interest_value)
        .map_err(|_| NormalizationError::InvalidOpenInterestNotional)?;
    let open_interest = EventEnvelope::new(
        base,
        UncheckedEventPayload::OpenInterestObservation(OpenInterestObservation {
            quantity: value.open_interest,
            quote_notional: Some(quote_notional),
            observed_at: value.event_time,
        }),
    )?;
    Ok(vec![mark, funding, open_interest])
}

fn normalize_liquidations(
    values: Vec<AllLiquidation>,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let instrument = resolve_instrument(
                context,
                &value.symbol,
                ProductType::Perpetual,
                value.event_time,
            )?;
            let metadata = metadata(
                context,
                instrument,
                Some(value.event_time),
                Some(value.transaction_time),
                None,
                None,
                SnapshotKind::NotApplicable,
                QualityFlags::NONE,
            );
            let payload = UncheckedEventPayload::LiquidationObservation(LiquidationObservation {
                liquidation_id: liquidation_id(context.raw, index, &value),
                price: value.bankruptcy_price,
                quantity: value.quantity,
                side: value.side,
            });
            Ok(EventEnvelope::new(metadata, payload)?)
        })
        .collect()
}

fn resolve_instrument(
    context: &NormalizationContext<'_>,
    symbol: &str,
    product_type: ProductType,
    event_time: UnixNanos,
) -> Result<InstrumentId, NormalizationError> {
    let venue = VenueId::new(BYBIT_VENUE)
        .map_err(|_| NormalizationError::InvalidContext("venue identity"))?;
    Ok(context
        .catalog
        .resolve_for_product(&venue, symbol, product_type, event_time)?
        .definition()
        .id()
        .clone())
}

#[allow(clippy::too_many_arguments)]
fn metadata(
    context: &NormalizationContext<'_>,
    instrument: InstrumentId,
    exchange_timestamp: Option<UnixNanos>,
    exchange_transaction_timestamp: Option<UnixNanos>,
    sequence_number: Option<u64>,
    previous_sequence_number: Option<u64>,
    snapshot_kind: SnapshotKind,
    quality_flags: QualityFlags,
) -> UncheckedEventMetadata {
    UncheckedEventMetadata {
        schema_version: EVENT_SCHEMA_VERSION,
        source: context.raw.source().clone(),
        venue: Some(instrument.venue().clone()),
        instrument_id: Some(instrument),
        exchange_timestamp,
        exchange_transaction_timestamp,
        receive_wall_timestamp: context.receive_wall_timestamp,
        receive_monotonic_ns: context.receive_monotonic_ns,
        normalization_timestamp: context.normalization_timestamp,
        connection_started_at: context.connection_started_at,
        sequence_number,
        previous_sequence_number,
        connection_epoch: context.raw.connection_epoch().get(),
        subscription_epoch: context.subscription_epoch.get(),
        snapshot_kind,
        source_checksum: None,
        raw_payload_hash: *context.raw.payload_hash(),
        parser_version: PARSER_VERSION.to_owned(),
        normalizer_version: NORMALIZER_VERSION.to_owned(),
        ingestion_instance: context.ingestion_instance.to_owned(),
        quality_score_ppm: event_envelope::MAX_QUALITY_SCORE_PPM,
        quality_flags,
    }
}

fn product_type(market: BybitMarket) -> ProductType {
    match market {
        BybitMarket::Spot => ProductType::Spot,
        BybitMarket::LinearPerpetual => ProductType::Perpetual,
    }
}

fn normalized_levels(
    levels: Vec<NativeBookLevel>,
    descending: bool,
) -> Result<Vec<BookLevel>, NormalizationError> {
    let mut levels = levels
        .into_iter()
        .map(|level| BookLevel {
            price: level.price,
            quantity: level.quantity,
            order_count: None,
        })
        .collect::<Vec<_>>();
    if descending {
        levels.sort_unstable_by_key(|level| Reverse(level.price));
    } else {
        levels.sort_unstable_by_key(|level| level.price);
    }
    if levels.windows(2).any(|pair| pair[0].price == pair[1].price) {
        return Err(NormalizationError::DuplicateBookLevel);
    }
    Ok(levels)
}

fn liquidation_id(raw: &DurableRawReference, index: usize, value: &AllLiquidation) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(LIQUIDATION_ID_DOMAIN);
    hasher.update(raw.payload_hash());
    hasher.update(&index.to_be_bytes());
    hasher.update(value.symbol.as_bytes());
    hasher.update(&value.transaction_time.value().to_be_bytes());
    hasher.update(&[value.side as u8]);
    hasher.finalize().to_hex().to_string()
}
