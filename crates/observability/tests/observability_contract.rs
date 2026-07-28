use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::PermissionsExt,
};

use observability::{
    Component, FieldName, LocalJsonLog, LocalLogLevel, LogRotationPolicy, MetricKey, MetricKind,
    MetricName, MetricUnit, Metrics, ObservabilityError, Outcome, RawPayload, SecretValue,
    SloObservation, Venue, init_local_tracing, init_test_observability,
};

#[test]
fn stable_metric_and_field_catalogs_are_complete_and_unique() {
    let metric_names = MetricName::ALL
        .iter()
        .map(|metric| metric.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(metric_names.len(), MetricName::ALL.len());
    for required in [
        "transition_ingestion_events_total",
        "transition_ingestion_bytes_total",
        "transition_parser_failures_total",
        "transition_sequence_gaps_total",
        "transition_checksum_failures_total",
        "transition_queue_depth",
        "transition_dropped_events_total",
        "transition_wal_fsync_seconds",
        "transition_event_to_normalized_seconds",
        "transition_fast_feature_to_forecast_seconds",
        "transition_model_inference_seconds",
        "transition_predictions_total",
        "transition_abstention_total",
        "transition_rpc_seconds",
        "transition_memory_resident_bytes",
        "transition_file_descriptor_count",
        "transition_thread_count",
    ] {
        assert!(metric_names.contains(required), "missing metric {required}");
    }
    assert_eq!(MetricName::QueueDepth.kind(), MetricKind::Gauge);
    assert_eq!(MetricName::WalFsyncLatency.kind(), MetricKind::Histogram);
    assert_eq!(MetricName::WalFsyncLatency.unit(), MetricUnit::Seconds);
    assert_eq!(
        Component::ALL
            .iter()
            .map(|component| component.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        Component::ALL.len()
    );
    assert_eq!(
        Venue::ALL
            .iter()
            .map(|venue| venue.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        Venue::ALL.len()
    );
    assert_eq!(
        Outcome::ALL
            .iter()
            .map(|outcome| outcome.as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        Outcome::ALL.len()
    );

    let field_names = FieldName::ALL
        .iter()
        .map(|field| field.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(field_names.len(), FieldName::ALL.len());
    for required in [
        "level",
        "event",
        "component",
        "connection_id",
        "event_id",
        "forecast_id",
        "replay_id",
        "incident_id",
    ] {
        assert!(field_names.contains(required), "missing field {required}");
    }
}

#[test]
fn sensitive_and_raw_values_are_non_rendering_by_construction() {
    let secret = SecretValue::new("secret-sentinel-never-log");
    let payload = RawPayload::new(br#"{"headline":"raw-news-sentinel"}"#);

    let rendered = format!("{secret} {secret:?} {payload} {payload:?}");
    assert!(!rendered.contains("secret-sentinel-never-log"));
    assert!(!rendered.contains("raw-news-sentinel"));
    assert!(rendered.contains("[REDACTED]"));
    assert!(rendered.contains("[RAW_PAYLOAD_REDACTED]"));

    let serialized =
        serde_json::to_string(&(secret, payload)).expect("redacted wrappers must serialize");
    assert!(!serialized.contains("secret-sentinel-never-log"));
    assert!(!serialized.contains("raw-news-sentinel"));
}

#[test]
fn structured_tracing_redacts_fields_and_preserves_correlation_spans() {
    let capture = init_test_observability();
    capture.with_default(|| {
        let span = tracing::info_span!("connector_session", connection_id = "connection-7");
        let _entered = span.enter();
        tracing::info!(
            event = "connector_failure",
            incident_id = "incident-7",
            session_secret = "secret-tracing-sentinel",
            raw_payload = r#"{"headline":"raw-tracing-sentinel"}"#,
            details = "session_secret=second-tracing-sentinel",
            message = r#"{"kind":"snapshot","raw":"ordinary-raw-body"}"#,
            outcome = %SecretValue::new("display-secret-sentinel"),
            numeric_secret = 918273645_u64
        );
    });
    let output = capture.finish().expect("captured tracing must render");

    for forbidden in [
        "secret-tracing-sentinel",
        "raw-tracing-sentinel",
        "second-tracing-sentinel",
        "ordinary-raw-body",
        "display-secret-sentinel",
        "918273645",
    ] {
        assert!(!output.contains(forbidden), "tracing leaked {forbidden}");
    }
    let event: serde_json::Value =
        serde_json::from_str(output.trim()).expect("captured tracing must be JSON");
    assert_eq!(event["level"], "info");
    assert_eq!(event["event"], "connector_failure");
    assert_eq!(event["incident_id"], "incident-7");
    assert_eq!(event["connection_id"], "connection-7");
    assert!(event.get("session_secret").is_none());
    assert!(event.get("raw_payload").is_none());
    assert!(event.get("details").is_none());
    assert!(event.get("numeric_secret").is_none());
    assert_eq!(event["message"], "[REDACTED]");
    assert_eq!(event["outcome"], "[REDACTED]");
    assert_eq!(event["spans"], serde_json::json!(["connector_session"]));
}

#[test]
fn local_tracing_writes_filtered_json_to_the_owned_log() {
    let directory = tempfile::tempdir().expect("local tracing directory must exist");
    let path = directory.path().join("events.jsonl");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .expect("local tracing file must open");
    let log = LocalJsonLog::from_file(file);
    let tracing = init_local_tracing(log, LocalLogLevel::Info);

    tracing.with_default(|| {
        tracing::debug!(event = "filtered_debug");
        tracing::info!(event = "visible_info", incident_id = "incident-8");
    });
    tracing
        .shutdown()
        .expect("local tracing must flush without losses");

    let output = fs::read_to_string(path).expect("local tracing output must read");
    assert!(!output.contains("filtered_debug"));
    assert!(output.contains("visible_info"));
    assert!(output.contains("incident-8"));
    assert_eq!(output.lines().count(), 1);
}

#[test]
fn every_local_log_level_preserves_its_exact_filter_boundary() {
    let directory = tempfile::tempdir().expect("local tracing directory must exist");
    for (index, (level, expected)) in [
        (LocalLogLevel::Error, ["error", "", "", "", ""]),
        (LocalLogLevel::Warn, ["error", "warn", "", "", ""]),
        (LocalLogLevel::Info, ["error", "warn", "info", "", ""]),
        (LocalLogLevel::Debug, ["error", "warn", "info", "debug", ""]),
        (
            LocalLogLevel::Trace,
            ["error", "warn", "info", "debug", "trace"],
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let path = directory.path().join(format!("level-{index}.jsonl"));
        let log = LocalJsonLog::from_file(private_file(&path));
        let tracing = init_local_tracing(log, level);
        tracing.with_default(|| {
            tracing::error!(event = "error");
            tracing::warn!(event = "warn");
            tracing::info!(event = "info");
            tracing::debug!(event = "debug");
            tracing::trace!(event = "trace");
        });
        tracing.shutdown().expect("filtered trace must sync");
        let actual = fs::read_to_string(path)
            .expect("filtered trace must read")
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .expect("filtered trace must remain JSON")["event"]
                    .as_str()
                    .expect("filtered trace event must be text")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            expected
                .into_iter()
                .filter(|event| !event.is_empty())
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn local_json_rotation_is_bounded_and_preserves_complete_lines() {
    let directory = tempfile::tempdir().expect("rotation directory must exist");
    let current_path = directory.path().join("cmti.jsonl");
    let previous_path = directory.path().join("cmti.jsonl.1");
    let current = private_file(&current_path);
    let previous = private_file(&previous_path);
    let policy = LogRotationPolicy::new(64 * 1024, 2).expect("rotation policy must validate");
    assert_eq!(policy.maximum_segments(), 2);
    let log = LocalJsonLog::from_rotating_files(current, previous, policy)
        .expect("rotating log descriptors must validate");

    for sequence in 0..2_000_u64 {
        log.write_event(&serde_json::json!({
            "level": "info",
            "event": "rotation_probe",
            "sequence": sequence,
            "message": "bounded local observability rotation payload"
        }))
        .expect("rotation event must write");
    }
    log.shutdown().expect("rotating log must sync");

    let current = fs::read_to_string(&current_path).expect("current segment must read");
    let previous = fs::read_to_string(&previous_path).expect("previous segment must read");
    assert!(!current.is_empty());
    assert!(!previous.is_empty());
    assert!(
        fs::metadata(&current_path)
            .expect("current metadata must read")
            .len()
            <= policy.maximum_segment_bytes()
    );
    assert!(
        fs::metadata(&previous_path)
            .expect("previous metadata must read")
            .len()
            <= policy.maximum_segment_bytes()
    );
    assert!(current.contains(r#""sequence":1999"#));
    for line in current.lines().chain(previous.lines()) {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("rotation must preserve complete JSON lines");
    }
}

#[test]
fn rotating_log_rejects_aliases_and_non_private_segments() {
    let directory = tempfile::tempdir().expect("rotation directory must exist");
    let current_path = directory.path().join("cmti.jsonl");
    let current = private_file(&current_path);
    let alias = current.try_clone().expect("descriptor clone must succeed");
    let policy = LogRotationPolicy::new(64 * 1024, 2).expect("rotation policy must validate");
    assert!(matches!(
        LocalJsonLog::from_rotating_files(current, alias, policy),
        Err(ObservabilityError::UnsafeLogFile)
    ));

    let current = private_file(&directory.path().join("new-current.jsonl"));
    let previous_path = directory.path().join("unsafe-previous.jsonl");
    let previous = private_file(&previous_path);
    fs::set_permissions(&previous_path, fs::Permissions::from_mode(0o640))
        .expect("unsafe fixture mode must set");
    assert!(matches!(
        LocalJsonLog::from_rotating_files(current, previous, policy),
        Err(ObservabilityError::UnsafeLogFile)
    ));
}

#[test]
fn rotating_log_retains_validated_descriptors_after_path_replacement() {
    let directory = tempfile::tempdir().expect("rotation directory must exist");
    let current_path = directory.path().join("cmti.jsonl");
    let previous_path = directory.path().join("cmti.jsonl.1");
    let current = private_file(&current_path);
    let previous = private_file(&previous_path);
    let policy = LogRotationPolicy::new(64 * 1024, 2).expect("rotation policy must validate");
    let log = LocalJsonLog::from_rotating_files(current, previous, policy)
        .expect("rotating log descriptors must validate");

    let retained_current_path = directory.path().join("retained-current.jsonl");
    let retained_previous_path = directory.path().join("retained-previous.jsonl");
    fs::rename(&current_path, &retained_current_path).expect("current inode must move");
    fs::rename(&previous_path, &retained_previous_path).expect("previous inode must move");
    drop(private_file(&current_path));
    drop(private_file(&previous_path));

    for sequence in 0..2_000_u64 {
        log.write_event(&serde_json::json!({
            "level": "info",
            "event": "descriptor_retention_probe",
            "sequence": sequence,
            "message": "bounded retained descriptor payload"
        }))
        .expect("retained descriptors must remain writable");
    }
    log.shutdown().expect("retained descriptors must sync");

    assert_eq!(
        fs::metadata(&current_path)
            .expect("replacement current metadata")
            .len(),
        0
    );
    assert_eq!(
        fs::metadata(&previous_path)
            .expect("replacement previous metadata")
            .len(),
        0
    );
    let retained_current =
        fs::read_to_string(retained_current_path).expect("retained current must read");
    let retained_previous =
        fs::read_to_string(retained_previous_path).expect("retained previous must read");
    assert!(retained_current.contains(r#""sequence":1999"#));
    assert!(!retained_previous.is_empty());
    for line in retained_current.lines().chain(retained_previous.lines()) {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("retained rotation must preserve complete JSON lines");
    }
}

#[test]
fn rotating_log_recovers_a_torn_current_tail_and_rejects_torn_history() {
    let directory = tempfile::tempdir().expect("rotation directory must exist");
    let current_path = directory.path().join("cmti.jsonl");
    let previous_path = directory.path().join("cmti.jsonl.1");
    let mut current = private_file(&current_path);
    current
        .write_all(b"{\"level\":\"info\",\"event\":\"complete\"}\n{\"level\":\"info\"")
        .expect("torn current fixture must write");
    let previous = private_file(&previous_path);
    let policy = LogRotationPolicy::new(64 * 1024, 2).expect("rotation policy must validate");
    let log = LocalJsonLog::from_rotating_files(current, previous, policy)
        .expect("current torn tail must recover to its last complete line");
    log.write_event(&serde_json::json!({
        "level": "info",
        "event": "after_recovery"
    }))
    .expect("post-recovery event must write");
    log.shutdown().expect("recovered log must sync");
    let recovered = fs::read_to_string(&current_path).expect("recovered log must read");
    assert!(!recovered.contains(r#"{"level":"info"{"#));
    let events = recovered
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .expect("every recovered line must be complete JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["event"], "after_recovery");

    let safe_current = private_file(&directory.path().join("second-current.jsonl"));
    let unsafe_previous_path = directory.path().join("second-previous.jsonl");
    let mut unsafe_previous = private_file(&unsafe_previous_path);
    unsafe_previous
        .write_all(b"{\"level\":\"info\"")
        .expect("torn history fixture must write");
    assert!(matches!(
        LocalJsonLog::from_rotating_files(safe_current, unsafe_previous, policy),
        Err(ObservabilityError::UnsafeLogFile)
    ));
}

fn private_file(path: &std::path::Path) -> std::fs::File {
    let file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .expect("private log segment must create");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .expect("private log segment mode must set");
    file
}

#[test]
fn diagnostics_snapshot_is_deterministic_typed_and_bounded() {
    let metrics = Metrics::default();
    let received = MetricKey {
        name: MetricName::EventsReceived,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Success,
    };
    let depth = MetricKey {
        name: MetricName::QueueDepth,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::NotApplicable,
    };
    let latency = MetricKey {
        name: MetricName::WalFsyncLatency,
        component: Component::Storage,
        venue: Venue::NotApplicable,
        outcome: Outcome::Success,
    };

    metrics
        .gauge(depth, 3)
        .expect("queue depth gauge must update");
    metrics
        .observe_seconds(latency, 0.010)
        .expect("finite latency must update");
    metrics
        .increment(received, 2)
        .expect("event counter must update");

    let first = metrics.snapshot(1_000_000).expect("snapshot must be valid");
    let second = metrics
        .snapshot(1_000_000)
        .expect("snapshot must be reproducible");
    assert_eq!(
        serde_json::to_vec(&first).expect("snapshot must serialize"),
        serde_json::to_vec(&second).expect("snapshot must serialize")
    );
    let encoded = serde_json::to_string(&first).expect("snapshot must serialize");
    assert!(encoded.contains("transition_ingestion_events_total"));
    assert!(!encoded.contains(r#""name":"events_received""#));
    assert_eq!(first.schema_version(), 1);
    assert_eq!(first.captured_at_unix_nanos(), 1_000_000);
    assert_eq!(first.series().len(), 3);
    assert!(
        first
            .series()
            .windows(2)
            .all(|pair| pair[0].key < pair[1].key)
    );

    assert!(metrics.increment(depth, 1).is_err());
    assert!(metrics.gauge(received, 1).is_err());
    assert!(metrics.observe_seconds(latency, f64::NAN).is_err());
    assert!(metrics.snapshot(0).is_err());
}

#[test]
fn metric_registry_rejects_the_first_series_beyond_its_fixed_capacity() {
    let metrics = Metrics::default();
    let mut inserted = 0_usize;
    for name in MetricName::ALL {
        for component in Component::ALL {
            for venue in Venue::ALL {
                for outcome in Outcome::ALL {
                    let key = MetricKey {
                        name,
                        component,
                        venue,
                        outcome,
                    };
                    let result = match name.kind() {
                        MetricKind::Counter => metrics.increment(key, 1),
                        MetricKind::Gauge => metrics.gauge(key, 1),
                        MetricKind::Histogram => metrics.observe_seconds(key, 0.001),
                    };
                    if inserted == 1_024 {
                        assert!(matches!(result, Err(ObservabilityError::SeriesCapacity)));
                        assert_eq!(
                            metrics
                                .snapshot(1)
                                .expect("bounded snapshot must remain readable")
                                .series()
                                .len(),
                            1_024
                        );
                        return;
                    }
                    result.expect("series within the fixed capacity must be accepted");
                    inserted += 1;
                }
            }
        }
    }
    panic!("metric catalog did not exercise the registry capacity boundary");
}

#[test]
fn slo_observation_requires_samples_and_finite_values() {
    let passing = SloObservation {
        metric: MetricName::EventToNormalizedLatency,
        observed: 0.024,
        target: 0.025,
        sample_count: 100,
    };
    let failing = SloObservation {
        observed: 0.026,
        ..passing
    };
    let empty = SloObservation {
        sample_count: 0,
        ..passing
    };

    assert!(passing.passes());
    assert!(!failing.passes());
    assert!(!empty.passes());
}
