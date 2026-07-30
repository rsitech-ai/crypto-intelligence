use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/evaluation/cusp-v1")
        .canonicalize()
        .expect("canonical fixture")
}

fn run(dataset: &Path, output: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crypto-evaluate"))
        .args([
            "cusp-ablation",
            "--dataset",
            dataset.to_str().expect("dataset path"),
            "--output",
            output.to_str().expect("output path"),
        ])
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
fn research_only_ablation_bundle_is_reproducible_and_explicit() {
    let directory = tempfile::tempdir().expect("temporary root");
    let first = directory.path().join("first");
    let second = directory.path().join("second");
    let first_output = run(&fixture(), &first);
    let second_output = run(&fixture(), &second);

    assert!(first_output.status.success());
    assert!(second_output.status.success());
    assert!(first_output.stderr.is_empty());
    assert!(second_output.stderr.is_empty());
    assert_eq!(first_output.stdout, second_output.stdout);
    assert_eq!(files(&first), files(&second));

    let generated = files(&first);
    let report: serde_json::Value = serde_json::from_slice(
        generated
            .get("ablation-report.json")
            .expect("ablation report"),
    )
    .expect("report JSON");
    assert_eq!(report["decision"], "research_only");
    assert_eq!(
        report["production_probability_use"],
        "not_used_in_production_probability"
    );
    assert_eq!(
        report["gate"]["decision"],
        serde_json::json!("research_only")
    );
    assert!(
        report["gate"]["checks"]
            .as_array()
            .expect("checks")
            .iter()
            .any(|check| check["kind"] == "improved_fold_fraction" && check["passed"] == false)
    );

    let card = std::str::from_utf8(generated.get("model-card.md").expect("model card"))
        .expect("UTF-8 card");
    assert!(card.contains("Gate decision: `research_only`"));
    assert!(card.contains("not used in production probability"));
    assert!(!card.contains("{{"));

    let manifest: serde_json::Value =
        serde_json::from_slice(generated.get("manifest.json").expect("manifest"))
            .expect("manifest JSON");
    for name in [
        "ablation-report.json",
        "experiment-ledger.jsonl",
        "model-card.md",
    ] {
        assert_eq!(
            manifest["files_blake3"][name],
            blake3::hash(generated.get(name).expect("manifest file"))
                .to_hex()
                .to_string()
        );
    }
}

#[test]
fn passing_evidence_can_be_eligible_but_still_assigns_no_weight() {
    let directory = tempfile::tempdir().expect("temporary root");
    let dataset = directory.path().join("eligible-dataset");
    fs::create_dir(&dataset).expect("dataset");
    let mut input: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture().join("ablation-input.json")).expect("fixture"))
            .expect("fixture JSON");
    for fold in input["folds"].as_array_mut().expect("folds") {
        fold["candidate_brier_score"] = serde_json::json!(0.16);
    }
    fs::write(
        dataset.join("ablation-input.json"),
        serde_json::to_vec_pretty(&input).expect("eligible input"),
    )
    .expect("write eligible input");
    let output = directory.path().join("eligible-output");

    let result = run(&dataset, &output);
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("ablation-report.json")).expect("report"))
            .expect("report JSON");
    assert_eq!(report["decision"], "eligible_for_production_weight");
    assert_eq!(
        report["production_probability_use"],
        "not_used_in_production_probability"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("experiment-ledger.jsonl")).expect("ledger"))
            .expect("ledger JSON");
    assert_eq!(ledger["production_weight_assigned"], false);
}

#[test]
fn unsafe_inputs_and_output_collisions_fail_without_partial_bundle() {
    let directory = tempfile::tempdir().expect("temporary root");
    let linked_dataset = directory.path().join("linked-dataset");
    fs::create_dir(&linked_dataset).expect("linked dataset");
    symlink(
        fixture().join("ablation-input.json"),
        linked_dataset.join("ablation-input.json"),
    )
    .expect("input symlink");
    let linked_output = directory.path().join("linked-output");
    let linked = run(&linked_dataset, &linked_output);
    assert_eq!(linked.status.code(), Some(1));
    assert!(!linked_output.exists());

    let unknown_dataset = directory.path().join("unknown-dataset");
    fs::create_dir(&unknown_dataset).expect("unknown dataset");
    let mut unknown: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture().join("ablation-input.json")).expect("fixture"))
            .expect("fixture JSON");
    unknown
        .as_object_mut()
        .expect("object")
        .insert("manual_override".to_owned(), serde_json::json!(true));
    fs::write(
        unknown_dataset.join("ablation-input.json"),
        serde_json::to_vec(&unknown).expect("unknown input"),
    )
    .expect("write unknown input");
    let unknown_output = directory.path().join("unknown-output");
    let rejected = run(&unknown_dataset, &unknown_output);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(!unknown_output.exists());

    let existing = directory.path().join("existing");
    fs::create_dir(&existing).expect("existing output");
    fs::write(existing.join("sentinel"), b"preserve").expect("sentinel");
    let collision = run(&fixture(), &existing);
    assert_eq!(collision.status.code(), Some(1));
    assert_eq!(
        fs::read(existing.join("sentinel")).expect("sentinel"),
        b"preserve"
    );
    assert_eq!(fs::read_dir(&existing).expect("output").count(), 1);
}
