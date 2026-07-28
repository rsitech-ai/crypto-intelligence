use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId,
};
use event_envelope::{
    AlertEvent, AlertStatus, AvailabilityState, BookDelta, BookLevel, BookSnapshot, ChainBlock,
    ChainMempoolObservation, ChainMetric, ChainTransactionAggregate, DataQualityObservation,
    EventEnvelope, EventType, ExternalMarketObservation, FinalityState, FundingObservation,
    FutureBasisObservation, LiquidationObservation, MAX_EVENT_WIRE_BYTES, MarkIndexObservation,
    OpenInterestObservation, OptionTicker, OptionTrade, PredictionRecord, QualityFlags, Side,
    SnapshotKind, StructuredEvent, StructuredFact, TopOfBook, Trade, UncheckedEventMetadata,
    UncheckedEventPayload, VenueState, VenueStatus, canonical_event_id_unchecked,
};
use fixed_decimal::{FixedDecimal, Notional, Price, Quantity, Rate};

fn price(value: &str) -> Price {
    Price::new(FixedDecimal::parse_canonical(value).expect("canonical fixture price"))
        .expect("fixture price must be positive")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(FixedDecimal::parse_canonical(value).expect("canonical fixture quantity"))
        .expect("fixture quantity must be nonnegative")
}

fn rate(value: &str) -> Rate {
    Rate::new(FixedDecimal::parse_canonical(value).expect("canonical fixture rate"))
        .expect("fixture rate must be valid")
}

fn notional(value: &str) -> Notional {
    Notional::new(FixedDecimal::parse_canonical(value).expect("canonical fixture notional"))
        .expect("fixture notional must be nonnegative")
}

fn native_asset() -> AssetId {
    AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("fixture native asset")
}

fn fiat_asset() -> AssetId {
    AssetId::new(AssetNamespace::Fiat, "", "", "USD", 1).expect("fixture fiat asset")
}

fn instrument_definition() -> InstrumentDefinition {
    let venue = VenueId::new("binance").expect("fixture venue");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new(venue, "BTCUSDT", 1).expect("fixture instrument"),
        product_type: ProductType::Spot,
        base_asset: native_asset(),
        quote_asset: fiat_asset(),
        settlement_asset: fiat_asset(),
        contract_multiplier: FixedDecimal::new(1, 0).expect("fixture multiplier"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::None,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.1"),
        quantity_step: quantity("0.001"),
        listing_time: UnixNanos::new(1),
        delisting_time: None,
    })
    .expect("fixture instrument definition")
}

fn metadata() -> UncheckedEventMetadata {
    let venue = VenueId::new("binance").expect("fixture venue");
    UncheckedEventMetadata {
        schema_version: 1,
        source: SourceId::new(SourceKind::Exchange, "binance-fixture", 1).expect("fixture source"),
        venue: Some(venue.clone()),
        instrument_id: Some(
            InstrumentId::new(venue, "BTCUSDT", 1).expect("fixture instrument identity"),
        ),
        exchange_timestamp: Some(UnixNanos::new(1_000)),
        exchange_transaction_timestamp: Some(UnixNanos::new(1_001)),
        receive_wall_timestamp: UnixNanos::new(1_100),
        receive_monotonic_ns: 100,
        normalization_timestamp: UnixNanos::new(1_101),
        connection_started_at: UnixNanos::new(900),
        sequence_number: Some(10),
        previous_sequence_number: Some(9),
        connection_epoch: 1,
        subscription_epoch: 1,
        snapshot_kind: SnapshotKind::NotApplicable,
        source_checksum: None,
        raw_payload_hash: [7; 32],
        parser_version: "parser-v1".to_owned(),
        normalizer_version: "normalizer-v1".to_owned(),
        ingestion_instance: "ingestion-1".to_owned(),
        quality_score_ppm: 1_000_000,
        quality_flags: QualityFlags::NONE,
    }
}

fn trade_payload() -> UncheckedEventPayload {
    UncheckedEventPayload::Trade(Trade {
        trade_id: "trade-1".to_owned(),
        price: price("60000"),
        quantity: quantity("1"),
        side: Side::Buy,
    })
}

#[test]
fn event_type_is_derived_from_the_payload() {
    let payload = trade_payload();
    assert_eq!(payload.event_type(), EventType::Trade);

    let envelope =
        EventEnvelope::new(metadata(), payload).expect("complete envelope must validate");
    assert_eq!(envelope.event_type(), EventType::Trade);
    let encoded = serde_json::to_value(&envelope).expect("envelope must serialize");
    assert_eq!(encoded["event_type"], serde_json::json!("trade"));
    assert!(encoded.get("event_id").is_some());
    assert!(encoded.get("id").is_none());
    assert!(encoded.get("metadata").is_none());
    for field in [
        "schema_version",
        "source",
        "venue",
        "instrument_id",
        "exchange_event_time_ns",
        "exchange_transaction_time_ns",
        "receive_wall_time_ns",
        "receive_monotonic_time_ns",
        "normalization_time_ns",
        "snapshot_or_delta",
    ] {
        assert!(
            encoded.get(field).is_some(),
            "required wire field {field} must be present"
        );
    }
}

#[test]
fn complete_timing_epoch_and_sequence_contract_fails_closed() {
    let valid = metadata();
    EventEnvelope::new(valid.clone(), trade_payload()).expect("fixture must be valid");

    let mut invalid = valid.clone();
    invalid.subscription_epoch = 0;
    assert!(EventEnvelope::new(invalid, trade_payload()).is_err());

    let mut invalid = valid.clone();
    invalid.normalization_timestamp = UnixNanos::new(1_099);
    assert!(EventEnvelope::new(invalid, trade_payload()).is_err());

    let mut invalid = valid.clone();
    invalid.previous_sequence_number = Some(10);
    assert!(EventEnvelope::new(invalid, trade_payload()).is_err());

    let mut invalid = valid.clone();
    invalid.schema_version = 3;
    assert!(EventEnvelope::new(invalid, trade_payload()).is_err());

    let mut invalid = valid;
    invalid.parser_version.clear();
    assert!(EventEnvelope::new(invalid, trade_payload()).is_err());
}

#[test]
fn approved_payload_registry_is_exhaustive_and_stable() {
    assert_eq!(EventType::ALL.len(), 22);
    assert_eq!(EventType::ALL[0], EventType::Trade);
    assert_eq!(EventType::ALL[21], EventType::AlertEvent);
}

fn approved_payloads() -> Vec<UncheckedEventPayload> {
    let level = BookLevel {
        price: price("60000"),
        quantity: quantity("1"),
        order_count: Some(1),
    };
    let chain = native_asset();
    let observed_at = UnixNanos::new(1_000);
    vec![
        trade_payload(),
        UncheckedEventPayload::TopOfBook(TopOfBook {
            bid: Some(level.clone()),
            ask: None,
        }),
        UncheckedEventPayload::BookSnapshot(BookSnapshot {
            bids: vec![level.clone()],
            asks: Vec::new(),
            last_sequence: 10,
        }),
        UncheckedEventPayload::BookDelta(BookDelta {
            bids: vec![level.clone()],
            asks: Vec::new(),
            first_sequence: 10,
            last_sequence: 10,
        }),
        UncheckedEventPayload::InstrumentDefinition(instrument_definition()),
        UncheckedEventPayload::FundingObservation(FundingObservation {
            funding_rate: rate("0.0001"),
            observed_at,
            next_funding_time: Some(UnixNanos::new(2_000)),
        }),
        UncheckedEventPayload::OpenInterestObservation(OpenInterestObservation {
            quantity: quantity("100"),
            quote_notional: Some(notional("6000000")),
            observed_at,
        }),
        UncheckedEventPayload::LiquidationObservation(LiquidationObservation {
            liquidation_id: "liquidation-1".to_owned(),
            price: price("59000"),
            quantity: quantity("1"),
            side: Side::Sell,
        }),
        UncheckedEventPayload::MarkIndexObservation(MarkIndexObservation {
            mark_price: price("60000"),
            index_price: price("60001"),
        }),
        UncheckedEventPayload::FutureBasisObservation(FutureBasisObservation {
            future_price: price("61000"),
            reference_price: price("60000"),
            basis_rate: rate("0.016666666666666666"),
            annualized_basis_rate: Some(rate("0.2")),
        }),
        UncheckedEventPayload::OptionTicker(OptionTicker {
            bid_price: Some(price("1000")),
            ask_price: Some(price("1001")),
            mark_price: price("1000.5"),
            mark_iv: rate("0.55"),
            delta: rate("0.5"),
            open_interest: quantity("100"),
        }),
        UncheckedEventPayload::OptionTrade(OptionTrade {
            trade_id: "option-trade-1".to_owned(),
            price: price("1000"),
            quantity: quantity("1"),
            side: Side::Buy,
            implied_volatility: Some(rate("0.55")),
        }),
        UncheckedEventPayload::VenueStatus(VenueStatus {
            state: VenueState::Operational,
            observed_at,
            message: Some("normal operations".to_owned()),
        }),
        UncheckedEventPayload::ChainBlock(ChainBlock {
            chain: chain.clone(),
            block_hash: "block-1".to_owned(),
            parent_hash: Some("block-0".to_owned()),
            height: 1,
            block_time: observed_at,
            confirmation_depth: 1,
            finality: FinalityState::Confirmed,
            transaction_count: 1,
        }),
        UncheckedEventPayload::ChainTransactionAggregate(ChainTransactionAggregate {
            chain: chain.clone(),
            interval_start: UnixNanos::new(900),
            interval_end: observed_at,
            transaction_count: 1,
            transferred_value: Some(notional("1")),
            fee_total: Some(notional("0.001")),
        }),
        UncheckedEventPayload::ChainMempoolObservation(ChainMempoolObservation {
            chain: chain.clone(),
            observed_at,
            transaction_count: 1,
            virtual_size: 100,
            total_fees: Some(notional("0.001")),
        }),
        UncheckedEventPayload::ChainMetric(ChainMetric {
            chain,
            metric_name: "hash_rate".to_owned(),
            value: FixedDecimal::new(1, 0).expect("fixture metric"),
            observed_at,
        }),
        UncheckedEventPayload::ExternalMarketObservation(ExternalMarketObservation {
            market_id: "dxy".to_owned(),
            value: FixedDecimal::new(100, 0).expect("fixture external value"),
            observed_at,
            source_native_id: Some("release-1".to_owned()),
        }),
        UncheckedEventPayload::StructuredEvent(StructuredEvent {
            structured_event_id: "structured-1".to_owned(),
            event_type: "listing".to_owned(),
            affected_assets: vec![native_asset()],
            affected_venues: vec![VenueId::new("binance").expect("fixture venue")],
            affected_protocols: vec!["bitcoin".to_owned()],
            source_identity: SourceId::new(SourceKind::News, "official-release", 1)
                .expect("fixture source"),
            source_reliability_ppm: 1_000_000,
            publication_time: observed_at,
            effective_time: Some(UnixNanos::new(2_000)),
            expected_end_time: None,
            direction_prior: rate("1"),
            severity_ppm: 500_000,
            extraction_confidence_ppm: 900_000,
            human_verified: true,
            source_document_hash: [3; 32],
            extractor_model_id: "rules-v1".to_owned(),
            extractor_prompt_version: "prompt-v1".to_owned(),
            extracted_facts: vec![StructuredFact {
                key: "symbol".to_owned(),
                value: "BTC".to_owned(),
            }],
        }),
        UncheckedEventPayload::DataQualityObservation(DataQualityObservation {
            component: "binance-book".to_owned(),
            observed_at,
            score_ppm: 1_000_000,
            flags: QualityFlags::NONE,
            details: Some("complete".to_owned()),
        }),
        UncheckedEventPayload::PredictionRecord(PredictionRecord {
            forecast_id: "forecast-1".to_owned(),
            issued_at: observed_at,
            entity_id: "BTC".to_owned(),
            model_package_id: "model-v1".to_owned(),
            forecast_schema_version: 1,
            prediction_event_type: "downside_transition".to_owned(),
            horizon_seconds: 14_400,
            calibrated_probability_ppm: 300_000,
            raw_score: rate("0.2"),
            base_rate_ppm: 100_000,
            lower_uncertainty_ppm: 200_000,
            upper_uncertainty_ppm: 400_000,
            availability: AvailabilityState::Available,
            quality_score_ppm: 900_000,
            applicability_score_ppm: 900_000,
            scenario_summary_id: "scenario-1".to_owned(),
            evidence_bundle_id: "evidence-1".to_owned(),
            supersedes_forecast_id: Some("forecast-0".to_owned()),
        }),
        UncheckedEventPayload::AlertEvent(AlertEvent {
            alert_id: "alert-1".to_owned(),
            rule_id: "rule-1".to_owned(),
            status: AlertStatus::Recovered,
            priority: 2,
            triggered_at: observed_at,
            recovered_at: Some(UnixNanos::new(1_001)),
            forecast_id: Some("forecast-1".to_owned()),
            message: "Threshold crossed".to_owned(),
        }),
    ]
}

#[test]
fn every_approved_payload_variant_has_one_derived_event_type() {
    let payloads = approved_payloads();
    assert_eq!(payloads.len(), EventType::ALL.len());
    assert_eq!(
        payloads
            .iter()
            .map(UncheckedEventPayload::event_type)
            .collect::<Vec<_>>(),
        EventType::ALL
    );
}

#[test]
fn every_approved_payload_validates_hashes_and_round_trips() {
    for payload in approved_payloads() {
        let event_type = payload.event_type();
        let mut event_metadata = metadata();
        match event_type {
            EventType::BookSnapshot => {
                event_metadata.snapshot_kind = SnapshotKind::Snapshot;
            }
            EventType::BookDelta => {
                event_metadata.snapshot_kind = SnapshotKind::Delta;
            }
            EventType::ChainBlock
            | EventType::ChainTransactionAggregate
            | EventType::ChainMempoolObservation
            | EventType::ChainMetric => {
                event_metadata.source =
                    SourceId::new(SourceKind::Chain, "bitcoin-core", 1).expect("chain source");
            }
            EventType::StructuredEvent => {
                event_metadata.source =
                    SourceId::new(SourceKind::News, "official-release", 1).expect("news source");
            }
            _ => {}
        }

        let envelope = EventEnvelope::new(event_metadata, payload)
            .unwrap_or_else(|error| panic!("{event_type:?} must validate: {error}"));
        envelope
            .verify()
            .unwrap_or_else(|error| panic!("{event_type:?} identity must verify: {error}"));
        let encoded = serde_json::to_string(&envelope).expect("envelope must serialize");
        let decoded =
            EventEnvelope::from_json_slice(encoded.as_bytes()).expect("envelope must revalidate");
        assert_eq!(decoded, envelope, "{event_type:?} must round-trip");
    }
}

#[test]
fn untrusted_json_is_size_bounded_before_deserialization() {
    let oversized = vec![b' '; MAX_EVENT_WIRE_BYTES + 1];
    assert_eq!(
        EventEnvelope::from_json_slice(&oversized),
        Err(event_envelope::EventError::WireTooLarge)
    );
}

fn structured_payload_with_protocols(protocols: Vec<String>) -> UncheckedEventPayload {
    let mut structured = match approved_payloads()
        .into_iter()
        .find(|payload| payload.event_type() == EventType::StructuredEvent)
        .expect("structured payload fixture")
    {
        UncheckedEventPayload::StructuredEvent(value) => value,
        _ => unreachable!("event type lookup is exact"),
    };
    structured.affected_protocols = protocols;
    UncheckedEventPayload::StructuredEvent(structured)
}

fn structured_metadata() -> UncheckedEventMetadata {
    let mut value = metadata();
    value.source =
        SourceId::new(SourceKind::News, "official-release", 1).expect("structured source");
    value
}

#[test]
fn aggregate_wire_budget_accepts_the_largest_event_and_rejects_one_byte_more() {
    let full_protocol = "a".repeat(4_096);
    let mut protocols = vec![full_protocol; 4_000];
    let base = EventEnvelope::new(
        structured_metadata(),
        structured_payload_with_protocols(protocols.clone()),
    )
    .expect("base boundary fixture must fit");
    let base_wire = serde_json::to_vec(&base).expect("base fixture must serialize");
    let remaining = MAX_EVENT_WIRE_BYTES - base_wire.len();

    let tail_count = (1_usize..=1_000)
        .find(|count| {
            let overhead = 3 * count;
            remaining >= overhead + count && remaining - overhead < 4_095 * count
        })
        .expect("remaining budget must be representable by bounded protocol strings");
    let mut content_remaining = remaining - (3 * tail_count);
    for index in 0..tail_count {
        let slots_after = tail_count - index - 1;
        let length = (content_remaining - slots_after).min(4_095);
        protocols.push("b".repeat(length));
        content_remaining -= length;
    }
    assert_eq!(content_remaining, 0);

    let largest = EventEnvelope::new(
        structured_metadata(),
        structured_payload_with_protocols(protocols.clone()),
    )
    .expect("exactly bounded event must construct");
    let largest_wire = serde_json::to_vec(&largest).expect("largest event must serialize");
    assert_eq!(largest_wire.len(), MAX_EVENT_WIRE_BYTES);
    assert_eq!(
        EventEnvelope::from_json_slice(&largest_wire).expect("largest event must revalidate"),
        largest
    );

    protocols
        .last_mut()
        .expect("tail protocol must exist")
        .push('c');
    assert_eq!(
        EventEnvelope::new(
            structured_metadata(),
            structured_payload_with_protocols(protocols),
        ),
        Err(event_envelope::EventError::WireTooLarge)
    );
}

#[derive(Clone, Debug)]
enum JsonPathPart {
    Key(String),
    Index(usize),
}

fn collect_leaf_paths(
    value: &serde_json::Value,
    path: &mut Vec<JsonPathPart>,
    paths: &mut Vec<Vec<JsonPathPart>>,
) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                path.push(JsonPathPart::Key(key.clone()));
                collect_leaf_paths(value, path, paths);
                path.pop();
            }
        }
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                path.push(JsonPathPart::Index(index));
                collect_leaf_paths(value, path, paths);
                path.pop();
            }
        }
        serde_json::Value::Null => {}
        _ => paths.push(path.clone()),
    }
}

fn value_at_path_mut<'a>(
    mut value: &'a mut serde_json::Value,
    path: &[JsonPathPart],
) -> &'a mut serde_json::Value {
    for part in path {
        value = match part {
            JsonPathPart::Key(key) => value
                .get_mut(key)
                .unwrap_or_else(|| panic!("missing object key {key}")),
            JsonPathPart::Index(index) => value
                .get_mut(*index)
                .unwrap_or_else(|| panic!("missing array index {index}")),
        };
    }
    value
}

fn mutation_candidates(value: &serde_json::Value) -> Vec<serde_json::Value> {
    match value {
        serde_json::Value::Bool(value) => vec![serde_json::Value::Bool(!value)],
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                vec![
                    serde_json::json!(value.saturating_add(1)),
                    serde_json::json!(value.saturating_sub(1)),
                ]
            } else if let Some(value) = number.as_i64() {
                vec![
                    serde_json::json!(value.saturating_add(1)),
                    serde_json::json!(value.saturating_sub(1)),
                ]
            } else {
                Vec::new()
            }
        }
        serde_json::Value::String(value) => {
            let mut candidates = vec![serde_json::Value::String(format!("{value}x"))];
            candidates.extend(
                [
                    "1",
                    "2",
                    "0.1",
                    "-0.1",
                    "buy",
                    "sell",
                    "unknown",
                    "operational",
                    "degraded",
                    "confirmed",
                    "finalized",
                    "available",
                    "unavailable",
                    "triggered",
                    "updated",
                    "recovered",
                    "acknowledged",
                    "native",
                    "evm",
                    "solana",
                    "fiat",
                    "synthetic",
                    "spot",
                    "perpetual",
                    "future",
                    "option",
                    "base",
                    "quote",
                    "none",
                    "linear",
                    "inverse",
                    "call",
                    "put",
                    "exchange",
                    "chain",
                    "macro",
                    "news",
                    "derived",
                    "system",
                ]
                .into_iter()
                .map(|candidate| serde_json::Value::String(candidate.to_owned())),
            );
            candidates
        }
        _ => Vec::new(),
    }
}

#[test]
fn every_independently_mutable_payload_leaf_changes_canonical_identity() {
    let metadata = metadata();
    let mut checked_mutations = 0_usize;

    for payload in approved_payloads() {
        let baseline =
            canonical_event_id_unchecked(&metadata, &payload).expect("baseline identity");
        let encoded = serde_json::to_value(&payload).expect("payload must serialize");
        let mut paths = Vec::new();
        collect_leaf_paths(
            &encoded["value"],
            &mut vec![JsonPathPart::Key("value".to_owned())],
            &mut paths,
        );

        for path in paths {
            let original = value_at_path_mut(&mut encoded.clone(), &path).clone();
            for candidate in mutation_candidates(&original) {
                if candidate == original {
                    continue;
                }
                let mut mutated = encoded.clone();
                *value_at_path_mut(&mut mutated, &path) = candidate;
                let Ok(mutated_payload) = serde_json::from_value::<UncheckedEventPayload>(mutated)
                else {
                    continue;
                };
                if mutated_payload == payload {
                    continue;
                }

                let changed = canonical_event_id_unchecked(&metadata, &mutated_payload)
                    .expect("mutated identity");
                assert_ne!(
                    changed,
                    baseline,
                    "{:?} omitted identity-significant leaf {path:?}",
                    payload.event_type()
                );
                checked_mutations += 1;
                break;
            }
        }
    }

    assert!(
        checked_mutations >= 150,
        "expected broad field-level identity coverage, checked only {checked_mutations} mutations"
    );
}

#[test]
fn payload_specific_economic_and_lineage_invariants_fail_closed() {
    assert!(
        EventEnvelope::new(
            metadata(),
            UncheckedEventPayload::TopOfBook(TopOfBook {
                bid: None,
                ask: None,
            }),
        )
        .is_err()
    );

    assert!(
        EventEnvelope::new(
            metadata(),
            UncheckedEventPayload::FundingObservation(FundingObservation {
                funding_rate: rate("0.0001"),
                observed_at: UnixNanos::new(1_000),
                next_funding_time: Some(UnixNanos::new(1_000)),
            }),
        )
        .is_err()
    );

    assert!(
        EventEnvelope::new(
            metadata(),
            UncheckedEventPayload::LiquidationObservation(LiquidationObservation {
                liquidation_id: "liquidation-1".to_owned(),
                price: price("59000"),
                quantity: quantity("0"),
                side: Side::Unknown,
            }),
        )
        .is_err()
    );

    assert!(
        EventEnvelope::new(
            metadata(),
            UncheckedEventPayload::OptionTicker(OptionTicker {
                bid_price: Some(price("1002")),
                ask_price: Some(price("1001")),
                mark_price: price("1001"),
                mark_iv: rate("-0.1"),
                delta: rate("0.5"),
                open_interest: quantity("1"),
            }),
        )
        .is_err()
    );

    let chain_payload = approved_payloads()
        .into_iter()
        .find(|payload| payload.event_type() == EventType::ChainBlock)
        .expect("chain payload fixture");
    assert!(
        EventEnvelope::new(metadata(), chain_payload,).is_err(),
        "chain records must reject a non-chain source"
    );

    let mut invalid_structured = match approved_payloads()
        .into_iter()
        .find(|payload| payload.event_type() == EventType::StructuredEvent)
        .expect("structured payload fixture")
    {
        UncheckedEventPayload::StructuredEvent(value) => value,
        _ => unreachable!("event type lookup is exact"),
    };
    invalid_structured.source_document_hash = [0; 32];
    assert!(
        EventEnvelope::new(
            metadata(),
            UncheckedEventPayload::StructuredEvent(invalid_structured),
        )
        .is_err()
    );

    let mut invalid_prediction = match approved_payloads()
        .into_iter()
        .find(|payload| payload.event_type() == EventType::PredictionRecord)
        .expect("prediction payload fixture")
    {
        UncheckedEventPayload::PredictionRecord(value) => value,
        _ => unreachable!("event type lookup is exact"),
    };
    invalid_prediction.lower_uncertainty_ppm = invalid_prediction.calibrated_probability_ppm + 1;
    assert!(
        EventEnvelope::new(
            metadata(),
            UncheckedEventPayload::PredictionRecord(invalid_prediction),
        )
        .is_err()
    );

    let mut invalid_alert = match approved_payloads()
        .into_iter()
        .find(|payload| payload.event_type() == EventType::AlertEvent)
        .expect("alert payload fixture")
    {
        UncheckedEventPayload::AlertEvent(value) => value,
        _ => unreachable!("event type lookup is exact"),
    };
    invalid_alert.recovered_at = None;
    assert!(
        EventEnvelope::new(metadata(), UncheckedEventPayload::AlertEvent(invalid_alert),).is_err()
    );
}

#[test]
fn partial_book_delta_is_not_misclassified_as_a_crossed_complete_book() {
    let mut event_metadata = metadata();
    event_metadata.snapshot_kind = SnapshotKind::Delta;
    let payload = UncheckedEventPayload::BookDelta(BookDelta {
        bids: vec![BookLevel {
            price: price("101"),
            quantity: quantity("0"),
            order_count: None,
        }],
        asks: vec![BookLevel {
            price: price("100"),
            quantity: quantity("1"),
            order_count: None,
        }],
        first_sequence: 10,
        last_sequence: 10,
    });

    EventEnvelope::new(event_metadata, payload)
        .expect("partial delta crossing is evaluated only after applying it to complete state");
}

#[test]
fn futures_delta_preserves_the_source_previous_final_sequence() {
    let mut event_metadata = metadata();
    event_metadata.snapshot_kind = SnapshotKind::Delta;
    event_metadata.sequence_number = Some(160);
    event_metadata.previous_sequence_number = Some(149);
    let payload = UncheckedEventPayload::BookDelta(BookDelta {
        bids: vec![BookLevel {
            price: price("101"),
            quantity: quantity("1"),
            order_count: None,
        }],
        asks: Vec::new(),
        first_sequence: 157,
        last_sequence: 160,
    });

    assert!(
        EventEnvelope::new(event_metadata.clone(), payload.clone()).is_err(),
        "legacy schema 1 cannot encode an independent previous-final sequence"
    );

    event_metadata.schema_version = 2;
    let valid = EventEnvelope::new(event_metadata.clone(), payload.clone())
        .expect("Binance futures pu is the previous batch final, not U - 1");

    event_metadata.previous_sequence_number = Some(150);
    let different_predecessor = EventEnvelope::new(event_metadata.clone(), payload.clone())
        .expect("another source predecessor before U is structurally valid");
    assert_ne!(
        valid.id(),
        different_predecessor.id(),
        "a behavior-significant predecessor must change event identity"
    );

    event_metadata.previous_sequence_number = None;
    assert!(
        EventEnvelope::new(event_metadata.clone(), payload.clone()).is_err(),
        "book deltas require an explicit source predecessor"
    );

    event_metadata.previous_sequence_number = Some(157);
    assert!(
        EventEnvelope::new(event_metadata, payload).is_err(),
        "the previous final must precede the delta range"
    );
}

#[test]
fn zero_origin_delta_uses_the_versioned_none_predecessor_boundary() {
    let mut event_metadata = metadata();
    event_metadata.schema_version = 2;
    event_metadata.snapshot_kind = SnapshotKind::Delta;
    event_metadata.sequence_number = Some(0);
    event_metadata.previous_sequence_number = None;
    let payload = UncheckedEventPayload::BookDelta(BookDelta {
        bids: vec![BookLevel {
            price: price("101"),
            quantity: quantity("1"),
            order_count: None,
        }],
        asks: Vec::new(),
        first_sequence: 0,
        last_sequence: 0,
    });

    EventEnvelope::new(event_metadata, payload)
        .expect("a zero-origin source has no representable predecessor");
}
