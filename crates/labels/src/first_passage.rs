//! Bounded point-in-time paths and persistent first-passage evaluation.

use quality::SourceHealthState;

use crate::{ExclusionReason, LabelError, LabelOutcome};

const MAXIMUM_PATH_FRAMES: usize = 4_096;
const MAXIMUM_CORROBORATING_SOURCES: u32 = 64;
const ONE_MILLION: u32 = 1_000_000;

/// Point-in-time market eligibility independent of numeric data quality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarketEligibility {
    Eligible,
    HaltedOrDelisted,
    InstrumentDefinitionChanged,
    UnresolvedCorrection,
    TimestampIntegrityCompromised,
}

impl MarketEligibility {
    const fn exclusion_reason(self) -> Option<ExclusionReason> {
        match self {
            Self::Eligible => None,
            Self::HaltedOrDelisted => Some(ExclusionReason::HaltedOrDelisted),
            Self::InstrumentDefinitionChanged => Some(ExclusionReason::InstrumentDefinitionChanged),
            Self::UnresolvedCorrection => Some(ExclusionReason::UnresolvedCorrection),
            Self::TimestampIntegrityCompromised => {
                Some(ExclusionReason::TimestampIntegrityCompromised)
            }
        }
    }
}

/// Construction input for one point-in-time consolidated-market frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarketFrameInput {
    pub offset_seconds: u64,
    pub known_at_offset_seconds: u64,
    pub price: Option<f64>,
    pub volatility: Option<f64>,
    pub spread_bps: Option<f64>,
    pub displayed_depth: Option<f64>,
    pub cancellation_rate: Option<f64>,
    pub sweep_cost_bps: Option<f64>,
    pub resiliency_seconds: Option<f64>,
    pub liquidation_notional: Option<f64>,
    pub open_interest: Option<f64>,
    pub corroborating_sources: u32,
    pub quality_millionths: u32,
    pub source_coverage_millionths: u32,
    pub liquidation_confidence_millionths: u32,
    pub source_health: SourceHealthState,
    pub eligibility: MarketEligibility,
    pub finalized: bool,
}

impl MarketFrameInput {
    pub const fn price(offset_seconds: u64, price: f64) -> Self {
        Self {
            offset_seconds,
            known_at_offset_seconds: offset_seconds,
            price: Some(price),
            volatility: None,
            spread_bps: None,
            displayed_depth: None,
            cancellation_rate: None,
            sweep_cost_bps: None,
            resiliency_seconds: None,
            liquidation_notional: None,
            open_interest: None,
            corroborating_sources: 0,
            quality_millionths: ONE_MILLION,
            source_coverage_millionths: ONE_MILLION,
            liquidation_confidence_millionths: 0,
            source_health: SourceHealthState::Healthy,
            eligibility: MarketEligibility::Eligible,
            finalized: true,
        }
    }

    pub const fn with_volatility(mut self, volatility: f64) -> Self {
        self.volatility = Some(volatility);
        self
    }

    pub const fn with_liquidity(mut self, spread_bps: f64, displayed_depth: f64) -> Self {
        self.spread_bps = Some(spread_bps);
        self.displayed_depth = Some(displayed_depth);
        self
    }

    pub const fn with_liquidity_mechanism(
        mut self,
        cancellation_rate: f64,
        sweep_cost_bps: f64,
        resiliency_seconds: f64,
    ) -> Self {
        self.cancellation_rate = Some(cancellation_rate);
        self.sweep_cost_bps = Some(sweep_cost_bps);
        self.resiliency_seconds = Some(resiliency_seconds);
        self
    }

    pub const fn with_sources(mut self, corroborating_sources: u32) -> Self {
        self.corroborating_sources = corroborating_sources;
        self
    }

    pub const fn with_derivatives(
        mut self,
        liquidation_notional: f64,
        open_interest: f64,
        corroborating_sources: u32,
        liquidation_confidence_millionths: u32,
    ) -> Self {
        self.liquidation_notional = Some(liquidation_notional);
        self.open_interest = Some(open_interest);
        self.corroborating_sources = corroborating_sources;
        self.liquidation_confidence_millionths = liquidation_confidence_millionths;
        self
    }

    pub const fn with_quality(
        mut self,
        quality_millionths: u32,
        source_coverage_millionths: u32,
    ) -> Self {
        self.quality_millionths = quality_millionths;
        self.source_coverage_millionths = source_coverage_millionths;
        self
    }

    pub const fn with_health(mut self, source_health: SourceHealthState) -> Self {
        self.source_health = source_health;
        self
    }

    pub const fn with_eligibility(mut self, eligibility: MarketEligibility) -> Self {
        self.eligibility = eligibility;
        self
    }
}

/// Validated immutable point-in-time frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarketFrame {
    offset_seconds: u64,
    known_at_offset_seconds: u64,
    price: Option<f64>,
    volatility: Option<f64>,
    spread_bps: Option<f64>,
    displayed_depth: Option<f64>,
    cancellation_rate: Option<f64>,
    sweep_cost_bps: Option<f64>,
    resiliency_seconds: Option<f64>,
    liquidation_notional: Option<f64>,
    open_interest: Option<f64>,
    corroborating_sources: u32,
    quality_millionths: u32,
    source_coverage_millionths: u32,
    liquidation_confidence_millionths: u32,
    source_health: SourceHealthState,
    eligibility: MarketEligibility,
    finalized: bool,
}

impl MarketFrame {
    fn try_new(input: MarketFrameInput) -> Result<Self, LabelError> {
        if input.known_at_offset_seconds < input.offset_seconds {
            return Err(LabelError::PreEventKnowledge);
        }
        if input.corroborating_sources > MAXIMUM_CORROBORATING_SOURCES
            || input.quality_millionths > ONE_MILLION
            || input.source_coverage_millionths > ONE_MILLION
            || input.liquidation_confidence_millionths > ONE_MILLION
            || input.price.is_some_and(|value| !positive_finite(value))
            || input
                .volatility
                .is_some_and(|value| !nonnegative_finite(value))
            || input
                .spread_bps
                .is_some_and(|value| !nonnegative_finite(value))
            || input
                .displayed_depth
                .is_some_and(|value| !nonnegative_finite(value))
            || input
                .cancellation_rate
                .is_some_and(|value| !nonnegative_finite(value))
            || input
                .sweep_cost_bps
                .is_some_and(|value| !nonnegative_finite(value))
            || input
                .resiliency_seconds
                .is_some_and(|value| !nonnegative_finite(value))
            || input
                .liquidation_notional
                .is_some_and(|value| !nonnegative_finite(value))
            || input
                .open_interest
                .is_some_and(|value| !positive_finite(value))
        {
            return Err(LabelError::InvalidFrame);
        }
        Ok(Self {
            offset_seconds: input.offset_seconds,
            known_at_offset_seconds: input.known_at_offset_seconds,
            price: input.price,
            volatility: input.volatility,
            spread_bps: input.spread_bps,
            displayed_depth: input.displayed_depth,
            cancellation_rate: input.cancellation_rate,
            sweep_cost_bps: input.sweep_cost_bps,
            resiliency_seconds: input.resiliency_seconds,
            liquidation_notional: input.liquidation_notional,
            open_interest: input.open_interest,
            corroborating_sources: input.corroborating_sources,
            quality_millionths: input.quality_millionths,
            source_coverage_millionths: input.source_coverage_millionths,
            liquidation_confidence_millionths: input.liquidation_confidence_millionths,
            source_health: input.source_health,
            eligibility: input.eligibility,
            finalized: input.finalized,
        })
    }

    pub const fn offset_seconds(self) -> u64 {
        self.offset_seconds
    }

    pub const fn known_at_offset_seconds(self) -> u64 {
        self.known_at_offset_seconds
    }

    pub const fn price(self) -> Option<f64> {
        self.price
    }

    pub const fn volatility(self) -> Option<f64> {
        self.volatility
    }

    pub const fn spread_bps(self) -> Option<f64> {
        self.spread_bps
    }

    pub const fn displayed_depth(self) -> Option<f64> {
        self.displayed_depth
    }

    pub const fn cancellation_rate(self) -> Option<f64> {
        self.cancellation_rate
    }

    pub const fn sweep_cost_bps(self) -> Option<f64> {
        self.sweep_cost_bps
    }

    pub const fn resiliency_seconds(self) -> Option<f64> {
        self.resiliency_seconds
    }

    pub const fn liquidation_notional(self) -> Option<f64> {
        self.liquidation_notional
    }

    pub const fn open_interest(self) -> Option<f64> {
        self.open_interest
    }

    pub const fn corroborating_sources(self) -> u32 {
        self.corroborating_sources
    }

    pub const fn quality_millionths(self) -> u32 {
        self.quality_millionths
    }

    pub const fn source_coverage_millionths(self) -> u32 {
        self.source_coverage_millionths
    }

    pub const fn liquidation_confidence_millionths(self) -> u32 {
        self.liquidation_confidence_millionths
    }

    pub const fn eligibility(self) -> MarketEligibility {
        self.eligibility
    }

    pub const fn is_trustworthy_at(
        self,
        decision_offset_seconds: u64,
        minimum_quality_millionths: u32,
        minimum_source_coverage_millionths: u32,
    ) -> bool {
        self.finalized
            && matches!(self.source_health, SourceHealthState::Healthy)
            && matches!(self.eligibility, MarketEligibility::Eligible)
            && self.known_at_offset_seconds <= decision_offset_seconds
            && self.quality_millionths >= minimum_quality_millionths
            && self.source_coverage_millionths >= minimum_source_coverage_millionths
    }
}

/// Strictly ordered, uniformly sampled path with explicit knowledge times.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelPath {
    entity_id: String,
    origin_time_ns: i64,
    episode_cluster_id: String,
    provenance_bound: bool,
    frames: Vec<MarketFrame>,
    evidence_hash: [u8; 32],
}

impl LabelPath {
    pub fn try_new(frames: Vec<MarketFrameInput>) -> Result<Self, LabelError> {
        let mut path =
            Self::try_new_bound("unbound_fixture", 1, "unbound_fixture_episode", frames)?;
        path.provenance_bound = false;
        path.evidence_hash = hash_frames(
            &path.entity_id,
            path.origin_time_ns,
            &path.episode_cluster_id,
            path.provenance_bound,
            &path.frames,
        );
        Ok(path)
    }

    /// Constructs a production label path whose entity, absolute origin, and
    /// episode/block cluster are explicit and evidence-hashed.
    pub fn try_new_bound(
        entity_id: impl Into<String>,
        origin_time_ns: i64,
        episode_cluster_id: impl Into<String>,
        frames: Vec<MarketFrameInput>,
    ) -> Result<Self, LabelError> {
        let entity_id = entity_id.into();
        let episode_cluster_id = episode_cluster_id.into();
        if origin_time_ns <= 0
            || !valid_identifier(&entity_id)
            || !valid_identifier(&episode_cluster_id)
        {
            return Err(LabelError::InvalidFrame);
        }
        if frames.len() < 2 || frames.len() > MAXIMUM_PATH_FRAMES {
            return Err(LabelError::PathCapacity);
        }
        let frames = frames
            .into_iter()
            .map(MarketFrame::try_new)
            .collect::<Result<Vec<_>, _>>()?;
        let cadence_seconds = frames[1].offset_seconds;
        if frames[0].offset_seconds != 0
            || frames[0].known_at_offset_seconds != 0
            || cadence_seconds == 0
            || frames.windows(2).any(|pair| {
                pair[0].offset_seconds.checked_add(cadence_seconds) != Some(pair[1].offset_seconds)
                    || pair[0].known_at_offset_seconds > pair[1].known_at_offset_seconds
            })
        {
            return Err(LabelError::InvalidChronology);
        }
        let provenance_bound = true;
        let evidence_hash = hash_frames(
            &entity_id,
            origin_time_ns,
            &episode_cluster_id,
            provenance_bound,
            &frames,
        );
        Ok(Self {
            entity_id,
            origin_time_ns,
            episode_cluster_id,
            provenance_bound,
            frames,
            evidence_hash,
        })
    }

    pub fn entity_id(&self) -> &str {
        &self.entity_id
    }

    pub const fn origin_time_ns(&self) -> i64 {
        self.origin_time_ns
    }

    pub fn episode_cluster_id(&self) -> &str {
        &self.episode_cluster_id
    }

    pub const fn provenance_bound(&self) -> bool {
        self.provenance_bound
    }

    pub fn frames(&self) -> &[MarketFrame] {
        &self.frames
    }

    pub fn origin(&self) -> MarketFrame {
        self.frames[0]
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

fn hash_frames(
    entity_id: &str,
    origin_time_ns: i64,
    episode_cluster_id: &str,
    provenance_bound: bool,
    frames: &[MarketFrame],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cmti:label-path:v2\0");
    hash_string(&mut hasher, entity_id);
    hasher.update(&origin_time_ns.to_le_bytes());
    hash_string(&mut hasher, episode_cluster_id);
    hasher.update(&[u8::from(provenance_bound)]);
    hasher.update(&(frames.len() as u64).to_le_bytes());
    for frame in frames {
        hash_market_frame(&mut hasher, *frame);
    }
    *hasher.finalize().as_bytes()
}

fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= 256
        && identifier
            .bytes()
            .all(|byte| !byte.is_ascii_control() && !byte.is_ascii_whitespace())
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn hash_market_frame(hasher: &mut blake3::Hasher, frame: MarketFrame) {
    hasher.update(&frame.offset_seconds.to_le_bytes());
    hasher.update(&frame.known_at_offset_seconds.to_le_bytes());
    for value in [
        frame.price,
        frame.volatility,
        frame.spread_bps,
        frame.displayed_depth,
        frame.cancellation_rate,
        frame.sweep_cost_bps,
        frame.resiliency_seconds,
        frame.liquidation_notional,
        frame.open_interest,
    ] {
        match value {
            Some(value) => {
                hasher.update(&[1]);
                hasher.update(&value.to_bits().to_le_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    hasher.update(&frame.corroborating_sources.to_le_bytes());
    hasher.update(&frame.quality_millionths.to_le_bytes());
    hasher.update(&frame.source_coverage_millionths.to_le_bytes());
    hasher.update(&frame.liquidation_confidence_millionths.to_le_bytes());
    hasher.update(&[match frame.source_health {
        SourceHealthState::Healthy => 1,
        SourceHealthState::Degraded => 2,
        SourceHealthState::Unhealthy => 3,
        SourceHealthState::Quarantined => 4,
        SourceHealthState::Recovering => 5,
    }]);
    hasher.update(&[match frame.eligibility {
        MarketEligibility::Eligible => 1,
        MarketEligibility::HaltedOrDelisted => 2,
        MarketEligibility::InstrumentDefinitionChanged => 3,
        MarketEligibility::UnresolvedCorrection => 4,
        MarketEligibility::TimestampIntegrityCompromised => 5,
    }]);
    hasher.update(&[u8::from(frame.finalized)]);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FirstPassageEvaluation {
    pub outcome: LabelOutcome,
    pub known_at_offset_seconds: u64,
}

pub(crate) fn first_persistent_passage(
    path: &LabelPath,
    horizon_seconds: u64,
    minimum_duration_seconds: u64,
    minimum_quality_millionths: u32,
    minimum_source_coverage_millionths: u32,
    mut condition: impl FnMut(MarketFrame) -> Option<bool>,
) -> Result<FirstPassageEvaluation, LabelError> {
    let mut run_start = None;
    let mut last_trustworthy = None;
    let mut last_known_at = 0;
    for frame in path
        .frames()
        .iter()
        .copied()
        .take_while(|frame| frame.offset_seconds <= horizon_seconds)
    {
        if !frame.is_trustworthy_at(
            horizon_seconds,
            minimum_quality_millionths,
            minimum_source_coverage_millionths,
        ) || frame.price.is_none()
        {
            if let Some(reason) = frame.eligibility.exclusion_reason() {
                return Ok(FirstPassageEvaluation {
                    outcome: LabelOutcome::Excluded(reason),
                    known_at_offset_seconds: frame.known_at_offset_seconds,
                });
            }
            return Ok(FirstPassageEvaluation {
                outcome: LabelOutcome::Censored {
                    observed_seconds: last_trustworthy.unwrap_or(0),
                },
                known_at_offset_seconds: last_known_at,
            });
        }
        let Some(matches) = condition(frame) else {
            return Ok(FirstPassageEvaluation {
                outcome: if frame.offset_seconds == 0 {
                    LabelOutcome::Excluded(ExclusionReason::MissingEvidence)
                } else {
                    LabelOutcome::Censored {
                        observed_seconds: last_trustworthy.unwrap_or(0),
                    }
                },
                known_at_offset_seconds: last_known_at,
            });
        };
        last_trustworthy = Some(frame.offset_seconds);
        last_known_at = frame.known_at_offset_seconds;
        if matches {
            let start = *run_start.get_or_insert(frame.offset_seconds);
            if frame.offset_seconds.saturating_sub(start) >= minimum_duration_seconds {
                return Ok(FirstPassageEvaluation {
                    outcome: LabelOutcome::Occurred {
                        offset_seconds: start,
                    },
                    known_at_offset_seconds: frame.known_at_offset_seconds,
                });
            }
        } else {
            run_start = None;
        }
    }
    let observed_seconds = last_trustworthy.unwrap_or(0);
    Ok(FirstPassageEvaluation {
        outcome: if observed_seconds < horizon_seconds {
            LabelOutcome::Censored { observed_seconds }
        } else {
            LabelOutcome::NotOccurred
        },
        known_at_offset_seconds: last_known_at,
    })
}

fn positive_finite(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn nonnegative_finite(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}
