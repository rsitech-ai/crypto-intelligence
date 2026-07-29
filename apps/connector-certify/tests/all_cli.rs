use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn certify_all() -> Output {
    let root = workspace_root();
    Command::new(env!("CARGO_BIN_EXE_connector-certify"))
        .args([
            "--all",
            "--fixtures",
            &root.join("fixtures/exchanges").display().to_string(),
            "--check",
        ])
        .current_dir(root)
        .output()
        .expect("run connector-certify")
}

#[test]
fn all_check_is_stable_and_covers_every_v1_venue() {
    let first = certify_all();
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        first.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = certify_all();
    assert!(second.status.success());
    assert_eq!(first.stdout, second.stdout);

    let line = std::str::from_utf8(&first.stdout)
        .expect("UTF-8")
        .trim_end();
    let prefix = "connector-certify: FIXTURE_PASS venues=binance,bybit,deribit,kraken production_certification=BLOCKED sha256=";
    let hash = line.strip_prefix(prefix).expect("stable aggregate summary");
    assert_eq!(
        hash,
        "30b94376709566177bf9b7d4a10866add7cf4b8f5adbe46af70b412fcc0bd3b7"
    );
}

#[test]
fn all_mode_rejects_generation_to_prevent_partial_writes() {
    let root = workspace_root();
    let output = Command::new(env!("CARGO_BIN_EXE_connector-certify"))
        .args([
            "--all",
            "--fixtures",
            &root.join("fixtures/exchanges").display().to_string(),
        ])
        .current_dir(root)
        .output()
        .expect("run connector-certify");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).expect("UTF-8"),
        "connector-certify: FAIL: --all requires --check to prevent partial artifact writes\n"
    );
}
