# App E2E Audit Report: Crypto Market Transition Intelligence

## Scope

- Date: 2026-07-27
- Auditor: Codex with three independent read-only review lanes
- App path: `/Users/s1kor/dev/apps/crypto-inteligence`
- Comparison point: `main` at `f201869`
- Candidate: `implementation/full-system-v3` / `52287af`, audited on `feat/andrzej_full_system_audit`
- Project/workspace/package: Rust workspace plus SwiftPM and Xcode macOS targets
- Intended scheme/target: `CuspObservatory`, `cryptoriskd`, `TransitionModelHost`
- Bundle id: unavailable; the Xcode project has no valid target
- Platform/surfaces: Apple Silicon macOS native app, local Rust daemon, local model host, replay/evaluation/release CLIs
- Readiness target: release-candidate-ready
- Forbidden actions: live trades, exchange-account mutation, private credentials, paid APIs, destructive system changes

## Executive Decision

**REJECT / HOLD. Do not merge the candidate to `main`.**

The candidate is a path-complete scaffold, not an end-to-end implementation.
The repository adds 690 files, but the Rust workspace cannot be parsed, the
Xcode project cannot be opened, all seven Rust apps are print-and-exit markers,
the native Swift product files are contract-only placeholders, and nearly all
claimed tests are tautologies. There is no daemon or native app process to
restart, no runtime data/log path to inspect, and no valid full-system flow to
exercise.

The pre-existing audit reported success because it mostly checked file
existence and token presence. This audit hardens that check so the same
repository now fails closed with 400 structural/placeholder errors.

## Official Documentation Baseline

- Cargo workspace members must be packages with valid manifests, and workspace
  members share the root lockfile:
  [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)
  and
  [manifest format](https://doc.rust-lang.org/cargo/reference/manifest.html).
- A production Tokio service needs shutdown detection, cancellation
  propagation, and waiting for tasks to finish:
  [Tokio graceful shutdown](https://tokio.rs/tokio/topics/shutdown).
- Buf lint treats structural and style violations as errors and recommends the
  `STANDARD` category:
  [Buf lint rules](https://buf.build/docs/lint/rules/).
- Tonic's maintained released line is 0.14.x, while this repository declares
  0.12 without a compatibility justification:
  [Tonic repository](https://github.com/hyperium/tonic) and
  [Tonic 0.14.6 API](https://docs.rs/tonic/latest/tonic/all.html).
- A native Mac product should support windows, menus, keyboard workflows, and
  adaptable desktop interaction:
  [Designing for macOS](https://developer.apple.com/design/human-interface-guidelines/designing-for-macos/)
  and
  [Accessibility](https://developer.apple.com/design/human-interface-guidelines/accessibility).

## Tool Plan

| Surface | Tool | Reason | Status |
| --- | --- | --- | --- |
| Rust build/test/logs | Cargo, rustfmt, rust-analyzer, terminal | Parse, compile, test, run, and inspect service lifecycle | Blocked by invalid manifest and source |
| Proto | Buf 1.72 | Build and lint the canonical RPC schema | Failed |
| Swift portable package | SwiftPM 6.3.3 | Check the only executable Swift subset | Portable subset only |
| Native macOS app | Xcode 26.6 | Discover scheme, build, test, and launch | Blocked by invalid project |
| Native app UI | Computer Use | Exercise real native controls and windows | Not possible; no app bundle/window |
| SwiftUI polish | Source/runtime matrix | Verify native views and interaction inventory | Failed; views are placeholders |
| Product quality | Cross-platform and Apple-native gates | Reconcile product claims with behavior | Failed |
| Security/release | Repository scripts and manual review | Verify boundaries, secrets, artifacts, and evidence truth | Failed overall; narrow secret scan passes |

## Commands And Artifacts

| Check | Command / Tool | Result | Evidence |
| --- | --- | --- | --- |
| Git comparison | `git diff --stat main...52287af` | 690 files, 10,728 additions, 26 deletions | Candidate is one commit ahead of spec-only `main` |
| Cargo metadata | `cargo metadata --locked --offline --no-deps --format-version 1` | Failed, exit 101 | `xtask/Cargo.toml` has neither `[package]` nor `[workspace]` |
| Cargo fmt/check/clippy/test/run | Workspace commands | Blocked, exit 101 | Same invalid member manifest |
| Independent Rust parse | `rustfmt --check crates/local-api/build.rs crates/cusp/src/lib.rs` | Failed | Path-only build script; edition-2024 `gen` keyword parse errors |
| Static audit tests | `python3 -m unittest discover -s scripts/tests -p 'test_*.py'` | Passed, 15 tests | Tests cover placeholder, manifest, Xcode, workflow, and secret-scan regressions |
| Hardened static audit | `python3 scripts/static-audit.py` | Failed closed, 400 errors | 197 generic contracts, 99 tautological tests, 68 Swift stubs, and structural blockers |
| Narrow security scan | `python3 scripts/security-audit.py` | Passed, 0 pattern findings | Report explicitly says it does not verify security implementation/runtime |
| Buf lint/build | `buf lint`; `buf build --exclude-source-info -o /dev/null` | Failed, exit 100 | Invalid Buf config/import roots and duplicate generated contract |
| Swift portable tests | `swift test --package-path apps/macos` | Passed only for five portable tests | Does not compile native app sources |
| Swift release build | `swift build --package-path apps/macos -c release -Xswiftc -warnings-as-errors -Xswiftc -strict-concurrency=complete` | Passed for portable CLI targets | No `.app` product |
| Portable launch | `swift run --package-path apps/macos CuspObservatory` | Exits after explanatory print | Explicitly says native target is required |
| Xcode discovery | `xcodebuild -list -project apps/macos/CuspObservatory.xcodeproj` | Failed, exit 74 | Project object graph is empty |
| Xcode tests | `xcodebuild test ... -scheme CuspObservatory -destination 'platform=macOS'` | Failed, exit 74 | Project cannot be loaded |
| Service process/log check | process and filesystem inspection | No service, logs, or data | `cryptoriskd` only prints and exits |
| Release scripts | package, proto, soak, SBOM, provenance, notarize, verify | False-success no-ops | Each prints `installed` and exits zero |

## Hardened Static-Audit Breakdown

| Failure class | Count |
| --- | ---: |
| Generic Rust identifier-contract modules | 197 |
| Tautological Rust tests | 99 |
| Contract-only Swift sources | 68 |
| Path-only source placeholders | 18 |
| Installed-only shell scripts | 9 |
| Non-executable GitHub workflows | 6 |
| Invalid Cargo manifests | 2 |
| Invalid Xcode projects | 1 |
| **Total** | **400** |

## Scenario Matrix

| Surface | Scenario | Expected | Actual | Status | Evidence |
| --- | --- | --- | --- | --- | --- |
| Rust daemon | Start with validated local config | Persistent service, bounded tasks, readiness and structured logs | Prints `source contract installed` and exits | Failed | `apps/cryptoriskd/src/main.rs` |
| Rust daemon | Graceful shutdown | Detect signal, cancel tasks, drain, flush, exit cleanly | No runtime/tasks/signals exist | Failed | No Tokio dependency or code path |
| Connector | Fixture capture and book recovery | Parse venue data, detect gaps/checksums, recover | Parser/book modules are generic contracts | Failed | Connector source inventory |
| WAL/replay | Append, crash-tail recovery, deterministic replay | Framed CRC data and reproducible output | WAL/replay modules are placeholders | Failed | `crates/raw-wal`, `crates/replay-engine` |
| Forecast pipeline | Fixture event to calibrated forecast | Point-in-time features, model, evidence, abstention | No wired dependency graph or service pipeline | Failed | Apps depend on no internal runtime/model crates |
| RPC | Authenticated loopback negotiation and stream | Generated schema, nonce/process binding, limits, cancellation | No server/client generation or transport | Failed | Buf failures and placeholder local API |
| Native app | First launch | Valid app bundle and window | No buildable Xcode target or SwiftUI `App` | Blocked | Xcode exit 74 |
| Native app | Navigation through product destinations | Real sidebar/menus/views | All native views are contract-only enums | Blocked | 68 Swift placeholders |
| Native app | Disconnected/degraded/offline recovery | Responsive stale state and recovery actions | No app model or UI | Blocked | App/daemon sources |
| Native app | Alerts/replay/export/settings | Stateful workflows with validation/cancel/recovery | Not implemented | Blocked | Feature files and no-op scripts |
| Native app | Accessibility/window adaptation | Keyboard, VoiceOver, Reduce Motion, resize, appearance | No window or controls | Blocked | No runnable app |
| Persistence | Quit/relaunch | Durable settings/watchlist/alerts/replay state | Portable store is memory-only | Failed | `PortableSources/CuspObservatoryCore/Store.swift` |
| Release | Build/package/verify | Signed artifact and fail-closed evidence | Scripts exit zero without artifacts | Failed | Nine no-op scripts |

## Product Quality Findings

| Category | Severity | Surface | Evidence | Product impact | Remediation | Status | Re-verification |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Objective defect | Blocker | Repository trust | Static audit previously passed a scaffold | Reviewers can mistake paths for behavior | Detect structural placeholders and false-green workflows | Fixed in audit tooling | 15 audit tests pass; repository now fails with 400 errors |
| Objective defect | Blocker | Rust workspace | Invalid `xtask` manifest and missing lockfile | No Rust build/test/service evidence exists | Restore valid manifests, lockfile, parse-clean sources | Blocked | Cargo exits 101 |
| Objective defect | Blocker | Native app | Empty Xcode project and no SwiftUI entry point | No user-facing product exists to operate | Create real targets/schemes/app shell | Blocked | Xcode exits 74 |
| Objective defect | Blocker | Runtime | Print-and-exit applications and disconnected crates | No capture, state, model, RPC, or audit flow | Implement one vertical runtime slice first | Blocked | No persistent process/log |
| Objective defect | Blocker | Tests | 99 tautological Rust tests and tautological native tests | Regressions and requirements are untested | Replace with behavioral boundary/property/E2E tests | Blocked | Static audit detects all 99 Rust tautologies |
| Objective defect | Blocker | Release gates | No-op scripts and workflows | Automation can report success without work | Fail closed until each gate has a real implementation | Blocked | Static audit detects 9 scripts and 6 workflows |
| Objective defect | High | Model packaging | BLAKE2s digest labeled `artifact_blake3` | Producers and verifiers disagree on integrity | Use pinned BLAKE3 with cross-language vectors | Blocked | Python BLAKE3 provider absent |
| Objective defect | High | Proto/RPC | Buf build/lint fail; generated output is a path string | No interoperable or authenticated API | Repair workspace, imports, unique contracts, generation | Blocked | Buf exits 100 |
| Objective defect | High | Configuration | Shipped schema rejects shipped defaults | Startup validation cannot be trusted | Generate schema from typed config and test defaults | Blocked | Manual/schema review |
| Objective defect | High | Security claims | File-presence checks substituted for runtime security | Local-only/auth/recovery claims remain unproven | Implement and test boundaries; keep narrow scan labeled | Partly fixed | Secret-pattern scan passes; implementation remains blocked |
| Objective defect | High | Documentation | README/progress/final audit call phases implemented | Public/reviewer trust is harmed | Relabel as scaffold and bind claims to executable evidence | Fixed in this audit pass | Documentation diff review |

## Code Review Findings

### Blockers

1. **No valid Rust workspace.** `xtask/Cargo.toml` is not a Cargo manifest;
   `fuzz/Cargo.toml` is also invalid; `Cargo.lock` is absent.
2. **No native macOS project.** The `.pbxproj` has an empty object graph and no
   scheme, target, bundle ID, build phases, or app entry point.
3. **No service architecture.** The application binaries do not depend on the
   workspace's connectors, storage, model, security, observability, or RPC
   crates.
4. **No behavioral test suite.** Claimed recovery, property, integration,
   concurrency, and acceptance tests assert only that `1 == 1`.
5. **No release automation.** Workflows have no triggers/jobs and named release
   scripts do not perform their operations.

### High-Severity Correctness And Security Findings

1. Event canonicalization omits normalized payload fields, so mutations can
   retain a valid event ID.
2. Authentication lacks the specified nonce/process/expiry/replay binding and
   is not connected to a Tonic interceptor.
3. Shadow-ledger in-memory uniqueness is updated before durable append, making
   retries fail after I/O errors.
4. Recovery path checks do not establish approved-root containment across all
   ancestors and restore invariants are incomplete.
5. Cusp and risk inputs do not comprehensively reject non-finite or incoherent
   values.
6. The local model host catches all errors, including cancellation and
   revocation-like failures, and returns a plausible fallback.
7. The no-trading/no-credential guarantee is currently a consequence of absent
   connectors, not a proven authority boundary.

### Maintainability And Performance Findings

1. Most non-placeholder Rust logic is minified onto very long physical lines,
   and rustfmt cannot parse representative files.
2. The workspace declares broad dependency ranges and a stale Tonic line
   without a lockfile or compatibility evidence.
3. There are only nine internal path-dependency edges across roughly 63 crates,
   so the documented modular monolith is not assembled.
4. No release/runtime process exists to measure launch time, throughput, memory,
   backpressure, shutdown latency, or UI responsiveness.

## Remediation Performed In This Audit

1. Added behavior tests for static-audit placeholder detection, Cargo manifest
   validity, Xcode project structure, GitHub workflow structure, and secret-scan
   false positives/true positives.
2. Hardened `scripts/static-audit.py` to reject:
   - tautological planned-contract tests;
   - generic identifier-contract modules;
   - contract-only Swift sources;
   - path-only source files;
   - installed-only shell scripts;
   - workflows without triggers/jobs;
   - Cargo manifests without package/workspace tables;
   - invalid Xcode project structure.
3. Fixed `scripts/security-audit.py` so it:
   - does not report its own pattern signatures or Python bytecode;
   - distinguishes redaction patterns from actual hard-coded assignments;
   - requires a PEM-like block rather than a substring;
   - discloses that its passing result covers only secret patterns and required
     file presence, not security implementation or runtime behavior.

These changes improve evidence truth; they do not implement the missing
product.

## Blocked Or Risky Actions

| Action | Why blocked | Next step |
| --- | --- | --- |
| Restart `cryptoriskd` | No service implementation exists; the binary prints and exits | Implement the first runtime vertical slice |
| Launch/click native app | Xcode project cannot load; no `.app` exists | Create valid native targets and app entry point |
| Inspect clean runtime logs | No persistent runtime or logging path exists | Add structured local logging with redaction |
| Run live connector certification | No connector implementation; live activity would add external risk without value | Implement fixture-backed connectors first |
| Run signing/notarization | No build artifact exists | Reach build-clean/package-ready before Apple gates |
| Create PR to `main` | Candidate fails blocker gates and would merge a false implementation | Keep `main` unchanged |
| Merge/push `main` | Explicitly prohibited by the user's own “only when approved and stable” condition | Re-audit after real vertical slices pass |

## Recommended Repair Sequence

1. Relabel the repository as scaffolded and preserve this fail-closed audit.
2. Restore a valid workspace/project shell: Cargo manifests, lockfile,
   parse-clean sources, Buf workspace, generated-code contract, and real Xcode
   targets.
3. Implement one vertical slice only:
   validated config → protected session bootstrap → one fixture-backed venue →
   framed WAL → normalized event/order book → authenticated loopback
   health/snapshot RPC → disconnected/native overview shell.
4. Replace tautologies with mutation-sensitive boundary, property, fault,
   integration, and native UI tests for that slice.
5. Add supervised Tokio tasks, bounded channels, graceful shutdown, structured
   local logs, recovery, and real fail-closed scripts/CI.
6. Expand connectors/features/models/UI only after the first slice is
   build-clean, runtime-proven, and reviewable.

## Final Readiness Label

- Label: **Untested native product / blocked: no runnable full system**
- Portable subset: Swift release-build-clean for five narrow portable tests;
  this is not native app or service proof.
- Evidence: Cargo exit 101, Buf exit 100, Xcode exit 74, no persistent service,
  no app bundle, and hardened static audit with 400 errors.
- Remaining blockers: implementation, behavioral tests, runtime, RPC,
  persistence, security boundaries, UI/accessibility, packaging, signing,
  notarization, soak, connector certification, and live-shadow evidence.
- Next audit pass: after the first real vertical slice and executable CI gates
  are present.
