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
use prost_types::{
    DescriptorProto, EnumDescriptorProto, FileDescriptorSet,
    compiler::{CodeGeneratorRequest, CodeGeneratorResponse, code_generator_response},
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

#[path = "../../build-support/proto_inventory.rs"]
mod proto_inventory;
#[path = "../../build-support/protoc_toolchain.rs"]
mod protoc_toolchain;

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
    #[error("could not read observability contract {path}: {source}")]
    ObservabilityDocumentRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("checked-in observability metric and field catalog is out of date")]
    ObservabilitySchemaOutOfDate,
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
    #[error("could not inspect model schema directory {path}: {source}")]
    ModelSchemaDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not read model schema {path}: {source}")]
    ModelSchemaRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("model schema {path} is invalid: {reason}")]
    InvalidModelSchema { path: PathBuf, reason: String },
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
        "generate-field-registry" => generate_field_registry(options),
        "generate-proto" => generate_proto_command(options),
        "observability-schema-check" if options.is_empty() => observability_schema_check(),
        "validate-model-schemas" if options.is_empty() => validate_model_schemas(),
        "proto-check" if options.is_empty() => proto_check(),
        "protoc-gen-local-api" if options.is_empty() => protoc_gen_local_api(),
        "help"
        | "license-check"
        | "observability-schema-check"
        | "validate-model-schemas"
        | "proto-check"
        | "protoc-gen-local-api"
        | "workspace-check" => Err(XtaskError::InvalidInvocation),
        command => Err(XtaskError::UnknownCommand {
            command: command.to_owned(),
        }),
    }
}

fn print_help() {
    println!(
        "xtask commands:\n  help\n  workspace-check\n  license-check\n  generate-config-schema [--check]\n  generate-field-registry [--check]\n  generate-proto [--check]\n  observability-schema-check\n  validate-model-schemas\n  proto-check"
    );
}

const MAX_MODEL_SCHEMA_FILES: usize = 256;
const MAX_MODEL_SCHEMA_BYTES: u64 = 1_048_576;

fn validate_model_schemas() -> Result<(), XtaskError> {
    let directory = workspace_root().join("models/schemas");
    let entries = fs::read_dir(&directory).map_err(|source| XtaskError::ModelSchemaDirectory {
        path: directory.clone(),
        source,
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| XtaskError::ModelSchemaDirectory {
            path: directory.clone(),
            source,
        })?;
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".schema.json"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.is_empty() || paths.len() > MAX_MODEL_SCHEMA_FILES {
        return Err(XtaskError::InvalidModelSchema {
            path: directory,
            reason: "schema inventory is empty or exceeds 256 files".to_owned(),
        });
    }

    let mut active = 0_usize;
    let mut manifests = 0_usize;
    for path in paths {
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| XtaskError::ModelSchemaRead {
                path: path.clone(),
                source,
            })?;
        if !metadata.file_type().is_file() || metadata.len() > MAX_MODEL_SCHEMA_BYTES {
            return Err(XtaskError::InvalidModelSchema {
                path,
                reason: "schema must be a regular file no larger than 1 MiB".to_owned(),
            });
        }
        let file = fs::File::open(&path).map_err(|source| XtaskError::ModelSchemaRead {
            path: path.clone(),
            source,
        })?;
        let opened_metadata = file
            .metadata()
            .map_err(|source| XtaskError::ModelSchemaRead {
                path: path.clone(),
                source,
            })?;
        if !opened_metadata.file_type().is_file()
            || opened_metadata.len() > MAX_MODEL_SCHEMA_BYTES
            || !same_file_identity(&metadata, &opened_metadata)
        {
            return Err(XtaskError::InvalidModelSchema {
                path,
                reason: "schema identity changed while opening or exceeds 1 MiB".to_owned(),
            });
        }
        let mut bytes = Vec::new();
        file.take(MAX_MODEL_SCHEMA_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| XtaskError::ModelSchemaRead {
                path: path.clone(),
                source,
            })?;
        if bytes.len() as u64 > MAX_MODEL_SCHEMA_BYTES {
            return Err(XtaskError::InvalidModelSchema {
                path,
                reason: "schema exceeds 1 MiB while reading".to_owned(),
            });
        }
        let value = serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|error| {
            XtaskError::InvalidModelSchema {
                path: path.clone(),
                reason: format!("invalid JSON: {error}"),
            }
        })?;
        if value.get("$schema").is_none() {
            validate_model_artifact_manifest(&path, &value)?;
            manifests += 1;
        } else {
            validate_active_model_schema(&path, &value)?;
            active += 1;
        }
    }
    println!("validate-model-schemas: ok ({active} active, {manifests} artifact manifests)");
    Ok(())
}

#[cfg(unix)]
fn same_file_identity(expected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    expected.dev() == opened.dev() && expected.ino() == opened.ino()
}

#[cfg(not(unix))]
fn same_file_identity(expected: &fs::Metadata, opened: &fs::Metadata) -> bool {
    expected.file_type().is_file() && opened.file_type().is_file() && expected.len() == opened.len()
}

fn validate_model_artifact_manifest(
    path: &Path,
    value: &serde_json::Value,
) -> Result<(), XtaskError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_model_schema(path, "artifact manifest must be an object"))?;
    let expected = BTreeSet::from(["artifact_id", "local_only", "schema_version", "status"]);
    let actual = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if actual != expected
        || value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
        || value.get("local_only").and_then(serde_json::Value::as_bool) != Some(true)
        || !matches!(
            value.get("status").and_then(serde_json::Value::as_str),
            Some("implemented" | "planned")
        )
    {
        return Err(invalid_model_schema(
            path,
            "artifact manifest must use the exact bounded repository contract",
        ));
    }
    let expected_artifact = path
        .strip_prefix(workspace_root())
        .ok()
        .and_then(Path::to_str)
        .is_some_and(|relative| {
            value.get("artifact_id").and_then(serde_json::Value::as_str) == Some(relative)
        });
    if !expected_artifact {
        return Err(invalid_model_schema(
            path,
            "artifact manifest artifact_id does not match its repository path",
        ));
    }
    Ok(())
}

fn validate_active_model_schema(path: &Path, value: &serde_json::Value) -> Result<(), XtaskError> {
    if value.get("$schema").and_then(serde_json::Value::as_str)
        != Some("https://json-schema.org/draft/2020-12/schema")
        || !value
            .get("$id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|id| id.starts_with("https://rsitech.ai/schemas/crypto-intelligence/"))
        || value.get("type").and_then(serde_json::Value::as_str) != Some("object")
        || value
            .get("additionalProperties")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
    {
        return Err(invalid_model_schema(
            path,
            "active schema must declare the Draft 2020-12 dialect, RSI Tech ID, strict object root",
        ));
    }
    let required = value
        .get("required")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| invalid_model_schema(path, "active schema must declare required fields"))?;
    let properties = value
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| invalid_model_schema(path, "active schema must declare properties"))?;
    if required.is_empty()
        || required.iter().any(|field| {
            field
                .as_str()
                .is_none_or(|field| !properties.contains_key(field))
        })
    {
        return Err(invalid_model_schema(
            path,
            "every required field must name a root property",
        ));
    }
    validate_schema_bounds(path, value)
}

fn validate_schema_bounds(path: &Path, value: &serde_json::Value) -> Result<(), XtaskError> {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                validate_schema_bounds(path, value)?;
            }
        }
        serde_json::Value::Object(object) => {
            if object.get("type").and_then(serde_json::Value::as_str) == Some("object")
                && object
                    .get("additionalProperties")
                    .and_then(serde_json::Value::as_bool)
                    != Some(false)
            {
                return Err(invalid_model_schema(
                    path,
                    "every object schema must reject unknown properties",
                ));
            }
            if object.get("type").and_then(serde_json::Value::as_str) == Some("array")
                && object
                    .get("maxItems")
                    .and_then(serde_json::Value::as_u64)
                    .is_none()
            {
                return Err(invalid_model_schema(
                    path,
                    "every array schema must declare maxItems",
                ));
            }
            for value in object.values() {
                validate_schema_bounds(path, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn invalid_model_schema(path: &Path, reason: &str) -> XtaskError {
    XtaskError::InvalidModelSchema {
        path: path.to_path_buf(),
        reason: reason.to_owned(),
    }
}

const OBSERVABILITY_CATALOG_START: &str = "<!-- BEGIN GENERATED OBSERVABILITY CATALOG -->";
const OBSERVABILITY_CATALOG_END: &str = "<!-- END GENERATED OBSERVABILITY CATALOG -->";

fn observability_schema_check() -> Result<(), XtaskError> {
    let path = workspace_root().join("docs/operations/observability.md");
    let checked_in =
        fs::read_to_string(&path).map_err(|source| XtaskError::ObservabilityDocumentRead {
            path: path.clone(),
            source,
        })?;
    let expected = render_observability_catalog();
    let Some((_, suffix)) = checked_in.split_once(OBSERVABILITY_CATALOG_START) else {
        return Err(XtaskError::ObservabilitySchemaOutOfDate);
    };
    let Some((actual, _)) = suffix.split_once(OBSERVABILITY_CATALOG_END) else {
        return Err(XtaskError::ObservabilitySchemaOutOfDate);
    };
    if actual != expected {
        return Err(XtaskError::ObservabilitySchemaOutOfDate);
    }
    println!("observability-schema-check: ok");
    Ok(())
}

fn render_observability_catalog() -> String {
    use observability::{Component, FieldName, MetricName, Outcome, Venue};

    let mut output = String::from(
        "\n\n## Generated contract catalog\n\n\
This block is checked against the Rust enums by `cargo run -p xtask -- \
observability-schema-check`.\n\n\
### Metrics\n\n\
| Name | Kind | Unit |\n\
|---|---|---|\n",
    );
    for metric in MetricName::ALL {
        output.push_str(&format!(
            "| `{}` | `{:?}` | `{:?}` |\n",
            metric.as_str(),
            metric.kind(),
            metric.unit()
        ));
    }
    output.push_str(
        "\n### Structured log fields\n\n\
| Field |\n\
|---|\n",
    );
    for field in FieldName::ALL {
        output.push_str(&format!("| `{}` |\n", field.as_str()));
    }
    output.push_str("\n### Bounded metric label values\n\n");
    for (label, values) in [
        (
            "component",
            Component::ALL
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "venue",
            Venue::ALL
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>(),
        ),
        (
            "outcome",
            Outcome::ALL
                .iter()
                .map(|value| value.as_str())
                .collect::<Vec<_>>(),
        ),
    ] {
        output.push_str(&format!("- `{label}`: `{}`\n", values.join("`, `")));
    }
    output.push('\n');
    output
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
        "vendored-sqlite",
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
            "vendored-sqlite" => {
                require_category_state(category, "tracked", "vendor/libsqlite3-sys", true)?;
                require_category_license(
                    category,
                    "MIT AND LicenseRef-SQLite-Public-Domain AND BSD-3-Clause",
                    "allowed-with-license-and-provenance",
                )?;
                validate_sqlite_vendor_verification(root, category)?;
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

fn validate_sqlite_vendor_verification(
    root: &Path,
    category: &ArtifactCategory,
) -> Result<(), XtaskError> {
    let expected = "scripts/verify-vendored-sqlite.sh";
    if category.verification.as_deref() != Some(expected) {
        return Err(license_policy(format!(
            "vendored-sqlite verification must be {expected}"
        )));
    }

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

    let manifest_path = root.join("vendor/libsqlite3-sys/CMTI_FILES.sha256");
    let manifest = fs::read_to_string(&manifest_path).map_err(|error| {
        license_policy(format!(
            "could not read vendored SQLite integrity manifest {}: {error}",
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
                "vendored SQLite integrity manifest line {} is malformed",
                index + 1
            )));
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(license_policy(format!(
                "vendored SQLite integrity manifest line {} has an invalid SHA-256 digest",
                index + 1
            )));
        }
        validate_safe_relative_path(path)?;
        if !Path::new(path).starts_with(&category.scope) {
            return Err(license_policy(format!(
                "vendored SQLite integrity path is outside its scope: {path}"
            )));
        }
        if !manifest_paths.insert(PathBuf::from(path)) {
            return Err(license_policy(format!(
                "vendored SQLite integrity path is listed more than once: {path}"
            )));
        }
    }
    covered.extend(manifest_paths);

    let mut actual = BTreeSet::new();
    collect_regular_files(root, &root.join(&category.scope), &mut actual)?;
    if covered != actual {
        return Err(license_policy(format!(
            "vendored-sqlite scope does not match its integrity coverage: expected {covered:?}, found {actual:?}"
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

fn generate_field_registry(options: &[OsString]) -> Result<(), XtaskError> {
    let check = match options {
        [] => false,
        [option] if option == "--check" => true,
        _ => return Err(XtaskError::InvalidInvocation),
    };
    let toolchain = read_proto_toolchain_contract()?;
    ensure_proto_tool("buf", &toolchain.buf)?;
    let descriptor_bytes = run_proto_tool(
        "buf build descriptor",
        "buf",
        &[
            "build",
            "proto",
            "--as-file-descriptor-set",
            "--output",
            "-",
        ],
    )?
    .stdout;
    let descriptor = FileDescriptorSet::decode(descriptor_bytes.as_slice()).map_err(|source| {
        XtaskError::ProtoGenerate {
            path: workspace_root().join("proto"),
            reason: format!("could not decode descriptor set: {source}"),
        }
    })?;
    let rendered = render_field_registry(&descriptor);
    let path = workspace_root().join("proto/FIELD_NUMBERS.md");
    if check {
        let checked_in =
            fs::read_to_string(&path).map_err(|source| XtaskError::ProtoOutputRead {
                path: path.clone(),
                source,
            })?;
        if checked_in != rendered {
            return Err(XtaskError::ProtoGenerate {
                path,
                reason: "checked-in field-number registry is stale".to_owned(),
            });
        }
    } else {
        fs::write(&path, rendered).map_err(|source| XtaskError::ProtoOutputRead {
            path: path.clone(),
            source,
        })?;
        println!("generate-field-registry: wrote {}", path.display());
    }
    Ok(())
}

#[derive(Default)]
struct RegistryAllocations {
    fields: BTreeSet<(String, String, i32)>,
    enums: BTreeSet<(String, String, i32)>,
    reserved_field_numbers: BTreeSet<(String, i32)>,
    reserved_field_names: BTreeSet<(String, String)>,
    reserved_enum_numbers: BTreeSet<(String, i32)>,
    reserved_enum_names: BTreeSet<(String, String)>,
}

fn render_field_registry(descriptor: &FileDescriptorSet) -> String {
    let mut allocations = RegistryAllocations::default();
    for file in &descriptor.file {
        let package = file.package();
        for message in &file.message_type {
            collect_registry_message(package, "", message, &mut allocations);
        }
        for enumeration in &file.enum_type {
            collect_registry_enum(package, "", enumeration, &mut allocations);
        }
    }

    let mut output = String::from(
        "# CMTI Protobuf Field Number Registry\n\n\
This file is generated from the reviewed descriptor by `cargo run -p xtask -- \
generate-field-registry`. Deleted field and enum names and numbers remain in the \
reserved tables and must never be reused.\n\n\
## Message fields\n\n\
| Fully qualified message | Field | Number | Status |\n\
|---|---|---:|---|\n",
    );
    for (message, field, number) in allocations.fields {
        output.push_str(&format!(
            "| `{message}` | `{field}` | {number} | Active |\n"
        ));
    }
    output.push_str(
        "\n## Enum values\n\n\
| Fully qualified enum | Value | Number | Status |\n\
|---|---|---:|---|\n",
    );
    for (enumeration, value, number) in allocations.enums {
        output.push_str(&format!(
            "| `{enumeration}` | `{value}` | {number} | Active |\n"
        ));
    }
    output.push_str(
        "\n## Reserved message field numbers\n\n\
| Fully qualified message | Number |\n\
|---|---:|\n",
    );
    for (message, number) in allocations.reserved_field_numbers {
        output.push_str(&format!("| `{message}` | {number} |\n"));
    }
    output.push_str(
        "\n## Reserved message field names\n\n\
| Fully qualified message | Name |\n\
|---|---|\n",
    );
    for (message, name) in allocations.reserved_field_names {
        output.push_str(&format!("| `{message}` | `{name}` |\n"));
    }
    output.push_str(
        "\n## Reserved enum numbers\n\n\
| Fully qualified enum | Number |\n\
|---|---:|\n",
    );
    for (enumeration, number) in allocations.reserved_enum_numbers {
        output.push_str(&format!("| `{enumeration}` | {number} |\n"));
    }
    output.push_str(
        "\n## Reserved enum names\n\n\
| Fully qualified enum | Name |\n\
|---|---|\n",
    );
    for (enumeration, name) in allocations.reserved_enum_names {
        output.push_str(&format!("| `{enumeration}` | `{name}` |\n"));
    }
    output
}

fn collect_registry_message(
    package: &str,
    parents: &str,
    message: &DescriptorProto,
    allocations: &mut RegistryAllocations,
) {
    let name = message.name();
    let qualified = if parents.is_empty() {
        format!("{package}.{name}")
    } else {
        format!("{package}.{parents}.{name}")
    };
    for field in &message.field {
        allocations
            .fields
            .insert((qualified.clone(), field.name().to_owned(), field.number()));
    }
    for reserved_name in &message.reserved_name {
        allocations
            .reserved_field_names
            .insert((qualified.clone(), reserved_name.clone()));
    }
    for range in &message.reserved_range {
        for number in range.start()..range.end() {
            allocations
                .reserved_field_numbers
                .insert((qualified.clone(), number));
        }
    }
    let nested_parents = qualified
        .strip_prefix(&format!("{package}."))
        .expect("qualified registry message must retain package");
    for enumeration in &message.enum_type {
        collect_registry_enum(package, nested_parents, enumeration, allocations);
    }
    for nested in &message.nested_type {
        collect_registry_message(package, nested_parents, nested, allocations);
    }
}

fn collect_registry_enum(
    package: &str,
    parents: &str,
    enumeration: &EnumDescriptorProto,
    allocations: &mut RegistryAllocations,
) {
    let name = enumeration.name();
    let qualified = if parents.is_empty() {
        format!("{package}.{name}")
    } else {
        format!("{package}.{parents}.{name}")
    };
    for value in &enumeration.value {
        allocations
            .enums
            .insert((qualified.clone(), value.name().to_owned(), value.number()));
    }
    for reserved_name in &enumeration.reserved_name {
        allocations
            .reserved_enum_names
            .insert((qualified.clone(), reserved_name.clone()));
    }
    for range in &enumeration.reserved_range {
        for number in range.start()..=range.end() {
            allocations
                .reserved_enum_numbers
                .insert((qualified.clone(), number));
        }
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
    run_proto_tool(
        "buf breaking proto",
        "buf",
        &[
            "breaking",
            "proto",
            "--against",
            "proto/baselines/cmti-v1.binpb",
        ],
    )?;
    generate_proto_with_protoc(true, protoc.path())?;
    generate_field_registry(&[OsString::from("--check")])?;
    verify_buf_generation_template()?;
    println!("proto-check: ok");
    Ok(())
}

fn verify_buf_generation_template() -> Result<(), XtaskError> {
    let temporary = tempfile::Builder::new()
        .prefix("cmti-buf-generate-")
        .tempdir()
        .map_err(|source| XtaskError::ProtoOutputDirectory {
            path: env::temp_dir(),
            source,
        })?;
    let output_base = temporary
        .path()
        .to_str()
        .ok_or_else(|| XtaskError::ProtoGenerate {
            path: temporary.path().to_path_buf(),
            reason: "temporary output path must be UTF-8".to_owned(),
        })?;
    let output = Command::new("buf")
        .args([
            "generate",
            "proto",
            "--template",
            "proto/buf.gen.yaml",
            "--output",
            output_base,
        ])
        .current_dir(workspace_root())
        .output()
        .map_err(|source| XtaskError::ProtoToolSpawn {
            tool: "buf",
            source,
        })?;
    if !output.status.success() {
        return Err(XtaskError::ProtoToolFailed {
            command: "buf generate proto",
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let generated = temporary.path().join("crates/local-api/src/generated");
    let checked_in = workspace_root().join("crates/local-api/src/generated");
    compare_generated_outputs(&generated, &checked_in, &expected_generated_proto_files()?)
}

fn generate_proto(check: bool) -> Result<(), XtaskError> {
    let toolchain = read_proto_toolchain_contract()?;
    let protoc = resolve_protoc(&toolchain)?;
    generate_proto_with_protoc(check, protoc.path())
}

fn generate_proto_with_protoc(check: bool, protoc: &Path) -> Result<(), XtaskError> {
    let generated_files = expected_generated_proto_files()?;
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
        compare_generated_outputs(first.path(), second.path(), &generated_files)?;
        compare_generated_outputs(
            first.path(),
            &workspace_root().join("crates/local-api/src/generated"),
            &generated_files,
        )?;
        println!(
            "generate-proto: reproducible ({} files)",
            generated_files.len()
        );
        return Ok(());
    }

    let temporary = tempfile::Builder::new()
        .prefix("cmti-proto-write-")
        .tempdir()
        .map_err(|source| XtaskError::ProtoOutputDirectory {
            path: env::temp_dir(),
            source,
        })?;
    generate_proto_into(temporary.path(), protoc)?;
    let output = workspace_root().join("crates/local-api/src/generated");
    fs::create_dir_all(&output).map_err(|source| XtaskError::ProtoOutputDirectory {
        path: output.clone(),
        source,
    })?;
    for file in &generated_files {
        fs::copy(temporary.path().join(file), output.join(file)).map_err(|source| {
            XtaskError::ProtoOutputRead {
                path: output.join(file),
                source,
            }
        })?;
    }
    remove_unexpected_generated_files(&output, &generated_files)?;
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
    let inventory = discover_proto_inventory()?;
    let proto_files = inventory
        .iter()
        .map(|input| proto_root.join(&input.relative_path))
        .collect::<Vec<_>>();
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
        .build_client(false)
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
    let expected = expected_generated_proto_files()?;
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

fn compare_generated_outputs(
    first: &Path,
    second: &Path,
    generated_files: &BTreeSet<String>,
) -> Result<(), XtaskError> {
    verify_generated_outputs(first)?;
    verify_generated_outputs(second)?;
    for file in generated_files {
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
            return Err(XtaskError::ProtoNotDeterministic { file: file.clone() });
        }
    }
    Ok(())
}

fn remove_unexpected_generated_files(
    output: &Path,
    expected: &BTreeSet<String>,
) -> Result<(), XtaskError> {
    for entry in fs::read_dir(output).map_err(|source| XtaskError::ProtoOutputRead {
        path: output.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| XtaskError::ProtoOutputRead {
            path: output.to_path_buf(),
            source,
        })?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().extension().and_then(|value| value.to_str()) == Some("rs")
            && !expected.contains(&file_name)
        {
            fs::remove_file(entry.path()).map_err(|source| XtaskError::ProtoOutputRead {
                path: entry.path(),
                source,
            })?;
        }
    }
    Ok(())
}

fn discover_proto_inventory() -> Result<Vec<proto_inventory::ProtoInput>, XtaskError> {
    let proto_root = workspace_root().join("proto");
    proto_inventory::discover(&proto_root).map_err(|source| XtaskError::ProtoGenerate {
        path: proto_root,
        reason: source.to_string(),
    })
}

fn expected_proto_files() -> Result<BTreeSet<String>, XtaskError> {
    Ok(discover_proto_inventory()?
        .into_iter()
        .map(|input| input.relative_path)
        .collect())
}

fn expected_generated_proto_files() -> Result<BTreeSet<String>, XtaskError> {
    Ok(proto_inventory::generated_rust_files(
        &discover_proto_inventory()?,
    ))
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
            .build_client(false)
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
    let expected = expected_proto_files()?;
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
    let expected = expected_generated_proto_files()?;
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

    #[test]
    fn model_schema_policy_rejects_permissive_objects_and_unbounded_arrays() {
        let permissive = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://rsitech.ai/schemas/crypto-intelligence/test-v1.json",
            "type": "object",
            "additionalProperties": true,
            "required": ["values"],
            "properties": {
                "values": {
                    "type": "array",
                    "items": {"type": "number"}
                }
            }
        });
        assert!(validate_active_model_schema(Path::new("test.schema.json"), &permissive).is_err());

        let unbounded = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://rsitech.ai/schemas/crypto-intelligence/test-v1.json",
            "type": "object",
            "additionalProperties": false,
            "required": ["values"],
            "properties": {
                "values": {
                    "type": "array",
                    "items": {"type": "number"}
                }
            }
        });
        assert!(validate_active_model_schema(Path::new("test.schema.json"), &unbounded).is_err());
    }

    #[test]
    fn model_artifact_manifest_policy_rejects_extra_or_misdirected_fields() {
        let manifest = serde_json::json!({
            "schema_version": 1,
            "artifact_id": "wrong.schema.json",
            "status": "implemented",
            "local_only": true,
            "extra": true
        });
        assert!(
            validate_model_artifact_manifest(
                &workspace_root().join("models/schemas/test.schema.json"),
                &manifest
            )
            .is_err()
        );
    }

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

    fn plugin_request(files: &[String]) -> CodeGeneratorRequest {
        let expected = expected_proto_files()
            .expect("checked-in protobuf inventory must be discoverable")
            .into_iter()
            .collect::<Vec<_>>();
        CodeGeneratorRequest {
            file_to_generate: files.to_vec(),
            proto_file: expected
                .iter()
                .map(|file| FileDescriptorProto {
                    name: Some(file.clone()),
                    package: Some(format!(
                        "cmti.{}.v1",
                        file.split('/').nth(1).expect("fixture path has a package")
                    )),
                    ..FileDescriptorProto::default()
                })
                .collect(),
            ..CodeGeneratorRequest::default()
        }
    }

    fn plugin_response(files: &[String]) -> CodeGeneratorResponse {
        CodeGeneratorResponse {
            file: files
                .iter()
                .map(|file| code_generator_response::File {
                    name: Some(file.clone()),
                    ..code_generator_response::File::default()
                })
                .collect(),
            ..CodeGeneratorResponse::default()
        }
    }

    #[test]
    fn plugin_requires_the_exact_discovered_input_paths() {
        let proto_files = expected_proto_files()
            .expect("checked-in protobuf inventory must be discoverable")
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(proto_files.len(), 7);
        assert!(validate_plugin_inputs(&plugin_request(&proto_files)).is_ok());

        let missing = &proto_files[..proto_files.len() - 1];
        assert!(matches!(
            validate_plugin_inputs(&plugin_request(missing)),
            Err(XtaskError::ProtoInputMismatch { .. })
        ));

        let mut extra = proto_files;
        extra.push("cmti/unknown/v1/unknown.proto".to_owned());
        assert!(matches!(
            validate_plugin_inputs(&plugin_request(&extra)),
            Err(XtaskError::ProtoInputMismatch { .. })
        ));
    }

    #[test]
    fn plugin_requires_the_exact_discovered_output_filenames() {
        let generated_files = expected_generated_proto_files()
            .expect("checked-in protobuf inventory must be discoverable")
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(generated_files.len(), 7);
        assert!(validate_plugin_outputs(&plugin_response(&generated_files)).is_ok());

        assert!(matches!(
            validate_plugin_outputs(&plugin_response(
                &generated_files[..generated_files.len() - 1]
            )),
            Err(XtaskError::ProtoOutputMismatch { .. })
        ));

        let mut extra = generated_files;
        extra.push("cmti.unknown.v1.rs".to_owned());
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
