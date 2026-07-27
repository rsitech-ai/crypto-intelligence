use config::{
    ConfigError, MAX_CONCURRENT_REQUESTS, MAX_INGESTION_QUEUE_CAPACITY, MAX_REQUEST_BYTES,
    MAX_REQUEST_TIMEOUT_SECONDS, MAX_SHUTDOWN_GRACE_SECONDS, Overrides, load, schema,
};
use std::{fs, io::Write as _, path::Path};
use tempfile::TempDir;

fn fixture_root() -> TempDir {
    let root = tempfile::tempdir().expect("temporary root");
    fs::create_dir(root.path().join("data")).expect("data root is writable");
    fs::create_dir(root.path().join("logs")).expect("log root is writable");
    fs::write(root.path().join("fixture.jsonl"), "{}\n").expect("fixture is writable");
    root
}

fn valid_config() -> String {
    r#"
schema_version = 1
data_root = "data"
log_root = "logs"
fixture_input = "fixture.jsonl"
bind_address = "127.0.0.1:0"
session_secret_fd = 3
ingestion_queue_capacity = 1024
maximum_request_bytes = 8388608
maximum_concurrent_requests = 128
request_timeout_seconds = 30
shutdown_grace_seconds = 5
remote_export = false
remote_telemetry = false
"#
    .trim_start()
    .to_owned()
}

fn load_valid(text: &str, root: &Path) -> Result<config::EffectiveConfig, ConfigError> {
    load(text, "test-default", None, Overrides::default(), root)
}

#[test]
fn unknown_keys_and_direct_secret_material_are_rejected() {
    let root = fixture_root();
    let unknown = format!("{}unknown_critical_key = true\n", valid_config());
    assert!(matches!(
        load_valid(&unknown, root.path()),
        Err(ConfigError::Parse { .. })
    ));

    let secret = format!("{}api_secret = \"plain-text\"\n", valid_config());
    assert!(matches!(
        load_valid(&secret, root.path()),
        Err(ConfigError::EmbeddedSecret { .. })
    ));
}

#[test]
fn only_keychain_credential_references_are_accepted() {
    let root = fixture_root();
    let direct = format!(
        "{}credential_ref = \"env://BINANCE_API_KEY\"\n",
        valid_config()
    );
    assert!(matches!(
        load_valid(&direct, root.path()),
        Err(ConfigError::CredentialReference)
    ));

    let keychain = format!(
        "{}credential_ref = \"keychain://cmti.binance/read-only\"\n",
        valid_config()
    );
    let effective = load_valid(&keychain, root.path()).expect("Keychain reference is valid");
    assert_eq!(
        effective
            .config
            .credential_ref()
            .expect("credential reference")
            .service(),
        "cmti.binance"
    );
}

#[test]
fn binding_and_resource_limits_fail_closed() {
    let root = fixture_root();
    let remote = valid_config().replace("127.0.0.1:0", "0.0.0.0:9000");
    assert!(matches!(
        load_valid(&remote, root.path()),
        Err(ConfigError::NonLoopbackBind)
    ));

    for (from, to) in [
        (
            "ingestion_queue_capacity = 1024".to_owned(),
            "ingestion_queue_capacity = 0".to_owned(),
        ),
        (
            "ingestion_queue_capacity = 1024".to_owned(),
            format!(
                "ingestion_queue_capacity = {}",
                MAX_INGESTION_QUEUE_CAPACITY + 1
            ),
        ),
        (
            "request_timeout_seconds = 30".to_owned(),
            "request_timeout_seconds = 0".to_owned(),
        ),
        (
            "request_timeout_seconds = 30".to_owned(),
            format!(
                "request_timeout_seconds = {}",
                MAX_REQUEST_TIMEOUT_SECONDS + 1
            ),
        ),
    ] {
        let invalid = valid_config().replace(&from, &to);
        assert!(
            matches!(
                load_valid(&invalid, root.path()),
                Err(ConfigError::OutOfRange { .. })
            ),
            "{to} must fail closed"
        );
    }
}

#[test]
fn runtime_binding_matches_the_exact_schema_rule() {
    let root = fixture_root();
    let other_loopback = valid_config().replace("127.0.0.1:0", "127.0.0.2:0");

    assert!(matches!(
        load_valid(&other_loopback, root.path()),
        Err(ConfigError::NonLoopbackBind)
    ));
}

#[test]
fn every_configured_limit_accepts_bounds_and_rejects_outside_values() {
    let root = fixture_root();
    let limits = [
        ("session_secret_fd = 3", 3_u64, u64::from(u32::MAX)),
        (
            "ingestion_queue_capacity = 1024",
            1,
            u64::try_from(MAX_INGESTION_QUEUE_CAPACITY).expect("capacity fits u64"),
        ),
        (
            "maximum_request_bytes = 8388608",
            1,
            u64::try_from(MAX_REQUEST_BYTES).expect("request bytes fit u64"),
        ),
        (
            "maximum_concurrent_requests = 128",
            1,
            u64::try_from(MAX_CONCURRENT_REQUESTS).expect("request count fits u64"),
        ),
        (
            "request_timeout_seconds = 30",
            1,
            MAX_REQUEST_TIMEOUT_SECONDS,
        ),
        ("shutdown_grace_seconds = 5", 1, MAX_SHUTDOWN_GRACE_SECONDS),
    ];

    for (field, minimum, maximum) in limits {
        for accepted in [minimum, maximum] {
            let text = valid_config().replace(field, &format_field(field, accepted));
            load_valid(&text, root.path())
                .unwrap_or_else(|error| panic!("{field}={accepted} must be accepted: {error}"));
        }

        let below = minimum - 1;
        let text = valid_config().replace(field, &format_field(field, below));
        assert!(
            matches!(
                load_valid(&text, root.path()),
                Err(ConfigError::OutOfRange { .. })
            ),
            "{field}={below} must be rejected"
        );

        if maximum < u64::from(u32::MAX) {
            let above = maximum + 1;
            let text = valid_config().replace(field, &format_field(field, above));
            assert!(
                matches!(
                    load_valid(&text, root.path()),
                    Err(ConfigError::OutOfRange { .. })
                ),
                "{field}={above} must be rejected"
            );
        }
    }
}

#[test]
fn each_remote_capability_flag_is_rejected_independently() {
    let root = fixture_root();
    for field in ["remote_export", "remote_telemetry"] {
        let text = valid_config().replace(&format!("{field} = false"), &format!("{field} = true"));
        assert!(
            matches!(
                load_valid(&text, root.path()),
                Err(ConfigError::RemoteCapabilityEnabled)
            ),
            "{field}=true must fail closed"
        );
    }
}

#[test]
fn writable_roots_must_already_exist() {
    let root = fixture_root();
    fs::remove_dir(root.path().join("data")).expect("remove data root");

    assert!(matches!(
        load_valid(&valid_config(), root.path()),
        Err(ConfigError::Filesystem {
            field: "data_root",
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn writable_roots_cannot_escape_through_symlinks() {
    use std::os::unix::fs::symlink;

    let approved = fixture_root();
    let outside = tempfile::tempdir().expect("outside root");
    symlink(outside.path(), approved.path().join("escaped-data")).expect("symlink");
    let escaped = valid_config().replace("data_root = \"data\"", "data_root = \"escaped-data\"");

    assert!(matches!(
        load_valid(&escaped, approved.path()),
        Err(ConfigError::PathEscape { field: "data_root" })
    ));
}

#[cfg(unix)]
#[test]
fn validated_directory_handle_survives_path_replacement() {
    use std::os::unix::fs::symlink;

    let root = fixture_root();
    let outside = tempfile::tempdir().expect("outside root");
    let effective =
        load_valid(&valid_config(), root.path()).expect("configuration must validate first");
    let retained_data_root = effective.paths.data_root.clone();

    fs::rename(root.path().join("data"), root.path().join("moved-data"))
        .expect("move validated directory");
    symlink(outside.path(), root.path().join("data")).expect("replace path with outside symlink");

    let mut file = retained_data_root
        .create_new_file("sentinel")
        .expect("retained descriptor remains authoritative");
    file.write_all(b"retained").expect("write through handle");

    assert_eq!(
        fs::read(root.path().join("moved-data/sentinel"))
            .expect("original directory receives file"),
        b"retained"
    );
    assert!(
        !outside.path().join("sentinel").exists(),
        "runtime access must not follow the replacement symlink"
    );
}

fn format_field(field: &str, value: u64) -> String {
    let name = field.split_once(" = ").expect("field assignment").0;
    format!("{name} = {value}")
}

#[test]
fn layering_and_fingerprint_are_deterministic() {
    let root = fixture_root();
    let user_a = "maximum_concurrent_requests = 64\nrequest_timeout_seconds = 20\n";
    let user_b = "request_timeout_seconds = 20\nmaximum_concurrent_requests = 64\n";
    let overrides = Overrides {
        request_timeout_seconds: Some(10),
        ..Overrides::default()
    };

    let first = load(
        &valid_config(),
        "default",
        Some((user_a, "user")),
        overrides.clone(),
        root.path(),
    )
    .expect("first layered config");
    let second = load(
        &valid_config(),
        "default",
        Some((user_b, "user")),
        overrides,
        root.path(),
    )
    .expect("second layered config");

    assert_eq!(first.config.request_timeout_seconds(), 10);
    assert_eq!(first.fingerprint, second.fingerprint);
    assert_eq!(first.sources, ["default", "user", "overrides"]);
}

#[test]
fn checked_in_default_and_schema_match_the_canonical_generators() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate must be under workspace root");
    let default_text =
        fs::read_to_string(workspace.join("configs/default.toml")).expect("checked-in default");
    let effective = load(
        &default_text,
        "configs/default.toml",
        None,
        Overrides::default(),
        workspace,
    )
    .expect("checked-in default validates");
    assert_eq!(effective.config.ingestion_queue_capacity(), 1024);
    assert!(!effective.fingerprint.is_empty());

    let checked_in =
        fs::read_to_string(workspace.join("configs/schema.json")).expect("checked-in schema");
    assert!(
        checked_in.contains("^keychain://"),
        "schema must encode the Keychain-only credential boundary"
    );
    let schema_json: serde_json::Value =
        serde_json::from_str(&checked_in).expect("checked-in schema is JSON");
    assert!(
        !schema_json["required"]
            .as_array()
            .expect("schema required fields")
            .iter()
            .any(|field| field == "credential_ref"),
        "optional credential_ref must not become a required TOML field"
    );
    assert_eq!(checked_in, schema::generate().expect("schema generation"));
}
