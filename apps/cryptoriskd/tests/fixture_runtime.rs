use std::{
    fs::{self, File, OpenOptions},
    io::{self, Cursor},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use local_api::{
    auth::{SessionAuthenticator, SessionSecret, insert_authentication_metadata},
    proto::market_v1::{
        GetSnapshotRequest, SnapshotHealth, market_service_client::MarketServiceClient,
    },
    session::SessionDescriptor,
};
use observability::{Component, MetricKey, MetricName, Metrics, Outcome, Venue};
use tokio::sync::mpsc::error::TrySendError;
use tonic::{Code, Request, transport::Endpoint};
use zeroize::Zeroizing;

#[path = "../src/runtime.rs"]
mod runtime;
#[allow(dead_code)]
#[path = "../src/startup.rs"]
mod startup;

use runtime::{
    INGESTION_QUEUE_CAPACITY, IngestionEngine, RuntimeError, RuntimeHealth, RuntimeOptions,
    WalSink, ingestion_channel, start_fixture_runtime,
};
use startup::{StartupError, issue_session_descriptor, read_session_secret};

const FIXTURE: &str = include_str!("../../../fixtures/binance/btcusdt-book-v1.jsonl");
const SECRET_BYTES: [u8; 32] = [0x5a; 32];

#[test]
fn inherited_reader_requires_exactly_32_bytes_without_exposing_them() {
    for length in [0, 1, 31, 33, 64] {
        let error = match read_session_secret(Cursor::new(vec![0xa5; length])) {
            Ok(_) => panic!("non-32-byte inherited secrets must fail"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            StartupError::SecretLength { actual } if actual == length.min(33)
        ));
        assert!(!error.to_string().contains("a5"));
    }

    let descriptor = SessionDescriptor::issue(1, 0, 7, [1; 16], [2; 16], 100)
        .expect("test descriptor must issue");
    let inherited = SessionAuthenticator::new(
        read_session_secret(Cursor::new(SECRET_BYTES)).expect("exact secret must read"),
    )
    .token(&descriptor);
    let direct = SessionAuthenticator::new(secret()).token(&descriptor);
    assert_eq!(inherited.as_bytes(), direct.as_bytes());
}

#[test]
fn generated_public_descriptor_has_exact_lifetime_and_process_identity() {
    let descriptor = issue_session_descriptor().expect("public descriptor must issue");

    assert_eq!(descriptor.protocol_major(), 1);
    assert_eq!(descriptor.protocol_minor(), 0);
    assert_eq!(descriptor.daemon_pid(), std::process::id());
    assert_eq!(
        descriptor.expiry_unix_seconds() - descriptor.issued_unix_seconds(),
        60
    );
    assert_ne!(descriptor.process_nonce(), &[0; 16]);
    assert_ne!(descriptor.server_nonce(), &[0; 16]);
    assert_ne!(descriptor.process_nonce(), descriptor.server_nonce());
}

#[tokio::test]
async fn runtime_serves_exact_authenticated_fixture_snapshot_without_secret_output() {
    let directory = tempfile::tempdir().expect("temporary runtime root must exist");
    let descriptor = descriptor();
    let running = start(directory.path(), descriptor.clone())
        .await
        .expect("fixture runtime must start");
    let readiness_json =
        serde_json::to_string(running.readiness()).expect("readiness must serialize");
    let authenticator = SessionAuthenticator::new(secret());
    let token = authenticator.token(&descriptor);

    for forbidden in [
        hex::encode(SECRET_BYTES),
        hex::encode(token.as_bytes()),
        "secret".to_owned(),
        "token".to_owned(),
    ] {
        assert!(
            !readiness_json.contains(&forbidden),
            "readiness cannot expose secret/token material"
        );
    }

    let mut market = connect(running_address(&running)).await;
    let response = market
        .get_snapshot(insert_authentication_metadata(
            Request::new(GetSnapshotRequest {}),
            &descriptor,
            &token,
        ))
        .await
        .expect("valid local session must authenticate")
        .into_inner();
    assert_eq!(response.source, "binance-fixture");
    assert_eq!(response.symbol, "BTCUSDT");
    assert_eq!(response.generation, 1);
    assert_eq!(response.sequence, 102);
    assert_eq!(response.best_bid, "60000.1");
    assert_eq!(response.best_ask, "60000.2");
    assert_eq!(response.health, SnapshotHealth::Healthy as i32);

    running
        .shutdown()
        .await
        .expect("healthy runtime must shut down");
}

#[tokio::test]
async fn restart_recovers_exact_fixture_without_appending_duplicates() {
    let directory = tempfile::tempdir().expect("temporary runtime root must exist");
    let first = start(directory.path(), descriptor())
        .await
        .expect("first fixture runtime must start");
    first
        .shutdown()
        .await
        .expect("first fixture runtime must stop");
    let wal_path = directory.path().join("market.wal");
    let first_length = fs::metadata(&wal_path).expect("first WAL must exist").len();

    let second_descriptor = descriptor();
    let second = start(directory.path(), second_descriptor.clone())
        .await
        .expect("recovered fixture runtime must start");
    let second_length = fs::metadata(&wal_path)
        .expect("recovered WAL must exist")
        .len();
    assert_eq!(second_length, first_length);

    let authenticator = SessionAuthenticator::new(secret());
    let token = authenticator.token(&second_descriptor);
    let mut market = connect(running_address(&second)).await;
    let response = market
        .get_snapshot(insert_authentication_metadata(
            Request::new(GetSnapshotRequest {}),
            &second_descriptor,
            &token,
        ))
        .await
        .expect("recovered runtime must authenticate")
        .into_inner();
    assert_eq!(response.sequence, 102);
    assert_eq!(response.best_bid, "60000.1");
    assert_eq!(response.best_ask, "60000.2");

    second
        .shutdown()
        .await
        .expect("recovered runtime must stop");
}

#[test]
fn persistence_failure_happens_before_parse_or_publication() {
    let metrics = Metrics::default();
    let mut engine = IngestionEngine::new(FailingWal, metrics);

    let error = engine
        .process_new(br#"not even JSON"#)
        .expect_err("injected sync failure must stop ingestion");

    assert!(matches!(error, RuntimeError::Persistence(_)));
    assert!(engine.published().is_none());
    assert_eq!(engine.source_health(), RuntimeHealth::Degraded);
}

#[test]
fn malformed_json_and_sequence_gap_preserve_last_healthy_snapshot() {
    let metrics = Metrics::default();
    let mut engine = IngestionEngine::new(MemoryWal::default(), metrics.clone());
    for line in FIXTURE.lines() {
        engine
            .process_new(line.as_bytes())
            .expect("frozen fixture must process");
    }
    let healthy = engine.published().expect("fixture must publish").clone();

    assert!(matches!(
        engine.process_new(br#"{"#),
        Err(RuntimeError::Parse(_))
    ));
    assert_eq!(engine.published(), Some(&healthy));
    assert_eq!(engine.source_health(), RuntimeHealth::Degraded);
    assert_eq!(
        metrics
            .counter(rejected_key())
            .expect("rejection counter must read"),
        1
    );

    let gap = br#"{"kind":"delta","source":"binance-fixture","symbol":"BTCUSDT","generation":1,"event_unix_nanos":1700000000300000000,"first_sequence":104,"last_sequence":104,"bids":[["60000.1","9"]],"asks":[]}"#;
    assert!(matches!(
        engine.process_new(gap),
        Err(RuntimeError::Book(_))
    ));
    assert_eq!(engine.published(), Some(&healthy));
    assert_eq!(engine.source_health(), RuntimeHealth::Degraded);
    assert_eq!(
        metrics
            .counter(rejected_key())
            .expect("rejection counter must read"),
        2
    );
}

#[test]
fn ingestion_channel_backpressures_at_exactly_1024_records() {
    let (sender, _receiver) = ingestion_channel();
    for sequence in 0..INGESTION_QUEUE_CAPACITY {
        sender
            .try_send(sequence.to_string().into_bytes())
            .expect("the first 1024 records must fit");
    }
    assert!(matches!(
        sender.try_send(b"overflow".to_vec()),
        Err(TrySendError::Full(_))
    ));
}

#[tokio::test]
async fn healthy_shutdown_completes_within_five_seconds_and_syncs_logs() {
    let directory = tempfile::tempdir().expect("temporary runtime root must exist");
    let running = start(directory.path(), descriptor())
        .await
        .expect("fixture runtime must start");

    tokio::time::timeout(Duration::from_secs(5), running.shutdown())
        .await
        .expect("shutdown must complete within five seconds")
        .expect("shutdown must succeed");

    let log = fs::read_to_string(directory.path().join("logs/cmti.jsonl"))
        .expect("synced local log must read");
    assert!(log.contains("fixture_runtime_ready"));
    assert!(log.contains("fixture_runtime_stopped"));
    for forbidden in ["panic", "\"level\":\"error\"", "\"level\":\"warn\""] {
        assert!(!log.contains(forbidden));
    }
}

#[tokio::test]
async fn authentication_failure_does_not_change_the_authoritative_snapshot() {
    let directory = tempfile::tempdir().expect("temporary runtime root must exist");
    let descriptor = descriptor();
    let running = start(directory.path(), descriptor.clone())
        .await
        .expect("fixture runtime must start");
    let mut market = connect(running_address(&running)).await;

    let status = market
        .get_snapshot(Request::new(GetSnapshotRequest {}))
        .await
        .expect_err("missing authentication must fail");
    assert_eq!(status.code(), Code::Unauthenticated);

    let authenticator = SessionAuthenticator::new(secret());
    let token = authenticator.token(&descriptor);
    let response = market
        .get_snapshot(insert_authentication_metadata(
            Request::new(GetSnapshotRequest {}),
            &descriptor,
            &token,
        ))
        .await
        .expect("valid authentication must still return the snapshot")
        .into_inner();
    assert_eq!(response.sequence, 102);
    assert_eq!(response.health, SnapshotHealth::Healthy as i32);

    running.shutdown().await.expect("runtime must stop");
}

async fn start(
    root: &Path,
    descriptor: SessionDescriptor,
) -> Result<runtime::RunningDaemon, RuntimeError> {
    fs::create_dir_all(root.join("logs")).expect("log root must exist");
    let fixture_path = root.join("fixture.jsonl");
    fs::write(&fixture_path, FIXTURE).expect("fixture copy must write");
    let fixture = File::open(fixture_path).expect("fixture copy must open");
    let wal = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join("market.wal"))
        .expect("WAL must open");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("logs/cmti.jsonl"))
        .expect("local log must open");
    start_fixture_runtime(RuntimeOptions {
        fixture,
        wal,
        log,
        secret: secret(),
        descriptor,
    })
    .await
}

fn descriptor() -> SessionDescriptor {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time must follow epoch")
        .as_secs() as i64;
    SessionDescriptor::issue(1, 0, std::process::id(), [0x11; 16], [0x22; 16], now)
        .expect("test descriptor must be valid")
}

fn secret() -> SessionSecret {
    SessionSecret::try_from(Zeroizing::new(SECRET_BYTES.to_vec()))
        .expect("test secret must be exact")
}

async fn connect(address: std::net::SocketAddr) -> MarketServiceClient<tonic::transport::Channel> {
    let endpoint = Endpoint::from_shared(format!("http://{address}"))
        .expect("loopback endpoint must be valid");
    let channel = endpoint.connect().await.expect("loopback RPC must connect");
    MarketServiceClient::new(channel)
}

fn running_address(running: &runtime::RunningDaemon) -> std::net::SocketAddr {
    running
        .readiness()
        .endpoint
        .strip_prefix("http://")
        .expect("readiness endpoint must use HTTP")
        .parse()
        .expect("readiness endpoint must contain a socket address")
}

fn rejected_key() -> MetricKey {
    MetricKey {
        name: MetricName::EventsRejected,
        component: Component::Ingestion,
        venue: Venue::Binance,
        outcome: Outcome::Degraded,
    }
}

struct FailingWal;

impl WalSink for FailingWal {
    fn append_and_sync(&mut self, _payload: &[u8]) -> io::Result<()> {
        Err(io::Error::other("injected sync failure"))
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct MemoryWal {
    records: Vec<Vec<u8>>,
}

impl WalSink for MemoryWal {
    fn append_and_sync(&mut self, payload: &[u8]) -> io::Result<()> {
        self.records.push(payload.to_vec());
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        Ok(())
    }
}
