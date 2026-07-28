use std::time::Duration;

use metadata_store::{AuditEvent, CheckpointMode, MetadataStore, StoreError, StoreOptions};
use rusqlite::{Connection, OpenFlags};
use tempfile::tempdir;

fn private_store_directory(root: &std::path::Path) -> std::path::PathBuf {
    let path = root.join("store");
    std::fs::create_dir(&path).expect("create private store directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("make store directory owner-private");
    }
    path
}

#[tokio::test]
async fn migration_is_idempotent_and_integrity_check_passes() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");

    store.migrate().await.expect("first migration check");
    store.migrate().await.expect("second migration check");
    assert_eq!(
        store.integrity_check().await.expect("integrity check"),
        "ok"
    );

    let sqlite = store.sqlite_info().await.expect("SQLite information");
    assert_eq!(sqlite.version, "3.53.4");
    assert_eq!(sqlite.journal_mode, "wal");
    assert_eq!(sqlite.synchronous, 2);
    assert!(sqlite.foreign_keys);
    assert!(!sqlite.trusted_schema);
    assert_eq!(sqlite.wal_autocheckpoint, 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        for protected_path in [
            path.clone(),
            path.with_extension("sqlite.lock"),
            path.with_extension("sqlite-wal"),
            path.with_extension("sqlite-shm"),
        ] {
            let metadata = std::fs::metadata(&protected_path).expect("protected SQLite file");
            assert_eq!(
                metadata.mode() & 0o077,
                0,
                "{} must not grant group or world access",
                protected_path.display()
            );
            assert_eq!(metadata.nlink(), 1);
        }
    }

    store.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn audit_rows_are_append_only_and_exact_retries_are_idempotent() {
    let store = MetadataStore::memory_for_test()
        .await
        .expect("open test metadata store");
    let event = AuditEvent::try_new(
        "audit-test-1",
        1_234,
        "system-test",
        "configuration",
        "cmti.test.v1",
        br#"{"enabled":true}"#.to_vec(),
    )
    .expect("valid audit event");
    let debug = format!("{event:?}");
    assert!(!debug.contains("enabled"));
    assert!(debug.contains("payload_bytes"));

    let first = store
        .append_audit(event.clone())
        .await
        .expect("append audit");
    let retry = store.append_audit(event).await.expect("retry audit");
    assert_eq!(retry, first);
    assert_eq!(first.payload(), br#"{"enabled":true}"#);
    #[cfg(feature = "test-support")]
    {
        assert!(store.delete_audit_for_test(&first.audit_id).await.is_err());
        assert!(store.update_audit_for_test(&first.audit_id).await.is_err());
    }

    let conflict = AuditEvent::try_new(
        "audit-test-1",
        1_234,
        "system-test",
        "configuration",
        "cmti.test.v1",
        br#"{"enabled":false}"#.to_vec(),
    )
    .expect("valid conflicting audit event");
    assert!(matches!(
        store.append_audit(conflict).await,
        Err(StoreError::AuditIdConflict)
    ));

    store.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn migration_checksum_mismatch_and_future_schema_fail_closed() {
    let directory = tempdir().expect("temporary directory");
    let store_directory = private_store_directory(directory.path());
    let mismatch_path = store_directory.join("mismatch.sqlite");
    MetadataStore::open(&mismatch_path, StoreOptions::test())
        .await
        .expect("open metadata store")
        .shutdown()
        .await
        .expect("clean shutdown");

    let connection = Connection::open(&mismatch_path).expect("open database externally");
    connection
        .execute_batch(
            "DROP TRIGGER schema_migrations_no_update;
             UPDATE schema_migrations SET checksum = zeroblob(32) WHERE version = 1;",
        )
        .expect("simulate migration tampering");
    drop(connection);
    assert!(matches!(
        MetadataStore::open(&mismatch_path, StoreOptions::test()).await,
        Err(StoreError::MigrationChecksumMismatch { version: 1 })
    ));

    let future_path = store_directory.join("future.sqlite");
    MetadataStore::open(&future_path, StoreOptions::test())
        .await
        .expect("open metadata store")
        .shutdown()
        .await
        .expect("clean shutdown");
    let connection = Connection::open(&future_path).expect("open database externally");
    connection
        .execute(
            "INSERT INTO schema_migrations(version, name, applied_at_ns, checksum)
             VALUES(2, 'future', 0, zeroblob(32))",
            [],
        )
        .expect("simulate future schema");
    drop(connection);
    assert!(matches!(
        MetadataStore::open(&future_path, StoreOptions::test()).await,
        Err(StoreError::UnknownSchemaVersion {
            found: 2,
            supported: 1
        })
    ));
}

#[tokio::test]
async fn explicit_checkpoint_is_observable_and_schema_has_no_high_rate_tables() {
    let directory = tempdir().expect("temporary directory");
    let store = MetadataStore::open(
        private_store_directory(directory.path()).join("meta.sqlite"),
        StoreOptions::test(),
    )
    .await
    .expect("open metadata store");

    store
        .append_audit_fixture()
        .await
        .expect("append fixture audit");
    let checkpoint = store
        .checkpoint(CheckpointMode::Passive)
        .await
        .expect("checkpoint");
    assert!(checkpoint.log_frames >= checkpoint.checkpointed_frames);
    assert!(checkpoint.elapsed <= Duration::from_secs(5));
    let health = store.health().await.expect("store health");
    assert_eq!(health.last_checkpoint, Some(checkpoint));
    assert!(health.last_checkpoint_at_ns.is_some());
    assert_eq!(health.last_checkpoint_ok, Some(true));
    store
        .integrity_check()
        .await
        .expect("record integrity health");
    let health = store.health().await.expect("updated store health");
    assert!(health.last_integrity_check_at_ns.is_some());
    assert_eq!(health.last_integrity_check_ok, Some(true));

    let tables = store.table_names_for_test().await.expect("table names");
    assert!(tables.contains(&"audit_records".to_owned()));
    for forbidden in ["trades", "order_books", "book_deltas", "raw_events"] {
        assert!(!tables.iter().any(|table| table == forbidden));
    }

    store.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn a_second_writer_observes_the_bounded_busy_timeout() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let options = StoreOptions::test().with_busy_timeout(Duration::from_millis(50));
    let store = MetadataStore::open(&path, options)
        .await
        .expect("open metadata store");

    let blocker = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("open blocking connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold external write lock");

    let error = store
        .append_audit_fixture()
        .await
        .expect_err("writer must report bounded busy");
    assert!(matches!(error, StoreError::DatabaseBusy));
    blocker.execute_batch("ROLLBACK").expect("release lock");

    store.shutdown().await.expect("clean shutdown");
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn simulated_disk_full_is_typed_and_rolls_back_the_audit_transaction() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");
    store
        .exhaust_page_capacity_for_test()
        .await
        .expect("set deterministic SQLite page limit");
    let event = AuditEvent::try_new(
        "disk-full-1",
        1,
        "fault-injection",
        "storage",
        "cmti.test.disk-full.v1",
        vec![0x5a; 64 * 1024],
    )
    .expect("maximum test audit payload");
    assert!(matches!(
        store.append_audit(event).await,
        Err(StoreError::DatabaseFull)
    ));
    store
        .shutdown()
        .await
        .expect("clean shutdown after rollback");

    let connection = Connection::open(&path).expect("inspect database after disk-full");
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM audit_records", [], |row| row
                .get::<_, i64>(0))
            .expect("audit row count"),
        0
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symbolic_link_database_targets_are_rejected() {
    use std::os::unix::fs::symlink;

    let directory = tempdir().expect("temporary directory");
    let store_directory = private_store_directory(directory.path());
    let real_path = store_directory.join("real.sqlite");
    Connection::open(&real_path).expect("create target");
    let link_path = store_directory.join("link.sqlite");
    symlink(&real_path, &link_path).expect("create database symlink");

    assert!(matches!(
        MetadataStore::open(&link_path, StoreOptions::test()).await,
        Err(StoreError::UnsafeDatabasePath)
    ));
}

#[tokio::test]
async fn process_lock_excludes_a_second_store_and_reader_is_read_only() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");

    assert!(matches!(
        MetadataStore::open(&path, StoreOptions::test()).await,
        Err(StoreError::StoreAlreadyLocked)
    ));

    let reader = store.reader().expect("open reader handle");
    assert_eq!(
        reader.integrity_check().await.expect("reader integrity"),
        "ok"
    );
    assert!(reader.attempt_write_for_test().await.is_err());

    store.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn startup_reports_clean_and_unclean_recovery_generations() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let first = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("first open");
    assert!(!first.startup_report().recovered_unclean_shutdown);
    assert_eq!(first.startup_report().startup_generation, 1);
    first.shutdown().await.expect("first clean shutdown");

    let second = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("second open");
    assert!(!second.startup_report().recovered_unclean_shutdown);
    assert_eq!(second.startup_report().startup_generation, 2);
    second.shutdown().await.expect("second clean shutdown");

    let connection = Connection::open(&path).expect("open database externally");
    connection
        .execute(
            "UPDATE runtime_state SET clean_shutdown = 0 WHERE singleton = 1",
            [],
        )
        .expect("simulate interrupted process");
    drop(connection);

    let recovered = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("unclean recovery");
    assert!(recovered.startup_report().recovered_unclean_shutdown);
    assert_eq!(recovered.startup_report().startup_generation, 3);
    recovered.shutdown().await.expect("recovery shutdown");
}

#[tokio::test]
async fn a_busy_final_checkpoint_never_persists_a_false_clean_marker() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");
    store
        .append_audit_fixture()
        .await
        .expect("create initial WAL frame");

    let blocker = Connection::open(&path).expect("open checkpoint-blocking reader");
    blocker
        .execute_batch("BEGIN")
        .expect("begin reader snapshot");
    blocker
        .query_row("SELECT COUNT(*) FROM audit_records", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("establish reader snapshot");
    store
        .append_audit_fixture()
        .await
        .expect("append after reader snapshot");

    assert!(matches!(
        store.shutdown().await,
        Err(StoreError::DatabaseBusy)
    ));
    blocker.execute_batch("ROLLBACK").expect("release reader");
    drop(blocker);

    let recovered = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("recover after failed final checkpoint");
    assert!(recovered.startup_report().recovered_unclean_shutdown);
    recovered.shutdown().await.expect("clean recovery shutdown");
}

#[tokio::test]
async fn audit_hash_corruption_fails_startup_closed() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");
    store
        .append_audit_fixture()
        .await
        .expect("append audit record");
    store.shutdown().await.expect("clean shutdown");

    let connection = Connection::open(&path).expect("open database externally");
    let audit_update_trigger = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'trigger' AND name = 'audit_records_no_update'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read exact trigger definition");
    connection
        .execute_batch(
            "DROP TRIGGER audit_records_no_update;
             UPDATE audit_records SET record_hash = zeroblob(32) WHERE sequence = 1;",
        )
        .expect("simulate audit corruption");
    connection
        .execute_batch(&audit_update_trigger)
        .expect("restore exact trigger definition");
    drop(connection);

    let error = MetadataStore::open(&path, StoreOptions::test())
        .await
        .err()
        .expect("audit corruption must fail startup");
    assert!(
        matches!(error, StoreError::AuditIntegrity),
        "unexpected startup error: {error:?}"
    );
}

#[tokio::test]
async fn audit_payload_corruption_fails_startup_closed() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");
    store
        .append_audit_fixture()
        .await
        .expect("append audit record");
    store.shutdown().await.expect("clean shutdown");

    let connection = Connection::open(&path).expect("open database externally");
    let audit_update_trigger = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'trigger' AND name = 'audit_records_no_update'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read exact trigger definition");
    connection
        .execute_batch(
            "DROP TRIGGER audit_records_no_update;
             UPDATE audit_records SET payload = x'00' WHERE sequence = 1;",
        )
        .expect("simulate audit payload corruption");
    connection
        .execute_batch(&audit_update_trigger)
        .expect("restore exact trigger definition");
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path, StoreOptions::test()).await,
        Err(StoreError::AuditIntegrity)
    ));
}

#[tokio::test]
async fn logical_schema_tampering_fails_startup_closed() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store")
        .shutdown()
        .await
        .expect("clean shutdown");

    let connection = Connection::open(&path).expect("open database externally");
    connection
        .execute_batch("DROP TRIGGER audit_records_no_update")
        .expect("simulate schema tampering");
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path, StoreOptions::test()).await,
        Err(StoreError::SchemaStateMismatch)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn foreign_database_is_rejected_without_mutation() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("foreign.sqlite");
    let connection = Connection::open(&path).expect("create foreign SQLite database");
    connection
        .execute("CREATE TABLE foreign_data(value TEXT)", [])
        .expect("create foreign schema");
    drop(connection);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("make foreign database private");
    let before = std::fs::read(&path).expect("read foreign database before open");

    assert!(matches!(
        MetadataStore::open(&path, StoreOptions::test()).await,
        Err(StoreError::UnclaimedNonEmptyDatabase)
    ));
    let after = std::fs::read(&path).expect("read foreign database after rejection");
    assert_eq!(after, before);
    assert!(!path.with_extension("sqlite-wal").exists());
    assert!(!path.with_extension("sqlite-shm").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn foreign_application_id_is_rejected_without_mutating_database_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("foreign.sqlite");
    let connection = Connection::open(&path).expect("create foreign SQLite database");
    connection
        .pragma_update(None, "application_id", 0x1234_5678_i64)
        .expect("set foreign application identity");
    drop(connection);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("make foreign database private");
    let before = std::fs::read(&path).expect("read foreign database before open");

    assert!(matches!(
        MetadataStore::open(&path, StoreOptions::test()).await,
        Err(StoreError::ApplicationIdMismatch)
    ));
    assert_eq!(
        std::fs::read(&path).expect("read foreign database after rejection"),
        before
    );
    assert!(!path.with_extension("sqlite-wal").exists());
    assert!(!path.with_extension("sqlite-shm").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn lock_symlink_and_hardlinked_database_are_rejected_without_touching_the_target() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = tempdir().expect("temporary directory");
    let store_directory = private_store_directory(directory.path());
    let database_path = store_directory.join("meta.sqlite");
    let lock_path = database_path.with_extension("sqlite.lock");
    let victim_path = store_directory.join("victim");
    std::fs::write(&victim_path, b"unchanged").expect("create victim");
    std::fs::set_permissions(&victim_path, std::fs::Permissions::from_mode(0o640))
        .expect("set victim mode");
    symlink(&victim_path, &lock_path).expect("create lock symlink");

    assert!(matches!(
        MetadataStore::open(&database_path, StoreOptions::test()).await,
        Err(StoreError::UnsafeDatabasePath)
    ));
    assert_eq!(
        std::fs::read(&victim_path).expect("read victim"),
        b"unchanged"
    );
    assert_eq!(
        std::fs::metadata(&victim_path)
            .expect("victim metadata")
            .permissions()
            .mode()
            & 0o777,
        0o640
    );

    let real_path = store_directory.join("real.sqlite");
    Connection::open(&real_path).expect("create real database");
    std::fs::set_permissions(&real_path, std::fs::Permissions::from_mode(0o600))
        .expect("make real database private");
    let alias_path = store_directory.join("alias.sqlite");
    std::fs::hard_link(&real_path, &alias_path).expect("create database hard link");
    assert!(matches!(
        MetadataStore::open(&alias_path, StoreOptions::test()).await,
        Err(StoreError::UnsafeDatabasePath)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn database_parent_must_be_owner_private() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().expect("temporary directory");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755))
        .expect("make directory non-private");
    assert!(matches!(
        MetadataStore::open(directory.path().join("meta.sqlite"), StoreOptions::test()).await,
        Err(StoreError::UnsafeDatabasePath)
    ));
}
