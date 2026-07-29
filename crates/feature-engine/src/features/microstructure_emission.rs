//! Evidence-derived typed observation emission for Task 5 recipes.

use blake3::Hasher;
use feature_registry::{
    CodeRevision, FeatureDatum, FeatureEntity, FeatureObservation, FeatureObservationInput,
    FeatureRegistry, FeatureValue, FinalityState, FiniteF64, FixedDecimalMap, LineageHash,
    ObservationRevision, QualityScore, RegistryError,
};
use fixed_decimal::{FixedDecimal, Price};
use thiserror::Error;

use crate::{Finalization, FinalizationDecision, WatermarkTracker};

use super::{
    BookStateEvidence, FeatureComputationError, TradeFlowWindow, active_level_counts,
    aggressive_trade_imbalances, aggressive_trade_sums, ask_book_shape_quadratic,
    ask_level_gap_density, bid_book_shape_quadratic, bid_level_gap_density, book_distribution,
    fully_observed_depth_within_band, interarrival_coefficient_of_variation, microprice,
    microstructure_catalog, normalized_imbalance, signed_volume_at_price, spread,
    trade_intensity_per_second, weighted_midpoint,
};

const BOOK_CORE_EVIDENCE_DOMAIN: &[u8] = b"crypto-intelligence/task5-book-core-evidence/v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Task5FeatureEmissionInput {
    pub computed_at: domain::UnixNanos,
    pub revision: ObservationRevision,
    pub code_commit: CodeRevision,
}

#[derive(Debug, Error)]
pub enum Task5EmissionError {
    #[error("Task 5 computation failed")]
    Computation(#[from] FeatureComputationError),
    #[error("Task 5 recipe is absent from the registry")]
    UnknownDefinition,
    #[error("Task 5 registry definition differs from the closed recipe")]
    DefinitionMismatch,
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Computes and emits the exact and relative spread outputs from one
/// authenticated trusted-book event and matching finality decision.
pub fn emit_book_spread_features(
    registry: &FeatureRegistry,
    emission: Task5FeatureEmissionInput,
    book: &orderbook::BookSnapshotView,
    evidence: &BookStateEvidence,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task5EmissionError> {
    let context = validate_book_snapshot_emission(book, evidence, tracker, decision)?;
    let metrics = spread(book)?;
    let inputs = [
        (
            "absolute_spread",
            FeatureDatum::Present(FeatureValue::FixedDecimal(metrics.absolute())),
        ),
        (
            "relative_spread",
            FeatureDatum::Present(FeatureValue::Float64(FiniteF64::new(metrics.relative())?)),
        ),
    ];
    inputs
        .into_iter()
        .map(|(recipe_id, datum)| {
            emit_bound_datum(
                registry,
                &emission,
                recipe_id,
                datum,
                &context.entity,
                evidence.as_known_at(),
                BOOK_CORE_EVIDENCE_DOMAIN,
                context.input_digest,
                tracker,
                decision,
                context.quality_score,
            )
        })
        .collect()
}

/// Computes the 37 one-second snapshot recipes whose complete formula identity
/// has no unresolved threshold, level-count, slippage, or inference parameter.
/// The two USD recipes are emitted as explicit missing values until conversion
/// evidence exists.
pub fn emit_book_snapshot_core_features(
    registry: &FeatureRegistry,
    emission: Task5FeatureEmissionInput,
    book: &orderbook::BookSnapshotView,
    evidence: &BookStateEvidence,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task5EmissionError> {
    let context = validate_book_snapshot_emission(book, evidence, tracker, decision)?;
    let reference_price = exact_midpoint(book)?;
    let metrics = spread(book)?;
    let mut inputs = vec![
        (
            "absolute_spread",
            FeatureDatum::Present(FeatureValue::FixedDecimal(metrics.absolute())),
        ),
        ("relative_spread", finite_datum(metrics.relative())?),
    ];
    const BANDS: [(&str, &str, &str, u32); 7] = [
        ("bid_depth_1bps", "ask_depth_1bps", "book_imbalance_1bps", 1),
        ("bid_depth_2bps", "ask_depth_2bps", "book_imbalance_2bps", 2),
        ("bid_depth_5bps", "ask_depth_5bps", "book_imbalance_5bps", 5),
        (
            "bid_depth_10bps",
            "ask_depth_10bps",
            "book_imbalance_10bps",
            10,
        ),
        (
            "bid_depth_25bps",
            "ask_depth_25bps",
            "book_imbalance_25bps",
            25,
        ),
        (
            "bid_depth_50bps",
            "ask_depth_50bps",
            "book_imbalance_50bps",
            50,
        ),
        (
            "bid_depth_100bps",
            "ask_depth_100bps",
            "book_imbalance_100bps",
            100,
        ),
    ];
    for (bid_id, ask_id, imbalance_id, band_bps) in BANDS {
        match fully_observed_depth_within_band(book, reference_price, band_bps) {
            Ok(depth) => {
                let bid_quantity = depth.bid_quantity();
                let ask_quantity = depth.ask_quantity();
                let bid_datum = match bid_quantity {
                    Ok(quantity) => {
                        FeatureDatum::Present(FeatureValue::FixedDecimal(quantity.value()))
                    }
                    Err(error) => computation_missing(error)?,
                };
                let ask_datum = match ask_quantity {
                    Ok(quantity) => {
                        FeatureDatum::Present(FeatureValue::FixedDecimal(quantity.value()))
                    }
                    Err(error) => computation_missing(error)?,
                };
                let imbalance = match (bid_quantity, ask_quantity) {
                    (Ok(_), Ok(_)) => match normalized_imbalance(book, reference_price, band_bps) {
                        Ok(value) => finite_datum(value)?,
                        Err(FeatureComputationError::ZeroDenominator) => FeatureDatum::Missing(
                            feature_registry::MissingnessReason::BelowLiquidityThreshold,
                        ),
                        Err(error) => computation_missing(error)?,
                    },
                    _ => FeatureDatum::Missing(
                        feature_registry::MissingnessReason::InsufficientHistory,
                    ),
                };
                inputs.push((bid_id, bid_datum));
                inputs.push((ask_id, ask_datum));
                inputs.push((imbalance_id, imbalance));
            }
            Err(error) => {
                let missing = computation_missing(error)?;
                inputs.push((bid_id, missing.clone()));
                inputs.push((ask_id, missing.clone()));
                inputs.push((imbalance_id, missing));
            }
        }
    }
    inputs.push((
        "weighted_midpoint",
        analytical_datum(weighted_midpoint(book))?,
    ));
    inputs.push(("microprice", analytical_datum(microprice(book))?));
    match bid_book_shape_quadratic(
        book,
        book.price_tick(),
        microstructure_catalog::BOOK_SHAPE_LEVEL_COUNT_V1,
    ) {
        Ok(shape) => {
            inputs.push(("bid_book_slope", finite_datum(shape.slope_at_inside())?));
            inputs.push(("bid_book_convexity", finite_datum(shape.convexity())?));
        }
        Err(error) => {
            let missing = computation_missing(error)?;
            inputs.push(("bid_book_slope", missing.clone()));
            inputs.push(("bid_book_convexity", missing));
        }
    }
    match ask_book_shape_quadratic(
        book,
        book.price_tick(),
        microstructure_catalog::BOOK_SHAPE_LEVEL_COUNT_V1,
    ) {
        Ok(shape) => {
            inputs.push(("ask_book_slope", finite_datum(shape.slope_at_inside())?));
            inputs.push(("ask_book_convexity", finite_datum(shape.convexity())?));
        }
        Err(error) => {
            let missing = computation_missing(error)?;
            inputs.push(("ask_book_slope", missing.clone()));
            inputs.push(("ask_book_convexity", missing));
        }
    }
    inputs.push((
        "bid_price_level_gap_density",
        analytical_datum(bid_level_gap_density(book, book.price_tick()))?,
    ));
    inputs.push((
        "ask_price_level_gap_density",
        analytical_datum(ask_level_gap_density(book, book.price_tick()))?,
    ));
    inputs.push((
        "expected_buy_sweep_cost_usd_10000",
        FeatureDatum::Missing(feature_registry::MissingnessReason::SourceNotSupported),
    ));
    inputs.push((
        "expected_sell_sweep_cost_usd_10000",
        FeatureDatum::Missing(feature_registry::MissingnessReason::SourceNotSupported),
    ));
    match active_level_counts(book) {
        Ok(levels) => {
            inputs.push((
                "bid_active_price_levels",
                FeatureDatum::Present(FeatureValue::Integer(usize_to_i64(levels.bid())?)),
            ));
            inputs.push((
                "ask_active_price_levels",
                FeatureDatum::Present(FeatureValue::Integer(usize_to_i64(levels.ask())?)),
            ));
        }
        Err(error) => {
            let missing = computation_missing(error)?;
            inputs.push(("bid_active_price_levels", missing.clone()));
            inputs.push(("ask_active_price_levels", missing));
        }
    }
    match book_distribution(book) {
        Ok(distribution) => {
            inputs.push((
                "local_book_entropy",
                finite_datum(distribution.normalized_entropy())?,
            ));
            inputs.push((
                "depth_concentration_by_level",
                finite_datum(distribution.concentration())?,
            ));
        }
        Err(error) => {
            let missing = computation_missing(error)?;
            inputs.push(("local_book_entropy", missing.clone()));
            inputs.push(("depth_concentration_by_level", missing));
        }
    }
    inputs
        .into_iter()
        .map(|(recipe_id, datum)| {
            emit_bound_datum(
                registry,
                &emission,
                recipe_id,
                datum,
                &context.entity,
                evidence.as_known_at(),
                BOOK_CORE_EVIDENCE_DOMAIN,
                context.input_digest,
                tracker,
                decision,
                context.quality_score,
            )
        })
        .collect()
}

struct BookEmissionContext {
    entity: FeatureEntity,
    input_digest: [u8; 32],
    quality_score: QualityScore,
}

fn validate_book_snapshot_emission(
    book: &orderbook::BookSnapshotView,
    evidence: &BookStateEvidence,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<BookEmissionContext, Task5EmissionError> {
    if !evidence.matches(book) {
        return Err(FeatureComputationError::UntrustedInput.into());
    }
    let definition_lineage = evidence
        .instrument_definition_lineage()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let entity = FeatureEntity::Instrument(book.instrument().clone());
    if !tracker.validates_decision(decision, &entity)
        || decision.state() != Finalization::Final
        || evidence.event_time() < decision.window().start()
        || evidence.event_time() >= decision.window().end()
    {
        return Err(FeatureComputationError::WindowNotFinal.into());
    }
    let coverage = tracker
        .coverage_for_decision(decision)
        .ok_or(FeatureComputationError::UntrustedInput)?;
    if coverage.expected_sources().len() != 1
        || !coverage.expected_sources().contains(evidence.source())
    {
        return Err(FeatureComputationError::EligibleUniverseChanged.into());
    }
    Ok(BookEmissionContext {
        entity,
        input_digest: book_input_digest(evidence, definition_lineage),
        quality_score: QualityScore::from_millionths(evidence.quality_score_ppm())?,
    })
}

/// Computes and emits the closed core trade-flow recipe set from authenticated
/// normalized trade events in one finalized one-minute window.
pub fn emit_trade_flow_core_features(
    registry: &FeatureRegistry,
    emission: Task5FeatureEmissionInput,
    window: &TradeFlowWindow,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task5EmissionError> {
    if !window.is_authenticated() {
        return Err(FeatureComputationError::UntrustedInput.into());
    }
    let entity = FeatureEntity::Instrument(window.instrument().clone());
    if !tracker.validates_decision(decision, &entity)
        || decision.state() != Finalization::Final
        || decision.window().start() != window.start()
        || decision.window().end() != window.end()
    {
        return Err(FeatureComputationError::WindowNotFinal.into());
    }
    let coverage = tracker
        .coverage_for_decision(decision)
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let sources = window.sources();
    if sources.len() != coverage.expected_sources().len()
        || sources
            .iter()
            .any(|source| !coverage.expected_sources().contains(source))
    {
        return Err(FeatureComputationError::EligibleUniverseChanged.into());
    }
    if window
        .authorities()
        .iter()
        .any(super::AggressorAuthority::is_inferred)
    {
        return Err(FeatureComputationError::SourceNotSupported.into());
    }
    let quality_score = QualityScore::from_millionths(
        window
            .minimum_quality_score_ppm()
            .ok_or(FeatureComputationError::UntrustedInput)?,
    )?;
    let sums = aggressive_trade_sums(window)?;
    let imbalances = aggressive_trade_imbalances(window);
    let price_map = signed_volume_at_price(window)?
        .into_iter()
        .map(|(price, quantity)| (price.value(), quantity))
        .collect();
    let fixed_map = FixedDecimalMap::try_new(price_map, window.observations().len().max(1))?;
    let inputs = [
        (
            "aggressive_buy_count",
            FeatureDatum::Present(FeatureValue::Integer(usize_to_i64(sums.buy_count())?)),
        ),
        (
            "aggressive_sell_count",
            FeatureDatum::Present(FeatureValue::Integer(usize_to_i64(sums.sell_count())?)),
        ),
        (
            "aggressive_buy_quantity",
            FeatureDatum::Present(FeatureValue::FixedDecimal(sums.buy_quantity().value())),
        ),
        (
            "aggressive_sell_quantity",
            FeatureDatum::Present(FeatureValue::FixedDecimal(sums.sell_quantity().value())),
        ),
        (
            "aggressive_buy_notional",
            FeatureDatum::Present(FeatureValue::FixedDecimal(sums.buy_notional())),
        ),
        (
            "aggressive_sell_notional",
            FeatureDatum::Present(FeatureValue::FixedDecimal(sums.sell_notional())),
        ),
        (
            "signed_trade_count_imbalance",
            match imbalances {
                Ok(value) => finite_datum(value.signed_count_imbalance())?,
                Err(FeatureComputationError::ZeroDenominator) => {
                    FeatureDatum::Missing(feature_registry::MissingnessReason::InsufficientHistory)
                }
                Err(error) => return Err(error.into()),
            },
        ),
        (
            "signed_trade_quantity_imbalance",
            match imbalances {
                Ok(value) => finite_datum(value.signed_quantity_imbalance())?,
                Err(FeatureComputationError::ZeroDenominator) => {
                    FeatureDatum::Missing(feature_registry::MissingnessReason::InsufficientHistory)
                }
                Err(error) => return Err(error.into()),
            },
        ),
        (
            "signed_trade_notional_imbalance",
            match imbalances {
                Ok(value) => finite_datum(value.signed_notional_imbalance())?,
                Err(FeatureComputationError::ZeroDenominator) => {
                    FeatureDatum::Missing(feature_registry::MissingnessReason::InsufficientHistory)
                }
                Err(error) => return Err(error.into()),
            },
        ),
        (
            "trade_intensity_per_second",
            finite_datum(trade_intensity_per_second(window)?)?,
        ),
        (
            "trade_interarrival_coefficient_of_variation",
            analytical_datum(interarrival_coefficient_of_variation(window))?,
        ),
        (
            "signed_volume_at_price",
            FeatureDatum::Present(FeatureValue::FixedDecimalMap(fixed_map)),
        ),
    ];
    let evidence_as_known_at = window
        .maximum_as_known_at()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let input_digest = window.canonical_evidence_digest()?;
    inputs
        .into_iter()
        .map(|(recipe_id, datum)| {
            emit_bound_datum(
                registry,
                &emission,
                recipe_id,
                datum,
                &entity,
                evidence_as_known_at,
                b"crypto-intelligence/task5-trade-flow-evidence/v1",
                input_digest,
                tracker,
                decision,
                quality_score,
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn emit_bound_datum(
    registry: &FeatureRegistry,
    emission: &Task5FeatureEmissionInput,
    recipe_id: &str,
    datum: FeatureDatum,
    entity: &FeatureEntity,
    evidence_as_known_at: domain::UnixNanos,
    evidence_domain: &[u8],
    input_digest: [u8; 32],
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
    quality_score: QualityScore,
) -> Result<FeatureObservation, Task5EmissionError> {
    let recipe = microstructure_catalog::task_five_recipe(recipe_id)
        .ok_or(Task5EmissionError::UnknownDefinition)?;
    let expected = recipe.definition()?;
    let definition = registry
        .get(expected.id(), expected.version())
        .ok_or(Task5EmissionError::UnknownDefinition)?;
    if definition != &expected
        || matches!(
            &datum,
            FeatureDatum::Present(value) if definition.value_type() != value.value_type()
        )
        || !super::registered_window_matches(definition, &recipe.window_id()?, decision.window())
    {
        return Err(Task5EmissionError::DefinitionMismatch);
    }
    let coverage = tracker
        .coverage_for_decision(decision)
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let lineage_hash = task_five_lineage_hash(
        definition,
        emission,
        evidence_domain,
        input_digest,
        decision,
        &datum,
    )?;
    let observation = FeatureObservation::try_new(FeatureObservationInput {
        feature_id: definition.id().clone(),
        feature_version: definition.version().clone(),
        entity: entity.clone(),
        window_id: recipe.window_id()?,
        resolution: definition.output_resolution(),
        datum,
        value_type: definition.value_type(),
        event_time_start: decision.window().start(),
        event_time_end: decision.window().end(),
        as_known_at: evidence_as_known_at.max(decision.as_known_at()),
        computed_at: emission.computed_at,
        watermark: decision.watermark(),
        finality_as_known_at: decision.as_known_at(),
        finality_state: FinalityState::Final,
        revision: emission.revision,
        source_coverage: coverage,
        quality_score,
        normalization_version: definition.normalization().version().clone(),
        formula_hash: definition.formula_hash(),
        code_commit: emission.code_commit.clone(),
        lineage_hash,
    })?;
    registry.validate_observation(&observation)?;
    Ok(observation)
}

fn task_five_lineage_hash(
    definition: &feature_registry::FeatureDefinition,
    emission: &Task5FeatureEmissionInput,
    evidence_domain: &[u8],
    input_digest: [u8; 32],
    decision: FinalizationDecision,
    datum: &FeatureDatum,
) -> Result<LineageHash, RegistryError> {
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/task5-feature-lineage/v2");
    hash_bytes(&mut hasher, evidence_domain);
    hash_bytes(&mut hasher, definition.id().as_str().as_bytes());
    hash_bytes(&mut hasher, definition.version().to_string().as_bytes());
    hasher.update(&definition.formula_hash().bytes());
    hasher.update(&decision.evidence_digest());
    hasher.update(&input_digest);
    hasher.update(&emission.computed_at.value().to_be_bytes());
    hasher.update(&emission.revision.value().to_be_bytes());
    hash_bytes(&mut hasher, emission.code_commit.as_str().as_bytes());
    hash_datum(&mut hasher, datum);
    LineageHash::new(*hasher.finalize().as_bytes())
}

fn book_input_digest(
    evidence: &BookStateEvidence,
    instrument_definition_lineage: [u8; 32],
) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/task5-book-input-evidence/v2");
    hasher.update(&evidence.lineage());
    hasher.update(&instrument_definition_lineage);
    hasher.update(&evidence.state_digest());
    hash_bytes(&mut hasher, evidence.source().to_string().as_bytes());
    *hasher.finalize().as_bytes()
}

fn exact_midpoint(book: &orderbook::BookSnapshotView) -> Result<Price, FeatureComputationError> {
    let bid = book
        .best_bid()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let ask = book
        .best_ask()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let sum = bid
        .price
        .value()
        .checked_add(ask.price.value())
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let divisor = FixedDecimal::new(2, 0).map_err(|_| FeatureComputationError::InvalidParameter)?;
    let midpoint = sum
        .checked_div_exact(divisor)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    Price::new(midpoint).map_err(|_| FeatureComputationError::InvalidInput)
}

fn usize_to_i64(value: usize) -> Result<i64, FeatureComputationError> {
    i64::try_from(value).map_err(|_| FeatureComputationError::CapacityExceeded)
}

fn finite_datum(value: f64) -> Result<FeatureDatum, RegistryError> {
    Ok(FeatureDatum::Present(FeatureValue::Float64(
        FiniteF64::new(value)?,
    )))
}

fn analytical_datum(
    result: Result<f64, FeatureComputationError>,
) -> Result<FeatureDatum, Task5EmissionError> {
    match result {
        Ok(value) => finite_datum(value).map_err(Task5EmissionError::Registry),
        Err(error) => error
            .missingness_reason()
            .map(FeatureDatum::Missing)
            .ok_or(Task5EmissionError::Computation(error)),
    }
}

fn computation_missing(error: FeatureComputationError) -> Result<FeatureDatum, Task5EmissionError> {
    error
        .missingness_reason()
        .map(FeatureDatum::Missing)
        .ok_or(Task5EmissionError::Computation(error))
}

fn hash_datum(hasher: &mut Hasher, datum: &FeatureDatum) {
    match datum {
        FeatureDatum::Present(FeatureValue::FixedDecimal(value)) => {
            hasher.update(&[1]);
            hash_bytes(hasher, value.to_string().as_bytes());
        }
        FeatureDatum::Present(FeatureValue::FixedDecimalMap(value)) => {
            hasher.update(&[2]);
            hasher.update(&(value.values().len() as u64).to_be_bytes());
            for (key, item) in value.values() {
                hash_bytes(hasher, key.to_string().as_bytes());
                hash_bytes(hasher, item.to_string().as_bytes());
            }
        }
        FeatureDatum::Present(FeatureValue::Float64(value)) => {
            hasher.update(&[3]);
            hasher.update(&value.value().to_bits().to_be_bytes());
        }
        FeatureDatum::Present(FeatureValue::Integer(value)) => {
            hasher.update(&[4]);
            hasher.update(&value.to_be_bytes());
        }
        FeatureDatum::Present(FeatureValue::Boolean(value)) => {
            hasher.update(&[5, u8::from(*value)]);
        }
        FeatureDatum::Missing(reason) => {
            hasher.update(&[6]);
            hash_bytes(hasher, format!("{reason:?}").as_bytes());
        }
    }
}

fn hash_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}
