use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode, ExitStatus, Output},
};

use prost::Message as _;
use prost_build::Module;
use prost_types::compiler::{CodeGeneratorRequest, CodeGeneratorResponse, code_generator_response};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

#[path = "../../build-support/protoc_toolchain.rs"]
mod protoc_toolchain;

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
    #[error("invalid protoc toolchain: {source}")]
    ProtocToolchain {
        #[source]
        source: protoc_toolchain::ProtocToolchainError,
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
    #[error("license policy violation: {reason}")]
    LicensePolicy { reason: String },
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
#[serde(deny_unknown_fields)]
struct ArtifactInventory {
    schema_version: u32,
    code_license: String,
    docs_license: String,
    category: Vec<ArtifactCategory>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactCategory {
    name: String,
    status: String,
    scope: String,
    origin: String,
    license: String,
    redistribution: String,
    ownership: String,
    version: String,
    retention: String,
    #[serde(default)]
    evidence: Vec<String>,
    verification: Option<String>,
    #[serde(default)]
    artifact: Vec<ArtifactEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactEntry {
    path: String,
    sha256: String,
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
        "license-check" if options.is_empty() => license_check(),
        "generate-config-schema" => generate_config_schema(options),
        "generate-proto" => generate_proto_command(options),
        "proto-check" if options.is_empty() => proto_check(),
        "protoc-gen-local-api" if options.is_empty() => protoc_gen_local_api(),
        "help" | "license-check" | "proto-check" | "protoc-gen-local-api" | "workspace-check" => {
            Err(XtaskError::InvalidInvocation)
        }
        command => Err(XtaskError::UnknownCommand {
            command: command.to_owned(),
        }),
    }
}

fn print_help() {
    println!(
        "xtask commands:\n  help\n  workspace-check\n  license-check\n  generate-config-schema [--check]\n  generate-proto [--check]\n  proto-check"
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

fn license_check() -> Result<(), XtaskError> {
    let path = workspace_root().join("licenses/artifact-provenance.toml");
    let source = fs::read_to_string(&path).map_err(|error| XtaskError::LicensePolicy {
        reason: format!("could not read {}: {error}", path.display()),
    })?;
    validate_license_inventory(workspace_root(), &source)?;
    println!("license-check: ok");
    Ok(())
}

fn validate_license_inventory(root: &Path, source: &str) -> Result<(), XtaskError> {
    let inventory = toml::from_str::<ArtifactInventory>(source).map_err(license_policy)?;
    if inventory.schema_version != 1 {
        return Err(license_policy("schema_version must be 1"));
    }
    if inventory.code_license != "MIT OR Apache-2.0"
        || inventory.docs_license != "MIT OR Apache-2.0"
    {
        return Err(license_policy(
            "code_license and docs_license must use the approved SPDX expression",
        ));
    }

    let expected_categories = BTreeSet::from([
        "dataset",
        "fixture",
        "generated-schema",
        "model",
        "vendored-source",
    ]);
    let actual_categories = inventory
        .category
        .iter()
        .map(|category| category.name.as_str())
        .collect::<BTreeSet<_>>();
    if inventory.category.len() != expected_categories.len()
        || actual_categories != expected_categories
    {
        return Err(license_policy(format!(
            "categories must be exactly {expected_categories:?}, got {actual_categories:?}"
        )));
    }

    let mut inventoried_paths = BTreeSet::new();
    for category in &inventory.category {
        validate_category_metadata(category)?;
        let category_paths = category
            .artifact
            .iter()
            .map(|artifact| PathBuf::from(&artifact.path))
            .collect::<BTreeSet<_>>();

        for artifact in &category.artifact {
            validate_artifact(root, category, artifact)?;
            if !inventoried_paths.insert(artifact.path.clone()) {
                return Err(license_policy(format!(
                    "artifact path is listed more than once: {}",
                    artifact.path
                )));
            }
        }
        for evidence in &category.evidence {
            validate_safe_relative_path(evidence)?;
            if !root.join(evidence).is_file() {
                return Err(license_policy(format!(
                    "{} evidence is missing: {evidence}",
                    category.name
                )));
            }
            if !category
                .artifact
                .iter()
                .any(|artifact| artifact.path == *evidence)
            {
                return Err(license_policy(format!(
                    "{} evidence is not integrity-checked: {evidence}",
                    category.name
                )));
            }
        }

        match category.name.as_str() {
            "fixture" => {
                require_category_state(category, "tracked", "fixtures", true)?;
                require_category_license(category, "MIT OR Apache-2.0", "allowed")?;
                require_scope_coverage(root, &category.scope, &category_paths)?;
            }
            "dataset" => {
                require_category_state(category, "absent", "data", false)?;
                require_category_license(category, "not-applicable", "not-applicable")?;
                require_absent_dataset_scope(root, &category.scope)?;
            }
            "model" => {
                require_category_state(category, "metadata-only", "models", true)?;
                require_category_license(category, "MIT OR Apache-2.0", "allowed")?;
                require_scope_coverage(root, &category.scope, &category_paths)?;
            }
            "generated-schema" => {
                require_category_state(category, "tracked", "configs/schema.json", true)?;
                require_category_license(category, "MIT OR Apache-2.0", "allowed")?;
                if category_paths != BTreeSet::from([PathBuf::from("configs/schema.json")]) {
                    return Err(license_policy(
                        "generated-schema must inventory configs/schema.json exactly",
                    ));
                }
            }
            "vendored-source" => {
                require_category_state(
                    category,
                    "tracked",
                    "apps/macos/Vendor/grpc-swift-nio-transport",
                    true,
                )?;
                require_category_license(
                    category,
                    "Apache-2.0",
                    "allowed-with-license-and-notices",
                )?;
                validate_vendor_verification(root, category)?;
            }
            _ => {
                return Err(license_policy(format!(
                    "unsupported artifact category {}",
                    category.name
                )));
            }
        }
    }
    Ok(())
}

fn validate_category_metadata(category: &ArtifactCategory) -> Result<(), XtaskError> {
    let fields = [
        ("name", category.name.as_str()),
        ("status", category.status.as_str()),
        ("scope", category.scope.as_str()),
        ("origin", category.origin.as_str()),
        ("license", category.license.as_str()),
        ("redistribution", category.redistribution.as_str()),
        ("ownership", category.ownership.as_str()),
        ("version", category.version.as_str()),
        ("retention", category.retention.as_str()),
    ];
    for (field, value) in fields {
        let normalized = value.trim().to_ascii_lowercase();
        if normalized.is_empty()
            || ["pending", "placeholder", "tbd", "todo", "unknown"]
                .iter()
                .any(|marker| normalized.contains(marker))
        {
            return Err(license_policy(format!(
                "{}.{field} is empty or contains placeholder metadata",
                category.name
            )));
        }
    }
    if category.status != "absent"
        && (category.license == "not-applicable" || category.redistribution == "not-applicable")
    {
        return Err(license_policy(format!(
            "{} must declare an applicable license and redistribution policy",
            category.name
        )));
    }
    Ok(())
}

fn require_category_license(
    category: &ArtifactCategory,
    license: &str,
    redistribution: &str,
) -> Result<(), XtaskError> {
    if category.license != license || category.redistribution != redistribution {
        return Err(license_policy(format!(
            "{} must declare license {license:?} and redistribution {redistribution:?}",
            category.name
        )));
    }
    Ok(())
}

fn require_category_state(
    category: &ArtifactCategory,
    status: &str,
    scope: &str,
    artifacts_required: bool,
) -> Result<(), XtaskError> {
    if category.status != status || category.scope != scope {
        return Err(license_policy(format!(
            "{} must have status {status:?} and scope {scope:?}",
            category.name
        )));
    }
    if category.artifact.is_empty() == artifacts_required {
        let requirement = if artifacts_required {
            "at least one artifact"
        } else {
            "no artifacts"
        };
        return Err(license_policy(format!(
            "{} must contain {requirement}",
            category.name
        )));
    }
    Ok(())
}

fn validate_artifact(
    root: &Path,
    category: &ArtifactCategory,
    artifact: &ArtifactEntry,
) -> Result<(), XtaskError> {
    validate_safe_relative_path(&artifact.path)?;
    let relative = Path::new(&artifact.path);
    let scope = Path::new(&category.scope);
    if relative != scope && !relative.starts_with(scope) {
        return Err(license_policy(format!(
            "{} artifact is outside its declared scope: {}",
            category.name, artifact.path
        )));
    }
    if artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(license_policy(format!(
            "{} has an invalid SHA-256 digest",
            artifact.path
        )));
    }

    let path = root.join(relative);
    let canonical_root = root.canonicalize().map_err(license_policy)?;
    let canonical_path = path.canonicalize().map_err(|error| {
        license_policy(format!(
            "could not resolve artifact {}: {error}",
            artifact.path
        ))
    })?;
    if !canonical_path.starts_with(&canonical_root) || !canonical_path.is_file() {
        return Err(license_policy(format!(
            "artifact is not a repository-owned regular file: {}",
            artifact.path
        )));
    }
    let bytes = fs::read(&canonical_path).map_err(|error| {
        license_policy(format!(
            "could not read artifact {}: {error}",
            artifact.path
        ))
    })?;
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != artifact.sha256 {
        return Err(license_policy(format!(
            "SHA-256 mismatch for {}: expected {}, got {actual}",
            artifact.path, artifact.sha256
        )));
    }
    Ok(())
}

fn validate_safe_relative_path(path: &str) -> Result<(), XtaskError> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(license_policy(format!(
            "inventory path is not a safe repository-relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_scope_coverage(
    root: &Path,
    scope: &str,
    expected: &BTreeSet<PathBuf>,
) -> Result<(), XtaskError> {
    let mut actual = BTreeSet::new();
    collect_regular_files(root, &root.join(scope), &mut actual)?;
    if &actual != expected {
        return Err(license_policy(format!(
            "scope {scope:?} does not match its inventory: expected {expected:?}, found {actual:?}"
        )));
    }
    Ok(())
}

fn require_absent_dataset_scope(root: &Path, scope: &str) -> Result<(), XtaskError> {
    let mut actual = BTreeSet::new();
    collect_regular_files(root, &root.join(scope), &mut actual)?;
    if actual.is_empty() {
        return Ok(());
    }

    let marker = PathBuf::from(scope).join(".gitkeep");
    if actual != BTreeSet::from([marker.clone()]) {
        return Err(license_policy(format!(
            "absent dataset scope may contain only {marker:?}, found {actual:?}"
        )));
    }
    let marker_path = root.join(marker);
    let metadata = fs::metadata(&marker_path).map_err(|error| {
        license_policy(format!(
            "could not inspect absent dataset marker {}: {error}",
            marker_path.display()
        ))
    })?;
    if metadata.len() != 0 {
        return Err(license_policy(format!(
            "absent dataset marker must be empty: {}",
            marker_path.display()
        )));
    }
    Ok(())
}

fn collect_regular_files(
    root: &Path,
    path: &Path,
    files: &mut BTreeSet<PathBuf>,
) -> Result<(), XtaskError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        license_policy(format!("could not inspect {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(license_policy(format!(
            "inventory scopes must not contain symlinks: {}",
            path.display()
        )));
    }
    if metadata.is_file() {
        let relative = path.strip_prefix(root).map_err(|error| {
            license_policy(format!(
                "could not make {} repository-relative: {error}",
                path.display()
            ))
        })?;
        files.insert(relative.to_path_buf());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(license_policy(format!(
            "inventory scope contains a non-file entry: {}",
            path.display()
        )));
    }
    let mut entries = fs::read_dir(path)
        .map_err(|error| license_policy(format!("could not read {}: {error}", path.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(license_policy)?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        collect_regular_files(root, &entry.path(), files)?;
    }
    Ok(())
}

fn validate_vendor_verification(
    root: &Path,
    category: &ArtifactCategory,
) -> Result<(), XtaskError> {
    let expected = "scripts/verify-vendored-grpc-transport.sh";
    if category.verification.as_deref() != Some(expected) {
        return Err(license_policy(format!(
            "vendored-source verification must be {expected}"
        )));
    }
    let package_path = root.join("apps/macos/Vendor/grpc-swift-nio-transport/Package.swift");
    let package = fs::read_to_string(&package_path).map_err(|error| {
        license_policy(format!(
            "could not read vendored package manifest {}: {error}",
            package_path.display()
        ))
    })?;
    validate_vendor_package_patch(&package)?;

    let output = Command::new(root.join(expected))
        .current_dir(root)
        .output()
        .map_err(|error| license_policy(format!("could not run {expected}: {error}")))?;
    if !output.status.success() {
        return Err(license_policy(format!(
            "{expected} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let manifest_path =
        root.join("apps/macos/Vendor/grpc-swift-nio-transport/UPSTREAM_FILES.sha256");
    let manifest = fs::read_to_string(&manifest_path).map_err(|error| {
        license_policy(format!(
            "could not read vendor integrity manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    let mut covered = category
        .artifact
        .iter()
        .map(|artifact| PathBuf::from(&artifact.path))
        .collect::<BTreeSet<_>>();
    let mut manifest_paths = BTreeSet::new();
    for (index, line) in manifest.lines().enumerate() {
        let Some((digest, path)) = line.split_once("  ") else {
            return Err(license_policy(format!(
                "vendor integrity manifest line {} is malformed",
                index + 1
            )));
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(license_policy(format!(
                "vendor integrity manifest line {} has an invalid SHA-256 digest",
                index + 1
            )));
        }
        validate_safe_relative_path(path)?;
        if !Path::new(path).starts_with(&category.scope) {
            return Err(license_policy(format!(
                "vendor integrity manifest path is outside its scope: {path}"
            )));
        }
        if !manifest_paths.insert(PathBuf::from(path)) {
            return Err(license_policy(format!(
                "vendor integrity path is listed more than once: {path}"
            )));
        }
    }
    covered.extend(manifest_paths);

    let mut actual = BTreeSet::new();
    collect_regular_files(root, &root.join(&category.scope), &mut actual)?;
    if covered != actual {
        return Err(license_policy(format!(
            "vendored-source scope does not match its upstream integrity coverage: expected {covered:?}, found {actual:?}"
        )));
    }
    Ok(())
}

fn validate_vendor_package_patch(source: &str) -> Result<(), XtaskError> {
    const APPROVED_LINES: [&str; 2] = [
        "      .product(name: \"NIOHTTP1\", package: \"swift-nio\"),",
        "      .product(name: \"NIOTLS\", package: \"swift-nio\"),",
    ];
    for approved in APPROVED_LINES {
        let signature = approved.trim();
        let exact_count = source.lines().filter(|line| *line == approved).count();
        let signature_count = source
            .lines()
            .filter(|line| line.contains(signature))
            .count();
        if exact_count != 1 || signature_count != 1 {
            return Err(license_policy(format!(
                "vendored Package.swift must contain exactly one canonical patch line {approved:?}"
            )));
        }
    }
    Ok(())
}

fn license_policy(reason: impl std::fmt::Display) -> XtaskError {
    XtaskError::LicensePolicy {
        reason: reason.to_string(),
    }
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
    let protoc = resolve_protoc(&toolchain)?;
    run_proto_tool("buf lint proto", "buf", &["lint", "proto"])?;
    run_proto_tool("buf build proto", "buf", &["build", "proto"])?;
    generate_proto_with_protoc(true, protoc.path())?;
    println!("proto-check: ok");
    Ok(())
}

fn generate_proto(check: bool) -> Result<(), XtaskError> {
    let toolchain = read_proto_toolchain_contract()?;
    let protoc = resolve_protoc(&toolchain)?;
    generate_proto_with_protoc(check, protoc.path())
}

fn generate_proto_with_protoc(check: bool, protoc: &Path) -> Result<(), XtaskError> {
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
        generate_proto_into(first.path(), protoc)?;
        generate_proto_into(second.path(), protoc)?;
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
    generate_proto_into(&output, protoc)?;
    println!("generate-proto: wrote {}", output.display());
    Ok(())
}

fn read_proto_toolchain_contract() -> Result<protoc_toolchain::ProtoToolchainContract, XtaskError> {
    protoc_toolchain::read_contract(workspace_root())
        .map_err(|source| XtaskError::ProtocToolchain { source })
}

fn resolve_protoc(
    toolchain: &protoc_toolchain::ProtoToolchainContract,
) -> Result<protoc_toolchain::ValidatedProtoc, XtaskError> {
    protoc_toolchain::resolve_and_validate(toolchain)
        .map_err(|source| XtaskError::ProtocToolchain { source })
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

fn generate_proto_into(output: &Path, protoc: &Path) -> Result<(), XtaskError> {
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

    let mut prost_config = prost_build::Config::new();
    prost_config.protoc_executable(protoc);
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .out_dir(output)
        .compile_with_config(prost_config, &proto_files, &[proto_root])
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

    fn checked_in_license_inventory() -> (String, String) {
        let source = fs::read_to_string(workspace_root().join("licenses/artifact-provenance.toml"))
            .expect("checked-in artifact inventory must be readable");
        let inventory = toml::from_str::<ArtifactInventory>(&source)
            .expect("checked-in artifact inventory must parse");
        let digest = inventory
            .category
            .iter()
            .find(|category| category.name == "generated-schema")
            .and_then(|category| category.artifact.first())
            .map(|artifact| artifact.sha256.clone())
            .expect("generated schema must have an integrity digest");
        (source, digest)
    }

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
            protoc_toolchain::validate_version_output(&contract.protoc, b"libprotoc 33.4\n")
                .is_ok()
        );
        assert!(matches!(
            validate_proto_tool_version("buf", &contract.buf, b"1.71.0\n"),
            Err(XtaskError::ProtoToolVersionMismatch { .. })
        ));
        assert!(matches!(
            protoc_toolchain::validate_version_output(&contract.protoc, b"libprotoc 33.3\n"),
            Err(protoc_toolchain::ProtocToolchainError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn checked_in_license_inventory_is_semantic_and_tamper_sensitive() {
        let (source, digest) = checked_in_license_inventory();
        validate_license_inventory(workspace_root(), &source)
            .expect("checked-in artifact inventory must validate");

        let mut replacement = digest.clone().into_bytes();
        replacement[0] = if replacement[0] == b'0' { b'1' } else { b'0' };
        let replacement =
            String::from_utf8(replacement).expect("a hexadecimal digest must remain UTF-8");
        let tampered = source.replacen(&digest, &replacement, 1);
        assert!(matches!(
            validate_license_inventory(workspace_root(), &tampered),
            Err(XtaskError::LicensePolicy { .. })
        ));
    }

    #[test]
    fn license_inventory_rejects_placeholder_integrity_and_missing_categories() {
        let (source, digest) = checked_in_license_inventory();

        let placeholder = source.replacen(&digest, "PENDING", 1);
        assert!(matches!(
            validate_license_inventory(workspace_root(), &placeholder),
            Err(XtaskError::LicensePolicy { .. })
        ));

        let missing_model = source.replacen("name = \"model\"", "name = \"fixture\"", 1);
        assert!(matches!(
            validate_license_inventory(workspace_root(), &missing_model),
            Err(XtaskError::LicensePolicy { .. })
        ));

        let incompatible_license = source.replacen(
            "license = \"MIT OR Apache-2.0\"",
            "license = \"Proprietary\"",
            1,
        );
        assert!(matches!(
            validate_license_inventory(workspace_root(), &incompatible_license),
            Err(XtaskError::LicensePolicy { .. })
        ));

        let denied_redistribution = source.replacen(
            "redistribution = \"allowed\"",
            "redistribution = \"denied\"",
            1,
        );
        assert!(matches!(
            validate_license_inventory(workspace_root(), &denied_redistribution),
            Err(XtaskError::LicensePolicy { .. })
        ));
    }

    #[test]
    fn absent_dataset_rejects_nested_or_nonempty_gitkeep_files() {
        let nested = tempfile::tempdir().expect("temporary repository root must exist");
        fs::create_dir_all(nested.path().join("data/private"))
            .expect("nested dataset directory must exist");
        fs::write(
            nested.path().join("data/private/.gitkeep"),
            b"secret dataset",
        )
        .expect("nested dataset fixture must be written");
        assert!(
            require_absent_dataset_scope(nested.path(), "data").is_err(),
            "a nested .gitkeep must not make committed data look absent"
        );

        let nonempty = tempfile::tempdir().expect("temporary repository root must exist");
        fs::create_dir(nonempty.path().join("data")).expect("dataset directory must exist");
        fs::write(nonempty.path().join("data/.gitkeep"), b"not empty")
            .expect("dataset marker fixture must be written");
        assert!(
            require_absent_dataset_scope(nonempty.path(), "data").is_err(),
            "a non-empty root .gitkeep must not make committed data look absent"
        );
    }

    #[test]
    fn vendor_patch_rejects_same_line_prefix_or_suffix_content() {
        let source = fs::read_to_string(
            workspace_root().join("apps/macos/Vendor/grpc-swift-nio-transport/Package.swift"),
        )
        .expect("vendored package manifest must be readable");
        validate_vendor_package_patch(&source).expect("checked-in patch must be canonical");

        let exact = "      .product(name: \"NIOHTTP1\", package: \"swift-nio\"),";
        let suffixed = source.replacen(exact, &format!("{exact} // hidden change"), 1);
        assert!(validate_vendor_package_patch(&suffixed).is_err());

        let prefixed = source.replacen(exact, &format!("unexpected {exact}"), 1);
        assert!(validate_vendor_package_patch(&prefixed).is_err());
    }
}
