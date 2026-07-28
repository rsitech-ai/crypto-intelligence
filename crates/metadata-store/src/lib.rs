//! Crash-aware, single-writer SQLite metadata and audit storage.
//!
//! Opening a [`MetadataStore`] completes SQLite configuration, schema
//! verification, recovery checks, audit-chain validation, and validated
//! instrument-catalog rehydration before the handle is returned. All mutations
//! are serialized by one dedicated writer thread behind a bounded command
//! queue.
//!
//! The audit chain detects corruption or rewriting within the retained
//! database, but its head is not externally anchored. Preventing an attacker
//! with arbitrary local file-write access from truncating both the tail and the
//! stored head remains an operating-system access-control and backup concern.

pub mod migrations;

use std::{
    fmt,
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use domain::{InstrumentDefinition, InstrumentDefinitionInput, UnixNanos};
use instrument_registry::{
    CatalogMutation, CatalogRecord, CatalogRevision, CatalogSnapshot, CorrectionInput,
    InstrumentRegistry, MAXIMUM_BATCH_RECORDS, RegistryError, RevisionMetadata,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, TransactionBehavior, config::DbConfig,
    ffi::ErrorCode, params,
};
use rustix::fs::{FlockOperation, Mode, OFlags, fchmod, flock, open};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

pub(crate) const APPLICATION_ID: i64 = 0x434D_5449;
const MINIMUM_SQLITE_VERSION_NUMBER: i32 = 3_053_003;
const MAXIMUM_QUEUE_CAPACITY: usize = 1_024;
const MAXIMUM_READER_CONCURRENCY: usize = 32;
const MAXIMUM_AUDIT_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAXIMUM_CATALOG_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAXIMUM_TEXT_BYTES: usize = 128;
const AUDIT_HASH_DOMAIN: &[u8] = b"cmti:metadata-audit-record:v1\0";
const SCHEMA_HASH_DOMAIN: &[u8] = b"cmti:metadata-schema:v1\0";
const EXPECTED_SCHEMA_DIGEST: [u8; 32] = [
    0xbc, 0x3f, 0xb8, 0xc3, 0x64, 0x78, 0x6a, 0xde, 0x83, 0x6f, 0x7d, 0x7e, 0xf0, 0x74, 0x61, 0xc7,
    0xa7, 0x54, 0x73, 0x7f, 0x0a, 0x02, 0xd0, 0xa7, 0x2b, 0xd1, 0x60, 0x79, 0x03, 0xea, 0xa1, 0xcb,
];
const ZERO_HASH: [u8; 32] = [0; 32];

static FIXTURE_AUDIT_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Bounded runtime settings for the store actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoreOptions {
    queue_capacity: usize,
    reader_concurrency: usize,
    busy_timeout: Duration,
    maximum_audit_payload_bytes: usize,
}

impl StoreOptions {
    /// Conservative settings used by focused local tests.
    pub const fn test() -> Self {
        Self {
            queue_capacity: 16,
            reader_concurrency: 2,
            busy_timeout: Duration::from_millis(250),
            maximum_audit_payload_bytes: 64 * 1024,
        }
    }

    /// Override the bounded SQLite busy timeout.
    #[must_use]
    pub const fn with_busy_timeout(mut self, busy_timeout: Duration) -> Self {
        self.busy_timeout = busy_timeout;
        self
    }

    fn validate(self) -> Result<Self, StoreError> {
        if self.queue_capacity == 0
            || self.queue_capacity > MAXIMUM_QUEUE_CAPACITY
            || self.reader_concurrency == 0
            || self.reader_concurrency > MAXIMUM_READER_CONCURRENCY
            || self.busy_timeout.is_zero()
            || self.busy_timeout > Duration::from_secs(30)
            || self.maximum_audit_payload_bytes == 0
            || self.maximum_audit_payload_bytes > MAXIMUM_AUDIT_PAYLOAD_BYTES
        {
            return Err(StoreError::InvalidOptions);
        }
        Ok(self)
    }
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            queue_capacity: 128,
            reader_concurrency: 4,
            busy_timeout: Duration::from_secs(2),
            maximum_audit_payload_bytes: 256 * 1024,
        }
    }
}

/// Validated input for one append-only audit-chain record.
#[derive(Clone, Eq, PartialEq)]
pub struct AuditEvent {
    audit_id: String,
    occurred_at_ns: i64,
    actor: String,
    category: String,
    payload_schema: String,
    payload: Vec<u8>,
}

impl fmt::Debug for AuditEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuditEvent")
            .field("audit_id", &self.audit_id)
            .field("occurred_at_ns", &self.occurred_at_ns)
            .field("actor", &self.actor)
            .field("category", &self.category)
            .field("payload_schema", &self.payload_schema)
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

impl AuditEvent {
    pub fn try_new(
        audit_id: impl Into<String>,
        occurred_at_ns: i64,
        actor: impl Into<String>,
        category: impl Into<String>,
        payload_schema: impl Into<String>,
        payload: Vec<u8>,
    ) -> Result<Self, StoreError> {
        let event = Self {
            audit_id: audit_id.into(),
            occurred_at_ns,
            actor: actor.into(),
            category: category.into(),
            payload_schema: payload_schema.into(),
            payload,
        };
        if !valid_text(&event.audit_id)
            || !valid_text(&event.actor)
            || !valid_text(&event.category)
            || !valid_text(&event.payload_schema)
            || event.payload.is_empty()
            || event.payload.len() > MAXIMUM_AUDIT_PAYLOAD_BYTES
        {
            return Err(StoreError::InvalidAuditEvent);
        }
        Ok(event)
    }
}

/// One committed audit record.
#[derive(Clone, Eq, PartialEq)]
pub struct AuditRecord {
    pub sequence: u64,
    pub audit_id: String,
    pub occurred_at_ns: i64,
    pub actor: String,
    pub category: String,
    pub payload_schema: String,
    payload: Vec<u8>,
    pub payload_hash: [u8; 32],
    pub previous_hash: [u8; 32],
    pub record_hash: [u8; 32],
}

impl AuditRecord {
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl fmt::Debug for AuditRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuditRecord")
            .field("sequence", &self.sequence)
            .field("audit_id", &self.audit_id)
            .field("occurred_at_ns", &self.occurred_at_ns)
            .field("actor", &self.actor)
            .field("category", &self.category)
            .field("payload_schema", &self.payload_schema)
            .field("payload_bytes", &self.payload.len())
            .field("payload_hash", &hex::encode(self.payload_hash))
            .field("previous_hash", &hex::encode(self.previous_hash))
            .field("record_hash", &hex::encode(self.record_hash))
            .finish()
    }
}

/// Validated, exactly-once request to append an instrument-catalog batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogRequest {
    request_id: String,
    actor: String,
    expected_revision: Option<CatalogRevision>,
    mutations: Vec<CatalogMutation>,
}

impl CatalogRequest {
    pub fn try_new(
        request_id: impl Into<String>,
        actor: impl Into<String>,
        expected_revision: Option<CatalogRevision>,
        mutations: Vec<CatalogMutation>,
    ) -> Result<Self, StoreError> {
        let request = Self {
            request_id: request_id.into(),
            actor: actor.into(),
            expected_revision,
            mutations,
        };
        if !valid_text(&request.request_id)
            || request.request_id.len() > 120
            || !valid_text(&request.actor)
            || request.mutations.is_empty()
            || request.mutations.len() > MAXIMUM_BATCH_RECORDS
        {
            return Err(StoreError::InvalidCatalogRequest);
        }
        Ok(request)
    }
}

/// Durable outcome of one exactly-once catalog request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogCommit {
    pub request_id: String,
    pub previous_revision: Option<CatalogRevision>,
    pub committed_revision: Option<CatalogRevision>,
    pub appended_records: usize,
    pub idempotent_records: usize,
    pub catalog_digest: [u8; 32],
    pub history_digest: [u8; 32],
    pub audit_sequence: u64,
}

/// Runtime SQLite identity and connection policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqliteInfo {
    pub version: String,
    pub source_id: String,
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: bool,
    pub trusted_schema: bool,
    pub wal_autocheckpoint: i64,
    pub application_id: i64,
    pub user_version: i64,
}

/// Explicit WAL checkpoint policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckpointMode {
    Passive,
    Restart,
    Truncate,
}

impl CheckpointMode {
    const fn sql(self) -> &'static str {
        match self {
            Self::Passive => "PRAGMA wal_checkpoint(PASSIVE)",
            Self::Restart => "PRAGMA wal_checkpoint(RESTART)",
            Self::Truncate => "PRAGMA wal_checkpoint(TRUNCATE)",
        }
    }
}

/// Observable result of an explicit checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CheckpointReport {
    pub busy: bool,
    pub log_frames: u64,
    pub checkpointed_frames: u64,
    pub elapsed: Duration,
}

/// Startup recovery evidence captured before the handle became ready.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartupReport {
    pub recovered_unclean_shutdown: bool,
    pub startup_generation: u64,
}

/// Latest bounded health observations produced by the writer actor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoreHealth {
    pub last_checkpoint: Option<CheckpointReport>,
    pub last_checkpoint_at_ns: Option<i64>,
    pub last_checkpoint_ok: Option<bool>,
    pub last_integrity_check_at_ns: Option<i64>,
    pub last_integrity_check_ok: Option<bool>,
}

/// Errors are typed at the storage boundary and avoid echoing SQL or payloads.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("metadata store options are outside their supported bounds")]
    InvalidOptions,
    #[error("metadata database path is unsafe")]
    UnsafeDatabasePath,
    #[error("metadata database is already owned by another process")]
    StoreAlreadyLocked,
    #[error("metadata database is busy")]
    DatabaseBusy,
    #[error("metadata database has no remaining storage capacity")]
    DatabaseFull,
    #[error("metadata database operation failed")]
    Database(#[source] rusqlite::Error),
    #[error("metadata store filesystem operation failed")]
    Io(#[source] io::Error),
    #[error("metadata writer actor stopped")]
    ActorStopped,
    #[error("metadata writer thread panicked")]
    ActorPanicked,
    #[error("metadata writer thread could not start")]
    ThreadSpawn(#[source] io::Error),
    #[error("a file-backed metadata reader is unavailable")]
    ReaderUnavailable,
    #[error("metadata reader task stopped unexpectedly")]
    ReaderTaskStopped,
    #[error("SQLite runtime is older than the required 3.53.3 release")]
    UnsupportedSqliteVersion,
    #[error("metadata database application identity does not match")]
    ApplicationIdMismatch,
    #[error("an unclaimed SQLite database already contains foreign schema objects")]
    UnclaimedNonEmptyDatabase,
    #[error("metadata schema state is internally inconsistent")]
    SchemaStateMismatch,
    #[error("metadata schema version {found} is newer than supported version {supported}")]
    UnknownSchemaVersion { found: i64, supported: i64 },
    #[error("metadata migration {version} is missing")]
    MissingMigration { version: i64 },
    #[error("metadata migration {version} name does not match the manifest")]
    MigrationNameMismatch { version: i64 },
    #[error("metadata migration {version} checksum does not match the manifest")]
    MigrationChecksumMismatch { version: i64 },
    #[error("audit input is invalid or exceeds its bound")]
    InvalidAuditEvent,
    #[error("audit identifier already exists with different content")]
    AuditIdConflict,
    #[error("audit chain integrity verification failed")]
    AuditIntegrity,
    #[error("catalog request is invalid or exceeds its bound")]
    InvalidCatalogRequest,
    #[error("catalog request identifier already exists with different content")]
    CatalogRequestConflict,
    #[error("catalog persistence or rehydration integrity verification failed")]
    CatalogIntegrity,
    #[error("instrument catalog operation failed: {0}")]
    Catalog(#[source] RegistryError),
    #[error("instrument catalog has no committed snapshot")]
    CatalogEmpty,
    #[error("database integrity verification failed")]
    IntegrityCheckFailed,
    #[error("system clock is before the Unix epoch")]
    SystemClockBeforeEpoch,
    #[error("system timestamp exceeds the supported range")]
    SystemTimeOverflow,
    #[error("stored integer exceeds the supported range")]
    IntegerRange,
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        map_sqlite(error)
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn map_sqlite(error: rusqlite::Error) -> StoreError {
    match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => StoreError::DatabaseBusy,
        Some(ErrorCode::DiskFull) => StoreError::DatabaseFull,
        _ => StoreError::Database(error),
    }
}

/// Handle to one dedicated SQLite writer.
pub struct MetadataStore {
    sender: mpsc::Sender<Command>,
    writer: Option<thread::JoinHandle<()>>,
    reader_path: Option<PathBuf>,
    reader_gate: Arc<tokio::sync::Semaphore>,
    startup_report: StartupReport,
    closed: bool,
}

impl MetadataStore {
    /// Open, configure, migrate, recover, and verify a file-backed store.
    pub async fn open(path: impl AsRef<Path>, options: StoreOptions) -> Result<Self, StoreError> {
        let options = options.validate()?;
        let path = validate_database_path(path.as_ref())?;
        Self::start(OpenTarget::File(path), options).await
    }

    /// Open an isolated in-memory store for focused tests.
    pub async fn memory_for_test() -> Result<Self, StoreError> {
        Self::start(OpenTarget::Memory, StoreOptions::test()).await
    }

    async fn start(target: OpenTarget, options: StoreOptions) -> Result<Self, StoreError> {
        let reader_path = target.file_path().map(Path::to_path_buf);
        let (sender, receiver) = mpsc::channel(options.queue_capacity);
        let (ready_sender, ready_receiver) = oneshot::channel();
        let writer = thread::Builder::new()
            .name("cmti-metadata-writer".to_owned())
            .spawn(move || run_writer(target, options, receiver, ready_sender))
            .map_err(StoreError::ThreadSpawn)?;
        let startup_report = match ready_receiver.await {
            Ok(Ok(report)) => report,
            Ok(Err(error)) => {
                writer.join().map_err(|_| StoreError::ActorPanicked)?;
                return Err(error);
            }
            Err(_) => {
                writer.join().map_err(|_| StoreError::ActorPanicked)?;
                return Err(StoreError::ActorStopped);
            }
        };
        Ok(Self {
            sender,
            writer: Some(writer),
            reader_path,
            reader_gate: Arc::new(tokio::sync::Semaphore::new(options.reader_concurrency)),
            startup_report,
            closed: false,
        })
    }

    pub const fn startup_report(&self) -> StartupReport {
        self.startup_report
    }

    /// Re-verify the immutable migration manifest. Opening already performs
    /// this check; the operation is intentionally idempotent.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Migrate { reply }).await?;
        receive(response).await
    }

    pub async fn integrity_check(&self) -> Result<String, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::IntegrityCheck { reply }).await?;
        receive(response).await
    }

    pub async fn sqlite_info(&self) -> Result<SqliteInfo, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::SqliteInfo { reply }).await?;
        receive(response).await
    }

    pub async fn checkpoint(&self, mode: CheckpointMode) -> Result<CheckpointReport, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Checkpoint { mode, reply }).await?;
        receive(response).await
    }

    pub async fn health(&self) -> Result<StoreHealth, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Health { reply }).await?;
        receive(response).await
    }

    pub async fn append_audit(&self, event: AuditEvent) -> Result<AuditRecord, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::AppendAudit { event, reply }).await?;
        receive(response).await
    }

    /// Validate and atomically persist a catalog batch and its audit record.
    pub async fn append_catalog(
        &self,
        request: CatalogRequest,
    ) -> Result<CatalogCommit, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::AppendCatalog { request, reply }).await?;
        receive(response).await
    }

    /// Return an immutable snapshot of the fully rehydrated catalog.
    pub async fn catalog_snapshot(&self) -> Result<Arc<CatalogSnapshot>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::CatalogSnapshot { reply }).await?;
        receive(response).await
    }

    /// Create a reader handle that opens independent read-only, private-cache
    /// SQLite connections for each bounded query.
    pub fn reader(&self) -> Result<MetadataReader, StoreError> {
        self.reader_path
            .clone()
            .map(|path| MetadataReader {
                path,
                gate: Arc::clone(&self.reader_gate),
            })
            .ok_or(StoreError::ReaderUnavailable)
    }

    #[doc(hidden)]
    pub async fn append_audit_fixture(&self) -> Result<AuditRecord, StoreError> {
        let counter = FIXTURE_AUDIT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = unix_time_ns()?;
        let event = AuditEvent::try_new(
            format!("fixture-{}-{counter}", std::process::id()),
            now,
            "metadata-store-test",
            "fixture",
            "cmti.metadata.fixture.v1",
            b"{}".to_vec(),
        )?;
        self.append_audit(event).await
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub async fn delete_audit_for_test(&self, audit_id: &str) -> Result<(), StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::DeleteAuditForTest {
            audit_id: audit_id.to_owned(),
            reply,
        })
        .await?;
        receive(response).await
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub async fn update_audit_for_test(&self, audit_id: &str) -> Result<(), StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::UpdateAuditForTest {
            audit_id: audit_id.to_owned(),
            reply,
        })
        .await?;
        receive(response).await
    }

    #[doc(hidden)]
    pub async fn table_names_for_test(&self) -> Result<Vec<String>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::TableNamesForTest { reply }).await?;
        receive(response).await
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub async fn exhaust_page_capacity_for_test(&self) -> Result<(), StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::ExhaustPageCapacityForTest { reply })
            .await?;
        receive(response).await
    }

    async fn send(&self, command: Command) -> Result<(), StoreError> {
        self.sender
            .send(command)
            .await
            .map_err(|_| StoreError::ActorStopped)
    }

    /// Drain accepted commands, checkpoint, mark the store clean, and join the
    /// dedicated writer thread.
    pub async fn shutdown(mut self) -> Result<(), StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::Shutdown { reply: Some(reply) }).await?;
        let shutdown_result = receive(response).await;
        self.closed = true;
        if let Some(writer) = self.writer.take() {
            writer.join().map_err(|_| StoreError::ActorPanicked)?;
        }
        shutdown_result
    }
}

impl Drop for MetadataStore {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.sender.try_send(Command::Shutdown { reply: None });
        }
    }
}

async fn receive<T>(response: oneshot::Receiver<Result<T, StoreError>>) -> Result<T, StoreError> {
    response.await.map_err(|_| StoreError::ActorStopped)?
}

enum Command {
    Migrate {
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    IntegrityCheck {
        reply: oneshot::Sender<Result<String, StoreError>>,
    },
    SqliteInfo {
        reply: oneshot::Sender<Result<SqliteInfo, StoreError>>,
    },
    Checkpoint {
        mode: CheckpointMode,
        reply: oneshot::Sender<Result<CheckpointReport, StoreError>>,
    },
    Health {
        reply: oneshot::Sender<Result<StoreHealth, StoreError>>,
    },
    AppendAudit {
        event: AuditEvent,
        reply: oneshot::Sender<Result<AuditRecord, StoreError>>,
    },
    AppendCatalog {
        request: CatalogRequest,
        reply: oneshot::Sender<Result<CatalogCommit, StoreError>>,
    },
    CatalogSnapshot {
        reply: oneshot::Sender<Result<Arc<CatalogSnapshot>, StoreError>>,
    },
    #[cfg(feature = "test-support")]
    DeleteAuditForTest {
        audit_id: String,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    #[cfg(feature = "test-support")]
    UpdateAuditForTest {
        audit_id: String,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    TableNamesForTest {
        reply: oneshot::Sender<Result<Vec<String>, StoreError>>,
    },
    #[cfg(feature = "test-support")]
    ExhaustPageCapacityForTest {
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    Shutdown {
        reply: Option<oneshot::Sender<Result<(), StoreError>>>,
    },
}

enum OpenTarget {
    File(PathBuf),
    Memory,
}

impl OpenTarget {
    fn file_path(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Memory => None,
        }
    }
}

/// Handle for isolated read-only SQLite queries.
#[derive(Clone, Debug)]
pub struct MetadataReader {
    path: PathBuf,
    gate: Arc<tokio::sync::Semaphore>,
}

impl MetadataReader {
    pub async fn integrity_check(&self) -> Result<String, StoreError> {
        let path = self.path.clone();
        let permit = Arc::clone(&self.gate)
            .acquire_owned()
            .await
            .map_err(|_| StoreError::ReaderTaskStopped)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let connection = open_read_only_connection(&path)?;
            let result = connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;
            if result != "ok" {
                return Err(StoreError::IntegrityCheckFailed);
            }
            let violations = connection.query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            if violations != 0 {
                return Err(StoreError::IntegrityCheckFailed);
            }
            Ok(result)
        })
        .await
        .map_err(|_| StoreError::ReaderTaskStopped)?
    }

    #[doc(hidden)]
    pub async fn attempt_write_for_test(&self) -> Result<(), StoreError> {
        let path = self.path.clone();
        let permit = Arc::clone(&self.gate)
            .acquire_owned()
            .await
            .map_err(|_| StoreError::ReaderTaskStopped)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let connection = open_read_only_connection(&path)?;
            connection
                .execute_batch("CREATE TABLE forbidden_reader_write(value INTEGER)")
                .map_err(map_sqlite)
        })
        .await
        .map_err(|_| StoreError::ReaderTaskStopped)?
    }
}

struct Writer {
    connection: Connection,
    options: StoreOptions,
    file_backed: bool,
    registry: InstrumentRegistry,
    health: StoreHealth,
    _lock_file: Option<File>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mutation_type", rename_all = "snake_case")]
enum CatalogMutationWire {
    Definition {
        definition: InstrumentDefinitionInput,
        known_at_ns: i64,
        source_reference: String,
    },
    Correction {
        supersedes_revision: u64,
        prior_definition_hash: [u8; 32],
        replacement: InstrumentDefinitionInput,
        known_at_ns: i64,
        source_reference: String,
        reason: String,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogRequestWire {
    actor: String,
    expected_revision: u64,
    mutations: Vec<CatalogMutationWire>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogAuditWire {
    request_hash: [u8; 32],
    previous_revision: u64,
    committed_revision: u64,
    appended_records: usize,
    idempotent_records: usize,
    catalog_digest: [u8; 32],
    history_digest: [u8; 32],
}

struct PersistedCatalogRequest {
    commit: CatalogCommit,
    payload: Vec<u8>,
    hash: [u8; 32],
}

fn run_writer(
    target: OpenTarget,
    options: StoreOptions,
    mut receiver: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<StartupReport, StoreError>>,
) {
    let mut writer = match Writer::open(target, options) {
        Ok((writer, report)) => {
            if ready.send(Ok(report)).is_err() {
                return;
            }
            writer
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    while let Some(command) = receiver.blocking_recv() {
        match command {
            Command::Migrate { reply } => {
                let _ = reply.send(migrations::migrate(&mut writer.connection));
            }
            Command::IntegrityCheck { reply } => {
                let _ = reply.send(writer.integrity_check());
            }
            Command::SqliteInfo { reply } => {
                let _ = reply.send(writer.sqlite_info());
            }
            Command::Checkpoint { mode, reply } => {
                let _ = reply.send(writer.checkpoint(mode));
            }
            Command::Health { reply } => {
                let _ = reply.send(Ok(writer.health));
            }
            Command::AppendAudit { event, reply } => {
                let _ = reply.send(writer.append_audit(event));
            }
            Command::AppendCatalog { request, reply } => {
                let _ = reply.send(writer.append_catalog(request));
            }
            Command::CatalogSnapshot { reply } => {
                let _ = reply.send(writer.catalog_snapshot());
            }
            #[cfg(feature = "test-support")]
            Command::DeleteAuditForTest { audit_id, reply } => {
                let result = writer
                    .connection
                    .execute("DELETE FROM audit_records WHERE audit_id = ?1", [audit_id])
                    .map(|_| ())
                    .map_err(map_sqlite);
                let _ = reply.send(result);
            }
            #[cfg(feature = "test-support")]
            Command::UpdateAuditForTest { audit_id, reply } => {
                let result = writer
                    .connection
                    .execute(
                        "UPDATE audit_records SET actor = actor WHERE audit_id = ?1",
                        [audit_id],
                    )
                    .map(|_| ())
                    .map_err(map_sqlite);
                let _ = reply.send(result);
            }
            Command::TableNamesForTest { reply } => {
                let _ = reply.send(writer.table_names());
            }
            #[cfg(feature = "test-support")]
            Command::ExhaustPageCapacityForTest { reply } => {
                let result = writer
                    .connection
                    .pragma_query_value(None, "page_count", |row| row.get::<_, i64>(0))
                    .and_then(|page_count| {
                        writer
                            .connection
                            .pragma_update(None, "max_page_count", page_count)
                    })
                    .map_err(map_sqlite);
                let _ = reply.send(result);
            }
            Command::Shutdown { reply } => {
                let result = writer.clean_shutdown();
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
                break;
            }
        }
    }
}

impl Writer {
    fn open(
        target: OpenTarget,
        options: StoreOptions,
    ) -> Result<(Self, StartupReport), StoreError> {
        let (connection, lock_file, file_backed) = match target {
            OpenTarget::File(path) => {
                let lock_file = acquire_process_lock(&path)?;
                prepare_database_file(&path)?;
                let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX
                    | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
                    | OpenFlags::SQLITE_OPEN_NOFOLLOW
                    | OpenFlags::SQLITE_OPEN_EXRESCODE;
                let connection = Connection::open_with_flags(&path, flags).map_err(map_sqlite)?;
                (connection, Some(lock_file), true)
            }
            OpenTarget::Memory => (
                Connection::open_in_memory().map_err(map_sqlite)?,
                None,
                false,
            ),
        };
        let mut writer = Self {
            connection,
            options,
            file_backed,
            registry: InstrumentRegistry::new(),
            health: StoreHealth::default(),
            _lock_file: lock_file,
        };
        writer.configure_session()?;
        writer.preflight_identity()?;
        let prior_runtime_state = writer.runtime_state_if_present()?;
        if prior_runtime_state.is_some_and(|state| !state.0) {
            writer.integrity_check()?;
        }
        migrations::migrate(&mut writer.connection)?;
        writer.verify_application_schema()?;
        writer.enable_wal()?;

        let (was_clean, generation) =
            prior_runtime_state.map_or_else(|| writer.load_runtime_state(), Ok)?;
        writer.verify_audit_chain()?;
        writer.registry = writer.rehydrate_catalog()?;
        let next_generation = generation.checked_add(1).ok_or(StoreError::IntegerRange)?;
        writer.connection.execute(
            "UPDATE runtime_state
             SET clean_shutdown = 0, startup_generation = ?1
             WHERE singleton = 1",
            [next_generation],
        )?;

        Ok((
            writer,
            StartupReport {
                recovered_unclean_shutdown: !was_clean,
                startup_generation: u64::try_from(next_generation)
                    .map_err(|_| StoreError::IntegerRange)?,
            },
        ))
    }

    fn configure_session(&self) -> Result<(), StoreError> {
        if rusqlite::version_number() < MINIMUM_SQLITE_VERSION_NUMBER {
            return Err(StoreError::UnsupportedSqliteVersion);
        }
        self.connection.busy_timeout(self.options.busy_timeout)?;
        self.connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true)?;
        self.connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
        self.connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
        self.connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)?;
        self.connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)?;
        self.connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_ATTACH_CREATE, false)?;
        self.connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_ATTACH_WRITE, false)?;
        self.connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        self.connection.pragma_update(None, "foreign_keys", true)?;
        self.connection
            .pragma_update(None, "trusted_schema", false)?;
        self.connection.pragma_update(None, "synchronous", "FULL")?;
        self.connection
            .pragma_update(None, "wal_autocheckpoint", 0)?;
        Ok(())
    }

    fn preflight_identity(&self) -> Result<(), StoreError> {
        let application_id = self
            .connection
            .pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))?;
        if application_id != 0 && application_id != APPLICATION_ID {
            return Err(StoreError::ApplicationIdMismatch);
        }
        if application_id == 0 {
            let object_count = self.connection.query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            if object_count != 0 {
                return Err(StoreError::UnclaimedNonEmptyDatabase);
            }
        }
        Ok(())
    }

    fn runtime_state_if_present(&self) -> Result<Option<(bool, i64)>, StoreError> {
        let exists = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_schema
                 WHERE type = 'table' AND name = 'runtime_state'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if exists {
            self.load_runtime_state().map(Some)
        } else {
            Ok(None)
        }
    }

    fn load_runtime_state(&self) -> Result<(bool, i64), StoreError> {
        self.connection
            .query_row(
                "SELECT clean_shutdown, startup_generation
                 FROM runtime_state WHERE singleton = 1",
                [],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(map_sqlite)
    }

    fn enable_wal(&self) -> Result<(), StoreError> {
        if self.file_backed {
            let mode =
                self.connection
                    .pragma_update_and_check(None, "journal_mode", "WAL", |row| {
                        row.get::<_, String>(0)
                    })?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StoreError::SchemaStateMismatch);
            }
        }
        Ok(())
    }

    fn verify_application_schema(&self) -> Result<(), StoreError> {
        let foreign_keys = self
            .connection
            .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))?;
        let trusted_schema = self
            .connection
            .pragma_query_value(None, "trusted_schema", |row| row.get::<_, bool>(0))?;
        if !foreign_keys || trusted_schema {
            return Err(StoreError::SchemaStateMismatch);
        }
        let application_id = self
            .connection
            .pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))?;
        if application_id != APPLICATION_ID {
            return Err(StoreError::ApplicationIdMismatch);
        }
        let violations = self.connection.query_row(
            "SELECT COUNT(*) FROM pragma_foreign_key_check",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if violations != 0 {
            return Err(StoreError::IntegrityCheckFailed);
        }
        if schema_digest(&self.connection)? != EXPECTED_SCHEMA_DIGEST {
            return Err(StoreError::SchemaStateMismatch);
        }
        Ok(())
    }

    fn integrity_check(&mut self) -> Result<String, StoreError> {
        let checked_at_ns = unix_time_ns()?;
        let outcome = (|| {
            let result = self
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;
            if result != "ok" {
                return Err(StoreError::IntegrityCheckFailed);
            }
            let violations = self.connection.query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            if violations != 0 {
                return Err(StoreError::IntegrityCheckFailed);
            }
            Ok(result)
        })();
        self.health.last_integrity_check_at_ns = Some(checked_at_ns);
        self.health.last_integrity_check_ok = Some(outcome.is_ok());
        outcome
    }

    fn sqlite_info(&self) -> Result<SqliteInfo, StoreError> {
        Ok(SqliteInfo {
            version: rusqlite::version().to_owned(),
            source_id: self
                .connection
                .query_row("SELECT sqlite_source_id()", [], |row| {
                    row.get::<_, String>(0)
                })?,
            journal_mode: self
                .connection
                .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?,
            synchronous: self
                .connection
                .pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))?,
            foreign_keys: self
                .connection
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))?,
            trusted_schema: self
                .connection
                .pragma_query_value(None, "trusted_schema", |row| row.get::<_, bool>(0))?,
            wal_autocheckpoint: self.connection.pragma_query_value(
                None,
                "wal_autocheckpoint",
                |row| row.get::<_, i64>(0),
            )?,
            application_id: self
                .connection
                .pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))?,
            user_version: self
                .connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
        })
    }

    fn checkpoint(&mut self, mode: CheckpointMode) -> Result<CheckpointReport, StoreError> {
        let checked_at_ns = unix_time_ns()?;
        let started = Instant::now();
        let outcome = self
            .connection
            .query_row(mode.sql(), [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(StoreError::from)
            .and_then(|(busy, log_frames, checkpointed_frames)| {
                Ok(CheckpointReport {
                    busy: busy != 0,
                    log_frames: u64::try_from(log_frames).map_err(|_| StoreError::IntegerRange)?,
                    checkpointed_frames: u64::try_from(checkpointed_frames)
                        .map_err(|_| StoreError::IntegerRange)?,
                    elapsed: started.elapsed(),
                })
            });
        self.health.last_checkpoint_at_ns = Some(checked_at_ns);
        self.health.last_checkpoint_ok = Some(outcome.as_ref().is_ok_and(|report| !report.busy));
        match outcome {
            Ok(report) => {
                self.health.last_checkpoint = Some(report);
                Ok(report)
            }
            Err(error) => Err(error),
        }
    }

    fn append_audit(&mut self, event: AuditEvent) -> Result<AuditRecord, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite)?;
        let record = append_audit_in_connection(
            &transaction,
            self.options.maximum_audit_payload_bytes,
            event,
        )?;
        transaction.commit().map_err(map_sqlite)?;
        Ok(record)
    }

    fn append_catalog(&mut self, request: CatalogRequest) -> Result<CatalogCommit, StoreError> {
        if request.mutations.is_empty() || request.mutations.len() > MAXIMUM_BATCH_RECORDS {
            return Err(StoreError::InvalidCatalogRequest);
        }
        let wire_mutations = request
            .mutations
            .iter()
            .map(CatalogMutationWire::from_mutation)
            .collect::<Result<Vec<_>, _>>()?;
        let request_payload = serde_json::to_vec(&CatalogRequestWire {
            actor: request.actor.clone(),
            expected_revision: revision_value(request.expected_revision),
            mutations: wire_mutations,
        })
        .map_err(|_| StoreError::InvalidCatalogRequest)?;
        if request_payload.is_empty() || request_payload.len() > MAXIMUM_CATALOG_PAYLOAD_BYTES {
            return Err(StoreError::InvalidCatalogRequest);
        }
        let request_hash = *blake3::hash(&request_payload).as_bytes();
        if let Some(existing) = load_catalog_request(&self.connection, &request.request_id)? {
            if existing.payload == request_payload && existing.hash == request_hash {
                return Ok(existing.commit);
            }
            return Err(StoreError::CatalogRequestConflict);
        }

        let record_offset = self.registry.records().len();
        let (staged, outcome) = self
            .registry
            .stage_batch(request.expected_revision, request.mutations)
            .map_err(StoreError::Catalog)?;
        let snapshot = staged
            .snapshot()
            .map_err(|_| StoreError::CatalogIntegrity)?;
        if !snapshot.verify_integrity() {
            return Err(StoreError::CatalogIntegrity);
        }
        let previous_revision = outcome.previous_revision();
        let committed_revision = outcome.committed_revision();
        let catalog_digest = *snapshot.catalog_digest();
        let history_digest = *snapshot.history_digest();
        let new_records = staged.records()[record_offset..].to_vec();
        if new_records.len() != outcome.appended_records() {
            return Err(StoreError::CatalogIntegrity);
        }
        let audit_payload = serde_json::to_vec(&CatalogAuditWire {
            request_hash,
            previous_revision: revision_value(previous_revision),
            committed_revision: revision_value(committed_revision),
            appended_records: outcome.appended_records(),
            idempotent_records: outcome.idempotent_records(),
            catalog_digest,
            history_digest,
        })
        .map_err(|_| StoreError::InvalidCatalogRequest)?;
        let audit_event = AuditEvent::try_new(
            format!("catalog:{}", request.request_id),
            unix_time_ns()?,
            request.actor,
            "instrument-catalog",
            "cmti.catalog.commit.v1",
            audit_payload,
        )?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite)?;
        if load_catalog_request(&transaction, &request.request_id)?.is_some() {
            return Err(StoreError::CatalogRequestConflict);
        }
        let audit = append_audit_in_connection(
            &transaction,
            self.options.maximum_audit_payload_bytes,
            audit_event,
        )?;
        let expected_revision = revision_to_i64(request.expected_revision)?;
        let result_revision = revision_to_i64(committed_revision)?;
        let audit_sequence = i64::try_from(audit.sequence).map_err(|_| StoreError::IntegerRange)?;
        let appended_records =
            i64::try_from(outcome.appended_records()).map_err(|_| StoreError::IntegerRange)?;
        let idempotent_records =
            i64::try_from(outcome.idempotent_records()).map_err(|_| StoreError::IntegerRange)?;
        transaction
            .execute(
                "INSERT INTO catalog_requests(
                     request_id, request_payload, request_hash, expected_revision,
                     result_revision, appended_records, idempotent_records,
                     catalog_digest, history_digest, audit_sequence
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    request.request_id,
                    request_payload,
                    request_hash.as_slice(),
                    expected_revision,
                    result_revision,
                    appended_records,
                    idempotent_records,
                    catalog_digest.as_slice(),
                    history_digest.as_slice(),
                    audit_sequence,
                ],
            )
            .map_err(map_sqlite)?;

        if !new_records.is_empty() {
            let revision = committed_revision.ok_or(StoreError::CatalogIntegrity)?;
            let revision_i64 = revision_to_i64(Some(revision))?;
            let previous_i64 = revision_to_i64(previous_revision)?;
            let known_at_ns = new_records
                .first()
                .ok_or(StoreError::CatalogIntegrity)?
                .metadata()
                .known_at()
                .value();
            transaction
                .execute(
                    "INSERT INTO catalog_commits(
                         revision, previous_revision, known_at_ns, record_count,
                         catalog_digest, history_digest, audit_sequence, request_id
                     ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        revision_i64,
                        previous_i64,
                        known_at_ns,
                        appended_records,
                        catalog_digest.as_slice(),
                        history_digest.as_slice(),
                        audit_sequence,
                        request.request_id,
                    ],
                )
                .map_err(map_sqlite)?;
            for (ordinal, record) in new_records.iter().enumerate() {
                if record.catalog_revision() != revision {
                    return Err(StoreError::CatalogIntegrity);
                }
                let payload = serde_json::to_vec(&CatalogMutationWire::from_record(record)?)
                    .map_err(|_| StoreError::CatalogIntegrity)?;
                if payload.is_empty() || payload.len() > MAXIMUM_CATALOG_PAYLOAD_BYTES {
                    return Err(StoreError::CatalogIntegrity);
                }
                let payload_hash = *blake3::hash(&payload).as_bytes();
                let ordinal = i64::try_from(ordinal).map_err(|_| StoreError::IntegerRange)?;
                transaction
                    .execute(
                        "INSERT INTO catalog_records(revision, ordinal, payload, payload_hash)
                         VALUES(?1, ?2, ?3, ?4)",
                        params![revision_i64, ordinal, payload, payload_hash.as_slice()],
                    )
                    .map_err(map_sqlite)?;
            }
            transaction
                .execute(
                    "UPDATE catalog_head
                     SET current_revision = ?1, catalog_digest = ?2, history_digest = ?3
                     WHERE singleton = 1",
                    params![
                        revision_i64,
                        catalog_digest.as_slice(),
                        history_digest.as_slice()
                    ],
                )
                .map_err(map_sqlite)?;
        }
        transaction.commit().map_err(map_sqlite)?;
        self.registry = staged;

        Ok(CatalogCommit {
            request_id: request.request_id,
            previous_revision,
            committed_revision,
            appended_records: outcome.appended_records(),
            idempotent_records: outcome.idempotent_records(),
            catalog_digest,
            history_digest,
            audit_sequence: audit.sequence,
        })
    }

    fn catalog_snapshot(&self) -> Result<Arc<CatalogSnapshot>, StoreError> {
        self.registry
            .snapshot()
            .map_err(|_| StoreError::CatalogEmpty)
    }

    fn rehydrate_catalog(&self) -> Result<InstrumentRegistry, StoreError> {
        verify_catalog_requests(&self.connection)?;
        let mut registry = InstrumentRegistry::new();
        let mut statement = self.connection.prepare(
            "SELECT revision, previous_revision, known_at_ns, record_count,
                    catalog_digest, history_digest, audit_sequence, request_id
             FROM catalog_commits ORDER BY revision",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let revision_i64 = row.get::<_, i64>(0)?;
            let previous_i64 = row.get::<_, i64>(1)?;
            let known_at_ns = row.get::<_, i64>(2)?;
            let record_count = row.get::<_, i64>(3)?;
            if !(1..=i64::try_from(MAXIMUM_BATCH_RECORDS).map_err(|_| StoreError::IntegerRange)?)
                .contains(&record_count)
            {
                return Err(StoreError::CatalogIntegrity);
            }
            let catalog_digest = catalog_hash_from_blob(row.get::<_, Vec<u8>>(4)?)?;
            let history_digest = catalog_hash_from_blob(row.get::<_, Vec<u8>>(5)?)?;
            let audit_sequence = row.get::<_, i64>(6)?;
            let request_id = row.get::<_, String>(7)?;
            let revision =
                catalog_revision_from_i64(revision_i64)?.ok_or(StoreError::CatalogIntegrity)?;
            let previous_revision = catalog_revision_from_i64(previous_i64)?;
            if previous_revision != registry.current_revision()
                || revision_value(Some(revision))
                    != revision_value(previous_revision)
                        .checked_add(1)
                        .ok_or(StoreError::IntegerRange)?
            {
                return Err(StoreError::CatalogIntegrity);
            }
            let mutations = load_catalog_records(
                &self.connection,
                revision_i64,
                usize::try_from(record_count).map_err(|_| StoreError::IntegerRange)?,
            )?;
            if mutations
                .iter()
                .any(|mutation| mutation_known_at(mutation) != known_at_ns)
            {
                return Err(StoreError::CatalogIntegrity);
            }
            let outcome = registry
                .append_batch(previous_revision, mutations)
                .map_err(|_| StoreError::CatalogIntegrity)?;
            if outcome.committed_revision() != Some(revision)
                || outcome.appended_records()
                    != usize::try_from(record_count).map_err(|_| StoreError::IntegerRange)?
            {
                return Err(StoreError::CatalogIntegrity);
            }
            let snapshot = registry
                .snapshot()
                .map_err(|_| StoreError::CatalogIntegrity)?;
            if !snapshot.verify_integrity()
                || snapshot.catalog_digest() != &catalog_digest
                || snapshot.history_digest() != &history_digest
            {
                return Err(StoreError::CatalogIntegrity);
            }
            let linked = self.connection.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM catalog_requests AS request
                     JOIN audit_records AS audit ON audit.sequence = request.audit_sequence
                     WHERE request.request_id = ?1
                       AND request.result_revision = ?2
                       AND request.audit_sequence = ?3
                       AND request.catalog_digest = ?4
                       AND request.history_digest = ?5
                 )",
                params![
                    request_id,
                    revision_i64,
                    audit_sequence,
                    catalog_digest.as_slice(),
                    history_digest.as_slice()
                ],
                |linked| linked.get::<_, bool>(0),
            )?;
            if !linked {
                return Err(StoreError::CatalogIntegrity);
            }
        }
        drop(rows);
        drop(statement);
        verify_catalog_head(&self.connection, &registry)?;
        Ok(registry)
    }

    fn verify_audit_chain(&self) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT sequence, audit_id, occurred_at_ns, actor, category,
                    payload_schema, payload, payload_hash, previous_hash, record_hash
             FROM audit_records ORDER BY sequence",
        )?;
        let mut rows = statement.query([])?;
        let mut expected_sequence = 1_u64;
        let mut expected_previous = ZERO_HASH;
        while let Some(row) = rows.next()? {
            let record = audit_record_from_row(row)?;
            if record.sequence != expected_sequence || record.previous_hash != expected_previous {
                return Err(StoreError::AuditIntegrity);
            }
            if *blake3::hash(&record.payload).as_bytes() != record.payload_hash {
                return Err(StoreError::AuditIntegrity);
            }
            let calculated = audit_record_hash(
                record.sequence,
                &record.audit_id,
                record.occurred_at_ns,
                &record.actor,
                &record.category,
                &record.payload_schema,
                &record.payload_hash,
                &record.previous_hash,
            );
            if calculated != record.record_hash {
                return Err(StoreError::AuditIntegrity);
            }
            expected_previous = record.record_hash;
            expected_sequence = expected_sequence
                .checked_add(1)
                .ok_or(StoreError::IntegerRange)?;
        }
        let (head_sequence, head_hash) = self.connection.query_row(
            "SELECT last_sequence, last_hash
             FROM audit_chain_head WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?;
        if u64::try_from(head_sequence).map_err(|_| StoreError::IntegerRange)?
            != expected_sequence - 1
            || hash_from_blob(head_hash)? != expected_previous
        {
            return Err(StoreError::AuditIntegrity);
        }
        Ok(())
    }

    fn table_names(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
    }

    fn clean_shutdown(&mut self) -> Result<(), StoreError> {
        if self.file_backed {
            let report = self.checkpoint(CheckpointMode::Truncate)?;
            if report.busy {
                return Err(StoreError::DatabaseBusy);
            }
        }
        self.connection.execute(
            "UPDATE runtime_state SET clean_shutdown = 1 WHERE singleton = 1",
            [],
        )?;
        Ok(())
    }
}

fn append_audit_in_connection(
    connection: &Connection,
    maximum_payload_bytes: usize,
    event: AuditEvent,
) -> Result<AuditRecord, StoreError> {
    if event.payload.len() > maximum_payload_bytes {
        return Err(StoreError::InvalidAuditEvent);
    }
    let payload_hash = *blake3::hash(&event.payload).as_bytes();
    if let Some(existing) = load_audit_by_id(connection, &event.audit_id)? {
        if existing.occurred_at_ns == event.occurred_at_ns
            && existing.actor == event.actor
            && existing.category == event.category
            && existing.payload_schema == event.payload_schema
            && existing.payload == event.payload
            && existing.payload_hash == payload_hash
        {
            return Ok(existing);
        }
        return Err(StoreError::AuditIdConflict);
    }

    let (last_sequence, previous_hash) = connection.query_row(
        "SELECT last_sequence, last_hash
         FROM audit_chain_head WHERE singleton = 1",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;
    let sequence = last_sequence
        .checked_add(1)
        .ok_or(StoreError::IntegerRange)?;
    let previous_hash = hash_from_blob(previous_hash)?;
    let sequence_u64 = u64::try_from(sequence).map_err(|_| StoreError::IntegerRange)?;
    let record_hash = audit_record_hash(
        sequence_u64,
        &event.audit_id,
        event.occurred_at_ns,
        &event.actor,
        &event.category,
        &event.payload_schema,
        &payload_hash,
        &previous_hash,
    );
    connection
        .execute(
            "INSERT INTO audit_records(
                 sequence, audit_id, occurred_at_ns, actor, category,
                 payload_schema, payload, payload_hash, previous_hash, record_hash
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                sequence,
                event.audit_id,
                event.occurred_at_ns,
                event.actor,
                event.category,
                event.payload_schema,
                event.payload,
                payload_hash.as_slice(),
                previous_hash.as_slice(),
                record_hash.as_slice(),
            ],
        )
        .map_err(map_sqlite)?;
    connection
        .execute(
            "UPDATE audit_chain_head
             SET last_sequence = ?1, last_hash = ?2
             WHERE singleton = 1",
            params![sequence, record_hash.as_slice()],
        )
        .map_err(map_sqlite)?;

    Ok(AuditRecord {
        sequence: sequence_u64,
        audit_id: event.audit_id,
        occurred_at_ns: event.occurred_at_ns,
        actor: event.actor,
        category: event.category,
        payload_schema: event.payload_schema,
        payload: event.payload,
        payload_hash,
        previous_hash,
        record_hash,
    })
}

impl CatalogMutationWire {
    fn from_mutation(mutation: &CatalogMutation) -> Result<Self, StoreError> {
        match mutation {
            CatalogMutation::Definition {
                definition,
                metadata,
            } => Ok(Self::Definition {
                definition: definition_input(definition),
                known_at_ns: metadata.known_at().value(),
                source_reference: metadata.source_reference().to_owned(),
            }),
            CatalogMutation::Correction(correction) => Ok(Self::Correction {
                supersedes_revision: correction.supersedes_revision().get(),
                prior_definition_hash: *correction.prior_definition_hash(),
                replacement: definition_input(correction.replacement()),
                known_at_ns: correction.metadata().known_at().value(),
                source_reference: correction.metadata().source_reference().to_owned(),
                reason: correction.reason().to_owned(),
            }),
        }
    }

    fn from_record(record: &CatalogRecord) -> Result<Self, StoreError> {
        match record {
            CatalogRecord::Definition(record) => Ok(Self::Definition {
                definition: definition_input(record.definition()),
                known_at_ns: record.metadata().known_at().value(),
                source_reference: record.metadata().source_reference().to_owned(),
            }),
            CatalogRecord::Correction(record) => Ok(Self::Correction {
                supersedes_revision: record.supersedes_revision().get(),
                prior_definition_hash: *record.prior_definition_hash(),
                replacement: definition_input(record.replacement()),
                known_at_ns: record.metadata().known_at().value(),
                source_reference: record.metadata().source_reference().to_owned(),
                reason: record.reason().to_owned(),
            }),
        }
    }

    fn into_mutation(self) -> Result<CatalogMutation, StoreError> {
        match self {
            Self::Definition {
                definition,
                known_at_ns,
                source_reference,
            } => Ok(CatalogMutation::Definition {
                definition: InstrumentDefinition::new(definition)
                    .map_err(|_| StoreError::CatalogIntegrity)?,
                metadata: RevisionMetadata::try_new(UnixNanos::new(known_at_ns), source_reference)
                    .map_err(|_| StoreError::CatalogIntegrity)?,
            }),
            Self::Correction {
                supersedes_revision,
                prior_definition_hash,
                replacement,
                known_at_ns,
                source_reference,
                reason,
            } => {
                let supersedes_revision = CatalogRevision::new(supersedes_revision)
                    .ok_or(StoreError::CatalogIntegrity)?;
                let replacement = InstrumentDefinition::new(replacement)
                    .map_err(|_| StoreError::CatalogIntegrity)?;
                let metadata =
                    RevisionMetadata::try_new(UnixNanos::new(known_at_ns), source_reference)
                        .map_err(|_| StoreError::CatalogIntegrity)?;
                Ok(CatalogMutation::Correction(
                    CorrectionInput::try_new(
                        supersedes_revision,
                        prior_definition_hash,
                        replacement,
                        metadata,
                        reason,
                    )
                    .map_err(|_| StoreError::CatalogIntegrity)?,
                ))
            }
        }
    }
}

fn definition_input(definition: &InstrumentDefinition) -> InstrumentDefinitionInput {
    InstrumentDefinitionInput {
        id: definition.id().clone(),
        product_type: definition.product_type(),
        base_asset: definition.base_asset().clone(),
        quote_asset: definition.quote_asset().clone(),
        settlement_asset: definition.settlement_asset().clone(),
        contract_multiplier: definition.contract_multiplier(),
        contract_value_unit: definition.contract_value_unit(),
        contract_kind: definition.contract_kind(),
        expiry_time: definition.expiry_time(),
        strike: definition.strike(),
        option_side: definition.option_side(),
        price_tick: definition.price_tick(),
        quantity_step: definition.quantity_step(),
        listing_time: definition.listing_time(),
        delisting_time: definition.delisting_time(),
    }
}

fn mutation_known_at(mutation: &CatalogMutation) -> i64 {
    match mutation {
        CatalogMutation::Definition { metadata, .. } => metadata.known_at().value(),
        CatalogMutation::Correction(correction) => correction.metadata().known_at().value(),
    }
}

fn revision_value(revision: Option<CatalogRevision>) -> u64 {
    revision.map_or(0, CatalogRevision::get)
}

fn revision_to_i64(revision: Option<CatalogRevision>) -> Result<i64, StoreError> {
    i64::try_from(revision_value(revision)).map_err(|_| StoreError::IntegerRange)
}

fn catalog_revision_from_i64(value: i64) -> Result<Option<CatalogRevision>, StoreError> {
    if value == 0 {
        return Ok(None);
    }
    let value = u64::try_from(value).map_err(|_| StoreError::CatalogIntegrity)?;
    CatalogRevision::new(value)
        .map(Some)
        .ok_or(StoreError::CatalogIntegrity)
}

fn catalog_hash_from_blob(blob: Vec<u8>) -> Result<[u8; 32], StoreError> {
    blob.try_into().map_err(|_| StoreError::CatalogIntegrity)
}

fn load_catalog_request(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<PersistedCatalogRequest>, StoreError> {
    connection
        .query_row(
            "SELECT request_payload, request_hash, expected_revision, result_revision,
                    appended_records, idempotent_records, catalog_digest,
                    history_digest, audit_sequence
             FROM catalog_requests WHERE request_id = ?1",
            [request_id],
            |row| {
                let expected_revision = row.get::<_, i64>(2)?;
                let result_revision = row.get::<_, i64>(3)?;
                let appended_records = row.get::<_, i64>(4)?;
                let idempotent_records = row.get::<_, i64>(5)?;
                let audit_sequence = row.get::<_, i64>(8)?;
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    expected_revision,
                    result_revision,
                    appended_records,
                    idempotent_records,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                    audit_sequence,
                ))
            },
        )
        .optional()
        .map_err(map_sqlite)?
        .map(
            |(
                payload,
                stored_hash,
                expected,
                result,
                appended,
                idempotent,
                catalog_digest,
                history_digest,
                audit_sequence,
            )| {
                let stored_hash = catalog_hash_from_blob(stored_hash)?;
                if *blake3::hash(&payload).as_bytes() != stored_hash {
                    return Err(StoreError::CatalogIntegrity);
                }
                let appended_records =
                    usize::try_from(appended).map_err(|_| StoreError::CatalogIntegrity)?;
                let idempotent_records =
                    usize::try_from(idempotent).map_err(|_| StoreError::CatalogIntegrity)?;
                if appended_records > MAXIMUM_BATCH_RECORDS
                    || idempotent_records > MAXIMUM_BATCH_RECORDS
                    || appended_records
                        .checked_add(idempotent_records)
                        .is_none_or(|count| count > MAXIMUM_BATCH_RECORDS)
                {
                    return Err(StoreError::CatalogIntegrity);
                }
                Ok(PersistedCatalogRequest {
                    commit: CatalogCommit {
                        request_id: request_id.to_owned(),
                        previous_revision: catalog_revision_from_i64(expected)?,
                        committed_revision: catalog_revision_from_i64(result)?,
                        appended_records,
                        idempotent_records,
                        catalog_digest: catalog_hash_from_blob(catalog_digest)?,
                        history_digest: catalog_hash_from_blob(history_digest)?,
                        audit_sequence: u64::try_from(audit_sequence)
                            .map_err(|_| StoreError::CatalogIntegrity)?,
                    },
                    payload,
                    hash: stored_hash,
                })
            },
        )
        .transpose()
}

fn load_catalog_records(
    connection: &Connection,
    revision: i64,
    expected_count: usize,
) -> Result<Vec<CatalogMutation>, StoreError> {
    if !(1..=MAXIMUM_BATCH_RECORDS).contains(&expected_count) {
        return Err(StoreError::CatalogIntegrity);
    }
    let mut statement = connection.prepare(
        "SELECT ordinal, payload, payload_hash
         FROM catalog_records WHERE revision = ?1 ORDER BY ordinal",
    )?;
    let mut rows = statement.query([revision])?;
    let mut mutations = Vec::with_capacity(expected_count);
    while let Some(row) = rows.next()? {
        let ordinal = row.get::<_, i64>(0)?;
        if usize::try_from(ordinal).map_err(|_| StoreError::CatalogIntegrity)? != mutations.len() {
            return Err(StoreError::CatalogIntegrity);
        }
        let payload = row.get::<_, Vec<u8>>(1)?;
        if payload.is_empty() || payload.len() > MAXIMUM_CATALOG_PAYLOAD_BYTES {
            return Err(StoreError::CatalogIntegrity);
        }
        let stored_hash = catalog_hash_from_blob(row.get::<_, Vec<u8>>(2)?)?;
        if *blake3::hash(&payload).as_bytes() != stored_hash {
            return Err(StoreError::CatalogIntegrity);
        }
        let wire = serde_json::from_slice::<CatalogMutationWire>(&payload)
            .map_err(|_| StoreError::CatalogIntegrity)?;
        mutations.push(wire.into_mutation()?);
    }
    if mutations.len() != expected_count {
        return Err(StoreError::CatalogIntegrity);
    }
    Ok(mutations)
}

fn verify_catalog_requests(connection: &Connection) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT request_id, request_payload, request_hash, expected_revision,
                result_revision, appended_records, idempotent_records,
                catalog_digest, history_digest, audit_sequence
         FROM catalog_requests ORDER BY rowid",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let request_id = row.get::<_, String>(0)?;
        let payload = row.get::<_, Vec<u8>>(1)?;
        let request_hash = catalog_hash_from_blob(row.get::<_, Vec<u8>>(2)?)?;
        let expected_revision = row.get::<_, i64>(3)?;
        let result_revision = row.get::<_, i64>(4)?;
        let appended_records = row.get::<_, i64>(5)?;
        let idempotent_records = row.get::<_, i64>(6)?;
        let catalog_digest = catalog_hash_from_blob(row.get::<_, Vec<u8>>(7)?)?;
        let history_digest = catalog_hash_from_blob(row.get::<_, Vec<u8>>(8)?)?;
        let audit_sequence = row.get::<_, i64>(9)?;
        let request_wire = serde_json::from_slice::<CatalogRequestWire>(&payload)
            .map_err(|_| StoreError::CatalogIntegrity)?;
        if payload.is_empty()
            || payload.len() > MAXIMUM_CATALOG_PAYLOAD_BYTES
            || *blake3::hash(&payload).as_bytes() != request_hash
            || u64::try_from(expected_revision).ok() != Some(request_wire.expected_revision)
            || request_wire.actor.is_empty()
            || request_wire.mutations.is_empty()
            || result_revision <= 0
            || appended_records < 0
            || idempotent_records < 0
        {
            return Err(StoreError::CatalogIntegrity);
        }
        let audit_payload = connection
            .query_row(
                "SELECT audit.payload
                 FROM audit_records AS audit
                 JOIN catalog_commits AS catalog_commit ON catalog_commit.revision = ?2
                 WHERE audit.sequence = ?1
                   AND audit.audit_id = ?3
                   AND audit.category = 'instrument-catalog'
                   AND audit.payload_schema = 'cmti.catalog.commit.v1'
                   AND catalog_commit.catalog_digest = ?4
                   AND catalog_commit.history_digest = ?5",
                params![
                    audit_sequence,
                    result_revision,
                    format!("catalog:{request_id}"),
                    catalog_digest.as_slice(),
                    history_digest.as_slice()
                ],
                |audit| audit.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let audit_payload = audit_payload.ok_or(StoreError::CatalogIntegrity)?;
        let audit = serde_json::from_slice::<CatalogAuditWire>(&audit_payload)
            .map_err(|_| StoreError::CatalogIntegrity)?;
        if audit.request_hash != request_hash
            || audit.previous_revision
                != u64::try_from(expected_revision).map_err(|_| StoreError::CatalogIntegrity)?
            || audit.committed_revision
                != u64::try_from(result_revision).map_err(|_| StoreError::CatalogIntegrity)?
            || audit.appended_records
                != usize::try_from(appended_records).map_err(|_| StoreError::CatalogIntegrity)?
            || audit.idempotent_records
                != usize::try_from(idempotent_records).map_err(|_| StoreError::CatalogIntegrity)?
            || audit.catalog_digest != catalog_digest
            || audit.history_digest != history_digest
        {
            return Err(StoreError::CatalogIntegrity);
        }
    }
    Ok(())
}

fn verify_catalog_head(
    connection: &Connection,
    registry: &InstrumentRegistry,
) -> Result<(), StoreError> {
    let (revision, catalog_digest, history_digest) = connection.query_row(
        "SELECT current_revision, catalog_digest, history_digest
         FROM catalog_head WHERE singleton = 1",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;
    if catalog_revision_from_i64(revision)? != registry.current_revision() {
        return Err(StoreError::CatalogIntegrity);
    }
    let catalog_digest = catalog_hash_from_blob(catalog_digest)?;
    let history_digest = catalog_hash_from_blob(history_digest)?;
    if registry.current_revision().is_none() {
        if catalog_digest != ZERO_HASH || history_digest != ZERO_HASH {
            return Err(StoreError::CatalogIntegrity);
        }
        return Ok(());
    }
    let snapshot = registry
        .snapshot()
        .map_err(|_| StoreError::CatalogIntegrity)?;
    if !snapshot.verify_integrity()
        || snapshot.catalog_digest() != &catalog_digest
        || snapshot.history_digest() != &history_digest
    {
        return Err(StoreError::CatalogIntegrity);
    }
    Ok(())
}

fn load_audit_by_id(
    connection: &Connection,
    audit_id: &str,
) -> Result<Option<AuditRecord>, StoreError> {
    connection
        .query_row(
            "SELECT sequence, audit_id, occurred_at_ns, actor, category,
                    payload_schema, payload, payload_hash, previous_hash, record_hash
             FROM audit_records WHERE audit_id = ?1",
            [audit_id],
            audit_record_from_row,
        )
        .optional()
        .map_err(map_sqlite)
}

fn audit_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditRecord> {
    let sequence = row.get::<_, i64>(0)?;
    let payload_hash = row.get::<_, Vec<u8>>(7)?;
    let previous_hash = row.get::<_, Vec<u8>>(8)?;
    let record_hash = row.get::<_, Vec<u8>>(9)?;
    Ok(AuditRecord {
        sequence: u64::try_from(sequence).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        audit_id: row.get(1)?,
        occurred_at_ns: row.get(2)?,
        actor: row.get(3)?,
        category: row.get(4)?,
        payload_schema: row.get(5)?,
        payload: row.get(6)?,
        payload_hash: blob_to_hash(payload_hash, 7)?,
        previous_hash: blob_to_hash(previous_hash, 8)?,
        record_hash: blob_to_hash(record_hash, 9)?,
    })
}

fn blob_to_hash(blob: Vec<u8>, column: usize) -> rusqlite::Result<[u8; 32]> {
    blob.try_into().map_err(|value: Vec<u8>| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Blob,
            format!("expected 32-byte hash, received {} bytes", value.len()).into(),
        )
    })
}

fn hash_from_blob(blob: Vec<u8>) -> Result<[u8; 32], StoreError> {
    blob.try_into().map_err(|_| StoreError::AuditIntegrity)
}

fn schema_digest(connection: &Connection) -> Result<[u8; 32], StoreError> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, sql
         FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite_%'
         ORDER BY type, name",
    )?;
    let mut rows = statement.query([])?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(SCHEMA_HASH_DOMAIN);
    while let Some(row) = rows.next()? {
        for column in 0..3 {
            let value = row.get::<_, String>(column)?;
            update_bounded_bytes(&mut hasher, value.as_bytes());
        }
        let sql = row.get::<_, Option<String>>(3)?;
        match sql {
            Some(sql) => {
                hasher.update(&[1]);
                update_bounded_bytes(&mut hasher, sql.as_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        };
    }
    Ok(*hasher.finalize().as_bytes())
}

#[allow(clippy::too_many_arguments)]
fn audit_record_hash(
    sequence: u64,
    audit_id: &str,
    occurred_at_ns: i64,
    actor: &str,
    category: &str,
    payload_schema: &str,
    payload_hash: &[u8; 32],
    previous_hash: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(AUDIT_HASH_DOMAIN);
    hasher.update(&APPLICATION_ID.to_be_bytes());
    hasher.update(&sequence.to_be_bytes());
    hasher.update(&occurred_at_ns.to_be_bytes());
    update_bounded_text(&mut hasher, audit_id);
    update_bounded_text(&mut hasher, actor);
    update_bounded_text(&mut hasher, category);
    update_bounded_text(&mut hasher, payload_schema);
    hasher.update(payload_hash);
    hasher.update(previous_hash);
    *hasher.finalize().as_bytes()
}

fn update_bounded_text(hasher: &mut blake3::Hasher, value: &str) {
    update_bounded_bytes(hasher, value.as_bytes());
}

fn update_bounded_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("bounded input length fits in u64");
    hasher.update(&length.to_be_bytes());
    hasher.update(value);
}

fn valid_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_TEXT_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn open_read_only_connection(path: &Path) -> Result<Connection, StoreError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_EXRESCODE;
    let connection = Connection::open_with_flags(path, flags).map_err(map_sqlite)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_ATTACH_CREATE, false)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_ATTACH_WRITE, false)?;
    connection.pragma_update(None, "query_only", true)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "trusted_schema", false)?;
    let application_id =
        connection.pragma_query_value(None, "application_id", |row| row.get::<_, i64>(0))?;
    if application_id != APPLICATION_ID || schema_digest(&connection)? != EXPECTED_SCHEMA_DIGEST {
        return Err(StoreError::SchemaStateMismatch);
    }
    Ok(connection)
}

fn validate_database_path(path: &Path) -> Result<PathBuf, StoreError> {
    if !path.is_absolute() {
        return Err(StoreError::UnsafeDatabasePath);
    }
    let parent = path.parent().ok_or(StoreError::UnsafeDatabasePath)?;
    let parent_metadata = parent.symlink_metadata()?;
    if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
        return Err(StoreError::UnsafeDatabasePath);
    }
    validate_private_directory(&parent_metadata)?;
    if let Ok(metadata) = path.symlink_metadata()
        && (!metadata.is_file()
            || metadata.file_type().is_symlink()
            || !private_regular_file(&metadata))
    {
        return Err(StoreError::UnsafeDatabasePath);
    }
    let file_name = path.file_name().ok_or(StoreError::UnsafeDatabasePath)?;
    Ok(parent.canonicalize()?.join(file_name))
}

fn acquire_process_lock(database_path: &Path) -> Result<File, StoreError> {
    let lock_path = database_path.with_extension(database_path.extension().map_or_else(
        || "lock".into(),
        |extension| {
            let mut value = extension.to_os_string();
            value.push(".lock");
            value
        },
    ));
    let descriptor = open(
        &lock_path,
        OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::LOOP {
            StoreError::UnsafeDatabasePath
        } else {
            StoreError::Io(io::Error::from_raw_os_error(error.raw_os_error()))
        }
    })?;
    let file = File::from(descriptor);
    if !private_regular_file(&file.metadata()?) {
        return Err(StoreError::UnsafeDatabasePath);
    }
    fchmod(&file, Mode::RUSR | Mode::WUSR)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(error) if error == rustix::io::Errno::WOULDBLOCK => Err(StoreError::StoreAlreadyLocked),
        Err(error) => Err(StoreError::Io(io::Error::from_raw_os_error(
            error.raw_os_error(),
        ))),
    }
}

fn prepare_database_file(path: &Path) -> Result<(), StoreError> {
    if path.exists() {
        return Ok(());
    }
    let descriptor = open(
        path,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    fchmod(&descriptor, Mode::RUSR | Mode::WUSR)
        .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = File::from(descriptor);
    if !private_regular_file(&file.metadata()?) {
        return Err(StoreError::UnsafeDatabasePath);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_directory(metadata: &std::fs::Metadata) -> Result<(), StoreError> {
    use std::os::unix::fs::MetadataExt;

    if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(StoreError::UnsafeDatabasePath);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_directory(_metadata: &std::fs::Metadata) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
fn private_regular_file(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.is_file()
        && metadata.nlink() == 1
        && metadata.mode() & 0o077 == 0
        && metadata.uid() == rustix::process::geteuid().as_raw()
}

#[cfg(not(unix))]
fn private_regular_file(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

fn unix_time_ns() -> Result<i64, StoreError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::SystemClockBeforeEpoch)?;
    i64::try_from(elapsed.as_nanos()).map_err(|_| StoreError::SystemTimeOverflow)
}

#[cfg(test)]
mod tests {
    use super::schema_digest;
    use crate::migrations;

    #[test]
    fn initial_schema_digest_is_frozen() {
        let mut connection = rusqlite::Connection::open_in_memory().expect("open SQLite");
        migrations::migrate(&mut connection).expect("apply initial migration");
        assert_eq!(
            hex::encode(schema_digest(&connection).expect("hash schema")),
            "bc3fb8c364786ade836f7d7ef07461c7a754737f0a02d0a72bd1607903eaa1cb"
        );
    }
}
