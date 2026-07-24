# Phase 05 — Options, On-Chain Data, Attribution, and Coupled Instability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add point-in-time options and local Bitcoin/Ethereum evidence, versioned attribution, stablecoin/contagion analysis, and an experimental BTC–ETH coupled cusp without degrading the trusted fast market-data path.

**Architecture:** Deribit chain snapshots and local blockchain clients feed slow structural feature pipelines with explicit publication lag, finality, revision, attribution coverage, and resource budgets. A separate coupled-cusp crate consumes aligned BTC/ETH structural state. Stablecoin and contagion modules remain independently gated research outputs until event counts and calibration support production status.

**Tech Stack:** Rust/Tokio, local Bitcoin JSON-RPC, Ethereum execution JSON-RPC and consensus REST, Arrow/Parquet, nalgebra, fixed-point token units, deterministic surface fitting, versioned local label registries, golden chain/option fixtures.

## Global Constraints

- Rust production code uses Rust 1.97.1 initially, edition 2024, with MSRV 1.88; `Cargo.lock` is committed.
- Tokio stays on the tested 1.51 LTS minor line; Tonic stays on the tested 0.14 minor line.
- Swift code uses Swift 6.3 strict concurrency. The app baseline is Apple Silicon macOS 26; Core AI is optional and capability-gated on macOS 27 or later.
- SQLite is bundled at 3.53.3 or later; 3.51.3 is the absolute floor. SQLite stores metadata and audit state only, never high-rate books or trades.
- Source monetary values use checked `i128` fixed-point wrappers. `f64` begins only at an explicitly tested analytical boundary.
- One Rust modular-monolith daemon owns authoritative state and probabilities. Swift renders values received through versioned protobuf contracts and never recalculates production forecasts.
- Raw capture, replay, feature materialization, training, inference, and audit are local-first. Remote telemetry and hosted inference are disabled by default.
- No trading or withdrawal credentials, order placement, or automated execution are introduced in v1.
- Every bounded queue has a declared capacity and overflow policy. No order-book delta is silently dropped.
- Every production forecast is persisted with model, feature, label, quality, evidence, and calibration identifiers before alert delivery.
- Development follows test-driven increments, warnings-as-errors where practical, frequent focused commits, and deterministic fixtures.
- No task may weaken point-in-time correctness, abstention, source-quality gating, artifact signing, or audit lineage to make a demo pass.

---

## Scope and completion boundary

This plan adds Deribit options analytics; Bitcoin and Ethereum local-node ingestion; chain/stablecoin/protocol features; point-in-time attribution; BTC–ETH coupled cusp; stablecoin and contagion labels/state; revision-aware slow-source scheduling; and incremental-value/resource gates. It excludes new cloud vendors, broad social feeds, all-chain coverage, production UI implementation, and any automatic model promotion.

## File and module map

- `crates/options`: option chains, cleaning, surfaces, skew/term/gamma features.
- `crates/chain-core`: shared chain source/finality/revision contracts.
- `crates/chain-bitcoin`, `crates/chain-ethereum`: local node clients and chain features.
- `crates/attribution`: versioned point-in-time address/protocol labels.
- `crates/coupled-cusp`: BTC–ETH joint potential, estimation, Hessian, and online output.
- `crates/contagion`: stablecoin labels, propagation state, and network features.
- `crates/slow-sources`: publication/revision scheduler and resource isolation.

## Exit gate

Options and chain replay are deterministic; node finality/reorg/revision semantics are correct; historical attribution never sees future labels; slow work cannot violate fast-path SLOs; option/on-chain features pass incremental-value reports or remain experimental; coupled cusp passes its numerical suite and receives only experimental status until a separate ablation/shadow gate; stablecoin/contagion outputs satisfy point-in-time and minimum-event policies.

---

### Task 1: Build the Deribit option-chain snapshot and normalized surface input layer

 **Files:**
 - Create: `crates/options/Cargo.toml`
- Create: `crates/options/src/lib.rs`
- Create: `crates/options/src/chain.rs`
- Create: `crates/options/src/forward.rs`
- Create: `crates/options/src/clean.rs`
- Test: `crates/options/tests/chain_snapshot.rs`

 **Interfaces:**
 - Consumes: Deribit option instruments/tickers/trades/books, consolidated index price, and event-time windows
 - Produces: `OptionChainSnapshot` grouped by asset/expiry, bid/ask/mark IV cleaning, forward estimation, no-arbitrage flags, source quality, and as-known-at lineage

 **Implementation notes**

 Preserve venue-reported IV/Greeks as raw normalized fields. Cleaning and recomputation produce new versioned analytical fields; they never overwrite the source record.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/options/tests/chain_snapshot.rs` with:

 ```text
 use options::OptionChainBuilder;

#[test]
fn chain_groups_expiry_and_rejects_crossed_or_stale_quotes() {
    let snapshot = OptionChainBuilder::fixture().build().unwrap();
    assert_eq!(snapshot.expiries.len(), 2);
    assert!(snapshot.excluded.iter().any(|v| v.reason == "crossed_quote"));
    assert!(snapshot.excluded.iter().any(|v| v.reason == "stale"));
}

#[test]
fn forward_uses_put_call_parity_when_coverage_is_sufficient() {
    let snapshot = OptionChainBuilder::fixture_parity().build().unwrap();
    assert!((snapshot.expiries[0].forward - 60_250.0).abs() < 1e-6);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p options --test chain_snapshot`

 Expected: FAIL because options chain and cleaning APIs are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/options/src/chain.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct OptionPoint {
    pub instrument_id: domain::InstrumentId,
    pub expiry_ns: i64,
    pub strike: f64,
    pub side: domain::OptionSide,
    pub bid_iv: Option<f64>,
    pub ask_iv: Option<f64>,
    pub mark_iv: Option<f64>,
    pub open_interest: f64,
    pub volume: f64,
    pub quality: f64,
}

#[derive(Clone, Debug)]
pub struct ExpirySlice {
    pub expiry_ns: i64,
    pub time_to_expiry_years: f64,
    pub forward: f64,
    pub points: Vec<OptionPoint>,
}

#[derive(Clone, Debug)]
pub struct OptionChainSnapshot {
    pub asset_id: domain::AssetId,
    pub as_known_at_ns: i64,
    pub expiries: Vec<ExpirySlice>,
    pub excluded: Vec<ExcludedOptionPoint>,
    pub quality_score: f64,
    pub lineage_hash: [u8; 32],
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p options --test chain_snapshot`

 Expected: PASS for expiry grouping, quote cleaning, forward parity, mark fallback, no-arbitrage flags, stale data, and lineage tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p options && cargo clippy -p options --all-targets -- -D warnings`

 Expected: options chain tests pass and cleaned versus source-reported fields remain separately auditable

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/options Cargo.toml Cargo.lock
 git commit -m "feat: add point-in-time option-chain snapshots"
 ```
### Task 2: Implement volatility-surface, skew, term, gamma, and downside-demand features

 **Files:**
 - Create: `crates/options/src/surface.rs`
- Create: `crates/options/src/features.rs`
- Create: `crates/options/tests/surface_features.rs`
- Create: `docs/data-dictionary/options-features.md`

 **Interfaces:**
 - Consumes: clean option-chain snapshots, forward prices, feature registry, and structural/tactical windows
 - Produces: ATM IV, term slope/curvature, 10/25-delta skew, risk reversals, butterflies, implied-realized spread, put/call measures, OI/gamma concentration, downside-demand change, and surface-quality fields

 **Implementation notes**

 Use a simple arbitrage-aware interpolation first and compare against source marks; do not introduce a complex neural surface. Surface extrapolation is prohibited for production features outside declared delta/expiry support.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/options/tests/surface_features.rs` with:

 ```text
 use options::{OptionSurface, SurfaceFeatures};

#[test]
fn reference_smile_produces_expected_twenty_five_delta_risk_reversal() {
    let surface = OptionSurface::fixture_smile().unwrap();
    let features = SurfaceFeatures::compute(&surface).unwrap();
    assert!((features.risk_reversal_25d - (-0.08)).abs() < 1e-6);
}

#[test]
fn sparse_expiry_returns_missing_skew_with_quality_reason() {
    let features = SurfaceFeatures::compute(&OptionSurface::fixture_sparse().unwrap()).unwrap();
    assert!(features.risk_reversal_25d.is_none());
    assert!(features.missingness.contains("insufficient_delta_coverage"));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p options --test surface_features`

 Expected: FAIL because surface fitting and features are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/options/src/features.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct SurfaceFeatures {
    pub atm_iv: Option<f64>,
    pub term_slope: Option<f64>,
    pub term_curvature: Option<f64>,
    pub risk_reversal_25d: Option<f64>,
    pub butterfly_25d: Option<f64>,
    pub implied_realized_spread: Option<f64>,
    pub put_call_volume_ratio: Option<f64>,
    pub put_call_open_interest_ratio: Option<f64>,
    pub gamma_concentration: Vec<(f64, f64)>,
    pub downside_put_demand_change: Option<f64>,
    pub missingness: std::collections::BTreeSet<String>,
    pub quality_score: f64,
}

impl SurfaceFeatures {
    pub fn compute(surface: &crate::OptionSurface) -> Result<Self, OptionError> {
        surface.validate()?;
        compute_version_one_features(surface)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p options --test surface_features`

 Expected: PASS against synthetic/reference smiles, sparse chains, deep ITM/OTM exclusions, expiry roll, gamma concentration, and feature lineage tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p options -p feature-engine && cargo clippy -p options --all-targets -- -D warnings`

 Expected: all options features register with exact formulas, versions, missingness, and quality requirements

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/options/src/surface.rs crates/options/src/features.rs crates/options/tests/surface_features.rs docs/data-dictionary/options-features.md
 git commit -m "feat: add options surface stress features"
 ```
### Task 3: Implement local Bitcoin Core RPC ingestion with confirmation and reorg semantics

 **Files:**
 - Create: `crates/chain-core/Cargo.toml`
- Create: `crates/chain-core/src/lib.rs`
- Create: `crates/chain-bitcoin/Cargo.toml`
- Create: `crates/chain-bitcoin/src/lib.rs`
- Create: `crates/chain-bitcoin/src/rpc.rs`
- Create: `crates/chain-bitcoin/src/sync.rs`
- Create: `fixtures/chains/bitcoin/manifest.toml`
- Test: `crates/chain-bitcoin/tests/reorg.rs`

 **Interfaces:**
 - Consumes: local-node configuration, canonical chain events, WAL/Parquet, and quality/finality contracts
 - Produces: `BitcoinSource` for blocks, transactions aggregates, mempool observations, RPC health, confirmation depth, reorg correction records, and event/as-known-at timestamps

 **Implementation notes**

 Use local RPC credentials through Keychain references or cookie-file permissions. Do not expose node credentials in config exports. Chain event time, local receive time, and confirmation-based as-known-at/finality remain separate.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/chain-bitcoin/tests/reorg.rs` with:

 ```text
 use chain_bitcoin::BitcoinSyncHarness;

#[tokio::test]
async fn reorg_marks_old_blocks_corrected_and_emits_replacement_chain() {
    let report = BitcoinSyncHarness::fixture_reorg_depth_two().run().await.unwrap();
    assert_eq!(report.corrected_blocks, 2);
    assert_eq!(report.replacement_blocks, 2);
    assert!(report.tip_finality_depth >= 0);
}

#[tokio::test]
async fn unreachable_node_is_source_unhealthy_not_empty_chain() {
    let report = BitcoinSyncHarness::fixture_unreachable().run().await.unwrap();
    assert!(report.health.is_unhealthy());
    assert_eq!(report.emitted_zero_metrics, 0);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p chain-bitcoin`

 Expected: FAIL because Bitcoin RPC/sync APIs are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/chain-bitcoin/src/lib.rs` with:

 ```text
 use async_trait::async_trait;

pub struct BitcoinSource {
    rpc: rpc::BitcoinRpcClient,
    confirmations_required: u32,
    poll_interval: std::time::Duration,
}

#[async_trait]
impl chain_core::ChainSource for BitcoinSource {
    fn chain_id(&self) -> &'static str { "bitcoin-mainnet" }
    async fn poll(&mut self, sink: &mut dyn chain_core::ChainEventSink) -> Result<chain_core::PollOutcome, chain_core::ChainError> {
        let best = self.rpc.get_best_block_hash().await?;
        self.sync_to(best, sink).await
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p chain-bitcoin`

 Expected: PASS for initial sync, incremental blocks, mempool, RPC lag, timeout, malformed result, depth-1/2 reorg, and restart checkpoint fixtures

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p chain-bitcoin && cargo clippy -p chain-bitcoin --all-targets -- -D warnings`

 Expected: Bitcoin source tests pass and provisional/final/corrected events preserve original and replacement lineage

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/chain-core crates/chain-bitcoin fixtures/chains/bitcoin Cargo.toml Cargo.lock
 git commit -m "feat: add local Bitcoin node ingestion"
 ```
### Task 4: Implement Bitcoin mempool, network, UTXO-age, realized-value, and flow features

 **Files:**
 - Create: `crates/chain-bitcoin/src/features.rs`
- Create: `crates/chain-bitcoin/src/utxo.rs`
- Create: `crates/chain-bitcoin/tests/features.rs`
- Create: `docs/data-dictionary/bitcoin-features.md`

 **Interfaces:**
 - Consumes: Bitcoin block/transaction/mempool events, optional attribution registry, and structural windows
 - Produces: transaction count/value, fee distribution, mempool pressure, UTXO/spent-output age bands, realized value/cap proxy, coin-days, miner/network metrics where available, and confidence-weighted exchange flow

 **Implementation notes**

 Any metric requiring historical UTXO state declares its node/index prerequisite and resource cost. Attribution-dependent values preserve labelled, unlabelled, ambiguous, and revised amounts separately.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/chain-bitcoin/tests/features.rs` with:

 ```text
 use chain_bitcoin::features::BitcoinFeatureEngine;

#[test]
fn utxo_age_bands_conserve_total_value() {
    let output = BitcoinFeatureEngine::fixture_utxo_ages().compute().unwrap();
    let sum: i128 = output.utxo_age_bands.iter().map(|b| b.value_sats).sum();
    assert_eq!(sum, output.total_utxo_value_sats);
}

#[test]
fn unlabeled_flow_is_not_classified_as_exchange_flow() {
    let output = BitcoinFeatureEngine::fixture_unlabeled_transfer().compute().unwrap();
    assert_eq!(output.exchange_inflow_sats, 0);
    assert!(output.unattributed_flow_sats > 0);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p chain-bitcoin --test features`

 Expected: FAIL because Bitcoin feature computation is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/chain-bitcoin/src/features.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct BitcoinFeatures {
    pub transaction_count: u64,
    pub adjusted_transfer_value_sats: i128,
    pub fee_percentiles_sats_vbyte: Vec<(u8, f64)>,
    pub mempool_vbytes: u64,
    pub utxo_age_bands: Vec<AgeBandValue>,
    pub spent_output_age_bands: Vec<AgeBandValue>,
    pub coin_days_destroyed: f64,
    pub exchange_inflow_sats: i128,
    pub exchange_outflow_sats: i128,
    pub unattributed_flow_sats: i128,
    pub attribution_coverage: f64,
    pub as_known_at_ns: i64,
    pub finality: feature_registry::FinalityState,
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p chain-bitcoin --test features`

 Expected: PASS for fee/mempool, UTXO conservation, spent ages, coin-days, realized value, attribution coverage, reorg correction, and finality tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p chain-bitcoin -p feature-engine && cargo clippy -p chain-bitcoin --all-targets -- -D warnings`

 Expected: Bitcoin features register and reproduce under chain replay

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/chain-bitcoin/src/features.rs crates/chain-bitcoin/src/utxo.rs crates/chain-bitcoin/tests/features.rs docs/data-dictionary/bitcoin-features.md
 git commit -m "feat: add Bitcoin structural features"
 ```
### Task 5: Implement Ethereum execution and consensus clients with finality/reorg tracking

 **Files:**
 - Create: `crates/chain-ethereum/Cargo.toml`
- Create: `crates/chain-ethereum/src/lib.rs`
- Create: `crates/chain-ethereum/src/execution.rs`
- Create: `crates/chain-ethereum/src/consensus.rs`
- Create: `crates/chain-ethereum/src/sync.rs`
- Create: `fixtures/chains/ethereum/manifest.toml`
- Test: `crates/chain-ethereum/tests/finality.rs`

 **Interfaces:**
 - Consumes: local execution JSON-RPC, consensus REST, chain-core, WAL/Parquet, and source quality
 - Produces: `EthereumSource` joining execution blocks/receipts/logs with consensus head/safe/finalized checkpoints, reorg corrections, client capability discovery, and archive-required query gating

 **Implementation notes**

 Do not require an archive node for current-state forward collection. Historical-state features declare archive/index needs and are unavailable when unsupported. Execution and consensus endpoints have separate health and freshness budgets.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/chain-ethereum/tests/finality.rs` with:

 ```text
 use chain_ethereum::EthereumSyncHarness;

#[tokio::test]
async fn safe_and_finalized_checkpoints_update_feature_finality() {
    let report = EthereumSyncHarness::fixture_finality_progression().run().await.unwrap();
    assert!(report.blocks.iter().any(|b| b.finality == "safe"));
    assert!(report.blocks.iter().any(|b| b.finality == "finalized"));
}

#[tokio::test]
async fn archive_only_query_fails_with_capability_reason() {
    let source = EthereumSyncHarness::fixture_full_node_without_archive();
    assert!(source.query_historical_state(1_000_000).await.unwrap_err().to_string().contains("archive_required"));
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p chain-ethereum`

 Expected: FAIL because Ethereum clients and finality model are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/chain-ethereum/src/lib.rs` with:

 ```text
 pub struct EthereumSource {
    execution: execution::ExecutionClient,
    consensus: consensus::ConsensusClient,
    capabilities: EthereumCapabilities,
}

impl EthereumSource {
    pub async fn poll(&mut self, sink: &mut dyn chain_core::ChainEventSink) -> Result<chain_core::PollOutcome, chain_core::ChainError> {
        let head = self.execution.block_number().await?;
        let checkpoints = self.consensus.finality_checkpoints().await?;
        self.sync_execution_to(head, checkpoints, sink).await
    }

    pub fn require_archive(&self) -> Result<(), chain_core::ChainError> {
        if self.capabilities.archive_state { Ok(()) } else { Err(chain_core::ChainError::Capability("archive_required".into())) }
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p chain-ethereum`

 Expected: PASS for execution/consensus sync, safe/finalized progression, reorg, client lag/disagreement, archive gating, timeout, and restart fixtures

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p chain-ethereum && cargo clippy -p chain-ethereum --all-targets -- -D warnings`

 Expected: Ethereum source tests pass and client disagreement becomes degraded quality rather than an arbitrary selected truth

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/chain-ethereum fixtures/chains/ethereum Cargo.toml Cargo.lock
 git commit -m "feat: add local Ethereum node ingestion"
 ```
### Task 6: Implement Ethereum fee/burn/staking/stablecoin/DEX/bridge features

 **Files:**
 - Create: `crates/chain-ethereum/src/features.rs`
- Create: `crates/chain-ethereum/src/stablecoins.rs`
- Create: `crates/chain-ethereum/src/staking.rs`
- Create: `crates/chain-ethereum/tests/features.rs`
- Create: `docs/data-dictionary/ethereum-features.md`

 **Interfaces:**
 - Consumes: execution/consensus events, token/protocol registry, attribution labels, and structural windows
 - Produces: gas/base/priority fee pressure, burn/issuance, active/contract activity, staking deposits/exits/queues, blob use, stablecoin mint/burn/transfers, selected DEX/bridge/protocol measures, and coverage/revision metadata

 **Implementation notes**

 Protocol/bridge/DEX coverage is a curated versioned registry, not an implicit “all DeFi” claim. Unknown contracts remain unattributed. Token decimal or proxy implementation changes create registry versions and correction records.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/chain-ethereum/tests/features.rs` with:

 ```text
 use chain_ethereum::features::EthereumFeatureEngine;

#[test]
fn stablecoin_mint_burn_uses_canonical_token_decimals() {
    let out = EthereumFeatureEngine::fixture_usdc_mint_burn().compute().unwrap();
    assert_eq!(out.stablecoin_minted["USDC"].to_string(), "1000000");
    assert_eq!(out.stablecoin_burned["USDC"].to_string(), "250000");
}

#[test]
fn unfinalized_bridge_event_remains_provisional() {
    let out = EthereumFeatureEngine::fixture_unfinalized_bridge().compute().unwrap();
    assert!(out.bridge_flow.finality.is_provisional());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p chain-ethereum --test features`

 Expected: FAIL because Ethereum features are absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/chain-ethereum/src/features.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct EthereumFeatures {
    pub gas_used: u128,
    pub base_fee_burn_wei: i128,
    pub priority_fee_wei: i128,
    pub issuance_wei: i128,
    pub blob_gas_used: u128,
    pub staking_deposits_wei: i128,
    pub staking_exits_wei: i128,
    pub activation_queue: u64,
    pub exit_queue: u64,
    pub stablecoin_minted: std::collections::BTreeMap<String, fixed_decimal::FixedDecimal>,
    pub stablecoin_burned: std::collections::BTreeMap<String, fixed_decimal::FixedDecimal>,
    pub dex_volume_usd: Option<f64>,
    pub bridge_flow: RevisionAwareFlow,
    pub coverage: FeatureCoverage,
    pub as_known_at_ns: i64,
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p chain-ethereum --test features`

 Expected: PASS for fees, burn/issuance, token decimals, staking, blob, DEX, bridge, protocol registry, finality, and correction tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p chain-ethereum -p feature-engine && cargo clippy -p chain-ethereum --all-targets -- -D warnings`

 Expected: Ethereum features register and reproduce with source/protocol coverage and revision metadata

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/chain-ethereum/src/features.rs crates/chain-ethereum/src/stablecoins.rs crates/chain-ethereum/src/staking.rs crates/chain-ethereum/tests/features.rs docs/data-dictionary/ethereum-features.md
 git commit -m "feat: add Ethereum and stablecoin structural features"
 ```
### Task 7: Create the point-in-time attribution and protocol-label registry

 **Files:**
 - Create: `crates/attribution/Cargo.toml`
- Create: `crates/attribution/src/lib.rs`
- Create: `crates/attribution/src/label.rs`
- Create: `crates/attribution/src/revision.rs`
- Create: `models/schemas/attribution-label.schema.json`
- Test: `crates/attribution/tests/as_known_at.rs`

 **Interfaces:**
 - Consumes: asset/chain identities, chain observations, metadata store, and external/local label imports
 - Produces: `AttributionRegistry` with provider/version/confidence/valid-time/as-known-at fields, ambiguous labels, revisions, rollback, coverage metrics, and point-in-time lookup

 **Implementation notes**

 The registry is local and supports user-supplied imports with explicit license manifests. An address label is derived evidence, not chain truth. Every derived flow records the exact label-set hash.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/attribution/tests/as_known_at.rs` with:

 ```text
 use attribution::AttributionRegistry;

#[test]
fn future_label_revision_is_not_visible_to_historical_query() {
    let registry = AttributionRegistry::fixture_with_revision();
    let old = registry.lookup("address-1", 100, 150).unwrap();
    let new = registry.lookup("address-1", 100, 250).unwrap();
    assert_eq!(old.label, "unknown");
    assert_eq!(new.label, "exchange");
}

#[test]
fn conflicting_high_confidence_labels_are_ambiguous_not_arbitrarily_resolved() {
    let registry = AttributionRegistry::fixture_conflict();
    assert!(registry.lookup("address-2", 100, 200).unwrap().is_ambiguous());
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p attribution`

 Expected: FAIL because attribution registry is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/attribution/src/label.rs` with:

 ```text
 #[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AttributionLabel {
    pub namespace: String,
    pub subject: String,
    pub label: String,
    pub provider: String,
    pub provider_version: String,
    pub confidence: f64,
    pub valid_from_ns: i64,
    pub valid_to_ns: Option<i64>,
    pub known_from_ns: i64,
    pub supersedes_id: Option<String>,
    pub evidence_hash: [u8; 32],
}

impl AttributionLabel {
    pub fn visible_at(&self, event_time_ns: i64, as_known_at_ns: i64) -> bool {
        self.known_from_ns <= as_known_at_ns
            && self.valid_from_ns <= event_time_ns
            && self.valid_to_ns.map_or(true, |end| event_time_ns < end)
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p attribution`

 Expected: PASS for future revision, ambiguity, confidence, effective windows, provider version, rollback, and coverage tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p attribution && cargo run -p xtask -- validate-model-schemas`

 Expected: attribution tests and schema validation pass; historical lookup never sees a later-known label

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/attribution models/schemas/attribution-label.schema.json Cargo.toml Cargo.lock
 git commit -m "feat: add versioned attribution registry"
 ```
### Task 8: Implement the BTC–ETH coupled cusp mathematical and inference engine

 **Files:**
 - Create: `crates/coupled-cusp/Cargo.toml`
- Create: `crates/coupled-cusp/src/lib.rs`
- Create: `crates/coupled-cusp/src/potential.rs`
- Create: `crates/coupled-cusp/src/equilibrium.rs`
- Create: `crates/coupled-cusp/src/fit.rs`
- Create: `crates/coupled-cusp/src/online.rs`
- Test: `crates/coupled-cusp/tests/reference.rs`

 **Interfaces:**
 - Consumes: univariate cusp control maps/states, aligned BTC/ETH structural features, numerical primitives, and walk-forward datasets
 - Produces: `CoupledCuspModel` with coupling lambda, joint equilibrium solver, Hessian/eigenvalues, joint instability probability, directional sensitivities, source uncertainty, fit diagnostics, and experimental gate

 **Implementation notes**

 Limit v1 to BTC/ETH. Use multiple starting points and report convergence/equilibrium multiplicity rather than silently returning the first local solution. Joint instability is not a contagion probability until calibrated by downstream evaluation.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/coupled-cusp/tests/reference.rs` with:

 ```text
 use coupled_cusp::{CoupledControls, CoupledPotential};

#[test]
fn analytic_hessian_matches_finite_difference() {
    let c = CoupledControls::fixture();
    let y = [0.5, -0.25];
    let analytic = CoupledPotential::hessian(y, c);
    let numeric = coupled_cusp::test_support::finite_difference_hessian(y, c);
    assert!((analytic - numeric).amax() < 1e-6);
}

#[test]
fn zero_coupling_reduces_to_independent_univariate_hessians() {
    let mut c = CoupledControls::fixture();
    c.lambda = 0.0;
    let h = CoupledPotential::hessian([1.0, 2.0], c);
    assert_eq!(h[(0,1)], 0.0);
    assert_eq!(h[(1,0)], 0.0);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p coupled-cusp`

 Expected: FAIL because coupled cusp math/inference is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/coupled-cusp/src/potential.rs` with:

 ```text
 #[derive(Clone, Copy, Debug)]
pub struct CoupledControls {
    pub btc: cusp::Controls,
    pub eth: cusp::Controls,
    pub lambda: f64,
}

pub struct CoupledPotential;

impl CoupledPotential {
    pub fn value(y: [f64; 2], c: CoupledControls) -> f64 {
        cusp::Potential::value(y[0], c.btc) + cusp::Potential::value(y[1], c.eth) - c.lambda * y[0] * y[1]
    }
    pub fn hessian(y: [f64; 2], c: CoupledControls) -> nalgebra::Matrix2<f64> {
        nalgebra::Matrix2::new(
            cusp::Potential::hessian(y[0], c.btc), -c.lambda,
            -c.lambda, cusp::Potential::hessian(y[1], c.eth),
        )
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p coupled-cusp`

 Expected: PASS for zero/nonzero coupling, equilibrium residuals, Hessian derivatives/eigenvalues, synthetic lambda recovery, uncertainty, deterministic online inference, and gate decisions

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p coupled-cusp && cargo clippy -p coupled-cusp --all-targets -- -D warnings`

 Expected: coupled model tests pass and status defaults to experimental pending a separate ablation/shadow gate

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/coupled-cusp Cargo.toml Cargo.lock
 git commit -m "feat: add experimental BTC ETH coupled cusp"
 ```
### Task 9: Implement stablecoin dislocation and cross-asset contagion labels/features

 **Files:**
 - Create: `crates/contagion/Cargo.toml`
- Create: `crates/contagion/src/lib.rs`
- Create: `crates/contagion/src/stablecoin.rs`
- Create: `crates/contagion/src/network.rs`
- Create: `crates/contagion/src/labels.rs`
- Test: `crates/contagion/tests/reference.rs`

 **Interfaces:**
 - Consumes: stablecoin-adjusted multi-venue prices, chain flows, coupled cusp, cross-asset returns/liquidity, and source health
 - Produces: stablecoin deviation/persistence labels, propagation source/time/affected assets, dynamic correlation/partial-correlation network, systemic-vs-idiosyncratic decomposition, and contagion evidence

 **Implementation notes**

 A depeg label requires a robust consolidated deviation across enough healthy venues for a minimum duration. Contagion source is probabilistic and controls for a common market factor; it is not a causal assertion.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/contagion/tests/reference.rs` with:

 ```text
 use contagion::{StablecoinLabeler, ContagionAnalyzer};

#[test]
fn single_unhealthy_venue_quote_does_not_define_depeg() {
    let outcome = StablecoinLabeler::fixture().label(&contagion::fixtures::one_bad_venue()).unwrap();
    assert!(!outcome.occurred);
}

#[test]
fn propagation_source_precedes_affected_assets_in_fixture() {
    let result = ContagionAnalyzer::fixture().analyze(&contagion::fixtures::btc_to_eth_sol()).unwrap();
    assert_eq!(result.likely_source, "BTC");
    assert!(result.propagation_delays_ns["ETH"] > 0);
    assert!(result.propagation_delays_ns["SOL"] > 0);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p contagion`

 Expected: FAIL because stablecoin/contagion logic is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/contagion/src/lib.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct ContagionState {
    pub as_known_at_ns: i64,
    pub likely_source: String,
    pub source_probability: f64,
    pub propagation_delays_ns: std::collections::BTreeMap<String, i64>,
    pub affected_assets: Vec<String>,
    pub systemic_component: f64,
    pub idiosyncratic_components: std::collections::BTreeMap<String, f64>,
    pub minimum_coupled_hessian_eigenvalue: Option<f64>,
    pub quality_score: f64,
    pub evidence_hash: [u8; 32],
}

impl ContagionState {
    pub fn validate(&self) -> Result<(), ContagionError> {
        if !(0.0..=1.0).contains(&self.source_probability) || !(0.0..=1.0).contains(&self.quality_score) { return Err(ContagionError::InvalidProbability); }
        Ok(())
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p contagion`

 Expected: PASS for venue-local depeg rejection, multi-venue persistence, source ordering, common-factor controls, missing assets, source uncertainty, and label overlap/censoring tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo test -p contagion -p labels && cargo clippy -p contagion --all-targets -- -D warnings`

 Expected: stablecoin/contagion outputs reproduce and remain experimental when event counts are insufficient

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/contagion Cargo.toml Cargo.lock
 git commit -m "feat: add stablecoin and contagion research outputs"
 ```
### Task 10: Integrate slow-source revisions, capacity isolation, evaluation, and Phase 5 gates

 **Files:**
 - Create: `crates/slow-sources/Cargo.toml`
- Create: `crates/slow-sources/src/lib.rs`
- Create: `crates/slow-sources/src/revision.rs`
- Create: `crates/slow-sources/src/resource_budget.rs`
- Modify: `crates/system-tests/Cargo.toml`
- Create: `crates/system-tests/tests/slow_sources.rs`
- Create: `fixtures/golden-replays/slow-sources-v1/manifest.toml`
- Modify: `apps/crypto-evaluate/src/main.rs`
- Test: `crates/slow-sources/tests/publication_lag.rs`

 **Interfaces:**
 - Consumes: options, Bitcoin, Ethereum, attribution, coupled cusp, contagion, feature engine, datasets, and model registry
 - Produces: publication-lag/revision scheduler, CPU/memory/IO budgets, low-priority backpressure, golden replay, incremental-value reports, and production/experimental gate decisions for slow-source and coupled outputs

 **Implementation notes**

 Slow data never blocks exchange capture or core inference. Every slow feature records publication lag and correction policy. Coupled cusp, stablecoin, and contagion remain experimental unless their own minimum-event and calibration gates pass.

 - [ ] **Step 1: Write the failing test**

 Create or replace `crates/slow-sources/tests/publication_lag.rs` with:

 ```text
 use slow_sources::{RevisionScheduler, SlowObservation};

#[test]
fn revised_value_is_invisible_before_revision_known_time() {
    let scheduler = RevisionScheduler::fixture();
    let old = scheduler.as_known_at("metric-1", 150).unwrap();
    let revised = scheduler.as_known_at("metric-1", 250).unwrap();
    assert_eq!(old.value, 10.0);
    assert_eq!(revised.value, 12.0);
}

#[tokio::test]
async fn slow_compaction_cannot_starve_book_or_risk_queues() {
    let report = slow_sources::ResourceHarness::fixture_saturated().run().await.unwrap();
    assert_eq!(report.book_delta_drops, 0);
    assert_eq!(report.risk_deadline_misses, 0);
    assert!(report.slow_jobs_paused > 0);
}
 ```

 - [ ] **Step 2: Run the focused test and confirm the intended failure**

 Run: `cargo test -p slow-sources`

 Expected: FAIL because revision/capacity scheduler is absent

 - [ ] **Step 3: Add the smallest complete implementation that satisfies the contract**

 Create or update `crates/slow-sources/src/resource_budget.rs` with:

 ```text
 #[derive(Clone, Debug)]
pub struct ResourceBudget {
    pub max_concurrent_chain_queries: usize,
    pub max_concurrent_option_surface_fits: usize,
    pub max_memory_bytes: u64,
    pub pause_when_fast_queue_above_fraction: f64,
}

impl ResourceBudget {
    pub fn permit(&self, state: &RuntimeLoad) -> PermitDecision {
        if state.fast_queue_fraction >= self.pause_when_fast_queue_above_fraction || state.memory_bytes >= self.max_memory_bytes {
            PermitDecision::PauseLowPriority
        } else {
            PermitDecision::Run
        }
    }
}
 ```

 - [ ] **Step 4: Run the focused test and confirm success**

 Run: `cargo test -p slow-sources`

 Expected: PASS for revision visibility, provisional/final/corrected states, resource preemption, node outage, surface backlog, and deterministic replay tests

 - [ ] **Step 5: Run the subsystem verification command**

 Run: `cargo run -p crypto-replay -- --manifest fixtures/golden-replays/slow-sources-v1/manifest.toml --verify && cargo run -p crypto-evaluate -- slow-source-ablation --output target/evaluation/slow-sources && cargo test -p system-tests --test slow_sources --release -- --ignored`

 Expected: replay and evaluation reports pass; fast-path SLOs remain within gate; each added feature/model is marked production-eligible or experimental from predeclared evidence

 - [ ] **Step 6: Inspect source lag, revision, attribution, and resource-isolation behavior**

 Run: `git diff --check && git status --short`

 Expected: no current label is silently applied to historical data; no provisional chain observation is represented as final; no coupled result is promoted without its own gate.

 - [ ] **Step 7: Commit the independently testable deliverable**

 ```bash
 git add crates/slow-sources crates/system-tests/Cargo.toml crates/system-tests/tests/slow_sources.rs fixtures/golden-replays/slow-sources-v1 apps/crypto-evaluate/src/main.rs Cargo.toml Cargo.lock
 git commit -m "feat: integrate slow sources and coupled research gates"
 ```
