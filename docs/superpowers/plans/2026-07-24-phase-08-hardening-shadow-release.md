# Phase 08 — Hardening, Shadow Validation, and Stable Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert the complete local-first observatory into a fault-tolerant, security-reviewed, continuously shadow-validated, signed, notarized, and independently releasable open-source product.

**Architecture:** Hardening is implemented as product code and executable evidence, not as a release-day checklist. A local observability and capacity layer makes limits explicit; deterministic concurrency, fuzz, chaos, recovery, upgrade, and soak suites exercise every critical boundary. Immutable shadow forecasts feed drift and calibration reports, which in turn drive a signed, independently approved model-promotion gate. Release tooling produces nested-signed macOS artifacts, SBOMs, provenance, upgrade/rollback evidence, and a machine-verifiable acceptance manifest.

**Tech Stack:** Rust 1.97.1/edition 2024, Tokio 1.51 LTS, `tracing`, local metrics, Tonic, Arrow/Parquet/DataFusion, bundled SQLite, `cargo-fuzz`, proptest, Loom, Criterion, Swift 6.3/XCTest, Xcode Instruments/ETTrace, Developer ID/notarytool/stapler, Ed25519 signatures, CycloneDX/SPDX manifests, GitHub Actions with pinned actions, local operator runbooks.

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

## Phase file map

```text
crates/
  observability/
  capacity/
  parser-limits/
  concurrency-testkit/
  chaos/
  recovery/
  runtime-health/
  security-policy/
  artifact-signing/
  shadow-ledger/
  model-monitor/
  promotion-gate/
apps/
  load-generator/
  shadow-reporter/
  release-cli/
tests/
  fuzz/
  loom/
  chaos/
  recovery/
  performance/
  soak/
  shadow/
  upgrades/
  security/
docs/
  operations/
  security/
  release/
  governance/
scripts/
  package-macos.sh
  notarize-macos.sh
  verify-release.sh
  run-soak.sh
.github/workflows/
  hardening.yml
  nightly-fuzz.yml
  nightly-soak.yml
  shadow-report.yml
  release.yml
```

Every file below is created within the repository root established in Phase 00. Existing files are modified only where explicitly listed.

### Task 1: Implement local observability, stable telemetry fields, log redaction, and SLO snapshots

**Files:**
- Modify: `crates/observability/Cargo.toml`
- Modify: `crates/observability/src/lib.rs`
- Create: `crates/observability/src/fields.rs`
- Modify: `crates/observability/src/metrics.rs`
- Modify: `crates/observability/src/redaction.rs`
- Create: `crates/observability/src/local_service.rs`
- Create: `crates/observability/tests/observability_contract.rs`
- Modify: `apps/cryptoriskd/src/main.rs`
- Create: `proto/health/v1/health.proto`
- Modify: `crates/local-api/src/generated.rs`
- Modify: `apps/macos/GeneratedProto`
- Create: `docs/operations/observability.md`

**Interfaces:**
- Consumes: Phase 0 local observability, canonical connection/event/forecast/replay/incident identifiers, RPC authentication, daemon component boundaries, and reference SLOs
- Produces: `ObservabilityHandle`, stable metric names and label cardinality rules, structured rotating local logs, secret/raw-payload redaction, local-only metrics snapshots, and SLO evaluation records

**Implementation notes**

Metric labels are bounded enums or stable IDs; instrument symbols, raw URLs, user text, and source payloads are not unbounded metric labels. Logs hold hashes and correlation IDs, while authoritative raw payloads remain only in the WAL. No exporter binds a non-loopback address or activates without explicit local configuration.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/observability/tests/observability_contract.rs` with:

```rust
use observability::{init_test_observability, MetricName, SecretValue};

#[test]
fn secrets_and_raw_payloads_never_enter_logs() {
    let capture = init_test_observability();
    tracing::info!(session_secret = %SecretValue::new("never-log-me"), raw_payload = "{\"apiKey\":\"x\"}", incident_id = "inc-7", "connector failure");
    let output = capture.finish();
    assert!(!output.contains("never-log-me"));
    assert!(!output.contains("apiKey"));
    assert!(output.contains("inc-7"));
}

#[test]
fn required_slo_metrics_have_stable_names() {
    assert_eq!(MetricName::EventToNormalizedLatency.as_str(), "transition_event_to_normalized_seconds");
    assert_eq!(MetricName::FastFeatureToForecastLatency.as_str(), "transition_fast_feature_to_forecast_seconds");
    assert_eq!(MetricName::AbstentionTotal.as_str(), "transition_abstention_total");
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p observability --test observability_contract`

Expected: FAIL because the observability crate, redaction wrapper, and stable metric registry do not exist

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/observability/src/metrics.rs` with:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum MetricName {
    EventToNormalizedLatency,
    FastFeatureToForecastLatency,
    AbstentionTotal,
    QueueDepth,
    WalFsyncLatency,
}

impl MetricName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EventToNormalizedLatency => "transition_event_to_normalized_seconds",
            Self::FastFeatureToForecastLatency => "transition_fast_feature_to_forecast_seconds",
            Self::AbstentionTotal => "transition_abstention_total",
            Self::QueueDepth => "transition_queue_depth",
            Self::WalFsyncLatency => "transition_wal_fsync_seconds",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SloObservation {
    pub metric: MetricName,
    pub observed: f64,
    pub target: f64,
    pub sample_count: u64,
}

impl SloObservation {
    pub fn passes(&self) -> bool {
        self.sample_count > 0 && self.observed.is_finite() && self.observed <= self.target
    }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p observability --test observability_contract`

Expected: PASS for redaction, stable names, bounded labels, local bind policy, log rotation, and SLO snapshot tests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p observability && cargo run -p xtask -- observability-schema-check`

Expected: the observability suite passes and the generated metric/log field catalog exactly matches the checked-in operations document

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: all logs are structured and locally rotated; secrets and raw payloads are excluded; metrics are local-only, bounded-cardinality, and include every required latency, integrity, capacity, RPC, and resource signal.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/observability apps/cryptoriskd/src/main.rs proto/health/v1/health.proto crates/local-api/src/generated.rs apps/macos/GeneratedProto docs/operations/observability.md Cargo.toml Cargo.lock
git commit -m "feat: add local observability and SLO snapshots"
```

### Task 2: Implement the hardware-aware capacity planner and source admission controller

**Files:**
- Create: `crates/capacity/Cargo.toml`
- Create: `crates/capacity/src/lib.rs`
- Create: `crates/capacity/src/estimate.rs`
- Create: `crates/capacity/src/admission.rs`
- Create: `crates/capacity/src/profile.rs`
- Create: `crates/capacity/tests/reference_profiles.rs`
- Create: `apps/crypto-evaluate/src/capacity.rs`
- Modify: `apps/crypto-evaluate/src/main.rs`
- Create: `proto/settings/v1/settings.proto`
- Modify: `crates/local-api/src/generated.rs`
- Modify: `apps/macos/GeneratedProto`
- Create: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/Domain/Capacity.swift`
- Create: `docs/operations/capacity-planning.md`

**Interfaces:**
- Consumes: source coverage tiers, retention policy, connector message-rate observations, book memory measurements, disk state, and hardware profile
- Produces: deterministic `CapacityEstimate`, `AdmissionDecision`, downgrade plan, CLI/UI preview, and a refusal path that protects required Tier A capture when configured resources are insufficient

**Implementation notes**

The planner uses measured bytes/message and events/s where available, conservative certified defaults otherwise, and reports uncertainty. Admission happens before subscriptions are enabled. A capacity failure never silently lowers integrity semantics; it proposes an explicit tier/retention change for user approval.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/capacity/tests/reference_profiles.rs` with:

```rust
use capacity::{admit, CapacityInput, AdmissionDecision, CoverageTier};

#[test]
fn refuses_configuration_that_cannot_preserve_tier_a() {
    let input = CapacityInput::fixture()
        .with_free_disk_bytes(200 * 1024 * 1024 * 1024)
        .with_tier_a_instruments(20)
        .with_retention_days(30)
        .with_normalized_events_per_second(50_000);
    let result = admit(&input).unwrap();
    assert!(matches!(result, AdmissionDecision::Rejected { .. }));
}

#[test]
fn accepted_estimate_reports_days_and_burst_headroom() {
    let result = admit(&CapacityInput::reference_64gb()).unwrap();
    let AdmissionDecision::Accepted(estimate) = result else { panic!("reference profile must be accepted") };
    assert!(estimate.retention_days_supported >= 30.0);
    assert!(estimate.burst_headroom_ratio >= 1.25);
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p capacity --test reference_profiles`

Expected: FAIL because capacity inputs, estimates, and admission policy are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/capacity/src/admission.rs` with:

```rust
use crate::{CapacityEstimate, CapacityError, CapacityInput};

#[derive(Clone, Debug, PartialEq)]
pub enum AdmissionDecision {
    Accepted(CapacityEstimate),
    Rejected { estimate: CapacityEstimate, reasons: Vec<String> },
}

pub fn admit(input: &CapacityInput) -> Result<AdmissionDecision, CapacityError> {
    let estimate = CapacityEstimate::calculate(input)?;
    let mut reasons = Vec::new();
    if estimate.retention_days_supported < input.retention_days as f64 {
        reasons.push("configured retention exceeds disk budget".to_owned());
    }
    if estimate.peak_memory_bytes > input.memory_budget_bytes {
        reasons.push("peak book and feature state exceeds memory budget".to_owned());
    }
    if estimate.burst_headroom_ratio < 1.25 {
        reasons.push("burst headroom is below the certified minimum".to_owned());
    }
    Ok(if reasons.is_empty() { AdmissionDecision::Accepted(estimate) } else { AdmissionDecision::Rejected { estimate, reasons } })
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p capacity --test reference_profiles`

Expected: PASS for minimum/recommended profiles, uncertainty bands, overflow checks, retention calculations, and explicit downgrade/refusal cases

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p capacity && cargo run -p crypto-evaluate -- capacity --config configs/reference-64gb.toml --format json`

Expected: tests pass and the CLI emits a deterministic estimate covering bandwidth, bytes/day, CPU, memory, write rate, retention, replay cost, and quality impact

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: no accepted configuration can exceed declared memory/disk/write budgets; estimates are visible before enabling sources; reductions are explicit and never implemented as silent delta dropping.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/capacity apps/crypto-evaluate/src/capacity.rs apps/crypto-evaluate/src/main.rs proto/settings/v1/settings.proto crates/local-api/src/generated.rs apps/macos/GeneratedProto apps/macos/Packages/TransitionClient/Sources/TransitionClient/Domain/Capacity.swift docs/operations/capacity-planning.md configs/reference-64gb.toml Cargo.toml Cargo.lock
git commit -m "feat: add capacity planning and source admission"
```

### Task 3: Harden every network parser with shared limits and continuous fuzzing

**Files:**
- Create: `crates/parser-limits/Cargo.toml`
- Create: `crates/parser-limits/src/lib.rs`
- Create: `crates/parser-limits/src/json.rs`
- Create: `crates/parser-limits/src/compression.rs`
- Create: `crates/parser-limits/tests/limit_contract.rs`
- Create: `fuzz/Cargo.toml`
- Create: `fuzz/fuzz_targets/binance_message.rs`
- Create: `fuzz/fuzz_targets/bybit_message.rs`
- Create: `fuzz/fuzz_targets/kraken_message.rs`
- Create: `fuzz/fuzz_targets/deribit_message.rs`
- Create: `fuzz/corpus/README.md`
- Create: `.github/workflows/nightly-fuzz.yml`
- Create: `crates/connector-core/src/parser.rs`

**Interfaces:**
- Consumes: all certified connector parsers, source profiles, typed parser errors, decompression layer, and sanitized legal fixtures
- Produces: one enforced `ParseLimits` contract, quarantine-safe parser failures, per-venue libFuzzer targets/corpora, deterministic fuzz smoke tests, and nightly bounded fuzz jobs

**Implementation notes**

All parsing begins with byte/decompression limits before allocation-heavy deserialization. Unknown fields are retained or rejected according to each versioned source profile. A malformed record is captured by hash and parser version, then quarantined; it cannot panic the collector or leak raw secrets into logs.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/parser-limits/tests/limit_contract.rs` with:

```rust
use parser_limits::{validate_json_envelope, ParseLimitError, ParseLimits};

#[test]
fn rejects_oversized_and_over_nested_messages_before_deserialization() {
    let limits = ParseLimits { max_bytes: 64, max_depth: 3, max_string_bytes: 16, max_collection_items: 8, max_decompression_ratio: 20 };
    assert_eq!(validate_json_envelope(&vec![b'x'; 65], limits), Err(ParseLimitError::MessageTooLarge));
    assert_eq!(validate_json_envelope(br#"[[[[0]]]]"#, limits), Err(ParseLimitError::NestingTooDeep));
}

#[test]
fn decompression_bomb_is_rejected() {
    let limits = ParseLimits::production();
    assert_eq!(limits.validate_ratio(1_000_000, 100), Err(ParseLimitError::DecompressionRatioExceeded));
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p parser-limits --test limit_contract`

Expected: FAIL because shared parser/decompression limits are not implemented

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/parser-limits/src/lib.rs` with:

```rust
#[derive(Clone, Copy, Debug)]
pub struct ParseLimits {
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_string_bytes: usize,
    pub max_collection_items: usize,
    pub max_decompression_ratio: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ParseLimitError {
    #[error("message too large")] MessageTooLarge,
    #[error("nesting too deep")] NestingTooDeep,
    #[error("string too large")] StringTooLarge,
    #[error("collection too large")] CollectionTooLarge,
    #[error("decompression ratio exceeded")] DecompressionRatioExceeded,
}

impl ParseLimits {
    pub fn validate_ratio(self, expanded: usize, compressed: usize) -> Result<(), ParseLimitError> {
        if compressed == 0 || expanded / compressed.max(1) > self.max_decompression_ratio {
            Err(ParseLimitError::DecompressionRatioExceeded)
        } else {
            Ok(())
        }
    }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p parser-limits --test limit_contract`

Expected: PASS for byte, depth, string, collection, numeric, UTF-8, decompression, timeout, unknown-field, and quarantine behavior

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p parser-limits -p connector-binance -p connector-bybit -p connector-kraken -p connector-deribit && for target in binance_message bybit_message kraken_message deribit_message; do cargo fuzz run "$target" -- -max_total_time=60 -timeout=2; done`

Expected: all parser tests pass and each venue fuzz target completes the smoke budget without crash, OOM, timeout, or invariant violation

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: all external bytes pass the shared limits before expensive allocation; fuzz corpora contain no proprietary or secret data; nightly jobs are bounded, reproducible, and upload only minimized crash artifacts.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/parser-limits crates/connector-core/src/parser.rs fuzz .github/workflows/nightly-fuzz.yml Cargo.toml Cargo.lock
git commit -m "test: harden and fuzz network parsers"
```

### Task 4: Add deterministic concurrency, cancellation, and shutdown verification

**Files:**
- Create: `crates/concurrency-testkit/Cargo.toml`
- Create: `crates/concurrency-testkit/src/lib.rs`
- Create: `crates/concurrency-testkit/src/shutdown.rs`
- Create: `crates/concurrency-testkit/src/bounded.rs`
- Create: `crates/concurrency-testkit/tests/shutdown_order.rs`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/loom_book_resync.rs`
- Create: `crates/system-tests/tests/loom_model_hot_swap.rs`
- Create: `crates/system-tests/tests/loom_rpc_teardown.rs`
- Create: `.github/workflows/hardening.yml`
- Create: `apps/cryptoriskd/src/supervisor.rs`

**Interfaces:**
- Consumes: daemon task graph, bounded channels, connector resynchronization, WAL/checkpoint coordination, model registry readers, and RPC subscription lifecycle
- Produces: `ShutdownGate`, deterministic bounded-channel harnesses, Loom models for critical state machines, cancellation propagation assertions, and a CI gate that detects leaked tasks or locks held across awaits

**Implementation notes**

Loom models remain deliberately small and model state transitions rather than full connectors. Production code exposes synchronization primitives through narrow wrappers so tests exercise the same atomics/state transitions. Shutdown has one declared partial order and a bounded deadline.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/concurrency-testkit/tests/shutdown_order.rs` with:

```rust
use concurrency_testkit::{Component, ShutdownGate};

#[tokio::test]
async fn shutdown_drains_forecasts_before_closing_storage() {
    let gate = ShutdownGate::new();
    gate.mark_stopped(Component::Collectors).await.unwrap();
    gate.mark_stopped(Component::Features).await.unwrap();
    gate.mark_stopped(Component::Models).await.unwrap();
    assert!(gate.mark_stopped(Component::Storage).await.is_ok());
}

#[tokio::test]
async fn storage_cannot_stop_before_model_and_forecast_drains() {
    let gate = ShutdownGate::new();
    assert!(gate.mark_stopped(Component::Storage).await.is_err());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p concurrency-testkit --test shutdown_order`

Expected: FAIL because the shutdown partial-order contract and deterministic test harness do not exist

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/concurrency-testkit/src/shutdown.rs` with:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Component { Collectors, Features, Models, Storage, Rpc }

#[derive(Default)]
pub struct ShutdownGate { stopped: tokio::sync::Mutex<std::collections::BTreeSet<Component>> }

impl ShutdownGate {
    pub fn new() -> Self { Self::default() }

    pub async fn mark_stopped(&self, component: Component) -> Result<(), &'static str> {
        let mut stopped = self.stopped.lock().await;
        let allowed = match component {
            Component::Collectors => true,
            Component::Features => stopped.contains(&Component::Collectors),
            Component::Models => stopped.contains(&Component::Features),
            Component::Storage => stopped.contains(&Component::Models),
            Component::Rpc => stopped.contains(&Component::Storage),
        };
        if !allowed { return Err("shutdown order violation"); }
        stopped.insert(component);
        Ok(())
    }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p concurrency-testkit --test shutdown_order`

Expected: PASS for shutdown ordering, cancellation safety, bounded backpressure, reconnect race, checkpoint atomicity, model hot-swap, and subscription teardown tests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p concurrency-testkit && cargo test -p system-tests --test loom_book_resync --test loom_model_hot_swap --test loom_rpc_teardown && cargo run -p xtask -- task-leak-check`

Expected: all deterministic schedules pass and the daemon integration test exits with zero unjoined tasks, zero active subscriptions, and all durable writes flushed

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: every spawned task is named, owned, cancellable, and joined; no critical mutex guard crosses an awaited network operation; required streams invalidate instead of skipping data on overflow.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/concurrency-testkit crates/system-tests/Cargo.toml crates/system-tests/tests/loom_book_resync.rs crates/system-tests/tests/loom_model_hot_swap.rs crates/system-tests/tests/loom_rpc_teardown.rs apps/cryptoriskd/src/supervisor.rs .github/workflows/hardening.yml Cargo.toml Cargo.lock
git commit -m "test: verify concurrency and shutdown invariants"
```

### Task 5: Build the fault-injection and chaos scenario runner

**Files:**
- Create: `crates/chaos/Cargo.toml`
- Create: `crates/chaos/src/lib.rs`
- Create: `crates/chaos/src/scenario.rs`
- Create: `crates/chaos/src/injector.rs`
- Create: `crates/chaos/src/oracle.rs`
- Create: `apps/crypto-replay/src/chaos.rs`
- Modify: `apps/crypto-replay/src/main.rs`
- Create: `tests/chaos/scenarios/reference-matrix.yaml`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/chaos_matrix.rs`
- Create: `docs/operations/chaos-testing.md`

**Interfaces:**
- Consumes: deterministic replay, source/clock/storage/model fault hooks, health-state contracts, incident records, and recovery invariants from all prior phases
- Produces: versioned chaos scenarios, deterministic seeded injection, expected-health/recovery oracles, machine-readable reports, and a complete required-failure coverage gate

**Implementation notes**

Fault hooks compile only for test/replay builds and cannot be activated through production RPC. Each scenario declares the injection point, exact trigger, expected health transitions, forbidden outputs, recovery deadline, and final digest. A scenario passes only when both degradation and recovery are correct.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/system-tests/tests/chaos_matrix.rs` with:

```rust
use chaos::{run_scenario, ScenarioCatalog};

#[tokio::test]
async fn required_fault_matrix_has_an_executable_oracle() {
    let catalog = ScenarioCatalog::load("tests/chaos/scenarios/reference-matrix.yaml").unwrap();
    for required in ["packet_loss", "sequence_gap", "checksum_corruption", "clock_jump", "disk_full", "wal_tail_truncation", "model_nan", "checkpoint_kill", "sleep_wake"] {
        assert!(catalog.contains(required), "missing scenario {required}");
        let report = run_scenario(catalog.get(required).unwrap()).await.unwrap();
        assert!(report.oracle_passed, "{}: {:?}", required, report.failures);
    }
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p system-tests --test chaos_matrix`

Expected: FAIL because the scenario catalog, injectors, and recovery oracles are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/chaos/src/scenario.rs` with:

```rust
#[derive(Clone, Debug, serde::Deserialize)]
pub struct Scenario {
    pub id: String,
    pub seed: u64,
    pub injection: Injection,
    pub expected_states: Vec<String>,
    pub forbidden_outputs: Vec<String>,
    pub recovery_deadline_ms: u64,
    pub expected_final_digest: String,
}

impl Scenario {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.expected_states.is_empty() || self.recovery_deadline_ms == 0 {
            return Err("scenario is missing an executable oracle".to_owned());
        }
        if self.forbidden_outputs.is_empty() {
            return Err("scenario must declare forbidden outputs".to_owned());
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p system-tests --test chaos_matrix`

Expected: PASS for the complete required fault matrix, expected degradation, forbidden forecast/alert checks, bounded recovery, and deterministic final digests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p crypto-replay -- chaos --catalog tests/chaos/scenarios/reference-matrix.yaml --all --report target/chaos-report.json`

Expected: the runner reports every required injection as passed and emits incident IDs, state transitions, recovery durations, and final replay hashes

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: test-only fault hooks cannot be enabled in release builds; no scenario accepts silent corruption, stale trusted books, unpersisted alerts, or recovery without invariant verification.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/chaos apps/crypto-replay/src/chaos.rs apps/crypto-replay/src/main.rs tests/chaos docs/operations/chaos-testing.md Cargo.toml Cargo.lock
git commit -m "test: add deterministic chaos and recovery matrix"
```

### Task 6: Complete WAL/checkpoint recovery, encrypted backup, restore, and disaster-recovery verification

**Files:**
- Create: `crates/recovery/Cargo.toml`
- Create: `crates/recovery/src/lib.rs`
- Create: `crates/recovery/src/wal_repair.rs`
- Create: `crates/recovery/src/checkpoint.rs`
- Create: `crates/recovery/src/backup.rs`
- Create: `crates/recovery/src/restore.rs`
- Create: `crates/recovery/tests/crash_recovery.rs`
- Create: `crates/recovery/tests/backup_roundtrip.rs`
- Create: `apps/crypto-evaluate/src/recovery.rs`
- Modify: `apps/crypto-evaluate/src/main.rs`
- Create: `docs/operations/disaster-recovery.md`
- Create: `apps/cryptoriskd/src/startup.rs`

**Interfaces:**
- Consumes: Phase 1 WAL framing/checkpoints, metadata/audit/model registries, Keychain-backed secret provider, schema migrations, and deterministic replay digests
- Produces: tail-safe WAL repair, atomic compatible checkpoint selection, encrypted manifest-driven backups, validated restore staging, RPO/RTO reports, and end-to-end disaster-recovery commands

**Implementation notes**

Repair may truncate only an incomplete final record after validating all prior hashes. Restores occur into a new staging directory, validate archive encryption, file hashes, signatures, schema compatibility, path traversal, and audit continuity, then atomically switch only after a dry-run replay passes. Keychain secrets are never included in the archive.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/recovery/tests/crash_recovery.rs` with:

```rust
use recovery::{recover, RecoveryInput};

#[test]
fn truncates_only_an_incomplete_wal_tail_and_replays_from_verified_checkpoint() {
    let input = RecoveryInput::fixture_killed_during_checkpoint();
    let report = recover(input).unwrap();
    assert_eq!(report.truncated_records, 0);
    assert!(report.truncated_tail_bytes > 0);
    assert!(report.checkpoint_verified);
    assert_eq!(report.recovered_digest, report.expected_digest);
}

#[test]
fn refuses_corruption_before_the_final_record_boundary() {
    assert!(recover(RecoveryInput::fixture_mid_segment_corruption()).is_err());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p recovery --test crash_recovery`

Expected: FAIL because the end-to-end recovery coordinator and strict repair policy are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/recovery/src/wal_repair.rs` with:

```rust
pub fn repair_tail(bytes: &[u8], scan: &WalScan) -> Result<RepairedTail, RecoveryError> {
    if let Some(corruption) = scan.corruption_before_final_boundary() {
        return Err(RecoveryError::NonTailCorruption { offset: corruption });
    }
    let valid_len = scan.last_complete_record_end();
    Ok(RepairedTail {
        bytes: bytes[..valid_len].to_vec(),
        truncated_tail_bytes: bytes.len().saturating_sub(valid_len),
        last_record_hash: scan.last_complete_record_hash(),
    })
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p recovery --test crash_recovery --test backup_roundtrip`

Expected: PASS for checkpoint-kill, incomplete-tail, incompatible checkpoint, encrypted backup, traversal attack, wrong key, interrupted restore, and exact digest round-trip tests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p crypto-evaluate -- recovery-drill --fixture tests/fixtures/recovery/reference --rpo-ms 250 --rto-seconds 60 --report target/recovery-drill.json`

Expected: the drill recovers to the declared digest, reports an RPO no worse than 250 ms and ordinary bounded-WAL RTO below 60 s on the reference machine, and verifies all audit/model/alert records

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: backup archives exclude Keychain secrets and mutable cache files; restore never overwrites the live directory before full verification; immutable audit records are not auto-deleted under pressure.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/recovery apps/crypto-evaluate/src/recovery.rs apps/crypto-evaluate/src/main.rs apps/cryptoriskd/src/startup.rs docs/operations/disaster-recovery.md tests/fixtures/recovery Cargo.toml Cargo.lock
git commit -m "feat: add verified disaster recovery and encrypted backup"
```

### Task 7: Implement disk, clock, sleep/wake, and network failure state machines

**Files:**
- Create: `crates/runtime-health/Cargo.toml`
- Create: `crates/runtime-health/src/lib.rs`
- Create: `crates/runtime-health/src/disk.rs`
- Create: `crates/runtime-health/src/clock.rs`
- Create: `crates/runtime-health/src/power.rs`
- Create: `crates/runtime-health/src/network.rs`
- Create: `crates/runtime-health/tests/failure_states.rs`
- Create: `apps/cryptoriskd/src/runtime_health.rs`
- Modify: `proto/health/v1/health.proto`
- Modify: `crates/local-api/src/generated.rs`
- Modify: `apps/macos/GeneratedProto`
- Create: `docs/operations/runtime-failure-policy.md`

**Interfaces:**
- Consumes: disk budget, monotonic/wall clocks, connector supervisor, macOS power/network notifications, persistence gate, and quality/incident engine
- Produces: typed disk/clock/power/network state machines, explicit forecast-publication gates, coverage reductions, resynchronization triggers, incidents, and bounded recovery transitions

**Implementation notes**

Critical storage pressure prevents new trusted forecasts when audit persistence cannot be guaranteed. Wall-clock steps never reorder receive events because monotonic sequence remains authoritative. Wake or interface change forces venue/session validation before trusted books resume.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/runtime-health/tests/failure_states.rs` with:

```rust
use runtime_health::{DiskEvent, DiskState, RuntimeGate};

#[test]
fn emergency_disk_state_blocks_new_forecasts_but_preserves_audit() {
    let mut gate = RuntimeGate::healthy();
    gate.apply(DiskEvent::FreeBytes(512 * 1024 * 1024));
    assert_eq!(gate.disk_state(), DiskState::ReadOnlyEmergency);
    assert!(!gate.may_publish_forecast());
    assert!(gate.may_append_existing_incident_audit());
}

#[test]
fn wall_clock_step_marks_latency_unreliable_without_reordering_monotonic_events() {
    let mut gate = RuntimeGate::healthy();
    gate.observe_clock_step(-120_000_000_000);
    assert!(!gate.cross_source_latency_reliable());
    assert!(gate.monotonic_ordering_authoritative());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p runtime-health --test failure_states`

Expected: FAIL because operational failure states and forecast gates are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/runtime-health/src/disk.rs` with:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiskState { Normal, Warning, Critical, ReadOnlyEmergency }

#[derive(Clone, Copy, Debug)]
pub struct DiskThresholds { pub warning: u64, pub critical: u64, pub emergency: u64 }

impl DiskThresholds {
    pub fn classify(self, free: u64) -> DiskState {
        if free <= self.emergency { DiskState::ReadOnlyEmergency }
        else if free <= self.critical { DiskState::Critical }
        else if free <= self.warning { DiskState::Warning }
        else { DiskState::Normal }
    }
}

impl DiskState {
    pub const fn may_publish_forecast(self) -> bool { !matches!(self, Self::ReadOnlyEmergency) }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p runtime-health --test failure_states`

Expected: PASS for threshold hysteresis, retention/coverage actions, forecast gating, wall-clock corrections, sleep/wake, interface changes, repeated flaps, and incident recovery

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p runtime-health && cargo run -p crypto-replay -- chaos --scenario runtime_failure_matrix`

Expected: all state-machine and integrated fault scenarios pass with the exact declared health state, user-visible message, recovery action, and no duplicate alert delivery

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: state transitions are monotonic where safety requires it and use hysteresis to avoid flapping; optional source loss degrades evidence while required integrity/persistence loss fails closed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/runtime-health apps/cryptoriskd/src/runtime_health.rs proto/health/v1/health.proto crates/local-api/src/generated.rs apps/macos/GeneratedProto docs/operations/runtime-failure-policy.md Cargo.toml Cargo.lock
git commit -m "feat: enforce runtime failure state machines"
```

### Task 8: Enforce local security, privacy, audit-chain, and outbound-network policy

**Files:**
- Create: `crates/security-policy/Cargo.toml`
- Create: `crates/security-policy/src/lib.rs`
- Create: `crates/security-policy/src/outbound.rs`
- Create: `crates/security-policy/src/files.rs`
- Create: `crates/security-policy/src/audit_chain.rs`
- Create: `crates/security-policy/src/code_identity.rs`
- Create: `crates/security-policy/tests/security_contract.rs`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/no_secret_output.rs`
- Create: `crates/system-tests/tests/outbound_allowlist.rs`
- Create: `apps/crypto-evaluate/src/security.rs`
- Modify: `apps/crypto-evaluate/src/main.rs`
- Modify: `apps/cryptoriskd/src/main.rs`
- Modify: `apps/macos/CuspObservatory/Daemon/DaemonSupervisor.swift`
- Create: `docs/security/threat-model.md`
- Create: `docs/security/privacy-model.md`

**Interfaces:**
- Consumes: Phase 0 challenge-response RPC, Keychain secrets, source destination catalog, configuration/model/forecast/alert/export records, and signed code identities
- Produces: deny-by-default outbound policy, user-private directory enforcement, client/daemon code-identity checks, hash-linked audit events, secret-output regression tests, and a machine-readable local privacy report

**Implementation notes**

Only declared source endpoints, optional user-enabled webhooks, and Apple notarization/update endpoints used by release tooling may be contacted. Runtime application code has no generic telemetry destination. Audit chain entries include prior hash, canonical event hash, sequence, actor, and monotonic/wall timestamps; verification failure is release-blocking and user-visible.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/security-policy/tests/security_contract.rs` with:

```rust
use security_policy::{AuditChain, AuditEvent, OutboundPolicy};

#[test]
fn outbound_is_denied_unless_destination_and_category_are_declared() {
    let policy = OutboundPolicy::from_fixture();
    assert!(policy.authorize("wss://stream.binance.com", "market_data").is_ok());
    assert!(policy.authorize("https://telemetry.invalid", "analytics").is_err());
}

#[test]
fn audit_chain_detects_reordering_or_mutation() {
    let mut chain = AuditChain::new();
    chain.append(AuditEvent::fixture("model_promoted")).unwrap();
    chain.append(AuditEvent::fixture("alert_rule_changed")).unwrap();
    let mut entries = chain.into_entries();
    entries.swap(0, 1);
    assert!(AuditChain::verify(&entries).is_err());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p security-policy --test security_contract`

Expected: FAIL because the centralized outbound policy and auditable hash chain do not exist

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/security-policy/src/audit_chain.rs` with:

```rust
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub sequence: u64,
    pub previous_hash: [u8; 32],
    pub event_hash: [u8; 32],
    pub chain_hash: [u8; 32],
}

pub fn next_hash(previous: [u8; 32], sequence: u64, event: &[u8]) -> [u8; 32] {
    let event_hash = *blake3::hash(event).as_bytes();
    let mut input = Vec::with_capacity(72);
    input.extend_from_slice(&previous);
    input.extend_from_slice(&sequence.to_be_bytes());
    input.extend_from_slice(&event_hash);
    *blake3::hash(&input).as_bytes()
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p security-policy --test security_contract && cargo test -p system-tests --test no_secret_output --test outbound_allowlist`

Expected: PASS for outbound allowlist, webhook consent, file modes, secure temporary files, Keychain redaction, code identity, audit mutation/reorder/truncation, and privacy-report tests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p crypto-evaluate -- security-audit --config configs/default.toml --report target/security-audit.json && cargo run -p xtask -- secret-scan`

Expected: the security audit reports no undeclared destinations, weak permissions, plaintext secrets, broken audit links, trading/withdrawal scopes, or automatic data export

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: runtime permits no remote analytics/hosted inference; every external destination is categorized and visible in the app; local-process trust requires permissions, session challenge, and configured code identity rather than loopback location alone.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/security-policy crates/system-tests/Cargo.toml crates/system-tests/tests/no_secret_output.rs crates/system-tests/tests/outbound_allowlist.rs apps/crypto-evaluate/src/security.rs apps/crypto-evaluate/src/main.rs apps/cryptoriskd/src/main.rs apps/macos/CuspObservatory/Daemon/DaemonSupervisor.swift docs/security/threat-model.md docs/security/privacy-model.md Cargo.toml Cargo.lock
git commit -m "security: enforce local privacy and audit policy"
```

### Task 9: Create signed model and release artifact tooling with offline key separation

**Files:**
- Create: `crates/artifact-signing/Cargo.toml`
- Create: `crates/artifact-signing/src/lib.rs`
- Create: `crates/artifact-signing/src/canonical.rs`
- Create: `crates/artifact-signing/src/verify.rs`
- Create: `crates/artifact-signing/tests/signature_contract.rs`
- Modify: `Cargo.toml`
- Create: `apps/release-cli/Cargo.toml`
- Create: `apps/release-cli/src/main.rs`
- Create: `apps/release-cli/src/sign.rs`
- Create: `models/trusted-keys.json`
- Create: `docs/security/signing-key-policy.md`
- Create: `crates/model-registry/src/verification.rs`

**Interfaces:**
- Consumes: canonical model package manifest, release manifest schema, registry transitions, Ed25519 verification, and user/release channel compatibility fields
- Produces: domain-separated canonical signatures, trusted-key rotation/revocation records, offline signing command, runtime verifier, tamper tests, and an explicit rule that private signing keys never enter the repository or CI logs

**Implementation notes**

The same key is never used for model, application-manifest, and emergency advisory domains. Signing input is canonical bytes with a domain separator and schema version. Verification checks key status, channel, validity interval, manifest hash, payload hashes, and runtime compatibility before registry activation.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/artifact-signing/tests/signature_contract.rs` with:

```rust
use artifact_signing::{sign_for_test, verify, ArtifactDomain, SignedBytes};

#[test]
fn domain_separation_prevents_reusing_model_signature_as_release_signature() {
    let signed = sign_for_test(ArtifactDomain::ModelPackage, b"manifest");
    assert!(verify(ArtifactDomain::ModelPackage, &signed).is_ok());
    assert!(verify(ArtifactDomain::ReleaseManifest, &signed).is_err());
}

#[test]
fn any_payload_or_manifest_mutation_is_rejected() {
    let mut signed: SignedBytes = sign_for_test(ArtifactDomain::ModelPackage, b"manifest");
    signed.payload[0] ^= 1;
    assert!(verify(ArtifactDomain::ModelPackage, &signed).is_err());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p artifact-signing --test signature_contract`

Expected: FAIL because domain-separated canonical signing and trusted-key status are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/artifact-signing/src/lib.rs` with:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactDomain { ModelPackage, ReleaseManifest, SecurityAdvisory }

impl ArtifactDomain {
    pub const fn separator(self) -> &'static [u8] {
        match self {
            Self::ModelPackage => b"cusp-observatory:model-package:v1\0",
            Self::ReleaseManifest => b"cusp-observatory:release-manifest:v1\0",
            Self::SecurityAdvisory => b"cusp-observatory:security-advisory:v1\0",
        }
    }
}

pub fn signing_message(domain: ArtifactDomain, canonical_payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(domain.separator().len() + canonical_payload.len());
    message.extend_from_slice(domain.separator());
    message.extend_from_slice(canonical_payload);
    message
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p artifact-signing --test signature_contract`

Expected: PASS for canonicalization, domain separation, mutation, wrong key, revoked key, expired key, channel mismatch, rotation, and model-registry activation tests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p release-cli -- verify-fixtures models/public-test-artifacts --trusted-keys models/trusted-keys.json`

Expected: every public fixture verifies or is rejected for its declared reason, and the command confirms no private key material is present under version control

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: signing commands accept private keys only through an inherited file descriptor or protected Keychain reference; key IDs and public keys are reviewable; activation remains impossible after signature or compatibility failure.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/artifact-signing apps/release-cli models/trusted-keys.json docs/security/signing-key-policy.md crates/model-registry/src/verification.rs Cargo.toml Cargo.lock
git commit -m "security: sign and verify model and release artifacts"
```

### Task 10: Establish reference-load benchmarks, latency regression gates, profiling, and 24-hour soak testing

**Files:**
- Modify: `Cargo.toml`
- Create: `apps/load-generator/Cargo.toml`
- Create: `apps/load-generator/src/main.rs`
- Create: `apps/load-generator/src/profile.rs`
- Create: `tests/performance/reference-load.toml`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/latency_gate.rs`
- Create: `crates/system-tests/tests/soak_assertions.rs`
- Create: `crates/performance-testkit/Cargo.toml`
- Create: `crates/performance-testkit/src/lib.rs`
- Create: `crates/performance-testkit/benches/orderbook.rs`
- Create: `crates/performance-testkit/benches/feature_pipeline.rs`
- Create: `crates/performance-testkit/benches/forecast.rs`
- Create: `scripts/run-soak.sh`
- Create: `.github/workflows/nightly-soak.yml`
- Create: `docs/operations/performance-methodology.md`

**Interfaces:**
- Consumes: deterministic source generator/replay, observability histograms, capacity profile, Tier A/B coverage, fast/structural forecast cadence, Swift UI stream simulator, and reference machine declaration
- Produces: 50k sustained/250k burst workload generator, Criterion microbenchmarks, end-to-end percentile gates, allocation/disk/queue reports, concurrent replay/UI load, profiling recipes, and 24-hour soak pass/fail evidence

**Implementation notes**

The load generator emits canonical events with realistic venue/instrument distributions and configurable reorder/reconnect storms. Correctness hashes and integrity incidents are checked alongside latency; a fast but corrupted run fails. Performance baselines are machine-profile-specific and changes require an attached comparison report.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/system-tests/tests/latency_gate.rs` with:

```rust
use performance_testkit::{run_reference_load, ReferenceTargets};

#[tokio::test]
async fn reference_load_meets_integrity_and_latency_targets() {
    let report = run_reference_load("tests/performance/reference-load.toml").await.unwrap();
    let targets = ReferenceTargets::production();
    assert_eq!(report.silent_integrity_failures, 0);
    assert!(report.event_to_normalized_p99_ms < targets.event_to_normalized_p99_ms);
    assert!(report.normalized_to_fast_feature_p99_ms < targets.normalized_to_fast_feature_p99_ms);
    assert!(report.fast_feature_to_forecast_p99_ms < targets.fast_feature_to_forecast_p99_ms);
    assert!(report.max_required_queue_loss == 0);
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p system-tests --test latency_gate --no-run`

Expected: FAIL because the reference load generator and performance report types are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `apps/load-generator/src/profile.rs` with:

```rust
#[derive(Clone, Debug, serde::Deserialize)]
pub struct LoadProfile {
    pub sustained_events_per_second: u64,
    pub burst_events_per_second: u64,
    pub burst_seconds: u64,
    pub tier_a_instruments: usize,
    pub tier_b_instruments: usize,
    pub concurrent_replay: bool,
    pub ui_subscribers: usize,
}

impl LoadProfile {
    pub fn validate_reference(&self) -> Result<(), String> {
        if self.sustained_events_per_second < 50_000 || self.burst_events_per_second < 250_000 || self.burst_seconds < 30 {
            return Err("profile is below the production reference load".to_owned());
        }
        if self.tier_a_instruments < 20 || self.tier_b_instruments < 100 {
            return Err("instrument coverage is below the production reference load".to_owned());
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p system-tests --test latency_gate && cargo bench -p performance-testkit --bench orderbook --bench feature_pipeline --bench forecast -- --noplot`

Expected: PASS for integrity and reference p99 targets; Criterion comparisons stay inside configured regression budgets

- [ ] **Step 5: Run the subsystem verification command**

Run: `scripts/run-soak.sh --profile tests/performance/reference-load.toml --duration 24h --report target/soak-report.json`

Expected: the candidate completes 24 hours with zero silent gaps, zero required-stream loss, bounded memory/file descriptors/queues, valid recovery after injected reconnects, and all declared p99/UI/resource thresholds passing

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: the exact hardware, OS, power mode, build flags, source seed, model packages, and toolchain hashes are included in every report; debug endpoints are absent from production builds.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add Cargo.toml Cargo.lock apps/load-generator crates/performance-testkit crates/system-tests/Cargo.toml crates/system-tests/tests/latency_gate.rs crates/system-tests/tests/soak_assertions.rs tests/performance scripts/run-soak.sh .github/workflows/nightly-soak.yml docs/operations/performance-methodology.md
git commit -m "perf: add reference load and soak release gates"
```

### Task 11: Persist immutable live shadow forecasts and point-in-time outcome joins

**Files:**
- Create: `crates/shadow-ledger/Cargo.toml`
- Create: `crates/shadow-ledger/src/lib.rs`
- Create: `crates/shadow-ledger/src/writer.rs`
- Create: `crates/shadow-ledger/src/outcomes.rs`
- Create: `crates/shadow-ledger/src/consistency.rs`
- Create: `crates/shadow-ledger/tests/immutable_shadow.rs`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/offline_live_consistency.rs`
- Create: `apps/cryptoriskd/src/forecast_pipeline.rs`
- Create: `docs/research/shadow-protocol.md`

**Interfaces:**
- Consumes: persist-before-alert forecast records, feature/model/calibration/evidence identifiers, point-in-time labels, finality/revision policy, replay pipeline, and model package signatures
- Produces: append-only shadow forecast ledger, delayed outcome linker, immutable prediction-time evidence, finality-aware revisions, offline-versus-live consistency report, and model-blind outcome generation

**Implementation notes**

Forecast rows are written before their horizon begins and never updated in place. Outcome rows refer to forecast IDs and label versions and may progress from provisional to final through separate revision records. Evaluation uses the data available at each as-known-at boundary and cannot regenerate features with later labels or source revisions.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/shadow-ledger/tests/immutable_shadow.rs` with:

```rust
use shadow_ledger::{ShadowLedger, ShadowRecord};

#[test]
fn forecast_payload_is_append_only_and_cannot_be_replaced_after_outcome() {
    let ledger = ShadowLedger::in_memory().unwrap();
    let record = ShadowRecord::fixture();
    ledger.append_forecast(&record).unwrap();
    assert!(ledger.append_forecast(&record.with_probability(0.99)).is_err());
    ledger.append_outcome(record.forecast_id, record.label_version, true).unwrap();
    assert_eq!(ledger.forecast(record.forecast_id).unwrap(), record);
}

#[test]
fn outcome_cannot_be_joined_before_label_availability() {
    let ledger = ShadowLedger::in_memory().unwrap();
    assert!(ledger.append_outcome_before_available_time_for_test().is_err());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p shadow-ledger --test immutable_shadow`

Expected: FAIL because an append-only shadow ledger and delayed outcome join do not exist

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/shadow-ledger/src/writer.rs` with:

```rust
pub fn append_forecast(&self, record: &ShadowRecord) -> Result<(), ShadowError> {
    record.validate_point_in_time()?;
    let bytes = record.canonical_bytes()?;
    let hash = *blake3::hash(&bytes).as_bytes();
    self.parquet_writer.append_once(record.forecast_id, &bytes, hash)?;
    self.audit.append("shadow_forecast_persisted", record.forecast_id, hash)?;
    Ok(())
}

pub fn append_outcome(&self, outcome: &OutcomeRecord, now_ns: i128) -> Result<(), ShadowError> {
    if now_ns < outcome.available_at_ns { return Err(ShadowError::OutcomeNotYetAvailable); }
    self.parquet_writer.append_outcome_revision(outcome)?;
    Ok(())
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p shadow-ledger --test immutable_shadow && cargo test -p system-tests --test offline_live_consistency`

Expected: PASS for append-only identity, availability cutoffs, provisional/final revisions, censoring, source revision, replay equivalence, and failed-write-before-alert tests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p shadow-reporter -- verify-ledger --root data/shadow --audit data/metadata/audit.sqlite --report target/shadow-integrity.json`

Expected: the ledger has no duplicate IDs, mutable prediction rows, missing model/feature/calibration/evidence hashes, impossible availability times, or alert records preceding forecast persistence

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: shadow collection never changes user-facing probabilities or model behavior; outcome generation is isolated from candidate predictions; raw source/feature revisions remain point-in-time traceable.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/shadow-ledger crates/system-tests/Cargo.toml crates/system-tests/tests/offline_live_consistency.rs apps/cryptoriskd/src/forecast_pipeline.rs docs/research/shadow-protocol.md Cargo.toml Cargo.lock
git commit -m "feat: add immutable live shadow forecast ledger"
```

### Task 12: Generate drift, calibration, subgroup, alert-outcome, and offline/live consistency reports

**Files:**
- Create: `crates/model-monitor/Cargo.toml`
- Create: `crates/model-monitor/src/lib.rs`
- Create: `crates/model-monitor/src/drift.rs`
- Create: `crates/model-monitor/src/calibration.rs`
- Create: `crates/model-monitor/src/alerts.rs`
- Create: `crates/model-monitor/src/report.rs`
- Create: `crates/model-monitor/tests/monitoring_metrics.rs`
- Modify: `Cargo.toml`
- Create: `apps/shadow-reporter/Cargo.toml`
- Create: `apps/shadow-reporter/src/main.rs`
- Create: `.github/workflows/shadow-report.yml`
- Create: `docs/research/model-monitoring.md`

**Interfaces:**
- Consumes: immutable shadow ledger, model cards, validation fold populations, base rates, alert lifecycle/outcomes, quality/availability states, and reference feature distributions
- Produces: versioned local monitoring report with feature/label/base-rate drift, calibration intercept/slope/ECE/Brier/log loss, event counts, subgroup/regime metrics, alert burden/lead time, OOD/abstention, and shadow/offline parity

**Implementation notes**

The report always includes sample/event counts and uncertainty; it never treats absence of events as evidence of good calibration. Drift thresholds are per feature family and population. A report can recommend review or suspension, but only the promotion gate changes registry state.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/model-monitor/tests/monitoring_metrics.rs` with:

```rust
use model_monitor::{evaluate, MonitoringInput};

#[test]
fn report_detects_miscalibration_and_feature_distribution_shift() {
    let input = MonitoringInput::fixture_shifted_and_overconfident();
    let report = evaluate(&input).unwrap();
    assert!(report.calibration.slope < 0.8);
    assert!(report.feature_drift.iter().any(|d| d.review_required));
    assert!(report.event_count > 0);
}

#[test]
fn zero_event_window_is_marked_insufficient_not_passing() {
    let report = evaluate(&MonitoringInput::fixture_zero_events()).unwrap();
    assert_eq!(report.status.as_str(), "insufficient_evidence");
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p model-monitor --test monitoring_metrics`

Expected: FAIL because the monitoring metrics and evidence-status policy are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/model-monitor/src/report.rs` with:

```rust
#[derive(Clone, Debug, serde::Serialize)]
pub enum MonitoringStatus { Pass, ReviewRequired, Suspend, InsufficientEvidence }

pub fn status(event_count: u64, minimum_events: u64, severe_drift: bool, calibration_failed: bool) -> MonitoringStatus {
    if event_count < minimum_events { MonitoringStatus::InsufficientEvidence }
    else if calibration_failed { MonitoringStatus::Suspend }
    else if severe_drift { MonitoringStatus::ReviewRequired }
    else { MonitoringStatus::Pass }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p model-monitor --test monitoring_metrics`

Expected: PASS for metrics, confidence intervals, rare/zero-event windows, subgroup minimums, drift thresholds, alert budgets, shadow/offline parity, and signed report hashes

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p shadow-reporter -- evaluate --ledger data/shadow --models data/models --output target/shadow-report && cargo run -p xtask -- validate-model-report target/shadow-report/report.json`

Expected: the reporter creates deterministic JSON, Parquet tables, and a local HTML summary with all required metrics, populations, counts, caveats, artifact hashes, and no network access

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: monitoring cannot silently aggregate across incompatible model/label/calibration versions; every chart/table states population, window, finality, baseline, and uncertainty; zero events never produce a green pass.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/model-monitor apps/shadow-reporter .github/workflows/shadow-report.yml docs/research/model-monitoring.md Cargo.toml Cargo.lock
git commit -m "feat: add shadow drift and calibration reporting"
```

### Task 13: Automate evidence-based model promotion with independent approval and revocation

**Files:**
- Create: `crates/promotion-gate/Cargo.toml`
- Create: `crates/promotion-gate/src/lib.rs`
- Create: `crates/promotion-gate/src/policy.rs`
- Create: `crates/promotion-gate/src/evidence.rs`
- Create: `crates/promotion-gate/src/approval.rs`
- Create: `crates/promotion-gate/tests/promotion_policy.rs`
- Create: `apps/release-cli/src/promote.rs`
- Modify: `apps/release-cli/src/main.rs`
- Create: `crates/model-registry/src/state.rs`
- Create: `docs/governance/model-promotion.md`

**Interfaces:**
- Consumes: signed model package, nested walk-forward report, shadow monitoring report, cusp ablation when applicable, performance/parity reports, registry states, and trusted reviewer keys
- Produces: machine-evaluated promotion decision, explicit unmet gates, independent reviewer signature, no-self-approval rule, expiry/review date, atomic registry transition, and immediate revocation/fallback workflow

**Implementation notes**

Promotion is a state transition over immutable evidence hashes. The model author may submit but cannot provide the independent approval signature. The gate refuses missing regimes/events, calibration failure, best-baseline underperformance, incompatibility, unsigned artifacts, severe unresolved drift, or expired evidence. Autonomous retraining/promotion remains excluded.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/promotion-gate/tests/promotion_policy.rs` with:

```rust
use promotion_gate::{evaluate, Approval, PromotionBundle, PromotionStatus};

#[test]
fn author_cannot_self_approve_a_production_promotion() {
    let mut bundle = PromotionBundle::passing_fixture();
    bundle.approval = Approval::signed_by(bundle.author_key_id.clone());
    assert_eq!(evaluate(&bundle).unwrap().status, PromotionStatus::Rejected);
}

#[test]
fn passing_bundle_requires_all_quantitative_shadow_and_compatibility_evidence() {
    let decision = evaluate(&PromotionBundle::passing_fixture()).unwrap();
    assert_eq!(decision.status, PromotionStatus::Approved);
    assert!(decision.unmet_gates.is_empty());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p promotion-gate --test promotion_policy`

Expected: FAIL because the signed evidence bundle and independent approval policy are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/promotion-gate/src/policy.rs` with:

```rust
pub fn evaluate(bundle: &PromotionBundle) -> Result<PromotionDecision, PromotionError> {
    bundle.verify_signatures_and_hashes()?;
    let mut unmet = bundle.quantitative_unmet_gates();
    if bundle.author_key_id == bundle.approval.reviewer_key_id {
        unmet.push("independent reviewer must differ from model author".to_owned());
    }
    if bundle.evidence_expired() { unmet.push("promotion evidence has expired".to_owned()); }
    let status = if unmet.is_empty() { PromotionStatus::Approved } else { PromotionStatus::Rejected };
    Ok(PromotionDecision { status, unmet_gates: unmet, evidence_root: bundle.evidence_root()? })
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p promotion-gate --test promotion_policy`

Expected: PASS for every quantitative gate, event/regime count, cusp ablation, drift, latency, parity, signature, independence, expiry, rollback, and revocation case

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p release-cli -- promotion-check --bundle tests/fixtures/promotion/passing --output target/promotion-decision.json`

Expected: the decision is approved only for the complete passing fixture and includes signed identities, immutable evidence root, model/channel/runtime compatibility, review date, and exact registry transition

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: no command can bypass a failed gate with a boolean force flag; exceptions require a new signed policy version and explicit experimental status; revocation immediately selects a declared verified fallback.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/promotion-gate apps/release-cli/src/promote.rs apps/release-cli/src/main.rs crates/model-registry/src/state.rs docs/governance/model-promotion.md tests/fixtures/promotion Cargo.toml Cargo.lock
git commit -m "feat: enforce independent model promotion gates"
```

### Task 14: Build signed macOS packaging plus fresh-install, upgrade, interruption, and rollback tests

**Files:**
- Create: `scripts/package-macos.sh`
- Create: `scripts/notarize-macos.sh`
- Create: `scripts/verify-release.sh`
- Create: `apps/macos/CuspObservatory/CuspObservatory.entitlements`
- Create: `apps/macos/TransitionModelHost/TransitionModelHost.entitlements`
- Create: `apps/macos/Resources/cryptoriskd.entitlements`
- Modify: `apps/macos/CuspObservatory.xcodeproj/project.pbxproj`
- Create: `tests/upgrades/matrix.yaml`
- Create: `tests/upgrades/run_upgrade_matrix.swift`
- Create: `tests/upgrades/fixtures/v1/manifest.json`
- Create: `.github/workflows/release.yml`
- Create: `docs/release/macos-packaging.md`
- Create: `docs/release/update-and-rollback.md`

**Interfaces:**
- Consumes: Swift/Xcode targets, daemon/model helper, signed artifacts, metadata/model schema migrations, version handshake, persisted alerts/forecasts/audit, and Developer ID/notary credentials supplied only by protected release environment
- Produces: correct nested code signing, hardened runtime/least entitlements, notarized/stapled preview artifact, Gatekeeper verification, upgrade compatibility matrix, interrupted-migration recovery, model incompatibility handling, and data-preserving rollback where supported

**Implementation notes**

Each nested executable/framework is signed explicitly from the inside out; `codesign --deep` is forbidden. Packaging runs from a clean checkout and records identities/entitlements. Upgrade fixtures are non-secret synthetic installations. Rollback is permitted only when the target binary declares compatibility with the on-disk schema; otherwise data are preserved and downgrade is refused with recovery guidance.

- [ ] **Step 1: Write the failing test**

Create or replace `tests/upgrades/run_upgrade_matrix.swift` with:

```rust
import Foundation

let matrix = try UpgradeMatrix.load(path: "tests/upgrades/matrix.yaml")
for scenario in matrix.scenarios {
    let result = try UpgradeRunner.run(scenario)
    precondition(result.alertAuditPreserved)
    precondition(result.forecastAuditPreserved)
    precondition(result.hashChainValid)
    precondition(result.expectedCompatibilityDecision == result.actualCompatibilityDecision)
}
print("upgrade matrix passed: \(matrix.scenarios.count)")
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `swift tests/upgrades/run_upgrade_matrix.swift`

Expected: FAIL because packaging fixtures and the executable upgrade matrix are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `scripts/package-macos.sh` with:

```rust
#!/usr/bin/env bash
set -euo pipefail
: "${APPLE_DEVELOPER_ID_APPLICATION_IDENTITY:?missing signing identity}"
ARCHIVE_PATH="${1:?archive path required}"
EXPORT_DIR="${2:?export directory required}"

xcodebuild -project apps/macos/CuspObservatory.xcodeproj \
  -scheme CuspObservatory -configuration Release \
  -archivePath "$ARCHIVE_PATH" archive

APP="$ARCHIVE_PATH/Products/Applications/CuspObservatory.app"
codesign --verify --strict --verbose=4 "$APP/Contents/Helpers/cryptoriskd"
codesign --verify --strict --verbose=4 "$APP/Contents/Helpers/TransitionModelHost"
codesign --verify --strict --verbose=4 "$APP"
mkdir -p "$EXPORT_DIR"
ditto -c -k --keepParent "$APP" "$EXPORT_DIR/CuspObservatory.zip"
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `swift tests/upgrades/run_upgrade_matrix.swift && scripts/package-macos.sh target/CuspObservatory.xcarchive target/release-dry-run`

Expected: PASS for fresh install, all supported prior versions, interrupted migration, incompatible model, app/daemon skew, permitted/refused rollback, audit preservation, and nested signature checks

- [ ] **Step 5: Run the subsystem verification command**

Run: `scripts/notarize-macos.sh target/release/CuspObservatory.zip && scripts/verify-release.sh target/release/CuspObservatory.app`

Expected: notarytool returns Accepted; stapler validation, `codesign --verify --strict`, and `spctl --assess --type execute` pass; the upgrade evidence matrix is attached to the release manifest

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: all nested code is signed individually with only reviewed entitlements; no release secret appears in arguments/logs/artifacts; package/update signatures are verified before installation; manual update mode remains supported.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add scripts/package-macos.sh scripts/notarize-macos.sh scripts/verify-release.sh apps/macos/CuspObservatory/CuspObservatory.entitlements apps/macos/TransitionModelHost/TransitionModelHost.entitlements apps/macos/Resources/cryptoriskd.entitlements apps/macos/CuspObservatory.xcodeproj/project.pbxproj tests/upgrades .github/workflows/release.yml docs/release/macos-packaging.md docs/release/update-and-rollback.md
git commit -m "build: add signed notarized macOS release pipeline"
```

### Task 15: Publish operator, incident, vulnerability, contribution, and data-license governance

**Files:**
- Modify: `SECURITY.md`
- Modify: `CONTRIBUTING.md`
- Modify: `CODE_OF_CONDUCT.md`
- Create: `GOVERNANCE.md`
- Modify: `LICENSE-APACHE`
- Modify: `LICENSE-MIT`
- Create: `docs/operations/operator-runbook.md`
- Create: `docs/operations/incidents.md`
- Create: `docs/operations/source-quarantine.md`
- Create: `docs/operations/model-revocation.md`
- Create: `docs/operations/disk-recovery.md`
- Create: `docs/operations/release-rollback.md`
- Create: `licenses/data-manifest.yaml`
- Create: `licenses/model-manifest.yaml`
- Create: `licenses/fixture-manifest.yaml`
- Create: `docs/governance/dco.md`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/governance_contract.rs`

**Interfaces:**
- Consumes: all operational state machines, source/fixture provenance, model/release channels, threat model, recovery commands, open-source scope, and dual-license decision
- Produces: executable operator runbooks, severity/incident process, private vulnerability disclosure policy, DCO contribution path, fixture/data/model license manifests, supported-version policy, and game-day records

**Implementation notes**

Runbooks use exact commands, expected states, escalation/rollback criteria, evidence to preserve, and closure checks. No runbook tells an operator to delete immutable audit data, disable integrity checks, or bypass a failed model/release gate. Data/model/fixture rights are separate from the code license and redistribution defaults to denied until documented.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/system-tests/tests/governance_contract.rs` with:

```rust
use std::fs;

#[test]
fn every_required_runbook_and_governance_document_exists_without_unresolved_markers() {
    let files = ["SECURITY.md", "CONTRIBUTING.md", "GOVERNANCE.md", "docs/operations/operator-runbook.md", "docs/operations/model-revocation.md", "licenses/data-manifest.yaml", "licenses/model-manifest.yaml", "licenses/fixture-manifest.yaml"];
    for path in files {
        let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let marker_a = String::from_utf8(vec![84, 66, 68]).unwrap();
        let marker_b = String::from_utf8(vec![84, 79, 68, 79]).unwrap();
        assert!(!text.contains(&marker_a));
        assert!(!text.contains(&marker_b));
        assert!(!text.trim().is_empty());
    }
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p system-tests --test governance_contract`

Expected: FAIL because governance, licenses, and executable runbooks are incomplete

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `licenses/fixture-manifest.yaml` with:

```rust
schema_version: 1
fixtures:
  - path: tests/fixtures/binance/public-depth-sanitized.json
    provenance: sanitized public protocol example
    redistributable: true
    license_basis: public documentation example and project transformation
    contains_secrets: false
  - path: tests/fixtures/private-source-captures
    provenance: user-supplied local captures
    redistributable: false
    license_basis: excluded from repository and release artifacts
    contains_secrets: unknown_until_local_scan
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p system-tests --test governance_contract`

Expected: PASS for document presence, marker scan, exact command validation, data/model/fixture manifest coverage, supported versions, security contact, DCO, and release-channel policy

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p xtask -- governance-check && cargo run -p xtask -- license-check && cargo run -p crypto-replay -- chaos --catalog tests/chaos/scenarios/reference-matrix.yaml --tag game-day`

Expected: governance and license checks pass and the game-day report proves operators can quarantine a source, recover disk pressure, revoke a model, restore from backup, and roll back a candidate without violating audit integrity

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: the repository makes no claim over third-party source data; private disclosure is documented; security advisories are signed; contributor data/fixtures require explicit provenance and redistribution rights.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add SECURITY.md CONTRIBUTING.md CODE_OF_CONDUCT.md GOVERNANCE.md LICENSE-APACHE LICENSE-MIT docs/operations docs/governance licenses crates/system-tests/Cargo.toml crates/system-tests/tests/governance_contract.rs
git commit -m "docs: add operations and open-source governance"
```

### Task 16: Produce SBOM, provenance, acceptance evidence, preview release, and stable release gate

**Files:**
- Create: `apps/release-cli/src/manifest.rs`
- Create: `apps/release-cli/src/acceptance.rs`
- Modify: `apps/release-cli/src/main.rs`
- Create: `scripts/build-sbom.sh`
- Create: `scripts/build-provenance.sh`
- Modify: `scripts/verify-release.sh`
- Create: `docs/release/reproducibility.md`
- Create: `docs/release/release-checklist.md`
- Create: `docs/release/release-notes-template.md`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/acceptance_manifest.rs`
- Create: `crates/system-tests/tests/no_execution_surface.rs`
- Modify: `.github/workflows/release.yml`
- Modify: `docs/implementation/traceability.md`

**Interfaces:**
- Consumes: all phase exit evidence, signed model promotions, shadow reports, security/load/soak/recovery/upgrade/accessibility reports, source commit/toolchains/locks, signed/notarized app, and licensing manifests
- Produces: CycloneDX/SPDX SBOMs, build provenance, signed checksums, reproducibility disclosure, machine-evaluated acceptance manifest, preview channel artifact, and a stable-release transition that refuses any missing/failed/expired critical evidence

**Implementation notes**

The release manifest is the root of evidence: each requirement points to a content hash, signer, creation time, expiry where applicable, and verification command. Apple signing/notarization is declared nonreproducible, while source, lockfiles, schemas, model hashes, build flags, and test inputs are disclosed. Stable is a state transition from an already verified preview artifact, not a separate untested rebuild.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/system-tests/tests/acceptance_manifest.rs` with:

```rust
use release_cli::{evaluate_acceptance, AcceptanceManifest, ReleaseChannel};

#[test]
fn stable_release_requires_every_critical_evidence_record() {
    let mut manifest = AcceptanceManifest::passing_fixture(ReleaseChannel::Stable);
    manifest.evidence.remove("soak_24h");
    let decision = evaluate_acceptance(&manifest).unwrap();
    assert!(!decision.approved);
    assert!(decision.failures.iter().any(|f| f.code == "MISSING_SOAK_EVIDENCE"));
}

#[test]
fn release_surface_contains_no_trading_or_withdrawal_capability() {
    let manifest = AcceptanceManifest::passing_fixture(ReleaseChannel::Stable);
    assert!(manifest.capabilities.iter().all(|c| !matches!(c.as_str(), "place_order" | "withdraw" | "trading_credentials")));
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p system-tests --test acceptance_manifest --test no_execution_surface`

Expected: FAIL because the evidence-rooted acceptance manifest and explicit no-execution assertion are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `apps/release-cli/src/acceptance.rs` with:

```rust
pub fn evaluate_acceptance(manifest: &AcceptanceManifest) -> Result<AcceptanceDecision, ReleaseError> {
    manifest.verify_signature_and_hashes()?;
    let required = ["spec_traceability", "full_ci", "golden_replay", "security_review", "recovery_drill", "reference_load", "soak_24h", "shadow_report", "model_promotion", "upgrade_matrix", "accessibility", "notarization", "sbom", "provenance", "license_scan"];
    let failures = required.iter().filter(|key| !manifest.valid_evidence(key)).map(|key| AcceptanceFailure::missing(key)).collect::<Vec<_>>();
    Ok(AcceptanceDecision { approved: failures.is_empty(), failures, evidence_root: manifest.evidence_root()? })
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p system-tests --test acceptance_manifest --test no_execution_surface`

Expected: PASS for missing, failed, expired, hash-mismatched, wrong-signer, preview/stable channel, no-execution, and complete passing-manifest cases

- [ ] **Step 5: Run the subsystem verification command**

Run: `scripts/build-sbom.sh target/release && scripts/build-provenance.sh target/release && cargo run -p release-cli -- assemble --channel preview --output target/release && scripts/verify-release.sh target/release && cargo run -p release-cli -- promote-channel --from preview --to stable --same-artifact target/release/release-manifest.json`

Expected: the exact preview artifact passes all verification, then the same artifact is promoted to stable with a signed acceptance decision; SBOM, provenance, checksums, model cards, validation/shadow/security/soak/upgrade reports, licenses, and reproducibility disclosure are present and hash-linked

- [ ] **Step 6: Inspect integrity, privacy, point-in-time, and release behavior**

Run: `git diff --check && git status --short`

Expected: no stable release is possible with skipped evidence, changed binaries after preview, unsigned models, unnotarized nested code, unresolved critical incidents, stale promotion reports, unknown fixture rights, or any execution/credential capability.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add apps/release-cli/src/manifest.rs apps/release-cli/src/acceptance.rs apps/release-cli/src/main.rs scripts/build-sbom.sh scripts/build-provenance.sh scripts/verify-release.sh docs/release crates/system-tests/Cargo.toml crates/system-tests/tests/acceptance_manifest.rs crates/system-tests/tests/no_execution_surface.rs .github/workflows/release.yml docs/implementation/traceability.md
git commit -m "release: gate and publish the stable open-source product"
```

## Phase 08 exit gate

Run the following from a clean checkout on the declared reference Apple Silicon machine:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo run -p xtask -- governance-check
cargo run -p xtask -- license-check
cargo run -p xtask -- secret-scan
cargo run -p crypto-replay -- chaos --catalog tests/chaos/scenarios/reference-matrix.yaml --all --report target/chaos-report.json
cargo run -p crypto-evaluate -- recovery-drill --fixture tests/fixtures/recovery/reference --rpo-ms 250 --rto-seconds 60 --report target/recovery-drill.json
cargo run -p shadow-reporter -- evaluate --ledger data/shadow --models data/models --output target/shadow-report
cargo run -p release-cli -- promotion-check --bundle target/promotion-bundle --output target/promotion-decision.json
swift test --package-path apps/macos/Packages/TransitionClient -Xswiftc -strict-concurrency=complete
xcodebuild test -project apps/macos/CuspObservatory.xcodeproj -scheme CuspObservatory -destination 'platform=macOS'
scripts/run-soak.sh --profile tests/performance/reference-load.toml --duration 24h --report target/soak-report.json
scripts/build-sbom.sh target/release
scripts/build-provenance.sh target/release
scripts/verify-release.sh target/release
cargo run -p release-cli -- acceptance-check --manifest target/release/release-manifest.json
```

The phase passes only when:

1. Every injected integrity failure is detected and produces its declared fail-closed or degraded behavior.
2. Recovery, backup, restore, upgrade, rollback, and audit continuity tests pass.
3. Reference throughput, p99 latency, resource, UI, and 24-hour soak gates pass on the declared machine.
4. Shadow event-count, calibration, drift, alert-budget, and offline/live consistency evidence satisfies the approved model policy; outputs without sufficient evidence remain experimental or unavailable.
5. Model promotion has independent signed approval and a verified fallback.
6. No secret, trading/withdrawal capability, remote analytics destination, unsigned model, or unlicensed redistributable fixture is present.
7. The identical preview artifact is nested-signed, notarized, stapled, Gatekeeper-accepted, SBOM/provenance-complete, and promoted to stable through a signed acceptance manifest.
8. The traceability matrix links every production acceptance requirement to immutable passing evidence.

## Stable-release definition of done

A stable release is complete only when the source commit, approved specification hash, complete implementation-plan hash, dependency locks, protobuf/schema hashes, model packages and cards, shadow/validation reports, security review, chaos/recovery/load/soak/upgrade/accessibility results, SBOM, provenance, signed checksums, notarized application, operator runbooks, licenses, and acceptance decision are content-addressed from one release manifest. Any later binary, model, schema, or evidence change creates a new candidate and repeats the applicable gates.
