use std::path::PathBuf;

use replay_engine::{ReplayConfig, ReplayRunner};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden-replays/market-foundation/manifest.toml")
}

fn stop_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden-replays/market-foundation/stop-at-20.toml")
}

#[test]
fn repeated_replay_produces_identical_books_events_and_incidents() {
    let config = ReplayConfig::from_manifest(manifest()).expect("valid golden replay manifest");
    let first = ReplayRunner::run_to_digest(&config).expect("first replay");
    let second = ReplayRunner::run_to_digest(&config).expect("second replay");

    assert_eq!(first, second);
    assert_eq!(first.silent_integrity_failures(), 0);
    assert!(
        first.matches_expected(config.expected()),
        "unexpected digest: events={} book={} quality={} records={} events_count={} incidents={} silent_failures={} elapsed_ns={}",
        first.normalized_event_hash_hex(),
        first.book_state_hash_hex(),
        first.quality_incident_hash_hex(),
        first.replayed_records(),
        first.normalized_events(),
        first.quality_incidents(),
        first.silent_integrity_failures(),
        first.logical_elapsed_ns(),
    );
}

#[test]
fn inclusive_stop_boundary_is_repeatable_and_excludes_later_records() {
    let config = ReplayConfig::from_manifest(stop_manifest()).expect("valid stop manifest");
    let first = ReplayRunner::run_to_digest(&config).expect("first stopped replay");
    let second = ReplayRunner::run_to_digest(&config).expect("second stopped replay");

    assert_eq!(first, second);
    assert_eq!(first.replayed_records(), 2);
    assert_eq!(first.normalized_events(), 2);
    assert_eq!(first.quality_incidents(), 0);
    assert_eq!(first.silent_integrity_failures(), 0);
    assert_eq!(first.logical_elapsed_ns(), 10);
    assert!(
        first.matches_expected(config.expected()),
        "unexpected stopped digest: events={} book={} quality={}",
        first.normalized_event_hash_hex(),
        first.book_state_hash_hex(),
        first.quality_incident_hash_hex(),
    );
}
