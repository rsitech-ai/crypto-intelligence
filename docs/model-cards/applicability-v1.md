# Applicability model v1

## Purpose and status

This artifact is the Phase 4 Task 7 safety layer between an exact calibrated
forecast and later forecast persistence or display. It evaluates whether the
model is applicable to the current feature, source, venue, product, quality,
age, drift, and empirical-residual context. It does not alter the forecast
probability, choose a fallback model, or create an alert.

The current Task 6 calibration artifact is explicitly `experimental`.
Consequently, an otherwise healthy Task 7 decision also remains
`experimental`; a good applicability score cannot promote it to `available`.

## Identity and point-in-time boundary

`ApplicabilityModel::fit_for_calibration` accepts only a validated Task 6
`CalibrationArtifact`. The immutable applicability identity binds:

- the calibration artifact and calibration-key identities;
- the raw model, raw-model training, and outer-fold identities;
- the training cutoff and model-fit time;
- the ordered numeric feature schema and range policy;
- supported categories, venues, and products;
- model-owned required and optional source requirements;
- the complete canonical training-observation evidence;
- the robust distribution, thresholds, and policy version.

Training observations known after the model training cutoff are rejected.
Runtime evaluation starts from `VerifiedCalibratedInput`, which can be created
only after `CalibratedOutput::verify` succeeds against the exact calibration
artifact. Feature, source-health, quality, drift, and residual diagnostics must
be known no later than forecast issue time and retain nonzero evidence hashes.

## Distance and novelty method

Numeric features use a per-feature median and normal-consistent median absolute
deviation scale. Zero-MAD dimensions are rejected. Normalized values are
winsorized at an absolute value of six only while estimating scatter, bounding
the covariance influence of extreme training observations. The scatter matrix
uses explicit shrinkage toward average variance times the identity. Both the
scatter and precision matrices must be symmetric positive definite, and their
product must reproduce the identity within the declared numerical tolerance.

The runtime layer reports squared Mahalanobis distance and exact nearest-neighbor
distance over at most 4,096 frozen normalized rows and 256 features. It also
reports required/optional missingness, numeric-range violations, unknown or
missing categories, unsupported venues/products, model age, coefficient drift,
empirical residual diagnostics, aggregate quality, minimum critical-component
quality, and required-source health.

This is a deterministic median/MAD, winsorized, shrunk-scatter estimator. It is
not the Minimum Covariance Determinant or FastMCD algorithm. Thresholds are
versioned policy inputs and require later walk-forward and shadow validation;
they are not universal chi-square claims.

## Availability and abstention

The exact precedence is:

1. `model_incompatible`;
2. `source_unhealthy`;
3. `insufficient_data`;
4. `out_of_distribution`;
5. `experimental`;
6. `degraded`;
7. `available`.

Every decision contains the exact calibrated probability bits, horizon,
calibration output identity, input evidence, feature schema, source-health
evidence, quality evidence, applicability artifact, diagnostics, stable reasons,
and decision hash. Public verification re-derives the complete decision from
the model, forecast, and runtime context; it does not trust a stored enum or a
checksum alone. There is deliberately no replacement or fallback probability
field. The validation base rate remains context inside the calibrated artifact,
not an abstained production forecast.

## Known limitations and later gates

- Exact nearest-neighbor evaluation is deliberately bounded but remains linear
  in retained rows and feature count. A later measured runtime may replace it
  only with a versioned approximation whose recall and latency are validated.
- Median/MAD winsorization limits marginal and covariance influence but does not
  provide the breakdown guarantees of FastMCD.
- Empirical-residual and coefficient-drift values are evidence-bound inputs;
  their upstream monitoring estimators and shadow thresholds belong to Phase 8.
- The domain layer preserves all seven availability states and exact reasons.
  The existing event envelope has only four coarse states; a later explicit
  adapter must document any projection without discarding the domain decision.
- No daemon pipeline, persistence receipt, RPC, macOS surface, live-source soak,
  signed applicability package, shadow validation, or production promotion is
  claimed by this slice.
