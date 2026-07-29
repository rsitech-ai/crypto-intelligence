use connector_bybit::{
    BybitBookSyncError, BybitBookSynchronizer, BybitInput, BybitMarket, BybitMessage,
    parse_native_message,
};
use domain::{InstrumentId, ProductType, VenueId};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{
    ApplyResult, BookConfig, BookError, BookSession, BookState, ChecksumPolicy, SequencePolicy,
};

const SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/bybit/spot-orderbook-snapshot.json");
const DELTA: &[u8] = include_bytes!("../../../fixtures/exchanges/bybit/spot-orderbook-delta.json");
const RESET: &[u8] = include_bytes!("../../../fixtures/exchanges/bybit/spot-orderbook-reset.json");

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("decimal")
}

fn config(product: ProductType) -> BookConfig {
    BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("bybit").expect("venue"),
            "BTCUSDT",
            product,
            1,
        )
        .expect("instrument"),
        price_tick: Price::new(decimal("0.01")).expect("tick"),
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

fn session() -> BookSession {
    BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    }
}

fn orderbook(raw: &[u8]) -> connector_bybit::OrderBookMessage {
    let BybitMessage::OrderBook(message) =
        parse_native_message(BybitInput::PublicWebSocket(BybitMarket::Spot), raw)
            .expect("fixture must parse")
    else {
        panic!("expected order-book message");
    };
    message
}

#[test]
fn later_snapshot_replaces_every_prior_level_before_deltas_resume() {
    let mut synchronizer =
        BybitBookSynchronizer::try_new(BybitMarket::Spot, config(ProductType::Spot))
            .expect("synchronizer");
    synchronizer.start_session(session()).expect("session");

    assert_eq!(
        synchronizer
            .apply_message(&orderbook(SNAPSHOT), 1)
            .expect("snapshot"),
        ApplyResult::Applied
    );
    assert_eq!(
        synchronizer
            .apply_message(&orderbook(DELTA), 2)
            .expect("delta"),
        ApplyResult::Applied
    );
    let before = synchronizer.snapshot().expect("trusted book");
    assert_eq!(before.best_bid().expect("bid").price.to_string(), "60000");
    assert_eq!(before.best_ask().expect("ask").price.to_string(), "60000.2");

    assert_eq!(
        synchronizer
            .apply_message(&orderbook(RESET), 3)
            .expect("reset snapshot"),
        ApplyResult::Applied
    );
    let after = synchronizer.snapshot().expect("replacement book");
    assert_eq!(after.best_bid().expect("bid").price.to_string(), "60100");
    assert_eq!(after.best_ask().expect("ask").price.to_string(), "60100.1");
    assert_eq!(after.bids().len(), 1);
    assert_eq!(after.asks().len(), 1);
    assert_eq!(synchronizer.snapshot_reset_count(), 2);
    assert_eq!(synchronizer.last_cross_sequence(), Some(9_532_239_500));
}

#[test]
fn delta_before_snapshot_and_update_gap_never_publish_trusted_state() {
    let mut synchronizer =
        BybitBookSynchronizer::try_new(BybitMarket::Spot, config(ProductType::Spot))
            .expect("synchronizer");
    synchronizer.start_session(session()).expect("session");
    assert_eq!(
        synchronizer
            .apply_message(&orderbook(DELTA), 1)
            .expect("typed outcome"),
        ApplyResult::SnapshotRequired
    );
    assert!(matches!(synchronizer.snapshot(), Err(BookError::Untrusted)));

    synchronizer
        .apply_message(&orderbook(SNAPSHOT), 2)
        .expect("snapshot");
    let mut gap = orderbook(DELTA);
    gap.update_id = 103;
    assert_eq!(
        synchronizer.apply_message(&gap, 3).expect("typed outcome"),
        ApplyResult::GapDetected
    );
    assert_eq!(synchronizer.state(), BookState::Untrusted);
    assert!(matches!(synchronizer.snapshot(), Err(BookError::Untrusted)));
}

#[test]
fn cross_sequence_regression_fails_closed_and_requires_a_new_snapshot() {
    let mut synchronizer =
        BybitBookSynchronizer::try_new(BybitMarket::Spot, config(ProductType::Spot))
            .expect("synchronizer");
    synchronizer.start_session(session()).expect("session");
    synchronizer
        .apply_message(&orderbook(SNAPSHOT), 1)
        .expect("snapshot");
    let mut regression = orderbook(DELTA);
    regression.cross_sequence = 9_532_239_399;
    assert_eq!(
        synchronizer.apply_message(&regression, 2),
        Err(BybitBookSyncError::CrossSequenceRegression)
    );
    assert_eq!(synchronizer.state(), BookState::Disconnected);
    assert!(matches!(synchronizer.snapshot(), Err(BookError::Untrusted)));
}

#[test]
fn market_and_engine_configuration_must_match_bybit_semantics() {
    assert!(matches!(
        BybitBookSynchronizer::try_new(BybitMarket::LinearPerpetual, config(ProductType::Spot)),
        Err(BybitBookSyncError::InvalidConfig)
    ));
    let mut wrong_policy = config(ProductType::Spot);
    wrong_policy.sequence_policy = SequencePolicy::RangeContainsNext;
    assert!(matches!(
        BybitBookSynchronizer::try_new(BybitMarket::Spot, wrong_policy),
        Err(BybitBookSyncError::InvalidConfig)
    ));
}
