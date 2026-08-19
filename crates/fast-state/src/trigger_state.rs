//! Registry-validated, missing-aware fast trigger state.

use std::{cmp::Ordering, collections::BTreeSet, sync::LazyLock};

use domain::{AssetId, InstrumentId, SourceId, UnixNanos};
use feature_engine::features::{
    Task6ComputationAvailability, task_five_definitions, task_four_definitions,
    task_six_definitions, task_six_recipe,
};
use feature_registry::{
    EntityScope, FeatureConsumptionRole, FeatureDatum, FeatureDefinition, FeatureEntity,
    FeatureObservation, FeatureRegistry, FeatureValue, FeatureValueType, MissingnessReason,
    QualityScore, RegistryError,
};
use instrument_registry::CatalogSnapshot;
use quality::SourceHealthState;
use thiserror::Error;

use crate::features::{
    cancellation_burst, completeness_weighted_liquidation_pressure, depth_disappearance,
    dispersion_acceleration, open_interest_destruction, require_nonnegative,
    validate_mark_index_divergence, validate_sweep_direction,
};

const MAX_TRIGGER_OBSERVATIONS: usize = 64;
const MAX_TRIGGER_SOURCES: usize = 64;
const HEALTHY_QUALITY_MILLIONTHS: u32 = 900_000;

/// Fail-closed trigger-state construction errors.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TriggerError {
    #[error("trigger-state target must contain 1..=64 unique sources")]
    InvalidTargetSources,
    #[error("trigger-state target is not bound to a verified point-in-time instrument catalogue")]
    InvalidInstrumentTarget,
    #[error("trigger-state evaluation time is invalid")]
    InvalidEvaluationTime,
    #[error("trigger-state observation count must be within 1..=64")]
    InvalidObservationCount,
    #[error("trigger-state observation is not part of the frozen input contract")]
    UnsupportedFeature,
    #[error("trigger-state feature {feature} does not match the frozen catalogue")]
    FeatureContractMismatch { feature: &'static str },
    #[error("trigger-state input contains the same observation more than once")]
    DuplicateObservation,
    #[error("trigger-state input contains multiple values for one feature slot")]
    DuplicateFeatureSlot,
    #[error("trigger-state observation does not belong to the requested target")]
    ObservationTargetMismatch,
    #[error("cascade gate source universe does not match the trigger target")]
    SourceUniverseMismatch,
    #[error("trigger-state observation is not point-in-time available")]
    ObservationNotAvailable,
    #[error("trigger-state feature {feature} has an invalid value")]
    InvalidFeatureValue { feature: &'static str },
    #[error("trigger-state feature {feature} has invalid history ordering")]
    InvalidFeatureHistory { feature: &'static str },
    #[error("cascade eligibility contradicts per-source completeness evidence")]
    InconsistentCascadeEligibility,
    #[error("trigger-state observation failed registry validation: {0}")]
    Registry(#[from] RegistryError),
}

/// Exact instrument/asset/source universe for one trigger evaluation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerStateTarget {
    instrument: InstrumentId,
    asset: AssetId,
    sources: Vec<SourceId>,
    effective_at_ns: i64,
    catalog_as_known_at_ns: i64,
    instrument_definition_hash: [u8; 32],
    catalog_digest: [u8; 32],
}

impl TriggerStateTarget {
    pub fn try_from_catalog(
        catalog: &CatalogSnapshot,
        instrument: &InstrumentId,
        effective_at_ns: i64,
        mut sources: Vec<SourceId>,
    ) -> Result<Self, TriggerError> {
        if sources.is_empty() || sources.len() > MAX_TRIGGER_SOURCES {
            return Err(TriggerError::InvalidTargetSources);
        }
        if effective_at_ns <= 0 || !catalog.verify_integrity() {
            return Err(TriggerError::InvalidInstrumentTarget);
        }
        sources.sort_by(compare_sources);
        if sources.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(TriggerError::InvalidTargetSources);
        }
        let resolved = catalog
            .resolve_id(instrument, UnixNanos::new(effective_at_ns))
            .map_err(|_| TriggerError::InvalidInstrumentTarget)?;
        Ok(Self {
            instrument: resolved.definition().id().clone(),
            asset: resolved.definition().base_asset().clone(),
            sources,
            effective_at_ns,
            catalog_as_known_at_ns: resolved.as_known_at().value(),
            instrument_definition_hash: *resolved.definition_hash(),
            catalog_digest: *resolved.catalog_digest(),
        })
    }

    pub const fn instrument(&self) -> &InstrumentId {
        &self.instrument
    }

    pub const fn asset(&self) -> &AssetId {
        &self.asset
    }

    pub fn sources(&self) -> &[SourceId] {
        &self.sources
    }

    pub const fn effective_at_ns(&self) -> i64 {
        self.effective_at_ns
    }

    pub const fn instrument_definition_hash(&self) -> [u8; 32] {
        self.instrument_definition_hash
    }

    pub const fn catalog_digest(&self) -> [u8; 32] {
        self.catalog_digest
    }
}

/// Closed reasons why trigger evidence is degraded or a value is absent.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TriggerMissingness {
    StaleBook,
    SourceLoss,
    SourceQualityDegraded,
    ExpiredInput,
    InsufficientHistory,
    IncompleteLiquidationCoverage,
    CascadeIneligible,
    DepthDisappearance,
    CancellationBurst,
    SweepDirection,
    OrderFlowImbalance,
    CrossVenueDispersionAcceleration,
    LiquidationVelocity,
    LiquidationPressure,
    OpenInterestDestruction,
    MarkIndexDivergence,
    FeatureAge,
}

/// Evidence-derived trigger health; callers cannot supply this label.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TriggerHealth {
    Healthy,
    Degraded,
    Unavailable,
}

/// Immutable fast trigger record, separate from structural regime state.
#[derive(Clone, Debug, PartialEq)]
pub struct TriggerState {
    depth_disappearance: Option<f64>,
    cancellation_burst: Option<f64>,
    sweep_direction: Option<f64>,
    order_flow_imbalance: Option<f64>,
    cross_venue_dispersion_acceleration: Option<f64>,
    liquidation_velocity: Option<f64>,
    liquidation_pressure: Option<f64>,
    liquidation_completeness_millionths: u32,
    open_interest_destruction: Option<f64>,
    mark_index_divergence: Option<f64>,
    maximum_feature_age_ns: Option<i64>,
    missingness: BTreeSet<TriggerMissingness>,
    minimum_quality_score: QualityScore,
    minimum_source_coverage_millionths: u32,
    health: TriggerHealth,
}

impl TriggerState {
    pub const fn depth_disappearance(&self) -> Option<f64> {
        self.depth_disappearance
    }

    pub const fn cancellation_burst(&self) -> Option<f64> {
        self.cancellation_burst
    }

    pub const fn sweep_direction(&self) -> Option<f64> {
        self.sweep_direction
    }

    pub const fn order_flow_imbalance(&self) -> Option<f64> {
        self.order_flow_imbalance
    }

    pub const fn cross_venue_dispersion_acceleration(&self) -> Option<f64> {
        self.cross_venue_dispersion_acceleration
    }

    pub const fn liquidation_velocity(&self) -> Option<f64> {
        self.liquidation_velocity
    }

    pub const fn liquidation_pressure(&self) -> Option<f64> {
        self.liquidation_pressure
    }

    pub const fn liquidation_completeness_millionths(&self) -> u32 {
        self.liquidation_completeness_millionths
    }

    pub const fn open_interest_destruction(&self) -> Option<f64> {
        self.open_interest_destruction
    }

    pub const fn mark_index_divergence(&self) -> Option<f64> {
        self.mark_index_divergence
    }

    pub const fn maximum_feature_age_ns(&self) -> Option<i64> {
        self.maximum_feature_age_ns
    }

    pub const fn missingness(&self) -> &BTreeSet<TriggerMissingness> {
        &self.missingness
    }

    pub const fn minimum_quality_score(&self) -> QualityScore {
        self.minimum_quality_score
    }

    pub const fn minimum_source_coverage_millionths(&self) -> u32 {
        self.minimum_source_coverage_millionths
    }

    pub const fn health(&self) -> TriggerHealth {
        self.health
    }
}

/// Trusted in-process handoff from the feature engine into trigger aggregation.
///
/// Registry checks below prove exact contract conformance, roles, and
/// point-in-time shape. They do not authenticate observation origin, so callers
/// must supply feature-engine emissions, not caller-assembled substitutes.
#[derive(Debug)]
pub struct TriggerStateBuilder<'a> {
    registry: &'a FeatureRegistry,
    target: TriggerStateTarget,
    evaluation_event_time_ns: i64,
    evaluation_known_at_ns: i64,
    observations: Vec<FeatureObservation>,
}

impl<'a> TriggerStateBuilder<'a> {
    pub fn try_new(
        registry: &'a FeatureRegistry,
        target: TriggerStateTarget,
        evaluation_event_time_ns: i64,
        evaluation_known_at_ns: i64,
        observations: Vec<FeatureObservation>,
    ) -> Result<Self, TriggerError> {
        if evaluation_event_time_ns <= 0
            || evaluation_known_at_ns < evaluation_event_time_ns
            || target.effective_at_ns != evaluation_event_time_ns
            || target.catalog_as_known_at_ns > evaluation_known_at_ns
        {
            return Err(TriggerError::InvalidEvaluationTime);
        }
        if observations.is_empty() || observations.len() > MAX_TRIGGER_OBSERVATIONS {
            return Err(TriggerError::InvalidObservationCount);
        }
        Ok(Self {
            registry,
            target,
            evaluation_event_time_ns,
            evaluation_known_at_ns,
            observations,
        })
    }

    #[allow(clippy::too_many_lines)]
    pub fn build(self) -> Result<TriggerState, TriggerError> {
        reject_duplicate_observations(&self.observations)?;
        let mut values = TriggerInputs::default();
        let mut seen_slots = BTreeSet::new();
        let mut source_slots = Vec::<(&'static str, SourceId)>::new();
        let mut missingness = BTreeSet::new();
        let mut minimum_quality_score: Option<QualityScore> = None;
        let mut minimum_source_coverage_millionths: Option<u32> = None;
        let mut source_quality_degraded = false;

        for observation in &self.observations {
            let contract = expected_contract(observation.feature_id().as_str())
                .ok_or(TriggerError::UnsupportedFeature)?;
            validate_contract(self.registry, observation, contract)?;
            validate_target(&self.target, observation, contract.scope)?;
            if observation.event_time_end().value() > self.evaluation_event_time_ns
                || observation.as_known_at().value() > self.evaluation_known_at_ns
                || observation.computed_at().value() > self.evaluation_known_at_ns
            {
                return Err(TriggerError::ObservationNotAvailable);
            }

            minimum_quality_score = Some(
                minimum_quality_score.map_or(observation.quality_score(), |current| {
                    current.min(observation.quality_score())
                }),
            );
            let coverage = observation.source_coverage().coverage_millionths();
            minimum_source_coverage_millionths = Some(
                minimum_source_coverage_millionths
                    .map_or(coverage, |current| current.min(coverage)),
            );
            source_quality_degraded |= observation
                .source_coverage()
                .entries()
                .iter()
                .any(|entry| entry.health() != SourceHealthState::Healthy);

            if contract.scope == EntityScope::Source {
                let source = match observation.entity() {
                    FeatureEntity::Source(source) => source.clone(),
                    _ => return Err(TriggerError::ObservationTargetMismatch),
                };
                if source_slots
                    .iter()
                    .any(|(id, prior_source)| *id == contract.id && prior_source == &source)
                {
                    return Err(TriggerError::DuplicateFeatureSlot);
                }
                source_slots.push((contract.id, source));
            } else if contract.id != "cross_venue_median_absolute_dispersion"
                && !seen_slots.insert(contract.id)
            {
                return Err(TriggerError::DuplicateFeatureSlot);
            }

            let definition = self
                .registry
                .get(observation.feature_id(), observation.feature_version())
                .ok_or(RegistryError::UnknownDefinition)?;
            let age_ns = self
                .evaluation_event_time_ns
                .checked_sub(observation.event_time_end().value())
                .ok_or(TriggerError::ObservationNotAvailable)?;
            let expired =
                u64::try_from(age_ns).map_or(true, |age| age > definition.time_to_live().value());
            if expired {
                missingness.insert(TriggerMissingness::ExpiredInput);
                mark_feature_missing(contract.id, &mut missingness);
                continue;
            }

            if let FeatureDatum::Missing(reason) = observation.datum() {
                mark_reason(*reason, contract.id, &mut missingness);
                continue;
            }
            collect_present(contract.id, observation, &mut values, &mut missingness)?;
        }

        let minimum_quality_score =
            minimum_quality_score.ok_or(TriggerError::InvalidObservationCount)?;
        let minimum_source_coverage_millionths =
            minimum_source_coverage_millionths.ok_or(TriggerError::InvalidObservationCount)?;
        if source_quality_degraded {
            missingness.insert(TriggerMissingness::SourceQualityDegraded);
        }

        if values.stale_quote_detected {
            missingness.insert(TriggerMissingness::StaleBook);
        }
        if values.source_outage_detected {
            missingness.insert(TriggerMissingness::SourceLoss);
        }
        let stale_book =
            values.stale_quote_detected || missingness.contains(&TriggerMissingness::StaleBook);
        let source_loss = values.source_outage_detected
            || missingness.contains(&TriggerMissingness::SourceLoss)
            || source_quality_degraded;
        if values.cascade_eligible == Some(true) && (stale_book || source_loss) {
            return Err(TriggerError::InconsistentCascadeEligibility);
        }

        let mut depth = if stale_book {
            None
        } else {
            values
                .bid_depth_change
                .zip(values.ask_depth_change)
                .map(|(bid, ask)| depth_disappearance(bid, ask))
                .transpose()?
        };
        let mut cancellation = values
            .cancellation_rate
            .zip(values.cancellation_ratio)
            .map(|(rate, ratio)| cancellation_burst(rate, ratio))
            .transpose()?;
        let mut sweep = values.sweep_direction;
        let mut ofi = values.order_flow_imbalance;
        values.dispersions.sort_unstable_by_key(|(time, _)| *time);
        let mut dispersion = match values.dispersions.as_slice() {
            [(prior_time, prior), (current_time, current)] => Some(dispersion_acceleration(
                *prior,
                *prior_time,
                *current,
                *current_time,
            )?),
            _ => {
                missingness.insert(TriggerMissingness::InsufficientHistory);
                None
            }
        };
        let mut liquidation_velocity = values.liquidation_velocity;
        let mut oi_destruction = match (
            values.open_interest_relative_change,
            values.aligned_price_return,
        ) {
            (Some((oi_time, oi_change)), Some((price_time, price_return))) => {
                if oi_time != price_time {
                    return Err(TriggerError::InvalidFeatureHistory {
                        feature: "open_interest_destruction",
                    });
                }
                Some(open_interest_destruction(price_return, oi_change)?)
            }
            _ => None,
        };
        let mut mark_index = values.mark_index_divergence;

        let complete_source_count = self
            .target
            .sources
            .iter()
            .filter(|source| values.complete_liquidation_sources.contains(*source))
            .count();
        let completeness_millionths = u32::try_from(
            complete_source_count * QualityScore::MAX_MILLIONTHS as usize
                / self.target.sources.len(),
        )
        .map_err(|_| TriggerError::InvalidTargetSources)?;
        if values.cascade_eligible == Some(true)
            && completeness_millionths != QualityScore::MAX_MILLIONTHS
        {
            return Err(TriggerError::InconsistentCascadeEligibility);
        }
        let confirmed_completeness = match values.cascade_eligible {
            Some(true) => {
                Some(f64::from(completeness_millionths) / f64::from(QualityScore::MAX_MILLIONTHS))
            }
            Some(false) => {
                missingness.insert(TriggerMissingness::CascadeIneligible);
                Some(0.0)
            }
            None => {
                missingness.insert(TriggerMissingness::CascadeIneligible);
                None
            }
        };
        let mut liquidation_pressure = confirmed_completeness
            .zip(liquidation_velocity.zip(oi_destruction))
            .map(|(completeness, (velocity, destruction))| {
                completeness_weighted_liquidation_pressure(velocity, completeness, destruction)
            })
            .transpose()?;

        if source_loss {
            depth = None;
            cancellation = None;
            sweep = None;
            ofi = None;
            dispersion = None;
            liquidation_velocity = None;
            liquidation_pressure = None;
            oi_destruction = None;
            mark_index = None;
        }

        mark_absent_outputs(
            depth,
            cancellation,
            sweep,
            ofi,
            dispersion,
            liquidation_velocity,
            liquidation_pressure,
            oi_destruction,
            mark_index,
            values.maximum_feature_age_ns,
            &mut missingness,
        );
        let health = derive_health(
            source_loss,
            &missingness,
            minimum_quality_score,
            minimum_source_coverage_millionths,
        );

        Ok(TriggerState {
            depth_disappearance: depth,
            cancellation_burst: cancellation,
            sweep_direction: sweep,
            order_flow_imbalance: ofi,
            cross_venue_dispersion_acceleration: dispersion,
            liquidation_velocity,
            liquidation_pressure,
            liquidation_completeness_millionths: completeness_millionths,
            open_interest_destruction: oi_destruction,
            mark_index_divergence: mark_index,
            maximum_feature_age_ns: values.maximum_feature_age_ns,
            missingness,
            minimum_quality_score,
            minimum_source_coverage_millionths,
            health,
        })
    }
}

#[derive(Default)]
struct TriggerInputs {
    bid_depth_change: Option<f64>,
    ask_depth_change: Option<f64>,
    cancellation_rate: Option<f64>,
    cancellation_ratio: Option<f64>,
    sweep_direction: Option<f64>,
    order_flow_imbalance: Option<f64>,
    dispersions: Vec<(i64, f64)>,
    liquidation_velocity: Option<f64>,
    open_interest_relative_change: Option<(i64, f64)>,
    aligned_price_return: Option<(i64, f64)>,
    mark_index_divergence: Option<f64>,
    complete_liquidation_sources: Vec<SourceId>,
    cascade_eligible: Option<bool>,
    maximum_feature_age_ns: Option<i64>,
    stale_quote_detected: bool,
    source_outage_detected: bool,
}

#[derive(Clone, Copy)]
struct ExpectedContract {
    id: &'static str,
    role: FeatureConsumptionRole,
    value_type: FeatureValueType,
    scope: EntityScope,
}

fn expected_contract(id: &str) -> Option<ExpectedContract> {
    use EntityScope::{Asset, Instrument, Source};
    use FeatureConsumptionRole::{GatingOnly, ModelEligible, UncertaintyOnly};
    use FeatureValueType::{Boolean, FixedDecimal, Float64, Integer};
    let contract = match id {
        "bid_depth_change" => (ModelEligible, FixedDecimal, Instrument),
        "ask_depth_change" => (ModelEligible, FixedDecimal, Instrument),
        "cancellation_rate_per_second" => (ModelEligible, Float64, Instrument),
        "cancellation_to_trade_ratio" => (ModelEligible, Float64, Instrument),
        "trade_print_sweep_direction" => (ModelEligible, Float64, Instrument),
        "top_of_book_ofi" => (ModelEligible, FixedDecimal, Instrument),
        "cross_venue_median_absolute_dispersion" => (ModelEligible, Float64, Asset),
        "liquidation_observed_velocity" => (ModelEligible, Float64, Instrument),
        "open_interest_relative_change" => (ModelEligible, Float64, Instrument),
        "log_return" => (ModelEligible, Float64, Asset),
        "mark_index_divergence" => (ModelEligible, Float64, Instrument),
        "liquidation_source_completeness_flag" => (UncertaintyOnly, Integer, Source),
        "feature_age_ns" => (UncertaintyOnly, Integer, Source),
        "stale_quote_duration_ns" => (UncertaintyOnly, Integer, Source),
        "source_outage_indicator" => (UncertaintyOnly, Boolean, Source),
        "cascade_eligibility_gate" => (GatingOnly, Boolean, Asset),
        _ => return None,
    };
    Some(ExpectedContract {
        id: match id {
            "bid_depth_change" => "bid_depth_change",
            "ask_depth_change" => "ask_depth_change",
            "cancellation_rate_per_second" => "cancellation_rate_per_second",
            "cancellation_to_trade_ratio" => "cancellation_to_trade_ratio",
            "trade_print_sweep_direction" => "trade_print_sweep_direction",
            "top_of_book_ofi" => "top_of_book_ofi",
            "cross_venue_median_absolute_dispersion" => "cross_venue_median_absolute_dispersion",
            "liquidation_observed_velocity" => "liquidation_observed_velocity",
            "open_interest_relative_change" => "open_interest_relative_change",
            "log_return" => "log_return",
            "mark_index_divergence" => "mark_index_divergence",
            "liquidation_source_completeness_flag" => "liquidation_source_completeness_flag",
            "feature_age_ns" => "feature_age_ns",
            "stale_quote_duration_ns" => "stale_quote_duration_ns",
            "source_outage_indicator" => "source_outage_indicator",
            "cascade_eligibility_gate" => "cascade_eligibility_gate",
            _ => unreachable!("matched above"),
        },
        role: contract.0,
        value_type: contract.1,
        scope: contract.2,
    })
}

fn validate_contract(
    registry: &FeatureRegistry,
    observation: &FeatureObservation,
    expected: ExpectedContract,
) -> Result<(), TriggerError> {
    let version = observation.feature_version();
    if version.major != 1
        || version.minor != 0
        || version.patch != 0
        || !version.pre.is_empty()
        || !version.build.is_empty()
    {
        return Err(TriggerError::FeatureContractMismatch {
            feature: expected.id,
        });
    }
    let definition = registry
        .get(observation.feature_id(), version)
        .ok_or(RegistryError::UnknownDefinition)?;
    let canonical = canonical_definition(expected.id)?;
    if definition.consumption_role() != expected.role
        || definition.value_type() != expected.value_type
        || definition.entities() != expected.scope
        || definition != canonical
    {
        return Err(TriggerError::FeatureContractMismatch {
            feature: expected.id,
        });
    }
    if expected.id == "log_return" && observation.window_id().as_str() != "rolling_5m" {
        return Err(TriggerError::FeatureContractMismatch {
            feature: expected.id,
        });
    }
    registry.validate_observation(observation)?;
    Ok(())
}

fn validate_target(
    target: &TriggerStateTarget,
    observation: &FeatureObservation,
    scope: EntityScope,
) -> Result<(), TriggerError> {
    let matches = match (scope, observation.entity()) {
        (EntityScope::Instrument, FeatureEntity::Instrument(instrument)) => {
            instrument == &target.instrument
        }
        (EntityScope::Asset, FeatureEntity::Asset(asset)) => asset == &target.asset,
        (EntityScope::Source, FeatureEntity::Source(source)) => target.sources.contains(source),
        _ => false,
    };
    if !matches {
        return Err(TriggerError::ObservationTargetMismatch);
    }
    if matches!(
        observation.feature_id().as_str(),
        "cascade_eligibility_gate" | "cross_venue_median_absolute_dispersion" | "log_return"
    ) && !same_source_universe(
        &target.sources,
        observation.source_coverage().expected_sources(),
    ) {
        return Err(TriggerError::SourceUniverseMismatch);
    }
    Ok(())
}

fn collect_present(
    id: &'static str,
    observation: &FeatureObservation,
    values: &mut TriggerInputs,
    missingness: &mut BTreeSet<TriggerMissingness>,
) -> Result<(), TriggerError> {
    match id {
        "bid_depth_change" => {
            values.bid_depth_change = Some(decimal_value(observation, id)?);
        }
        "ask_depth_change" => {
            values.ask_depth_change = Some(decimal_value(observation, id)?);
        }
        "cancellation_rate_per_second" => {
            values.cancellation_rate =
                Some(require_nonnegative(id, float_value(observation, id)?)?);
        }
        "cancellation_to_trade_ratio" => {
            values.cancellation_ratio =
                Some(require_nonnegative(id, float_value(observation, id)?)?);
        }
        "trade_print_sweep_direction" => {
            values.sweep_direction = Some(validate_sweep_direction(float_value(observation, id)?)?);
        }
        "top_of_book_ofi" => {
            values.order_flow_imbalance = Some(decimal_value(observation, id)?);
        }
        "cross_venue_median_absolute_dispersion" => {
            if values.dispersions.len() == 2 {
                return Err(TriggerError::DuplicateFeatureSlot);
            }
            values.dispersions.push((
                observation.event_time_end().value(),
                require_nonnegative(id, float_value(observation, id)?)?,
            ));
        }
        "liquidation_observed_velocity" => {
            values.liquidation_velocity =
                Some(require_nonnegative(id, float_value(observation, id)?)?);
        }
        "open_interest_relative_change" => {
            let value = float_value(observation, id)?;
            if value < -1.0 {
                return Err(TriggerError::InvalidFeatureValue { feature: id });
            }
            values.open_interest_relative_change =
                Some((observation.event_time_end().value(), value));
        }
        "log_return" => {
            values.aligned_price_return = Some((
                observation.event_time_end().value(),
                float_value(observation, id)?,
            ));
        }
        "mark_index_divergence" => {
            values.mark_index_divergence = Some(validate_mark_index_divergence(float_value(
                observation,
                id,
            )?)?);
        }
        "liquidation_source_completeness_flag" => {
            let class = integer_value(observation, id)?;
            if !(0..=4).contains(&class) {
                return Err(TriggerError::InvalidFeatureValue { feature: id });
            }
            if matches!(class, 1 | 2) {
                let FeatureEntity::Source(source) = observation.entity() else {
                    return Err(TriggerError::ObservationTargetMismatch);
                };
                values.complete_liquidation_sources.push(source.clone());
            } else {
                missingness.insert(TriggerMissingness::IncompleteLiquidationCoverage);
            }
        }
        "feature_age_ns" => {
            let age = integer_value(observation, id)?;
            if age < 0 {
                return Err(TriggerError::InvalidFeatureValue { feature: id });
            }
            values.maximum_feature_age_ns = Some(
                values
                    .maximum_feature_age_ns
                    .map_or(age, |current| current.max(age)),
            );
        }
        "stale_quote_duration_ns" => {
            let duration = integer_value(observation, id)?;
            if duration < 0 {
                return Err(TriggerError::InvalidFeatureValue { feature: id });
            }
            values.stale_quote_detected |= duration > 0;
        }
        "source_outage_indicator" => {
            values.source_outage_detected |= boolean_value(observation, id)?;
        }
        "cascade_eligibility_gate" => {
            values.cascade_eligible = Some(boolean_value(observation, id)?);
        }
        _ => return Err(TriggerError::UnsupportedFeature),
    }
    Ok(())
}

fn float_value(
    observation: &FeatureObservation,
    feature: &'static str,
) -> Result<f64, TriggerError> {
    match observation.datum() {
        FeatureDatum::Present(FeatureValue::Float64(value)) => Ok(value.value()),
        _ => Err(TriggerError::InvalidFeatureValue { feature }),
    }
}

fn decimal_value(
    observation: &FeatureObservation,
    feature: &'static str,
) -> Result<f64, TriggerError> {
    match observation.datum() {
        FeatureDatum::Present(FeatureValue::FixedDecimal(value)) => {
            let converted = value.to_f64_lossy_for_analysis();
            if converted.is_finite() {
                Ok(converted)
            } else {
                Err(TriggerError::InvalidFeatureValue { feature })
            }
        }
        _ => Err(TriggerError::InvalidFeatureValue { feature }),
    }
}

fn integer_value(
    observation: &FeatureObservation,
    feature: &'static str,
) -> Result<i64, TriggerError> {
    match observation.datum() {
        FeatureDatum::Present(FeatureValue::Integer(value)) => Ok(*value),
        _ => Err(TriggerError::InvalidFeatureValue { feature }),
    }
}

fn boolean_value(
    observation: &FeatureObservation,
    feature: &'static str,
) -> Result<bool, TriggerError> {
    match observation.datum() {
        FeatureDatum::Present(FeatureValue::Boolean(value)) => Ok(*value),
        _ => Err(TriggerError::InvalidFeatureValue { feature }),
    }
}

fn mark_reason(
    reason: MissingnessReason,
    feature: &'static str,
    missingness: &mut BTreeSet<TriggerMissingness>,
) {
    match reason {
        MissingnessReason::Stale if feature == "stale_quote_duration_ns" => {
            missingness.insert(TriggerMissingness::StaleBook);
        }
        MissingnessReason::SourceDisconnected | MissingnessReason::SequenceGap
            if is_source_evidence_feature(feature) =>
        {
            missingness.insert(TriggerMissingness::SourceLoss);
        }
        MissingnessReason::Stale
        | MissingnessReason::SourceDisconnected
        | MissingnessReason::SequenceGap => {}
        MissingnessReason::InsufficientHistory | MissingnessReason::WindowNotFinal => {
            missingness.insert(TriggerMissingness::InsufficientHistory);
        }
        MissingnessReason::NotListed
        | MissingnessReason::SourceNotSupported
        | MissingnessReason::BelowLiquidityThreshold
        | MissingnessReason::VendorRevisionPending
        | MissingnessReason::ModelNotApplicable
        | MissingnessReason::PrivacyOrLicenseRestriction
        | MissingnessReason::Unknown => {}
    }
    mark_feature_missing(feature, missingness);
}

fn is_source_evidence_feature(feature: &str) -> bool {
    matches!(
        feature,
        "liquidation_source_completeness_flag"
            | "feature_age_ns"
            | "stale_quote_duration_ns"
            | "source_outage_indicator"
    )
}

fn mark_feature_missing(feature: &str, missingness: &mut BTreeSet<TriggerMissingness>) {
    let reason = match feature {
        "bid_depth_change" | "ask_depth_change" => TriggerMissingness::DepthDisappearance,
        "cancellation_rate_per_second" | "cancellation_to_trade_ratio" => {
            TriggerMissingness::CancellationBurst
        }
        "trade_print_sweep_direction" => TriggerMissingness::SweepDirection,
        "top_of_book_ofi" => TriggerMissingness::OrderFlowImbalance,
        "cross_venue_median_absolute_dispersion" => {
            TriggerMissingness::CrossVenueDispersionAcceleration
        }
        "liquidation_observed_velocity" => TriggerMissingness::LiquidationVelocity,
        "open_interest_relative_change" | "log_return" => {
            TriggerMissingness::OpenInterestDestruction
        }
        "mark_index_divergence" => TriggerMissingness::MarkIndexDivergence,
        "feature_age_ns" => TriggerMissingness::FeatureAge,
        "liquidation_source_completeness_flag" | "cascade_eligibility_gate" => {
            TriggerMissingness::IncompleteLiquidationCoverage
        }
        "stale_quote_duration_ns" | "source_outage_indicator" => return,
        _ => return,
    };
    missingness.insert(reason);
}

#[allow(clippy::too_many_arguments)]
fn mark_absent_outputs(
    depth: Option<f64>,
    cancellation: Option<f64>,
    sweep: Option<f64>,
    ofi: Option<f64>,
    dispersion: Option<f64>,
    liquidation_velocity: Option<f64>,
    liquidation_pressure: Option<f64>,
    oi_destruction: Option<f64>,
    mark_index: Option<f64>,
    feature_age: Option<i64>,
    missingness: &mut BTreeSet<TriggerMissingness>,
) {
    for (absent, reason) in [
        (depth.is_none(), TriggerMissingness::DepthDisappearance),
        (
            cancellation.is_none(),
            TriggerMissingness::CancellationBurst,
        ),
        (sweep.is_none(), TriggerMissingness::SweepDirection),
        (ofi.is_none(), TriggerMissingness::OrderFlowImbalance),
        (
            dispersion.is_none(),
            TriggerMissingness::CrossVenueDispersionAcceleration,
        ),
        (
            liquidation_velocity.is_none(),
            TriggerMissingness::LiquidationVelocity,
        ),
        (
            liquidation_pressure.is_none(),
            TriggerMissingness::LiquidationPressure,
        ),
        (
            oi_destruction.is_none(),
            TriggerMissingness::OpenInterestDestruction,
        ),
        (
            mark_index.is_none(),
            TriggerMissingness::MarkIndexDivergence,
        ),
        (feature_age.is_none(), TriggerMissingness::FeatureAge),
    ] {
        if absent {
            missingness.insert(reason);
        }
    }
}

fn derive_health(
    source_loss: bool,
    missingness: &BTreeSet<TriggerMissingness>,
    minimum_quality_score: QualityScore,
    minimum_source_coverage_millionths: u32,
) -> TriggerHealth {
    if source_loss {
        TriggerHealth::Unavailable
    } else if !missingness.is_empty()
        || minimum_quality_score.millionths() < HEALTHY_QUALITY_MILLIONTHS
        || minimum_source_coverage_millionths < QualityScore::MAX_MILLIONTHS
    {
        TriggerHealth::Degraded
    } else {
        TriggerHealth::Healthy
    }
}

fn reject_duplicate_observations(observations: &[FeatureObservation]) -> Result<(), TriggerError> {
    for (index, observation) in observations.iter().enumerate() {
        if observations[index + 1..]
            .iter()
            .any(|candidate| same_observation_slot(observation, candidate))
        {
            return Err(TriggerError::DuplicateObservation);
        }
    }
    Ok(())
}

fn same_observation_slot(left: &FeatureObservation, right: &FeatureObservation) -> bool {
    left.feature_id() == right.feature_id()
        && left.feature_version() == right.feature_version()
        && left.entity() == right.entity()
        && left.window_id() == right.window_id()
        && left.event_time_start() == right.event_time_start()
        && left.event_time_end() == right.event_time_end()
}

fn compare_sources(left: &SourceId, right: &SourceId) -> Ordering {
    (left.kind() as u8, left.name(), left.generation()).cmp(&(
        right.kind() as u8,
        right.name(),
        right.generation(),
    ))
}

fn same_source_universe(left: &[SourceId], right: &[SourceId]) -> bool {
    left.len() == right.len() && left.iter().all(|source| right.contains(source))
}

static CANONICAL_DEFINITIONS: LazyLock<Result<Vec<FeatureDefinition>, RegistryError>> =
    LazyLock::new(|| {
        let mut definitions = task_four_definitions()?;
        definitions.extend(task_five_definitions()?);
        definitions.extend(task_six_definitions()?);
        Ok(definitions)
    });

fn canonical_definition(id: &'static str) -> Result<&'static FeatureDefinition, TriggerError> {
    if let Some(recipe) = task_six_recipe(id)
        && recipe.computation_availability() != Task6ComputationAvailability::Implemented
    {
        return Err(TriggerError::FeatureContractMismatch { feature: id });
    }
    CANONICAL_DEFINITIONS
        .as_ref()
        .map_err(|_| TriggerError::FeatureContractMismatch { feature: id })?
        .iter()
        .find(|definition| definition.id().as_str() == id)
        .ok_or(TriggerError::FeatureContractMismatch { feature: id })
}
