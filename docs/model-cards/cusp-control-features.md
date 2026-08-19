# Cusp control features

## Purpose and status

This contract maps a bounded, versioned vector of training-normalized features
to the product cusp controls `(alpha, beta)`. It is a Phase 3 implementation
component, not a calibrated production model or trading signal. Coefficient
groups express fitted constraints and regularization policy; they are not
claims that a feature has a universal economic direction.

## Identity and point-in-time boundary

Every feature is bound to its canonical feature-registry identifier and exact
semantic version. Before a map is packaged, `ControlSchema::validate_against_registry`
requires each entry to be eligible for predictive model use. Asset overrides use
the complete generation-aware `AssetId`; a symbol alone never selects an
override.

The `normalization_hash` commits the training-fitted centers and scales to the
model package. A zero hash is rejected. Serving inputs must use the exact
packaged feature set and versions.

## Mapping semantics

Each feature declares:

- whether it is required;
- its fitted center and strictly positive scale;
- one constraint/penalty group;
- whether it may affect neither control, alpha, beta, or both.

The evaluated control is the intercept plus shared coefficients and the exact
asset-generation override, applied to `(value - center) / scale`. Coefficients
for an undeclared control and coefficients that violate a group sign constraint
are rejected. Sparse penalties remain explicit package metadata and include
both shared and asset-specific coefficients.

A missing required feature makes the control unavailable. An optional missing
feature contributes no normalized value and is listed in the evaluation
evidence; it is not silently reported as an observed zero.

## Uncertainty and sensitivities

Analytic sensitivities are the total shared-plus-asset coefficient divided by
the packaged feature scale. Given an exact positive-definite feature covariance
in schema order, the map propagates uncertainty as `J Σ Jᵀ` and then adds the
validated positive-definite residual alpha/beta covariance. Optional missing
features have zero effective Jacobian contribution for that evaluation and
remain listed as missing.

## Wire and validation contract

`models/schemas/cusp-control-map.schema.json` is strict Draft 2020-12 JSON
Schema: required fields are explicit, unknown object fields are rejected, and
arrays have fixed capacity bounds. Asset overrides serialize as a canonical
array of structured asset/coefficient entries because structured asset
identities cannot safely serve as JSON object keys.

JSON Schema cannot express positive definiteness, registry eligibility, exact
feature-set equality, coefficient target/sign rules, or normalization-hash
meaning. Rust constructors and deserialization re-run those semantic checks.

## Known limitations

- This card defines the control-map boundary. Phase 3 Tasks 7–8 now supply
  bounded offline estimator candidates plus active-set Laplace and fold-bound
  blocked-bootstrap uncertainty, but feature/hyperparameter selection evidence
  still belongs to nested walk-forward evaluation.
- Fixed-size covariance propagation is in-memory and capped at 64 features.
- A positive-definite covariance is required; singular estimates must be
  regularized during training with that decision recorded by later fit
  diagnostics.
- Phase 3 online branch inference, ablation validation, calibration, shadow
  operation, and release gates remain incomplete. Task 8 uncertainty quality
  does not itself grant production eligibility.
