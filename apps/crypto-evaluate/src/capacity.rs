use std::{
    io::{self, Write as _},
    path::PathBuf,
    process::ExitCode,
};

use capacity::{AdmissionDecision, CapacityError, CapacityInput, admit};
use clap::{Args, ValueEnum};
use thiserror::Error;

const MAXIMUM_INPUT_BYTES: usize = 1024 * 1024;
const EVIDENCE_REQUIRED_EXIT: u8 = 3;
const REJECTED_EXIT: u8 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum OutputFormat {
    Json,
    JsonPretty,
}

#[derive(Debug, Args)]
pub struct CapacityArguments {
    #[arg(long)]
    input: PathBuf,
    #[arg(long, value_enum, default_value = "json")]
    format: OutputFormat,
}

pub fn run(arguments: CapacityArguments) -> Result<ExitCode, CliError> {
    let input = config::read_bounded_file(&arguments.input, MAXIMUM_INPUT_BYTES)
        .map_err(CliError::Input)?;
    let input = serde_json::from_str::<CapacityInput>(&input).map_err(CliError::Decode)?;
    let decision = admit(&input)?;
    let output = match arguments.format {
        OutputFormat::Json => serde_json::to_vec(&decision),
        OutputFormat::JsonPretty => serde_json::to_vec_pretty(&decision),
    }
    .map_err(CliError::Encode)?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&output).map_err(CliError::Output)?;
    stdout.write_all(b"\n").map_err(CliError::Output)?;
    Ok(match decision {
        AdmissionDecision::EvidenceRequired { .. } => ExitCode::from(EVIDENCE_REQUIRED_EXIT),
        AdmissionDecision::Rejected { .. } => ExitCode::from(REJECTED_EXIT),
    })
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("capacity input could not be read safely")]
    Input(#[source] config::ConfigError),
    #[error("capacity input is not valid versioned JSON")]
    Decode(#[source] serde_json::Error),
    #[error("capacity calculation failed: {0}")]
    Capacity(#[from] CapacityError),
    #[error("capacity decision could not be encoded")]
    Encode(#[source] serde_json::Error),
    #[error("capacity decision could not be written")]
    Output(#[source] io::Error),
}
