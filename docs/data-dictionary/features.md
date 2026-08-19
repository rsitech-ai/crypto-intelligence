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
| `windows` | Bounded, uniquely named window declarations with typed time, UTC-anchored session, EWMA half-life, event-count, volume, or notional parameters. |
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
| `watermark` | Positive event-time watermark, not after `computed_at`. |
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
watermark <= computed_at
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
