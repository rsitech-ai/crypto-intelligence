# ADR 0001: Local modular monolith

- Status: Accepted
- Date: 2026-07-24

## Context

Crypto Intelligence must preserve deterministic replay, bounded resources,
point-in-time correctness, and a local trust boundary while its domain model is
still evolving. Splitting capture, normalization, books, features, models,
storage, replay, and audit across independently deployed services would add
network and operational failure modes before measured isolation needs exist.

## Decision

The product is a modular monolith. A single Rust daemon owns the local data
plane. Module APIs and durable formats remain explicit, but module boundaries
do not imply deployment boundaries. The native application is a separate
process and communicates with the daemon through the local API described by ADR
0002. A new process boundary requires measured isolation, security, or
throughput evidence.

## Alternatives considered

- Independent local microservices were rejected because they add discovery,
  version skew, partial availability, and shutdown ordering without evidence
  that those costs solve a current constraint.
- A Swift-only application was rejected because deterministic ingestion,
  replay, and durable-state ownership belong in the Rust data plane.
- A hosted backend was rejected because it would break the local-only product
  boundary and create remote data custody.

## Consequences

Installation, replay, recovery, and versioning stay comparatively simple.
Modules must still expose narrow interfaces and explicit state so a component
can be isolated later without redesigning its data contract. A daemon failure
has a wider local blast radius, so bounded queues, durable writes, health
reporting, and controlled shutdown remain required.

## Security and reliability

The single Rust daemon is the only owner of production ingestion, storage, and
local RPC capabilities. The Swift process does not gain direct exchange,
credential, or storage authority. Consolidation reduces local network surface,
but it also makes resource caps, authenticated loopback RPC, recovery testing,
and least-authority module design mandatory.

## Migration and reversal

This decision may be reversed only by a new accepted ADR that identifies the
component, process boundary, protocol/versioning plan, secret and data
ownership, shutdown behavior, and measured benefit. Existing module and storage
contracts must remain migratable during a staged split; an ad hoc second daemon
is not an accepted migration.
