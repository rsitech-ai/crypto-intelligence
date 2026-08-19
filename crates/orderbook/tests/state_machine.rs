use event_envelope::{
    BookDelta, BookLevel, BookSnapshot, EventEnvelope, QualityFlags, SnapshotKind,
    UncheckedEventMetadata, UncheckedEventPayload,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{
    ApplyResult, BookClassification, BookConfig, BookError, BookEventApplyOutcome, BookSession,
    BookState, ChecksumPolicy, OrderBookEngine, SequencePolicy, SnapshotStrategy,
};

#[test]
fn buffered_range_replay_aligns_snapshot_and_publishes_once_trusted() {
    let mut engine = engine();
    let session = session(1, 1, 1);
    engine
        .start_session(session, SnapshotStrategy::ExternalBuffered)
        .expect("session starts");
    assert_eq!(engine.state(), BookState::Buffering);

    assert_eq!(
        engine
            .apply_delta(delta(101, 102, &[("100", "3")], &[]), session, 10)
            .expect("delta buffers"),
        ApplyResult::SnapshotRequired
    );
    assert_eq!(engine.snapshot(), Err(BookError::Untrusted));

    assert_eq!(
        engine
            .apply_snapshot(snapshot(100, &[("100", "2")], &[("101", "4")]), session, 20)
            .expect("snapshot and buffered range align"),
        ApplyResult::Applied
    );
    assert_eq!(engine.state(), BookState::Synchronized);
    let view = engine.snapshot().expect("synchronized book is publishable");
    assert_eq!(view.last_source_sequence(), 102);
    assert_eq!(view.best_bid().expect("best bid").quantity.to_string(), "3");
    assert_eq!(view.classification(), BookClassification::Normal);
}

#[test]
fn applied_event_receipt_binds_the_event_to_exact_post_apply_state() {
    let mut engine = engine();
    engine
        .start_session(session(1, 1, 1), SnapshotStrategy::StreamSnapshot)
        .unwrap();
    let snapshot = book_event(
        UncheckedEventPayload::BookSnapshot(snapshot(10, &[("100", "2")], &[("101", "4")])),
        10,
        None,
        SnapshotKind::Snapshot,
        10,
        1,
    );
    let snapshot_receipt = match engine.apply_event_with_receipt(&snapshot).unwrap() {
        BookEventApplyOutcome::Applied(receipt) => receipt,
        other => panic!("snapshot must produce an applied receipt, got {other:?}"),
    };
    assert_eq!(snapshot_receipt.event_id(), snapshot.id());
    assert_eq!(snapshot_receipt.snapshot().last_source_sequence(), 10);
    assert_eq!(snapshot_receipt.snapshot().price_tick(), price("0.1"));
    assert_eq!(snapshot_receipt.snapshot().quantity_step(), quantity("0.1"));

    let delta = book_event(
        UncheckedEventPayload::BookDelta(delta(11, 11, &[("100", "9")], &[])),
        11,
        Some(10),
        SnapshotKind::Delta,
        20,
        2,
    );
    let delta_receipt = match engine.apply_event_with_receipt(&delta).unwrap() {
        BookEventApplyOutcome::Applied(receipt) => receipt,
        other => panic!("delta must produce an applied receipt, got {other:?}"),
    };
    assert_eq!(delta_receipt.event_id(), delta.id());
    assert_eq!(delta_receipt.snapshot().last_source_sequence(), 11);
    assert_eq!(
        delta_receipt
            .snapshot()
            .best_bid()
            .expect("post-apply bid")
            .quantity
            .to_string(),
        "9"
    );

    assert_eq!(
        engine.apply_event_with_receipt(&delta).unwrap(),
        BookEventApplyOutcome::NotApplied(ApplyResult::Duplicate)
    );
}

#[test]
fn buffered_replay_applies_without_forging_a_single_event_receipt() {
    let mut engine = engine();
    engine
        .start_session(session(1, 1, 1), SnapshotStrategy::ExternalBuffered)
        .unwrap();
    let delta = book_event(
        UncheckedEventPayload::BookDelta(delta(11, 11, &[("100", "9")], &[])),
        11,
        Some(10),
        SnapshotKind::Delta,
        10,
        1,
    );
    assert_eq!(
        engine.apply_event_with_receipt(&delta).unwrap(),
        BookEventApplyOutcome::NotApplied(ApplyResult::SnapshotRequired)
    );

    let snapshot = book_event(
        UncheckedEventPayload::BookSnapshot(snapshot(10, &[("100", "2")], &[("101", "4")])),
        10,
        None,
        SnapshotKind::Snapshot,
        20,
        2,
    );
    assert_eq!(
        engine.apply_event_with_receipt(&snapshot).unwrap(),
        BookEventApplyOutcome::AppliedWithoutReceipt
    );
    assert_eq!(
        engine.snapshot().unwrap().last_source_sequence(),
        11,
        "post-state includes the buffered delta and cannot use snapshot-only lineage"
    );
}

#[test]
fn sequence_gap_invalidates_book_until_fresh_snapshot() {
    let mut engine = synchronized_external_at(10);
    let session = session(1, 1, 1);

    assert_eq!(
        engine
            .apply_delta(delta(12, 12, &[("100", "3")], &[]), session, 20)
            .expect("gaps are state results"),
        ApplyResult::GapDetected
    );
    assert_eq!(engine.state(), BookState::Untrusted);
    assert_eq!(engine.snapshot(), Err(BookError::Untrusted));
    assert_eq!(
        engine.apply_snapshot(snapshot(20, &[("100", "5")], &[("101", "4")]), session, 19,),
        Err(BookError::MonotonicTimeRegression)
    );

    engine
        .begin_resync()
        .expect("external recovery begins buffering before snapshot request");
    assert_eq!(
        engine
            .apply_delta(delta(21, 21, &[("100", "6")], &[]), session, 25)
            .expect("in-flight recovery delta buffers"),
        ApplyResult::SnapshotRequired
    );
    assert_eq!(
        engine
            .apply_snapshot(snapshot(20, &[("100", "5")], &[("101", "4")]), session, 30)
            .expect("fresh snapshot aligns and replays recovery deltas"),
        ApplyResult::Applied
    );
    assert_eq!(engine.state(), BookState::Synchronized);
    let recovered = engine
        .snapshot_at(1_500_030)
        .expect("fresh snapshot is trusted");
    assert_eq!(recovered.last_source_sequence(), 21);
    assert_eq!(recovered.best_bid().unwrap().quantity.to_string(), "6");
    assert_eq!(recovered.quality().last_update_age_ms, 1);
    assert_eq!(recovered.quality().resync_count_1h, 1);
    assert_eq!(recovered.quality().missing_sequence_count_1h, 1);

    let aged = engine
        .snapshot_at(3_600_000_000_031)
        .expect("trusted book remains readable after the incident window");
    assert_eq!(aged.quality().resync_count_1h, 0);
    assert_eq!(aged.quality().missing_sequence_count_1h, 0);
}

#[test]
fn stale_connection_epoch_and_generation_are_rejected_without_mutation() {
    let mut engine = synchronized_at(10);
    let before = engine.snapshot().expect("initial book");

    assert_eq!(
        engine.apply_delta(delta(11, 11, &[("100", "9")], &[]), session(0, 1, 1), 20),
        Err(BookError::InvalidEpoch)
    );
    assert_eq!(
        engine.apply_delta(delta(11, 11, &[("100", "9")], &[]), session(1, 1, 2), 20),
        Err(BookError::StaleInstrumentGeneration)
    );
    assert_eq!(engine.snapshot().expect("book remains trusted"), before);
}

#[test]
fn same_symbol_spot_and_perpetual_events_never_cross_instrument_shards() {
    let spot_event = product_snapshot_event(ProductType::Spot);
    let perpetual_event = product_snapshot_event(ProductType::Perpetual);
    let mut spot_engine =
        OrderBookEngine::new(book_config_for(ProductType::Spot)).expect("spot config");
    let mut perpetual_engine =
        OrderBookEngine::new(book_config_for(ProductType::Perpetual)).expect("perpetual config");

    assert_eq!(
        spot_engine.apply_event(&perpetual_event),
        Err(BookError::WrongInstrument)
    );
    assert_eq!(
        perpetual_engine.apply_event(&spot_event),
        Err(BookError::WrongInstrument)
    );
}

#[test]
fn duplicate_is_ignored_but_reordered_overlap_invalidates() {
    let mut engine = synchronized_with_policy(10, SequencePolicy::ExactNext);
    let session = session(1, 1, 1);
    let before = engine.snapshot().expect("initial book");

    assert_eq!(
        engine
            .apply_delta(delta(9, 10, &[("100", "9")], &[]), session, 20)
            .expect("old range is a duplicate"),
        ApplyResult::Duplicate
    );
    assert_eq!(
        engine.snapshot().expect("duplicate preserves state"),
        before
    );

    assert_eq!(
        engine
            .apply_delta(delta(10, 11, &[("100", "9")], &[]), session, 21)
            .expect("stale overlap is an integrity result"),
        ApplyResult::GapDetected
    );
    assert_eq!(engine.snapshot(), Err(BookError::Untrusted));
}

#[test]
fn range_containing_next_is_valid_for_live_binance_spot_updates() {
    let mut engine = synchronized_at(10);
    let session = session(1, 1, 1);

    assert_eq!(
        engine
            .apply_delta(delta(10, 11, &[("100", "9")], &[]), session, 20)
            .unwrap(),
        ApplyResult::Applied
    );
    assert_eq!(engine.snapshot().unwrap().last_source_sequence(), 11);
}

#[test]
fn previous_final_policy_requires_the_connector_chain_after_alignment() {
    let session = session(1, 1, 1);
    let mut engine = OrderBookEngine::new(BookConfig {
        sequence_policy: SequencePolicy::PreviousFinal,
        ..book_config()
    })
    .unwrap();
    engine
        .start_session(session, SnapshotStrategy::ExternalBuffered)
        .unwrap();
    engine
        .apply_delta_with_previous(delta(10, 11, &[("100", "3")], &[]), session, 2, 9)
        .unwrap();
    engine
        .apply_snapshot(snapshot(10, &[("100", "2")], &[("101", "4")]), session, 3)
        .unwrap();

    assert_eq!(
        engine
            .apply_delta_with_previous(delta(11, 12, &[("100", "4")], &[]), session, 4, 11)
            .unwrap(),
        ApplyResult::Applied
    );
    assert_eq!(
        engine
            .apply_delta_with_previous(delta(12, 13, &[("100", "5")], &[]), session, 5, 10)
            .unwrap(),
        ApplyResult::GapDetected
    );
    assert_eq!(engine.state(), BookState::Untrusted);
}

#[test]
fn previous_final_policy_accepts_a_native_futures_batch_range() {
    let session = session(1, 1, 1);
    let mut engine = OrderBookEngine::new(BookConfig {
        sequence_policy: SequencePolicy::PreviousFinal,
        ..book_config()
    })
    .unwrap();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();
    engine
        .apply_snapshot(snapshot(149, &[("100", "2")], &[("101", "4")]), session, 1)
        .unwrap();

    assert_eq!(
        engine
            .apply_delta_with_previous(delta(157, 160, &[("100", "3")], &[]), session, 2, 149,)
            .unwrap(),
        ApplyResult::Applied
    );
    assert_eq!(engine.snapshot().unwrap().last_source_sequence(), 160);
}

#[test]
fn stream_snapshot_reset_discards_pre_snapshot_deltas() {
    let session = session(1, 1, 1);
    let mut engine = engine();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();
    engine
        .apply_delta(delta(100, 100, &[("100", "9")], &[]), session, 1)
        .unwrap();

    engine
        .apply_snapshot(snapshot(1, &[("100", "2")], &[("101", "4")]), session, 2)
        .expect("Bybit-style stream snapshot replaces all buffered state");
    let view = engine.snapshot().unwrap();
    assert_eq!(view.last_source_sequence(), 1);
    assert_eq!(view.best_bid().unwrap().quantity.to_string(), "2");
}

#[test]
fn locked_and_crossed_books_are_classified_without_repair() {
    let session = session(1, 1, 1);
    let mut locked = engine();
    locked
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .expect("session starts");
    locked
        .apply_snapshot(snapshot(1, &[("100", "1")], &[("100", "2")]), session, 1)
        .expect("locked snapshot is retained");
    let locked_view = locked
        .snapshot()
        .expect("locked state is explicit and trusted");
    assert_eq!(locked_view.classification(), BookClassification::Locked);
    assert_eq!(
        locked_view.best_bid().expect("bid").price.to_string(),
        "100"
    );
    assert_eq!(
        locked_view.best_ask().expect("ask").price.to_string(),
        "100"
    );

    let mut crossed = engine();
    crossed
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .expect("session starts");
    crossed
        .apply_snapshot(snapshot(1, &[("101", "1")], &[("100", "2")]), session, 1)
        .expect("crossed snapshot is retained");
    let crossed_view = crossed
        .snapshot()
        .expect("crossed state is explicit and trusted");
    assert_eq!(crossed_view.classification(), BookClassification::Crossed);
    assert_eq!(
        crossed_view.best_bid().expect("bid").price.to_string(),
        "101"
    );
    assert_eq!(
        crossed_view.best_ask().expect("ask").price.to_string(),
        "100"
    );
}

#[test]
fn zero_quantity_deletes_and_capacity_failure_is_transactional() {
    let mut engine = synchronized_at(10);
    let session = session(1, 1, 1);

    assert_eq!(
        engine
            .apply_delta(delta(11, 11, &[("100", "0")], &[]), session, 20)
            .expect("zero delete applies"),
        ApplyResult::Applied
    );
    assert_eq!(
        engine
            .snapshot()
            .expect("book remains trusted")
            .best_bid()
            .expect("second bid becomes best")
            .price
            .to_string(),
        "99"
    );
}

#[test]
fn disconnect_retains_epoch_watermark_and_requires_a_new_session() {
    let mut engine = synchronized_at(10);
    engine.disconnect();
    assert_eq!(engine.state(), BookState::Disconnected);
    assert_eq!(
        engine.start_session(session(1, 1, 1), SnapshotStrategy::StreamSnapshot),
        Err(BookError::InvalidEpoch)
    );
    engine
        .start_session(session(1, 2, 1), SnapshotStrategy::StreamSnapshot)
        .expect("a new subscription epoch can reconnect");
    assert_eq!(engine.state(), BookState::AwaitingSnapshot);
}

#[test]
fn buffered_level_updates_have_an_aggregate_memory_bound() {
    let session = session(1, 1, 1);
    let mut engine = OrderBookEngine::new(BookConfig {
        max_buffered_level_updates: 1,
        ..book_config()
    })
    .expect("bounded config");
    engine
        .start_session(session, SnapshotStrategy::ExternalBuffered)
        .unwrap();
    assert_eq!(
        engine
            .apply_delta(delta(1, 1, &[("100", "1")], &[]), session, 1)
            .unwrap(),
        ApplyResult::SnapshotRequired
    );
    assert_eq!(
        engine.apply_delta(delta(2, 2, &[], &[("101", "1")]), session, 2),
        Err(BookError::BufferCapacity)
    );
    assert_eq!(engine.state(), BookState::Untrusted);
}

fn synchronized_at(sequence: u64) -> OrderBookEngine {
    synchronized_with_policy(sequence, SequencePolicy::RangeContainsNext)
}

fn synchronized_with_policy(sequence: u64, policy: SequencePolicy) -> OrderBookEngine {
    let session = session(1, 1, 1);
    let mut engine = OrderBookEngine::new(BookConfig {
        sequence_policy: policy,
        ..book_config()
    })
    .unwrap();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .expect("session starts");
    engine
        .apply_snapshot(
            snapshot(
                sequence,
                &[("100", "2"), ("99", "1")],
                &[("101", "4"), ("102", "1")],
            ),
            session,
            10,
        )
        .expect("snapshot applies");
    engine
}

fn synchronized_external_at(sequence: u64) -> OrderBookEngine {
    let session = session(1, 1, 1);
    let mut engine = engine();
    engine
        .start_session(session, SnapshotStrategy::ExternalBuffered)
        .expect("session starts");
    engine
        .apply_snapshot(
            snapshot(
                sequence,
                &[("100", "2"), ("99", "1")],
                &[("101", "4"), ("102", "1")],
            ),
            session,
            10,
        )
        .expect("snapshot applies");
    engine
}

fn engine() -> OrderBookEngine {
    OrderBookEngine::new(book_config()).expect("valid config")
}

fn book_config() -> BookConfig {
    book_config_for(ProductType::Spot)
}

fn book_config_for(product_type: ProductType) -> BookConfig {
    BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("test").expect("venue"),
            "BTCUSDT",
            product_type,
            1,
        )
        .expect("instrument"),
        price_tick: price("0.1"),
        quantity_step: quantity("0.1"),
        max_levels_per_side: 8,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: SequencePolicy::RangeContainsNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    }
}

fn product_snapshot_event(product_type: ProductType) -> EventEnvelope {
    let venue = VenueId::new("test").expect("venue");
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 3,
            source: SourceId::new(SourceKind::Exchange, "test", 1).expect("source"),
            venue: Some(venue.clone()),
            instrument_id: Some(
                InstrumentId::new_for_product(venue, "BTCUSDT", product_type, 1)
                    .expect("instrument"),
            ),
            exchange_timestamp: Some(UnixNanos::new(1)),
            exchange_transaction_timestamp: None,
            receive_wall_timestamp: UnixNanos::new(2),
            receive_monotonic_ns: 2,
            normalization_timestamp: UnixNanos::new(3),
            connection_started_at: UnixNanos::new(1),
            sequence_number: Some(1),
            previous_sequence_number: None,
            connection_epoch: 1,
            subscription_epoch: 1,
            snapshot_kind: SnapshotKind::Snapshot,
            source_checksum: None,
            raw_payload_hash: [1; 32],
            parser_version: "test-parser".to_owned(),
            normalizer_version: "test-normalizer".to_owned(),
            ingestion_instance: "test".to_owned(),
            quality_score_ppm: 1_000_000,
            quality_flags: QualityFlags::NONE,
        },
        UncheckedEventPayload::BookSnapshot(BookSnapshot {
            bids: Vec::new(),
            asks: Vec::new(),
            last_sequence: 1,
        }),
    )
    .expect("product snapshot event")
}

fn book_event(
    payload: UncheckedEventPayload,
    sequence_number: u64,
    previous_sequence_number: Option<u64>,
    snapshot_kind: SnapshotKind,
    receive_monotonic_ns: u64,
    ordinal: u8,
) -> EventEnvelope {
    let venue = VenueId::new("test").expect("venue");
    let event_time = i64::try_from(sequence_number).expect("test sequence fits event time");
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 3,
            source: SourceId::new(SourceKind::Exchange, "test", 1).expect("source"),
            venue: Some(venue.clone()),
            instrument_id: Some(
                InstrumentId::new_for_product(venue, "BTCUSDT", ProductType::Spot, 1)
                    .expect("instrument"),
            ),
            exchange_timestamp: Some(UnixNanos::new(event_time)),
            exchange_transaction_timestamp: None,
            receive_wall_timestamp: UnixNanos::new(event_time + 1),
            receive_monotonic_ns,
            normalization_timestamp: UnixNanos::new(event_time + 2),
            connection_started_at: UnixNanos::new(1),
            sequence_number: Some(sequence_number),
            previous_sequence_number,
            connection_epoch: 1,
            subscription_epoch: 1,
            snapshot_kind,
            source_checksum: None,
            raw_payload_hash: [ordinal; 32],
            parser_version: "test-parser".to_owned(),
            normalizer_version: "test-normalizer".to_owned(),
            ingestion_instance: "test".to_owned(),
            quality_score_ppm: 1_000_000,
            quality_flags: QualityFlags::NONE,
        },
        payload,
    )
    .expect("book event")
}

fn session(
    connection_epoch: u64,
    subscription_epoch: u64,
    instrument_generation: u32,
) -> BookSession {
    BookSession {
        connection_epoch,
        subscription_epoch,
        instrument_generation,
    }
}

fn snapshot(last_sequence: u64, bids: &[(&str, &str)], asks: &[(&str, &str)]) -> BookSnapshot {
    BookSnapshot {
        bids: levels(bids),
        asks: levels(asks),
        last_sequence,
    }
}

fn delta(
    first_sequence: u64,
    last_sequence: u64,
    bids: &[(&str, &str)],
    asks: &[(&str, &str)],
) -> BookDelta {
    BookDelta {
        bids: levels(bids),
        asks: levels(asks),
        first_sequence,
        last_sequence,
    }
}

fn levels(values: &[(&str, &str)]) -> Vec<BookLevel> {
    values
        .iter()
        .map(|(price_value, quantity_value)| BookLevel {
            price: price(price_value),
            quantity: quantity(quantity_value),
            order_count: None,
        })
        .collect()
}

fn price(value: &str) -> Price {
    Price::new(FixedDecimal::parse_canonical(value).expect("canonical price"))
        .expect("positive price")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(FixedDecimal::parse_canonical(value).expect("canonical quantity"))
        .expect("nonnegative quantity")
}
use domain::{InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId};
