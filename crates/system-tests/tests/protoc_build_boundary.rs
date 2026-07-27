#![cfg(unix)]

use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("system-tests must be nested in the workspace")
        .to_path_buf()
}

fn path_protoc() -> PathBuf {
    env::split_paths(&env::var_os("PATH").expect("test PATH must be present"))
        .map(|directory| directory.join("protoc"))
        .find(|candidate| candidate.is_file())
        .expect("the pinned protoc must be discoverable on PATH")
        .canonicalize()
        .expect("the PATH protoc must canonicalize")
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\"'\"'"))
}

fn write_wrapper(path: &Path, source: &str) -> io::Result<()> {
    fs::create_dir_all(
        path.parent()
            .expect("wrapper path must have a parent directory"),
    )?;
    fs::write(path, source)?;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)
}

fn cargo_check(target: &Path, path_directory: &Path, protoc_override: &Path) -> Output {
    let path = env::join_paths(std::iter::once(path_directory.to_path_buf()).chain(
        env::split_paths(&env::var_os("PATH").expect("test PATH must be present")),
    ))
    .expect("test PATH must be joinable");

    Command::new(env!("CARGO"))
        .args(["check", "-p", "local-api", "--locked"])
        .current_dir(workspace_root())
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_TARGET_DIR", target)
        .env("PATH", path)
        .env("PROTOC", protoc_override)
        .output()
        .expect("nested Cargo check must run")
}

#[test]
fn cargo_build_time_protoc_identity_is_fail_closed() {
    let temporary = tempfile::Builder::new()
        .prefix("cmti-protoc-build-boundary-")
        .tempdir()
        .expect("temporary build boundary must be created");
    let target = temporary.path().join("target");
    let real_protoc = path_protoc();

    let drift_directory = temporary.path().join("drift-bin");
    let drift_protoc = drift_directory.join("protoc");
    let drift_generation_marker = temporary.path().join("drift-generated");
    write_wrapper(
        &drift_protoc,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'libprotoc 0.0\\n'\n  exit 0\nfi\n: > {}\nexit 0\n",
            shell_quote(&drift_generation_marker)
        ),
    )
    .expect("drift wrapper must be written");
    let drift = cargo_check(&target, &drift_directory, &drift_protoc);

    let valid_directory = temporary.path().join("valid-bin");
    let valid_protoc = valid_directory.join("protoc");
    let validation_marker = temporary.path().join("valid-validated");
    let generation_marker = temporary.path().join("valid-generated");
    write_wrapper(
        &valid_protoc,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  : > {}\n  exec {} --version\nfi\n: > {}\nexec {} \"$@\"\n",
            shell_quote(&validation_marker),
            shell_quote(&real_protoc),
            shell_quote(&generation_marker),
            shell_quote(&real_protoc),
        ),
    )
    .expect("valid wrapper must be written");
    let valid = cargo_check(&target, &valid_directory, &valid_protoc);

    let override_directory = temporary.path().join("override-bin");
    let override_protoc = override_directory.join("protoc");
    let override_generation_marker = temporary.path().join("override-generated");
    write_wrapper(
        &override_protoc,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  exec {} --version\nfi\n: > {}\nexec {} \"$@\"\n",
            shell_quote(&real_protoc),
            shell_quote(&override_generation_marker),
            shell_quote(&real_protoc),
        ),
    )
    .expect("override wrapper must be written");
    let mismatch = cargo_check(&target, &valid_directory, &override_protoc);

    let drift_stderr = String::from_utf8_lossy(&drift.stderr);
    assert!(!drift.status.success(), "drifted protoc must fail");
    assert!(
        drift_stderr.contains("protoc version mismatch")
            && drift_stderr.contains("libprotoc 33.4")
            && drift_stderr.contains("libprotoc 0.0"),
        "unexpected drift failure: {drift_stderr}"
    );
    assert!(
        !drift_generation_marker.exists(),
        "drifted protoc must be rejected before generation"
    );

    assert!(
        valid.status.success(),
        "validated protoc build failed: {}",
        String::from_utf8_lossy(&valid.stderr)
    );
    assert!(
        validation_marker.exists(),
        "the generation executable must first be validated"
    );
    assert!(
        generation_marker.exists(),
        "the same validated executable must perform generation"
    );

    let mismatch_stderr = String::from_utf8_lossy(&mismatch.stderr);
    assert!(
        !mismatch.status.success(),
        "a different PROTOC override must fail"
    );
    assert!(
        mismatch_stderr.contains("PROTOC override does not match the PATH protoc"),
        "unexpected override failure: {mismatch_stderr}"
    );
    assert!(
        !override_generation_marker.exists(),
        "a mismatched override must be rejected before generation"
    );
}
