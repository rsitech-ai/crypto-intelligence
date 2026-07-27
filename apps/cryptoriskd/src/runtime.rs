use std::{
    fs::File,
    io::{self, Read},
    net::SocketAddr,
};

use connector_binance::{ParseError, parse_fixture_line};
use domain::{InstrumentId, SourceId, UnixNanos};
use event_envelope::UncheckedEventPayload;
use fixed_decimal::Price;
use local_api::{
    auth::SessionSecret,
    proto::market_v1::SnapshotHealth,
    server::{LoopbackServer, MarketSnapshot, ServerError, SnapshotError},
    session::SessionDescriptor,
};
use observability::{
    Component, LocalJsonLog, MetricKey, MetricName, Metrics, ObservabilityError, Outcome, Venue,
};
use orderbook::{BookError, OrderBook};
use raw_wal::{
    RecoveryError,
    segment::{Segment, SegmentError},
};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle},
};

pub const INGESTION_QUEUE_CAPACITY: usize = 1_024;
const MAX_FIXTURE_BYTES: usize = 1024 * 1024;
const EXPECTED_FINAL_SEQUENCE: u64 = 102;
const EXPECTED_BEST_BID: &str = "60000.1";
const EXPECTED_BEST_ASK: &str = "60000.2";

pub struct RuntimeOptions {
    pub fixture: File,
    pub wal: File,
    pub log: File,
    pub secret: SessionSecret,
    pub descriptor: SessionDescriptor,
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
        if let Err(source) = self.wal.append_and_sync(raw) {
            self.reject()?;
            return Err(RuntimeError::Persistence(source));
        }
        self.process_persisted(raw)
    }

    pub fn process_persisted(&mut self, raw: &[u8]) -> Result<(), RuntimeError> {
        let event = match parse_fixture_line(raw) {
            Ok(event) => event,
            Err(source) => {
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
            UncheckedEventPayload::Trade(_) => Err(BookError::Invalid),
        };
        if let Err(source) = apply_result {
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
    segment: Segment,
}

impl WalSink for SegmentSink {
    fn append_and_sync(&mut self, payload: &[u8]) -> io::Result<()> {
        self.segment
            .append_synced(payload)
            .map(|_| ())
            .map_err(segment_io_error)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.segment.sync()
    }
}

pub struct RunningDaemon {
    server: LoopbackServer,
    wal: SegmentSink,
    log: LocalJsonLog,
    readiness: Readiness,
}

impl RunningDaemon {
    pub const fn readiness(&self) -> &Readiness {
        &self.readiness
    }

    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        let server_result = self.server.shutdown().await;
        let wal_result = self.wal.sync();
        let log_result = self.log.write_event(&json!({
            "level": "info",
            "event": "fixture_runtime_stopped",
            "sequence": EXPECTED_FINAL_SEQUENCE
        }));
        let log_shutdown_result = self.log.shutdown();

        server_result?;
        wal_result.map_err(RuntimeError::Persistence)?;
        log_result?;
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

pub async fn start_fixture_runtime(options: RuntimeOptions) -> Result<RunningDaemon, RuntimeError> {
    let RuntimeOptions {
        fixture,
        wal,
        log,
        secret,
        descriptor,
    } = options;
    let log = LocalJsonLog::from_file(log);
    let fixture_records = read_fixture_records(fixture)?;
    let mut segment = Segment::from_file(wal);
    let recovery = segment.recover()?;
    if recovery.records().len() > fixture_records.len()
        || !recovery
            .records()
            .iter()
            .zip(&fixture_records)
            .all(|(recovered, expected)| recovered == expected)
    {
        return Err(RuntimeError::RecoveryMismatch);
    }

    let metrics = Metrics::default();
    let mut engine = IngestionEngine::new(SegmentSink { segment }, metrics.clone());
    for record in recovery.records() {
        engine.process_persisted(record)?;
    }

    let remaining = fixture_records
        .into_iter()
        .skip(recovery.records().len())
        .collect::<Vec<_>>();
    let (sender, mut receiver) = ingestion_channel();
    let producer = spawn_fixture_producer(sender, remaining);
    while let Some(record) = receiver.recv().await {
        engine.process_new(&record)?;
    }
    producer.await??;

    let published = engine
        .published()
        .cloned()
        .ok_or(RuntimeError::MissingSnapshot)?;
    validate_final_snapshot(&published, engine.source_health())?;
    let market_snapshot = MarketSnapshot::new(
        published.source,
        published.instrument,
        published.sequence,
        published.best_bid,
        published.best_ask,
        SnapshotHealth::Healthy,
        published.event_timestamp,
        published.receive_timestamp,
        published.freshness_millis,
    )?;
    let wal = engine.into_wal();
    let server = LoopbackServer::spawn(
        "127.0.0.1:0"
            .parse()
            .expect("constant loopback bind must parse"),
        secret,
        descriptor.clone(),
        market_snapshot,
    )
    .await?;
    let readiness = Readiness::new(server.local_addr(), &descriptor);
    log.write_event(&json!({
        "level": "info",
        "event": "fixture_runtime_ready",
        "source": "binance-fixture",
        "symbol": "BTCUSDT",
        "generation": 1,
        "sequence": EXPECTED_FINAL_SEQUENCE,
        "endpoint": &readiness.endpoint
    }))?;
    log.sync()?;

    Ok(RunningDaemon {
        server,
        wal,
        log,
        readiness,
    })
}

pub fn ingestion_channel() -> (mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
    mpsc::channel(INGESTION_QUEUE_CAPACITY)
}

fn spawn_fixture_producer(
    sender: mpsc::Sender<Vec<u8>>,
    records: Vec<Vec<u8>>,
) -> JoinHandle<Result<(), RuntimeError>> {
    tokio::spawn(async move {
        for record in records {
            sender
                .send(record)
                .await
                .map_err(|_| RuntimeError::QueueClosed)?;
        }
        Ok(())
    })
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

fn rejected_key() -> MetricKey {
    MetricKey {
        name: MetricName::EventsRejected,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Degraded,
    }
}

fn segment_io_error(error: SegmentError) -> io::Error {
    io::Error::other(error)
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("market WAL persistence failed")]
    Persistence(#[source] io::Error),
    #[error("market fixture parsing failed")]
    Parse(#[source] ParseError),
    #[error("authoritative order book update failed")]
    Book(#[source] BookError),
    #[error("market WAL recovery failed")]
    Recovery(#[from] RecoveryError),
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
    #[error("fixture producer task failed")]
    ProducerJoin(#[from] JoinError),
    #[error("observability failed")]
    Observability(#[from] ObservabilityError),
    #[error("local RPC server failed")]
    Server(#[from] ServerError),
    #[error("authoritative RPC snapshot is invalid")]
    Snapshot(#[from] SnapshotError),
}
