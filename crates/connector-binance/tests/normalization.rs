use std::num::{NonZeroU32, NonZeroU64};

use connector_binance::{
    BinanceDerivativeStream, BinanceInput, BinanceMarket, NormalizationContext, NormalizationError,
    normalize_depth_snapshot, normalize_derivative_with_receipts, normalize_native_message,
    normalize_trade_with_receipt, parse_durable_depth_snapshot, parse_durable_native_message,
};
use connector_core::{
    ChannelError, Completeness, DurableRawCaptureChannel, DurableRawReference, NormalizedOutput,
    RawCapture, wal_stream_source_identity,
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

const SPOT_TRADE: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-aggregate-trade.json");
const USDM_DEPTH: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-depth-update.json");
const USDM_MARK: &[u8] = include_bytes!("../../../fixtures/exchanges/binance/usdm-mark-price.json");
const USDM_OI: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-open-interest.json");
const USDM_LIQUIDATION: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-liquidation.json");
const SPOT_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-depth-snapshot.json");

const RECEIVE_TIME: UnixNanos = UnixNanos::new(1_672_515_782_137_000_000);
const NORMALIZATION_TIME: UnixNanos = UnixNanos::new(1_672_515_782_137_000_001);
const CONNECTION_START: UnixNanos = UnixNanos::new(1_672_515_700_000_000_000);

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "binance", 1).expect("source")
}

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("decimal")
}

fn definition(product_type: ProductType) -> InstrumentDefinition {
    let venue = VenueId::new("binance").expect("venue");
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
        price_tick: Price::new(decimal("0.01")).expect("tick"),
        quantity_step: Quantity::new(decimal("0.001")).expect("step"),
        listing_time: UnixNanos::new(1),
        delisting_time: None,
    })
    .expect("definition")
}

fn catalog() -> std::sync::Arc<instrument_registry::CatalogSnapshot> {
    catalog_known_at(UnixNanos::new(1))
}

fn catalog_known_at(
    as_known_at: UnixNanos,
) -> std::sync::Arc<instrument_registry::CatalogSnapshot> {
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            definition(ProductType::Spot),
            RevisionMetadata::try_new(as_known_at, "fixture:spot").expect("metadata"),
        )
        .expect("spot");
    registry
        .append_definition(
            definition(ProductType::Perpetual),
            RevisionMetadata::try_new(as_known_at, "fixture:perpetual").expect("metadata"),
        )
        .expect("perpetual");
    registry.snapshot().expect("catalog")
}

#[tokio::test]
async fn normalization_rejects_catalog_knowledge_from_after_processing_time() {
    let raw = durable_reference(
        USDM_MARK,
        BinanceInput::UsdMMarkPriceWebSocket.wal_stream_name(),
    )
    .await;
    let future_catalog = catalog_known_at(UnixNanos::new(NORMALIZATION_TIME.value() + 1));

    assert!(matches!(
        NormalizationContext::try_new(
            &future_catalog,
            &raw,
            NORMALIZATION_TIME,
            CONNECTION_START,
            NonZeroU64::new(11).expect("subscription"),
            "normalization-test",
        ),
        Err(NormalizationError::InvalidContext(
            "catalog knowledge ordering"
        ))
    ));
}

async fn durable_reference(payload: &[u8], stream_name: &str) -> DurableRawReference {
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
            StreamDescriptor::new(7, wal_stream_source_identity(&source()), stream_name)
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

#[tokio::test]
async fn durable_parser_rejects_bytes_from_a_different_raw_receipt() {
    let raw = durable_reference(SPOT_TRADE, BinanceInput::SpotWebSocket.wal_stream_name()).await;

    assert_eq!(
        parse_durable_native_message(BinanceInput::UsdMOpenInterestRest, USDM_OI, &raw),
        Err(connector_binance::NativeParseError::RawPayloadMismatch)
    );
}

#[tokio::test]
async fn durable_parser_rejects_identical_bytes_from_the_wrong_wal_stream_contract() {
    let raw = durable_reference(
        SPOT_TRADE,
        BinanceInput::UsdMMarkPriceWebSocket.wal_stream_name(),
    )
    .await;

    assert_eq!(
        parse_durable_native_message(BinanceInput::SpotWebSocket, SPOT_TRADE, &raw),
        Err(connector_binance::NativeParseError::RawStreamMismatch)
    );
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
async fn spot_and_perpetual_same_symbol_bind_to_distinct_product_identities() {
    let catalog = catalog();

    let spot_raw =
        durable_reference(SPOT_TRADE, BinanceInput::SpotWebSocket.wal_stream_name()).await;
    let spot = parse_durable_native_message(BinanceInput::SpotWebSocket, SPOT_TRADE, &spot_raw)
        .expect("spot parse");
    let spot_events =
        normalize_native_message(spot, &context(&catalog, &spot_raw)).expect("spot normalize");
    assert_eq!(spot_events.len(), 1);
    let spot_metadata = spot_events[0].metadata().as_unchecked();
    assert_eq!(
        spot_metadata
            .instrument_id
            .as_ref()
            .expect("instrument")
            .product_type(),
        ProductType::Spot
    );
    assert_eq!(spot_events[0].event_type(), EventType::Trade);
    let UncheckedEventPayload::Trade(trade) = spot_events[0].payload().as_unchecked() else {
        panic!("trade payload");
    };
    assert_eq!(trade.side, Side::Sell);

    let futures_raw = durable_reference(
        USDM_DEPTH,
        BinanceInput::UsdMDepthWebSocket.wal_stream_name(),
    )
    .await;
    let futures =
        parse_durable_native_message(BinanceInput::UsdMDepthWebSocket, USDM_DEPTH, &futures_raw)
            .expect("depth parse");
    let futures_events =
        normalize_native_message(futures, &context(&catalog, &futures_raw)).expect("normalize");
    let metadata = futures_events[0].metadata().as_unchecked();
    assert_eq!(metadata.schema_version, 3);
    assert_eq!(
        metadata
            .instrument_id
            .as_ref()
            .expect("instrument")
            .product_type(),
        ProductType::Perpetual
    );
    assert_eq!(metadata.sequence_number, Some(160));
    assert_eq!(metadata.previous_sequence_number, Some(149));
    assert_eq!(metadata.receive_wall_timestamp, RECEIVE_TIME);
    assert_eq!(metadata.receive_monotonic_ns, 9);
    futures_events[0].verify().expect("event identity");
}

#[tokio::test]
async fn authoritative_trade_receipt_is_minted_from_the_sealed_durable_parser_output() {
    let catalog = catalog();
    let raw = durable_reference(SPOT_TRADE, BinanceInput::SpotWebSocket.wal_stream_name()).await;
    let parsed = parse_durable_native_message(BinanceInput::SpotWebSocket, SPOT_TRADE, &raw)
        .expect("durable trade parse");
    let receipt = normalize_trade_with_receipt(parsed, &context(&catalog, &raw))
        .expect("authoritative trade receipt");
    assert_eq!(receipt.first_trade_id(), 100);
    assert_eq!(receipt.last_trade_id(), 105);
    assert_eq!(receipt.event().event_type(), EventType::Trade);
    let UncheckedEventPayload::Trade(trade) = receipt.event().payload().as_unchecked() else {
        panic!("trade payload");
    };
    assert_eq!(trade.side, Side::Sell);

    let depth_raw = durable_reference(
        USDM_DEPTH,
        BinanceInput::UsdMDepthWebSocket.wal_stream_name(),
    )
    .await;
    let depth =
        parse_durable_native_message(BinanceInput::UsdMDepthWebSocket, USDM_DEPTH, &depth_raw)
            .expect("durable depth parse");
    assert_eq!(
        normalize_trade_with_receipt(depth, &context(&catalog, &depth_raw)),
        Err(NormalizationError::ExpectedAggregateTrade)
    );
}

#[tokio::test]
async fn durable_snapshot_normalizes_to_a_canonical_product_aware_event() {
    let catalog = catalog();
    let raw = durable_reference(
        SPOT_SNAPSHOT,
        BinanceMarket::Spot.snapshot_wal_stream_name(),
    )
    .await;
    let snapshot =
        parse_durable_depth_snapshot(BinanceMarket::Spot, "BTCUSDT", SPOT_SNAPSHOT, &raw)
            .expect("durable snapshot");

    let event =
        normalize_depth_snapshot(snapshot, &context(&catalog, &raw)).expect("snapshot normalize");

    assert_eq!(event.event_type(), EventType::BookSnapshot);
    let metadata = event.metadata().as_unchecked();
    assert_eq!(metadata.sequence_number, Some(156));
    assert_eq!(
        metadata.snapshot_kind,
        event_envelope::SnapshotKind::Snapshot
    );
    assert_eq!(
        metadata
            .instrument_id
            .as_ref()
            .expect("instrument")
            .product_type(),
        ProductType::Spot
    );
    assert_eq!(metadata.raw_payload_hash, *raw.payload_hash());
    event.verify().expect("snapshot identity");
}

#[tokio::test]
async fn derivative_observations_normalize_with_sampled_liquidation_lineage() {
    let catalog = catalog();

    let mark_raw = durable_reference(
        USDM_MARK,
        BinanceInput::UsdMMarkPriceWebSocket.wal_stream_name(),
    )
    .await;
    let mark =
        parse_durable_native_message(BinanceInput::UsdMMarkPriceWebSocket, USDM_MARK, &mark_raw)
            .expect("mark");
    let mark_receipts =
        normalize_derivative_with_receipts(mark.clone(), &context(&catalog, &mark_raw))
            .expect("mark receipts");
    assert_eq!(mark_receipts.len(), 2);
    assert_eq!(
        mark_receipts
            .iter()
            .map(|receipt| receipt.stream())
            .collect::<Vec<_>>(),
        vec![
            BinanceDerivativeStream::MarkIndex,
            BinanceDerivativeStream::Funding
        ]
    );
    assert!(mark_receipts.iter().all(|receipt| matches!(
        receipt.completeness(),
        Completeness::VenueReportedComplete {
            delivery_uncertainty: true
        }
    )));
    assert!(
        mark_receipts
            .iter()
            .all(|receipt| receipt.connector_version() == "1.0.0")
    );
    assert!(mark_receipts.iter().all(|receipt| {
        receipt.catalog_as_known_at() == UnixNanos::new(1)
            && receipt.catalog_digest() == catalog.catalog_digest()
            && receipt.definition_hash() != &[0; 32]
    }));
    assert!(
        mark_receipts
            .iter()
            .all(|receipt| receipt.instrument_definition() == &definition(ProductType::Perpetual))
    );
    assert!(
        mark_receipts
            .iter()
            .all(|receipt| receipt.catalog_digest() == catalog.catalog_digest())
    );
    assert_eq!(
        normalize_native_message(mark, &context(&catalog, &mark_raw)),
        Err(NormalizationError::ExpectedDerivativeObservation)
    );
    assert_eq!(
        mark_receipts
            .iter()
            .map(|receipt| receipt.event().event_type())
            .collect::<Vec<_>>(),
        vec![
            EventType::MarkIndexObservation,
            EventType::FundingObservation
        ]
    );
    assert_ne!(mark_receipts[0].event().id(), mark_receipts[1].event().id());
    assert_eq!(
        NormalizedOutput::try_new(mark_raw.clone(), mark_receipts[0].event().clone()),
        Err(ChannelError::DerivativeAuthorityRequired)
    );

    let oi_raw = durable_reference(
        USDM_OI,
        BinanceInput::UsdMOpenInterestRest.wal_stream_name(),
    )
    .await;
    let oi = parse_durable_native_message(BinanceInput::UsdMOpenInterestRest, USDM_OI, &oi_raw)
        .expect("OI");
    let oi_receipts = normalize_derivative_with_receipts(oi.clone(), &context(&catalog, &oi_raw))
        .expect("OI receipts");
    assert_eq!(oi_receipts.len(), 1);
    assert_eq!(
        oi_receipts[0].stream(),
        BinanceDerivativeStream::OpenInterest
    );
    assert_eq!(
        normalize_native_message(oi, &context(&catalog, &oi_raw)),
        Err(NormalizationError::ExpectedDerivativeObservation)
    );
    assert_eq!(
        oi_receipts[0].event().event_type(),
        EventType::OpenInterestObservation
    );
    let oi_metadata = oi_receipts[0].event().metadata().as_unchecked();
    assert_eq!(oi_metadata.exchange_timestamp, None);
    assert_eq!(
        oi_metadata.exchange_transaction_timestamp,
        Some(UnixNanos::new(1_672_515_782_136_000_000))
    );

    let liquidation_raw = durable_reference(
        USDM_LIQUIDATION,
        BinanceInput::UsdMLiquidationWebSocket.wal_stream_name(),
    )
    .await;
    let liquidation = parse_durable_native_message(
        BinanceInput::UsdMLiquidationWebSocket,
        USDM_LIQUIDATION,
        &liquidation_raw,
    )
    .expect("liquidation");
    let liquidation_receipts = normalize_derivative_with_receipts(
        liquidation.clone(),
        &context(&catalog, &liquidation_raw),
    )
    .expect("liquidation receipt");
    assert_eq!(liquidation_receipts.len(), 1);
    assert_eq!(
        liquidation_receipts[0].stream(),
        BinanceDerivativeStream::Liquidation
    );
    assert_eq!(
        liquidation_receipts[0].completeness(),
        Completeness::SampledLargestPerSymbolWindow {
            window_ms: NonZeroU32::new(1_000).expect("window")
        }
    );
    assert_eq!(
        liquidation_receipts[0]
            .event()
            .metadata()
            .as_unchecked()
            .quality_flags,
        QualityFlags::PARTIAL
    );
    assert_eq!(
        normalize_native_message(liquidation.clone(), &context(&catalog, &liquidation_raw)),
        Err(NormalizationError::ExpectedDerivativeObservation)
    );
    let first = normalize_derivative_with_receipts(
        liquidation.clone(),
        &context(&catalog, &liquidation_raw),
    )
    .expect("liquidation normalize");
    let second =
        normalize_derivative_with_receipts(liquidation, &context(&catalog, &liquidation_raw))
            .expect("deterministic normalize");
    assert_eq!(first, second);
    assert_eq!(
        first[0].event().metadata().as_unchecked().quality_flags,
        QualityFlags::PARTIAL
    );
    let UncheckedEventPayload::LiquidationObservation(value) =
        first[0].event().payload().as_unchecked()
    else {
        panic!("liquidation payload");
    };
    assert_eq!(value.liquidation_id.len(), 64);
}

#[tokio::test]
async fn unresolved_instruments_fail_closed_without_guessing_a_generation() {
    let catalog = catalog();
    let unknown_symbol = String::from_utf8(SPOT_TRADE.to_vec())
        .expect("utf8 fixture")
        .replace("BTCUSDT", "ETHUSDT");
    let raw = durable_reference(
        unknown_symbol.as_bytes(),
        BinanceInput::SpotWebSocket.wal_stream_name(),
    )
    .await;
    let message =
        parse_durable_native_message(BinanceInput::SpotWebSocket, unknown_symbol.as_bytes(), &raw)
            .expect("trade parse");

    assert_eq!(
        normalize_native_message(message, &context(&catalog, &raw)),
        Err(NormalizationError::InstrumentResolution(
            ResolveError::UnknownSymbol
        ))
    );
}
