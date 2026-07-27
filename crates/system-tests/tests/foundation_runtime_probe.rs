use std::process::Command;

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
