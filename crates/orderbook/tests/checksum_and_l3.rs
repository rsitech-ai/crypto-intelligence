use event_envelope::{BookDelta, BookLevel, BookSnapshot};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{
    ApplyResult, BookConfig, BookError, BookSession, BookState, ChecksumPolicy, ChecksumStatus,
    ExactChecksumLevel, ExactL3ChecksumOrder, KrakenV2Checksum, KrakenV2L3Checksum, L3Event,
    L3Order, L3OrderId, L3Side, OrderBookEngine, SequencePolicy, SnapshotStrategy,
};

const KRAKEN_BIDS: [(&str, &str); 10] = [
    ("45283.5", "0.10000000"),
    ("45283.4", "1.54582015"),
    ("45282.1", "0.10000000"),
    ("45281.0", "0.10000000"),
    ("45280.3", "1.54592586"),
    ("45279.0", "0.07990000"),
    ("45277.6", "0.03310103"),
    ("45277.5", "0.30000000"),
    ("45277.3", "1.54602737"),
    ("45276.6", "0.15445238"),
];
const KRAKEN_ASKS: [(&str, &str); 10] = [
    ("45285.2", "0.00100000"),
    ("45286.4", "1.54571953"),
    ("45286.6", "1.54571109"),
    ("45289.6", "1.54560911"),
    ("45290.2", "0.15890660"),
    ("45291.8", "1.54553491"),
    ("45294.7", "0.04454749"),
    ("45296.1", "0.35380000"),
    ("45297.5", "0.09945542"),
    ("45299.5", "0.18772827"),
];

#[test]
fn required_checksum_matches_official_vector_and_mismatch_suppresses_publication() {
    let session = session();
    let snapshot = BookSnapshot {
        bids: book_levels(&KRAKEN_BIDS),
        asks: book_levels(&KRAKEN_ASKS),
        last_sequence: 1,
    };
    let recovery_snapshot = snapshot.clone();

    let mut valid = checksum_engine();
    valid
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();
    assert_eq!(
        valid
            .apply_snapshot_checked(
                snapshot.clone(),
                session,
                1,
                7,
                kraken_checksum(3_310_070_434),
            )
            .unwrap(),
        ApplyResult::Applied
    );
    assert_eq!(
        valid.snapshot().unwrap().quality().checksum_status,
        ChecksumStatus::Valid
    );

    let mut mismatch = checksum_engine();
    mismatch
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();
    assert_eq!(
        mismatch
            .apply_snapshot_checked(snapshot, session, 1, 8, kraken_checksum(3_310_070_435))
            .unwrap(),
        ApplyResult::ChecksumMismatch
    );
    assert_eq!(mismatch.state(), BookState::Untrusted);
    assert_eq!(mismatch.snapshot(), Err(BookError::Untrusted));
    assert_eq!(
        mismatch
            .apply_snapshot_checked(
                recovery_snapshot,
                session,
                2,
                9,
                kraken_checksum(3_310_070_434),
            )
            .unwrap(),
        ApplyResult::Applied
    );
    let quality = mismatch.snapshot().unwrap().quality().clone();
    assert_eq!(quality.checksum_failure_count_1h, 1);
    assert_eq!(quality.resync_count_1h, 1);
    assert_eq!(quality.source_latency_percentiles.sample_count, 2);
    assert_eq!(quality.source_latency_percentiles.p50_ms, Some(8));
    assert_eq!(quality.source_latency_percentiles.p95_ms, Some(9));
}

#[test]
fn required_checksum_cannot_be_silently_omitted() {
    let session = session();
    let mut engine = checksum_engine();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();

    assert_eq!(
        engine.apply_snapshot(
            BookSnapshot {
                bids: book_levels(&KRAKEN_BIDS),
                asks: book_levels(&KRAKEN_ASKS),
                last_sequence: 1,
            },
            session,
            1,
        ),
        Err(BookError::ChecksumRequired)
    );
    assert_eq!(engine.state(), BookState::Untrusted);
}

#[test]
fn source_exact_checksum_shape_errors_are_observable_after_recovery() {
    let session = session();
    let snapshot = BookSnapshot {
        bids: book_levels(&KRAKEN_BIDS),
        asks: book_levels(&KRAKEN_ASKS),
        last_sequence: 1,
    };
    let mut wrong_asks = exact_levels(&KRAKEN_ASKS);
    wrong_asks[0] = ExactChecksumLevel::new("45285.2", "0.00200000").unwrap();
    let wrong_source = KrakenV2Checksum::new(0, wrong_asks, exact_levels(&KRAKEN_BIDS)).unwrap();
    let mut engine = checksum_engine();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();

    assert_eq!(
        engine.apply_snapshot_checked(snapshot.clone(), session, 1, 11, wrong_source),
        Err(BookError::InvalidChecksumInput)
    );
    assert_eq!(engine.state(), BookState::Untrusted);
    assert_eq!(
        engine
            .apply_snapshot_checked(snapshot, session, 2, 12, kraken_checksum(3_310_070_434))
            .unwrap(),
        ApplyResult::Applied
    );
    assert_eq!(
        engine
            .snapshot()
            .unwrap()
            .quality()
            .checksum_failure_count_1h,
        1
    );
}

#[test]
fn bounded_l3_mutations_require_queue_checksum_and_fail_closed() {
    let session = session();
    let mut engine = l3_engine();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();
    engine
        .apply_snapshot(
            BookSnapshot {
                bids: vec![level("100", "10")],
                asks: vec![level("101", "10")],
                last_sequence: 1,
            },
            session,
            1,
        )
        .unwrap();
    let initial = vec![
        l3("bid-a", L3Side::Bid, "100", "2", 10),
        l3("ask-a", L3Side::Ask, "101", "3", 11),
    ];
    assert_eq!(
        engine
            .apply_l3_snapshot_checked(initial.clone(), session, 2, 13, l3_checksum(&initial))
            .unwrap(),
        ApplyResult::Applied
    );
    assert_eq!(engine.snapshot().unwrap().l3_orders().len(), 2);

    engine
        .apply_l3_events_checked(
            &[
                L3Event::Modify {
                    id: L3OrderId::new("bid-a").unwrap(),
                    quantity: quantity("4"),
                },
                L3Event::Delete {
                    id: L3OrderId::new("ask-a").unwrap(),
                },
                L3Event::Add(l3("ask-b", L3Side::Ask, "101", "1", 13)),
            ],
            session,
            3,
            14,
            l3_checksum(&[
                l3("bid-a", L3Side::Bid, "100", "4", 10),
                l3("ask-b", L3Side::Ask, "101", "1", 13),
            ]),
        )
        .unwrap();
    let before = engine.snapshot().unwrap();
    assert_eq!(before.l3_orders().len(), 2);
    assert_eq!(before.l3_orders()[0].id().as_str(), "bid-a");
    assert_eq!(before.l3_orders()[0].quantity().to_string(), "4");

    assert_eq!(
        engine.apply_l3_events_checked(
            &[L3Event::Add(l3("bid-a", L3Side::Bid, "100", "9", 14))],
            session,
            4,
            15,
            l3_checksum(&[
                l3("bid-a", L3Side::Bid, "100", "4", 10),
                l3("ask-b", L3Side::Ask, "101", "1", 13),
            ]),
        ),
        Err(BookError::AmbiguousL3Order)
    );
    assert!(
        engine.snapshot().unwrap().l3_orders().is_empty(),
        "any invalid L3 mutation suppresses the independently trusted L3 view"
    );

    engine
        .apply_delta(
            BookDelta {
                bids: vec![level("100", "9")],
                asks: Vec::new(),
                first_sequence: 2,
                last_sequence: 2,
            },
            session,
            5,
        )
        .unwrap();
    assert!(
        engine.snapshot().unwrap().l3_orders().is_empty(),
        "an independently updated L2 book must not publish stale L3 orders"
    );
}

#[test]
fn l3_processes_the_full_snapshot_then_truncates_to_subscribed_price_depth() {
    let session = session();
    let mut engine = l3_engine();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();
    engine
        .apply_snapshot(
            BookSnapshot {
                bids: vec![level("100", "10")],
                asks: vec![level("101", "10")],
                last_sequence: 1,
            },
            session,
            1,
        )
        .unwrap();

    let source_orders = vec![
        l3_with_queue("ask-late", L3Side::Ask, "101", "1", 10, 2),
        l3_with_queue("ask-first", L3Side::Ask, "101", "2", 10, 1),
        l3("ask-mid", L3Side::Ask, "102", "3", 11),
        l3("ask-outside", L3Side::Ask, "103", "4", 12),
    ];
    let trusted_orders = vec![
        l3_with_queue("ask-first", L3Side::Ask, "101", "2", 10, 1),
        l3_with_queue("ask-late", L3Side::Ask, "101", "1", 10, 2),
        l3("ask-mid", L3Side::Ask, "102", "3", 11),
    ];
    assert_eq!(
        engine
            .apply_l3_snapshot_checked(source_orders, session, 2, 16, l3_checksum(&trusted_orders),)
            .unwrap(),
        ApplyResult::Applied
    );
    let published = engine.snapshot().unwrap();
    assert_eq!(
        published
            .l3_orders()
            .iter()
            .map(|order| order.id().as_str())
            .collect::<Vec<_>>(),
        ["ask-first", "ask-late", "ask-mid"]
    );

    assert_eq!(
        engine.apply_l3_snapshot(trusted_orders, session, 3),
        Err(BookError::ChecksumRequired)
    );
    assert!(engine.snapshot().unwrap().l3_orders().is_empty());
}

fn checksum_engine() -> OrderBookEngine {
    OrderBookEngine::new(BookConfig {
        instrument: instrument(),
        price_tick: price("0.00000001"),
        quantity_step: quantity("0.00000001"),
        max_levels_per_side: 10,
        max_buffered_deltas: 16,
        max_buffered_level_updates: 64,
        sequence_policy: SequencePolicy::RangeContainsNext,
        checksum_policy: ChecksumPolicy::Required,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    })
    .unwrap()
}

fn l3_engine() -> OrderBookEngine {
    OrderBookEngine::new(BookConfig {
        instrument: instrument(),
        price_tick: price("0.1"),
        quantity_step: quantity("0.1"),
        max_levels_per_side: 8,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: SequencePolicy::ExactNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: Some(8),
        max_l3_levels_per_side: Some(2),
    })
    .unwrap()
}

fn session() -> BookSession {
    BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    }
}

fn instrument() -> InstrumentId {
    InstrumentId::new(VenueId::new("kraken").unwrap(), "BTCUSD", 1).unwrap()
}

fn kraken_checksum(expected: u32) -> KrakenV2Checksum {
    KrakenV2Checksum::new(
        expected,
        exact_levels(&KRAKEN_ASKS),
        exact_levels(&KRAKEN_BIDS),
    )
    .unwrap()
}

fn exact_levels(values: &[(&str, &str)]) -> Vec<ExactChecksumLevel> {
    values
        .iter()
        .map(|(price_value, quantity_value)| {
            ExactChecksumLevel::new(*price_value, *quantity_value).unwrap()
        })
        .collect()
}

fn book_levels(values: &[(&str, &str)]) -> Vec<BookLevel> {
    values
        .iter()
        .map(|(price_value, quantity_value)| level(price_value, quantity_value))
        .collect()
}

fn l3(
    id: &str,
    side: L3Side,
    price_value: &str,
    quantity_value: &str,
    priority_ns: u64,
) -> L3Order {
    l3_with_queue(
        id,
        side,
        price_value,
        quantity_value,
        priority_ns,
        priority_ns,
    )
}

fn l3_with_queue(
    id: &str,
    side: L3Side,
    price_value: &str,
    quantity_value: &str,
    priority_ns: u64,
    queue_order: u64,
) -> L3Order {
    L3Order::new(
        L3OrderId::new(id).unwrap(),
        side,
        price(price_value),
        quantity(quantity_value),
        priority_ns,
        queue_order,
    )
    .unwrap()
}

fn l3_checksum(orders: &[L3Order]) -> KrakenV2L3Checksum {
    let mut asks = orders
        .iter()
        .filter(|order| order.side() == L3Side::Ask)
        .cloned()
        .collect::<Vec<_>>();
    let mut bids = orders
        .iter()
        .filter(|order| order.side() == L3Side::Bid)
        .cloned()
        .collect::<Vec<_>>();
    asks.sort_by_key(|order| (order.price(), order.priority_ns(), order.queue_order()));
    bids.sort_by(|left, right| {
        right
            .price()
            .cmp(&left.price())
            .then_with(|| left.priority_ns().cmp(&right.priority_ns()))
            .then_with(|| left.queue_order().cmp(&right.queue_order()))
    });
    let asks = exact_l3_orders(asks);
    let bids = exact_l3_orders(bids);
    let computed = KrakenV2L3Checksum::new(0, asks.clone(), bids.clone())
        .unwrap()
        .computed();
    KrakenV2L3Checksum::new(computed, asks, bids).unwrap()
}

fn exact_l3_orders(orders: Vec<L3Order>) -> Vec<ExactL3ChecksumOrder> {
    orders
        .into_iter()
        .map(|order| {
            let price_text = order.price().to_string();
            let quantity_text = order.quantity().to_string();
            ExactL3ChecksumOrder::new(order, price_text, quantity_text).unwrap()
        })
        .collect()
}

fn level(price_value: &str, quantity_value: &str) -> BookLevel {
    BookLevel {
        price: price(price_value),
        quantity: quantity(quantity_value),
        order_count: None,
    }
}

fn price(value: &str) -> Price {
    Price::new(FixedDecimal::parse(value).unwrap()).unwrap()
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(FixedDecimal::parse(value).unwrap()).unwrap()
}
use domain::{InstrumentId, VenueId};
