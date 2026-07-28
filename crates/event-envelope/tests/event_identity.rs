use domain::{InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId};
use event_envelope::{
    BookDelta, BookLevel, BookSnapshot, EventEnvelope, QualityFlags, Side, SnapshotKind, Trade,
    UncheckedEventMetadata, UncheckedEventPayload, canonical_event_id_unchecked,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};

fn price(value: &str) -> Price {
    Price::new(fixed_decimal::FixedDecimal::parse_canonical(value).expect("canonical price"))
        .expect("positive price")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(fixed_decimal::FixedDecimal::parse_canonical(value).expect("canonical quantity"))
        .expect("non-negative quantity")
}

fn fixture() -> (UncheckedEventMetadata, UncheckedEventPayload) {
    let venue = VenueId::new("binance").expect("venue");
    let instrument = InstrumentId::new(venue.clone(), "BTCUSDT", 1).expect("instrument identity");
    let metadata = UncheckedEventMetadata {
        schema_version: 2,
        source: SourceId::new(SourceKind::Exchange, "binance-fixture", 1).expect("source"),
        venue: Some(venue),
        instrument_id: Some(instrument),
        exchange_timestamp: Some(UnixNanos::new(1_000)),
        exchange_transaction_timestamp: Some(UnixNanos::new(1_001)),
        receive_wall_timestamp: UnixNanos::new(1_100),
        receive_monotonic_ns: 100,
        normalization_timestamp: UnixNanos::new(1_101),
        connection_started_at: UnixNanos::new(900),
        sequence_number: Some(100),
        previous_sequence_number: Some(99),
        connection_epoch: 1,
        subscription_epoch: 1,
        snapshot_kind: SnapshotKind::Snapshot,
        source_checksum: Some("checksum".to_owned()),
        raw_payload_hash: [7; 32],
        parser_version: "parser-v1".to_owned(),
        normalizer_version: "normalizer-v1".to_owned(),
        ingestion_instance: "ingestion-1".to_owned(),
        quality_score_ppm: 1_000_000,
        quality_flags: QualityFlags::NONE,
    };
    let payload = UncheckedEventPayload::BookSnapshot(BookSnapshot {
        bids: vec![BookLevel {
            price: price("60000.1"),
            quantity: quantity("2"),
            order_count: Some(3),
        }],
        asks: vec![BookLevel {
            price: price("60000.2"),
            quantity: quantity("4"),
            order_count: Some(5),
        }],
        last_sequence: 100,
    });
    (metadata, payload)
}

#[test]
fn event_id_is_stable_and_domain_separated() {
    let (metadata, payload) = fixture();
    let first = canonical_event_id_unchecked(&metadata, &payload).expect("identity");
    let second = canonical_event_id_unchecked(&metadata, &payload).expect("identity");
    assert_eq!(first, second);

    let envelope = EventEnvelope::new(metadata, payload).expect("valid envelope");
    assert_eq!(envelope.id(), first);
    envelope.verify().expect("identity verifies");
}

#[test]
fn source_event_identity_fields_change_event_identity() {
    let (metadata, payload) = fixture();
    let baseline = canonical_event_id_unchecked(&metadata, &payload).expect("baseline identity");
    let mut mutations = Vec::new();

    macro_rules! mutation {
        ($field:ident, $value:expr) => {{
            let mut changed = metadata.clone();
            changed.$field = $value;
            mutations.push((stringify!($field), changed));
        }};
    }

    mutation!(
        source,
        SourceId::new(SourceKind::Exchange, "binance-fixture", 2).expect("source")
    );
    mutation!(schema_version, 1);
    mutation!(venue, Some(VenueId::new("kraken").expect("venue")));
    mutation!(
        instrument_id,
        Some(
            InstrumentId::new(VenueId::new("binance").expect("venue"), "BTCUSDT", 2)
                .expect("instrument")
        )
    );
    mutation!(exchange_timestamp, Some(UnixNanos::new(1_001)));
    mutation!(exchange_transaction_timestamp, Some(UnixNanos::new(1_002)));
    mutation!(sequence_number, Some(101));
    mutation!(previous_sequence_number, Some(98));

    for (field, changed) in mutations {
        assert_ne!(
            canonical_event_id_unchecked(&changed, &payload).expect("mutated identity"),
            baseline,
            "metadata field {field} was omitted from event identity"
        );
    }
}

#[test]
fn schema_three_identity_distinguishes_spot_and_perpetual_with_the_same_symbol() {
    let (mut spot_metadata, payload) = fixture();
    spot_metadata.schema_version = 3;
    let mut perpetual_metadata = spot_metadata.clone();
    perpetual_metadata.instrument_id = Some(
        InstrumentId::new_for_product(
            VenueId::new("binance").expect("venue"),
            "BTCUSDT",
            ProductType::Perpetual,
            1,
        )
        .expect("perpetual instrument"),
    );

    assert_ne!(
        canonical_event_id_unchecked(&spot_metadata, &payload).expect("spot identity"),
        canonical_event_id_unchecked(&perpetual_metadata, &payload).expect("perpetual identity")
    );
}

#[test]
fn local_processing_lineage_does_not_change_source_event_identity() {
    let (metadata, payload) = fixture();
    let baseline = canonical_event_id_unchecked(&metadata, &payload).expect("baseline identity");
    let mut mutations = Vec::new();

    macro_rules! mutation {
        ($field:ident, $value:expr) => {{
            let mut changed = metadata.clone();
            changed.$field = $value;
            mutations.push((stringify!($field), changed));
        }};
    }

    mutation!(receive_wall_timestamp, UnixNanos::new(1_101));
    mutation!(receive_monotonic_ns, 101);
    mutation!(normalization_timestamp, UnixNanos::new(1_102));
    mutation!(connection_started_at, UnixNanos::new(899));
    mutation!(connection_epoch, 2);
    mutation!(subscription_epoch, 2);
    mutation!(snapshot_kind, SnapshotKind::Delta);
    mutation!(source_checksum, Some("other-checksum".to_owned()));
    mutation!(raw_payload_hash, [8; 32]);
    mutation!(parser_version, "parser-v2".to_owned());
    mutation!(normalizer_version, "normalizer-v2".to_owned());
    mutation!(ingestion_instance, "ingestion-2".to_owned());
    mutation!(quality_score_ppm, 999_999);
    mutation!(quality_flags, QualityFlags::STALE);

    for (field, changed) in mutations {
        assert_eq!(
            canonical_event_id_unchecked(&changed, &payload).expect("mutated identity"),
            baseline,
            "lineage-only metadata field {field} must not fragment source-event identity"
        );
    }
}

#[test]
fn every_book_snapshot_payload_field_changes_event_identity() {
    let (metadata, payload) = fixture();
    let baseline = canonical_event_id_unchecked(&metadata, &payload).expect("baseline identity");
    let UncheckedEventPayload::BookSnapshot(snapshot) = payload else {
        panic!("fixture payload must be a book snapshot");
    };
    let mut mutations = Vec::new();

    let mut changed = snapshot.clone();
    changed.bids[0].price = price("60000");
    mutations.push(("bid price", changed));
    let mut changed = snapshot.clone();
    changed.bids[0].quantity = quantity("3");
    mutations.push(("bid quantity", changed));
    let mut changed = snapshot.clone();
    changed.bids[0].order_count = Some(4);
    mutations.push(("bid order count", changed));
    let mut changed = snapshot.clone();
    changed.asks[0].price = price("60000.3");
    mutations.push(("ask price", changed));
    let mut changed = snapshot.clone();
    changed.asks[0].quantity = quantity("5");
    mutations.push(("ask quantity", changed));
    let mut changed = snapshot.clone();
    changed.asks[0].order_count = Some(6);
    mutations.push(("ask order count", changed));
    let mut changed = snapshot;
    changed.last_sequence = 101;
    mutations.push(("last sequence", changed));

    for (field, changed) in mutations {
        let payload = UncheckedEventPayload::BookSnapshot(changed);
        assert_ne!(
            canonical_event_id_unchecked(&metadata, &payload).expect("mutated identity"),
            baseline,
            "payload field {field} was omitted from event identity"
        );
    }
}

#[test]
fn every_trade_payload_field_changes_event_identity() {
    let (mut metadata, _) = fixture();
    metadata.snapshot_kind = SnapshotKind::NotApplicable;
    metadata.sequence_number = None;
    let trade = Trade {
        trade_id: "trade-1".to_owned(),
        price: price("60000.1"),
        quantity: quantity("2"),
        side: Side::Buy,
    };
    let baseline_payload = UncheckedEventPayload::Trade(trade.clone());
    let baseline =
        canonical_event_id_unchecked(&metadata, &baseline_payload).expect("baseline identity");
    let mut mutations = Vec::new();

    let mut changed = trade.clone();
    changed.trade_id = "trade-2".to_owned();
    mutations.push(("trade id", changed));
    let mut changed = trade.clone();
    changed.price = price("60000.2");
    mutations.push(("price", changed));
    let mut changed = trade.clone();
    changed.quantity = quantity("3");
    mutations.push(("quantity", changed));
    let mut changed = trade;
    changed.side = Side::Sell;
    mutations.push(("side", changed));

    for (field, changed) in mutations {
        assert_ne!(
            canonical_event_id_unchecked(&metadata, &UncheckedEventPayload::Trade(changed))
                .expect("mutated identity"),
            baseline,
            "trade field {field} was omitted from event identity"
        );
    }
}

#[test]
fn every_book_delta_payload_field_changes_event_identity() {
    let (mut metadata, _) = fixture();
    metadata.snapshot_kind = SnapshotKind::Delta;
    metadata.sequence_number = Some(101);
    let delta = BookDelta {
        bids: vec![BookLevel {
            price: price("60000.1"),
            quantity: quantity("2"),
            order_count: Some(3),
        }],
        asks: vec![BookLevel {
            price: price("60000.2"),
            quantity: quantity("4"),
            order_count: Some(5),
        }],
        first_sequence: 100,
        last_sequence: 101,
    };
    let baseline_payload = UncheckedEventPayload::BookDelta(delta.clone());
    let baseline =
        canonical_event_id_unchecked(&metadata, &baseline_payload).expect("baseline identity");
    let mut mutations = Vec::new();

    let mut changed = delta.clone();
    changed.bids[0].price = price("60000");
    mutations.push(("bid price", changed));
    let mut changed = delta.clone();
    changed.bids[0].quantity = quantity("3");
    mutations.push(("bid quantity", changed));
    let mut changed = delta.clone();
    changed.bids[0].order_count = Some(4);
    mutations.push(("bid order count", changed));
    let mut changed = delta.clone();
    changed.asks[0].price = price("60000.3");
    mutations.push(("ask price", changed));
    let mut changed = delta.clone();
    changed.asks[0].quantity = quantity("5");
    mutations.push(("ask quantity", changed));
    let mut changed = delta.clone();
    changed.asks[0].order_count = Some(6);
    mutations.push(("ask order count", changed));
    let mut changed = delta.clone();
    changed.first_sequence = 99;
    mutations.push(("first sequence", changed));
    let mut changed = delta;
    changed.last_sequence = 102;
    mutations.push(("last sequence", changed));

    for (field, changed) in mutations {
        assert_ne!(
            canonical_event_id_unchecked(&metadata, &UncheckedEventPayload::BookDelta(changed))
                .expect("mutated identity"),
            baseline,
            "delta field {field} was omitted from event identity"
        );
    }
}

#[test]
fn option_vector_and_variant_framing_are_identity_significant() {
    let (metadata, payload) = fixture();

    let level = BookLevel {
        price: price("60000.1"),
        quantity: quantity("2"),
        order_count: None,
    };
    let mut counted_level = level.clone();
    counted_level.order_count = Some(0);
    let bids_none = UncheckedEventPayload::BookSnapshot(BookSnapshot {
        bids: vec![level.clone()],
        asks: Vec::new(),
        last_sequence: 100,
    });
    let bids_some = UncheckedEventPayload::BookSnapshot(BookSnapshot {
        bids: vec![counted_level],
        asks: Vec::new(),
        last_sequence: 100,
    });
    assert_ne!(
        canonical_event_id_unchecked(&metadata, &bids_none).expect("none order count"),
        canonical_event_id_unchecked(&metadata, &bids_some).expect("zero order count")
    );

    let asks_only = UncheckedEventPayload::BookSnapshot(BookSnapshot {
        bids: Vec::new(),
        asks: vec![level],
        last_sequence: 100,
    });
    assert_ne!(
        canonical_event_id_unchecked(&metadata, &bids_none).expect("bids framing"),
        canonical_event_id_unchecked(&metadata, &asks_only).expect("asks framing")
    );

    let trade = UncheckedEventPayload::Trade(Trade {
        trade_id: "same-shape-check".to_owned(),
        price: price("60000.1"),
        quantity: quantity("2"),
        side: Side::Buy,
    });
    let delta = UncheckedEventPayload::BookDelta(BookDelta {
        bids: Vec::new(),
        asks: Vec::new(),
        first_sequence: 0,
        last_sequence: 0,
    });
    let ids = [
        canonical_event_id_unchecked(&metadata, &trade).expect("trade discriminator"),
        canonical_event_id_unchecked(&metadata, &payload).expect("snapshot discriminator"),
        canonical_event_id_unchecked(&metadata, &delta).expect("delta discriminator"),
    ];
    assert!(ids[0] != ids[1] && ids[1] != ids[2] && ids[0] != ids[2]);
}

#[test]
fn deserialization_revalidates_components_and_identity() {
    let (metadata, payload) = fixture();
    let envelope = EventEnvelope::new(metadata, payload).expect("valid envelope");
    let encoded = serde_json::to_value(&envelope).expect("serialize envelope");
    let decoded: EventEnvelope =
        serde_json::from_value(encoded.clone()).expect("valid wire envelope");
    assert_eq!(decoded, envelope);

    let mut tampered_type = encoded.clone();
    tampered_type["event_type"] = serde_json::json!("trade");
    assert!(
        serde_json::from_value::<EventEnvelope>(tampered_type)
            .expect_err("tampered event type must fail")
            .to_string()
            .contains("event type")
    );

    let mut tampered_id = encoded.clone();
    let original_id = tampered_id["event_id"]
        .as_str()
        .expect("event id must be hex");
    let replacement = if original_id.starts_with('0') {
        "1"
    } else {
        "0"
    };
    tampered_id["event_id"] = serde_json::json!(format!("{replacement}{}", &original_id[1..]));
    assert!(
        serde_json::from_value::<EventEnvelope>(tampered_id)
            .expect_err("tampered identity must fail")
            .to_string()
            .contains("event identity")
    );

    let mut invalid_metadata = encoded;
    invalid_metadata["connection_epoch"] = serde_json::json!(0);
    assert!(
        serde_json::from_value::<EventEnvelope>(invalid_metadata)
            .expect_err("invalid metadata must fail")
            .to_string()
            .contains("connection_epoch")
    );
}

#[test]
fn independent_known_vector_freezes_the_canonical_contract() {
    let (metadata, payload) = fixture();
    let production =
        canonical_event_id_unchecked(&metadata, &payload).expect("production identity");
    let independent = independent_fixture_identity(&metadata, &payload);

    assert_eq!(
        hex::encode(independent),
        "ef2b8d555e27846bbd44cd484ff47856f8568d68b531e21abf52e74846579915"
    );
    assert_eq!(production.as_bytes(), &independent);

    let mut product_aware_metadata = metadata.clone();
    product_aware_metadata.schema_version = 3;
    let product_aware_production = canonical_event_id_unchecked(&product_aware_metadata, &payload)
        .expect("product-aware identity");
    let product_aware_independent = independent_fixture_identity(&product_aware_metadata, &payload);
    assert_eq!(
        hex::encode(product_aware_independent),
        "2fbc4ab900c204f32c8ed326ed2c78ddf3db84dfe7ff4d98310344645efd19db"
    );
    assert_eq!(
        product_aware_production.as_bytes(),
        &product_aware_independent
    );

    let v2_envelope = EventEnvelope::new(metadata.clone(), payload.clone())
        .expect("schema 2 envelope remains supported");
    let v2_wire = serde_json::to_value(&v2_envelope).expect("serialize schema 2 envelope");
    assert!(
        v2_wire["instrument_id"].get("product_type").is_none(),
        "legacy Spot wire identity must not gain a product_type field"
    );
    let decoded_v2: EventEnvelope =
        serde_json::from_value(v2_wire).expect("deserialize legacy v2 Spot envelope");
    assert_eq!(decoded_v2.id(), v2_envelope.id());

    let mut legacy_metadata = metadata;
    legacy_metadata.schema_version = 1;
    let legacy_production =
        canonical_event_id_unchecked(&legacy_metadata, &payload).expect("legacy identity");
    let legacy_independent = independent_fixture_identity(&legacy_metadata, &payload);
    assert_eq!(
        hex::encode(legacy_independent),
        "4d37e6c1c70bcb78f1208ff6aa90d08de4968d7b23acae15ad887234e6e9a3b9"
    );
    assert_eq!(legacy_production.as_bytes(), &legacy_independent);

    let legacy_envelope =
        EventEnvelope::new(legacy_metadata, payload).expect("legacy envelope remains supported");
    let wire = serde_json::to_value(&legacy_envelope).expect("serialize legacy envelope");
    assert!(
        wire["instrument_id"].get("product_type").is_none(),
        "legacy Spot wire identity must not gain a product_type field"
    );
    let decoded: EventEnvelope =
        serde_json::from_value(wire).expect("deserialize legacy v1 envelope");
    assert_eq!(decoded.id(), legacy_envelope.id());
}

fn independent_fixture_identity(
    metadata: &UncheckedEventMetadata,
    payload: &UncheckedEventPayload,
) -> [u8; 32] {
    fn u32_field(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn u64_field(bytes: &mut Vec<u8>, value: u64) {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn i64_field(bytes: &mut Vec<u8>, value: i64) {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    fn string(bytes: &mut Vec<u8>, value: &str) {
        u32_field(
            bytes,
            u32::try_from(value.len()).expect("fixture string fits"),
        );
        bytes.extend_from_slice(value.as_bytes());
    }
    fn decimal(bytes: &mut Vec<u8>, value: FixedDecimal) {
        string(bytes, &value.to_string());
    }
    fn level(bytes: &mut Vec<u8>, value: &BookLevel) {
        decimal(bytes, value.price.value());
        decimal(bytes, value.quantity.value());
        match value.order_count {
            Some(count) => {
                bytes.push(1);
                u32_field(bytes, count);
            }
            None => bytes.push(0),
        }
    }

    let mut bytes = Vec::new();
    bytes.push(metadata.source.kind() as u8);
    string(&mut bytes, metadata.source.name());
    u32_field(&mut bytes, metadata.source.generation());
    bytes.push(1);
    string(
        &mut bytes,
        metadata.venue.as_ref().expect("fixture venue").as_str(),
    );
    bytes.push(1);
    let instrument = metadata.instrument_id.as_ref().expect("fixture instrument");
    string(&mut bytes, instrument.venue().as_str());
    string(&mut bytes, instrument.venue_symbol());
    if metadata.schema_version >= 3 {
        bytes.push(instrument.product_type() as u8);
    }
    u32_field(&mut bytes, instrument.generation());
    bytes.push(1);
    i64_field(
        &mut bytes,
        metadata
            .exchange_timestamp
            .expect("fixture exchange timestamp")
            .value(),
    );
    bytes.push(1);
    i64_field(
        &mut bytes,
        metadata
            .exchange_transaction_timestamp
            .expect("fixture exchange transaction timestamp")
            .value(),
    );
    bytes.push(1);
    u64_field(
        &mut bytes,
        metadata.sequence_number.expect("fixture sequence"),
    );
    if metadata.schema_version >= 2 {
        bytes.push(1);
        u64_field(
            &mut bytes,
            metadata
                .previous_sequence_number
                .expect("fixture previous sequence"),
        );
    }

    let UncheckedEventPayload::BookSnapshot(snapshot) = payload else {
        panic!("known vector uses snapshot");
    };
    bytes.push(2);
    u32_field(
        &mut bytes,
        u32::try_from(snapshot.bids.len()).expect("bid count"),
    );
    for value in &snapshot.bids {
        level(&mut bytes, value);
    }
    u32_field(
        &mut bytes,
        u32::try_from(snapshot.asks.len()).expect("ask count"),
    );
    for value in &snapshot.asks {
        level(&mut bytes, value);
    }
    u64_field(&mut bytes, snapshot.last_sequence);

    let mut hasher = blake3::Hasher::new();
    hasher.update(match metadata.schema_version {
        1 => b"cmti:event:v1\0",
        2 => b"cmti:event:v2\0",
        3 => b"cmti:event:v3\0",
        _ => panic!("known vector schema is supported"),
    });
    hasher.update(&bytes);
    *hasher.finalize().as_bytes()
}
