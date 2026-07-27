use std::path::Path;

#[test]
fn required_workspace_paths_exist() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("system-tests must be nested in the workspace");

    for path in [
        "Cargo.toml",
        "rust-toolchain.toml",
        "crates/domain/src/lib.rs",
        "xtask/src/main.rs",
        "crates/system-tests/Cargo.toml",
        "proto",
        "docs/adr",
    ] {
        assert!(workspace_root.join(path).exists(), "missing {path}");
    }
}
