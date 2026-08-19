use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
use feature_registry::{
    CodeRevision, DurationNanos, EntityScope, EventTimePolicy, FeatureDatum, FeatureDefinition,
    FeatureDefinitionInput, FeatureDocumentation, FeatureEntity, FeatureId, FeatureObservation,
    FeatureObservationInput, FeatureRegistry, FeatureStatus, FeatureValue, FeatureValueType,
    FinalityState, FormulaHash, InputRequirement, LineageHash, MissingnessPolicy,
    MissingnessReason, NormalizationKind, NormalizationPolicy, ObservationRevision,
    QualityRequirement, QualityScore, RegistryError, SourceCoverage, SourceCoverageEntry,
    WindowDefinition, WindowId, WindowKind,
};
use fixed_decimal::FixedDecimal;
use quality::SourceHealthState;
use semver::Version;
use std::num::NonZeroU64;

fn version(value: &str) -> Version {
    Version::parse(value).expect("test version should be valid")
}

fn hash(byte: u8) -> [u8; 32] {
    [byte; 32]
}

fn source(name: &str) -> SourceId {
    SourceId::new(SourceKind::Exchange, name, 1).expect("test source should be valid")
}

fn instrument() -> InstrumentId {
    InstrumentId::new(
        VenueId::new("binance").expect("test venue should be valid"),
        "BTCUSDT",
        1,
    )
    .expect("test instrument should be valid")
}

fn definition_input(id: &str, feature_version: &str) -> FeatureDefinitionInput {
    FeatureDefinitionInput {
        id: FeatureId::new(id).expect("test feature ID should be valid"),
        version: version(feature_version),
        status: FeatureStatus::Required,
        value_type: FeatureValueType::FixedDecimal,
        entities: EntityScope::Instrument,
        required_inputs: vec![
            InputRequirement::new("normalized.trades").expect("test input should be valid"),
        ],
        event_time_policy: EventTimePolicy::SourceEventTime,
        windows: vec![
            WindowDefinition::try_new_time(
                WindowId::new("rolling_5m").expect("test window should be valid"),
                WindowKind::Sliding,
                DurationNanos::new(300_000_000_000),
                Some(DurationNanos::new(60_000_000_000)),
            )
            .expect("test window definition should be valid"),
        ],
        output_resolution: DurationNanos::new(60_000_000_000),
        allowed_lateness: DurationNanos::new(5_000_000_000),
        time_to_live: DurationNanos::new(86_400_000_000_000),
        normalization: NormalizationPolicy::try_new(
            NormalizationKind::RobustRolling,
            version("1.0.0"),
        )
        .expect("test normalization should be valid"),
        missingness: MissingnessPolicy::Explicit,
        quality_gate: QualityRequirement::try_new(
            QualityScore::from_millionths(900_000).expect("test score should be valid"),
            QualityScore::from_millionths(1_000_000).expect("test coverage should be valid"),
            vec![SourceHealthState::Healthy, SourceHealthState::Recovering],
        )
        .expect("test quality gate should be valid"),
        formula_hash: FormulaHash::new(hash(7)).expect("test formula hash should be valid"),
        documentation: FeatureDocumentation::new(format!(
            "docs/data-dictionary/features.md#{}",
            id.replace(['_', '.'], "-")
        ))
        .expect("test documentation should be valid"),
    }
}

fn definition(id: &str, feature_version: &str) -> FeatureDefinition {
    FeatureDefinition::try_new(definition_input(id, feature_version))
        .expect("test feature definition should be valid")
}

fn observation() -> FeatureObservation {
    FeatureObservation::try_new(FeatureObservationInput {
        feature_id: FeatureId::new("realized_volatility").expect("test ID should be valid"),
        feature_version: version("1.0.0"),
        entity: FeatureEntity::Instrument(instrument()),
        window_id: WindowId::new("rolling_5m").expect("test window should be valid"),
        resolution: DurationNanos::new(60_000_000_000),
        datum: FeatureDatum::Present(FeatureValue::FixedDecimal(
            FixedDecimal::parse_canonical("0.125").expect("test decimal should be valid"),
        )),
        value_type: FeatureValueType::FixedDecimal,
        event_time_start: UnixNanos::new(1_000_000_000),
        event_time_end: UnixNanos::new(2_000_000_000),
        as_known_at: UnixNanos::new(2_100_000_000),
        computed_at: UnixNanos::new(2_200_000_000),
        watermark: UnixNanos::new(2_100_000_000),
        finality_state: FinalityState::Provisional,
        revision: ObservationRevision::new(1).expect("test revision should be valid"),
        source_coverage: SourceCoverage::try_new(vec![SourceCoverageEntry::new(
            source("binance"),
            SourceHealthState::Healthy,
        )])
        .expect("test coverage should be valid"),
        quality_score: QualityScore::from_millionths(950_000).expect("test score should be valid"),
        normalization_version: version("1.0.0"),
        formula_hash: FormulaHash::new(hash(7)).expect("test formula hash should be valid"),
        code_commit: CodeRevision::new("0123456789abcdef0123456789abcdef01234567")
            .expect("test revision should be valid"),
        lineage_hash: LineageHash::new(hash(9)).expect("test lineage should be valid"),
    })
    .expect("test observation should be valid")
}

#[test]
fn duplicate_id_and_version_is_rejected_without_mutation() {
    let mut registry = FeatureRegistry::new();
    let feature = definition("realized_volatility", "1.0.0");
    registry
        .register(feature.clone())
        .expect("first definition should register");
    let before = registry.snapshot_digest().expect("snapshot should hash");

    assert_eq!(
        registry.register(feature),
        Err(RegistryError::DuplicateDefinition)
    );
    assert_eq!(registry.len(), 1);
    assert_eq!(
        registry.snapshot_digest().expect("snapshot should hash"),
        before
    );
}

#[test]
fn a_new_semantic_version_is_an_explicit_distinct_definition() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("first definition should register");
    registry
        .register(definition("realized_volatility", "1.1.0"))
        .expect("new version should register");

    assert_eq!(registry.len(), 2);
    assert!(registry.contains(
        &FeatureId::new("realized_volatility").expect("test ID should be valid"),
        &version("1.1.0")
    ));
}

#[test]
fn registry_snapshot_and_digest_are_insertion_order_independent() {
    let first = definition("realized_volatility", "1.0.0");
    let second = definition("trade_imbalance", "1.0.0");
    let mut left = FeatureRegistry::new();
    let mut right = FeatureRegistry::new();
    left.register(first.clone())
        .expect("definition should register");
    left.register(second.clone())
        .expect("definition should register");
    right.register(second).expect("definition should register");
    right.register(first).expect("definition should register");

    assert_eq!(
        left.canonical_snapshot()
            .expect("snapshot should serialize"),
        right
            .canonical_snapshot()
            .expect("snapshot should serialize")
    );
    assert_eq!(
        left.snapshot_digest().expect("snapshot should hash"),
        right.snapshot_digest().expect("snapshot should hash")
    );
}

#[test]
fn observation_is_validated_against_the_registered_definition() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("definition should register");

    registry
        .validate_observation(&observation())
        .expect("matching observation should validate");
}

#[test]
fn observation_rejects_invalid_point_in_time_ordering() {
    let mut input = observation().into_input();
    input.as_known_at = UnixNanos::new(1_900_000_000);

    assert_eq!(
        FeatureObservation::try_new(input),
        Err(RegistryError::InvalidTimeOrdering)
    );
}

#[test]
fn quality_is_exact_and_bounded() {
    assert_eq!(
        QualityScore::from_millionths(1_000_001),
        Err(RegistryError::InvalidQualityScore)
    );
    assert_eq!(
        QualityScore::from_millionths(900_000)
            .expect("score should be valid")
            .millionths(),
        900_000
    );
}

#[test]
fn present_and_missing_states_cannot_be_contradictory() {
    let missing = FeatureDatum::Missing(MissingnessReason::SequenceGap);
    assert!(missing.value().is_none());
    assert_eq!(
        missing.missingness_reason(),
        Some(MissingnessReason::SequenceGap)
    );
}

#[test]
fn formula_documentation_lineage_and_commit_are_fail_closed() {
    assert_eq!(
        FormulaHash::new([0; 32]),
        Err(RegistryError::ZeroDigest {
            field: "formula_hash"
        })
    );
    assert_eq!(
        LineageHash::new([0; 32]),
        Err(RegistryError::ZeroDigest {
            field: "lineage_hash"
        })
    );
    assert!(FeatureDocumentation::new("https://example.com/undurable").is_err());
    assert!(CodeRevision::new("main").is_err());
}

#[test]
fn corrected_observation_requires_a_later_revision() {
    let mut input = observation().into_input();
    input.finality_state = FinalityState::Corrected;

    assert_eq!(
        FeatureObservation::try_new(input),
        Err(RegistryError::InvalidCorrectionRevision)
    );
}

#[test]
fn source_coverage_rejects_duplicate_source_identities() {
    let binance = source("binance");
    assert_eq!(
        SourceCoverage::try_new(vec![
            SourceCoverageEntry::new(binance.clone(), SourceHealthState::Healthy),
            SourceCoverageEntry::new(binance, SourceHealthState::Degraded),
        ]),
        Err(RegistryError::DuplicateSource)
    );
}

#[test]
fn source_coverage_rejects_oversized_input_before_cloning() {
    let entries = (0..65)
        .map(|index| {
            SourceCoverageEntry::new(
                source(&format!("venue_{index}")),
                SourceHealthState::Healthy,
            )
        })
        .collect();

    assert_eq!(
        SourceCoverage::try_new(entries),
        Err(RegistryError::InvalidSourceCoverage)
    );
}

#[test]
fn registry_rejects_observation_contract_drift() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("definition should register");
    let mut input = observation().into_input();
    input.normalization_version = version("2.0.0");
    let drifted = FeatureObservation::try_new(input).expect("observation shape should be valid");

    assert_eq!(
        registry.validate_observation(&drifted),
        Err(RegistryError::NormalizationVersionMismatch)
    );
}

#[test]
fn identifiers_and_code_revisions_are_canonical() {
    assert!(FeatureId::new("").is_err());
    assert!(FeatureId::new("Future_Leak").is_err());
    assert!(FeatureId::new("foo.bar").is_err());
    assert!(FeatureId::new("foo-bar").is_err());
    assert_eq!(
        FeatureId::new("foo_bar")
            .expect("canonical feature ID should be valid")
            .documentation_anchor(),
        "foo-bar"
    );
    assert!(WindowId::new("../escape").is_err());
    assert!(InputRequirement::new("normalized trades").is_err());
    assert!(CodeRevision::new("0123456").is_err());
    assert!(CodeRevision::new("0123456789ABCDEF0123456789ABCDEF01234567").is_err());
    assert!(CodeRevision::new("0000000000000000000000000000000000000000").is_err());
    assert!(
        CodeRevision::new("0000000000000000000000000000000000000000000000000000000000000000")
            .is_err()
    );
}

#[test]
fn window_parameters_match_their_semantics() {
    assert!(
        WindowDefinition::try_new_time(
            WindowId::new("count").expect("test ID should be valid"),
            WindowKind::EventCount,
            DurationNanos::new(1),
            None,
        )
        .is_err()
    );
    assert!(
        WindowDefinition::try_new_event_count(
            WindowId::new("count_100").expect("test ID should be valid"),
            NonZeroU64::new(100).expect("test count should be nonzero"),
            Some(NonZeroU64::new(10).expect("test advance should be nonzero")),
        )
        .is_ok()
    );
    assert!(
        WindowDefinition::try_new_session_aligned(
            WindowId::new("utc_day").expect("test ID should be valid"),
            DurationNanos::new(86_400_000_000_000),
            UnixNanos::new(1_704_067_200_000_000_000),
        )
        .is_ok()
    );
    assert!(
        WindowDefinition::try_new_exponentially_weighted(
            WindowId::new("ewma_5m").expect("test ID should be valid"),
            DurationNanos::new(300_000_000_000),
        )
        .is_ok()
    );
    assert!(
        WindowDefinition::try_new_threshold(
            WindowId::new("volume_1btc").expect("test ID should be valid"),
            WindowKind::Volume,
            FixedDecimal::parse_canonical("1").expect("test threshold should be valid"),
        )
        .is_ok()
    );
    assert!(
        WindowDefinition::try_new_threshold(
            WindowId::new("negative").expect("test ID should be valid"),
            WindowKind::Notional,
            FixedDecimal::parse_canonical("-1").expect("test threshold should parse"),
        )
        .is_err()
    );
}

#[test]
fn finite_float_rejects_nan_and_canonicalizes_negative_zero() {
    assert!(feature_registry::FiniteF64::new(f64::NAN).is_err());
    assert!(feature_registry::FiniteF64::new(f64::INFINITY).is_err());
    assert_eq!(
        feature_registry::FiniteF64::new(-0.0)
            .expect("negative zero should canonicalize")
            .value()
            .to_bits(),
        0.0_f64.to_bits()
    );
}

#[test]
fn registry_capacity_is_explicit_and_transactional() {
    let mut registry = FeatureRegistry::with_capacity(1).expect("capacity should be valid");
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("first definition should register");

    assert_eq!(
        registry.register(definition("trade_imbalance", "1.0.0")),
        Err(RegistryError::RegistryCapacityExceeded)
    );
    assert_eq!(registry.len(), 1);
    assert!(FeatureRegistry::with_capacity(0).is_err());
}

#[test]
fn registry_rejects_formula_quality_source_and_finality_drift() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("definition should register");

    let mut formula_input = observation().into_input();
    formula_input.formula_hash =
        FormulaHash::new(hash(8)).expect("test formula hash should be valid");
    assert_eq!(
        registry.validate_observation(
            &FeatureObservation::try_new(formula_input).expect("observation should be shaped")
        ),
        Err(RegistryError::FormulaHashMismatch)
    );

    let mut quality_input = observation().into_input();
    quality_input.quality_score =
        QualityScore::from_millionths(899_999).expect("test score should be valid");
    assert_eq!(
        registry.validate_observation(
            &FeatureObservation::try_new(quality_input).expect("observation should be shaped")
        ),
        Err(RegistryError::QualityBelowRequirement)
    );

    let mut source_input = observation().into_input();
    source_input.source_coverage = SourceCoverage::try_new(vec![SourceCoverageEntry::new(
        source("binance"),
        SourceHealthState::Quarantined,
    )])
    .expect("coverage should be shaped");
    assert_eq!(
        registry.validate_observation(
            &FeatureObservation::try_new(source_input).expect("observation should be shaped")
        ),
        Err(RegistryError::SourceHealthRejected)
    );

    let mut final_input = observation().into_input();
    final_input.finality_state = FinalityState::Final;
    assert_eq!(
        registry.validate_observation(
            &FeatureObservation::try_new(final_input).expect("observation should be shaped")
        ),
        Err(RegistryError::FinalityBeforeWatermark)
    );
}

#[test]
fn registry_rejects_incomplete_expected_source_coverage() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("definition should register");
    let mut input = observation().into_input();
    input.source_coverage = SourceCoverage::try_new_partial(
        vec![source("binance"), source("kraken")],
        vec![SourceCoverageEntry::new(
            source("binance"),
            SourceHealthState::Healthy,
        )],
    )
    .expect("partial coverage should be representable");
    let incomplete = FeatureObservation::try_new(input).expect("observation should be shaped");

    assert_eq!(incomplete.source_coverage().coverage_millionths(), 500_000);
    assert_eq!(
        registry.validate_observation(&incomplete),
        Err(RegistryError::CoverageBelowRequirement)
    );
}

#[test]
fn machine_snapshot_is_canonical_json_with_no_runtime_capacity() {
    let mut registry = FeatureRegistry::with_capacity(3).expect("capacity should be valid");
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("definition should register");
    let snapshot: serde_json::Value = serde_json::from_slice(
        &registry
            .canonical_snapshot()
            .expect("snapshot should serialize"),
    )
    .expect("snapshot should be JSON");

    assert_eq!(snapshot["schema_version"], 1);
    assert_eq!(snapshot["definitions"][0]["id"], "realized_volatility");
    assert!(snapshot.get("capacity").is_none());
}

#[test]
fn observation_wire_shape_matches_the_normative_contract() {
    let wire = serde_json::to_value(observation()).expect("observation should serialize");
    let object = wire
        .as_object()
        .expect("observation wire representation should be an object");

    for field in [
        "feature_id",
        "feature_version",
        "entity_id",
        "window_id",
        "resolution",
        "value",
        "value_type",
        "event_time_start",
        "event_time_end",
        "as_known_at",
        "computed_at",
        "watermark",
        "finality_state",
        "revision",
        "source_coverage",
        "quality_score",
        "missingness_reason",
        "normalization_version",
        "formula_hash",
        "code_commit",
        "lineage_hash",
    ] {
        assert!(object.contains_key(field), "wire shape is missing {field}");
    }
    assert_eq!(object.len(), 21);
    assert_eq!(wire["value"], "0.125");
    assert!(wire["missingness_reason"].is_null());
    assert!(object.get("datum").is_none());
    assert!(object.get("entity").is_none());
}

#[test]
fn registry_snapshot_digest_is_schema_pinned() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("definition should register");

    assert_eq!(
        registry.snapshot_digest().expect("snapshot should hash"),
        [
            191, 110, 2, 31, 210, 189, 255, 121, 89, 151, 224, 194, 55, 37, 145, 81, 154, 208, 70,
            28, 129, 116, 27, 111, 207, 35, 230, 85, 106, 99, 170, 149,
        ],
        "update this golden only with an intentional snapshot schema version change"
    );
}

#[test]
fn semantic_version_metadata_is_bounded() {
    let oversized = version(&format!("1.0.0+{}", "a".repeat(129)));
    let mut input = definition_input("oversized_version", "1.0.0");
    input.version = oversized.clone();

    assert_eq!(
        FeatureDefinition::try_new(input),
        Err(RegistryError::InvalidVersion {
            field: "feature_version"
        })
    );
    assert_eq!(
        NormalizationPolicy::try_new(NormalizationKind::None, oversized.clone()),
        Err(RegistryError::InvalidVersion {
            field: "normalization_version"
        })
    );

    let mut observation_input = observation().into_input();
    observation_input.feature_version = oversized.clone();
    assert_eq!(
        FeatureObservation::try_new(observation_input),
        Err(RegistryError::InvalidVersion {
            field: "feature_version"
        })
    );

    let registry = FeatureRegistry::new();
    let id = FeatureId::new("realized_volatility").expect("test ID should be valid");
    assert!(!registry.contains(&id, &oversized));
    assert!(registry.get(&id, &oversized).is_none());
}

#[test]
fn missing_observation_still_must_match_definition_value_type() {
    let mut registry = FeatureRegistry::new();
    registry
        .register(definition("realized_volatility", "1.0.0"))
        .expect("definition should register");
    let mut input = observation().into_input();
    input.datum = FeatureDatum::Missing(MissingnessReason::SequenceGap);
    input.value_type = FeatureValueType::Boolean;
    let drifted = FeatureObservation::try_new(input).expect("observation shape should be valid");

    assert_eq!(
        registry.validate_observation(&drifted),
        Err(RegistryError::ValueTypeMismatch)
    );
}

#[test]
fn definition_documentation_anchor_must_match_feature_id() {
    let mut input = definition_input("trade_imbalance", "1.0.0");
    input.documentation =
        FeatureDocumentation::new("docs/data-dictionary/features.md#realized-volatility")
            .expect("documentation shape should be valid");

    assert_eq!(
        FeatureDefinition::try_new(input),
        Err(RegistryError::DocumentationFeatureMismatch)
    );
}

#[test]
fn audit_accessors_expose_point_in_time_and_lineage_fields() {
    let observation = observation();

    assert_eq!(
        observation.event_time_start(),
        UnixNanos::new(1_000_000_000)
    );
    assert_eq!(observation.as_known_at(), UnixNanos::new(2_100_000_000));
    assert_eq!(observation.computed_at(), UnixNanos::new(2_200_000_000));
    assert_eq!(observation.revision().value(), 1);
    assert_eq!(
        observation.code_commit().as_str(),
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(observation.lineage_hash().bytes(), hash(9));
}

#[test]
fn data_dictionary_covers_the_normative_contract_and_fixture() {
    let dictionary = include_str!("../../../docs/data-dictionary/features.md");
    for required in [
        "## Observation contract",
        "`as_known_at`",
        "`missingness_reason`",
        "`lineage_hash`",
        "## Feature definition contract",
        "## realized-volatility",
        "## trade-imbalance",
        "No-leakage rule",
    ] {
        assert!(
            dictionary.contains(required),
            "feature dictionary is missing {required}"
        );
    }

    for feature in [
        definition("realized_volatility", "1.0.0"),
        definition("trade_imbalance", "1.0.0"),
    ] {
        let heading = format!("## {}", feature.id().documentation_anchor());
        assert_eq!(
            dictionary.lines().filter(|line| *line == heading).count(),
            1,
            "each registry fixture must own one exact dictionary heading"
        );
        assert_eq!(
            feature.documentation().anchor(),
            feature.id().documentation_anchor()
        );
    }
}
