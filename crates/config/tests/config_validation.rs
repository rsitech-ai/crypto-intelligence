use config::{
    ChangeContext, ChangeImpact, ConfigError, ConfigLayers, EnvironmentOverrides, LogLevel,
    MAX_CONCURRENT_REQUESTS, MAX_INGESTION_QUEUE_CAPACITY, MAX_REQUEST_BYTES,
    MAX_REQUEST_TIMEOUT_SECONDS, MAX_SHUTDOWN_GRACE_SECONDS, Overrides, RuntimeOverrides,
    TextLayer, audit_change, load, load_layers, read_bounded_file, schema,
};
use std::{fs, io::Write as _, path::Path};
use tempfile::TempDir;

fn fixture_root() -> TempDir {
    let root = tempfile::tempdir().expect("temporary root");
    fs::create_dir(root.path().join("data")).expect("data root is writable");
    fs::create_dir(root.path().join("logs")).expect("log root is writable");
    fs::create_dir_all(root.path().join("models/production")).expect("model registry is readable");
    fs::write(root.path().join("fixture.jsonl"), "{}\n").expect("fixture is writable");
    root
}

#[cfg(unix)]
#[test]
fn bounded_configuration_reads_use_one_regular_nofollow_descriptor() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("temporary config root");
    let config_path = root.path().join("config.toml");
    fs::write(&config_path, "schema_version = 1\n").expect("config fixture");
    assert_eq!(
        read_bounded_file(&config_path, 64).expect("regular bounded config must read"),
        "schema_version = 1\n"
    );

    let link_path = root.path().join("config-link.toml");
    symlink(&config_path, &link_path).expect("config symlink");
    assert!(
        read_bounded_file(&link_path, 64).is_err(),
        "the configuration reader must not follow a substituted final symlink"
    );

    fs::write(&config_path, vec![b'x'; 65]).expect("oversized config fixture");
    assert!(
        read_bounded_file(&config_path, 64).is_err(),
        "the configuration reader must enforce its bound while reading"
    );
}

fn valid_config() -> String {
    r#"
schema_version = 1
profile = "production-local"
timezone = "Europe/Warsaw"

[daemon]
data_dir = "data"
log_dir = "logs"
fixture_input = "fixture.jsonl"
bind_address = "127.0.0.1:0"
session_secret_fd = 3
ingestion_queue_capacity = 1024
maximum_request_bytes = 8388608
maximum_concurrent_requests = 128
request_timeout_seconds = 30
shutdown_grace_seconds = 5
wal_flush_interval_ms = 250
max_memory_gib = 40
log_level = "info"
wal_format = "v1"
schema_compatibility = "strict"
database_engine = "sqlite"
runtime_threads = 4

[coverage]
max_tier_a_instruments = 20
max_tier_b_instruments = 100
reject_over_capacity = true
watchlist = ["BTC"]

[[venues]]
id = "binance"
enabled = true
coverage_tier = "a"
products = ["spot", "linear_perpetual"]
instrument_budget = 10
raw_retention_days = 14

[models]
production_registry = "models/production"
allow_experimental_views = true
allow_core_ai = true
allow_foundation_models = true
abstain_on_incompatible_model = true
selected_view = "production"

[alerts]
local_notifications = true
outbound_webhooks = false
minimum_quality_ppm = 900000

[privacy]
remote_export = false
remote_telemetry = false
automatic_crash_upload = false
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

    for secret_name in ["client-secret", "bearerToken", "mnemonic"] {
        let secret = format!("{}{secret_name} = \"plain-text\"\n", valid_config());
        assert!(
            matches!(
                load_valid(&secret, root.path()),
                Err(ConfigError::EmbeddedSecret { .. })
            ),
            "{secret_name} must be recognized as secret material"
        );
    }
}

#[test]
fn parse_and_override_errors_do_not_retain_untrusted_values() {
    let root = fixture_root();
    let sentinel = "CMTI_SENTINEL_DO_NOT_LOG";
    let invalid = format!("{}unknown_field = \"{sentinel}\"\n", valid_config());
    let error = load_valid(&invalid, root.path()).expect_err("unknown field must fail");
    assert!(!error.to_string().contains(sentinel));
    assert!(!format!("{error:?}").contains(sentinel));

    let error = EnvironmentOverrides::from_pairs([("CMTI_LOG_LEVEL", sentinel)])
        .expect_err("invalid environment value must fail");
    assert!(!error.to_string().contains(sentinel));
    assert!(!format!("{error:?}").contains(sentinel));
}

#[test]
fn only_keychain_credential_references_are_accepted() {
    let root = fixture_root();
    let direct = valid_config().replace(
        "raw_retention_days = 14",
        "raw_retention_days = 14\ncredential_ref = \"env://BINANCE_API_KEY\"",
    );
    assert!(matches!(
        load_valid(&direct, root.path()),
        Err(ConfigError::CredentialReference)
    ));

    let keychain = valid_config().replace(
        "raw_retention_days = 14",
        "raw_retention_days = 14\ncredential_ref = \"keychain://cmti.binance/read-only\"",
    );
    let effective = load_valid(&keychain, root.path()).expect("Keychain reference is valid");
    assert_eq!(
        effective.config.venues()[0]
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
    for invalid_address in ["127.0.0.2:0", "127.0.0.1:00", "127.0.0.1:00000"] {
        let invalid = valid_config().replace("127.0.0.1:0", invalid_address);
        assert!(
            matches!(
                load_valid(&invalid, root.path()),
                Err(ConfigError::NonLoopbackBind)
            ),
            "{invalid_address} must not cross the schema/runtime boundary"
        );
    }
}

#[test]
fn every_configured_limit_accepts_bounds_and_rejects_outside_values() {
    let root = fixture_root();
    let limits = [
        (
            "session_secret_fd = 3",
            3_u64,
            u64::try_from(i32::MAX).expect("positive RawFd bound"),
        ),
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
    for field in [
        "remote_export",
        "remote_telemetry",
        "automatic_crash_upload",
        "outbound_webhooks",
    ] {
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
            field: "data_dir",
            ..
        })
    ));
}

#[test]
fn production_model_registry_must_exist_as_a_readable_local_directory() {
    let root = fixture_root();
    fs::remove_dir(root.path().join("models/production")).expect("remove model registry");
    assert!(matches!(
        load_valid(&valid_config(), root.path()),
        Err(ConfigError::Filesystem {
            field: "production_registry",
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
    let escaped = valid_config().replace("data_dir = \"data\"", "data_dir = \"escaped-data\"");

    assert!(matches!(
        load_valid(&escaped, approved.path()),
        Err(ConfigError::PathEscape { field: "data_dir" })
    ));

    fs::remove_dir_all(approved.path().join("models/production")).expect("remove model registry");
    symlink(outside.path(), approved.path().join("models/production"))
        .expect("model registry symlink");
    assert!(matches!(
        load_valid(&valid_config(), approved.path()),
        Err(ConfigError::PathEscape {
            field: "production_registry"
        })
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
    let user_a = "[daemon]\nmaximum_concurrent_requests = 64\nrequest_timeout_seconds = 20\n";
    let user_b = "[daemon]\nrequest_timeout_seconds = 20\nmaximum_concurrent_requests = 64\n";
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
    assert_eq!(first.sources, ["default", "user", "command-line"]);
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
    assert_eq!(
        effective.fingerprint, "a60828375e5f6ff5c7956db401a6d75efddfeaf37e7eda2053a931b9904e26a3",
        "the domain-separated canonical default fingerprint is a frozen contract"
    );
    effective
        .config
        .validate_foundation_runtime()
        .expect("checked-in default must describe only the active foundation runtime");

    let future_root = fixture_root();
    let future_contract = load_valid(&valid_config(), future_root.path())
        .expect("broader contract fixture must remain valid");
    assert!(matches!(
        future_contract.config.validate_foundation_runtime(),
        Err(ConfigError::UnsupportedRuntimeCapability { .. })
    ));

    let checked_in =
        fs::read_to_string(workspace.join("configs/schema.json")).expect("checked-in schema");
    assert!(
        checked_in.contains("^keychain://"),
        "schema must encode the Keychain-only credential boundary"
    );
    let schema_json: serde_json::Value =
        serde_json::from_str(&checked_in).expect("checked-in schema is JSON");
    assert!(
        !schema_json["definitions"]["RawVenueConfig"]["required"]
            .as_array()
            .expect("venue schema required fields")
            .iter()
            .any(|field| field == "credential_ref"),
        "optional credential_ref must not become a required TOML field"
    );
    assert_eq!(checked_in, schema::generate().expect("schema generation"));
}

#[test]
fn all_six_layers_apply_in_the_approved_precedence_order() {
    let root = fixture_root();
    let defaults = valid_config();
    let environment = EnvironmentOverrides::from_pairs([("CMTI_LOG_LEVEL", "debug")])
        .expect("supported environment override");
    let layers = ConfigLayers {
        compiled_defaults: TextLayer::new(&defaults, "compiled-defaults"),
        system_policy: Some(TextLayer::new(
            "[daemon]\nlog_level = \"warn\"\n",
            "system-policy",
        )),
        user_config: Some(TextLayer::new(
            "[daemon]\nlog_level = \"info\"\n",
            "user-config",
        )),
        environment,
        command_line: Overrides {
            log_level: Some(LogLevel::Trace),
            ..Overrides::default()
        },
        runtime_settings: RuntimeOverrides {
            log_level: Some(LogLevel::Error),
            ..RuntimeOverrides::default()
        },
    };

    let effective = load_layers(layers, root.path()).expect("all layers must assemble");
    assert_eq!(effective.config.log_level(), LogLevel::Error);
    assert_eq!(
        effective.sources,
        [
            "compiled-defaults",
            "system-policy",
            "user-config",
            "environment",
            "command-line",
            "runtime-settings",
        ]
    );
}

#[test]
fn environment_overrides_are_an_explicit_non_secret_allowlist() {
    assert!(matches!(
        EnvironmentOverrides::from_pairs([("CMTI_API_SECRET", "plain")]),
        Err(ConfigError::UnsupportedOverride { .. })
    ));
    assert!(matches!(
        EnvironmentOverrides::from_pairs([("CMTI_LOG_LEVEL", "verbose")]),
        Err(ConfigError::InvalidOverride { .. })
    ));
}

#[test]
fn venue_capacity_retention_and_model_policy_fail_closed() {
    let root = fixture_root();
    for invalid in [
        valid_config().replace("id = \"binance\"", "id = \"BINANCE\""),
        valid_config().replace("raw_retention_days = 14", "raw_retention_days = 31"),
        valid_config().replace("instrument_budget = 10", "instrument_budget = 21"),
        valid_config().replace(
            "products = [\"spot\", \"linear_perpetual\"]",
            "products = [\"option\"]",
        ),
        valid_config().replace(
            "max_tier_a_instruments = 20",
            "max_tier_a_instruments = 10001",
        ),
        valid_config().replace(
            "abstain_on_incompatible_model = true",
            "abstain_on_incompatible_model = false",
        ),
        valid_config()
            .replace(
                "allow_experimental_views = true",
                "allow_experimental_views = false",
            )
            .replace(
                "selected_view = \"production\"",
                "selected_view = \"experimental\"",
            ),
    ] {
        assert!(
            load_valid(&invalid, root.path()).is_err(),
            "invalid domain configuration must fail closed"
        );
    }

    let duplicate = valid_config().replace(
        "[models]",
        "[[venues]]\nid = \"binance\"\nenabled = false\ncoverage_tier = \"b\"\nproducts = [\"spot\"]\ninstrument_budget = 1\nraw_retention_days = 7\n\n[models]",
    );
    assert!(matches!(
        load_valid(&duplicate, root.path()),
        Err(ConfigError::DuplicateVenue { .. })
    ));
}

#[test]
fn dynamic_and_restart_required_changes_emit_complete_canonical_audit_records() {
    let root = fixture_root();
    let original =
        load_valid(&valid_config(), root.path()).expect("original configuration must load");
    let dynamic = load_valid(
        &valid_config().replace("log_level = \"info\"", "log_level = \"debug\""),
        root.path(),
    )
    .expect("dynamic configuration must load");
    let context = ChangeContext {
        actor: "local-user".to_owned(),
        changed_at_unix_nanos: 1_700_000_000_000_000_000,
        restart_occurred: false,
        rollback_reference: "config-revision-41".to_owned(),
    };
    let dynamic_record =
        audit_change(&original, &dynamic, context.clone()).expect("dynamic change must audit");
    assert_eq!(dynamic_record.impact, ChangeImpact::Dynamic);
    assert_eq!(dynamic_record.old_hash, original.fingerprint);
    assert_eq!(dynamic_record.new_hash, dynamic.fingerprint);
    assert_ne!(dynamic_record.old_hash, dynamic_record.new_hash);
    assert!(dynamic_record.affected_sources.is_empty());
    assert!(dynamic_record.affected_models.is_empty());

    let coverage = load_valid(
        &valid_config().replace("watchlist = [\"BTC\"]", "watchlist = [\"ETH\"]"),
        root.path(),
    )
    .expect("dynamic coverage configuration must load");
    let coverage_record =
        audit_change(&original, &coverage, context.clone()).expect("coverage change must audit");
    assert_eq!(coverage_record.impact, ChangeImpact::Dynamic);
    assert_eq!(coverage_record.affected_sources, ["binance"]);

    let model = load_valid(
        &valid_config().replace(
            "selected_view = \"production\"",
            "selected_view = \"experimental\"",
        ),
        root.path(),
    )
    .expect("dynamic model view must load");
    let model_record =
        audit_change(&original, &model, context.clone()).expect("model change must audit");
    assert_eq!(model_record.impact, ChangeImpact::Dynamic);
    assert_eq!(model_record.affected_models, ["experimental", "production"]);

    let restart = load_valid(
        &valid_config().replace("runtime_threads = 4", "runtime_threads = 6"),
        root.path(),
    )
    .expect("restart configuration must load");
    let restart_record =
        audit_change(&original, &restart, context).expect("restart change must audit");
    assert_eq!(restart_record.impact, ChangeImpact::RestartRequired);
    assert!(!restart_record.restart_occurred);
    assert_eq!(restart_record.actor, "local-user");
    assert_eq!(restart_record.rollback_reference, "config-revision-41");
}
