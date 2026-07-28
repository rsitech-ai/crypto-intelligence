use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("system-tests must live two levels below the repository root")
        .to_path_buf()
}

fn read_repository_file(path: &str) -> String {
    fs::read_to_string(repository_root().join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

#[test]
fn artifact_inventory_separates_code_docs_and_distributable_categories() {
    let inventory = read_repository_file("licenses/artifact-provenance.toml");
    let required = [
        "schema_version = 1",
        "code_license = \"MIT OR Apache-2.0\"",
        "docs_license = \"MIT OR Apache-2.0\"",
        "name = \"fixture\"",
        "name = \"dataset\"",
        "name = \"model\"",
        "name = \"generated-schema\"",
        "name = \"vendored-source\"",
        "fixtures/binance/btcusdt-book-v1.jsonl",
        "configs/schema.json",
        "apps/macos/Vendor/grpc-swift-nio-transport/UPSTREAM.md",
        "apps/macos/Vendor/grpc-swift-nio-transport/UPSTREAM_FILES.sha256",
        "apps/macos/Vendor/grpc-swift-nio-transport/LICENSE",
        "apps/macos/Vendor/grpc-swift-nio-transport/NOTICES.txt",
    ];

    for term in required {
        assert!(
            inventory.contains(term),
            "artifact inventory is missing `{term}`"
        );
    }

    for placeholder in ["planned contract", "TODO", "TBD", "placeholder"] {
        assert!(
            !inventory.contains(placeholder),
            "artifact inventory still contains placeholder text `{placeholder}`"
        );
    }
}

#[test]
fn human_readable_fixture_and_model_inventories_are_not_scaffolds() {
    for path in [
        "licenses/fixture-manifest.yaml",
        "licenses/model-manifest.yaml",
    ] {
        let contents = read_repository_file(path);
        assert!(
            contents.contains("schema_version: 1"),
            "{path} does not declare its schema"
        );
        assert!(
            contents.contains("authoritative_inventory: licenses/artifact-provenance.toml"),
            "{path} does not point to the authoritative inventory"
        );
        assert!(
            !contents.contains("planned contract"),
            "{path} still contains scaffold text"
        );
    }
}

#[test]
fn xtask_license_check_accepts_the_repository_policy() {
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "--locked",
            "--offline",
            "-p",
            "xtask",
            "--",
            "license-check",
        ])
        .current_dir(repository_root())
        .output()
        .expect("failed to execute xtask license-check");

    assert!(
        output.status.success(),
        "xtask license-check failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
