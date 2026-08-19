//! Definition- and finality-bound Task 6 observation emission.

use std::collections::BTreeSet;

use blake3::Hasher;
use collector_runtime::{OperationalQualityReceipt, StoragePressureReceipt};
use connector_core::{
    Completeness, CompletenessReason, DerivativeNormalizationReceipt, DerivativeStream,
};
use consolidated_market::FairPrice;
use domain::{InstrumentDefinition, SourceId, UnixNanos};
use event_envelope::UncheckedEventPayload;
use feature_registry::{
    CodeRevision, FeatureDatum, FeatureEntity, FeatureObservation, FeatureObservationInput,
    FeatureRegistry, FeatureValue, FinalityState, LineageHash, MissingnessReason,
    ObservationRevision, QualityScore, RegistryError,
};
use thiserror::Error;

use crate::{Finalization, FinalizationDecision, WatermarkTracker};

use super::{
    CrossVenueSnapshot, FeatureComputationError, MAX_DERIVATIVE_WINDOW_OBSERVATIONS,
    Task6ComputationAvailability, funding_change, hash_entity, liquidation_velocity,
    mark_index_divergence, open_interest_change, operational_quality_snapshot,
    predicted_funding_rate, task_six_recipe,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Task6MissingEmissionInput {
    pub computed_at: domain::UnixNanos,
    pub revision: ObservationRevision,
    pub code_commit: CodeRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Task6FeatureEmissionInput {
    pub computed_at: UnixNanos,
    pub revision: ObservationRevision,
    pub code_commit: CodeRevision,
}

#[derive(Debug, Error)]
pub enum Task6EmissionError {
    #[error("Task 6 computation failed")]
    Computation(#[from] FeatureComputationError),
    #[error("Task 6 recipe is absent")]
    UnknownRecipe,
    #[error("Task 6 registry definition differs from the closed recipe")]
    DefinitionMismatch,
    #[error("implemented Task 6 recipes cannot use the unavailable-feature emitter")]
    ImplementedRecipe,
    #[error("Task 6 finality or entity evidence is untrusted")]
    UntrustedFinality,
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// Emits the four asset-scoped, descriptive cross-venue snapshot features.
///
/// Per-source features use [`emit_cross_venue_source_features`] so their
/// source-scoped finality authority cannot be confused with this asset scope.
pub fn emit_cross_venue_aggregate_features(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    fair_price: &FairPrice,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task6EmissionError> {
    let snapshot = CrossVenueSnapshot::try_from_fair_price(fair_price)?;
    let entity = FeatureEntity::Asset(fair_price.base_asset().clone());
    validate_cross_venue_authority(fair_price, &entity, tracker, decision, emission.computed_at)?;
    let quality_score =
        QualityScore::from_millionths(fair_price.minimum_included_quality().value())?;
    let eligible_count = fair_price
        .lineage()
        .eligible_sources()
        .map(|sources| sources.len())
        .filter(|count| *count > 0)
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let source_coverage = feature_registry::FiniteF64::new(
        fair_price.lineage().included().len() as f64 / eligible_count as f64,
    )?;
    let inputs = [
        (
            "cross_venue_median_absolute_dispersion",
            FeatureDatum::Present(FeatureValue::Float64(snapshot.relative_mad())),
        ),
        (
            "indicative_cross_venue_price_range",
            FeatureDatum::Present(FeatureValue::Float64(snapshot.relative_range())),
        ),
        (
            "venue_concentration_index",
            FeatureDatum::Present(FeatureValue::Float64(snapshot.depth_concentration())),
        ),
        (
            "healthy_venue_fraction",
            FeatureDatum::Present(FeatureValue::Float64(snapshot.healthy_venue_fraction())),
        ),
        (
            "source_coverage_fraction",
            FeatureDatum::Present(FeatureValue::Float64(source_coverage)),
        ),
        (
            "cross_source_disagreement",
            FeatureDatum::Present(FeatureValue::Float64(snapshot.relative_mad())),
        ),
    ];
    inputs
        .into_iter()
        .map(|(recipe_id, datum)| {
            emit_bound_task_six_datum(
                registry,
                &emission,
                recipe_id,
                datum,
                &entity,
                fair_price.lineage().as_of(),
                b"crypto-intelligence/task6-cross-venue-aggregate-evidence/v1",
                snapshot.lineage_digest(),
                tracker,
                decision,
                quality_score,
            )
        })
        .collect()
}

/// Emits the flow-window cascade gate separately from the snapshot
/// uncertainty family so window geometry cannot be reinterpreted.
pub fn emit_cascade_eligibility_gate(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    fair_price: &FairPrice,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<FeatureObservation, Task6EmissionError> {
    let snapshot = CrossVenueSnapshot::try_from_fair_price(fair_price)?;
    let entity = FeatureEntity::Asset(fair_price.base_asset().clone());
    validate_cross_venue_authority(fair_price, &entity, tracker, decision, emission.computed_at)?;
    let eligible_count = fair_price
        .lineage()
        .eligible_sources()
        .map(|sources| sources.len())
        .filter(|count| *count > 0)
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let eligible = fair_price.lineage().included().len() == eligible_count
        && snapshot.healthy_venue_fraction().value() == 1.0;
    emit_bound_task_six_datum(
        registry,
        &emission,
        "cascade_eligibility_gate",
        FeatureDatum::Present(FeatureValue::Boolean(eligible)),
        &entity,
        fair_price.lineage().as_of(),
        b"crypto-intelligence/task6-cascade-gate-evidence/v1",
        snapshot.lineage_digest(),
        tracker,
        decision,
        QualityScore::from_millionths(fair_price.minimum_included_quality().value())?,
    )
}

/// Emits deviation and depth share for one venue using an exact
/// `AssetSource` finality decision over the same eligible-source universe.
pub fn emit_cross_venue_source_features(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    fair_price: &FairPrice,
    source: &SourceId,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task6EmissionError> {
    let snapshot = CrossVenueSnapshot::try_from_fair_price(fair_price)?;
    let venue = snapshot
        .venues()
        .iter()
        .find(|venue| venue.source() == source)
        .ok_or(FeatureComputationError::MixedEntity)?;
    let entity = FeatureEntity::AssetSource(fair_price.base_asset().clone(), source.clone());
    validate_cross_venue_authority(fair_price, &entity, tracker, decision, emission.computed_at)?;
    let quality_score =
        QualityScore::from_millionths(fair_price.minimum_included_quality().value())?;
    let inputs = [
        (
            "venue_midprice_deviation",
            FeatureDatum::Present(FeatureValue::Float64(venue.relative_to_fair())),
        ),
        (
            "venue_depth_share",
            FeatureDatum::Present(FeatureValue::Float64(venue.reference_depth_share())),
        ),
    ];
    inputs
        .into_iter()
        .map(|(recipe_id, datum)| {
            emit_bound_task_six_datum(
                registry,
                &emission,
                recipe_id,
                datum,
                &entity,
                fair_price.lineage().as_of(),
                b"crypto-intelligence/task6-cross-venue-source-evidence/v1",
                snapshot.lineage_digest(),
                tracker,
                decision,
                quality_score,
            )
        })
        .collect()
}

/// Emits the collector-owned source quality and gate family for the receipt's
/// exact one-second snapshot or 60-second flow window.
pub fn emit_operational_source_features(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    receipt: &OperationalQualityReceipt,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task6EmissionError> {
    let sample = receipt.sample();
    let entity = FeatureEntity::Source(receipt.source().clone());
    if decision.state() != Finalization::Final
        || !tracker.validates_decision(decision, &entity)
        || !decision.matches_required_sources(std::slice::from_ref(receipt.source()))
        || decision.window().start() != sample.window_start
        || decision.window().end() != sample.window_end
        || sample.as_known_at > emission.computed_at
    {
        return Err(Task6EmissionError::UntrustedFinality);
    }
    let coverage = tracker
        .coverage_for_decision(decision)
        .ok_or(Task6EmissionError::UntrustedFinality)?;
    if coverage.entries().len() != 1
        || coverage.entries()[0].source() != receipt.source()
        || coverage.entries()[0].health() != receipt.source_health()
    {
        return Err(Task6EmissionError::UntrustedFinality);
    }
    let snapshot = operational_quality_snapshot(receipt)?;
    let uncertainty = snapshot.uncertainty_fields();
    let gates = snapshot.gating_fields();
    let extent = sample
        .window_end
        .value()
        .checked_sub(sample.window_start.value())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(FeatureComputationError::InvalidInput)?;
    let inputs = match extent {
        1_000_000_000 => vec![
            (
                "source_latency_ns",
                integer_datum(uncertainty.source_latency_ns())?,
            ),
            (
                "clock_skew_estimate_ns",
                FeatureDatum::Present(FeatureValue::Integer(uncertainty.clock_skew_estimate_ns())),
            ),
            (
                "stale_quote_duration_ns",
                integer_datum(uncertainty.stale_quote_duration_ns())?,
            ),
            (
                "feature_age_ns",
                integer_datum(
                    sample
                        .as_known_at
                        .value()
                        .checked_sub(sample.window_end.value())
                        .and_then(|value| u64::try_from(value).ok())
                        .ok_or(FeatureComputationError::InvalidInput)?,
                )?,
            ),
            (
                "local_processing_lag_ns",
                integer_datum(
                    emission
                        .computed_at
                        .value()
                        .checked_sub(sample.window_end.value())
                        .and_then(|value| u64::try_from(value).ok())
                        .ok_or(FeatureComputationError::InvalidInput)?,
                )?,
            ),
            (
                "source_outage_indicator",
                FeatureDatum::Present(FeatureValue::Boolean(matches!(
                    receipt.source_health(),
                    quality::SourceHealthState::Unhealthy | quality::SourceHealthState::Quarantined
                ))),
            ),
            (
                "source_health_gate",
                FeatureDatum::Present(FeatureValue::Boolean(gates.eligible())),
            ),
        ],
        60_000_000_000 => vec![
            (
                "feed_jitter_ns",
                integer_datum(uncertainty.feed_jitter_ns())?,
            ),
            (
                "sequence_gap_count",
                integer_datum(uncertainty.sequence_gap_count())?,
            ),
            (
                "checksum_failure_count",
                integer_datum(uncertainty.checksum_failure_count())?,
            ),
            (
                "reconnect_count",
                integer_datum(uncertainty.reconnect_count())?,
            ),
            (
                "recovery_count",
                integer_datum(uncertainty.recovery_count())?,
            ),
            (
                "correction_count",
                integer_datum(uncertainty.correction_count())?,
            ),
            (
                "revision_count",
                integer_datum(uncertainty.revision_count())?,
            ),
            (
                "raw_to_normalized_rejection_count",
                integer_datum(uncertainty.raw_to_normalized_rejection_count())?,
            ),
            (
                "source_completeness_gate",
                FeatureDatum::Present(FeatureValue::Boolean(gates.complete_for_cascade())),
            ),
        ],
        _ => return Err(FeatureComputationError::InvalidInput.into()),
    };
    let input_digest = operational_receipt_digest(receipt);
    inputs
        .into_iter()
        .map(|(recipe_id, datum)| {
            emit_bound_task_six_datum(
                registry,
                &emission,
                recipe_id,
                datum,
                &entity,
                sample.as_known_at,
                b"crypto-intelligence/task6-operational-source-evidence/v1",
                input_digest,
                tracker,
                decision,
                QualityScore::from_millionths(QualityScore::MAX_MILLIONTHS)?,
            )
        })
        .collect()
}

/// Emits the global disk-pressure fraction from supervisor-sealed physical
/// volume capacity evidence.
pub fn emit_storage_pressure_feature(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    receipt: StoragePressureReceipt,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<FeatureObservation, Task6EmissionError> {
    let entity = FeatureEntity::Global;
    if receipt.as_known_at() > emission.computed_at
        || receipt.as_known_at() < decision.window().start()
        || receipt.as_known_at() >= decision.window().end()
    {
        return Err(Task6EmissionError::UntrustedFinality);
    }
    let ratio = receipt.used_bytes() as f64 / receipt.capacity_bytes().get() as f64;
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/task6-storage-pressure-receipt/v1");
    hasher.update(&receipt.used_bytes().to_be_bytes());
    hasher.update(&receipt.capacity_bytes().get().to_be_bytes());
    hasher.update(&receipt.as_known_at().value().to_be_bytes());
    emit_bound_task_six_datum(
        registry,
        &emission,
        "disk_pressure_fraction",
        FeatureDatum::Present(FeatureValue::Float64(feature_registry::FiniteF64::new(
            ratio,
        )?)),
        &entity,
        receipt.as_known_at(),
        b"crypto-intelligence/task6-storage-pressure-evidence/v1",
        *hasher.finalize().as_bytes(),
        tracker,
        decision,
        QualityScore::from_millionths(QualityScore::MAX_MILLIONTHS)?,
    )
}

/// Emits the implemented one-receipt derivative snapshot feature for the
/// receipt's sealed stream. Liquidations require the windowed emitter below.
pub fn emit_derivative_snapshot_features(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    receipt: &DerivativeNormalizationReceipt,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task6EmissionError> {
    validate_derivative_authority(
        &[receipt],
        receipt.instrument_definition(),
        tracker,
        decision,
        emission.computed_at,
    )?;
    let (recipe_id, datum) = match receipt.stream() {
        DerivativeStream::MarkIndex => (
            "mark_index_divergence",
            FeatureDatum::Present(FeatureValue::Float64(mark_index_divergence(receipt)?)),
        ),
        DerivativeStream::Funding => (
            "predicted_funding_rate",
            FeatureDatum::Present(FeatureValue::FixedDecimal(
                predicted_funding_rate(receipt)?.rate(),
            )),
        ),
        DerivativeStream::OpenInterest => {
            let UncheckedEventPayload::OpenInterestObservation(observation) =
                receipt.event().payload().as_unchecked()
            else {
                return Err(FeatureComputationError::UntrustedInput.into());
            };
            (
                "open_interest_native",
                FeatureDatum::Present(FeatureValue::FixedDecimal(observation.quantity.value())),
            )
        }
        DerivativeStream::Liquidation => {
            return Err(FeatureComputationError::InvalidInput.into());
        }
    };
    let entity = FeatureEntity::Instrument(receipt.instrument_definition().id().clone());
    Ok(vec![emit_bound_task_six_datum(
        registry,
        &emission,
        recipe_id,
        datum,
        &entity,
        receipt
            .event()
            .metadata()
            .as_unchecked()
            .normalization_timestamp,
        b"crypto-intelligence/task6-derivative-snapshot-evidence/v1",
        receipt_input_digest(&[receipt]),
        tracker,
        decision,
        receipt_quality_score(&[receipt])?,
    )?])
}

/// Emits the implemented change feature for a sealed, time-ordered receipt
/// pair. Both receipts must belong to the exact finalized flow window.
pub fn emit_derivative_change_features(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    previous: &DerivativeNormalizationReceipt,
    current: &DerivativeNormalizationReceipt,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task6EmissionError> {
    validate_derivative_authority(
        &[previous, current],
        current.instrument_definition(),
        tracker,
        decision,
        emission.computed_at,
    )?;
    let (recipe_id, datum) = match current.stream() {
        DerivativeStream::Funding if previous.stream() == DerivativeStream::Funding => (
            "funding_rate_change",
            FeatureDatum::Present(FeatureValue::FixedDecimal(funding_change(
                previous, current,
            )?)),
        ),
        DerivativeStream::OpenInterest if previous.stream() == DerivativeStream::OpenInterest => (
            "open_interest_relative_change",
            FeatureDatum::Present(FeatureValue::Float64(
                open_interest_change(previous, current, current.instrument_definition())?
                    .relative_change(),
            )),
        ),
        _ => return Err(FeatureComputationError::InvalidInput.into()),
    };
    let entity = FeatureEntity::Instrument(current.instrument_definition().id().clone());
    let as_known_at = previous
        .event()
        .metadata()
        .as_unchecked()
        .normalization_timestamp
        .max(
            current
                .event()
                .metadata()
                .as_unchecked()
                .normalization_timestamp,
        );
    Ok(vec![emit_bound_task_six_datum(
        registry,
        &emission,
        recipe_id,
        datum,
        &entity,
        as_known_at,
        b"crypto-intelligence/task6-derivative-change-evidence/v1",
        receipt_input_digest(&[previous, current]),
        tracker,
        decision,
        receipt_quality_score(&[previous, current])?,
    )?])
}

/// Emits count, quote notional, and velocity from the same sealed liquidation
/// receipts already validated by the lower-bound window computation.
pub fn emit_liquidation_features(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    receipts: &[DerivativeNormalizationReceipt],
    instrument: &InstrumentDefinition,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<Vec<FeatureObservation>, Task6EmissionError> {
    let metrics = liquidation_velocity(receipts, instrument, tracker, decision)?;
    let mut receipt_refs = receipts.iter().collect::<Vec<_>>();
    receipt_refs.sort_unstable_by_key(|receipt| *receipt.event().id().as_bytes());
    validate_derivative_authority(
        &receipt_refs,
        instrument,
        tracker,
        decision,
        emission.computed_at,
    )?;
    let entity = FeatureEntity::Instrument(instrument.id().clone());
    let evidence_as_known_at = receipts
        .iter()
        .map(|receipt| {
            receipt
                .event()
                .metadata()
                .as_unchecked()
                .normalization_timestamp
        })
        .max()
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    let count = i64::try_from(metrics.observed_count())
        .map_err(|_| FeatureComputationError::CapacityExceeded)?;
    let inputs = [
        (
            "liquidation_observed_count",
            FeatureDatum::Present(FeatureValue::Integer(count)),
        ),
        (
            "liquidation_observed_notional",
            FeatureDatum::Present(FeatureValue::FixedDecimal(
                metrics.observed_quote_notional(),
            )),
        ),
        (
            "liquidation_observed_velocity",
            FeatureDatum::Present(FeatureValue::Float64(metrics.observed_count_per_second())),
        ),
    ];
    let input_digest = receipt_input_digest(&receipt_refs);
    let quality_score = receipt_quality_score(&receipt_refs)?;
    inputs
        .into_iter()
        .map(|(recipe_id, datum)| {
            emit_bound_task_six_datum(
                registry,
                &emission,
                recipe_id,
                datum,
                &entity,
                evidence_as_known_at,
                b"crypto-intelligence/task6-liquidation-window-evidence/v1",
                input_digest,
                tracker,
                decision,
                quality_score,
            )
        })
        .collect()
}

/// Emits the connector/window-bound liquidation completeness class as a
/// source-scoped uncertainty feature. The numeric class is stable; cadence
/// and partial-reason detail remain committed in lineage.
pub fn emit_liquidation_completeness_feature(
    registry: &FeatureRegistry,
    emission: Task6FeatureEmissionInput,
    receipts: &[DerivativeNormalizationReceipt],
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<FeatureObservation, Task6EmissionError> {
    if receipts.len() > MAX_DERIVATIVE_WINDOW_OBSERVATIONS {
        return Err(FeatureComputationError::CapacityExceeded.into());
    }
    let first = receipts
        .first()
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    let source = first.event().metadata().as_unchecked().source.clone();
    let entity = FeatureEntity::Source(source.clone());
    if decision.state() != Finalization::Final
        || !tracker.validates_decision(decision, &entity)
        || !decision.matches_required_sources(std::slice::from_ref(&source))
    {
        return Err(Task6EmissionError::UntrustedFinality);
    }
    let completeness = first.completeness();
    let mut lineage = BTreeSet::new();
    let mut as_known_at = first
        .event()
        .metadata()
        .as_unchecked()
        .normalization_timestamp;
    for receipt in receipts {
        let metadata = receipt.event().metadata().as_unchecked();
        let event_time = derivative_event_time(receipt)?;
        if receipt.stream() != DerivativeStream::Liquidation
            || metadata.source != source
            || receipt.completeness() != completeness
            || event_time < decision.window().start()
            || event_time >= decision.window().end()
        {
            return Err(FeatureComputationError::UntrustedInput.into());
        }
        if !lineage.insert(*receipt.event().id().as_bytes()) {
            return Err(FeatureComputationError::DuplicateLineage.into());
        }
        as_known_at = as_known_at.max(metadata.normalization_timestamp);
    }
    let mut receipt_refs = receipts.iter().collect::<Vec<_>>();
    receipt_refs.sort_unstable_by_key(|receipt| *receipt.event().id().as_bytes());
    let quality_score = receipt_quality_score(&receipt_refs)?;
    emit_bound_task_six_datum(
        registry,
        &emission,
        "liquidation_source_completeness_flag",
        FeatureDatum::Present(FeatureValue::Integer(i64::from(completeness_class(
            completeness,
        )))),
        &entity,
        as_known_at,
        b"crypto-intelligence/task6-liquidation-completeness-evidence/v1",
        receipt_input_digest(&receipt_refs),
        tracker,
        decision,
        quality_score,
    )
}

/// Emits an explicit missing observation only for a recipe whose computation
/// contract is not implemented. This is the sole generic Task 6 path: callers
/// cannot use it to publish a present value.
#[allow(clippy::too_many_arguments)]
pub fn emit_unavailable_task_six_feature(
    registry: &FeatureRegistry,
    input: Task6MissingEmissionInput,
    recipe_id: &str,
    entity: FeatureEntity,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<FeatureObservation, Task6EmissionError> {
    let recipe = task_six_recipe(recipe_id).ok_or(Task6EmissionError::UnknownRecipe)?;
    if recipe.computation_availability() != Task6ComputationAvailability::ExplicitlyUnavailable {
        return Err(Task6EmissionError::ImplementedRecipe);
    }
    let expected = recipe.definition()?;
    let definition = registry
        .get(expected.id(), expected.version())
        .ok_or(Task6EmissionError::DefinitionMismatch)?;
    if definition != &expected {
        return Err(Task6EmissionError::DefinitionMismatch);
    }
    if decision.state() != Finalization::Final
        || !decision.matches_entity(&entity)
        || !super::registered_window_matches(definition, &recipe.window_id()?, decision.window())
    {
        return Err(Task6EmissionError::UntrustedFinality);
    }
    let coverage = tracker
        .coverage_for_decision(decision)
        .ok_or(Task6EmissionError::UntrustedFinality)?;
    let reason = MissingnessReason::ModelNotApplicable;
    let datum = FeatureDatum::Missing(reason);
    let lineage_hash = missing_lineage_hash(definition, &input, &entity, decision, reason)?;
    let observation = FeatureObservation::try_new(FeatureObservationInput {
        feature_id: definition.id().clone(),
        feature_version: definition.version().clone(),
        entity,
        window_id: recipe.window_id()?,
        resolution: definition.output_resolution(),
        datum,
        value_type: definition.value_type(),
        event_time_start: decision.window().start(),
        event_time_end: decision.window().end(),
        as_known_at: decision.as_known_at(),
        computed_at: input.computed_at,
        watermark: decision.watermark(),
        finality_as_known_at: decision.as_known_at(),
        finality_state: FinalityState::Final,
        revision: input.revision,
        source_coverage: coverage,
        quality_score: QualityScore::from_millionths(QualityScore::MAX_MILLIONTHS)?,
        normalization_version: definition.normalization().version().clone(),
        formula_hash: definition.formula_hash(),
        code_commit: input.code_commit,
        lineage_hash,
    })?;
    registry.validate_observation(&observation)?;
    Ok(observation)
}

fn validate_cross_venue_authority(
    fair_price: &FairPrice,
    entity: &FeatureEntity,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
    computed_at: UnixNanos,
) -> Result<(), Task6EmissionError> {
    let eligible = fair_price
        .lineage()
        .eligible_sources()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let event_time = fair_price.event_time();
    if decision.state() != Finalization::Final
        || !tracker.validates_decision(decision, entity)
        || !decision.matches_required_sources(eligible)
        || event_time < decision.window().start()
        || event_time >= decision.window().end()
        || fair_price.lineage().as_of() > computed_at
    {
        return Err(Task6EmissionError::UntrustedFinality);
    }
    Ok(())
}

fn validate_derivative_authority(
    receipts: &[&DerivativeNormalizationReceipt],
    instrument: &InstrumentDefinition,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
    computed_at: UnixNanos,
) -> Result<(), Task6EmissionError> {
    let first = receipts
        .first()
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    let source = first.event().metadata().as_unchecked().source.clone();
    let entity = FeatureEntity::Instrument(instrument.id().clone());
    if decision.state() != Finalization::Final
        || !tracker.validates_decision(decision, &entity)
        || !decision.matches_required_sources(std::slice::from_ref(&source))
    {
        return Err(Task6EmissionError::UntrustedFinality);
    }
    for receipt in receipts {
        let metadata = receipt.event().metadata().as_unchecked();
        let event_time = derivative_event_time(receipt)?;
        if receipt.instrument_definition() != instrument
            || metadata.instrument_id.as_ref() != Some(instrument.id())
            || metadata.source != source
            || metadata.normalization_timestamp > computed_at
            || event_time < decision.window().start()
            || event_time >= decision.window().end()
        {
            return Err(FeatureComputationError::UntrustedInput.into());
        }
    }
    Ok(())
}

fn derivative_event_time(
    receipt: &DerivativeNormalizationReceipt,
) -> Result<UnixNanos, FeatureComputationError> {
    let metadata = receipt.event().metadata().as_unchecked();
    metadata
        .exchange_transaction_timestamp
        .or(metadata.exchange_timestamp)
        .ok_or(FeatureComputationError::InvalidInput)
}

#[allow(clippy::too_many_arguments)]
fn emit_bound_task_six_datum(
    registry: &FeatureRegistry,
    emission: &Task6FeatureEmissionInput,
    recipe_id: &str,
    datum: FeatureDatum,
    entity: &FeatureEntity,
    evidence_as_known_at: UnixNanos,
    evidence_domain: &[u8],
    input_digest: [u8; 32],
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
    quality_score: QualityScore,
) -> Result<FeatureObservation, Task6EmissionError> {
    let recipe = task_six_recipe(recipe_id).ok_or(Task6EmissionError::UnknownRecipe)?;
    if recipe.computation_availability() != Task6ComputationAvailability::Implemented {
        return Err(Task6EmissionError::DefinitionMismatch);
    }
    let expected = recipe.definition()?;
    let definition = registry
        .get(expected.id(), expected.version())
        .ok_or(Task6EmissionError::DefinitionMismatch)?;
    if definition != &expected
        || matches!(
            &datum,
            FeatureDatum::Present(value) if definition.value_type() != value.value_type()
        )
        || !super::registered_window_matches(definition, &recipe.window_id()?, decision.window())
    {
        return Err(Task6EmissionError::DefinitionMismatch);
    }
    if decision.state() != Finalization::Final || !tracker.validates_decision(decision, entity) {
        return Err(Task6EmissionError::UntrustedFinality);
    }
    let coverage = tracker
        .coverage_for_decision(decision)
        .ok_or(Task6EmissionError::UntrustedFinality)?;
    let as_known_at = evidence_as_known_at.max(decision.as_known_at());
    if as_known_at > emission.computed_at {
        return Err(FeatureComputationError::FutureKnowledge.into());
    }
    let lineage_hash = task_six_lineage_hash(
        definition,
        emission,
        evidence_domain,
        input_digest,
        entity,
        decision,
        &datum,
        quality_score,
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
        as_known_at,
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

#[allow(clippy::too_many_arguments)]
fn task_six_lineage_hash(
    definition: &feature_registry::FeatureDefinition,
    emission: &Task6FeatureEmissionInput,
    evidence_domain: &[u8],
    input_digest: [u8; 32],
    entity: &FeatureEntity,
    decision: FinalizationDecision,
    datum: &FeatureDatum,
    quality_score: QualityScore,
) -> Result<LineageHash, RegistryError> {
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/task6-feature-lineage/v1");
    hash_bytes(&mut hasher, evidence_domain);
    hash_bytes(&mut hasher, definition.id().as_str().as_bytes());
    hash_bytes(&mut hasher, definition.version().to_string().as_bytes());
    hasher.update(&definition.formula_hash().bytes());
    hasher.update(&decision.evidence_digest());
    hasher.update(&input_digest);
    hash_entity(&mut hasher, entity);
    hash_datum(&mut hasher, datum);
    hasher.update(&quality_score.millionths().to_be_bytes());
    hasher.update(&emission.computed_at.value().to_be_bytes());
    hasher.update(&emission.revision.value().to_be_bytes());
    hash_bytes(&mut hasher, emission.code_commit.as_str().as_bytes());
    LineageHash::new(*hasher.finalize().as_bytes())
}

fn receipt_input_digest(receipts: &[&DerivativeNormalizationReceipt]) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/task6-derivative-receipts/v1");
    for receipt in receipts {
        hasher.update(receipt.event().id().as_bytes());
        hasher.update(&[derivative_stream_tag(receipt.stream())]);
        hasher.update(receipt.catalog_digest());
        hasher.update(receipt.definition_hash());
        let metadata = receipt.event().metadata().as_unchecked();
        hasher.update(&metadata.quality_score_ppm.to_be_bytes());
        hasher.update(&metadata.quality_flags.bits().to_be_bytes());
        hash_completeness(&mut hasher, receipt.completeness());
        let authority = receipt.catalog_authority();
        hasher.update(&authority.catalog_revision().get().to_be_bytes());
        hasher.update(&authority.definition_revision().get().to_be_bytes());
        hash_bytes(&mut hasher, receipt.connector_version().as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn operational_receipt_digest(receipt: &OperationalQualityReceipt) -> [u8; 32] {
    let sample = receipt.sample();
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/task6-operational-quality-receipt/v1");
    hash_entity(
        &mut hasher,
        &FeatureEntity::Source(receipt.source().clone()),
    );
    hasher.update(&receipt.connection_epoch().get().to_be_bytes());
    hasher.update(&[source_health_tag(receipt.source_health())]);
    hasher.update(&[receipt.stream() as u8]);
    hasher.update(&sample.window_start.value().to_be_bytes());
    hasher.update(&sample.window_end.value().to_be_bytes());
    hasher.update(&sample.last_trusted_event_time.value().to_be_bytes());
    hasher.update(&sample.receive_wall_time.value().to_be_bytes());
    hasher.update(&sample.as_known_at.value().to_be_bytes());
    hasher.update(&sample.stale_after_ns.to_be_bytes());
    hasher.update(&sample.feed_jitter_ns.to_be_bytes());
    hasher.update(&sample.clock_skew_estimate_ns.to_be_bytes());
    hasher.update(&receipt.sequence_gap_count().to_be_bytes());
    hasher.update(&receipt.checksum_failure_count().to_be_bytes());
    hasher.update(&receipt.reconnect_count().to_be_bytes());
    hasher.update(&receipt.recovery_count().to_be_bytes());
    hasher.update(&receipt.correction_count().to_be_bytes());
    hasher.update(&receipt.revision_count().to_be_bytes());
    hasher.update(&receipt.raw_to_normalized_rejection_count().to_be_bytes());
    hash_completeness(&mut hasher, receipt.completeness());
    *hasher.finalize().as_bytes()
}

fn integer_datum(value: u64) -> Result<FeatureDatum, FeatureComputationError> {
    Ok(FeatureDatum::Present(FeatureValue::Integer(
        i64::try_from(value).map_err(|_| FeatureComputationError::CapacityExceeded)?,
    )))
}

const fn source_health_tag(state: quality::SourceHealthState) -> u8 {
    match state {
        quality::SourceHealthState::Healthy => 1,
        quality::SourceHealthState::Degraded => 2,
        quality::SourceHealthState::Unhealthy => 3,
        quality::SourceHealthState::Quarantined => 4,
        quality::SourceHealthState::Recovering => 5,
    }
}

const fn derivative_stream_tag(stream: DerivativeStream) -> u8 {
    match stream {
        DerivativeStream::Funding => 1,
        DerivativeStream::OpenInterest => 2,
        DerivativeStream::MarkIndex => 3,
        DerivativeStream::Liquidation => 4,
    }
}

fn receipt_quality_score(
    receipts: &[&DerivativeNormalizationReceipt],
) -> Result<QualityScore, Task6EmissionError> {
    let minimum = receipts
        .iter()
        .try_fold(u32::MAX, |minimum, receipt| {
            let metadata = receipt.event().metadata().as_unchecked();
            let sampled_liquidation = receipt.stream() == DerivativeStream::Liquidation
                && matches!(
                    receipt.completeness(),
                    Completeness::SampledLargestPerSymbolWindow { .. }
                );
            let permitted_flags = if sampled_liquidation {
                event_envelope::QualityFlags::PARTIAL.bits()
            } else {
                event_envelope::QualityFlags::NONE.bits()
            };
            if metadata.quality_flags.bits() != permitted_flags {
                return Err(FeatureComputationError::UntrustedInput);
            }
            Ok(minimum.min(metadata.quality_score_ppm))
        })
        .map_err(Task6EmissionError::Computation)?;
    if minimum == u32::MAX {
        return Err(FeatureComputationError::InsufficientHistory.into());
    }
    Ok(QualityScore::from_millionths(minimum)?)
}

fn hash_completeness(hasher: &mut Hasher, completeness: Completeness) {
    match completeness {
        Completeness::NotSupported => {
            hasher.update(&[0]);
        }
        Completeness::VenueReportedComplete {
            delivery_uncertainty,
        } => {
            hasher.update(&[1, u8::from(delivery_uncertainty)]);
        }
        Completeness::VenueReportedAll {
            push_cadence_ms,
            delivery_uncertainty,
        } => {
            hasher.update(&[2, u8::from(delivery_uncertainty)]);
            hasher.update(&push_cadence_ms.get().to_be_bytes());
        }
        Completeness::SampledLargestPerSymbolWindow { window_ms } => {
            hasher.update(&[3]);
            hasher.update(&window_ms.get().to_be_bytes());
        }
        Completeness::Partial { reason } => {
            hasher.update(&[
                4,
                match reason {
                    CompletenessReason::VenueSampling => 1,
                    CompletenessReason::SubscriptionScope => 2,
                    CompletenessReason::LicensingRestriction => 3,
                    CompletenessReason::HistoricalGap => 4,
                },
            ]);
        }
    }
}

const fn completeness_class(completeness: Completeness) -> u8 {
    match completeness {
        Completeness::NotSupported => 0,
        Completeness::VenueReportedComplete { .. } => 1,
        Completeness::VenueReportedAll { .. } => 2,
        Completeness::SampledLargestPerSymbolWindow { .. } => 3,
        Completeness::Partial { .. } => 4,
    }
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
            hasher.update(&[6, missingness_tag(*reason)]);
        }
    }
}

fn missing_lineage_hash(
    definition: &feature_registry::FeatureDefinition,
    input: &Task6MissingEmissionInput,
    entity: &FeatureEntity,
    decision: FinalizationDecision,
    reason: MissingnessReason,
) -> Result<LineageHash, RegistryError> {
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/task6-unavailable-feature-lineage/v1");
    hash_bytes(&mut hasher, definition.id().as_str().as_bytes());
    hash_bytes(&mut hasher, definition.version().to_string().as_bytes());
    hasher.update(&definition.formula_hash().bytes());
    hasher.update(&decision.evidence_digest());
    hash_entity(&mut hasher, entity);
    hasher.update(&[missingness_tag(reason)]);
    hasher.update(&input.computed_at.value().to_be_bytes());
    hasher.update(&input.revision.value().to_be_bytes());
    hash_bytes(&mut hasher, input.code_commit.as_str().as_bytes());
    LineageHash::new(*hasher.finalize().as_bytes())
}

const fn missingness_tag(reason: MissingnessReason) -> u8 {
    match reason {
        MissingnessReason::NotListed => 1,
        MissingnessReason::SourceNotSupported => 2,
        MissingnessReason::SourceDisconnected => 3,
        MissingnessReason::SequenceGap => 4,
        MissingnessReason::Stale => 5,
        MissingnessReason::InsufficientHistory => 6,
        MissingnessReason::WindowNotFinal => 7,
        MissingnessReason::BelowLiquidityThreshold => 8,
        MissingnessReason::VendorRevisionPending => 9,
        MissingnessReason::ModelNotApplicable => 10,
        MissingnessReason::PrivacyOrLicenseRestriction => 11,
        MissingnessReason::Unknown => 12,
    }
}

fn hash_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}
