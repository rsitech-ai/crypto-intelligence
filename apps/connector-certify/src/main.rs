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
use connector_deribit::{
    DeribitBookSynchronizer, DeribitInput, DeribitMessage, NativeParseError as DeribitParseError,
    capabilities as deribit_capabilities, normalize_option_ticker,
    parse_native_message as parse_deribit_message,
};
use connector_kraken::{
    KrakenBookSynchronizer, KrakenInput, KrakenL3Policy, KrakenMessage,
    NativeParseError as KrakenParseError, capabilities as kraken_capabilities,
    parse_native_message as parse_kraken_message,
};
use domain::{AssetId, AssetNamespace, InstrumentId, ProductType, VenueId};
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
    #[arg(long, required_unless_present = "all", conflicts_with = "all")]
    venue: Option<String>,
    #[arg(long, required_unless_present = "venue", conflicts_with = "venue")]
    all: bool,
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
        Ok(CertificationResult::Venue(report_hash)) => {
            println!(
                "connector-certify: FIXTURE_PASS production_certification=BLOCKED sha256={report_hash}"
            );
            ExitCode::SUCCESS
        }
        Ok(CertificationResult::All(report_hash)) => {
            println!(
                "connector-certify: FIXTURE_PASS venues=binance,bybit,deribit,kraken production_certification=BLOCKED sha256={report_hash}"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("connector-certify: FAIL: {error}");
            ExitCode::FAILURE
        }
    }
}

enum CertificationResult {
    Venue(String),
    All(String),
}

fn run(arguments: Arguments) -> Result<CertificationResult, String> {
    if arguments.all {
        return run_all(arguments).map(CertificationResult::All);
    }
    let venue = arguments
        .venue
        .clone()
        .ok_or_else(|| "one certification mode is required".to_owned())?;
    run_venue(&venue, arguments).map(CertificationResult::Venue)
}

fn run_venue(venue: &str, arguments: Arguments) -> Result<String, String> {
    match venue {
        "binance" => run_binance(arguments),
        "bybit" => run_bybit(arguments),
        "deribit" => run_deribit(arguments),
        "kraken" => run_kraken(arguments),
        _ => Err("supported venues are binance, bybit, deribit, and kraken".to_owned()),
    }
}

fn run_all(arguments: Arguments) -> Result<String, String> {
    if !arguments.check {
        return Err("--all requires --check to prevent partial artifact writes".to_owned());
    }
    let workspace = compiled_workspace_root()?;
    let expected_root = workspace
        .join("fixtures/exchanges")
        .canonicalize()
        .map_err(|error| format!("could not resolve retained fixture root: {error}"))?;
    let provided_root = arguments.fixtures.canonicalize().map_err(|error| {
        format!(
            "could not resolve {}: {error}",
            arguments.fixtures.display()
        )
    })?;
    if provided_root != expected_root {
        return Err("fixture path is not the retained all-venue fixture directory".to_owned());
    }

    let mut aggregate = Sha256::new();
    aggregate.update(b"cmti:connector-certification:all:v1\0");
    for venue in ["binance", "bybit", "deribit", "kraken"] {
        let report_hash = run_venue(
            venue,
            Arguments {
                venue: Some(venue.to_owned()),
                all: false,
                fixtures: provided_root.join(venue),
                check: true,
            },
        )?;
        aggregate.update(
            u64::try_from(venue.len())
                .map_err(|_| "venue length overflow".to_owned())?
                .to_be_bytes(),
        );
        aggregate.update(venue.as_bytes());
        aggregate.update(
            u64::try_from(report_hash.len())
                .map_err(|_| "report hash length overflow".to_owned())?
                .to_be_bytes(),
        );
        aggregate.update(report_hash.as_bytes());
    }
    Ok(format!("{:x}", aggregate.finalize()))
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

fn run_kraken(arguments: Arguments) -> Result<String, String> {
    let workspace = workspace_root_for(&arguments.fixtures, "kraken")?;
    let manifest_path = arguments.fixtures.join("manifest.toml");
    let manifest_bytes = read_regular_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: FixtureManifest = toml::from_str(
        std::str::from_utf8(&manifest_bytes)
            .map_err(|_| "fixture manifest is not UTF-8".to_owned())?,
    )
    .map_err(|error| format!("fixture manifest is invalid: {error}"))?;
    validate_kraken_manifest_metadata(&manifest, &workspace)?;
    validate_kraken_manifest_coverage(&manifest)?;
    let mut fixture_hashes = BTreeMap::new();
    for fixture in &manifest.fixture {
        validate_kraken_safe_fixture_path(&fixture.path)?;
        let path = workspace.join(&fixture.path);
        let raw = read_regular_bounded(&path, MAX_FIXTURE_BYTES)?;
        let actual_hash = sha256_hex(&raw);
        if actual_hash != fixture.sha256 {
            return Err(format!(
                "fixture hash mismatch for {}: expected {}, got {actual_hash}",
                fixture.path, fixture.sha256
            ));
        }
        validate_kraken_fixture(fixture, &raw)?;
        let reordered = serde_json::to_vec(
            &serde_json::from_slice::<serde_json::Value>(&raw)
                .map_err(|error| format!("fixture JSON is invalid: {error}"))?,
        )
        .map_err(|error| format!("could not reorder fixture JSON: {error}"))?;
        validate_kraken_fixture(fixture, &reordered)?;
        if fixture_hashes
            .insert(fixture.path.clone(), actual_hash)
            .is_some()
        {
            return Err(format!("duplicate fixture path: {}", fixture.path));
        }
    }
    validate_kraken_injected_failures()?;
    validate_kraken_book_integrity(&workspace)?;
    let capabilities =
        kraken_capabilities().map_err(|error| format!("capabilities invalid: {error}"))?;
    if capabilities.trade_semantics() != TradeSemantics::Individual
        || capabilities.liquidation_completeness() != Completeness::NotSupported
    {
        return Err("Kraken capability surface overstates the retained parser".to_owned());
    }

    let report = CertificationReport {
        schema_version: 1,
        venue: "kraken".to_owned(),
        fixture_status: "pass".to_owned(),
        production_certification_status: "blocked".to_owned(),
        evidence_scope: "retained-fixture".to_owned(),
        live_exchange_acceptance: false,
        manifest_sha256: sha256_hex(&manifest_bytes),
        connector_version: capabilities.connector_version().to_owned(),
        trade_semantics: "individual".to_owned(),
        liquidation_completeness: "not_supported".to_owned(),
        fixture_count: fixture_hashes.len(),
        fixtures: fixture_hashes,
        checks: [
            "fixture_hashes",
            "all_retained_fixture_record_types",
            "spot_v2_exact_decimal_checksum",
            "selected_l3_queue_checksum",
            "futures_sequence_fields",
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
        production_gates: kraken_production_gates(),
        limitations: vec![
            "No authenticated token acquisition, account data, order placement, or trading"
                .to_owned(),
            "L3 parser and integrity behavior use local fixtures; live authenticated L3 acceptance is not proven".to_owned(),
            "This artifact validates retained parser and checksum fixtures, not live exchange acceptance".to_owned(),
            "Production transport, operational rate enforcement, supervised reconnect, raw-WAL session replay, and canary evidence remain blocked or unrun".to_owned(),
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

fn run_deribit(arguments: Arguments) -> Result<String, String> {
    let workspace = workspace_root_for(&arguments.fixtures, "deribit")?;
    let manifest_path = arguments.fixtures.join("manifest.toml");
    let manifest_bytes = read_regular_bounded(&manifest_path, MAX_MANIFEST_BYTES)?;
    let manifest: FixtureManifest = toml::from_str(
        std::str::from_utf8(&manifest_bytes)
            .map_err(|_| "fixture manifest is not UTF-8".to_owned())?,
    )
    .map_err(|error| format!("fixture manifest is invalid: {error}"))?;
    validate_deribit_manifest_metadata(&manifest, &workspace)?;
    validate_deribit_manifest_coverage(&manifest)?;
    let mut fixture_hashes = BTreeMap::new();
    for fixture in &manifest.fixture {
        validate_deribit_safe_fixture_path(&fixture.path)?;
        let path = workspace.join(&fixture.path);
        let raw = read_regular_bounded(&path, MAX_FIXTURE_BYTES)?;
        let actual_hash = sha256_hex(&raw);
        if actual_hash != fixture.sha256 {
            return Err(format!(
                "fixture hash mismatch for {}: expected {}, got {actual_hash}",
                fixture.path, fixture.sha256
            ));
        }
        validate_deribit_fixture(fixture, &raw)?;
        if fixture_hashes
            .insert(fixture.path.clone(), actual_hash)
            .is_some()
        {
            return Err(format!("duplicate fixture path: {}", fixture.path));
        }
    }
    validate_deribit_injected_failures()?;
    validate_deribit_option_identity(&workspace)?;
    validate_deribit_book_integrity(&workspace)?;
    let capabilities =
        deribit_capabilities().map_err(|error| format!("capabilities invalid: {error}"))?;
    if capabilities.trade_semantics() != TradeSemantics::Individual
        || capabilities.liquidation_completeness() != Completeness::NotSupported
    {
        return Err("Deribit capability surface overstates the retained parser".to_owned());
    }

    let report = CertificationReport {
        schema_version: 1,
        venue: "deribit".to_owned(),
        fixture_status: "pass".to_owned(),
        production_certification_status: "blocked".to_owned(),
        evidence_scope: "retained-fixture".to_owned(),
        live_exchange_acceptance: false,
        manifest_sha256: sha256_hex(&manifest_bytes),
        connector_version: capabilities.connector_version().to_owned(),
        trade_semantics: "individual".to_owned(),
        liquidation_completeness: "not_supported".to_owned(),
        fixture_count: fixture_hashes.len(),
        fixtures: fixture_hashes,
        checks: [
            "fixture_hashes",
            "all_retained_fixture_record_types",
            "complete_generation_aware_option_identity",
            "source_iv_and_greeks_are_distinct",
            "previous_change_id_gap_recovery",
            "delivery_and_settlement_observations",
            "field_reordering",
            "scientific_notation_exactness",
            "schema_drift_and_duplicate_key_rejection",
            "payload_size_rejection",
            "truthful_capabilities",
        ]
        .into_iter()
        .map(|name| CertificationCheck {
            name: name.to_owned(),
            status: "pass".to_owned(),
        })
        .collect(),
        production_gates: deribit_production_gates(),
        limitations: vec![
            "No authenticated raw-channel subscription, account data, order placement, or trading"
                .to_owned(),
            "This artifact validates repository-owned retained fixtures, not live exchange acceptance"
                .to_owned(),
            "Production WebSocket/REST transport, operational rate enforcement, heartbeat supervision, raw-WAL session replay, and canary evidence remain blocked or unrun".to_owned(),
            "Source-reported option IV and Greeks are retained as observations and are not a locally recomputed volatility surface".to_owned(),
            "Deribit's ungrouped book has no source depth limit; this adapter intentionally rejects snapshots above 10,000 levels per side".to_owned(),
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

fn validate_kraken_manifest_metadata(
    manifest: &FixtureManifest,
    workspace: &Path,
) -> Result<(), String> {
    if manifest.schema_version != 1
        || manifest.artifact_id != "fixtures/exchanges/kraken/manifest.toml"
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
        return Err("Kraken fixture manifest metadata is not certification-ready".to_owned());
    }
    for source in &manifest.source {
        if source.id.is_empty()
            || !source.url.starts_with("https://docs.kraken.com/")
            || source.scope.is_empty()
            || validate_kraken_safe_fixture_path(&source.snapshot).is_err()
            || source.snapshot_sha256.len() != 64
            || !source
                .snapshot_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("Kraken fixture documentation source is invalid".to_owned());
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

fn validate_deribit_manifest_metadata(
    manifest: &FixtureManifest,
    workspace: &Path,
) -> Result<(), String> {
    if manifest.schema_version != 1
        || manifest.artifact_id != "fixtures/exchanges/deribit/manifest.toml"
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
        return Err("Deribit fixture manifest metadata is not certification-ready".to_owned());
    }
    for source in &manifest.source {
        if source.id.is_empty()
            || !(source.url.starts_with("https://docs.deribit.com/")
                || source
                    .url
                    .starts_with("https://support.deribit.com/hc/en-us/articles/"))
            || source.scope.is_empty()
            || validate_deribit_safe_fixture_path(&source.snapshot).is_err()
            || source.snapshot_sha256.len() != 64
            || !source
                .snapshot_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("Deribit fixture documentation source is invalid".to_owned());
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

fn validate_kraken_manifest_coverage(manifest: &FixtureManifest) -> Result<(), String> {
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
        ("futures", "websocket", "book_snapshot"),
        ("futures", "websocket", "book_update"),
        ("futures", "websocket", "trade"),
        ("spot", "websocket_v2", "book_snapshot"),
        ("spot", "websocket_v2", "heartbeat"),
        ("spot", "websocket_v2", "instrument"),
        ("spot", "websocket_v2", "level3_snapshot"),
        ("spot", "websocket_v2", "status"),
        ("spot", "websocket_v2", "trade"),
    ]);
    if actual != expected || manifest.fixture.len() != expected.len() {
        return Err(format!(
            "Kraken fixture coverage must be exact: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
}

fn validate_deribit_manifest_coverage(manifest: &FixtureManifest) -> Result<(), String> {
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
        ("future", "public_get_instruments", "instrument"),
        ("future", "websocket_subscription", "ticker"),
        (
            "future_option",
            "public_get_delivery_prices",
            "delivery_prices",
        ),
        ("option", "public_get_instruments", "instrument"),
        ("option", "websocket_subscription", "ticker"),
        ("perpetual", "public_get_instruments", "instrument"),
        ("perpetual", "websocket_subscription", "book_change"),
        ("perpetual", "websocket_subscription", "book_snapshot"),
        ("perpetual", "websocket_subscription", "perpetual_interest"),
        ("perpetual", "websocket_subscription", "ticker"),
        ("perpetual", "websocket_subscription", "trade"),
    ]);
    if actual != expected || manifest.fixture.len() != expected.len() {
        return Err(format!(
            "Deribit fixture coverage must be exact: expected {expected:?}, got {actual:?}"
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

fn validate_kraken_safe_fixture_path(path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        || !path.starts_with("fixtures/exchanges/kraken")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe Kraken fixture path: {}", path.display()));
    }
    Ok(())
}

fn validate_deribit_safe_fixture_path(path: &str) -> Result<(), String> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
        || !path.starts_with("fixtures/exchanges/deribit")
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("unsafe Deribit fixture path: {}", path.display()));
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

fn validate_kraken_fixture(fixture: &ManifestFixture, raw: &[u8]) -> Result<(), String> {
    let input = match fixture.route.as_str() {
        "websocket_v2" => KrakenInput::SpotWebSocketV2,
        "websocket" => KrakenInput::FuturesWebSocket,
        other => return Err(format!("unsupported Kraken fixture route: {other}")),
    };
    let message = parse_kraken_message(input, raw).map_err(|error| error.to_string())?;
    let matches = matches!(
        (fixture.record_type.as_str(), &message),
        ("book_snapshot", KrakenMessage::SpotBook(value))
            if value.kind == connector_kraken::BookMessageKind::Snapshot
    ) || matches!(
        (fixture.record_type.as_str(), &message),
        ("level3_snapshot", KrakenMessage::SpotLevel3(value))
            if value.kind == connector_kraken::BookMessageKind::Snapshot
    ) || matches!(
        (fixture.record_type.as_str(), &message),
        ("trade", KrakenMessage::SpotTrades { .. })
            | ("instrument", KrakenMessage::SpotInstrument(_))
            | ("status", KrakenMessage::SpotStatus { .. })
            | ("heartbeat", KrakenMessage::Heartbeat)
    ) || matches!(
        (fixture.record_type.as_str(), &message),
        ("book_snapshot", KrakenMessage::FuturesBook(value))
            if value.kind == connector_kraken::BookMessageKind::Snapshot
    ) || matches!(
        (fixture.record_type.as_str(), &message),
        ("book_update", KrakenMessage::FuturesBook(value))
            if value.kind == connector_kraken::BookMessageKind::Update
    ) || matches!(
        (fixture.record_type.as_str(), &message),
        ("trade", KrakenMessage::FuturesTrades(_))
    );
    if matches {
        Ok(())
    } else {
        Err(format!(
            "fixture {} did not parse as {}",
            fixture.path, fixture.record_type
        ))
    }
}

fn validate_deribit_fixture(fixture: &ManifestFixture, raw: &[u8]) -> Result<(), String> {
    let input = match fixture.route.as_str() {
        "websocket_subscription" => DeribitInput::Subscription,
        "public_get_instruments" => DeribitInput::InstrumentsResponse,
        "public_get_delivery_prices" => DeribitInput::DeliveryPricesResponse,
        other => return Err(format!("unsupported Deribit fixture route: {other}")),
    };
    let message = parse_deribit_message(input, raw).map_err(|error| error.to_string())?;
    let matches = matches!(
        (fixture.record_type.as_str(), &message),
        ("instrument", DeribitMessage::Instruments(value)) if value.len() == 1
    ) || matches!(
        (fixture.record_type.as_str(), &message),
        ("ticker", DeribitMessage::Ticker(_))
            | ("trade", DeribitMessage::Trades(_))
            | ("perpetual_interest", DeribitMessage::PerpetualInterest(_))
            | ("delivery_prices", DeribitMessage::DeliveryPrices { .. })
    ) || matches!(
        (fixture.record_type.as_str(), &message),
        ("book_snapshot", DeribitMessage::Book(value))
            if value.kind == connector_deribit::BookMessageKind::Snapshot
    ) || matches!(
        (fixture.record_type.as_str(), &message),
        ("book_change", DeribitMessage::Book(value))
            if value.kind == connector_deribit::BookMessageKind::Change
    );
    if matches {
        Ok(())
    } else {
        Err(format!(
            "fixture {} did not parse as {}",
            fixture.path, fixture.record_type
        ))
    }
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

fn validate_kraken_injected_failures() -> Result<(), String> {
    let drift = br#"{"channel":"heartbeat","undocumented":true}"#;
    if parse_kraken_message(KrakenInput::SpotWebSocketV2, drift)
        != Err(KrakenParseError::SchemaDrift)
    {
        return Err("Kraken unknown-field injection did not fail closed".to_owned());
    }
    let duplicate = br#"{"channel":"heartbeat","channel":"heartbeat"}"#;
    if parse_kraken_message(KrakenInput::SpotWebSocketV2, duplicate)
        != Err(KrakenParseError::MalformedJson)
    {
        return Err("Kraken duplicate-field injection did not fail closed".to_owned());
    }
    let oversized = vec![b' '; connector_kraken::MAX_NATIVE_PAYLOAD_BYTES + 1];
    if parse_kraken_message(KrakenInput::SpotWebSocketV2, &oversized)
        != Err(KrakenParseError::PayloadTooLarge)
    {
        return Err("Kraken oversized-payload injection did not fail closed".to_owned());
    }
    Ok(())
}

fn validate_deribit_injected_failures() -> Result<(), String> {
    let reordered = br#"{"params":{"data":{"index_price":64500.25,"timestamp":1760000000823,"interest":0.004999511380756577},"channel":"perpetual.BTC-PERPETUAL.100ms"},"method":"subscription","jsonrpc":"2.0"}"#;
    if !matches!(
        parse_deribit_message(DeribitInput::Subscription, reordered),
        Ok(DeribitMessage::PerpetualInterest(_))
    ) {
        return Err("Deribit field-reordering probe did not parse".to_owned());
    }
    let scientific = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"perpetual.BTC-PERPETUAL.100ms","data":{"interest":4.999511380756577e-3,"timestamp":1760000000823,"index_price":6.450025e4}}}"#;
    let Ok(DeribitMessage::PerpetualInterest(scientific)) =
        parse_deribit_message(DeribitInput::Subscription, scientific)
    else {
        return Err("Deribit scientific-notation probe did not parse".to_owned());
    };
    if scientific.interest
        != FixedDecimal::parse_canonical("0.004999511380756577")
            .map_err(|error| error.to_string())?
        || scientific.index_price
            != Price::new(
                FixedDecimal::parse_canonical("64500.25").map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?
    {
        return Err("Deribit scientific notation changed the numeric value".to_owned());
    }
    let drift = br#"{"jsonrpc":"2.0","method":"subscription","params":{"channel":"perpetual.BTC-PERPETUAL.100ms","data":{"interest":1,"timestamp":1,"index_price":1,"undocumented":true}}}"#;
    if parse_deribit_message(DeribitInput::Subscription, drift)
        != Err(DeribitParseError::SchemaDrift)
    {
        return Err("Deribit unknown-field injection did not fail closed".to_owned());
    }
    let duplicate = br#"{"jsonrpc":"2.0","jsonrpc":"2.0","method":"subscription","params":{"channel":"perpetual.BTC-PERPETUAL.100ms","data":{"interest":1,"timestamp":1,"index_price":1}}}"#;
    if parse_deribit_message(DeribitInput::Subscription, duplicate)
        != Err(DeribitParseError::MalformedJson)
    {
        return Err("Deribit duplicate-field injection did not fail closed".to_owned());
    }
    let oversized = vec![b' '; connector_deribit::MAX_NATIVE_PAYLOAD_BYTES + 1];
    if parse_deribit_message(DeribitInput::Subscription, &oversized)
        != Err(DeribitParseError::PayloadTooLarge)
    {
        return Err("Deribit oversized-payload injection did not fail closed".to_owned());
    }
    Ok(())
}

fn validate_deribit_option_identity(workspace: &Path) -> Result<(), String> {
    let instrument_raw = read_regular_bounded(
        &workspace.join("fixtures/exchanges/deribit/option-instrument.json"),
        MAX_FIXTURE_BYTES,
    )?;
    let ticker_raw = read_regular_bounded(
        &workspace.join("fixtures/exchanges/deribit/option-ticker.json"),
        MAX_FIXTURE_BYTES,
    )?;
    let DeribitMessage::Instruments(mut instruments) =
        parse_deribit_message(DeribitInput::InstrumentsResponse, &instrument_raw)
            .map_err(|error| error.to_string())?
    else {
        return Err("Deribit option metadata fixture has the wrong shape".to_owned());
    };
    let metadata = instruments
        .pop()
        .filter(|_| instruments.is_empty())
        .ok_or_else(|| "Deribit option metadata must contain one instrument".to_owned())?;
    let DeribitMessage::Ticker(ticker) =
        parse_deribit_message(DeribitInput::Subscription, &ticker_raw)
            .map_err(|error| error.to_string())?
    else {
        return Err("Deribit option ticker fixture has the wrong shape".to_owned());
    };
    let base = AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1)
        .map_err(|error| error.to_string())?;
    let quote =
        AssetId::new(AssetNamespace::Fiat, "", "", "USD", 1).map_err(|error| error.to_string())?;
    let binding =
        connector_deribit::DeribitInstrumentBinding::try_new(7, base.clone(), quote, base)
            .map_err(|error| error.to_string())?;
    let definition = metadata
        .to_definition(&binding)
        .map_err(|error| error.to_string())?;
    let normalized =
        normalize_option_ticker(&metadata, &ticker, &binding).map_err(|error| error.to_string())?;
    if definition.product_type() != ProductType::Option
        || definition.expiry_time().is_none()
        || definition.strike().is_none()
        || definition.option_side().is_none()
        || definition.id().generation() != 7
        || normalized.instrument != *definition.id()
        || normalized.mark_iv.is_negative()
    {
        return Err("Deribit option identity or source metrics are incomplete".to_owned());
    }
    Ok(())
}

fn validate_deribit_book_integrity(workspace: &Path) -> Result<(), String> {
    let read_book = |name: &str| -> Result<connector_deribit::DeribitBookMessage, String> {
        let raw = read_regular_bounded(
            &workspace.join(format!("fixtures/exchanges/deribit/{name}")),
            MAX_FIXTURE_BYTES,
        )?;
        let DeribitMessage::Book(message) = parse_deribit_message(DeribitInput::Subscription, &raw)
            .map_err(|error| error.to_string())?
        else {
            return Err(format!("{name} did not parse as a Deribit book"));
        };
        Ok(message)
    };
    let snapshot = read_book("book-snapshot.json")?;
    let change = read_book("book-change.json")?;
    let mut synchronizer = DeribitBookSynchronizer::try_new(deribit_book_config()?)
        .map_err(|error| error.to_string())?;
    let session = BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    };
    synchronizer
        .start_session(session)
        .map_err(|error| error.to_string())?;
    if synchronizer
        .apply(&snapshot, 1)
        .map_err(|error| error.to_string())?
        != ApplyResult::Applied
        || synchronizer
            .apply(&change, 2)
            .map_err(|error| error.to_string())?
            != ApplyResult::Applied
    {
        return Err("Deribit retained book did not synchronize".to_owned());
    }
    let mut gap = change.clone();
    gap.change_id = 103;
    gap.previous_change_id = Some(102);
    if synchronizer
        .apply(&gap, 3)
        .map_err(|error| error.to_string())?
        != ApplyResult::GapDetected
        || synchronizer.snapshot().is_ok()
    {
        return Err("Deribit continuity gap did not suppress publication".to_owned());
    }
    let mut replacement = snapshot;
    replacement.change_id = 200;
    if synchronizer
        .apply(&replacement, 4)
        .map_err(|error| error.to_string())?
        != ApplyResult::Applied
        || synchronizer.snapshot().is_err()
    {
        return Err("Deribit replacement snapshot did not restore trust".to_owned());
    }
    Ok(())
}

fn validate_kraken_book_integrity(workspace: &Path) -> Result<(), String> {
    let read_spot = |name: &str| -> Result<KrakenMessage, String> {
        let raw = read_regular_bounded(
            &workspace.join(format!("fixtures/exchanges/kraken/{name}")),
            MAX_FIXTURE_BYTES,
        )?;
        parse_kraken_message(KrakenInput::SpotWebSocketV2, &raw).map_err(|error| error.to_string())
    };
    let KrakenMessage::SpotBook(snapshot) = read_spot("spot-book-snapshot.json")? else {
        return Err("Kraken Spot fixture did not parse as an L2 snapshot".to_owned());
    };
    let KrakenMessage::SpotLevel3(l3_snapshot) = read_spot("spot-level3-snapshot.json")? else {
        return Err("Kraken Spot fixture did not parse as an L3 snapshot".to_owned());
    };
    let policy = KrakenL3Policy::try_new(["BTC/USD".to_owned()], 10, 5_000)
        .map_err(|error| error.to_string())?;
    let mut synchronizer =
        KrakenBookSynchronizer::try_new_with_l3(kraken_spot_book_config()?, policy)
            .map_err(|error| error.to_string())?;
    let session = BookSession {
        connection_epoch: 1,
        subscription_epoch: 1,
        instrument_generation: 1,
    };
    synchronizer
        .start_session(session)
        .map_err(|error| error.to_string())?;
    if synchronizer
        .apply_spot(&snapshot, 1, 1)
        .map_err(|error| error.to_string())?
        != ApplyResult::Applied
        || synchronizer
            .apply_level3(&l3_snapshot, 2, 1)
            .map_err(|error| error.to_string())?
            != ApplyResult::Applied
    {
        return Err("Kraken L2/L3 reference checksums did not synchronize".to_owned());
    }
    let mut invalid_l2 = snapshot.clone();
    invalid_l2.checksum = invalid_l2.checksum.wrapping_add(1);
    if synchronizer
        .apply_spot(&invalid_l2, 3, 1)
        .map_err(|error| error.to_string())?
        != ApplyResult::ChecksumMismatch
        || synchronizer.snapshot().is_ok()
    {
        return Err("Kraken L2 checksum mismatch did not suppress publication".to_owned());
    }
    if synchronizer
        .apply_spot(&snapshot, 4, 1)
        .map_err(|error| error.to_string())?
        != ApplyResult::Applied
    {
        return Err("Kraken L2 did not recover from a fresh snapshot".to_owned());
    }
    synchronizer.disconnect();
    synchronizer
        .start_session(BookSession {
            connection_epoch: 2,
            subscription_epoch: 2,
            instrument_generation: 1,
        })
        .map_err(|error| error.to_string())?;
    if synchronizer
        .apply_spot(&snapshot, 5, 1)
        .map_err(|error| error.to_string())?
        != ApplyResult::Applied
    {
        return Err("Kraken reconnect did not require and accept a fresh snapshot".to_owned());
    }
    Ok(())
}

fn kraken_spot_book_config() -> Result<BookConfig, String> {
    let decimal = |value: &str| {
        FixedDecimal::parse_canonical(value)
            .map_err(|error| format!("invalid certification decimal: {error}"))
    };
    Ok(BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("kraken").map_err(|error| error.to_string())?,
            "BTCUSD",
            ProductType::Spot,
            1,
        )
        .map_err(|error| error.to_string())?,
        price_tick: Price::new(decimal("0.1")?).map_err(|error| error.to_string())?,
        quantity_step: Quantity::new(decimal("0.00000001")?).map_err(|error| error.to_string())?,
        max_levels_per_side: 10,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: SequencePolicy::ExactNext,
        checksum_policy: ChecksumPolicy::Required,
        max_l3_orders: Some(5_000),
        max_l3_levels_per_side: Some(10),
    })
}

fn deribit_book_config() -> Result<BookConfig, String> {
    let decimal = |value: &str| {
        FixedDecimal::parse_canonical(value)
            .map_err(|error| format!("invalid certification decimal: {error}"))
    };
    Ok(BookConfig {
        instrument: InstrumentId::new_for_product(
            VenueId::new("deribit").map_err(|error| error.to_string())?,
            "BTC-PERPETUAL",
            ProductType::Perpetual,
            1,
        )
        .map_err(|error| error.to_string())?,
        price_tick: Price::new(decimal("0.5")?).map_err(|error| error.to_string())?,
        quantity_step: Quantity::new(decimal("10")?).map_err(|error| error.to_string())?,
        max_levels_per_side: 100,
        max_buffered_deltas: 8,
        max_buffered_level_updates: 32,
        sequence_policy: SequencePolicy::PreviousFinal,
        checksum_policy: ChecksumPolicy::Disabled,
        max_l3_orders: None,
        max_l3_levels_per_side: None,
    })
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

fn kraken_production_gates() -> Vec<CertificationGate> {
    [
        (
            1,
            "golden parser fixtures for every subscribed message type",
            "partial",
            "9 provenance-bound retained fixtures cover the certified parser subset; no production subscription inventory exists",
        ),
        (
            2,
            "unknown-field and field-reordering tests",
            "pass",
            "runner reorders every fixture and rejects injected undocumented and duplicate fields",
        ),
        (
            3,
            "numeric boundary tests and fixed-point round trips",
            "partial",
            "source decimal lexemes and official CRC32 vector pass; wider property tests remain focused-test work",
        ),
        (
            4,
            "snapshot/delta reconstruction",
            "partial",
            "Spot L2/L3 snapshots are reconstructed and checksum-gated; retained Spot update and Futures engine reconstruction are not yet certified",
        ),
        (
            5,
            "missing duplicated and reordered sequences",
            "partial",
            "Futures sequence fields are retained; transport-session discontinuity tests are not run by this runner",
        ),
        (
            6,
            "checksum mismatch and forced resnapshot where supported",
            "pass",
            "focused connector tests prove Spot L2 and selected L3 mismatch suppression and snapshot recovery",
        ),
        (
            7,
            "heartbeat timeout and clean reconnect",
            "partial",
            "fixture book state is cleared and resnapshotted across a synthetic reconnect; heartbeat timeout supervision is not implemented",
        ),
        (
            8,
            "rate-limit and backoff",
            "blocked",
            "L3 subscription capacity is declared, but production transport enforcement is not implemented",
        ),
        (
            9,
            "schema-drift fail-safe",
            "pass",
            "runner injects and rejects an undocumented field and duplicate key",
        ),
        (
            10,
            "clock-skew and timestamp-unit",
            "partial",
            "Spot RFC3339 and Futures millisecond shapes are retained; clock-skew policy is not implemented",
        ),
        (
            11,
            "burst-load and bounded queue",
            "partial",
            "payload and collection bounds exist; production session queue evidence is not available",
        ),
        (
            12,
            "raw-WAL replay equivalence",
            "not_run",
            "durable parser binding exists, but a Kraken session replay test is not yet executed",
        ),
        (
            13,
            "kill-and-restart recovery",
            "blocked",
            "no supervised production Kraken transport restart path exists",
        ),
        (
            14,
            "data-completeness metadata",
            "pass",
            "unsupported liquidations and delivery uncertainty are declared without overstatement",
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

fn deribit_production_gates() -> Vec<CertificationGate> {
    [
        (
            1,
            "golden parser fixtures for every retained message type",
            "pass",
            "11 provenance-bound fixtures cover futures, perpetuals, options, books, trades, interest, and delivery prices",
        ),
        (
            2,
            "unknown-field, duplicate-key, and field-reordering tests",
            "pass",
            "runner reorders a representative subscription payload and rejects injected schema drift and duplicate keys",
        ),
        (
            3,
            "numeric boundary tests and fixed-point round trips",
            "partial",
            "ordinary and scientific source lexemes map to exact fixed decimals within explicit bounds; broader property matrices remain future work",
        ),
        (
            4,
            "snapshot and delta reconstruction",
            "pass",
            "runner reconstructs the retained snapshot and change atomically",
        ),
        (
            5,
            "missing, duplicated, and reordered change identifiers",
            "partial",
            "gap suppression and snapshot recovery pass; a production transport disorder soak has not run",
        ),
        (
            6,
            "checksum mismatch and forced resnapshot",
            "not_applicable",
            "Deribit publishes change continuity identifiers but no checksum on this channel",
        ),
        (
            7,
            "heartbeat timeout and clean reconnect",
            "partial",
            "book state requires a replacement snapshot after a synthetic reconnect; transport heartbeat supervision is not implemented",
        ),
        (
            8,
            "rate-limit and backoff",
            "blocked",
            "public subscription cadence is declared, but production transport enforcement is not implemented",
        ),
        (
            9,
            "schema-drift fail-safe",
            "pass",
            "unknown fields, duplicate keys, unsupported intervals, and route mismatches fail closed",
        ),
        (
            10,
            "clock-skew and timestamp-unit tests",
            "partial",
            "millisecond conversion is checked and the perpetual sentinel is excluded from canonical expiry; clock-skew policy is not implemented",
        ),
        (
            11,
            "burst-load and bounded queue",
            "partial",
            "payload and collection bounds exist; production session queue evidence is unavailable",
        ),
        (
            12,
            "raw-WAL replay equivalence",
            "not_run",
            "focused tests bind parser output to a WAL receipt, but the certifier does not execute a persisted session replay",
        ),
        (
            13,
            "long soak and resubscribe",
            "blocked",
            "production transport and live canary are not implemented",
        ),
        (
            14,
            "live testnet or production canary",
            "blocked",
            "no live exchange acceptance was requested or recorded",
        ),
        (
            15,
            "terms and redistribution review",
            "pass",
            "manifest records reviewed-restricted terms and repository-owned synthetic fixture provenance",
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
    if !matches!(venue, "binance" | "bybit" | "deribit" | "kraken") {
        return Err("unsupported fixture venue".to_owned());
    }
    let workspace = compiled_workspace_root()?;
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

fn compiled_workspace_root() -> Result<PathBuf, String> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "certification app is not inside a workspace".to_owned())?
        .canonicalize()
        .map_err(|error| format!("could not resolve compiled workspace: {error}"))?;
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
