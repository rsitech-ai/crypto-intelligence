use domain::{InstrumentId, VenueId};
use event_envelope::{BookDelta, BookLevel, BookSnapshot};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{
    ApplyResult, BookConfig, BookSession, ChecksumPolicy, ExactChecksumLevel, KrakenV2Checksum,
    OrderBookEngine, SequencePolicy, SnapshotStrategy,
};

const LEVELS_PER_SIDE: usize = 20_000;
const UPDATE_COUNT: u64 = 50_000;
const CHECKSUM_UPDATE_COUNT: u64 = 20_000;

#[test]
fn sustained_single_level_updates_preserve_a_deep_bounded_book() {
    let session = BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    };
    let mut engine = OrderBookEngine::new(BookConfig {
        instrument: InstrumentId::new(VenueId::new("soak").unwrap(), "DEEP-BOOK", 1).unwrap(),
        price_tick: price(1),
        quantity_step: quantity(1),
        max_levels_per_side: LEVELS_PER_SIDE,
        max_buffered_deltas: 16,
        max_buffered_level_updates: 64,
        sequence_policy: SequencePolicy::ExactNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    })
    .unwrap();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();

    let snapshot = BookSnapshot {
        bids: (0..LEVELS_PER_SIDE)
            .map(|offset| level(100_000 - i128::try_from(offset).unwrap(), 1))
            .collect(),
        asks: (0..LEVELS_PER_SIDE)
            .map(|offset| level(100_001 + i128::try_from(offset).unwrap(), 1))
            .collect(),
        last_sequence: 1,
    };
    assert_eq!(
        engine.apply_snapshot(snapshot, session, 1).unwrap(),
        ApplyResult::Applied
    );

    for offset in 0..UPDATE_COUNT {
        let sequence = offset + 2;
        let quantity_value = i128::from(u8::try_from(offset % 9).unwrap() + 1);
        assert_eq!(
            engine
                .apply_delta(
                    BookDelta {
                        bids: vec![level(100_000, quantity_value)],
                        asks: Vec::new(),
                        first_sequence: sequence,
                        last_sequence: sequence,
                    },
                    session,
                    sequence,
                )
                .unwrap(),
            ApplyResult::Applied
        );
    }

    let view = engine.snapshot().unwrap();
    assert_eq!(view.bids().len(), LEVELS_PER_SIDE);
    assert_eq!(view.asks().len(), LEVELS_PER_SIDE);
    assert_eq!(view.last_source_sequence(), UPDATE_COUNT + 1);
    assert_eq!(
        view.best_bid().unwrap().quantity,
        quantity(i128::from(
            u8::try_from((UPDATE_COUNT - 1) % 9).unwrap() + 1
        ))
    );
}

#[test]
fn checksum_required_updates_inspect_only_the_top_ten_of_a_deep_book() {
    let session = BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    };
    let mut engine = OrderBookEngine::new(BookConfig {
        instrument: InstrumentId::new(VenueId::new("kraken").unwrap(), "DEEP-BOOK", 1).unwrap(),
        price_tick: price(1),
        quantity_step: quantity(1),
        max_levels_per_side: LEVELS_PER_SIDE,
        max_buffered_deltas: 16,
        max_buffered_level_updates: 64,
        sequence_policy: SequencePolicy::ExactNext,
        checksum_policy: ChecksumPolicy::Required,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    })
    .unwrap();
    engine
        .start_session(session, SnapshotStrategy::StreamSnapshot)
        .unwrap();
    assert_eq!(
        engine
            .apply_snapshot_checked(deep_snapshot(), session, 1, 1, deep_checksum(1))
            .unwrap(),
        ApplyResult::Applied
    );

    for offset in 0..CHECKSUM_UPDATE_COUNT {
        let sequence = offset + 2;
        let quantity_value = i128::from(u8::try_from(offset % 9).unwrap() + 1);
        assert_eq!(
            engine
                .apply_delta_checked(
                    BookDelta {
                        bids: vec![level(100_000, quantity_value)],
                        asks: Vec::new(),
                        first_sequence: sequence,
                        last_sequence: sequence,
                    },
                    session,
                    sequence,
                    2,
                    deep_checksum(quantity_value),
                )
                .unwrap(),
            ApplyResult::Applied
        );
    }

    let view = engine.snapshot().unwrap();
    assert_eq!(view.bids().len(), LEVELS_PER_SIDE);
    assert_eq!(view.asks().len(), LEVELS_PER_SIDE);
    assert_eq!(view.last_source_sequence(), CHECKSUM_UPDATE_COUNT + 1);
    assert_eq!(
        view.quality().source_latency_percentiles.sample_count,
        4_096,
        "latency observations remain explicitly bounded under sustained updates"
    );
}

fn deep_snapshot() -> BookSnapshot {
    BookSnapshot {
        bids: (0..LEVELS_PER_SIDE)
            .map(|offset| level(100_000 - i128::try_from(offset).unwrap(), 1))
            .collect(),
        asks: (0..LEVELS_PER_SIDE)
            .map(|offset| level(100_001 + i128::try_from(offset).unwrap(), 1))
            .collect(),
        last_sequence: 1,
    }
}

fn deep_checksum(best_bid_quantity: i128) -> KrakenV2Checksum {
    let asks = (0_i128..10)
        .map(|offset| exact_level(100_001 + offset, 1))
        .collect::<Vec<_>>();
    let bids = (0_i128..10)
        .map(|offset| {
            exact_level(
                100_000 - offset,
                if offset == 0 { best_bid_quantity } else { 1 },
            )
        })
        .collect::<Vec<_>>();
    let computed = KrakenV2Checksum::new(0, asks.clone(), bids.clone())
        .unwrap()
        .computed();
    KrakenV2Checksum::new(computed, asks, bids).unwrap()
}

fn exact_level(price_value: i128, quantity_value: i128) -> ExactChecksumLevel {
    ExactChecksumLevel::new(price_value.to_string(), quantity_value.to_string()).unwrap()
}

fn level(price_value: i128, quantity_value: i128) -> BookLevel {
    BookLevel {
        price: price(price_value),
        quantity: quantity(quantity_value),
        order_count: None,
    }
}

fn price(value: i128) -> Price {
    Price::new(FixedDecimal::new(value, 0).unwrap()).unwrap()
}

fn quantity(value: i128) -> Quantity {
    Quantity::new(FixedDecimal::new(value, 0).unwrap()).unwrap()
}
