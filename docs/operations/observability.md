# Local observability contract

The daemon owns a local, in-process observability plane. It does not configure a
remote exporter, open an observability listener, or duplicate authoritative raw
exchange payloads from the WAL.

## Structured logs

`cryptoriskd` writes newline-delimited JSON to `cmti.jsonl` and
`cmti.jsonl.1` under the configured log directory. Both paths are opened
relative to the already validated log-directory descriptor. Existing segments
must be regular files owned by the current user, have one link, and grant no
group or other access. New segments are created with mode `0600`.

The two-segment policy rotates before an event would make the current segment
exceed 8 MiB. Rotation copies complete lines only between the two retained file
descriptors, synchronizes the previous segment, and truncates the current
segment. Runtime writes never re-resolve either pathname. A single encoded
event is limited to 64 KiB. On restart, an incomplete current tail is truncated
to the last complete JSON line; malformed retained history fails closed.
Shutdown synchronizes both segments and reports any structured-event write
failure.

The configured levels are `error`, `warn`, `info`, `debug`, and `trace`.
Lifecycle events currently include `fixture_runtime_ready`,
`fixture_runtime_stopped`, and `fixture_runtime_cancelled`. Secret-shaped fields,
credential-like values, and raw-payload/news fields are redacted before JSON
serialization. Code that handles an explicit secret or raw body should use
`SecretValue` or `RawPayload`, whose display, debug, and serialization
implementations never reveal the wrapped value.

## Metrics and diagnostics

`ObservabilityHandle` retains a bounded metrics registry in the daemon.
Metric keys contain only the generated enum values below; arbitrary symbols,
URLs, error strings, user text, and payload content cannot become labels. The
registry accepts at most 1,024 distinct series. Counters saturate, gauges use
signed integers, and latency histograms reject negative or non-finite values.

`DiagnosticsSnapshot` is local process state with schema version 1, a positive
capture time in Unix nanoseconds, and metric series sorted by key. Equal state
and capture time serialize identically. The current foundation daemon records
fixture ingestion events and bytes, rejected/parser/integrity counts, queue
depth, WAL fsync latency, and event-to-normalized latency. Metrics for later
feature, model, storage-compaction, RPC-lag, and process-resource phases are
reserved in the stable catalog but are not fabricated before those boundaries
exist.

The snapshot is currently an internal Rust API used by the daemon and
behavioral tests. A user-facing diagnostics export or RPC must be
user-initiated, authenticated on the existing loopback transport, bounded by
the RPC limits, and redacted again at export time. No unauthenticated or remote
metrics endpoint is permitted.

## Reference SLO interpretation

`SloObservation` passes only with at least one sample and finite observed and
target values where observed is less than or equal to the target. This type
records an evaluation; it does not claim reference-machine certification.
Latency and recovery targets in the product specification become release gates
only after a declared machine profile and representative load evidence exist.

## Operator checks

Run the focused contract and generated-catalog checks:

```sh
CARGO_INCREMENTAL=0 cargo +1.88.0 test -p observability --all-targets
cargo +1.88.0 run -p xtask -- observability-schema-check
```

For a process smoke, start the daemon through the foundation runtime runbook,
wait for its single readiness record, request the authenticated market
snapshot, terminate it, and restart against the same WAL. Confirm both log
segments remain private regular single-link files, every non-empty line parses
as JSON, no secret or fixture payload appears, and shutdown emits no write
failure.

<!-- BEGIN GENERATED OBSERVABILITY CATALOG -->

## Generated contract catalog

This block is checked against the Rust enums by `cargo run -p xtask -- observability-schema-check`.

### Metrics

| Name | Kind | Unit |
|---|---|---|
| `transition_ingestion_events_total` | `Counter` | `Count` |
| `transition_ingestion_bytes_total` | `Counter` | `Bytes` |
| `transition_ingestion_rejected_events_total` | `Counter` | `Count` |
| `transition_reconnects_total` | `Counter` | `Count` |
| `transition_parser_failures_total` | `Counter` | `Count` |
| `transition_sequence_gaps_total` | `Counter` | `Count` |
| `transition_checksum_failures_total` | `Counter` | `Count` |
| `transition_queue_depth` | `Gauge` | `Count` |
| `transition_dropped_events_total` | `Counter` | `Count` |
| `transition_wal_fsync_seconds` | `Histogram` | `Seconds` |
| `transition_event_to_normalized_seconds` | `Histogram` | `Seconds` |
| `transition_normalized_to_fast_feature_seconds` | `Histogram` | `Seconds` |
| `transition_fast_feature_to_forecast_seconds` | `Histogram` | `Seconds` |
| `transition_model_inference_seconds` | `Histogram` | `Seconds` |
| `transition_predictions_total` | `Counter` | `Count` |
| `transition_abstention_total` | `Counter` | `Count` |
| `transition_storage_compaction_seconds` | `Histogram` | `Seconds` |
| `transition_disk_budget_bytes` | `Gauge` | `Bytes` |
| `transition_rpc_seconds` | `Histogram` | `Seconds` |
| `transition_client_lag_seconds` | `Histogram` | `Seconds` |
| `transition_cpu_utilization_ppm` | `Gauge` | `PartsPerMillion` |
| `transition_memory_resident_bytes` | `Gauge` | `Bytes` |
| `transition_file_descriptor_count` | `Gauge` | `Count` |
| `transition_thread_count` | `Gauge` | `Count` |

### Structured log fields

| Field |
|---|
| `level` |
| `event` |
| `target` |
| `spans` |
| `message` |
| `component` |
| `outcome` |
| `source_id` |
| `instrument_id` |
| `connection_id` |
| `event_id` |
| `forecast_id` |
| `replay_id` |
| `incident_id` |
| `sequence` |
| `queue_depth` |
| `duration_micros` |
| `pending_wal_compression_jobs` |

### Bounded metric label values

- `component`: `ingestion`, `order_book`, `feature_engine`, `model_engine`, `rpc`, `alerting`, `storage`, `replay`, `system`
- `venue`: `binance`, `bybit`, `kraken`, `deribit`, `other`, `not_applicable`
- `outcome`: `success`, `rejected`, `timeout`, `degraded`, `abstained`, `not_applicable`

<!-- END GENERATED OBSERVABILITY CATALOG -->
