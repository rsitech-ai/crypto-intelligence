use std::{
    fs::File,
    io::{self, Read},
    net::SocketAddr,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use connector_binance::{ParseError, parse_fixture_line};
use domain::{AssetId, AssetNamespace, InstrumentId, SourceId, UnixNanos};
use event_envelope::UncheckedEventPayload;
use fixed_decimal::Price;
use local_api::{
    auth::SessionSecret,
    proto::market_v1::SnapshotHealth,
    server::{LoopbackServer, MarketSnapshot, ServerError, ServerLimits, SnapshotError},
    session::SessionDescriptor,
};
use observability::{
    Component, DiagnosticsSnapshot, LocalJsonLog, LocalLogLevel, LocalTracing, MetricKey,
    MetricName, Metrics, ObservabilityError, ObservabilityHandle, Outcome, Venue,
    init_local_tracing,
};
use orderbook::{BookError, OrderBook};
use rand::{RngCore, rngs::OsRng};
use raw_wal::{
    frame::RecordMetadata,
    manager::{ManagerError, RotationPolicy, SegmentedWalWriter},
    prologue::{PrologueError, SegmentMetadata, StreamDescriptor},
    seal::CompressionJob,
};
use serde::Serialize;
use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::{JoinError, JoinHandle},
};

const MAX_FIXTURE_BYTES: usize = 1024 * 1024;
const EXPECTED_FINAL_SEQUENCE: u64 = 102;
const EXPECTED_BEST_BID: &str = "60000.1";
const EXPECTED_BEST_ASK: &str = "60000.2";

pub struct RuntimeOptions {
    pub fixture: File,
    pub wal_directory: File,
    pub wal_path: PathBuf,
    pub wal_policy: RotationPolicy,
    pub log: LocalJsonLog,
    pub secret: SessionSecret,
    pub descriptor: SessionDescriptor,
    pub cancellation: watch::Receiver<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeLimits {
    ingestion_queue_capacity: usize,
    server: ServerLimits,
    log_level: LocalLogLevel,
}

impl RuntimeLimits {
    pub fn new(
        ingestion_queue_capacity: usize,
        maximum_request_bytes: usize,
        maximum_concurrent_requests: usize,
        request_timeout: std::time::Duration,
        shutdown_grace: std::time::Duration,
        log_level: LocalLogLevel,
    ) -> Result<Self, RuntimeError> {
        if ingestion_queue_capacity == 0 {
            return Err(RuntimeError::InvalidQueueCapacity);
        }
        Ok(Self {
            ingestion_queue_capacity,
            server: ServerLimits::new(
                maximum_request_bytes,
                maximum_concurrent_requests,
                request_timeout,
                shutdown_grace,
            )?,
            log_level,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedSnapshot {
    source: SourceId,
    instrument: InstrumentId,
    sequence: u64,
    best_bid: Price,
    best_ask: Price,
    event_timestamp: UnixNanos,
    receive_timestamp: UnixNanos,
    freshness_millis: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeHealth {
    Initializing,
    Healthy,
    Degraded,
}

pub trait WalSink {
    fn append_and_sync(&mut self, payload: &[u8]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}

pub struct IngestionEngine<W> {
    wal: W,
    metrics: Metrics,
    order_book: OrderBook,
    published: Option<PublishedSnapshot>,
    source_health: RuntimeHealth,
}

impl<W: WalSink> IngestionEngine<W> {
    pub fn new(wal: W, metrics: Metrics) -> Self {
        Self {
            wal,
            metrics,
            order_book: OrderBook::new(10_000),
            published: None,
            source_health: RuntimeHealth::Initializing,
        }
    }

    pub fn process_new(&mut self, raw: &[u8]) -> Result<(), RuntimeError> {
        let wal_started = Instant::now();
        if let Err(source) = self.wal.append_and_sync(raw) {
            self.reject()?;
            return Err(RuntimeError::Persistence(source));
        }
        self.metrics
            .observe_seconds(wal_fsync_key(), wal_started.elapsed().as_secs_f64())?;
        self.metrics.increment(
            ingestion_bytes_key(),
            u64::try_from(raw.len()).map_err(|_| RuntimeError::MetricValueOverflow)?,
        )?;
        self.process_persisted(raw)
    }

    pub fn process_persisted(&mut self, raw: &[u8]) -> Result<(), RuntimeError> {
        let normalization_started = Instant::now();
        let event = match parse_fixture_line(raw) {
            Ok(event) => event,
            Err(source) => {
                self.metrics.increment(parser_failures_key(), 1)?;
                self.reject()?;
                return Err(RuntimeError::Parse(source));
            }
        };
        let metadata = event.metadata().as_unchecked();
        let source = metadata.source.clone();
        let instrument = metadata
            .instrument_id
            .clone()
            .ok_or(RuntimeError::MissingInstrument)?;
        let event_timestamp = metadata
            .exchange_timestamp
            .ok_or(RuntimeError::MissingTimestamp)?;
        let receive_timestamp = metadata.receive_wall_timestamp;
        let now_monotonic_ns = metadata.receive_monotonic_ns;

        let apply_result = match event.payload().as_unchecked() {
            UncheckedEventPayload::BookSnapshot(snapshot) => {
                self.order_book.apply_snapshot(snapshot, now_monotonic_ns)
            }
            UncheckedEventPayload::BookDelta(delta) => {
                self.order_book.apply_delta(delta, now_monotonic_ns)
            }
            _ => Err(BookError::Invalid),
        };
        if let Err(source) = apply_result {
            match source {
                BookError::Gap => self.metrics.increment(sequence_gaps_key(), 1)?,
                BookError::Checksum => self.metrics.increment(checksum_failures_key(), 1)?,
                _ => {}
            }
            self.reject()?;
            return Err(RuntimeError::Book(source));
        }

        if self.source_health != RuntimeHealth::Degraded {
            let best_bid = self
                .order_book
                .best_bid()
                .ok_or(RuntimeError::MissingBestPrices)?;
            let best_ask = self
                .order_book
                .best_ask()
                .ok_or(RuntimeError::MissingBestPrices)?;
            let freshness_millis = receive_timestamp
                .value()
                .saturating_sub(event_timestamp.value())
                .unsigned_abs()
                / 1_000_000;
            self.published = Some(PublishedSnapshot {
                source,
                instrument,
                sequence: self.order_book.sequence(),
                best_bid,
                best_ask,
                event_timestamp,
                receive_timestamp,
                freshness_millis,
            });
            self.source_health = RuntimeHealth::Healthy;
        }
        self.metrics.increment(received_key(), 1)?;
        self.metrics.observe_seconds(
            event_to_normalized_key(),
            normalization_started.elapsed().as_secs_f64(),
        )?;
        Ok(())
    }

    pub fn published(&self) -> Option<&PublishedSnapshot> {
        self.published.as_ref()
    }

    pub const fn source_health(&self) -> RuntimeHealth {
        self.source_health
    }

    fn reject(&mut self) -> Result<(), RuntimeError> {
        self.source_health = RuntimeHealth::Degraded;
        self.metrics.increment(rejected_key(), 1)?;
        Ok(())
    }

    fn into_wal(self) -> W {
        self.wal
    }
}

struct SegmentSink {
    writer: SegmentedWalWriter,
    clock: Instant,
    next_sequence: u64,
    pending_compression_jobs: Vec<CompressionJob>,
}

impl WalSink for SegmentSink {
    fn append_and_sync(&mut self, payload: &[u8]) -> io::Result<()> {
        let following_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("WAL record sequence overflow"))?;
        let receive_monotonic_time_ns = u64::try_from(self.clock.elapsed().as_nanos())
            .map_err(|_| io::Error::other("monotonic timestamp overflow"))?;
        let receive_wall_time_ns = current_wall_time_ns()?;
        let metadata = RecordMetadata {
            flags: 0,
            stream_id: 7,
            connection_epoch: 1,
            record_sequence: self.next_sequence,
            receive_wall_time_ns,
            receive_monotonic_time_ns,
        };
        let outcome = self
            .writer
            .append(
                metadata,
                payload,
                receive_monotonic_time_ns,
                receive_wall_time_ns,
            )
            .map_err(manager_io_error)?;
        if let Some(job) = outcome.compression_job() {
            self.pending_compression_jobs.push(job.clone());
        }
        self.next_sequence = following_sequence;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.writer.sync().map_err(manager_io_error)
    }
}

impl SegmentSink {
    fn poll_rotation(&mut self) -> io::Result<()> {
        let now_monotonic_ns = u64::try_from(self.clock.elapsed().as_nanos())
            .map_err(|_| io::Error::other("monotonic timestamp overflow"))?;
        let job = self
            .writer
            .poll_rotation(now_monotonic_ns, current_wall_time_ns()?)
            .map_err(manager_io_error)?;
        if let Some(job) = job {
            self.pending_compression_jobs.push(job);
        }
        Ok(())
    }
}

pub struct RunningDaemon {
    server: LoopbackServer,
    wal: SegmentSink,
    tracing: LocalTracing,
    readiness: Readiness,
    observability: ObservabilityHandle,
}

impl RunningDaemon {
    pub const fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    pub fn diagnostics_snapshot(
        &self,
        captured_at_unix_nanos: i64,
    ) -> Result<DiagnosticsSnapshot, ObservabilityError> {
        self.observability.snapshot(captured_at_unix_nanos)
    }

    pub fn poll_wal_rotation(&mut self) -> Result<(), RuntimeError> {
        self.wal.poll_rotation().map_err(RuntimeError::Persistence)
    }

    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        let server_result = self.server.shutdown().await;
        let wal_result = self.wal.sync();
        self.tracing.with_default(|| {
            tracing::info!(
                event = "fixture_runtime_stopped",
                component = "runtime",
                sequence = EXPECTED_FINAL_SEQUENCE
            );
        });
        let log_shutdown_result = self.tracing.shutdown();

        server_result?;
        wal_result.map_err(RuntimeError::Persistence)?;
        log_shutdown_result?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Readiness {
    pub endpoint: String,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub daemon_pid: u32,
    pub process_nonce: String,
    pub server_nonce: String,
    pub issued_unix_seconds: i64,
    pub expiry_unix_seconds: i64,
}

impl Readiness {
    fn new(local_addr: SocketAddr, descriptor: &SessionDescriptor) -> Self {
        Self {
            endpoint: format!("http://{local_addr}"),
            protocol_major: descriptor.protocol_major(),
            protocol_minor: descriptor.protocol_minor(),
            daemon_pid: descriptor.daemon_pid(),
            process_nonce: hex::encode(descriptor.process_nonce()),
            server_nonce: hex::encode(descriptor.server_nonce()),
            issued_unix_seconds: descriptor.issued_unix_seconds(),
            expiry_unix_seconds: descriptor.expiry_unix_seconds(),
        }
    }
}

pub async fn start_fixture_runtime_with_limits(
    options: RuntimeOptions,
    limits: RuntimeLimits,
) -> Result<RunningDaemon, RuntimeError> {
    let RuntimeOptions {
        fixture,
        wal_directory,
        wal_path,
        wal_policy,
        log,
        secret,
        descriptor,
        mut cancellation,
    } = options;
    let tracing = init_local_tracing(log, limits.log_level);
    let fixture_records = read_fixture_records(fixture)?;
    if is_cancelled(&cancellation) {
        cancel_before_wal_startup(tracing)?;
        return Err(RuntimeError::Cancelled);
    }
    let clock = Instant::now();
    let initial_metadata = initial_segment_metadata()?;
    let mut writer = SegmentedWalWriter::open_or_create_in(
        wal_directory,
        &wal_path,
        initial_metadata,
        wal_policy,
        0,
    )?;
    if is_cancelled(&cancellation) {
        cancel_startup(
            SegmentSink {
                writer,
                clock,
                next_sequence: 1,
                pending_compression_jobs: Vec::new(),
            },
            tracing,
        )?;
        return Err(RuntimeError::Cancelled);
    }
    let mut recovered_count = 0_usize;
    let mut recovery_mismatch = false;
    writer.visit_records(|record| {
        let payload_matches = fixture_records
            .get(recovered_count)
            .is_some_and(|expected| record.payload() == expected);
        let expected_sequence = u64::try_from(recovered_count)
            .ok()
            .and_then(|count| count.checked_add(1));
        let metadata_matches = record.metadata().is_some_and(|metadata| {
            metadata.flags == 0
                && metadata.stream_id == 7
                && metadata.connection_epoch == 1
                && Some(metadata.record_sequence) == expected_sequence
        });
        recovery_mismatch |= !payload_matches || !metadata_matches;
        recovered_count = recovered_count.saturating_add(1);
    })?;
    if recovery_mismatch || recovered_count > fixture_records.len() {
        return Err(RuntimeError::RecoveryMismatch);
    }

    let observability = ObservabilityHandle::default();
    let metrics = observability.metrics().clone();
    initialize_runtime_metrics(&metrics)?;
    let next_sequence = u64::try_from(recovered_count)
        .ok()
        .and_then(|count| count.checked_add(1))
        .ok_or(RuntimeError::RecordSequenceOverflow)?;
    let pending_compression_jobs = writer.take_recovered_compression_jobs()?;
    let mut engine = IngestionEngine::new(
        SegmentSink {
            writer,
            clock,
            next_sequence,
            pending_compression_jobs,
        },
        metrics.clone(),
    );
    for record in fixture_records.iter().take(recovered_count) {
        engine.process_persisted(record)?;
    }
    if is_cancelled(&cancellation) {
        cancel_startup(engine.into_wal(), tracing)?;
        return Err(RuntimeError::Cancelled);
    }

    let remaining = fixture_records
        .into_iter()
        .skip(recovered_count)
        .collect::<Vec<_>>();
    let (sender, mut receiver) = ingestion_channel_with_capacity(limits.ingestion_queue_capacity);
    let producer = spawn_fixture_producer(sender, remaining, cancellation.clone(), metrics.clone());
    let mut cancelled = false;
    loop {
        tokio::select! {
            biased;
            changed = cancellation.changed(), if !cancelled => {
                let _ = changed;
                cancelled = true;
                receiver.close();
            }
            record = receiver.recv() => match record {
                Some(record) => {
                    metrics.gauge(
                        queue_depth_key(),
                        i64::try_from(receiver.len())
                            .map_err(|_| RuntimeError::MetricValueOverflow)?,
                    )?;
                    engine.process_new(&record)?;
                }
                None => break,
            }
        }
    }
    producer.await??;
    metrics.gauge(queue_depth_key(), 0)?;
    if cancelled || is_cancelled(&cancellation) {
        cancel_startup(engine.into_wal(), tracing)?;
        return Err(RuntimeError::Cancelled);
    }

    let published = engine
        .published()
        .cloned()
        .ok_or(RuntimeError::MissingSnapshot)?;
    validate_final_snapshot(&published, engine.source_health())?;
    let market_snapshot = MarketSnapshot::new(
        published.source,
        published.instrument,
        AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1)
            .expect("the fixed BTC fixture identity must be valid"),
        published.sequence,
        published.best_bid,
        published.best_ask,
        SnapshotHealth::Healthy,
        published.event_timestamp,
        published.receive_timestamp,
        published.freshness_millis,
    )?;
    let wal = engine.into_wal();
    let server = LoopbackServer::spawn_with_limits(
        "127.0.0.1:0"
            .parse()
            .expect("constant loopback bind must parse"),
        secret,
        descriptor.clone(),
        market_snapshot,
        limits.server,
    )
    .await?;
    let readiness = Readiness::new(server.local_addr(), &descriptor);
    let pending_wal_compression_jobs = u64::try_from(wal.pending_compression_jobs.len())
        .expect("pending job count fits the supported 64-bit platform");
    tracing.with_default(|| {
        tracing::info!(
            event = "fixture_runtime_ready",
            component = "runtime",
            source_id = "binance-fixture",
            instrument_id = "BTCUSDT",
            sequence = EXPECTED_FINAL_SEQUENCE,
            pending_wal_compression_jobs = pending_wal_compression_jobs
        );
    });
    tracing.sync()?;

    let diagnostics_timestamp = descriptor
        .issued_unix_seconds()
        .checked_mul(1_000_000_000)
        .ok_or(RuntimeError::MetricValueOverflow)?;
    let running = RunningDaemon {
        server,
        wal,
        tracing,
        readiness,
        observability,
    };
    running.diagnostics_snapshot(diagnostics_timestamp)?;
    Ok(running)
}

pub fn ingestion_channel_with_capacity(
    capacity: usize,
) -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    mpsc::channel(capacity)
}

fn spawn_fixture_producer(
    sender: mpsc::Sender<Vec<u8>>,
    records: Vec<Vec<u8>>,
    mut cancellation: watch::Receiver<bool>,
    metrics: Metrics,
) -> JoinHandle<Result<(), RuntimeError>> {
    tokio::spawn(async move {
        for record in records {
            tokio::select! {
                biased;
                changed = cancellation.changed() => {
                    let _ = changed;
                    return Ok(());
                }
                result = sender.send(record) => {
                    if result.is_err() && !is_cancelled(&cancellation) {
                        return Err(RuntimeError::QueueClosed);
                    }
                    metrics.gauge(
                        queue_depth_key(),
                        i64::try_from(sender.max_capacity().saturating_sub(sender.capacity()))
                            .map_err(|_| RuntimeError::MetricValueOverflow)?,
                    )?;
                }
            }
        }
        Ok(())
    })
}

fn is_cancelled(cancellation: &watch::Receiver<bool>) -> bool {
    *cancellation.borrow()
}

fn cancel_startup(mut wal: SegmentSink, tracing: LocalTracing) -> Result<(), RuntimeError> {
    wal.sync().map_err(RuntimeError::Persistence)?;
    tracing.with_default(|| {
        tracing::info!(event = "fixture_runtime_cancelled", component = "runtime");
    });
    tracing.shutdown()?;
    Ok(())
}

fn cancel_before_wal_startup(tracing: LocalTracing) -> Result<(), RuntimeError> {
    tracing.with_default(|| {
        tracing::info!(event = "fixture_runtime_cancelled", component = "runtime");
    });
    tracing.shutdown()?;
    Ok(())
}

fn initial_segment_metadata() -> Result<SegmentMetadata, RuntimeError> {
    let mut segment_id = [0_u8; 16];
    OsRng
        .try_fill_bytes(&mut segment_id)
        .map_err(|_| RuntimeError::Random)?;
    if segment_id == [0_u8; 16] {
        return Err(RuntimeError::Random);
    }
    SegmentMetadata::new(
        segment_id,
        current_wall_time_ns()?,
        "fixture-json-v1",
        "foundation-fixture",
        concat!("cryptoriskd-", env!("CARGO_PKG_VERSION")),
        vec![StreamDescriptor::new(7, "binance", "spot-btcusdt")?],
    )
    .map_err(RuntimeError::Prologue)
}

fn current_wall_time_ns() -> io::Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| io::Error::other("system clock precedes the Unix epoch"))?;
    i64::try_from(elapsed.as_nanos()).map_err(|_| io::Error::other("wall timestamp overflow"))
}

fn read_fixture_records(mut fixture: File) -> Result<Vec<Vec<u8>>, RuntimeError> {
    let mut contents = Vec::new();
    fixture
        .by_ref()
        .take((MAX_FIXTURE_BYTES + 1) as u64)
        .read_to_end(&mut contents)?;
    if contents.len() > MAX_FIXTURE_BYTES {
        return Err(RuntimeError::FixtureTooLarge);
    }
    let mut records = contents
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if records.last().is_some_and(Vec::is_empty) {
        records.pop();
    }
    Ok(records)
}

fn validate_final_snapshot(
    snapshot: &PublishedSnapshot,
    health: RuntimeHealth,
) -> Result<(), RuntimeError> {
    if snapshot.source.name() != "binance-fixture"
        || snapshot.instrument.venue_symbol() != "BTCUSDT"
        || snapshot.source.generation() != 1
        || snapshot.instrument.generation() != 1
        || snapshot.sequence != EXPECTED_FINAL_SEQUENCE
        || snapshot.best_bid.to_string() != EXPECTED_BEST_BID
        || snapshot.best_ask.to_string() != EXPECTED_BEST_ASK
        || health != RuntimeHealth::Healthy
    {
        Err(RuntimeError::UnexpectedFinalSnapshot)
    } else {
        Ok(())
    }
}

fn received_key() -> MetricKey {
    MetricKey {
        name: MetricName::EventsReceived,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Success,
    }
}

fn ingestion_bytes_key() -> MetricKey {
    MetricKey {
        name: MetricName::IngestionBytes,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Success,
    }
}

fn rejected_key() -> MetricKey {
    MetricKey {
        name: MetricName::EventsRejected,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Degraded,
    }
}

fn parser_failures_key() -> MetricKey {
    MetricKey {
        name: MetricName::ParserFailures,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Rejected,
    }
}

fn sequence_gaps_key() -> MetricKey {
    MetricKey {
        name: MetricName::SequenceGaps,
        component: Component::OrderBook,
        venue: Venue::Binance,
        outcome: Outcome::Degraded,
    }
}

fn checksum_failures_key() -> MetricKey {
    MetricKey {
        name: MetricName::ChecksumFailures,
        component: Component::OrderBook,
        venue: Venue::Binance,
        outcome: Outcome::Degraded,
    }
}

fn queue_depth_key() -> MetricKey {
    MetricKey {
        name: MetricName::QueueDepth,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::NotApplicable,
    }
}

fn wal_fsync_key() -> MetricKey {
    MetricKey {
        name: MetricName::WalFsyncLatency,
        component: Component::Storage,
        venue: Venue::Binance,
        outcome: Outcome::Success,
    }
}

fn event_to_normalized_key() -> MetricKey {
    MetricKey {
        name: MetricName::EventToNormalizedLatency,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Success,
    }
}

fn initialize_runtime_metrics(metrics: &Metrics) -> Result<(), ObservabilityError> {
    for key in [
        received_key(),
        ingestion_bytes_key(),
        rejected_key(),
        parser_failures_key(),
        sequence_gaps_key(),
        checksum_failures_key(),
    ] {
        metrics.increment(key, 0)?;
    }
    metrics.gauge(queue_depth_key(), 0)
}

fn manager_io_error(error: ManagerError) -> io::Error {
    io::Error::other(error)
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("fixture ingestion queue capacity must be nonzero")]
    InvalidQueueCapacity,
    #[error("runtime metric value cannot be represented safely")]
    MetricValueOverflow,
    #[error("market WAL persistence failed")]
    Persistence(#[source] io::Error),
    #[error("market fixture parsing failed")]
    Parse(#[source] ParseError),
    #[error("authoritative order book update failed")]
    Book(#[source] BookError),
    #[error("segmented market WAL failed: {0}")]
    Wal(#[from] ManagerError),
    #[error("segmented market WAL metadata is invalid")]
    Prologue(#[from] PrologueError),
    #[error("operating system randomness is unavailable")]
    Random,
    #[error("WAL record sequence overflow")]
    RecordSequenceOverflow,
    #[error("recovered WAL does not match the frozen fixture prefix")]
    RecoveryMismatch,
    #[error("fixture I/O failed: {0}")]
    FixtureIo(#[from] io::Error),
    #[error("fixture exceeds the bounded input size")]
    FixtureTooLarge,
    #[error("normalized market event is missing an instrument")]
    MissingInstrument,
    #[error("normalized market event is missing an event timestamp")]
    MissingTimestamp,
    #[error("order book is missing authoritative best prices")]
    MissingBestPrices,
    #[error("fixture ingestion did not publish a snapshot")]
    MissingSnapshot,
    #[error("fixture ingestion produced an unexpected final snapshot")]
    UnexpectedFinalSnapshot,
    #[error("fixture ingestion queue closed before all records were accepted")]
    QueueClosed,
    #[error("fixture runtime startup was cancelled")]
    Cancelled,
    #[error("fixture producer task failed")]
    ProducerJoin(#[from] JoinError),
    #[error("observability failed")]
    Observability(#[from] ObservabilityError),
    #[error("local RPC server failed")]
    Server(#[from] ServerError),
    #[error("authoritative RPC snapshot is invalid")]
    Snapshot(#[from] SnapshotError),
}
