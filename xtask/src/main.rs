use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitCode, ExitStatus},
};

use serde::Deserialize;
use thiserror::Error;

const UNKNOWN_COMMAND_EXIT_CODE: u8 = 2;

#[derive(Debug, Error)]
enum XtaskError {
    #[error("unknown command: {command}")]
    UnknownCommand { command: String },
    #[error("expected exactly one command")]
    InvalidInvocation,
    #[error("could not start cargo metadata: {source}")]
    MetadataSpawn {
        #[source]
        source: io::Error,
    },
    #[error("cargo metadata failed with {status}: {stderr}")]
    MetadataFailed { status: ExitStatus, stderr: String },
    #[error("could not parse cargo metadata: {source}")]
    MetadataParse {
        #[source]
        source: serde_json::Error,
    },
    #[error("could not read active member manifest {path}: {source}")]
    ManifestRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse active member manifest {path}: {source}")]
    ManifestParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("active member manifest {path} must opt into workspace lints")]
    MissingWorkspaceLints { path: PathBuf },
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<MetadataPackage>,
    workspace_members: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
struct MetadataPackage {
    id: String,
    manifest_path: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
enum ManifestLintPolicy {
    Compliant,
    MissingLints,
    WorkspaceLintsDisabled,
    MalformedWorkspaceLints,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(XtaskError::UnknownCommand { command }) => {
            eprintln!("error[unknown-command]: {command}");
            ExitCode::from(UNKNOWN_COMMAND_EXIT_CODE)
        }
        Err(error) => {
            eprintln!("error[xtask]: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), XtaskError> {
    let arguments = env::args_os().skip(1).collect::<Vec<OsString>>();
    let [command] = arguments.as_slice() else {
        return Err(XtaskError::InvalidInvocation);
    };

    match command.to_string_lossy().as_ref() {
        "help" => {
            print_help();
            Ok(())
        }
        "workspace-check" => workspace_check(),
        command => Err(XtaskError::UnknownCommand {
            command: command.to_owned(),
        }),
    }
}

fn print_help() {
    println!("xtask commands:\n  help\n  workspace-check");
}

fn workspace_check() -> Result<(), XtaskError> {
    let metadata = read_metadata()?;

    for package in metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
    {
        ensure_workspace_lints(&package.manifest_path)?;
    }

    println!("workspace-check: ok");
    Ok(())
}

fn read_metadata() -> Result<CargoMetadata, XtaskError> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must be nested in the workspace");
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .args(["metadata", "--locked", "--format-version", "1"])
        .current_dir(workspace_root)
        .output()
        .map_err(|source| XtaskError::MetadataSpawn { source })?;

    if !output.status.success() {
        return Err(XtaskError::MetadataFailed {
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    serde_json::from_slice(&output.stdout).map_err(|source| XtaskError::MetadataParse { source })
}

fn ensure_workspace_lints(manifest_path: &Path) -> Result<(), XtaskError> {
    let manifest =
        fs::read_to_string(manifest_path).map_err(|source| XtaskError::ManifestRead {
            path: manifest_path.to_path_buf(),
            source,
        })?;
    let value =
        toml::from_str::<toml::Value>(&manifest).map_err(|source| XtaskError::ManifestParse {
            path: manifest_path.to_path_buf(),
            source,
        })?;
    if manifest_lint_policy(&value) == ManifestLintPolicy::Compliant {
        Ok(())
    } else {
        Err(XtaskError::MissingWorkspaceLints {
            path: manifest_path.to_path_buf(),
        })
    }
}

fn manifest_lint_policy(manifest: &toml::Value) -> ManifestLintPolicy {
    let Some(lints) = manifest.get("lints") else {
        return ManifestLintPolicy::MissingLints;
    };
    let Some(lints) = lints.as_table() else {
        return ManifestLintPolicy::MalformedWorkspaceLints;
    };

    match lints.get("workspace") {
        Some(toml::Value::Boolean(true)) => ManifestLintPolicy::Compliant,
        Some(toml::Value::Boolean(false)) => ManifestLintPolicy::WorkspaceLintsDisabled,
        Some(_) => ManifestLintPolicy::MalformedWorkspaceLints,
        None => ManifestLintPolicy::MissingLints,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(source: &str) -> toml::Value {
        toml::from_str(source).expect("test manifest must parse")
    }

    #[test]
    fn manifest_lint_policy_accepts_workspace_lints() {
        assert_eq!(
            manifest_lint_policy(&manifest("[lints]\nworkspace = true")),
            ManifestLintPolicy::Compliant
        );
    }

    #[test]
    fn manifest_lint_policy_rejects_missing_lints() {
        assert_eq!(
            manifest_lint_policy(&manifest("[package]\nname = 'fixture'")),
            ManifestLintPolicy::MissingLints
        );
    }

    #[test]
    fn manifest_lint_policy_rejects_disabled_workspace_lints() {
        assert_eq!(
            manifest_lint_policy(&manifest("[lints]\nworkspace = false")),
            ManifestLintPolicy::WorkspaceLintsDisabled
        );
    }

    #[test]
    fn manifest_lint_policy_rejects_malformed_workspace_lints() {
        assert_eq!(
            manifest_lint_policy(&manifest("[lints]\nworkspace = 'true'")),
            ManifestLintPolicy::MalformedWorkspaceLints
        );
    }
}
