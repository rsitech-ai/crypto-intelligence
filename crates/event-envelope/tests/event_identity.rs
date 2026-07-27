use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
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
        schema_version: 1,
        source: SourceId::new(SourceKind::Exchange, "binance-fixture", 1).expect("source"),
        venue: Some(venue),
        instrument_id: Some(instrument),
        exchange_timestamp: Some(UnixNanos::new(1_000)),
        receive_wall_timestamp: UnixNanos::new(1_100),
        receive_monotonic_ns: 100,
        connection_started_at: UnixNanos::new(900),
        sequence_number: Some(100),
        connection_epoch: 1,
        snapshot_kind: SnapshotKind::Snapshot,
        source_checksum: Some("checksum".to_owned()),
        raw_payload_hash: [7; 32],
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
fn every_metadata_field_changes_event_identity() {
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

    mutation!(schema_version, 2);
    mutation!(
        source,
        SourceId::new(SourceKind::Exchange, "binance-fixture", 2).expect("source")
    );
    mutation!(venue, Some(VenueId::new("kraken").expect("venue")));
    mutation!(
        instrument_id,
        Some(
            InstrumentId::new(VenueId::new("binance").expect("venue"), "BTCUSDT", 2)
                .expect("instrument")
        )
    );
    mutation!(exchange_timestamp, Some(UnixNanos::new(1_001)));
    mutation!(receive_wall_timestamp, UnixNanos::new(1_101));
    mutation!(receive_monotonic_ns, 101);
    mutation!(connection_started_at, UnixNanos::new(899));
    mutation!(sequence_number, Some(101));
    mutation!(connection_epoch, 2);
    mutation!(snapshot_kind, SnapshotKind::Delta);
    mutation!(source_checksum, Some("other-checksum".to_owned()));
    mutation!(raw_payload_hash, [8; 32]);
    mutation!(normalizer_version, "normalizer-v2".to_owned());
    mutation!(ingestion_instance, "ingestion-2".to_owned());
    mutation!(quality_score_ppm, 999_999);
    mutation!(quality_flags, QualityFlags::STALE);

    for (field, changed) in mutations {
        assert_ne!(
            canonical_event_id_unchecked(&changed, &payload).expect("mutated identity"),
            baseline,
            "metadata field {field} was omitted from event identity"
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

    let mut no_checksum = metadata.clone();
    no_checksum.source_checksum = None;
    let mut empty_checksum = metadata.clone();
    empty_checksum.source_checksum = Some(String::new());
    assert_ne!(
        canonical_event_id_unchecked(&no_checksum, &payload).expect("none checksum"),
        canonical_event_id_unchecked(&empty_checksum, &payload).expect("empty checksum")
    );

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

    let mut tampered_id = encoded.clone();
    tampered_id["id"][0] = serde_json::json!(255);
    assert!(
        serde_json::from_value::<EventEnvelope>(tampered_id)
            .expect_err("tampered identity must fail")
            .to_string()
            .contains("event identity")
    );

    let mut invalid_metadata = encoded;
    invalid_metadata["metadata"]["connection_epoch"] = serde_json::json!(0);
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
        "03ee8d375476c366d19c5282f802bc3f11ffca0d3ae814f1fe04380afa796c51"
    );
    assert_eq!(production.as_bytes(), &independent);
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
    u32_field(&mut bytes, metadata.schema_version);
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
    u32_field(&mut bytes, instrument.generation());
    bytes.push(1);
    i64_field(
        &mut bytes,
        metadata
            .exchange_timestamp
            .expect("fixture exchange timestamp")
            .value(),
    );
    i64_field(&mut bytes, metadata.receive_wall_timestamp.value());
    u64_field(&mut bytes, metadata.receive_monotonic_ns);
    i64_field(&mut bytes, metadata.connection_started_at.value());
    bytes.push(1);
    u64_field(
        &mut bytes,
        metadata.sequence_number.expect("fixture sequence"),
    );
    u64_field(&mut bytes, metadata.connection_epoch);
    bytes.push(metadata.snapshot_kind as u8);
    bytes.push(1);
    string(
        &mut bytes,
        metadata
            .source_checksum
            .as_deref()
            .expect("fixture checksum"),
    );
    u32_field(
        &mut bytes,
        u32::try_from(metadata.raw_payload_hash.len()).expect("hash length"),
    );
    bytes.extend_from_slice(&metadata.raw_payload_hash);
    string(&mut bytes, &metadata.normalizer_version);
    string(&mut bytes, &metadata.ingestion_instance);
    u32_field(&mut bytes, metadata.quality_score_ppm);
    u64_field(&mut bytes, metadata.quality_flags.bits());

    let UncheckedEventPayload::BookSnapshot(snapshot) = payload else {
        panic!("known vector uses snapshot");
    };
    bytes.push(1);
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
    hasher.update(b"cmti:event:v1\0");
    hasher.update(&bytes);
    *hasher.finalize().as_bytes()
}
