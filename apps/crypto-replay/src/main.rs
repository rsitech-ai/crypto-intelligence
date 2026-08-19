use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::ExitCode};

use clap::Parser;
use feature_engine::{MaterializationMode, Materializer};
use replay_engine::{ReplayConfig, ReplayRunner};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Replay a bounded golden market-data run deterministically")]
struct Arguments {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long, conflicts_with = "verify_features")]
    verify: bool,
    #[arg(long, conflicts_with = "verify")]
    verify_features: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum VerifySummary {
    Foundation(FoundationVerifySummary),
    Features(FeatureVerifySummary),
}

#[derive(Debug, Serialize)]
struct FoundationVerifySummary {
    schema_version: u32,
    status: &'static str,
    normalized_event_hash: String,
    book_state_hash: String,
    quality_incident_hash: String,
    replayed_records: usize,
    normalized_events: usize,
    quality_incidents: usize,
    silent_integrity_failures: u64,
    logical_elapsed_ns: u64,
}

#[derive(Debug, Serialize)]
struct FeatureVerifySummary {
    schema_version: u32,
    status: &'static str,
    fixed_point_hash: String,
    lineage_hash: String,
    finality_quality_hash: String,
    wal_segment_blake3: String,
    parquet_manifest_blake3: String,
    replayed_feature_rows: usize,
    float_max_abs_error: f64,
}

fn main() -> ExitCode {
    match run(Arguments::parse()) {
        Ok(summary) => match serde_json::to_string(&summary) {
            Ok(encoded) => {
                println!("{encoded}");
                ExitCode::SUCCESS
            }
            Err(error) => fail(format!("summary encoding failed: {error}")),
        },
        Err(error) => fail(error),
    }
}

fn run(arguments: Arguments) -> Result<VerifySummary, String> {
    if arguments.verify_features {
        return verify_features(arguments.manifest);
    }
    if !arguments.verify {
        return Err("--verify or --verify-features is required".to_owned());
    }
    let config = ReplayConfig::from_manifest(&arguments.manifest)
        .map_err(|error| format!("manifest rejected: {error}"))?;
    let digest =
        ReplayRunner::run_to_digest(&config).map_err(|error| format!("replay failed: {error}"))?;
    if !digest.matches_expected(config.expected()) {
        return Err(format!(
            "digest mismatch: events={} book={} quality={} records={} normalized={} incidents={} silent_failures={} elapsed_ns={}",
            digest.normalized_event_hash_hex(),
            digest.book_state_hash_hex(),
            digest.quality_incident_hash_hex(),
            digest.replayed_records(),
            digest.normalized_events(),
            digest.quality_incidents(),
            digest.silent_integrity_failures(),
            digest.logical_elapsed_ns(),
        ));
    }
    Ok(VerifySummary::Foundation(FoundationVerifySummary {
        schema_version: 1,
        status: "pass",
        normalized_event_hash: digest.normalized_event_hash_hex(),
        book_state_hash: digest.book_state_hash_hex(),
        quality_incident_hash: digest.quality_incident_hash_hex(),
        replayed_records: digest.replayed_records(),
        normalized_events: digest.normalized_events(),
        quality_incidents: digest.quality_incidents(),
        silent_integrity_failures: digest.silent_integrity_failures(),
        logical_elapsed_ns: digest.logical_elapsed_ns(),
    }))
}

fn verify_features(manifest: PathBuf) -> Result<VerifySummary, String> {
    let live_root = private_root("cmti-feature-live")?;
    let wal_root = private_root("cmti-feature-wal")?;
    let parquet_root = private_root("cmti-feature-parquet")?;
    let live = Materializer::run_fixture(
        &manifest,
        MaterializationMode::LiveSimulation,
        live_root.path(),
    )
    .map_err(|error| format!("live feature materialization failed: {error}"))?;
    let wal = Materializer::run_fixture(&manifest, MaterializationMode::WalReplay, wal_root.path())
        .map_err(|error| format!("WAL feature materialization failed: {error}"))?;
    let parquet = Materializer::run_fixture(
        &manifest,
        MaterializationMode::ParquetReplay,
        parquet_root.path(),
    )
    .map_err(|error| format!("Parquet feature materialization failed: {error}"))?;

    let float_max_abs_error = live
        .float_max_abs_error(&parquet)
        .map_err(|error| format!("floating feature parity failed: {error}"))?;
    if live.rows() != wal.rows()
        || wal.rows() != parquet.rows()
        || live.fixed_point_digest() != wal.fixed_point_digest()
        || wal.fixed_point_digest() != parquet.fixed_point_digest()
        || live.lineage_digest() != parquet.lineage_digest()
        || live.finality_quality_digest() != parquet.finality_quality_digest()
        || float_max_abs_error > 1e-12
    {
        return Err(format!(
            "feature parity mismatch: rows={} float_max_abs_error={float_max_abs_error}",
            live.rows().len()
        ));
    }

    let wal_segment_blake3 = wal
        .wal_segment_blake3()
        .filter(|digest| *digest != [0; 32])
        .ok_or_else(|| "WAL replay did not produce durable segment evidence".to_owned())?;
    let parquet_manifest_blake3 = parquet
        .sealed_manifest_blake3()
        .filter(|digest| *digest != [0; 32])
        .ok_or_else(|| "Parquet replay did not produce durable manifest evidence".to_owned())?;
    Ok(VerifySummary::Features(FeatureVerifySummary {
        schema_version: 1,
        status: "pass",
        fixed_point_hash: hex::encode(live.fixed_point_digest()),
        lineage_hash: hex::encode(live.lineage_digest()),
        finality_quality_hash: hex::encode(live.finality_quality_digest()),
        wal_segment_blake3: hex::encode(wal_segment_blake3),
        parquet_manifest_blake3: hex::encode(parquet_manifest_blake3),
        replayed_feature_rows: live.rows().len(),
        float_max_abs_error,
    }))
}

fn private_root(prefix: &str) -> Result<tempfile::TempDir, String> {
    let root = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .map_err(|error| format!("private replay root failed: {error}"))?;
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("private replay permissions failed: {error}"))?;
    Ok(root)
}

fn fail(error: String) -> ExitCode {
    eprintln!("crypto-replay: FAIL: {error}");
    ExitCode::FAILURE
}
