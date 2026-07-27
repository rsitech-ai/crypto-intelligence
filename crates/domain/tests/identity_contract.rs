use domain::{DomainError, InstrumentId, SourceId, SourceKind, VenueId};

#[test]
fn generation_zero_is_rejected() {
    let venue = VenueId::new("binance").expect("venue is valid");

    assert_eq!(
        SourceId::new(SourceKind::Exchange, "binance-fixture", 0),
        Err(DomainError::GenerationZero)
    );
    assert_eq!(
        InstrumentId::new(venue, "BTCUSDT", 0),
        Err(DomainError::GenerationZero)
    );
}

#[test]
fn identities_normalize_ascii_case_before_equality_and_serialization() {
    let mixed_venue = VenueId::new("Binance").expect("mixed-case venue is accepted");
    let canonical_venue = VenueId::new("binance").expect("canonical venue is accepted");
    assert_eq!(mixed_venue, canonical_venue);
    assert_eq!(mixed_venue.as_str(), "binance");

    let mixed_source =
        SourceId::new(SourceKind::Exchange, "Binance-Fixture", 1).expect("source is valid");
    let canonical_source =
        SourceId::new(SourceKind::Exchange, "binance-fixture", 1).expect("source is valid");
    assert_eq!(mixed_source, canonical_source);
    assert_eq!(mixed_source.name(), "binance-fixture");

    let mixed_instrument =
        InstrumentId::new(mixed_venue, "btcusdt", 1).expect("instrument is valid");
    let canonical_instrument =
        InstrumentId::new(canonical_venue, "BTCUSDT", 1).expect("instrument is valid");
    assert_eq!(mixed_instrument, canonical_instrument);
    assert_eq!(mixed_instrument.venue_symbol(), "BTCUSDT");

    let encoded = serde_json::to_string(&mixed_instrument).expect("identity serializes");
    assert!(encoded.contains("binance"));
    assert!(encoded.contains("BTCUSDT"));
}

#[test]
fn generation_is_part_of_source_and_instrument_identity() {
    let venue = VenueId::new("binance").expect("venue is valid");
    assert_ne!(
        SourceId::new(SourceKind::Exchange, "binance-fixture", 1).expect("source is valid"),
        SourceId::new(SourceKind::Exchange, "binance-fixture", 2).expect("source is valid")
    );
    assert_ne!(
        InstrumentId::new(venue.clone(), "BTCUSDT", 1).expect("instrument is valid"),
        InstrumentId::new(venue, "BTCUSDT", 2).expect("instrument is valid")
    );
}

#[test]
fn identities_reject_whitespace_and_punctuation_outside_the_contract() {
    assert!(VenueId::new(" binance").is_err());
    assert!(VenueId::new("binance/spot").is_err());
    assert!(SourceId::new(SourceKind::Exchange, "binance fixture", 1).is_err());

    let venue = VenueId::new("binance").expect("venue is valid");
    assert!(InstrumentId::new(venue, "BTC/USDT", 1).is_err());
}
