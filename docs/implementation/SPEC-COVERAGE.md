# Approved Specification Coverage

Every top-level section of the approved specification is assigned to one or more executable plans. Detailed task-level mappings are in `traceability.md`.

| Section | Subject | Owning plans | Coverage |
|---:|---|---|---|
| 1 | Document purpose and normative language | Master, P0 | Normative baseline, task discipline, approval and traceability |
| 2 | Executive decision | Master, P0 | Architecture, local-first, probabilistic rather than deterministic claims |
| 3 | Product principles | Master, P0, P4, P8 | Integrity, uncertainty, abstention, persistence and release controls |
| 4 | Goals, non-goals, and users | Master, P6, P8 | Research/monitoring workflows and explicit no-execution surface |
| 5 | Release scope | Master, P1–P8 | Assets, venues, horizons, event types, platform and release cut |
| 6 | Forecast contract | P0, P2, P4 | Typed competing risks, cumulative incidence, uncertainty and horizons |
| 7 | System context and architecture | Master, P0, P1, P6 | Rust authority, Swift client, local storage and process boundaries |
| 8 | Local RPC and process security | P0, P6, P7, P8 | Authenticated loopback RPC, helper supervision and compatibility |
| 9 | Data-source strategy | P1, P5 | Native venue connectors, options, blockchain and external sources |
| 10 | Canonical identifiers and numeric types | P0, P1 | Fixed point, instrument generations and exact contracts |
| 11 | Canonical event envelope | P0, P1 | Immutable envelope, timestamps, sequence, quality and provenance |
| 12 | Order-book engine | P1, P8 | Snapshot/delta state machines, checksums, recovery and property tests |
| 13 | Consolidated market state | P1, P2, P4 | Cross-venue state, fast-state snapshots and quality-aware aggregation |
| 14 | Local blockchain data | P5, P8 | Bitcoin/Ethereum nodes, finality, lag and resource isolation |
| 15 | Event and language-model ingestion | P5, P7, P8 | Structured events, source evidence, constrained local extraction |
| 16 | Storage architecture | P1, P2, P8 | WAL, Parquet/Arrow/DataFusion, SQLite metadata and disaster recovery |
| 17 | Coverage tiers and capacity control | P1, P8 | Tier A/B/C retention and pre-admission capacity planner |
| 18 | Point-in-time feature engine | P2, P5, P8 | As-known-at lineage, stream/batch parity and revision controls |
| 19 | Production feature catalogue | P2, P4, P5 | Price, volatility, book, derivatives, options, chain, macro and quality |
| 20 | Event labels and outcome taxonomy | P2, P4, P5, P8 | Competing events, censoring, stablecoin/contagion and shadow outcomes |
| 21 | Statistical and machine-learning architecture | P2, P3, P4, P5, P7 | Baselines, cusp, regimes, hazards, stack, calibration and optional local neural models |
| 22 | Training, research, and model artifacts | P2, P3, P4, P5, P7, P8 | Reproducible datasets, signed packages, registry and monitoring |
| 23 | Validation and scientific release gates | P2, P3, P4, P5, P8 | Purged walk-forward evaluation, baselines, event counts, shadow and independent approval |
| 24 | Data-quality and applicability engine | P1, P2, P4, P8 | Health states, freshness, reconciliation, fallback and incidents |
| 25 | Forecast, evidence, and alert contracts | P0, P4, P6, P7, P8 | Immutable forecast/evidence records, alert lifecycle and budgets |
| 26 | Native macOS product specification | P6, P8 | All views, daemon lifecycle, replay, accessibility and packaging |
| 27 | Local language-model and Apple model-host specification | P7, P8 | Core ML/Core AI/Foundation Models with signed artifacts and deterministic fallback |
| 28 | Observability, performance, and capacity | P0, P4, P6, P8 | Local metrics/logs, reference SLOs/load, capacity and soak evidence |
| 29 | Reliability, recovery, and failure handling | P1, P4, P6, P7, P8 | Backpressure, WAL/checkpoint recovery, runtime state machines and backups |
| 30 | Security and privacy specification | P0, P6, P7, P8 | Threat model, parser/RPC/file/audit/outbound controls and notarization |
| 31 | Verification and test strategy | P0–P8 | Unit, property, fuzz, numerical, replay, concurrency, chaos, model, UI, upgrade and release tests |
| 32 | Dependency and framework policy | Master, P0–P8 | Pinned Rust/Swift/Apple/data dependencies and numerical boundaries |
| 33 | Configuration specification | P0, P6, P8 | Typed layered config, validation, restart classes and secure settings |
| 34 | API and schema specification | P0, P3, P4, P6, P7, P8 | Protobuf services, compatibility, stream behavior and stable errors |
| 35 | Build, CI, release, and open-source governance | P0, P8 | CI stages, ADRs, dual licensing, DCO, channels, updates and provenance |
| 36 | Implementation roadmap and exit gates | Master, P0–P8 | Ordered evidence-gated plans and stable-release exit condition |
| 37 | Production acceptance criteria | All plans, especially P8 | Machine-linked data, model, reliability, security, UX and release evidence |
| 38 | Risk register | Master, P1–P8 | Mitigations and release-blocking policies embedded in task gates |
| 39 | Architecture decision summary | Master, P0 | Approved decisions frozen in ADR/contract tasks |
| 40 | Recommended v1 product cut | Master, P1–P8 | Narrow BTC/ETH/SOL/stablecoin, four-venue, four-horizon, no-execution cut |

## Appendix coverage

| Appendix | Owning plans |
|---|---|
| A — Mathematical reference | P3, P4 |
| B — Source-completeness profiles | P1, P8 |
| C — Model-card template | P2, P3, P4, P5, P7, P8 |
| D — Operational runbooks | P8 |
| E — Validation report | P2, P3, P4, P5, P8 |
| F — Source/documentation references | P0, P1, P5, P7, P8 |
| G — Specification self-review | Planning self-review and P8 acceptance manifest |
