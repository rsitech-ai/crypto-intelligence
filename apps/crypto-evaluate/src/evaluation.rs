use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    os::unix::fs::PermissionsExt as _,
    path::{Component, Path, PathBuf},
    process::ExitCode,
};

use clap::Args;
use serde::Serialize;
use thiserror::Error;

const MAXIMUM_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
const MAXIMUM_OBSERVATIONS: usize = 1_000_000;

#[derive(Debug, Args)]
pub struct EvaluationArguments {
    #[arg(long)]
    pub manifest: PathBuf,
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
struct FeatureDefinition {
    feature_id: String,
    feature_version: String,
    formula_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct AttemptedDefinition {
    schema_version: u32,
    definition_id: &'static str,
    model_families: Vec<&'static str>,
    calibration_methods: Vec<&'static str>,
    feature_definitions: Vec<FeatureDefinition>,
    hyperparameter_policy: &'static str,
    fold_policy: &'static str,
}

#[derive(Serialize)]
struct ModelCard<'a> {
    schema_version: u32,
    title: &'static str,
    evidence_boundary: &'static str,
    input_artifact_id: &'a str,
    limitations: [&'static str; 3],
}

#[derive(Serialize)]
struct EvaluationReport<'a> {
    schema_version: u32,
    report_type: &'static str,
    status: &'static str,
    candidate_eligibility: &'static str,
    input_artifact_id: &'a str,
    input_manifest_blake3: String,
    source_code_commit: &'a str,
    dataset_version: i64,
    normalization_version: &'a str,
    observation_count: usize,
    latest_as_known_at_ns: i64,
    attempted_definition_blake3: String,
    fold_metrics: [serde_json::Value; 0],
    reliability: [serde_json::Value; 0],
    ablations: [serde_json::Value; 0],
    model_card_blake3: String,
    candidate_package_blake3: Option<String>,
    hold_reasons: [&'static str; 3],
}

#[derive(Serialize)]
struct LedgerEntry<'a> {
    schema_version: u32,
    attempt_id: String,
    input_manifest_blake3: &'a str,
    attempted_definition_blake3: &'a str,
    outcome: &'static str,
    candidate_created: bool,
    failure_code: &'static str,
}

#[derive(Serialize)]
struct BundleManifest {
    schema_version: u32,
    status: &'static str,
    files_blake3: BTreeMap<&'static str, String>,
}

pub fn run(arguments: EvaluationArguments) -> Result<ExitCode, EvaluationError> {
    validate_output_target(&arguments.output)?;
    let input = config::read_bounded_file(&arguments.manifest, MAXIMUM_MANIFEST_BYTES)
        .map_err(EvaluationError::Input)?;
    let parsed = input
        .parse::<toml::Value>()
        .map_err(EvaluationError::Decode)?;
    let table = parsed
        .as_table()
        .ok_or(EvaluationError::InvalidManifest("root"))?;
    require_integer(table, "schema_version", 1)?;
    let artifact_id = require_string(table, "artifact_id")?;
    let code_commit = require_hex(table, "code_commit", 40)?;
    let normalization_version = require_string(table, "normalization_version")?;
    let dataset_version = require_positive_integer(table, "dataset_version")?;
    let declared_maximum =
        usize::try_from(require_positive_integer(table, "maximum_observations")?)
            .map_err(|_| EvaluationError::ObservationCapacity)?;
    if declared_maximum > MAXIMUM_OBSERVATIONS {
        return Err(EvaluationError::ObservationCapacity);
    }
    let observations = table
        .get("observation")
        .and_then(toml::Value::as_array)
        .ok_or(EvaluationError::InvalidManifest("observation"))?;
    if observations.is_empty()
        || observations.len() > declared_maximum
        || observations.len() > MAXIMUM_OBSERVATIONS
    {
        return Err(EvaluationError::ObservationCapacity);
    }

    let mut definitions = BTreeSet::new();
    let mut latest_as_known_at_ns = 0_i64;
    for observation in observations {
        let observation = observation
            .as_table()
            .ok_or(EvaluationError::InvalidManifest("observation"))?;
        let event_time_start_ns = require_positive_integer(observation, "event_time_start_ns")?;
        let event_time_end_ns = require_positive_integer(observation, "event_time_end_ns")?;
        let as_known_at_ns = require_positive_integer(observation, "as_known_at_ns")?;
        if event_time_end_ns <= event_time_start_ns || as_known_at_ns < event_time_end_ns {
            return Err(EvaluationError::PointInTimeViolation);
        }
        latest_as_known_at_ns = latest_as_known_at_ns.max(as_known_at_ns);
        definitions.insert(FeatureDefinition {
            feature_id: require_string(observation, "feature_id")?.to_owned(),
            feature_version: require_string(observation, "feature_version")?.to_owned(),
            formula_hash: require_hex(observation, "formula_hash", 64)?.to_owned(),
        });
    }
    let attempted = AttemptedDefinition {
        schema_version: 1,
        definition_id: "phase-02-baseline-evaluation-v1",
        model_families: vec![
            "base-rate",
            "ewma",
            "har-rv",
            "bocpd",
            "student-t-hmm",
            "competing-risk-hazard",
        ],
        calibration_methods: vec!["platt-log-odds-v1", "beta-monotone-v1", "pav-isotonic-v1"],
        feature_definitions: definitions.into_iter().collect(),
        hyperparameter_policy: "predeclared-bounded-walk-forward-v1",
        fold_policy: "fit-then-calibration-then-freeze-then-untouched-test-v1",
    };
    let attempted_bytes = encode_json_line(&attempted).map_err(EvaluationError::Encode)?;
    let attempted_blake3 = hash_hex(&attempted_bytes);
    let input_blake3 = hash_hex(input.as_bytes());
    let card = ModelCard {
        schema_version: 1,
        title: "Crypto Intelligence baseline evaluation evidence",
        evidence_boundary: "fixture-only; no production quality or promotion claim",
        input_artifact_id: artifact_id,
        limitations: [
            "the input contains feature observations but no binary outcomes",
            "no non-overlapping fit, calibration, freeze, and outer-test folds are present",
            "fold metrics, reliability, ablations, and a signed candidate package are therefore withheld",
        ],
    };
    let card_bytes = encode_json_line(&card).map_err(EvaluationError::Encode)?;
    let card_blake3 = hash_hex(&card_bytes);
    let report = EvaluationReport {
        schema_version: 1,
        report_type: "walk_forward_evaluation",
        status: "evidence_only",
        candidate_eligibility: "held",
        input_artifact_id: artifact_id,
        input_manifest_blake3: input_blake3.clone(),
        source_code_commit: code_commit,
        dataset_version,
        normalization_version,
        observation_count: observations.len(),
        latest_as_known_at_ns,
        attempted_definition_blake3: attempted_blake3.clone(),
        fold_metrics: [],
        reliability: [],
        ablations: [],
        model_card_blake3: card_blake3.clone(),
        candidate_package_blake3: None,
        hold_reasons: [
            "missing_labeled_outcomes",
            "missing_non_overlapping_folds",
            "missing_signed_candidate_package",
        ],
    };
    let report_bytes = encode_json_line(&report).map_err(EvaluationError::Encode)?;
    let ledger = LedgerEntry {
        schema_version: 1,
        attempt_id: hash_hex(format!("{input_blake3}\0{attempted_blake3}").as_bytes()),
        input_manifest_blake3: &input_blake3,
        attempted_definition_blake3: &attempted_blake3,
        outcome: "held",
        candidate_created: false,
        failure_code: "missing_labeled_walk_forward_folds",
    };
    let ledger_bytes = encode_json_line(&ledger).map_err(EvaluationError::Encode)?;
    let mut file_hashes = BTreeMap::new();
    file_hashes.insert("attempted-definition.json", hash_hex(&attempted_bytes));
    file_hashes.insert("evaluation-report.json", hash_hex(&report_bytes));
    file_hashes.insert("experiment-ledger.jsonl", hash_hex(&ledger_bytes));
    file_hashes.insert("model-card.json", card_blake3);
    let bundle = BundleManifest {
        schema_version: 1,
        status: "evidence_only",
        files_blake3: file_hashes,
    };
    let bundle_bytes = encode_json_line(&bundle).map_err(EvaluationError::Encode)?;

    write_bundle(
        &arguments.output,
        [
            ("attempted-definition.json", attempted_bytes.as_slice()),
            ("evaluation-report.json", report_bytes.as_slice()),
            ("experiment-ledger.jsonl", ledger_bytes.as_slice()),
            ("model-card.json", card_bytes.as_slice()),
            ("manifest.json", bundle_bytes.as_slice()),
        ],
    )?;
    println!(
        "crypto-evaluate: EVIDENCE_ONLY candidate=HELD observations={} manifest_blake3={}",
        observations.len(),
        input_blake3
    );
    Ok(ExitCode::SUCCESS)
}

fn validate_output_target(output: &Path) -> Result<(), EvaluationError> {
    if output.as_os_str().is_empty()
        || !matches!(output.components().next_back(), Some(Component::Normal(_)))
    {
        return Err(EvaluationError::InvalidOutput);
    }
    match fs::symlink_metadata(output) {
        Ok(_) => return Err(EvaluationError::OutputExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(EvaluationError::Output(error)),
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    validate_existing_output_ancestor(parent)?;
    fs::create_dir_all(parent).map_err(EvaluationError::Output)?;
    let metadata = fs::symlink_metadata(parent).map_err(EvaluationError::Output)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(EvaluationError::InvalidOutput);
    }
    Ok(())
}

fn validate_existing_output_ancestor(directory: &Path) -> Result<(), EvaluationError> {
    if directory
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::ParentDir))
    {
        return Err(EvaluationError::InvalidOutput);
    }
    let mut ancestor = Some(directory);
    while let Some(path) = ancestor {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(EvaluationError::InvalidOutput);
            }
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = path.parent();
            }
            Err(error) => return Err(EvaluationError::Output(error)),
        }
    }
    Err(EvaluationError::InvalidOutput)
}

fn write_bundle<const N: usize>(
    output: &Path,
    files: [(&str, &[u8]); N],
) -> Result<(), EvaluationError> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::Builder::new()
        .prefix(".crypto-evaluate-")
        .tempdir_in(parent)
        .map_err(EvaluationError::Output)?;
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
        .map_err(EvaluationError::Output)?;
    for (name, bytes) in files {
        let path = temporary.path().join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(EvaluationError::Output)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(EvaluationError::Output)?;
        file.write_all(bytes).map_err(EvaluationError::Output)?;
        file.sync_all().map_err(EvaluationError::Output)?;
    }
    File::open(temporary.path())
        .and_then(|directory| directory.sync_all())
        .map_err(EvaluationError::Output)?;
    let temporary_path = temporary.keep();
    fs::rename(&temporary_path, output).map_err(|error| {
        let _ = fs::remove_dir_all(&temporary_path);
        EvaluationError::Output(error)
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(EvaluationError::Output)?;
    Ok(())
}

fn encode_json_line<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut encoded = serde_json::to_vec(value)?;
    encoded.push(b'\n');
    Ok(encoded)
}

fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn require_string<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    field: &'static str,
) -> Result<&'a str, EvaluationError> {
    let value = table
        .get(field)
        .and_then(toml::Value::as_str)
        .ok_or(EvaluationError::InvalidManifest(field))?;
    if value.is_empty() || value.len() > 1_024 || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(EvaluationError::InvalidManifest(field));
    }
    Ok(value)
}

fn require_hex<'a>(
    table: &'a toml::map::Map<String, toml::Value>,
    field: &'static str,
    length: usize,
) -> Result<&'a str, EvaluationError> {
    let value = require_string(table, field)?;
    if value.len() != length
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(EvaluationError::InvalidManifest(field));
    }
    Ok(value)
}

fn require_positive_integer(
    table: &toml::map::Map<String, toml::Value>,
    field: &'static str,
) -> Result<i64, EvaluationError> {
    let value = table
        .get(field)
        .and_then(toml::Value::as_integer)
        .ok_or(EvaluationError::InvalidManifest(field))?;
    if value <= 0 {
        return Err(EvaluationError::InvalidManifest(field));
    }
    Ok(value)
}

fn require_integer(
    table: &toml::map::Map<String, toml::Value>,
    field: &'static str,
    expected: i64,
) -> Result<(), EvaluationError> {
    if table.get(field).and_then(toml::Value::as_integer) == Some(expected) {
        Ok(())
    } else {
        Err(EvaluationError::InvalidManifest(field))
    }
}

#[derive(Debug, Error)]
pub enum EvaluationError {
    #[error("input manifest could not be read safely")]
    Input(#[source] config::ConfigError),
    #[error("input manifest is not valid TOML")]
    Decode(#[source] toml::de::Error),
    #[error("input manifest field `{0}` is invalid")]
    InvalidManifest(&'static str),
    #[error("input observation count exceeds its bounded contract")]
    ObservationCapacity,
    #[error("input feature violates the point-in-time knowledge boundary")]
    PointInTimeViolation,
    #[error("output target or its direct parent is invalid")]
    InvalidOutput,
    #[error("output target already exists")]
    OutputExists,
    #[error("evaluation evidence could not be encoded")]
    Encode(#[source] serde_json::Error),
    #[error("evaluation evidence could not be committed atomically")]
    Output(#[source] io::Error),
}
