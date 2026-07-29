use std::num::{NonZeroU32, NonZeroU64};

use connector_core::{
    DurableRawCaptureChannel, OptionsFields, RawCapture, SequenceSemantics,
    wal_stream_source_identity,
};
use connector_deribit::{
    BookMessageKind, DeribitBookSynchronizer, DeribitInput, DeribitLifecycle, DeribitMessage,
    NativeParseError, capabilities, normalize_option_ticker, parse_durable_native_message,
    parse_native_message,
};
use domain::{
    AssetId, AssetNamespace, InstrumentId, OptionSide, ProductType, SourceId, SourceKind,
    UnixNanos, VenueId,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{ApplyResult, BookConfig, BookSession, BookState, ChecksumPolicy, SequencePolicy};
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter},
    prologue::{SegmentMetadata, StreamDescriptor},
};

const OPTION_INSTRUMENT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/option-instrument.json");
const FUTURE_INSTRUMENT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/future-instrument.json");
const PERPETUAL_INSTRUMENT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/perpetual-instrument.json");
const OPTION_TICKER: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/option-ticker.json");
const PERPETUAL_TICKER: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/perpetual-ticker.json");
const FUTURE_TICKER: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/future-ticker.json");
const BOOK_SNAPSHOT: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/book-snapshot.json");
const BOOK_CHANGE: &[u8] = include_bytes!("../../../fixtures/exchanges/deribit/book-change.json");
const TRADES: &[u8] = include_bytes!("../../../fixtures/exchanges/deribit/trades.json");
const PERPETUAL_INTEREST: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/perpetual-interest.json");
const DELIVERY_PRICES: &[u8] =
    include_bytes!("../../../fixtures/exchanges/deribit/delivery-prices.json");

#[test]
fn option_identity_preserves_expiry_strike_side_and_source_metrics() {
    let metadata = one_instrument(OPTION_INSTRUMENT);
    let DeribitMessage::Ticker(ticker) =
        parse_native_message(DeribitInput::Subscription, OPTION_TICKER).unwrap()
    else {
        panic!("expected option ticker");
    };
    let binding = inverse_btc_binding(7);
    let definition = metadata.to_definition(&binding).unwrap();
    assert_eq!(definition.product_type(), ProductType::Option);
    assert_eq!(definition.option_side(), Some(OptionSide::Call));
    assert_eq!(definition.strike(), Some(price("65000")));
    assert_eq!(
        definition.expiry_time(),
        Some(UnixNanos::new(1_782_720_000_000_000_000))
    );
    assert_eq!(definition.id().generation(), 7);

    let normalized = normalize_option_ticker(&metadata, &ticker, &binding).unwrap();
    assert_eq!(normalized.instrument.venue_symbol(), "BTC-29JUN26-65000-C");
    assert_eq!(normalized.mark_iv, decimal("52.75"));
    assert_eq!(normalized.greeks.rho, decimal("12.75"));
    assert_eq!(normalized.greeks.theta, decimal("-18.5"));
    assert_eq!(normalized.open_interest_contracts, quantity("1250.5"));
}

#[test]
fn futures_and_perpetual_metadata_form_distinct_canonical_products() {
    let future = one_instrument(FUTURE_INSTRUMENT);
    let perpetual = one_instrument(PERPETUAL_INSTRUMENT);
    let binding = inverse_btc_binding(3);
    let future_definition = future.to_definition(&binding).unwrap();
    let perpetual_definition = perpetual.to_definition(&binding).unwrap();
    assert_eq!(future_definition.product_type(), ProductType::Future);
    assert_eq!(
        future_definition.expiry_time(),
        Some(UnixNanos::new(1_782_720_000_000_000_000))
    );
    assert_eq!(perpetual_definition.product_type(), ProductType::Perpetual);
    assert_eq!(perpetual_definition.expiry_time(), None);
}

#[test]
fn linear_future_uses_base_multiplier_and_quote_settlement() {
    let raw = br#"{
      "jsonrpc":"2.0",
      "id":44,
      "result":[{
        "tick_size":0.05,
        "tick_size_steps":[{"above_price":100000,"tick_size":0.1}],
        "settlement_period":"week",
        "settlement_currency":"USDC",
        "quote_currency":"USDC",
        "price_index":"eth_usdc",
        "min_trade_amount":0.001,
        "kind":"future",
        "is_active":true,
        "instrument_name":"ETH-29JUN26",
        "instrument_id":900004,
        "instrument_type":"linear",
        "future_type":"linear",
        "expiration_timestamp":1782720000000,
        "creation_timestamp":1750000000000,
        "counter_currency":"USDC",
        "contract_size":1,
        "base_currency":"ETH",
        "state":"open",
        "qty_tick_size":0.001,
        "underlying_type":"crypto"
      }]
    }"#;
    let metadata = one_instrument(raw);
    let base = AssetId::new(AssetNamespace::Native, "ethereum", "", "ETH", 1).unwrap();
    let quote = AssetId::new(AssetNamespace::Synthetic, "", "", "USDC", 1).unwrap();
    let binding =
        connector_deribit::DeribitInstrumentBinding::try_new(4, base, quote.clone(), quote)
            .unwrap();
    let definition = metadata.to_definition(&binding).unwrap();
    assert_eq!(definition.product_type(), ProductType::Future);
    assert_eq!(definition.contract_multiplier(), decimal("1"));
    assert_eq!(definition.quantity_step(), quantity("0.001"));
}

#[test]
fn ticker_variants_preserve_funding_and_delivery_without_conflation() {
    let DeribitMessage::Ticker(perpetual) =
        parse_native_message(DeribitInput::Subscription, PERPETUAL_TICKER).unwrap()
    else {
        panic!("expected perpetual ticker");
    };
    assert_eq!(perpetual.current_funding, Some(decimal("0.0000021")));
    assert_eq!(perpetual.funding_8h, Some(decimal("0.0000168")));
    assert_eq!(perpetual.delivery_price, None);
    assert_eq!(perpetual.minimum_price, price("63000"));
    assert_eq!(perpetual.maximum_price, price("66000"));
    assert_eq!(perpetual.stats.volume, quantity("7871.02139035"));
    assert_eq!(
        perpetual.stats.price_change_percent,
        Some(decimal("0.7229"))
    );

    let DeribitMessage::Ticker(future) =
        parse_native_message(DeribitInput::Subscription, FUTURE_TICKER).unwrap()
    else {
        panic!("expected future ticker");
    };
    assert_eq!(future.lifecycle, DeribitLifecycle::Settlement);
    assert_eq!(future.delivery_price, Some(price("64498.125")));
    assert_eq!(future.current_funding, None);
    assert_eq!(future.best_bid_price, None);
    assert_eq!(future.best_bid_amount, quantity("0"));
}

#[test]
fn book_continuity_gap_and_replacement_snapshot_are_fail_closed() {
    let DeribitMessage::Book(snapshot) =
        parse_native_message(DeribitInput::Subscription, BOOK_SNAPSHOT).unwrap()
    else {
        panic!("expected book snapshot");
    };
    let DeribitMessage::Book(change) =
        parse_native_message(DeribitInput::Subscription, BOOK_CHANGE).unwrap()
    else {
        panic!("expected book change");
    };
    assert_eq!(snapshot.kind, BookMessageKind::Snapshot);
    assert_eq!(change.previous_change_id, Some(100));

    let mut sync = DeribitBookSynchronizer::try_new(book_config()).unwrap();
    sync.start_session(session(1)).unwrap();
    assert_eq!(sync.apply(&snapshot, 1).unwrap(), ApplyResult::Applied);
    assert_eq!(sync.apply(&change, 2).unwrap(), ApplyResult::Applied);
    let view = sync.snapshot().unwrap();
    assert_eq!(view.bids()[0].quantity, quantity("1200"));
    assert_eq!(view.asks().len(), 2);

    let mut gap = change.clone();
    gap.change_id = 103;
    gap.previous_change_id = Some(102);
    assert_eq!(sync.apply(&gap, 3).unwrap(), ApplyResult::GapDetected);
    assert_eq!(sync.state(), BookState::Untrusted);
    assert!(sync.snapshot().is_err());

    let mut replacement = snapshot.clone();
    replacement.change_id = 200;
    assert_eq!(sync.apply(&replacement, 4).unwrap(), ApplyResult::Applied);
    assert_eq!(sync.state(), BookState::Synchronized);
    assert_eq!(
        sync.snapshot().unwrap().bids()[0].quantity,
        quantity("1000")
    );

    sync.disconnect();
    sync.start_session(session(2)).unwrap();
    assert_eq!(
        sync.apply(&change, 5).unwrap(),
        ApplyResult::SnapshotRequired
    );
}

#[test]
fn book_actions_must_match_local_state_transactionally() {
    let DeribitMessage::Book(snapshot) =
        parse_native_message(DeribitInput::Subscription, BOOK_SNAPSHOT).unwrap()
    else {
        panic!("expected book snapshot");
    };
    let DeribitMessage::Book(mut change) =
        parse_native_message(DeribitInput::Subscription, BOOK_CHANGE).unwrap()
    else {
        panic!("expected book change");
    };
    change.asks[0].price = price("65000");
    change.asks[0].price_text = "65000".to_owned();
    let mut sync = DeribitBookSynchronizer::try_new(book_config()).unwrap();
    sync.start_session(session(1)).unwrap();
    sync.apply(&snapshot, 1).unwrap();
    assert!(sync.apply(&change, 2).is_err());
    assert_eq!(sync.state(), BookState::Synchronized);
    assert_eq!(sync.snapshot().unwrap().asks().len(), 2);
}

#[test]
fn trade_interest_and_delivery_records_parse_exactly() {
    let DeribitMessage::Trades(trades) =
        parse_native_message(DeribitInput::Subscription, TRADES).unwrap()
    else {
        panic!("expected trades");
    };
    assert_eq!(trades.len(), 2);
    assert_eq!(trades[1].trade_sequence, 30_289_443);
    assert_eq!(trades[1].liquidation.as_deref(), Some("T"));
    assert_eq!(
        trades[1].starbase_timestamp,
        Some(UnixNanos::new(1_760_000_000_723_000_000))
    );
    assert_eq!(trades[1].starbase_match_id, Some(90_210));
    assert_eq!(trades[1].block_rfq_id, Some(77));

    let DeribitMessage::PerpetualInterest(interest) =
        parse_native_message(DeribitInput::Subscription, PERPETUAL_INTEREST).unwrap()
    else {
        panic!("expected perpetual interest");
    };
    assert_eq!(interest.interest, decimal("0.004999511380756577"));

    let DeribitMessage::DeliveryPrices {
        records,
        records_total,
    } = parse_native_message(DeribitInput::DeliveryPricesResponse, DELIVERY_PRICES).unwrap()
    else {
        panic!("expected delivery prices");
    };
    assert_eq!(records_total, 2);
    assert_eq!(records[0].delivery_price, price("64498.125"));
}

#[test]
fn documented_option_trade_extensions_are_retained() {
    let raw = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"trades.BTC-29JUN26-65000-C.100ms","data":[{"trade_seq":7,"trade_id":"option-7","timestamp":1760000000000,"starbase_timestamp":1760000000000000000,"tick_direction":1,"price":0.0525,"mark_price":0.0524,"instrument_name":"BTC-29JUN26-65000-C","index_price":64500.25,"direction":"buy","amount":0.2,"contracts":2,"iv":52.7,"block_trade_id":"block-9","block_trade_leg_count":2,"combo_id":"BTC-COMBO-1","combo_trade_id":"combo-trade-1","starbase_match_id":123,"block_rfq_id":456}]}}"#;
    let DeribitMessage::Trades(trades) =
        parse_native_message(DeribitInput::Subscription, raw).unwrap()
    else {
        panic!("expected option trade");
    };
    let trade = &trades[0];
    assert_eq!(trade.implied_volatility, Some(decimal("52.7")));
    assert_eq!(trade.block_trade_id.as_deref(), Some("block-9"));
    assert_eq!(trade.block_trade_leg_count, Some(2));
    assert_eq!(trade.combo_id.as_deref(), Some("BTC-COMBO-1"));
    assert_eq!(trade.combo_trade_id.as_deref(), Some("combo-trade-1"));
    assert_eq!(trade.starbase_match_id, Some(123));
    assert_eq!(trade.block_rfq_id, Some(456));
}

#[test]
fn parser_rejects_duplicates_schema_drift_route_mismatch_and_unbounded_input() {
    let duplicate = br#"{"jsonrpc":"2.0","jsonrpc":"2.0","method":"subscription","params":{"channel":"perpetual.BTC-PERPETUAL.100ms","data":{"interest":1,"timestamp":1,"index_price":1}}}"#;
    assert_eq!(
        parse_native_message(DeribitInput::Subscription, duplicate),
        Err(NativeParseError::MalformedJson)
    );
    let drift = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"perpetual.BTC-PERPETUAL.100ms","data":{"interest":1,"timestamp":1,"index_price":1,"surprise":true}}}"#;
    assert_eq!(
        parse_native_message(DeribitInput::Subscription, drift),
        Err(NativeParseError::SchemaDrift)
    );
    let mismatch = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.ETH-PERPETUAL.100ms","data":{"type":"snapshot","timestamp":1,"instrument_name":"BTC-PERPETUAL","change_id":1,"bids":[["new",1,1]],"asks":[]}}}"#;
    assert_eq!(
        parse_native_message(DeribitInput::Subscription, mismatch),
        Err(NativeParseError::InvalidValue)
    );
    let exponent = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"perpetual.BTC-PERPETUAL.100ms","data":{"interest":1e-39,"timestamp":1,"index_price":1}}}"#;
    assert_eq!(
        parse_native_message(DeribitInput::Subscription, exponent),
        Err(NativeParseError::SchemaDrift)
    );
    let oversized = vec![b' '; connector_deribit::MAX_NATIVE_PAYLOAD_BYTES + 1];
    assert_eq!(
        parse_native_message(DeribitInput::Subscription, &oversized),
        Err(NativeParseError::PayloadTooLarge)
    );
}

#[test]
fn scientific_decimals_and_fractional_linear_option_symbols_are_supported() {
    let book = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"book.BTC-PERPETUAL.100ms","data":{"type":"snapshot","timestamp":1760000000000,"instrument_name":"BTC-PERPETUAL","change_id":1,"bids":[["new",6.451E+4,1e3]],"asks":[]}}}"#;
    let DeribitMessage::Book(book) =
        parse_native_message(DeribitInput::Subscription, book).unwrap()
    else {
        panic!("expected scientific-notation book");
    };
    assert_eq!(book.bids[0].price, price("64510"));
    assert_eq!(book.bids[0].quantity, quantity("1000"));
    assert_eq!(book.bids[0].price_text, "6.451E+4");
    assert_eq!(book.bids[0].quantity_text, "1e3");

    let instrument = br#"{"jsonrpc":"2.0","id":0,"result":[{"tick_size":1e-4,"tick_size_steps":[],"settlement_period":"month","settlement_currency":"USDC","quote_currency":"USDC","price_index":"xrp_usdc","min_trade_amount":1,"kind":"option","is_active":true,"instrument_name":"XRP_USDC-29JUN26-0d025-C","instrument_id":9,"instrument_type":"linear","expiration_timestamp":1782720000000,"creation_timestamp":1750000000000,"counter_currency":"USDC","contract_size":1,"base_currency":"XRP","state":"open","qty_tick_size":1,"underlying_type":"crypto","strike":2.5e-2,"option_type":"call"}]}"#;
    let metadata = one_instrument(instrument);
    assert_eq!(metadata.strike, Some(price("0.025")));
    assert_eq!(
        metadata.instrument_type,
        connector_deribit::DeribitInstrumentType::Linear
    );
}

#[test]
fn channel_interval_and_product_specific_requirements_are_enforced() {
    let invalid_interval = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"perpetual.BTC-PERPETUAL.10ms","data":{"interest":1,"timestamp":1,"index_price":1}}}"#;
    assert_eq!(
        parse_native_message(DeribitInput::Subscription, invalid_interval),
        Err(NativeParseError::UnsupportedMessage)
    );
    let incomplete_option = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-29JUN26-65000-C.100ms","data":{"instrument_name":"BTC-29JUN26-65000-C","timestamp":1,"state":"open","stats":{"volume":0,"low":null,"high":null,"price_change":null},"index_price":1,"min_price":0.1,"max_price":2,"mark_price":1,"estimated_delivery_price":1,"last_price":null,"best_bid_price":null,"best_bid_amount":0,"best_ask_price":null,"best_ask_amount":0,"open_interest":1,"mark_iv":50}}}"#;
    assert_eq!(
        parse_native_message(DeribitInput::Subscription, incomplete_option),
        Err(NativeParseError::InvalidValue)
    );
    let mismatched_option_symbol = br#"{"jsonrpc":"2.0","id":1,"result":[{"tick_size":0.1,"tick_size_steps":[],"settlement_period":"month","settlement_currency":"BTC","quote_currency":"USD","price_index":"btc_usd","min_trade_amount":0.1,"kind":"option","is_active":true,"instrument_name":"BTC-29JUN26-65001-C","instrument_id":1,"instrument_type":"reversed","expiration_timestamp":1782720000000,"creation_timestamp":1750000000000,"counter_currency":"USD","contract_size":1,"base_currency":"BTC","state":"open","qty_tick_size":0.1,"underlying_type":"crypto","strike":65000,"option_type":"call"}]}"#;
    assert_eq!(
        parse_native_message(DeribitInput::InstrumentsResponse, mismatched_option_symbol),
        Err(NativeParseError::InvalidValue)
    );
    let missing_required_stats = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"ticker.BTC-PERPETUAL.100ms","data":{"instrument_name":"BTC-PERPETUAL","timestamp":1,"state":"open","index_price":1,"min_price":0.5,"max_price":2,"mark_price":1,"estimated_delivery_price":1,"last_price":null,"best_bid_price":null,"best_bid_amount":0,"best_ask_price":null,"best_ask_amount":0,"open_interest":0}}}"#;
    assert_eq!(
        parse_native_message(DeribitInput::Subscription, missing_required_stats),
        Err(NativeParseError::SchemaDrift)
    );
    let invalid_delivery_date = br#"{"jsonrpc":"2.0","id":1,"result":{"data":[{"date":"2026-02-30","delivery_price":1}],"records_total":1}}"#;
    assert_eq!(
        parse_native_message(DeribitInput::DeliveryPricesResponse, invalid_delivery_date),
        Err(NativeParseError::InvalidValue)
    );
    assert_eq!(
        parse_native_message(
            DeribitInput::InstrumentsResponse,
            br#"{"jsonrpc":"2.0","id":0,"result":[]}"#
        ),
        Ok(DeribitMessage::Instruments(Vec::new()))
    );
    assert_eq!(
        parse_native_message(
            DeribitInput::DeliveryPricesResponse,
            br#"{"jsonrpc":"2.0","id":0,"result":{"data":[],"records_total":0}}"#
        ),
        Ok(DeribitMessage::DeliveryPrices {
            records: Vec::new(),
            records_total: 0,
        })
    );
}

#[test]
fn declared_capabilities_match_parser_and_recovery_contract() {
    let capabilities = capabilities().unwrap();
    assert_eq!(capabilities.venue().as_str(), "deribit");
    assert_eq!(
        capabilities
            .book_sequence_semantics()
            .iter()
            .map(|rule| rule.semantics())
            .collect::<Vec<_>>(),
        vec![SequenceSemantics::PreviousAndCurrent; 4]
    );
    assert_ne!(
        capabilities.options_fields().bits() & OptionsFields::RHO.bits(),
        0
    );
    assert_eq!(
        capabilities
            .rate_limit_model()
            .iter()
            .map(|rule| rule.applies_to())
            .collect::<Vec<_>>(),
        vec![
            "public_subscribe",
            "public_get_instruments_sustained",
            "public_stream_connections",
        ]
    );
}

#[tokio::test]
async fn durable_parser_rejects_payload_not_bound_to_wal_receipt() {
    let reference = durable_reference(OPTION_TICKER).await;
    let parsed =
        parse_durable_native_message(DeribitInput::Subscription, OPTION_TICKER, &reference)
            .unwrap();
    assert_eq!(
        parsed.raw_payload_hash(),
        blake3::hash(OPTION_TICKER).as_bytes()
    );
    assert_eq!(
        parse_durable_native_message(DeribitInput::Subscription, FUTURE_TICKER, &reference),
        Err(NativeParseError::RawPayloadMismatch)
    );
}

fn one_instrument(raw: &[u8]) -> connector_deribit::DeribitInstrument {
    let DeribitMessage::Instruments(mut instruments) =
        parse_native_message(DeribitInput::InstrumentsResponse, raw).unwrap()
    else {
        panic!("expected instrument response");
    };
    assert_eq!(instruments.len(), 1);
    instruments.remove(0)
}

fn inverse_btc_binding(generation: u32) -> connector_deribit::DeribitInstrumentBinding {
    let base = AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).unwrap();
    let quote = AssetId::new(AssetNamespace::Fiat, "", "", "USD", 1).unwrap();
    connector_deribit::DeribitInstrumentBinding::try_new(generation, base.clone(), quote, base)
        .unwrap()
}

fn book_config() -> BookConfig {
    BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("deribit").unwrap(),
            "BTC-PERPETUAL",
            ProductType::Perpetual,
            1,
        )
        .unwrap(),
        price_tick: price("0.5"),
        quantity_step: quantity("10"),
        max_levels_per_side: 100,
        max_buffered_deltas: 16,
        max_buffered_level_updates: 128,
        sequence_policy: SequencePolicy::PreviousFinal,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    }
}

fn session(connection_epoch: u64) -> BookSession {
    BookSession {
        connection_epoch,
        subscription_epoch: 1,
        instrument_generation: 1,
    }
}

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse(value).unwrap()
}

fn price(value: &str) -> Price {
    Price::new(decimal(value)).unwrap()
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(decimal(value)).unwrap()
}

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "deribit", 1).unwrap()
}

async fn durable_reference(payload: &[u8]) -> connector_core::DurableRawReference {
    let directory = tempfile::tempdir().unwrap();
    let mut segment_id = [0_u8; 16];
    segment_id.copy_from_slice(&blake3::hash(payload).as_bytes()[..16]);
    let metadata = SegmentMetadata::new(
        segment_id,
        1,
        "schema",
        "installation",
        "build",
        vec![StreamDescriptor::new(7, wal_stream_source_identity(&source()), "fixture").unwrap()],
    )
    .unwrap();
    let mut writer =
        SegmentedWalWriter::create(directory.path(), metadata, RotationPolicy::default(), 1)
            .unwrap();
    let authority = writer.append_authority();
    let receive_time = UnixNanos::new(1_760_000_000_000_000_000);
    let (proof, compression) = writer
        .append(
            RecordMetadata {
                flags: 0,
                stream_id: 7,
                connection_epoch: 3,
                record_sequence: 1,
                receive_wall_time_ns: receive_time.value(),
                receive_monotonic_time_ns: 9,
            },
            payload,
            9,
            receive_time.value(),
        )
        .unwrap()
        .into_parts();
    assert!(compression.is_none());
    let channel = DurableRawCaptureChannel::new(
        authority,
        source(),
        NonZeroU32::new(7).unwrap(),
        1,
        payload.len() + 1,
    )
    .unwrap();
    let (client, mut worker) = channel.split();
    let pending = client
        .try_submit(
            RawCapture::try_new(
                source(),
                NonZeroU32::new(7).unwrap(),
                NonZeroU64::new(3).unwrap(),
                NonZeroU64::new(1).unwrap(),
                receive_time,
                9,
                payload.to_vec().into_boxed_slice(),
            )
            .unwrap(),
        )
        .unwrap();
    worker.recv().await.unwrap().acknowledge(proof).unwrap();
    pending.wait().await.unwrap()
}
