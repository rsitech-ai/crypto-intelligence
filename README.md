# Crypto Intelligence

A local-first Rust and native Swift system for estimating the probability,
direction, mechanism, horizon, and severity of cryptocurrency market
transitions.

The project combines trusted multi-venue market data, deterministic replay,
point-in-time features and event labels, stochastic and coupled cusp
catastrophe models, calibrated competing-risk forecasts, options and on-chain
evidence, explicit uncertainty and abstention, a native macOS workstation, and
an isolated local model host.

> Version 1 is research, monitoring, replay, scenario analysis, and alerting
> software. It does not accept trading or withdrawal credentials, place orders,
> mutate exchange accounts, or use hosted inference.

## Architecture

```text
public/read-only exchanges, nodes, macro and event sources
                         │
                  Rust collectors
                         │
             append-only WAL and replay
                         │
       exact books and point-in-time features
                         │
  cusp/regime/changepoint/competing-risk ensemble
                         │
        calibration, scenarios, evidence, alerts
                         │
       authenticated loopback gRPC contracts
                         │
        native SwiftUI macOS research client
```

Rust owns authoritative numerical state, probabilities, evidence, persistence,
and audit. Swift owns presentation and Apple-platform integration. Local language
models may extract structured events and draft evidence-grounded text; they
cannot create or alter probabilities.

## Repository status

The repository now has a tested foundation runtime: canonical identities and
event envelopes, layered configuration, local observability, an authenticated
loopback API, crash-safe segmented WAL persistence and replay, a fixture-backed
daemon, the native macOS foundation shell, and fail-closed connector runtime
contracts.

This is still an incremental implementation of the approved Phase 00–08
specification, not a complete research product or release candidate. The
current fail-closed verification evidence records 357 later-phase scaffold
findings. Fresh XCUITest is also blocked on macOS automation authorization, and
an upstream SwiftNIO CNIOWindows warning remains a release gate. Live venue
connectors, analytical/model layers, full product flows, signing, notarization,
and release qualification remain out of scope for the validated foundation
slice.

See:

- `crypto-market-transition-intelligence-production-spec-v1.0-approved.md`
- `docs/superpowers/plans/2026-07-24-transition-intelligence-master-plan.md`
- `docs/implementation/IMPLEMENTATION-GOAL.md`
- `docs/implementation/FINAL-AUDIT.md`
- `docs/audits/2026-07-27-full-system-audit.md`
- `SECURITY.md`

## Available verification

```bash
cargo +1.88.0 check --workspace --all-targets --all-features --locked
cargo +1.88.0 clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.88.0 test --workspace --all-targets --all-features --locked
swift test --package-path apps/macos/Packages/TransitionClient
scripts/verify-foundation-runtime-twice.py
```

The two-run verifier starts from a clean commit, executes the repository gates,
builds and exercises the packaged macOS app and daemon, inspects logs and
process cleanup, and writes the exact result to
`release/evidence/foundation-runtime-verification.json`. A blocked evidence
record must not be represented as release readiness.

## License

Source code is available under MIT or Apache-2.0 at your option. Datasets,
address labels, model weights, news corpora, and generated artifacts require
their own provenance and license manifests.
