use event_envelope::{BookDelta, BookLevel, BookSnapshot};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{
    ApplyResult, BookConfig, BookSession, ChecksumPolicy, OrderBookEngine, SequencePolicy,
    SnapshotStrategy,
};
use proptest::prelude::*;

proptest! {
    #[test]
    fn contiguous_updates_preserve_order_and_zero_deletes(
        quantities in prop::collection::vec(0_u32..1_000, 1..64)
    ) {
        let session = session();
        let mut engine = engine();
        engine.start_session(session, SnapshotStrategy::StreamSnapshot).unwrap();
        engine.apply_snapshot(snapshot(), session, 1).unwrap();

        for (offset, quantity_mantissa) in quantities.into_iter().enumerate() {
            let sequence = u64::try_from(offset).unwrap() + 2;
            let quantity = Quantity::new(FixedDecimal::new(i128::from(quantity_mantissa), 1).unwrap()).unwrap();
            let result = engine.apply_delta(
                BookDelta {
                    bids: vec![BookLevel {
                        price: price("100"),
                        quantity,
                        order_count: None,
                    }],
                    asks: Vec::new(),
                    first_sequence: sequence,
                    last_sequence: sequence,
                },
                session,
                sequence,
            ).unwrap();
            prop_assert_eq!(result, ApplyResult::Applied);
            let view = engine.snapshot().unwrap();
            prop_assert!(view.bids().windows(2).all(|pair| pair[0].price > pair[1].price));
            prop_assert!(view.asks().windows(2).all(|pair| pair[0].price < pair[1].price));
            prop_assert!(view.bids().iter().chain(view.asks()).all(|level| !level.quantity.value().is_zero()));
        }
    }

    #[test]
    fn every_positive_gap_suppresses_trusted_publication(gap in 1_u64..10_000) {
        let session = session();
        let mut engine = engine();
        engine.start_session(session, SnapshotStrategy::StreamSnapshot).unwrap();
        engine.apply_snapshot(snapshot(), session, 1).unwrap();

        let missing = 2_u64.checked_add(gap).unwrap();
        let result = engine.apply_delta(
            BookDelta {
                bids: vec![level("100", "2")],
                asks: Vec::new(),
                first_sequence: missing,
                last_sequence: missing,
            },
            session,
            2,
        ).unwrap();

        prop_assert_eq!(result, ApplyResult::GapDetected);
        prop_assert!(engine.snapshot().is_err());
    }
}

fn engine() -> OrderBookEngine {
    OrderBookEngine::new(BookConfig {
        instrument: InstrumentId::new(VenueId::new("test").unwrap(), "BTCUSDT", 1).unwrap(),
        price_tick: price("0.1"),
        quantity_step: quantity("0.1"),
        max_levels_per_side: 8,
        max_buffered_deltas: 64,
        max_buffered_level_updates: 128,
        sequence_policy: SequencePolicy::RangeContainsNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
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

fn snapshot() -> BookSnapshot {
    BookSnapshot {
        bids: vec![level("100", "1"), level("99", "1")],
        asks: vec![level("101", "1"), level("102", "1")],
        last_sequence: 1,
    }
}

fn level(price_value: &str, quantity_value: &str) -> BookLevel {
    BookLevel {
        price: price(price_value),
        quantity: quantity(quantity_value),
        order_count: None,
    }
}

fn price(value: &str) -> Price {
    Price::new(FixedDecimal::parse_canonical(value).unwrap()).unwrap()
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(FixedDecimal::parse_canonical(value).unwrap()).unwrap()
}
use domain::{InstrumentId, VenueId};
