# Crypto Intelligence

A local-first Rust and Swift system for estimating the probability, direction,
mechanism, horizon, and severity of cryptocurrency market transitions.

The workspace combines multi-venue market data, deterministic replay,
point-in-time features, stochastic and competing-risk models, a native macOS
research client, and an isolated local model host.

> Research, monitoring, replay, scenario analysis, and alerting software. It
> does not accept trading or withdrawal credentials, place orders, mutate
> exchange accounts, or use hosted inference.

Maintained by [RSI Tech](https://rsitech.ai). Source is dual-licensed
[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE). Datasets, model weights,
and generated artifacts need their own provenance.

## Current status

The repository has a tested foundation runtime: canonical identities and event
envelopes, layered configuration, local observability, an authenticated
loopback API, crash-safe WAL persistence and replay, a fixture-backed daemon,
and a native macOS foundation shell.

Live venue connectors, full analytical and product flows, signing,
notarization, and release qualification are out of scope for the validated
foundation slice. This is not a complete research product or a release
candidate.

## Architecture

```text
public/read-only exchanges, nodes, and event sources
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

Rust owns numerical state, probabilities, evidence, and persistence. Swift
owns presentation. Local language models may draft evidence-grounded text;
they cannot create or alter probabilities.

## Verify

```bash
git clone https://github.com/rsitech-ai/crypto-intelligence.git
cd crypto-intelligence
cargo +1.88.0 check --workspace --all-targets --all-features --locked
cargo +1.88.0 clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.88.0 test --workspace --all-targets --all-features --locked
swift test --package-path apps/macos/Packages/TransitionClient
scripts/verify-foundation-runtime-twice.py
```

The two-run verifier starts from a clean commit, runs repository gates, builds
and exercises the packaged macOS app and daemon, and writes
`release/evidence/foundation-runtime-verification.json`. A blocked evidence
record is not release readiness.

## Policy

[Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) ·
[Code of Conduct](CODE_OF_CONDUCT.md) · [Governance](GOVERNANCE.md)

Contact: [info@rsitech.ai](mailto:info@rsitech.ai)
