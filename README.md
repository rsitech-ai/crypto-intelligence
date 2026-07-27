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

The `implementation/full-system-v3` candidate is a path-complete scaffold, not
a working Phase 00–08 implementation. A 2026-07-27 independent audit found an
invalid Cargo workspace and Xcode project, placeholder services and native
surfaces, tautological tests, non-executable CI workflows, and no-op release
scripts. No daemon, native app, end-to-end forecast flow, or release artifact is
currently runtime-proven.

The portable Swift package compiles a narrow contract/model-host subset under
Swift 6 strict concurrency. That result does not compile or test the native
SwiftUI sources and must not be represented as native product readiness.

See:

- `crypto-market-transition-intelligence-production-spec-v1.0-approved.md`
- `docs/superpowers/plans/2026-07-24-transition-intelligence-master-plan.md`
- `docs/implementation/IMPLEMENTATION-GOAL.md`
- `docs/implementation/FINAL-AUDIT.md`
- `docs/audits/2026-07-27-full-system-audit.md`
- `SECURITY.md`

## Available verification

```bash
python3 scripts/static-audit.py
python3 scripts/security-audit.py
swift test --package-path apps/macos \
  -Xswiftc -warnings-as-errors \
  -Xswiftc -strict-concurrency=complete
```

The hardened static audit is expected to fail until scaffold placeholders and
invalid project structures are replaced with behavioral implementations. The
security audit is only a secret-pattern and required-file-presence scan; it does
not verify runtime security.

Run the commands in `docs/implementation/VERIFICATION-DEBT.md` only after Cargo,
Buf, and Xcode can load their projects. A passing placeholder or file-existence
check is not release evidence.

## License

Source code is available under MIT or Apache-2.0 at your option. Datasets,
address labels, model weights, news corpora, and generated artifacts require
their own provenance and license manifests.
