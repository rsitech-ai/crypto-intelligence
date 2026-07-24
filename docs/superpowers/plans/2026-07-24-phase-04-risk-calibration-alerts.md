# Phase 04 — Fast Risk, Calibration, Scenarios, and Alerts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert structural and fast market state into coherent, calibrated, quality-gated multi-horizon transition probabilities, durable evidence, conditional scenarios, and auditable alerts under the reference latency budget.

**Architecture:** Immutable fast-state snapshots feed the Phase 2 competing-risk model and out-of-fold-trained meta-stacker, with cusp input enabled only by its Phase 3 gate. Separate validation calibrators and applicability/OOD policy produce final curves or explicit abstention. A local scenario engine and evidence builder feed an append-only forecast ledger; publication and alert evaluation occur only after durable persistence. Authenticated gRPC services isolate slow UI clients from ingestion and inference.

**Tech Stack:** Rust/Tokio, nalgebra, Tonic/Tower, Parquet/SQLite audit stores, deterministic Monte Carlo, typed alert DSL, proptest/numerical fixtures, Criterion-style performance harnesses.

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

This plan implements the primary production forecast path: 100 ms/1 s trigger state, hazard curves, leakage-safe stacking, calibration, applicability/OOD, scenario distributions, immutable forecast/evidence records, alert rules/lifecycle, risk pipeline, local API, golden replay, and latency/coherence gates. It excludes new source families, coupled cusp, native Swift screens, and Apple model frameworks.

## File and module map

- `crates/fast-state`: immutable high-frequency trigger snapshots and cadence.
- `crates/hazard`: bucket definitions, competing-risk probabilities, and cumulative incidence.
- `crates/ensemble`: out-of-fold module matrix, constrained stacker, and ablations.
- `crates/calibration`: validation-only calibrators and selection.
- `crates/applicability`: OOD, novelty, quality, compatibility, and abstention.
- `crates/scenarios`: local conditional path simulation and summaries.
- `crates/forecast-ledger`: durable forecast/evidence records and persistence receipts.
- `crates/alerts`: safe DSL, lifecycle, budget, persistence, recovery, and outcome review.
- `crates/risk-engine`: integrated inference transaction and model-bundle snapshot.
- `crates/local-api`: forecast/alert services, stream buffers, resume, and slow-client isolation.

## Exit gate

Forecast cause probabilities and survival conserve probability; horizon curves are monotone; all stacker inputs are out of fold; calibration uses a separate validation segment; applicability/abstention behaves as specified; forecasts and alerts persist before publication/delivery; replay reproduces forecast/evidence/alert hashes; slow clients cannot block core processing; and the documented reference hardware meets the Phase 4 latency and alert-budget improvement gates.

---

### Task 1: Create the 100 ms/1 s fast-state scheduler and immutable feature snapshots

 **Files:**
 - Create: `crates/fast-state/Cargo.toml`
- Create: `crates/fast-state/src/lib.rs`
- Create: `crates/fast-state/src/scheduler.rs`
- Create: `crates/fast-state/src/snapshot.rs`
- Test: `crates/fast-state/tests/cadence.rs`

 **Interfaces:**
 - Consumes: trusted order books/trades, feature engine, replay clock, and bounded compute pool
 - Produces: `FastStateScheduler`, 100 ms internal ticks, 1 s published snapshots, source watermark/quality capture, coalesced UI-only state, and deterministic replay cadence

 **Implementation notes**

 The 100 ms cadence is an internal evaluation schedule, not a claim that all venues supply complete 100 ms data. The scheduler snapshots immutable feature arrays so inference never reads partially updated state.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/fast-state/tests/cadence.rs` with:

 ```text
 use fast_state::{FastStateScheduler, TickKind};

#[test]
fn replay_emits_exact_internal_and_public_tick_counts() {
    let mut scheduler = FastStateScheduler::fixture(0);
    scheduler.advance_to(5_000_000_000).unwrap();
    assert_eq!(scheduler.count(TickKind::Internal100Ms), 50);
    assert_eq!(scheduler.count(TickKind::Published1S), 5);
}

#[test]
fn clock_regression_is_rejected() {
    let mut scheduler = FastStateScheduler::fixture(1_000);
    assert!(scheduler.advance_to(999).is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p fast-state`

 Expected: FAIL because fast cadence and snapshots are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/fast-state/src/snapshot.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct FastStateSnapshot {
    pub entity_id: domain::EntityId,
    pub as_of_event_time_ns: i64,
    pub as_known_at_ns: i64,
    pub feature_values: std::sync::Arc<[f64]>,
    pub missingness_mask: std::sync::Arc<[u64]>,
    pub source_coverage: f64,
    pub quality_score: f64,
    pub feature_schema_hash: [u8; 32],
    pub lineage_hash: [u8; 32],
}

impl FastStateSnapshot {
    pub fn validate(&self) -> Result<(), FastStateError> {
        if self.as_known_at_ns < self.as_of_event_time_ns || !(0.0..=1.0).contains(&self.quality_score) {
            return Err(FastStateError::InvalidSnapshot);
        }
        Ok(())
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p fast-state`

 Expected: PASS for cadence, replay equality, late events, missed ticks, quality capture, cancellation, and bounded compute backlog

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p fast-state && cargo clippy -p fast-state --all-targets -- -D warnings`

 Expected: fast-state tests pass with deterministic hashes and no source-ingestion blocking from a slow consumer

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/fast-state Cargo.toml Cargo.lock
 git commit -m "feat: add deterministic fast-state pipeline"
 ```
### Task 2: Add fast liquidity, flow, liquidation, and trigger-state aggregations

 **Files:**
 - Create: `crates/fast-state/src/features.rs`
- Create: `crates/fast-state/src/trigger_state.rs`
- Create: `crates/fast-state/tests/trigger_features.rs`

 **Interfaces:**
 - Consumes: Phase 2 microstructure/derivative features and current source quality
 - Produces: `TriggerState` with depth disappearance, cancellation burst, sweep direction, short-horizon OFI, cross-venue dispersion acceleration, liquidation/OI destruction, mark-index divergence, and feature age

 **Implementation notes**

 Do not collapse structural state and trigger state into one feature. Fast features answer whether current microstructure can trigger a transition; cusp and regime features describe the slower background.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/fast-state/tests/trigger_features.rs` with:

 ```text
 use fast_state::TriggerStateBuilder;

#[test]
fn liquidation_pressure_requires_completeness_weight_and_oi_confirmation() {
    let state = TriggerStateBuilder::fixture_sampled_liquidations_without_oi_drop().build().unwrap();
    assert!(state.liquidation_pressure < 0.5);
    let confirmed = TriggerStateBuilder::fixture_confirmed_cascade().build().unwrap();
    assert!(confirmed.liquidation_pressure > state.liquidation_pressure);
}

#[test]
fn stale_book_feature_is_missing_not_zero() {
    let state = TriggerStateBuilder::fixture_stale_book().build().unwrap();
    assert!(state.depth_disappearance.is_none());
    assert!(state.missingness.contains("stale_book"));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p fast-state --test trigger_features`

 Expected: FAIL because trigger-state aggregations are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/fast-state/src/trigger_state.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct TriggerState {
    pub depth_disappearance: Option<f64>,
    pub cancellation_burst: Option<f64>,
    pub order_flow_imbalance: Option<f64>,
    pub cross_venue_dispersion_acceleration: Option<f64>,
    pub liquidation_pressure: f64,
    pub open_interest_destruction: Option<f64>,
    pub mark_index_divergence: Option<f64>,
    pub maximum_feature_age_ns: i64,
    pub missingness: std::collections::BTreeSet<String>,
    pub quality_score: f64,
}

pub fn completeness_weighted_liquidation_pressure(velocity: f64, completeness: f64, oi_destruction: f64) -> Result<f64, TriggerError> {
    if !velocity.is_finite() || !(0.0..=1.0).contains(&completeness) || !oi_destruction.is_finite() { return Err(TriggerError::InvalidInput); }
    Ok((velocity.max(0.0) * completeness * (1.0 + oi_destruction.max(0.0))).ln_1p())
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p fast-state --test trigger_features`

 Expected: PASS for confirmed/unconfirmed liquidations, book staleness, source loss, dispersion, cancellation, sweep, and age/quality tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p fast-state && cargo clippy -p fast-state --all-targets -- -D warnings`

 Expected: trigger-state tests pass and completeness/quality effects are explicit in the feature record

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/fast-state/src/features.rs crates/fast-state/src/trigger_state.rs crates/fast-state/tests/trigger_features.rs
 git commit -m "feat: add fast transition trigger state"
 ```
### Task 3: Define production hazard buckets and cumulative-incidence conversion

 **Files:**
 - Create: `crates/hazard/src/buckets.rs`
- Create: `crates/hazard/src/incidence.rs`
- Create: `crates/hazard/tests/cumulative_incidence.rs`

 **Interfaces:**
 - Consumes: Phase 2 competing-risk model and v1 forecast horizons
 - Produces: fixed bucket edges covering 15m/1h/4h/24h, cause-plus-survival probability validation, cumulative incidence, conditional survival, and horizon interpolation policy

 **Implementation notes**

 Bucket edges are a versioned label/model contract. The initial grid is denser inside 15 minutes and progressively wider through 24 hours; exact edges are checked into the model schema and changing them changes the label/model version.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/hazard/tests/cumulative_incidence.rs` with:

 ```text
 use hazard::{cumulative_incidence, BucketProbability};

#[test]
fn cumulative_incidence_matches_manual_two_bucket_example() {
    let buckets = vec![
        BucketProbability { causes: vec![0.10, 0.20], survival: 0.70 },
        BucketProbability { causes: vec![0.20, 0.10], survival: 0.70 },
    ];
    let result = cumulative_incidence(&buckets).unwrap();
    assert!((result[0][1] - 0.24).abs() < 1e-12);
    assert!((result[1][1] - 0.27).abs() < 1e-12);
    assert!((result.total_survival[1] - 0.49).abs() < 1e-12);
}

#[test]
fn invalid_bucket_sum_is_rejected_not_renormalized() {
    let invalid = vec![BucketProbability { causes: vec![0.8, 0.5], survival: 0.0 }];
    assert!(cumulative_incidence(&invalid).is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p hazard --test cumulative_incidence`

 Expected: FAIL because bucket and incidence contracts are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/hazard/src/incidence.rs` with:

 ```text
 pub struct CumulativeIncidence {
    pub by_cause: Vec<Vec<f64>>,
    pub total_survival: Vec<f64>,
}

pub fn cumulative_incidence(buckets: &[crate::BucketProbability]) -> Result<CumulativeIncidence, crate::HazardError> {
    let cause_count = buckets.first().ok_or(crate::HazardError::EmptyBuckets)?.causes.len();
    let mut by_cause = vec![Vec::with_capacity(buckets.len()); cause_count];
    let mut totals = vec![0.0; cause_count];
    let mut survival = 1.0;
    let mut total_survival = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        bucket.validate(cause_count)?;
        for c in 0..cause_count { totals[c] += survival * bucket.causes[c]; by_cause[c].push(totals[c]); }
        survival *= bucket.survival;
        total_survival.push(survival);
    }
    Ok(CumulativeIncidence { by_cause, total_survival })
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p hazard --test cumulative_incidence`

 Expected: PASS for manual examples, invalid sums, monotonicity, empty buckets, underflow-safe long curves, and horizon extraction

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p hazard && cargo clippy -p hazard --all-targets -- -D warnings`

 Expected: hazard probability invariants pass without post-hoc clipping or renormalization

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/hazard/src/buckets.rs crates/hazard/src/incidence.rs crates/hazard/tests/cumulative_incidence.rs
 git commit -m "feat: formalize production hazard curves"
 ```
### Task 4: Build the out-of-fold module matrix and leakage-safe model input contract

 **Files:**
 - Create: `crates/ensemble/Cargo.toml`
- Create: `crates/ensemble/src/lib.rs`
- Create: `crates/ensemble/src/module_output.rs`
- Create: `crates/ensemble/src/oof_matrix.rs`
- Test: `crates/ensemble/tests/oof_only.rs`

 **Interfaces:**
 - Consumes: base rates, volatility, BOCPD, HMM, hazard, cusp eligibility/output, datasets, and folds
 - Produces: `ModuleOutput`, `OutOfFoldMatrix`, feature timestamps, module availability, and an assertion that stacker training consumes only predictions made by models not trained on that row

 **Implementation notes**

 Cusp columns remain available for research comparisons even when their production weight is locked to zero. Missing module output is represented by availability/missingness fields, not a zero score.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/ensemble/tests/oof_only.rs` with:

 ```text
 use ensemble::OutOfFoldMatrix;

#[test]
fn in_fold_prediction_is_rejected_from_stacker_training() {
    let rows = ensemble::fixtures::contains_in_fold_prediction();
    assert!(OutOfFoldMatrix::build(rows).is_err());
}

#[test]
fn experimental_cusp_column_is_present_but_weight_locked_to_zero() {
    let matrix = OutOfFoldMatrix::fixture_with_research_only_cusp().unwrap();
    assert!(matrix.columns().contains(&"cusp_region_probability"));
    assert!(matrix.constraints().is_weight_locked("cusp_region_probability"));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p ensemble`

 Expected: FAIL because ensemble module/OOF contracts are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/ensemble/src/module_output.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct ModuleOutput {
    pub module_id: String,
    pub model_package_id: String,
    pub entity_id: domain::EntityId,
    pub as_of_ns: i64,
    pub trained_through_ns: i64,
    pub outer_fold_id: String,
    pub values: std::collections::BTreeMap<String, f64>,
    pub availability: quality::AvailabilityState,
    pub evidence_hash: [u8; 32],
}

impl ModuleOutput {
    pub fn is_out_of_fold_for(&self, row: &TrainingRow) -> bool {
        self.outer_fold_id == row.outer_fold_id && self.trained_through_ns < row.test_start_ns
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p ensemble`

 Expected: PASS for in-fold rejection, timestamp leakage, missing modules, weight locks, schema order, and deterministic matrix hashes

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p ensemble && cargo clippy -p ensemble --all-targets -- -D warnings`

 Expected: OOF matrix tests pass and no production stacker can be fit from in-sample module scores

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/ensemble Cargo.toml Cargo.lock
 git commit -m "feat: create leakage-safe ensemble inputs"
 ```
### Task 5: Implement the regularized constrained meta-model and module ablations

 **Files:**
 - Create: `crates/ensemble/src/stacker.rs`
- Create: `crates/ensemble/src/constraints.rs`
- Create: `crates/ensemble/src/ablation.rs`
- Test: `crates/ensemble/tests/stacker.rs`

 **Interfaces:**
 - Consumes: out-of-fold module matrix, event/horizon targets, and module eligibility constraints
 - Produces: `MetaStacker` with regularized multinomial/logistic weights, optional nonnegative/simplex constraints, convergence diagnostics, per-module ablations, and deterministic inference

 **Implementation notes**

 Start with the simplest regularized stacker. A complex meta-model is disallowed until it wins predeclared walk-forward metrics without calibration degradation. Weight sign/constraints are configuration and model metadata, never hidden solver defaults.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/ensemble/tests/stacker.rs` with:

 ```text
 use ensemble::{MetaStacker, StackerConfig};

#[test]
fn locked_module_gets_exactly_zero_weight() {
    let data = ensemble::fixtures::oof_training_matrix();
    let model = MetaStacker::fit(&data, StackerConfig::fixture_with_locked("cusp")).unwrap();
    assert_eq!(model.weight("cusp"), Some(0.0));
}

#[test]
fn nonnegative_simplex_weights_respect_constraints() {
    let model = MetaStacker::fixture_simplex();
    let weights = model.module_weights();
    assert!(weights.iter().all(|w| *w >= -1e-12));
    assert!((weights.iter().sum::<f64>() - 1.0).abs() < 1e-10);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p ensemble --test stacker`

 Expected: FAIL because stacker fitting and constraints are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/ensemble/src/stacker.rs` with:

 ```text
 pub struct MetaStacker {
    pub module_order: Vec<String>,
    pub weights: nalgebra::DMatrix<f64>,
    pub intercepts: nalgebra::DVector<f64>,
    pub constraints: crate::WeightConstraints,
    pub diagnostics: numerics::OptimizationDiagnostics,
}

impl MetaStacker {
    pub fn predict_logits(&self, input: &ModuleVector) -> Result<Vec<f64>, EnsembleError> {
        input.validate_order(&self.module_order)?;
        let x = nalgebra::DVector::from_column_slice(&input.values);
        let logits = &self.weights * x + &self.intercepts;
        Ok(logits.iter().copied().collect())
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p ensemble --test stacker`

 Expected: PASS for constraints, regularization, convergence rejection, deterministic fit, missing modules, and module-removal ablations

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p ensemble && cargo clippy -p ensemble --all-targets -- -D warnings`

 Expected: stacker and ablation reports pass with no ineligible module receiving nonzero weight

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/ensemble/src/stacker.rs crates/ensemble/src/constraints.rs crates/ensemble/src/ablation.rs crates/ensemble/tests/stacker.rs
 git commit -m "feat: add constrained forecast stacker"
 ```
### Task 6: Fit and select per-event/horizon calibrators on a separate validation segment

 **Files:**
 - Create: `crates/calibration/src/platt.rs`
- Create: `crates/calibration/src/beta.rs`
- Create: `crates/calibration/src/isotonic.rs`
- Create: `crates/calibration/src/select.rs`
- Test: `crates/calibration/tests/reference.rs`

 **Interfaces:**
 - Consumes: uncalibrated hazard/stacker scores, validation-only rows, base rates, and minimum sample rules
 - Produces: Platt, beta, and isotonic calibrators; monotonic transforms; effective sample counts; confidence intervals; and a validation-only selector by event/horizon/liquidity class

 **Implementation notes**

 Calibrate cumulative event/horizon outputs consistently and rerun monotonicity checks afterward. If independently fit horizon calibrators break monotonicity, use a joint monotone calibration policy rather than clipping values after the fact.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/calibration/tests/reference.rs` with:

 ```text
 use calibration::{Calibrator, PlattCalibrator};

#[test]
fn platt_transform_is_monotone() {
    let c = PlattCalibrator::new(1.2, -0.3).unwrap();
    let values: Vec<_> = [-5.0, -1.0, 0.0, 1.0, 5.0].into_iter().map(|x| c.calibrate_logit(x)).collect();
    assert!(values.windows(2).all(|w| w[1] >= w[0]));
}

#[test]
fn isotonic_is_rejected_when_effective_sample_is_too_small() {
    let data = calibration::fixtures::small_validation_set();
    assert!(calibration::select(&data, 200).unwrap().selected_kind() != "isotonic");
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p calibration --test reference`

 Expected: FAIL because concrete calibrators and selection are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/calibration/src/platt.rs` with:

 ```text
 #[derive(Clone, Copy, Debug)]
pub struct PlattCalibrator { slope: f64, intercept: f64 }

impl PlattCalibrator {
    pub fn new(slope: f64, intercept: f64) -> Result<Self, CalibrationError> {
        if !slope.is_finite() || slope < 0.0 || !intercept.is_finite() { return Err(CalibrationError::InvalidParameters); }
        Ok(Self { slope, intercept })
    }
    pub fn calibrate_logit(&self, raw_logit: f64) -> f64 {
        let z = self.slope * raw_logit + self.intercept;
        if z >= 0.0 { 1.0 / (1.0 + (-z).exp()) } else { z.exp() / (1.0 + z.exp()) }
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p calibration --test reference`

 Expected: PASS against committed reference vectors, monotonicity, held-out fit, sample-size selection, interval, and no-test-leakage checks

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p calibration && cargo clippy -p calibration --all-targets -- -D warnings`

 Expected: calibration reports reproduce and test outcomes never influence calibrator selection or fit

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/calibration Cargo.toml Cargo.lock
 git commit -m "feat: add validation-only probability calibration"
 ```
### Task 7: Implement applicability, out-of-distribution detection, and abstention

 **Files:**
 - Create: `crates/applicability/Cargo.toml`
- Create: `crates/applicability/src/lib.rs`
- Create: `crates/applicability/src/mahalanobis.rs`
- Create: `crates/applicability/src/novelty.rs`
- Create: `crates/applicability/src/policy.rs`
- Test: `crates/applicability/tests/abstention.rs`

 **Interfaces:**
 - Consumes: training feature distribution, runtime feature/missingness/venue/product/model-age state, quality requirements, and conformal residual data
 - Produces: `ApplicabilityModel`, robust Mahalanobis distance, feature-range/categorical novelty, nearest-neighbor distance, model-age/drift checks, and explicit availability/abstention reasons

 **Implementation notes**

 The unconditional base rate may be shown as context only if clearly labeled “baseline, model unavailable.” It never replaces an abstained production forecast under the same forecast ID.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/applicability/tests/abstention.rs` with:

 ```text
 use applicability::{ApplicabilityModel, Availability};

#[test]
fn extreme_feature_vector_is_out_of_distribution() {
    let model = ApplicabilityModel::fixture();
    let result = model.evaluate(&[100.0, -100.0], applicability::fixtures::healthy_context()).unwrap();
    assert_eq!(result.availability, Availability::OutOfDistribution);
}

#[test]
fn unhealthy_required_source_overrides_in_distribution_features() {
    let model = ApplicabilityModel::fixture();
    let result = model.evaluate(&[0.0, 0.0], applicability::fixtures::unhealthy_required_source()).unwrap();
    assert_eq!(result.availability, Availability::SourceUnhealthy);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p applicability`

 Expected: FAIL because applicability/OOD policy is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/applicability/src/lib.rs` with:

 ```text
 #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    Available, Degraded, Experimental, OutOfDistribution, InsufficientData, SourceUnhealthy, ModelIncompatible,
}

#[derive(Clone, Debug)]
pub struct ApplicabilityResult {
    pub availability: Availability,
    pub score: f64,
    pub mahalanobis_distance: Option<f64>,
    pub novelty_reasons: Vec<String>,
    pub minimum_quality_met: bool,
}

pub struct ApplicabilityModel {
    pub center: Vec<f64>,
    pub inverse_covariance: nalgebra::DMatrix<f64>,
    pub thresholds: ApplicabilityThresholds,
}

impl ApplicabilityModel {
    pub fn evaluate(&self, features: &[f64], context: RuntimeContext) -> Result<ApplicabilityResult, ApplicabilityError> {
        context.validate_requirements()?;
        self.evaluate_distribution_and_policy(features, context)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p applicability`

 Expected: PASS for OOD, categorical novelty, missingness, source/product support, drift/model age, conformal residual, and degraded/abstained states

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p applicability && cargo clippy -p applicability --all-targets -- -D warnings`

 Expected: applicability tests pass and unavailable output retains the reason instead of silently showing a baseline

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/applicability Cargo.toml Cargo.lock
 git commit -m "feat: add applicability and abstention layer"
 ```
### Task 8: Build conditional scenario simulation with reproducible paths and threshold summaries

 **Files:**
 - Create: `crates/scenarios/Cargo.toml`
- Create: `crates/scenarios/src/lib.rs`
- Create: `crates/scenarios/src/process.rs`
- Create: `crates/scenarios/src/liquidity.rs`
- Create: `crates/scenarios/src/summary.rs`
- Test: `crates/scenarios/tests/reference.rs`

 **Interfaces:**
 - Consumes: current volatility/regime/cusp/fast state, calibrated hazards, options inputs when available, and deterministic RNG seed
 - Produces: `ScenarioEngine` for stochastic volatility, heavy-tailed/jump shocks, liquidity-dependent impact, optional liquidation amplification, conditional branches, quantiles, crossing probabilities, and assumption metadata

 **Implementation notes**

 Scenario paths are conditional simulations, not forecast observations. Validate CRPS, coverage, tail coverage, and crossing calibration in Phase 8 before production display. Large path arrays remain local and are summarized before RPC unless replay/model-lab requests them.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/scenarios/tests/reference.rs` with:

 ```text
 use scenarios::{ScenarioConfig, ScenarioEngine};

#[test]
fn fixed_seed_produces_identical_summary_and_path_digest() {
    let engine = ScenarioEngine::fixture();
    let a = engine.simulate(ScenarioConfig::fixture_with_seed(42)).unwrap();
    let b = engine.simulate(ScenarioConfig::fixture_with_seed(42)).unwrap();
    assert_eq!(a.path_digest, b.path_digest);
    assert_eq!(a.quantiles, b.quantiles);
}

#[test]
fn worse_liquidity_increases_tail_impact_in_fixture() {
    let engine = ScenarioEngine::fixture();
    let liquid = engine.simulate(ScenarioConfig::fixture_liquidity(1.0)).unwrap();
    let fragile = engine.simulate(ScenarioConfig::fixture_liquidity(0.2)).unwrap();
    assert!(fragile.quantile(0.01) < liquid.quantile(0.01));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p scenarios`

 Expected: FAIL because scenario simulator is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/scenarios/src/lib.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct ScenarioDistribution {
    pub as_of_ns: i64,
    pub horizon_seconds: u64,
    pub path_count: u32,
    pub quantiles: std::collections::BTreeMap<String, f64>,
    pub threshold_probabilities: std::collections::BTreeMap<String, f64>,
    pub path_digest: [u8; 32],
    pub assumptions: ScenarioAssumptions,
    pub model_package_ids: Vec<String>,
}

pub struct ScenarioEngine {
    process: process::MarketProcess,
    impact: liquidity::ImpactModel,
}

impl ScenarioEngine {
    pub fn simulate(&self, config: ScenarioConfig) -> Result<ScenarioDistribution, ScenarioError> {
        let paths = self.simulate_paths(&config)?;
        summary::summarize(paths, config)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p scenarios`

 Expected: PASS for seed parity, quantile ordering, threshold probability bounds, stress directions, missing options fallback, and path labeling tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p scenarios && cargo clippy -p scenarios --all-targets -- -D warnings`

 Expected: scenario tests pass and every result is explicitly labeled simulated with assumptions and model versions

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/scenarios Cargo.toml Cargo.lock
 git commit -m "feat: add conditional scenario simulation"
 ```
### Task 9: Persist immutable forecast and evidence bundles before publication

 **Files:**
 - Create: `crates/forecast-ledger/Cargo.toml`
- Create: `crates/forecast-ledger/src/lib.rs`
- Create: `crates/forecast-ledger/src/forecast.rs`
- Create: `crates/forecast-ledger/src/evidence.rs`
- Create: `crates/forecast-ledger/src/writer.rs`
- Test: `crates/forecast-ledger/tests/persist_before_publish.rs`

 **Interfaces:**
 - Consumes: calibrated curves, base rates, uncertainty, applicability, module outputs, feature lineage, scenario references, metadata/Parquet stores, and UUIDv7
 - Produces: `ForecastRecord`, `EvidenceBundle`, atomic ledger writer, audit-chain link, persistence receipt, and publication event that can only be constructed from a durable receipt

 **Implementation notes**

 Persist the compact audit record and analytical forecast/evidence Parquet row before emitting the publication command. Evidence contains deterministic feature contributions and structural states; it does not claim causality.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/forecast-ledger/tests/persist_before_publish.rs` with:

 ```text
 use forecast_ledger::{ForecastLedger, Publication};

#[tokio::test]
async fn publication_requires_durable_receipt() {
    let ledger = ForecastLedger::fixture_failing_store();
    let forecast = forecast_ledger::fixtures::forecast();
    assert!(ledger.persist(forecast).await.is_err());
    assert!(Publication::without_receipt_for_test().is_err());
}

#[tokio::test]
async fn evidence_hash_matches_forecast_reference() {
    let ledger = ForecastLedger::memory_for_test().await.unwrap();
    let receipt = ledger.persist(forecast_ledger::fixtures::forecast()).await.unwrap();
    let loaded = ledger.get(&receipt.forecast_id).await.unwrap();
    assert_eq!(loaded.evidence_bundle_id, receipt.evidence_bundle_id);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p forecast-ledger`

 Expected: FAIL because the ledger and durable receipt are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/forecast-ledger/src/forecast.rs` with:

 ```text
 #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ForecastRecord {
    pub forecast_id: uuid::Uuid,
    pub entity_id: domain::EntityId,
    pub as_of_event_time_ns: i64,
    pub as_of_receive_time_ns: i64,
    pub event_type: labels::EventType,
    pub horizons: Vec<HorizonProbability>,
    pub calibrated_base_rates: Vec<f64>,
    pub uncertainty: ForecastUncertainty,
    pub quality_score: f64,
    pub applicability: applicability::ApplicabilityResult,
    pub model_bundle_id: String,
    pub feature_set_id: String,
    pub label_definition_id: String,
    pub calibration_id: String,
    pub evidence_bundle_id: String,
}

#[derive(Clone, Debug)]
pub struct DurableReceipt {
    pub forecast_id: uuid::Uuid,
    pub evidence_bundle_id: String,
    pub ledger_offset: u64,
    pub fsync_completed: bool,
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p forecast-ledger`

 Expected: PASS for storage failure, hash mismatch, duplicate ID, audit chain, retrieval, restart, and persistence-before-publication tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p forecast-ledger && cargo clippy -p forecast-ledger --all-targets -- -D warnings`

 Expected: ledger tests pass and no public forecast/alert stream can be fed by an unpersisted in-memory object

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/forecast-ledger Cargo.toml Cargo.lock
 git commit -m "feat: add immutable forecast and evidence ledger"
 ```
### Task 10: Implement a safe typed alert-rule language and validator

 **Files:**
 - Create: `crates/alerts/Cargo.toml`
- Create: `crates/alerts/src/lib.rs`
- Create: `crates/alerts/src/ast.rs`
- Create: `crates/alerts/src/parser.rs`
- Create: `crates/alerts/src/evaluate.rs`
- Test: `crates/alerts/tests/rule_language.rs`

 **Interfaces:**
 - Consumes: forecast/evidence/applicability fields, model/event/horizon catalogs, and configuration quality floor
 - Produces: `AlertRule` AST, parser without dynamic eval, typed field registry, comparison/boolean/persistence/cooldown/recovery semantics, validation diagnostics, and deterministic evaluation

 **Implementation notes**

 Rules reference stable field IDs, not localized UI labels. Experimental outputs require an explicit `allow_experimental = true` rule flag and remain visually distinct. Parsing enforces length, token count, nesting, and numeric bounds.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/alerts/tests/rule_language.rs` with:

 ```text
 use alerts::{parse_rule, AlertContext};

#[test]
fn documented_rule_parses_and_matches() {
    let rule = parse_rule("downside_probability[4h] > 0.30 AND cusp_probability > 0.65 AND data_quality >= 0.90 FOR 3 EVALUATIONS").unwrap();
    let result = rule.evaluate(&AlertContext::fixture_matching()).unwrap();
    assert!(result.condition_true);
    assert_eq!(rule.persistence_evaluations(), 3);
}

#[test]
fn unknown_field_and_code_injection_are_rejected() {
    assert!(parse_rule("unknown_score > 0.1").is_err());
    assert!(parse_rule("system("rm -rf /")").is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p alerts --test rule_language`

 Expected: FAIL because the alert parser and AST are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/alerts/src/ast.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub enum Expr {
    Compare { field: FieldRef, op: CompareOp, value: f64 },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
}

#[derive(Clone, Debug)]
pub struct AlertRule {
    pub rule_id: uuid::Uuid,
    pub expression: Expr,
    pub persistence_evaluations: u32,
    pub cooldown_seconds: u64,
    pub recovery_evaluations: u32,
    pub minimum_quality: f64,
}

impl AlertRule {
    pub fn validate(&self, catalog: &FieldCatalog) -> Result<(), RuleError> {
        self.expression.validate(catalog)?;
        if self.persistence_evaluations == 0 || !(0.0..=1.0).contains(&self.minimum_quality) { return Err(RuleError::InvalidPolicy); }
        Ok(())
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p alerts --test rule_language`

 Expected: PASS for precedence, units/horizons, unknown fields, unsupported experimental fields, quality gates, persistence, cooldown, recovery, and malicious-input fixtures

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p alerts && cargo clippy -p alerts --all-targets -- -D warnings`

 Expected: alert language tests pass with bounded parse depth/length and no general-purpose evaluation capability

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/alerts Cargo.toml Cargo.lock
 git commit -m "feat: add typed alert rule language"
 ```
### Task 11: Implement alert lifecycle, persistence budget, recovery, and local delivery events

 **Files:**
 - Create: `crates/alerts/src/lifecycle.rs`
- Create: `crates/alerts/src/budget.rs`
- Create: `crates/alerts/src/store.rs`
- Create: `crates/alerts/tests/lifecycle.rs`

 **Interfaces:**
 - Consumes: durably published forecasts, typed rules, metadata store, source/model availability, and local notification boundary
 - Produces: inactive/pending/fired/cooldown/recovered/suppressed states, alert budget counters, evidence attachment, outcome-review records, and delivery commands created only after alert persistence

 **Implementation notes**

 Alert budgets are measurable false-alert/attention controls, not throttles that rewrite forecasts. Suppressed, abstained, and budget-blocked conditions remain in the ledger for outcome analysis.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/alerts/tests/lifecycle.rs` with:

 ```text
 use alerts::{AlertEngine, AlertState};

#[tokio::test]
async fn alert_fires_after_persistence_and_enters_cooldown() {
    let mut engine = AlertEngine::fixture_rule_with_persistence(3);
    for _ in 0..2 { assert_ne!(engine.evaluate_fixture().await.unwrap().state, AlertState::Fired); }
    let fired = engine.evaluate_fixture().await.unwrap();
    assert_eq!(fired.state, AlertState::Fired);
    assert!(fired.persisted_before_delivery);
    assert_eq!(engine.state(), AlertState::Cooldown);
}

#[tokio::test]
async fn unhealthy_data_suppresses_without_resetting_auditable_history() {
    let mut engine = AlertEngine::fixture_ready();
    let event = engine.evaluate_unhealthy_fixture().await.unwrap();
    assert_eq!(event.state, AlertState::Suppressed);
    assert!(event.reason.contains("source_unhealthy"));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p alerts --test lifecycle`

 Expected: FAIL because lifecycle and budget state are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/alerts/src/lifecycle.rs` with:

 ```text
 #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlertState { Inactive, Pending, Fired, Cooldown, Recovered, Suppressed }

#[derive(Clone, Debug)]
pub struct AlertEvent {
    pub alert_event_id: uuid::Uuid,
    pub rule_id: uuid::Uuid,
    pub forecast_id: uuid::Uuid,
    pub state: AlertState,
    pub occurred_at_ns: i64,
    pub reason: String,
    pub evidence_bundle_id: String,
    pub persisted_before_delivery: bool,
}

pub struct AlertEngine {
    rule: crate::AlertRule,
    state: AlertState,
    consecutive_true: u32,
    consecutive_recovery: u32,
    budget: crate::AlertBudget,
}

impl AlertEngine {
    pub async fn evaluate(&mut self, publication: &forecast_ledger::Publication) -> Result<Option<AlertEvent>, AlertError> {
        self.evaluate_and_persist_transition(publication).await
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p alerts --test lifecycle`

 Expected: PASS for persistence, cooldown, recovery, suppression, budget exhaustion, duplicate forecast, restart restoration, and outcome-review tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p alerts && cargo clippy -p alerts --all-targets -- -D warnings`

 Expected: alert lifecycle tests pass and delivery commands always reference a persisted alert event and forecast

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/alerts/src/lifecycle.rs crates/alerts/src/budget.rs crates/alerts/src/store.rs crates/alerts/tests/lifecycle.rs
 git commit -m "feat: add auditable alert lifecycle"
 ```
### Task 12: Wire the production risk pipeline and persist-evaluate-publish ordering

 **Files:**
 - Create: `crates/risk-engine/Cargo.toml`
- Create: `crates/risk-engine/src/lib.rs`
- Create: `crates/risk-engine/src/pipeline.rs`
- Create: `crates/risk-engine/src/evidence.rs`
- Modify: `apps/cryptoriskd/src/main.rs`
- Test: `crates/risk-engine/tests/pipeline_order.rs`

 **Interfaces:**
 - Consumes: fast/structural/module states, hazard, stacker, calibrators, applicability, scenarios, forecast ledger, alerts, and model registry
 - Produces: `RiskEngine` evaluation transaction with frozen model bundle snapshot, coherent curves, evidence, durable ledger receipt, publication, and alert evaluation in the required order

 **Implementation notes**

 A model bundle is immutable for one evaluation. Scenario failure may omit scenarios with an explicit error while preserving a valid forecast; core inference/calibration/applicability/evidence/persistence failures prevent publication.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/risk-engine/tests/pipeline_order.rs` with:

 ```text
 use risk_engine::RiskEngineHarness;

#[tokio::test]
async fn pipeline_order_is_infer_calibrate_applicability_persist_publish_alert() {
    let report = RiskEngineHarness::fixture().run_one().await.unwrap();
    assert_eq!(report.steps, ["infer", "calibrate", "applicability", "evidence", "persist", "publish", "alert"]);
}

#[tokio::test]
async fn storage_failure_prevents_publication_and_alert() {
    let report = RiskEngineHarness::fixture_with_storage_failure().run_one().await.unwrap_err();
    assert!(!report.publication_attempted);
    assert!(!report.alert_attempted);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p risk-engine`

 Expected: FAIL because the integrated risk pipeline is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/risk-engine/src/pipeline.rs` with:

 ```text
 pub struct RiskEngine {
    models: model_registry::AtomicModelBundle,
    applicability: applicability::ApplicabilityModel,
    scenarios: scenarios::ScenarioEngine,
    ledger: forecast_ledger::ForecastLedger,
    publisher: ForecastPublisher,
    alerts: alerts::AlertEngineSet,
}

impl RiskEngine {
    pub async fn evaluate(&self, state: fast_state::FastStateSnapshot) -> Result<uuid::Uuid, RiskError> {
        let bundle = self.models.snapshot();
        let raw = bundle.infer(&state)?;
        let calibrated = bundle.calibrate(raw)?;
        let applicability = self.applicability.evaluate(&state.feature_values, state.runtime_context())?;
        let forecast = self.build_forecast_and_evidence(state, calibrated, applicability, &bundle)?;
        let receipt = self.ledger.persist(forecast).await?;
        let publication = self.publisher.publish(receipt).await?;
        self.alerts.evaluate(&publication).await?;
        Ok(publication.forecast_id)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p risk-engine`

 Expected: PASS for required ordering, model hot-swap snapshot isolation, storage/model/scenario failure, abstention, and replay determinism

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p risk-engine && cargo clippy -p risk-engine --all-targets -- -D warnings`

 Expected: risk pipeline tests pass and model hot swap cannot mix artifacts inside one forecast

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/risk-engine apps/cryptoriskd/src/main.rs Cargo.toml Cargo.lock
 git commit -m "feat: assemble the production risk pipeline"
 ```
### Task 13: Expose forecast/evidence/scenario/alert services and prove latency/coherence under load

 **Files:**
 - Create: `crates/local-api/src/forecast_service.rs`
- Create: `crates/local-api/src/alert_service.rs`
- Create: `crates/local-api/src/stream_buffer.rs`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/risk_pipeline.rs`
- Create: `fixtures/golden-replays/risk-v1/manifest.toml`
- Modify: `proto/risk/v1/risk.proto`
- Test: `crates/local-api/tests/forecast_stream.rs`

 **Interfaces:**
 - Consumes: risk engine publications, ledger retrieval, alert engine, authenticated Tonic sessions, stream metadata, and replay fixtures
 - Produces: ForecastService/AlertService snapshot+delta streams, bounded resume tokens, slow-client isolation, typed terminal errors, golden risk replay, and reference p99 latency test

 **Implementation notes**

 Overview streams may coalesce superseded snapshots and must report coalescing. Audit, alert, and replay streams are lossless. A resume token is signed/opaque and scoped to stream/session/schema; it is not a raw sequence supplied without validation.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/local-api/tests/forecast_stream.rs` with:

 ```text
 use local_api::testkit::ForecastServiceHarness;

#[tokio::test]
async fn reconnect_resumes_or_receives_fresh_snapshot() {
    let harness = ForecastServiceHarness::fixture().await;
    let first = harness.read_n(3, None).await.unwrap();
    let resumed = harness.read_n(2, first.last().unwrap().resume_token.clone()).await.unwrap();
    assert!(resumed[0].stream_sequence > first.last().unwrap().stream_sequence);
}

#[tokio::test]
async fn slow_client_does_not_block_risk_engine_or_audit_stream() {
    let report = ForecastServiceHarness::fixture().run_slow_client_stress().await.unwrap();
    assert_eq!(report.ingestion_blocked_count, 0);
    assert_eq!(report.audit_records_dropped, 0);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p local-api --test forecast_stream`

 Expected: FAIL because risk/alert stream services and resume buffer are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/local-api/src/stream_buffer.rs` with:

 ```text
 pub struct StreamRecord<T> {
    pub sequence: u64,
    pub resume_token: String,
    pub value: T,
}

pub struct StreamBuffer<T> {
    capacity: usize,
    next_sequence: u64,
    records: std::collections::VecDeque<StreamRecord<T>>,
}

impl<T: Clone> StreamBuffer<T> {
    pub fn resume_after(&self, token: &str) -> Result<Vec<StreamRecord<T>>, ResumeError> {
        let sequence = verify_and_decode_token(token)?;
        let oldest = self.records.front().map(|v| v.sequence).unwrap_or(self.next_sequence);
        if sequence < oldest { return Err(ResumeError::SnapshotRequired); }
        Ok(self.records.iter().filter(|v| v.sequence > sequence).cloned().collect())
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p local-api --test forecast_stream`

 Expected: PASS for auth, snapshot, resume, expired token, coalesced overview, lossless alert/audit, cancellation, max message, and typed errors

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo run -p crypto-replay -- --manifest fixtures/golden-replays/risk-v1/manifest.toml --verify && cargo test -p system-tests --test risk_pipeline --release -- --ignored`

 Expected: golden forecasts/alerts reproduce; reference load meets declared p99 event-to-feature and post-feature inference targets on the documented hardware profile

 - [ ] **Step 6: Inspect probability, quality, persistence, and latency behavior**

 Run: `git diff --check && git status --short`

 Expected: no probability is manually clipped without an invariant test; no alert can precede forecast persistence; no degraded input is silently relabeled healthy.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/local-api/src/forecast_service.rs crates/local-api/src/alert_service.rs crates/local-api/src/stream_buffer.rs crates/local-api/tests/forecast_stream.rs crates/system-tests/Cargo.toml crates/system-tests/tests/risk_pipeline.rs fixtures/golden-replays/risk-v1 proto/risk/v1/risk.proto apps/macos/GeneratedProto crates/local-api/src/generated.rs
 git commit -m "feat: publish coherent risk and alert streams"
 ```
