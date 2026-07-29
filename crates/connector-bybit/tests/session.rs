use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::time::Duration;

use connector_bybit::{
    BybitConfig, BybitConnector, BybitInput, BybitMarket, BybitSessionRecord,
    bounded_session_channel, parse_native_message,
};
use connector_core::{
    BookStreamKey, BoundedNormalizedChannel, CancellationChannel, ConnectorCommand,
    ConnectorCommandChannel, ConnectorContext, ConnectorState, ConnectorTermination,
    DurableRawCaptureChannel, DurableRawCaptureClient, DurableRawCaptureReceiver, LifecycleChannel,
    MarketDataConnector, QuarantineReason, RecoverableDisconnect, WalRejectionReason,
    wal_stream_source_identity,
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

const SPOT_TRADES: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/spot-public-trade.json");
const SPOT_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/spot-orderbook-snapshot.json");
const SPOT_DELTA: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/spot-orderbook-delta.json");
const SPOT_RESET: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/spot-orderbook-reset.json");
const RECEIVE_TIME: UnixNanos = UnixNanos::new(1_760_325_052_800_000_000);
const CONNECTION_START: UnixNanos = UnixNanos::new(1_760_325_000_000_000_000);

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "bybit", 1).expect("source")
}

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("decimal")
}

fn catalog() -> std::sync::Arc<instrument_registry::CatalogSnapshot> {
    let mut registry = InstrumentRegistry::new();
    for product_type in [ProductType::Spot, ProductType::Perpetual] {
        let quote =
            AssetId::new(AssetNamespace::Synthetic, "", "", "USDT", 1).expect("quote asset");
        let definition = InstrumentDefinition::new(InstrumentDefinitionInput {
            id: InstrumentId::new_for_product(
                VenueId::new("bybit").expect("venue"),
                "BTCUSDT",
                product_type,
                1,
            )
            .expect("instrument"),
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

fn spot_book_config() -> BookConfig {
    BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("bybit").expect("venue"),
            "BTCUSDT",
            ProductType::Spot,
            1,
        )
        .expect("instrument"),
        price_tick: Price::new(decimal("0.1")).expect("tick"),
        quantity_step: Quantity::new(decimal("0.001")).expect("step"),
        max_levels_per_side: 100,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: SequencePolicy::ExactNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    }
}

fn record(input: BybitInput, sequence: u64, monotonic: u64, payload: &[u8]) -> BybitSessionRecord {
    BybitSessionRecord::try_new(
        input,
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(sequence).expect("sequence"),
        UnixNanos::new(RECEIVE_TIME.value() + i64::try_from(sequence).expect("sequence")),
        monotonic,
        payload.to_vec().into_boxed_slice(),
    )
    .expect("record")
}

fn raw_channel(
    directory: &tempfile::TempDir,
) -> (
    DurableRawCaptureClient,
    DurableRawCaptureReceiver,
    SegmentedWalWriter,
) {
    let metadata = SegmentMetadata::new(
        [8; 16],
        1,
        "schema",
        "installation",
        "build",
        vec![
            StreamDescriptor::new(7, wal_stream_source_identity(&source()), "bybit-public")
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
        8,
        8 * 1024 * 1024,
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
        let (proof, compression) = writer
            .append(
                RecordMetadata {
                    flags: 0,
                    stream_id: capture.stream_id().get(),
                    connection_epoch: capture.connection_epoch().get(),
                    record_sequence: capture.record_sequence().get(),
                    receive_wall_time_ns: capture.receive_wall_time().value(),
                    receive_monotonic_time_ns: capture.receive_monotonic_ns(),
                },
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

fn config(ingestion_instance: &str) -> BybitConfig {
    BybitConfig::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        catalog(),
        NonZeroU64::MIN,
        CONNECTION_START,
        ingestion_instance,
    )
    .expect("config")
}

async fn run_trade_once() -> (
    Vec<connector_core::NormalizedOutput>,
    Vec<ConnectorState>,
    ConnectorTermination,
) {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(2).expect("input");
    input_sender
        .send(record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            1,
            10,
            SPOT_TRADES,
        ))
        .await
        .expect("send");
    drop(input_sender);
    let connector =
        BybitConnector::try_new(config("session-test"), input_receiver).expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(2, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::new(4, 64 * 1024)
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

    let termination = Box::new(connector).run(context).await;
    assert_eq!(wal_task.await.expect("WAL join"), 1);
    let mut outputs = Vec::new();
    while let Some(output) = normalized_receiver.recv().await {
        outputs.push(output);
    }
    let mut states = Vec::new();
    while let Some(event) = lifecycle_receiver.recv().await {
        states.push(event.to());
    }
    (outputs, states, termination)
}

#[tokio::test]
async fn repeated_raw_first_ingestion_produces_identical_canonical_events() {
    let (first, first_states, first_termination) = run_trade_once().await;
    let (second, second_states, second_termination) = run_trade_once().await;

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
    assert_eq!(first, second);
    assert_eq!(first.len(), 2);
    assert_ne!(first[0].event().id(), first[1].event().id());
    assert!(first.iter().all(|output| {
        output.event().metadata().as_unchecked().raw_payload_hash
            == *blake3::hash(SPOT_TRADES).as_bytes()
            && output
                .event()
                .metadata()
                .as_unchecked()
                .receive_wall_timestamp
                == output.raw().receive_wall_time()
    }));
}

#[tokio::test]
async fn verified_wal_replay_rebuilds_the_identical_canonical_events() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    input_sender
        .send(record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            1,
            10,
            SPOT_TRADES,
        ))
        .await
        .expect("send");
    drop(input_sender);
    let replay_catalog = catalog();
    let connector = BybitConnector::try_new(
        BybitConfig::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            std::sync::Arc::clone(&replay_catalog),
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
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::new(4, 64 * 1024)
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
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::RemoteClosed)
    );
    assert_eq!(wal_task.await.expect("WAL join"), 1);
    let mut original = Vec::new();
    while let Some(output) = normalized_receiver.recv().await {
        original.push(output);
    }

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
            assert_eq!(record.stream_name(), "bybit-public");
            let reference =
                connector_core::DurableRawReference::try_from_recovered(source(), &record)
                    .expect("durable replay reference");
            let parsed = connector_bybit::parse_durable_native_message(
                BybitInput::PublicWebSocket(BybitMarket::Spot),
                record.payload(),
                &reference,
            )
            .expect("durable replay parse");
            let normalization = connector_bybit::NormalizationContext::try_new(
                &replay_catalog,
                &reference,
                reference.receive_wall_time(),
                CONNECTION_START,
                NonZeroU64::MIN,
                "wal-replay-test",
            )
            .expect("replay normalization context");
            let events = connector_bybit::normalize_native_message(parsed, &normalization)
                .expect("replay normalization");
            replayed.extend(events.into_iter().map(|event| {
                connector_core::NormalizedOutput::try_new(reference.clone(), event)
                    .expect("replay raw/event linkage")
            }));
        })
        .expect("verified replay");
    assert_eq!(replayed, original);
}

#[tokio::test]
async fn gapped_and_pre_snapshot_book_updates_are_durable_but_never_published() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(8).expect("input");
    let mut gap: serde_json::Value =
        serde_json::from_slice(SPOT_DELTA).expect("delta fixture JSON");
    gap["data"]["u"] = serde_json::json!(103);
    gap["data"]["seq"] = serde_json::json!(9_532_239_403_u64);
    let gap_bytes = serde_json::to_vec(&gap).expect("gap JSON");
    parse_native_message(BybitInput::PublicWebSocket(BybitMarket::Spot), &gap_bytes)
        .expect("synthetic gap must remain schema-valid");
    for record in [
        record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            1,
            10,
            SPOT_DELTA,
        ),
        record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            2,
            20,
            SPOT_SNAPSHOT,
        ),
        record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            3,
            30,
            SPOT_DELTA,
        ),
        record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            4,
            40,
            &gap_bytes,
        ),
        record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            5,
            50,
            SPOT_RESET,
        ),
    ] {
        input_sender.send(record).await.expect("send");
    }
    drop(input_sender);
    let connector = BybitConnector::try_new(
        config("book-session-test")
            .with_book(BybitMarket::Spot, spot_book_config())
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
        let mut outputs = Vec::new();
        while let Some(output) = normalized_receiver.recv().await {
            outputs.push(output);
        }
        outputs
    });
    let termination = connector_task.await.expect("connector join");
    let captured_records = wal_task.await.expect("WAL join");
    assert_eq!(
        (termination, captured_records),
        (
            ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::RemoteClosed),
            5,
        )
    );
    let outputs = output_task.await.expect("output join");
    assert_eq!(outputs.len(), 3);
    assert_eq!(
        outputs
            .iter()
            .map(|output| output.event().metadata().as_unchecked().sequence_number)
            .collect::<Vec<_>>(),
        vec![Some(100), Some(101), Some(1)]
    );
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
async fn cross_sequence_regression_forces_protocol_reconnect_without_publication() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(2).expect("input");
    let mut regression: serde_json::Value =
        serde_json::from_slice(SPOT_DELTA).expect("delta fixture JSON");
    regression["data"]["seq"] = serde_json::json!(9_532_239_399_u64);
    input_sender
        .send(record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            1,
            10,
            SPOT_SNAPSHOT,
        ))
        .await
        .expect("snapshot");
    input_sender
        .send(record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            2,
            20,
            &serde_json::to_vec(&regression).expect("regression JSON"),
        ))
        .await
        .expect("regression");
    let connector = BybitConnector::try_new(
        config("cross-sequence-test")
            .with_book(BybitMarket::Spot, spot_book_config())
            .expect("book"),
        input_receiver,
    )
    .expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::with_book_streams(
        2,
        128 * 1024,
        NonZeroUsize::MIN,
        [BookStreamKey::new(
            source(),
            spot_book_config().instrument,
            NonZeroU64::MIN,
        )],
    )
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
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::ProtocolReset)
    );
    drop(input_sender);
    assert_eq!(wal_task.await.expect("WAL join"), 2);
    let first = normalized_receiver.recv().await.expect("trusted snapshot");
    assert_eq!(
        first.event().metadata().as_unchecked().sequence_number,
        Some(100)
    );
    assert!(normalized_receiver.recv().await.is_none());
}

#[tokio::test]
async fn rejected_wal_record_produces_no_normalized_output() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, mut raw_receiver, _writer) = raw_channel(&directory);
    let (input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    input_sender
        .send(record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            1,
            10,
            SPOT_TRADES,
        ))
        .await
        .expect("send");
    let connector =
        BybitConnector::try_new(config("wal-rejection-test"), input_receiver).expect("connector");
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
    drop(input_sender);
    assert!(normalized_receiver.recv().await.is_none());
}

#[tokio::test]
async fn malformed_payload_is_durable_before_schema_quarantine() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    input_sender
        .send(record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            1,
            10,
            br#"{"unexpected":true}"#,
        ))
        .await
        .expect("send");
    let connector =
        BybitConnector::try_new(config("schema-test"), input_receiver).expect("connector");
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
        ConnectorTermination::Quarantined(QuarantineReason::SchemaViolation)
    );
    drop(input_sender);
    assert_eq!(wal_task.await.expect("WAL join"), 1);
    assert!(normalized_receiver.recv().await.is_none());
    let mut states = Vec::new();
    while let Some(event) = lifecycle_receiver.recv().await {
        states.push(event.to());
    }
    assert_eq!(states.last(), Some(&ConnectorState::Quarantined));
}

#[tokio::test]
async fn heartbeat_loss_degrades_and_disconnects_a_quiet_session() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, None));
    let (_input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    let connector = BybitConnector::try_new(
        config("heartbeat-test")
            .with_heartbeat_timeout(Duration::from_millis(10))
            .expect("heartbeat"),
        input_receiver,
    )
    .expect("connector");
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
            ConnectorState::BackingOff,
        ]
    );
}

#[tokio::test]
async fn shutdown_interrupts_normalized_backpressure_without_orphaning_the_session() {
    let directory = tempfile::tempdir().expect("WAL directory");
    let (raw_client, raw_receiver, writer) = raw_channel(&directory);
    let (notify_sender, mut notify_receiver) = tokio::sync::mpsc::channel(1);
    let wal_task = tokio::spawn(acknowledge_all(raw_receiver, writer, Some(notify_sender)));
    let (input_sender, input_receiver) = bounded_session_channel(1).expect("input");
    input_sender
        .send(record(
            BybitInput::PublicWebSocket(BybitMarket::Spot),
            1,
            10,
            SPOT_TRADES,
        ))
        .await
        .expect("first");
    let connector =
        BybitConnector::try_new(config("backpressure-test"), input_receiver).expect("connector");
    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let (normalized_sink, mut normalized_receiver) = BoundedNormalizedChannel::new(1, 64 * 1024)
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

    let task = tokio::spawn(Box::new(connector).run(context));
    notify_receiver.recv().await.expect("first WAL append");
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
    assert_eq!(wal_task.await.expect("WAL join"), 1);
    assert!(normalized_receiver.recv().await.is_some());
    assert!(normalized_receiver.recv().await.is_none());
}

#[test]
fn records_are_bounded_and_debug_output_never_contains_provider_bytes() {
    assert!(bounded_session_channel(0).is_err());
    assert!(bounded_session_channel(64).is_ok());
    assert!(bounded_session_channel(65).is_err());
    assert!(
        config("heartbeat-bound-test")
            .with_heartbeat_timeout(Duration::from_secs(601))
            .is_err()
    );
    let record = record(
        BybitInput::PublicWebSocket(BybitMarket::Spot),
        1,
        10,
        SPOT_TRADES,
    );
    let debug = format!("{record:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("publicTrade.BTCUSDT"));
}
