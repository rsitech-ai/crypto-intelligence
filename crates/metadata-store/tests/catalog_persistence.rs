use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, UnixNanos, VenueId,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use instrument_registry::{CatalogMutation, CorrectionInput, RegistryError, RevisionMetadata};
use metadata_store::{CatalogRequest, MetadataStore, StoreError, StoreOptions};
use rusqlite::Connection;
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

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("canonical fixture decimal")
}

fn definition() -> InstrumentDefinition {
    definition_with_delisting(None)
}

fn definition_with_delisting(delisting: Option<i64>) -> InstrumentDefinition {
    let quote = AssetId::new(AssetNamespace::Fiat, "", "", "USD", 1).expect("quote asset");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new(VenueId::new("coinbase").expect("venue"), "BTC-USD", 1)
            .expect("instrument"),
        product_type: ProductType::Spot,
        base_asset: AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1)
            .expect("base asset"),
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: decimal("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::None,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: Price::new(decimal("0.01")).expect("price tick"),
        quantity_step: Quantity::new(decimal("0.0001")).expect("quantity step"),
        listing_time: UnixNanos::new(100),
        delisting_time: delisting.map(UnixNanos::new),
    })
    .expect("valid definition")
}

fn request(request_id: &str, actor: &str) -> CatalogRequest {
    CatalogRequest::try_new(
        request_id,
        actor,
        None,
        vec![CatalogMutation::Definition {
            definition: definition(),
            metadata: RevisionMetadata::try_new(UnixNanos::new(200), "coinbase-products-1")
                .expect("revision metadata"),
        }],
    )
    .expect("catalog request")
}

#[test]
fn catalog_request_rejects_more_than_the_registry_batch_limit() {
    let mutation = CatalogMutation::Definition {
        definition: definition(),
        metadata: RevisionMetadata::try_new(UnixNanos::new(200), "oversized-batch")
            .expect("revision metadata"),
    };
    assert!(matches!(
        CatalogRequest::try_new(
            "oversized-catalog-request",
            "connector",
            None,
            vec![mutation; 65],
        ),
        Err(StoreError::InvalidCatalogRequest)
    ));
}

#[tokio::test]
async fn catalog_commit_is_exactly_once_and_rehydrates_after_restart() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");

    let first = store
        .append_catalog(request("catalog-request-1", "connector"))
        .await
        .expect("commit catalog");
    assert_eq!(first.appended_records, 1);
    assert_eq!(first.idempotent_records, 0);
    assert_eq!(first.committed_revision.expect("revision").get(), 1);

    let retry = store
        .append_catalog(request("catalog-request-1", "connector"))
        .await
        .expect("exact request retry");
    assert_eq!(retry, first);
    assert!(matches!(
        store
            .append_catalog(request("catalog-request-1", "different-actor"))
            .await,
        Err(StoreError::CatalogRequestConflict)
    ));

    let snapshot = store.catalog_snapshot().await.expect("catalog snapshot");
    assert!(snapshot.verify_integrity());
    assert_eq!(snapshot.records().len(), 1);
    assert_eq!(snapshot.catalog_digest(), &first.catalog_digest);
    assert_eq!(snapshot.history_digest(), &first.history_digest);

    let stale = CatalogRequest::try_new(
        "catalog-request-stale",
        "connector",
        None,
        vec![CatalogMutation::Definition {
            definition: definition(),
            metadata: RevisionMetadata::try_new(UnixNanos::new(200), "coinbase-products-1")
                .expect("revision metadata"),
        }],
    )
    .expect("stale request");
    assert!(matches!(
        store.append_catalog(stale).await,
        Err(StoreError::Catalog(RegistryError::RevisionConflict { .. }))
    ));
    assert_eq!(
        store
            .catalog_snapshot()
            .await
            .expect("snapshot after rejected request")
            .catalog_digest(),
        &first.catalog_digest
    );

    store.shutdown().await.expect("clean shutdown");
    let reopened = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("rehydrate metadata store");
    let rehydrated = reopened
        .catalog_snapshot()
        .await
        .expect("rehydrated catalog");
    assert_eq!(rehydrated.catalog_digest(), &first.catalog_digest);
    assert_eq!(rehydrated.history_digest(), &first.history_digest);
    assert_eq!(rehydrated.records().len(), 1);
    reopened.shutdown().await.expect("clean shutdown");

    let connection = Connection::open(&path).expect("inspect committed database");
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM catalog_requests", [], |row| row
                .get::<_, i64>(0))
            .expect("request count"),
        1
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM catalog_commits", [], |row| row
                .get::<_, i64>(0))
            .expect("commit count"),
        1
    );
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM catalog_records", [], |row| row
                .get::<_, i64>(0))
            .expect("record count"),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM audit_records WHERE category = 'instrument-catalog'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("catalog audit count"),
        1
    );
}

#[tokio::test]
async fn correction_history_rehydrates_through_the_validated_registry_path() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");
    let first = store
        .append_catalog(request("catalog-definition-1", "connector"))
        .await
        .expect("commit definition");
    let revision = first.committed_revision.expect("definition revision");
    let snapshot = store.catalog_snapshot().await.expect("definition snapshot");
    let prior_hash = *snapshot.records()[0].definition_hash();
    let correction = CorrectionInput::try_new(
        revision,
        prior_hash,
        definition_with_delisting(Some(10_000)),
        RevisionMetadata::try_new(UnixNanos::new(300), "operator-correction-1")
            .expect("correction metadata"),
        "venue corrected the price increment",
    )
    .expect("valid correction");
    let corrected = store
        .append_catalog(
            CatalogRequest::try_new(
                "catalog-correction-1",
                "operator",
                Some(revision),
                vec![CatalogMutation::Correction(correction)],
            )
            .expect("correction request"),
        )
        .await
        .expect("commit correction");
    assert_eq!(corrected.committed_revision.expect("revision").get(), 2);
    store.shutdown().await.expect("clean shutdown");

    let reopened = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("rehydrate corrected catalog");
    let snapshot = reopened
        .catalog_snapshot()
        .await
        .expect("corrected snapshot");
    assert_eq!(snapshot.records().len(), 2);
    assert_eq!(
        snapshot.entries()[0].definition().delisting_time(),
        Some(UnixNanos::new(10_000))
    );
    assert_eq!(snapshot.catalog_digest(), &corrected.catalog_digest);
    assert_eq!(snapshot.history_digest(), &corrected.history_digest);
    reopened.shutdown().await.expect("clean shutdown");
}

#[tokio::test]
async fn catalog_payload_corruption_fails_startup_closed() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");
    store
        .append_catalog(request("catalog-corruption-1", "connector"))
        .await
        .expect("commit catalog");
    store.shutdown().await.expect("clean shutdown");

    let connection = Connection::open(&path).expect("open database externally");
    let update_trigger = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'trigger' AND name = 'catalog_records_no_update'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read exact trigger definition");
    connection
        .execute_batch(
            "DROP TRIGGER catalog_records_no_update;
             UPDATE catalog_records SET payload = x'00' WHERE revision = 1 AND ordinal = 0;",
        )
        .expect("simulate record corruption");
    connection
        .execute_batch(&update_trigger)
        .expect("restore exact trigger definition");
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path, StoreOptions::test()).await,
        Err(StoreError::CatalogIntegrity)
    ));
}

#[tokio::test]
async fn oversized_persisted_catalog_count_fails_startup_before_allocation() {
    let directory = tempdir().expect("temporary directory");
    let path = private_store_directory(directory.path()).join("meta.sqlite");
    let store = MetadataStore::open(&path, StoreOptions::test())
        .await
        .expect("open metadata store");
    store
        .append_catalog(request("catalog-count-corruption-1", "connector"))
        .await
        .expect("commit catalog");
    store.shutdown().await.expect("clean shutdown");

    let connection = Connection::open(&path).expect("open database externally");
    let update_trigger = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'trigger' AND name = 'catalog_commits_no_update'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read exact trigger definition");
    connection
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             DROP TRIGGER catalog_commits_no_update;
             UPDATE catalog_commits SET record_count = 65 WHERE revision = 1;",
        )
        .expect("simulate oversized stored count");
    connection
        .execute_batch(&update_trigger)
        .expect("restore exact trigger definition");
    drop(connection);

    assert!(matches!(
        MetadataStore::open(&path, StoreOptions::test()).await,
        Err(StoreError::CatalogIntegrity)
    ));
}
