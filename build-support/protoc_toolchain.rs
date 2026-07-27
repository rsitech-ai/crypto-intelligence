use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ProtoToolchainContract {
    // The build-script consumer needs only protoc; xtask also validates Buf.
    #[allow(dead_code)]
    pub buf: String,
    pub protoc: String,
}

#[derive(Debug)]
pub struct ValidatedProtoc {
    path: PathBuf,
}

impl ValidatedProtoc {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug)]
pub enum ProtocToolchainError {
    ContractRead {
        path: PathBuf,
        source: io::Error,
    },
    ContractParse {
        path: PathBuf,
        source: toml::de::Error,
    },
    PathMissing,
    ExecutableNotFound {
        requested: OsString,
    },
    Canonicalize {
        path: PathBuf,
        source: io::Error,
    },
    OverrideMismatch {
        path_protoc: PathBuf,
        override_protoc: PathBuf,
    },
    VersionSpawn {
        path: PathBuf,
        source: io::Error,
    },
    VersionCommandFailed {
        path: PathBuf,
        status: ExitStatus,
        stderr: String,
    },
    VersionMismatch {
        expected: String,
        actual: String,
    },
}

impl fmt::Display for ProtocToolchainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContractRead { path, source } => write!(
                formatter,
                "could not read protobuf toolchain contract {}: {source}",
                path.display()
            ),
            Self::ContractParse { path, source } => write!(
                formatter,
                "could not parse protobuf toolchain contract {}: {source}",
                path.display()
            ),
            Self::PathMissing => formatter.write_str("PATH is missing while resolving protoc"),
            Self::ExecutableNotFound { requested } => {
                write!(
                    formatter,
                    "could not resolve protoc executable {requested:?}"
                )
            }
            Self::Canonicalize { path, source } => write!(
                formatter,
                "could not canonicalize protoc executable {}: {source}",
                path.display()
            ),
            Self::OverrideMismatch {
                path_protoc,
                override_protoc,
            } => write!(
                formatter,
                "PROTOC override does not match the PATH protoc: PATH resolved {}, override resolved {}",
                path_protoc.display(),
                override_protoc.display()
            ),
            Self::VersionSpawn { path, source } => write!(
                formatter,
                "could not run protoc version check at {}: {source}",
                path.display()
            ),
            Self::VersionCommandFailed {
                path,
                status,
                stderr,
            } => write!(
                formatter,
                "protoc version check at {} failed with {status}: {stderr}",
                path.display()
            ),
            Self::VersionMismatch { expected, actual } => write!(
                formatter,
                "protoc version mismatch: expected {expected:?}, got {actual:?}"
            ),
        }
    }
}

impl Error for ProtocToolchainError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ContractRead { source, .. }
            | Self::Canonicalize { source, .. }
            | Self::VersionSpawn { source, .. } => Some(source),
            Self::ContractParse { source, .. } => Some(source),
            Self::PathMissing
            | Self::ExecutableNotFound { .. }
            | Self::OverrideMismatch { .. }
            | Self::VersionCommandFailed { .. }
            | Self::VersionMismatch { .. } => None,
        }
    }
}

pub fn read_contract(
    workspace_root: &Path,
) -> Result<ProtoToolchainContract, ProtocToolchainError> {
    let path = workspace_root.join("proto/toolchain.toml");
    let source =
        fs::read_to_string(&path).map_err(|source| ProtocToolchainError::ContractRead {
            path: path.clone(),
            source,
        })?;
    toml::from_str(&source).map_err(|source| ProtocToolchainError::ContractParse { path, source })
}

pub fn resolve_and_validate(
    contract: &ProtoToolchainContract,
) -> Result<ValidatedProtoc, ProtocToolchainError> {
    let path_value = env::var_os("PATH").ok_or(ProtocToolchainError::PathMissing)?;
    let path_protoc = resolve_from_path(OsStr::new(executable_name()), &path_value)?;

    if let Some(protoc_override) = env::var_os("PROTOC") {
        let override_protoc = resolve_requested(&protoc_override, &path_value)?;
        if override_protoc != path_protoc {
            return Err(ProtocToolchainError::OverrideMismatch {
                path_protoc,
                override_protoc,
            });
        }
    }

    let output = Command::new(&path_protoc)
        .arg("--version")
        .output()
        .map_err(|source| ProtocToolchainError::VersionSpawn {
            path: path_protoc.clone(),
            source,
        })?;
    if !output.status.success() {
        return Err(ProtocToolchainError::VersionCommandFailed {
            path: path_protoc,
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    validate_version_output(&contract.protoc, &output.stdout)?;
    Ok(ValidatedProtoc { path: path_protoc })
}

pub fn validate_version_output(expected: &str, stdout: &[u8]) -> Result<(), ProtocToolchainError> {
    let actual = String::from_utf8_lossy(stdout).trim().to_owned();
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocToolchainError::VersionMismatch {
            expected: expected.to_owned(),
            actual,
        })
    }
}

fn resolve_requested(
    requested: &OsStr,
    path_value: &OsStr,
) -> Result<PathBuf, ProtocToolchainError> {
    let path = Path::new(requested);
    if path.components().count() == 1 {
        resolve_from_path(requested, path_value)
    } else {
        canonicalize_executable(path)
    }
}

fn resolve_from_path(
    executable: &OsStr,
    path_value: &OsStr,
) -> Result<PathBuf, ProtocToolchainError> {
    env::split_paths(path_value)
        .map(|directory| directory.join(executable))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| ProtocToolchainError::ExecutableNotFound {
            requested: executable.to_owned(),
        })
        .and_then(|path| canonicalize_executable(&path))
}

fn canonicalize_executable(path: &Path) -> Result<PathBuf, ProtocToolchainError> {
    fs::canonicalize(path).map_err(|source| ProtocToolchainError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })
}

const fn executable_name() -> &'static str {
    if cfg!(windows) {
        "protoc.exe"
    } else {
        "protoc"
    }
}
