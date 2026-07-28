use connector_binance::parse_fixture_line;
use event_envelope::{BookDelta, BookLevel, UncheckedEventPayload};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{BookError, BookHealth, OrderBook};

const FIXTURE: &str = include_str!("../../../fixtures/binance/btcusdt-book-v1.jsonl");

#[test]
fn fixture_reaches_exact_authoritative_state() {
    let mut book = OrderBook::new(16);
    apply_fixture(&mut book);

    assert_eq!(book.sequence(), 102);
    assert_eq!(book.health(), BookHealth::Healthy);
    let best_bid = book.best_bid().expect("fixture must have a best bid");
    assert_eq!(best_bid.to_string(), "60000.1");
    assert_eq!(best_bid.value().mantissa(), 600_001);
    assert_eq!(best_bid.value().scale(), 1);
    let best_ask = book.best_ask().expect("fixture must have a best ask");
    assert_eq!(best_ask.to_string(), "60000.2");
    assert_eq!(best_ask.value().mantissa(), 600_002);
    assert_eq!(best_ask.value().scale(), 1);
}

#[test]
fn sequence_gap_preserves_last_state_and_blocks_later_deltas() {
    let mut book = OrderBook::new(16);
    apply_fixture(&mut book);
    let prior_bid = book.best_bid();
    let prior_ask = book.best_ask();

    let error = book
        .apply_delta(&delta(104, 104, "60000.1", "4"), 4)
        .expect_err("sequence gap must fail");

    assert_eq!(error, BookError::Gap);
    assert_eq!(book.sequence(), 102);
    assert_eq!(book.best_bid(), prior_bid);
    assert_eq!(book.best_ask(), prior_ask);
    assert_eq!(book.health(), BookHealth::Gapped);
    assert_eq!(
        book.apply_delta(&delta(103, 103, "60000.1", "4"), 5),
        Err(BookError::Untrusted)
    );
}

#[test]
fn stale_overlap_is_a_gap_not_a_replacement_snapshot() {
    let mut book = OrderBook::new(16);
    apply_fixture(&mut book);
    let prior_bid = book.best_bid();

    let error = book
        .apply_delta(&delta(101, 103, "60000.1", "9"), 4)
        .expect_err("overlapping stale sequence range must fail");

    assert_eq!(error, BookError::Gap);
    assert_eq!(book.sequence(), 102);
    assert_eq!(book.best_bid(), prior_bid);
    assert_eq!(book.health(), BookHealth::Gapped);
}

#[test]
fn invalid_crossing_delta_preserves_prices_and_quarantines_book() {
    let mut book = OrderBook::new(16);
    apply_fixture(&mut book);
    let prior_bid = book.best_bid();
    let prior_ask = book.best_ask();

    let crossing = BookDelta {
        bids: Vec::new(),
        asks: vec![level("59999", "1")],
        first_sequence: 103,
        last_sequence: 103,
    };
    let error = book
        .apply_delta(&crossing, 4)
        .expect_err("crossing delta must fail");

    assert_eq!(error, BookError::Invalid);
    assert_eq!(book.sequence(), 102);
    assert_eq!(book.best_bid(), prior_bid);
    assert_eq!(book.best_ask(), prior_ask);
    assert_eq!(book.health(), BookHealth::Quarantined);
}

fn apply_fixture(book: &mut OrderBook) {
    for (index, line) in FIXTURE.lines().enumerate() {
        let event = parse_fixture_line(line.as_bytes()).expect("frozen fixture line must parse");
        match event.payload().as_unchecked() {
            UncheckedEventPayload::BookSnapshot(snapshot) => book
                .apply_snapshot(snapshot, index as u64 + 1)
                .expect("fixture snapshot must apply"),
            UncheckedEventPayload::BookDelta(delta) => book
                .apply_delta(delta, index as u64 + 1)
                .expect("fixture delta must apply"),
            _ => panic!("fixture contains only order-book payloads"),
        }
    }
}

fn delta(first_sequence: u64, last_sequence: u64, price: &str, quantity: &str) -> BookDelta {
    BookDelta {
        bids: vec![level(price, quantity)],
        asks: Vec::new(),
        first_sequence,
        last_sequence,
    }
}

fn level(price: &str, quantity: &str) -> BookLevel {
    BookLevel {
        price: Price::new(
            FixedDecimal::parse_canonical(price).expect("test price must be canonical"),
        )
        .expect("test price must be positive"),
        quantity: Quantity::new(
            FixedDecimal::parse_canonical(quantity).expect("test quantity must be canonical"),
        )
        .expect("test quantity must not be negative"),
        order_count: None,
    }
}
