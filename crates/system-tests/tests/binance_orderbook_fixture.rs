use connector_binance::parse_fixture_line;
use domain::{InstrumentId, VenueId};
use event_envelope::UncheckedEventPayload;
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{
    ApplyResult, BookClassification, BookConfig, BookError, BookSession, BookState, ChecksumPolicy,
    ChecksumStatus, OrderBookEngine, SequencePolicy, SnapshotStrategy,
};

const FIXTURE: &str = include_str!("../../../fixtures/binance/btcusdt-book-v1.jsonl");

#[test]
fn validated_fixture_envelopes_reach_exact_authoritative_state() {
    let mut book = engine();
    book.start_session(session(), SnapshotStrategy::StreamSnapshot)
        .expect("fixture session starts");

    for line in FIXTURE.lines() {
        let event = parse_fixture_line(line.as_bytes()).expect("frozen fixture line must parse");
        assert_eq!(
            book.apply_event(&event).expect("validated event applies"),
            ApplyResult::Applied
        );
    }

    let view = book.snapshot().expect("fixture book must be trusted");
    assert_eq!(book.state(), BookState::Synchronized);
    assert_eq!(view.last_source_sequence(), 102);
    assert_eq!(view.classification(), BookClassification::Normal);
    assert_eq!(
        view.best_bid().expect("best bid").price.to_string(),
        "60000.1"
    );
    assert_eq!(
        view.best_bid().expect("best bid").price.value().mantissa(),
        600_001
    );
    assert_eq!(
        view.best_ask().expect("best ask").price.to_string(),
        "60000.2"
    );
    assert_eq!(view.quality().checksum_status, ChecksumStatus::NotSupported);
    assert_eq!(view.quality().health_state, BookState::Synchronized);
    assert_eq!(view.quality().last_source_sequence, 102);
    assert_eq!(view.quality().last_update_age_ms, 0);
    assert_eq!(view.quality().missing_sequence_count_1h, 0);
    assert_eq!(
        view.quality().source_latency_percentiles.sample_count,
        FIXTURE.lines().count()
    );
    assert_eq!(view.quality().source_latency_percentiles.p50_ms, Some(0));
    assert_eq!(view.quality().source_latency_percentiles.p95_ms, Some(0));
    assert_eq!(view.quality().source_latency_percentiles.p99_ms, Some(0));
    assert_eq!(view.quality().quality_score_ppm, 1_000_000);
    assert_eq!(view.quality().quality_score_0_to_1(), 1.0);
}

#[test]
fn envelope_payload_and_session_context_are_bound_together() {
    let first = parse_fixture_line(FIXTURE.lines().next().expect("snapshot fixture").as_bytes())
        .expect("snapshot parses");
    assert!(matches!(
        first.payload().as_unchecked(),
        UncheckedEventPayload::BookSnapshot(_)
    ));

    let mut wrong_generation = OrderBookEngine::new(BookConfig {
        instrument: instrument(2),
        ..config()
    })
    .expect("valid alternate config");
    assert!(
        wrong_generation
            .start_session(session(), SnapshotStrategy::StreamSnapshot)
            .is_err()
    );

    let mut wrong_symbol = OrderBookEngine::new(BookConfig {
        instrument: InstrumentId::new(VenueId::new("binance").unwrap(), "ETHUSDT", 1).unwrap(),
        ..config()
    })
    .expect("valid alternate symbol config");
    wrong_symbol
        .start_session(session(), SnapshotStrategy::StreamSnapshot)
        .unwrap();
    assert_eq!(
        wrong_symbol.apply_event(&first),
        Err(BookError::WrongInstrument)
    );
    assert_eq!(wrong_symbol.state(), BookState::AwaitingSnapshot);
}

fn engine() -> OrderBookEngine {
    OrderBookEngine::new(config()).expect("valid book config")
}

fn config() -> BookConfig {
    BookConfig {
        instrument: instrument(1),
        price_tick: price("0.1"),
        quantity_step: quantity("0.1"),
        max_levels_per_side: 16,
        max_buffered_deltas: 16,
        max_buffered_level_updates: 64,
        sequence_policy: SequencePolicy::RangeContainsNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    }
}

fn session() -> BookSession {
    BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    }
}

fn instrument(generation: u32) -> InstrumentId {
    InstrumentId::new(VenueId::new("binance").unwrap(), "BTCUSDT", generation).unwrap()
}

fn price(value: &str) -> Price {
    Price::new(FixedDecimal::parse_canonical(value).expect("canonical price"))
        .expect("positive price")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(FixedDecimal::parse_canonical(value).expect("canonical quantity"))
        .expect("nonnegative quantity")
}
