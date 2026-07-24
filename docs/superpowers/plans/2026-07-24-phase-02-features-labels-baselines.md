# Phase 02 — Point-in-Time Features, Labels, and Baseline Models Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transform trusted normalized events into reproducible point-in-time features and labels, then establish calibrated statistical baselines and signed model artifacts against which all advanced models must earn inclusion.

**Architecture:** A registry-driven Rust feature engine processes the same event sequence in live and replay modes under explicit watermarks, finality, missingness, quality, normalization, and lineage policies. Versioned labels feed purged nested walk-forward datasets; simple base-rate, volatility, BOCPD, Student-t HMM, and elastic-net competing-risk models produce auditable evaluation and calibration artifacts.

**Tech Stack:** Rust, Tokio deterministic clocks, Arrow/Parquet/DataFusion, nalgebra, semver, BLAKE3, Ed25519 package verification, proptest, numerical reference fixtures, clap evaluation CLI.

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

This plan covers feature definitions and materialization, event-time semantics, the production feature families available from Phase 1 data, v1 event labels, point-in-time datasets, nested walk-forward folds, required statistical baselines, calibration primitives, model package/registry contracts, and automated evaluation reports. It does not include stochastic cusp estimation, production stacking/scenarios/alerts, options/on-chain sources, or Swift UI.

## File and module map

- `crates/feature-registry`: definitions, status, observations, missingness, documentation, and schema hashes.
- `crates/feature-engine`: event-time execution, windows, live/replay materialization, feature implementations, and lineage.
- `crates/consolidated-market`: robust fair price, venue eligibility, and stablecoin quote adjustment.
- `crates/volatility`: realized measures plus EWMA/HAR-RV forecasting.
- `crates/labels`: versioned event outcomes, first passage, joint mechanisms, overlap, censoring, and exclusions.
- `crates/dataset`: point-in-time joins, manifests, purge/embargo, and nested walk-forward folds.
- `crates/baselines`, `crates/changepoint`, `crates/regime`, `crates/hazard`: required baseline models.
- `crates/calibration`, `crates/model-registry`, `apps/crypto-evaluate`: calibration, metrics, signed packages, registry lifecycle, and reports.

## Exit gate

The same committed market replay produces matching live/WAL/Parquet feature and label hashes; every feature has as-known-at, finality, quality, missingness, formula, and lineage metadata; no leakage probe succeeds; nested walk-forward reports reproduce; required baselines and probability/calibration metrics are generated; model packages fail closed on signature/schema/runtime mismatch; and all candidate outputs remain research/candidate until later shadow gates are met.

---

### Task 1: Define the feature registry, observation record, and documentation contract

 **Files:**
 - Create: `crates/feature-registry/Cargo.toml`
- Create: `crates/feature-registry/src/lib.rs`
- Create: `crates/feature-registry/src/definition.rs`
- Create: `crates/feature-registry/src/observation.rs`
- Create: `docs/data-dictionary/features.md`
- Test: `crates/feature-registry/tests/registry.rs`

 **Interfaces:**
 - Consumes: canonical entities, quality states, semantic versions, and event-time fields
 - Produces: `FeatureDefinition`, `FeatureObservation`, required/optional/experimental classification, formula hashes, missingness reasons, and duplicate-ID/version rejection

 **Implementation notes**

 The registry is data, not a Rust macro hidden from audit. Generate a machine-readable registry snapshot and hash it into training datasets and model packages.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/feature-registry/tests/registry.rs` with:

 ```text
 use feature_registry::{FeatureDefinition, FeatureRegistry, FeatureStatus};

#[test]
fn duplicate_id_and_version_is_rejected() {
    let mut registry = FeatureRegistry::new();
    let definition = FeatureDefinition::fixture("realized_volatility", "1.0.0", FeatureStatus::Required);
    registry.register(definition.clone()).unwrap();
    assert!(registry.register(definition).is_err());
}

#[test]
fn observation_requires_as_known_at_and_lineage() {
    assert!(feature_registry::FeatureObservation::invalid_fixture_without_lineage().validate().is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p feature-registry`

 Expected: FAIL because registry and observation contracts are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/feature-registry/src/observation.rs` with:

 ```text
 use domain::EntityId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalityState { Provisional, Final, Corrected, Invalid }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingnessReason {
    NotListed, SourceNotSupported, SourceDisconnected, SequenceGap, Stale,
    InsufficientHistory, WindowNotFinal, BelowLiquidityThreshold,
    VendorRevisionPending, ModelNotApplicable, LicenseRestriction, Unknown,
}

#[derive(Clone, Debug)]
pub struct FeatureObservation {
    pub feature_id: String,
    pub feature_version: semver::Version,
    pub entity_id: EntityId,
    pub window_id: String,
    pub value: FeatureValue,
    pub event_time_start_ns: i64,
    pub event_time_end_ns: i64,
    pub as_known_at_ns: i64,
    pub computed_at_ns: i64,
    pub watermark_ns: i64,
    pub finality: FinalityState,
    pub quality_score: f64,
    pub missingness_reason: Option<MissingnessReason>,
    pub formula_hash: [u8; 32],
    pub lineage_hash: [u8; 32],
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p feature-registry`

 Expected: PASS with duplicate, invalid-time, invalid-quality, missingness, formula-hash, and documentation-link tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p feature-registry && cargo clippy -p feature-registry --all-targets -- -D warnings`

 Expected: registry tests pass and every required feature has a data-dictionary entry

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/feature-registry docs/data-dictionary/features.md Cargo.toml Cargo.lock
 git commit -m "feat: define versioned feature contracts"
 ```
### Task 2: Implement event-time watermarks, windowing, lateness, and deterministic timers

 **Files:**
 - Create: `crates/feature-engine/Cargo.toml`
- Create: `crates/feature-engine/src/lib.rs`
- Create: `crates/feature-engine/src/watermark.rs`
- Create: `crates/feature-engine/src/window.rs`
- Create: `crates/feature-engine/src/clock.rs`
- Test: `crates/feature-engine/tests/watermarks.rs`

 **Interfaces:**
 - Consumes: normalized events, feature registry definitions, replay clock, and source-quality state
 - Produces: `WatermarkTracker`, tumbling/sliding/EWMA/event-count/notional windows, provisional/final/corrected emissions, and deterministic live/replay timer abstraction

 **Implementation notes**

 Use integer UTC nanoseconds internally. A correction emits a new observation version rather than mutating a sealed historical record. The replay clock advances from recorded event/receive time and never reads the machine clock.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/feature-engine/tests/watermarks.rs` with:

 ```text
 use feature_engine::{Finalization, WatermarkTracker, WindowSpec};

#[test]
fn window_finalizes_only_after_all_required_watermarks_and_lateness() {
    let mut tracker = WatermarkTracker::fixture(["binance", "kraken"], 5_000_000_000);
    tracker.advance("binance", 60_000_000_000).unwrap();
    assert_eq!(tracker.status(WindowSpec::minute(0)), Finalization::Provisional);
    tracker.advance("kraken", 65_000_000_000).unwrap();
    assert_eq!(tracker.status(WindowSpec::minute(0)), Finalization::Final);
}

#[test]
fn watermark_never_moves_backward() {
    let mut tracker = WatermarkTracker::fixture(["binance"], 0);
    tracker.advance("binance", 10).unwrap();
    assert!(tracker.advance("binance", 9).is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p feature-engine --test watermarks`

 Expected: FAIL because watermark and window APIs are missing

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/feature-engine/src/watermark.rs` with:

 ```text
 use std::collections::BTreeMap;

pub struct WatermarkTracker {
    required: BTreeMap<String, i64>,
    allowed_lateness_ns: i64,
}

impl WatermarkTracker {
    pub fn advance(&mut self, source: &str, event_time_ns: i64) -> Result<(), WatermarkError> {
        let current = self.required.get_mut(source).ok_or_else(|| WatermarkError::UnknownSource(source.into()))?;
        if event_time_ns < *current { return Err(WatermarkError::Regression { current: *current, attempted: event_time_ns }); }
        *current = event_time_ns;
        Ok(())
    }

    pub fn effective(&self) -> Option<i64> {
        self.required.values().min().copied().map(|v| v - self.allowed_lateness_ns)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p feature-engine --test watermarks`

 Expected: PASS for required/optional sources, late correction, invalid source, replay equivalence, DST-independent UTC windows, and timer cancellation

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p feature-engine && cargo clippy -p feature-engine --all-targets -- -D warnings`

 Expected: all window families pass deterministic live/replay fixtures

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/feature-engine Cargo.toml Cargo.lock
 git commit -m "feat: add point-in-time window engine"
 ```
### Task 3: Build consolidated fair price and venue inclusion state

 **Files:**
 - Create: `crates/consolidated-market/Cargo.toml`
- Create: `crates/consolidated-market/src/lib.rs`
- Create: `crates/consolidated-market/src/fair_price.rs`
- Create: `crates/consolidated-market/src/stablecoin.rs`
- Test: `crates/consolidated-market/tests/fair_price.rs`

 **Interfaces:**
 - Consumes: trusted venue books, instrument conversion, stablecoin quotes, and quality records
 - Produces: `FairPriceEstimator`, capped depth/quality/freshness weights, stablecoin-adjusted quote conversion, robust median, venue exclusion reasons, and consolidated-state lineage

 **Implementation notes**

 Never call a midprice-only gap executable. Fee, lot, settlement, depth, and latency adjustments are separate derived features. Stablecoin adjustment uses a versioned reference and abstains during insufficient cross-venue coverage.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/consolidated-market/tests/fair_price.rs` with:

 ```text
 use consolidated_market::{FairPriceEstimator, VenueQuote};

#[test]
fn unhealthy_outlier_venue_cannot_move_fair_price() {
    let quotes = vec![
        VenueQuote::healthy("a", 100.0, 10.0),
        VenueQuote::healthy("b", 101.0, 10.0),
        VenueQuote::unhealthy("c", 1000.0, 1000.0),
    ];
    let result = FairPriceEstimator::default().estimate(&quotes).unwrap();
    assert!((result.price - 100.5).abs() < 0.6);
    assert!(result.excluded_venues.contains_key("c"));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p consolidated-market`

 Expected: FAIL because consolidated pricing APIs are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/consolidated-market/src/fair_price.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct VenueQuote {
    pub venue: String,
    pub midpoint: f64,
    pub executable_depth_usd: f64,
    pub quality: f64,
    pub freshness: f64,
    pub eligible: bool,
}

pub struct FairPriceEstimator { pub max_venue_weight: f64 }

impl FairPriceEstimator {
    pub fn estimate(&self, quotes: &[VenueQuote]) -> Result<FairPrice, FairPriceError> {
        let mut eligible: Vec<_> = quotes.iter().filter(|q| q.eligible && q.quality > 0.0 && q.freshness > 0.0).collect();
        if eligible.len() < 2 { return Err(FairPriceError::InsufficientHealthyVenues); }
        eligible.sort_by(|a,b| a.midpoint.total_cmp(&b.midpoint));
        weighted_capped_median(&eligible, self.max_venue_weight)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p consolidated-market`

 Expected: PASS for outliers, one-venue insufficiency, stale quote, stablecoin adjustment, inverse contract conversion, and weight-cap tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p consolidated-market && cargo clippy -p consolidated-market --all-targets -- -D warnings`

 Expected: consolidated market tests pass and every exclusion is represented in lineage/quality output

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/consolidated-market Cargo.toml Cargo.lock
 git commit -m "feat: add quality-aware consolidated market state"
 ```
### Task 4: Implement price, return, volatility, semivariance, and jump features

 **Files:**
 - Create: `crates/feature-engine/src/features/price.rs`
- Create: `crates/feature-engine/src/features/volatility.rs`
- Create: `crates/feature-engine/src/features/mod.rs`
- Create: `crates/volatility/Cargo.toml`
- Create: `crates/volatility/src/measures.rs`
- Test: `crates/volatility/tests/reference_measures.rs`

 **Interfaces:**
 - Consumes: point-in-time windows and consolidated prices
 - Produces: multi-horizon returns, ranges, VWAP distances, drawdown/run-up, realized volatility/semivariance, Parkinson, Garman–Klass, bipower/jump variation, and seasonality-adjusted measures

 **Implementation notes**

 Invalid analytical input produces a typed missing observation or NaN only inside an internal function immediately converted to missingness; persisted feature values never silently store nonfinite numbers.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/volatility/tests/reference_measures.rs` with:

 ```text
 use volatility::{bipower_variation, realized_variance, downside_semivariance};

#[test]
fn reference_path_matches_hand_calculation() {
    let returns = [0.01, -0.02, 0.03];
    assert!((realized_variance(&returns) - 0.0014).abs() < 1e-12);
    assert!((downside_semivariance(&returns) - 0.0004).abs() < 1e-12);
    assert!(bipower_variation(&returns).unwrap() >= 0.0);
}

#[test]
fn nonfinite_input_is_rejected() {
    assert!(realized_variance(&[f64::NAN]).is_nan());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p volatility`

 Expected: FAIL because volatility measures are missing

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/volatility/src/measures.rs` with:

 ```text
 pub fn realized_variance(returns: &[f64]) -> f64 {
    if returns.iter().any(|v| !v.is_finite()) { return f64::NAN; }
    returns.iter().map(|v| v * v).sum()
}

pub fn downside_semivariance(returns: &[f64]) -> f64 {
    if returns.iter().any(|v| !v.is_finite()) { return f64::NAN; }
    returns.iter().filter(|v| **v < 0.0).map(|v| v * v).sum()
}

pub fn bipower_variation(returns: &[f64]) -> Result<f64, MeasureError> {
    if returns.len() < 2 || returns.iter().any(|v| !v.is_finite()) { return Err(MeasureError::InvalidInput); }
    let scale = std::f64::consts::PI / 2.0;
    Ok(scale * returns.windows(2).map(|w| w[0].abs() * w[1].abs()).sum::<f64>())
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p volatility`

 Expected: PASS with reference, minimum-history, flat-market, jump, OHLC validity, and rolling-window tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p volatility -p feature-engine && cargo clippy -p volatility -p feature-engine --all-targets -- -D warnings`

 Expected: measure tests and feature emission/lineage tests pass at all declared resolutions

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/volatility crates/feature-engine/src/features Cargo.toml Cargo.lock
 git commit -m "feat: add return and volatility features"
 ```
### Task 5: Implement order-book and order-flow feature families

 **Files:**
 - Create: `crates/feature-engine/src/features/orderbook.rs`
- Create: `crates/feature-engine/src/features/orderflow.rs`
- Create: `crates/feature-engine/tests/microstructure_features.rs`

 **Interfaces:**
 - Consumes: trusted book snapshots, trades, fixed-point notional conversion, and window engine
 - Produces: spread/depth/imbalance/microprice/slope/convexity/gaps/sweep cost/resiliency plus signed flow, OFI, cancellation, intensity, impact, adverse-selection, and inferred-aggressor metadata

 **Implementation notes**

 Aggressor inference is venue/version specific and emits an `inferred` boolean. L3-only features remain experimental and are absent when the source or license does not support them.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/feature-engine/tests/microstructure_features.rs` with:

 ```text
 use feature_engine::features::{microprice, normalized_imbalance, sweep_cost};
use orderbook::BookSnapshot;

#[test]
fn microprice_and_imbalance_match_reference_book() {
    let book = BookSnapshot::fixture_top(100, 10, 102, 30);
    assert!((microprice(&book).unwrap() - 100.5).abs() < 1e-12);
    assert!((normalized_imbalance(&book, 100).unwrap() + 0.5).abs() < 1e-12);
}

#[test]
fn sweep_cost_rejects_untrusted_book() {
    assert!(sweep_cost(&BookSnapshot::untrusted_fixture(), 10_000).is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p feature-engine --test microstructure_features`

 Expected: FAIL because microstructure feature functions are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/feature-engine/src/features/orderbook.rs` with:

 ```text
 pub fn microprice(book: &orderbook::BookSnapshot) -> Result<f64, FeatureError> {
    book.require_trusted()?;
    let bid = book.best_bid().ok_or(FeatureError::EmptyBook)?;
    let ask = book.best_ask().ok_or(FeatureError::EmptyBook)?;
    let qb = bid.quantity.to_f64_checked()?;
    let qa = ask.quantity.to_f64_checked()?;
    let total = qb + qa;
    if total <= 0.0 { return Err(FeatureError::ZeroTopQuantity); }
    Ok((ask.price.to_f64_checked()? * qb + bid.price.to_f64_checked()? * qa) / total)
}

pub fn normalized_imbalance(book: &orderbook::BookSnapshot, band_bps: u32) -> Result<f64, FeatureError> {
    let (bid, ask) = book.depth_within_bps(band_bps)?;
    let total = bid + ask;
    if total <= 0.0 { return Err(FeatureError::ZeroDepth); }
    Ok((bid - ask) / total)
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p feature-engine --test microstructure_features`

 Expected: PASS for all depth bands, locked/crossed books, zero quantities, sweep limits, replenishment, inferred side, and event-count/notional windows

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p feature-engine && cargo clippy -p feature-engine --all-targets -- -D warnings`

 Expected: all microstructure features emit declared missingness/quality and preserve fixed-to-float conversion tolerances

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/feature-engine/src/features crates/feature-engine/tests/microstructure_features.rs
 git commit -m "feat: add microstructure feature families"
 ```
### Task 6: Implement cross-venue, derivatives, leverage, and operational-quality features

 **Files:**
 - Create: `crates/feature-engine/src/features/cross_venue.rs`
- Create: `crates/feature-engine/src/features/derivatives.rs`
- Create: `crates/feature-engine/src/features/quality.rs`
- Test: `crates/feature-engine/tests/cross_venue_derivatives.rs`

 **Interfaces:**
 - Consumes: consolidated state, funding/OI/mark/index/basis/liquidation observations, and source quality
 - Produces: price/depth dispersion, venue shares/concentration, spot-perpetual disagreement, funding/OI/basis/liquidation stress, source latency/gap/checksum/staleness, and completeness-aware cascade inputs

 **Implementation notes**

 An executable-dispersion feature includes fee, lot, depth, settlement, and latency inputs. Source outage indicators are separated into model features versus gating fields so unavailable signals are not accidentally learned as crash precursors without an outage ablation.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/feature-engine/tests/cross_venue_derivatives.rs` with:

 ```text
 use feature_engine::features::{annualized_basis, liquidation_velocity};

#[test]
fn annualized_basis_uses_seconds_to_expiry_and_rejects_expired_contract() {
    let value = annualized_basis(101.0, 100.0, 30 * 86_400).unwrap();
    assert!((value - 0.1216666667).abs() < 1e-8);
    assert!(annualized_basis(101.0, 100.0, 0).is_err());
}

#[test]
fn sampled_liquidations_reduce_coverage_notional_confidence() {
    let obs = feature_engine::fixtures::sampled_liquidations();
    let feature = liquidation_velocity(&obs).unwrap();
    assert!(feature.source_coverage < 1.0);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p feature-engine --test cross_venue_derivatives`

 Expected: FAIL because derivative and quality features are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/feature-engine/src/features/derivatives.rs` with:

 ```text
 pub fn annualized_basis(future: f64, spot: f64, seconds_to_expiry: i64) -> Result<f64, FeatureError> {
    if !future.is_finite() || !spot.is_finite() || spot <= 0.0 || seconds_to_expiry <= 0 {
        return Err(FeatureError::InvalidInput);
    }
    let years = seconds_to_expiry as f64 / (365.0 * 86_400.0);
    Ok((future / spot - 1.0) / years)
}

pub fn open_interest_destruction(price_return: f64, oi_change: f64) -> Result<f64, FeatureError> {
    if !price_return.is_finite() || !oi_change.is_finite() { return Err(FeatureError::InvalidInput); }
    Ok((-oi_change).max(0.0) * price_return.abs())
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p feature-engine --test cross_venue_derivatives`

 Expected: PASS for fees/contract conversion, dispersion, stale venue, funding cadence, inverse OI, sampled liquidation, and source-health interactions

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p feature-engine && cargo clippy -p feature-engine --all-targets -- -D warnings`

 Expected: cross-venue/derivative/quality tests pass and no sampled feed is promoted to complete

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/feature-engine/src/features crates/feature-engine/tests/cross_venue_derivatives.rs
 git commit -m "feat: add cross-venue and leverage features"
 ```
### Task 7: Persist point-in-time feature datasets and prove stream/batch parity

 **Files:**
 - Create: `crates/feature-engine/src/materialize.rs`
- Create: `crates/feature-engine/src/lineage.rs`
- Create: `fixtures/golden-replays/features-v1/manifest.toml`
- Test: `crates/feature-engine/tests/stream_batch_parity.rs`

 **Interfaces:**
 - Consumes: feature registry, live/replay window engine, Parquet store, and golden market replay
 - Produces: feature materializer for live/WAL/Parquet inputs, lineage DAG hashes, partition manifests, correction versions, and exact fixed-point/numerically bounded floating parity reports

 **Implementation notes**

 A finalized feature is immutable. Corrections create a new dataset version linked to the prior observation. Training queries join on `as_known_at <= prediction_time` and assert this condition in generated SQL/DataFusion plans.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/feature-engine/tests/stream_batch_parity.rs` with:

 ```text
 use feature_engine::{MaterializationMode, Materializer};

#[test]
fn live_wal_and_parquet_paths_produce_same_feature_digest() {
    let manifest = "fixtures/golden-replays/features-v1/manifest.toml";
    let live = Materializer::run_fixture(manifest, MaterializationMode::LiveSimulation).unwrap();
    let wal = Materializer::run_fixture(manifest, MaterializationMode::WalReplay).unwrap();
    let parquet = Materializer::run_fixture(manifest, MaterializationMode::ParquetReplay).unwrap();
    assert_eq!(live.fixed_point_digest, wal.fixed_point_digest);
    assert_eq!(wal.fixed_point_digest, parquet.fixed_point_digest);
    assert!(live.float_max_abs_error(parquet) <= 1e-12);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p feature-engine --test stream_batch_parity`

 Expected: FAIL because materialization modes and golden manifest are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/feature-engine/src/lineage.rs` with:

 ```text
 #[derive(Clone, Debug, serde::Serialize)]
pub struct FeatureLineage {
    pub feature_id: String,
    pub feature_version: semver::Version,
    pub formula_hash: [u8; 32],
    pub input_event_hashes: Vec<[u8; 32]>,
    pub source_coverage_hash: [u8; 32],
    pub code_commit: String,
}

impl FeatureLineage {
    pub fn digest(&self) -> Result<[u8; 32], LineageError> {
        let bytes = postcard::to_stdvec(self)?;
        Ok(*blake3::hash(&bytes).as_bytes())
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p feature-engine --test stream_batch_parity`

 Expected: PASS with identical fixed-point digests and declared floating tolerances

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo run -p crypto-replay -- --manifest fixtures/golden-replays/features-v1/manifest.toml --verify-features && cargo test -p feature-engine`

 Expected: replay verification reports matching event, window, feature, finality, quality, and lineage hashes

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/feature-engine/src/materialize.rs crates/feature-engine/src/lineage.rs fixtures/golden-replays/features-v1
 git commit -m "feat: persist auditable point-in-time features"
 ```
### Task 8: Implement versioned event labels, competing outcomes, censoring, and exclusions

 **Files:**
 - Create: `crates/labels/Cargo.toml`
- Create: `crates/labels/src/lib.rs`
- Create: `crates/labels/src/first_passage.rs`
- Create: `crates/labels/src/definitions.rs`
- Create: `docs/data-dictionary/labels.md`
- Test: `crates/labels/tests/reference_labels.rs`

 **Interfaces:**
 - Consumes: point-in-time prices, volatility, books, liquidations/OI, source health, and v1 horizons
 - Produces: downside/upside/volatility/liquidity/liquidation labels, absolute and volatility-scaled thresholds, first-passage times, overlap policy, right censoring, exclusions, and definition hashes

 **Implementation notes**

 Liquidation cascade requires price movement, elevated liquidation evidence, OI destruction, book deterioration, and cross-source corroboration; no single incomplete liquidation stream defines the event. Exact numeric thresholds are versioned in checked definition files and reviewed as model semantics.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/labels/tests/reference_labels.rs` with:

 ```text
 use labels::{LabelEngine, LabelOutcome};

#[test]
fn first_passage_uses_first_threshold_crossing_not_window_close() {
    let path = labels::fixtures::price_path([100.0, 99.0, 94.0, 96.0]);
    let outcome = LabelEngine::downside_fixture(0.05).label(&path).unwrap();
    assert_eq!(outcome, LabelOutcome::Occurred { offset_seconds: 120 });
}

#[test]
fn outage_before_horizon_is_right_censored() {
    let path = labels::fixtures::censored_path();
    assert!(matches!(LabelEngine::downside_fixture(0.05).label(&path).unwrap(), LabelOutcome::Censored { .. }));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p labels`

 Expected: FAIL because label definitions and outcomes are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/labels/src/lib.rs` with:

 ```text
 #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventType { Downside, Upside, VolatilityExplosion, LiquidityVacuum, LiquidationCascade }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelOutcome {
    Occurred { offset_seconds: u64 },
    NotOccurred,
    Censored { observed_seconds: u64 },
    Excluded(ExclusionReason),
}

#[derive(Clone, Debug)]
pub struct LabelDefinition {
    pub id: String,
    pub version: semver::Version,
    pub event_type: EventType,
    pub horizons_seconds: Vec<u64>,
    pub absolute_threshold: Option<f64>,
    pub volatility_multiple: Option<f64>,
    pub minimum_duration_seconds: u64,
    pub definition_hash: [u8; 32],
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p labels`

 Expected: PASS for first passage, volatility scaling, persistence, liquidity joint conditions, liquidation corroboration, overlap, censoring, and unhealthy-source exclusions

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p labels && cargo clippy -p labels --all-targets -- -D warnings`

 Expected: all label reference fixtures and definition-hash snapshots pass

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/labels docs/data-dictionary/labels.md Cargo.toml Cargo.lock
 git commit -m "feat: add versioned market-transition labels"
 ```
### Task 9: Build dataset manifests and purged nested walk-forward folds

 **Files:**
 - Create: `crates/dataset/Cargo.toml`
- Create: `crates/dataset/src/lib.rs`
- Create: `crates/dataset/src/manifest.rs`
- Create: `crates/dataset/src/folds.rs`
- Test: `crates/dataset/tests/no_leakage.rs`

 **Interfaces:**
 - Consumes: feature/label datasets, instrument history, as-known-at timestamps, and maximum outcome horizon
 - Produces: `DatasetManifest`, point-in-time joins, exclusion counts, partition hashes, inner/outer time folds, purge/embargo ranges, and leakage assertions

 **Implementation notes**

 Random splits are not exposed by the production evaluation API. Inner-fold selection, outer training, calibration, and untouched test segments are explicit types so callers cannot reuse the test segment for fitting.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/dataset/tests/no_leakage.rs` with:

 ```text
 use dataset::{FoldBuilder, Sample};

#[test]
fn embargo_covers_max_horizon_and_publication_lag() {
    let samples = Sample::daily_fixture(500);
    let folds = FoldBuilder::new(86_400, 7_200).outer_folds(&samples).unwrap();
    for fold in folds {
        assert!(fold.training_end_ns + (86_400 + 7_200) * 1_000_000_000 <= fold.test_start_ns);
    }
}

#[test]
fn join_rejects_feature_known_after_prediction() {
    assert!(dataset::join_point_in_time(dataset::fixtures::late_feature()).is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p dataset`

 Expected: FAIL because manifest and fold builders are missing

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/dataset/src/manifest.rs` with:

 ```text
 #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DatasetManifest {
    pub dataset_id: String,
    pub dataset_version: semver::Version,
    pub created_at_ns: i64,
    pub as_known_at_cutoff_ns: i64,
    pub entity_universe_hash: [u8; 32],
    pub source_versions: std::collections::BTreeMap<String, String>,
    pub schema_versions: std::collections::BTreeMap<String, u32>,
    pub feature_versions: std::collections::BTreeMap<String, semver::Version>,
    pub label_versions: std::collections::BTreeMap<String, semver::Version>,
    pub partition_hashes: Vec<[u8; 32]>,
    pub exclusion_counts: std::collections::BTreeMap<String, u64>,
    pub code_commit: String,
    pub license_manifest_hash: [u8; 32],
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p dataset`

 Expected: PASS for as-known-at joins, overlapping labels, purge, embargo, universe membership, delistings, revisions, and fold reproducibility

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p dataset && cargo clippy -p dataset --all-targets -- -D warnings`

 Expected: dataset tests pass and repeated fold construction yields the same manifest/fold hashes

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/dataset Cargo.toml Cargo.lock
 git commit -m "feat: add point-in-time datasets and walk-forward folds"
 ```
### Task 10: Implement historical, seasonal, and rolling base-rate forecasters

 **Files:**
 - Create: `crates/baselines/Cargo.toml`
- Create: `crates/baselines/src/lib.rs`
- Create: `crates/baselines/src/base_rate.rs`
- Test: `crates/baselines/tests/base_rate.rs`

 **Interfaces:**
 - Consumes: walk-forward folds, event labels, assets, horizons, and optional regime/time-of-week strata
 - Produces: `BaseRateModel` with unconditional, rolling, time-of-week, and predeclared regime-conditioned estimates plus empirical uncertainty and minimum-count fallback

 **Implementation notes**

 Persist baseline forecasts with the same forecast/evidence identifiers used later. A fallback is labeled, not presented as the requested fine-grained estimate.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/baselines/tests/base_rate.rs` with:

 ```text
 use baselines::BaseRateModel;

#[test]
fn test_period_outcomes_never_enter_base_rate_fit() {
    let data = baselines::fixtures::train_and_test_with_different_rates();
    let model = BaseRateModel::fit(&data.train).unwrap();
    assert!((model.probability("BTC", 3600).unwrap() - 0.10).abs() < 1e-12);
}

#[test]
fn sparse_stratum_falls_back_with_explicit_level() {
    let result = BaseRateModel::fixture_sparse().probability_with_provenance("BTC", 3600).unwrap();
    assert_eq!(result.fallback_level, 1);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p baselines`

 Expected: FAIL because baseline model APIs are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/baselines/src/base_rate.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct BaseRateEstimate {
    pub probability: f64,
    pub lower: f64,
    pub upper: f64,
    pub positive_count: u64,
    pub total_count: u64,
    pub fallback_level: u8,
}

pub fn beta_binomial_estimate(positive: u64, total: u64, alpha: f64, beta: f64) -> Result<BaseRateEstimate, BaselineError> {
    if positive > total || alpha <= 0.0 || beta <= 0.0 { return Err(BaselineError::InvalidInput); }
    let probability = (positive as f64 + alpha) / (total as f64 + alpha + beta);
    let (lower, upper) = credible_interval(positive, total, alpha, beta)?;
    Ok(BaseRateEstimate { probability, lower, upper, positive_count: positive, total_count: total, fallback_level: 0 })
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p baselines`

 Expected: PASS for no-leakage, sparse fallback, seasonal strata, censoring, and uncertainty tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p baselines && cargo clippy -p baselines --all-targets -- -D warnings`

 Expected: baseline tests pass and every future candidate can compare against a persisted base-rate forecast

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/baselines Cargo.toml Cargo.lock
 git commit -m "feat: add auditable event base-rate models"
 ```
### Task 11: Implement EWMA and HAR-RV volatility forecasts with walk-forward selection

 **Files:**
 - Create: `crates/volatility/src/ewma.rs`
- Create: `crates/volatility/src/har.rs`
- Create: `crates/volatility/src/forecast.rs`
- Test: `crates/volatility/tests/forecast_models.rs`

 **Interfaces:**
 - Consumes: realized volatility features, walk-forward folds, and training-only normalization
 - Produces: `EwmaVolatility`, ridge-stabilized `HarRvModel`, forecast uncertainty, model-selection report, and volatility-normalized state input

 **Implementation notes**

 The reference value in the test must be recalculated from the exact initialization rule committed in the model documentation; keep the test and formula synchronized. The selector reports both models even when one wins.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/volatility/tests/forecast_models.rs` with:

 ```text
 use volatility::{EwmaVolatility, HarRvModel};

#[test]
fn ewma_matches_recursive_reference() {
    let mut model = EwmaVolatility::new(0.94).unwrap();
    for r in [0.01, -0.02, 0.03] { model.update(r).unwrap(); }
    assert!((model.variance().unwrap() - 0.00016492).abs() < 1e-10);
}

#[test]
fn har_fit_uses_training_rows_only() {
    let data = volatility::fixtures::har_train_test();
    let model = HarRvModel::fit(&data.train, 1e-6).unwrap();
    assert_eq!(model.fit_rows(), data.train.len());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p volatility --test forecast_models`

 Expected: FAIL because forecast model types are missing

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/volatility/src/ewma.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct EwmaVolatility { lambda: f64, variance: Option<f64> }

impl EwmaVolatility {
    pub fn new(lambda: f64) -> Result<Self, ForecastError> {
        if !(0.0..1.0).contains(&lambda) { return Err(ForecastError::InvalidLambda); }
        Ok(Self { lambda, variance: None })
    }
    pub fn update(&mut self, return_value: f64) -> Result<(), ForecastError> {
        if !return_value.is_finite() { return Err(ForecastError::NonFinite); }
        let squared = return_value * return_value;
        self.variance = Some(self.variance.map_or(squared, |v| self.lambda * v + (1.0 - self.lambda) * squared));
        Ok(())
    }
    pub fn variance(&self) -> Option<f64> { self.variance }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p volatility --test forecast_models`

 Expected: PASS for recursive references, ridge conditioning, uncertainty, missing history, and outer-fold selection tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p volatility && cargo clippy -p volatility --all-targets -- -D warnings`

 Expected: volatility selection report is reproducible and the chosen model never uses outer-test outcomes

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/volatility Cargo.toml Cargo.lock
 git commit -m "feat: add EWMA and HAR-RV forecasts"
 ```
### Task 12: Implement Bayesian online changepoint detection with versioned hazard and truncation

 **Files:**
 - Create: `crates/changepoint/Cargo.toml`
- Create: `crates/changepoint/src/lib.rs`
- Create: `crates/changepoint/src/nig.rs`
- Create: `crates/changepoint/src/bocpd.rs`
- Test: `crates/changepoint/tests/reference.rs`

 **Interfaces:**
 - Consumes: standardized feature streams and deterministic numerical utilities
 - Produces: `Bocpd` with Normal-Inverse-Gamma Student-t predictive, log-space run-length posterior, configured hazard, truncation, reset, entropy, expected run length, and quality output

 **Implementation notes**

 Use log-sum-exp and explicit underflow checks. Hazard prior, observation family, max run length, reset policy, and feature family are included in the model version and evidence bundle.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/changepoint/tests/reference.rs` with:

 ```text
 use changepoint::{Bocpd, BocpdConfig};

#[test]
fn abrupt_mean_shift_raises_changepoint_probability() {
    let mut model = Bocpd::new(BocpdConfig::fixture()).unwrap();
    for value in [0.0; 50] { model.update(value).unwrap(); }
    let before = model.changepoint_probability();
    model.update(8.0).unwrap();
    assert!(model.changepoint_probability() > before);
}

#[test]
fn posterior_stays_normalized_after_truncation() {
    let mut model = Bocpd::new(BocpdConfig::fixture_with_max_run_length(32)).unwrap();
    for value in (0..100).map(|v| v as f64 / 100.0) { model.update(value).unwrap(); }
    assert!((model.posterior_sum() - 1.0).abs() < 1e-12);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p changepoint`

 Expected: FAIL because BOCPD is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/changepoint/src/lib.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct BocpdOutput {
    pub changepoint_probability: f64,
    pub expected_run_length: f64,
    pub run_length_entropy: f64,
    pub posterior: Vec<f64>,
}

pub struct Bocpd {
    config: BocpdConfig,
    log_run_length: Vec<f64>,
    sufficient_statistics: Vec<nig::NormalInverseGamma>,
}

impl Bocpd {
    pub fn update(&mut self, value: f64) -> Result<BocpdOutput, BocpdError> {
        if !value.is_finite() { return Err(BocpdError::NonFiniteObservation); }
        self.update_log_posterior(value)?;
        self.normalize_and_truncate()?;
        Ok(self.output())
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p changepoint`

 Expected: PASS against a committed high-precision reference vector and shift/truncation/reset tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p changepoint && cargo clippy -p changepoint --all-targets -- -D warnings`

 Expected: BOCPD outputs remain normalized, deterministic, and finite for long fixtures

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/changepoint Cargo.toml Cargo.lock
 git commit -m "feat: add online changepoint detector"
 ```
### Task 13: Implement a Student-t hidden Markov regime model with neutral state identities

 **Files:**
 - Create: `crates/regime/Cargo.toml`
- Create: `crates/regime/src/lib.rs`
- Create: `crates/regime/src/forward_backward.rs`
- Create: `crates/regime/src/em.rs`
- Create: `crates/regime/src/describe.rs`
- Test: `crates/regime/tests/reference_hmm.rs`

 **Interfaces:**
 - Consumes: walk-forward datasets, selected standardized emissions, and deterministic seeds
 - Produces: `StudentTHmm` fitting/inference, 3–6 state selection, transition/stationary probabilities, smoothed/filtered posteriors, convergence diagnostics, and post-hoc state descriptions

 **Implementation notes**

 State labels such as “liquidity stress” are generated from fitted state statistics after training and stored separately from neutral state IDs. Use covariance regularization and a documented floor to avoid singular emissions.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/regime/tests/reference_hmm.rs` with:

 ```text
 use regime::{HmmConfig, StudentTHmm};

#[test]
fn transition_rows_are_stochastic_and_posteriors_normalize() {
    let data = regime::fixtures::two_regime_series();
    let model = StudentTHmm::fit(&data, HmmConfig::fixture(3)).unwrap();
    for row in model.transition_matrix().row_iter() {
        assert!((row.iter().sum::<f64>() - 1.0).abs() < 1e-10);
    }
    for posterior in model.filtered_probabilities(&data).unwrap() {
        assert!((posterior.iter().sum::<f64>() - 1.0).abs() < 1e-10);
    }
}

#[test]
fn internal_state_names_are_neutral() {
    assert_eq!(regime::StateId(0).to_string(), "state_0");
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p regime`

 Expected: FAIL because HMM types and algorithms are missing

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/regime/src/lib.rs` with:

 ```text
 #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct StateId(pub usize);

#[derive(Clone, Debug)]
pub struct FitDiagnostics {
    pub converged: bool,
    pub iterations: usize,
    pub log_likelihood: f64,
    pub max_parameter_change: f64,
    pub seed: u64,
}

pub struct StudentTHmm {
    state_count: usize,
    transition: nalgebra::DMatrix<f64>,
    initial: nalgebra::DVector<f64>,
    emissions: Vec<StudentTEmission>,
    diagnostics: FitDiagnostics,
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p regime`

 Expected: PASS for reference likelihood, normalization, convergence failure, state selection, deterministic seed, and post-hoc description tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p regime && cargo clippy -p regime --all-targets -- -D warnings`

 Expected: HMM tests pass and nonconverged fits cannot create candidate artifacts

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/regime Cargo.toml Cargo.lock
 git commit -m "feat: add Student-t regime model"
 ```
### Task 14: Implement an elastic-net discrete-time competing-risk baseline

 **Files:**
 - Create: `crates/hazard/Cargo.toml`
- Create: `crates/hazard/src/lib.rs`
- Create: `crates/hazard/src/design.rs`
- Create: `crates/hazard/src/softmax.rs`
- Create: `crates/hazard/src/elastic_net.rs`
- Test: `crates/hazard/tests/probability_contract.rs`

 **Interfaces:**
 - Consumes: purged training folds, competing event labels, training-only normalization, and deterministic optimization
 - Produces: `CompetingRiskHazard` softmax over event causes plus survival per bucket, elastic-net fitting, convergence diagnostics, and cumulative incidence curves for 15m/1h/4h/24h

 **Implementation notes**

 Use the elastic-net model as the first production-capable supervised baseline; gradient boosting remains a later candidate only after a reviewed dependency/artifact path. The test suite includes cause/survival conservation and horizon monotonicity.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/hazard/tests/probability_contract.rs` with:

 ```text
 use hazard::{CompetingRiskHazard, HazardConfig};

#[test]
fn cause_probabilities_and_survival_sum_to_one_per_bucket() {
    let model = CompetingRiskHazard::fixture();
    let output = model.predict(&[0.1, -0.2]).unwrap();
    for bucket in output.buckets {
        assert!((bucket.causes.iter().sum::<f64>() + bucket.survival - 1.0).abs() < 1e-12);
    }
}

#[test]
fn cumulative_incidence_is_monotone_by_horizon() {
    let curve = CompetingRiskHazard::fixture().predict(&[0.1, -0.2]).unwrap().cumulative_incidence;
    for values in curve.values() { assert!(values.windows(2).all(|w| w[1] + 1e-12 >= w[0])); }
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p hazard`

 Expected: FAIL because competing-risk model APIs are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/hazard/src/lib.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct BucketProbability {
    pub causes: Vec<f64>,
    pub survival: f64,
}

#[derive(Clone, Debug)]
pub struct HazardPrediction {
    pub buckets: Vec<BucketProbability>,
    pub cumulative_incidence: std::collections::BTreeMap<String, Vec<f64>>,
}

pub struct CompetingRiskHazard {
    coefficients: nalgebra::DMatrix<f64>,
    intercepts: nalgebra::DMatrix<f64>,
    config: HazardConfig,
    diagnostics: FitDiagnostics,
}

impl CompetingRiskHazard {
    pub fn predict(&self, features: &[f64]) -> Result<HazardPrediction, HazardError> {
        let logits = self.bucket_logits(features)?;
        probabilities_and_cumulative_incidence(&logits, &self.config)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p hazard`

 Expected: PASS for probability coherence, right censoring, equivalent bucket partitioning, convergence, regularization path, and deterministic fit tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p hazard && cargo clippy -p hazard --all-targets -- -D warnings`

 Expected: hazard tests pass and every fit exposes objective, gradient norm, iterations, conditioning, and convergence status

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/hazard Cargo.toml Cargo.lock
 git commit -m "feat: add competing-risk hazard baseline"
 ```
### Task 15: Create calibration, signed model packages, registry states, and evaluation reports

 **Files:**
 - Create: `crates/calibration/Cargo.toml`
- Create: `crates/calibration/src/lib.rs`
- Create: `crates/model-registry/Cargo.toml`
- Create: `crates/model-registry/src/lib.rs`
- Create: `crates/model-registry/src/package.rs`
- Create: `apps/crypto-evaluate/Cargo.toml`
- Create: `apps/crypto-evaluate/src/main.rs`
- Create: `models/schemas/model-package.schema.json`
- Test: `crates/model-registry/tests/lifecycle.rs`

 **Interfaces:**
 - Consumes: baseline, volatility, BOCPD, HMM, hazard models, dataset/fold manifests, and Ed25519 signing policy
 - Produces: Platt/beta/isotonic calibrators, probability metrics, immutable model package manifest, research→candidate→shadow→production lifecycle, signature verification, compatibility checks, and walk-forward evaluation CLI

 **Implementation notes**

 The CLI persists every attempted material model/feature/hyperparameter definition in the experiment ledger, including failures. Signing uses test keys only in fixtures; release keys never live in the repository or application bundle.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/model-registry/tests/lifecycle.rs` with:

 ```text
 use model_registry::{ModelRegistry, ModelState, PromotionError};

#[test]
fn production_promotion_requires_signature_metrics_and_independent_review() {
    let mut registry = ModelRegistry::fixture_candidate();
    assert!(matches!(registry.promote_fixture(ModelState::Production), Err(PromotionError::MissingIndependentReview)));
}

#[test]
fn revoked_model_cannot_issue_new_forecasts_but_remains_auditable() {
    let registry = ModelRegistry::fixture_revoked();
    assert!(!registry.may_infer_production("model-1"));
    assert!(registry.get("model-1").is_ok());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p model-registry`

 Expected: FAIL because calibration and registry/package APIs are missing

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/model-registry/src/package.rs` with:

 ```text
 #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelPackageManifest {
    pub model_id: String,
    pub semantic_version: semver::Version,
    pub model_family: String,
    pub supported_assets: Vec<String>,
    pub supported_event_types: Vec<String>,
    pub supported_horizons_seconds: Vec<u64>,
    pub training_start_ns: i64,
    pub training_cutoff_ns: i64,
    pub data_hash: [u8; 32],
    pub feature_schema_hash: [u8; 32],
    pub normalization_hash: [u8; 32],
    pub label_definition_hash: [u8; 32],
    pub code_commit: String,
    pub calibration_method: String,
    pub quality_requirements: QualityRequirements,
    pub runtime_requirements: RuntimeRequirements,
    pub artifact_hash: [u8; 32],
    pub signature: Vec<u8>,
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p model-registry`

 Expected: PASS for state transitions, signature, schema/feature/label/runtime mismatch, revocation, calibrator monotonicity, and metric serialization tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo run -p crypto-evaluate -- --manifest fixtures/golden-replays/features-v1/manifest.toml --output target/evaluation/baselines && cargo test -p calibration -p model-registry`

 Expected: evaluation produces reproducible fold metrics, reliability data, ablations, package/card hashes, and a candidate registry entry without using the outer test for calibration

 - [ ] **Step 6: Inspect point-in-time and lineage behavior in the diff**

 Run: `git diff --check && git status --short`

 Expected: no full-history normalization, future fill, random split, silent zero imputation, or unversioned formula is introduced.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/calibration crates/model-registry apps/crypto-evaluate models/schemas/model-package.schema.json Cargo.toml Cargo.lock
 git commit -m "feat: add calibration model packages and evaluation"
 ```
