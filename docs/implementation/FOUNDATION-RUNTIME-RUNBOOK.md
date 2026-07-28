# Foundation runtime verification runbook

This runbook operates only the local, fixture-backed foundation slice. It
builds and exercises the Rust daemon and native macOS application, but it does
not connect to an exchange, place orders, use live credentials, qualify a
model, or establish release/App Store readiness.

The single-run diagnostic entry point is:

```sh
scripts/verify-foundation-runtime.sh
```

It is deliberately fail-closed and can only emit one invocation. It never
accepts earlier evidence and therefore always retains the
`two-clean-full-gate-runs-not-observed` blocker when every subgate passes. Run
it from the repository root at an exact clean commit with no existing
`cryptoriskd`, `CuspObservatory`, or `cmti-daemon` process. It refuses a dirty
tree or stale process state instead of modifying either condition.

## What the verifier proves

One invocation:

1. records the exact commit and tool versions;
2. runs locked Rust build, Rust 1.88 MSRV, format, clippy, and workspace tests;
3. validates Buf schemas and deterministic protobuf, configuration, and Xcode
   generation;
4. runs the pinned Swift package and native unit/UI test paths;
5. launches the real daemon twice against a unique local fixture root;
6. authenticates the RPC, checks the exact BTCUSDT snapshot, deliberately
   verifies that missing authentication is rejected, performs a graceful
   shutdown, and proves identical snapshot and WAL digests after relaunch;
7. launches the built native application, observes the exact
   app-to-daemon-to-watcher topology, exercises controlled quit, forces
   supervisor loss, then relaunches against the same WAL;
8. inspects arguments, open files, logs, temporary artifacts, and raw/hex/base64
   secret forms; and
9. runs static/security audits plus clean-tree and stale-process checks.

Every build, test, and launched-runtime gate is bounded. Cleanup sends TERM and
then KILL when necessary, checks that the owned process group is actually gone,
removes only verifier roots matching `CuspObservatoryTests/Task6*`, and returns
nonzero if cleanup cannot be proved.

The behavioral no-execution test inventories the production dependency
closure, active Cargo features, protobuf RPCs, daemon CLI options,
configuration fields, positively owned network capabilities, and production
Rust identifiers. The only approved production network capability is the
exact loopback `TcpListener::bind(crate::server::LOOPBACK_BIND)` in
`crates/local-api`, backed by the exact loopback-only default and runtime
guard. Production source indirection is rejected rather than allowing modules
to escape inventory. UDP sends, DNS resolution,
client connects, raw sockets/endpoints, network command tools, and unsafe
network FFI fail the gate for explicit review. The inventory reads the locked
resolved Cargo graph: the approved Rustix package is pinned to its exact
non-network `alloc,default,fs,process,std` feature set, so direct, transitive,
or aliased `rustix::net` activation fails closed. Tonic `Endpoint` and
`Channel` client-type ownership is also rejected independent of the selected
connect or balancing method spelling. The production protobuf build is
server-only and rejects generated `_client`/`Client<` output.

## Prerequisites

Use the repository-pinned Rust, Swift, protobuf, and Xcode inputs. The verifier
checks its complete executable tool list before starting. In particular, the
machine needs:

- the pinned Rust toolchain plus Rust 1.88.0;
- `buf`, the pinned `protoc`, Swift, Xcode, and XcodeGen;
- `python3`, `shellcheck`, and the standard macOS process/log inspection tools;
- permission to build and run the local application and XCUITest.

The verifier records `DevToolsSecurity -status`; it never changes Developer
Mode, automation authorization, signing state, or any other host security
setting.

Before starting:

```sh
git status --short
pgrep -x cryptoriskd || true
pgrep -x CuspObservatory || true
ps -axo pid=,ppid=,command= | rg '[c]mti-daemon' || true
```

All four outputs must be empty.

## Required two-invocation sequence

The final `runtime-proven foundation slice` label requires the parent wrapper:

```sh
scripts/verify-foundation-runtime-twice.py \
  --evidence-output release/evidence/foundation-runtime-verification.json
```

The wrapper creates a mode-`0700` private directory, launches the exact
single-run verifier, validates the exact evidence schema and detailed records,
and writes an exclusive mode-`0600` SHA-256 anchor before it starts the second
invocation. It rechecks that anchor, validates the second evidence
independently, and emits a promotion receipt only when both otherwise-green
runs name the same commit. There is no option for user-supplied prior
evidence.

Validation requires the exact command inventory, successful command output
hashes and artifact inventory, two detailed direct-daemon records, the
deliberate unauthenticated response record, all three native-app lifecycle
records, identical snapshot/WAL recovery details, an exact 35-of-35 native
unit summary, a successful non-empty UI summary, zero current
static/security findings, and empty tree-mutation checks. Summary booleans
are recomputed from their detailed records.

If either invocation has any repository, runtime, toolchain, or external
blocker, the wrapper does not start or complete promotion and preserves the
latest blocked single-run evidence at the requested output.

## Evidence and readiness labels

`release/evidence/foundation-runtime-verification.json` records:

- exact commit, clean-tree checks, start/end times, and invocation count;
- for promoted evidence, a two-run receipt containing private-input digests,
  per-command output hashes, artifact hashes, runtime/WAL digests, and native
  test totals for both invocations;
- tool versions, exact commands, exit codes, durations, and output hashes;
- executable/product hashes and native test summaries;
- authenticated snapshot values/digests and deliberate authentication failure;
- graceful shutdown, forced-supervisor-loss, process topology, relaunch, and
  WAL recovery observations;
- argument, open-file, log, stale-process, and secret-scan findings;
- current audit counts, blockers, limitations, and readiness label.

The possible labels are:

- `runtime-proven foundation slice`: both complete clean invocations and all
  gates passed;
- `blocked:verification-incomplete`: an otherwise-green first invocation still
  lacks the independently validated second invocation;
- `blocked:repo`, `blocked:external`, or `blocked:repo+external`: one or more
  classified blockers remain.

The native SwiftNIO `CNIOWindows` module-map warning is recorded as an external
toolchain blocker when present. Disabled macOS automation/Developer Mode is
recorded as external only when it prevents the UI result. Every other warning,
missing result bundle, failed audit, unexpected log, cleanup uncertainty, or
tree mutation is a repository blocker.

## Failure handling

The verifier writes evidence even when a completed invocation is blocked.
Read `blockers`, then use the command ledger and hashed log metadata in the
evidence to identify the exact failing gate. The temporary logs are removed by
the cleanup trap, so rerun an individual recorded command when detailed local
diagnostics are needed.

If the verifier is interrupted, its signal handler returns a signal-specific
nonzero status and the EXIT trap performs the same owned-process cleanup. After
any abnormal termination, repeat the preflight process checks before rerunning.
Do not delete unrelated processes or data in order to make the check green.

The evidence is foundation-only. Even a passing label leaves market-data
coverage, feature/label pipelines, models, risk/alert services, slow sources,
hardening, distribution signing/notarization, live-provider qualification,
and production acceptance outside this gate.
