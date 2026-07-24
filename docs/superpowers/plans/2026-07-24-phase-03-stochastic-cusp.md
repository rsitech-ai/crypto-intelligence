# Phase 03 — Stochastic Cusp Structural Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a numerically robust, uncertainty-aware stochastic cusp module whose structural outputs are reproducible and whose use in production probabilities is controlled by a predefined incremental-value gate.

**Architecture:** A pure-Rust numerical core implements the approved potential, stable cubic roots, fold geometry, stability/barrier calculations, and deterministic optimization. Offline stationary-density and heavy-tailed transition estimators produce frozen hierarchical control maps plus Laplace/bootstrap uncertainty; online inference evaluates current controls and tracks branches/hysteresis without refitting. A walk-forward ablation gate controls production eligibility.

**Tech Stack:** Rust, nalgebra, deterministic pure-Rust numerical routines, Student-t likelihoods, elastic-net/hierarchical penalties, BLAKE3 evidence hashes, Parquet datasets, Tonic/Protobuf exposure, numerical reference fixtures.

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

This plan implements the univariate structural cusp module for eligible assets. It includes math, estimation, uncertainty, online state, evidence, RPC, numerical validation, and the production-inclusion gate. It excludes the BTC–ETH coupled cusp, which is implemented after on-chain/options coverage; it also excludes the fast hazard stack, calibration meta-model, alerts, and UI rendering.

## File and module map

- `crates/numerics`: shared deterministic optimization and derivative utilities.
- `crates/cusp/src/potential.rs`, `roots.rs`, `equilibria.rs`, `barrier.rs`, `fold_distance.rs`: mathematical kernel.
- `crates/cusp/src/controls.rs`: hierarchical alpha/beta mapping and feature sensitivities.
- `crates/cusp/src/fit`: stationary and Student-t transition estimators.
- `crates/cusp/src/uncertainty.rs`, `bootstrap.rs`: Laplace and blocked-bootstrap uncertainty.
- `crates/cusp/src/online.rs`, `branch_tracker.rs`, `evidence.rs`: live/replay structural inference.
- `crates/cusp/src/ablation.rs`, `apps/crypto-evaluate/src/cusp_report.rs`: scientific release gate.
- `crates/local-api/src/cusp_service.rs`, `proto/risk/v1/risk.proto`: local API contract.

## Exit gate

All root/fold/barrier/derivative/scaling/reference tests pass near repeated roots and extreme controls; synthetic estimators recover declared directions and reject nonconvergence; online replay is deterministic; branch tracking does not flap on root ordering; uncertainty diagnostics are explicit; the ablation report is reproducible; and cusp status is mechanically set to production-weight-eligible or research-only without manual override.

---

### Task 1: Create shared deterministic numerical primitives and optimizer diagnostics

 **Files:**
 - Create: `crates/numerics/Cargo.toml`
- Create: `crates/numerics/src/lib.rs`
- Create: `crates/numerics/src/brent.rs`
- Create: `crates/numerics/src/finite_difference.rs`
- Create: `crates/numerics/src/logsumexp.rs`
- Test: `crates/numerics/tests/reference.rs`

 **Interfaces:**
 - Consumes: Rust floating-point policy and deterministic seed conventions
 - Produces: bracketed one-dimensional minimization/root search, finite-difference checks, log-sum-exp, positive-definite checks, and common `OptimizationDiagnostics`

 **Implementation notes**

 The implementations reject nonfinite evaluations and invalid brackets. Do not return an optimum when convergence is false. All tolerance constants are named, documented, and serialized into model metadata when they affect output.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/numerics/tests/reference.rs` with:

 ```text
 use numerics::{brent_minimize, central_gradient, logsumexp};

#[test]
fn brent_finds_quadratic_minimum_inside_bracket() {
    let result = brent_minimize(|x| (x - 2.0).powi(2), -5.0, 5.0, 1e-12, 200).unwrap();
    assert!((result.argmin - 2.0).abs() < 1e-9);
    assert!(result.diagnostics.converged);
}

#[test]
fn logsumexp_stays_finite_for_large_logits() {
    assert!((logsumexp(&[1000.0, 1000.0]).unwrap() - (1000.0 + 2.0_f64.ln())).abs() < 1e-10);
}

#[test]
fn analytic_and_numeric_gradient_agree() {
    let numeric = central_gradient(|x| x[0].powi(3), &[2.0], 1e-6).unwrap();
    assert!((numeric[0] - 12.0).abs() < 1e-5);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p numerics`

 Expected: FAIL because shared numerical primitives are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/numerics/src/lib.rs` with:

 ```text
 pub mod brent;
pub mod finite_difference;
pub mod logsumexp;

#[derive(Clone, Debug)]
pub struct OptimizationDiagnostics {
    pub converged: bool,
    pub iterations: usize,
    pub objective: f64,
    pub gradient_norm: Option<f64>,
    pub condition_number: Option<f64>,
    pub termination: String,
    pub deterministic_seed: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ScalarOptimum {
    pub argmin: f64,
    pub value: f64,
    pub diagnostics: OptimizationDiagnostics,
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p numerics`

 Expected: PASS against analytic roots/minima, failure brackets, nonfinite functions, iteration limits, and gradient references

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p numerics && cargo clippy -p numerics --all-targets -- -D warnings`

 Expected: all numerical references pass with fixed tolerances and deterministic diagnostics

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/numerics Cargo.toml Cargo.lock
 git commit -m "feat: add deterministic numerical primitives"
 ```
### Task 2: Freeze the cusp sign convention, potential, derivatives, controls, and state contract

 **Files:**
 - Create: `crates/cusp/Cargo.toml`
- Create: `crates/cusp/src/lib.rs`
- Create: `crates/cusp/src/potential.rs`
- Create: `crates/cusp/src/types.rs`
- Create: `docs/adr/0004-cusp-estimator-and-sign.md`
- Test: `crates/cusp/tests/sign_convention.rs`

 **Interfaces:**
 - Consumes: volatility-normalized state, feature observations, and approved mathematical convention
 - Produces: `Controls { alpha, beta }`, `Potential`, analytic gradient/Hessian, discriminant, cusp-region predicate, and `CuspState` output fields

 **Implementation notes**

 ADR 0004 selects two offline candidates: stationary-density replication baseline and Student-t transition pseudo-likelihood production candidate. The production release rule remains ablation-based. All UI/API labels derive from this one sign convention.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/sign_convention.rs` with:

 ```text
 use cusp::{Controls, Potential};

#[test]
fn potential_derivatives_match_approved_sign_convention() {
    let c = Controls { alpha: 2.0, beta: 3.0 };
    let y = 1.5;
    assert!((Potential::gradient(y, c) - (y.powi(3) - c.beta * y - c.alpha)).abs() < 1e-12);
    assert!((Potential::hessian(y, c) - (3.0 * y * y - c.beta)).abs() < 1e-12);
}

#[test]
fn cusp_region_requires_positive_beta_and_discriminant() {
    assert!(Controls { alpha: 0.0, beta: 1.0 }.inside_cusp());
    assert!(!Controls { alpha: 0.0, beta: -1.0 }.inside_cusp());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test sign_convention`

 Expected: FAIL because cusp controls and potential functions are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/potential.rs` with:

 ```text
 use crate::Controls;

pub struct Potential;

impl Potential {
    pub fn value(y: f64, c: Controls) -> f64 { 0.25 * y.powi(4) - 0.5 * c.beta * y * y - c.alpha * y }
    pub fn gradient(y: f64, c: Controls) -> f64 { y.powi(3) - c.beta * y - c.alpha }
    pub fn hessian(y: f64, c: Controls) -> f64 { 3.0 * y * y - c.beta }
}

impl Controls {
    pub fn discriminant(self) -> f64 { 4.0 * self.beta.powi(3) - 27.0 * self.alpha.powi(2) }
    pub fn inside_cusp(self) -> bool { self.beta > 0.0 && self.discriminant() > 0.0 }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test sign_convention`

 Expected: PASS for approved convention, equivalent alternate convention mapping, finite-difference derivatives, and boundary cases

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p cusp && cargo clippy -p cusp --all-targets -- -D warnings`

 Expected: cusp math tests pass and ADR 0004 records the production sign and chosen estimator candidates

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/cusp docs/adr/0004-cusp-estimator-and-sign.md Cargo.toml Cargo.lock
 git commit -m "feat: freeze cusp mathematical contract"
 ```
### Task 3: Implement stable real cubic roots at ordinary, near-fold, and repeated-root points

 **Files:**
 - Create: `crates/cusp/src/roots.rs`
- Create: `crates/cusp/tests/cubic_roots.rs`
- Create: `fixtures/numerical/cusp_roots.json`

 **Interfaces:**
 - Consumes: cusp polynomial `y^3 - beta*y - alpha = 0` and numerical tolerance policy
 - Produces: `real_equilibria(Controls)` returning sorted roots with multiplicity/conditioning metadata and Newton-polished residuals

 **Implementation notes**

 Scale the polynomial before formulas, use `cbrt` rather than fractional powers, clamp trigonometric arguments only within a documented roundoff tolerance, and polish roots with bounded Newton steps that cannot cross neighboring roots.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/cubic_roots.rs` with:

 ```text
 use cusp::{real_equilibria, Controls};

#[test]
fn three_root_region_returns_three_low_residual_roots() {
    let c = Controls { alpha: 0.0, beta: 3.0 };
    let roots = real_equilibria(c).unwrap();
    assert_eq!(roots.len(), 3);
    for root in roots { assert!((root.value.powi(3) - c.beta * root.value - c.alpha).abs() < 1e-11); }
}

#[test]
fn exact_fold_reports_repeated_root_without_nan() {
    let s: f64 = 1.0;
    let c = Controls { alpha: -2.0 * s.powi(3), beta: 3.0 * s.powi(2) };
    let roots = real_equilibria(c).unwrap();
    assert!(roots.iter().any(|r| r.multiplicity >= 2));
    assert!(roots.iter().all(|r| r.value.is_finite()));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test cubic_roots`

 Expected: FAIL because stable cubic roots are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/roots.rs` with:

 ```text
 #[derive(Clone, Copy, Debug)]
pub struct EquilibriumRoot {
    pub value: f64,
    pub multiplicity: u8,
    pub residual: f64,
    pub condition_proxy: f64,
}

pub fn real_equilibria(c: crate::Controls) -> Result<Vec<EquilibriumRoot>, crate::CuspError> {
    let discriminant = c.discriminant();
    let scale = c.alpha.abs().max(c.beta.abs().powf(1.5)).max(1.0);
    let tolerance = 64.0 * f64::EPSILON * scale * scale;
    if discriminant > tolerance { three_real_roots_trigonometric(c) }
    else if discriminant < -tolerance { one_real_root_cbrt(c) }
    else { repeated_roots_scaled(c) }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test cubic_roots`

 Expected: PASS across committed high-precision fixtures covering one/three roots, exact/near folds, extreme scaling, and sign-equivalent forms

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p cusp --test cubic_roots && cargo test -p cusp --test sign_convention`

 Expected: all root residuals and continuity tolerances pass without branch-order flapping

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/cusp/src/roots.rs crates/cusp/tests/cubic_roots.rs fixtures/numerical/cusp_roots.json
 git commit -m "feat: add stable cusp equilibrium roots"
 ```
### Task 4: Calculate stability, branch topology, barrier height, and restoring force

 **Files:**
 - Create: `crates/cusp/src/equilibria.rs`
- Create: `crates/cusp/src/barrier.rs`
- Create: `crates/cusp/tests/equilibria_barrier.rs`

 **Interfaces:**
 - Consumes: stable cubic roots and analytic potential derivatives
 - Produces: `EquilibriumSet`, stable/unstable classification, adjacent barrier calculation, restoring-force magnitude, and topology errors at/near folds

 **Implementation notes**

 Barrier height is only defined from a stable root to the intervening unstable root. Store missing/undefined explicitly outside the three-root topology. Restoring-force magnitude is a structural indicator, never a calibrated event probability.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/equilibria_barrier.rs` with:

 ```text
 use cusp::{analyze_equilibria, Controls, Stability};

#[test]
fn three_root_region_has_two_stable_and_one_unstable_equilibrium() {
    let set = analyze_equilibria(Controls { alpha: 0.0, beta: 3.0 }).unwrap();
    assert_eq!(set.roots.iter().filter(|r| r.stability == Stability::Stable).count(), 2);
    assert_eq!(set.roots.iter().filter(|r| r.stability == Stability::Unstable).count(), 1);
    assert!(set.barriers.iter().all(|b| b.height > 0.0));
}

#[test]
fn barrier_tends_to_zero_near_fold() {
    let far = analyze_equilibria(Controls { alpha: 0.0, beta: 3.0 }).unwrap().minimum_barrier().unwrap();
    let near = analyze_equilibria(Controls { alpha: -1.99, beta: 3.0 }).unwrap().minimum_barrier().unwrap();
    assert!(near < far);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test equilibria_barrier`

 Expected: FAIL because equilibrium topology and barrier APIs are missing

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/equilibria.rs` with:

 ```text
 #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stability { Stable, Unstable, NeutralAtTolerance }

#[derive(Clone, Debug)]
pub struct ClassifiedRoot {
    pub value: f64,
    pub stability: Stability,
    pub hessian: f64,
    pub restoring_force: f64,
}

#[derive(Clone, Debug)]
pub struct Barrier {
    pub stable_root: f64,
    pub unstable_root: f64,
    pub height: f64,
}

pub fn classify(value: f64, controls: crate::Controls, tolerance: f64) -> Stability {
    let h = crate::Potential::hessian(value, controls);
    if h > tolerance { Stability::Stable } else if h < -tolerance { Stability::Unstable } else { Stability::NeutralAtTolerance }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test equilibria_barrier`

 Expected: PASS for topology, barrier positivity, fold limit, one-root absence of inter-branch barrier, and finite-difference potential checks

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p cusp --test equilibria_barrier && cargo test -p cusp`

 Expected: equilibrium and barrier references pass with no negative height from root ordering

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/cusp/src/equilibria.rs crates/cusp/src/barrier.rs crates/cusp/tests/equilibria_barrier.rs
 git commit -m "feat: add cusp stability and barrier analysis"
 ```
### Task 5: Implement nearest-fold distance in whitened control space

 **Files:**
 - Create: `crates/cusp/src/fold_distance.rs`
- Create: `crates/cusp/tests/fold_distance.rs`

 **Interfaces:**
 - Consumes: fold parameterization `(alpha,beta)=(-2s^3,3s^2)`, control covariance, and Brent minimizer
 - Produces: `nearest_fold` returning signed whitened distance, nearest fold point, fold parameter, optimizer diagnostics, and covariance conditioning state

 **Implementation notes**

 Whitening parameters are fit on the training fold and packaged with the model. The signed convention is negative inside the cusp, positive outside, zero on the fold; API documentation and charts use the same convention.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/fold_distance.rs` with:

 ```text
 use cusp::{nearest_fold, ControlWhitening, Controls};

#[test]
fn exact_fold_has_zero_distance() {
    let c = Controls { alpha: -2.0, beta: 3.0 };
    let result = nearest_fold(c, &ControlWhitening::identity()).unwrap();
    assert!(result.distance.abs() < 1e-10);
    assert!((result.fold_parameter - 1.0).abs() < 1e-8);
}

#[test]
fn whitening_changes_scale_not_fold_membership_sign() {
    let c = Controls { alpha: 0.0, beta: 1.0 };
    let a = nearest_fold(c, &ControlWhitening::identity()).unwrap();
    let b = nearest_fold(c, &ControlWhitening::diagonal(4.0, 0.25).unwrap()).unwrap();
    assert_eq!(a.distance.is_sign_negative(), b.distance.is_sign_negative());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test fold_distance`

 Expected: FAIL because fold-distance optimization is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/fold_distance.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct FoldDistance {
    pub distance: f64,
    pub nearest: crate::Controls,
    pub fold_parameter: f64,
    pub diagnostics: numerics::OptimizationDiagnostics,
}

pub fn fold_point(s: f64) -> crate::Controls {
    crate::Controls { alpha: -2.0 * s.powi(3), beta: 3.0 * s.powi(2) }
}

pub fn nearest_fold(c: crate::Controls, whitening: &ControlWhitening) -> Result<FoldDistance, crate::CuspError> {
    let objective = |s: f64| whitening.squared_distance(c, fold_point(s));
    let bound = fold_search_bound(c, whitening)?;
    let optimum = numerics::brent_minimize(objective, -bound, bound, 1e-12, 256)?;
    let nearest = fold_point(optimum.argmin);
    let sign = if c.inside_cusp() { -1.0 } else { 1.0 };
    Ok(FoldDistance { distance: sign * optimum.value.sqrt(), nearest, fold_parameter: optimum.argmin, diagnostics: optimum.diagnostics })
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test fold_distance`

 Expected: PASS for exact folds, symmetry, inside/outside sign, singular covariance rejection, extreme scaling, and brute-force reference comparisons

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p cusp --test fold_distance && cargo clippy -p cusp --all-targets -- -D warnings`

 Expected: fold-distance references pass and nonconvergence yields unavailable structural output

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/cusp/src/fold_distance.rs crates/cusp/tests/fold_distance.rs
 git commit -m "feat: add normalized cusp fold distance"
 ```
### Task 6: Define hierarchical alpha/beta control mappings and constrained feature groups

 **Files:**
 - Create: `crates/cusp/src/controls.rs`
- Create: `crates/cusp/src/control_schema.rs`
- Create: `models/schemas/cusp-control-map.schema.json`
- Create: `docs/model-cards/cusp-control-features.md`
- Test: `crates/cusp/tests/control_mapping.rs`

 **Interfaces:**
 - Consumes: feature registry, training-only normalization, asset identities, and quality/missingness policies
 - Produces: `ControlMap` with shared and asset-specific coefficients, optional group sign constraints, sparse penalties, overlap between alpha/beta when declared, covariance propagation, and feature sensitivities

 **Implementation notes**

 Feature-group semantics are priors/constraints, not unquestioned truth. Each feature can affect alpha, beta, both, or neither only through an explicit schema entry. Missing required controls force abstention.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/control_mapping.rs` with:

 ```text
 use cusp::{ControlMap, ControlVector};

#[test]
fn shared_and_asset_specific_terms_sum_exactly() {
    let map = ControlMap::fixture();
    let controls = map.evaluate("BTC", &ControlVector::fixture()).unwrap();
    assert!((controls.alpha - 1.25).abs() < 1e-12);
    assert!((controls.beta - 0.75).abs() < 1e-12);
}

#[test]
fn missing_required_feature_returns_unavailable_not_zero() {
    let map = ControlMap::fixture();
    assert!(map.evaluate("BTC", &ControlVector::missing_required()).is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test control_mapping`

 Expected: FAIL because control mapping and schemas are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/controls.rs` with:

 ```text
 #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LinearControl {
    pub intercept: f64,
    pub shared: Vec<FeatureCoefficient>,
    pub by_asset: std::collections::BTreeMap<String, Vec<FeatureCoefficient>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ControlMap {
    pub version: semver::Version,
    pub alpha: LinearControl,
    pub beta: LinearControl,
    pub normalization_hash: [u8; 32],
    pub covariance: Vec<f64>,
}

impl ControlMap {
    pub fn evaluate(&self, asset: &str, features: &ControlVector) -> Result<crate::Controls, ControlError> {
        Ok(crate::Controls {
            alpha: self.alpha.evaluate(asset, features)?,
            beta: self.beta.evaluate(asset, features)?,
        })
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test control_mapping`

 Expected: PASS for shared/asset terms, sign constraints, overlap declaration, missing required/optional features, covariance propagation, and sensitivity derivatives

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p cusp --test control_mapping && cargo run -p xtask -- validate-model-schemas`

 Expected: control mapping tests and JSON Schema validation pass

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/cusp/src/controls.rs crates/cusp/src/control_schema.rs models/schemas/cusp-control-map.schema.json docs/model-cards/cusp-control-features.md
 git commit -m "feat: add hierarchical cusp control mapping"
 ```
### Task 7: Implement offline stationary-density replication and Student-t transition pseudo-likelihood estimators

 **Files:**
 - Create: `crates/cusp/src/fit/mod.rs`
- Create: `crates/cusp/src/fit/stationary.rs`
- Create: `crates/cusp/src/fit/transition.rs`
- Create: `crates/cusp/src/fit/penalty.rs`
- Create: `crates/cusp/src/fit/optimizer.rs`
- Test: `crates/cusp/tests/estimation_synthetic.rs`

 **Interfaces:**
 - Consumes: walk-forward datasets, control-map design matrices, numerical primitives, and deterministic optimization
 - Produces: two estimator candidates with Student-t innovations, hierarchical elastic-net penalties, analytic gradients, convergence/conditioning diagnostics, and synthetic parameter-recovery reports

 **Implementation notes**

 The stationary estimator exists to replicate published-style analyses and provide a comparator. The Student-t transition pseudo-likelihood is the production candidate because it models time evolution and heavy tails. Estimator selection happens only inside training/validation folds.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/estimation_synthetic.rs` with:

 ```text
 use cusp::fit::{EstimatorKind, FitConfig, fit};

#[test]
fn transition_estimator_recovers_synthetic_control_direction() {
    let data = cusp::fixtures::synthetic_transition_data(7);
    let result = fit(&data, FitConfig::fixture(EstimatorKind::StudentTTransition)).unwrap();
    assert!(result.diagnostics.converged);
    assert!(result.control_map.alpha.shared[0].coefficient > 0.0);
    assert!(result.control_map.beta.shared[0].coefficient > 0.0);
}

#[test]
fn nonconverged_fit_cannot_be_packaged() {
    let data = cusp::fixtures::ill_conditioned_data();
    let result = fit(&data, FitConfig::fixture_with_max_iterations(1)).unwrap();
    assert!(!result.diagnostics.converged);
    assert!(result.into_candidate_artifact().is_err());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test estimation_synthetic`

 Expected: FAIL because cusp fitting is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/fit/transition.rs` with:

 ```text
 pub fn negative_log_likelihood(params: &[f64], data: &TransitionDataset, config: &TransitionConfig) -> Result<Objective, FitError> {
    let map = decode_control_map(params, &data.schema)?;
    let mut value = 0.0;
    let mut gradient = vec![0.0; params.len()];
    for row in &data.rows {
        let controls = map.evaluate(&row.asset, &row.features)?;
        let drift = -(row.y.powi(3) - controls.beta * row.y - controls.alpha);
        let standardized = (row.delta_y - drift * row.delta_t) / (row.scale * row.delta_t.sqrt());
        let contribution = student_t_negative_log_density(standardized, config.degrees_of_freedom)?;
        value += row.weight * contribution;
        accumulate_analytic_gradient(&mut gradient, row, controls, standardized, config)?;
    }
    add_hierarchical_elastic_net(&mut value, &mut gradient, params, &config.penalty)?;
    Ok(Objective { value, gradient })
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test estimation_synthetic`

 Expected: PASS for synthetic recovery, analytic-vs-finite gradient, heavy-tail superiority fixture, scaling, regularization, and convergence rejection

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p cusp --test estimation_synthetic && cargo test -p cusp --release`

 Expected: both estimator candidates fit deterministically and emit complete diagnostics; no nonconverged result enters the registry

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/cusp/src/fit crates/cusp/tests/estimation_synthetic.rs Cargo.toml Cargo.lock
 git commit -m "feat: add offline stochastic cusp estimators"
 ```
### Task 8: Add Laplace parameter uncertainty and blocked bootstrap validation

 **Files:**
 - Create: `crates/cusp/src/uncertainty.rs`
- Create: `crates/cusp/src/bootstrap.rs`
- Create: `crates/cusp/tests/uncertainty.rs`

 **Interfaces:**
 - Consumes: converged cusp fit, Hessian/gradient utilities, time-block definitions, and deterministic seeds
 - Produces: `LaplaceApproximation`, positive-definite covariance regularization, blocked bootstrap refits, coefficient/control intervals, posterior control samples, and effective-sample diagnostics

 **Implementation notes**

 Do not hide ill conditioning by large diagonal loading. If regularization or failed-refit thresholds exceed the declared quality limit, output becomes experimental/unavailable. Bootstrap blocks preserve serial dependence and never cross outer-fold boundaries.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/uncertainty.rs` with:

 ```text
 use cusp::{bootstrap::BlockedBootstrap, uncertainty::LaplaceApproximation};

#[test]
fn laplace_covariance_is_symmetric_positive_definite_after_declared_regularization() {
    let fit = cusp::fixtures::well_conditioned_fit();
    let approximation = LaplaceApproximation::from_fit(&fit).unwrap();
    assert!(approximation.covariance_is_symmetric(1e-12));
    assert!(approximation.minimum_eigenvalue() > 0.0);
}

#[test]
fn blocked_bootstrap_is_reproducible_for_fixed_seed() {
    let data = cusp::fixtures::synthetic_transition_data(11);
    let a = BlockedBootstrap::fixture(99).run(&data).unwrap();
    let b = BlockedBootstrap::fixture(99).run(&data).unwrap();
    assert_eq!(a.digest(), b.digest());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test uncertainty`

 Expected: FAIL because uncertainty and bootstrap APIs are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/uncertainty.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct LaplaceApproximation {
    pub mode: Vec<f64>,
    pub covariance: nalgebra::DMatrix<f64>,
    pub regularization_added: f64,
    pub condition_number: f64,
}

impl LaplaceApproximation {
    pub fn from_hessian(mode: Vec<f64>, hessian: nalgebra::DMatrix<f64>, floor: f64) -> Result<Self, UncertaintyError> {
        let symmetric = 0.5 * (&hessian + hessian.transpose());
        let (regularized, added) = regularize_positive_definite(symmetric, floor)?;
        let condition_number = matrix_condition_number(&regularized)?;
        let covariance = regularized.try_inverse().ok_or(UncertaintyError::SingularHessian)?;
        Ok(Self { mode, covariance, regularization_added: added, condition_number })
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test uncertainty`

 Expected: PASS for covariance, singular Hessian, deterministic samples, block boundaries, failed-refit accounting, and interval-coverage simulations

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p cusp --test uncertainty && cargo clippy -p cusp --all-targets -- -D warnings`

 Expected: uncertainty tests pass and every interval records method, seed, effective samples, failed refits, and regularization

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/cusp/src/uncertainty.rs crates/cusp/src/bootstrap.rs crates/cusp/tests/uncertainty.rs
 git commit -m "feat: quantify cusp parameter uncertainty"
 ```
### Task 9: Implement online structural inference, posterior branch tracking, and hysteresis state

 **Files:**
 - Create: `crates/cusp/src/online.rs`
- Create: `crates/cusp/src/branch_tracker.rs`
- Create: `crates/cusp/src/evidence.rs`
- Test: `crates/cusp/tests/online_branch_tracking.rs`

 **Interfaces:**
 - Consumes: frozen control map, parameter samples, current feature vector/quality, roots, fold distance, barriers, and prior branch posterior
 - Produces: `CuspEngine::update` at 5/15-minute cadence, posterior structural samples, branch continuity/HMM-like transition penalty, hysteresis path state, feature sensitivities, and quality-aware `CuspSnapshot`

 **Implementation notes**

 Branch tracking uses posterior continuity and allowed transition topology, not array index. A structural snapshot may be shown in research mode when unavailable for production, but missing controls are never converted to neutral/zero risk.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/online_branch_tracking.rs` with:

 ```text
 use cusp::{BranchId, CuspEngine};

#[test]
fn numeric_root_order_change_does_not_flip_branch_without_evidence() {
    let mut engine = CuspEngine::fixture_on_upper_branch();
    let first = engine.update(cusp::fixtures::controls_near_symmetric(0.01)).unwrap();
    let second = engine.update(cusp::fixtures::controls_near_symmetric(-0.01)).unwrap();
    assert_eq!(first.most_likely_branch, BranchId::Upper);
    assert_eq!(second.most_likely_branch, BranchId::Upper);
}

#[test]
fn invalid_required_feature_yields_unavailable_snapshot() {
    let mut engine = CuspEngine::fixture_on_upper_branch();
    let snapshot = engine.update(cusp::fixtures::invalid_control_features()).unwrap();
    assert!(snapshot.availability.is_unavailable());
    assert!(snapshot.controls.is_none());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test online_branch_tracking`

 Expected: FAIL because online and branch-tracking APIs are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/online.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct CuspSnapshot {
    pub as_of_ns: i64,
    pub controls: Option<crate::Controls>,
    pub cusp_region_probability: Option<f64>,
    pub signed_discriminant: Option<f64>,
    pub standardized_discriminant: Option<f64>,
    pub fold_distance: Option<crate::FoldDistance>,
    pub equilibria: Option<crate::EquilibriumSet>,
    pub most_likely_branch: crate::BranchId,
    pub branch_probabilities: Vec<(crate::BranchId, f64)>,
    pub minimum_barrier: Option<f64>,
    pub hysteresis: crate::HysteresisState,
    pub sensitivities: Vec<crate::FeatureSensitivity>,
    pub availability: quality::AvailabilityState,
    pub evidence_hash: [u8; 32],
}

impl CuspEngine {
    pub fn update(&mut self, input: StructuralInput) -> Result<CuspSnapshot, CuspError> {
        if !input.quality.meets(&self.model.quality_requirements) { return Ok(self.unavailable(input)); }
        self.infer_posterior_and_track_branch(input)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test online_branch_tracking`

 Expected: PASS for branch continuity, true fold transition, posterior normalization, missing quality, deterministic samples, and hysteresis-path fixtures

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo test -p cusp && cargo run -p crypto-replay -- --manifest fixtures/golden-replays/features-v1/manifest.toml --verify-module cusp`

 Expected: cusp module outputs and evidence hashes reproduce across repeated replay

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/cusp/src/online.rs crates/cusp/src/branch_tracker.rs crates/cusp/src/evidence.rs crates/cusp/tests/online_branch_tracking.rs
 git commit -m "feat: add online cusp structural inference"
 ```
### Task 10: Expose cusp RPC data and automate the production-inclusion ablation gate

 **Files:**
 - Modify: `proto/risk/v1/risk.proto`
- Create: `crates/local-api/src/cusp_service.rs`
- Create: `crates/cusp/src/ablation.rs`
- Create: `apps/crypto-evaluate/src/cusp_report.rs`
- Create: `docs/model-cards/cusp-template.md`
- Test: `crates/cusp/tests/ablation_gate.rs`

 **Interfaces:**
 - Consumes: online cusp snapshots, model registry, walk-forward evaluator, required non-cusp baselines, and protobuf stream metadata
 - Produces: `CuspService` snapshot/history, model-card report, sign/scaling/numerical checks, and a predeclared gate that sets cusp status to production-weight-eligible or research-only

 **Implementation notes**

 The gate decides eligibility for a later stacker; it does not assign a production weight by itself. If the gate fails, RPC availability remains `EXPERIMENTAL` and the UI must display “not used in production probability.”

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/cusp/tests/ablation_gate.rs` with:

 ```text
 use cusp::ablation::{CuspGate, GateDecision};

#[test]
fn one_crash_only_improvement_is_not_production_eligible() {
    let report = cusp::fixtures::ablation_improves_only_one_fold();
    assert_eq!(CuspGate::default().decide(&report).unwrap(), GateDecision::ResearchOnly);
}

#[test]
fn stable_incremental_brier_skill_can_become_eligible() {
    let report = cusp::fixtures::ablation_passes_all_predeclared_rules();
    assert_eq!(CuspGate::default().decide(&report).unwrap(), GateDecision::EligibleForProductionWeight);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p cusp --test ablation_gate`

 Expected: FAIL because the gate and service are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/cusp/src/ablation.rs` with:

 ```text
 #[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateDecision { EligibleForProductionWeight, ResearchOnly }

pub struct CuspGate {
    pub minimum_improved_fold_fraction: f64,
    pub require_positive_primary_score: bool,
    pub require_sign_scaling_parity: bool,
    pub require_multi_regime_improvement: bool,
}

impl CuspGate {
    pub fn decide(&self, report: &AblationReport) -> Result<GateDecision, GateError> {
        let passes = report.incremental_brier_skill > 0.0
            && report.improved_fold_fraction >= self.minimum_improved_fold_fraction
            && report.sign_scaling_parity
            && report.multi_regime_improvement
            && report.numerical_failures == 0;
        Ok(if passes { GateDecision::EligibleForProductionWeight } else { GateDecision::ResearchOnly })
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p cusp --test ablation_gate`

 Expected: PASS for pass/fail reasons, insufficient events, single-episode dependence, sign/scaling disagreement, numerical failure, and serialized decision evidence

 - [ ] **Step 5: Run the numerical or subsystem verification command**

 Run: `cargo run -p crypto-evaluate -- cusp-ablation --dataset target/datasets/reference --output target/evaluation/cusp && cargo run -p xtask -- proto-check && cargo test -p cusp -p local-api`

 Expected: report contains fold metrics, coefficient stability, calibration controls, numerical suite, decision, and signed evidence hash; RPC regeneration is clean

 - [ ] **Step 6: Inspect numerical diagnostics and sign/scaling consistency**

 Run: `git diff --check && git status --short`

 Expected: every optimizer exposes convergence evidence; no test silently widens tolerances; production status is not granted by visual fit.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add proto/risk/v1/risk.proto crates/local-api/src/cusp_service.rs crates/cusp/src/ablation.rs apps/crypto-evaluate/src/cusp_report.rs docs/model-cards/cusp-template.md apps/macos/GeneratedProto crates/local-api/src/generated.rs
 git commit -m "feat: gate and expose cusp structural signals"
 ```
