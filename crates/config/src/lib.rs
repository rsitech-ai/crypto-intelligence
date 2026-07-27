//! Deterministic layered configuration for the local-only runtime.

pub mod schema;

use schemars::{
    JsonSchema,
    schema::{InstanceType, Schema, SchemaObject, StringValidation},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{
    fs::{self, File, OpenOptions},
    io,
    net::{SocketAddr, SocketAddrV4},
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

pub const MAX_INGESTION_QUEUE_CAPACITY: usize = 65_536;
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CONCURRENT_REQUESTS: usize = 1_024;
pub const MAX_REQUEST_TIMEOUT_SECONDS: u64 = 300;
pub const MAX_SHUTDOWN_GRACE_SECONDS: u64 = 30;

#[derive(Clone, Debug, Eq, PartialEq, JsonSchema)]
pub struct KeychainReference {
    service: String,
    account: String,
}

impl KeychainReference {
    pub fn parse(value: &str) -> Result<Self, ConfigError> {
        let body = value
            .strip_prefix("keychain://")
            .ok_or(ConfigError::CredentialReference)?;
        let (service, account) = body
            .split_once('/')
            .ok_or(ConfigError::CredentialReference)?;
        if account.contains('/') || ![service, account].into_iter().all(valid_keychain_component) {
            return Err(ConfigError::CredentialReference);
        }
        Ok(Self {
            service: service.to_owned(),
            account: account.to_owned(),
        })
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    pub fn account(&self) -> &str {
        &self.account
    }
}

impl Serialize for KeychainReference {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("keychain://{}/{}", self.service, self.account))
    }
}

impl<'de> Deserialize<'de> for KeychainReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    pub data_root: PathBuf,
    pub log_root: PathBuf,
    pub fixture_input: PathBuf,
    #[schemars(schema_with = "loopback_bind_schema")]
    pub bind_address: String,
    #[schemars(range(min = 3))]
    pub session_secret_fd: u32,
    #[schemars(range(min = 1, max = 65536))]
    pub ingestion_queue_capacity: usize,
    #[schemars(range(min = 1, max = 16777216))]
    pub maximum_request_bytes: usize,
    #[schemars(range(min = 1, max = 1024))]
    pub maximum_concurrent_requests: usize,
    #[schemars(range(min = 1, max = 300))]
    pub request_timeout_seconds: u64,
    #[schemars(range(min = 1, max = 30))]
    pub shutdown_grace_seconds: u64,
    #[schemars(schema_with = "local_only_boolean_schema")]
    pub remote_export: bool,
    #[schemars(schema_with = "local_only_boolean_schema")]
    pub remote_telemetry: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_keychain_reference_schema")]
    pub credential_ref: Option<KeychainReference>,
}

#[derive(Clone, Debug, Default)]
pub struct Overrides {
    pub data_root: Option<PathBuf>,
    pub log_root: Option<PathBuf>,
    pub fixture_input: Option<PathBuf>,
    pub bind_address: Option<String>,
    pub session_secret_fd: Option<u32>,
    pub ingestion_queue_capacity: Option<usize>,
    pub request_timeout_seconds: Option<u64>,
}

impl Overrides {
    fn apply(self, config: &mut AppConfig) -> bool {
        let mut changed = false;
        apply_override(&mut config.data_root, self.data_root, &mut changed);
        apply_override(&mut config.log_root, self.log_root, &mut changed);
        apply_override(&mut config.fixture_input, self.fixture_input, &mut changed);
        apply_override(&mut config.bind_address, self.bind_address, &mut changed);
        apply_override(
            &mut config.session_secret_fd,
            self.session_secret_fd,
            &mut changed,
        );
        apply_override(
            &mut config.ingestion_queue_capacity,
            self.ingestion_queue_capacity,
            &mut changed,
        );
        apply_override(
            &mut config.request_timeout_seconds,
            self.request_timeout_seconds,
            &mut changed,
        );
        changed
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPaths {
    pub approved_root: PathBuf,
    pub data_root: PathBuf,
    pub log_root: PathBuf,
    pub fixture_input: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveConfig {
    pub config: AppConfig,
    pub fingerprint: String,
    pub sources: Vec<String>,
    pub paths: ResolvedPaths,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not parse configuration: {message}")]
    Parse { message: String },
    #[error("configuration embeds secret material at {path}")]
    EmbeddedSecret { path: String },
    #[error("credential references must use keychain://service/account")]
    CredentialReference,
    #[error("unsupported configuration schema version {0}")]
    UnsupportedSchema(u32),
    #[error("bind address must be ephemeral IPv4 loopback")]
    NonLoopbackBind,
    #[error("{field} must be in {minimum}..={maximum}, got {actual}")]
    OutOfRange {
        field: &'static str,
        minimum: u64,
        maximum: u64,
        actual: u64,
    },
    #[error("remote export and telemetry must remain disabled")]
    RemoteCapabilityEnabled,
    #[error("{field} escapes the approved local root")]
    PathEscape { field: &'static str },
    #[error("{field} is not a readable regular file: {path}")]
    FixtureUnreadable { field: &'static str, path: PathBuf },
    #[error("filesystem validation failed for {field} at {path}: {source}")]
    Filesystem {
        field: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not serialize canonical configuration: {0}")]
    CanonicalSerialization(#[from] serde_json::Error),
}

/// Loads defaults, then an optional user layer, then typed overrides.
///
/// Secrets are rejected before typed deserialization. The returned fingerprint
/// covers only the canonical effective configuration, not source labels or
/// absolute path resolution, so identical inputs remain reproducible across
/// machines.
pub fn load(
    default_text: &str,
    default_source: &str,
    user: Option<(&str, &str)>,
    overrides: Overrides,
    approved_root: &Path,
) -> Result<EffectiveConfig, ConfigError> {
    let mut merged = parse_layer(default_text)?;
    let mut sources = vec![default_source.to_owned()];
    if let Some((user_text, user_source)) = user {
        merge(&mut merged, parse_layer(user_text)?);
        sources.push(user_source.to_owned());
    }

    let serialized = toml::to_string(&merged).map_err(parse_error)?;
    let mut config = toml::from_str::<AppConfig>(&serialized).map_err(parse_error)?;
    if overrides.apply(&mut config) {
        sources.push("overrides".to_owned());
    }

    let paths = config.validate(approved_root)?;
    let canonical = serde_json::to_vec(&config)?;
    let fingerprint = blake3::hash(&canonical).to_hex().to_string();
    Ok(EffectiveConfig {
        config,
        fingerprint,
        sources,
        paths,
    })
}

impl AppConfig {
    pub fn validate(&self, approved_root: &Path) -> Result<ResolvedPaths, ConfigError> {
        if self.schema_version != 1 {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
        }

        validate_bind_address(&self.bind_address)?;
        validate_range("session_secret_fd", self.session_secret_fd, 3, u32::MAX)?;
        validate_range(
            "ingestion_queue_capacity",
            self.ingestion_queue_capacity,
            1,
            MAX_INGESTION_QUEUE_CAPACITY,
        )?;
        validate_range(
            "maximum_request_bytes",
            self.maximum_request_bytes,
            1,
            MAX_REQUEST_BYTES,
        )?;
        validate_range(
            "maximum_concurrent_requests",
            self.maximum_concurrent_requests,
            1,
            MAX_CONCURRENT_REQUESTS,
        )?;
        validate_range(
            "request_timeout_seconds",
            self.request_timeout_seconds,
            1,
            MAX_REQUEST_TIMEOUT_SECONDS,
        )?;
        validate_range(
            "shutdown_grace_seconds",
            self.shutdown_grace_seconds,
            1,
            MAX_SHUTDOWN_GRACE_SECONDS,
        )?;

        if self.remote_export || self.remote_telemetry {
            return Err(ConfigError::RemoteCapabilityEnabled);
        }

        let approved_root =
            fs::canonicalize(approved_root).map_err(|source| ConfigError::Filesystem {
                field: "approved_root",
                path: approved_root.to_path_buf(),
                source,
            })?;
        let metadata = fs::metadata(&approved_root).map_err(|source| ConfigError::Filesystem {
            field: "approved_root",
            path: approved_root.clone(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(ConfigError::PathEscape {
                field: "approved_root",
            });
        }

        let fixture_input =
            resolve_readable_file(&approved_root, &self.fixture_input, "fixture_input")?;
        let data_root = resolve_writable_root(&approved_root, &self.data_root, "data_root")?;
        let log_root = resolve_writable_root(&approved_root, &self.log_root, "log_root")?;

        Ok(ResolvedPaths {
            approved_root,
            data_root,
            log_root,
            fixture_input,
        })
    }
}

fn parse_layer(text: &str) -> Result<toml::Value, ConfigError> {
    let value = toml::from_str::<toml::Value>(text).map_err(parse_error)?;
    reject_embedded_secrets(&value, "")?;
    validate_credential_references(&value)?;
    Ok(value)
}

fn reject_embedded_secrets(value: &toml::Value, path: &str) -> Result<(), ConfigError> {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                if is_secret_key(key) {
                    return Err(ConfigError::EmbeddedSecret { path: child_path });
                }
                reject_embedded_secrets(value, &child_path)?;
            }
        }
        toml::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                reject_embedded_secrets(value, &format!("{path}[{index}]"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_secret_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "api_key"
            | "api_secret"
            | "password"
            | "private_key"
            | "secret"
            | "token"
            | "access_token"
            | "refresh_token"
    )
}

fn validate_credential_references(value: &toml::Value) -> Result<(), ConfigError> {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                if key == "credential_ref" {
                    let reference = value.as_str().ok_or(ConfigError::CredentialReference)?;
                    KeychainReference::parse(reference)?;
                } else {
                    validate_credential_references(value)?;
                }
            }
        }
        toml::Value::Array(values) => {
            for value in values {
                validate_credential_references(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(existing) = base.get_mut(&key) {
                    merge(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn apply_override<T>(target: &mut T, value: Option<T>, changed: &mut bool) {
    if let Some(value) = value {
        *target = value;
        *changed = true;
    }
}

fn validate_bind_address(value: &str) -> Result<SocketAddrV4, ConfigError> {
    let address = value
        .parse::<SocketAddr>()
        .map_err(|_| ConfigError::NonLoopbackBind)?;
    match address {
        SocketAddr::V4(address) if address.ip().is_loopback() && address.port() == 0 => Ok(address),
        _ => Err(ConfigError::NonLoopbackBind),
    }
}

fn validate_range<T>(
    field: &'static str,
    value: T,
    minimum: T,
    maximum: T,
) -> Result<(), ConfigError>
where
    T: Copy + Ord + TryInto<u64>,
{
    if value < minimum || value > maximum {
        Err(ConfigError::OutOfRange {
            field,
            minimum: minimum.try_into().unwrap_or(u64::MAX),
            maximum: maximum.try_into().unwrap_or(u64::MAX),
            actual: value.try_into().unwrap_or(u64::MAX),
        })
    } else {
        Ok(())
    }
}

fn resolve_readable_file(
    approved_root: &Path,
    configured: &Path,
    field: &'static str,
) -> Result<PathBuf, ConfigError> {
    reject_parent_components(configured, field)?;
    let candidate = configured_path(approved_root, configured);
    let resolved = fs::canonicalize(&candidate).map_err(|_| ConfigError::FixtureUnreadable {
        field,
        path: candidate.clone(),
    })?;
    ensure_contained(approved_root, &resolved, field)?;
    let metadata = fs::metadata(&resolved).map_err(|_| ConfigError::FixtureUnreadable {
        field,
        path: resolved.clone(),
    })?;
    if !metadata.is_file() || File::open(&resolved).is_err() {
        return Err(ConfigError::FixtureUnreadable {
            field,
            path: resolved,
        });
    }
    Ok(resolved)
}

fn resolve_writable_root(
    approved_root: &Path,
    configured: &Path,
    field: &'static str,
) -> Result<PathBuf, ConfigError> {
    reject_parent_components(configured, field)?;
    let candidate = configured_path(approved_root, configured);
    let (existing_ancestor, missing_components) = existing_ancestor(&candidate, field)?;
    let resolved_ancestor =
        fs::canonicalize(&existing_ancestor).map_err(|source| ConfigError::Filesystem {
            field,
            path: existing_ancestor.clone(),
            source,
        })?;
    ensure_contained(approved_root, &resolved_ancestor, field)?;

    let metadata = fs::metadata(&resolved_ancestor).map_err(|source| ConfigError::Filesystem {
        field,
        path: resolved_ancestor.clone(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(ConfigError::Filesystem {
            field,
            path: resolved_ancestor,
            source: io::Error::new(io::ErrorKind::NotADirectory, "ancestor is not a directory"),
        });
    }
    prove_writable(&resolved_ancestor, field)?;

    Ok(missing_components
        .iter()
        .rev()
        .fold(resolved_ancestor, |path, component| path.join(component)))
}

fn existing_ancestor(
    candidate: &Path,
    field: &'static str,
) -> Result<(PathBuf, Vec<PathBuf>), ConfigError> {
    let mut current = candidate.to_path_buf();
    let mut missing = Vec::new();
    while fs::symlink_metadata(&current).is_err() {
        let name = current
            .file_name()
            .ok_or(ConfigError::PathEscape { field })?;
        missing.push(PathBuf::from(name));
        current = current
            .parent()
            .ok_or(ConfigError::PathEscape { field })?
            .to_path_buf();
    }
    Ok((current, missing))
}

fn prove_writable(directory: &Path, field: &'static str) -> Result<(), ConfigError> {
    let probe = directory.join(format!(".cmti-write-probe-{}-{field}", std::process::id()));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .and_then(|file| {
            drop(file);
            fs::remove_file(&probe)
        });
    result.map_err(|source| ConfigError::Filesystem {
        field,
        path: directory.to_path_buf(),
        source,
    })
}

fn reject_parent_components(path: &Path, field: &'static str) -> Result<(), ConfigError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        Err(ConfigError::PathEscape { field })
    } else {
        Ok(())
    }
}

fn configured_path(approved_root: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        approved_root.join(configured)
    }
}

fn ensure_contained(
    approved_root: &Path,
    resolved: &Path,
    field: &'static str,
) -> Result<(), ConfigError> {
    if resolved.starts_with(approved_root) {
        Ok(())
    } else {
        Err(ConfigError::PathEscape { field })
    }
}

fn valid_keychain_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@'))
}

fn parse_error(error: impl fmt::Display) -> ConfigError {
    ConfigError::Parse {
        message: error.to_string(),
    }
}

fn local_only_boolean_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
    let mut schema = SchemaObject {
        instance_type: Some(InstanceType::Boolean.into()),
        ..SchemaObject::default()
    };
    schema.enum_values = Some(vec![serde_json::Value::Bool(false)]);
    Schema::Object(schema)
}

fn loopback_bind_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
    let schema = SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        string: Some(Box::new(StringValidation {
            pattern: Some(r"^127\.0\.0\.1:0$".to_owned()),
            ..StringValidation::default()
        })),
        ..SchemaObject::default()
    };
    Schema::Object(schema)
}

fn optional_keychain_reference_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
    let schema = SchemaObject {
        instance_type: Some(vec![InstanceType::String, InstanceType::Null].into()),
        string: Some(Box::new(StringValidation {
            pattern: Some(r"^keychain://[A-Za-z0-9._@-]{1,128}/[A-Za-z0-9._@-]{1,128}$".to_owned()),
            ..StringValidation::default()
        })),
        ..SchemaObject::default()
    };
    Schema::Object(schema)
}

use std::fmt;
