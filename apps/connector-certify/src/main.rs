use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    process::ExitCode,
};

use clap::Parser;
use connector_binance::{
    BinanceInput, BinanceMarket, BinanceMessage, NativeParseError, binance_capabilities,
    parse_depth_snapshot, parse_exchange_info_report, parse_native_message,
};
use connector_bybit::{
    BybitBookSynchronizer, BybitInput, BybitMarket, BybitMessage,
    NativeParseError as BybitParseError, capabilities as bybit_capabilities,
    parse_instruments_info as parse_bybit_instruments_info,
    parse_native_message as parse_bybit_message,
};
use connector_core::{Completeness, StreamClass, TradeSemantics};
use domain::{InstrumentId, ProductType, VenueId};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{ApplyResult, BookConfig, BookSession, ChecksumPolicy, SequencePolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_DOCUMENTATION_REVIEW_BYTES: u64 = 256 * 1024;
const MAX_FIXTURE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(about = "Deterministically certify retained connector fixtures")]
struct Arguments {
    #[arg(long)]
    venue: String,
    #[arg(long)]
    fixtures: PathBuf,
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureManifest {
    schema_version: u32,
    artifact_id: String,
    status: String,
    certification_status: String,
    local_only: bool,
    fixture_origin: String,
    captured_exchange_traffic: bool,
    contains_credentials: bool,
    redistribution: String,
    reviewed_on: String,
    source: Vec<ManifestSource>,
    fixture: Vec<ManifestFixture>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestSource {
    id: String,
    url: String,
    scope: String,
    snapshot: String,
    snapshot_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFixture {
    path: String,
    sha256: String,
    market: String,
    route: String,
    record_type: String,
}

#[derive(Debug, Serialize)]
struct CertificationReport {
    schema_version: u32,
    venue: String,
    fixture_status: String,
    production_certification_status: String,
    evidence_scope: String,
    live_exchange_acceptance: bool,
    manifest_sha256: String,
    connector_version: String,
    trade_semantics: String,
    liquidation_completeness: String,
    fixture_count: usize,
    fixtures: BTreeMap<String, String>,
    checks: Vec<CertificationCheck>,
    production_gates: Vec<CertificationGate>,
    limitations: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CertificationCheck {
    name: String,
    status: String,
}

#[derive(Debug, Serialize)]
struct CertificationGate {
    number: u8,
    name: String,
    status: String,
    evidence: String,
}

fn main() -> ExitCode {
    match run(Arguments::parse()) {
        Ok(report_hash) => {
            println!(
                "connector-certify: FIXTURE_PASS production_certification=BLOCKED sha256={report_hash}"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("connector-certify: FAIL: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Arguments) -> Result<String, String> {
    match arguments.venue.as_str() {
        "binance" => run_binance(arguments),
        "bybit" => run_bybit(arguments),
        _ => Err("supported venues are binance and bybit".to_owned()),
    }
}

fn run_binance(arguments: Arguments) -> Result<String, String> {
    let workspace = workspace_root(&arguments.fixtures)?;
    let manifest_path = arguments.fixtures.join("manifest.toml");
    let manifest_bytes = read_regular_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: FixtureManifest = toml::from_str(
        std::str::from_utf8(&manifest_bytes)
            .map_err(|_| "fixture manifest is not UTF-8".to_owned())?,
    )
    .map_err(|error| format!("fixture manifest is invalid: {error}"))?;
    validate_manifest_metadata(&manifest, &workspace)?;
    validate_manifest_coverage(&manifest)?;
    let mut fixture_hashes = BTreeMap::new();
    for fixture in &manifest.fixture {
        validate_safe_fixture_path(&fixture.path)?;
        let path = workspace.join(&fixture.path);
        let raw = read_regular_bounded(&path, MAX_FIXTURE_BYTES)?;
        let actual_hash = sha256_hex(&raw);
        if actual_hash != fixture.sha256 {
            return Err(format!(
                "fixture hash mismatch for {}: expected {}, got {actual_hash}",
                fixture.path, fixture.sha256
            ));
        }
        validate_fixture(fixture, &raw)?;
        let reordered = serde_json::to_vec(
            &serde_json::from_slice::<serde_json::Value>(&raw)
                .map_err(|error| format!("fixture JSON is invalid: {error}"))?,
        )
        .map_err(|error| format!("could not reorder fixture JSON: {error}"))?;
        validate_fixture(fixture, &reordered)?;
        if fixture_hashes
            .insert(fixture.path.clone(), actual_hash)
            .is_some()
        {
            return Err(format!("duplicate fixture path: {}", fixture.path));
        }
    }

    validate_injected_failures(&workspace)?;
    let capabilities =
        binance_capabilities().map_err(|error| format!("capabilities invalid: {error}"))?;
    if capabilities.trade_semantics() != TradeSemantics::Aggregate {
        return Err("trade capability does not match the retained parser".to_owned());
    }
    let liquidation = capabilities.completeness().get(StreamClass::Liquidations);
    if liquidation
        != (Completeness::SampledLatestPerSymbolWindow {
            window_ms: std::num::NonZeroU32::new(1_000).expect("constant nonzero"),
        })
    {
        return Err("liquidation completeness is overstated".to_owned());
    }

    let report = CertificationReport {
        schema_version: 1,
        venue: "binance".to_owned(),
        fixture_status: "pass".to_owned(),
        production_certification_status: "blocked".to_owned(),
        evidence_scope: "retained-fixture".to_owned(),
        live_exchange_acceptance: false,
        manifest_sha256: sha256_hex(&manifest_bytes),
        connector_version: capabilities.connector_version().to_owned(),
        trade_semantics: "aggregate".to_owned(),
        liquidation_completeness: "sampled_latest_per_symbol_window_1000ms".to_owned(),
        fixture_count: fixture_hashes.len(),
        fixtures: fixture_hashes,
        checks: [
            "fixture_hashes",
            "all_supported_record_types",
            "spot_and_usdm_depth_snapshots",
            "active_exchange_info_filters",
            "field_reordering",
            "schema_drift_rejection",
            "payload_size_rejection",
            "truthful_capabilities",
        ]
        .into_iter()
        .map(|name| CertificationCheck {
            name: name.to_owned(),
            status: "pass".to_owned(),
        })
        .collect(),
        production_gates: production_gates(),
        limitations: vec![
            "No authenticated endpoints, account data, order placement, or trading".to_owned(),
            "This artifact validates retained parser fixtures, not the full specification-level connector certification matrix".to_owned(),
            "Production transport, rate limiting, heartbeat supervision, kill/restart recovery, and operational canary remain blocked or unrun".to_owned(),
            "Raw-WAL replay equivalence has focused test evidence but is not executed or commit-bound by this report runner".to_owned(),
        ],
    };
    let mut encoded = serde_json::to_vec_pretty(&report)
        .map_err(|error| format!("could not encode report: {error}"))?;
    encoded.push(b'\n');
    let report_hash = sha256_hex(&encoded);
    let report_path = arguments.fixtures.join("certification.json");
    let hash_path = arguments.fixtures.join("certification.sha256");
    let hash_bytes = format!("{report_hash}  certification.json\n").into_bytes();
    if arguments.check {
        compare_exact(&report_path, &encoded)?;
        compare_exact(&hash_path, &hash_bytes)?;
    } else {
        fs::write(&report_path, encoded)
            .map_err(|error| format!("could not write {}: {error}", report_path.display()))?;
        fs::write(&hash_path, hash_bytes)
            .map_err(|error| format!("could not write {}: {error}", hash_path.display()))?;
    }
    Ok(report_hash)
}

fn run_bybit(arguments: Arguments) -> Result<String, String> {
    let workspace = workspace_root_for(&arguments.fixtures, "bybit")?;
    let manifest_path = arguments.fixtures.join("manifest.toml");
    let manifest_bytes = read_regular_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: FixtureManifest = toml::from_str(
        std::str::from_utf8(&manifest_bytes)
            .map_err(|_| "fixture manifest is not UTF-8".to_owned())?,
    )
    .map_err(|error| format!("fixture manifest is invalid: {error}"))?;
    validate_bybit_manifest_metadata(&manifest, &workspace)?;
    validate_bybit_manifest_coverage(&manifest)?;
    let mut fixture_hashes = BTreeMap::new();
    for fixture in &manifest.fixture {
        validate_bybit_safe_fixture_path(&fixture.path)?;
        let path = workspace.join(&fixture.path);
        let raw = read_regular_bounded(&path, MAX_FIXTURE_BYTES)?;
        let actual_hash = sha256_hex(&raw);
        if actual_hash != fixture.sha256 {
            return Err(format!(
                "fixture hash mismatch for {}: expected {}, got {actual_hash}",
                fixture.path, fixture.sha256
            ));
        }
        validate_bybit_fixture(fixture, &raw)?;
        let reordered = serde_json::to_vec(
            &serde_json::from_slice::<serde_json::Value>(&raw)
                .map_err(|error| format!("fixture JSON is invalid: {error}"))?,
        )
        .map_err(|error| format!("could not reorder fixture JSON: {error}"))?;
        validate_bybit_fixture(fixture, &reordered)?;
        if fixture_hashes
            .insert(fixture.path.clone(), actual_hash)
            .is_some()
        {
            return Err(format!("duplicate fixture path: {}", fixture.path));
        }
    }

    validate_bybit_injected_failures(&workspace)?;
    validate_bybit_snapshot_reset(&workspace)?;
    let capabilities =
        bybit_capabilities().map_err(|error| format!("capabilities invalid: {error}"))?;
    if capabilities.trade_semantics() != TradeSemantics::Individual {
        return Err("trade capability does not match the retained parser".to_owned());
    }
    let liquidation = capabilities.completeness().get(StreamClass::Liquidations);
    if liquidation
        != (Completeness::VenueReportedAll {
            push_cadence_ms: std::num::NonZeroU32::new(500).expect("constant nonzero"),
            delivery_uncertainty: true,
        })
    {
        return Err("liquidation completeness is overstated".to_owned());
    }

    let report = CertificationReport {
        schema_version: 1,
        venue: "bybit".to_owned(),
        fixture_status: "pass".to_owned(),
        production_certification_status: "blocked".to_owned(),
        evidence_scope: "retained-fixture".to_owned(),
        live_exchange_acceptance: false,
        manifest_sha256: sha256_hex(&manifest_bytes),
        connector_version: capabilities.connector_version().to_owned(),
        trade_semantics: "individual".to_owned(),
        liquidation_completeness:
            "venue_reported_all_500ms_delivery_uncertainty_true".to_owned(),
        fixture_count: fixture_hashes.len(),
        fixtures: fixture_hashes,
        checks: [
            "fixture_hashes",
            "all_supported_record_types",
            "spot_and_linear_instrument_filters",
            "field_reordering",
            "schema_drift_rejection",
            "payload_size_rejection",
            "snapshot_replacement",
            "delta_before_snapshot_suppression",
            "sequence_gap_suppression",
            "truthful_capabilities",
        ]
        .into_iter()
        .map(|name| CertificationCheck {
            name: name.to_owned(),
            status: "pass".to_owned(),
        })
        .collect(),
        production_gates: bybit_production_gates(),
        limitations: vec![
            "No authenticated endpoints, account data, order placement, or trading".to_owned(),
            "This artifact validates retained parser and book fixtures, not live exchange acceptance".to_owned(),
            "Production transport, operational rate enforcement, supervised reconnect, and canary evidence remain blocked or unrun".to_owned(),
            "Raw-WAL replay equivalence is proven by focused connector tests but is not executed or commit-bound by this report runner".to_owned(),
        ],
    };
    let mut encoded = serde_json::to_vec_pretty(&report)
        .map_err(|error| format!("could not encode report: {error}"))?;
    encoded.push(b'\n');
    let report_hash = sha256_hex(&encoded);
    let report_path = arguments.fixtures.join("certification.json");
    let hash_path = arguments.fixtures.join("certification.sha256");
    let hash_bytes = format!("{report_hash}  certification.json\n").into_bytes();
    if arguments.check {
        compare_exact(&report_path, &encoded)?;
        compare_exact(&hash_path, &hash_bytes)?;
    } else {
        fs::write(&report_path, encoded)
            .map_err(|error| format!("could not write {}: {error}", report_path.display()))?;
        fs::write(&hash_path, hash_bytes)
            .map_err(|error| format!("could not write {}: {error}", hash_path.display()))?;
    }
    Ok(report_hash)
}

fn validate_manifest_metadata(manifest: &FixtureManifest, workspace: &Path) -> Result<(), String> {
    if manifest.schema_version != 1
        || manifest.artifact_id != "fixtures/exchanges/binance/manifest.toml"
        || manifest.status != "tracked"
        || manifest.certification_status != "fixture_parser_pass"
        || !manifest.local_only
        || manifest.captured_exchange_traffic
        || manifest.contains_credentials
        || manifest.redistribution != "MIT OR Apache-2.0"
        || manifest.fixture_origin.is_empty()
        || manifest.reviewed_on.is_empty()
        || manifest.source.is_empty()
        || manifest.fixture.is_empty()
    {
        return Err("fixture manifest metadata is not certification-ready".to_owned());
    }
    for source in &manifest.source {
        if source.id.is_empty()
            || !source.url.starts_with("https://")
            || source.scope.is_empty()
            || validate_safe_fixture_path(&source.snapshot).is_err()
            || source.snapshot_sha256.len() != 64
            || !source
                .snapshot_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("fixture documentation source is invalid".to_owned());
        }
        let snapshot_path = workspace.join(&source.snapshot);
        let snapshot = read_regular_bounded(&snapshot_path, MAX_DOCUMENTATION_REVIEW_BYTES)?;
        if sha256_hex(&snapshot) != source.snapshot_sha256 {
            return Err(format!(
                "documentation snapshot hash mismatch: {}",
                source.snapshot
            ));
        }
    }
    Ok(())
}

fn validate_bybit_manifest_metadata(
    manifest: &FixtureManifest,
    workspace: &Path,
) -> Result<(), String> {
    if manifest.schema_version != 1
        || manifest.artifact_id != "fixtures/exchanges/bybit/manifest.toml"
        || manifest.status != "tracked"
        || manifest.certification_status != "fixture_parser_pass"
        || !manifest.local_only
        || manifest.captured_exchange_traffic
        || manifest.contains_credentials
        || manifest.redistribution != "MIT OR Apache-2.0"
        || manifest.fixture_origin.is_empty()
        || manifest.reviewed_on.is_empty()
        || manifest.source.is_empty()
        || manifest.fixture.is_empty()
    {
        return Err("fixture manifest metadata is not certification-ready".to_owned());
    }
    for source in &manifest.source {
        if source.id.is_empty()
            || !source.url.starts_with("https://bybit-exchange.github.io/")
            || source.scope.is_empty()
            || validate_bybit_safe_fixture_path(&source.snapshot).is_err()
            || source.snapshot_sha256.len() != 64
            || !source
                .snapshot_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("fixture documentation source is invalid".to_owned());
        }
        let snapshot_path = workspace.join(&source.snapshot);
        let snapshot = read_regular_bounded(&snapshot_path, MAX_DOCUMENTATION_REVIEW_BYTES)?;
        if sha256_hex(&snapshot) != source.snapshot_sha256 {
            return Err(format!(
                "documentation snapshot hash mismatch: {}",
                source.snapshot
            ));
        }
    }
    Ok(())
}

fn validate_manifest_coverage(manifest: &FixtureManifest) -> Result<(), String> {
    let actual = manifest
        .fixture
        .iter()
        .map(|fixture| {
            (
                fixture.market.as_str(),
                fixture.route.as_str(),
                fixture.record_type.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        ("all", "rest", "venue_status"),
        ("spot", "rest", "depth_snapshot"),
        ("spot", "rest", "exchange_info"),
        ("spot", "websocket", "aggregate_trade"),
        ("spot", "websocket", "book_ticker"),
        ("spot", "websocket", "depth_update"),
        ("usd_m", "market_websocket", "aggregate_trade"),
        ("usd_m", "market_websocket", "mark_index_funding"),
        ("usd_m", "market_websocket", "sampled_liquidation"),
        ("usd_m", "public_websocket", "depth_update"),
        ("usd_m", "rest", "depth_snapshot"),
        ("usd_m", "rest", "exchange_info"),
        ("usd_m", "rest", "open_interest"),
    ]);
    if actual != expected || manifest.fixture.len() != expected.len() {
        return Err(format!(
            "fixture coverage must be exact: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
}

fn validate_bybit_manifest_coverage(manifest: &FixtureManifest) -> Result<(), String> {
    let actual = manifest
        .fixture
        .iter()
        .map(|fixture| {
            (
                fixture.market.as_str(),
                fixture.route.as_str(),
                fixture.record_type.as_str(),
            )
        })
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        ("linear_perpetual", "public_websocket", "all_liquidation"),
        ("linear_perpetual", "public_websocket", "orderbook_delta"),
        ("linear_perpetual", "public_websocket", "orderbook_snapshot"),
        ("linear_perpetual", "public_websocket", "public_trade"),
        ("linear_perpetual", "public_websocket", "ticker"),
        ("linear_perpetual", "rest", "instruments_info"),
        ("spot", "public_websocket", "orderbook_delta"),
        (
            "spot",
            "public_websocket",
            "orderbook_service_reset_snapshot",
        ),
        ("spot", "public_websocket", "orderbook_snapshot"),
        ("spot", "public_websocket", "public_trade"),
        ("spot", "rest", "instruments_info"),
    ]);
    if actual != expected || manifest.fixture.len() != expected.len() {
        return Err(format!(
            "fixture coverage must be exact: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
}

fn validate_safe_fixture_path(path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        || !path.starts_with("fixtures/exchanges/binance")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe fixture path: {}", path.display()));
    }
    Ok(())
}

fn validate_bybit_safe_fixture_path(path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        || !path.starts_with("fixtures/exchanges/bybit")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe fixture path: {}", path.display()));
    }
    Ok(())
}

fn validate_fixture(fixture: &ManifestFixture, raw: &[u8]) -> Result<(), String> {
    let result = match (
        fixture.market.as_str(),
        fixture.route.as_str(),
        fixture.record_type.as_str(),
    ) {
        ("spot", "websocket", "aggregate_trade") => {
            require_message(BinanceInput::SpotWebSocket, raw, "aggregate_trade")
        }
        ("spot", "websocket", "book_ticker") => {
            require_message(BinanceInput::SpotWebSocket, raw, "book_ticker")
        }
        ("spot", "websocket", "depth_update") => {
            require_message(BinanceInput::SpotWebSocket, raw, "depth_update")
        }
        ("usd_m", "market_websocket", "aggregate_trade") => {
            require_message(BinanceInput::UsdMMarketWebSocket, raw, "aggregate_trade")
        }
        ("usd_m", "public_websocket", "depth_update") => {
            require_message(BinanceInput::UsdMPublicWebSocket, raw, "depth_update")
        }
        ("usd_m", "market_websocket", "mark_index_funding") => {
            require_message(BinanceInput::UsdMMarketWebSocket, raw, "mark_index_funding")
        }
        ("usd_m", "market_websocket", "sampled_liquidation") => require_message(
            BinanceInput::UsdMMarketWebSocket,
            raw,
            "sampled_liquidation",
        ),
        ("usd_m", "rest", "open_interest") => {
            require_message(BinanceInput::UsdMOpenInterestRest, raw, "open_interest")
        }
        ("all", "rest", "venue_status") => {
            require_message(BinanceInput::SystemStatusRest, raw, "venue_status")
        }
        ("spot", "rest", "depth_snapshot") => {
            parse_depth_snapshot(BinanceMarket::Spot, "BTCUSDT", raw)
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
        ("usd_m", "rest", "depth_snapshot") => {
            parse_depth_snapshot(BinanceMarket::UsdMarginedPerpetual, "BTCUSDT", raw)
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
        ("spot", "rest", "exchange_info") => require_exchange_info(BinanceMarket::Spot, raw),
        ("usd_m", "rest", "exchange_info") => {
            require_exchange_info(BinanceMarket::UsdMarginedPerpetual, raw)
        }
        other => return Err(format!("unsupported fixture routing tuple: {other:?}")),
    };
    result.map_err(|error| {
        format!(
            "fixture {} failed {} parser on route {}: {error}",
            fixture.path, fixture.record_type, fixture.route
        )
    })
}

fn validate_bybit_fixture(fixture: &ManifestFixture, raw: &[u8]) -> Result<(), String> {
    let result = match (
        fixture.market.as_str(),
        fixture.route.as_str(),
        fixture.record_type.as_str(),
    ) {
        ("spot", "public_websocket", "orderbook_snapshot") => {
            require_bybit_message(BybitMarket::Spot, raw, "orderbook_snapshot")
        }
        ("spot", "public_websocket", "orderbook_service_reset_snapshot") => {
            require_bybit_message(BybitMarket::Spot, raw, "orderbook_reset")
        }
        ("spot", "public_websocket", "orderbook_delta") => {
            require_bybit_message(BybitMarket::Spot, raw, "orderbook_delta")
        }
        ("spot", "public_websocket", "public_trade") => {
            require_bybit_message(BybitMarket::Spot, raw, "public_trade")
        }
        ("linear_perpetual", "public_websocket", "orderbook_snapshot") => {
            require_bybit_message(BybitMarket::LinearPerpetual, raw, "orderbook_snapshot")
        }
        ("linear_perpetual", "public_websocket", "orderbook_delta") => {
            require_bybit_message(BybitMarket::LinearPerpetual, raw, "orderbook_delta")
        }
        ("linear_perpetual", "public_websocket", "public_trade") => {
            require_bybit_message(BybitMarket::LinearPerpetual, raw, "public_trade")
        }
        ("linear_perpetual", "public_websocket", "ticker") => {
            require_bybit_message(BybitMarket::LinearPerpetual, raw, "ticker")
        }
        ("linear_perpetual", "public_websocket", "all_liquidation") => {
            require_bybit_message(BybitMarket::LinearPerpetual, raw, "all_liquidation")
        }
        ("spot", "rest", "instruments_info") => require_bybit_instruments(BybitMarket::Spot, raw),
        ("linear_perpetual", "rest", "instruments_info") => {
            require_bybit_instruments(BybitMarket::LinearPerpetual, raw)
        }
        other => return Err(format!("unsupported fixture routing tuple: {other:?}")),
    };
    result.map_err(|error| {
        format!(
            "fixture {} failed {} parser on route {}: {error}",
            fixture.path, fixture.record_type, fixture.route
        )
    })
}

fn require_bybit_message(market: BybitMarket, raw: &[u8], expected: &str) -> Result<(), String> {
    let message = parse_bybit_message(BybitInput::PublicWebSocket(market), raw)
        .map_err(|error| error.to_string())?;
    let actual = match message {
        BybitMessage::OrderBook(value)
            if value.kind == connector_bybit::BookMessageKind::Snapshot && value.update_id == 1 =>
        {
            "orderbook_reset"
        }
        BybitMessage::OrderBook(value)
            if value.kind == connector_bybit::BookMessageKind::Snapshot =>
        {
            "orderbook_snapshot"
        }
        BybitMessage::OrderBook(_) => "orderbook_delta",
        BybitMessage::PublicTrades(_) => "public_trade",
        BybitMessage::LinearTicker(_) => "ticker",
        BybitMessage::AllLiquidations(_) => "all_liquidation",
    };
    if actual != expected {
        return Err(format!("expected {expected}, parsed {actual}"));
    }
    Ok(())
}

fn require_bybit_instruments(market: BybitMarket, raw: &[u8]) -> Result<(), String> {
    let report = parse_bybit_instruments_info(market, raw).map_err(|error| error.to_string())?;
    if report.instruments.len() != 1
        || report.instruments[0].lifecycle != connector_bybit::BybitInstrumentLifecycle::Active
    {
        return Err(
            "retained instruments-info fixture does not contain one active instrument".to_owned(),
        );
    }
    Ok(())
}

fn require_message(input: BinanceInput, raw: &[u8], expected: &str) -> Result<(), String> {
    let message = parse_native_message(input, raw).map_err(|error| error.to_string())?;
    let actual = match message {
        BinanceMessage::AggregateTrade(_) => "aggregate_trade",
        BinanceMessage::BookTicker(_) => "book_ticker",
        BinanceMessage::DepthUpdate(_) => "depth_update",
        BinanceMessage::MarkPrice(_) => "mark_index_funding",
        BinanceMessage::OpenInterest(_) => "open_interest",
        BinanceMessage::Liquidation(_) => "sampled_liquidation",
        BinanceMessage::VenueStatus { .. } => "venue_status",
    };
    if actual != expected {
        return Err(format!("expected {expected}, parsed {actual}"));
    }
    Ok(())
}

fn require_exchange_info(market: BinanceMarket, raw: &[u8]) -> Result<(), String> {
    let report = parse_exchange_info_report(market, raw).map_err(|error| error.to_string())?;
    if report.instruments().len() != 1
        || report.instruments()[0].status() != "TRADING"
        || report.skipped_unsupported_symbols() != 0
        || report.skipped_unsupported_contracts() != 0
        || report.skipped_inactive() != 0
    {
        return Err(
            "retained exchange-info fixture does not contain one active instrument".to_owned(),
        );
    }
    Ok(())
}

fn validate_injected_failures(workspace: &Path) -> Result<(), String> {
    let path = workspace.join("fixtures/exchanges/binance/spot-aggregate-trade.json");
    let raw = read_regular_bounded(&path, MAX_FIXTURE_BYTES)?;
    let mut value: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|error| format!("fixture JSON invalid: {error}"))?;
    value
        .as_object_mut()
        .ok_or_else(|| "trade fixture is not an object".to_owned())?
        .insert("undocumented".to_owned(), serde_json::json!(true));
    let drift = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    if parse_native_message(BinanceInput::SpotWebSocket, &drift)
        != Err(NativeParseError::SchemaDrift)
    {
        return Err("unknown-field injection did not fail closed".to_owned());
    }
    let oversized = vec![b' '; connector_binance::MAX_NATIVE_PAYLOAD_BYTES + 1];
    if parse_native_message(BinanceInput::SpotWebSocket, &oversized)
        != Err(NativeParseError::PayloadTooLarge)
    {
        return Err("oversized-payload injection did not fail closed".to_owned());
    }
    Ok(())
}

fn validate_bybit_injected_failures(workspace: &Path) -> Result<(), String> {
    let path = workspace.join("fixtures/exchanges/bybit/spot-public-trade.json");
    let raw = read_regular_bounded(&path, MAX_FIXTURE_BYTES)?;
    let mut value: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|error| format!("fixture JSON invalid: {error}"))?;
    value
        .as_object_mut()
        .ok_or_else(|| "trade fixture is not an object".to_owned())?
        .insert("undocumented".to_owned(), serde_json::json!(true));
    let drift = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    if parse_bybit_message(BybitInput::PublicWebSocket(BybitMarket::Spot), &drift)
        != Err(BybitParseError::SchemaDrift)
    {
        return Err("unknown-field injection did not fail closed".to_owned());
    }
    let oversized = vec![b' '; connector_bybit::MAX_NATIVE_PAYLOAD_BYTES + 1];
    if parse_bybit_message(BybitInput::PublicWebSocket(BybitMarket::Spot), &oversized)
        != Err(BybitParseError::PayloadTooLarge)
    {
        return Err("oversized-payload injection did not fail closed".to_owned());
    }
    Ok(())
}

fn validate_bybit_snapshot_reset(workspace: &Path) -> Result<(), String> {
    let read_book = |name: &str| -> Result<connector_bybit::OrderBookMessage, String> {
        let raw = read_regular_bounded(
            &workspace.join(format!("fixtures/exchanges/bybit/{name}")),
            MAX_FIXTURE_BYTES,
        )?;
        let message = parse_bybit_message(BybitInput::PublicWebSocket(BybitMarket::Spot), &raw)
            .map_err(|error| error.to_string())?;
        match message {
            BybitMessage::OrderBook(value) => Ok(value),
            _ => Err(format!("{name} did not parse as an order book")),
        }
    };
    let snapshot = read_book("spot-orderbook-snapshot.json")?;
    let delta = read_book("spot-orderbook-delta.json")?;
    let reset = read_book("spot-orderbook-reset.json")?;
    let mut synchronizer =
        BybitBookSynchronizer::try_new(BybitMarket::Spot, bybit_spot_book_config()?)
            .map_err(|error| error.to_string())?;
    synchronizer
        .start_session(BookSession {
            connection_epoch: 1,
            subscription_epoch: 1,
            instrument_generation: 1,
        })
        .map_err(|error| error.to_string())?;
    if synchronizer
        .apply_message(&delta, 1)
        .map_err(|error| error.to_string())?
        != ApplyResult::SnapshotRequired
    {
        return Err("delta before snapshot was not suppressed".to_owned());
    }
    if synchronizer
        .apply_message(&snapshot, 2)
        .map_err(|error| error.to_string())?
        != ApplyResult::Applied
        || synchronizer
            .apply_message(&delta, 3)
            .map_err(|error| error.to_string())?
            != ApplyResult::Applied
    {
        return Err("snapshot and exact-next delta did not synchronize".to_owned());
    }
    let mut gap = delta.clone();
    gap.update_id = 103;
    gap.cross_sequence = gap.cross_sequence.saturating_add(1);
    if synchronizer
        .apply_message(&gap, 4)
        .map_err(|error| error.to_string())?
        != ApplyResult::GapDetected
    {
        return Err("sequence gap was not detected".to_owned());
    }
    if synchronizer
        .apply_message(&reset, 5)
        .map_err(|error| error.to_string())?
        != ApplyResult::Applied
    {
        return Err("service-reset snapshot was not applied".to_owned());
    }
    let replacement = synchronizer.snapshot().map_err(|error| error.to_string())?;
    if replacement.bids().len() != 1
        || replacement.asks().len() != 1
        || replacement
            .best_bid()
            .is_none_or(|level| level.price.to_string() != "60100")
        || replacement
            .best_ask()
            .is_none_or(|level| level.price.to_string() != "60100.1")
        || synchronizer.snapshot_reset_count() != 2
    {
        return Err("new snapshot did not fully replace prior book state".to_owned());
    }
    Ok(())
}

fn bybit_spot_book_config() -> Result<BookConfig, String> {
    let decimal = |value: &str| {
        FixedDecimal::parse_canonical(value)
            .map_err(|error| format!("invalid certification decimal: {error}"))
    };
    Ok(BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("bybit").map_err(|error| error.to_string())?,
            "BTCUSDT",
            ProductType::Spot,
            1,
        )
        .map_err(|error| error.to_string())?,
        price_tick: Price::new(decimal("0.1")?).map_err(|error| error.to_string())?,
        quantity_step: Quantity::new(decimal("0.001")?).map_err(|error| error.to_string())?,
        max_levels_per_side: 100,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: SequencePolicy::ExactNext,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    })
}

fn production_gates() -> Vec<CertificationGate> {
    [
        (
            1,
            "golden parser fixtures for every subscribed message type",
            "pass",
            "13 provenance-bound retained fixtures",
        ),
        (
            2,
            "unknown-field and field-reordering tests",
            "pass",
            "runner reorders every fixture and injects an undocumented field",
        ),
        (
            3,
            "numeric boundary tests and fixed-point round trips",
            "not_run",
            "focused crate tests are not executed by this runner",
        ),
        (
            4,
            "snapshot/delta reconstruction",
            "not_run",
            "book synchronization tests are not executed by this runner",
        ),
        (
            5,
            "missing duplicated and reordered sequences",
            "not_run",
            "sequence fault tests are not executed by this runner",
        ),
        (
            6,
            "checksum mismatch and forced resnapshot where supported",
            "not_applicable",
            "Binance capability declares source checksums unsupported",
        ),
        (
            7,
            "connection expiry and clean renewal",
            "not_run",
            "session renewal test is not executed by this runner",
        ),
        (
            8,
            "rate-limit and backoff",
            "blocked",
            "production transport and operational limiter are not implemented",
        ),
        (
            9,
            "schema-drift fail-safe",
            "pass",
            "runner injects and rejects an undocumented field",
        ),
        (
            10,
            "clock-skew and timestamp-unit",
            "not_run",
            "timestamp boundary tests are not executed by this runner",
        ),
        (
            11,
            "burst-load and bounded queue",
            "not_run",
            "session capacity tests are not executed by this runner",
        ),
        (
            12,
            "raw-WAL replay equivalence",
            "not_run",
            "focused same-WAL test exists but is not executed or bound by this runner",
        ),
        (
            13,
            "kill-and-restart recovery",
            "blocked",
            "no supervised production transport restart path exists",
        ),
        (
            14,
            "data-completeness metadata",
            "pass",
            "capability is sampled-latest-per-symbol-window at 1000 ms",
        ),
        (
            15,
            "terms and redistribution review",
            "pass",
            "manifest records reviewed-restricted terms and local synthetic fixture provenance",
        ),
    ]
    .into_iter()
    .map(|(number, name, status, evidence)| CertificationGate {
        number,
        name: name.to_owned(),
        status: status.to_owned(),
        evidence: evidence.to_owned(),
    })
    .collect()
}

fn bybit_production_gates() -> Vec<CertificationGate> {
    [
        (
            1,
            "golden parser fixtures for every subscribed message type",
            "pass",
            "11 provenance-bound retained fixtures",
        ),
        (
            2,
            "unknown-field and field-reordering tests",
            "pass",
            "runner reorders every fixture and injects an undocumented field",
        ),
        (
            3,
            "numeric boundary tests and fixed-point round trips",
            "not_run",
            "focused crate tests are not executed by this runner",
        ),
        (
            4,
            "snapshot/delta reconstruction",
            "pass",
            "runner proves initial snapshot, exact-next delta, and full reset replacement",
        ),
        (
            5,
            "missing duplicated and reordered sequences",
            "partial",
            "runner suppresses delta-before-snapshot and detects an injected update-ID gap; duplicate and reordered cases remain focused-test evidence",
        ),
        (
            6,
            "checksum mismatch and forced resnapshot where supported",
            "not_applicable",
            "Bybit capability declares source checksums unsupported",
        ),
        (
            7,
            "heartbeat timeout and clean reconnect",
            "not_run",
            "focused session test is not executed by this runner",
        ),
        (
            8,
            "rate-limit and backoff",
            "blocked",
            "production transport and operational limiter are not implemented",
        ),
        (
            9,
            "schema-drift fail-safe",
            "pass",
            "runner injects and rejects an undocumented field",
        ),
        (
            10,
            "clock-skew and timestamp-unit",
            "not_run",
            "timestamp boundary tests are not executed by this runner",
        ),
        (
            11,
            "burst-load and bounded queue",
            "not_run",
            "focused session capacity tests are not executed by this runner",
        ),
        (
            12,
            "raw-WAL replay equivalence",
            "not_run",
            "focused verified-WAL replay test exists but is not executed or bound by this runner",
        ),
        (
            13,
            "kill-and-restart recovery",
            "blocked",
            "no supervised production transport restart path exists",
        ),
        (
            14,
            "data-completeness metadata",
            "pass",
            "capability is venue-reported-all at 500 ms with delivery uncertainty",
        ),
        (
            15,
            "terms and redistribution review",
            "pass",
            "manifest records reviewed-restricted terms and local synthetic fixture provenance",
        ),
    ]
    .into_iter()
    .map(|(number, name, status, evidence)| CertificationGate {
        number,
        name: name.to_owned(),
        status: status.to_owned(),
        evidence: evidence.to_owned(),
    })
    .collect()
}

fn workspace_root(fixtures: &Path) -> Result<PathBuf, String> {
    workspace_root_for(fixtures, "binance")
}

fn workspace_root_for(fixtures: &Path, venue: &str) -> Result<PathBuf, String> {
    if !matches!(venue, "binance" | "bybit") {
        return Err("unsupported fixture venue".to_owned());
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "certification app is not inside a workspace".to_owned())?
        .canonicalize()
        .map_err(|error| format!("could not resolve compiled workspace: {error}"))?;
    let expected = workspace
        .join(format!("fixtures/exchanges/{venue}"))
        .canonicalize()
        .map_err(|error| format!("could not resolve retained fixture directory: {error}"))?;
    let provided = fixtures
        .canonicalize()
        .map_err(|error| format!("could not resolve {}: {error}", fixtures.display()))?;
    if provided != expected {
        return Err(format!(
            "fixture path is not the retained {venue} fixture directory"
        ));
    }
    Ok(workspace)
}

fn read_regular_bounded(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>, String> {
    let metadata = path
        .symlink_metadata()
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > maximum_bytes {
        return Err(format!(
            "{} is not a bounded regular certification input",
            path.display()
        ));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > maximum_bytes) {
        return Err(format!(
            "{} exceeded the certification input bound while reading",
            path.display()
        ));
    }
    Ok(bytes)
}

fn compare_exact(path: &Path, expected: &[u8]) -> Result<(), String> {
    let actual =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if actual != expected {
        return Err(format!(
            "generated certification is stale: {}",
            path.display()
        ));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    #[test]
    fn workspace_root_rejects_an_unrelated_fixture_tree() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let fixtures = temporary.path().join("fixtures/exchanges/binance");
        fs::create_dir_all(&fixtures).expect("fixture tree");

        assert!(super::workspace_root(&fixtures).is_err());
    }

    #[test]
    fn fixture_paths_reject_traversal_and_control_characters() {
        for path in [
            "../fixtures/exchanges/binance/trade.json",
            "fixtures/exchanges/binance/../trade.json",
            "fixtures/exchanges/binance/trade\n.json",
        ] {
            assert!(
                super::validate_safe_fixture_path(path).is_err(),
                "unsafe path was accepted: {path:?}"
            );
        }
    }

    #[test]
    fn bounded_reader_rejects_oversized_inputs() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let input = temporary.path().join("input.json");
        fs::write(&input, b"12345").expect("bounded input");

        assert!(super::read_regular_bounded(&input, 4).is_err());
        assert_eq!(
            super::read_regular_bounded(&input, 5).expect("exact bound"),
            b"12345"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_reader_rejects_symbolic_links() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let target = temporary.path().join("target.json");
        let alias = temporary.path().join("alias.json");
        fs::write(&target, b"{}").expect("target");
        symlink(&target, &alias).expect("symlink");

        assert!(super::read_regular_bounded(Path::new(&alias), 2).is_err());
    }
}
