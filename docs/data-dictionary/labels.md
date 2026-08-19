# Market-transition labels

This document is the checked semantic contract for `crates/labels`. Labels are
pure, deterministic evaluations over a bounded, uniformly sampled,
point-in-time path. They do not fetch data, repair data, or silently interpret
missing observations.

## Definition identity

Every `LabelDefinition` contains:

- a lowercase identifier and stable semantic version;
- a `production_v1` or `research` scope;
- the complete event rule and every numeric threshold;
- one or more horizons;
- a persistence duration;
- minimum quality and source-coverage requirements;
- an overlap policy; and
- a domain-separated BLAKE3 hash of all semantic fields.

Construction sorts horizons before hashing and rejects duplicate, zero, or
unbounded horizons. A definition is immutable after construction. Changing any
semantic field produces a different hash.

`production_v1` definitions have exactly these horizons:

| Horizon | Seconds |
| --- | ---: |
| 15 minutes | 900 |
| 1 hour | 3,600 |
| 4 hours | 14,400 |
| 24 hours | 86,400 |

Research definitions may use other positive, bounded horizons without changing
the production definition. The engine supports the v1 examples of 2%, 3%, 5%,
and 10% absolute thresholds and 1.5, 2, and 3 volatility multiples without
making those examples an undocumented exhaustive allowlist. Production
directional definitions should normally select the combined threshold form.
Production liquidation-cascade definitions should normally select a
volatility-scaled price threshold.

## Point-in-time path

Each market frame records an event offset and the offset at which the frame
became known. Knowledge cannot predate the event, regress across the path, or
arrive after the requested decision horizon. Frames also carry:

- consolidated reference price;
- realized volatility;
- spread, executable displayed depth, cancellation rate, sweep cost, and
  resiliency;
- liquidation notional, open interest, and liquidation-feed confidence;
- healthy-source corroboration count;
- quality and source coverage in millionths;
- source health, finalization state, and market eligibility.

Paths start at offset zero, contain at least two and at most 4,096 frames, and
use one fixed positive cadence. NaN, infinity, invalid signs, confidence or
coverage above one million, and more than 64 corroborating sources are rejected.

## Outcome states

`Occurred { offset_seconds }` records the first start of a matching run. If a
persistence duration is configured, the occurrence is not known until a later
frame confirms that duration; `LabelEvaluation.outcome_known_at_offset_seconds`
records that knowledge time.

`NotOccurred` is returned only when trustworthy observations cover the full
horizon. Loss of finalized data, health, quality, coverage, required evidence,
or point-in-time availability before the horizon returns
`Censored { observed_seconds }`.

`Excluded(reason)` is distinct from both states. Stable reasons include an
unhealthy or low-quality origin, missing origin evidence, an invalid volatility
scale, insufficient corroboration or liquidation confidence, a halt or
delisting, an instrument-definition change, an unresolved correction,
compromised timestamp integrity, or a disallowed simultaneous event.

Every `LabelEvaluation` carries its horizon, outcome knowledge time, confidence,
source coverage, and definition hash. A liquidation cascade uses the feed
confidence at the event start. Other label families use the lower of quality
and source coverage as their conservative confidence.

## Event rules

### Downside and upside transition

Directional first passage uses the consolidated price and log return:

`log(price_at_offset / origin_price) <= -threshold`

for downside and the analogous positive comparison for upside. An absolute
threshold is a log-return magnitude. A volatility-scaled threshold is the
origin volatility multiplied by the definition multiple; later volatility
cannot move the threshold. A combined threshold uses the larger of the absolute
floor and origin-scaled threshold.

Generic directional labels do not require liquidation or other mechanism
evidence.

### Volatility explosion

A volatility definition declares:

- estimator;
- sampling resolution;
- immutable reference-distribution hash;
- percentile in millionths;
- the point-in-time reference threshold;
- threshold multiple;
- persistence duration; and
- whether jumps are included or separated.

The forward realized-volatility field must remain at or above
`reference_threshold * minimum_multiple` for the declared persistence duration.
The origin must contain volatility evidence, and later missing evidence censors
the outcome.

### Systemic liquidity vacuum

A systemic liquidity vacuum jointly requires all of the following relative to
the point-in-time origin:

- spread widening;
- executable depth loss;
- cancellation or quote-withdrawal increase;
- sweep-cost increase;
- slower replenishment or resiliency; and
- at least the declared number of healthy corroborating sources.

The production-compatible systemic rule requires at least two sources. A
spread-only or single-venue disturbance does not satisfy this rule. Venue-local
labels require a separate definition and are not silently promoted to systemic.

### Liquidation cascade

A liquidation cascade jointly requires:

- a versioned, normally volatility-scaled price movement;
- liquidation notional at or above the declared minimum;
- open-interest destruction;
- spread widening and depth loss;
- cross-source corroboration; and
- liquidation-feed confidence at or above the declared minimum.

The confidence field is mandatory evidence. A partial single-source feed cannot
produce a high-confidence complete-market cascade.

## Competing risks

Raw event-family outcomes remain available. A derived training view may:

- keep all competing outcomes;
- choose the earliest outcome; or
- exclude simultaneous ties.

For simultaneous events represented by the v1 crate, the documented cause
priority is:

1. liquidity vacuum;
2. liquidation cascade;
3. generic downside or upside transition;
4. volatility explosion.

Stablecoin and venue dislocations precede this list in the product taxonomy but
are not part of this crate's current closed event family. Priority selects a
modeled cause; it does not erase the raw co-occurring labels.

## Non-goals and invariants

- No future fill, silent zero imputation, random split, or full-history
  normalization is performed.
- No source repair or correction is performed inside the evaluator.
- A missing or unhealthy interval is never converted to `NotOccurred`.
- A changed definition is a new hash and must not overwrite a sealed one.
