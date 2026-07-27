use config::{
    ConfigError, MAX_INGESTION_QUEUE_CAPACITY, MAX_REQUEST_TIMEOUT_SECONDS, Overrides, load, schema,
};
use std::{fs, path::Path};
use tempfile::TempDir;

fn fixture_root() -> TempDir {
    let root = tempfile::tempdir().expect("temporary root");
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
            .credential_ref
            .as_ref()
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

    assert_eq!(first.config.request_timeout_seconds, 10);
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
    assert_eq!(effective.config.ingestion_queue_capacity, 1024);
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
