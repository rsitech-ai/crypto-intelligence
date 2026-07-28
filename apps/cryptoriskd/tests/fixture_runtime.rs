use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use local_api::{
    auth::{SessionAuthenticator, SessionSecret, insert_authentication_metadata},
    proto::market_v1::{GetOrderBookSnapshotRequest, SnapshotHealth},
    session::SessionDescriptor,
};
use observability::{Component, MetricKey, MetricName, Metrics, Outcome, Venue};
use tokio::sync::mpsc::error::TrySendError;
use tonic::{Code, Request};
use zeroize::Zeroizing;

#[path = "../src/runtime.rs"]
mod runtime;
#[allow(dead_code)]
#[path = "../src/startup.rs"]
mod startup;
mod support;

use runtime::{
    IngestionEngine, RuntimeError, RuntimeHealth, RuntimeLimits, RuntimeOptions, WalSink,
    ingestion_channel_with_capacity, start_fixture_runtime_with_limits,
};
use startup::{
    StartupError, issue_session_descriptor, open_log_file, open_wal_file, parse_session_secret,
};

const FIXTURE: &str = include_str!("../../../fixtures/binance/btcusdt-book-v1.jsonl");
const SECRET_BYTES: [u8; 32] = [0x5a; 32];

#[test]
fn inherited_reader_requires_exactly_32_bytes_without_exposing_them() {
    for length in [0, 1, 31, 33, 64] {
        let error = match parse_session_secret(Zeroizing::new(vec![0xa5; length.min(33)])) {
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
        parse_session_secret(Zeroizing::new(SECRET_BYTES.to_vec()))
            .expect("exact secret must read"),
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
        .get_order_book_snapshot(insert_authentication_metadata(
            Request::new(GetOrderBookSnapshotRequest {}),
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
        .get_order_book_snapshot(insert_authentication_metadata(
            Request::new(GetOrderBookSnapshotRequest {}),
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

#[tokio::test]
async fn blank_fixture_records_are_persisted_before_the_runtime_rejects_them() {
    let fixture_lines = FIXTURE.split_terminator('\n').collect::<Vec<_>>();
    for (case, records, expected_prefix) in [
        (
            "leading",
            vec!["", fixture_lines[0], fixture_lines[1], fixture_lines[2]],
            vec![b"".as_slice()],
        ),
        (
            "internal",
            vec![fixture_lines[0], "", fixture_lines[1], fixture_lines[2]],
            vec![fixture_lines[0].as_bytes(), b"".as_slice()],
        ),
        (
            "repeated",
            vec![fixture_lines[0], "", "", fixture_lines[1], fixture_lines[2]],
            vec![fixture_lines[0].as_bytes(), b"".as_slice()],
        ),
    ] {
        let directory = tempfile::tempdir().expect("temporary runtime root must exist");
        let fixture = records.join("\n");
        let error = match start_with_fixture(directory.path(), descriptor(), &fixture).await {
            Ok(_) => panic!("blank records must degrade and reject startup"),
            Err(error) => error,
        };
        assert!(
            matches!(error, RuntimeError::Parse(_)),
            "{case} blank must reach the parser after persistence"
        );

        let mut segment = raw_wal::segment::Segment::open(&directory.path().join("market.wal"))
            .expect("persisted WAL must reopen");
        let recovery = segment.recover().expect("persisted prefix must recover");
        assert_eq!(
            recovery.records(),
            expected_prefix,
            "{case} blank must remain in the WAL"
        );
    }
}

#[tokio::test]
async fn cancellation_is_observed_before_wal_recovery_and_syncs_partial_state() {
    let directory = tempfile::tempdir().expect("temporary runtime root must exist");
    fs::create_dir(directory.path().join("logs")).expect("log root must exist");
    let fixture_path = directory.path().join("fixture.jsonl");
    fs::write(&fixture_path, FIXTURE).expect("fixture must write");
    let wal_path = directory.path().join("market.wal");
    let mut original = raw_wal::frame::encode(b"first").expect("first frame must encode");
    let mut torn = raw_wal::frame::encode(b"second").expect("second frame must encode");
    let last = torn.last_mut().expect("second frame must have a checksum");
    *last ^= 0x80;
    original.extend_from_slice(&torn);
    fs::write(&wal_path, &original).expect("recoverable WAL tail must write");
    let wal = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&wal_path)
        .expect("WAL must open");
    let log_path = directory.path().join("logs/cmti.jsonl");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .expect("log must open");
    let (cancellation_sender, cancellation) = tokio::sync::watch::channel(false);
    cancellation_sender
        .send(true)
        .expect("startup cancellation must send");

    let error = match start_fixture_runtime_with_limits(
        RuntimeOptions {
            fixture: File::open(fixture_path).expect("fixture must open"),
            wal,
            log,
            secret: secret(),
            descriptor: descriptor(),
            cancellation,
        },
        runtime_limits(),
    )
    .await
    {
        Ok(_) => panic!("cancelled startup must not become ready"),
        Err(error) => error,
    };

    assert!(matches!(error, RuntimeError::Cancelled));
    assert_eq!(
        fs::read(&wal_path).expect("cancelled WAL must read"),
        original,
        "cancellation before recovery must not repair or mutate the WAL"
    );
    assert!(
        fs::read_to_string(log_path)
            .expect("cancel log must read")
            .contains("fixture_runtime_cancelled"),
        "cancelled startup must sync its cleanup event"
    );
}

#[test]
fn existing_wal_and_log_reject_hardlinks_and_special_files() {
    let hardlink_root = runtime_root();
    let effective = effective_config(hardlink_root.path());
    let wal_path = hardlink_root.path().join("data/market.wal");
    fs::write(&wal_path, []).expect("WAL placeholder must write");
    fs::set_permissions(&wal_path, fs::Permissions::from_mode(0o600))
        .expect("WAL permissions must set");
    fs::hard_link(&wal_path, hardlink_root.path().join("wal-alias"))
        .expect("WAL hardlink must create");
    assert!(
        open_wal_file(&effective.paths.data_root).is_err(),
        "hardlinked WAL must fail closed"
    );

    let log_path = hardlink_root.path().join("logs/cmti.jsonl");
    fs::write(&log_path, []).expect("log placeholder must write");
    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600))
        .expect("log permissions must set");
    fs::hard_link(&log_path, hardlink_root.path().join("log-alias"))
        .expect("log hardlink must create");
    assert!(
        open_log_file(&effective.paths.log_root).is_err(),
        "hardlinked log must fail closed"
    );

    let special_root = runtime_root();
    let effective = effective_config(special_root.path());
    let fifo_status = std::process::Command::new("mkfifo")
        .arg(special_root.path().join("data/market.wal"))
        .status()
        .expect("mkfifo test helper must run");
    assert!(fifo_status.success(), "test FIFO must create");
    assert!(
        open_wal_file(&effective.paths.data_root).is_err(),
        "special WAL file must fail closed"
    );
}

#[test]
fn state_files_reject_unsafe_modes_and_created_wal_retains_its_exact_inode() {
    let unsafe_root = runtime_root();
    let effective = effective_config(unsafe_root.path());
    let wal_path = unsafe_root.path().join("data/market.wal");
    fs::write(&wal_path, []).expect("WAL placeholder must write");
    fs::set_permissions(&wal_path, fs::Permissions::from_mode(0o644))
        .expect("unsafe WAL permissions must set");
    assert!(
        open_wal_file(&effective.paths.data_root).is_err(),
        "group/world-readable WAL must fail closed"
    );
    let log_path = unsafe_root.path().join("logs/cmti.jsonl");
    fs::write(&log_path, []).expect("log placeholder must write");
    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o640))
        .expect("unsafe log permissions must set");
    assert!(
        open_log_file(&effective.paths.log_root).is_err(),
        "group-readable log must fail closed"
    );

    let replacement_root = runtime_root();
    let effective = effective_config(replacement_root.path());
    let mut wal = open_wal_file(&effective.paths.data_root).expect("new WAL must open securely");
    let wal_path = replacement_root.path().join("data/market.wal");
    let retained_path = replacement_root.path().join("data/retained.wal");
    fs::rename(&wal_path, &retained_path).expect("test replacement must move pathname");
    fs::write(&wal_path, b"replacement").expect("replacement pathname must write");
    fs::set_permissions(&wal_path, fs::Permissions::from_mode(0o600))
        .expect("replacement permissions must set");
    wal.write_all(b"retained")
        .expect("retained exact WAL fd must remain writable");
    wal.sync_data().expect("retained exact WAL fd must sync");

    assert_eq!(
        fs::read(&retained_path).expect("retained inode must read"),
        b"retained"
    );
    assert_eq!(
        fs::read(&wal_path).expect("replacement inode must read"),
        b"replacement"
    );
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
fn ingestion_channel_backpressures_at_the_configured_capacity() {
    let configured_capacity = 3;
    let (sender, _receiver) = ingestion_channel_with_capacity(configured_capacity);
    for sequence in 0..configured_capacity {
        sender
            .try_send(sequence.to_string().into_bytes())
            .expect("records up to the configured capacity must fit");
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
async fn error_log_level_suppresses_info_lifecycle_events() {
    let directory = tempfile::tempdir().expect("temporary runtime root must exist");
    let limits = RuntimeLimits::new(
        1_024,
        256,
        1,
        Duration::from_secs(2),
        Duration::from_secs(5),
        false,
    )
    .expect("error-level runtime limits must validate");
    let running = start_with_fixture_and_limits(directory.path(), descriptor(), FIXTURE, limits)
        .await
        .expect("runtime must start with info logs disabled");
    running.shutdown().await.expect("runtime must stop");
    let log = fs::read_to_string(directory.path().join("logs/cmti.jsonl"))
        .expect("local log must remain readable");
    assert!(
        !log.contains("fixture_runtime_ready")
            && !log.contains("fixture_runtime_stopped")
            && !log.contains("fixture_runtime_cancelled"),
        "error logging must not retain informational lifecycle events"
    );
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
        .get_order_book_snapshot(Request::new(GetOrderBookSnapshotRequest {}))
        .await
        .expect_err("missing authentication must fail");
    assert_eq!(status.code(), Code::Unauthenticated);

    let authenticator = SessionAuthenticator::new(secret());
    let token = authenticator.token(&descriptor);
    let response = market
        .get_order_book_snapshot(insert_authentication_metadata(
            Request::new(GetOrderBookSnapshotRequest {}),
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
    start_with_fixture(root, descriptor, FIXTURE).await
}

async fn start_with_fixture(
    root: &Path,
    descriptor: SessionDescriptor,
    fixture_contents: &str,
) -> Result<runtime::RunningDaemon, RuntimeError> {
    start_with_fixture_and_limits(root, descriptor, fixture_contents, runtime_limits()).await
}

async fn start_with_fixture_and_limits(
    root: &Path,
    descriptor: SessionDescriptor,
    fixture_contents: &str,
    limits: RuntimeLimits,
) -> Result<runtime::RunningDaemon, RuntimeError> {
    fs::create_dir_all(root.join("logs")).expect("log root must exist");
    let fixture_path = root.join("fixture.jsonl");
    fs::write(&fixture_path, fixture_contents).expect("fixture copy must write");
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
    start_fixture_runtime_with_limits(
        RuntimeOptions {
            fixture,
            wal,
            log,
            secret: secret(),
            descriptor,
            cancellation: tokio::sync::watch::channel(false).1,
        },
        limits,
    )
    .await
}

fn runtime_limits() -> RuntimeLimits {
    RuntimeLimits::new(
        1_024,
        256,
        1,
        Duration::from_secs(2),
        Duration::from_secs(5),
        true,
    )
    .expect("test runtime limits must validate")
}

fn runtime_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temporary runtime root must exist");
    fs::create_dir(root.path().join("data")).expect("data root must exist");
    fs::create_dir(root.path().join("logs")).expect("log root must exist");
    fs::create_dir_all(root.path().join("models/public-test-artifacts"))
        .expect("model registry must exist");
    fs::write(root.path().join("fixture.jsonl"), FIXTURE).expect("fixture must exist");
    root
}

fn effective_config(root: &Path) -> config::EffectiveConfig {
    let defaults = include_str!("../../../configs/default.toml")
        .replace("fixtures/binance/btcusdt-book-v1.jsonl", "fixture.jsonl");
    config::load(
        &defaults,
        "test-default",
        None,
        config::Overrides::default(),
        root,
    )
    .expect("test configuration must load")
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

async fn connect(address: std::net::SocketAddr) -> support::MarketTestClient {
    support::MarketTestClient::connect(format!("http://{address}")).await
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
