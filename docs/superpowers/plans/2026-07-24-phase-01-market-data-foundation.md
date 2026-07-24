# Phase 01 — Trusted Market-Data Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Capture, validate, normalize, persist, and deterministically replay v1 venue data without silent order-book or source-integrity failures.

**Architecture:** Native venue adapters write raw frames to an append-only WAL before normalization. Point-in-time instrument resolution and venue-specific snapshot/delta/checksum logic feed single-writer order-book shards, immutable Parquet datasets, metadata/audit state, and explicit quality records under a supervised bounded-channel runtime.

**Tech Stack:** Tokio, reqwest/rustls, tokio-tungstenite, bytes, Serde, CRC32/CRC32C, Arrow/Parquet, bundled SQLite/rusqlite, BLAKE3, proptest, Loom-ready state machines, clap-based operational CLIs.

## Global Constraints

- Rust production code uses Rust 1.97.1 initially, edition 2024, with MSRV 1.88; `Cargo.lock` is committed.
- Tokio stays on the tested 1.51 LTS minor line; Tonic stays on the tested 0.14 minor line.
- Swift code uses Swift 6.3 strict concurrency. The app baseline is Apple Silicon macOS 26; Core AI is optional and capability-gated on macOS 27 or later.
- SQLite is bundled at 3.53.3 or later; 3.51.3 is the absolute floor. SQLite stores metadata and audit state only, never high-rate books or trades.
- Source monetary values use checked `i128` fixed-point wrappers. `f64` begins only at an explicitly tested analytical boundary.
- One Rust modular-monolith daemon owns authoritative state and probabilities. Swift renders values received through versioned protobuf contracts and never recalculates production forecasts.
- Raw capture, replay, feature materialization, training, inference, and audit are local-first. Remote telemetry and hosted inference are disabled by default.
- No trading or withdrawal credentials, order placement, or automated execution are introduced in v1.
- Every bounded queue has a declared capacity and overflow policy. No order-book delta is silently dropped.
- Every production forecast is persisted with model, feature, label, quality, evidence, and calibration identifiers before alert delivery.
- Development follows test-driven increments, warnings-as-errors where practical, frequent focused commits, and deterministic fixtures.
- No task may weaken point-in-time correctness, abstention, source-quality gating, artifact signing, or audit lineage to make a demo pass.

---

## Scope and completion boundary

This plan implements the Phase 1 roadmap gate: WAL and recovery; historical instrument registry; Binance, Bybit, Kraken, and Deribit native connectors; trustworthy L2 and selected L3 books; normalized analytical storage; quality/admission controls; certification; and deterministic replay. It excludes feature computation, labels, statistical models, production forecasts, alerts, and Swift UI.

## File and module map

- `crates/connector-core`: source lifecycle, capabilities, completeness, commands, and connector errors.
- `crates/instrument-registry`: historical definitions and event-time symbol/generation resolution.
- `crates/raw-wal`: authoritative raw capture and crash recovery.
- `crates/metadata-store`: single-writer SQLite metadata and audit state.
- `crates/parquet-store`: immutable normalized analytical datasets.
- `crates/orderbook`: venue-parameterized L2/L3 state and integrity invariants.
- `crates/connector-*`: native source parsers, synchronization, and normalization.
- `crates/collector-runtime`: supervision, bounded queues, retries, admission, and shutdown.
- `crates/quality`: source/book health and completeness records.
- `crates/replay-engine`, `apps/crypto-replay`, `apps/connector-certify`: deterministic replay and certification.

## Exit gate

All four connectors pass committed certification suites; injected gaps/checksum errors never remain silent; books match golden references; raw WAL restart/tail recovery is deterministic; normalized Parquet manifests verify; source-completeness semantics are explicit; reference replay produces stable hashes; and the collector runtime sustains the Phase 1 fixture load without state corruption or undeclared drops.

---

### Task 1: Define the connector lifecycle, source capabilities, and bounded output contract

    **Files:**
    - Create: `crates/connector-core/Cargo.toml`
- Create: `crates/connector-core/src/lib.rs`
- Create: `crates/connector-core/src/capabilities.rs`
- Create: `crates/connector-core/src/lifecycle.rs`
- Test: `crates/connector-core/tests/connector_contract.rs`

    **Interfaces:**
    - Consumes: canonical events, configuration, observability, and Tokio bounded channels
    - Produces: `MarketDataConnector`, `ConnectorContext`, `SourceCapabilities`, `CompletenessProfile`, `ConnectorCommand`, and typed lifecycle/termination events

    **Implementation notes**

    Every connector publishes raw-capture references and normalized events separately. The contract distinguishes fatal schema incompatibility, recoverable disconnect, requested shutdown, and local capacity rejection. Retry policy belongs to the supervisor, not the connector.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/connector-core/tests/connector_contract.rs` with:

    ```text
    use connector_core::{Completeness, CompletenessProfile, ConnectorState};

#[test]
fn liquidation_completeness_is_explicit() {
    let profile = CompletenessProfile::binance_futures();
    assert_eq!(profile.liquidations, Completeness::SampledLatestPerWindow);
}

#[test]
fn invalid_lifecycle_transition_is_rejected() {
    assert!(ConnectorState::Connected.transition_to(ConnectorState::Starting).is_err());
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p connector-core`

    Expected: FAIL because connector lifecycle and completeness types do not exist

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/connector-core/src/lib.rs` with:

    ```text
    use async_trait::async_trait;
use event_envelope::NormalizedEvent;
use tokio::sync::{mpsc, watch};

pub struct ConnectorContext {
    pub events: mpsc::Sender<NormalizedEvent>,
    pub commands: watch::Receiver<ConnectorCommand>,
    pub connection_epoch: u64,
}

#[async_trait]
pub trait MarketDataConnector: Send + 'static {
    fn source_id(&self) -> &'static str;
    fn capabilities(&self) -> SourceCapabilities;
    async fn run(self, context: ConnectorContext) -> Result<ConnectorExit, ConnectorError>;
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p connector-core`

    Expected: PASS with lifecycle, capability, completeness, queue-closure, and cancellation tests

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo clippy -p connector-core --all-targets -- -D warnings && cargo test -p connector-core`

    Expected: connector-core passes with no unbounded channel or source-agnostic liquidation assumption

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/connector-core Cargo.toml Cargo.lock
    git commit -m "feat: define source connector contracts"
    ```
### Task 2: Build the point-in-time instrument registry and generation resolver

    **Files:**
    - Create: `crates/instrument-registry/Cargo.toml`
- Create: `crates/instrument-registry/src/lib.rs`
- Create: `crates/instrument-registry/src/history.rs`
- Create: `crates/instrument-registry/src/resolve.rs`
- Test: `crates/instrument-registry/tests/generation_history.rs`

    **Interfaces:**
    - Consumes: canonical `InstrumentDefinition` values and metadata-store trait
    - Produces: `InstrumentRegistry` with historical definitions, generation changes, symbol-at-time resolution, and immutable catalog snapshots

    **Implementation notes**

    Definitions are append-only except for a versioned correction record. The registry never mutates an old generation in place. A connector must resolve metadata before publishing a normalized market event.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/instrument-registry/tests/generation_history.rs` with:

    ```text
    use instrument_registry::{InstrumentRegistry, ResolveError};

#[test]
fn reused_symbol_resolves_by_event_time_and_generation() {
    let registry = InstrumentRegistry::fixture_with_symbol_reuse();
    let old = registry.resolve("venue", "ABCUSD", 100).unwrap();
    let new = registry.resolve("venue", "ABCUSD", 500).unwrap();
    assert_ne!(old.generation, new.generation);
}

#[test]
fn gap_between_listings_does_not_guess() {
    let registry = InstrumentRegistry::fixture_with_symbol_reuse();
    assert!(matches!(registry.resolve("venue", "ABCUSD", 300), Err(ResolveError::NotListedAtTime)));
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p instrument-registry`

    Expected: FAIL because point-in-time registry APIs are missing

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/instrument-registry/src/lib.rs` with:

    ```text
    use domain::{InstrumentDefinition, InstrumentId, VenueId};
use std::collections::BTreeMap;

pub struct InstrumentRegistry {
    by_symbol: BTreeMap<(VenueId, String), Vec<InstrumentDefinition>>,
    by_id: BTreeMap<InstrumentId, InstrumentDefinition>,
}

impl InstrumentRegistry {
    pub fn insert(&mut self, definition: InstrumentDefinition) -> Result<(), RegistryError> {
        definition.validate()?;
        self.reject_overlap(&definition)?;
        self.by_id.insert(definition.id().clone(), definition.clone());
        self.by_symbol.entry((definition.id().venue.clone(), definition.id().venue_symbol.clone())).or_default().push(definition);
        Ok(())
    }
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p instrument-registry`

    Expected: PASS with overlap, delisting, symbol reuse, option identity, and snapshot-hash tests

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo test -p instrument-registry && cargo clippy -p instrument-registry --all-targets -- -D warnings`

    Expected: registry tests pass and catalog snapshots have deterministic hashes

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/instrument-registry Cargo.toml Cargo.lock
    git commit -m "feat: add historical instrument registry"
    ```
### Task 3: Implement segmented append-only raw WAL framing and crash recovery

    **Files:**
    - Create: `crates/raw-wal/Cargo.toml`
- Create: `crates/raw-wal/src/lib.rs`
- Create: `crates/raw-wal/src/frame.rs`
- Create: `crates/raw-wal/src/segment.rs`
- Create: `crates/raw-wal/src/recovery.rs`
- Test: `crates/raw-wal/tests/recovery.rs`

    **Interfaces:**
    - Consumes: raw payload hashes, source IDs, connection epochs, and configured fsync policy
    - Produces: `WalWriter`, `WalReader`, CRC-protected frames, segment rotation, durable offsets, tail truncation recovery, and replay ordering

    **Implementation notes**

    WAL append happens before parsing/normalization acknowledgement. Segment headers include schema, host, build, and source tables. Recovery quarantines mid-segment corruption; only an incomplete final frame is truncated automatically.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/raw-wal/tests/recovery.rs` with:

    ```text
    use raw_wal::{Durability, WalReader, WalWriter};
use tempfile::tempdir;

#[test]
fn truncated_tail_recovers_only_complete_frames() {
    let dir = tempdir().unwrap();
    let mut writer = WalWriter::open(dir.path(), Durability::EveryFrame).unwrap();
    writer.append_fixture(b"one").unwrap();
    writer.append_fixture(b"two").unwrap();
    writer.corrupt_tail_for_test(5).unwrap();
    let records = WalReader::recover(dir.path()).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].payload(), b"one");
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p raw-wal --test recovery`

    Expected: FAIL because WAL framing and recovery are absent

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/raw-wal/src/frame.rs` with:

    ```text
    pub const MAGIC: [u8; 4] = *b"TIW1";

#[derive(Clone, Debug)]
pub struct WalFrame {
    pub version: u16,
    pub source_id: u32,
    pub connection_epoch: u64,
    pub receive_wall_time_ns: i64,
    pub receive_monotonic_time_ns: u64,
    pub payload: bytes::Bytes,
}

impl WalFrame {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), WalError> {
        let payload_len = u32::try_from(self.payload.len()).map_err(|_| WalError::FrameTooLarge)?;
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&payload_len.to_le_bytes());
        out.extend_from_slice(&self.source_id.to_le_bytes());
        out.extend_from_slice(&self.connection_epoch.to_le_bytes());
        out.extend_from_slice(&self.receive_wall_time_ns.to_le_bytes());
        out.extend_from_slice(&self.receive_monotonic_time_ns.to_le_bytes());
        out.extend_from_slice(&self.payload);
        let checksum = crc32c::crc32c(out);
        out.extend_from_slice(&checksum.to_le_bytes());
        Ok(())
    }
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p raw-wal --test recovery`

    Expected: PASS for clean, truncated, corrupted, rotated, duplicate-open, and kill/restart fixtures

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo test -p raw-wal && cargo clippy -p raw-wal --all-targets -- -D warnings`

    Expected: all recovery tests pass and every accepted frame is ordered by durable segment/offset

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/raw-wal Cargo.toml Cargo.lock
    git commit -m "feat: add crash-safe raw event WAL"
    ```
### Task 4: Create the single-writer SQLite metadata and audit store

    **Files:**
    - Create: `crates/metadata-store/Cargo.toml`
- Create: `crates/metadata-store/src/lib.rs`
- Create: `crates/metadata-store/src/migrations.rs`
- Create: `crates/metadata-store/migrations/0001_initial.sql`
- Test: `crates/metadata-store/tests/migration_and_integrity.rs`

    **Interfaces:**
    - Consumes: bundled SQLite policy, instrument registry records, config/model/alert audit requirements
    - Produces: `MetadataStore` command actor, schema migrations, integrity checks, explicit checkpoints, and append-only audit records

    **Implementation notes**

    Open read-only connections separately. Expose checkpoint and integrity results as health metrics. A migration checksum mismatch fails startup before sources connect.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/metadata-store/tests/migration_and_integrity.rs` with:

    ```text
    use metadata_store::{MetadataStore, StoreOptions};
use tempfile::tempdir;

#[tokio::test]
async fn migration_is_idempotent_and_integrity_check_passes() {
    let dir = tempdir().unwrap();
    let store = MetadataStore::open(dir.path().join("meta.sqlite"), StoreOptions::test()).await.unwrap();
    store.migrate().await.unwrap();
    store.migrate().await.unwrap();
    assert_eq!(store.integrity_check().await.unwrap(), "ok");
}

#[tokio::test]
async fn audit_rows_are_append_only() {
    let store = MetadataStore::memory_for_test().await.unwrap();
    let id = store.append_audit_fixture().await.unwrap();
    assert!(store.delete_audit_for_test(id).await.is_err());
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p metadata-store`

    Expected: FAIL because the store actor and migration are missing

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/metadata-store/migrations/0001_initial.sql` with:

    ```text
    PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;

CREATE TABLE schema_migrations (
  version INTEGER PRIMARY KEY,
  applied_at_ns INTEGER NOT NULL,
  checksum BLOB NOT NULL
);

CREATE TABLE audit_records (
  audit_id TEXT PRIMARY KEY,
  occurred_at_ns INTEGER NOT NULL,
  actor TEXT NOT NULL,
  category TEXT NOT NULL,
  payload_hash BLOB NOT NULL,
  previous_hash BLOB,
  record_hash BLOB NOT NULL
);

CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit_records
BEGIN SELECT RAISE(ABORT, 'audit records are append-only'); END;
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p metadata-store`

    Expected: PASS with migration checksums, single-writer serialization, busy handling, explicit checkpoint, and unclean-open integrity tests

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo test -p metadata-store && cargo clippy -p metadata-store --all-targets -- -D warnings`

    Expected: metadata tests pass using the bundled SQLite and no high-rate event table exists

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/metadata-store Cargo.toml Cargo.lock
    git commit -m "feat: add metadata and audit store"
    ```
### Task 5: Implement immutable normalized Parquet datasets and atomic partition sealing

    **Files:**
    - Create: `crates/parquet-store/Cargo.toml`
- Create: `crates/parquet-store/src/lib.rs`
- Create: `crates/parquet-store/src/layout.rs`
- Create: `crates/parquet-store/src/writer.rs`
- Create: `crates/parquet-store/src/manifest.rs`
- Test: `crates/parquet-store/tests/atomic_seal.rs`

    **Interfaces:**
    - Consumes: Arrow-compatible normalized event batches and content hashing
    - Produces: `DatasetWriter`, partition layout, active/sealed lifecycle, Zstandard Parquet policy, atomic manifests, and immutable correction versions

    **Implementation notes**

    Write to a private temporary path, fsync file and directory, then atomically publish the manifest. Include schema, parser/normalizer versions, source coverage, code commit, and event-time range in file metadata.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/parquet-store/tests/atomic_seal.rs` with:

    ```text
    use parquet_store::{DatasetWriter, PartitionKey};
use tempfile::tempdir;

#[test]
fn sealed_partition_is_immutable_and_manifest_hashes_files() {
    let dir = tempdir().unwrap();
    let mut writer = DatasetWriter::fixture(dir.path()).unwrap();
    let key = PartitionKey::fixture();
    writer.write_fixture(&key).unwrap();
    let manifest = writer.seal(&key).unwrap();
    assert!(manifest.verify(dir.path()).unwrap());
    assert!(writer.write_fixture(&key).is_err());
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p parquet-store`

    Expected: FAIL because dataset writer and manifest APIs are absent

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/parquet-store/src/layout.rs` with:

    ```text
    #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct PartitionKey {
    pub dataset: String,
    pub schema_version: u32,
    pub venue: String,
    pub instrument_generation: u32,
    pub utc_date: time::Date,
    pub hour: u8,
}

impl PartitionKey {
    pub fn relative_path(&self) -> String {
        format!(
            "dataset={}/schema={}/venue={}/generation={}/date={}/hour={:02}",
            self.dataset, self.schema_version, self.venue, self.instrument_generation, self.utc_date, self.hour
        )
    }
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p parquet-store`

    Expected: PASS for atomic rename, crash-before-manifest, sealed immutability, correction dataset, metadata, and compaction fixtures

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo test -p parquet-store && cargo clippy -p parquet-store --all-targets -- -D warnings`

    Expected: Parquet files validate, row-group targets are configurable, and no small-file compaction rewrites sealed history in place

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/parquet-store Cargo.toml Cargo.lock
    git commit -m "feat: add immutable normalized Parquet store"
    ```
### Task 6: Build the single-writer L2/L3 order-book state machine and recovery invariants

    **Files:**
    - Create: `crates/orderbook/Cargo.toml`
- Create: `crates/orderbook/src/lib.rs`
- Create: `crates/orderbook/src/state.rs`
- Create: `crates/orderbook/src/book.rs`
- Create: `crates/orderbook/src/checksum.rs`
- Test: `crates/orderbook/tests/state_machine.rs`
- Test: `crates/orderbook/tests/properties.rs`

    **Interfaces:**
    - Consumes: fixed-point levels, event envelopes, instrument metadata, and connector snapshot/delta semantics
    - Produces: `OrderBookEngine`, per-instrument synchronization states, gap/checksum invalidation, snapshots, L2 levels, optional L3 orders, and trusted-state quality output

    **Implementation notes**

    Use one writer per stable instrument shard. Bids remain strictly descending, asks ascending, zero quantity deletes, negative quantity is rejected, and locked/crossed state is preserved as an explicit classification rather than “fixed.”

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/orderbook/tests/state_machine.rs` with:

    ```text
    use orderbook::{ApplyResult, BookState, OrderBookEngine};

#[test]
fn sequence_gap_invalidates_book_until_fresh_snapshot() {
    let mut book = OrderBookEngine::fixture_synced_at(10);
    assert_eq!(book.apply_delta_fixture(12), ApplyResult::GapDetected);
    assert_eq!(book.state(), BookState::Untrusted);
    assert!(book.snapshot().is_err());
    book.apply_snapshot_fixture(20).unwrap();
    assert_eq!(book.state(), BookState::Synchronized);
}

#[test]
fn stale_connection_epoch_delta_is_rejected() {
    let mut book = OrderBookEngine::fixture_synced_at(10);
    assert!(book.apply_old_epoch_delta_fixture().is_err());
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p orderbook --test state_machine`

    Expected: FAIL because the state machine and trusted snapshot contract are absent

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/orderbook/src/state.rs` with:

    ```text
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookState {
    Disconnected,
    Buffering,
    AwaitingSnapshot,
    Replaying,
    Synchronized,
    Untrusted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyResult {
    Applied,
    Duplicate,
    GapDetected,
    ChecksumMismatch,
    SnapshotRequired,
}

impl BookState {
    pub fn permits_publication(self) -> bool { matches!(self, Self::Synchronized) }
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p orderbook --test state_machine`

    Expected: PASS for snapshot alignment, buffered replay, duplicate, reorder, gap, checksum mismatch, epoch reset, and locked/crossed classification

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo test -p orderbook && cargo clippy -p orderbook --all-targets -- -D warnings`

    Expected: unit and proptest invariants pass; no untrusted book can produce trusted features

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/orderbook Cargo.toml Cargo.lock
    git commit -m "feat: add integrity-first order-book engine"
    ```
### Task 7: Implement and certify the Binance spot and USD-margined derivatives adapter

    **Files:**
    - Create: `crates/connector-binance/Cargo.toml`
- Create: `crates/connector-binance/src/lib.rs`
- Create: `crates/connector-binance/src/parser.rs`
- Create: `crates/connector-binance/src/book_sync.rs`
- Create: `crates/connector-binance/src/metadata.rs`
- Create: `fixtures/exchanges/binance/manifest.toml`
- Test: `crates/connector-binance/tests/fixtures.rs`

    **Interfaces:**
    - Consumes: connector-core, WAL, instrument registry, order-book engine, and retained Binance fixtures
    - Produces: native Binance parser/normalizer for spot and linear perpetual metadata, trades, depth, mark/index, funding, OI, venue status, and explicitly sampled liquidation observations

    **Implementation notes**

    Do not infer complete liquidations from the one-second “latest per symbol” stream. Separate REST snapshot rate limits from WebSocket reconnect limits. Capture every raw message before parser acknowledgement.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/connector-binance/tests/fixtures.rs` with:

    ```text
    use connector_binance::FixtureRunner;

#[test]
fn depth_gap_forces_resnapshot_and_liquidation_is_marked_sampled() {
    let report = FixtureRunner::run("fixtures/exchanges/binance/depth_gap.jsonl").unwrap();
    assert_eq!(report.resnapshot_count, 1);
    assert!(!report.published_untrusted_books);
    assert!(report.liquidations.iter().all(|v| v.completeness.is_sampled()));
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p connector-binance`

    Expected: FAIL because the adapter and fixtures do not exist

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/connector-binance/src/lib.rs` with:

    ```text
    use async_trait::async_trait;
use connector_core::{ConnectorContext, ConnectorError, ConnectorExit, MarketDataConnector, SourceCapabilities};

pub struct BinanceConnector {
    config: BinanceConfig,
}

#[async_trait]
impl MarketDataConnector for BinanceConnector {
    fn source_id(&self) -> &'static str { "binance" }
    fn capabilities(&self) -> SourceCapabilities { SourceCapabilities::binance_v1() }
    async fn run(self, context: ConnectorContext) -> Result<ConnectorExit, ConnectorError> {
        self.run_session(context).await
    }
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p connector-binance`

    Expected: PASS across normal, duplicate, gap, reconnect, metadata change, oversized, heartbeat-loss, and liquidation-completeness fixtures

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo run -p connector-certify -- --venue binance --fixtures fixtures/exchanges/binance`

    Expected: certification report is PASS and its hash is written beside the fixture manifest

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/connector-binance fixtures/exchanges/binance Cargo.toml Cargo.lock
    git commit -m "feat: add certified Binance market-data adapter"
    ```
### Task 8: Implement and certify the Bybit spot and linear-derivatives adapter

    **Files:**
    - Create: `crates/connector-bybit/Cargo.toml`
- Create: `crates/connector-bybit/src/lib.rs`
- Create: `crates/connector-bybit/src/parser.rs`
- Create: `crates/connector-bybit/src/book_sync.rs`
- Create: `fixtures/exchanges/bybit/manifest.toml`
- Test: `crates/connector-bybit/tests/fixtures.rs`

    **Interfaces:**
    - Consumes: connector-core, WAL, registry, and order-book reset semantics
    - Produces: native Bybit adapter with snapshot-reset behavior, trades, depth, funding, OI, mark/index, and all-liquidation completeness profile

    **Implementation notes**

    A new snapshot discards the prior local book; never merge it as a delta. Preserve source timestamps and Bybit sequence/update IDs separately in the envelope.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/connector-bybit/tests/fixtures.rs` with:

    ```text
    use connector_bybit::FixtureRunner;

#[test]
fn later_snapshot_replaces_local_book_before_deltas_resume() {
    let report = FixtureRunner::run("fixtures/exchanges/bybit/snapshot_reset.jsonl").unwrap();
    assert_eq!(report.snapshot_resets, 2);
    assert_eq!(report.final_best_bid, "100.00");
    assert!(report.final_book_trusted);
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p connector-bybit`

    Expected: FAIL because the Bybit adapter is absent

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/connector-bybit/src/book_sync.rs` with:

    ```text
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BybitBookMessage { Snapshot, Delta }

pub fn apply_message(engine: &mut orderbook::OrderBookEngine, message: NormalizedBybitBook) -> Result<(), BybitError> {
    match message.kind {
        BybitBookMessage::Snapshot => {
            engine.reset_for_snapshot(message.connection_epoch);
            engine.apply_snapshot(message.into_snapshot())?;
        }
        BybitBookMessage::Delta => { engine.apply_delta(message.into_delta())?; }
    }
    Ok(())
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p connector-bybit`

    Expected: PASS across reset, gap, reconnect, malformed, heartbeat, metadata, and liquidation fixtures

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo run -p connector-certify -- --venue bybit --fixtures fixtures/exchanges/bybit`

    Expected: certification report is PASS with documented snapshot and 500 ms liquidation semantics

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/connector-bybit fixtures/exchanges/bybit Cargo.toml Cargo.lock
    git commit -m "feat: add certified Bybit market-data adapter"
    ```
### Task 9: Implement and certify the Kraken spot/derivatives adapter with checksum and selected L3 support

    **Files:**
    - Create: `crates/connector-kraken/Cargo.toml`
- Create: `crates/connector-kraken/src/lib.rs`
- Create: `crates/connector-kraken/src/parser.rs`
- Create: `crates/connector-kraken/src/checksum.rs`
- Create: `crates/connector-kraken/src/l3.rs`
- Create: `fixtures/exchanges/kraken/manifest.toml`
- Test: `crates/connector-kraken/tests/checksum.rs`

    **Interfaces:**
    - Consumes: connector-core, order-book checksum interface, and Tier A L3 configuration
    - Produces: Kraken adapter for spot/derivatives L2 plus configured L3, exact CRC formatting, queue-order records, and checksum-driven recovery

    **Implementation notes**

    Checksum string formatting is source-specific and covered by golden fixtures. L3 is enabled only for configured Tier A instruments; all individual-order IDs remain source scoped.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/connector-kraken/tests/checksum.rs` with:

    ```text
    use connector_kraken::checksum::kraken_crc32;

#[test]
fn checksum_matches_committed_reference_fixture() {
    let book = connector_kraken::fixtures::checksum_book();
    assert_eq!(kraken_crc32(&book).unwrap(), 974947235);
}

#[test]
fn checksum_mismatch_never_publishes_book() {
    let report = connector_kraken::FixtureRunner::run("fixtures/exchanges/kraken/bad_checksum.jsonl").unwrap();
    assert!(report.source_became_unhealthy);
    assert!(!report.published_untrusted_books);
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p connector-kraken`

    Expected: FAIL because checksum formatting and adapter code are missing

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/connector-kraken/src/checksum.rs` with:

    ```text
    pub fn kraken_crc32(book: &orderbook::BookSnapshot) -> Result<u32, KrakenError> {
    let mut input = String::new();
    for level in book.asks().iter().take(10).chain(book.bids().iter().take(10)) {
        input.push_str(&level.price.kraken_checksum_component()?);
        input.push_str(&level.quantity.kraken_checksum_component()?);
    }
    Ok(crc32fast::hash(input.as_bytes()))
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p connector-kraken`

    Expected: PASS for official checksum examples, L2/L3 ordering, reconnect, malformed payload, and metadata fixtures

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo run -p connector-certify -- --venue kraken --fixtures fixtures/exchanges/kraken`

    Expected: certification report is PASS and selected L3 instruments respect capacity configuration

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/connector-kraken fixtures/exchanges/kraken Cargo.toml Cargo.lock
    git commit -m "feat: add certified Kraken market-data adapter"
    ```
### Task 10: Implement and certify the Deribit futures, perpetuals, and options adapter

    **Files:**
    - Create: `crates/connector-deribit/Cargo.toml`
- Create: `crates/connector-deribit/src/lib.rs`
- Create: `crates/connector-deribit/src/parser.rs`
- Create: `crates/connector-deribit/src/options.rs`
- Create: `fixtures/exchanges/deribit/manifest.toml`
- Test: `crates/connector-deribit/tests/fixtures.rs`

    **Interfaces:**
    - Consumes: connector-core, canonical option identity, registry, and order books
    - Produces: Deribit adapter for futures/perpetuals/options metadata, trades, books, implied volatility, Greeks, OI, funding, mark/index, and delivery records

    **Implementation notes**

    Keep source-reported IV/Greeks separate from later recomputed surfaces. Record delivery and settlement observations because expiry labels and basis calculations require them.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/connector-deribit/tests/fixtures.rs` with:

    ```text
    use connector_deribit::FixtureRunner;

#[test]
fn option_identity_preserves_expiry_strike_and_side() {
    let report = FixtureRunner::run("fixtures/exchanges/deribit/option_ticker.jsonl").unwrap();
    let option = &report.options[0];
    assert_eq!(option.instrument.option_side.unwrap().as_str(), "call");
    assert_eq!(option.instrument.strike.unwrap().to_string(), "65000");
    assert!(option.mark_iv.is_some());
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p connector-deribit`

    Expected: FAIL because Deribit option normalization is missing

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/connector-deribit/src/options.rs` with:

    ```text
    #[derive(Clone, Debug)]
pub struct NormalizedOptionTicker {
    pub instrument: domain::InstrumentId,
    pub underlying_index: fixed_decimal::Price,
    pub mark_price: fixed_decimal::Price,
    pub mark_iv: Option<f64>,
    pub bid_iv: Option<f64>,
    pub ask_iv: Option<f64>,
    pub delta: Option<f64>,
    pub gamma: Option<f64>,
    pub theta: Option<f64>,
    pub vega: Option<f64>,
    pub open_interest_contracts: fixed_decimal::Quantity,
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p connector-deribit`

    Expected: PASS across futures, perpetual, option chain, Greeks, book, reconnect, malformed, and settlement fixtures

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo run -p connector-certify -- --venue deribit --fixtures fixtures/exchanges/deribit`

    Expected: certification report is PASS and every option record resolves a complete canonical instrument generation

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/connector-deribit fixtures/exchanges/deribit Cargo.toml Cargo.lock
    git commit -m "feat: add certified Deribit derivatives adapter"
    ```
### Task 11: Supervise collectors with explicit backpressure, quality states, and admission control

    **Files:**
    - Create: `crates/quality/Cargo.toml`
- Create: `crates/quality/src/lib.rs`
- Create: `crates/quality/src/source_health.rs`
- Create: `crates/collector-runtime/Cargo.toml`
- Create: `crates/collector-runtime/src/lib.rs`
- Create: `crates/collector-runtime/src/supervisor.rs`
- Modify: `apps/cryptoriskd/src/main.rs`
- Test: `crates/collector-runtime/tests/backpressure.rs`

    **Interfaces:**
    - Consumes: certified connectors, WAL, order books, coverage tiers, and local metrics
    - Produces: collector supervisor with retry budgets, connection epochs, bounded queues, Tier A/B/C admission, coalescing only for declared noncritical updates, and quality-state publication

    **Implementation notes**

    A source moves through healthy, degraded, unhealthy, quarantined, and recovering states. Backoff uses deterministic jitter in tests. Shutdown stops source reads, drains WAL, checkpoints books, seals eligible partitions, then closes metadata.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/collector-runtime/tests/backpressure.rs` with:

    ```text
    use collector_runtime::{OverflowAction, QueueClass, SupervisorHarness};

#[tokio::test]
async fn full_book_delta_queue_degrades_and_resyncs_instead_of_dropping() {
    let report = SupervisorHarness::with_capacity(1).saturate_book_deltas().await;
    assert_eq!(report.action, OverflowAction::DegradeAndResnapshot);
    assert_eq!(report.silent_drops, 0);
}

#[test]
fn ticker_updates_may_coalesce_by_instrument() {
    assert!(QueueClass::UiTicker.coalescing_allowed());
    assert!(!QueueClass::BookDelta.coalescing_allowed());
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p collector-runtime`

    Expected: FAIL because supervision and queue policies do not exist

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/collector-runtime/src/supervisor.rs` with:

    ```text
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverflowAction {
    Backpressure,
    CoalesceByKey,
    DegradeAndResnapshot,
    RejectAdmission,
}

pub fn overflow_action(class: QueueClass) -> OverflowAction {
    match class {
        QueueClass::BookDelta => OverflowAction::DegradeAndResnapshot,
        QueueClass::RawWal => OverflowAction::Backpressure,
        QueueClass::UiTicker => OverflowAction::CoalesceByKey,
        QueueClass::HistoricalCompaction => OverflowAction::RejectAdmission,
    }
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p collector-runtime`

    Expected: PASS for saturation, retry, cancellation, shutdown ordering, source quarantine, and capacity rejection tests

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo test -p collector-runtime -p quality && cargo clippy -p collector-runtime -p quality --all-targets -- -D warnings`

    Expected: supervisor/quality tests pass and every queue exports capacity, occupancy, overflow action, and event counters

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add crates/collector-runtime crates/quality apps/cryptoriskd/src/main.rs Cargo.toml Cargo.lock
    git commit -m "feat: supervise sources with explicit quality gates"
    ```
### Task 12: Deliver deterministic replay and the connector certification CLI

    **Files:**
    - Create: `apps/crypto-replay/Cargo.toml`
- Create: `apps/crypto-replay/src/main.rs`
- Create: `apps/connector-certify/Cargo.toml`
- Create: `apps/connector-certify/src/main.rs`
- Create: `crates/replay-engine/Cargo.toml`
- Create: `crates/replay-engine/src/lib.rs`
- Create: `fixtures/golden-replays/market-foundation/manifest.toml`
- Test: `crates/replay-engine/tests/golden_market_foundation.rs`

    **Interfaces:**
    - Consumes: WAL, connectors, registry, order books, stores, quality engine, and source fixtures
    - Produces: replay engine using the production normalization/book path, CLI controls, deterministic run manifests, and certification reports for all four v1 venues

    **Implementation notes**

    Replay time is driven by recorded receive monotonic deltas, not wall-clock sleeps, and can run faster than real time. The certification report includes fixture provenance, source completeness, parser versions, injected failures, and exact artifact hashes.

    - [ ] **Step 1: Write the failing test**

    Create or replace `crates/replay-engine/tests/golden_market_foundation.rs` with:

    ```text
    use replay_engine::{ReplayConfig, ReplayRunner};

#[test]
fn repeated_replay_produces_identical_books_events_and_incidents() {
    let cfg = ReplayConfig::from_manifest("fixtures/golden-replays/market-foundation/manifest.toml").unwrap();
    let first = ReplayRunner::run_to_digest(&cfg).unwrap();
    let second = ReplayRunner::run_to_digest(&cfg).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.silent_integrity_failures, 0);
}
    ```

    - [ ] **Step 2: Run the focused test and confirm the intended failure**

    Run: `cargo test -p replay-engine --test golden_market_foundation`

    Expected: FAIL because replay engine and golden manifest are absent

    - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

    Create or update `crates/replay-engine/src/lib.rs` with:

    ```text
    pub struct ReplayConfig {
    pub wal_paths: Vec<std::path::PathBuf>,
    pub deterministic_seed: u64,
    pub stop_at_receive_time_ns: Option<i64>,
}

pub struct ReplayDigest {
    pub normalized_event_hash: blake3::Hash,
    pub book_state_hash: blake3::Hash,
    pub quality_incident_hash: blake3::Hash,
    pub silent_integrity_failures: u64,
}

pub struct ReplayRunner;

impl ReplayRunner {
    pub fn run_to_digest(config: &ReplayConfig) -> Result<ReplayDigest, ReplayError> {
        replay_with_production_pipeline(config)
    }
}
    ```

    - [ ] **Step 4: Run the focused test and confirm success**

    Run: `cargo test -p replay-engine --test golden_market_foundation`

    Expected: PASS and repeated runs produce identical event/book/incident hashes

    - [ ] **Step 5: Run the subsystem verification command**

    Run: `cargo test -p replay-engine && cargo run -p connector-certify -- --all --fixtures fixtures/exchanges && cargo run -p crypto-replay -- --manifest fixtures/golden-replays/market-foundation/manifest.toml --verify`

    Expected: all connectors certify and replay verification reports zero mismatches

    - [ ] **Step 6: Inspect the diff and fixture provenance**

    Run: `git diff --check && git status --short`

    Expected: no whitespace errors; every source-derived fixture has a provenance sidecar and redistribution classification.

    - [ ] **Step 7: Commit the independently testable deliverable**

    ```bash
    git add apps/crypto-replay apps/connector-certify crates/replay-engine fixtures/golden-replays/market-foundation Cargo.toml Cargo.lock
    git commit -m "feat: add deterministic replay and connector certification"
    ```
