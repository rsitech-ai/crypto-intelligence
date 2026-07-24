# Phase 00 — Foundations and Contracts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create the greenfield repository, frozen domain/wire/configuration contracts, authenticated local daemon skeleton, and CI gates required before connector or model work begins.

**Architecture:** Use small Rust crates for canonical types and a Tonic boundary for the future Swift client. Phase 0 contains no market-source business logic; it creates stable seams, typed errors, deterministic code generation, and governance evidence that downstream plans can consume in parallel.

**Tech Stack:** Rust 1.97.1, edition 2024, Cargo workspace, Tokio 1.51, Tonic/Prost, Buf/protoc code generation, Serde/TOML/JSON Schema, tracing, UUIDv7, BLAKE3, Ed25519-ready artifact policy, GitHub Actions.

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

This plan covers specification sections 1–11, 30, 32–35, and the Phase 0 roadmap gate insofar as they define repository, identity, event, configuration, RPC, security, and governance contracts. It explicitly excludes live exchange connections, order books, feature computation, statistical estimation, and user-interface implementation.

## File and module map

- `crates/fixed-decimal`: authoritative checked decimal values and typed monetary wrappers.
- `crates/domain`: asset, venue, instrument, product, and contract identities.
- `crates/event-envelope`: canonical event metadata, payload families, quality flags, and deterministic IDs.
- `crates/config`: configuration merge, validation, schema generation, and audit hashes.
- `crates/observability`: local tracing, metrics, redaction, and diagnostics snapshots.
- `crates/local-api`: generated RPC types, authentication, compatibility, and Tonic server helpers.
- `apps/cryptoriskd`: minimal daemon entry point and protected health/runtime service.
- `xtask`: reproducible code generation and policy checks.
- `crates/system-tests`: cross-crate contract, integration, hardening, performance, and release tests owned by a real Cargo package.

## Exit gate

A clean checkout compiles with the pinned toolchain; canonical numeric/identity/event/config/RPC tests pass; loopback authentication fails closed; protobuf regeneration is deterministic; license, ADR, security, and traceability checks pass; no unresolved contract ambiguity can alter a later label or forecast meaning.

---

### Task 1: Bootstrap the pinned Rust workspace and developer command surface

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.cargo/config.toml`
- Create: `crates/domain/Cargo.toml`
- Create: `crates/domain/src/lib.rs`
- Create: `xtask/Cargo.toml`
- Create: `xtask/src/main.rs`
- Create: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/src/lib.rs`
- Test: `crates/system-tests/tests/repository_layout.rs`

**Interfaces:**
- Consumes: the approved repository layout and toolchain policy
- Produces: a compiling edition-2024 workspace, `xtask` command shell, and a test that prevents required top-level paths from disappearing

**Implementation notes**

`xtask` implements only `help` and `workspace-check` in this bootstrap task. Later tasks add `proto-check`, `license-check`, and `spec-traceability` together with their complete tests; unknown commands fail with a typed `UnknownCommand` error. Do not add a second async runtime.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/system-tests/tests/repository_layout.rs` with:

```text
use std::path::Path;

#[test]
fn required_workspace_paths_exist() {
    for path in [
        "Cargo.toml",
        "rust-toolchain.toml",
        "crates/domain/src/lib.rs",
        "xtask/src/main.rs",
        "crates/system-tests/Cargo.toml",
        "proto",
        "docs/adr",
    ] {
        assert!(Path::new(path).exists(), "missing {path}");
    }
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p system-tests --test repository_layout`

Expected: FAIL because the workspace and required paths do not yet exist

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `Cargo.toml` with:

```text
[workspace]
resolver = "2"
members = [
  "crates/*",
  "apps/*",
  "xtask",
]

[workspace.package]
edition = "2024"
rust-version = "1.88"
license = "MIT OR Apache-2.0"

[workspace.dependencies]
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
tokio = { version = "1.51", features = ["macros", "rt-multi-thread", "signal", "sync", "time"] }
tracing = "0.1"
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p system-tests --test repository_layout`

Expected: PASS after the task also creates the listed directories and minimal crate manifests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo metadata --no-deps --format-version 1 >/dev/null && cargo check --workspace`

Expected: workspace metadata resolves and every initial target compiles

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add Cargo.toml Cargo.lock rust-toolchain.toml .cargo/config.toml crates/domain crates/system-tests xtask proto docs/adr
    git commit -m "build: bootstrap pinned Rust workspace"
```


### Task 2: Record licenses, governance, threat boundary, and foundational ADRs

**Files:**
- Create: `LICENSE-APACHE`
- Create: `LICENSE-MIT`
- Create: `SECURITY.md`
- Create: `CONTRIBUTING.md`
- Create: `CODE_OF_CONDUCT.md`
- Create: `docs/adr/0001-modular-monolith.md`
- Create: `docs/adr/0002-local-data-plane.md`
- Create: `docs/adr/0003-no-execution-v1.md`
- Modify: `crates/system-tests/Cargo.toml`
- Test: `crates/system-tests/tests/governance_files.rs`

**Interfaces:**
- Consumes: Phase 0 workspace and approved architecture decisions
- Produces: machine-checked governance files and accepted ADRs that freeze modular-monolith, local-first storage, and no-execution boundaries

**Implementation notes**

The security policy must name supported release channels, a private reporting path, severity handling, and signed advisory policy. Fixture/data manifests are explicitly outside the code license unless their own manifest says otherwise.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/system-tests/tests/governance_files.rs` with:

```text
use std::fs;

#[test]
fn governance_files_state_required_boundaries() {
    let security = fs::read_to_string("SECURITY.md").unwrap();
    let contributing = fs::read_to_string("CONTRIBUTING.md").unwrap();
    let adr = fs::read_to_string("docs/adr/0003-no-execution-v1.md").unwrap();
    assert!(security.contains("private disclosure"));
    assert!(contributing.contains("Developer Certificate of Origin"));
    assert!(adr.contains("No trading or withdrawal credentials"));
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p system-tests --test governance_files`

Expected: FAIL because governance and ADR files are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `docs/adr/0001-modular-monolith.md` with:

```text
# ADR 0001: Local modular monolith

- Status: Accepted
- Date: 2026-07-24

## Decision
One Rust daemon owns capture, normalization, books, features, models, storage, replay, and audit. A SwiftUI application connects over authenticated loopback gRPC. Process separation requires measured isolation or throughput evidence.

## Consequences
Deterministic replay and local installation remain simple. Module APIs must stay explicit so selected components can be separated without schema redesign.
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p system-tests --test governance_files`

Expected: PASS with all governance phrases and ADR status fields present

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p xtask -- license-check`

Expected: exit code 0 after the command verifies dual-license headers and separate data/model licensing policy

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add LICENSE-APACHE LICENSE-MIT SECURITY.md CONTRIBUTING.md CODE_OF_CONDUCT.md docs/adr crates/system-tests/Cargo.toml crates/system-tests/tests/governance_files.rs xtask/src/main.rs
    git commit -m "docs: freeze governance and architecture boundaries"
```


### Task 3: Implement checked canonical fixed-decimal arithmetic

**Files:**
- Create: `crates/fixed-decimal/Cargo.toml`
- Create: `crates/fixed-decimal/src/lib.rs`
- Create: `crates/fixed-decimal/src/error.rs`
- Test: `crates/fixed-decimal/tests/fixed_decimal.rs`

**Interfaces:**
- Consumes: workspace error and serialization conventions
- Produces: `FixedDecimal`, `Price`, `Quantity`, `Notional`, and `Rate` wrappers with canonical parsing, checked arithmetic, explicit scale conversion, and analytical `f64` boundary methods

**Implementation notes**

Add proptest coverage for arbitrary canonical strings, round trips, overflow, negative-zero normalization, and exact-rescale laws. `DecimalError` includes `InvalidSyntax`, `Overflow`, and `PrecisionLoss`; every conversion is checked and authoritative parsing never touches `f64`.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/fixed-decimal/tests/fixed_decimal.rs` with:

```text
use fixed_decimal::FixedDecimal;

#[test]
fn canonical_parse_never_round_trips_through_f64() {
    let value = FixedDecimal::parse("123.4500").unwrap();
    assert_eq!(value.mantissa(), 12_345);
    assert_eq!(value.scale(), 2);
    assert_eq!(value.to_string(), "123.45");
}

#[test]
fn checked_rescale_rejects_precision_loss() {
    let value = FixedDecimal::parse("1.234").unwrap();
    assert!(value.rescale_exact(2).is_err());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p fixed-decimal`

Expected: FAIL because `fixed_decimal` and its public types are undefined

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/fixed-decimal/src/lib.rs` with:

```text
mod error;

pub use error::DecimalError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct FixedDecimal {
    mantissa: i128,
    scale: u32,
}

impl FixedDecimal {
    pub fn parse(input: &str) -> Result<Self, DecimalError> {
        let (negative, body) = input.strip_prefix('-').map_or((false, input), |v| (true, v));
        let mut parts = body.split('.');
        let whole = parts.next().ok_or(DecimalError::InvalidSyntax)?;
        let frac = parts.next().unwrap_or("");
        if parts.next().is_some() || whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return Err(DecimalError::InvalidSyntax);
        }
        let frac = frac.trim_end_matches('0');
        let digits = format!("{whole}{frac}");
        let mut mantissa: i128 = digits.parse().map_err(|_| DecimalError::Overflow)?;
        if negative { mantissa = mantissa.checked_neg().ok_or(DecimalError::Overflow)?; }
        Ok(Self { mantissa, scale: frac.len() as u32 })
    }

    pub const fn mantissa(self) -> i128 { self.mantissa }
    pub const fn scale(self) -> u32 { self.scale }

    pub fn rescale_exact(self, target: u32) -> Result<Self, DecimalError> {
        if target == self.scale { return Ok(self); }
        if target > self.scale {
            let power = target - self.scale;
            let factor = 10_i128.checked_pow(power).ok_or(DecimalError::Overflow)?;
            let mantissa = self.mantissa.checked_mul(factor).ok_or(DecimalError::Overflow)?;
            return Ok(Self { mantissa, scale: target });
        }
        let power = self.scale - target;
        let factor = 10_i128.checked_pow(power).ok_or(DecimalError::Overflow)?;
        if self.mantissa % factor != 0 { return Err(DecimalError::PrecisionLoss); }
        Ok(Self { mantissa: self.mantissa / factor, scale: target })
    }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p fixed-decimal`

Expected: PASS with checked integer multiplication/division, explicit `PrecisionLoss`, canonical `Display`, and no `f64` in parsing or authoritative arithmetic

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p fixed-decimal --all-features && cargo clippy -p fixed-decimal --all-targets -- -D warnings`

Expected: unit, integration, and property tests pass with no warnings

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/fixed-decimal Cargo.toml Cargo.lock
    git commit -m "feat: add checked fixed-decimal domain types"
```


### Task 4: Define canonical asset, instrument, venue, and generation identities

**Files:**
- Create: `crates/domain/src/id.rs`
- Create: `crates/domain/src/instrument.rs`
- Create: `crates/domain/src/source.rs`
- Modify: `crates/domain/src/lib.rs`
- Test: `crates/domain/tests/identifiers.rs`

**Interfaces:**
- Consumes: checked fixed-decimal wrappers
- Produces: stable `AssetId`, `InstrumentId`, `InstrumentDefinition`, `VenueId`, `ProductType`, and linear/inverse contract conversion APIs

**Implementation notes**

Validate empty IDs, generation zero, option expiry/strike/side, settlement assets, listing/delisting order, tick/step positivity, and inverse notional conversion. Symbols never serve as equality by themselves.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/domain/tests/identifiers.rs` with:

```text
use domain::{AssetId, AssetNamespace, InstrumentDefinition, ProductType};

#[test]
fn symbol_reuse_requires_new_generation() {
    let first = InstrumentDefinition::test_perpetual("BTCUSDT", 1);
    let second = InstrumentDefinition::test_perpetual("BTCUSDT", 2);
    assert_ne!(first.id(), second.id());
    assert_eq!(first.product_type(), ProductType::Perpetual);
}

#[test]
fn native_and_wrapped_assets_are_not_equal() {
    let native = AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).unwrap();
    let wrapped = AssetId::new(AssetNamespace::Evm, "1", "0xbtc", "WBTC", 1).unwrap();
    assert_ne!(native, wrapped);
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p domain --test identifiers`

Expected: FAIL because canonical identifier types and constructors are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/domain/src/instrument.rs` with:

```text
use fixed_decimal::{FixedDecimal, Price, Quantity};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProductType { Spot, Perpetual, Future, Option }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ContractKind { Linear, Inverse, None }

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct InstrumentId {
    pub venue: String,
    pub venue_symbol: String,
    pub generation: u32,
}

#[derive(Clone, Debug)]
pub struct InstrumentDefinition {
    id: InstrumentId,
    product_type: ProductType,
    contract_kind: ContractKind,
    price_tick: Price,
    quantity_step: Quantity,
    contract_multiplier: FixedDecimal,
}

impl InstrumentDefinition {
    pub fn id(&self) -> &InstrumentId { &self.id }
    pub const fn product_type(&self) -> ProductType { self.product_type }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p domain --test identifiers`

Expected: PASS after all validation and test constructors are implemented

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p domain && cargo clippy -p domain --all-targets -- -D warnings`

Expected: all identifier, serialization, and contract-conversion tests pass

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/domain Cargo.toml Cargo.lock
    git commit -m "feat: define canonical market identities"
```


### Task 5: Implement canonical event envelopes and deterministic event identities

**Files:**
- Create: `crates/event-envelope/Cargo.toml`
- Create: `crates/event-envelope/src/lib.rs`
- Create: `crates/event-envelope/src/payload.rs`
- Create: `crates/event-envelope/src/quality.rs`
- Test: `crates/event-envelope/tests/envelope.rs`

**Interfaces:**
- Consumes: domain identities, UUIDv7 policy, BLAKE3 hashing, and integer nanosecond timestamps
- Produces: typed `EventEnvelope<P>`, source-quality flags, snapshot/delta semantics, canonical BLAKE3 event IDs, and normalized payload enums

**Implementation notes**

Use a canonical binary encoding for event-ID input; never hash arbitrary JSON key order. Preserve source-native IDs and raw payload hashes. Normalized payloads include all records named in section 11.1, even when their connector arrives in later phases.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/event-envelope/tests/envelope.rs` with:

```text
use event_envelope::{CanonicalEventId, EventEnvelope, SnapshotKind, Trade};

#[test]
fn event_id_is_stable_for_canonical_payload() {
    let event = EventEnvelope::fixture(Trade::fixture(), SnapshotKind::Delta);
    let first = CanonicalEventId::from_envelope(&event).unwrap();
    let second = CanonicalEventId::from_envelope(&event).unwrap();
    assert_eq!(first, second);
}

#[test]
fn receive_time_cannot_precede_connection_epoch_start() {
    assert!(EventEnvelope::invalid_fixture().validate().is_err());
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p event-envelope`

Expected: FAIL because envelope, payload, validation, and identity APIs do not exist

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/event-envelope/src/lib.rs` with:

```text
pub mod payload;
pub mod quality;

use domain::{InstrumentId, VenueId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotKind { Snapshot, Delta, Observation }

#[derive(Clone, Debug)]
pub struct EventEnvelope<P> {
    pub schema_version: u32,
    pub event_id: [u8; 32],
    pub source: String,
    pub venue: Option<VenueId>,
    pub instrument_id: Option<InstrumentId>,
    pub exchange_event_time_ns: Option<i64>,
    pub receive_wall_time_ns: i64,
    pub receive_monotonic_time_ns: u64,
    pub sequence_number: Option<u64>,
    pub previous_sequence_number: Option<u64>,
    pub connection_epoch: u64,
    pub subscription_epoch: u64,
    pub snapshot_kind: SnapshotKind,
    pub raw_payload_hash: [u8; 32],
    pub quality_flags: quality::QualityFlags,
    pub payload: P,
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p event-envelope`

Expected: PASS after complete validation and canonical serialization/hashing are added

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p event-envelope --all-features && cargo clippy -p event-envelope --all-targets -- -D warnings`

Expected: event identity stays stable across serialization order and validation rejects inconsistent timestamps/sequences

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/event-envelope Cargo.toml Cargo.lock
    git commit -m "feat: add canonical event envelope"
```


### Task 6: Create versioned protobuf contracts and reproducible code generation

**Files:**
- Create: `proto/buf.yaml`
- Create: `proto/buf.lock`
- Create: `proto/buf.gen.yaml`
- Create: `proto/common/v1/common.proto`
- Create: `proto/market/v1/market.proto`
- Create: `proto/risk/v1/risk.proto`
- Create: `proto/replay/v1/replay.proto`
- Create: `proto/admin/v1/admin.proto`
- Create: `crates/local-api/build.rs`
- Create: `crates/local-api/src/generated.rs`
- Create: `scripts/generate-proto.sh`
- Modify: `crates/system-tests/Cargo.toml`
- Test: `crates/system-tests/tests/proto_contract.rs`

**Interfaces:**
- Consumes: domain and forecast contract names from the approved specification
- Produces: normative versioned RPC schemas, Rust/Swift generation commands, stream envelopes, stable errors, and compatibility checks

**Implementation notes**

Use one canonical field-number registry document under `proto/FIELD_NUMBERS.md`. Deleted fields are reserved by number and name. Generated-file check-in policy is “checked in and reproducibly regenerated” for both Rust and Swift.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/system-tests/tests/proto_contract.rs` with:

```text
use std::fs;

#[test]
fn protobuf_contract_reserves_zero_enum_values_and_stream_metadata() {
    let risk = fs::read_to_string("proto/risk/v1/risk.proto").unwrap();
    assert!(risk.contains("EVENT_TYPE_UNSPECIFIED = 0"));
    assert!(risk.contains("uint64 stream_sequence"));
    assert!(risk.contains("string resume_token"));
    assert!(risk.contains("evidence_bundle_id"));
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p system-tests --test proto_contract`

Expected: FAIL because normative `.proto` files do not exist

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `proto/risk/v1/risk.proto` with:

```text
syntax = "proto3";
package transitionintel.risk.v1;

import "google/protobuf/timestamp.proto";

enum EventType {
  EVENT_TYPE_UNSPECIFIED = 0;
  EVENT_TYPE_DOWNSIDE_TRANSITION = 1;
  EVENT_TYPE_UPSIDE_TRANSITION = 2;
  EVENT_TYPE_VOLATILITY_EXPLOSION = 3;
  EVENT_TYPE_LIQUIDITY_VACUUM = 4;
  EVENT_TYPE_LIQUIDATION_CASCADE = 5;
  EVENT_TYPE_BASIS_DISLOCATION = 6;
  EVENT_TYPE_STABLECOIN_DISLOCATION = 7;
  EVENT_TYPE_CONTAGION = 8;
}

message StreamMeta {
  string stream_id = 1;
  uint64 stream_sequence = 2;
  string resume_token = 3;
  uint32 schema_version = 4;
  google.protobuf.Timestamp as_of_time = 5;
}

message ForecastSnapshot {
  StreamMeta stream = 1;
  string forecast_id = 2;
  EventType event_type = 3;
  string evidence_bundle_id = 4;
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p system-tests --test proto_contract`

Expected: PASS after all packages, services, messages, reserved-field rules, and generated-code checks are present

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p xtask -- proto-check && git diff --exit-code -- apps/macos/GeneratedProto crates/local-api/src/generated.rs`

Expected: schemas lint, breaking-change checks pass against the checked baseline, and regeneration yields no diff

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add proto crates/local-api scripts/generate-proto.sh apps/macos/GeneratedProto crates/system-tests/tests/proto_contract.rs xtask/src/main.rs Cargo.toml Cargo.lock
    git commit -m "feat: define versioned local RPC contracts"
```


### Task 7: Implement layered configuration with generated JSON Schema and safe validation

**Files:**
- Create: `crates/config/Cargo.toml`
- Create: `crates/config/src/lib.rs`
- Create: `crates/config/src/schema.rs`
- Create: `configs/default.toml`
- Create: `configs/schema.json`
- Test: `crates/config/tests/config_validation.rs`

**Interfaces:**
- Consumes: domain venue IDs, model capability policy, and approved precedence rules
- Produces: typed `EffectiveConfig`, deterministic merge precedence, secret-reference validation, dynamic/restart-required classification, and canonical audit hashes

**Implementation notes**

The test fixture uses explicit `KeychainReference` values for any future authenticated read-only source. Runtime mutations return both the canonical old/new hash and `ChangeImpact::{Dynamic,RestartRequired}`.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/config/tests/config_validation.rs` with:

```text
use config::{ConfigError, EffectiveConfig};

#[test]
fn plaintext_secret_is_rejected_before_network_start() {
    let text = r#"schema_version = 1
[credentials]
api_secret = "plain-text""#;
    assert!(matches!(EffectiveConfig::from_toml(text), Err(ConfigError::EmbeddedSecret(_))));
}

#[test]
fn command_line_override_wins_over_user_file() {
    let cfg = EffectiveConfig::fixture_with_log_overrides("info", "debug").unwrap();
    assert_eq!(cfg.logging.level.as_str(), "debug");
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p config`

Expected: FAIL because configuration types and validators are undefined

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/config/src/lib.rs` with:

```text
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration embeds a secret at {0}")]
    EmbeddedSecret(String),
    #[error("unknown critical field: {0}")]
    UnknownCriticalField(String),
    #[error("unsafe capacity request: {0}")]
    UnsafeCapacity(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveConfig {
    pub schema_version: u32,
    pub profile: String,
    pub timezone: String,
    pub daemon: DaemonConfig,
    pub coverage: CoverageConfig,
    pub venues: Vec<VenueConfig>,
    pub models: ModelConfig,
    pub alerts: AlertConfig,
    pub privacy: PrivacyConfig,
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p config`

Expected: PASS with safe-range, path, retention, capacity, incompatible-model, unknown-field, and secret-reference tests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo run -p xtask -- generate-config-schema && git diff --exit-code -- configs/schema.json && cargo test -p config`

Expected: generated schema is stable and all merge/audit tests pass

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/config configs xtask/src/main.rs Cargo.toml Cargo.lock
    git commit -m "feat: add validated layered configuration"
```


### Task 8: Establish local structured observability without remote export

**Files:**
- Create: `crates/observability/Cargo.toml`
- Create: `crates/observability/src/lib.rs`
- Create: `crates/observability/src/metrics.rs`
- Create: `crates/observability/src/redaction.rs`
- Test: `crates/observability/tests/redaction.rs`

**Interfaces:**
- Consumes: configuration privacy defaults and canonical source/instrument IDs
- Produces: local tracing setup, bounded in-process metrics registry, stable field names, sensitive-value redaction, and diagnostics snapshots

**Implementation notes**

Metric labels use bounded enums/IDs, never raw error strings or payloads. A diagnostics export is user initiated and runs through a separate redaction pass.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/observability/tests/redaction.rs` with:

```text
use observability::redaction::RedactedFields;

#[test]
fn secrets_and_raw_news_text_never_enter_structured_logs() {
    let fields = RedactedFields::from_pairs([
        ("session_secret", "abc"),
        ("instrument_id", "binance:BTCUSDT:1"),
    ]);
    let rendered = fields.to_string();
    assert!(!rendered.contains("abc"));
    assert!(rendered.contains("instrument_id"));
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p observability`

Expected: FAIL because redaction and local metrics APIs are missing

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/observability/src/lib.rs` with:

```text
pub mod metrics;
pub mod redaction;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_local_logging(filter: &str) -> Result<(), Box<dyn std::error::Error>> {
    let fmt = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true)
        .with_span_list(true);
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(filter))
        .with(fmt)
        .try_init()?;
    Ok(())
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p observability`

Expected: PASS with redaction, metric cardinality, queue-gauge, and local-only exporter tests

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p observability && cargo clippy -p observability --all-targets -- -D warnings`

Expected: all tests pass and no logging API accepts a secret-bearing type implementing `Display`

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/observability Cargo.toml Cargo.lock
    git commit -m "feat: add local structured observability"
```


### Task 9: Implement authenticated loopback session bootstrap and compatibility handshake

**Files:**
- Create: `crates/local-api/Cargo.toml`
- Create: `crates/local-api/src/lib.rs`
- Create: `crates/local-api/src/auth.rs`
- Create: `crates/local-api/src/compatibility.rs`
- Create: `apps/cryptoriskd/Cargo.toml`
- Create: `apps/cryptoriskd/src/main.rs`
- Test: `crates/local-api/tests/session_auth.rs`

**Interfaces:**
- Consumes: generated protobuf contracts, secrecy/zeroize policy, config, and local observability
- Produces: loopback-only Tonic server bootstrap, inherited-descriptor secret intake, constant-time metadata authentication, process nonce validation, and major/minor compatibility decisions

**Implementation notes**

The daemon reads the initial 256-bit secret from an inherited descriptor, never command-line arguments or environment variables. Test the descriptor lifecycle on macOS and Linux. Health preflight exposes no sensitive build or path data before authentication.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/local-api/tests/session_auth.rs` with:

```text
use local_api::auth::{authenticate, SessionSecret};

#[test]
fn wrong_secret_and_wrong_nonce_fail_closed() {
    let expected = SessionSecret::from_bytes([7_u8; 32]);
    assert!(authenticate(&expected, "00", "nonce-a", "nonce-a").is_err());
    assert!(authenticate(&expected, &expected.to_hex(), "nonce-b", "nonce-a").is_err());
}

#[test]
fn major_api_mismatch_is_incompatible() {
    assert!(!local_api::compatibility::is_compatible(2, 0, 1, 9));
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p local-api --test session_auth`

Expected: FAIL because auth and compatibility APIs are missing

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `crates/local-api/src/auth.rs` with:

```text
use secrecy::{ExposeSecret, SecretBox};
use subtle::ConstantTimeEq;
use thiserror::Error;

#[derive(Clone)]
pub struct SessionSecret(SecretBox<[u8; 32]>);

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("invalid session credentials")] Invalid,
}

pub fn authenticate(expected: &SessionSecret, supplied_hex: &str, supplied_nonce: &str, process_nonce: &str) -> Result<(), AuthError> {
    let supplied = hex::decode(supplied_hex).map_err(|_| AuthError::Invalid)?;
    let secret_ok = supplied.as_slice().ct_eq(expected.0.expose_secret()).into();
    let nonce_ok = supplied_nonce.as_bytes().ct_eq(process_nonce.as_bytes()).into();
    if secret_ok && nonce_ok { Ok(()) } else { Err(AuthError::Invalid) }
}
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p local-api --test session_auth`

Expected: PASS with valid, invalid, expired, missing, oversized metadata, and version-skew cases

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo test -p local-api && cargo test -p cryptoriskd && cargo clippy -p local-api -p cryptoriskd --all-targets -- -D warnings`

Expected: server binds only loopback, reflection is off in release configuration, and all mutating RPCs require authorization

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add crates/local-api apps/cryptoriskd Cargo.toml Cargo.lock
    git commit -m "feat: secure the local daemon RPC bootstrap"
```


### Task 10: Create the contract CI pipeline and Phase 0 traceability gate

**Files:**
- Create: `.github/workflows/ci.yml`
- Create: `deny.toml`
- Create: `scripts/verify-reproducibility.sh`
- Create: `docs/implementation/traceability.md`
- Create: `crates/system-tests/tests/spec_traceability.rs`
- Modify: `xtask/src/main.rs`

**Interfaces:**
- Consumes: all Phase 0 contracts and approved specification section numbers
- Produces: format/lint/license/protobuf/test/reproducibility CI and a machine-checked mapping from Phase 0 requirements to tests and artifacts

**Implementation notes**

Pin third-party actions by immutable commit SHA before merging. The plan excerpt uses a readable tag only; the committed workflow must use reviewed SHAs and document update ownership.

- [ ] **Step 1: Write the failing test**

Create or replace `crates/system-tests/tests/spec_traceability.rs` with:

```text
use std::fs;

#[test]
fn phase_zero_traceability_names_every_contract_family() {
    let text = fs::read_to_string("docs/implementation/traceability.md").unwrap();
    for required in ["fixed-decimal", "instrument identity", "event envelope", "protobuf", "configuration", "session authentication"] {
        assert!(text.contains(required), "missing traceability row for {required}");
    }
}
```

- [ ] **Step 2: Run the focused test and confirm the intended failure**

Run: `cargo test -p system-tests --test spec_traceability`

Expected: FAIL because traceability and CI policy files are absent

- [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

Create or update `.github/workflows/ci.yml` with:

```text
name: ci
on:
  pull_request:
  push:
    branches: [main]

jobs:
  rust-contracts:
    runs-on: macos-15
    steps:
      - uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683
      - run: rustup toolchain install 1.97.1 --profile minimal
      - run: rustup default 1.97.1
      - run: rustup show active-toolchain
      - run: cargo fmt --all -- --check
      - run: cargo clippy --workspace --all-targets --all-features -- -D warnings
      - run: cargo test --workspace --all-features
      - run: cargo run -p xtask -- proto-check
      - run: cargo run -p xtask -- license-check
      - run: cargo run -p xtask -- spec-traceability
```

- [ ] **Step 4: Run the focused test and confirm success**

Run: `cargo test -p system-tests --test spec_traceability`

Expected: PASS with explicit requirement, owner, implementation, test, and evidence columns

- [ ] **Step 5: Run the subsystem verification command**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features && cargo run -p xtask -- proto-check && cargo run -p xtask -- spec-traceability`

Expected: all Phase 0 gates pass from a clean checkout

- [ ] **Step 6: Review the diff for contract, lineage, and error-type consistency**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only the files listed for this task and intentionally generated lock/codegen files are changed.

- [ ] **Step 7: Commit the independently testable deliverable**

```bash
git add .github/workflows/ci.yml deny.toml scripts/verify-reproducibility.sh docs/implementation/traceability.md crates/system-tests/tests/spec_traceability.rs xtask/src/main.rs
    git commit -m "ci: enforce foundational contracts and traceability"
```
