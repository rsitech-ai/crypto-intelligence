use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use replay_engine::{ReplayConfig, ReplayRunner};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(about = "Replay a bounded golden market-data run deterministically")]
struct Arguments {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    verify: bool,
}

#[derive(Debug, Serialize)]
struct VerifySummary {
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
    if !arguments.verify {
        return Err("--verify is required for the foundation replay".to_owned());
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
    Ok(VerifySummary {
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
    })
}

fn fail(error: String) -> ExitCode {
    eprintln!("crypto-replay: FAIL: {error}");
    ExitCode::FAILURE
}
