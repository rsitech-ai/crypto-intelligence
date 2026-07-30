use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/golden-replays/features-v1/manifest.toml")
}

fn run(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crypto-evaluate"))
        .args(arguments)
        .output()
        .expect("crypto-evaluate must run")
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(root)
        .expect("output directory")
        .map(|entry| {
            let entry = entry.expect("output entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).expect("output file"),
            )
        })
        .collect()
}

#[test]
fn prescribed_evaluation_is_reproducible_and_holds_candidate_status() {
    let directory = tempfile::tempdir().expect("temporary root");
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let manifest = fixture();
    let first_output = run(&[
        "--manifest",
        manifest.to_str().expect("fixture path"),
        "--output",
        first.to_str().expect("output path"),
    ]);
    let second_output = run(&[
        "evaluate",
        "--manifest",
        manifest.to_str().expect("fixture path"),
        "--output",
        second.to_str().expect("output path"),
    ]);
    assert!(first_output.status.success());
    assert!(second_output.status.success());
    assert!(first_output.stderr.is_empty());
    assert!(second_output.stderr.is_empty());
    assert_eq!(first_output.stdout, second_output.stdout);
    assert_eq!(files(&first), files(&second));

    let report: serde_json::Value = serde_json::from_slice(
        files(&first)
            .get("evaluation-report.json")
            .expect("evaluation report"),
    )
    .expect("report JSON");
    assert_eq!(report["status"], "evidence_only");
    assert_eq!(report["candidate_eligibility"], "held");
    assert_eq!(report["observation_count"], 3);
    assert_eq!(report["fold_metrics"], serde_json::json!([]));
    assert_eq!(report["candidate_package_blake3"], serde_json::Value::Null);
}

#[test]
fn unsafe_inputs_and_output_collisions_fail_without_partial_evidence() {
    let directory = tempfile::tempdir().expect("temporary root");
    let linked_manifest = directory.path().join("manifest-link.toml");
    symlink(fixture(), &linked_manifest).expect("manifest symlink");
    let linked_output = directory.path().join("linked-output");
    let linked = run(&[
        "--manifest",
        linked_manifest.to_str().expect("link path"),
        "--output",
        linked_output.to_str().expect("output path"),
    ]);
    assert_eq!(linked.status.code(), Some(1));
    assert!(!linked_output.exists());

    let outside = tempfile::tempdir().expect("outside directory");
    let symlink_parent = directory.path().join("symlink-parent");
    symlink(outside.path(), &symlink_parent).expect("output parent symlink");
    let traversed_output = symlink_parent.join("evidence");
    let traversed = run(&[
        "--manifest",
        fixture().to_str().expect("fixture path"),
        "--output",
        traversed_output.to_str().expect("output path"),
    ]);
    assert_eq!(traversed.status.code(), Some(1));
    assert!(!outside.path().join("evidence").exists());

    let nested_symlink_parent = directory.path().join("nested-symlink-parent");
    symlink(outside.path(), &nested_symlink_parent).expect("nested output ancestor symlink");
    let nested_traversed_output = nested_symlink_parent.join("created").join("evidence");
    let nested_traversed = run(&[
        "--manifest",
        fixture().to_str().expect("fixture path"),
        "--output",
        nested_traversed_output.to_str().expect("output path"),
    ]);
    assert_eq!(nested_traversed.status.code(), Some(1));
    assert!(!outside.path().join("created").exists());

    let existing = directory.path().join("existing");
    fs::create_dir(&existing).expect("existing output");
    fs::write(existing.join("sentinel"), b"preserve").expect("sentinel");
    let collision = run(&[
        "--manifest",
        fixture().to_str().expect("fixture path"),
        "--output",
        existing.to_str().expect("output path"),
    ]);
    assert_eq!(collision.status.code(), Some(1));
    assert_eq!(
        fs::read(existing.join("sentinel")).expect("sentinel"),
        b"preserve"
    );
    assert_eq!(fs::read_dir(&existing).expect("output").count(), 1);
}
