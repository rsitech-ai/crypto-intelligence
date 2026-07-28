use std::{
    collections::BTreeSet,
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
};

#[path = "../../build-support/generated_binding_policy.rs"]
mod generated_binding_policy;
#[path = "../../build-support/protoc_toolchain.rs"]
mod protoc_toolchain;

const PROTO_FILES: [&str; 3] = [
    "common/v1/common.proto",
    "health/v1/health.proto",
    "market/v1/market.proto",
];
const GENERATED_FILES: [&str; 3] = [
    "cmti.common.v1.rs",
    "cmti.health.v1.rs",
    "cmti.market.v1.rs",
];

fn main() -> Result<(), Box<dyn Error>> {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "CARGO_MANIFEST_DIR is missing")
        })?);
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("local-api must be nested in the workspace"))?;
    let proto_root = workspace_root.join("proto");
    let toolchain_path = proto_root.join("toolchain.toml");
    println!("cargo:rerun-if-changed={}", toolchain_path.display());
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root
            .join("build-support/protoc_toolchain.rs")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root
            .join("build-support/generated_binding_policy.rs")
            .display()
    );
    println!("cargo:rerun-if-env-changed=PATH");
    println!("cargo:rerun-if-env-changed=PROTOC");
    let toolchain = protoc_toolchain::read_contract(workspace_root)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let protoc = protoc_toolchain::resolve_and_validate(&toolchain)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let proto_files = PROTO_FILES
        .map(|relative| proto_root.join(relative))
        .to_vec();

    for proto_file in &proto_files {
        if !proto_file.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "required protobuf input is missing: {}",
                    proto_file.display()
                ),
            )
            .into());
        }
        println!("cargo:rerun-if-changed={}", proto_file.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        proto_root.join("buf.yaml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        proto_root.join("buf.gen.yaml").display()
    );

    let mut prost_config = prost_build::Config::new();
    prost_config.protoc_executable(protoc.path());
    tonic_prost_build::configure()
        .build_client(false)
        .build_server(true)
        .compile_with_config(prost_config, &proto_files, &[proto_root])?;

    let output_dir = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "OUT_DIR is missing"))?,
    );
    verify_generated_outputs(&output_dir)?;
    verify_server_only_outputs(&output_dir)?;
    Ok(())
}

fn verify_generated_outputs(output_dir: &Path) -> Result<(), io::Error> {
    let expected = GENERATED_FILES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let actual = fs::read_dir(output_dir)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();

    if actual != expected {
        return Err(io::Error::other(format!(
            "generated protobuf outputs differ: expected {expected:?}, got {actual:?}"
        )));
    }
    Ok(())
}

fn verify_server_only_outputs(output_dir: &Path) -> Result<(), io::Error> {
    for generated_file in GENERATED_FILES {
        let generated = fs::read_to_string(output_dir.join(generated_file))?;
        if let Some(module) = generated_binding_policy::grpc_client_module(&generated) {
            return Err(io::Error::other(format!(
                "generated protobuf output {generated_file} contains forbidden client module {module}"
            )));
        }
    }
    Ok(())
}
