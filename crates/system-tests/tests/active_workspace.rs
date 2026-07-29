use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;

const UNKNOWN_COMMAND_EXIT_CODE: i32 = 2;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("system-tests must be nested in the workspace")
        .to_path_buf()
}

fn cargo(args: &[&str]) -> Output {
    Command::new(env!("CARGO"))
        .args(args)
        .current_dir(workspace_root())
        .output()
        .expect("cargo command should run")
}

fn xtask(arguments: &[&str]) -> Output {
    let mut cargo_arguments = vec!["run", "--locked", "-p", "xtask", "--"];
    cargo_arguments.extend_from_slice(arguments);
    cargo(&cargo_arguments)
}

#[test]
fn active_workspace_contains_only_implemented_packages() {
    let output = cargo(&["metadata", "--locked", "--format-version", "1"]);
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata must be JSON");
    let workspace_members = metadata["workspace_members"]
        .as_array()
        .expect("metadata must include workspace members")
        .iter()
        .map(|member| member.as_str().expect("workspace member must be a string"))
        .collect::<BTreeSet<_>>();
    let package_names = metadata["packages"]
        .as_array()
        .expect("metadata must include packages")
        .iter()
        .filter(|package| {
            package["id"]
                .as_str()
                .is_some_and(|id| workspace_members.contains(id))
        })
        .map(|package| {
            package["name"]
                .as_str()
                .expect("workspace package must have a name")
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(
        package_names,
        BTreeSet::from([
            "capacity",
            "config",
            "consolidated-market",
            "connector-binance",
            "connector-bybit",
            "connector-certify",
            "connector-core",
            "connector-deribit",
            "connector-kraken",
            "collector-runtime",
            "crypto-evaluate",
            "crypto-replay",
            "cryptoriskd",
            "dataset",
            "domain",
            "event-envelope",
            "feature-engine",
            "feature-registry",
            "fixed-decimal",
            "instrument-registry",
            "labels",
            "local-api",
            "metadata-store",
            "observability",
            "orderbook",
            "parquet-store",
            "quality",
            "raw-wal",
            "replay-engine",
            "system-tests",
            "volatility",
            "xtask",
        ])
    );
}

#[test]
fn supported_xtask_commands_succeed() {
    for command in ["help", "proto-check", "workspace-check"] {
        let output = xtask(&[command]);
        assert!(
            output.status.success(),
            "xtask {command} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = xtask(&["generate-config-schema", "--check"]);
    assert!(
        output.status.success(),
        "xtask generate-config-schema --check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn unknown_xtask_commands_fail_with_a_stable_exit_code() {
    let output = xtask(&["not-a-command"]);

    assert_eq!(output.status.code(), Some(UNKNOWN_COMMAND_EXIT_CODE));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("error[unknown-command]"),
        "unexpected error output: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
