use std::num::{NonZeroU32, NonZeroU64};

use connector_binance::{
    BinanceBookSynchronizer, BinanceInput, BinanceMarket, NormalizationContext,
    normalize_depth_snapshot, normalize_native_message, parse_durable_depth_snapshot,
    parse_durable_native_message,
};
use connector_core::{
    DurableRawCaptureChannel, DurableRawReference, NormalizedOutput, RawCapture,
    wal_stream_source_identity,
};
use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use instrument_registry::{CatalogSnapshot, InstrumentRegistry, RevisionMetadata};
use orderbook::{
    ApplyResult, BookConfig, BookError, BookSession, BookState, ChecksumPolicy, SequencePolicy,
};
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter},
    prologue::{SegmentMetadata, StreamDescriptor},
};

const SPOT_DEPTH: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-depth-update.json");
const USDM_DEPTH: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-depth-update.json");
const SPOT_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-depth-snapshot.json");
const USDM_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-depth-snapshot.json");
const RECEIVE_TIME_NS: i64 = 1_672_515_782_137_000_000;
const CONNECTION_START: UnixNanos = UnixNanos::new(1_672_515_700_000_000_000);

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("decimal")
}

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "binance", 1).expect("source")
}

fn instrument(product: ProductType) -> InstrumentId {
    InstrumentId::new_for_product(
        VenueId::new("binance").expect("venue"),
        "BTCUSDT",
        product,
        1,
    )
    .expect("instrument")
}

fn definition(product: ProductType) -> InstrumentDefinition {
    let quote = AssetId::new(AssetNamespace::Synthetic, "", "", "USDT", 1).expect("quote");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: instrument(product),
        product_type: product,
        base_asset: AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("base"),
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: decimal("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: if product == ProductType::Spot {
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

fn catalog() -> std::sync::Arc<CatalogSnapshot> {
    let mut registry = InstrumentRegistry::new();
    for product in [ProductType::Spot, ProductType::Perpetual] {
        registry
            .append_definition(
                definition(product),
                RevisionMetadata::try_new(UnixNanos::new(1), "book-sync-fixture")
                    .expect("revision"),
            )
            .expect("definition");
    }
    registry.snapshot().expect("catalog")
}

fn config(product: ProductType, policy: SequencePolicy) -> BookConfig {
    BookConfig {
        instrument: instrument(product),
        price_tick: Price::new(decimal("0.01")).expect("tick"),
        quantity_step: Quantity::new(decimal("0.001")).expect("step"),
        max_levels_per_side: 100,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: policy,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    }
}

fn session(connection: u64, subscription: u64) -> BookSession {
    BookSession {
        connection_epoch: connection,
        subscription_epoch: subscription,
        instrument_generation: 1,
    }
}

async fn durable_reference(
    payload: &[u8],
    connection_epoch: u64,
    record_sequence: u64,
    receive_monotonic_ns: u64,
) -> DurableRawReference {
    let receive_wall_time =
        UnixNanos::new(RECEIVE_TIME_NS + i64::try_from(record_sequence).expect("record sequence"));
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
                connection_epoch,
                record_sequence,
                receive_wall_time_ns: receive_wall_time.value(),
                receive_monotonic_time_ns: receive_monotonic_ns,
            },
            payload,
            receive_monotonic_ns,
            receive_wall_time.value(),
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
                NonZeroU64::new(connection_epoch).expect("connection"),
                NonZeroU64::new(record_sequence).expect("record"),
                receive_wall_time,
                receive_monotonic_ns,
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
    catalog: &'a CatalogSnapshot,
    raw: &'a DurableRawReference,
    subscription_epoch: u64,
) -> NormalizationContext<'a> {
    NormalizationContext::try_new(
        catalog,
        raw,
        UnixNanos::new(raw.receive_wall_time().value() + 1),
        CONNECTION_START,
        NonZeroU64::new(subscription_epoch).expect("subscription"),
        "book-sync-test",
    )
    .expect("context")
}

async fn depth_output(
    catalog: &CatalogSnapshot,
    input: BinanceInput,
    raw: &[u8],
    connection: u64,
    subscription: u64,
    record: u64,
    monotonic: u64,
) -> NormalizedOutput {
    let reference = durable_reference(raw, connection, record, monotonic).await;
    let parsed = parse_durable_native_message(input, raw, &reference).expect("durable depth parse");
    let mut events = normalize_native_message(parsed, &context(catalog, &reference, subscription))
        .expect("normalize depth");
    assert_eq!(events.len(), 1);
    NormalizedOutput::try_new(reference, events.remove(0)).expect("raw/event link")
}

async fn snapshot_output(
    catalog: &CatalogSnapshot,
    market: BinanceMarket,
    raw: &[u8],
    connection: u64,
    subscription: u64,
    record: u64,
    monotonic: u64,
) -> NormalizedOutput {
    let reference = durable_reference(raw, connection, record, monotonic).await;
    let parsed = parse_durable_depth_snapshot(market, "BTCUSDT", raw, &reference)
        .expect("durable snapshot parse");
    let event = normalize_depth_snapshot(parsed, &context(catalog, &reference, subscription))
        .expect("normalize snapshot");
    NormalizedOutput::try_new(reference, event).expect("raw/event link")
}

#[tokio::test]
async fn spot_snapshot_buffer_gap_recovery_and_renewal_use_only_normalized_outputs() {
    let catalog = catalog();
    let active = session(1, 1);
    let mut sync = BinanceBookSynchronizer::try_new(
        BinanceMarket::Spot,
        config(ProductType::Spot, SequencePolicy::RangeContainsNext),
    )
    .expect("spot synchronizer");
    sync.start_session(active).expect("session");

    let first = depth_output(
        &catalog,
        BinanceInput::SpotWebSocket,
        SPOT_DEPTH,
        1,
        1,
        1,
        10,
    )
    .await;
    assert_eq!(
        sync.apply_output(&first).expect("buffer"),
        ApplyResult::SnapshotRequired
    );
    assert_eq!(sync.snapshot(), Err(BookError::Untrusted));

    let snapshot = snapshot_output(&catalog, BinanceMarket::Spot, SPOT_SNAPSHOT, 1, 1, 2, 20).await;
    assert_eq!(
        sync.apply_output(&snapshot).expect("align snapshot"),
        ApplyResult::Applied
    );
    assert_eq!(
        sync.snapshot().expect("trusted").last_source_sequence(),
        160
    );

    let duplicate = depth_output(
        &catalog,
        BinanceInput::SpotWebSocket,
        SPOT_DEPTH,
        1,
        1,
        3,
        21,
    )
    .await;
    assert_eq!(
        sync.apply_output(&duplicate).expect("duplicate"),
        ApplyResult::Duplicate
    );

    let gap_raw = String::from_utf8(SPOT_DEPTH.to_vec())
        .expect("utf8")
        .replace("\"U\": 157", "\"U\": 162")
        .replace("\"u\": 160", "\"u\": 165");
    let gap = depth_output(
        &catalog,
        BinanceInput::SpotWebSocket,
        gap_raw.as_bytes(),
        1,
        1,
        4,
        22,
    )
    .await;
    assert_eq!(
        sync.apply_output(&gap).expect("gap"),
        ApplyResult::GapDetected
    );
    assert_eq!(sync.state(), BookState::Buffering);
    assert_eq!(sync.resnapshot_count(), 1);
    assert_eq!(sync.snapshot(), Err(BookError::Untrusted));

    let recovery_raw = String::from_utf8(SPOT_DEPTH.to_vec())
        .expect("utf8")
        .replace("\"U\": 157", "\"U\": 166")
        .replace("\"u\": 160", "\"u\": 166");
    let recovery = depth_output(
        &catalog,
        BinanceInput::SpotWebSocket,
        recovery_raw.as_bytes(),
        1,
        1,
        5,
        23,
    )
    .await;
    sync.apply_output(&recovery).expect("buffer recovery delta");
    let recovery_snapshot_raw = String::from_utf8(SPOT_SNAPSHOT.to_vec())
        .expect("utf8")
        .replace("\"lastUpdateId\": 156", "\"lastUpdateId\": 165");
    let recovery_snapshot = snapshot_output(
        &catalog,
        BinanceMarket::Spot,
        recovery_snapshot_raw.as_bytes(),
        1,
        1,
        6,
        24,
    )
    .await;
    assert_eq!(
        sync.apply_output(&recovery_snapshot)
            .expect("fresh recovery snapshot"),
        ApplyResult::Applied
    );
    assert_eq!(
        sync.snapshot()
            .expect("trusted after recovery")
            .last_source_sequence(),
        166
    );

    sync.heartbeat_timeout();
    assert_eq!(sync.state(), BookState::Disconnected);
    assert_eq!(sync.resnapshot_count(), 2);
    sync.start_session(session(2, 1)).expect("new connection");
    assert_eq!(
        sync.apply_output(&duplicate),
        Err(connector_binance::BinanceBookSyncError::Book(
            BookError::InvalidEpoch
        ))
    );
    sync.renew_session(session(2, 2))
        .expect("planned subscription renewal");
    assert_eq!(sync.state(), BookState::Buffering);
    assert_eq!(sync.resnapshot_count(), 3);
}

#[tokio::test]
async fn usd_m_uses_native_previous_final_sequence_continuity() {
    let catalog = catalog();
    let active = session(7, 11);
    let mut sync = BinanceBookSynchronizer::try_new(
        BinanceMarket::UsdMarginedPerpetual,
        config(ProductType::Perpetual, SequencePolicy::PreviousFinal),
    )
    .expect("USD-M synchronizer");
    sync.start_session(active).expect("session");
    let first = depth_output(
        &catalog,
        BinanceInput::UsdMPublicWebSocket,
        USDM_DEPTH,
        7,
        11,
        1,
        10,
    )
    .await;
    sync.apply_output(&first).expect("buffer");
    let snapshot = snapshot_output(
        &catalog,
        BinanceMarket::UsdMarginedPerpetual,
        USDM_SNAPSHOT,
        7,
        11,
        2,
        20,
    )
    .await;
    assert_eq!(
        sync.apply_output(&snapshot).expect("snapshot"),
        ApplyResult::Applied
    );

    let wrong_previous_raw = String::from_utf8(USDM_DEPTH.to_vec())
        .expect("utf8")
        .replace("\"U\": 157", "\"U\": 161")
        .replace("\"u\": 160", "\"u\": 162")
        .replace("\"pu\": 149", "\"pu\": 158");
    let wrong_previous = depth_output(
        &catalog,
        BinanceInput::UsdMPublicWebSocket,
        wrong_previous_raw.as_bytes(),
        7,
        11,
        3,
        21,
    )
    .await;
    assert_eq!(
        sync.apply_output(&wrong_previous).expect("continuity gap"),
        ApplyResult::GapDetected
    );
    assert_eq!(sync.state(), BookState::Buffering);
}

#[tokio::test]
async fn stale_snapshot_retries_without_publishing_an_untrusted_book() {
    let catalog = catalog();
    let active = session(1, 1);
    let mut sync = BinanceBookSynchronizer::try_new(
        BinanceMarket::Spot,
        config(ProductType::Spot, SequencePolicy::RangeContainsNext),
    )
    .expect("synchronizer");
    sync.start_session(active).expect("session");
    let delta = depth_output(
        &catalog,
        BinanceInput::SpotWebSocket,
        SPOT_DEPTH,
        1,
        1,
        1,
        10,
    )
    .await;
    sync.apply_output(&delta).expect("buffer");

    let stale_raw = String::from_utf8(SPOT_SNAPSHOT.to_vec())
        .expect("utf8")
        .replace("\"lastUpdateId\": 156", "\"lastUpdateId\": 150");
    let stale = snapshot_output(
        &catalog,
        BinanceMarket::Spot,
        stale_raw.as_bytes(),
        1,
        1,
        2,
        20,
    )
    .await;
    assert_eq!(
        sync.apply_output(&stale).expect("stale snapshot"),
        ApplyResult::GapDetected
    );
    assert_eq!(sync.snapshot(), Err(BookError::Untrusted));
    assert_eq!(sync.resnapshot_count(), 1);

    let retry_delta = depth_output(
        &catalog,
        BinanceInput::SpotWebSocket,
        SPOT_DEPTH,
        1,
        1,
        3,
        21,
    )
    .await;
    sync.apply_output(&retry_delta).expect("retry delta");
    let fresh = snapshot_output(&catalog, BinanceMarket::Spot, SPOT_SNAPSHOT, 1, 1, 4, 22).await;
    assert_eq!(
        sync.apply_output(&fresh).expect("fresh snapshot"),
        ApplyResult::Applied
    );
}

#[test]
fn configuration_must_match_binance_market_semantics() {
    assert_eq!(
        BinanceBookSynchronizer::try_new(
            BinanceMarket::Spot,
            config(ProductType::Perpetual, SequencePolicy::RangeContainsNext)
        )
        .err(),
        Some(connector_binance::BinanceBookSyncError::InvalidConfig)
    );
    assert_eq!(
        BinanceBookSynchronizer::try_new(
            BinanceMarket::Spot,
            config(ProductType::Spot, SequencePolicy::PreviousFinal)
        )
        .err(),
        Some(connector_binance::BinanceBookSyncError::InvalidConfig)
    );
    assert_eq!(
        BinanceBookSynchronizer::try_new(
            BinanceMarket::UsdMarginedPerpetual,
            config(ProductType::Perpetual, SequencePolicy::RangeContainsNext)
        )
        .err(),
        Some(connector_binance::BinanceBookSyncError::InvalidConfig)
    );
}

#[tokio::test]
async fn bounded_delta_buffer_fails_closed_and_requests_a_new_snapshot() {
    let catalog = catalog();
    let mut bounded = config(ProductType::Spot, SequencePolicy::RangeContainsNext);
    bounded.max_buffered_deltas = 1;
    let mut sync =
        BinanceBookSynchronizer::try_new(BinanceMarket::Spot, bounded).expect("synchronizer");
    sync.start_session(session(1, 1)).expect("session");
    let first = depth_output(
        &catalog,
        BinanceInput::SpotWebSocket,
        SPOT_DEPTH,
        1,
        1,
        1,
        10,
    )
    .await;
    sync.apply_output(&first).expect("first buffered");

    let second_raw = String::from_utf8(SPOT_DEPTH.to_vec())
        .expect("utf8")
        .replace("\"U\": 157", "\"U\": 161")
        .replace("\"u\": 160", "\"u\": 161");
    let second = depth_output(
        &catalog,
        BinanceInput::SpotWebSocket,
        second_raw.as_bytes(),
        1,
        1,
        2,
        11,
    )
    .await;
    assert_eq!(
        sync.apply_output(&second),
        Err(connector_binance::BinanceBookSyncError::Book(
            BookError::BufferCapacity
        ))
    );
    assert_eq!(sync.state(), BookState::Buffering);
    assert_eq!(sync.resnapshot_count(), 1);
    assert_eq!(sync.snapshot(), Err(BookError::Untrusted));
}
