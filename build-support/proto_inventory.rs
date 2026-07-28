use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtoInput {
    pub relative_path: String,
    pub package: String,
}

pub fn discover(proto_root: &Path) -> Result<Vec<ProtoInput>, io::Error> {
    let schema_root = proto_root.join("cmti");
    let mut paths = Vec::new();
    collect_proto_paths(&schema_root, &mut paths)?;
    paths.sort();

    let mut packages = BTreeSet::new();
    let mut inputs = Vec::with_capacity(paths.len());
    for path in paths {
        let relative_path = path
            .strip_prefix(proto_root)
            .map_err(|_| io::Error::other("protobuf path escaped its root"))?
            .to_str()
            .ok_or_else(|| io::Error::other("protobuf paths must be UTF-8"))?
            .replace('\\', "/");
        let source = fs::read_to_string(&path)?;
        let package = source
            .lines()
            .map(str::trim)
            .find_map(|line| {
                line.strip_prefix("package ")
                    .and_then(|value| value.strip_suffix(';'))
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                io::Error::other(format!(
                    "protobuf input {} has no package declaration",
                    path.display()
                ))
            })?;
        if !packages.insert(package.clone()) {
            return Err(io::Error::other(format!(
                "duplicate protobuf package {package}"
            )));
        }
        inputs.push(ProtoInput {
            relative_path,
            package,
        });
    }
    if inputs.is_empty() {
        return Err(io::Error::other("no protobuf inputs discovered"));
    }
    Ok(inputs)
}

fn collect_proto_paths(directory: &Path, paths: &mut Vec<PathBuf>) -> Result<(), io::Error> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_proto_paths(&path, paths)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("proto") {
            paths.push(path);
        }
    }
    Ok(())
}

pub fn generated_rust_files(inputs: &[ProtoInput]) -> BTreeSet<String> {
    inputs
        .iter()
        .map(|input| format!("{}.rs", input.package))
        .collect()
}
