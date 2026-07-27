use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
use event_envelope::{
    BookLevel, BookSnapshot, EventEnvelope, EventMetadata, EventPayload, QualityFlags,
    SnapshotKind, canonical_event_id,
};
use fixed_decimal::{Price, Quantity};

fn price(value: &str) -> Price {
    Price::new(fixed_decimal::FixedDecimal::parse_canonical(value).expect("canonical price"))
        .expect("positive price")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(fixed_decimal::FixedDecimal::parse_canonical(value).expect("canonical quantity"))
        .expect("non-negative quantity")
}

fn fixture() -> (EventMetadata, EventPayload) {
    let venue = VenueId::new("binance").expect("venue");
    let instrument = InstrumentId::new(venue.clone(), "BTCUSDT", 1).expect("instrument identity");
    let metadata = EventMetadata {
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
    let payload = EventPayload::BookSnapshot(BookSnapshot {
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
    let first = canonical_event_id(&metadata, &payload).expect("identity");
    let second = canonical_event_id(&metadata, &payload).expect("identity");
    assert_eq!(first, second);

    let envelope = EventEnvelope::new(metadata, payload).expect("valid envelope");
    assert_eq!(envelope.id(), first);
    envelope.verify().expect("identity verifies");
}

#[test]
fn every_metadata_field_changes_event_identity() {
    let (metadata, payload) = fixture();
    let baseline = canonical_event_id(&metadata, &payload).expect("baseline identity");
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
            canonical_event_id(&changed, &payload).expect("mutated identity"),
            baseline,
            "metadata field {field} was omitted from event identity"
        );
    }
}

#[test]
fn every_book_snapshot_payload_field_changes_event_identity() {
    let (metadata, payload) = fixture();
    let baseline = canonical_event_id(&metadata, &payload).expect("baseline identity");
    let EventPayload::BookSnapshot(snapshot) = payload else {
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
        let payload = EventPayload::BookSnapshot(changed);
        assert_ne!(
            canonical_event_id(&metadata, &payload).expect("mutated identity"),
            baseline,
            "payload field {field} was omitted from event identity"
        );
    }
}
