//! Deterministic layered configuration for the local-only runtime.

pub mod schema;

use rustix::fs::{self as rustix_fs, AtFlags, FileType, Mode, OFlags};
use schemars::{
    JsonSchema,
    schema::{InstanceType, Schema, SchemaObject, StringValidation},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File},
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    os::fd::{AsFd, BorrowedFd},
    path::{Component, Path, PathBuf},
    sync::Arc,
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

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "AppConfig")]
pub(crate) struct RawAppConfig {
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

/// A configuration whose complete local-only contract has been validated.
///
/// Its fields are intentionally private: callers can only obtain this type
/// through [`load`], so invalid values cannot cross the runtime boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppConfig {
    schema_version: u32,
    data_root: PathBuf,
    log_root: PathBuf,
    fixture_input: PathBuf,
    bind_address: SocketAddrV4,
    session_secret_fd: u32,
    ingestion_queue_capacity: usize,
    maximum_request_bytes: usize,
    maximum_concurrent_requests: usize,
    request_timeout_seconds: u64,
    shutdown_grace_seconds: u64,
    remote_export: bool,
    remote_telemetry: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_ref: Option<KeychainReference>,
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
    fn apply(self, config: &mut RawAppConfig) -> bool {
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

#[derive(Clone, Debug)]
pub struct ValidatedDirectory {
    path: PathBuf,
    handle: Arc<File>,
}

impl ValidatedDirectory {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.handle.as_fd()
    }

    /// Creates a new direct child without resolving the configured pathname.
    pub fn create_new_file(&self, name: impl AsRef<OsStr>) -> io::Result<File> {
        let name = name.as_ref();
        if !is_single_normal_component(Path::new(name)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file name must be one normal path component",
            ));
        }
        let owned = rustix_fs::openat(
            self.handle.as_fd(),
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(io::Error::from)?;
        Ok(File::from(owned))
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedFile {
    path: PathBuf,
    handle: Arc<File>,
}

impl ValidatedFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.handle.as_fd()
    }

    pub fn try_clone(&self) -> io::Result<File> {
        self.handle.try_clone()
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedPaths {
    pub approved_root: ValidatedDirectory,
    pub data_root: ValidatedDirectory,
    pub log_root: ValidatedDirectory,
    pub fixture_input: ValidatedFile,
}

#[derive(Clone, Debug)]
pub struct EffectiveConfig {
    pub config: AppConfig,
    pub fingerprint: String,
    pub sources: Vec<String>,
    pub paths: ValidatedPaths,
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
    let mut raw = toml::from_str::<RawAppConfig>(&serialized).map_err(parse_error)?;
    if overrides.apply(&mut raw) {
        sources.push("overrides".to_owned());
    }

    let (config, paths) = AppConfig::validate(raw, approved_root)?;
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
    fn validate(
        raw: RawAppConfig,
        approved_root: &Path,
    ) -> Result<(Self, ValidatedPaths), ConfigError> {
        if raw.schema_version != 1 {
            return Err(ConfigError::UnsupportedSchema(raw.schema_version));
        }

        let bind_address = validate_bind_address(&raw.bind_address)?;
        validate_range("session_secret_fd", raw.session_secret_fd, 3, u32::MAX)?;
        validate_range(
            "ingestion_queue_capacity",
            raw.ingestion_queue_capacity,
            1,
            MAX_INGESTION_QUEUE_CAPACITY,
        )?;
        validate_range(
            "maximum_request_bytes",
            raw.maximum_request_bytes,
            1,
            MAX_REQUEST_BYTES,
        )?;
        validate_range(
            "maximum_concurrent_requests",
            raw.maximum_concurrent_requests,
            1,
            MAX_CONCURRENT_REQUESTS,
        )?;
        validate_range(
            "request_timeout_seconds",
            raw.request_timeout_seconds,
            1,
            MAX_REQUEST_TIMEOUT_SECONDS,
        )?;
        validate_range(
            "shutdown_grace_seconds",
            raw.shutdown_grace_seconds,
            1,
            MAX_SHUTDOWN_GRACE_SECONDS,
        )?;

        if raw.remote_export || raw.remote_telemetry {
            return Err(ConfigError::RemoteCapabilityEnabled);
        }

        let approved = open_approved_root(approved_root)?;
        let fixture_input = open_readable_file(&approved, &raw.fixture_input, "fixture_input")?;
        let data_root = open_writable_root(&approved, &raw.data_root, "data_root")?;
        let log_root = open_writable_root(&approved, &raw.log_root, "log_root")?;
        let paths = ValidatedPaths {
            approved_root: approved,
            data_root,
            log_root,
            fixture_input,
        };
        let config = Self {
            schema_version: raw.schema_version,
            data_root: raw.data_root,
            log_root: raw.log_root,
            fixture_input: raw.fixture_input,
            bind_address,
            session_secret_fd: raw.session_secret_fd,
            ingestion_queue_capacity: raw.ingestion_queue_capacity,
            maximum_request_bytes: raw.maximum_request_bytes,
            maximum_concurrent_requests: raw.maximum_concurrent_requests,
            request_timeout_seconds: raw.request_timeout_seconds,
            shutdown_grace_seconds: raw.shutdown_grace_seconds,
            remote_export: false,
            remote_telemetry: false,
            credential_ref: raw.credential_ref,
        };
        Ok((config, paths))
    }

    pub fn bind_address(&self) -> SocketAddrV4 {
        self.bind_address
    }

    pub fn session_secret_fd(&self) -> u32 {
        self.session_secret_fd
    }

    pub fn ingestion_queue_capacity(&self) -> usize {
        self.ingestion_queue_capacity
    }

    pub fn maximum_request_bytes(&self) -> usize {
        self.maximum_request_bytes
    }

    pub fn maximum_concurrent_requests(&self) -> usize {
        self.maximum_concurrent_requests
    }

    pub fn request_timeout_seconds(&self) -> u64 {
        self.request_timeout_seconds
    }

    pub fn shutdown_grace_seconds(&self) -> u64 {
        self.shutdown_grace_seconds
    }

    pub fn credential_ref(&self) -> Option<&KeychainReference> {
        self.credential_ref.as_ref()
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
        SocketAddr::V4(address) if *address.ip() == Ipv4Addr::LOCALHOST && address.port() == 0 => {
            Ok(address)
        }
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

fn open_approved_root(path: &Path) -> Result<ValidatedDirectory, ConfigError> {
    let owned = rustix_fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|source| ConfigError::Filesystem {
        field: "approved_root",
        path: path.to_path_buf(),
        source: source.into(),
    })?;
    let canonical = fs::canonicalize(path).map_err(|source| ConfigError::Filesystem {
        field: "approved_root",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(ValidatedDirectory {
        path: canonical,
        handle: Arc::new(File::from(owned)),
    })
}

fn open_readable_file(
    approved_root: &ValidatedDirectory,
    configured: &Path,
    field: &'static str,
) -> Result<ValidatedFile, ConfigError> {
    let components = relative_components(configured, field)?;
    let (file_name, directories) = components
        .split_last()
        .ok_or(ConfigError::PathEscape { field })?;
    let parent = open_directory_chain(approved_root, directories, configured, field)?;
    let owned = rustix_fs::openat(
        parent.as_fd(),
        file_name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| ConfigError::FixtureUnreadable {
        field,
        path: approved_root.path.join(configured),
    })?;
    let stat = rustix_fs::fstat(&owned).map_err(|_| ConfigError::FixtureUnreadable {
        field,
        path: approved_root.path.join(configured),
    })?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(ConfigError::FixtureUnreadable {
            field,
            path: approved_root.path.join(configured),
        });
    }
    Ok(ValidatedFile {
        path: approved_root.path.join(configured),
        handle: Arc::new(File::from(owned)),
    })
}

fn open_writable_root(
    approved_root: &ValidatedDirectory,
    configured: &Path,
    field: &'static str,
) -> Result<ValidatedDirectory, ConfigError> {
    let components = relative_components(configured, field)?;
    let directory = open_directory_chain(approved_root, &components, configured, field)?;
    prove_writable(&directory, field)?;
    Ok(directory)
}

fn open_directory_chain(
    approved_root: &ValidatedDirectory,
    components: &[OsString],
    configured: &Path,
    field: &'static str,
) -> Result<ValidatedDirectory, ConfigError> {
    let mut current =
        approved_root
            .handle
            .try_clone()
            .map_err(|source| ConfigError::Filesystem {
                field,
                path: approved_root.path.clone(),
                source,
            })?;
    let mut display = approved_root.path.clone();
    for component in components {
        let owned = rustix_fs::openat(
            current.as_fd(),
            component,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|source| map_directory_open_error(source, field, configured))?;
        current = File::from(owned);
        display.push(component);
    }
    Ok(ValidatedDirectory {
        path: display,
        handle: Arc::new(current),
    })
}

fn map_directory_open_error(
    source: rustix::io::Errno,
    field: &'static str,
    configured: &Path,
) -> ConfigError {
    if matches!(source, rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) {
        ConfigError::PathEscape { field }
    } else {
        ConfigError::Filesystem {
            field,
            path: configured.to_path_buf(),
            source: source.into(),
        }
    }
}

fn prove_writable(directory: &ValidatedDirectory, field: &'static str) -> Result<(), ConfigError> {
    let name = format!(".cmti-write-probe-{}-{field}", std::process::id());
    let owned = rustix_fs::openat(
        directory.handle.as_fd(),
        name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|source| ConfigError::Filesystem {
        field,
        path: directory.path.clone(),
        source: source.into(),
    })?;
    drop(owned);
    rustix_fs::unlinkat(directory.handle.as_fd(), name.as_str(), AtFlags::empty()).map_err(
        |source| ConfigError::Filesystem {
            field,
            path: directory.path.clone(),
            source: source.into(),
        },
    )
}

fn relative_components(path: &Path, field: &'static str) -> Result<Vec<OsString>, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::PathEscape { field });
    }
    let mut normal = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normal.push(value.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ConfigError::PathEscape { field });
            }
        }
    }
    (!normal.is_empty())
        .then_some(normal)
        .ok_or(ConfigError::PathEscape { field })
}

fn is_single_normal_component(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
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
