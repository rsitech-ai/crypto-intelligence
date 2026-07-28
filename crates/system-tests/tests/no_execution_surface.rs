use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

#[path = "../../../build-support/generated_binding_policy.rs"]
mod generated_binding_policy;

const APPROVED_FOUNDATION_DEPENDENCIES: &[&str] = &[
    "blake3",
    "clap",
    "config",
    "connector-binance",
    "crc32c",
    "domain",
    "event-envelope",
    "fixed-decimal",
    "hex",
    "hmac",
    "local-api",
    "observability",
    "orderbook",
    "prost",
    "prost-build",
    "rand",
    "raw-wal",
    "rustix",
    "schemars",
    "serde",
    "serde_json",
    "sha2",
    "thiserror",
    "tokio",
    "tokio-stream",
    "toml",
    "tonic",
    "tonic-prost",
    "tonic-prost-build",
    "zeroize",
];
const APPROVED_FOUNDATION_NETWORK_CAPABILITIES: &[&str] = &[
    "configs/default.toml bind_address=\"127.0.0.1:0\"",
    "crates/local-api/src/server.rs TcpListener::bind(crate::server::LOOPBACK_BIND)",
];
const APPROVED_FOUNDATION_DEPENDENCY_CAPABILITIES: &[&str] =
    &["rustix@1.1.4:features=alloc,default,fs,process,std"];
const APPROVED_LOCAL_API_BUILD_SCRIPT_BLAKE3: &str =
    "c763491bf93f30a5f85ceeb4c9d853053d1788facc5748df1635047fa6b87b3a";
const APPROVED_GENERATED_BINDING_POLICY_BLAKE3: &str =
    "cc67fe4f3dfe64e5040d2c7138c11be0b97ea4c1dacc9ba62eebc1b58c91b70e";

#[derive(Debug, Eq, PartialEq)]
struct SurfaceInventory {
    cargo_features: Vec<String>,
    direct_dependencies: Vec<String>,
    dependency_capabilities: Vec<String>,
    generated_binding_capabilities: Vec<String>,
    source_ownership_capabilities: Vec<String>,
    protobuf_methods: Vec<String>,
    cli_options: Vec<String>,
    configuration_fields: Vec<String>,
    network_capabilities: Vec<String>,
    runtime_declarations: Vec<String>,
}

impl SurfaceInventory {
    fn discover(root: &Path) -> Result<Self, Box<dyn Error>> {
        let root = fs::canonicalize(root)?;
        let metadata_output = Command::new("cargo")
            .args(cargo_metadata_args())
            .current_dir(&root)
            .output()?;
        if !metadata_output.status.success() {
            return Err(format!(
                "cargo metadata failed: {}",
                String::from_utf8_lossy(&metadata_output.stderr)
            )
            .into());
        }
        let metadata: Value = serde_json::from_slice(&metadata_output.stdout)?;
        let workspace_members = metadata["workspace_members"]
            .as_array()
            .ok_or("cargo metadata omitted workspace_members")?
            .iter()
            .filter_map(Value::as_str)
            .collect::<BTreeSet<_>>();
        let packages = metadata["packages"]
            .as_array()
            .ok_or("cargo metadata omitted packages")?;
        let production_members = production_package_closure(packages, &workspace_members)?;
        let dependency_capabilities =
            resolved_dependency_capabilities(&metadata, &production_members)?;
        let mut cargo_features = BTreeSet::new();
        let mut direct_dependencies = BTreeSet::new();
        let mut active_sources = Vec::new();
        let mut production_build_scripts = Vec::new();
        let mut source_ownership_capabilities = Vec::new();
        for package in packages {
            let id = package["id"].as_str().ok_or("package omitted id")?;
            if !production_members.contains(id) {
                continue;
            }
            let package_name = package["name"].as_str().ok_or("package omitted name")?;
            for feature in package["features"]
                .as_object()
                .ok_or("package features were not an object")?
                .keys()
            {
                cargo_features.insert(format!("{package_name}:{feature}"));
            }
            for dependency in package["dependencies"]
                .as_array()
                .ok_or("package dependencies were not an array")?
            {
                if dependency["kind"].as_str() == Some("dev") {
                    continue;
                }
                direct_dependencies.insert(
                    dependency["name"]
                        .as_str()
                        .ok_or("dependency omitted name")?
                        .to_owned(),
                );
            }
            let manifest = fs::canonicalize(PathBuf::from(
                package["manifest_path"]
                    .as_str()
                    .ok_or("package omitted manifest_path")?,
            ))?;
            let package_root = manifest.parent().ok_or("manifest omitted parent")?;
            collect_rust_sources(&package_root.join("src"), &mut active_sources)?;
            let targets = package["targets"]
                .as_array()
                .ok_or("package targets were not an array")?;
            collect_production_target_sources(
                package_name,
                package_root,
                targets,
                &mut active_sources,
                &mut production_build_scripts,
                &mut source_ownership_capabilities,
            )?;
        }
        active_sources.sort();
        active_sources.dedup();
        production_build_scripts.sort();
        production_build_scripts.dedup();
        source_ownership_capabilities.sort();
        source_ownership_capabilities.dedup();

        let help_output = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "--locked",
                "-p",
                "cryptoriskd",
                "--",
                "--help",
            ])
            .current_dir(&root)
            .output()?;
        if !help_output.status.success() {
            return Err(format!(
                "cryptoriskd --help failed: {}",
                String::from_utf8_lossy(&help_output.stderr)
            )
            .into());
        }
        let cli_options = long_options(&String::from_utf8(help_output.stdout)?);
        let protobuf_methods = active_protobuf_methods(&root)?;
        let configuration_fields = configuration_fields(&root)?;
        let network_capabilities = network_capabilities(&root, &active_sources)?;
        let generated_binding_capabilities =
            generated_binding_capabilities(&root, &production_build_scripts)?;
        let runtime_declarations = active_sources
            .iter()
            .map(|source| execution_declarations_in_source(&root, source))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();

        Ok(Self {
            cargo_features: cargo_features.into_iter().collect(),
            direct_dependencies: direct_dependencies.into_iter().collect(),
            dependency_capabilities,
            generated_binding_capabilities,
            source_ownership_capabilities,
            protobuf_methods,
            cli_options,
            configuration_fields,
            network_capabilities,
            runtime_declarations,
        })
    }

    fn execution_authority_violations(&self) -> Vec<String> {
        let mut violations = Vec::new();
        for feature in &self.cargo_features {
            if has_execution_term(feature) {
                violations.push(format!("cargo feature: {feature}"));
            }
        }
        let approved = APPROVED_FOUNDATION_DEPENDENCIES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for dependency in &self.direct_dependencies {
            if !approved.contains(dependency.as_str()) {
                violations.push(format!(
                    "dependency outside the foundation allowlist: {dependency}"
                ));
            }
        }
        let approved_dependency_capabilities = APPROVED_FOUNDATION_DEPENDENCY_CAPABILITIES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for capability in &self.dependency_capabilities {
            if !approved_dependency_capabilities.contains(capability.as_str()) {
                violations.push(format!(
                    "dependency capability outside ownership policy: {capability}"
                ));
            }
        }
        for capability in &self.generated_binding_capabilities {
            violations.push(format!(
                "generated binding capability outside ownership policy: {capability}"
            ));
        }
        for capability in &self.source_ownership_capabilities {
            violations.push(format!("source ownership outside policy: {capability}"));
        }
        for method in &self.protobuf_methods {
            if has_execution_term(method) {
                violations.push(format!("protobuf method: {method}"));
            }
        }
        for option in &self.cli_options {
            if has_execution_term(option) {
                violations.push(format!("CLI option: {option}"));
            }
        }
        for field in &self.configuration_fields {
            if has_execution_term(field) {
                violations.push(format!("configuration field: {field}"));
            }
        }
        let approved_network = APPROVED_FOUNDATION_NETWORK_CAPABILITIES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for capability in &self.network_capabilities {
            if !approved_network.contains(capability.as_str()) {
                violations.push(format!(
                    "network capability outside ownership policy: {capability}"
                ));
            }
        }
        for declaration in &self.runtime_declarations {
            violations.push(format!("runtime declaration: {declaration}"));
        }
        violations
    }
}

fn cargo_metadata_args() -> [&'static str; 5] {
    [
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--all-features",
    ]
}

fn production_package_closure<'a>(
    packages: &'a [Value],
    workspace_members: &BTreeSet<&'a str>,
) -> Result<BTreeSet<&'a str>, Box<dyn Error>> {
    let mut package_by_name = BTreeMap::new();
    for package in packages {
        let id = package["id"].as_str().ok_or("package omitted id")?;
        if workspace_members.contains(id) {
            package_by_name.insert(
                package["name"].as_str().ok_or("package omitted name")?,
                package,
            );
        }
    }
    let root = package_by_name
        .get("cryptoriskd")
        .ok_or("active workspace omitted cryptoriskd")?;
    let mut pending = vec![*root];
    let mut closure = BTreeSet::new();
    while let Some(package) = pending.pop() {
        let id = package["id"].as_str().ok_or("package omitted id")?;
        if !closure.insert(id) {
            continue;
        }
        for dependency in package["dependencies"]
            .as_array()
            .ok_or("package dependencies were not an array")?
        {
            if dependency["kind"].as_str() == Some("dev") {
                continue;
            }
            if let Some(local) = dependency["name"]
                .as_str()
                .and_then(|name| package_by_name.get(name))
            {
                pending.push(*local);
            }
        }
    }
    Ok(closure)
}

fn resolved_dependency_capabilities(
    metadata: &Value,
    production_members: &BTreeSet<&str>,
) -> Result<Vec<String>, Box<dyn Error>> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata omitted packages")?;
    let package_by_id = packages
        .iter()
        .map(|package| Ok((package["id"].as_str().ok_or("package omitted id")?, package)))
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;
    let nodes = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("cargo metadata omitted resolved nodes")?;
    let node_by_id = nodes
        .iter()
        .map(|node| Ok((node["id"].as_str().ok_or("resolved node omitted id")?, node)))
        .collect::<Result<BTreeMap<_, _>, Box<dyn Error>>>()?;

    let mut pending = production_members.iter().copied().collect::<Vec<_>>();
    let mut resolved_closure = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !resolved_closure.insert(id) {
            continue;
        }
        let node = node_by_id
            .get(id)
            .ok_or("production package omitted resolved node")?;
        for dependency in node["dependencies"]
            .as_array()
            .ok_or("resolved node dependencies were not an array")?
        {
            pending.push(
                dependency
                    .as_str()
                    .ok_or("resolved dependency was not a package id")?,
            );
        }
    }

    let mut capabilities = BTreeSet::new();
    for id in resolved_closure {
        let package = package_by_id
            .get(id)
            .ok_or("resolved package omitted metadata")?;
        if package["name"].as_str() != Some("rustix") {
            continue;
        }
        let node = node_by_id
            .get(id)
            .ok_or("resolved rustix package omitted node")?;
        let mut features = node["features"]
            .as_array()
            .ok_or("resolved rustix features were not an array")?
            .iter()
            .map(|feature| {
                feature
                    .as_str()
                    .ok_or("resolved rustix feature was not a string")
            })
            .collect::<Result<Vec<_>, _>>()?;
        features.sort_unstable();
        capabilities.insert(format!(
            "rustix@{}:features={}",
            package["version"]
                .as_str()
                .ok_or("rustix package omitted version")?,
            features.join(",")
        ));
    }
    Ok(capabilities.into_iter().collect())
}

fn collect_rust_sources(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_rust_sources(&path, output)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            output.push(path);
        }
    }
    Ok(())
}

fn collect_production_target_sources(
    package_name: &str,
    package_root: &Path,
    targets: &[Value],
    active_sources: &mut Vec<PathBuf>,
    production_build_scripts: &mut Vec<PathBuf>,
    ownership_violations: &mut Vec<String>,
) -> Result<(), Box<dyn Error>> {
    let package_root = fs::canonicalize(package_root)?;
    for target in targets {
        let target_name = target["name"].as_str().ok_or("target omitted name")?;
        let kinds = target["kind"]
            .as_array()
            .ok_or("target kind was not an array")?
            .iter()
            .map(|kind| kind.as_str().ok_or("target kind was not a string"))
            .collect::<Result<BTreeSet<_>, _>>()?;
        let source = fs::canonicalize(PathBuf::from(
            target["src_path"]
                .as_str()
                .ok_or("target omitted src_path")?,
        ))?;

        if kinds.contains("custom-build") {
            production_build_scripts.push(source);
            continue;
        }
        if kinds.contains("test") {
            continue;
        }

        let (ownership_directory, ownership_label) = if kinds.contains("example") {
            (package_root.join("examples"), "package/examples")
        } else if kinds.contains("bench") {
            (package_root.join("benches"), "package/benches")
        } else {
            (package_root.join("src"), "package/src")
        };
        let ownership_root = fs::canonicalize(&ownership_directory)?;
        if !source.starts_with(&ownership_root) {
            ownership_violations.push(format!(
                "{package_name} target {target_name} source {} escapes {ownership_label}",
                source.display()
            ));
        }
        active_sources.push(source);
    }
    Ok(())
}

fn generated_binding_capabilities(
    root: &Path,
    build_scripts: &[PathBuf],
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut capabilities = BTreeSet::new();
    let expected = fs::canonicalize(root.join("crates/local-api/build.rs"))?;
    if build_scripts != [expected.clone()] {
        capabilities.insert(format!(
            "production custom-build target inventory differs: {build_scripts:?}"
        ));
    }
    for build_script in build_scripts {
        let relative = build_script
            .strip_prefix(root)
            .unwrap_or(build_script)
            .to_string_lossy();
        if build_script != &expected {
            capabilities.insert(format!(
                "{relative} unapproved production custom-build target"
            ));
            continue;
        }
        let bytes = fs::read(build_script)?;
        let digest = blake3::hash(&bytes).to_hex().to_string();
        if digest != APPROVED_LOCAL_API_BUILD_SCRIPT_BLAKE3 {
            capabilities.insert(format!("{relative} build policy digest {digest}"));
        }
    }
    let generated_policy = root.join("build-support/generated_binding_policy.rs");
    let generated_policy_digest = blake3::hash(&fs::read(&generated_policy)?)
        .to_hex()
        .to_string();
    if generated_policy_digest != APPROVED_GENERATED_BINDING_POLICY_BLAKE3 {
        capabilities.insert(format!(
            "build-support/generated_binding_policy.rs digest {generated_policy_digest}"
        ));
    }
    Ok(capabilities.into_iter().collect())
}

fn long_options(help: &str) -> Vec<String> {
    let mut options = BTreeSet::new();
    for token in help.split_whitespace() {
        if let Some(option) = token.strip_prefix("--") {
            let name = option.trim_end_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '-'
            });
            if !name.is_empty() {
                options.insert(format!("--{name}"));
            }
        }
    }
    options.into_iter().collect()
}

fn active_protobuf_methods(root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let buf = fs::read_to_string(root.join("proto/buf.yaml"))?;
    let mut excludes = BTreeSet::new();
    let mut in_excludes = false;
    for line in buf.lines() {
        let trimmed = line.trim();
        if trimmed == "excludes:" {
            in_excludes = true;
            continue;
        }
        if in_excludes {
            if let Some(excluded) = trimmed.strip_prefix("- ") {
                excludes.insert(excluded.to_owned());
                continue;
            }
            if !trimmed.is_empty() {
                in_excludes = false;
            }
        }
    }

    let mut methods = BTreeSet::new();
    let proto_root = root.join("proto");
    for version_directory in fs::read_dir(&proto_root)? {
        let version_directory = version_directory?.path();
        if !version_directory.is_dir() {
            continue;
        }
        let component = version_directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("protobuf component was not UTF-8")?;
        if excludes.contains(component) {
            continue;
        }
        collect_proto_methods(&version_directory, &mut methods)?;
    }
    Ok(methods.into_iter().collect())
}

fn collect_proto_methods(
    directory: &Path,
    methods: &mut BTreeSet<String>,
) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_proto_methods(&path, methods)?;
            continue;
        }
        if path
            .extension()
            .is_none_or(|extension| extension != "proto")
        {
            continue;
        }
        let source = fs::read_to_string(path)?;
        let tokens = proto_structure_tokens(&source);
        let mut brace_depth = 0_usize;
        let mut service: Option<(String, usize)> = None;
        let mut index = 0_usize;
        while index < tokens.len() {
            match tokens[index].as_str() {
                "service" if index + 2 < tokens.len() && tokens[index + 2] == "{" => {
                    brace_depth += 1;
                    service = Some((tokens[index + 1].clone(), brace_depth));
                    index += 3;
                }
                "rpc" if service.is_some() && index + 1 < tokens.len() => {
                    let (service_name, _) = service.as_ref().expect("service was checked");
                    methods.insert(format!("{service_name}.{}", tokens[index + 1]));
                    index += 2;
                }
                "{" => {
                    brace_depth += 1;
                    index += 1;
                }
                "}" => {
                    if service
                        .as_ref()
                        .is_some_and(|(_, service_depth)| *service_depth == brace_depth)
                    {
                        service = None;
                    }
                    brace_depth = brace_depth.saturating_sub(1);
                    index += 1;
                }
                _ => index += 1,
            }
        }
    }
    Ok(())
}

fn proto_structure_tokens(source: &str) -> Vec<String> {
    let code = rust_code_without_literals_and_comments(source);
    let mut tokens = Vec::new();
    let mut identifier = String::new();
    for character in code.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            identifier.push(character);
            continue;
        }
        if !identifier.is_empty() {
            tokens.push(std::mem::take(&mut identifier));
        }
        if matches!(character, '{' | '}') {
            tokens.push(character.to_string());
        }
    }
    if !identifier.is_empty() {
        tokens.push(identifier);
    }
    tokens
}

fn configuration_fields(root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let default = fs::read_to_string(root.join("configs/default.toml"))?;
    let mut fields = default
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, _)| name.trim().to_owned())
        .collect::<BTreeSet<_>>();
    let schema: Value = serde_json::from_slice(&fs::read(root.join("configs/schema.json"))?)?;
    collect_schema_fields(&schema, "", &mut fields);
    Ok(fields.into_iter().collect())
}

fn collect_schema_fields(schema: &Value, prefix: &str, fields: &mut BTreeSet<String>) {
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            fields.insert(path.clone());
            collect_schema_fields(property, &path, fields);
        }
    }
    if let Some(items) = schema.get("items") {
        collect_schema_fields(items, prefix, fields);
    }
    for combinator in ["allOf", "anyOf", "oneOf"] {
        if let Some(variants) = schema.get(combinator).and_then(Value::as_array) {
            for variant in variants {
                collect_schema_fields(variant, prefix, fields);
            }
        }
    }
    if let Some(definitions) = schema
        .get("$defs")
        .or_else(|| schema.get("definitions"))
        .and_then(Value::as_object)
    {
        for definition in definitions.values() {
            collect_schema_fields(definition, prefix, fields);
        }
    }
}

fn network_capabilities(
    root: &Path,
    active_sources: &[PathBuf],
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut capabilities = BTreeSet::new();
    let default = fs::read_to_string(root.join("configs/default.toml"))?;
    let bind = default
        .lines()
        .find_map(|line| {
            line.strip_prefix("bind_address")
                .and_then(|value| value.split_once('=').map(|(_, value)| value.trim()))
        })
        .ok_or("default configuration omitted bind_address")?;
    capabilities.insert(format!("configs/default.toml bind_address={bind}"));
    for source in active_sources {
        let production_text = production_rust_source(&fs::read_to_string(source)?);
        capabilities.extend(network_capability_declarations(
            root,
            source,
            &production_text,
        ));
    }
    Ok(capabilities.into_iter().collect())
}

fn network_capability_declarations(
    root: &Path,
    source: &Path,
    production_text: &str,
) -> BTreeSet<String> {
    let relative = source
        .strip_prefix(root)
        .unwrap_or(source)
        .to_string_lossy();
    let mut capabilities = BTreeSet::new();
    let uncommented = rust_source_without_comments(production_text);
    let skeleton = rust_code_without_literals_and_comments(production_text);
    let file_code = skeleton.split_whitespace().collect::<String>();
    let file_identifiers = skeleton
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|identifier| !identifier.is_empty())
        .collect::<Vec<_>>();
    let has_udp_socket = file_code.contains("UdpSocket");
    let tcp_listener_identifier_count = file_identifiers
        .iter()
        .filter(|identifier| **identifier == "TcpListener")
        .count();
    let loopback_bind_identifier_count = file_identifiers
        .iter()
        .filter(|identifier| **identifier == "LOOPBACK_BIND")
        .count();
    let owns_approved_tcp_listener = relative == "crates/local-api/src/server.rs"
        && tcp_listener_identifier_count == 2
        && loopback_bind_identifier_count == 3
        && file_code.matches("TcpListener::bind(").count() == 1
        && file_code.contains(
            "constLOOPBACK_BIND:SocketAddr=SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),0);",
        )
        && file_code.contains("ifbind!=crate::server::LOOPBACK_BIND{")
        && file_code.contains(
            "letlistener=TcpListener::bind(crate::server::LOOPBACK_BIND)",
        );
    let aliases_rustix_crate = file_identifiers
        .windows(3)
        .any(|tokens| tokens == ["use", "rustix", "as"])
        || file_identifiers
            .windows(4)
            .any(|tokens| tokens == ["extern", "crate", "rustix", "as"]);
    if file_code.contains("rustix::net") {
        capabilities.insert(format!("{relative} rustix net module ownership"));
    }
    if aliases_rustix_crate {
        capabilities.insert(format!("{relative} rustix crate alias"));
    }
    if file_code.contains("#[path=")
        || (file_code.contains("#[cfg_attr(") && file_code.contains("path="))
        || file_code.contains("include!(")
    {
        capabilities.insert(format!("{relative} production source indirection"));
    }
    if file_identifiers
        .iter()
        .enumerate()
        .any(|(index, identifier)| {
            *identifier == "extern" && file_identifiers.get(index + 1) != Some(&"crate")
        })
    {
        capabilities.insert(format!("{relative} foreign interface ownership"));
    }
    for (number, (line, literal_line)) in skeleton.lines().zip(uncommented.lines()).enumerate() {
        let code = rust_code_without_literals_and_comments(line)
            .split_whitespace()
            .collect::<String>();
        let identifiers = line
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .filter(|identifier| !identifier.is_empty())
            .collect::<BTreeSet<_>>();
        let location = format!("{relative}:{}", number + 1);
        if code.contains("TcpListener::bind(") {
            if owns_approved_tcp_listener {
                capabilities.insert(
                    "crates/local-api/src/server.rs TcpListener::bind(crate::server::LOOPBACK_BIND)"
                        .to_owned(),
                );
            } else {
                capabilities.insert(format!("{location} TCP listener bind"));
            }
        }
        if identifiers.contains("TcpListener") && !owns_approved_tcp_listener {
            capabilities.insert(format!("{location} TCP listener type"));
        }
        for network_type in [
            "TcpStream",
            "TcpSocket",
            "UdpSocket",
            "UnixStream",
            "UnixDatagram",
            "UnixListener",
        ] {
            if identifiers.contains(network_type) {
                capabilities.insert(format!("{location} network type {network_type}"));
            }
        }
        if identifiers.contains("Command") {
            capabilities.insert(format!("{location} process command ownership"));
        }
        if identifiers
            .iter()
            .any(|identifier| identifier.ends_with("Client") || identifier.ends_with("_client"))
        {
            capabilities.insert(format!("{location} generated client ownership"));
        }
        if identifiers.contains("Endpoint")
            || identifiers.contains("Channel")
            || code.contains("TcpStream")
            || code.contains("::connect(")
            || code.contains(".connect(")
            || code.contains("::connect_timeout(")
            || code.contains("connect_async(")
            || code.contains("connect_with_config(")
            || code.contains("Endpoint::from_shared(")
            || code.contains("Endpoint::from_static(")
            || code.contains("Channel::from_shared(")
            || code.contains("Channel::from_static(")
            || code.contains("Client::builder(")
        {
            capabilities.insert(format!("{location} network client"));
        }
        if code.contains("lookup_host(") || code.contains("ToSocketAddrs") {
            capabilities.insert(format!("{location} DNS resolution"));
        }
        if code.contains("send_to(")
            || code.contains("sendto(")
            || (has_udp_socket && code.contains(".send("))
        {
            capabilities.insert(format!("{location} UDP send"));
        }
        if code.contains("libc::socket(")
            || code.contains("libc::connect(")
            || code.contains("libc::send(")
            || code.contains("libc::sendto(")
            || code.contains("rustix::net::socket(")
            || code.contains("socket2::Socket")
        {
            capabilities.insert(format!("{location} raw socket API"));
        }
        if code.contains("Command::new(") && has_network_tool_literal(literal_line) {
            capabilities.insert(format!("{location} network command"));
        }
        for endpoint in endpoint_literals(literal_line) {
            if endpoint != "http://{local_addr}" {
                capabilities.insert(format!("{location} raw endpoint {endpoint}"));
            }
        }
    }
    capabilities
}

fn has_network_tool_literal(line: &str) -> bool {
    [
        "curl", "wget", "nc", "ncat", "netcat", "socat", "ssh", "openssl",
    ]
    .iter()
    .any(|tool| {
        line.contains(&format!("\"{tool}\""))
            || line.contains(&format!("\"/usr/bin/{tool}\""))
            || line.contains(&format!("\"/bin/{tool}\""))
    })
}

fn endpoint_literals(line: &str) -> Vec<String> {
    line.split(|character: char| {
        character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ')' | ']' | ';')
    })
    .filter(|word| {
        word.starts_with("http://")
            || word.starts_with("https://")
            || word.starts_with("ws://")
            || word.starts_with("wss://")
    })
    .map(str::to_owned)
    .collect()
}

fn execution_declarations_in_source(
    root: &Path,
    source: &Path,
) -> Result<Vec<String>, Box<dyn Error>> {
    let text = fs::read_to_string(source)?;
    let production_text = production_rust_source(&text);
    let declarations_only = rust_code_without_literals_and_comments(&production_text);
    let mut declarations = BTreeSet::new();
    for (number, line) in declarations_only.lines().enumerate() {
        for token in
            line.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        {
            if !token.is_empty() && has_execution_term(token) {
                declarations.insert(format!(
                    "{}:{} {token}",
                    source
                        .strip_prefix(root)
                        .unwrap_or(source)
                        .to_string_lossy(),
                    number + 1
                ));
            }
        }
    }
    Ok(declarations.into_iter().collect())
}

fn production_rust_source(source: &str) -> String {
    let skeleton = rust_code_without_literals_and_comments(source);
    let bytes = skeleton.as_bytes();
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = skeleton[cursor..].find("#[cfg(test)]") {
        let start = cursor + relative;
        let item_start = start + "#[cfg(test)]".len();
        let Some(relative_end) = bytes[item_start..]
            .iter()
            .position(|byte| matches!(*byte, b'{' | b';'))
        else {
            ranges.push(start..bytes.len());
            break;
        };
        let boundary = item_start + relative_end;
        if bytes[boundary] == b';' {
            ranges.push(start..boundary + 1);
            cursor = boundary + 1;
            continue;
        }
        let mut depth = 1_usize;
        let mut end = boundary + 1;
        while end < bytes.len() && depth > 0 {
            match bytes[end] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            end += 1;
        }
        ranges.push(start..end);
        cursor = end;
    }

    let mut production = source.as_bytes().to_vec();
    for range in ranges {
        for byte in &mut production[range] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(production).expect("Rust source was valid UTF-8")
}

fn rust_source_without_comments(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        LineComment,
        BlockComment(usize),
        String(bool),
        RawString(usize),
    }

    let bytes = source.as_bytes();
    let mut output = String::with_capacity(bytes.len());
    let mut state = State::Code;
    let mut index = 0;
    while index < bytes.len() {
        match state {
            State::Code if bytes[index..].starts_with(b"//") => {
                output.push_str("  ");
                index += 2;
                state = State::LineComment;
            }
            State::Code if bytes[index..].starts_with(b"/*") => {
                output.push_str("  ");
                index += 2;
                state = State::BlockComment(1);
            }
            State::Code if bytes[index] == b'"' => {
                output.push('"');
                index += 1;
                state = State::String(false);
            }
            State::Code if bytes[index] == b'r' => {
                let mut cursor = index + 1;
                while cursor < bytes.len() && bytes[cursor] == b'#' {
                    cursor += 1;
                }
                if cursor < bytes.len() && bytes[cursor] == b'"' {
                    let hashes = cursor - index - 1;
                    output.push_str(
                        std::str::from_utf8(&bytes[index..=cursor])
                            .expect("Rust source was valid UTF-8"),
                    );
                    index = cursor + 1;
                    state = State::RawString(hashes);
                } else {
                    output.push('r');
                    index += 1;
                }
            }
            State::Code => {
                output.push(char::from(bytes[index]));
                index += 1;
            }
            State::LineComment if bytes[index] == b'\n' => {
                output.push('\n');
                index += 1;
                state = State::Code;
            }
            State::LineComment => {
                output.push(' ');
                index += 1;
            }
            State::BlockComment(depth) if bytes[index..].starts_with(b"/*") => {
                output.push_str("  ");
                index += 2;
                state = State::BlockComment(depth + 1);
            }
            State::BlockComment(depth) if bytes[index..].starts_with(b"*/") => {
                output.push_str("  ");
                index += 2;
                state = if depth == 1 {
                    State::Code
                } else {
                    State::BlockComment(depth - 1)
                };
            }
            State::BlockComment(depth) => {
                output.push(if bytes[index] == b'\n' { '\n' } else { ' ' });
                index += 1;
                state = State::BlockComment(depth);
            }
            State::String(escaped) if escaped => {
                output.push(char::from(bytes[index]));
                index += 1;
                state = State::String(false);
            }
            State::String(_) if bytes[index] == b'\\' => {
                output.push('\\');
                index += 1;
                state = State::String(true);
            }
            State::String(_) if bytes[index] == b'"' => {
                output.push('"');
                index += 1;
                state = State::Code;
            }
            State::String(_) => {
                output.push(char::from(bytes[index]));
                index += 1;
            }
            State::RawString(hashes) if bytes[index] == b'"' => {
                let terminator_end = index + 1 + hashes;
                if terminator_end <= bytes.len()
                    && bytes[index + 1..terminator_end]
                        .iter()
                        .all(|byte| *byte == b'#')
                {
                    output.push_str(
                        std::str::from_utf8(&bytes[index..terminator_end])
                            .expect("Rust source was valid UTF-8"),
                    );
                    index = terminator_end;
                    state = State::Code;
                } else {
                    output.push('"');
                    index += 1;
                }
            }
            State::RawString(hashes) => {
                output.push(char::from(bytes[index]));
                index += 1;
                state = State::RawString(hashes);
            }
        }
    }
    output
}

fn rust_code_without_literals_and_comments(source: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        LineComment,
        BlockComment(usize),
        String(bool),
        RawString(usize),
    }

    let bytes = source.as_bytes();
    let mut output = String::with_capacity(bytes.len());
    let mut state = State::Code;
    let mut index = 0;
    while index < bytes.len() {
        match state {
            State::Code if bytes[index..].starts_with(b"//") => {
                output.push_str("  ");
                index += 2;
                state = State::LineComment;
            }
            State::Code if bytes[index..].starts_with(b"/*") => {
                output.push_str("  ");
                index += 2;
                state = State::BlockComment(1);
            }
            State::Code if bytes[index] == b'"' => {
                output.push(' ');
                index += 1;
                state = State::String(false);
            }
            State::Code if bytes[index] == b'r' => {
                let mut cursor = index + 1;
                while cursor < bytes.len() && bytes[cursor] == b'#' {
                    cursor += 1;
                }
                if cursor < bytes.len() && bytes[cursor] == b'"' {
                    let hashes = cursor - index - 1;
                    output.extend(std::iter::repeat_n(' ', cursor - index + 1));
                    index = cursor + 1;
                    state = State::RawString(hashes);
                } else {
                    output.push('r');
                    index += 1;
                }
            }
            State::Code => {
                output.push(char::from(bytes[index]));
                index += 1;
            }
            State::LineComment if bytes[index] == b'\n' => {
                output.push('\n');
                index += 1;
                state = State::Code;
            }
            State::LineComment => {
                output.push(' ');
                index += 1;
            }
            State::BlockComment(depth) if bytes[index..].starts_with(b"/*") => {
                output.push_str("  ");
                index += 2;
                state = State::BlockComment(depth + 1);
            }
            State::BlockComment(depth) if bytes[index..].starts_with(b"*/") => {
                output.push_str("  ");
                index += 2;
                state = if depth == 1 {
                    State::Code
                } else {
                    State::BlockComment(depth - 1)
                };
            }
            State::BlockComment(depth) => {
                output.push(if bytes[index] == b'\n' { '\n' } else { ' ' });
                index += 1;
                state = State::BlockComment(depth);
            }
            State::String(escaped) if escaped => {
                output.push(' ');
                index += 1;
                state = State::String(false);
            }
            State::String(_) if bytes[index] == b'\\' => {
                output.push(' ');
                index += 1;
                state = State::String(true);
            }
            State::String(_) if bytes[index] == b'"' => {
                output.push(' ');
                index += 1;
                state = State::Code;
            }
            State::String(_) => {
                output.push(if bytes[index] == b'\n' { '\n' } else { ' ' });
                index += 1;
            }
            State::RawString(hashes) if bytes[index] == b'"' => {
                let terminator_end = index + 1 + hashes;
                if terminator_end <= bytes.len()
                    && bytes[index + 1..terminator_end]
                        .iter()
                        .all(|byte| *byte == b'#')
                {
                    output.extend(std::iter::repeat_n(' ', hashes + 1));
                    index = terminator_end;
                    state = State::Code;
                } else {
                    output.push(' ');
                    index += 1;
                }
            }
            State::RawString(hashes) => {
                output.push(if bytes[index] == b'\n' { '\n' } else { ' ' });
                index += 1;
                state = State::RawString(hashes);
            }
        }
    }
    output
}

fn has_execution_term(value: &str) -> bool {
    let compact = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "placeorder",
        "submitorder",
        "createorder",
        "amendorder",
        "orderamend",
        "replaceorder",
        "orderreplace",
        "executeorder",
        "orderexecute",
        "sendorder",
        "ordersend",
        "cancelorder",
        "openposition",
        "positionopen",
        "closeposition",
        "positionclose",
        "setleverage",
        "updateleverage",
        "changeleverage",
        "withdraw",
        "tradingcredential",
        "exchangecredential",
        "secretcredential",
        "walletseed",
        "seedphrase",
        "mnemonic",
        "walletsecret",
        "walletsigningcredential",
        "walletsigningkey",
        "apikey",
        "apisecret",
        "privatekey",
        "liveexecution",
        "livetrading",
    ]
    .iter()
    .any(|term| compact.contains(term))
}

#[test]
fn proto_parser_tracks_same_line_services_and_nested_braces() {
    let directory = tempfile::tempdir().expect("proto fixture root must exist");
    fs::write(
        directory.path().join("authority.proto"),
        concat!(
            "syntax = \"proto3\";\n",
            "service Trading { rpc PlaceOrder (Request) returns (Reply); }\n",
            "service Observing {\n",
            "  option (scope) = { nested: { value: 1 } };\n",
            "  rpc Snapshot (Request) returns (Reply);\n",
            "}\n",
        ),
    )
    .expect("proto mutation fixture must write");
    let mut methods = BTreeSet::new();

    collect_proto_methods(directory.path(), &mut methods)
        .expect("proto mutation fixture must parse");

    assert_eq!(
        methods,
        BTreeSet::from([
            "Observing.Snapshot".to_owned(),
            "Trading.PlaceOrder".to_owned()
        ])
    );
}

#[test]
fn detector_rejects_equivalent_order_position_leverage_and_wallet_authority() {
    for identifier in [
        "amend_order",
        "order_replace",
        "executeOrder",
        "send_order",
        "open_position",
        "position_close",
        "set_leverage",
        "updateLeverage",
        "wallet_seed",
        "seed_phrase",
        "mnemonic",
        "wallet_secret",
        "wallet_signing_credentials",
        "secret_credentials",
    ] {
        assert!(
            has_execution_term(identifier),
            "execution authority term was not rejected: {identifier}"
        );
    }
}

#[test]
fn detector_rejects_every_execution_authority_class() {
    let inventory = SurfaceInventory {
        cargo_features: vec!["live_trading".into()],
        direct_dependencies: vec!["exchange-order-sdk".into()],
        dependency_capabilities: vec!["rustix@1.1.4:features=default,net,std".into()],
        generated_binding_capabilities: vec!["local-api client generation enabled".into()],
        source_ownership_capabilities: vec![
            "cryptoriskd target remote-helper escapes package/src".into(),
        ],
        protobuf_methods: vec!["TradingService.PlaceOrder".into()],
        cli_options: vec!["--api-key".into()],
        configuration_fields: vec!["withdrawal_address".into()],
        network_capabilities: vec!["network.rs:8 raw endpoint https://api.exchange.invalid".into()],
        runtime_declarations: vec!["src/runtime.rs:8 place_order".into()],
    };

    let violations = inventory.execution_authority_violations();

    assert_eq!(
        violations,
        [
            "cargo feature: live_trading",
            "dependency outside the foundation allowlist: exchange-order-sdk",
            "dependency capability outside ownership policy: rustix@1.1.4:features=default,net,std",
            "generated binding capability outside ownership policy: local-api client generation enabled",
            "source ownership outside policy: cryptoriskd target remote-helper escapes package/src",
            "protobuf method: TradingService.PlaceOrder",
            "CLI option: --api-key",
            "configuration field: withdrawal_address",
            "network capability outside ownership policy: network.rs:8 raw endpoint https://api.exchange.invalid",
            "runtime declaration: src/runtime.rs:8 place_order",
        ]
    );
}

#[test]
fn resolved_rustix_network_feature_is_an_unapproved_capability() {
    let metadata = serde_json::json!({
        "packages": [
            {"id": "cryptoriskd 0.1.0", "name": "cryptoriskd", "version": "0.1.0"},
            {"id": "rustix 1.1.4", "name": "rustix", "version": "1.1.4"},
        ],
        "resolve": {
            "nodes": [
                {
                    "id": "cryptoriskd 0.1.0",
                    "dependencies": ["rustix 1.1.4"],
                    "features": [],
                },
                {
                    "id": "rustix 1.1.4",
                    "dependencies": [],
                    "features": ["std", "net", "default", "alloc"],
                },
            ]
        }
    });
    let production = BTreeSet::from(["cryptoriskd 0.1.0"]);
    let capabilities = resolved_dependency_capabilities(&metadata, &production)
        .expect("resolved dependency feature mutation must be inspectable");

    assert_eq!(
        capabilities,
        ["rustix@1.1.4:features=alloc,default,net,std"]
    );
    assert!(!APPROVED_FOUNDATION_DEPENDENCY_CAPABILITIES.contains(&capabilities[0].as_str()));
}

#[test]
fn resolved_dependency_graph_expands_every_workspace_feature() {
    assert_eq!(
        cargo_metadata_args(),
        [
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--all-features",
        ],
        "dependency capability inspection must match the all-features gate"
    );
}

#[test]
fn source_declaration_parser_rejects_execution_identifiers_from_a_real_fixture() {
    let directory = tempfile::tempdir().expect("source fixture root must exist");
    let source = directory.path().join("execution.rs");
    fs::write(
        &source,
        concat!(
            "// fn submit_order() {}\n",
            "const DENIED: &str = \"api_secret\";\n",
            "#[cfg(test)]\n",
            "mod tests { pub fn create_order() {} }\n",
            "pub fn place_order() {}\n",
            "pub struct WithdrawalCredential;\n",
        ),
    )
    .expect("source mutation fixture must write");

    let declarations = execution_declarations_in_source(directory.path(), &source)
        .expect("source mutation fixture must parse");

    assert_eq!(
        declarations,
        [
            "execution.rs:5 place_order",
            "execution.rs:6 WithdrawalCredential"
        ]
    );
}

#[test]
fn nested_configuration_and_constructed_network_capabilities_are_detected() {
    let schema = serde_json::json!({
        "properties": {
            "local": {
                "type": "object",
                "properties": {
                    "trading_credentials": {"type": "string"}
                }
            }
        }
    });
    let mut fields = BTreeSet::new();
    collect_schema_fields(&schema, "", &mut fields);
    assert!(fields.contains("local.trading_credentials"));

    let directory = tempfile::tempdir().expect("outbound fixture root must exist");
    let source = directory.path().join("network.rs");
    let text = concat!(
        "#[cfg(test)]\n",
        "mod tests { fn local_only() { endpoint.connect(); } }\n",
        "pub async fn observe(remote: Address) {\n",
        "    socket.connect(remote).await;\n",
        "}\n",
    );
    fs::write(&source, text).expect("outbound fixture must write");
    let production = production_rust_source(text);

    assert_eq!(
        network_capability_declarations(directory.path(), &source, &production),
        BTreeSet::from(["network.rs:4 network client".to_owned()])
    );
}

#[test]
fn every_network_capability_family_is_detected_by_mutation() {
    let fixtures = [
        (
            "udp-send-to",
            "pub async fn mutate(socket: tokio::net::UdpSocket, remote: std::net::SocketAddr) { socket.send_to(b\"x\", remote).await; }\n",
        ),
        (
            "udp-send",
            "pub async fn mutate(socket: tokio::net::UdpSocket) { socket.send(b\"x\").await; }\n",
        ),
        (
            "dns-lookup",
            "pub async fn mutate() { tokio::net::lookup_host(\"exchange.invalid:443\").await; }\n",
        ),
        (
            "connect-timeout",
            "pub fn mutate(address: &std::net::SocketAddr) { std::net::TcpStream::connect_timeout(address, std::time::Duration::from_secs(1)); }\n",
        ),
        (
            "std-network-type-alias",
            concat!(
                "use std::net::TcpStream as Stream;\n",
                "pub fn mutate(address: &std::net::SocketAddr) { Stream::connect_timeout(address, std::time::Duration::from_secs(1)); }\n",
            ),
        ),
        (
            "tokio-network-import-aliases",
            concat!(
                "use tokio::net::{UdpSocket as Datagram, lookup_host as resolve};\n",
                "pub async fn mutate(socket: Datagram) { let _ = resolve(\"exchange.invalid:443\").await; let _ = socket.send(b\"x\").await; }\n",
            ),
        ),
        (
            "socket-syscall",
            "pub unsafe fn mutate() { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0); }\n",
        ),
        (
            "network-command",
            "pub fn mutate() { std::process::Command::new(\"curl\").arg(\"https://exchange.invalid\"); }\n",
        ),
        (
            "network-command-type-alias",
            concat!(
                "use std::process::Command as Runner;\n",
                "pub fn mutate(program: &str) { Runner::new(program); }\n",
            ),
        ),
        (
            "raw-endpoint",
            "pub const REMOTE: &str = \"wss://exchange.invalid/stream\";\n",
        ),
        (
            "unsafe-network-ffi",
            "unsafe extern \"C\" { fn connect(fd: i32, address: *const u8, length: u32) -> i32; }\n",
        ),
        (
            "unsafe-network-ffi-link-name",
            concat!(
                "unsafe extern \"C\" {\n",
                "    #[link_name = \"connect\"]\n",
                "    fn dial(fd: i32, address: *const u8, length: u32) -> i32;\n",
                "}\n",
            ),
        ),
        (
            "extern-crate-plus-unsafe-network-ffi",
            concat!(
                "extern crate rustix;\n",
                "unsafe extern \"C\" {\n",
                "    #[link_name = \"connect\"]\n",
                "    fn dial(fd: i32, address: *const u8, length: u32) -> i32;\n",
                "}\n",
            ),
        ),
        (
            "std-unix-socket-aliases",
            concat!(
                "use std::os::unix::net::{UnixStream as Stream, UnixDatagram as Datagram};\n",
                "pub fn mutate(stream: Stream, datagram: Datagram) { let _ = (stream, datagram); }\n",
            ),
        ),
        (
            "tokio-unix-socket-alias",
            concat!(
                "use tokio::net::UnixStream as Stream;\n",
                "pub fn mutate(stream: Stream) { let _ = stream; }\n",
            ),
        ),
        (
            "rustix-direct-import",
            concat!(
                "use rustix::net::{socket_with, connect, send};\n",
                "pub fn mutate() { let _ = (socket_with, connect, send); }\n",
            ),
        ),
        (
            "rustix-function-aliases",
            concat!(
                "use rustix::net::{socket_with as open_socket, connect as dial, send as transmit};\n",
                "pub fn mutate() { let _ = (open_socket, dial, transmit); }\n",
            ),
        ),
        (
            "rustix-crate-alias",
            concat!(
                "use rustix as system;\n",
                "use system::net::{socket_with as open_socket, connect as dial, send as transmit};\n",
                "pub fn mutate() { let _ = (open_socket, dial, transmit); }\n",
            ),
        ),
        (
            "rustix-extern-crate-alias",
            concat!(
                "extern crate rustix as system;\n",
                "use system::net::{socket_with as open_socket, connect as dial, send as transmit};\n",
                "pub fn mutate() { let _ = (open_socket, dial, transmit); }\n",
            ),
        ),
        (
            "tonic-connect-lazy",
            "pub fn mutate(endpoint: tonic::transport::Endpoint) { let _ = endpoint.connect_lazy(); }\n",
        ),
        (
            "tonic-connect-with-connector-lazy",
            "pub fn mutate(endpoint: tonic::transport::Endpoint) { let _ = endpoint.connect_with_connector_lazy(Connector); }\n",
        ),
        (
            "tonic-balance-list",
            "pub fn mutate() { let _ = tonic::transport::Channel::balance_list(Vec::new().into_iter()); }\n",
        ),
        (
            "tonic-client-type-aliases",
            concat!(
                "use tonic::transport::{Endpoint as Remote, Channel as Pool};\n",
                "pub fn mutate(endpoint: Remote) { let _ = endpoint.connect_lazy(); let _: Option<Pool> = None; }\n",
            ),
        ),
        (
            "generated-tonic-client-alias",
            concat!(
                "use local_api::proto::market_v1::market_service_client::MarketServiceClient as RemoteClient;\n",
                "pub async fn mutate(uri: String) { let _ = RemoteClient::connect::<String>(uri).await; }\n",
            ),
        ),
    ];
    assert_eq!(fixtures.len(), 24, "network mutation inventory drifted");
    let directory = tempfile::tempdir().expect("network mutation root must exist");
    let mut missed = Vec::new();
    for (name, text) in fixtures {
        let source = directory.path().join(format!("{name}.rs"));
        fs::write(&source, text).expect("network mutation must write");
        let production = production_rust_source(text);
        if network_capability_declarations(directory.path(), &source, &production).is_empty() {
            missed.push(name);
        }
    }
    assert_eq!(
        missed,
        Vec::<&str>::new(),
        "network capability families escaped detection"
    );
}

#[test]
fn approved_listener_owner_rejects_a_second_aliased_listener() {
    let directory = tempfile::tempdir().expect("listener ownership root must exist");
    let source = directory.path().join("crates/local-api/src/server.rs");
    fs::create_dir_all(source.parent().expect("listener source must have parent"))
        .expect("listener ownership directory must write");
    let text = concat!(
        "use std::net::SocketAddr;\n",
        "use tokio::net::TcpListener;\n",
        "use tokio::net::TcpListener as AdditionalListener;\n",
        "const LOOPBACK_BIND: SocketAddr =\n",
        "    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);\n",
        "pub async fn mutate(bind: SocketAddr, remote: SocketAddr) {\n",
        "    if bind != crate::server::LOOPBACK_BIND { return; }\n",
        "    let listener = TcpListener::bind(crate::server::LOOPBACK_BIND).await;\n",
        "    let extra = AdditionalListener::bind(remote).await;\n",
        "    let _ = (listener, extra);\n",
        "}\n",
    );
    fs::write(&source, text).expect("listener ownership mutation must write");
    let capabilities =
        network_capability_declarations(directory.path(), &source, &production_rust_source(text));

    assert!(
        capabilities.iter().any(|capability| {
            capability
                != "crates/local-api/src/server.rs TcpListener::bind(crate::server::LOOPBACK_BIND)"
        }),
        "a second aliased listener escaped the approved owner"
    );
}

#[test]
fn approved_listener_owner_rejects_post_guard_bind_shadowing() {
    let directory = tempfile::tempdir().expect("listener shadow root must exist");
    let source = directory.path().join("crates/local-api/src/server.rs");
    fs::create_dir_all(source.parent().expect("listener source must have parent"))
        .expect("listener shadow directory must write");
    let text = concat!(
        "use std::net::SocketAddr;\n",
        "use tokio::net::TcpListener;\n",
        "const LOOPBACK_BIND: SocketAddr =\n",
        "    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);\n",
        "pub async fn mutate(bind: SocketAddr) {\n",
        "    if bind != LOOPBACK_BIND { return; }\n",
        "    let bind = if std::env::var_os(\"REMOTE_BIND\").is_some() {\n",
        "        \"0.0.0.0:9000\".parse().unwrap()\n",
        "    } else { bind };\n",
        "    let listener = TcpListener::bind(bind).await;\n",
        "    let _ = listener;\n",
        "}\n",
    );
    fs::write(&source, text).expect("listener shadow mutation must write");
    let capabilities =
        network_capability_declarations(directory.path(), &source, &production_rust_source(text));

    assert!(
        capabilities.iter().any(|capability| {
            capability
                != "crates/local-api/src/server.rs TcpListener::bind(crate::server::LOOPBACK_BIND)"
        }),
        "post-guard bind shadowing escaped the approved owner"
    );
}

#[test]
fn production_source_indirection_is_not_outside_capability_ownership() {
    let directory = tempfile::tempdir().expect("source indirection root must exist");
    let source = directory.path().join("crates/local-api/src/lib.rs");
    fs::create_dir_all(source.parent().expect("source must have parent"))
        .expect("source indirection directory must write");
    let text = "#[path = \"../network_impl.rs\"] mod network_impl;\n";
    fs::write(&source, text).expect("source indirection mutation must write");
    fs::write(
        directory.path().join("crates/local-api/network_impl.rs"),
        "pub async fn connect(endpoint: tonic::transport::Endpoint) { let _ = endpoint.connect_lazy(); }\n",
    )
    .expect("escaped network module must write");
    let capabilities =
        network_capability_declarations(directory.path(), &source, &production_rust_source(text));

    assert!(
        !capabilities.is_empty(),
        "out-of-src production module escaped capability ownership"
    );
}

#[test]
fn production_generated_grpc_clients_are_disabled() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("system-tests must be nested under the workspace");
    let build_script = fs::read_to_string(root.join("crates/local-api/build.rs"))
        .expect("local-api build script must be readable");
    let code = rust_code_without_literals_and_comments(&build_script)
        .split_whitespace()
        .collect::<String>();

    assert!(
        code.matches("tonic_prost_build::configure()").count() == 1
            && code.matches(".compile_with_config(").count() == 1
            && code.matches(".build_client(").count() == 1
            && code.matches(".build_client(false)").count() == 1
            && code.matches(".build_client(true)").count() == 0
            && code.matches(".build_server(").count() == 1
            && code.matches(".build_server(true)").count() == 1,
        "cryptoriskd production bindings must not generate gRPC clients"
    );
}

#[test]
fn production_custom_build_output_policy_rejects_appended_client_generation() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("system-tests must be nested under the workspace");
    let directory = tempfile::tempdir().expect("custom-build mutation root must exist");
    let build_script = directory.path().join("crates/local-api/build.rs");
    fs::create_dir_all(
        build_script
            .parent()
            .expect("build script must have parent"),
    )
    .expect("custom-build mutation directory must write");
    let generated_policy = directory
        .path()
        .join("build-support/generated_binding_policy.rs");
    fs::create_dir_all(
        generated_policy
            .parent()
            .expect("generated policy must have parent"),
    )
    .expect("generated policy mutation directory must write");
    fs::copy(
        workspace.join("build-support/generated_binding_policy.rs"),
        &generated_policy,
    )
    .expect("approved generated policy must copy");
    let mut mutated = fs::read_to_string(workspace.join("crates/local-api/build.rs"))
        .expect("approved local-api build script must read");
    mutated.push_str(
        "\nfn append_client_surface(output: &std::path::Path) {\n\
         std::fs::write(output.join(\"cmti.market.v1.rs\"), \"pub mod market_service_client {}\").unwrap();\n\
         }\n",
    );
    fs::write(&build_script, mutated).expect("custom-build mutation must write");

    let capabilities =
        generated_binding_capabilities(directory.path(), std::slice::from_ref(&build_script))
            .expect("custom-build mutation must be inspectable");
    assert!(
        !capabilities.is_empty(),
        "post-generation client output mutation escaped ownership"
    );
}

fn copy_tracked_repository(source: &Path, destination: &Path) {
    let listed = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(source)
        .output()
        .expect("tracked repository inventory must run");
    assert!(
        listed.status.success(),
        "tracked repository inventory failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    for relative in listed.stdout.split(|byte| *byte == 0) {
        if relative.is_empty() {
            continue;
        }
        let relative = Path::new(
            std::str::from_utf8(relative).expect("tracked repository path must be UTF-8"),
        );
        let from = source.join(relative);
        let to = destination.join(relative);
        fs::create_dir_all(to.parent().expect("tracked path must have a parent"))
            .expect("tracked destination directory must write");
        fs::copy(&from, &to).unwrap_or_else(|error| {
            panic!(
                "failed to copy tracked path {} to mutation root: {error}",
                relative.display()
            )
        });
    }
}

#[test]
fn cargo_metadata_target_outside_package_src_cannot_escape_source_ownership() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("system-tests must be nested under the workspace");
    let directory = tempfile::tempdir().expect("Cargo target mutation root must exist");
    copy_tracked_repository(workspace, directory.path());

    let manifest_path = directory.path().join("apps/cryptoriskd/Cargo.toml");
    let mut manifest = fs::read_to_string(&manifest_path).expect("cryptoriskd manifest must read");
    manifest = manifest.replacen(
        "publish.workspace = true",
        "publish.workspace = true\ndefault-run = \"cryptoriskd\"",
        1,
    );
    manifest.push_str(
        "\n[[bin]]\n\
         name = \"remote-helper\"\n\
         path = \"../../remote-helper.rs\"\n\
         \n[lib]\n\
         name = \"remote_library\"\n\
         path = \"../../remote-library.rs\"\n",
    );
    fs::write(&manifest_path, manifest).expect("mutated cryptoriskd manifest must write");
    fs::write(
        directory.path().join("remote-helper.rs"),
        concat!(
            "fn main() {\n",
            "    if false {\n",
            "        let _ = std::process::Command::new(\"curl\")\n",
            "            .arg(\"https://exchange.invalid/place-order\")\n",
            "            .status();\n",
            "    }\n",
            "}\n",
        ),
    )
    .expect("escaped Cargo target source must write");
    fs::write(
        directory.path().join("remote-library.rs"),
        "pub const REMOTE: &str = \"https://exchange.invalid/place-order\";\n",
    )
    .expect("escaped Cargo library source must write");

    let inventory = SurfaceInventory::discover(directory.path())
        .expect("escaped Cargo target mutation must remain inspectable");
    let violations = inventory.execution_authority_violations();
    for escaped in ["remote-helper.rs", "remote-library.rs"] {
        assert!(
            violations.iter().any(|violation| {
                violation.contains("source ownership") && violation.contains(escaped)
            }),
            "metadata-declared production target {escaped} outside package/src escaped capability ownership: {violations:#?}"
        );
    }
}

#[test]
fn generated_client_policy_rejects_multiline_module_declarations() {
    let generated = concat!(
        "pub mod market_service_client\n",
        "{\n",
        "    pub struct MarketServiceClient<T>(T);\n",
        "}\n",
    );

    assert_eq!(
        generated_binding_policy::grpc_client_module(generated),
        Some("market_service_client"),
        "valid multiline Rust module declaration escaped generated-client policy"
    );
}

#[test]
fn generated_client_policy_skips_comments_between_module_tokens() {
    for generated in [
        "pub /* outer /* nested */ comment */ mod market_service_client {}\n",
        "pub // generated visibility\nmod market_service_client {}\n",
        "pub mod market_service_client /* generated body */ {}\n",
        "pub mod r#market_service_client {}\n",
    ] {
        assert_eq!(
            generated_binding_policy::grpc_client_module(generated),
            Some("market_service_client"),
            "valid commented or raw-identifier module escaped generated-client policy"
        );
    }
}

#[test]
fn ordinary_cargo_targets_must_remain_under_package_src() {
    let directory = tempfile::tempdir().expect("ordinary target mutation root must exist");
    let package_root = directory.path().join("package");
    fs::create_dir_all(package_root.join("src")).expect("ordinary target package src must write");

    for (name, kind, relative) in [
        ("escaped-bin", "bin", "../escaped-bin.rs"),
        ("escaped-lib", "lib", "escaped-lib.rs"),
        ("normalized-bin", "bin", "src/../normalized-bin.rs"),
    ] {
        let source = package_root.join(relative);
        fs::write(&source, "pub fn mutation() {}\n")
            .expect("ordinary escaped target source must write");
        let target = serde_json::json!({
            "name": name,
            "kind": [kind],
            "src_path": source,
        });
        let mut active_sources = Vec::new();
        let mut build_scripts = Vec::new();
        let mut violations = Vec::new();

        collect_production_target_sources(
            "fixture-package",
            &package_root,
            std::slice::from_ref(&target),
            &mut active_sources,
            &mut build_scripts,
            &mut violations,
        )
        .expect("ordinary Cargo target mutation must be inspectable");

        assert!(
            violations.iter().any(|violation| violation.contains(name)),
            "{kind} target `{name}` escaped package/src ownership: {violations:#?}"
        );
    }
}

#[test]
fn example_and_bench_targets_are_scanned_but_cannot_escape_their_roots() {
    let directory = tempfile::tempdir().expect("all-target mutation root must exist");
    let package_root = directory.path().join("package");
    fs::create_dir_all(package_root.join("src")).expect("all-target package src must write");
    fs::create_dir_all(package_root.join("examples")).expect("example root must write");
    fs::create_dir_all(package_root.join("benches")).expect("bench root must write");
    let example = package_root.join("examples/network.rs");
    let bench = package_root.join("benches/network.rs");
    fs::write(&example, "fn main() {}\n").expect("example source must write");
    fs::write(&bench, "fn main() {}\n").expect("bench source must write");
    let targets = [
        serde_json::json!({
            "name": "network-example",
            "kind": ["example"],
            "src_path": example,
        }),
        serde_json::json!({
            "name": "network-bench",
            "kind": ["bench"],
            "src_path": bench,
        }),
    ];
    let mut active_sources = Vec::new();
    let mut build_scripts = Vec::new();
    let mut violations = Vec::new();

    collect_production_target_sources(
        "fixture-package",
        &package_root,
        &targets,
        &mut active_sources,
        &mut build_scripts,
        &mut violations,
    )
    .expect("example and bench targets must be inspectable");

    assert!(violations.is_empty(), "owned all-target sources rejected");
    assert_eq!(
        active_sources.len(),
        2,
        "all-target entrypoints were not scanned"
    );

    let escaped = package_root.join("escaped-example.rs");
    fs::write(&escaped, "fn main() {}\n").expect("escaped example source must write");
    let escaped_target = serde_json::json!({
        "name": "escaped-example",
        "kind": ["example"],
        "src_path": escaped,
    });
    violations.clear();
    collect_production_target_sources(
        "fixture-package",
        &package_root,
        std::slice::from_ref(&escaped_target),
        &mut active_sources,
        &mut build_scripts,
        &mut violations,
    )
    .expect("escaped example target must be inspectable");
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("escaped-example")),
        "example target outside package/examples escaped ownership"
    );
}

#[cfg(unix)]
#[test]
fn cargo_target_symlink_cannot_escape_canonical_source_ownership() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("symlink target mutation root must exist");
    let package_root = directory.path().join("package");
    fs::create_dir_all(package_root.join("src")).expect("symlink package src must write");
    let escaped = directory.path().join("escaped.rs");
    fs::write(&escaped, "pub fn mutation() {}\n").expect("escaped symlink source must write");
    let linked = package_root.join("src/linked.rs");
    symlink(&escaped, &linked).expect("escaped target symlink must write");
    let target = serde_json::json!({
        "name": "symlink-bin",
        "kind": ["bin"],
        "src_path": linked,
    });
    let mut active_sources = Vec::new();
    let mut build_scripts = Vec::new();
    let mut violations = Vec::new();

    collect_production_target_sources(
        "fixture-package",
        &package_root,
        std::slice::from_ref(&target),
        &mut active_sources,
        &mut build_scripts,
        &mut violations,
    )
    .expect("symlink Cargo target must be inspectable");

    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("symlink-bin")),
        "canonical symlink escape bypassed package/src ownership"
    );
}

#[test]
fn active_runtime_exposes_market_observation_but_no_execution_authority() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("system-tests must remain under the workspace crates directory");
    let inventory =
        SurfaceInventory::discover(root).expect("active runtime surface must be inspectable");

    assert_eq!(
        inventory.protobuf_methods,
        ["HealthService.Check", "MarketService.GetSnapshot"]
    );
    assert_eq!(
        inventory.cli_options,
        ["--approved-root", "--config", "--help"]
    );
    assert_eq!(
        inventory.dependency_capabilities,
        ["rustix@1.1.4:features=alloc,default,fs,process,std"]
    );
    assert!(
        inventory.generated_binding_capabilities.is_empty(),
        "production generated bindings escaped ownership: {:#?}",
        inventory.generated_binding_capabilities
    );
    assert!(
        inventory.execution_authority_violations().is_empty(),
        "active runtime gained execution authority: {:#?}",
        inventory.execution_authority_violations()
    );
}
