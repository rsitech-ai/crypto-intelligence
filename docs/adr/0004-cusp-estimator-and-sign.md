# ADR 0004: Cusp estimator candidates and sign convention

- Status: Accepted
- Date: 2026-07-30

## Context

Stochastic-cusp research uses multiple equivalent parameter signs. Allowing
those conventions to cross model, API, or UI boundaries without an explicit
mapping would reverse the meaning of the controls, roots, and fold geometry.
The product also needs to reproduce stationary-density research without
mistaking in-sample replication for evidence that the cusp should influence a
production probability.

## Decision

The sole product convention is

```text
V(y; alpha, beta) = y^4 / 4 - beta * y^2 / 2 - alpha * y
dV/dy             = y^3 - beta * y - alpha
d2V/dy2           = 3 * y^2 - beta
D(alpha, beta)    = 4 * beta^3 - 27 * alpha^2
```

The strict multiple-equilibrium region is `beta > 0 && D > 0`. A fold has
`D == 0` and is not inside that strict region. The product uses `alpha`,
`beta`, and normalized state `y` in Rust, packages, RPC, logs, charts, and UI
labels.

Research using `z^4/4 + a*z^2/2 + b*z` must declare that alternative convention
and map `a = -beta`, `b = -alpha`, and `z = y` at one typed boundary. Ad hoc
sign changes are not accepted.

Two offline estimator candidates are retained:

- a stationary-density estimator used only to replicate published-style
  analyses and provide a comparator;
- a Student-t transition pseudo-likelihood estimator as the production
  candidate because it models time evolution and heavy-tailed innovations.

Candidate selection occurs inside point-in-time training and validation folds.
Neither estimator receives production weight from fit quality, visual
agreement, or this ADR. Cusp-derived features may affect production
probabilities only after the predefined walk-forward ablation gate demonstrates
stable incremental out-of-sample value beyond volatility, leverage, regime,
and microstructure baselines. A failed gate leaves the cusp view research-only
without a manual override.

## Alternatives considered

- Using the `a`, `b` research convention throughout the product was rejected
  because the approved design, API vocabulary, and fold parameterization use
  `alpha`, `beta`.
- Supporting both conventions in public APIs was rejected because implicit
  mapping would make sign/scaling defects difficult to detect.
- Selecting the stationary-density estimator as the production candidate was
  rejected because it does not model temporal transition dynamics and does not
  answer the heavy-tail requirement.
- Granting production eligibility after estimator convergence was rejected
  because numerical convergence is not evidence of incremental forecasting
  value or calibration.

## Consequences

All analytic derivatives, roots, stability, barriers, fold distance, fitting,
online inference, serialization, and presentation must derive from this
convention. Boundary constructors reject nonfinite controls and checked
evaluations reject nonfinite derived values. Root and fold algorithms still
need their own scale-aware numerical policies in later tasks.

The stationary estimator remains useful for research replication, but reports
must label it as a comparator. The Student-t transition estimator remains a
candidate until its synthetic recovery, walk-forward evaluation, uncertainty,
and ablation gates pass.

## Security and reliability

The cusp core is pure local computation and gains no network, credential,
storage, execution, or trading authority. Invalid source quality must cause
abstention at the later online boundary; it must never be converted to zero
risk. Serialized mathematical state is re-derived and checked so callers
cannot forge values that disagree with the approved controls and convention.

## Migration and reversal

Changing the product sign convention or estimator candidates requires a new
accepted ADR, an explicit version migration for model packages and RPC/UI
labels, regenerated mathematical fixtures, and complete root, derivative,
fold, fitting, replay, and ablation verification. Existing packages must not be
silently reinterpreted.
