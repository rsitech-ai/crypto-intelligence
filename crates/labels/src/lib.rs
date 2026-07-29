//! Deterministic, versioned point-in-time market-transition labels.

pub mod definitions;
pub mod first_passage;

use semver::Version;
use thiserror::Error;

pub use definitions::{
    DefinitionScope, EventType, JumpTreatment, LabelDefinition, LabelDefinitionInput, LabelRule,
    OverlapPolicy, PriceThreshold, VolatilityEstimator,
};
pub use first_passage::{LabelPath, MarketEligibility, MarketFrame, MarketFrameInput};

use first_passage::{FirstPassageEvaluation, first_persistent_passage};

/// Complete outcome state: missing observation is never silently negative.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelOutcome {
    Occurred { offset_seconds: u64 },
    NotOccurred,
    Censored { observed_seconds: u64 },
    Excluded(ExclusionReason),
}

/// Stable reason why an outcome cannot enter model data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExclusionReason {
    UnhealthyOrigin,
    LowQualityOrigin,
    MissingEvidence,
    InvalidVolatilityScale,
    InsufficientCorroboration,
    InsufficientConfidence,
    HaltedOrDelisted,
    InstrumentDefinitionChanged,
    UnresolvedCorrection,
    TimestampIntegrityCompromised,
    SimultaneousCompetingEvents,
}

/// Auditable evaluation record for one definition and horizon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LabelEvaluation {
    pub outcome: LabelOutcome,
    pub horizon_seconds: u64,
    pub outcome_known_at_offset_seconds: u64,
    pub confidence_millionths: u32,
    pub source_coverage_millionths: u32,
    pub definition_hash: [u8; 32],
}

/// Explicit resolution of competing event families.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedLabels {
    None,
    Multiple(Vec<(EventType, u64)>),
    Primary {
        event_type: EventType,
        offset_seconds: u64,
        simultaneous: Vec<EventType>,
    },
    Excluded(ExclusionReason),
}

/// Validated label evaluator for one semantic definition.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelEngine {
    definition: LabelDefinition,
}

impl LabelEngine {
    pub fn try_new(definition: LabelDefinitionInput) -> Result<Self, LabelError> {
        Ok(Self {
            definition: LabelDefinition::try_new(definition)?,
        })
    }

    pub fn downside_fixture(threshold: f64) -> Result<Self, LabelError> {
        Self::try_new(LabelDefinitionInput {
            id: "downside_fixture".to_owned(),
            version: Version::new(1, 0, 0),
            scope: DefinitionScope::Research,
            rule: LabelRule::Downside {
                threshold: PriceThreshold::AbsoluteFraction(threshold),
            },
            horizons_seconds: vec![180],
            minimum_duration_seconds: 0,
            minimum_quality_millionths: 900_000,
            minimum_source_coverage_millionths: 900_000,
            overlap_policy: OverlapPolicy::EarliestWins,
        })
    }

    pub const fn definition(&self) -> &LabelDefinition {
        &self.definition
    }

    pub fn label(&self, path: &LabelPath) -> Result<LabelOutcome, LabelError> {
        if self.definition.horizons_seconds().len() != 1 {
            return Err(LabelError::AmbiguousHorizon);
        }
        Ok(self
            .evaluate_at_horizon(path, self.definition.horizons_seconds()[0])?
            .outcome)
    }

    pub fn label_at_horizon(
        &self,
        path: &LabelPath,
        horizon_seconds: u64,
    ) -> Result<LabelOutcome, LabelError> {
        Ok(self.evaluate_at_horizon(path, horizon_seconds)?.outcome)
    }

    pub fn evaluate_at_horizon(
        &self,
        path: &LabelPath,
        horizon_seconds: u64,
    ) -> Result<LabelEvaluation, LabelError> {
        if self
            .definition
            .horizons_seconds()
            .binary_search(&horizon_seconds)
            .is_err()
        {
            return Err(LabelError::UnknownHorizon);
        }
        let origin = path.origin();
        let minimum_quality = self.definition.minimum_quality_millionths();
        let minimum_coverage = self.definition.minimum_source_coverage_millionths();
        let eligibility_exclusion = match origin.eligibility() {
            MarketEligibility::Eligible => None,
            MarketEligibility::HaltedOrDelisted => Some(ExclusionReason::HaltedOrDelisted),
            MarketEligibility::InstrumentDefinitionChanged => {
                Some(ExclusionReason::InstrumentDefinitionChanged)
            }
            MarketEligibility::UnresolvedCorrection => Some(ExclusionReason::UnresolvedCorrection),
            MarketEligibility::TimestampIntegrityCompromised => {
                Some(ExclusionReason::TimestampIntegrityCompromised)
            }
        };
        if let Some(reason) = eligibility_exclusion {
            return Ok(self.excluded(horizon_seconds, reason, origin));
        }
        if !origin.is_trustworthy_at(horizon_seconds, 0, 0) {
            return Ok(self.excluded(horizon_seconds, ExclusionReason::UnhealthyOrigin, origin));
        }
        if !origin.is_trustworthy_at(horizon_seconds, minimum_quality, minimum_coverage) {
            return Ok(self.excluded(horizon_seconds, ExclusionReason::LowQualityOrigin, origin));
        }
        let Some(origin_price) = origin.price() else {
            return Ok(self.excluded(horizon_seconds, ExclusionReason::MissingEvidence, origin));
        };
        let duration = self.definition.minimum_duration_seconds();
        let passage = match self.definition.rule() {
            LabelRule::Downside { threshold } => {
                let Some(log_threshold) = price_log_threshold(threshold, origin) else {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::InvalidVolatilityScale,
                        origin,
                    ));
                };
                first_persistent_passage(
                    path,
                    horizon_seconds,
                    duration,
                    minimum_quality,
                    minimum_coverage,
                    |frame| {
                        frame
                            .price()
                            .map(|price| (price / origin_price).ln() <= -log_threshold)
                    },
                )?
            }
            LabelRule::Upside { threshold } => {
                let Some(log_threshold) = price_log_threshold(threshold, origin) else {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::InvalidVolatilityScale,
                        origin,
                    ));
                };
                first_persistent_passage(
                    path,
                    horizon_seconds,
                    duration,
                    minimum_quality,
                    minimum_coverage,
                    |frame| {
                        frame
                            .price()
                            .map(|price| (price / origin_price).ln() >= log_threshold)
                    },
                )?
            }
            LabelRule::VolatilityExplosion {
                reference_threshold,
                minimum_multiple,
                ..
            } => {
                if origin.volatility().is_none() {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::MissingEvidence,
                        origin,
                    ));
                }
                first_persistent_passage(
                    path,
                    horizon_seconds,
                    duration,
                    minimum_quality,
                    minimum_coverage,
                    |frame| {
                        frame
                            .volatility()
                            .map(|value| value >= reference_threshold * minimum_multiple)
                    },
                )?
            }
            LabelRule::LiquidityVacuum {
                minimum_spread_multiple,
                maximum_depth_fraction,
                minimum_cancellation_multiple,
                minimum_sweep_cost_multiple,
                minimum_resiliency_multiple,
                minimum_corroborating_sources,
            } => {
                if maximum_trustworthy_sources(
                    path,
                    horizon_seconds,
                    minimum_quality,
                    minimum_coverage,
                ) < minimum_corroborating_sources
                {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::InsufficientCorroboration,
                        origin,
                    ));
                }
                let (
                    Some(origin_spread),
                    Some(origin_depth),
                    Some(origin_cancellation),
                    Some(origin_sweep_cost),
                    Some(origin_resiliency),
                ) = (
                    origin.spread_bps(),
                    origin.displayed_depth(),
                    origin.cancellation_rate(),
                    origin.sweep_cost_bps(),
                    origin.resiliency_seconds(),
                )
                else {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::MissingEvidence,
                        origin,
                    ));
                };
                if origin_spread <= 0.0
                    || origin_depth <= 0.0
                    || origin_cancellation <= 0.0
                    || origin_sweep_cost <= 0.0
                    || origin_resiliency <= 0.0
                {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::MissingEvidence,
                        origin,
                    ));
                }
                first_persistent_passage(
                    path,
                    horizon_seconds,
                    duration,
                    minimum_quality,
                    minimum_coverage,
                    |frame| {
                        Some(
                            frame.spread_bps()? >= origin_spread * minimum_spread_multiple
                                && frame.displayed_depth()?
                                    <= origin_depth * maximum_depth_fraction
                                && frame.cancellation_rate()?
                                    >= origin_cancellation * minimum_cancellation_multiple
                                && frame.sweep_cost_bps()?
                                    >= origin_sweep_cost * minimum_sweep_cost_multiple
                                && frame.resiliency_seconds()?
                                    >= origin_resiliency * minimum_resiliency_multiple
                                && frame.corroborating_sources() >= minimum_corroborating_sources,
                        )
                    },
                )?
            }
            LabelRule::LiquidationCascade {
                price_threshold,
                minimum_liquidation_notional,
                minimum_open_interest_drop_fraction,
                minimum_spread_multiple,
                maximum_depth_fraction,
                minimum_corroborating_sources,
                minimum_confidence_millionths,
            } => {
                let Some(price_log_threshold) = price_log_threshold(price_threshold, origin) else {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::InvalidVolatilityScale,
                        origin,
                    ));
                };
                if maximum_trustworthy_sources(
                    path,
                    horizon_seconds,
                    minimum_quality,
                    minimum_coverage,
                ) < minimum_corroborating_sources
                {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::InsufficientCorroboration,
                        origin,
                    ));
                }
                if path
                    .frames()
                    .iter()
                    .copied()
                    .take_while(|frame| frame.offset_seconds() <= horizon_seconds)
                    .map(MarketFrame::liquidation_confidence_millionths)
                    .max()
                    .unwrap_or(0)
                    < minimum_confidence_millionths
                {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::InsufficientConfidence,
                        origin,
                    ));
                }
                let (Some(origin_spread), Some(origin_depth), Some(origin_open_interest)) = (
                    origin.spread_bps(),
                    origin.displayed_depth(),
                    origin.open_interest(),
                ) else {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::MissingEvidence,
                        origin,
                    ));
                };
                if origin_spread <= 0.0 || origin_depth <= 0.0 || origin_open_interest <= 0.0 {
                    return Ok(self.excluded(
                        horizon_seconds,
                        ExclusionReason::MissingEvidence,
                        origin,
                    ));
                }
                first_persistent_passage(
                    path,
                    horizon_seconds,
                    duration,
                    minimum_quality,
                    minimum_coverage,
                    |frame| {
                        Some(
                            (frame.price()? / origin_price).ln() <= -price_log_threshold
                                && frame.liquidation_notional()? >= minimum_liquidation_notional
                                && frame.open_interest()?
                                    <= origin_open_interest
                                        * (1.0 - minimum_open_interest_drop_fraction)
                                && frame.spread_bps()? >= origin_spread * minimum_spread_multiple
                                && frame.displayed_depth()?
                                    <= origin_depth * maximum_depth_fraction
                                && frame.corroborating_sources() >= minimum_corroborating_sources
                                && frame.liquidation_confidence_millionths()
                                    >= minimum_confidence_millionths,
                        )
                    },
                )?
            }
        };
        Ok(self.evaluation(path, horizon_seconds, passage))
    }

    fn evaluation(
        &self,
        path: &LabelPath,
        horizon_seconds: u64,
        passage: FirstPassageEvaluation,
    ) -> LabelEvaluation {
        let evidence = match passage.outcome {
            LabelOutcome::Occurred { offset_seconds } => path
                .frames()
                .iter()
                .copied()
                .find(|frame| frame.offset_seconds() == offset_seconds)
                .unwrap_or_else(|| path.origin()),
            LabelOutcome::NotOccurred
            | LabelOutcome::Censored { .. }
            | LabelOutcome::Excluded(_) => path
                .frames()
                .iter()
                .copied()
                .take_while(|frame| frame.offset_seconds() <= horizon_seconds)
                .last()
                .unwrap_or_else(|| path.origin()),
        };
        let confidence_millionths = match self.definition.rule() {
            LabelRule::LiquidationCascade { .. } => evidence.liquidation_confidence_millionths(),
            _ => evidence
                .quality_millionths()
                .min(evidence.source_coverage_millionths()),
        };
        LabelEvaluation {
            outcome: passage.outcome,
            horizon_seconds,
            outcome_known_at_offset_seconds: passage.known_at_offset_seconds,
            confidence_millionths,
            source_coverage_millionths: evidence.source_coverage_millionths(),
            definition_hash: self.definition.definition_hash(),
        }
    }

    fn excluded(
        &self,
        horizon_seconds: u64,
        reason: ExclusionReason,
        origin: MarketFrame,
    ) -> LabelEvaluation {
        LabelEvaluation {
            outcome: LabelOutcome::Excluded(reason),
            horizon_seconds,
            outcome_known_at_offset_seconds: origin.known_at_offset_seconds(),
            confidence_millionths: 0,
            source_coverage_millionths: origin.source_coverage_millionths(),
            definition_hash: self.definition.definition_hash(),
        }
    }
}

pub fn resolve_competing(
    outcomes: &[(EventType, LabelOutcome)],
    policy: OverlapPolicy,
) -> ResolvedLabels {
    let mut occurred = outcomes
        .iter()
        .filter_map(|(event_type, outcome)| match outcome {
            LabelOutcome::Occurred { offset_seconds } => Some((*event_type, *offset_seconds)),
            LabelOutcome::NotOccurred
            | LabelOutcome::Censored { .. }
            | LabelOutcome::Excluded(_) => None,
        })
        .collect::<Vec<_>>();
    occurred.sort_by(|left, right| {
        left.1
            .cmp(&right.1)
            .then_with(|| event_priority(left.0).cmp(&event_priority(right.0)))
    });
    let Some((first_type, first_offset)) = occurred.first().copied() else {
        return ResolvedLabels::None;
    };
    if policy == OverlapPolicy::AllowCompeting {
        return ResolvedLabels::Multiple(occurred);
    }
    let simultaneous = occurred
        .iter()
        .skip(1)
        .take_while(|(_, offset)| *offset == first_offset)
        .map(|(event_type, _)| *event_type)
        .collect::<Vec<_>>();
    if policy == OverlapPolicy::ExcludeTies && !simultaneous.is_empty() {
        ResolvedLabels::Excluded(ExclusionReason::SimultaneousCompetingEvents)
    } else {
        ResolvedLabels::Primary {
            event_type: first_type,
            offset_seconds: first_offset,
            simultaneous,
        }
    }
}

const fn event_priority(event_type: EventType) -> u8 {
    match event_type {
        EventType::LiquidityVacuum => 0,
        EventType::LiquidationCascade => 1,
        EventType::Downside | EventType::Upside => 2,
        EventType::VolatilityExplosion => 3,
    }
}

fn price_log_threshold(threshold: PriceThreshold, origin: MarketFrame) -> Option<f64> {
    let threshold = match threshold {
        PriceThreshold::AbsoluteFraction(value) => value,
        PriceThreshold::VolatilityScaled { multiple } => origin.volatility()? * multiple,
        PriceThreshold::Combined {
            absolute_floor_fraction,
            volatility_multiple,
        } => absolute_floor_fraction.max(origin.volatility()? * volatility_multiple),
    };
    (threshold.is_finite() && threshold > 0.0 && threshold < 1.0).then_some(threshold)
}

fn maximum_trustworthy_sources(
    path: &LabelPath,
    horizon_seconds: u64,
    minimum_quality_millionths: u32,
    minimum_source_coverage_millionths: u32,
) -> u32 {
    path.frames()
        .iter()
        .copied()
        .take_while(|frame| frame.offset_seconds() <= horizon_seconds)
        .filter(|frame| {
            frame.is_trustworthy_at(
                horizon_seconds,
                minimum_quality_millionths,
                minimum_source_coverage_millionths,
            )
        })
        .map(MarketFrame::corroborating_sources)
        .max()
        .unwrap_or(0)
}

/// Deterministic reference paths used by documentation and integration tests.
pub mod fixtures {
    use quality::SourceHealthState;

    use crate::{LabelError, LabelPath, MarketFrameInput};

    pub fn price_path<const N: usize>(prices: [f64; N]) -> Result<LabelPath, LabelError> {
        let frames = prices
            .into_iter()
            .enumerate()
            .map(|(index, price)| {
                MarketFrameInput::price(
                    u64::try_from(index).unwrap_or(u64::MAX).saturating_mul(60),
                    price,
                )
            })
            .collect();
        LabelPath::try_new(frames)
    }

    pub fn censored_path() -> Result<LabelPath, LabelError> {
        LabelPath::try_new(vec![
            MarketFrameInput::price(0, 100.0),
            MarketFrameInput::price(60, 99.0),
            MarketFrameInput::price(120, 98.0).with_health(SourceHealthState::Unhealthy),
        ])
    }
}

/// Fail-closed label construction or evaluation error.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LabelError {
    #[error("label definition is invalid or exceeds its bound")]
    InvalidDefinition,
    #[error("label definition could not be serialized canonically")]
    Serialization,
    #[error("label path exceeds its capacity")]
    PathCapacity,
    #[error("label path chronology is invalid")]
    InvalidChronology,
    #[error("label frame contains invalid numeric or source evidence")]
    InvalidFrame,
    #[error("label frame claims knowledge before its event offset")]
    PreEventKnowledge,
    #[error("label definition has multiple horizons; choose one explicitly")]
    AmbiguousHorizon,
    #[error("requested horizon is not declared by the label definition")]
    UnknownHorizon,
}
