use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, ExitStatus, Output},
};

use prost::Message as _;
use prost_build::Module;
use prost_types::compiler::{CodeGeneratorRequest, CodeGeneratorResponse, code_generator_response};
use serde::Deserialize;
use thiserror::Error;

const UNKNOWN_COMMAND_EXIT_CODE: u8 = 2;
const PROTO_FILES: [&str; 3] = [
    "common/v1/common.proto",
    "health/v1/health.proto",
    "market/v1/market.proto",
];
const GENERATED_PROTO_FILES: [&str; 3] = [
    "cmti.common.v1.rs",
    "cmti.health.v1.rs",
    "cmti.market.v1.rs",
];

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
    #[error("could not generate configuration schema: {source}")]
    SchemaGenerate {
        #[source]
        source: config::ConfigError,
    },
    #[error("could not read configuration schema {path}: {source}")]
    SchemaRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not write configuration schema {path}: {source}")]
    SchemaWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("checked-in configuration schema is out of date")]
    SchemaOutOfDate,
    #[error("could not start required protobuf tool {tool}: {source}")]
    ProtoToolSpawn {
        tool: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("could not read protobuf toolchain contract {path}: {source}")]
    ProtoToolchainRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse protobuf toolchain contract {path}: {source}")]
    ProtoToolchainParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("protobuf tool {tool} version mismatch: expected {expected:?}, got {actual:?}")]
    ProtoToolVersionMismatch {
        tool: &'static str,
        expected: String,
        actual: String,
    },
    #[error("protobuf command {command} failed with {status}: {stderr}")]
    ProtoToolFailed {
        command: &'static str,
        status: ExitStatus,
        stderr: String,
    },
    #[error("could not prepare protobuf output directory {path}: {source}")]
    ProtoOutputDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("protobuf generation failed for {path}: {reason}")]
    ProtoGenerate { path: PathBuf, reason: String },
    #[error("could not inspect generated protobuf output {path}: {source}")]
    ProtoOutputRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("unexpected protobuf plugin inputs: expected {expected:?}, got {actual:?}")]
    ProtoInputMismatch {
        expected: BTreeSet<String>,
        actual: BTreeSet<String>,
    },
    #[error("unexpected generated protobuf outputs: expected {expected:?}, got {actual:?}")]
    ProtoOutputMismatch {
        expected: BTreeSet<String>,
        actual: BTreeSet<String>,
    },
    #[error("generated protobuf file differs between clean runs: {file}")]
    ProtoNotDeterministic { file: String },
    #[error("could not read protobuf plugin request: {source}")]
    ProtoPluginRead {
        #[source]
        source: io::Error,
    },
    #[error("could not decode protobuf plugin request: {source}")]
    ProtoPluginDecode {
        #[source]
        source: prost::DecodeError,
    },
    #[error("protobuf plugin generation failed: {reason}")]
    ProtoPluginGenerate { reason: String },
    #[error("could not encode protobuf plugin response: {source}")]
    ProtoPluginEncode {
        #[source]
        source: prost::EncodeError,
    },
    #[error("could not write protobuf plugin response: {source}")]
    ProtoPluginWrite {
        #[source]
        source: io::Error,
    },
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

#[derive(Debug, Deserialize)]
struct ProtoToolchainContract {
    buf: String,
    protoc: String,
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
    let Some((command, options)) = arguments.split_first() else {
        return Err(XtaskError::InvalidInvocation);
    };

    match command.to_string_lossy().as_ref() {
        "help" if options.is_empty() => {
            print_help();
            Ok(())
        }
        "workspace-check" if options.is_empty() => workspace_check(),
        "generate-config-schema" => generate_config_schema(options),
        "generate-proto" => generate_proto_command(options),
        "proto-check" if options.is_empty() => proto_check(),
        "protoc-gen-local-api" if options.is_empty() => protoc_gen_local_api(),
        "help" | "proto-check" | "protoc-gen-local-api" | "workspace-check" => {
            Err(XtaskError::InvalidInvocation)
        }
        command => Err(XtaskError::UnknownCommand {
            command: command.to_owned(),
        }),
    }
}

fn print_help() {
    println!(
        "xtask commands:\n  help\n  workspace-check\n  generate-config-schema [--check]\n  generate-proto [--check]\n  proto-check"
    );
}

fn generate_config_schema(options: &[OsString]) -> Result<(), XtaskError> {
    let check = match options {
        [] => false,
        [option] if option == "--check" => true,
        _ => return Err(XtaskError::InvalidInvocation),
    };
    let generated =
        config::schema::generate().map_err(|source| XtaskError::SchemaGenerate { source })?;
    let path = workspace_root().join("configs/schema.json");

    if check {
        let checked_in = fs::read_to_string(&path).map_err(|source| XtaskError::SchemaRead {
            path: path.clone(),
            source,
        })?;
        if checked_in != generated {
            return Err(XtaskError::SchemaOutOfDate);
        }
        println!("generate-config-schema: up to date");
        return Ok(());
    }

    fs::write(&path, generated).map_err(|source| XtaskError::SchemaWrite {
        path: path.clone(),
        source,
    })?;
    println!("generate-config-schema: wrote {}", path.display());
    Ok(())
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

fn generate_proto_command(options: &[OsString]) -> Result<(), XtaskError> {
    let check = match options {
        [] => false,
        [option] if option == "--check" => true,
        _ => return Err(XtaskError::InvalidInvocation),
    };
    generate_proto(check)
}

fn proto_check() -> Result<(), XtaskError> {
    let toolchain = read_proto_toolchain_contract()?;
    ensure_proto_tool("buf", &toolchain.buf)?;
    ensure_proto_tool("protoc", &toolchain.protoc)?;
    run_proto_tool("buf lint proto", "buf", &["lint", "proto"])?;
    run_proto_tool("buf build proto", "buf", &["build", "proto"])?;
    generate_proto_with_toolchain(true, &toolchain)?;
    println!("proto-check: ok");
    Ok(())
}

fn generate_proto(check: bool) -> Result<(), XtaskError> {
    let toolchain = read_proto_toolchain_contract()?;
    ensure_proto_tool("protoc", &toolchain.protoc)?;
    generate_proto_with_toolchain(check, &toolchain)
}

fn generate_proto_with_toolchain(
    check: bool,
    _toolchain: &ProtoToolchainContract,
) -> Result<(), XtaskError> {
    if check {
        let first = tempfile::Builder::new()
            .prefix("cmti-proto-first-")
            .tempdir()
            .map_err(|source| XtaskError::ProtoOutputDirectory {
                path: env::temp_dir(),
                source,
            })?;
        let second = tempfile::Builder::new()
            .prefix("cmti-proto-second-")
            .tempdir()
            .map_err(|source| XtaskError::ProtoOutputDirectory {
                path: env::temp_dir(),
                source,
            })?;
        generate_proto_into(first.path())?;
        generate_proto_into(second.path())?;
        compare_generated_outputs(first.path(), second.path())?;
        println!(
            "generate-proto: reproducible ({} files)",
            GENERATED_PROTO_FILES.len()
        );
        return Ok(());
    }

    let output = workspace_root().join("target/generated/local-api");
    fs::create_dir_all(&output).map_err(|source| XtaskError::ProtoOutputDirectory {
        path: output.clone(),
        source,
    })?;
    generate_proto_into(&output)?;
    println!("generate-proto: wrote {}", output.display());
    Ok(())
}

fn read_proto_toolchain_contract() -> Result<ProtoToolchainContract, XtaskError> {
    let path = workspace_root().join("proto/toolchain.toml");
    let source = fs::read_to_string(&path).map_err(|source| XtaskError::ProtoToolchainRead {
        path: path.clone(),
        source,
    })?;
    toml::from_str(&source).map_err(|source| XtaskError::ProtoToolchainParse { path, source })
}

fn ensure_proto_tool(tool: &'static str, expected: &str) -> Result<(), XtaskError> {
    let output = run_proto_tool(tool, tool, &["--version"])?;
    validate_proto_tool_version(tool, expected, &output.stdout)
}

fn validate_proto_tool_version(
    tool: &'static str,
    expected: &str,
    stdout: &[u8],
) -> Result<(), XtaskError> {
    let actual = String::from_utf8_lossy(stdout).trim().to_owned();
    if actual == expected {
        Ok(())
    } else {
        Err(XtaskError::ProtoToolVersionMismatch {
            tool,
            expected: expected.to_owned(),
            actual,
        })
    }
}

fn run_proto_tool(
    command: &'static str,
    program: &'static str,
    arguments: &[&str],
) -> Result<Output, XtaskError> {
    let output = Command::new(program)
        .args(arguments)
        .current_dir(workspace_root())
        .output()
        .map_err(|source| XtaskError::ProtoToolSpawn {
            tool: program,
            source,
        })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(XtaskError::ProtoToolFailed {
            command,
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

fn generate_proto_into(output: &Path) -> Result<(), XtaskError> {
    let proto_root = workspace_root().join("proto");
    let proto_files = PROTO_FILES
        .map(|relative| proto_root.join(relative))
        .to_vec();
    for proto_file in &proto_files {
        if !proto_file.is_file() {
            return Err(XtaskError::ProtoGenerate {
                path: output.to_path_buf(),
                reason: format!("required input is missing: {}", proto_file.display()),
            });
        }
    }

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .out_dir(output)
        .compile_protos(&proto_files, &[proto_root])
        .map_err(|source| XtaskError::ProtoGenerate {
            path: output.to_path_buf(),
            reason: source.to_string(),
        })?;
    verify_generated_outputs(output)
}

fn verify_generated_outputs(output: &Path) -> Result<(), XtaskError> {
    let expected = GENERATED_PROTO_FILES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let actual = fs::read_dir(output)
        .map_err(|source| XtaskError::ProtoOutputRead {
            path: output.to_path_buf(),
            source,
        })?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| XtaskError::ProtoOutputRead {
            path: output.to_path_buf(),
            source,
        })?
        .into_iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();

    if actual == expected {
        Ok(())
    } else {
        Err(XtaskError::ProtoOutputMismatch { expected, actual })
    }
}

fn compare_generated_outputs(first: &Path, second: &Path) -> Result<(), XtaskError> {
    for file in GENERATED_PROTO_FILES {
        let first_path = first.join(file);
        let second_path = second.join(file);
        let first_bytes = fs::read(&first_path).map_err(|source| XtaskError::ProtoOutputRead {
            path: first_path,
            source,
        })?;
        let second_bytes =
            fs::read(&second_path).map_err(|source| XtaskError::ProtoOutputRead {
                path: second_path,
                source,
            })?;
        if first_bytes != second_bytes {
            return Err(XtaskError::ProtoNotDeterministic {
                file: file.to_owned(),
            });
        }
    }
    Ok(())
}

fn protoc_gen_local_api() -> Result<(), XtaskError> {
    let toolchain = read_proto_toolchain_contract()?;
    ensure_proto_tool("buf", &toolchain.buf)?;
    let mut encoded_request = Vec::new();
    io::stdin()
        .read_to_end(&mut encoded_request)
        .map_err(|source| XtaskError::ProtoPluginRead { source })?;
    let request = CodeGeneratorRequest::decode(encoded_request.as_slice())
        .map_err(|source| XtaskError::ProtoPluginDecode { source })?;
    validate_plugin_inputs(&request)?;
    let files_to_generate = request
        .file_to_generate
        .into_iter()
        .collect::<BTreeSet<_>>();
    let requests = request
        .proto_file
        .into_iter()
        .filter(|descriptor| {
            descriptor
                .name
                .as_ref()
                .is_some_and(|name| files_to_generate.contains(name))
        })
        .map(|descriptor| {
            (
                Module::from_protobuf_package_name(descriptor.package()),
                descriptor,
            )
        })
        .collect::<Vec<_>>();

    let mut config = prost_build::Config::new();
    config.service_generator(
        tonic_prost_build::configure()
            .build_client(true)
            .build_server(true)
            .service_generator(),
    );
    let generated =
        config
            .generate(requests)
            .map_err(|source| XtaskError::ProtoPluginGenerate {
                reason: source.to_string(),
            })?;
    let files = generated
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .map(|(module, content)| code_generator_response::File {
            name: Some(module.to_file_name_or("_")),
            insertion_point: None,
            content: Some(content),
            generated_code_info: None,
        })
        .collect();
    let response = CodeGeneratorResponse {
        error: None,
        supported_features: None,
        file: files,
    };
    validate_plugin_outputs(&response)?;
    let mut encoded_response = Vec::new();
    response
        .encode(&mut encoded_response)
        .map_err(|source| XtaskError::ProtoPluginEncode { source })?;
    io::stdout()
        .write_all(&encoded_response)
        .map_err(|source| XtaskError::ProtoPluginWrite { source })
}

fn validate_plugin_inputs(request: &CodeGeneratorRequest) -> Result<(), XtaskError> {
    let expected = PROTO_FILES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let actual = request
        .file_to_generate
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let described_inputs = request
        .proto_file
        .iter()
        .filter_map(|descriptor| descriptor.name.as_ref())
        .filter(|name| expected.contains(*name))
        .cloned()
        .collect::<BTreeSet<_>>();

    if request.file_to_generate.len() == expected.len()
        && actual == expected
        && described_inputs == expected
    {
        Ok(())
    } else {
        Err(XtaskError::ProtoInputMismatch { expected, actual })
    }
}

fn validate_plugin_outputs(response: &CodeGeneratorResponse) -> Result<(), XtaskError> {
    let expected = GENERATED_PROTO_FILES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let actual = response
        .file
        .iter()
        .filter_map(|file| file.name.clone())
        .collect::<BTreeSet<_>>();
    if response.file.len() == expected.len() && actual == expected {
        Ok(())
    } else {
        Err(XtaskError::ProtoOutputMismatch { expected, actual })
    }
}

fn read_metadata() -> Result<CargoMetadata, XtaskError> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let output = Command::new(cargo)
        .args(["metadata", "--locked", "--format-version", "1"])
        .current_dir(workspace_root())
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

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must be nested in the workspace")
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
    use prost_types::FileDescriptorProto;

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

    fn plugin_request(files: &[&str]) -> CodeGeneratorRequest {
        CodeGeneratorRequest {
            file_to_generate: files.iter().map(|file| (*file).to_owned()).collect(),
            proto_file: PROTO_FILES
                .iter()
                .map(|file| FileDescriptorProto {
                    name: Some((*file).to_owned()),
                    package: Some(format!(
                        "cmti.{}.v1",
                        file.split('/').next().expect("fixture path has a package")
                    )),
                    ..FileDescriptorProto::default()
                })
                .collect(),
            ..CodeGeneratorRequest::default()
        }
    }

    fn plugin_response(files: &[&str]) -> CodeGeneratorResponse {
        CodeGeneratorResponse {
            file: files
                .iter()
                .map(|file| code_generator_response::File {
                    name: Some((*file).to_owned()),
                    ..code_generator_response::File::default()
                })
                .collect(),
            ..CodeGeneratorResponse::default()
        }
    }

    #[test]
    fn plugin_requires_the_exact_three_input_paths() {
        assert!(validate_plugin_inputs(&plugin_request(&PROTO_FILES)).is_ok());

        let missing = &PROTO_FILES[..2];
        assert!(matches!(
            validate_plugin_inputs(&plugin_request(missing)),
            Err(XtaskError::ProtoInputMismatch { .. })
        ));

        let mut extra = PROTO_FILES.to_vec();
        extra.push("admin/v1/admin.proto");
        assert!(matches!(
            validate_plugin_inputs(&plugin_request(&extra)),
            Err(XtaskError::ProtoInputMismatch { .. })
        ));
    }

    #[test]
    fn plugin_requires_the_exact_three_output_filenames() {
        assert!(validate_plugin_outputs(&plugin_response(&GENERATED_PROTO_FILES)).is_ok());

        assert!(matches!(
            validate_plugin_outputs(&plugin_response(&GENERATED_PROTO_FILES[..2])),
            Err(XtaskError::ProtoOutputMismatch { .. })
        ));

        let mut extra = GENERATED_PROTO_FILES.to_vec();
        extra.push("cmti.admin.v1.rs");
        assert!(matches!(
            validate_plugin_outputs(&plugin_response(&extra)),
            Err(XtaskError::ProtoOutputMismatch { .. })
        ));
    }

    #[test]
    fn checked_in_tool_versions_accept_exact_output_and_reject_drift() {
        let contract =
            read_proto_toolchain_contract().expect("checked-in toolchain contract must parse");
        assert_eq!(contract.buf, "1.72.0");
        assert_eq!(contract.protoc, "libprotoc 33.4");
        assert!(validate_proto_tool_version("buf", &contract.buf, b"1.72.0\n").is_ok());
        assert!(
            validate_proto_tool_version("protoc", &contract.protoc, b"libprotoc 33.4\n").is_ok()
        );
        assert!(matches!(
            validate_proto_tool_version("buf", &contract.buf, b"1.71.0\n"),
            Err(XtaskError::ProtoToolVersionMismatch { .. })
        ));
        assert!(matches!(
            validate_proto_tool_version("protoc", &contract.protoc, b"libprotoc 33.3\n"),
            Err(XtaskError::ProtoToolVersionMismatch { .. })
        ));
    }
}
