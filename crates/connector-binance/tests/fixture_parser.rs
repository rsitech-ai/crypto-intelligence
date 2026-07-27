use connector_binance::{
    EXPECTED_GENERATION, EXPECTED_SOURCE, EXPECTED_SYMBOL, ParseError, parse_fixture_line,
};
use event_envelope::UncheckedEventPayload;

const FIXTURE: &str = include_str!("../../../fixtures/binance/btcusdt-book-v1.jsonl");

#[test]
fn fixture_normalizes_to_one_snapshot_and_two_contiguous_deltas() {
    let events = FIXTURE
        .lines()
        .map(|line| parse_fixture_line(line.as_bytes()).expect("frozen fixture line must parse"))
        .collect::<Vec<_>>();

    assert_eq!(events.len(), 3);
    for event in &events {
        let metadata = event.metadata().as_unchecked();
        assert_eq!(metadata.source.name(), EXPECTED_SOURCE);
        assert_eq!(
            metadata
                .instrument_id
                .as_ref()
                .expect("book events require an instrument")
                .venue_symbol(),
            EXPECTED_SYMBOL
        );
        assert_eq!(metadata.source.generation(), EXPECTED_GENERATION);
        assert_eq!(
            metadata
                .instrument_id
                .as_ref()
                .expect("book events require an instrument")
                .generation(),
            EXPECTED_GENERATION
        );
        event.verify().expect("normalized identity must verify");
    }

    let snapshot = match events[0].payload().as_unchecked() {
        UncheckedEventPayload::BookSnapshot(snapshot) => snapshot,
        payload => panic!("first fixture event must be a snapshot, got {payload:?}"),
    };
    assert_eq!(snapshot.last_sequence, 100);

    let first_delta = match events[1].payload().as_unchecked() {
        UncheckedEventPayload::BookDelta(delta) => delta,
        payload => panic!("second fixture event must be a delta, got {payload:?}"),
    };
    assert_eq!(
        (first_delta.first_sequence, first_delta.last_sequence),
        (101, 101)
    );

    let final_delta = match events[2].payload().as_unchecked() {
        UncheckedEventPayload::BookDelta(delta) => delta,
        payload => panic!("third fixture event must be a delta, got {payload:?}"),
    };
    assert_eq!(
        (final_delta.first_sequence, final_delta.last_sequence),
        (102, 102)
    );
    assert_eq!(final_delta.asks[0].price.to_string(), "60000.2");
    assert_eq!(final_delta.asks[0].price.value().mantissa(), 600_002);
    assert_eq!(final_delta.asks[0].price.value().scale(), 1);
}

#[test]
fn malformed_json_and_unknown_fields_fail_closed() {
    assert!(matches!(
        parse_fixture_line(br#"{"kind":"snapshot""#),
        Err(ParseError::MalformedJson)
    ));
    assert!(matches!(
        parse_fixture_line(
            br#"{"kind":"snapshot","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"sequence":100,"bids":[["1","1"]],"asks":[["2","1"]],"unexpected":true}"#
        ),
        Err(ParseError::MalformedJson)
    ));
}

#[test]
fn zero_or_negative_values_fail_closed() {
    let invalid_lines: [&[u8]; 4] = [
        br#"{"kind":"snapshot","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"sequence":100,"bids":[["0","1"]],"asks":[["2","1"]]}"#,
        br#"{"kind":"snapshot","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"sequence":100,"bids":[["1","0"]],"asks":[["2","1"]]}"#,
        br#"{"kind":"delta","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"first_sequence":101,"last_sequence":101,"bids":[["-1","1"]],"asks":[]}"#,
        br#"{"kind":"delta","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"first_sequence":101,"last_sequence":101,"bids":[["1","-1"]],"asks":[]}"#,
    ];

    for line in invalid_lines {
        assert!(
            parse_fixture_line(line).is_err(),
            "invalid monetary input must fail closed"
        );
    }
}

#[test]
fn wrong_fixture_identity_or_generation_fails_closed() {
    let invalid_lines: [&[u8]; 4] = [
        br#"{"kind":"snapshot","source":"binance","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"sequence":100,"bids":[["1","1"]],"asks":[["2","1"]]}"#,
        br#"{"kind":"snapshot","source":"binance-fixture","symbol":"ETHUSDT","generation":1,"event_unix_nanos":1,"sequence":100,"bids":[["1","1"]],"asks":[["2","1"]]}"#,
        br#"{"kind":"snapshot","source":"binance-fixture","symbol":"BTCUSDT","generation":0,"event_unix_nanos":1,"sequence":100,"bids":[["1","1"]],"asks":[["2","1"]]}"#,
        br#"{"kind":"snapshot","source":"binance-fixture","symbol":"BTCUSDT","generation":2,"event_unix_nanos":1,"sequence":100,"bids":[["1","1"]],"asks":[["2","1"]]}"#,
    ];

    for line in invalid_lines {
        assert!(
            matches!(parse_fixture_line(line), Err(ParseError::Identity)),
            "wrong fixture identity must fail closed"
        );
    }
}

#[test]
fn invalid_or_noncanonical_sequences_and_decimals_fail_closed() {
    let invalid_lines: [&[u8]; 4] = [
        br#"{"kind":"snapshot","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"sequence":0,"bids":[["1","1"]],"asks":[["2","1"]]}"#,
        br#"{"kind":"delta","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"first_sequence":102,"last_sequence":101,"bids":[["1","1"]],"asks":[]}"#,
        br#"{"kind":"snapshot","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"sequence":100,"bids":[["01","1"]],"asks":[["2","1"]]}"#,
        br#"{"kind":"snapshot","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1,"sequence":100,"bids":[["1","1.0"]],"asks":[["2","1"]]}"#,
    ];

    for line in invalid_lines {
        assert!(
            parse_fixture_line(line).is_err(),
            "invalid sequence or noncanonical decimal must fail closed"
        );
    }
}
