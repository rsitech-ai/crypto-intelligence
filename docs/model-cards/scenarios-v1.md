# Conditional scenarios v1

## Purpose and status

This artifact is the bounded Phase 4 Task 8 conditional-simulation layer. It
turns one evidence-bound market condition and one frozen process/impact model
into a reproducible distribution of simulated outcomes. It is not a price
forecast, a calibrated production risk measure, an execution recommendation,
or an observed future path. Every representative path carries the
`simulated` label.

The implementation is research-only until the Phase 8 CRPS, interval coverage,
tail coverage, threshold-crossing calibration, shadow, and promotion gates are
complete. A successful deterministic replay proves implementation parity, not
statistical calibration.

## Inputs and identity

The scenario condition binds the issue and knowledge times, entity, starting
price and volatility, regime/transition/Cusp/fast state, a compressed systemic
return condition, point-in-time liquidity/open interest, explicit available or
unavailable options state, upstream forecast/applicability/source evidence, and
the sorted model-package identities. Component knowledge times may not exceed
the condition knowledge time.

The engine binds a frozen empirical innovation set, optional empirical jump
returns, stochastic-volatility and state-conditioned jump parameters, and a
liquidity/impact/liquidation model. The process model declares its calibration
step size; simulation rejects a different step size because per-step
innovations, persistence, and jump probabilities are not safely interchangeable
across time geometries.

The configuration binds the SplitMix64 seed, bounded path count and horizons,
event thresholds, sweep notionals, representative quantiles, liquidation
feedback switch, and bounded user stress injections. Canonical BLAKE3 identities
include collection lengths and exact floating-point bit patterns. Public result
verification validates every upstream artifact, re-simulates the bounded paths,
and compares the complete result rather than trusting a checksum alone.

## Simulation and summaries

Empirical innovations are centered and sample-standardized, then resampled with
replacement. Each step combines idiosyncratic and systemic innovations using a
frozen correlation, stochastic annualized volatility, a signed options-skew
adjustment when options are available, state-conditioned empirical jumps, the
compressed systemic condition, and active user stresses. The liquidity model
adds depth/fragility-dependent impact and optional liquidation/open-interest
feedback. Arithmetic, inputs, work, and allocations are bounded and fail
closed on invalid or non-finite values.

The engine retains full paths only inside the local simulation call. Its public
result contains return and volatility quantiles by horizon, expected and 95th
percentile sweep cost, liquidation and open-interest ranges, event-threshold
crossing probabilities, and a bounded set of quantile-selected representative
paths. Empirical quantiles use the documented NIST linear convention
`position = n * p - 0.5`, with endpoint clipping.

## Known limitations and later gates

- The systemic condition is a compressed cross-asset input, not a joint
  multi-asset path simulator or fitted contagion model.
- Empirical resampling preserves the supplied marginal observations but does
  not by itself preserve volatility clustering, serial dependence, or
  multivariate tail dependence. The source artifact must document how its
  innovations and jumps were estimated.
- Process, impact, stress, and applicability thresholds are versioned model
  inputs. Their numerical bounds are software-safety bounds, not evidence that
  the chosen values are economically calibrated.
- Options-unavailable behavior is explicit and neutral; it is not an invented
  options estimate. Options-present behavior is a bounded multiplier/skew
  condition, not an options-surface simulator.
- Quantile-selected paths are illustrative members of the simulated sample,
  not medoids, predictions, confidence bands, or likely realized paths.
- The current slice has no durable forecast ledger, daemon integration, RPC,
  macOS display, live-source soak, signed scenario package, latency benchmark,
  shadow evaluation, or production promotion claim.

## Method references

- NIST/SEMATECH, empirical distribution and quantile conventions:
  <https://www.itl.nist.gov/div898/software/dataplot/refman2/auxillar/empqua.htm>
- Rust `f64` total ordering and bit identity:
  <https://doc.rust-lang.org/stable/core/primitive.f64.html>
- Rust wrapping integer arithmetic used by the frozen RNG:
  <https://doc.rust-lang.org/stable/std/primitive.u64.html>
