# Implementation Plan Self-Review

**Review date:** 2026-07-24  
**Reviewed baseline:** approved production specification SHA-256 `893a7add81b9c3a6f529e87b3a41238a32d840e612748811dd0ae5aea02f66be`

## Result

The implementation package passes the planning-artifact audit after inline corrections.

| Check | Result | Evidence |
|---|---|---|
| Scope decomposition | Pass | One master roadmap and nine phase plans isolate contracts, ingestion, features/models, cusp research, risk/alerts, slow sources, macOS, Apple models, and release hardening. |
| Task numbering | Pass | 106 tasks are contiguous within their phase. |
| TDD step structure | Pass | Every task has Steps 1–7: failing test, observed failure, minimal implementation, passing test, subsystem verification, integrity review, focused commit. |
| Checklist count | Pass | 742 actionable checklist steps. |
| Placeholder scan | Pass | No unresolved marker, omitted-body marker, or staged “implement later” instruction remains in a phase plan. |
| Markdown structure | Pass | All plan code fences are balanced and required headers/sections are present. |
| Global constraints | Pass | Every phase repeats the exact Rust, Swift, SQLite, numeric, local-first, no-execution, queue, persistence, and evidence constraints. |
| Repository naming | Pass | The frozen executable/package names are used consistently: `cryptoriskd`, `crypto-replay`, `crypto-evaluate`, `connector-certify`, `load-generator`, `shadow-reporter`, and `release-cli`. |
| Cross-workspace test ownership | Pass | Cross-crate tests are assigned to `crates/system-tests`; benchmark targets are assigned to `crates/performance-testkit`. No virtual-workspace root test target is assumed. |
| Traceability | Pass | 106 task rows map to approved specification sections and testable deliverables. |
| Specification coverage | Pass | All 40 top-level specification sections and all appendices have owning plans. |
| Integrity files | Pass | Approved-spec, phase-plan, and combined-plan SHA-256 values are recorded; the final package receives a complete `MANIFEST.sha256`. |
| Local links | Pass | Package README and plan-index relative links resolve. |

## Corrections made during review

The review replaced an incomplete fixed-decimal rescaling body with checked integer multiplication/division and explicit precision-loss handling; corrected the EWMA reference value; pinned the checkout action in the CI example; normalized Phase 00 Markdown; assigned cross-workspace tests to a real Cargo package; aligned late-phase CLI and daemon paths with the frozen repository layout; removed duplicate file-creation declarations; and made Phase 08 extend rather than recreate the Phase 00 observability and governance foundations.

## Consistency decisions

1. `apps/*` and `crates/*` are Cargo workspace members, allowing later phase packages without reopening the workspace topology.
2. `crates/system-tests` owns cross-component integration, concurrency, chaos, performance, shadow, governance, and release tests.
3. `crates/performance-testkit` owns Criterion benchmarks and the reusable reference-load harness.
4. Phase 08 combines the specification’s shadow-validation/release-candidate and stable-open-source release gates into one hardening plan while retaining separate preview-to-stable evidence transitions.
5. The approved specification remains authoritative. A plan change that alters forecast meaning, labels, security boundaries, source semantics, or release gates requires an ADR and an updated approval record.

## Explicit non-claims

This review validates the **planning artifacts**, not an implementation that does not yet exist. No source repository was supplied in this conversation, so the listed Cargo, Swift, Xcode, replay, fuzz, soak, notarization, and model-validation commands have not yet been executed against product code. Their outputs become mandatory evidence during implementation and cannot be inferred from this document review.
