# ADR 0003: Strict Offline and no-execution V1

- Status: Accepted
- Date: 2026-07-24

## Context

Version 1 is a local research and monitoring product. Forecasts and risk
signals can look actionable, so an ambiguous boundary around accounts, wallets,
orders, or remote services would create unacceptable financial and security
risk. Future read-only market connectors do not justify execution authority.

## Decision

Strict Offline is the default runtime posture: imported or explicitly approved
read-only inputs are processed locally, results stay local, and there is no
hosted inference or remote telemetry fallback. Version 1 must not place orders,
must not amend or cancel orders, must not open or close positions, must not set
leverage, must not withdraw assets, must not sign transactions, and must not
mutate an exchange or wallet account. Public market-data connectors, when
separately approved, are read-only and purpose-scoped. The local API exposes
observation and health, never execution.

## Alternatives considered

- Disabled or hidden trading code was rejected because dormant authority can
  still be activated by configuration, dependency features, or UI drift.
- Paper-trading endpoints were rejected from the foundation because they reuse
  execution-shaped contracts and can blur environment boundaries.
- A generic credential vault was rejected because Version 1 needs no trading,
  withdrawal, seed, or signing credential.

## Consequences

Research output is advisory and cannot be promoted directly into a trade.
Configuration, dependencies, RPCs, CLI options, generated clients, and runtime
source are scanned for execution capability. Read-only network ingestion
remains a later, separately reviewed capability and is not implied by this ADR.

## Security and reliability

The boundary is enforced in code and tests rather than by UI copy alone.
Unexpected network clients, process commands, external endpoints, account
mutation terms, or generated client surfaces fail closed. Secrets and financial
identifiers must not enter logs. Abstention, stale-data signaling, and data
quality remain visible because no downstream action can be assumed safe.

## Migration and reversal

Any execution, wallet-signing, account-mutation, paper-order, or remote
inference capability requires a new versioned product boundary, a new accepted
ADR, explicit user authorization, segregated credentials and environments,
risk limits, monitoring, rollback, and independent review. It cannot be enabled
by a feature flag or configuration-only change to Version 1.
