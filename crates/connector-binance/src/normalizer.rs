use std::cmp::Reverse;
use std::num::NonZeroU64;

use connector_core::DurableRawReference;
use domain::{InstrumentId, ProductType, SourceKind, UnixNanos, VenueId};
use event_envelope::{
    BookDelta, BookLevel, BookSnapshot, EventEnvelope, EventError, FundingObservation,
    LiquidationObservation, MarkIndexObservation, OpenInterestObservation, QualityFlags,
    SnapshotKind, TopOfBook, Trade, UncheckedEventMetadata, UncheckedEventPayload, VenueState,
    VenueStatus,
};
use instrument_registry::{CatalogSnapshot, ResolveError};
use thiserror::Error;

use crate::{
    AggregateTrade, BinanceMarket, BinanceMessage, BookTicker, DepthUpdate, DurableBinanceMessage,
    DurableDepthSnapshot, Liquidation, MarkPrice, NativeBookLevel, OpenInterest,
};

const EVENT_SCHEMA_VERSION: u32 = 3;
const PARSER_VERSION: &str = "binance-native-v1";
const NORMALIZER_VERSION: &str = "binance-normalizer-v1";
const BINANCE_VENUE: &str = "binance";
const MAX_INGESTION_INSTANCE_BYTES: usize = 256;
const LIQUIDATION_ID_DOMAIN: &[u8] = b"cmti:binance:sampled-liquidation:v1\0";

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
    /// Builds deterministic event lineage.
    ///
    /// The session driver supplies the durable receive wall time as
    /// `normalization_timestamp`. This is a replay-stable logical processing
    /// timestamp, not a wall-clock measurement of parser latency.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        catalog: &'a CatalogSnapshot,
        raw: &'a DurableRawReference,
        normalization_timestamp: UnixNanos,
        connection_started_at: UnixNanos,
        subscription_epoch: NonZeroU64,
        ingestion_instance: &'a str,
    ) -> Result<Self, NormalizationError> {
        if raw.source().kind() != SourceKind::Exchange || raw.source().name() != BINANCE_VENUE {
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
    #[error("durable Binance message is not an aggregate trade")]
    ExpectedAggregateTrade,
}

/// Opaque evidence minted only after a durable raw Binance aggregate-trade
/// payload passes the sealed native parser and venue-specific normalizer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinanceTradeNormalizationReceipt {
    event: EventEnvelope,
    aggregate_trade_id: u64,
    first_trade_id: u64,
    last_trade_id: u64,
}

impl BinanceTradeNormalizationReceipt {
    pub const fn event(&self) -> &EventEnvelope {
        &self.event
    }

    pub const fn aggregate_trade_id(&self) -> u64 {
        self.aggregate_trade_id
    }

    pub const fn first_trade_id(&self) -> u64 {
        self.first_trade_id
    }

    pub const fn last_trade_id(&self) -> u64 {
        self.last_trade_id
    }

    pub fn into_event(self) -> EventEnvelope {
        self.event
    }
}

pub fn normalize_native_message(
    message: DurableBinanceMessage,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let message = validated_native_message(message, context)?;
    match message {
        BinanceMessage::AggregateTrade(value) => normalize_trade(value, context),
        BinanceMessage::BookTicker(value) => normalize_book_ticker(value, context),
        BinanceMessage::DepthUpdate(value) => normalize_depth(value, context),
        BinanceMessage::MarkPrice(value) => normalize_mark_price(value, context),
        BinanceMessage::OpenInterest(value) => normalize_open_interest(value, context),
        BinanceMessage::Liquidation(value) => normalize_liquidation(value, context),
        BinanceMessage::VenueStatus { state, message } => {
            normalize_venue_status(state, message, context)
        }
    }
}

pub fn normalize_trade_with_receipt(
    message: DurableBinanceMessage,
    context: &NormalizationContext<'_>,
) -> Result<BinanceTradeNormalizationReceipt, NormalizationError> {
    let BinanceMessage::AggregateTrade(trade) = validated_native_message(message, context)? else {
        return Err(NormalizationError::ExpectedAggregateTrade);
    };
    let aggregate_trade_id = trade.aggregate_trade_id;
    let first_trade_id = trade.first_trade_id;
    let last_trade_id = trade.last_trade_id;
    let mut events = normalize_trade(trade, context)?;
    let event = events
        .pop()
        .ok_or(NormalizationError::ExpectedAggregateTrade)?;
    if !events.is_empty() {
        return Err(NormalizationError::ExpectedAggregateTrade);
    }
    Ok(BinanceTradeNormalizationReceipt {
        event,
        aggregate_trade_id,
        first_trade_id,
        last_trade_id,
    })
}

fn validated_native_message(
    message: DurableBinanceMessage,
    context: &NormalizationContext<'_>,
) -> Result<BinanceMessage, NormalizationError> {
    let (message, raw_payload_hash) = message.into_parts();
    if &raw_payload_hash != context.raw.payload_hash() {
        return Err(NormalizationError::RawPayloadMismatch);
    }
    Ok(message)
}

pub fn normalize_depth_snapshot(
    snapshot: DurableDepthSnapshot,
    context: &NormalizationContext<'_>,
) -> Result<EventEnvelope, NormalizationError> {
    let (snapshot, raw_payload_hash) = snapshot.into_parts();
    if &raw_payload_hash != context.raw.payload_hash() {
        return Err(NormalizationError::RawPayloadMismatch);
    }
    let product = product_type(snapshot.market);
    let resolution_time = snapshot
        .event_time
        .unwrap_or(context.receive_wall_timestamp);
    let instrument = resolve_instrument(context, &snapshot.symbol, product, resolution_time)?;
    let metadata = metadata(
        context,
        instrument,
        snapshot.event_time,
        snapshot.transaction_time,
        Some(snapshot.last_update_id),
        None,
        SnapshotKind::Snapshot,
        QualityFlags::NONE,
    );
    let payload = UncheckedEventPayload::BookSnapshot(BookSnapshot {
        bids: normalized_levels(snapshot.bids, true)?,
        asks: normalized_levels(snapshot.asks, false)?,
        last_sequence: snapshot.last_update_id,
    });
    Ok(EventEnvelope::new(metadata, payload)?)
}

fn normalize_trade(
    value: AggregateTrade,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let product = product_type(value.market);
    let instrument = resolve_instrument(context, &value.symbol, product, value.event_time)?;
    let payload = UncheckedEventPayload::Trade(Trade {
        trade_id: value.aggregate_trade_id.to_string(),
        price: value.price,
        quantity: value.quantity,
        side: if value.buyer_is_market_maker {
            event_envelope::Side::Sell
        } else {
            event_envelope::Side::Buy
        },
    });
    let metadata = metadata(
        context,
        instrument,
        Some(value.event_time),
        Some(value.trade_time),
        Some(value.aggregate_trade_id),
        None,
        SnapshotKind::NotApplicable,
        QualityFlags::NONE,
    );
    Ok(vec![EventEnvelope::new(metadata, payload)?])
}

fn normalize_book_ticker(
    value: BookTicker,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let instrument = resolve_instrument(
        context,
        &value.symbol,
        ProductType::Spot,
        context.receive_wall_timestamp,
    )?;
    let payload = UncheckedEventPayload::TopOfBook(TopOfBook {
        bid: Some(BookLevel {
            price: value.bid_price,
            quantity: value.bid_quantity,
            order_count: None,
        }),
        ask: Some(BookLevel {
            price: value.ask_price,
            quantity: value.ask_quantity,
            order_count: None,
        }),
    });
    let metadata = metadata(
        context,
        instrument,
        None,
        None,
        Some(value.update_id),
        None,
        SnapshotKind::NotApplicable,
        QualityFlags::NONE,
    );
    Ok(vec![EventEnvelope::new(metadata, payload)?])
}

fn normalize_depth(
    value: DepthUpdate,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let product = product_type(value.market);
    let instrument = resolve_instrument(context, &value.symbol, product, value.event_time)?;
    let previous_sequence_number = match value.market {
        BinanceMarket::Spot => value.first_update_id.checked_sub(1),
        BinanceMarket::UsdMarginedPerpetual => value.previous_final_update_id,
    };
    let payload = UncheckedEventPayload::BookDelta(BookDelta {
        bids: normalized_levels(value.bids, true)?,
        asks: normalized_levels(value.asks, false)?,
        first_sequence: value.first_update_id,
        last_sequence: value.final_update_id,
    });
    let metadata = metadata(
        context,
        instrument,
        Some(value.event_time),
        value.transaction_time,
        Some(value.final_update_id),
        previous_sequence_number,
        SnapshotKind::Delta,
        QualityFlags::NONE,
    );
    Ok(vec![EventEnvelope::new(metadata, payload)?])
}

fn normalize_mark_price(
    value: MarkPrice,
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
        None,
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
        base,
        UncheckedEventPayload::FundingObservation(FundingObservation {
            funding_rate: value.funding_rate,
            observed_at: value.event_time,
            next_funding_time: Some(value.next_funding_time),
        }),
    )?;
    Ok(vec![mark, funding])
}

fn normalize_open_interest(
    value: OpenInterest,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let instrument = resolve_instrument(
        context,
        &value.symbol,
        ProductType::Perpetual,
        value.observed_at,
    )?;
    let metadata = metadata(
        context,
        instrument,
        None,
        Some(value.observed_at),
        None,
        None,
        SnapshotKind::NotApplicable,
        QualityFlags::NONE,
    );
    let payload = UncheckedEventPayload::OpenInterestObservation(OpenInterestObservation {
        quantity: value.quantity,
        quote_notional: None,
        observed_at: value.observed_at,
    });
    Ok(vec![EventEnvelope::new(metadata, payload)?])
}

fn normalize_liquidation(
    value: Liquidation,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
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
        QualityFlags::PARTIAL,
    );
    let payload = UncheckedEventPayload::LiquidationObservation(LiquidationObservation {
        liquidation_id: sampled_liquidation_id(context.raw, &value),
        price: value.average_price,
        quantity: value.filled_quantity,
        side: value.side,
    });
    Ok(vec![EventEnvelope::new(metadata, payload)?])
}

fn normalize_venue_status(
    state: VenueState,
    message: String,
    context: &NormalizationContext<'_>,
) -> Result<Vec<EventEnvelope>, NormalizationError> {
    let venue = VenueId::new(BINANCE_VENUE)
        .map_err(|_| NormalizationError::InvalidContext("venue identity"))?;
    let metadata = UncheckedEventMetadata {
        schema_version: EVENT_SCHEMA_VERSION,
        source: context.raw.source().clone(),
        venue: Some(venue),
        instrument_id: None,
        exchange_timestamp: None,
        exchange_transaction_timestamp: None,
        receive_wall_timestamp: context.receive_wall_timestamp,
        receive_monotonic_ns: context.receive_monotonic_ns,
        normalization_timestamp: context.normalization_timestamp,
        connection_started_at: context.connection_started_at,
        sequence_number: None,
        previous_sequence_number: None,
        connection_epoch: context.raw.connection_epoch().get(),
        subscription_epoch: context.subscription_epoch.get(),
        snapshot_kind: SnapshotKind::NotApplicable,
        source_checksum: None,
        raw_payload_hash: *context.raw.payload_hash(),
        parser_version: PARSER_VERSION.to_owned(),
        normalizer_version: NORMALIZER_VERSION.to_owned(),
        ingestion_instance: context.ingestion_instance.to_owned(),
        quality_score_ppm: event_envelope::MAX_QUALITY_SCORE_PPM,
        quality_flags: QualityFlags::NONE,
    };
    let payload = UncheckedEventPayload::VenueStatus(VenueStatus {
        state,
        observed_at: context.receive_wall_timestamp,
        message: Some(message),
    });
    Ok(vec![EventEnvelope::new(metadata, payload)?])
}

fn resolve_instrument(
    context: &NormalizationContext<'_>,
    symbol: &str,
    product_type: ProductType,
    event_time: UnixNanos,
) -> Result<InstrumentId, NormalizationError> {
    let venue = VenueId::new(BINANCE_VENUE)
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

fn product_type(market: BinanceMarket) -> ProductType {
    match market {
        BinanceMarket::Spot => ProductType::Spot,
        BinanceMarket::UsdMarginedPerpetual => ProductType::Perpetual,
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

fn sampled_liquidation_id(raw: &DurableRawReference, value: &Liquidation) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(LIQUIDATION_ID_DOMAIN);
    hasher.update(raw.payload_hash());
    hasher.update(value.symbol.as_bytes());
    hasher.update(&value.transaction_time.value().to_be_bytes());
    hasher.update(&[value.side as u8]);
    hasher.finalize().to_hex().to_string()
}
