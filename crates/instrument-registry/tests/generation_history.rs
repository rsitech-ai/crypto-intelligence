use std::{num::NonZeroUsize, sync::Arc};

use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, OptionSide, ProductType, UnixNanos, VenueId,
};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use instrument_registry::{
    AppendOutcome, CatalogMutation, CatalogRecord, CorrectionInput, InstrumentRegistry,
    RegistryError, RegistryLimits, RegistryLimitsInput, ResolveError, RevisionMetadata,
};

fn decimal(value: &str) -> FixedDecimal {
    FixedDecimal::parse_canonical(value).expect("canonical fixture decimal")
}

fn price(value: &str) -> Price {
    Price::new(decimal(value)).expect("positive fixture price")
}

fn quantity(value: &str) -> Quantity {
    Quantity::new(decimal(value)).expect("nonnegative fixture quantity")
}

fn native_asset(chain: &str, symbol: &str) -> AssetId {
    AssetId::new(AssetNamespace::Native, chain, "", symbol, 1).expect("native fixture asset")
}

fn fiat_asset(symbol: &str) -> AssetId {
    AssetId::new(AssetNamespace::Fiat, "", "", symbol, 1).expect("fiat fixture asset")
}

fn spot_definition(
    venue: &str,
    symbol: &str,
    generation: u32,
    listing: i64,
    delisting: Option<i64>,
) -> InstrumentDefinition {
    let quote = fiat_asset("USD");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new(
            VenueId::new(venue).expect("fixture venue"),
            symbol,
            generation,
        )
        .expect("fixture instrument"),
        product_type: ProductType::Spot,
        base_asset: native_asset("bitcoin", "BTC"),
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: decimal("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::None,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.01"),
        quantity_step: quantity("0.001"),
        listing_time: UnixNanos::new(listing),
        delisting_time: delisting.map(UnixNanos::new),
    })
    .expect("valid fixture definition")
}

fn perpetual_definition(
    venue: &str,
    symbol: &str,
    generation: u32,
    listing: i64,
) -> InstrumentDefinition {
    let quote = fiat_asset("USD");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new_for_product(
            VenueId::new(venue).expect("fixture venue"),
            symbol,
            ProductType::Perpetual,
            generation,
        )
        .expect("fixture instrument"),
        product_type: ProductType::Perpetual,
        base_asset: native_asset("bitcoin", "BTC"),
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: decimal("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::Linear,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.01"),
        quantity_step: quantity("0.001"),
        listing_time: UnixNanos::new(listing),
        delisting_time: None,
    })
    .expect("valid fixture definition")
}

fn option_definition() -> InstrumentDefinition {
    let quote = fiat_asset("USD");
    InstrumentDefinition::new(InstrumentDefinitionInput {
        id: InstrumentId::new_for_product(
            VenueId::new("deribit").expect("fixture venue"),
            "BTC-30JUN30-50000-C",
            ProductType::Option,
            7,
        )
        .expect("fixture option"),
        product_type: ProductType::Option,
        base_asset: native_asset("bitcoin", "BTC"),
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: decimal("1"),
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::Linear,
        expiry_time: Some(UnixNanos::new(900)),
        strike: Some(price("50000")),
        option_side: Some(OptionSide::Call),
        price_tick: price("0.0001"),
        quantity_step: quantity("0.1"),
        listing_time: UnixNanos::new(100),
        delisting_time: Some(UnixNanos::new(900)),
    })
    .expect("valid option definition")
}

fn metadata(known_at: i64, source: &str) -> RevisionMetadata {
    RevisionMetadata::try_new(UnixNanos::new(known_at), source).expect("fixture revision metadata")
}

fn digest_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn appended_revision(outcome: AppendOutcome) -> instrument_registry::CatalogRevision {
    match outcome {
        AppendOutcome::Appended { revision } => revision,
        AppendOutcome::AlreadyPresent { .. } => panic!("fixture append must create a record"),
    }
}

fn correction(
    registry: &InstrumentRegistry,
    definition: &InstrumentDefinition,
    replacement: InstrumentDefinition,
    known_at: i64,
) -> CorrectionInput {
    let current = registry
        .definition_revision(definition.id())
        .expect("current definition revision");
    CorrectionInput::try_new(
        current.revision(),
        *current.definition_hash(),
        replacement,
        metadata(known_at, "fixture:instrument-correction"),
        "venue metadata correction",
    )
    .expect("valid correction input")
}

#[test]
fn pinned_snapshots_resolve_half_open_intervals_without_guessing_across_gaps() {
    let venue = VenueId::new("binance").expect("fixture venue");
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 1, 100, Some(200)),
            metadata(10, "fixture:generation-1"),
        )
        .expect("first generation");
    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 2, 400, None),
            metadata(20, "fixture:generation-2"),
        )
        .expect("second generation");
    let snapshot = registry.snapshot().expect("current snapshot");

    assert_eq!(
        snapshot
            .resolve(&venue, "abcusd", UnixNanos::new(100))
            .expect("listing boundary")
            .definition()
            .id()
            .generation(),
        1
    );
    assert_eq!(
        snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(199))
            .expect("last listed instant")
            .definition()
            .id()
            .generation(),
        1
    );
    for event_time in [200, 300, 399] {
        assert_eq!(
            snapshot.resolve(&venue, "ABCUSD", UnixNanos::new(event_time)),
            Err(ResolveError::NotListedAtTime)
        );
    }
    assert_eq!(
        snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(400))
            .expect("second listing boundary")
            .definition()
            .id()
            .generation(),
        2
    );
}

#[test]
fn overlapping_spot_and_perpetual_symbols_resolve_only_with_product_type() {
    let venue = VenueId::new("binance").expect("venue");
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            spot_definition("binance", "BTCUSDT", 1, 100, None),
            metadata(10, "fixture:spot"),
        )
        .expect("spot definition");
    registry
        .append_definition(
            perpetual_definition("binance", "BTCUSDT", 1, 100),
            metadata(10, "fixture:perpetual"),
        )
        .expect("perpetual definition");
    let snapshot = registry.snapshot().expect("snapshot");

    assert_eq!(
        snapshot.resolve(&venue, "BTCUSDT", UnixNanos::new(200)),
        Err(ResolveError::AmbiguousProductType)
    );
    assert_eq!(
        snapshot
            .resolve_for_product(&venue, "BTCUSDT", ProductType::Spot, UnixNanos::new(200))
            .expect("spot")
            .definition()
            .product_type(),
        ProductType::Spot
    );
    assert_eq!(
        snapshot
            .resolve_for_product(
                &venue,
                "BTCUSDT",
                ProductType::Perpetual,
                UnixNanos::new(200)
            )
            .expect("perpetual")
            .definition()
            .product_type(),
        ProductType::Perpetual
    );
}

#[test]
fn historical_generations_may_arrive_late_but_must_follow_listing_order() {
    let venue = VenueId::new("binance").expect("fixture venue");
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 2, 400, None),
            metadata(10, "fixture:known-current"),
        )
        .expect("current generation may be learned first");
    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 1, 100, Some(400)),
            metadata(20, "fixture:historical-backfill"),
        )
        .expect("older generation may be backfilled later");
    let snapshot = registry.snapshot().expect("backfilled snapshot");
    assert_eq!(
        snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(399))
            .expect("historical generation")
            .definition()
            .id()
            .generation(),
        1
    );
    assert_eq!(
        snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(400))
            .expect("current generation")
            .definition()
            .id()
            .generation(),
        2
    );

    let mut invalid = InstrumentRegistry::new();
    invalid
        .append_definition(
            spot_definition("binance", "ABCUSD", 1, 400, None),
            metadata(10, "fixture:late-generation-one"),
        )
        .expect("first accepted definition");
    let baseline = invalid.snapshot().expect("baseline");
    assert_eq!(
        invalid.append_definition(
            spot_definition("binance", "ABCUSD", 2, 100, Some(400)),
            metadata(20, "fixture:early-generation-two"),
        ),
        Err(RegistryError::GenerationNotIncreasing)
    );
    assert_eq!(invalid.snapshot().expect("atomic rejection"), baseline);
}

#[test]
fn exact_generation_resolution_separates_invalid_unknown_venue_symbol_and_time() {
    let definition = spot_definition("binance", "BTCUSD", 3, 100, Some(200));
    let id = definition.id().clone();
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(definition, metadata(10, "fixture:definition"))
        .expect("definition");
    let snapshot = registry.snapshot().expect("snapshot");

    assert_eq!(
        snapshot
            .resolve_id(&id, UnixNanos::new(150))
            .expect("exact generation")
            .definition()
            .id(),
        &id
    );
    assert_eq!(
        snapshot.resolve_id(&id, UnixNanos::new(200)),
        Err(ResolveError::NotListedAtTime)
    );
    let unknown_id = InstrumentId::new(VenueId::new("binance").expect("venue"), "BTCUSD", 4)
        .expect("unknown generation");
    assert_eq!(
        snapshot.resolve_id(&unknown_id, UnixNanos::new(150)),
        Err(ResolveError::UnknownInstrument)
    );
    assert_eq!(
        snapshot.resolve(
            &VenueId::new("kraken").expect("venue"),
            "BTCUSD",
            UnixNanos::new(150),
        ),
        Err(ResolveError::UnknownVenue)
    );
    assert_eq!(
        snapshot.resolve(
            &VenueId::new("binance").expect("venue"),
            "UNKNOWN",
            UnixNanos::new(150),
        ),
        Err(ResolveError::UnknownSymbol)
    );
    assert!(matches!(
        snapshot.resolve(
            &VenueId::new("binance").expect("venue"),
            "bad symbol",
            UnixNanos::new(150),
        ),
        Err(ResolveError::InvalidIdentity(_))
    ));
}

#[test]
fn duplicate_conflict_overlap_generation_and_capacity_fail_atomically() {
    let limits = RegistryLimits::try_new(RegistryLimitsInput {
        maximum_records: NonZeroUsize::new(3).expect("records"),
        maximum_definitions: NonZeroUsize::new(2).expect("definitions"),
        maximum_symbol_keys: NonZeroUsize::new(1).expect("symbols"),
        maximum_history_per_symbol: NonZeroUsize::new(2).expect("history"),
        maximum_corrections_per_instrument: NonZeroUsize::new(1).expect("corrections"),
        maximum_batch_records: NonZeroUsize::new(2).expect("batch"),
        maximum_snapshot_bytes: NonZeroUsize::new(64 * 1024).expect("snapshot bytes"),
    })
    .expect("small valid limits");
    let mut registry = InstrumentRegistry::with_limits(limits);
    let first = spot_definition("binance", "ABCUSD", 1, 100, Some(300));
    let first_revision = appended_revision(
        registry
            .append_definition(first.clone(), metadata(10, "fixture:first"))
            .expect("first definition"),
    );
    let baseline = registry.snapshot().expect("baseline snapshot");

    assert_eq!(
        registry
            .append_definition(first.clone(), metadata(11, "fixture:duplicate"))
            .expect("identical append is idempotent"),
        AppendOutcome::AlreadyPresent {
            revision: first_revision
        }
    );
    let conflicting = InstrumentDefinition::new(InstrumentDefinitionInput {
        price_tick: price("0.1"),
        ..serde_json::from_value(serde_json::to_value(&first).expect("definition encode"))
            .expect("definition input decode")
    })
    .expect("conflicting definition remains individually valid");
    assert_eq!(
        registry.append_definition(conflicting, metadata(11, "fixture:conflict")),
        Err(RegistryError::GenerationConflict)
    );
    assert_eq!(
        registry.append_definition(
            spot_definition("binance", "ABCUSD", 2, 200, Some(400)),
            metadata(11, "fixture:overlap"),
        ),
        Err(RegistryError::OverlappingListing)
    );
    assert_eq!(
        registry.append_definition(
            spot_definition("binance", "ABCUSD", 1, 400, Some(500)),
            metadata(11, "fixture:reused-generation"),
        ),
        Err(RegistryError::GenerationNotIncreasing)
    );
    assert_eq!(registry.snapshot().expect("unchanged snapshot"), baseline);

    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 2, 400, None),
            metadata(12, "fixture:second"),
        )
        .expect("second generation");
    assert_eq!(
        registry.append_definition(
            spot_definition("binance", "XYZUSD", 1, 100, None),
            metadata(13, "fixture:capacity"),
        ),
        Err(RegistryError::DefinitionCapacityExceeded)
    );
}

#[test]
fn corrections_are_bitemporal_versioned_and_never_rewrite_old_snapshots() {
    let venue = VenueId::new("binance").expect("fixture venue");
    let mut registry = InstrumentRegistry::new();
    let original = spot_definition("binance", "ABCUSD", 1, 100, Some(200));
    registry
        .append_definition(original.clone(), metadata(10, "fixture:original"))
        .expect("original");
    let before = registry
        .snapshot_as_known_at(UnixNanos::new(20))
        .expect("old as-known snapshot");
    let before_catalog_digest = *before.catalog_digest();
    let before_history_digest = *before.history_digest();

    let correction = correction(
        &registry,
        &original,
        spot_definition("binance", "ABCUSD", 1, 100, Some(250)),
        30,
    );
    let correction_revision =
        appended_revision(registry.append_correction(correction).expect("correction"));
    let after = registry
        .snapshot_as_known_at(UnixNanos::new(30))
        .expect("corrected snapshot");

    assert_eq!(
        before.resolve(&venue, "ABCUSD", UnixNanos::new(225)),
        Err(ResolveError::NotListedAtTime)
    );
    assert_eq!(*before.catalog_digest(), before_catalog_digest);
    assert_eq!(*before.history_digest(), before_history_digest);
    assert_eq!(
        after
            .resolve(&venue, "ABCUSD", UnixNanos::new(225))
            .expect("corrected interval")
            .definition()
            .id()
            .generation(),
        1
    );
    let correction = registry.records().last().expect("correction record");
    assert!(matches!(
        correction,
        CatalogRecord::Correction(record)
            if record.correction_version().get() == 1
                && record.catalog_revision() == correction_revision
                && record.reason() == "venue metadata correction"
    ));
}

#[test]
fn stale_noop_identity_changing_and_time_regressing_corrections_fail_closed() {
    let mut registry = InstrumentRegistry::new();
    let original = spot_definition("binance", "ABCUSD", 1, 100, Some(200));
    registry
        .append_definition(original.clone(), metadata(10, "fixture:original"))
        .expect("original");
    let initial = registry
        .definition_revision(original.id())
        .expect("initial version");

    let no_op = CorrectionInput::try_new(
        initial.revision(),
        *initial.definition_hash(),
        original.clone(),
        metadata(11, "fixture:no-op"),
        "no change",
    )
    .expect("structurally valid correction input");
    assert_eq!(
        registry.append_correction(no_op),
        Err(RegistryError::NoOpCorrection)
    );
    let identity_change = CorrectionInput::try_new(
        initial.revision(),
        *initial.definition_hash(),
        spot_definition("binance", "ABCUSD", 2, 100, Some(200)),
        metadata(11, "fixture:identity-change"),
        "incorrect identity",
    )
    .expect("structurally valid correction input");
    assert_eq!(
        registry.append_correction(identity_change),
        Err(RegistryError::CorrectionIdentityMismatch)
    );
    let economic_change = InstrumentDefinition::new(InstrumentDefinitionInput {
        price_tick: price("0.1"),
        ..serde_json::from_value(serde_json::to_value(&original).expect("definition encode"))
            .expect("definition input decode")
    })
    .expect("economic replacement");
    let economic = CorrectionInput::try_new(
        initial.revision(),
        *initial.definition_hash(),
        economic_change,
        metadata(11, "fixture:economic-change"),
        "tick changed",
    )
    .expect("structurally valid correction input");
    assert_eq!(
        registry.append_correction(economic),
        Err(RegistryError::CorrectionRequiresNewGeneration)
    );

    let accepted = correction(
        &registry,
        &original,
        spot_definition("binance", "ABCUSD", 1, 100, Some(210)),
        20,
    );
    registry
        .append_correction(accepted)
        .expect("first correction");
    let stale = CorrectionInput::try_new(
        initial.revision(),
        *initial.definition_hash(),
        spot_definition("binance", "ABCUSD", 1, 100, Some(220)),
        metadata(21, "fixture:stale"),
        "stale target",
    )
    .expect("stale correction input");
    assert_eq!(
        registry.append_correction(stale),
        Err(RegistryError::StaleCorrection)
    );
    let current = registry
        .definition_revision(original.id())
        .expect("corrected version");
    let time_regression = CorrectionInput::try_new(
        current.revision(),
        *current.definition_hash(),
        spot_definition("binance", "ABCUSD", 1, 100, Some(230)),
        metadata(19, "fixture:time-regression"),
        "recorded too early",
    )
    .expect("time-regressing input");
    assert_eq!(
        registry.append_correction(time_regression),
        Err(RegistryError::RecordTimeRegression)
    );
}

#[test]
fn one_atomic_batch_can_close_an_open_generation_and_open_its_successor() {
    let venue = VenueId::new("binance").expect("fixture venue");
    let mut registry = InstrumentRegistry::new();
    let original = spot_definition("binance", "ABCUSD", 1, 100, None);
    registry
        .append_definition(original.clone(), metadata(10, "fixture:open"))
        .expect("open generation");
    let old_snapshot = registry.snapshot().expect("old snapshot");
    let current = registry.current_revision();

    assert_eq!(
        registry.append_definition(
            spot_definition("binance", "ABCUSD", 2, 400, None),
            metadata(20, "fixture:premature-successor"),
        ),
        Err(RegistryError::OverlappingListing)
    );
    let close = correction(
        &registry,
        &original,
        spot_definition("binance", "ABCUSD", 1, 100, Some(400)),
        20,
    );
    let outcome = registry
        .append_batch(
            current,
            vec![
                CatalogMutation::Correction(close),
                CatalogMutation::Definition {
                    definition: spot_definition("binance", "ABCUSD", 2, 400, None),
                    metadata: metadata(20, "fixture:successor"),
                },
            ],
        )
        .expect("atomic handoff");
    assert_eq!(outcome.appended_records(), 2);
    let committed_revision = outcome.committed_revision().expect("batch commit revision");
    assert_ne!(Some(committed_revision), current);
    assert!(
        registry.records()[1..]
            .iter()
            .all(|record| record.catalog_revision() == committed_revision)
    );
    assert_eq!(
        registry
            .snapshot_at_revision(current.expect("prior revision"))
            .expect("pre-batch snapshot"),
        old_snapshot
    );
    assert_eq!(
        registry
            .snapshot_at_revision(committed_revision)
            .expect("complete batch snapshot")
            .entries()
            .len(),
        2
    );

    assert_eq!(
        old_snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(500))
            .expect("old snapshot remains immutable")
            .definition()
            .id()
            .generation(),
        1
    );
    let new_snapshot = registry.snapshot().expect("new snapshot");
    assert_eq!(
        new_snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(400))
            .expect("successor boundary")
            .definition()
            .id()
            .generation(),
        2
    );
    assert_eq!(
        new_snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(399))
            .expect("pre-boundary generation")
            .definition()
            .id()
            .generation(),
        1
    );
}

#[test]
fn atomic_handoff_validation_is_independent_of_mutation_order() {
    let venue = VenueId::new("binance").expect("fixture venue");
    let mut registry = InstrumentRegistry::new();
    let original = spot_definition("binance", "ABCUSD", 1, 100, None);
    registry
        .append_definition(original.clone(), metadata(10, "fixture:open"))
        .expect("open generation");
    let expected = registry.current_revision();
    let close = correction(
        &registry,
        &original,
        spot_definition("binance", "ABCUSD", 1, 100, Some(400)),
        20,
    );

    registry
        .append_batch(
            expected,
            vec![
                CatalogMutation::Definition {
                    definition: spot_definition("binance", "ABCUSD", 2, 400, None),
                    metadata: metadata(20, "fixture:successor"),
                },
                CatalogMutation::Correction(close),
            ],
        )
        .expect("final batch state is unambiguous");

    let snapshot = registry.snapshot().expect("handoff snapshot");
    assert_eq!(
        snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(399))
            .expect("pre-handoff")
            .definition()
            .id()
            .generation(),
        1
    );
    assert_eq!(
        snapshot
            .resolve(&venue, "ABCUSD", UnixNanos::new(400))
            .expect("post-handoff")
            .definition()
            .id()
            .generation(),
        2
    );
}

#[test]
fn atomic_batches_require_one_knowledge_time_and_never_publish_partial_cutoffs() {
    let venue = VenueId::new("binance").expect("fixture venue");
    let mut registry = InstrumentRegistry::new();
    let original = spot_definition("binance", "ABCUSD", 1, 100, None);
    registry
        .append_definition(original.clone(), metadata(10, "fixture:open"))
        .expect("open generation");
    let baseline = registry.snapshot().expect("baseline");
    let expected = registry.current_revision();
    let mismatched_close = correction(
        &registry,
        &original,
        spot_definition("binance", "ABCUSD", 1, 100, Some(400)),
        20,
    );
    assert_eq!(
        registry.append_batch(
            expected,
            vec![
                CatalogMutation::Correction(mismatched_close),
                CatalogMutation::Definition {
                    definition: spot_definition("binance", "ABCUSD", 2, 400, None),
                    metadata: metadata(30, "fixture:late-successor"),
                },
            ],
        ),
        Err(RegistryError::BatchKnownAtMismatch)
    );
    assert_eq!(registry.snapshot().expect("atomic failure"), baseline);

    let close = correction(
        &registry,
        &original,
        spot_definition("binance", "ABCUSD", 1, 100, Some(400)),
        20,
    );
    registry
        .append_batch(
            expected,
            vec![
                CatalogMutation::Correction(close),
                CatalogMutation::Definition {
                    definition: spot_definition("binance", "ABCUSD", 2, 400, None),
                    metadata: metadata(20, "fixture:successor"),
                },
            ],
        )
        .expect("one-time batch");

    let before_commit = registry
        .snapshot_as_known_at(UnixNanos::new(19))
        .expect("before commit cutoff");
    assert_eq!(before_commit.records(), baseline.records());
    assert_eq!(before_commit.entries(), baseline.entries());
    assert_eq!(
        before_commit.catalog_revision(),
        baseline.catalog_revision()
    );
    let committed = registry
        .snapshot_as_known_at(UnixNanos::new(20))
        .expect("at commit cutoff");
    assert_eq!(committed.records().len(), 3);
    assert_eq!(
        committed
            .resolve(&venue, "ABCUSD", UnixNanos::new(400))
            .expect("complete successor")
            .definition()
            .id()
            .generation(),
        2
    );
}

#[test]
fn stale_batch_revision_and_late_overlap_leave_the_published_snapshot_unchanged() {
    let mut registry = InstrumentRegistry::new();
    let original = spot_definition("binance", "ABCUSD", 1, 100, Some(300));
    let revision = appended_revision(
        registry
            .append_definition(original.clone(), metadata(10, "fixture:original"))
            .expect("original"),
    );
    let baseline = registry.snapshot().expect("baseline");

    assert_eq!(
        registry.append_batch(
            None,
            vec![CatalogMutation::Definition {
                definition: spot_definition("binance", "XYZUSD", 1, 100, None),
                metadata: metadata(20, "fixture:stale-batch"),
            }],
        ),
        Err(RegistryError::RevisionConflict {
            expected: None,
            actual: Some(revision),
        })
    );
    let overlapping = correction(
        &registry,
        &original,
        spot_definition("binance", "ABCUSD", 1, 100, Some(500)),
        20,
    );
    assert_eq!(
        registry.append_batch(
            Some(revision),
            vec![
                CatalogMutation::Definition {
                    definition: spot_definition("binance", "ABCUSD", 2, 400, None),
                    metadata: metadata(20, "fixture:new"),
                },
                CatalogMutation::Correction(overlapping),
            ],
        ),
        Err(RegistryError::OverlappingListing)
    );
    assert_eq!(registry.snapshot().expect("atomic failure"), baseline);
}

#[test]
fn exact_duplicates_remain_idempotent_at_capacity_and_snapshot_overflow_is_atomic() {
    let full_limits = RegistryLimits::try_new(RegistryLimitsInput {
        maximum_records: NonZeroUsize::new(1).expect("records"),
        maximum_definitions: NonZeroUsize::new(1).expect("definitions"),
        maximum_symbol_keys: NonZeroUsize::new(1).expect("symbols"),
        maximum_history_per_symbol: NonZeroUsize::new(1).expect("history"),
        maximum_corrections_per_instrument: NonZeroUsize::new(1).expect("corrections"),
        maximum_batch_records: NonZeroUsize::new(1).expect("batch"),
        maximum_snapshot_bytes: NonZeroUsize::new(64 * 1024).expect("snapshot"),
    })
    .expect("full-capacity limits");
    let definition = spot_definition("binance", "ABCUSD", 1, 100, None);
    let mut full = InstrumentRegistry::with_limits(full_limits);
    let revision = appended_revision(
        full.append_definition(definition.clone(), metadata(10, "fixture:first"))
            .expect("first definition"),
    );
    let outcome = full
        .append_batch(
            Some(revision),
            vec![CatalogMutation::Definition {
                definition,
                metadata: metadata(5, "fixture:duplicate"),
            }],
        )
        .expect("duplicate does not consume capacity or regress record time");
    assert_eq!(outcome.appended_records(), 0);
    assert_eq!(outcome.idempotent_records(), 1);
    assert_eq!(outcome.committed_revision(), Some(revision));
    assert_eq!(full.records().len(), 1);

    let bounded_limits = RegistryLimits::try_new(RegistryLimitsInput {
        maximum_records: NonZeroUsize::new(4).expect("records"),
        maximum_definitions: NonZeroUsize::new(4).expect("definitions"),
        maximum_symbol_keys: NonZeroUsize::new(4).expect("symbols"),
        maximum_history_per_symbol: NonZeroUsize::new(4).expect("history"),
        maximum_corrections_per_instrument: NonZeroUsize::new(4).expect("corrections"),
        maximum_batch_records: NonZeroUsize::new(4).expect("batch"),
        maximum_snapshot_bytes: NonZeroUsize::new(1_024).expect("snapshot"),
    })
    .expect("small snapshot limit");
    let long_source = "s".repeat(256);
    let mutations = ["AAAUSD", "BBBUSD", "CCCUSD", "DDDUSD"]
        .into_iter()
        .map(|symbol| CatalogMutation::Definition {
            definition: spot_definition("binance", symbol, 1, 100, None),
            metadata: metadata(10, &long_source),
        })
        .collect();
    let mut bounded = InstrumentRegistry::with_limits(bounded_limits);
    assert_eq!(
        bounded.append_batch(None, mutations),
        Err(RegistryError::CanonicalEncodingCapacityExceeded)
    );
    assert!(bounded.records().is_empty());
    assert_eq!(bounded.current_revision(), None);
}

#[test]
fn correction_hash_and_unknown_targets_fail_without_mutation() {
    let mut registry = InstrumentRegistry::new();
    let original = spot_definition("binance", "ABCUSD", 1, 100, Some(200));
    registry
        .append_definition(original.clone(), metadata(10, "fixture:original"))
        .expect("original");
    let baseline = registry.snapshot().expect("baseline");
    let current = registry
        .definition_revision(original.id())
        .expect("current definition");
    let wrong_hash = CorrectionInput::try_new(
        current.revision(),
        [0xA5; 32],
        spot_definition("binance", "ABCUSD", 1, 100, Some(250)),
        metadata(20, "fixture:wrong-hash"),
        "wrong prior hash",
    )
    .expect("bounded correction input");
    assert_eq!(
        registry.append_correction(wrong_hash),
        Err(RegistryError::CorrectionHashMismatch)
    );

    let unknown = CorrectionInput::try_new(
        instrument_registry::CatalogRevision::new(99).expect("revision"),
        [0; 32],
        spot_definition("kraken", "UNKNOWN", 1, 100, Some(250)),
        metadata(20, "fixture:unknown"),
        "unknown target",
    )
    .expect("bounded correction input");
    assert_eq!(
        registry.append_correction(unknown),
        Err(RegistryError::UnknownCorrectionTarget)
    );
    assert_eq!(registry.snapshot().expect("unchanged snapshot"), baseline);
}

#[test]
fn correction_targets_are_disambiguated_by_shared_revision_and_definition_hash() {
    let first = spot_definition("binance", "AAAUSD", 1, 100, None);
    let second = spot_definition("binance", "BBBUSD", 1, 100, None);
    let mut registry = InstrumentRegistry::new();
    let outcome = registry
        .append_batch(
            None,
            vec![
                CatalogMutation::Definition {
                    definition: first.clone(),
                    metadata: metadata(10, "fixture:first"),
                },
                CatalogMutation::Definition {
                    definition: second,
                    metadata: metadata(10, "fixture:second"),
                },
            ],
        )
        .expect("shared-revision definitions");
    let shared_revision = outcome
        .committed_revision()
        .expect("shared catalog revision");
    let first_hash = *registry
        .definition_revision(first.id())
        .expect("first definition")
        .definition_hash();
    let unknown_replacement = spot_definition("binance", "CCCUSD", 1, 100, Some(200));
    let baseline = registry.snapshot().expect("baseline");

    let unknown = CorrectionInput::try_new(
        shared_revision,
        [0; 32],
        unknown_replacement.clone(),
        metadata(20, "fixture:unknown-hash"),
        "unknown target",
    )
    .expect("bounded correction");
    assert_eq!(
        registry.append_correction(unknown),
        Err(RegistryError::UnknownCorrectionTarget)
    );

    let wrong_identity = CorrectionInput::try_new(
        shared_revision,
        first_hash,
        unknown_replacement,
        metadata(20, "fixture:known-hash"),
        "wrong identity",
    )
    .expect("bounded correction");
    assert_eq!(
        registry.append_correction(wrong_identity),
        Err(RegistryError::CorrectionIdentityMismatch)
    );
    assert_eq!(registry.snapshot().expect("unchanged snapshot"), baseline);
}

#[test]
fn exact_correction_retries_are_idempotent_after_unrelated_catalog_progress() {
    let mut registry = InstrumentRegistry::new();
    let original = spot_definition("binance", "ABCUSD", 1, 100, Some(200));
    registry
        .append_definition(original.clone(), metadata(10, "fixture:original"))
        .expect("original");
    let retryable = correction(
        &registry,
        &original,
        spot_definition("binance", "ABCUSD", 1, 100, Some(250)),
        20,
    );
    let correction_revision = appended_revision(
        registry
            .append_correction(retryable.clone())
            .expect("first correction"),
    );
    registry
        .append_definition(
            spot_definition("kraken", "XYZUSD", 1, 100, None),
            metadata(30, "fixture:unrelated"),
        )
        .expect("unrelated catalog progress");
    let record_count = registry.records().len();

    assert_eq!(
        registry
            .append_correction(retryable)
            .expect("exact retry is idempotent"),
        AppendOutcome::AlreadyPresent {
            revision: correction_revision
        }
    );
    assert_eq!(registry.records().len(), record_count);
}

#[test]
fn canonical_snapshots_preserve_options_and_separate_catalog_from_history_identity() {
    let option = option_definition();
    let spot = spot_definition("binance", "BTCUSD", 1, 100, None);
    let mut first = InstrumentRegistry::new();
    first
        .append_definition(option.clone(), metadata(10, "fixture:option"))
        .expect("option");
    first
        .append_definition(spot.clone(), metadata(10, "fixture:spot"))
        .expect("spot");
    let first_snapshot = first.snapshot().expect("first snapshot");

    let mut second = InstrumentRegistry::new();
    second
        .append_definition(spot, metadata(10, "fixture:spot"))
        .expect("spot");
    second
        .append_definition(option, metadata(10, "fixture:option"))
        .expect("option");
    let second_snapshot = second.snapshot().expect("second snapshot");

    assert_eq!(
        first_snapshot.catalog_digest(),
        second_snapshot.catalog_digest(),
        "effective catalog identity is insertion-order independent"
    );
    assert_ne!(
        first_snapshot.history_digest(),
        second_snapshot.history_digest(),
        "append history identity retains record order"
    );
    assert!(first_snapshot.verify_integrity());
    let resolved = first_snapshot
        .resolve(
            &VenueId::new("deribit").expect("venue"),
            "btc-30jun30-50000-c",
            UnixNanos::new(500),
        )
        .expect("option resolution");
    assert_eq!(resolved.definition().product_type(), ProductType::Option);
    assert_eq!(resolved.definition().option_side(), Some(OptionSide::Call));
    assert_eq!(resolved.definition().strike(), Some(price("50000")));
    assert_eq!(
        resolved.catalog_revision(),
        first_snapshot.catalog_revision()
    );
    assert_eq!(resolved.catalog_digest(), first_snapshot.catalog_digest());

    let encoded = serde_json::to_value(&*first_snapshot).expect("snapshot serialization");
    let object = encoded.as_object().expect("snapshot object");
    let keys = object
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from([
            "schema_version",
            "catalog_revision",
            "as_known_at",
            "records",
            "entries",
            "catalog_digest",
            "history_digest",
        ])
    );
}

#[test]
fn canonical_snapshot_identity_matches_the_frozen_known_vector() {
    let mut registry = InstrumentRegistry::new();
    registry
        .append_batch(
            None,
            vec![
                CatalogMutation::Definition {
                    definition: spot_definition("binance", "BTCUSD", 1, 100, Some(400)),
                    metadata: metadata(10, "vector:spot"),
                },
                CatalogMutation::Definition {
                    definition: option_definition(),
                    metadata: metadata(10, "vector:option"),
                },
            ],
        )
        .expect("known-vector catalog");
    let snapshot = registry
        .snapshot_as_known_at(UnixNanos::new(10))
        .expect("known-vector snapshot");

    assert_eq!(
        digest_hex(snapshot.catalog_digest()),
        "a009eb5fdf4fb35ceb4fa4a23a8a864473665bc4123e3c679669eb3492fa67b2"
    );
    assert_eq!(
        digest_hex(snapshot.history_digest()),
        "926b81f231a03775e14eb4a18dfe12ca82bce6468f1b5384806acb623d0ccf59"
    );
}

#[test]
fn i64_boundaries_venue_isolation_limits_and_snapshot_cutoffs_fail_closed() {
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 1, i64::MIN, Some(0)),
            metadata(i64::MIN, "fixture:min"),
        )
        .expect("minimum-time definition");
    registry
        .append_definition(
            spot_definition("kraken", "ABCUSD", 1, 0, None),
            metadata(0, "fixture:venue-isolation"),
        )
        .expect("same symbol on another venue");
    let snapshot = registry.snapshot().expect("snapshot");
    assert_eq!(
        snapshot
            .resolve(
                &VenueId::new("binance").expect("venue"),
                "ABCUSD",
                UnixNanos::new(i64::MIN),
            )
            .expect("minimum boundary")
            .definition()
            .id()
            .venue()
            .as_str(),
        "binance"
    );
    assert_eq!(
        snapshot.resolve(
            &VenueId::new("binance").expect("venue"),
            "ABCUSD",
            UnixNanos::new(0),
        ),
        Err(ResolveError::NotListedAtTime)
    );
    assert_eq!(
        snapshot
            .resolve(
                &VenueId::new("kraken").expect("venue"),
                "ABCUSD",
                UnixNanos::new(i64::MAX),
            )
            .expect("open-ended maximum")
            .definition()
            .id()
            .venue()
            .as_str(),
        "kraken"
    );
    assert_eq!(
        registry.snapshot_as_known_at(UnixNanos::new(i64::MIN)),
        registry.snapshot_at_revision(
            registry
                .records()
                .first()
                .expect("first record")
                .catalog_revision(),
        )
    );
    assert_eq!(
        registry
            .snapshot_at_revision(instrument_registry::CatalogRevision::new(3).expect("revision")),
        Err(ResolveError::CatalogRevisionUnavailable)
    );
    assert_eq!(
        RegistryLimits::try_new(RegistryLimitsInput {
            maximum_records: NonZeroUsize::new(1).expect("records"),
            maximum_definitions: NonZeroUsize::new(2).expect("definitions"),
            maximum_symbol_keys: NonZeroUsize::new(1).expect("symbols"),
            maximum_history_per_symbol: NonZeroUsize::new(1).expect("history"),
            maximum_corrections_per_instrument: NonZeroUsize::new(1).expect("corrections"),
            maximum_batch_records: NonZeroUsize::new(1).expect("batch"),
            maximum_snapshot_bytes: NonZeroUsize::new(1).expect("snapshot"),
        }),
        Err(RegistryError::InvalidLimits)
    );
}

#[test]
fn snapshots_are_arc_owned_and_remain_stable_after_later_registry_mutation() {
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 1, 100, Some(200)),
            metadata(10, "fixture:first"),
        )
        .expect("first");
    let snapshot: Arc<_> = registry.snapshot().expect("snapshot");
    let digest = *snapshot.catalog_digest();
    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 2, 400, None),
            metadata(20, "fixture:second"),
        )
        .expect("second");
    assert_eq!(*snapshot.catalog_digest(), digest);
    assert_eq!(snapshot.entries().len(), 1);
    assert_eq!(registry.snapshot().expect("latest").entries().len(), 2);
}

#[test]
fn staged_batch_isolatedly_advances_the_current_registry_without_replay() {
    let mut registry = InstrumentRegistry::new();
    registry
        .append_definition(
            spot_definition("binance", "ABCUSD", 1, 100, Some(200)),
            metadata(10, "fixture:first"),
        )
        .expect("first");
    let original_revision = registry.current_revision();
    let original_digest = *registry
        .snapshot()
        .expect("original snapshot")
        .history_digest();

    let (staged, outcome) = registry
        .stage_batch(
            original_revision,
            vec![CatalogMutation::Definition {
                definition: spot_definition("binance", "ABCUSD", 2, 200, None),
                metadata: metadata(20, "fixture:staged"),
            }],
        )
        .expect("stage next batch");

    assert_eq!(registry.current_revision(), original_revision);
    assert_eq!(
        *registry
            .snapshot()
            .expect("unchanged original snapshot")
            .history_digest(),
        original_digest
    );
    assert_eq!(staged.records().len(), registry.records().len() + 1);
    assert_eq!(outcome.previous_revision(), original_revision);
    assert_eq!(outcome.committed_revision(), staged.current_revision());
}
