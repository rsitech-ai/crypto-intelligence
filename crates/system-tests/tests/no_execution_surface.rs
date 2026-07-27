use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

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

#[derive(Debug, Eq, PartialEq)]
struct SurfaceInventory {
    cargo_features: Vec<String>,
    direct_dependencies: Vec<String>,
    protobuf_methods: Vec<String>,
    cli_options: Vec<String>,
    configuration_fields: Vec<String>,
    outbound_destinations: Vec<String>,
    runtime_declarations: Vec<String>,
}

impl SurfaceInventory {
    fn discover(root: &Path) -> Result<Self, Box<dyn Error>> {
        let metadata_output = Command::new("cargo")
            .args(["metadata", "--locked", "--format-version", "1", "--no-deps"])
            .current_dir(root)
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
        let mut cargo_features = BTreeSet::new();
        let mut direct_dependencies = BTreeSet::new();
        let mut active_sources = Vec::new();
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
            let manifest = PathBuf::from(
                package["manifest_path"]
                    .as_str()
                    .ok_or("package omitted manifest_path")?,
            );
            let package_root = manifest.parent().ok_or("manifest omitted parent")?;
            collect_rust_sources(&package_root.join("src"), &mut active_sources)?;
        }

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
            .current_dir(root)
            .output()?;
        if !help_output.status.success() {
            return Err(format!(
                "cryptoriskd --help failed: {}",
                String::from_utf8_lossy(&help_output.stderr)
            )
            .into());
        }
        let cli_options = long_options(&String::from_utf8(help_output.stdout)?);
        let protobuf_methods = active_protobuf_methods(root)?;
        let configuration_fields = configuration_fields(root)?;
        let outbound_destinations = outbound_destinations(root, &active_sources)?;
        let runtime_declarations = active_sources
            .iter()
            .map(|source| execution_declarations_in_source(root, source))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();

        Ok(Self {
            cargo_features: cargo_features.into_iter().collect(),
            direct_dependencies: direct_dependencies.into_iter().collect(),
            protobuf_methods,
            cli_options,
            configuration_fields,
            outbound_destinations,
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
        for destination in &self.outbound_destinations {
            violations.push(format!("outbound destination: {destination}"));
        }
        for declaration in &self.runtime_declarations {
            violations.push(format!("runtime declaration: {declaration}"));
        }
        violations
    }
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

fn outbound_destinations(
    root: &Path,
    active_sources: &[PathBuf],
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut destinations = BTreeSet::new();
    let default = fs::read_to_string(root.join("configs/default.toml"))?;
    let bind = default
        .lines()
        .find_map(|line| {
            line.strip_prefix("bind_address")
                .and_then(|value| value.split_once('=').map(|(_, value)| value.trim()))
        })
        .ok_or("default configuration omitted bind_address")?;
    if bind != "\"127.0.0.1:0\"" {
        destinations.insert(format!("configured bind {bind}"));
    }
    for source in active_sources {
        let production_text = production_rust_source(&fs::read_to_string(source)?);
        for word in production_text.split(|character: char| {
            character.is_whitespace() || matches!(character, '"' | '\'' | ',' | ')' | ']')
        }) {
            if (word.starts_with("http://")
                || word.starts_with("https://")
                || word.starts_with("ws://")
                || word.starts_with("wss://"))
                && word != "http://{local_addr}"
            {
                destinations.insert(word.to_owned());
            }
        }
        destinations.extend(outbound_connection_declarations(
            root,
            source,
            &production_text,
        ));
    }
    Ok(destinations.into_iter().collect())
}

fn outbound_connection_declarations(
    root: &Path,
    source: &Path,
    production_text: &str,
) -> Vec<String> {
    let mut destinations = Vec::new();
    for (number, line) in production_text.lines().enumerate() {
        let code = rust_code_without_literals_and_comments(line)
            .split_whitespace()
            .collect::<String>();
        if has_outbound_connection_call(&code) {
            destinations.push(format!(
                "{}:{} outbound client declaration",
                source
                    .strip_prefix(root)
                    .unwrap_or(source)
                    .to_string_lossy(),
                number + 1
            ));
        }
    }
    destinations
}

fn has_outbound_connection_call(code: &str) -> bool {
    code.contains(".connect(")
        || code.contains("::connect(")
        || code.contains("connect_async(")
        || code.contains("connect_with_config(")
        || code.contains("Endpoint::from_shared(")
        || code.contains("Endpoint::from_static(")
        || code.contains("Channel::from_shared(")
        || code.contains("Channel::from_static(")
        || code.contains("Client::builder(")
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
        protobuf_methods: vec!["TradingService.PlaceOrder".into()],
        cli_options: vec!["--api-key".into()],
        configuration_fields: vec!["withdrawal_address".into()],
        outbound_destinations: vec!["https://api.exchange.invalid".into()],
        runtime_declarations: vec!["src/runtime.rs:8 place_order".into()],
    };

    let violations = inventory.execution_authority_violations();

    assert_eq!(
        violations,
        [
            "cargo feature: live_trading",
            "dependency outside the foundation allowlist: exchange-order-sdk",
            "protobuf method: TradingService.PlaceOrder",
            "CLI option: --api-key",
            "configuration field: withdrawal_address",
            "outbound destination: https://api.exchange.invalid",
            "runtime declaration: src/runtime.rs:8 place_order",
        ]
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
fn nested_configuration_and_constructed_outbound_destinations_are_detected() {
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
        outbound_connection_declarations(directory.path(), &source, &production),
        ["network.rs:4 outbound client declaration"]
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
    assert!(
        inventory.execution_authority_violations().is_empty(),
        "active runtime gained execution authority: {:#?}",
        inventory.execution_authority_violations()
    );
}
