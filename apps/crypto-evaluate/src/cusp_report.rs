use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    os::unix::ffi::OsStrExt as _,
    os::unix::fs::PermissionsExt as _,
    path::{Component, Path, PathBuf},
    process::ExitCode,
};

use clap::Args;
use cusp::ablation::{
    ABLATION_REPORT_SCHEMA_VERSION, AblationError, AblationReport, AblationReportInput, CuspGate,
    FoldMetricInput, GateError, GateEvaluation, RequiredBaseline,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const INPUT_FILE: &str = "ablation-input.json";
const MAXIMUM_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MODEL_CARD_TEMPLATE: &str = include_str!("../../../docs/model-cards/cusp-template.md");

#[derive(Debug, Args)]
pub struct CuspAblationArguments {
    #[arg(long)]
    pub dataset: PathBuf,
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CuspAblationDatasetInput {
    schema_version: u32,
    dataset_manifest_blake3: String,
    candidate_model_blake3: String,
    baseline_model_blake3: String,
    required_baselines: Vec<RequiredBaseline>,
    folds: Vec<FoldMetricInput>,
    maximum_standardized_coefficient_drift: f64,
    sign_scaling_parity: bool,
    numerical_failures: u32,
    attempted_model_variants: u32,
}

#[derive(Serialize)]
struct CuspAblationArtifact<'a> {
    schema_version: u32,
    report_type: &'static str,
    decision: &'static str,
    production_probability_use: &'static str,
    input_blake3: &'a str,
    dataset_manifest_blake3: String,
    candidate_model_blake3: String,
    baseline_model_blake3: String,
    report_evidence_blake3: String,
    gate_evidence_blake3: String,
    report: &'a AblationReport,
    gate: &'a GateEvaluation,
}

#[derive(Serialize)]
struct ExperimentLedger<'a> {
    schema_version: u32,
    attempt_id: String,
    input_blake3: &'a str,
    candidate_model_blake3: String,
    attempted_model_variants: u32,
    outcome: &'static str,
    production_weight_assigned: bool,
    gate_evidence_blake3: String,
}

#[derive(Serialize)]
struct BundleManifest {
    schema_version: u32,
    decision: &'static str,
    production_probability_use: &'static str,
    files_blake3: BTreeMap<&'static str, String>,
}

pub fn run(arguments: CuspAblationArguments) -> Result<ExitCode, CuspReportError> {
    validate_dataset_directory(&arguments.dataset)?;
    validate_output_target(&arguments.output)?;
    let input_path = arguments.dataset.join(INPUT_FILE);
    let input = config::read_bounded_file(&input_path, MAXIMUM_INPUT_BYTES)
        .map_err(CuspReportError::Input)?;
    let decoded: CuspAblationDatasetInput =
        serde_json::from_str(&input).map_err(CuspReportError::Decode)?;
    if decoded.schema_version != ABLATION_REPORT_SCHEMA_VERSION {
        return Err(CuspReportError::UnsupportedSchema {
            found: decoded.schema_version,
        });
    }
    let report = AblationReport::try_new(AblationReportInput {
        schema_version: decoded.schema_version,
        dataset_manifest_hash: decode_hash(
            "dataset_manifest_blake3",
            &decoded.dataset_manifest_blake3,
        )?,
        candidate_model_hash: decode_hash(
            "candidate_model_blake3",
            &decoded.candidate_model_blake3,
        )?,
        baseline_model_hash: decode_hash("baseline_model_blake3", &decoded.baseline_model_blake3)?,
        required_baselines: decoded.required_baselines,
        folds: decoded.folds,
        maximum_standardized_coefficient_drift: decoded.maximum_standardized_coefficient_drift,
        sign_scaling_parity: decoded.sign_scaling_parity,
        numerical_failures: decoded.numerical_failures,
        attempted_model_variants: decoded.attempted_model_variants,
    })?;
    let gate = CuspGate::default().evaluate(&report)?;
    let decision = gate.decision().as_str();
    let input_blake3 = hash_hex(input.as_bytes());
    let artifact = CuspAblationArtifact {
        schema_version: 1,
        report_type: "cusp_walk_forward_ablation",
        decision,
        production_probability_use: "not_used_in_production_probability",
        input_blake3: &input_blake3,
        dataset_manifest_blake3: hex::encode(report.dataset_manifest_hash()),
        candidate_model_blake3: hex::encode(report.candidate_model_hash()),
        baseline_model_blake3: hex::encode(report.baseline_model_hash()),
        report_evidence_blake3: hex::encode(report.evidence_hash()),
        gate_evidence_blake3: hex::encode(gate.evidence_hash()),
        report: &report,
        gate: &gate,
    };
    let report_bytes = encode_json_line(&artifact)?;
    let model_card = render_model_card(&artifact)?;
    let model_card_bytes = model_card.into_bytes();
    let ledger = ExperimentLedger {
        schema_version: 1,
        attempt_id: hash_hex(
            format!("{}\0{}", input_blake3, hex::encode(gate.evidence_hash())).as_bytes(),
        ),
        input_blake3: &input_blake3,
        candidate_model_blake3: hex::encode(report.candidate_model_hash()),
        attempted_model_variants: report.attempted_model_variants(),
        outcome: decision,
        production_weight_assigned: false,
        gate_evidence_blake3: hex::encode(gate.evidence_hash()),
    };
    let ledger_bytes = encode_json_line(&ledger)?;
    let mut files_blake3 = BTreeMap::new();
    files_blake3.insert("ablation-report.json", hash_hex(&report_bytes));
    files_blake3.insert("experiment-ledger.jsonl", hash_hex(&ledger_bytes));
    files_blake3.insert("model-card.md", hash_hex(&model_card_bytes));
    let manifest = BundleManifest {
        schema_version: 1,
        decision,
        production_probability_use: "not_used_in_production_probability",
        files_blake3,
    };
    let manifest_bytes = encode_json_line(&manifest)?;
    write_bundle(
        &arguments.output,
        [
            ("ablation-report.json", report_bytes.as_slice()),
            ("experiment-ledger.jsonl", ledger_bytes.as_slice()),
            ("model-card.md", model_card_bytes.as_slice()),
            ("manifest.json", manifest_bytes.as_slice()),
        ],
    )?;
    println!(
        "crypto-evaluate: CUSP_ABLATION decision={} production_probability_use=NOT_USED gate_blake3={}",
        decision,
        hex::encode(gate.evidence_hash())
    );
    Ok(ExitCode::SUCCESS)
}

fn render_model_card(artifact: &CuspAblationArtifact<'_>) -> Result<String, CuspReportError> {
    let rendered = MODEL_CARD_TEMPLATE
        .replace("{{decision}}", artifact.decision)
        .replace(
            "{{production_probability_use}}",
            artifact.production_probability_use,
        )
        .replace("{{input_blake3}}", artifact.input_blake3)
        .replace(
            "{{dataset_manifest_blake3}}",
            &artifact.dataset_manifest_blake3,
        )
        .replace(
            "{{candidate_model_blake3}}",
            &artifact.candidate_model_blake3,
        )
        .replace("{{baseline_model_blake3}}", &artifact.baseline_model_blake3)
        .replace(
            "{{report_evidence_blake3}}",
            &artifact.report_evidence_blake3,
        )
        .replace("{{gate_evidence_blake3}}", &artifact.gate_evidence_blake3);
    if rendered.contains("{{") || rendered.contains("}}") {
        return Err(CuspReportError::UnresolvedModelCardTemplate);
    }
    Ok(rendered)
}

fn decode_hash(field: &'static str, value: &str) -> Result<[u8; 32], CuspReportError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CuspReportError::InvalidHash { field });
    }
    let decoded = hex::decode(value).map_err(|_| CuspReportError::InvalidHash { field })?;
    decoded
        .try_into()
        .map_err(|_| CuspReportError::InvalidHash { field })
}

fn validate_dataset_directory(dataset: &Path) -> Result<(), CuspReportError> {
    if dataset.as_os_str().is_empty()
        || dataset
            .components()
            .any(|component| matches!(component, Component::Prefix(_) | Component::ParentDir))
    {
        return Err(CuspReportError::InvalidDataset);
    }
    let metadata = fs::symlink_metadata(dataset).map_err(CuspReportError::Dataset)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CuspReportError::InvalidDataset);
    }
    Ok(())
}

fn validate_output_target(output: &Path) -> Result<(), CuspReportError> {
    if output.as_os_str().is_empty()
        || !matches!(output.components().next_back(), Some(Component::Normal(_)))
    {
        return Err(CuspReportError::InvalidOutput);
    }
    match fs::symlink_metadata(output) {
        Ok(_) => return Err(CuspReportError::OutputExists),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(CuspReportError::Output(error)),
    }
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    validate_existing_output_ancestor(parent)?;
    fs::create_dir_all(parent).map_err(CuspReportError::Output)?;
    let metadata = fs::symlink_metadata(parent).map_err(CuspReportError::Output)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CuspReportError::InvalidOutput);
    }
    Ok(())
}

fn validate_existing_output_ancestor(directory: &Path) -> Result<(), CuspReportError> {
    if directory
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::ParentDir))
    {
        return Err(CuspReportError::InvalidOutput);
    }
    let mut ancestor = Some(directory);
    while let Some(path) = ancestor {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(CuspReportError::InvalidOutput);
            }
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = path.parent();
            }
            Err(error) => return Err(CuspReportError::Output(error)),
        }
    }
    Err(CuspReportError::InvalidOutput)
}

fn write_bundle<const N: usize>(
    output: &Path,
    files: [(&str, &[u8]); N],
) -> Result<(), CuspReportError> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let temporary = tempfile::Builder::new()
        .prefix(".crypto-evaluate-cusp-")
        .tempdir_in(parent)
        .map_err(CuspReportError::Output)?;
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
        .map_err(CuspReportError::Output)?;
    for (name, bytes) in files {
        let path = temporary.path().join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(CuspReportError::Output)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(CuspReportError::Output)?;
        file.write_all(bytes).map_err(CuspReportError::Output)?;
        file.sync_all().map_err(CuspReportError::Output)?;
    }
    File::open(temporary.path())
        .and_then(|directory| directory.sync_all())
        .map_err(CuspReportError::Output)?;
    let temporary_path = temporary.keep();
    rename_noreplace(&temporary_path, output).map_err(|error| {
        let _ = fs::remove_dir_all(&temporary_path);
        CuspReportError::Output(error)
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(CuspReportError::Output)?;
    Ok(())
}

fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    rename_noreplace_platform(&source, &destination)
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn rename_noreplace_platform(source: &CString, destination: &CString) -> io::Result<()> {
    // SAFETY: both pointers come from live `CString` values and remain valid for
    // the duration of this call. `RENAME_EXCL` makes publication no-clobber.
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn rename_noreplace_platform(source: &CString, destination: &CString) -> io::Result<()> {
    // SAFETY: both pointers come from live `CString` values and remain valid for
    // the duration of this call. `RENAME_NOREPLACE` makes publication no-clobber.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace_platform(_source: &CString, _destination: &CString) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-replace publication is unsupported on this platform",
    ))
}

fn encode_json_line<T: Serialize>(value: &T) -> Result<Vec<u8>, CuspReportError> {
    let mut encoded = serde_json::to_vec(value).map_err(CuspReportError::Encode)?;
    encoded.push(b'\n');
    Ok(encoded)
}

fn hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[derive(Debug, Error)]
pub enum CuspReportError {
    #[error("cusp ablation dataset directory is invalid")]
    InvalidDataset,
    #[error("could not inspect cusp ablation dataset: {0}")]
    Dataset(#[source] io::Error),
    #[error("could not read cusp ablation input: {0}")]
    Input(#[source] config::ConfigError),
    #[error("could not decode cusp ablation input: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("unsupported cusp ablation schema version {found}")]
    UnsupportedSchema { found: u32 },
    #[error("cusp ablation {field} must be a lowercase 32-byte BLAKE3 hex digest")]
    InvalidHash { field: &'static str },
    #[error("invalid cusp ablation report: {0}")]
    Ablation(#[from] AblationError),
    #[error("cusp gate evaluation failed: {0}")]
    Gate(#[from] GateError),
    #[error("could not encode cusp ablation artifact: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("cusp model-card template contains an unresolved placeholder")]
    UnresolvedModelCardTemplate,
    #[error("cusp ablation output path is invalid")]
    InvalidOutput,
    #[error("cusp ablation output already exists")]
    OutputExists,
    #[error("could not write cusp ablation output: {0}")]
    Output(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_publication_never_replaces_an_existing_directory() {
        let parent = tempfile::tempdir().expect("parent");
        let source = tempfile::tempdir_in(parent.path()).expect("source");
        let destination = parent.path().join("published");
        fs::create_dir(&destination).expect("destination");
        fs::write(destination.join("owner.txt"), b"original").expect("marker");

        assert!(rename_noreplace(source.path(), &destination).is_err());
        assert!(source.path().is_dir());
        assert_eq!(
            fs::read(destination.join("owner.txt")).expect("marker"),
            b"original"
        );
    }
}
