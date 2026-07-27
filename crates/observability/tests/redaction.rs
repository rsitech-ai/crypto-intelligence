use std::fs::{self, OpenOptions};

use observability::{
    Component, LocalJsonLog, MetricKey, MetricName, Metrics, Outcome, REDACTED, Redactor, Venue,
};
use serde_json::json;

#[test]
fn nested_sensitive_keys_and_secret_shaped_values_are_redacted() {
    let redactor = Redactor;
    let sanitized = redactor.json(&json!({
        "event": "fixture_ready",
        "token": "not-for-output",
        "nested": {
            "authorization": "Bearer abc",
            "message": "Bearer still-sensitive"
        },
        "items": [
            {"api_key": "also-secret"},
            "safe"
        ]
    }));

    assert_eq!(sanitized["event"], "fixture_ready");
    assert_eq!(sanitized["token"], REDACTED);
    assert_eq!(sanitized["nested"]["authorization"], REDACTED);
    assert_eq!(sanitized["nested"]["message"], REDACTED);
    assert_eq!(sanitized["items"][0]["api_key"], REDACTED);
    assert_eq!(sanitized["items"][1], "safe");
    let encoded = serde_json::to_string(&sanitized).expect("sanitized JSON must serialize");
    for forbidden in ["not-for-output", "Bearer abc", "also-secret"] {
        assert!(!encoded.contains(forbidden));
    }
}

#[test]
fn bounded_metrics_saturate_and_preserve_typed_dimensions() {
    let metrics = Metrics::default();
    let key = MetricKey {
        name: MetricName::EventsRejected,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Degraded,
    };

    metrics
        .increment(key, u64::MAX)
        .expect("first bounded increment must work");
    metrics
        .increment(key, 1)
        .expect("overflowing increment must saturate");

    assert_eq!(
        metrics
            .counter(key)
            .expect("typed counter must be readable"),
        u64::MAX
    );
}

#[test]
fn local_json_log_flushes_without_drops_or_sensitive_values() {
    let directory = tempfile::tempdir().expect("temporary log root must exist");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(directory.path().join("cmti.jsonl"))
        .expect("validated log file must open");
    let logger = LocalJsonLog::from_file(file);
    logger
        .write_event(&json!({
            "event": "fixture_runtime_ready",
            "token": "fixture-token-must-not-leak",
            "sequence": 102
        }))
        .expect("local event must write");
    logger.shutdown().expect("local log must sync");

    let entries = fs::read_dir(directory.path())
        .expect("log directory must read")
        .map(|entry| entry.expect("log entry must read").path())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1);
    let contents = fs::read_to_string(&entries[0]).expect("JSON log must read");
    assert!(contents.contains("fixture_runtime_ready"));
    assert!(contents.contains(REDACTED));
    assert!(!contents.contains("fixture-token-must-not-leak"));
    for line in contents.lines() {
        serde_json::from_str::<serde_json::Value>(line).expect("every log line must be JSON");
    }
}
