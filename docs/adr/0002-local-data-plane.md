# ADR 0002: Rust data plane and native presentation boundary

- Status: Accepted
- Date: 2026-07-24

## Context

The product needs a native macOS experience without duplicating ingestion,
storage, replay, or risk logic in the UI process. Loopback alone is not an
authentication boundary, and native lifecycle events must not silently create
additional network or execution authority.

## Decision

The Rust data plane owns validated inputs, durable state, computation, and the
versioned local API. The Swift presentation layer owns windows, user
interaction, accessibility, and daemon supervision. They communicate only
through authenticated loopback RPC with bounded messages and explicit health
and session semantics. The Swift client does not connect directly to exchanges,
hosted inference, or remote telemetry, and it never receives trading or wallet
credentials.

## Alternatives considered

- Direct Swift access to venues and local storage was rejected because it would
  duplicate authority and produce two sources of runtime truth.
- In-process Rust FFI was rejected for the foundation because memory and crash
  ownership would be harder to isolate than a versioned local protocol.
- A hosted API was rejected because it would move research data and service
  availability outside the local machine.

## Consequences

The UI can restart independently while the daemon retains durable ownership.
The protocol and authentication handshake become compatibility contracts.
Native features must degrade honestly when the daemon is absent or unhealthy;
they may not fall back to a remote service.

## Security and reliability

Binding to loopback is necessary but insufficient: sessions are authenticated,
secrets are inherited rather than exposed in arguments or files, messages and
concurrency are bounded, and logs are redacted. The daemon must reject
non-loopback binding and stale or unauthenticated sessions. No remote telemetry
or hidden remote execution path may be introduced by either process.

## Migration and reversal

Changing transport, embedding the daemon, or adding another client requires a
new accepted ADR with authentication, compatibility, resource, shutdown, and
rollback evidence. During migration, the current authenticated loopback RPC
remains the authoritative boundary until the replacement passes equivalent
tests.
