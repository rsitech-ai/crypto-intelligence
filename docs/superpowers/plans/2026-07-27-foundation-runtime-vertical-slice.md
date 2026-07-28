# Foundation Runtime Vertical Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a stable, deterministic, local-only product slice in which a native macOS app securely starts the Rust daemon, the daemon ingests a checked-in Binance-compatible BTC order-book fixture through a durable WAL, and the app renders the authenticated server snapshot with explicit health and freshness.

**Architecture:** A small active Cargo workspace contains only behaviorally implemented crates. `cryptoriskd` owns authoritative configuration, raw capture, normalization, order-book state, durability, and RPC. The Swift app owns process supervision and presentation only. A 32-byte random secret travels through an inherited descriptor; HMAC-SHA256 binds the RPC bearer token to protocol version, process ID, process nonce, server nonce, issue time, and expiry. Public liveness is intentionally minimal, while market data requires authentication. All market values remain fixed-decimal strings across the wire and are formatted, not recomputed, by Swift.

**Tech Stack:** Rust 1.97.1 / MSRV 1.88, edition 2024, Tokio 1.51, Tonic 0.14, Prost 0.14, Buf v2, Serde/TOML/JSON Schema, BLAKE3, HMAC-SHA256, CRC32C, tracing; Swift 6.3 strict concurrency, SwiftUI, Observation, OSLog, CryptoKit, gRPC Swift 2, SwiftProtobuf, XcodeGen, XCTest/XCUITest.

## Global Constraints

- Rust production code uses Rust 1.97.1 initially, edition 2024, with MSRV 1.88; `Cargo.lock` is committed.
- Tokio stays on the tested 1.51 minor line and Tonic stays on the tested 0.14 minor line.
- Swift uses Swift 6.3 strict concurrency and targets Apple Silicon macOS 26.
- The Rust daemon is the only numerical and market-state authority. Swift never derives order-book state, quality, freshness, or probability.
- Source monetary values use checked `i128` fixed-decimal values. RPC represents them as canonical decimal strings.
- The system is local-first and offline-capable. Remote telemetry, crash upload, hosted inference, live trading, order placement, withdrawals, and trading credentials are absent.
- The daemon binds only an ephemeral IPv4 loopback address. The initial 256-bit secret is read exactly once from an inherited descriptor and never appears in arguments, environment, readiness output, errors, or logs.
- The authenticated token is `HMAC-SHA256(secret, canonical_session_bytes)` where the canonical bytes include the exact domain `cmti:session:v1\0`, protocol major/minor, daemon PID, 128-bit process nonce, 128-bit server nonce, issued Unix seconds, and expiry Unix seconds in a documented fixed order. Tokens expire after 60 seconds and use constant-time comparison.
- Unauthenticated liveness returns only serving/not-serving and protocol major/minor. Every market snapshot call requires a valid unexpired token and matching session metadata.
- The raw fixture record is appended and `sync_data` completes before its normalized event can update the order book or RPC snapshot.
- WAL frames use an explicit magic, schema version, payload length, payload, and CRC32C checksum. Recovery truncates only an incomplete/corrupt final frame; corruption before the final frame fails closed.
- The ingestion queue capacity is exactly 1,024. Producers await capacity; they never silently drop order-book snapshots or deltas.
- The first fixture is `BTCUSDT` spot generation 1. Its final expected state is sequence `102`, best bid `60000.10`, best ask `60000.20`, health `healthy`, and source `binance-fixture`.
- Order-book gaps, invalid prices/quantities, malformed JSON, authentication failures, and persistence failures cannot produce a healthy published snapshot.
- Generated protobuf sources and the Xcode project must be reproducible from checked-in inputs. Generation scripts fail nonzero when prerequisites or expected outputs are missing.
- Tests assert behavior and failure modes. Tautological assertions and path-existence-only acceptance tests do not satisfy a task.
- Every task finishes with focused tests, the strongest applicable subsystem gate, a commit, an independent spec/quality review, and clean test output.
- This plan proves a `runtime-proven foundation slice`, not full approved-spec, model, live-source, App Store, or release readiness.

---

### Task 1: Restore the pinned Rust workspace and fail-closed developer commands

**Files:**
- Modify: `Cargo.toml`
- Create: `Cargo.lock`
- Modify: `.cargo/config.toml`
- Modify: `xtask/Cargo.toml`
- Modify: `xtask/src/main.rs`
- Modify: `crates/system-tests/Cargo.toml`
- Replace: `crates/system-tests/tests/repository_layout.rs`
- Create: `crates/system-tests/tests/active_workspace.rs`

**Interfaces:**
- Consumes: pinned toolchain policy and the audited scaffold inventory
- Produces: a resolvable active workspace containing only `xtask` and `system-tests`, plus `xtask help` and `xtask workspace-check`

**Implementation notes:**

The root workspace must use an explicit member list. Do not include an unimplemented crate merely to make a path appear complete. `xtask workspace-check` runs `cargo metadata --locked --format-version 1`, validates that every active member has `[lints] workspace = true`, and returns a typed nonzero error for unknown commands. It must not invoke another async runtime or shell-expand user input. Keep the existing scaffold paths in place for later tasks.

- [ ] Write `active_workspace.rs` tests that parse `cargo metadata`, assert the exact initial packages `system-tests` and `xtask`, invoke both supported commands, and assert an unknown command fails nonzero with a stable error code.
- [ ] Run `cargo test -p system-tests --test active_workspace` and capture the expected manifest failure before implementation.
- [ ] Repair the manifests and command implementation, format the touched Rust code, and generate `Cargo.lock`.
- [ ] Run `cargo metadata --locked --format-version 1`, `cargo test --workspace --locked`, `cargo check --workspace --all-targets --locked`, and `cargo run --locked -p xtask -- workspace-check`.
- [ ] Run `git diff --check`; commit only the task files with subject `build: restore active Rust workspace`.

---

### Task 2: Implement canonical contracts and validated local configuration

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/fixed-decimal/Cargo.toml`
- Replace: `crates/fixed-decimal/src/lib.rs`
- Create: `crates/fixed-decimal/tests/canonical_decimal.rs`
- Modify: `crates/domain/Cargo.toml`
- Replace: `crates/domain/src/lib.rs`
- Create: `crates/domain/tests/identity_contract.rs`
- Modify: `crates/event-envelope/Cargo.toml`
- Replace: `crates/event-envelope/src/lib.rs`
- Create: `crates/event-envelope/tests/event_identity.rs`
- Modify: `crates/config/Cargo.toml`
- Replace: `crates/config/src/lib.rs`
- Create: `crates/config/src/schema.rs`
- Modify: `configs/default.toml`
- Replace: `configs/schema.json`
- Replace: `crates/config/tests/config_validation.rs`
- Modify: `xtask/Cargo.toml`
- Modify: `xtask/src/main.rs`

**Interfaces:**
- Consumes: Task 1 workspace
- Produces: checked fixed decimals, generation-aware identities, complete canonical event hashing, layered local configuration, exact schema generation, and a canonical configuration fingerprint

**Implementation notes:**

Add these crates to the active workspace only after their focused tests pass. `FixedDecimal::parse_canonical` rejects whitespace, leading plus, exponent notation, leading whole-part zeros other than `0`, trailing fractional zeros, negative zero, empty fractional parts, and scale above 38. Programmatic construction may canonicalize. Event IDs use BLAKE3 with domain `cmti:event:v1\0` and must change when any metadata or payload field changes. Configuration uses `deny_unknown_fields`, rejects direct secret material and non-Keychain credential references, enforces loopback binding, nonzero bounded capacities/timeouts, disabled remote export/telemetry, readable fixture input, and writable contained data/log roots without following a symlink outside the approved root. The checked-in schema must byte-match generation.

- [ ] Add failing boundary/property tests for canonical decimal parsing, arithmetic overflow, generation zero, identity normalization, complete event mutation sensitivity, unknown config keys, direct secrets, non-loopback binding, zero/oversized limits, symlink escape, and default-config/schema agreement.
- [ ] Run the four focused crate test commands and capture the intended failures.
- [ ] Implement the smallest cohesive contracts, expand `xtask generate-config-schema`, and add the four crates to the workspace.
- [ ] Run `cargo run --locked -p xtask -- generate-config-schema --check`, the four crate test suites, `cargo fmt --all -- --check`, and workspace clippy with `-D warnings`.
- [ ] Run `git diff --check`; commit with subject `feat: add canonical local configuration contracts`.

---

### Task 3: Establish versioned protobufs and authenticated loopback RPC

**Files:**
- Replace: `proto/buf.yaml`
- Replace: `proto/buf.gen.yaml`
- Replace: `proto/common/v1/common.proto`
- Replace: `proto/health/v1/health.proto`
- Replace: `proto/market/v1/market.proto`
- Modify: `Cargo.toml`
- Modify: `crates/local-api/Cargo.toml`
- Replace: `crates/local-api/build.rs`
- Replace: `crates/local-api/src/lib.rs`
- Replace: `crates/local-api/src/auth.rs`
- Create: `crates/local-api/src/session.rs`
- Create: `crates/local-api/src/server.rs`
- Create: `crates/local-api/tests/authentication.rs`
- Create: `crates/local-api/tests/loopback_server.rs`
- Replace: `scripts/generate-proto.sh`
- Modify: `xtask/Cargo.toml`
- Modify: `xtask/src/main.rs`

**Interfaces:**
- Consumes: Task 2 canonical types/configuration
- Produces: Buf-valid v1 contracts, deterministic Rust generation, `SessionDescriptor`, `SessionAuthenticator`, minimal `HealthService`, authenticated `MarketStateService.GetOrderBookSnapshot`, and a loopback server handle with graceful shutdown

**Implementation notes:**

Use unique protobuf packages `cmti.common.v1`, `cmti.health.v1`, and `cmti.market.v1`. `SessionDescriptor` documents and serializes the canonical authentication fields, never the secret/token. The server accepts a pre-read 32-byte zeroizing secret and binds `127.0.0.1:0`; it rejects IPv6-any and all non-loopback addresses. The liveness method has no paths, build hashes, hostnames, PIDs, nonces, or configuration values. The snapshot response contains source, symbol, generation, sequence, canonical best-bid/ask strings, health enum, event/receive timestamps, and freshness milliseconds. Reflection is disabled.

- [ ] Add failing tests for malformed/short descriptor secrets, token mutation, wrong PID/nonce/version, expiry boundaries, constant-time validation interface, missing metadata, replay after expiry, non-loopback bind, unauthenticated liveness minimization, authenticated snapshot success, and unauthenticated snapshot rejection.
- [ ] Run `buf lint proto`, `buf build proto`, and the local-api tests to capture the intended failures.
- [ ] Implement the contracts/server and a generation script that uses pinned project dependencies, writes only expected outputs, and supports `--check`.
- [ ] Run `buf lint proto`, `buf build proto`, `scripts/generate-proto.sh --check`, `cargo test -p local-api --locked`, and workspace fmt/clippy.
- [ ] Run `git diff --check`; commit with subject `feat: add authenticated loopback market RPC`.

---

### Task 4: Run durable deterministic market ingestion in the daemon

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/raw-wal/Cargo.toml`
- Replace: `crates/raw-wal/src/lib.rs`
- Replace: `crates/raw-wal/src/frame.rs`
- Replace: `crates/raw-wal/src/recovery.rs`
- Replace: `crates/raw-wal/src/segment.rs`
- Create: `crates/raw-wal/tests/wal_recovery.rs`
- Modify: `crates/connector-binance/Cargo.toml`
- Replace: `crates/connector-binance/src/lib.rs`
- Replace: `crates/connector-binance/src/parser.rs`
- Create: `crates/connector-binance/tests/fixture_parser.rs`
- Modify: `crates/orderbook/Cargo.toml`
- Replace: `crates/orderbook/src/lib.rs`
- Create: `crates/orderbook/tests/sequence_and_quality.rs`
- Modify: `crates/observability/Cargo.toml`
- Replace: `crates/observability/src/lib.rs`
- Create: `crates/observability/tests/redaction.rs`
- Modify: `apps/cryptoriskd/Cargo.toml`
- Replace: `apps/cryptoriskd/src/main.rs`
- Replace: `apps/cryptoriskd/src/startup.rs`
- Create: `apps/cryptoriskd/src/runtime.rs`
- Create: `apps/cryptoriskd/tests/fixture_runtime.rs`
- Create: `fixtures/binance/btcusdt-book-v1.jsonl`

**Interfaces:**
- Consumes: Tasks 2–3 types/configuration/RPC
- Produces: persist-before-process WAL, bounded fixture connector, normalized snapshot/deltas, authoritative order book, authenticated runtime snapshot, structured local logs, and graceful shutdown

**Implementation notes:**

Use Castagnoli CRC32C, not IEEE CRC32. The fixture has one snapshot ending at sequence 100 and two contiguous deltas ending at 101 and 102. Runtime owns a `tokio::mpsc::channel(1_024)`; fixture ingestion awaits sends. Every raw line is framed, appended, and synced before parsing/normalization. A parse error or sequence gap increments a bounded metric, marks the source degraded, and does not replace the last healthy snapshot. Readiness is one JSON line on stdout after WAL recovery, RPC bind, and initial fixture ingestion succeed; it contains endpoint and public session descriptor fields but no secret or token. All later diagnostics use JSON logs with secret/token redaction. SIGTERM/Ctrl-C cancels tasks, drains accepted work, syncs WAL/logs, and stops RPC within five seconds.

- [ ] Add failing tests for exact fixture output, malformed JSON, negative/zero values, wrong symbol/generation, sequence gap, queue capacity, WAL magic/version/length/CRC, final-tail truncation, mid-file corruption, sync-before-publication fault injection, log redaction, clean shutdown, and restart recovery.
- [ ] Run the focused crate/runtime tests and capture their expected failures.
- [ ] Implement the WAL, parser, order book, observability, daemon orchestration, and fixture; add the crates/app to the active workspace after their focused tests pass.
- [ ] Run all focused tests, then workspace fmt, clippy, and tests with the lockfile.
- [ ] Launch the daemon test harness twice, inspect the process and JSON logs, and confirm no secret/token, panic, error, or warning appears on the healthy path.
- [ ] Run `git diff --check`; commit with subject `feat: run durable fixture market pipeline`.

---

### Task 5: Build the native macOS supervisor and market overview

**Files:**
- Create: `apps/macos/project.yml`
- Replace: `apps/macos/CuspObservatory.xcodeproj/project.pbxproj`
- Replace: `apps/macos/Packages/TransitionClient/Package.swift`
- Create: `apps/macos/Packages/TransitionClient/Package.resolved`
- Replace: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/TransitionClient.swift`
- Replace: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC/RPCTransport.swift`
- Replace: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC/GRPCTransport.swift`
- Replace: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC/Compatibility.swift`
- Create: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/RPC/SessionAuthentication.swift`
- Create: `apps/macos/Packages/TransitionClient/Sources/TransitionClient/Generated/`
- Replace: `apps/macos/Packages/TransitionClient/Tests/TransitionClientTests/PackageContractTests.swift`
- Replace: `apps/macos/Packages/TransitionClient/Tests/TransitionClientTests/RPCTransportTests.swift`
- Replace: `apps/macos/CuspObservatory/App/CuspObservatoryApp.swift`
- Replace: `apps/macos/CuspObservatory/App/AppEnvironment.swift`
- Replace: `apps/macos/CuspObservatory/App/AppModel.swift`
- Replace: `apps/macos/CuspObservatory/Daemon/DaemonState.swift`
- Replace: `apps/macos/CuspObservatory/Daemon/SessionBootstrap.swift`
- Replace: `apps/macos/CuspObservatory/Daemon/DaemonSupervisor.swift`
- Replace: `apps/macos/CuspObservatory/Features/Overview/MarketOverviewModel.swift`
- Replace: `apps/macos/CuspObservatory/Features/Overview/MarketOverviewView.swift`
- Replace: `apps/macos/CuspObservatory/Features/Health/DataHealthView.swift`
- Replace: `apps/macos/CuspObservatory/Design/DesignTokens.swift`
- Replace: `apps/macos/CuspObservatory/CuspObservatory.entitlements`
- Replace: `apps/macos/CuspObservatoryTests/DaemonSupervisorTests.swift`
- Replace: `apps/macos/CuspObservatoryTests/AppModelTests.swift`
- Replace: `apps/macos/CuspObservatoryTests/MarketOverviewModelTests.swift`
- Replace: `apps/macos/CuspObservatoryUITests/MarketOverviewUITests.swift`
- Replace: `apps/macos/CuspObservatoryUITests/FailureStateUITests.swift`
- Modify: `scripts/generate-proto.sh`
- Create: `scripts/generate-xcode-project.sh`

**Interfaces:**
- Consumes: Task 3 protobuf/session contract and Task 4 daemon executable
- Produces: strict-concurrency Swift client, protected inherited-descriptor bootstrap, actor-isolated daemon/RPC lifecycle, immutable market view state, real SwiftUI overview/health UI, and reproducible Xcode project

**Implementation notes:**

Pin the three official gRPC Swift 2 packages (`grpc-swift-2`, `grpc-swift-nio-transport`, `grpc-swift-protobuf`) and SwiftProtobuf through `Package.resolved`. `SessionBootstrap` creates 32 random bytes with `SecRandomCopyBytes`, writes them through a pipe inherited by the daemon, and zeroes temporary mutable storage. It derives the token with CryptoKit from the public readiness descriptor. The secret/token never enters `Process.arguments`, `Process.environment`, OSLog, UI, or thrown error descriptions. `DaemonSupervisor` is an actor with explicit `stopped`, `starting`, `healthy`, `degraded`, `failed`, and `stopping` states; it bounds startup and shutdown, cancels streams, and owns every child task. `AppModel` is `@MainActor` and only maps validated RPC snapshots. The overview displays BTCUSDT, best bid/ask, sequence, source, health, and freshness, plus disconnected/loading/error states and an accessible summary. Use XcodeGen as a deterministic generator and check the project file in.

- [ ] Add failing Swift tests for privacy defaults, HMAC parity with a Rust vector, descriptor validation, argument/environment secrecy, startup timeout, incompatible protocol, cancellation, clean termination, immutable snapshot mapping, stale/degraded state, and exact BTC row values.
- [ ] Add failing XCUITests that launch with the built daemon/fixture, wait for the healthy BTC row, verify its accessibility summary, then launch with an invalid daemon path and verify the recovery state.
- [ ] Implement the Swift package, generated stubs, app, project spec, entitlements, and deterministic generation scripts.
- [ ] Run `swift test --package-path apps/macos/Packages/TransitionClient`, `scripts/generate-proto.sh --check`, `scripts/generate-xcode-project.sh --check`, and strict-concurrency `xcodebuild test`.
- [ ] Launch the built `.app`, confirm the child daemon remains running only while supervised, exercise quit/relaunch, and inspect unified/app/daemon logs for secrets, warnings, crashes, hangs, or unexpected errors.
- [ ] Run `git diff --check`; commit with subject `feat: add native supervised market overview`.

---

### Task 6: Add the fail-closed stability gate and current evidence

**Files:**
- Create: `scripts/verify-foundation-runtime.sh`
- Create: `crates/system-tests/tests/no_execution_surface.rs`
- Modify: `docs/implementation/IMPLEMENTATION-PROGRESS.md`
- Modify: `docs/implementation/SPEC-COVERAGE.md`
- Create: `docs/implementation/FOUNDATION-RUNTIME-RUNBOOK.md`
- Create: `release/evidence/foundation-runtime-verification.json`

**Interfaces:**
- Consumes: Tasks 1–5
- Produces: one noninteractive verification entry point, a no-execution boundary test, an operator smoke/recovery runbook, commit-bound machine evidence, and honest phase status

**Implementation notes:**

The verification script uses a unique temporary root, builds the exact locked Rust and Swift/Xcode products, launches the actual daemon and app test path, validates the authenticated snapshot, shuts down, relaunches, validates WAL recovery, checks process cleanup, and inspects logs. It must use traps for cleanup and fail on missing tools, missing outputs, warnings configured as errors, panics, secrets, unexpected error-level logs, stale child processes, or evidence/commit mismatch. Evidence records commit, clean-tree state, tool versions, exact commands, exit codes, test counts, artifact hashes, and readiness label. Documentation must retain all still-unimplemented phases and the static-audit count rather than calling the repository fully implemented.

- [ ] Add a failing no-execution test that inspects active Cargo features, protobuf services, CLI help, configuration fields, and runtime outbound destination declarations for any order/withdrawal/trading-credential authority.
- [ ] Write the verification script and first run it to capture missing/dirty evidence failures.
- [ ] Implement the boundary test, runbook, evidence generator, and accurate status updates.
- [ ] Run the full gate twice from clean process state, with a deliberate auth-failure probe between healthy runs; confirm both healthy runs produce identical market snapshot digests.
- [ ] Run workspace fmt/clippy/tests, Buf lint/build/generation check, Swift package tests, Xcode unit/UI tests, `scripts/static-audit.py`, and `scripts/security-audit.py`; record remaining scaffold findings as later-phase blockers rather than suppressing them.
- [ ] Run `git diff --check`; commit with subject `test: prove foundation runtime stability`.

---

## Plan Completion Gate

The plan is complete only when every task has an independent clean review, the final whole-branch review has no open load-bearing finding, the complete verification script passes twice from clean process state, the app-to-daemon runtime is manually observed, logs are clean and secret-free, and documentation says `runtime-proven foundation slice` while accurately listing every unimplemented approved-spec phase.
