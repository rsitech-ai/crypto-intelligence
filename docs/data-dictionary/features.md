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
| `value_type` | `fixed_decimal`, bounded canonical `fixed_decimal_map`, finite `float64`, `integer`, or `boolean`. |
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

## Task 5 market-microstructure catalogue

The Market Microstructure team owns these instrument-scoped contracts. Static
book outputs use trusted, normal, non-stale L2 state in one-second tumbling
windows. Flow outputs use bounded, finalized one-minute sliding windows.
Displayed-depth recovery outputs use bounded fifteen-minute sliding windows.
All advance at one-second cadence, permit five seconds of lateness, expire
after one day, require complete expected-source coverage and healthy sources,
and use explicit missingness with identity normalization.

Authoritative prices, quantities, depths, and notionals remain checked fixed
decimals. Conversion to `float64` occurs only for explicitly analytical ratios,
regressions, dispersion, entropy, rates, and response estimates. The required
depth bands are separate identities at 1, 2, 5, 10, 25, 50, and 100 basis
points; each boundary is inclusive and part of its formula hash. USD sweep
recipes additionally require a point-in-time quote-to-USD conversion input.
Until that conversion evidence is available, they emit
`source_not_supported`; the quote-notional sweep helper is not presented as a
USD result.

The current pure quote-notional helpers support spot instruments, where
checked `price * base quantity` has quote-asset units. Perpetual, future, and
option inputs fail with `source_not_supported` until the computation is bound
to the exact point-in-time `InstrumentDefinition::quote_notional` contract;
inverse contract counts are never treated as base quantity.

Lifecycle rates require one continuous, gap-free, versioned and licensed
certified-L3 capability. Aggregate L2 never implies a placement, cancellation,
or modification. Trade side is supplied by the venue or inferred by a
versioned venue-specific rule, with that authority retained in lineage.
Top-of-book OFI is the displayed-quote-pressure identity; it is not a
cancellation-adjusted measure. Replenishment, sweep, impact, and adverse
selection retain `displayed`, `trade_print`, or `proxy` semantics and do not
claim causal attribution, maker fills, fees, rebates, or queue position.

Formula hashes bind the ID, semantic version, exact formula text, window,
value type, required inputs, and bounded book/recovery/flow capacities.
Observation production must additionally bind the exact event/book lineage,
instrument generation, source session and sequence, quality, ordering and
reference-price policies, aggressor authority, versioned point-in-time
threshold policy where configured, watermark decision, and code revision. The
catalogue proves in-memory registry validity; durable
stream/WAL/Parquet parity remains a later phase gate.

The current closed observation producer supports 37 parameter-free snapshot
recipes: spread, the seven fixed bid/ask depth and imbalance bands, weighted
midpoint, microprice, bid/ask price-level gap density over all authenticated
displayed levels, three-level bid/ask quadratic slope and convexity,
active-level counts, entropy/concentration, and explicit
`source_not_supported` USD sweep records. It also supports the 12
venue-supplied-side core trade-flow recipes. Wall, slippage-bounded execution,
temporal/recovery, inferred-side, and thresholded shock/cluster recipes remain
computation-only or unavailable until every configuration value is frozen into
recoverable formula identity. Present book outputs require a validated
instrument-definition event whose generation, product, exact price tick, and
quantity step match the trusted book; that definition's lineage and quality
participate in the observation. Snapshot and delta events are both supported
when the order-book engine returns an exact single-event post-apply receipt. A
snapshot that replays buffered deltas is applied without such a receipt, so
partial lineage cannot emit a feature. Observed-empty trade emission remains
unavailable until the ingestion layer supplies an authenticated
trade-partition completeness receipt.

### Task 5 ownership and traceability

| Feature ID | Maintainer | Verification family |
|---|---|---|
| `absolute_spread` | Market Microstructure | trusted book core |
| `relative_spread` | Market Microstructure | trusted book core |
| `bid_depth_1bps` | Market Microstructure | exact band depth |
| `bid_depth_2bps` | Market Microstructure | exact band depth |
| `bid_depth_5bps` | Market Microstructure | exact band depth |
| `bid_depth_10bps` | Market Microstructure | exact band depth |
| `bid_depth_25bps` | Market Microstructure | exact band depth |
| `bid_depth_50bps` | Market Microstructure | exact band depth |
| `bid_depth_100bps` | Market Microstructure | exact band depth |
| `ask_depth_1bps` | Market Microstructure | exact band depth |
| `ask_depth_2bps` | Market Microstructure | exact band depth |
| `ask_depth_5bps` | Market Microstructure | exact band depth |
| `ask_depth_10bps` | Market Microstructure | exact band depth |
| `ask_depth_25bps` | Market Microstructure | exact band depth |
| `ask_depth_50bps` | Market Microstructure | exact band depth |
| `ask_depth_100bps` | Market Microstructure | exact band depth |
| `book_imbalance_1bps` | Market Microstructure | exact band imbalance |
| `book_imbalance_2bps` | Market Microstructure | exact band imbalance |
| `book_imbalance_5bps` | Market Microstructure | exact band imbalance |
| `book_imbalance_10bps` | Market Microstructure | exact band imbalance |
| `book_imbalance_25bps` | Market Microstructure | exact band imbalance |
| `book_imbalance_50bps` | Market Microstructure | exact band imbalance |
| `book_imbalance_100bps` | Market Microstructure | exact band imbalance |
| `weighted_midpoint` | Market Microstructure | trusted top of book |
| `microprice` | Market Microstructure | trusted top of book |
| `bid_book_slope` | Market Microstructure | quadratic book shape |
| `ask_book_slope` | Market Microstructure | quadratic book shape |
| `bid_book_convexity` | Market Microstructure | quadratic book shape |
| `ask_book_convexity` | Market Microstructure | quadratic book shape |
| `bid_liquidity_wall_distance` | Market Microstructure | material wall |
| `ask_liquidity_wall_distance` | Market Microstructure | material wall |
| `bid_price_level_gap_density` | Market Microstructure | tick gap density |
| `ask_price_level_gap_density` | Market Microstructure | tick gap density |
| `expected_buy_sweep_cost_usd_10000` | Market Microstructure | USD conversion gated sweep |
| `expected_sell_sweep_cost_usd_10000` | Market Microstructure | USD conversion gated sweep |
| `maximum_executable_buy_quote_notional` | Market Microstructure | bounded sweep |
| `maximum_executable_sell_quote_notional` | Market Microstructure | bounded sweep |
| `book_quote_age_ms` | Market Microstructure | freshness |
| `bid_stale_side_duration_ns` | Market Microstructure | observed side change |
| `ask_stale_side_duration_ns` | Market Microstructure | observed side change |
| `bid_active_price_levels` | Market Microstructure | level count |
| `ask_active_price_levels` | Market Microstructure | level count |
| `local_book_entropy` | Market Microstructure | depth distribution |
| `depth_concentration_by_level` | Market Microstructure | depth distribution |
| `bid_depth_change` | Market Microstructure | temporal book evidence |
| `ask_depth_change` | Market Microstructure | temporal book evidence |
| `absolute_spread_change` | Market Microstructure | temporal book evidence |
| `displayed_depth_replenishment_fraction` | Market Microstructure | displayed recovery proxy |
| `displayed_depth_recovery_time_ns` | Market Microstructure | displayed recovery proxy |
| `book_half_life_ns` | Market Microstructure | displayed recovery proxy |
| `aggressive_buy_count` | Market Microstructure | aggressor totals |
| `aggressive_sell_count` | Market Microstructure | aggressor totals |
| `aggressive_buy_quantity` | Market Microstructure | aggressor totals |
| `aggressive_sell_quantity` | Market Microstructure | aggressor totals |
| `aggressive_buy_notional` | Market Microstructure | aggressor totals |
| `aggressive_sell_notional` | Market Microstructure | aggressor totals |
| `signed_trade_count_imbalance` | Market Microstructure | trade imbalance |
| `signed_trade_quantity_imbalance` | Market Microstructure | trade imbalance |
| `signed_trade_notional_imbalance` | Market Microstructure | trade imbalance |
| `top_of_book_ofi` | Market Microstructure | displayed quote OFI |
| `placement_rate_per_second` | Market Microstructure | certified L3 lifecycle |
| `cancellation_rate_per_second` | Market Microstructure | certified L3 lifecycle |
| `modification_rate_per_second` | Market Microstructure | certified L3 lifecycle |
| `cancellation_to_trade_ratio` | Market Microstructure | certified L3 lifecycle |
| `quote_to_trade_ratio` | Market Microstructure | certified L3 lifecycle |
| `trade_intensity_per_second` | Market Microstructure | bounded trade window |
| `trade_interarrival_coefficient_of_variation` | Market Microstructure | bounded trade window |
| `signed_volume_at_price` | Market Microstructure | bounded exact map |
| `trade_shock_midprice_impact` | Market Microstructure | trade response proxy |
| `trade_effective_spread` | Market Microstructure | trade response proxy |
| `trade_realized_spread` | Market Microstructure | trade response proxy |
| `trade_adverse_selection_proxy` | Market Microstructure | trade response proxy |
| `trade_print_sweep_direction` | Market Microstructure | trade-print sweep proxy |
| `large_trade_cluster_count` | Market Microstructure | fixed-threshold cluster |
| `clustered_large_trade_count` | Market Microstructure | fixed-threshold cluster |
| `clustered_large_trade_notional` | Market Microstructure | fixed-threshold cluster |
| `large_trade_cluster_activity_rate` | Market Microstructure | fixed-threshold cluster |

### Task 5 per-feature contracts

The table below is normative for version `1.0.0`. `Book` means a trusted,
normal, fresh, sequence-valid L2 snapshot; its explicit absence reasons are
`source_disconnected`, `sequence_gap`, and `stale`. `Flow` additionally uses
`insufficient_history` or `window_not_final`. `L3` adds
`source_not_supported` and `privacy_or_license_restriction`. `USD` remains
`source_not_supported` until the point-in-time quote-to-USD input is present.
Zero denominators and invalid policy or domain inputs fail closed rather than
becoming zero. All ranges below are mathematical ranges before finite
representation limits.

| Feature and anchor | Exact formula identity | Units and expected range | Parameters | Missingness profile |
|---|---|---|---|---|
| <a id="absolute-spread"></a>`absolute_spread` | `ask-best-bid;exact` | quote price, `>= 0` | Book; one-second snapshot | Book |
| <a id="relative-spread"></a>`relative_spread` | `absolute-spread/same-venue-midpoint` | dimensionless, `>= 0` | Book; same-venue midpoint v1 | Book |
| <a id="bid-depth-1bps"></a>`bid_depth_1bps` | `sum-bid-quantity-within-inclusive-1bps` | base quantity, `>= 0` | Book; inclusive 1 bps band | Book |
| <a id="bid-depth-2bps"></a>`bid_depth_2bps` | `sum-bid-quantity-within-inclusive-2bps` | base quantity, `>= 0` | Book; inclusive 2 bps band | Book |
| <a id="bid-depth-5bps"></a>`bid_depth_5bps` | `sum-bid-quantity-within-inclusive-5bps` | base quantity, `>= 0` | Book; inclusive 5 bps band | Book |
| <a id="bid-depth-10bps"></a>`bid_depth_10bps` | `sum-bid-quantity-within-inclusive-10bps` | base quantity, `>= 0` | Book; inclusive 10 bps band | Book |
| <a id="bid-depth-25bps"></a>`bid_depth_25bps` | `sum-bid-quantity-within-inclusive-25bps` | base quantity, `>= 0` | Book; inclusive 25 bps band | Book |
| <a id="bid-depth-50bps"></a>`bid_depth_50bps` | `sum-bid-quantity-within-inclusive-50bps` | base quantity, `>= 0` | Book; inclusive 50 bps band | Book |
| <a id="bid-depth-100bps"></a>`bid_depth_100bps` | `sum-bid-quantity-within-inclusive-100bps` | base quantity, `>= 0` | Book; inclusive 100 bps band | Book |
| <a id="ask-depth-1bps"></a>`ask_depth_1bps` | `sum-ask-quantity-within-inclusive-1bps` | base quantity, `>= 0` | Book; inclusive 1 bps band | Book |
| <a id="ask-depth-2bps"></a>`ask_depth_2bps` | `sum-ask-quantity-within-inclusive-2bps` | base quantity, `>= 0` | Book; inclusive 2 bps band | Book |
| <a id="ask-depth-5bps"></a>`ask_depth_5bps` | `sum-ask-quantity-within-inclusive-5bps` | base quantity, `>= 0` | Book; inclusive 5 bps band | Book |
| <a id="ask-depth-10bps"></a>`ask_depth_10bps` | `sum-ask-quantity-within-inclusive-10bps` | base quantity, `>= 0` | Book; inclusive 10 bps band | Book |
| <a id="ask-depth-25bps"></a>`ask_depth_25bps` | `sum-ask-quantity-within-inclusive-25bps` | base quantity, `>= 0` | Book; inclusive 25 bps band | Book |
| <a id="ask-depth-50bps"></a>`ask_depth_50bps` | `sum-ask-quantity-within-inclusive-50bps` | base quantity, `>= 0` | Book; inclusive 50 bps band | Book |
| <a id="ask-depth-100bps"></a>`ask_depth_100bps` | `sum-ask-quantity-within-inclusive-100bps` | base quantity, `>= 0` | Book; inclusive 100 bps band | Book |
| <a id="book-imbalance-1bps"></a>`book_imbalance_1bps` | `(bid-depth-ask-depth)/(bid-depth+ask-depth);1bps` | dimensionless, `[-1, 1]` | Book; inclusive 1 bps band | Book |
| <a id="book-imbalance-2bps"></a>`book_imbalance_2bps` | `(bid-depth-ask-depth)/(bid-depth+ask-depth);2bps` | dimensionless, `[-1, 1]` | Book; inclusive 2 bps band | Book |
| <a id="book-imbalance-5bps"></a>`book_imbalance_5bps` | `(bid-depth-ask-depth)/(bid-depth+ask-depth);5bps` | dimensionless, `[-1, 1]` | Book; inclusive 5 bps band | Book |
| <a id="book-imbalance-10bps"></a>`book_imbalance_10bps` | `(bid-depth-ask-depth)/(bid-depth+ask-depth);10bps` | dimensionless, `[-1, 1]` | Book; inclusive 10 bps band | Book |
| <a id="book-imbalance-25bps"></a>`book_imbalance_25bps` | `(bid-depth-ask-depth)/(bid-depth+ask-depth);25bps` | dimensionless, `[-1, 1]` | Book; inclusive 25 bps band | Book |
| <a id="book-imbalance-50bps"></a>`book_imbalance_50bps` | `(bid-depth-ask-depth)/(bid-depth+ask-depth);50bps` | dimensionless, `[-1, 1]` | Book; inclusive 50 bps band | Book |
| <a id="book-imbalance-100bps"></a>`book_imbalance_100bps` | `(bid-depth-ask-depth)/(bid-depth+ask-depth);100bps` | dimensionless, `[-1, 1]` | Book; inclusive 100 bps band | Book |
| <a id="weighted-midpoint"></a>`weighted_midpoint` | `quantity-weighted-best-bid-ask-midpoint` | quote price, inside best quotes | Book; positive best quantities | Book |
| <a id="microprice"></a>`microprice` | `(ask-price*bid-qty+bid-price*ask-qty)/(bid-qty+ask-qty)` | quote price, inside best quotes | Book; positive best quantities | Book |
| <a id="bid-book-slope"></a>`bid_book_slope` | `quadratic-cumulative-depth;bid-linear-coefficient;first-3-displayed-levels` | base quantity per tick, signed finite | Book; authenticated exact tick and first three displayed bid levels | Book |
| <a id="ask-book-slope"></a>`ask_book_slope` | `quadratic-cumulative-depth;ask-linear-coefficient;first-3-displayed-levels` | base quantity per tick, signed finite | Book; authenticated exact tick and first three displayed ask levels | Book |
| <a id="bid-book-convexity"></a>`bid_book_convexity` | `quadratic-cumulative-depth;bid-second-derivative;first-3-displayed-levels` | base quantity per tick squared, signed finite | Book; authenticated exact tick and first three displayed bid levels | Book |
| <a id="ask-book-convexity"></a>`ask_book_convexity` | `quadratic-cumulative-depth;ask-second-derivative;first-3-displayed-levels` | base quantity per tick squared, signed finite | Book; authenticated exact tick and first three displayed ask levels | Book |
| <a id="bid-liquidity-wall-distance"></a>`bid_liquidity_wall_distance` | `nearest-bid-level-with-quote-notional-at-least-threshold;distance-bps` | bps, `>= 0` | Book; point-in-time quote-notional threshold | Book |
| <a id="ask-liquidity-wall-distance"></a>`ask_liquidity_wall_distance` | `nearest-ask-level-with-quote-notional-at-least-threshold;distance-bps` | bps, `>= 0` | Book; point-in-time quote-notional threshold | Book |
| <a id="bid-price-level-gap-density"></a>`bid_price_level_gap_density` | `missing-ticks/inside-to-outer-tick-span;bid` | dimensionless, `[0, 1]` | Book; authenticated exact tick and all displayed bid levels; at least two levels | Book |
| <a id="ask-price-level-gap-density"></a>`ask_price_level_gap_density` | `missing-ticks/inside-to-outer-tick-span;ask` | dimensionless, `[0, 1]` | Book; authenticated exact tick and all displayed ask levels; at least two levels | Book |
| <a id="expected-buy-sweep-cost-usd-10000"></a>`expected_buy_sweep_cost_usd_10000` | `consume-asks-to-10000-usd-using-point-in-time-quote-usd-conversion;relative-vwap-cost` | dimensionless, `>= 0` | Book; USD 10,000; point-in-time FX | USD |
| <a id="expected-sell-sweep-cost-usd-10000"></a>`expected_sell_sweep_cost_usd_10000` | `consume-bids-to-10000-usd-using-point-in-time-quote-usd-conversion;relative-vwap-cost` | dimensionless, `>= 0` | Book; USD 10,000; point-in-time FX | USD |
| <a id="maximum-executable-buy-quote-notional"></a>`maximum_executable_buy_quote_notional` | `sum-ask-notional-within-inclusive-configured-slippage` | quote notional, `>= 0` | Book; versioned inclusive slippage bound | Book |
| <a id="maximum-executable-sell-quote-notional"></a>`maximum_executable_sell_quote_notional` | `sum-bid-notional-within-inclusive-configured-slippage` | quote notional, `>= 0` | Book; versioned inclusive slippage bound | Book |
| <a id="book-quote-age-ms"></a>`book_quote_age_ms` | `evaluation-time-minus-book-last-update-time;milliseconds` | integer milliseconds, `>= 0` | Book; monotonic evaluation time | Book |
| <a id="bid-stale-side-duration-ns"></a>`bid_stale_side_duration_ns` | `evaluation-time-minus-last-observed-bid-change` | integer nanoseconds, `>= 0` | Flow; observed bid-side history | Flow |
| <a id="ask-stale-side-duration-ns"></a>`ask_stale_side_duration_ns` | `evaluation-time-minus-last-observed-ask-change` | integer nanoseconds, `>= 0` | Flow; observed ask-side history | Flow |
| <a id="bid-active-price-levels"></a>`bid_active_price_levels` | `count-positive-displayed-bid-levels` | integer levels, `>= 0` | Book; bounded L2 state | Book |
| <a id="ask-active-price-levels"></a>`ask_active_price_levels` | `count-positive-displayed-ask-levels` | integer levels, `>= 0` | Book; bounded L2 state | Book |
| <a id="local-book-entropy"></a>`local_book_entropy` | `normalized-shannon-entropy-over-displayed-quantity` | dimensionless, `[0, 1]` | Book; both sides, positive displayed quantity | Book |
| <a id="depth-concentration-by-level"></a>`depth_concentration_by_level` | `displayed-quantity-herfindahl-index` | dimensionless, `(0, 1]` | Book; both sides, positive displayed quantity | Book |
| <a id="bid-depth-change"></a>`bid_depth_change` | `current-minus-prior-exact-bid-depth;same-policy-session-band` | signed base quantity | Flow; same session, policy, and band | Flow |
| <a id="ask-depth-change"></a>`ask_depth_change` | `current-minus-prior-exact-ask-depth;same-policy-session-band` | signed base quantity | Flow; same session, policy, and band | Flow |
| <a id="absolute-spread-change"></a>`absolute_spread_change` | `current-minus-prior-exact-absolute-spread;same-policy-session` | signed quote price | Flow; same session and policy | Flow |
| <a id="displayed-depth-replenishment-fraction"></a>`displayed_depth_replenishment_fraction` | `(later-depth-post-depth)/(pre-depth-post-depth);passive-side-proxy;point-in-time-recovery-policy-v1` | dimensionless, `>= 0` | Recovery; bounded 15 minutes and 4,096 observations | Flow |
| <a id="displayed-depth-recovery-time-ns"></a>`displayed_depth_recovery_time_ns` | `first-time-displayed-depth-recovers-100pct-within-15m;point-in-time-recovery-policy-v1` | integer nanoseconds, `[0, 15m]` | Recovery; full displayed-depth threshold | Flow |
| <a id="book-half-life-ns"></a>`book_half_life_ns` | `first-time-displayed-depth-recovers-50pct-within-15m;point-in-time-recovery-policy-v1` | integer nanoseconds, `[0, 15m]` | Recovery; half displayed-depth threshold | Flow |
| <a id="aggressive-buy-count"></a>`aggressive_buy_count` | `count-authoritative-buy-aggressor-trades` | integer trades, `>= 0` | Flow; authoritative side; max 100,000 trades | Flow |
| <a id="aggressive-sell-count"></a>`aggressive_sell_count` | `count-authoritative-sell-aggressor-trades` | integer trades, `>= 0` | Flow; authoritative side; max 100,000 trades | Flow |
| <a id="aggressive-buy-quantity"></a>`aggressive_buy_quantity` | `sum-authoritative-buy-base-quantity` | base quantity, `>= 0` | Flow; authoritative side | Flow |
| <a id="aggressive-sell-quantity"></a>`aggressive_sell_quantity` | `sum-authoritative-sell-base-quantity` | base quantity, `>= 0` | Flow; authoritative side | Flow |
| <a id="aggressive-buy-notional"></a>`aggressive_buy_notional` | `sum-authoritative-buy-quote-notional` | quote notional, `>= 0` | Flow; spot instrument contract | Flow |
| <a id="aggressive-sell-notional"></a>`aggressive_sell_notional` | `sum-authoritative-sell-quote-notional` | quote notional, `>= 0` | Flow; spot instrument contract | Flow |
| <a id="signed-trade-count-imbalance"></a>`signed_trade_count_imbalance` | `(buy-count-sell-count)/(buy-count+sell-count)` | dimensionless, `[-1, 1]` | Flow; nonempty authoritative trade count | Flow |
| <a id="signed-trade-quantity-imbalance"></a>`signed_trade_quantity_imbalance` | `(buy-quantity-sell-quantity)/(buy-quantity+sell-quantity)` | dimensionless, `[-1, 1]` | Flow; positive total quantity | Flow |
| <a id="signed-trade-notional-imbalance"></a>`signed_trade_notional_imbalance` | `(buy-notional-sell-notional)/(buy-notional+sell-notional)` | dimensionless, `[-1, 1]` | Flow; positive spot quote notional | Flow |
| <a id="top-of-book-ofi"></a>`top_of_book_ofi` | `cont-kukanov-stoikov-best-quote-ofi;buy-pressure-positive` | signed base quantity | Flow; ordered trusted top-of-book states | Flow |
| <a id="placement-rate-per-second"></a>`placement_rate_per_second` | `certified-l3-place-count/window-seconds` | events per second, `>= 0` | Flow; continuous certified L3 | L3 |
| <a id="cancellation-rate-per-second"></a>`cancellation_rate_per_second` | `certified-l3-cancel-count/window-seconds` | events per second, `>= 0` | Flow; continuous certified L3 | L3 |
| <a id="modification-rate-per-second"></a>`modification_rate_per_second` | `certified-l3-modify-count/window-seconds` | events per second, `>= 0` | Flow; continuous certified L3 | L3 |
| <a id="cancellation-to-trade-ratio"></a>`cancellation_to_trade_ratio` | `certified-l3-cancel-count/aggressive-trade-count` | dimensionless, `>= 0` | Flow; continuous certified L3 and nonzero trades | L3 |
| <a id="quote-to-trade-ratio"></a>`quote_to_trade_ratio` | `certified-l3-place-cancel-modify-count/aggressive-trade-count` | dimensionless, `>= 0` | Flow; continuous certified L3 and nonzero trades | L3 |
| <a id="trade-intensity-per-second"></a>`trade_intensity_per_second` | `authoritative-trade-count/window-seconds` | trades per second, `>= 0` | Flow; exact 60-second window | Flow |
| <a id="trade-interarrival-coefficient-of-variation"></a>`trade_interarrival_coefficient_of_variation` | `population-standard-deviation-interarrival/mean-interarrival` | dimensionless, `>= 0` | Flow; at least three ordered trades | Flow |
| <a id="signed-volume-at-price"></a>`signed_volume_at_price` | `sum(sign-aggressor*base-quantity)-by-exact-price` | exact price to signed base quantity map | Flow; bounded by trade count and 100,000 map entries | Flow |
| <a id="trade-shock-midprice-impact"></a>`trade_shock_midprice_impact` | `side-signed-mid-response-minus-pre-mid-over-pre-mid;point-in-time-threshold-policy-v1` | signed dimensionless finite | Flow; versioned shock threshold and response horizon | Flow |
| <a id="trade-effective-spread"></a>`trade_effective_spread` | `quantity-weighted-2*side*(trade-price-pre-mid)/pre-mid` | signed dimensionless finite | Flow; trusted pre-trade midpoint | Flow |
| <a id="trade-realized-spread"></a>`trade_realized_spread` | `quantity-weighted-2*side*(trade-price-response-mid)/pre-mid` | signed dimensionless finite | Flow; trusted response midpoint and horizon | Flow |
| <a id="trade-adverse-selection-proxy"></a>`trade_adverse_selection_proxy` | `effective-spread-minus-realized-spread` | signed dimensionless finite | Flow; same shock episode and policy | Flow |
| <a id="trade-print-sweep-direction"></a>`trade_print_sweep_direction` | `(qualifying-buy-episodes-qualifying-sell-episodes)/all-qualifying-episodes` | dimensionless, `[-1, 1]` | Flow; versioned max inter-trade gap and equal-price policy | Flow |
| <a id="large-trade-cluster-count"></a>`large_trade_cluster_count` | `count-time-clusters-with-at-least-two-fixed-threshold-large-trades` | integer clusters, `>= 0` | Flow; quote-notional threshold and max gap | Flow |
| <a id="clustered-large-trade-count"></a>`clustered_large_trade_count` | `count-prints-in-qualifying-large-trade-clusters` | integer trades, `>= 0` | Flow; quote-notional threshold and max gap | Flow |
| <a id="clustered-large-trade-notional"></a>`clustered_large_trade_notional` | `sum-exact-quote-notional-in-qualifying-large-trade-clusters` | quote notional, `>= 0` | Flow; spot instrument contract, threshold, and max gap | Flow |
| <a id="large-trade-cluster-activity-rate"></a>`large_trade_cluster_activity_rate` | `qualifying-large-trade-cluster-count/window-seconds` | clusters per second, `>= 0` | Flow; exact 60-second window, threshold, and max gap | Flow |

These anchors now target the feature's own formula, units, range, parameters,
and absence policy. A semantic or configuration change requires a new feature
version and formula hash; it must not reinterpret an existing ID/version pair.

### Experimental L3-only candidates not registered in version 1

The specification's queue-position, order-age, cancel-replace-chain,
maker-fill, and hidden-liquidity candidates remain deliberately absent from
the 77-feature registry. They require exact licensed L3 event semantics,
stable source identifiers, and separately reviewed formulas. L2 or trade-print
proxies must not be registered under those identities; attempts to request
them are `source_not_supported`.
