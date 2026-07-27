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

The source tree implements every Phase 00–08 path in the approved implementation
plans. The portable Swift package is verified under Swift 6 complete concurrency
checking with warnings denied. Rust, Buf, Xcode, Core AI, signing, notarization,
long-running soak/recovery, connector certification, and sufficient live-shadow
model evidence remain fail-closed release gates until run on their required
platforms.

See:

- `crypto-market-transition-intelligence-production-spec-v1.0-approved.md`
- `docs/superpowers/plans/2026-07-24-transition-intelligence-master-plan.md`
- `docs/implementation/IMPLEMENTATION-GOAL.md`
- `docs/implementation/FINAL-AUDIT.md`
- `SECURITY.md`

## Available verification

```bash
python3 scripts/static-audit.py
python3 scripts/security-audit.py
swift test --package-path apps/macos \
  -Xswiftc -warnings-as-errors \
  -Xswiftc -strict-concurrency=complete
```

On a pinned Rust/macOS release machine, also run the commands documented in
`docs/implementation/VERIFICATION-DEBT.md`.

## License

Source code is available under MIT or Apache-2.0 at your option. Datasets,
address labels, model weights, news corpora, and generated artifacts require
their own provenance and license manifests.
