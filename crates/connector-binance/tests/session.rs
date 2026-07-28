use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::time::Duration;

use connector_binance::{
    BinanceConfig, BinanceConnector, BinanceInput, BinanceMarket, BinanceSessionRecord,
    BinanceSessionRoute, bounded_session_channel,
};
use connector_core::{
    BookStreamKey, BoundedNormalizedChannel, CancellationChannel, ConnectorCommand,
    ConnectorCommandChannel, ConnectorContext, ConnectorState, ConnectorTermination,
    DurableRawCaptureChannel, DurableRawCaptureClient, DurableRawCaptureReceiver, LifecycleChannel,
    MarketDataConnector, RecoverableDisconnect, WalRejectionReason, wal_stream_source_identity,
};
use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use instrument_registry::{InstrumentRegistry, RevisionMetadata};
use orderbook::{BookConfig, ChecksumPolicy, SequencePolicy};
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter},
    prologue::{SegmentMetadata, StreamDescriptor},
};

const SPOT_TRADE: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-aggregate-trade.json");
const SPOT_DEPTH: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-depth-update.json");
const SPOT_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/spot-depth-snapshot.json");
const USDM_DEPTH: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-depth-update.json");
const USDM_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/binance/usdm-depth-snapshot.json");
const RECEIVE_TIME: UnixNanos = UnixNanos::new(1_672_515_782_137_000_000);
const CONNECTION_START: UnixNanos = UnixNanos::new(1_672_515_700_000_000_000);

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "binance", 1).expect("source")
}

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("decimal")
}

fn catalog() -> std::sync::Arc<instrument_registry::CatalogSnapshot> {
    let mut registry = InstrumentRegistry::new();
    for product_type in [ProductType::Spot, ProductType::Perpetual] {
        let quote = AssetId::new(AssetNamespace::Synthetic, "", "", "USDT", 1).expect("quote");
        let definition = InstrumentDefinition::new(InstrumentDefinitionInput {
            id: InstrumentId::new_for_product(
                VenueId::new("binance").expect("venue"),
                "BTCUSDT",
                product_type,
                1,
            )
            .expect("instrument"),
            product_type,
            base_asset: AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1)
                .expect("base"),
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
        .expect("definition");
        registry
            .append_definition(
                definition,
                RevisionMetadata::try_new(UnixNanos::new(1), "session-fixture").expect("revision"),
            )
            .expect("append definition");
    }
    registry.snapshot().expect("catalog")
}

fn record(sequence: u64, monotonic: u64) -> BinanceSessionRecord {
    BinanceSessionRecord::try_new(
        BinanceSessionRoute::Native(BinanceInput::SpotWebSocket),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(sequence).expect("sequence"),
        UnixNanos::new(RECEIVE_TIME.value() + i64::try_from(sequence).expect("sequence")),
        monotonic,
        SPOT_TRADE.to_vec().into_boxed_slice(),
    )
    .expect("record")
}

fn routed_record(
    route: BinanceSessionRoute,
    sequence: u64,
    monotonic: u64,
    payload: &[u8],
) -> BinanceSessionRecord {
    BinanceSessionRecord::try_new(
        route,
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(sequence).expect("sequence"),
        UnixNanos::new(RECEIVE_TIME.value() + i64::try_from(sequence).expect("sequence")),
        monotonic,
        payload.to_vec().into_boxed_slice(),
    )
    .expect("record")
}

fn spot_book_config() -> BookConfig {
    BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("binance").expect("venue"),
            "BTCUSDT",
            ProductType::Spot,
            1,
        )
        .expect("instrument"),
        price_tick: Price::new(decimal("0.01")).expect("tick"),
        quantity_step: Quantity::new(decimal("0.001")).expect("step"),
        max_levels_per_side: 100,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: SequencePolicy::RangeContainsNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    }
}

fn usdm_book_config() -> BookConfig {
    BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("binance").expect("venue"),
            "BTCUSDT",
            ProductType::Perpetual,
            1,
        )
        .expect("instrument"),
        price_tick: Price::new(decimal("0.01")).expect("tick"),
        quantity_step: Quantity::new(decimal("0.001")).expect("step"),
        max_levels_per_side: 100,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: SequencePolicy::PreviousFinal,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    }
}

fn raw_channel(
    directory: &tempfile::TempDir,
) -> (
    DurableRawCaptureClient,
    DurableRawCaptureReceiver,
    SegmentedWalWriter,
) {
    let metadata = SegmentMetadata::new(
        [7; 16],
        1,
        "schema",
        "installation",
        "build",
        vec![
            StreamDescriptor::new(7, wal_stream_source_identity(&source()), "spot-trades")
                .expect("stream"),
        ],
    )
    .expect("segment");
    let writer =
        SegmentedWalWriter::create(directory.path(), metadata, RotationPolicy::default(), 1)
            .expect("writer");
    let channel = DurableRawCaptureChannel::new(
        writer.append_authority(),
        source(),
        NonZeroU32::new(7).expect("stream"),
        4,
        4 * 1024 * 1024,
    )
    .expect("raw channel");
    let (client, receiver) = channel.split();
    (client, receiver, writer)
}

async fn acknowledge_all(
    mut receiver: DurableRawCaptureReceiver,
    mut writer: SegmentedWalWriter,
    notify: Option<tokio::sync::mpsc::Sender<()>>,
) -> usize {
    let mut count = 0;
    while let Some(pending) = receiver.recv().await {
        let capture = pending.capture();
        let metadata = RecordMetadata {
            flags: 0,
            stream_id: capture.stream_id().get(),
            connection_epoch: capture.connection_epoch().get(),
            record_sequence: capture.record_sequence().get(),
            receive_wall_time_ns: capture.receive_wall_time().value(),
            receive_monotonic_time_ns: capture.receive_monotonic_ns(),
        };
        let (proof, compression) = writer
            .append(
                metadata,
                capture.payload(),
                capture.receive_monotonic_ns(),
                capture.receive_wall_time().value(),
            )
            .expect("WAL append")
            .into_parts();
        assert!(compression.is_none());
        pending.acknowledge(proof).expect("acknowledge");
        count += 1;
        if let Some(notify) = &notify {
            notify.send(()).await.expect("notify");
        }
    }
    count
}

async fn run_once() -> (
    connector_core::NormalizedOutput,
    Vec<ConnectorState>,
    ConnectorTermination,
) {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(2).expect("input");
    input_sender.send(record(1, 10)).await.expect("send");
    drop(input_sender);

    let config = BinanceConfig::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        catalog(),
        NonZeroU64::new(1).expect("subscription"),
        CONNECTION_START,
        "session-test",
    )
    .expect("config");
    let connector = BinanceConnector::try_new(config, input_receiver).expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(2, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::new(4, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, mut lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::new(1).expect("connection"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );

    let connector_task = tokio::spawn(Box::new(connector).run(context));
    let output = normalized_receiver.recv().await.expect("normalized output");
    let termination = connector_task.await.expect("connector join");
    assert_eq!(wal_task.await.expect("WAL join"), 1);
    let mut states = Vec::new();
    while let Some(event) = lifecycle_receiver.recv().await {
        states.push(event.to());
    }
    (output, states, termination)
}

#[tokio::test]
async fn repeated_raw_first_ingestion_produces_the_same_canonical_event() {
    let (first, first_states, first_termination) = run_once().await;
    let (second, second_states, second_termination) = run_once().await;

    assert_eq!(
        first_termination,
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::RemoteClosed)
    );
    assert_eq!(second_termination, first_termination);
    assert_eq!(
        first_states,
        vec![
            ConnectorState::Connecting,
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::Synchronizing,
            ConnectorState::Healthy,
            ConnectorState::BackingOff,
        ]
    );
    assert_eq!(second_states, first_states);
    assert_eq!(first.event(), second.event());
    assert_eq!(
        first.event().metadata().as_unchecked().raw_payload_hash,
        *blake3::hash(SPOT_TRADE).as_bytes()
    );
    assert_eq!(
        first
            .event()
            .metadata()
            .as_unchecked()
            .receive_wall_timestamp,
        first.raw().receive_wall_time()
    );
}

#[tokio::test]
async fn recovered_wal_record_replays_to_the_identical_canonical_event() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(2).expect("input");
    input_sender.send(record(1, 10)).await.expect("send");
    drop(input_sender);
    let catalog = catalog();
    let connector = BinanceConnector::try_new(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            std::sync::Arc::clone(&catalog),
            NonZeroU64::MIN,
            CONNECTION_START,
            "wal-replay-test",
        )
        .expect("config"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::new(2, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::MIN,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );

    let connector_task = tokio::spawn(Box::new(connector).run(context));
    let original = normalized_receiver.recv().await.expect("original output");
    assert_eq!(
        connector_task.await.expect("connector join"),
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::RemoteClosed)
    );
    assert_eq!(wal_task.await.expect("WAL join"), 1);

    let recovered = SegmentedWalWriter::recover(
        directory.path(),
        RotationPolicy::default(),
        100,
        RECEIVE_TIME.value() + 100,
    )
    .expect("recover WAL");
    let mut replayed = Vec::new();
    recovered
        .visit_verified_records(|record| {
            assert_eq!(record.stream_name(), "spot-trades");
            let reference =
                connector_core::DurableRawReference::try_from_recovered(source(), &record)
                    .expect("durable replay reference");
            let parsed = connector_binance::parse_durable_native_message(
                BinanceInput::SpotWebSocket,
                record.payload(),
                &reference,
            )
            .expect("durable replay parse");
            let normalization = connector_binance::NormalizationContext::try_new(
                &catalog,
                &reference,
                reference.receive_wall_time(),
                CONNECTION_START,
                NonZeroU64::MIN,
                "wal-replay-test",
            )
            .expect("replay normalization context");
            let events = connector_binance::normalize_native_message(parsed, &normalization)
                .expect("replay normalization");
            replayed.extend(events.into_iter().map(|event| {
                connector_core::NormalizedOutput::try_new(reference.clone(), event)
                    .expect("replay raw/event linkage")
            }));
        })
        .expect("verified replay");
    assert_eq!(replayed, vec![original]);
}

#[tokio::test]
async fn book_session_reaches_healthy_and_recovers_only_after_a_fresh_snapshot() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(8).expect("input");
    let gap = String::from_utf8(SPOT_DEPTH.to_vec())
        .expect("utf8")
        .replace("\"U\": 157", "\"U\": 162")
        .replace("\"u\": 160", "\"u\": 165");
    let recovery_delta = String::from_utf8(SPOT_DEPTH.to_vec())
        .expect("utf8")
        .replace("\"U\": 157", "\"U\": 166")
        .replace("\"u\": 160", "\"u\": 166");
    let recovery_snapshot = String::from_utf8(SPOT_SNAPSHOT.to_vec())
        .expect("utf8")
        .replace("\"lastUpdateId\": 156", "\"lastUpdateId\": 165");
    for record in [
        routed_record(
            BinanceSessionRoute::Native(BinanceInput::SpotWebSocket),
            1,
            10,
            SPOT_DEPTH,
        ),
        routed_record(
            BinanceSessionRoute::DepthSnapshot {
                market: BinanceMarket::Spot,
                symbol: "BTCUSDT".to_owned(),
            },
            2,
            20,
            SPOT_SNAPSHOT,
        ),
        routed_record(
            BinanceSessionRoute::Native(BinanceInput::SpotWebSocket),
            3,
            21,
            gap.as_bytes(),
        ),
        routed_record(
            BinanceSessionRoute::Native(BinanceInput::SpotWebSocket),
            4,
            22,
            recovery_delta.as_bytes(),
        ),
        routed_record(
            BinanceSessionRoute::DepthSnapshot {
                market: BinanceMarket::Spot,
                symbol: "BTCUSDT".to_owned(),
            },
            5,
            23,
            recovery_snapshot.as_bytes(),
        ),
    ] {
        input_sender.send(record).await.expect("send");
    }
    drop(input_sender);

    let connector = BinanceConnector::try_new(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            catalog(),
            NonZeroU64::MIN,
            CONNECTION_START,
            "book-session-test",
        )
        .expect("config")
        .with_book(BinanceMarket::Spot, spot_book_config())
        .expect("book"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::with_book_streams(
        8,
        512 * 1024,
        NonZeroUsize::MIN,
        [BookStreamKey::new(
            source(),
            spot_book_config().instrument,
            NonZeroU64::MIN,
        )],
    )
    .expect("normalized")
    .split();
    let (lifecycle_sink, mut lifecycle_receiver) =
        LifecycleChannel::bounded(16).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::MIN,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );

    let connector_task = tokio::spawn(Box::new(connector).run(context));
    let output_task = tokio::spawn(async move {
        let mut count = 0;
        while normalized_receiver.recv().await.is_some() {
            count += 1;
        }
        count
    });
    assert_eq!(
        connector_task.await.expect("connector join"),
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::RemoteClosed)
    );
    assert_eq!(wal_task.await.expect("WAL join"), 5);
    assert_eq!(output_task.await.expect("output join"), 5);
    let mut states = Vec::new();
    while let Some(event) = lifecycle_receiver.recv().await {
        states.push(event.to());
    }
    assert_eq!(
        states,
        vec![
            ConnectorState::Connecting,
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::Synchronizing,
            ConnectorState::Healthy,
            ConnectorState::Degraded,
            ConnectorState::Recovering,
            ConnectorState::Synchronizing,
            ConnectorState::Healthy,
            ConnectorState::BackingOff,
        ]
    );
}

#[tokio::test]
async fn multi_book_session_waits_until_every_configured_book_is_synchronized() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(8).expect("input");
    for record in [
        routed_record(
            BinanceSessionRoute::Native(BinanceInput::SpotWebSocket),
            1,
            10,
            SPOT_DEPTH,
        ),
        routed_record(
            BinanceSessionRoute::DepthSnapshot {
                market: BinanceMarket::Spot,
                symbol: "BTCUSDT".to_owned(),
            },
            2,
            20,
            SPOT_SNAPSHOT,
        ),
        routed_record(
            BinanceSessionRoute::Native(BinanceInput::UsdMPublicWebSocket),
            3,
            30,
            USDM_DEPTH,
        ),
        routed_record(
            BinanceSessionRoute::DepthSnapshot {
                market: BinanceMarket::UsdMarginedPerpetual,
                symbol: "BTCUSDT".to_owned(),
            },
            4,
            40,
            USDM_SNAPSHOT,
        ),
    ] {
        input_sender.send(record).await.expect("send");
    }
    drop(input_sender);

    let connector = BinanceConnector::try_new(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            catalog(),
            NonZeroU64::MIN,
            CONNECTION_START,
            "multi-book-session-test",
        )
        .expect("config")
        .with_book(BinanceMarket::Spot, spot_book_config())
        .expect("spot book")
        .with_book(BinanceMarket::UsdMarginedPerpetual, usdm_book_config())
        .expect("USD-M book"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::with_book_streams(
        8,
        512 * 1024,
        NonZeroUsize::new(2).expect("capacity"),
        [
            BookStreamKey::new(source(), spot_book_config().instrument, NonZeroU64::MIN),
            BookStreamKey::new(source(), usdm_book_config().instrument, NonZeroU64::MIN),
        ],
    )
    .expect("normalized")
    .split();
    let (lifecycle_sink, mut lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::MIN,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let connector_task = tokio::spawn(Box::new(connector).run(context));
    let output_task = tokio::spawn(async move {
        let mut count = 0;
        while normalized_receiver.recv().await.is_some() {
            count += 1;
        }
        count
    });

    assert_eq!(
        connector_task.await.expect("connector join"),
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::RemoteClosed)
    );
    assert_eq!(wal_task.await.expect("WAL join"), 4);
    assert_eq!(output_task.await.expect("output join"), 4);
    let mut events = Vec::new();
    while let Some(event) = lifecycle_receiver.recv().await {
        events.push(event);
    }
    assert_eq!(
        events.iter().map(|event| event.to()).collect::<Vec<_>>(),
        vec![
            ConnectorState::Connecting,
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::Synchronizing,
            ConnectorState::Healthy,
            ConnectorState::BackingOff,
        ]
    );
    assert_eq!(
        events
            .iter()
            .find(|event| event.to() == ConnectorState::Healthy)
            .expect("healthy transition")
            .observed_at(),
        UnixNanos::new(RECEIVE_TIME.value() + 4)
    );
}

#[tokio::test]
async fn rejected_wal_record_produces_no_normalized_output() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, mut raw_receiver, _writer) = raw_channel(&directory);
    let (input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    input_sender.send(record(1, 10)).await.expect("send");

    let connector = BinanceConnector::try_new(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            catalog(),
            NonZeroU64::new(1).expect("subscription"),
            CONNECTION_START,
            "wal-rejection-test",
        )
        .expect("config"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::new(1, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::new(1).expect("connection"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let task = tokio::spawn(Box::new(connector).run(context));
    raw_receiver
        .recv()
        .await
        .expect("pending raw")
        .reject(WalRejectionReason::StorageUnavailable)
        .expect("reject");
    assert_eq!(
        task.await.expect("join"),
        ConnectorTermination::LocalWalFailure(WalRejectionReason::StorageUnavailable)
    );
    assert!(normalized_receiver.recv().await.is_none());
}

#[tokio::test]
async fn shutdown_interrupts_normalized_backpressure_without_orphaning_the_session() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let (notify_sender, mut notify_receiver) = tokio::sync::mpsc::channel(2);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, Some(notify_sender)));
    let (input_sender, input_receiver) = bounded_session_channel(2).expect("input");
    input_sender.send(record(1, 10)).await.expect("first");
    input_sender.send(record(2, 11)).await.expect("second");

    let connector = BinanceConnector::try_new(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            catalog(),
            NonZeroU64::new(1).expect("subscription"),
            CONNECTION_START,
            "backpressure-test",
        )
        .expect("config"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::new(1, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::new(1).expect("connection"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let task = tokio::spawn(Box::new(connector).run(context));
    notify_receiver.recv().await.expect("first WAL append");
    notify_receiver.recv().await.expect("second WAL append");
    let command_id = NonZeroU64::new(9).expect("command");
    commands
        .send(ConnectorCommand::Shutdown { id: command_id })
        .await
        .expect("shutdown");
    assert_eq!(
        task.await.expect("join"),
        ConnectorTermination::RequestedShutdown { command_id }
    );
    drop(input_sender);
    assert_eq!(wal_task.await.expect("WAL join"), 2);
    assert!(normalized_receiver.recv().await.is_some());
    assert!(normalized_receiver.recv().await.is_none());
}

#[tokio::test]
async fn session_renews_before_the_venue_connection_lifetime() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (_input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    let config = BinanceConfig::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        catalog(),
        NonZeroU64::new(1).expect("subscription"),
        CONNECTION_START,
        "renewal-test",
    )
    .expect("config")
    .with_renewal_after(Duration::from_millis(10))
    .expect("renewal");
    let connector = BinanceConnector::try_new(config, input_receiver).expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(1, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, mut lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::new(1).expect("connection"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );

    assert_eq!(
        Box::new(connector).run(context).await,
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::ConnectionExpired)
    );
    assert_eq!(wal_task.await.expect("WAL join"), 0);
    let mut events = Vec::new();
    while let Some(event) = lifecycle_receiver.recv().await {
        events.push(event);
    }
    assert_eq!(
        events.iter().map(|event| event.to()).collect::<Vec<_>>(),
        vec![
            ConnectorState::Connecting,
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::Synchronizing,
            ConnectorState::Healthy,
            ConnectorState::BackingOff,
        ]
    );
    assert_eq!(
        events.last().expect("renewal lifecycle").observed_at(),
        UnixNanos::new(CONNECTION_START.value() + 10_000_000)
    );
}

#[tokio::test]
async fn heartbeat_loss_degrades_and_disconnects_the_session() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (_input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    let config = BinanceConfig::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        catalog(),
        NonZeroU64::MIN,
        CONNECTION_START,
        "heartbeat-test",
    )
    .expect("config")
    .with_heartbeat_timeout(Duration::from_millis(10))
    .expect("heartbeat");
    let connector = BinanceConnector::try_new(config, input_receiver).expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(1, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, mut lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::MIN,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );

    assert_eq!(
        Box::new(connector).run(context).await,
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::TransportUnavailable)
    );
    assert_eq!(wal_task.await.expect("WAL join"), 0);
    let mut events = Vec::new();
    while let Some(event) = lifecycle_receiver.recv().await {
        events.push(event);
    }
    assert_eq!(
        events.iter().map(|event| event.to()).collect::<Vec<_>>(),
        vec![
            ConnectorState::Connecting,
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::Synchronizing,
            ConnectorState::Healthy,
            ConnectorState::Degraded,
            ConnectorState::BackingOff,
        ]
    );
    assert_eq!(
        events.last().expect("backoff lifecycle").observed_at(),
        UnixNanos::new(CONNECTION_START.value() + 10_000_000)
    );
}

#[test]
fn session_channel_rejects_zero_and_unbounded_record_capacity() {
    assert!(bounded_session_channel(0).is_err());
    assert!(bounded_session_channel(64).is_ok());
    assert!(bounded_session_channel(65).is_err());
    assert!(bounded_session_channel(usize::MAX).is_err());
    assert!(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::MIN,
            catalog(),
            NonZeroU64::MIN,
            CONNECTION_START,
            "heartbeat-bound-test",
        )
        .expect("base config")
        .with_heartbeat_timeout(Duration::from_secs(601))
        .is_err()
    );
}

#[tokio::test]
async fn a_record_for_another_wal_stream_fails_before_raw_capture() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, mut raw_receiver, _writer) = raw_channel(&directory);
    let (input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    input_sender
        .send(
            BinanceSessionRecord::try_new(
                BinanceSessionRoute::Native(BinanceInput::SpotWebSocket),
                NonZeroU32::new(8).expect("foreign stream"),
                NonZeroU64::MIN,
                RECEIVE_TIME,
                10,
                SPOT_TRADE.to_vec().into_boxed_slice(),
            )
            .expect("record"),
        )
        .await
        .expect("send");

    let connector = BinanceConnector::try_new(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            catalog(),
            NonZeroU64::MIN,
            CONNECTION_START,
            "stream-binding-test",
        )
        .expect("config"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(1, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::MIN,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );

    assert_eq!(
        Box::new(connector).run(context).await,
        ConnectorTermination::Fatal(connector_core::FatalConnectorError::InvalidConfiguration)
    );
    assert!(raw_receiver.recv().await.is_none());
}

#[tokio::test]
async fn schema_violation_is_not_mislabeled_as_repeated() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    input_sender
        .send(
            BinanceSessionRecord::try_new(
                BinanceSessionRoute::Native(BinanceInput::SpotWebSocket),
                NonZeroU32::new(7).expect("stream"),
                NonZeroU64::MIN,
                RECEIVE_TIME,
                10,
                br#"{"unexpected":true}"#.to_vec().into_boxed_slice(),
            )
            .expect("record"),
        )
        .await
        .expect("send");

    let connector = BinanceConnector::try_new(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            catalog(),
            NonZeroU64::MIN,
            CONNECTION_START,
            "schema-test",
        )
        .expect("config"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::new(1, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, mut lifecycle_receiver) = LifecycleChannel::bounded(8).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::MIN,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );

    assert_eq!(
        Box::new(connector).run(context).await,
        ConnectorTermination::Quarantined(connector_core::QuarantineReason::SchemaViolation)
    );
    assert_eq!(wal_task.await.expect("WAL join"), 1);
    assert!(normalized_receiver.recv().await.is_none());
    let mut states = Vec::new();
    while let Some(event) = lifecycle_receiver.recv().await {
        states.push(event.to());
    }
    assert_eq!(
        states,
        vec![
            ConnectorState::Connecting,
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::Synchronizing,
            ConnectorState::Healthy,
            ConnectorState::Quarantined,
        ]
    );
}

#[tokio::test]
async fn lifecycle_output_failure_is_not_hidden_by_remote_close() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, _raw_receiver, _writer) = raw_channel(&directory);
    let (input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    let connector = BinanceConnector::try_new(
        BinanceConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            catalog(),
            NonZeroU64::MIN,
            CONNECTION_START,
            "lifecycle-failure-test",
        )
        .expect("config"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(1, 64 * 1024)
        .expect("normalized")
        .split();
    let (lifecycle_sink, mut lifecycle_receiver) = LifecycleChannel::bounded(4).expect("lifecycle");
    let context = ConnectorContext::new(
        source(),
        NonZeroU64::MIN,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let task = tokio::spawn(Box::new(connector).run(context));
    for _ in 0..4 {
        lifecycle_receiver.recv().await.expect("startup lifecycle");
    }
    drop(lifecycle_receiver);
    drop(input_sender);
    assert_eq!(
        task.await.expect("join"),
        ConnectorTermination::LocalOutputFailed
    );
}
