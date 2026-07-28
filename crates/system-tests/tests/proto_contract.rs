use local_api::proto::health_v1::{CheckResponse, ServingStatus};
use prost::Message as _;
use prost_types::{DescriptorProto, EnumDescriptorProto, FileDescriptorSet};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("system-tests must be nested under the workspace")
        .to_owned()
}

fn collect_proto_files(root: &Path, directory: &Path, files: &mut BTreeSet<String>) {
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", directory.display()))
        .collect::<Result<Vec<_>, _>>()
        .expect("protobuf directory entries must be readable");
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_proto_files(root, &path, files);
        } else if path.extension().and_then(|value| value.to_str()) == Some("proto") {
            files.insert(
                path.strip_prefix(root)
                    .expect("protobuf path must remain below root")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir_all(destination)
        .unwrap_or_else(|error| panic!("could not create {}: {error}", destination.display()));
    let mut entries = fs::read_dir(source)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", source.display()))
        .collect::<Result<Vec<_>, _>>()
        .expect("directory entries must be readable");
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_directory(&source_path, &destination_path);
        } else {
            fs::copy(&source_path, &destination_path).unwrap_or_else(|error| {
                panic!(
                    "could not copy {} to {}: {error}",
                    source_path.display(),
                    destination_path.display()
                )
            });
        }
    }
}

fn run_buf(arguments: &[&str]) -> Output {
    Command::new("buf")
        .args(arguments)
        .current_dir(workspace_root())
        .output()
        .unwrap_or_else(|error| panic!("could not start buf: {error}"))
}

fn require_success(output: Output, command: &str) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{command} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn descriptor_set() -> FileDescriptorSet {
    let stdout = require_success(
        run_buf(&[
            "build",
            "proto",
            "--as-file-descriptor-set",
            "--output",
            "-",
        ]),
        "buf build proto --as-file-descriptor-set --output -",
    );
    FileDescriptorSet::decode(stdout.as_slice()).expect("Buf descriptor set must decode")
}

#[test]
fn every_checked_in_schema_is_active_in_buf() {
    let root = workspace_root().join("proto");
    let mut checked_in = BTreeSet::new();
    collect_proto_files(&root, &root, &mut checked_in);

    let stdout = require_success(run_buf(&["ls-files", "proto"]), "buf ls-files proto");
    let active = String::from_utf8(stdout)
        .expect("buf file list must be UTF-8")
        .lines()
        .map(|line| line.strip_prefix("proto/").unwrap_or(line).to_owned())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        active, checked_in,
        "Buf must lint, build, and generate every checked-in schema"
    );
}

#[test]
fn descriptor_contains_the_complete_versioned_package_set() {
    let packages = descriptor_set()
        .file
        .into_iter()
        .filter_map(|file| file.package)
        .collect::<BTreeSet<_>>();
    let expected = [
        "cmti.admin.v1",
        "cmti.common.v1",
        "cmti.health.v1",
        "cmti.market.v1",
        "cmti.replay.v1",
        "cmti.risk.v1",
        "cmti.settings.v1",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    assert_eq!(packages, expected);
}

#[test]
fn descriptor_contains_the_exact_approved_service_and_method_inventory() {
    let descriptor = descriptor_set();
    let actual = descriptor
        .file
        .into_iter()
        .flat_map(|file| {
            let package = file.package.expect("schema must have a package");
            file.service.into_iter().map(move |service| {
                (
                    format!(
                        "{package}.{}",
                        service.name.as_deref().expect("service must have a name")
                    ),
                    service
                        .method
                        .into_iter()
                        .map(|method| method.name.expect("RPC method must have a name"))
                        .collect::<BTreeSet<_>>(),
                )
            })
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let expected = [
        ("cmti.common.v1.SessionService", &["NegotiateSession"][..]),
        (
            "cmti.admin.v1.AdminService",
            &[
                "ApplyConfiguration",
                "GetRuntimeStatus",
                "RequestGracefulShutdown",
            ][..],
        ),
        (
            "cmti.admin.v1.ModelRegistryService",
            &["GetModelCard", "ListModels"][..],
        ),
        (
            "cmti.admin.v1.ModelHostService",
            &["GetCapabilities", "InferBatch", "WarmModel"][..],
        ),
        ("cmti.health.v1.HealthService", &["Check"][..]),
        (
            "cmti.health.v1.DataQualityService",
            &["SubscribeQuality"][..],
        ),
        (
            "cmti.market.v1.CatalogService",
            &["GetInstrument", "ListAssets", "ListVenues"][..],
        ),
        (
            "cmti.market.v1.MarketStateService",
            &[
                "GetOrderBookSnapshot",
                "SubscribeAssetState",
                "SubscribeVenueState",
            ][..],
        ),
        (
            "cmti.risk.v1.RiskService",
            &[
                "GetCuspState",
                "GetEvidenceBundle",
                "GetForecastSnapshot",
                "GetScenarioDistribution",
                "SubscribeForecasts",
            ][..],
        ),
        (
            "cmti.risk.v1.ForecastService",
            &[
                "GetEvidence",
                "GetForecast",
                "GetScenario",
                "SubscribeForecasts",
            ][..],
        ),
        ("cmti.risk.v1.CuspService", &["GetCuspState"][..]),
        (
            "cmti.risk.v1.AlertService",
            &["ListRules", "SubscribeAlertEvents", "UpsertRule"][..],
        ),
        (
            "cmti.replay.v1.ReplayService",
            &["ControlReplay", "CreateReplay", "SubscribeReplayState"][..],
        ),
        (
            "cmti.settings.v1.SettingsService",
            &["GetSettings", "UpdateSettings"][..],
        ),
        ("cmti.settings.v1.ExportService", &["CreateExport"][..]),
    ]
    .into_iter()
    .map(|(service, methods)| {
        (
            service.to_owned(),
            methods
                .iter()
                .map(|method| (*method).to_owned())
                .collect::<BTreeSet<_>>(),
        )
    })
    .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(actual, expected);
}

fn collect_message_fields(
    package: &str,
    parents: &str,
    message: &DescriptorProto,
    fields: &mut BTreeSet<(String, String, i32)>,
) {
    let message_name = message.name.as_deref().expect("message must have a name");
    let qualified_name = if parents.is_empty() {
        format!("{package}.{message_name}")
    } else {
        format!("{package}.{parents}.{message_name}")
    };
    for field in &message.field {
        fields.insert((
            qualified_name.clone(),
            field.name.clone().expect("field must have a name"),
            field.number.expect("field must have a number"),
        ));
    }
    let nested_parents = qualified_name
        .strip_prefix(&format!("{package}."))
        .expect("qualified message must retain its package");
    for nested in &message.nested_type {
        collect_message_fields(package, nested_parents, nested, fields);
    }
}

fn collect_enum_values(
    package: &str,
    parents: &str,
    enumeration: &EnumDescriptorProto,
    values: &mut BTreeSet<(String, String, i32)>,
) {
    let enum_name = enumeration.name.as_deref().expect("enum must have a name");
    let qualified_name = if parents.is_empty() {
        format!("{package}.{enum_name}")
    } else {
        format!("{package}.{parents}.{enum_name}")
    };
    for value in &enumeration.value {
        values.insert((
            qualified_name.clone(),
            value.name.clone().expect("enum value must have a name"),
            value.number.expect("enum value must have a number"),
        ));
    }
}

#[derive(Default)]
struct Reservations {
    field_names: BTreeSet<(String, String)>,
    field_numbers: BTreeSet<(String, i32)>,
    enum_names: BTreeSet<(String, String)>,
    enum_numbers: BTreeSet<(String, i32)>,
}

fn collect_enum_reservations(
    package: &str,
    parents: &str,
    enumeration: &EnumDescriptorProto,
    reservations: &mut Reservations,
) {
    let enum_name = enumeration.name.as_deref().expect("enum must have a name");
    let qualified_name = if parents.is_empty() {
        format!("{package}.{enum_name}")
    } else {
        format!("{package}.{parents}.{enum_name}")
    };
    for name in &enumeration.reserved_name {
        reservations
            .enum_names
            .insert((qualified_name.clone(), name.clone()));
    }
    for range in &enumeration.reserved_range {
        let start = range.start.expect("enum reserved range must have a start");
        let end = range.end.expect("enum reserved range must have an end");
        for number in start..=end {
            reservations
                .enum_numbers
                .insert((qualified_name.clone(), number));
        }
    }
}

fn collect_message_reservations(
    package: &str,
    parents: &str,
    message: &DescriptorProto,
    reservations: &mut Reservations,
) {
    let message_name = message.name.as_deref().expect("message must have a name");
    let qualified_name = if parents.is_empty() {
        format!("{package}.{message_name}")
    } else {
        format!("{package}.{parents}.{message_name}")
    };
    for name in &message.reserved_name {
        reservations
            .field_names
            .insert((qualified_name.clone(), name.clone()));
    }
    for range in &message.reserved_range {
        let start = range
            .start
            .expect("message reserved range must have a start");
        let end = range.end.expect("message reserved range must have an end");
        for number in start..end {
            reservations
                .field_numbers
                .insert((qualified_name.clone(), number));
        }
    }
    let nested_parents = qualified_name
        .strip_prefix(&format!("{package}."))
        .expect("qualified message must retain its package");
    for enumeration in &message.enum_type {
        collect_enum_reservations(package, nested_parents, enumeration, reservations);
    }
    for nested in &message.nested_type {
        collect_message_reservations(package, nested_parents, nested, reservations);
    }
}

fn parse_registry_row(line: &str) -> Option<((String, String, i32), &str)> {
    let columns = line.split('|').map(str::trim).collect::<Vec<_>>();
    if columns.len() != 6 || !matches!(columns[4], "Active" | "Reserved") {
        return None;
    }
    let qualified_name = columns[1].strip_prefix('`')?.strip_suffix('`')?;
    let member_name = columns[2].strip_prefix('`')?.strip_suffix('`')?;
    let number = columns[3].parse().ok()?;
    Some((
        (qualified_name.to_owned(), member_name.to_owned(), number),
        columns[4],
    ))
}

fn parse_registry_reservation(line: &str) -> Option<(String, String)> {
    let columns = line.split('|').map(str::trim).collect::<Vec<_>>();
    if columns.len() != 4 {
        return None;
    }
    let qualified_name = columns[1].strip_prefix('`')?.strip_suffix('`')?;
    let member = columns[2].trim_matches('`');
    if member.is_empty() || member.starts_with('-') {
        return None;
    }
    Some((qualified_name.to_owned(), member.to_owned()))
}

#[test]
fn field_number_registry_exactly_covers_the_descriptor() {
    let registry = fs::read_to_string(workspace_root().join("proto/FIELD_NUMBERS.md"))
        .expect("field number registry must be readable");
    let mut registered_fields = BTreeSet::new();
    let mut registered_enums = BTreeSet::new();
    let mut registered_reservations = Reservations::default();
    let mut section = "";
    for line in registry.lines() {
        if line.starts_with("## ") {
            section = line;
        }
        if let Some(((qualified_name, member_name, number), status)) = parse_registry_row(line) {
            match (section, status) {
                ("## Message fields", "Active") => {
                    registered_fields.insert((qualified_name, member_name, number));
                }
                ("## Enum values", "Active") => {
                    registered_enums.insert((qualified_name, member_name, number));
                }
                _ => panic!("active registry row appears in the wrong section"),
            }
        }
        if let Some((qualified_name, member)) = parse_registry_reservation(line) {
            match section {
                "## Reserved message field numbers" => {
                    registered_reservations.field_numbers.insert((
                        qualified_name,
                        member.parse().expect("reserved field number"),
                    ));
                }
                "## Reserved message field names" => {
                    registered_reservations
                        .field_names
                        .insert((qualified_name, member));
                }
                "## Reserved enum numbers" => {
                    registered_reservations.enum_numbers.insert((
                        qualified_name,
                        member.parse().expect("reserved enum number"),
                    ));
                }
                "## Reserved enum names" => {
                    registered_reservations
                        .enum_names
                        .insert((qualified_name, member));
                }
                _ => {}
            }
        }
    }

    let descriptor = descriptor_set();
    let mut actual_fields = BTreeSet::new();
    let mut actual_enums = BTreeSet::new();
    let mut actual_reservations = Reservations::default();
    for file in descriptor.file {
        let package = file.package.expect("schema must have a package");
        for message in &file.message_type {
            collect_message_fields(&package, "", message, &mut actual_fields);
            let message_name = message.name.as_deref().expect("message must have a name");
            collect_message_reservations(&package, "", message, &mut actual_reservations);
            for enumeration in &message.enum_type {
                collect_enum_values(&package, message_name, enumeration, &mut actual_enums);
            }
        }
        for enumeration in &file.enum_type {
            collect_enum_values(&package, "", enumeration, &mut actual_enums);
            collect_enum_reservations(&package, "", enumeration, &mut actual_reservations);
        }
    }

    assert_eq!(registered_fields, actual_fields);
    assert_eq!(registered_enums, actual_enums);
    assert_eq!(
        registered_reservations.field_names,
        actual_reservations.field_names
    );
    assert_eq!(
        registered_reservations.field_numbers,
        actual_reservations.field_numbers
    );
    assert_eq!(
        registered_reservations.enum_names,
        actual_reservations.enum_names
    );
    assert_eq!(
        registered_reservations.enum_numbers,
        actual_reservations.enum_numbers
    );
}

#[test]
fn streaming_and_error_contracts_are_structurally_complete() {
    let descriptor = descriptor_set();
    let mut stream_response_types = BTreeSet::new();
    let mut stream_request_types = BTreeSet::new();
    let mut streaming_methods = BTreeSet::new();
    let mut messages = std::collections::BTreeMap::new();
    let mut enums = std::collections::BTreeMap::new();
    for file in descriptor.file {
        let package = file.package.expect("schema must have a package");
        for service in file.service {
            let service_name = service.name.expect("service must have a name");
            for method in service.method {
                if method.server_streaming == Some(true) {
                    streaming_methods.insert(format!(
                        "{package}.{service_name}.{}",
                        method.name.as_deref().expect("RPC method must have a name")
                    ));
                    stream_request_types.insert(
                        method
                            .input_type
                            .clone()
                            .expect("streaming method must have an input type"),
                    );
                    stream_response_types.insert(
                        method
                            .output_type
                            .expect("streaming method must have an output type"),
                    );
                }
            }
        }
        for message in file.message_type {
            messages.insert(
                format!(
                    ".{package}.{}",
                    message.name.as_deref().expect("message must have a name")
                ),
                message,
            );
        }
        for enumeration in file.enum_type {
            enums.insert(
                format!(
                    ".{package}.{}",
                    enumeration.name.as_deref().expect("enum must have a name")
                ),
                enumeration,
            );
        }
    }

    assert!(
        !stream_response_types.is_empty(),
        "the normative contract must exercise server-streaming semantics"
    );
    assert_eq!(
        streaming_methods,
        [
            "cmti.health.v1.DataQualityService.SubscribeQuality",
            "cmti.market.v1.MarketStateService.SubscribeAssetState",
            "cmti.market.v1.MarketStateService.SubscribeVenueState",
            "cmti.replay.v1.ReplayService.SubscribeReplayState",
            "cmti.risk.v1.AlertService.SubscribeAlertEvents",
            "cmti.risk.v1.ForecastService.SubscribeForecasts",
            "cmti.risk.v1.RiskService.SubscribeForecasts",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    for request_type in stream_request_types {
        let request = messages
            .get(&request_type)
            .unwrap_or_else(|| panic!("stream request descriptor is missing: {request_type}"));
        assert!(
            request
                .field
                .iter()
                .any(|field| field.name.as_deref() == Some("resume_token")),
            "{request_type} must support bounded resumption"
        );
        assert!(
            request
                .field
                .iter()
                .all(|field| field.name.as_deref() != Some("include_initial_snapshot")),
            "{request_type} must not allow clients to suppress the initial snapshot"
        );
    }
    for response_type in stream_response_types {
        let response = messages
            .get(&response_type)
            .unwrap_or_else(|| panic!("stream response descriptor is missing: {response_type}"));
        assert!(
            response.field.iter().any(|field| {
                field.type_name.as_deref() == Some(".cmti.common.v1.StreamMetadata")
            }),
            "{response_type} must carry the canonical stream metadata"
        );
    }

    let stream = messages
        .get(".cmti.common.v1.StreamMetadata")
        .expect("stream metadata must exist");
    let stream_fields = stream
        .field
        .iter()
        .filter_map(|field| field.name.as_deref())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        stream_fields,
        [
            "as_of_time",
            "resume_token",
            "schema_version",
            "snapshot_or_delta",
            "stream_id",
            "stream_sequence",
            "coalesced_since_previous",
            "delivery_policy",
            "dropped_since_previous",
            "terminal_error",
            "terminal_status",
        ]
        .into_iter()
        .collect()
    );

    let errors = enums
        .get(".cmti.common.v1.ErrorCode")
        .expect("stable error code enum must exist");
    assert_eq!(errors.value.first().and_then(|value| value.number), Some(0));
    let actual_error_codes = errors
        .value
        .iter()
        .map(|value| value.name.clone().expect("error code must have a name"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        actual_error_codes,
        [
            "ERROR_CODE_UNSPECIFIED",
            "ERROR_CODE_INVALID_ARGUMENT",
            "ERROR_CODE_UNAUTHENTICATED",
            "ERROR_CODE_PERMISSION_DENIED",
            "ERROR_CODE_NOT_FOUND",
            "ERROR_CODE_INCOMPATIBLE_VERSION",
            "ERROR_CODE_SOURCE_UNHEALTHY",
            "ERROR_CODE_MODEL_UNAVAILABLE",
            "ERROR_CODE_MODEL_INCOMPATIBLE",
            "ERROR_CODE_INSUFFICIENT_DATA",
            "ERROR_CODE_OUT_OF_DISTRIBUTION",
            "ERROR_CODE_CAPACITY_EXCEEDED",
            "ERROR_CODE_STORAGE_PRESSURE",
            "ERROR_CODE_REPLAY_NOT_AVAILABLE",
            "ERROR_CODE_INTERNAL_INVARIANT_VIOLATION",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
}

#[test]
fn protocol_handshake_exposes_required_compatibility_fields() {
    let descriptor = descriptor_set();
    let mut messages = std::collections::BTreeMap::new();
    for file in descriptor.file {
        let package = file.package.expect("schema must have a package");
        for message in file.message_type {
            messages.insert(
                format!(
                    "{package}.{}",
                    message.name.as_deref().expect("message must have a name")
                ),
                message,
            );
        }
    }
    let request = messages
        .get("cmti.common.v1.NegotiateSessionRequest")
        .expect("protocol request must exist");
    let response = messages
        .get("cmti.common.v1.NegotiateSessionResponse")
        .expect("protocol response must exist");
    let exchanged = request
        .field
        .iter()
        .chain(&response.field)
        .filter_map(|field| field.name.as_deref())
        .collect::<BTreeSet<_>>();
    for required in [
        "api_major",
        "api_minor",
        "daemon_build_id",
        "app_build_id",
        "schema_bundle_hash",
        "feature_capabilities",
        "model_runtime_capabilities",
        "minimum_compatible_version",
    ] {
        assert!(
            exchanged.contains(required),
            "protocol handshake is missing {required}"
        );
    }
}

#[test]
fn rust_binding_accepts_an_unknown_optional_field_without_corrupting_known_fields() {
    let future_wire = [0x08, 0x01, 0x98, 0x06, 0x07];
    let decoded =
        CheckResponse::decode(future_wire.as_slice()).expect("future optional field must decode");
    assert_eq!(decoded.status, ServingStatus::Serving as i32);
    assert!(decoded.protocol.is_none());
    assert_eq!(
        decoded.encode_to_vec(),
        [0x08, 0x01],
        "prost accepts but does not retain unknown fields; Swift preservation is tested separately"
    );
}

#[test]
fn rust_binding_preserves_a_future_enum_number_for_application_fallback() {
    let future_wire = [0x08, 0x63];
    let decoded =
        CheckResponse::decode(future_wire.as_slice()).expect("future enum value must decode");
    assert_eq!(decoded.status, 99);
    assert!(
        ServingStatus::try_from(decoded.status).is_err(),
        "application mapping must recognize the numeric value as unknown"
    );
    assert_eq!(decoded.encode_to_vec(), future_wire);
}

#[test]
fn checked_in_bindings_cover_every_package_with_one_way_authority() {
    let descriptor = descriptor_set();
    let rust_expected = descriptor
        .file
        .iter()
        .filter_map(|file| file.package.as_ref())
        .map(|package| format!("{package}.rs"))
        .collect::<BTreeSet<_>>();
    let rust_directory = workspace_root().join("crates/local-api/src/generated");
    let rust_actual = fs::read_dir(&rust_directory)
        .expect("checked-in Rust bindings directory must be readable")
        .map(|entry| {
            entry
                .expect("checked-in Rust binding entry must be readable")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(rust_actual, rust_expected);
    for file in &rust_actual {
        let source = fs::read_to_string(rust_directory.join(file))
            .expect("checked-in Rust binding must be readable");
        assert!(
            !source.contains("_client {"),
            "daemon Rust binding must not expose a generated client: {file}"
        );
    }

    let swift_expected = descriptor
        .file
        .iter()
        .filter_map(|file| file.name.as_ref())
        .flat_map(|name| {
            let stem = name
                .strip_suffix(".proto")
                .expect("descriptor input must end in .proto")
                .replace('/', "_");
            [format!("{stem}.pb.swift"), format!("{stem}.grpc.swift")]
        })
        .collect::<BTreeSet<_>>();
    let swift_directory = workspace_root()
        .join("apps/macos/Packages/TransitionClient/Sources/TransitionClient/Generated");
    let swift_actual = fs::read_dir(&swift_directory)
        .expect("checked-in Swift bindings directory must be readable")
        .map(|entry| {
            entry
                .expect("checked-in Swift binding entry must be readable")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(swift_actual, swift_expected);
    for file in swift_actual
        .iter()
        .filter(|file| file.ends_with(".grpc.swift"))
    {
        let source = fs::read_to_string(swift_directory.join(file))
            .expect("checked-in Swift gRPC binding must be readable");
        assert!(
            !source.contains("RegistrableRPCService")
                && !source.contains("ServerRequest")
                && !source.contains("ServerResponse"),
            "native Swift binding must not expose a generated server: {file}"
        );
        if !source.contains("This file contained no services.") {
            assert!(
                source.contains("ClientProtocol"),
                "native Swift gRPC binding must expose its generated client: {file}"
            );
        }
    }
}

#[test]
fn buf_standard_lint_and_descriptor_build_succeed() {
    require_success(run_buf(&["lint", "proto"]), "buf lint proto");
    require_success(run_buf(&["build", "proto"]), "buf build proto");
}

#[test]
fn dependency_lock_exists_if_and_only_if_buf_has_dependencies() {
    let configuration =
        fs::read_to_string(workspace_root().join("proto/buf.yaml")).expect("buf.yaml must exist");
    let has_dependencies = configuration
        .lines()
        .any(|line| line.trim_start().starts_with("deps:"));
    assert_eq!(
        workspace_root().join("proto/buf.lock").is_file(),
        has_dependencies,
        "buf.lock is generated provenance only when external dependencies exist"
    );
}

#[test]
fn checked_in_baseline_has_the_declared_digest_and_accepts_the_current_contract() {
    let baseline = workspace_root().join("proto/baselines/cmti-v1.binpb");
    let bytes = fs::read(&baseline).expect("compatibility baseline must be readable");
    let actual = hex::encode(Sha256::digest(bytes));
    let declaration = fs::read_to_string(workspace_root().join("proto/baselines/cmti-v1.sha256"))
        .expect("baseline digest declaration must be readable");
    let expected = declaration
        .split_whitespace()
        .next()
        .expect("baseline digest declaration must contain a digest");
    assert_eq!(actual, expected);
    require_success(
        run_buf(&[
            "breaking",
            "proto",
            "--against",
            "proto/baselines/cmti-v1.binpb",
        ]),
        "buf breaking proto --against proto/baselines/cmti-v1.binpb",
    );
}

#[test]
fn compatibility_gate_rejects_a_deleted_field() {
    let temporary = tempfile::tempdir().expect("temporary candidate module must be creatable");
    let source_root = workspace_root().join("proto");
    fs::copy(
        source_root.join("buf.yaml"),
        temporary.path().join("buf.yaml"),
    )
    .expect("candidate Buf configuration must copy");
    copy_directory(&source_root.join("cmti"), &temporary.path().join("cmti"));

    let market_path = temporary.path().join("cmti/market/v1/market.proto");
    let market =
        fs::read_to_string(&market_path).expect("candidate market schema must be readable");
    let mutated = market.replace("  string symbol = 2;\n", "");
    assert_ne!(market, mutated, "mutation fixture must delete a real field");
    fs::write(&market_path, mutated).expect("candidate mutation must be writable");

    let baseline = source_root.join("baselines/cmti-v1.binpb");
    let output = Command::new("buf")
        .args([
            "breaking",
            temporary
                .path()
                .to_str()
                .expect("temporary path must be UTF-8"),
            "--against",
            baseline.to_str().expect("baseline path must be UTF-8"),
        ])
        .current_dir(workspace_root())
        .output()
        .expect("could not start buf breaking");
    assert!(
        !output.status.success(),
        "deleting a frozen field must fail compatibility"
    );
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostic.contains("Previously present field")
            && diagnostic.contains("symbol")
            && diagnostic.contains("was deleted"),
        "failure must identify the deleted field:\n{diagnostic}"
    );
}
