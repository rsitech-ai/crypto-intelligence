use std::process::Command;

fn readiness(endpoint: &str, issued: i64, expiry: i64) -> serde_json::Value {
    serde_json::json!({
        "endpoint": endpoint,
        "protocol_major": 1,
        "protocol_minor": 0,
        "daemon_pid": 42,
        "process_nonce": "000102030405060708090a0b0c0d0e0f",
        "server_nonce": "101112131415161718191a1b1c1d1e1f",
        "issued_unix_seconds": issued,
        "expiry_unix_seconds": expiry,
    })
}

fn run_probe(readiness: &serde_json::Value) -> std::process::Output {
    let directory = tempfile::tempdir().expect("probe test root must exist");
    let readiness_path = directory.path().join("readiness.json");
    let secret = directory.path().join("secret.bin");
    std::fs::write(
        &readiness_path,
        serde_json::to_vec(readiness).expect("readiness fixture must serialize"),
    )
    .expect("readiness fixture must write");
    std::fs::write(&secret, [0x7b; 32]).expect("test secret must write");

    Command::new(env!("CARGO_BIN_EXE_foundation-runtime-probe"))
        .args([
            "--readiness",
            readiness_path
                .to_str()
                .expect("readiness path must be UTF-8"),
            "--secret",
            secret.to_str().expect("secret path must be UTF-8"),
            "--mode",
            "healthy",
        ])
        .output()
        .expect("probe must launch")
}

#[test]
fn probe_rejects_invalid_readiness_without_echoing_secret_material() {
    let directory = tempfile::tempdir().expect("probe test root must exist");
    let readiness = directory.path().join("readiness.json");
    let secret = directory.path().join("secret.bin");
    std::fs::write(&readiness, b"not-json").expect("invalid readiness must write");
    std::fs::write(&secret, [0x7b; 32]).expect("test secret must write");

    let output = Command::new(env!("CARGO_BIN_EXE_foundation-runtime-probe"))
        .args([
            "--readiness",
            readiness.to_str().expect("readiness path must be UTF-8"),
            "--secret",
            secret.to_str().expect("secret path must be UTF-8"),
            "--mode",
            "healthy",
        ])
        .output()
        .expect("probe must launch");

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout must be UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr must be UTF-8");
    assert!(stdout.is_empty());
    assert!(stderr.contains("readiness document is invalid"));
    let encoded_secret = hex::encode([0x7b; 32]);
    assert!(!stderr.contains(&encoded_secret));
}

#[test]
fn probe_rejects_every_noncanonical_loopback_endpoint_before_connecting() {
    for endpoint in [
        "https://127.0.0.1:43127",
        "http://localhost:43127",
        "http://[::1]:43127",
        "http://127.0.0.1",
        "http://127.0.0.1:0",
        "http://127.0.0.1:not-a-port",
        "http://127.0.0.1:043127",
        "http://127.0.0.1:43127/",
        "http://127.0.0.1:43127/path",
        "http://127.0.0.1:43127?query",
        "http://127.0.0.1:43127#fragment",
        "http://127.0.0.1:43127@exchange.invalid:80",
        "http://127.0.0.1:43127.exchange.invalid:80",
    ] {
        let output = run_probe(&readiness(endpoint, 1_700_000_000, 1_700_000_060));
        assert!(
            !output.status.success(),
            "noncanonical endpoint was accepted: {endpoint}"
        );
        let stderr = String::from_utf8(output.stderr).expect("stderr must be UTF-8");
        assert!(
            stderr.contains("readiness document is invalid"),
            "endpoint was not rejected at the readiness boundary: {endpoint}: {stderr}"
        );
        assert!(!stderr.contains("local daemon connection failed"));
        assert!(!stderr.contains("readiness endpoint is invalid"));
    }
}

#[test]
fn probe_rejects_timestamp_subtraction_overflow_without_panicking() {
    for (issued, expiry) in [(i64::MIN, i64::MAX), (i64::MAX, i64::MIN)] {
        let output = run_probe(&readiness("http://127.0.0.1:43127", issued, expiry));
        assert!(!output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("stderr must be UTF-8");
        assert!(stderr.contains("readiness document is invalid"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert!(!stderr.contains("overflow"), "{stderr}");
    }
}
