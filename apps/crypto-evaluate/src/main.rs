mod capacity;
mod cusp_report;
mod evaluation;

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(name = "crypto-evaluate")]
struct Arguments {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, value_name = "PATH", requires = "output")]
    manifest: Option<std::path::PathBuf>,
    #[arg(long, value_name = "DIRECTORY", requires = "manifest")]
    output: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Capacity(capacity::CapacityArguments),
    CuspAblation(cusp_report::CuspAblationArguments),
    Evaluate(evaluation::EvaluationArguments),
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let result = match (arguments.command, arguments.manifest, arguments.output) {
        (Some(Command::Capacity(arguments)), None, None) => {
            capacity::run(arguments).map_err(ApplicationError::Capacity)
        }
        (Some(Command::CuspAblation(arguments)), None, None) => {
            cusp_report::run(arguments).map_err(ApplicationError::CuspReport)
        }
        (Some(Command::Evaluate(arguments)), None, None) => {
            evaluation::run(arguments).map_err(ApplicationError::Evaluation)
        }
        (None, Some(manifest), Some(output)) => {
            evaluation::run(evaluation::EvaluationArguments { manifest, output })
                .map_err(ApplicationError::Evaluation)
        }
        _ => Err(ApplicationError::InvalidArguments),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error[crypto-evaluate]: {error}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Error)]
enum ApplicationError {
    #[error("capacity command failed: {0}")]
    Capacity(#[source] capacity::CliError),
    #[error("cusp ablation command failed: {0}")]
    CuspReport(#[source] cusp_report::CuspReportError),
    #[error("evaluation command failed: {0}")]
    Evaluation(#[source] evaluation::EvaluationError),
    #[error("provide a subcommand or both --manifest and --output")]
    InvalidArguments,
}
