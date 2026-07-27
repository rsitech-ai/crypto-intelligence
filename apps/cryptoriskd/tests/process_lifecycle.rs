use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use local_api::{
    auth::{SessionAuthenticator, SessionSecret, insert_authentication_metadata},
    proto::market_v1::{
        GetSnapshotRequest, SnapshotHealth, market_service_client::MarketServiceClient,
    },
    session::SessionDescriptor,
};
use serde::Deserialize;
use tonic::{Request, transport::Endpoint};
use zeroize::Zeroizing;

const FIXTURE: &str = include_str!("../../../fixtures/binance/btcusdt-book-v1.jsonl");
const SECRET_BYTES: [u8; 32] = [0x6b; 32];
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct Readiness {
    endpoint: String,
    protocol_major: u32,
    protocol_minor: u32,
    daemon_pid: u32,
    process_nonce: String,
    server_nonce: String,
    issued_unix_seconds: i64,
    expiry_unix_seconds: i64,
}

#[tokio::test(flavor = "multi_thread")]
async fn real_daemon_authenticates_closes_secret_fd_locks_and_restarts_same_wal() {
    let root = runtime_root();
    let secret_path = root.path().join("session-secret.bin");
    fs::write(&secret_path, SECRET_BYTES).expect("session secret source must write");

    let mut first = DaemonProcess::spawn(root.path(), &secret_path, SecretOpenMode::ReadOnly);
    let first_readiness = first.readiness();
    assert_authenticated_snapshot(&first_readiness).await;
    assert_fd_table_has_no_secret(&first, &secret_path);

    let mut competing = DaemonProcess::spawn(root.path(), &secret_path, SecretOpenMode::ReadOnly);
    let competing_status = competing.wait_for_exit(EXIT_TIMEOUT);
    assert!(
        !competing_status.success(),
        "second daemon must fail closed"
    );
    let competing_output = competing.capture();
    assert!(
        competing_output.stdout.lines().next().is_none(),
        "locked daemon must not announce readiness"
    );
    assert!(competing_output.stderr.contains("market WAL open failed"));
    assert_no_secret_material(&competing_output);

    let first_status = first.signal_and_wait("-TERM", EXIT_TIMEOUT);
    assert!(first_status.success(), "SIGTERM shutdown must be clean");
    let first_output = first.capture();
    assert_eq!(
        first_output.stdout.lines().count(),
        1,
        "daemon must print exactly one readiness line"
    );
    assert!(
        first_output.stderr.is_empty(),
        "healthy stderr must be empty"
    );
    assert_no_secret_material(&first_output);
    let wal_path = root.path().join("data/market.wal");
    let first_wal_length = fs::metadata(&wal_path).expect("first WAL must exist").len();

    let mut restarted = DaemonProcess::spawn(root.path(), &secret_path, SecretOpenMode::ReadOnly);
    let restarted_readiness = restarted.readiness();
    assert_authenticated_snapshot(&restarted_readiness).await;
    assert_fd_table_has_no_secret(&restarted, &secret_path);
    assert_eq!(
        fs::metadata(&wal_path)
            .expect("restarted WAL must exist")
            .len(),
        first_wal_length,
        "restart must recover without duplicate append"
    );

    let restarted_status = restarted.signal_and_wait("-INT", EXIT_TIMEOUT);
    assert!(restarted_status.success(), "Ctrl-C shutdown must be clean");
    let restarted_output = restarted.capture();
    assert_eq!(restarted_output.stdout.lines().count(), 1);
    assert!(restarted_output.stderr.is_empty());
    assert_no_secret_material(&restarted_output);
}

#[test]
fn signal_during_deliberately_blocked_secret_startup_exits_without_readiness() {
    let root = runtime_root();
    let fifo = root.path().join("blocked-session-secret");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo helper must run");
    assert!(status.success(), "secret FIFO must create");

    let mut writer = ChildGuard::spawn(
        Command::new("/bin/sh")
            .arg("-c")
            .arg("exec 4> \"$1\"; exec sleep 30")
            .arg("cryptoriskd-blocked-writer")
            .arg(&fifo),
    );
    let mut daemon = DaemonProcess::spawn(root.path(), &fifo, SecretOpenMode::ReadOnly);
    wait_until_executable_is_cryptoriskd(&daemon);
    thread::sleep(Duration::from_millis(100));
    assert!(
        daemon
            .child
            .try_wait()
            .expect("blocked daemon status must read")
            .is_none(),
        "daemon must be deliberately blocked before readiness"
    );

    let status = daemon.signal_and_wait("-TERM", EXIT_TIMEOUT);
    let output = daemon.capture();
    assert!(
        status.success(),
        "startup SIGTERM must be handled cleanly: status={status:?}, stderr={}",
        output.stderr
    );
    assert!(
        output.stdout.lines().next().is_none(),
        "cancelled startup must not announce readiness"
    );
    assert!(
        output.stderr.is_empty(),
        "cancelled startup must not emit a failure diagnostic"
    );
    assert_no_secret_material(&output);
    writer.stop();
}

#[test]
fn signal_after_runtime_sync_before_readiness_suppresses_stdout() {
    let root = runtime_root();
    let secret_path = root.path().join("session-secret.bin");
    fs::write(&secret_path, SECRET_BYTES).expect("session secret source must write");
    let gate = root.path().join("pre-readiness-gate");
    let status = Command::new("mkfifo")
        .arg(&gate)
        .status()
        .expect("mkfifo helper must run");
    assert!(status.success(), "readiness gate FIFO must create");
    let mut writer = ChildGuard::spawn(
        Command::new("/bin/sh")
            .arg("-c")
            .arg("exec 4> \"$1\"; exec sleep 30")
            .arg("cryptoriskd-readiness-gate-writer")
            .arg(&gate),
    );
    let mut daemon = DaemonProcess::spawn_with_readiness_gate(root.path(), &secret_path, &gate);
    wait_until_executable_is_cryptoriskd(&daemon);
    wait_for_log_event(
        &root.path().join("logs/cmti.jsonl"),
        "fixture_runtime_ready",
    );
    assert!(
        daemon
            .child
            .try_wait()
            .expect("gated daemon status must read")
            .is_none(),
        "daemon must remain alive at the pre-readiness gate"
    );
    assert!(
        matches!(
            daemon.readiness_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ),
        "gated daemon must not write readiness before the final cancellation check"
    );

    let status = daemon.signal_and_wait("-TERM", EXIT_TIMEOUT);
    let output = daemon.capture();
    assert!(
        status.success(),
        "late startup SIGTERM must be handled cleanly: status={status:?}, stderr={}",
        output.stderr
    );
    assert!(
        output.stdout.lines().next().is_none(),
        "late cancellation must suppress readiness stdout"
    );
    assert!(output.stderr.is_empty());
    assert_no_secret_material(&output);
    writer.stop();
}

#[test]
fn existing_log_fifo_fails_promptly_without_blocking_startup() {
    let root = runtime_root();
    let secret_path = root.path().join("session-secret.bin");
    fs::write(&secret_path, SECRET_BYTES).expect("session secret source must write");
    let log_fifo = root.path().join("logs/cmti.jsonl");
    let status = Command::new("mkfifo")
        .arg(&log_fifo)
        .status()
        .expect("mkfifo helper must run");
    assert!(status.success(), "log FIFO must create");

    let mut daemon = DaemonProcess::spawn(root.path(), &secret_path, SecretOpenMode::ReadOnly);
    let status = daemon.wait_for_exit(Duration::from_secs(1));
    assert!(
        !status.success(),
        "nonregular existing log must fail startup"
    );
    let output = daemon.capture();
    assert!(
        output.stdout.lines().next().is_none(),
        "log FIFO failure must happen before readiness"
    );
    assert!(
        output.stderr.contains("local JSON log open failed"),
        "failure must identify the log boundary without blocking"
    );
    assert_no_secret_material(&output);
}

async fn assert_authenticated_snapshot(readiness: &Readiness) {
    assert_eq!(
        readiness.expiry_unix_seconds - readiness.issued_unix_seconds,
        60
    );
    let process_nonce: [u8; 16] = hex::decode(&readiness.process_nonce)
        .expect("process nonce must be hex")
        .try_into()
        .expect("process nonce must be 16 bytes");
    let server_nonce: [u8; 16] = hex::decode(&readiness.server_nonce)
        .expect("server nonce must be hex")
        .try_into()
        .expect("server nonce must be 16 bytes");
    let descriptor = SessionDescriptor::issue(
        readiness.protocol_major,
        readiness.protocol_minor,
        readiness.daemon_pid,
        process_nonce,
        server_nonce,
        readiness.issued_unix_seconds,
    )
    .expect("readiness descriptor must reconstruct");
    let secret = SessionSecret::try_from(Zeroizing::new(SECRET_BYTES.to_vec()))
        .expect("test secret must be exact");
    let token = SessionAuthenticator::new(secret).token(&descriptor);
    let channel = Endpoint::from_shared(readiness.endpoint.clone())
        .expect("readiness endpoint must parse")
        .connect()
        .await
        .expect("real daemon RPC must connect");
    let mut market = MarketServiceClient::new(channel);
    let snapshot = market
        .get_snapshot(insert_authentication_metadata(
            Request::new(GetSnapshotRequest {}),
            &descriptor,
            &token,
        ))
        .await
        .expect("real daemon RPC must authenticate")
        .into_inner();
    assert_eq!(snapshot.sequence, 102);
    assert_eq!(snapshot.best_bid, "60000.1");
    assert_eq!(snapshot.best_ask, "60000.2");
    assert_eq!(snapshot.health, SnapshotHealth::Healthy as i32);
}

fn assert_fd_table_has_no_secret(daemon: &DaemonProcess, secret_path: &Path) {
    let output = Command::new("/usr/sbin/lsof")
        .args(["-a", "-p", &daemon.id().to_string(), "-Fn"])
        .output()
        .expect("lsof must inspect the real daemon");
    assert!(output.status.success(), "lsof inspection must succeed");
    let table = String::from_utf8(output.stdout).expect("lsof output must be UTF-8");
    assert!(
        !table.contains(&secret_path.to_string_lossy().into_owned()),
        "inherited secret descriptor must be closed after one read"
    );
    assert!(
        table.contains("market.wal"),
        "retained WAL fd must be visible"
    );
    assert!(
        table.contains("cmti.jsonl"),
        "retained log fd must be visible"
    );
}

fn assert_no_secret_material(output: &CapturedOutput) {
    let encoded = hex::encode(SECRET_BYTES);
    assert!(!output.stdout.contains(&encoded));
    assert!(!output.stderr.contains(&encoded));
}

fn wait_until_executable_is_cryptoriskd(daemon: &DaemonProcess) {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        let output = Command::new("ps")
            .args(["-p", &daemon.id().to_string(), "-o", "ucomm="])
            .output()
            .expect("ps helper must run");
        let executable = String::from_utf8_lossy(&output.stdout);
        let threads = Command::new("ps")
            .args(["-M", &daemon.id().to_string()])
            .output()
            .expect("thread inspection helper must run");
        if executable.trim() == "cryptoriskd"
            && String::from_utf8_lossy(&threads.stdout).lines().count() > 2
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "blocked startup must reach the daemon executable"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_log_event(path: &Path, event: &str) {
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        if fs::read_to_string(path)
            .ok()
            .is_some_and(|contents| contents.contains(event))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon log must contain {event} within five seconds"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn runtime_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("temporary process root must exist");
    fs::create_dir(root.path().join("data")).expect("data root must exist");
    fs::create_dir(root.path().join("logs")).expect("log root must exist");
    fs::write(root.path().join("fixture.jsonl"), FIXTURE).expect("fixture must write");
    fs::write(
        root.path().join("config.toml"),
        r#"
schema_version = 1
data_root = "data"
log_root = "logs"
fixture_input = "fixture.jsonl"
bind_address = "127.0.0.1:0"
session_secret_fd = 3
ingestion_queue_capacity = 1024
maximum_request_bytes = 8388608
maximum_concurrent_requests = 128
request_timeout_seconds = 30
shutdown_grace_seconds = 5
remote_export = false
remote_telemetry = false
"#,
    )
    .expect("process config must write");
    root
}

#[derive(Clone, Copy)]
enum SecretOpenMode {
    ReadOnly,
}

struct DaemonProcess {
    child: Child,
    readiness_receiver: Receiver<String>,
    stdout_thread: Option<JoinHandle<String>>,
    stderr_thread: Option<JoinHandle<String>>,
}

impl DaemonProcess {
    fn spawn(root: &Path, secret_path: &Path, mode: SecretOpenMode) -> Self {
        let redirect = match mode {
            SecretOpenMode::ReadOnly => "exec 3< \"$1\"",
        };
        Self::spawn_with_script(
            root,
            secret_path,
            None,
            &format!("{redirect}; exec \"$2\" --approved-root \"$3\" --config \"$4\""),
        )
    }

    fn spawn_with_readiness_gate(root: &Path, secret_path: &Path, gate: &Path) -> Self {
        Self::spawn_with_script(
            root,
            secret_path,
            Some(gate),
            "exec 3< \"$1\"; exec 4< \"$5\"; exec \"$2\" --approved-root \"$3\" --config \"$4\" --readiness-gate-fd 4",
        )
    }

    fn spawn_with_script(
        root: &Path,
        secret_path: &Path,
        gate: Option<&Path>,
        script: &str,
    ) -> Self {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .arg("cryptoriskd-process-test")
            .arg(secret_path)
            .arg(env!("CARGO_BIN_EXE_cryptoriskd"))
            .arg(root)
            .arg(root.join("config.toml"));
        if let Some(gate) = gate {
            command.arg(gate);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("real cryptoriskd process must spawn");
        let stdout = child.stdout.take().expect("stdout must be captured");
        let stderr = child.stderr.take().expect("stderr must be captured");
        let (readiness_sender, readiness_receiver) = mpsc::channel();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut output = String::new();
            let _ = reader.read_line(&mut output);
            let _ = readiness_sender.send(output.clone());
            reader
                .read_to_string(&mut output)
                .expect("stdout remainder must read");
            output
        });
        let stderr_thread = thread::spawn(move || {
            let mut output = String::new();
            BufReader::new(stderr)
                .read_to_string(&mut output)
                .expect("stderr must read");
            output
        });
        Self {
            child,
            readiness_receiver,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn readiness(&self) -> Readiness {
        let line = self
            .readiness_receiver
            .recv_timeout(EXIT_TIMEOUT)
            .expect("daemon readiness must arrive within five seconds");
        serde_json::from_str(&line).expect("readiness must be one JSON line")
    }

    fn signal_and_wait(&mut self, signal: &str, timeout: Duration) -> ExitStatus {
        let status = Command::new("kill")
            .arg(signal)
            .arg(self.id().to_string())
            .status()
            .expect("signal helper must run");
        assert!(status.success(), "signal delivery must succeed");
        self.wait_for_exit(timeout)
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("process status must read") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "daemon must exit within five seconds"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn capture(&mut self) -> CapturedOutput {
        CapturedOutput {
            stdout: self
                .stdout_thread
                .take()
                .expect("stdout capture must exist")
                .join()
                .expect("stdout capture must join"),
            stderr: self
                .stderr_thread
                .take()
                .expect("stderr capture must exist")
                .join()
                .expect("stderr capture must join"),
        }
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct CapturedOutput {
    stdout: String,
    stderr: String,
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self(Some(
            command.spawn().expect("guarded helper process must spawn"),
        ))
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}
