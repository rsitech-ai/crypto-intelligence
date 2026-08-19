# Feature data dictionary

This file is the repository-owned human-readable companion to the
`feature-registry` machine snapshot. The registry defines contracts only; it
does not claim that a feature engine, materialized dataset, model, or live
inference path exists.

Every production or research feature must have:

- one validated registry definition keyed by `(feature_id, semantic_version)`;
- one section in this file whose anchor is stored by that definition;
- a nonzero formula hash and an explicit normalization version;
- point-in-time observations with source quality and lineage;
- an explicit required, optional, or experimental status.

The canonical JSON snapshot is emitted by
`FeatureRegistry::canonical_snapshot`. Its domain-separated BLAKE3 digest is
emitted by `FeatureRegistry::snapshot_digest`. Registry insertion order and
runtime capacity do not affect either artifact.

## Feature definition contract

| Field | Contract |
|---|---|
| `id` | Canonical lower-case feature identifier. |
| `version` | Semantic version of the complete feature contract. |
| `status` | `required`, `optional`, or `experimental`. |
| `value_type` | `fixed_decimal`, finite `float64`, `integer`, or `boolean`. |
| `entities` | One canonical scope: instrument, asset, venue, source, or global. |
| `required_inputs` | Bounded, nonempty, unique, auditable required input identifiers; no hidden dependency is permitted. |
| `event_time_policy` | Explicit source-event-time or receive-time policy. |
| `windows` | Bounded, uniquely named window declarations with typed time, UTC-anchored session, EWMA half-life and lookback, event-count, volume, or notional parameters. |
| `output_resolution` | Positive output cadence in nanoseconds. |
| `allowed_lateness` | Nonnegative lateness bound in nanoseconds. |
| `time_to_live` | Positive retention/usefulness bound, at least the output resolution. |
| `normalization` | Leakage-safe normalization family plus semantic version. |
| `missingness` | Explicit missingness, training-fold-only deterministic imputation, or abstention. |
| `quality_gate` | Exact minimum quality and expected-source coverage in millionths plus permitted source-health states. |
| `formula_hash` | Nonzero 32-byte digest of the formula implementation and parameters. |
| `documentation` | Durable anchor in this file. |

Exact duplicate `(id, version)` registrations fail without mutating the
registry. A changed formula, window, normalization, input, or semantic meaning
requires a new semantic version.

## Window execution contract

The `feature-engine` executes only validated `WindowDefinition` values. It
does not accept caller-selected runtime parameters that can diverge from the
registry snapshot.

- Tumbling and sliding time windows use the canonical UTC anchor
  `UnixNanos(0)` so 100 ms, one-second, one-minute, and longer boundaries stay
  epoch-congruent. Session-aligned windows use their registered positive UTC
  anchor.
- Sliding membership is half-open, canonical, and limited to 1,024 windows per
  event.
- Exponentially weighted windows use their registered half-life and bounded
  lookback. For elapsed
  event time `delta`, the previous value has decay
  `2^(-delta / half_life)`. These outputs are finite `float64` values governed
  by the declared numerical-tolerance rule. The state keeps an exact compacted
  prefix plus a caller-bounded late-event suffix of at most 4,096 events.
  The engine derives the compaction boundary from the bound tracker as the
  minimum healthy required-source watermark minus allowed lateness; callers
  cannot supply an arbitrary timestamp. Samples at or before that boundary
  become immutable, late events newer than it replay in event-time and
  arrival-tie-break order, and older corrections fail closed for persistent
  replay/materialization to handle. A late correction never moves the logical
  clock backward.
- Event-count windows use the registered event extent. A missing advance means
  a non-overlapping advance equal to the extent; an explicit advance cannot
  exceed the extent.
- Volume and notional windows close on the event that reaches or crosses the
  positive registered threshold. The closing total includes that event, then
  the next window starts from zero.

Watermark evaluation produces opaque evidence bound to one exact window and
canonical policy identity. Only that evidence may create provisional, final,
or invalid lifecycle revisions. Correction evidence additionally identifies a
configured, currently healthy source and can be minted only while the window
still evaluates as final. Repeated provisional updates, finalization,
invalidation, and late correction all append immutable monotonically
increasing revisions. After source recovery, fresh correction evidence may
append a corrected revision after an invalid revision; the invalid historical
revision remains retained.

Task 2 parity covers live-recorded, serialized replay-input, and batch-iterator
drivers through the same engine implementation. It is not yet an end-to-end
raw-WAL or Parquet storage integration claim; those adapters remain later
phase work.

## Observation contract

| Field | Contract |
|---|---|
| `feature_id` | Must identify a registered definition. |
| `feature_version` | Must match that exact definition version. |
| `entity_id` | Generation-aware canonical domain identity matching the definition scope. |
| `window_id` | Must name a window declared by the definition. |
| `resolution` | Must equal the definition output resolution. |
| `value` | Typed value when present; `null` when the observation is explicitly missing. |
| `value_type` | Must match both the present value, when any, and the definition. |
| `event_time_start` | Positive inclusive economic-window start in Unix nanoseconds. |
| `event_time_end` | Positive economic-window end, not before the start. |
| `as_known_at` | Earliest time the complete trustworthy observation was available. |
| `computed_at` | Emission time, not before `as_known_at`. |
| `watermark` | Positive event-time watermark, not after `computed_at`; may be absent only on an invalid missing observation when no required source has ever advanced. |
| `finality_as_known_at` | Earliest processing time at which the exact watermark/health decision was knowable; it cannot be after `as_known_at` or `computed_at`. |
| `finality_state` | `provisional`, `final`, `corrected`, or `invalid`. |
| `revision` | Positive monotonic observation revision; `corrected` requires revision greater than one. |
| `source_coverage` | Bounded expected canonical source identities plus the observed subset and their health states. |
| `quality_score` | Exact integer millionths in inclusive range 0 through 1,000,000. |
| `normalization_version` | Must equal the registered normalization version. |
| `formula_hash` | Must equal the registered nonzero formula hash. |
| `code_commit` | Full lower-case Git object ID: 40 hex characters for SHA-1 or 64 for SHA-256 repositories. |
| `lineage_hash` | Nonzero 32-byte digest of the complete observation lineage DAG. |
| `missingness_reason` | Carried by the missing datum variant; it cannot coexist with a value. |

The universal ordering invariant is:

```text
event_time_start <= event_time_end <= as_known_at <= computed_at
watermark <= computed_at, when a watermark exists
finality_as_known_at <= as_known_at
```

Historical training and evaluation joins must use
`as_known_at <= prediction_time`. Joining only by economic event time leaks
future availability.

Final and corrected records are accepted only after the watermark reaches the
window end plus allowed lateness. Invalid records must be missing records. A
low quality score or rejected source state may be retained only as an invalid
observation; it cannot pass the usable-observation quality gate.

### Missingness reasons

Missing values never default silently to zero. The exact reasons are:

- `not_listed`
- `source_not_supported`
- `source_disconnected`
- `sequence_gap`
- `stale`
- `insufficient_history`
- `window_not_final`
- `below_liquidity_threshold`
- `vendor_revision_pending`
- `model_not_applicable`
- `privacy_or_license_restriction`
- `unknown`

Models must consume an explicit missingness indicator, apply a documented
deterministic imputation learned only on the training fold, or abstain.

The feature engine must derive `expected_sources` from the point-in-time
eligible-source universe, not from whichever sources happened to respond. The
eligible set and its policy belong in lineage so a producer cannot shrink the
coverage denominator.

### No-leakage rule

Normalization parameters must be fitted only from data available before the
evaluated prediction. Permitted policies include robust rolling statistics,
expanding quantiles, training-fold z-scores, logarithmic transforms,
volatility scaling, asset-relative transforms, point-in-time cross-sectional
ranks, time-of-week adjustment, and liquidity-bucket normalization.

Full-history statistics, future fill, label-conditioned transforms, random
time-series splits, and normalization fitted across evaluation boundaries are
forbidden.

<a id="realized-volatility"></a>
## realized-volatility

- Registry ID: `realized_volatility`
- Version: `1.0.0`
- Status: required contract fixture; computation is not implemented in Task 1
- Entity scope: instrument
- Value type: fixed decimal
- Required input: `normalized.trades`
- Event-time policy: source event time
- Window: `rolling_5m`, sliding five minutes, advanced every minute
- Output resolution: one minute
- Allowed lateness: five seconds
- Time to live: one day
- Normalization: versioned robust rolling policy
- Missingness: explicit
- Quality gate: at least 900,000 quality millionths, 1,000,000 expected-source coverage millionths, and healthy or recovering sources

The formula digest identifies the exact realized-volatility estimator,
sampling rule, annualization convention, and parameters. Those details must be
specified when the computation is implemented; this Task 1 fixture proves only
the registry and observation boundary.

Version `2.0.0` is the implemented consolidated-price analytical contract. It
does not alter or reinterpret the version `1.0.0` fixture:

- Entity scope: base asset, derived from the consolidated fair-price policy
- Value type: finite canonical `float64`
- Required input: `consolidated.fair_price`
- Return convention: consecutive one-minute log returns from exact positive
  fixed-decimal prices
- Windows: `rolling_5m`, `rolling_15m`, and `rolling_1h`, half-open sliding
  windows advanced every minute
- Realized variance: sum of squared returns
- Realized volatility: square root of realized variance, unannualized
- Bipower variation: `(pi / 2)` times the sum of adjacent absolute-return
  products
- Jump variation: `max(realized variance - bipower variation, 0)`
- Observation bound: 65,536 prices/measures per analytical call
- Output policy: reject nonfinite results and canonicalize negative zero
- Normalization: version `2.0.0` identity/no transform; any later robust or
  seasonal transform is a separate versioned point-in-time feature contract

The version `2.0.0` formula digest commits the return convention, one-minute
sampling cadence, annualization policy, realized/bipower/jump estimators,
capacity bounds, and finite-output policy. Intraday seasonal adjustment accepts
only a positive versioned baseline whose fitted-through and as-known-at times
are no later than the evaluated window start.

## Task 4 price, return, and volatility catalogue

The Feature Platform team owns the required contracts below. Unless a section
states otherwise, each output is a finite dimensionless `float64` at one-minute
cadence for base-asset entities, supports registered 5-minute, 15-minute, and
one-hour sliding windows, uses source-event time, permits five seconds of
lateness, expires after one day, and requires 900,000 quality millionths,
complete expected-source coverage, and healthy or recovering sources.
Realized covariance and correlation use a canonically ordered base-asset pair
entity. Anchored VWAP uses the registered UTC-day session window. EWMA uses
exactly five minutes of one-minute returns, a registered five-minute half-life,
and the first squared return as its initial state. It cannot be reinterpreted
with a 15-minute or one-hour input window.
Observations use explicit missingness and identity normalization. Formula
digests bind the displayed estimator, fixed parameters, one-minute sampling,
registered horizons, capacity limits, and semantic version. Input lineage,
eligible-source identity, finalization evidence, code revision, and output
value are committed by the observation lineage hash. Leakage review: all
inputs and fitted artifacts must satisfy `as_known_at` no later than the
observation's point-in-time boundary. Initial production contracts were added
as version `1.0.0` on 2026-07-29; realized volatility retains its separately
documented `2.0.0` contract above.

### Task 4 ownership and traceability

The following metadata applies to every Task 4 feature section and is reviewed
together with its registry definition:

- Named maintainer: Feature Platform team.
- Mathematical or procedural definition: the formula in the feature section
  and the exact formula identity committed by the registry hash.
- Units: dimensionless unless the section states price-per-time units.
- Supported entities and update cadence: the common catalogue contract above,
  with the explicit asset-pair, UTC-session, and EWMA exceptions.
- Source dependencies: the registry definition's bounded
  `required_inputs`; no undeclared input is permitted.
- Missingness behavior: expected data absence is an explicit invalid
  observation with a typed reason; trust, parameter, and domain errors fail.
- Expected range and invariants: the feature section plus finite-output,
  window-geometry, quality, coverage, and lineage registry validation.
- Leakage review: all source observations, watermark decisions, and fitted
  artifacts obey the point-in-time rules above.
- Unit tests: `price_volatility_features` and `reference_measures`; deterministic
  stream/WAL/Parquet replay remains a phase-level §18.8 gate and is not claimed
  by this library slice.
- Change history: initial Task 4 contract 2026-07-29; identity corrections
  remain semantic-version and formula-hash controlled.

Every closed-catalogue ID has an ownership row, which the catalogue test checks:

| Feature ID | Maintainer | Verification family |
|---|---|---|
| `log_return` | Feature Platform | price path |
| `cumulative_return` | Feature Platform | price path |
| `high_low_range` | Feature Platform | price path |
| `open_close_range` | Feature Platform | price path |
| `rolling_vwap_distance` | Feature Platform | normalized trade/VWAP |
| `anchored_vwap_distance` | Feature Platform | normalized trade/VWAP |
| `consolidated_fair_price_distance` | Feature Platform | trade/fair price |
| `trend_slope` | Feature Platform | price path |
| `trend_acceleration` | Feature Platform | price path |
| `maximum_drawdown` | Feature Platform | price path |
| `maximum_run_up` | Feature Platform | price path |
| `upside_semivariance` | Feature Platform | realized measures |
| `downside_semivariance` | Feature Platform | realized measures |
| `return_autocorrelation` | Feature Platform | return distribution |
| `variance_ratio` | Feature Platform | return distribution |
| `rolling_skewness` | Feature Platform | return distribution |
| `rolling_excess_kurtosis` | Feature Platform | return distribution |
| `return_reversal` | Feature Platform | return distribution |
| `finalized_interval_gap` | Feature Platform | adjacent final windows |
| `realized_variance` | Feature Platform | realized measures |
| `realized_volatility` | Feature Platform | realized measures |
| `exponentially_weighted_volatility` | Feature Platform | EWMA |
| `parkinson_volatility` | Feature Platform | OHLC range |
| `garman_klass_volatility` | Feature Platform | OHLC range |
| `bipower_variation` | Feature Platform | realized measures |
| `jump_variation` | Feature Platform | realized measures |
| `volatility_of_volatility` | Feature Platform | derived volatility |
| `volatility_term_ratio` | Feature Platform | derived volatility |
| `seasonality_adjusted_volatility` | Feature Platform | fitted seasonal artifact |
| `realized_covariance` | Feature Platform | paired realized measures |
| `realized_correlation` | Feature Platform | paired realized measures |
| `volatility_forecast_residual` | Feature Platform | point-in-time forecast artifact |

<a id="log-return"></a>
### log-return

`ln(close_t / close_t-h)` from consolidated fair prices. Expected range is
unbounded signed finite log-return; inputs must be positive.

<a id="cumulative-return"></a>
### cumulative-return

`close_t / open_h - 1` from consolidated fair prices. Expected range is
`(-1, +infinity)` subject to finite representation.

<a id="high-low-range"></a>
### high-low-range

`high_h / low_h - 1` from positive consolidated fair prices. Expected range is
nonnegative.

<a id="open-close-range"></a>
### open-close-range

`close_h / open_h - 1` from consolidated fair prices. Expected range is
`(-1, +infinity)` subject to finite representation.

<a id="rolling-vwap-distance"></a>
### rolling-vwap-distance

`price / rolling_vwap - 1` from normalized trades plus consolidated fair
price. Each trade is derived from a validated normalized event envelope and
exact instrument definition; trade event time, normalization availability,
source, quality, event identity, and finalization evidence enter lineage.
Zero-size trades are ignored and an all-zero denominator is unavailable.

<a id="anchored-vwap-distance"></a>
### anchored-vwap-distance

`price / utc_session_vwap - 1` over the registered UTC-day session anchor.
Trade, anchor, entity, finality, and source lineage are required.

<a id="consolidated-fair-price-distance"></a>
### consolidated-fair-price-distance

`price / robust_consolidated_fair_price - 1`. The fair price must come from the
policy-fixed eligible venue universe; `price` is the latest normalized trade
in the same finalized window. Both trade and fair-price source universes enter
coverage and lineage. The fair price is non-executable.

<a id="trend-slope"></a>
### trend-slope

Event-time ordinary-least-squares price slope. Units are price units per
nanosecond. The production recipe requires the complete exact one-minute grid;
the lower-level analytical helper also supports validated irregular samples.

<a id="trend-acceleration"></a>
### trend-acceleration

Event-time ordinary-least-squares slope of adjacent interval price slopes.
Units are price units per nanosecond squared.

<a id="maximum-drawdown"></a>
### maximum-drawdown

Maximum `(running_peak - price) / running_peak`. Expected range is `[0, 1)`.

<a id="maximum-run-up"></a>
### maximum-run-up

Maximum `(price - running_trough) / running_trough`. Expected range is
nonnegative.

<a id="upside-semivariance"></a>
### upside-semivariance

Sum of squared positive consecutive log returns. Expected range is
nonnegative.

<a id="downside-semivariance"></a>
### downside-semivariance

Sum of squared negative consecutive log returns. Expected range is
nonnegative.

<a id="return-autocorrelation"></a>
### return-autocorrelation

Pearson correlation between consecutive log returns and their one-sample lag.
Expected range is `[-1, 1]`; zero variance is undefined and rejected.

<a id="variance-ratio"></a>
### variance-ratio

Population variance of overlapping two-return sums divided by twice the
one-return population variance. Zero one-return variance is undefined and
rejected.

<a id="rolling-skewness"></a>
### rolling-skewness

Population third central moment divided by population variance to power
`1.5`. Expected range is signed finite.

<a id="rolling-excess-kurtosis"></a>
### rolling-excess-kurtosis

Population fourth central moment divided by squared population variance minus
three. Expected range is signed finite.

<a id="return-reversal"></a>
### return-reversal

Mean `-sign(r_t) * r_t+1` after moves with absolute log return at least `0.05`.
No qualifying move is explicit insufficient-history missingness.

<a id="finalized-interval-gap"></a>
### finalized-interval-gap

`current_final_open / previous_final_close - 1`. Both intervals must be final,
adjacent, chronological, and for the same canonical entity. Both watermark
decisions, source universes, and price lineages enter the output lineage.

<a id="realized-variance"></a>
### realized-variance

Sum of squared consecutive one-minute log returns. Expected range is
nonnegative.

<a id="exponentially-weighted-volatility"></a>
### exponentially-weighted-volatility

Square root of the time-aware EWMA of squared returns over exactly five minutes
of complete one-minute returns, with registered five-minute half-life and the
first squared return as the initial state. The watermark-bound EWMA state owns
replay ordering.

<a id="parkinson-volatility"></a>
### parkinson-volatility

Square root of mean `log(high / low)^2 / (4 * ln(2))` from finalized,
lineage-bearing OHLC bars aggregated from the exact consolidated fair-price
grid. Expected range is nonnegative.

<a id="garman-klass-volatility"></a>
### garman-klass-volatility

Square root of the mean Garman-Klass variance from finalized, valid,
lineage-bearing OHLC bars aggregated from the exact consolidated fair-price
grid. A mean below negative `f64::EPSILON` is rejected; a mean in the inclusive
range from negative `f64::EPSILON` through zero is clamped to zero before the
square root. Expected range is nonnegative.

<a id="bipower-variation"></a>
### bipower-variation

`(pi / 2) * sum(abs(r_t-1) * abs(r_t))`. Expected range is nonnegative.

<a id="jump-variation"></a>
### jump-variation

`max(realized_variance - bipower_variation, 0)`. Expected range is
nonnegative.

<a id="volatility-of-volatility"></a>
### volatility-of-volatility

Sample standard deviation of aligned realized-volatility observations.
The inputs must be final `realized_volatility@2.0.0` observations with
monotonic event ends and watermarks, identical entity and source coverage, and
one observation at every exact one-minute boundary of the derived window.
Sparse or irregular series are rejected. Expected range is nonnegative.

<a id="volatility-term-ratio"></a>
### volatility-term-ratio

Short-horizon realized volatility divided by positive long-horizon realized
volatility. Both inputs must be final `realized_volatility@2.0.0` observations
for the same asset and event-time end. The only identities are 5-minute over
15-minute and 15-minute over one-hour; the output window is the long horizon.
Both feature observations and horizons enter lineage.

<a id="seasonality-adjusted-volatility"></a>
### seasonality-adjusted-volatility

Realized volatility divided by a point-in-time UTC time-of-week factor. The
baseline entity, exact UTC hour-of-week bucket, fit interval, availability,
quality, version, and lineage are required and must predate the evaluated
window. Bucket zero is Monday 00:00-00:59 UTC; the Unix epoch's Thursday
00:00 hour is bucket 72. The declared fit interval must end exactly at the baseline's
`fitted_through` time.

<a id="realized-covariance"></a>
### realized-covariance

Realized covariation, the uncentered sum of aligned high-frequency log-return
products, over two point-in-time windows. Pair ordering is canonical and part
of identity.

<a id="realized-correlation"></a>
### realized-correlation

Realized covariation divided by the square root of the product of the two
realized variances. Expected range is `[-1, 1]`; zero realized variance is
undefined and rejected.

<a id="volatility-forecast-residual"></a>
### volatility-forecast-residual

Realized volatility minus a nonnegative point-in-time forecast. Forecast model
package, semantic version, generation time, availability, quality, artifact
hash, and lineage are required. Generation and availability must be no later
than the evaluated window start.

## trade-imbalance

- Registry ID: `trade_imbalance`
- Version: `1.0.0`
- Status: required contract-ordering fixture; computation is not implemented in Task 1
- Entity scope: instrument
- Value type: fixed decimal
- Required input: `normalized.trades`
- Event-time policy: source event time
- Window: `rolling_5m`, sliding five minutes, advanced every minute
- Output resolution: one minute
- Allowed lateness: five seconds
- Time to live: one day
- Normalization: versioned robust rolling policy
- Missingness: explicit
- Quality gate: at least 900,000 quality millionths, 1,000,000 expected-source coverage millionths, and healthy or recovering sources

This fixture exists to prove registry ordering is independent of insertion
order. Its formula parameters must be documented and hashed when the
computation is implemented.
