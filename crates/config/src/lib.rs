//! Deterministic layered configuration for the local-only runtime.

pub mod schema;

use domain::VenueId;
use rustix::fs::{self as rustix_fs, AtFlags, FileType, Mode, OFlags};
use schemars::{
    JsonSchema,
    schema::{InstanceType, Schema, SchemaObject, StringValidation},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File},
    io::{self, Read as _},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    os::fd::{AsFd, BorrowedFd},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub const MAX_INGESTION_QUEUE_CAPACITY: usize = 65_536;
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CONCURRENT_REQUESTS: usize = 1_024;
pub const MAX_REQUEST_TIMEOUT_SECONDS: u64 = 300;
pub const MAX_SHUTDOWN_GRACE_SECONDS: u64 = 30;
pub const MAX_MEMORY_GIB: u64 = 512;
pub const MAX_RUNTIME_THREADS: usize = 256;
pub const MAX_RAW_RETENTION_DAYS: u32 = 30;
pub const MIN_RAW_RETENTION_DAYS: u32 = 7;
pub const MAX_QUALITY_PPM: u32 = 1_000_000;
static WRITE_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, JsonSchema, Serialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl std::str::FromStr for LogLevel {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(ConfigError::InvalidOverride {
                name: "CMTI_LOG_LEVEL".to_owned(),
            }),
        }
    }
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, JsonSchema, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CoverageTier {
    A,
    B,
    C,
}

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, JsonSchema, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum VenueProduct {
    Spot,
    LinearPerpetual,
    InversePerpetual,
    Future,
    Option,
    Derivatives,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WalFormat {
    V1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SchemaCompatibility {
    Strict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DatabaseEngine {
    Sqlite,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDaemonConfig {
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
    pub fixture_input: PathBuf,
    #[schemars(schema_with = "loopback_bind_schema")]
    pub bind_address: String,
    #[schemars(range(min = 3, max = 2147483647))]
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
    #[schemars(range(min = 1, max = 60000))]
    pub wal_flush_interval_ms: u64,
    #[schemars(range(min = 1, max = 512))]
    pub max_memory_gib: u64,
    pub log_level: LogLevel,
    pub wal_format: WalFormat,
    pub schema_compatibility: SchemaCompatibility,
    pub database_engine: DatabaseEngine,
    #[schemars(range(min = 1, max = 256))]
    pub runtime_threads: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawCoverageConfig {
    #[schemars(range(min = 1, max = 10000))]
    pub max_tier_a_instruments: u32,
    #[schemars(range(min = 1, max = 10000))]
    pub max_tier_b_instruments: u32,
    pub reject_over_capacity: bool,
    #[schemars(inner(length(min = 1, max = 32), regex(pattern = r"^[A-Z0-9]+$")))]
    pub watchlist: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawVenueConfig {
    #[schemars(schema_with = "venue_id_schema")]
    pub id: String,
    pub enabled: bool,
    pub coverage_tier: CoverageTier,
    #[schemars(length(min = 1))]
    pub products: Vec<VenueProduct>,
    #[schemars(range(min = 1, max = 10000))]
    pub instrument_budget: u32,
    #[schemars(range(min = 7, max = 30))]
    pub raw_retention_days: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_keychain_reference_schema")]
    pub credential_ref: Option<KeychainReference>,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawModelConfig {
    pub production_registry: PathBuf,
    pub allow_experimental_views: bool,
    pub allow_core_ai: bool,
    pub allow_foundation_models: bool,
    pub abstain_on_incompatible_model: bool,
    #[schemars(schema_with = "model_view_schema")]
    pub selected_view: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawAlertConfig {
    pub local_notifications: bool,
    #[schemars(schema_with = "local_only_boolean_schema")]
    pub outbound_webhooks: bool,
    #[schemars(range(min = 0, max = 1000000))]
    pub minimum_quality_ppm: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawPrivacyConfig {
    #[schemars(schema_with = "local_only_boolean_schema")]
    pub remote_export: bool,
    #[schemars(schema_with = "local_only_boolean_schema")]
    pub remote_telemetry: bool,
    #[schemars(schema_with = "local_only_boolean_schema")]
    pub automatic_crash_upload: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "AppConfig")]
pub(crate) struct RawAppConfig {
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u32,
    #[schemars(schema_with = "production_profile_schema")]
    pub profile: String,
    #[schemars(schema_with = "timezone_schema")]
    pub timezone: String,
    pub daemon: RawDaemonConfig,
    pub coverage: RawCoverageConfig,
    #[schemars(length(min = 1))]
    pub venues: Vec<RawVenueConfig>,
    pub models: RawModelConfig,
    pub alerts: RawAlertConfig,
    pub privacy: RawPrivacyConfig,
}

/// A configuration whose complete local-only contract has been validated.
///
/// Its fields are intentionally private: callers can only obtain this type
/// through [`load`], so invalid values cannot cross the runtime boundary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppConfig {
    schema_version: u32,
    profile: String,
    timezone: String,
    daemon: DaemonConfig,
    coverage: CoverageConfig,
    venues: Vec<VenueConfig>,
    models: ModelConfig,
    alerts: AlertConfig,
    privacy: PrivacyConfig,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DaemonConfig {
    data_dir: PathBuf,
    log_dir: PathBuf,
    fixture_input: PathBuf,
    bind_address: SocketAddrV4,
    session_secret_fd: u32,
    ingestion_queue_capacity: usize,
    maximum_request_bytes: usize,
    maximum_concurrent_requests: usize,
    request_timeout_seconds: u64,
    shutdown_grace_seconds: u64,
    wal_flush_interval_ms: u64,
    max_memory_gib: u64,
    log_level: LogLevel,
    wal_format: WalFormat,
    schema_compatibility: SchemaCompatibility,
    database_engine: DatabaseEngine,
    runtime_threads: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoverageConfig {
    max_tier_a_instruments: u32,
    max_tier_b_instruments: u32,
    reject_over_capacity: bool,
    watchlist: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VenueConfig {
    id: VenueId,
    enabled: bool,
    coverage_tier: CoverageTier,
    products: Vec<VenueProduct>,
    instrument_budget: u32,
    raw_retention_days: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_ref: Option<KeychainReference>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelConfig {
    production_registry: PathBuf,
    allow_experimental_views: bool,
    allow_core_ai: bool,
    allow_foundation_models: bool,
    abstain_on_incompatible_model: bool,
    selected_view: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AlertConfig {
    local_notifications: bool,
    outbound_webhooks: bool,
    minimum_quality_ppm: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PrivacyConfig {
    remote_export: bool,
    remote_telemetry: bool,
    automatic_crash_upload: bool,
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
    pub maximum_concurrent_requests: Option<usize>,
    pub log_level: Option<LogLevel>,
}

impl Overrides {
    fn apply(self, config: &mut RawAppConfig) -> bool {
        let mut changed = false;
        apply_override(&mut config.daemon.data_dir, self.data_root, &mut changed);
        apply_override(&mut config.daemon.log_dir, self.log_root, &mut changed);
        apply_override(
            &mut config.daemon.fixture_input,
            self.fixture_input,
            &mut changed,
        );
        apply_override(
            &mut config.daemon.bind_address,
            self.bind_address,
            &mut changed,
        );
        apply_override(
            &mut config.daemon.session_secret_fd,
            self.session_secret_fd,
            &mut changed,
        );
        apply_override(
            &mut config.daemon.ingestion_queue_capacity,
            self.ingestion_queue_capacity,
            &mut changed,
        );
        apply_override(
            &mut config.daemon.request_timeout_seconds,
            self.request_timeout_seconds,
            &mut changed,
        );
        apply_override(
            &mut config.daemon.maximum_concurrent_requests,
            self.maximum_concurrent_requests,
            &mut changed,
        );
        apply_override(&mut config.daemon.log_level, self.log_level, &mut changed);
        changed
    }
}

#[derive(Clone, Debug, Default)]
pub struct EnvironmentOverrides {
    overrides: Overrides,
}

impl EnvironmentOverrides {
    pub fn from_pairs<I, K, V>(pairs: I) -> Result<Self, ConfigError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut overrides = Overrides::default();
        for (name, value) in pairs {
            let name = name.as_ref();
            let value = value.as_ref();
            match name {
                "CMTI_LOG_LEVEL" => overrides.log_level = Some(value.parse()?),
                "CMTI_REQUEST_TIMEOUT_SECONDS" => {
                    overrides.request_timeout_seconds = Some(parse_override(name, value)?);
                }
                "CMTI_MAXIMUM_CONCURRENT_REQUESTS" => {
                    overrides.maximum_concurrent_requests = Some(parse_override(name, value)?);
                }
                _ => {
                    return Err(ConfigError::UnsupportedOverride {
                        name: name.to_owned(),
                    });
                }
            }
        }
        Ok(Self { overrides })
    }

    fn apply(self, config: &mut RawAppConfig) -> bool {
        self.overrides.apply(config)
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeOverrides {
    pub log_level: Option<LogLevel>,
    pub allow_experimental_views: Option<bool>,
    pub selected_model_view: Option<String>,
    pub local_notifications: Option<bool>,
}

impl RuntimeOverrides {
    fn apply(self, config: &mut RawAppConfig) -> bool {
        let mut changed = false;
        apply_override(&mut config.daemon.log_level, self.log_level, &mut changed);
        apply_override(
            &mut config.models.allow_experimental_views,
            self.allow_experimental_views,
            &mut changed,
        );
        apply_override(
            &mut config.models.selected_view,
            self.selected_model_view,
            &mut changed,
        );
        apply_override(
            &mut config.alerts.local_notifications,
            self.local_notifications,
            &mut changed,
        );
        changed
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TextLayer<'a> {
    text: &'a str,
    source: &'a str,
}

impl<'a> TextLayer<'a> {
    pub const fn new(text: &'a str, source: &'a str) -> Self {
        Self { text, source }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigLayers<'a> {
    pub compiled_defaults: TextLayer<'a>,
    pub system_policy: Option<TextLayer<'a>>,
    pub user_config: Option<TextLayer<'a>>,
    pub environment: EnvironmentOverrides,
    pub command_line: Overrides,
    pub runtime_settings: RuntimeOverrides,
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
    pub model_registry: ValidatedDirectory,
}

#[derive(Clone, Debug)]
pub struct EffectiveConfig {
    pub config: AppConfig,
    pub fingerprint: String,
    pub sources: Vec<String>,
    pub paths: ValidatedPaths,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeImpact {
    Dynamic,
    RestartRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationResult {
    Validated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeContext {
    pub actor: String,
    pub changed_at_unix_nanos: i64,
    pub restart_occurred: bool,
    pub rollback_reference: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigurationChange {
    pub old_hash: String,
    pub new_hash: String,
    pub actor: String,
    pub changed_at_unix_nanos: i64,
    pub validation_result: ValidationResult,
    pub impact: ChangeImpact,
    pub restart_occurred: bool,
    pub affected_sources: Vec<String>,
    pub affected_models: Vec<String>,
    pub rollback_reference: String,
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
    #[error("remote export, telemetry, crash upload, and webhooks must remain disabled")]
    RemoteCapabilityEnabled,
    #[error("unsupported configuration override {name}")]
    UnsupportedOverride { name: String },
    #[error("invalid value for configuration override {name}")]
    InvalidOverride { name: String },
    #[error("configuration profile must be production-local")]
    InvalidProfile,
    #[error("timezone must be UTC or a canonical Area/Location identifier")]
    InvalidTimezone,
    #[error("venue {id} is duplicated")]
    DuplicateVenue { id: String },
    #[error("venue configuration is invalid: {reason}")]
    InvalidVenue { reason: &'static str },
    #[error("coverage request exceeds the configured capacity")]
    UnsafeCapacity,
    #[error("enabled local model capabilities require fail-closed abstention")]
    IncompatibleModelPolicy,
    #[error("model configuration is invalid: {reason}")]
    InvalidModel { reason: &'static str },
    #[error("configuration field is not available in the foundation runtime: {field}")]
    UnsupportedRuntimeCapability { field: &'static str },
    #[error("configuration audit {field} must not be empty")]
    InvalidAuditContext { field: &'static str },
    #[error("configuration change has no effective value difference")]
    NoConfigurationChange,
    #[error("{field} escapes the approved local root")]
    PathEscape { field: &'static str },
    #[error("{field} must be an owner-only directory owned by the current user")]
    UnsafePrivateDirectory { field: &'static str },
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
    #[error("configuration path is not a regular file: {path}")]
    ConfigFileNotRegular { path: PathBuf },
    #[error("configuration file exceeds the {maximum_bytes}-byte local bound")]
    ConfigFileTooLarge { maximum_bytes: usize },
    #[error("configuration file is not valid UTF-8")]
    ConfigFileUtf8(#[source] std::string::FromUtf8Error),
    #[error("configuration file I/O failed at {path}: {source}")]
    ConfigFileIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Reads one regular configuration file through a single non-following
/// descriptor and enforces the byte bound while reading.
pub fn read_bounded_file(path: &Path, maximum_bytes: usize) -> Result<String, ConfigError> {
    let owned = rustix_fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|source| ConfigError::ConfigFileIo {
        path: path.to_path_buf(),
        source: source.into(),
    })?;
    let stat = rustix_fs::fstat(&owned).map_err(|source| ConfigError::ConfigFileIo {
        path: path.to_path_buf(),
        source: source.into(),
    })?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(ConfigError::ConfigFileNotRegular {
            path: path.to_path_buf(),
        });
    }
    if u64::try_from(stat.st_size).unwrap_or(u64::MAX)
        > u64::try_from(maximum_bytes).unwrap_or(u64::MAX)
    {
        return Err(ConfigError::ConfigFileTooLarge { maximum_bytes });
    }

    let mut bytes = Vec::new();
    File::from(owned)
        .take(
            u64::try_from(maximum_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::ConfigFileIo {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > maximum_bytes {
        return Err(ConfigError::ConfigFileTooLarge { maximum_bytes });
    }
    String::from_utf8(bytes).map_err(ConfigError::ConfigFileUtf8)
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
    load_layers(
        ConfigLayers {
            compiled_defaults: TextLayer::new(default_text, default_source),
            system_policy: None,
            user_config: user.map(|(text, source)| TextLayer::new(text, source)),
            environment: EnvironmentOverrides::default(),
            command_line: overrides,
            runtime_settings: RuntimeOverrides::default(),
        },
        approved_root,
    )
}

/// Applies all approved layers in increasing precedence order.
pub fn load_layers(
    layers: ConfigLayers<'_>,
    approved_root: &Path,
) -> Result<EffectiveConfig, ConfigError> {
    let mut merged = parse_layer(layers.compiled_defaults.text)?;
    let mut sources = vec![layers.compiled_defaults.source.to_owned()];
    for layer in [layers.system_policy, layers.user_config]
        .into_iter()
        .flatten()
    {
        merge(&mut merged, parse_layer(layer.text)?);
        sources.push(layer.source.to_owned());
    }

    let serialized = toml::to_string(&merged).map_err(parse_error)?;
    let mut raw = toml::from_str::<RawAppConfig>(&serialized).map_err(parse_error)?;
    if layers.environment.apply(&mut raw) {
        sources.push("environment".to_owned());
    }
    if layers.command_line.apply(&mut raw) {
        sources.push("command-line".to_owned());
    }
    if layers.runtime_settings.apply(&mut raw) {
        sources.push("runtime-settings".to_owned());
    }

    let (config, paths) = AppConfig::validate(raw, approved_root)?;
    let fingerprint = canonical_fingerprint(&config)?;
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
        if raw.profile != "production-local" {
            return Err(ConfigError::InvalidProfile);
        }
        validate_timezone(&raw.timezone)?;

        let bind_address = validate_bind_address(&raw.daemon.bind_address)?;
        validate_range(
            "session_secret_fd",
            raw.daemon.session_secret_fd,
            3,
            i32::MAX as u32,
        )?;
        validate_range(
            "ingestion_queue_capacity",
            raw.daemon.ingestion_queue_capacity,
            1,
            MAX_INGESTION_QUEUE_CAPACITY,
        )?;
        validate_range(
            "maximum_request_bytes",
            raw.daemon.maximum_request_bytes,
            1,
            MAX_REQUEST_BYTES,
        )?;
        validate_range(
            "maximum_concurrent_requests",
            raw.daemon.maximum_concurrent_requests,
            1,
            MAX_CONCURRENT_REQUESTS,
        )?;
        validate_range(
            "request_timeout_seconds",
            raw.daemon.request_timeout_seconds,
            1,
            MAX_REQUEST_TIMEOUT_SECONDS,
        )?;
        validate_range(
            "shutdown_grace_seconds",
            raw.daemon.shutdown_grace_seconds,
            1,
            MAX_SHUTDOWN_GRACE_SECONDS,
        )?;
        validate_range(
            "wal_flush_interval_ms",
            raw.daemon.wal_flush_interval_ms,
            1,
            60_000,
        )?;
        validate_range(
            "max_memory_gib",
            raw.daemon.max_memory_gib,
            1,
            MAX_MEMORY_GIB,
        )?;
        validate_range(
            "runtime_threads",
            raw.daemon.runtime_threads,
            1,
            MAX_RUNTIME_THREADS,
        )?;

        if raw.privacy.remote_export
            || raw.privacy.remote_telemetry
            || raw.privacy.automatic_crash_upload
            || raw.alerts.outbound_webhooks
        {
            return Err(ConfigError::RemoteCapabilityEnabled);
        }
        validate_range(
            "minimum_quality_ppm",
            raw.alerts.minimum_quality_ppm,
            0,
            MAX_QUALITY_PPM,
        )?;
        validate_range(
            "max_tier_a_instruments",
            raw.coverage.max_tier_a_instruments,
            1,
            10_000,
        )?;
        validate_range(
            "max_tier_b_instruments",
            raw.coverage.max_tier_b_instruments,
            1,
            10_000,
        )?;
        if !raw.coverage.reject_over_capacity {
            return Err(ConfigError::UnsafeCapacity);
        }
        if (raw.models.allow_core_ai || raw.models.allow_foundation_models)
            && !raw.models.abstain_on_incompatible_model
        {
            return Err(ConfigError::IncompatibleModelPolicy);
        }
        let model_host_enabled = raw.models.allow_core_ai || raw.models.allow_foundation_models;
        if !model_host_enabled
            && (raw.models.allow_experimental_views || raw.models.selected_view != "disabled")
        {
            return Err(ConfigError::InvalidModel {
                reason: "disabled model hosts require the disabled view",
            });
        }
        if model_host_enabled && raw.models.selected_view == "disabled" {
            return Err(ConfigError::InvalidModel {
                reason: "enabled model hosts require an active view",
            });
        }
        if model_host_enabled
            && !raw.models.allow_experimental_views
            && raw.models.selected_view != "production"
        {
            return Err(ConfigError::InvalidModel {
                reason: "experimental selected_view requires allow_experimental_views",
            });
        }
        relative_components(&raw.models.production_registry, "production_registry")?;
        if !valid_model_view(&raw.models.selected_view) {
            return Err(ConfigError::InvalidModel {
                reason: "selected_view",
            });
        }

        let (venues, tier_a_requested, tier_b_requested) = validate_venues(raw.venues)?;
        if tier_a_requested > raw.coverage.max_tier_a_instruments
            || tier_b_requested > raw.coverage.max_tier_b_instruments
        {
            return Err(ConfigError::UnsafeCapacity);
        }
        let watchlist = validate_watchlist(raw.coverage.watchlist)?;

        let approved = open_approved_root(approved_root)?;
        let fixture_input =
            open_readable_file(&approved, &raw.daemon.fixture_input, "fixture_input")?;
        let model_registry = open_readable_root(
            &approved,
            &raw.models.production_registry,
            "production_registry",
        )?;
        let data_root = open_writable_root(&approved, &raw.daemon.data_dir, "data_dir")?;
        let log_root = open_writable_root(&approved, &raw.daemon.log_dir, "log_dir")?;
        let paths = ValidatedPaths {
            approved_root: approved,
            data_root,
            log_root,
            fixture_input,
            model_registry,
        };
        let config = Self {
            schema_version: raw.schema_version,
            profile: raw.profile,
            timezone: raw.timezone,
            daemon: DaemonConfig {
                data_dir: raw.daemon.data_dir,
                log_dir: raw.daemon.log_dir,
                fixture_input: raw.daemon.fixture_input,
                bind_address,
                session_secret_fd: raw.daemon.session_secret_fd,
                ingestion_queue_capacity: raw.daemon.ingestion_queue_capacity,
                maximum_request_bytes: raw.daemon.maximum_request_bytes,
                maximum_concurrent_requests: raw.daemon.maximum_concurrent_requests,
                request_timeout_seconds: raw.daemon.request_timeout_seconds,
                shutdown_grace_seconds: raw.daemon.shutdown_grace_seconds,
                wal_flush_interval_ms: raw.daemon.wal_flush_interval_ms,
                max_memory_gib: raw.daemon.max_memory_gib,
                log_level: raw.daemon.log_level,
                wal_format: raw.daemon.wal_format,
                schema_compatibility: raw.daemon.schema_compatibility,
                database_engine: raw.daemon.database_engine,
                runtime_threads: raw.daemon.runtime_threads,
            },
            coverage: CoverageConfig {
                max_tier_a_instruments: raw.coverage.max_tier_a_instruments,
                max_tier_b_instruments: raw.coverage.max_tier_b_instruments,
                reject_over_capacity: true,
                watchlist,
            },
            venues,
            models: ModelConfig {
                production_registry: raw.models.production_registry,
                allow_experimental_views: raw.models.allow_experimental_views,
                allow_core_ai: raw.models.allow_core_ai,
                allow_foundation_models: raw.models.allow_foundation_models,
                abstain_on_incompatible_model: raw.models.abstain_on_incompatible_model,
                selected_view: raw.models.selected_view,
            },
            alerts: AlertConfig {
                local_notifications: raw.alerts.local_notifications,
                outbound_webhooks: false,
                minimum_quality_ppm: raw.alerts.minimum_quality_ppm,
            },
            privacy: PrivacyConfig {
                remote_export: false,
                remote_telemetry: false,
                automatic_crash_upload: false,
            },
        };
        Ok((config, paths))
    }

    pub fn bind_address(&self) -> SocketAddrV4 {
        self.daemon.bind_address
    }

    pub fn session_secret_fd(&self) -> u32 {
        self.daemon.session_secret_fd
    }

    pub fn ingestion_queue_capacity(&self) -> usize {
        self.daemon.ingestion_queue_capacity
    }

    pub fn maximum_request_bytes(&self) -> usize {
        self.daemon.maximum_request_bytes
    }

    pub fn maximum_concurrent_requests(&self) -> usize {
        self.daemon.maximum_concurrent_requests
    }

    pub fn request_timeout_seconds(&self) -> u64 {
        self.daemon.request_timeout_seconds
    }

    pub fn shutdown_grace_seconds(&self) -> u64 {
        self.daemon.shutdown_grace_seconds
    }

    pub const fn log_level(&self) -> LogLevel {
        self.daemon.log_level
    }

    pub const fn runtime_threads(&self) -> usize {
        self.daemon.runtime_threads
    }

    pub const fn max_memory_gib(&self) -> u64 {
        self.daemon.max_memory_gib
    }

    pub fn venues(&self) -> &[VenueConfig] {
        &self.venues
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Rejects configuration that the current fixture-backed foundation
    /// runtime cannot truthfully activate yet.
    pub fn validate_foundation_runtime(&self) -> Result<(), ConfigError> {
        if self.daemon.wal_flush_interval_ms != 250
            || self.daemon.max_memory_gib != 40
            || self.daemon.wal_format != WalFormat::V1
            || self.daemon.schema_compatibility != SchemaCompatibility::Strict
            || self.daemon.database_engine != DatabaseEngine::Sqlite
            || self.daemon.runtime_threads != 4
        {
            return Err(ConfigError::UnsupportedRuntimeCapability {
                field: "daemon policy",
            });
        }
        if self.coverage.max_tier_a_instruments != 1
            || self.coverage.max_tier_b_instruments != 1
            || self.coverage.watchlist != ["BTC"]
        {
            return Err(ConfigError::UnsupportedRuntimeCapability { field: "coverage" });
        }
        let venue = self
            .venues
            .first()
            .filter(|_| self.venues.len() == 1)
            .ok_or(ConfigError::UnsupportedRuntimeCapability { field: "venues" })?;
        if venue.id.as_str() != "binance"
            || !venue.enabled
            || venue.coverage_tier != CoverageTier::A
            || venue.products != [VenueProduct::Spot]
            || venue.instrument_budget != 1
            || venue.raw_retention_days != 14
            || venue.credential_ref.is_some()
        {
            return Err(ConfigError::UnsupportedRuntimeCapability { field: "venues" });
        }
        if self.models.allow_experimental_views
            || self.models.allow_core_ai
            || self.models.allow_foundation_models
            || self.models.selected_view != "disabled"
        {
            return Err(ConfigError::UnsupportedRuntimeCapability { field: "models" });
        }
        if self.alerts.local_notifications {
            return Err(ConfigError::UnsupportedRuntimeCapability { field: "alerts" });
        }
        Ok(())
    }
}

impl VenueConfig {
    pub fn id(&self) -> &VenueId {
        &self.id
    }

    pub fn credential_ref(&self) -> Option<&KeychainReference> {
        self.credential_ref.as_ref()
    }

    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub const fn coverage_tier(&self) -> CoverageTier {
        self.coverage_tier
    }

    pub const fn instrument_budget(&self) -> u32 {
        self.instrument_budget
    }

    pub const fn raw_retention_days(&self) -> u32 {
        self.raw_retention_days
    }
}

/// Builds the durable audit record for a validated effective change.
pub fn audit_change(
    old: &EffectiveConfig,
    new: &EffectiveConfig,
    context: ChangeContext,
) -> Result<ConfigurationChange, ConfigError> {
    if context.actor.trim().is_empty() {
        return Err(ConfigError::InvalidAuditContext { field: "actor" });
    }
    if context.rollback_reference.trim().is_empty() {
        return Err(ConfigError::InvalidAuditContext {
            field: "rollback_reference",
        });
    }
    if context.changed_at_unix_nanos <= 0 {
        return Err(ConfigError::InvalidAuditContext {
            field: "changed_at_unix_nanos",
        });
    }
    if old.fingerprint == new.fingerprint {
        return Err(ConfigError::NoConfigurationChange);
    }

    let impact = if restart_required(&old.config, &new.config) {
        ChangeImpact::RestartRequired
    } else {
        ChangeImpact::Dynamic
    };
    Ok(ConfigurationChange {
        old_hash: old.fingerprint.clone(),
        new_hash: new.fingerprint.clone(),
        actor: context.actor,
        changed_at_unix_nanos: context.changed_at_unix_nanos,
        validation_result: ValidationResult::Validated,
        impact,
        restart_occurred: context.restart_occurred,
        affected_sources: affected_sources(&old.config, &new.config),
        affected_models: affected_models(&old.config, &new.config),
        rollback_reference: context.rollback_reference,
    })
}

fn parse_override<T>(name: &str, value: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| ConfigError::InvalidOverride {
        name: name.to_owned(),
    })
}

fn validate_timezone(value: &str) -> Result<(), ConfigError> {
    if value == "UTC" {
        return Ok(());
    }
    let mut components = value.split('/');
    let area = components.next().unwrap_or_default();
    let location = components.next().unwrap_or_default();
    let valid_component = |component: &str| {
        !component.is_empty()
            && component.len() <= 64
            && component
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+'))
    };
    if !valid_component(area)
        || !valid_component(location)
        || !components.all(valid_component)
        || value.len() > 255
    {
        return Err(ConfigError::InvalidTimezone);
    }
    Ok(())
}

fn valid_model_view(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_venues(
    raw_venues: Vec<RawVenueConfig>,
) -> Result<(Vec<VenueConfig>, u32, u32), ConfigError> {
    if raw_venues.is_empty() {
        return Err(ConfigError::InvalidVenue {
            reason: "at least one venue is required",
        });
    }

    let mut ids = BTreeSet::new();
    let mut tier_a_requested = 0_u32;
    let mut tier_b_requested = 0_u32;
    let mut venues = Vec::with_capacity(raw_venues.len());
    for raw in raw_venues {
        let id = VenueId::new(&raw.id).map_err(|_| ConfigError::InvalidVenue { reason: "id" })?;
        if id.as_str() != raw.id {
            return Err(ConfigError::InvalidVenue {
                reason: "id must already be canonical lowercase",
            });
        }
        if !ids.insert(raw.id.clone()) {
            return Err(ConfigError::DuplicateVenue { id: raw.id });
        }
        if raw.products.is_empty() {
            return Err(ConfigError::InvalidVenue {
                reason: "products must not be empty",
            });
        }
        let product_count = raw.products.len();
        let mut products = raw.products;
        products.sort_unstable();
        products.dedup();
        if products.len() != product_count {
            return Err(ConfigError::InvalidVenue {
                reason: "products must not contain duplicates",
            });
        }
        if products
            .iter()
            .any(|product| !venue_product_supported(id.as_str(), *product))
        {
            return Err(ConfigError::InvalidVenue {
                reason: "unsupported venue/product combination",
            });
        }
        validate_range("instrument_budget", raw.instrument_budget, 1, 10_000)?;
        validate_range(
            "raw_retention_days",
            raw.raw_retention_days,
            MIN_RAW_RETENTION_DAYS,
            MAX_RAW_RETENTION_DAYS,
        )?;

        if raw.enabled {
            match raw.coverage_tier {
                CoverageTier::A => {
                    tier_a_requested = tier_a_requested
                        .checked_add(raw.instrument_budget)
                        .ok_or(ConfigError::UnsafeCapacity)?;
                }
                CoverageTier::B => {
                    tier_b_requested = tier_b_requested
                        .checked_add(raw.instrument_budget)
                        .ok_or(ConfigError::UnsafeCapacity)?;
                }
                CoverageTier::C => {}
            }
        }
        venues.push(VenueConfig {
            id,
            enabled: raw.enabled,
            coverage_tier: raw.coverage_tier,
            products,
            instrument_budget: raw.instrument_budget,
            raw_retention_days: raw.raw_retention_days,
            credential_ref: raw.credential_ref,
        });
    }
    if !venues.iter().any(|venue| venue.enabled) {
        return Err(ConfigError::InvalidVenue {
            reason: "at least one venue must be enabled",
        });
    }
    venues.sort_by(|left, right| left.id.cmp(&right.id));
    Ok((venues, tier_a_requested, tier_b_requested))
}

fn venue_product_supported(venue: &str, product: VenueProduct) -> bool {
    match venue {
        "binance" => matches!(product, VenueProduct::Spot | VenueProduct::LinearPerpetual),
        "bybit" => matches!(
            product,
            VenueProduct::Spot | VenueProduct::LinearPerpetual | VenueProduct::InversePerpetual
        ),
        "kraken" => matches!(product, VenueProduct::Spot | VenueProduct::Derivatives),
        "deribit" => matches!(
            product,
            VenueProduct::LinearPerpetual
                | VenueProduct::InversePerpetual
                | VenueProduct::Future
                | VenueProduct::Option
        ),
        _ => false,
    }
}

fn validate_watchlist(mut watchlist: Vec<String>) -> Result<Vec<String>, ConfigError> {
    if watchlist.iter().any(|symbol| {
        symbol.is_empty()
            || symbol.len() > 32
            || !symbol
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    }) {
        return Err(ConfigError::UnsafeCapacity);
    }
    watchlist.sort();
    watchlist.dedup();
    Ok(watchlist)
}

fn canonical_fingerprint(config: &AppConfig) -> Result<String, ConfigError> {
    #[derive(Serialize)]
    struct CanonicalConfig<'a> {
        domain: &'static str,
        config: &'a AppConfig,
    }

    let canonical = serde_json::to_vec(&CanonicalConfig {
        domain: "cmti:effective-config:v1",
        config,
    })?;
    Ok(blake3::hash(&canonical).to_hex().to_string())
}

fn restart_required(old: &AppConfig, new: &AppConfig) -> bool {
    old.schema_version != new.schema_version
        || old.profile != new.profile
        || old.timezone != new.timezone
        || daemon_restart_required(&old.daemon, &new.daemon)
        || old.coverage.max_tier_a_instruments != new.coverage.max_tier_a_instruments
        || old.coverage.max_tier_b_instruments != new.coverage.max_tier_b_instruments
        || old.coverage.reject_over_capacity != new.coverage.reject_over_capacity
        || venue_restart_required(&old.venues, &new.venues)
        || old.models.production_registry != new.models.production_registry
        || old.models.allow_core_ai != new.models.allow_core_ai
        || old.models.allow_foundation_models != new.models.allow_foundation_models
        || old.models.abstain_on_incompatible_model != new.models.abstain_on_incompatible_model
        || old.privacy != new.privacy
}

fn daemon_restart_required(old: &DaemonConfig, new: &DaemonConfig) -> bool {
    old.data_dir != new.data_dir
        || old.log_dir != new.log_dir
        || old.fixture_input != new.fixture_input
        || old.bind_address != new.bind_address
        || old.session_secret_fd != new.session_secret_fd
        || old.ingestion_queue_capacity != new.ingestion_queue_capacity
        || old.maximum_request_bytes != new.maximum_request_bytes
        || old.maximum_concurrent_requests != new.maximum_concurrent_requests
        || old.request_timeout_seconds != new.request_timeout_seconds
        || old.shutdown_grace_seconds != new.shutdown_grace_seconds
        || old.wal_flush_interval_ms != new.wal_flush_interval_ms
        || old.max_memory_gib != new.max_memory_gib
        || old.wal_format != new.wal_format
        || old.schema_compatibility != new.schema_compatibility
        || old.database_engine != new.database_engine
        || old.runtime_threads != new.runtime_threads
}

fn venue_restart_required(old: &[VenueConfig], new: &[VenueConfig]) -> bool {
    old.len() != new.len()
        || old.iter().zip(new).any(|(old, new)| {
            old.id != new.id
                || old.coverage_tier != new.coverage_tier
                || old.products != new.products
                || old.instrument_budget != new.instrument_budget
                || old.credential_ref != new.credential_ref
        })
}

fn affected_sources(old: &AppConfig, new: &AppConfig) -> Vec<String> {
    let old_by_id = old
        .venues
        .iter()
        .map(|venue| (venue.id.as_str(), venue))
        .collect::<BTreeMap<_, _>>();
    let new_by_id = new
        .venues
        .iter()
        .map(|venue| (venue.id.as_str(), venue))
        .collect::<BTreeMap<_, _>>();
    let mut affected = old_by_id
        .keys()
        .chain(new_by_id.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|id| old_by_id.get(id) != new_by_id.get(id))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if old.coverage != new.coverage {
        affected.extend(
            old.venues
                .iter()
                .chain(&new.venues)
                .filter(|venue| venue.enabled)
                .map(|venue| venue.id.to_string()),
        );
    }
    affected.into_iter().collect()
}

fn affected_models(old: &AppConfig, new: &AppConfig) -> Vec<String> {
    let mut affected = BTreeSet::new();
    if old.models.production_registry != new.models.production_registry {
        affected.insert(
            old.models
                .production_registry
                .to_string_lossy()
                .into_owned(),
        );
        affected.insert(
            new.models
                .production_registry
                .to_string_lossy()
                .into_owned(),
        );
    }
    if old.models.selected_view != new.models.selected_view {
        affected.insert(old.models.selected_view.clone());
        affected.insert(new.models.selected_view.clone());
    }
    if old.models.allow_core_ai != new.models.allow_core_ai {
        affected.insert("core-ai".to_owned());
    }
    if old.models.allow_foundation_models != new.models.allow_foundation_models {
        affected.insert("foundation-models".to_owned());
    }
    if old.models.allow_experimental_views != new.models.allow_experimental_views {
        affected.insert("experimental-views".to_owned());
    }
    if old.models.abstain_on_incompatible_model != new.models.abstain_on_incompatible_model {
        affected.insert("abstention-policy".to_owned());
    }
    affected.into_iter().collect()
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
    let normalized = key
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    matches!(
        normalized.as_slice(),
        b"apikey"
            | b"apisecret"
            | b"clientsecret"
            | b"secretkey"
            | b"accesskey"
            | b"password"
            | b"passphrase"
            | b"privatekey"
            | b"secret"
            | b"token"
            | b"authtoken"
            | b"bearertoken"
            | b"accesstoken"
            | b"refreshtoken"
            | b"mnemonic"
            | b"seedphrase"
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

fn apply_override<T: PartialEq>(target: &mut T, value: Option<T>, changed: &mut bool) {
    if let Some(value) = value
        && *target != value
    {
        *target = value;
        *changed = true;
    }
}

fn validate_bind_address(value: &str) -> Result<SocketAddrV4, ConfigError> {
    if value != "127.0.0.1:0" {
        return Err(ConfigError::NonLoopbackBind);
    }
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
    validate_private_directory(&directory, field)?;
    prove_writable(&directory, field)?;
    Ok(directory)
}

fn validate_private_directory(
    directory: &ValidatedDirectory,
    field: &'static str,
) -> Result<(), ConfigError> {
    let stat = rustix_fs::fstat(directory.handle.as_fd())
        .map_err(|_| ConfigError::UnsafePrivateDirectory { field })?;
    let safe = FileType::from_raw_mode(stat.st_mode).is_dir()
        && stat.st_uid == rustix::process::geteuid().as_raw()
        && stat.st_mode & 0o077 == 0;
    if safe {
        Ok(())
    } else {
        Err(ConfigError::UnsafePrivateDirectory { field })
    }
}

fn open_readable_root(
    approved_root: &ValidatedDirectory,
    configured: &Path,
    field: &'static str,
) -> Result<ValidatedDirectory, ConfigError> {
    let components = relative_components(configured, field)?;
    open_directory_chain(approved_root, &components, configured, field)
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
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..8 {
        let sequence = WRITE_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            ".cmti-write-probe-{}-{timestamp}-{sequence}-{field}",
            std::process::id()
        );
        let owned = match rustix_fs::openat(
            directory.handle.as_fd(),
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(owned) => owned,
            Err(rustix::io::Errno::EXIST) => continue,
            Err(source) => {
                return Err(ConfigError::Filesystem {
                    field,
                    path: directory.path.clone(),
                    source: source.into(),
                });
            }
        };
        drop(owned);
        return rustix_fs::unlinkat(directory.handle.as_fd(), name.as_str(), AtFlags::empty())
            .map_err(|source| ConfigError::Filesystem {
                field,
                path: directory.path.clone(),
                source: source.into(),
            });
    }
    Err(ConfigError::Filesystem {
        field,
        path: directory.path.clone(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique write probe",
        ),
    })
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

fn parse_error(_: impl fmt::Display) -> ConfigError {
    ConfigError::Parse {
        message: "invalid TOML or typed configuration".to_owned(),
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

fn production_profile_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
    let mut schema = SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        ..SchemaObject::default()
    };
    schema.enum_values = Some(vec![serde_json::Value::String(
        "production-local".to_owned(),
    )]);
    Schema::Object(schema)
}

fn timezone_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
    let schema = SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        string: Some(Box::new(StringValidation {
            max_length: Some(255),
            pattern: Some(
                r"^(UTC|[A-Za-z0-9_+\-]{1,64}/[A-Za-z0-9_+\-]{1,64}(?:/[A-Za-z0-9_+\-]{1,64})*)$"
                    .to_owned(),
            ),
            ..StringValidation::default()
        })),
        ..SchemaObject::default()
    };
    Schema::Object(schema)
}

fn venue_id_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
    let schema = SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        string: Some(Box::new(StringValidation {
            max_length: Some(64),
            min_length: Some(1),
            pattern: Some(r"^[a-z0-9._:-]+$".to_owned()),
        })),
        ..SchemaObject::default()
    };
    Schema::Object(schema)
}

fn model_view_schema(_: &mut schemars::r#gen::SchemaGenerator) -> Schema {
    let schema = SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        string: Some(Box::new(StringValidation {
            max_length: Some(64),
            min_length: Some(1),
            pattern: Some(r"^[A-Za-z0-9._-]+$".to_owned()),
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
