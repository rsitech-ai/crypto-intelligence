use std::num::{NonZeroU32, NonZeroU64};

use connector_bybit::{
    BybitInput, BybitMarket, NormalizationContext, NormalizationError, normalize_native_message,
    parse_durable_native_message,
};
use connector_core::{
    DurableRawCaptureChannel, DurableRawReference, RawCapture, wal_stream_source_identity,
};
use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId,
};
use event_envelope::{EventType, QualityFlags, Side, UncheckedEventPayload};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use instrument_registry::{InstrumentRegistry, ResolveError, RevisionMetadata};
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter},
    prologue::{SegmentMetadata, StreamDescriptor},
};

const SPOT_BOOK: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/spot-orderbook-snapshot.json");
const SPOT_BOOK_DELTA: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/spot-orderbook-delta.json");
const SPOT_TRADES: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/spot-public-trade.json");
const LINEAR_BOOK: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/linear-orderbook-snapshot.json");
const LINEAR_TICKER: &[u8] = include_bytes!("../../../fixtures/exchanges/bybit/linear-ticker.json");
const LINEAR_LIQUIDATIONS: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/linear-all-liquidation.json");

const RECEIVE_TIME: UnixNanos = UnixNanos::new(1_760_325_052_800_000_000);
const NORMALIZATION_TIME: UnixNanos = UnixNanos::new(1_760_325_052_800_000_001);
const CONNECTION_START: UnixNanos = UnixNanos::new(1_760_325_000_000_000_000);

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "bybit", 1).expect("source")
}

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("decimal")
}

fn definition(product_type: ProductType) -> InstrumentDefinition {
    let venue = VenueId::new("bybit").expect("venue");
    let id = match product_type {
        ProductType::Spot => InstrumentId::new(venue, "BTCUSDT", 1),
        ProductType::Perpetual => {
            InstrumentId::new_for_product(venue, "BTCUSDT", ProductType::Perpetual, 1)
        }
        ProductType::Future | ProductType::Option => panic!("fixture product"),
    }
    .expect("instrument");
    let quote = AssetId::new(AssetNamespace::Synthetic, "", "", "USDT", 1).expect("quote asset");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id,
        product_type,
        base_asset: AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1)
            .expect("base asset"),
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: decimal("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: if product_type == ProductType::Spot {
            ContractKind::None
        } else {
            ContractKind::Linear
        },
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: Price::new(decimal("0.1")).expect("tick"),
        quantity_step: Quantity::new(decimal("0.001")).expect("step"),
        listing_time: UnixNanos::new(1),
        delisting_time: None,
    })
    .expect("definition")
}

fn catalog() -> std::sync::Arc<instrument_registry::CatalogSnapshot> {
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            definition(ProductType::Spot),
            RevisionMetadata::try_new(UnixNanos::new(1), "fixture:spot").expect("metadata"),
        )
        .expect("spot");
    registry
        .append_definition(
            definition(ProductType::Perpetual),
            RevisionMetadata::try_new(UnixNanos::new(1), "fixture:perpetual").expect("metadata"),
        )
        .expect("perpetual");
    registry.snapshot().expect("catalog")
}

async fn durable_reference(payload: &[u8]) -> DurableRawReference {
    let directory = tempfile::tempdir().expect("temporary WAL");
    let mut segment_id = [0_u8; 16];
    segment_id.copy_from_slice(&blake3::hash(payload).as_bytes()[..16]);
    let metadata = SegmentMetadata::new(
        segment_id,
        1,
        "schema",
        "installation",
        "build",
        vec![
            StreamDescriptor::new(7, wal_stream_source_identity(&source()), "fixture")
                .expect("stream"),
        ],
    )
    .expect("segment");
    let mut writer =
        SegmentedWalWriter::create(directory.path(), metadata, RotationPolicy::default(), 1)
            .expect("writer");
    let authority = writer.append_authority();
    let (proof, compression) = writer
        .append(
            RecordMetadata {
                flags: 0,
                stream_id: 7,
                connection_epoch: 3,
                record_sequence: 1,
                receive_wall_time_ns: RECEIVE_TIME.value(),
                receive_monotonic_time_ns: 9,
            },
            payload,
            9,
            RECEIVE_TIME.value(),
        )
        .expect("append")
        .into_parts();
    assert!(compression.is_none());

    let channel = DurableRawCaptureChannel::new(
        authority,
        source(),
        NonZeroU32::new(7).expect("stream"),
        1,
        payload.len() + 1,
    )
    .expect("channel");
    let (client, mut worker) = channel.split();
    let pending_receipt = client
        .try_submit(
            RawCapture::try_new(
                source(),
                NonZeroU32::new(7).expect("stream"),
                NonZeroU64::new(3).expect("epoch"),
                NonZeroU64::new(1).expect("record"),
                RECEIVE_TIME,
                9,
                payload.to_vec().into_boxed_slice(),
            )
            .expect("capture"),
        )
        .expect("submit");
    worker
        .recv()
        .await
        .expect("pending capture")
        .acknowledge(proof)
        .expect("acknowledge");
    pending_receipt.wait().await.expect("durable reference")
}

fn context<'a>(
    catalog: &'a instrument_registry::CatalogSnapshot,
    raw: &'a DurableRawReference,
) -> NormalizationContext<'a> {
    NormalizationContext::try_new(
        catalog,
        raw,
        NORMALIZATION_TIME,
        CONNECTION_START,
        NonZeroU64::new(11).expect("subscription"),
        "normalization-test",
    )
    .expect("context")
}

#[tokio::test]
async fn durable_parser_rejects_bytes_from_a_different_raw_receipt() {
    let raw = durable_reference(SPOT_TRADES).await;

    assert_eq!(
        parse_durable_native_message(
            BybitInput::PublicWebSocket(BybitMarket::LinearPerpetual),
            LINEAR_TICKER,
            &raw,
        ),
        Err(connector_bybit::NativeParseError::RawPayloadMismatch)
    );
}

#[tokio::test]
async fn spot_and_perpetual_same_symbol_bind_to_distinct_product_identities() {
    let catalog = catalog();

    let spot_raw = durable_reference(SPOT_TRADES).await;
    let spot = parse_durable_native_message(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        SPOT_TRADES,
        &spot_raw,
    )
    .expect("spot parse");
    let spot_events =
        normalize_native_message(spot, &context(&catalog, &spot_raw)).expect("spot normalize");
    assert_eq!(spot_events.len(), 2);
    assert!(spot_events.iter().all(|event| {
        event
            .metadata()
            .as_unchecked()
            .instrument_id
            .as_ref()
            .expect("instrument")
            .product_type()
            == ProductType::Spot
    }));
    assert_ne!(spot_events[0].id(), spot_events[1].id());
    assert_eq!(
        spot_events[0].metadata().as_unchecked().sequence_number,
        spot_events[1].metadata().as_unchecked().sequence_number
    );
    let UncheckedEventPayload::Trade(first_trade) = spot_events[0].payload().as_unchecked() else {
        panic!("trade payload");
    };
    assert_eq!(first_trade.side, Side::Buy);

    let linear_raw = durable_reference(LINEAR_BOOK).await;
    let linear = parse_durable_native_message(
        BybitInput::PublicWebSocket(BybitMarket::LinearPerpetual),
        LINEAR_BOOK,
        &linear_raw,
    )
    .expect("linear parse");
    let events = normalize_native_message(linear, &context(&catalog, &linear_raw))
        .expect("linear normalize");
    let metadata = events[0].metadata().as_unchecked();
    assert_eq!(metadata.schema_version, 3);
    assert_eq!(
        metadata
            .instrument_id
            .as_ref()
            .expect("instrument")
            .product_type(),
        ProductType::Perpetual
    );
    assert_eq!(metadata.sequence_number, Some(220));
    assert_eq!(metadata.raw_payload_hash, *linear_raw.payload_hash());
    assert_eq!(metadata.receive_wall_timestamp, RECEIVE_TIME);
    assert_eq!(metadata.receive_monotonic_ns, 9);
    events[0].verify().expect("event identity");
}

#[tokio::test]
async fn orderbook_snapshot_normalizes_to_a_canonical_raw_linked_event() {
    let catalog = catalog();
    let raw = durable_reference(SPOT_BOOK).await;
    let message = parse_durable_native_message(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        SPOT_BOOK,
        &raw,
    )
    .expect("book parse");

    let events =
        normalize_native_message(message, &context(&catalog, &raw)).expect("book normalize");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type(), EventType::BookSnapshot);
    assert_eq!(
        events[0].metadata().as_unchecked().snapshot_kind,
        event_envelope::SnapshotKind::Snapshot
    );
    assert_eq!(
        events[0].metadata().as_unchecked().raw_payload_hash,
        *raw.payload_hash()
    );
    events[0].verify().expect("event identity");
}

#[tokio::test]
async fn exact_next_delta_derives_the_required_predecessor_without_using_cross_sequence() {
    let catalog = catalog();
    let raw = durable_reference(SPOT_BOOK_DELTA).await;
    let message = parse_durable_native_message(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        SPOT_BOOK_DELTA,
        &raw,
    )
    .expect("delta parse");

    let events =
        normalize_native_message(message, &context(&catalog, &raw)).expect("delta normalize");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type(), EventType::BookDelta);
    let metadata = events[0].metadata().as_unchecked();
    assert_eq!(metadata.sequence_number, Some(101));
    assert_eq!(metadata.previous_sequence_number, Some(100));
    assert_ne!(metadata.sequence_number, Some(9_532_239_402));
    events[0].verify().expect("delta identity");
}

#[tokio::test]
async fn linear_ticker_and_liquidations_preserve_distinct_deterministic_events() {
    let catalog = catalog();

    let ticker_raw = durable_reference(LINEAR_TICKER).await;
    let ticker = parse_durable_native_message(
        BybitInput::PublicWebSocket(BybitMarket::LinearPerpetual),
        LINEAR_TICKER,
        &ticker_raw,
    )
    .expect("ticker");
    let ticker_events = normalize_native_message(ticker, &context(&catalog, &ticker_raw))
        .expect("ticker normalize");
    assert_eq!(
        ticker_events
            .iter()
            .map(|event| event.event_type())
            .collect::<Vec<_>>(),
        vec![
            EventType::MarkIndexObservation,
            EventType::FundingObservation,
            EventType::OpenInterestObservation,
        ]
    );
    assert_ne!(ticker_events[0].id(), ticker_events[1].id());
    assert_ne!(ticker_events[1].id(), ticker_events[2].id());
    ticker_events
        .iter()
        .try_for_each(|event| event.verify())
        .expect("ticker event identities");

    let liquidation_raw = durable_reference(LINEAR_LIQUIDATIONS).await;
    let liquidation = parse_durable_native_message(
        BybitInput::PublicWebSocket(BybitMarket::LinearPerpetual),
        LINEAR_LIQUIDATIONS,
        &liquidation_raw,
    )
    .expect("liquidation");
    let first = normalize_native_message(liquidation.clone(), &context(&catalog, &liquidation_raw))
        .expect("liquidation normalize");
    let second = normalize_native_message(liquidation, &context(&catalog, &liquidation_raw))
        .expect("deterministic normalize");
    assert_eq!(first, second);
    assert_eq!(first.len(), 2);
    assert_ne!(first[0].id(), first[1].id());
    assert!(
        first
            .iter()
            .all(|event| { event.metadata().as_unchecked().quality_flags == QualityFlags::NONE })
    );
    for event in &first {
        let UncheckedEventPayload::LiquidationObservation(value) = event.payload().as_unchecked()
        else {
            panic!("liquidation payload");
        };
        assert_eq!(value.liquidation_id.len(), 64);
    }
}

#[tokio::test]
async fn unresolved_instruments_fail_closed_without_guessing_a_generation() {
    let catalog = catalog();
    let unknown_symbol = String::from_utf8(SPOT_TRADES.to_vec())
        .expect("utf8 fixture")
        .replace("BTCUSDT", "ETHUSDT");
    let raw = durable_reference(unknown_symbol.as_bytes()).await;
    let message = parse_durable_native_message(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        unknown_symbol.as_bytes(),
        &raw,
    )
    .expect("trade parse");

    assert_eq!(
        normalize_native_message(message, &context(&catalog, &raw)),
        Err(NormalizationError::InstrumentResolution(
            ResolveError::UnknownSymbol
        ))
    );
}
