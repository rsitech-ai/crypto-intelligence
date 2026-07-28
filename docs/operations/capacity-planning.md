# Local capacity planning and admission

The capacity planner produces a deterministic local preview before coverage is
enabled. It has no network probe, remote benchmark, execution authority, or
automatic configuration mutation.

## Readiness labels

Capacity results have four distinct meanings:

- `arithmetic-valid`: versioned input passed bounds and every calculation
  completed with checked integer arithmetic;
- `evidence-required`: the declared workload fits after conservative
  uncertainty margins, but no repository-owned certification proves it;
- `startup-admitted`: the implemented foundation fixture fits the smaller of
  detected physical memory and the configured process budget, plus
  descriptor-measured free disk and conservative throughput defaults;
- `reference-certified`: a reviewed machine/load profile has passed sustained,
  burst, write, replay, correctness, provenance, and release gates.

This repository does not contain a reference certification record or registry.
Consequently schema version 1 cannot emit `accepted`. Input supports only
`conservative_default` and `measured`; the string `certified` is invalid JSON
for this contract. Measured values remain evidence-required until a future
repository-owned verifier binds them to durable certification evidence.

## Versioned input and evidence

`CapacityInput` schema version 1 contains:

- `HardwareProfile`: architecture, logical CPU count, memory and free-disk
  bytes, sustained/burst inbound bytes/s, sustained/burst disk-write bytes/s,
  sustained/burst normalized event capacity, and evidence metadata;
- `CapacityEvidence`: conservative or measured provenance, a downward capacity
  uncertainty margin, sample count, observation time, and the exact 32-byte
  BLAKE3 digest of the measurement artifact. Conservative defaults require
  zero samples/time/digest and at least a 25% rate-capacity discount. Measured
  evidence requires all three provenance fields;
- `WorkloadProfile`: Tier A/B/C instruments and separate retention, nominal
  sustained/burst event rates, an upward demand uncertainty multiplier,
  raw/normalized bytes per event, write amplification, feature CPU,
  book/feature/model memory, ingestion-queue/RPC/runtime reservations, and
  concurrent replay demand;
- `CapacityPolicy`: memory/disk/CPU reserves plus separate sustained/burst
  event, inbound-bandwidth, and write headroom requirements.

All byte, event, nanosecond, second, day, and parts-per-million values are
integers. Unknown JSON fields, inconsistent optional-tier retention, fabricated
certification, invalid evidence provenance, invalid versions, invalid
reserve/headroom ranges, and arithmetic overflow fail closed.

## Checked formulas

`ceil_ppm(value, multiplier)` rounds demand upward. `budget(value, reserve)`
rounds capacity downward. `ratio(capacity, demand)` rounds headroom downward.

```text
sustained_eps = ceil_ppm(nominal_sustained_eps, demand_uncertainty_ppm)
burst_eps     = ceil_ppm(nominal_burst_eps, demand_uncertainty_ppm)

inbound_Bps       = sustained_eps * raw_B_per_event
burst_inbound_Bps = burst_eps * raw_B_per_event
normalized_Bps    = sustained_eps * normalized_B_per_event
raw_B_per_day     = inbound_Bps * 86_400
normalized_B_day  = normalized_Bps * 86_400

wal_write_Bps      = ceil_ppm(inbound_Bps, wal_amplification_ppm)
parquet_write_Bps  = ceil_ppm(normalized_Bps, parquet_amplification_ppm)
total_write_Bps    = wal_write_Bps + parquet_write_Bps
burst_write_Bps    = burst WAL + burst Parquet
retained_B_day     = total_write_Bps * 86_400

feature_cores_ppm = ceil(feature_cpu_ns_per_event * sustained_eps
                         * 1_000_000 / 1_000_000_000)
replay_cores_ppm  = ceil(replay_eps * feature_cpu_ns_per_event
                         * replay_multiplier_ppm
                         / 1_000_000_000)
replay_duration_s = ceil(replay_event_count / replay_eps)
replay_read_B     = replay_event_count * (raw_B_per_event + normalized_B_per_event)
replay_core_s     = ceil(total_replay_cpu_ns / 1_000_000_000)

book_memory_B = tier_A_books + tier_B_books + tier_C_books
peak_memory_B = books + features + models + ingestion_queue_reservation
                + RPC_inflight_reservation + runtime_reservation

effective_rate_capacity = floor(declared_capacity
                                * (1_000_000 - capacity_uncertainty_ppm)
                                / 1_000_000)
usable_budget  = floor(total * (1_000_000 - reserve_ppm) / 1_000_000)
headroom_ppm   = effective_rate_capacity * 1_000_000 / workload_demand
retention_days = usable_disk_B / retained_B_day
```

Memory and current free-disk observations are direct budgets; the uncertainty
discount is applied to benchmark-derived CPU, network, event, and disk-write
rates. Workload uncertainty is applied before bytes/day, CPU, network, and
write estimates.

## Decisions and quality impact

A fitting declared profile returns `evidence_required`, never `accepted`.
Resource violations return `rejected` with deterministic sorted reasons for
architecture, memory, retention, CPU, sustained/burst event capacity,
sustained/burst inbound bandwidth, and sustained/burst write capacity.

A downgrade is only a proposal:

- Tier A instrument count and Tier A retention are copied unchanged;
- optional Tier B/C coverage and their retention may be removed;
- a typed `quality_impact` records every preserved Tier A value and every
  removed optional instrument/retention value;
- `automatically_applied` is always `false`;
- if Tier A still fails after optional B/C removal, no downgrade is offered.

The planner never mutates configuration and never trades required integrity for
silent event loss.

## Preview CLI

The CLI reads one owner-resolved regular JSON file without following a final
symlink and rejects inputs larger than 1 MiB:

```sh
cargo +1.88.0 run -p crypto-evaluate -- \
  capacity --input /absolute/path/capacity-input.json --format json-pretty
```

Exit codes are:

- `3`: estimate fits declared resources but verified certification is absent;
- `4`: resource/headroom rejection;
- `1`: unsafe input, invalid JSON/evidence, invalid values, overflow, or I/O
  failure.

Schema version 1 intentionally has no exit-zero accepted path. Stdout contains
only decision JSON. Failure diagnostics do not echo input contents.

## Foundation daemon gate

During `prepare`, before Tokio runtime creation, session-secret reading,
WAL/log opening, RPC bind, fixture ingestion, or readiness, `cryptoriskd`:

- verifies the compiled target is Apple Silicon macOS;
- detects logical parallelism;
- reads physical memory with the read-only `sysctlbyname("hw.memsize")` system
  API and caps it by configured `max_memory_gib`;
- reads free bytes from the already validated data-directory descriptor using
  `fstatvfs`;
- reserves `queue_capacity * 1 MiB` for bounded ingestion messages,
  `maximum_request_bytes * maximum_concurrent_requests` for in-flight RPC
  input, and 2 MiB per configured Tokio worker;
- applies labeled conservative capacities of 10/50 MiB/s sustained/burst
  inbound, 50/100 MiB/s sustained/burst write, and 50,000/250,000
  sustained/burst normalized events/s;
- discounts those default rate capacities by 50% uncertainty and increases the
  modeled foundation workload by a 25% demand uncertainty multiplier;
- models the implemented fixture at one nominal event/s, 512 raw bytes and 256
  normalized bytes per event, with its checked book/feature memory constants.

Resource rejection aborts startup before external effects. A fitting result is
`startup-admitted`, not `reference-certified`. Enabling real external
connectors, larger coverage, concurrent replay, or model workloads requires
measured observations and a repository-verified representative Phase 8 load
gate.

## Verification

```sh
CARGO_INCREMENTAL=0 cargo test -p capacity --all-targets
CARGO_INCREMENTAL=0 cargo test -p crypto-evaluate --all-targets
CARGO_INCREMENTAL=0 cargo test -p cryptoriskd --all-targets
CARGO_INCREMENTAL=0 cargo test -p system-tests --test no_execution_surface
CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets -- -D warnings
CARGO_INCREMENTAL=0 cargo +1.88.0 check --workspace --all-targets
```

System probes follow the documented contracts for Apple
[`sysctlbyname`](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man3/sysctlbyname.3.html),
Rust
[`available_parallelism`](https://doc.rust-lang.org/std/thread/fn.available_parallelism.html),
and Rustix
[`fstatvfs`](https://docs.rs/rustix/latest/rustix/fs/fn.fstatvfs.html).
