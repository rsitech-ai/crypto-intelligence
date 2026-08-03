//! Bounded, evidence-bound conditional market scenario simulation.

pub mod liquidity;
pub mod process;
pub mod summary;

pub use liquidity::{ImpactModel, ImpactParameters, LiquiditySnapshot};
pub use process::{EmpiricalInnovations, MarketProcess, ProcessParameters};
pub use summary::{
    HorizonSummary, QuantileValue, RangeSummary, RepresentativePath, ScenarioPathPoint,
    SimulationLabel, SweepCostSummary, ThresholdProbability,
};

use process::{DeterministicRng, ProcessStepInput};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use summary::{RawHorizonValue, RawPath};
use thiserror::Error;

const CONDITION_DOMAIN: &[u8] = b"cmti:scenario-condition:v1\0";
const CONFIG_DOMAIN: &[u8] = b"cmti:scenario-config:v1\0";
const ENGINE_DOMAIN: &[u8] = b"cmti:scenario-engine:v1\0";
const PATH_DOMAIN: &[u8] = b"cmti:scenario-paths:v1\0";
const RESULT_DOMAIN: &[u8] = b"cmti:scenario-result:v1\0";
const MAXIMUM_PATHS: u32 = 8_192;
const MAXIMUM_STEPS: u64 = 1_440;
const MAXIMUM_WORK: u64 = 2_000_000;
const MAXIMUM_SUMMARY_WORK: u64 = 16_000_000;
const MAXIMUM_HORIZONS: usize = 32;
const MAXIMUM_THRESHOLDS: usize = 64;
const MAXIMUM_NOTIONALS: usize = 16;
const MAXIMUM_REPRESENTATIVES: usize = 9;
const MAXIMUM_STRESSES: usize = 32;
const MAXIMUM_MODEL_PACKAGES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossingDirection {
    Below,
    Above,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventThreshold {
    id: String,
    direction: CrossingDirection,
    return_fraction: f64,
}

impl EventThreshold {
    pub fn try_new(
        id: impl Into<String>,
        direction: CrossingDirection,
        return_fraction: f64,
    ) -> Result<Self, ScenarioError> {
        let value = Self {
            id: id.into(),
            direction,
            return_fraction,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn direction(&self) -> CrossingDirection {
        self.direction
    }

    pub const fn return_fraction(&self) -> f64 {
        self.return_fraction
    }

    fn validate(&self) -> Result<(), ScenarioError> {
        let direction_matches = match self.direction {
            CrossingDirection::Below => self.return_fraction < 0.0,
            CrossingDirection::Above => self.return_fraction > 0.0,
        };
        if identifier_is_valid(&self.id)
            && self.return_fraction.is_finite()
            && (-0.99..=10.0).contains(&self.return_fraction)
            && direction_matches
        {
            Ok(())
        } else {
            Err(ScenarioError::InvalidThreshold)
        }
    }

    fn crossed(&self, price_return: f64) -> bool {
        match self.direction {
            CrossingDirection::Below => price_return <= self.return_fraction,
            CrossingDirection::Above => price_return >= self.return_fraction,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum OptionsCondition {
    Available {
        volatility_multiplier: f64,
        skew: f64,
        as_known_at_ns: i64,
        evidence_hash: [u8; 32],
    },
    Unavailable {
        reason: String,
    },
}

impl OptionsCondition {
    pub fn available(
        volatility_multiplier: f64,
        skew: f64,
        as_known_at_ns: i64,
        evidence_hash: [u8; 32],
    ) -> Result<Self, ScenarioError> {
        let value = Self::Available {
            volatility_multiplier,
            skew,
            as_known_at_ns,
            evidence_hash,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn unavailable(reason: impl Into<String>) -> Result<Self, ScenarioError> {
        let value = Self::Unavailable {
            reason: reason.into(),
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ScenarioError> {
        match self {
            Self::Available {
                volatility_multiplier,
                skew,
                as_known_at_ns,
                evidence_hash,
            } if volatility_multiplier.is_finite()
                && (0.25..=4.0).contains(volatility_multiplier)
                && skew.is_finite()
                && (-2.0..=2.0).contains(skew)
                && *as_known_at_ns > 0
                && *evidence_hash != [0; 32] =>
            {
                Ok(())
            }
            Self::Unavailable { reason } if identifier_is_valid(reason) => Ok(()),
            Self::Available { .. } | Self::Unavailable { .. } => {
                Err(ScenarioError::InvalidOptionsCondition)
            }
        }
    }

    fn multiplier(&self) -> f64 {
        match self {
            Self::Available {
                volatility_multiplier,
                ..
            } => *volatility_multiplier,
            Self::Unavailable { .. } => 1.0,
        }
    }

    fn skew(&self) -> f64 {
        match self {
            Self::Available { skew, .. } => *skew,
            Self::Unavailable { .. } => 0.0,
        }
    }

    fn as_known_at_ns(&self) -> Option<i64> {
        match self {
            Self::Available { as_known_at_ns, .. } => Some(*as_known_at_ns),
            Self::Unavailable { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScenarioConditionInput {
    pub as_of_ns: i64,
    pub as_known_at_ns: i64,
    pub entity_id: String,
    pub starting_price: f64,
    pub starting_volatility_annualized: f64,
    pub regime_probability: f64,
    pub calibrated_transition_probability: f64,
    pub cusp_instability: f64,
    pub fast_stress: f64,
    pub systemic_return_condition: f64,
    pub liquidity: LiquiditySnapshot,
    pub options: OptionsCondition,
    pub forecast_evidence_hash: [u8; 32],
    pub applicability_evidence_hash: [u8; 32],
    pub source_evidence_hash: [u8; 32],
    pub model_package_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioCondition {
    schema_version: u32,
    as_of_ns: i64,
    as_known_at_ns: i64,
    entity_id: String,
    starting_price: f64,
    starting_volatility_annualized: f64,
    regime_probability: f64,
    calibrated_transition_probability: f64,
    cusp_instability: f64,
    fast_stress: f64,
    systemic_return_condition: f64,
    liquidity: LiquiditySnapshot,
    options: OptionsCondition,
    forecast_evidence_hash: [u8; 32],
    applicability_evidence_hash: [u8; 32],
    source_evidence_hash: [u8; 32],
    model_package_ids: Vec<String>,
    evidence_hash: [u8; 32],
}

impl ScenarioCondition {
    pub fn try_new(mut input: ScenarioConditionInput) -> Result<Self, ScenarioError> {
        input.model_package_ids.sort();
        let mut value = Self {
            schema_version: 1,
            as_of_ns: input.as_of_ns,
            as_known_at_ns: input.as_known_at_ns,
            entity_id: input.entity_id,
            starting_price: input.starting_price,
            starting_volatility_annualized: input.starting_volatility_annualized,
            regime_probability: input.regime_probability,
            calibrated_transition_probability: input.calibrated_transition_probability,
            cusp_instability: input.cusp_instability,
            fast_stress: input.fast_stress,
            systemic_return_condition: input.systemic_return_condition,
            liquidity: input.liquidity,
            options: input.options,
            forecast_evidence_hash: input.forecast_evidence_hash,
            applicability_evidence_hash: input.applicability_evidence_hash,
            source_evidence_hash: input.source_evidence_hash,
            model_package_ids: input.model_package_ids,
            evidence_hash: [0; 32],
        };
        value.validate_without_hash()?;
        value.evidence_hash = value.calculate_hash();
        Ok(value)
    }

    pub fn as_input(&self) -> ScenarioConditionInput {
        ScenarioConditionInput {
            as_of_ns: self.as_of_ns,
            as_known_at_ns: self.as_known_at_ns,
            entity_id: self.entity_id.clone(),
            starting_price: self.starting_price,
            starting_volatility_annualized: self.starting_volatility_annualized,
            regime_probability: self.regime_probability,
            calibrated_transition_probability: self.calibrated_transition_probability,
            cusp_instability: self.cusp_instability,
            fast_stress: self.fast_stress,
            systemic_return_condition: self.systemic_return_condition,
            liquidity: self.liquidity,
            options: self.options.clone(),
            forecast_evidence_hash: self.forecast_evidence_hash,
            applicability_evidence_hash: self.applicability_evidence_hash,
            source_evidence_hash: self.source_evidence_hash,
            model_package_ids: self.model_package_ids.clone(),
        }
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub fn validate(&self) -> Result<(), ScenarioError> {
        self.validate_without_hash()?;
        if self.evidence_hash == self.calculate_hash() {
            Ok(())
        } else {
            Err(ScenarioError::InvalidArtifact)
        }
    }

    fn validate_without_hash(&self) -> Result<(), ScenarioError> {
        let packages: BTreeSet<&str> = self.model_package_ids.iter().map(String::as_str).collect();
        let finite_probabilities = [
            self.regime_probability,
            self.calibrated_transition_probability,
            self.cusp_instability,
            self.fast_stress,
        ]
        .into_iter()
        .all(|value| value.is_finite() && (0.0..=1.0).contains(&value));
        if self.schema_version != 1
            || self.as_known_at_ns <= 0
            || self.as_known_at_ns > self.as_of_ns
            || !identifier_is_valid(&self.entity_id)
            || !self.starting_price.is_finite()
            || self.starting_price <= 0.0
            || !self.starting_volatility_annualized.is_finite()
            || !(0.0..=10.0).contains(&self.starting_volatility_annualized)
            || self.starting_volatility_annualized == 0.0
            || !finite_probabilities
            || !self.systemic_return_condition.is_finite()
            || !(-0.50..=0.50).contains(&self.systemic_return_condition)
            || self.liquidity.validate().is_err()
            || self.liquidity.as_known_at_ns() > self.as_known_at_ns
            || self.options.validate().is_err()
            || self
                .options
                .as_known_at_ns()
                .is_some_and(|known| known > self.as_known_at_ns)
            || [
                self.forecast_evidence_hash,
                self.applicability_evidence_hash,
                self.source_evidence_hash,
            ]
            .contains(&[0; 32])
            || !(1..=MAXIMUM_MODEL_PACKAGES).contains(&self.model_package_ids.len())
            || packages.len() != self.model_package_ids.len()
            || self
                .model_package_ids
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self
                .model_package_ids
                .iter()
                .any(|value| !identifier_is_valid(value))
        {
            Err(ScenarioError::InvalidCondition)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CONDITION_DOMAIN);
        hasher.update(&self.schema_version.to_le_bytes());
        hasher.update(&self.as_of_ns.to_le_bytes());
        hasher.update(&self.as_known_at_ns.to_le_bytes());
        hash_string(&mut hasher, &self.entity_id);
        for value in [
            self.starting_price,
            self.starting_volatility_annualized,
            self.regime_probability,
            self.calibrated_transition_probability,
            self.cusp_instability,
            self.fast_stress,
            self.systemic_return_condition,
        ] {
            hash_f64(&mut hasher, value);
        }
        hasher.update(&self.liquidity.identity_hash());
        hash_options(&mut hasher, &self.options);
        hasher.update(&self.forecast_evidence_hash);
        hasher.update(&self.applicability_evidence_hash);
        hasher.update(&self.source_evidence_hash);
        hash_u64(&mut hasher, self.model_package_ids.len() as u64);
        for package in &self.model_package_ids {
            hash_string(&mut hasher, package);
        }
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StressInjection {
    id: String,
    start_step: u64,
    duration_steps: u64,
    return_shock_per_step: f64,
    volatility_multiplier: f64,
    liquidity_multiplier: f64,
    jump_probability_addition: f64,
}

impl StressInjection {
    pub fn try_new(
        id: impl Into<String>,
        start_step: u64,
        duration_steps: u64,
        return_shock_per_step: f64,
        volatility_multiplier: f64,
        liquidity_multiplier: f64,
        jump_probability_addition: f64,
    ) -> Result<Self, ScenarioError> {
        let value = Self {
            id: id.into(),
            start_step,
            duration_steps,
            return_shock_per_step,
            volatility_multiplier,
            liquidity_multiplier,
            jump_probability_addition,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ScenarioError> {
        if identifier_is_valid(&self.id)
            && self.start_step > 0
            && self.duration_steps > 0
            && self.return_shock_per_step.is_finite()
            && (-0.25..=0.25).contains(&self.return_shock_per_step)
            && self.volatility_multiplier.is_finite()
            && (0.25..=4.0).contains(&self.volatility_multiplier)
            && self.liquidity_multiplier.is_finite()
            && (0.05..=2.0).contains(&self.liquidity_multiplier)
            && self.jump_probability_addition.is_finite()
            && (0.0..=1.0).contains(&self.jump_probability_addition)
        {
            Ok(())
        } else {
            Err(ScenarioError::InvalidStress)
        }
    }

    fn is_active(&self, step: u64) -> bool {
        self.start_step <= step && step < self.start_step.saturating_add(self.duration_steps)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScenarioConfigInput {
    pub seed: u64,
    pub path_count: u32,
    pub step_seconds: u64,
    pub horizons_seconds: Vec<u64>,
    pub thresholds: Vec<EventThreshold>,
    pub sweep_notionals_usd: Vec<f64>,
    pub representative_quantiles: Vec<f64>,
    pub liquidation_amplification: bool,
    pub stress_injections: Vec<StressInjection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioConfig {
    schema_version: u32,
    seed: u64,
    path_count: u32,
    step_seconds: u64,
    horizons_seconds: Vec<u64>,
    thresholds: Vec<EventThreshold>,
    sweep_notionals_usd: Vec<f64>,
    representative_quantiles: Vec<f64>,
    liquidation_amplification: bool,
    stress_injections: Vec<StressInjection>,
    evidence_hash: [u8; 32],
}

impl ScenarioConfig {
    pub fn try_new(mut input: ScenarioConfigInput) -> Result<Self, ScenarioError> {
        input.horizons_seconds.sort_unstable();
        input
            .thresholds
            .sort_by(|left, right| left.id.cmp(&right.id));
        input.sweep_notionals_usd.sort_by(f64::total_cmp);
        input.representative_quantiles.sort_by(f64::total_cmp);
        input
            .stress_injections
            .sort_by(|left, right| left.id.cmp(&right.id));
        let mut value = Self {
            schema_version: 1,
            seed: input.seed,
            path_count: input.path_count,
            step_seconds: input.step_seconds,
            horizons_seconds: input.horizons_seconds,
            thresholds: input.thresholds,
            sweep_notionals_usd: input.sweep_notionals_usd,
            representative_quantiles: input.representative_quantiles,
            liquidation_amplification: input.liquidation_amplification,
            stress_injections: input.stress_injections,
            evidence_hash: [0; 32],
        };
        value.validate_without_hash()?;
        value.evidence_hash = value.calculate_hash();
        Ok(value)
    }

    pub fn as_input(&self) -> ScenarioConfigInput {
        ScenarioConfigInput {
            seed: self.seed,
            path_count: self.path_count,
            step_seconds: self.step_seconds,
            horizons_seconds: self.horizons_seconds.clone(),
            thresholds: self.thresholds.clone(),
            sweep_notionals_usd: self.sweep_notionals_usd.clone(),
            representative_quantiles: self.representative_quantiles.clone(),
            liquidation_amplification: self.liquidation_amplification,
            stress_injections: self.stress_injections.clone(),
        }
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub fn validate(&self) -> Result<(), ScenarioError> {
        self.validate_without_hash()?;
        if self.evidence_hash == self.calculate_hash() {
            Ok(())
        } else {
            Err(ScenarioError::InvalidArtifact)
        }
    }

    fn maximum_steps(&self) -> Result<u64, ScenarioError> {
        self.horizons_seconds
            .last()
            .copied()
            .ok_or(ScenarioError::InvalidConfig)?
            .checked_div(self.step_seconds)
            .ok_or(ScenarioError::InvalidConfig)
    }

    fn validate_without_hash(&self) -> Result<(), ScenarioError> {
        let maximum_horizon = self.horizons_seconds.last().copied().unwrap_or(0);
        let maximum_steps = maximum_horizon.checked_div(self.step_seconds.max(1));
        let work = maximum_steps.and_then(|steps| steps.checked_mul(u64::from(self.path_count)));
        let summary_width = self
            .thresholds
            .len()
            .checked_add(self.sweep_notionals_usd.len());
        let summary_work = summary_width
            .and_then(|width| width.checked_mul(self.horizons_seconds.len()))
            .and_then(|work| work.checked_mul(self.path_count as usize))
            .and_then(|work| u64::try_from(work).ok());
        if self.schema_version != 1
            || !(1..=MAXIMUM_PATHS).contains(&self.path_count)
            || self.step_seconds == 0
            || self.horizons_seconds.is_empty()
            || self.horizons_seconds.len() > MAXIMUM_HORIZONS
            || self
                .horizons_seconds
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self
                .horizons_seconds
                .iter()
                .any(|value| *value == 0 || !value.is_multiple_of(self.step_seconds))
            || maximum_steps.is_none_or(|value| value == 0 || value > MAXIMUM_STEPS)
            || work.is_none_or(|value| value > MAXIMUM_WORK)
            || summary_work.is_none_or(|value| value > MAXIMUM_SUMMARY_WORK)
            || self.thresholds.is_empty()
            || self.thresholds.len() > MAXIMUM_THRESHOLDS
            || self
                .thresholds
                .iter()
                .any(|value| value.validate().is_err())
            || self
                .thresholds
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
            || self.sweep_notionals_usd.is_empty()
            || self.sweep_notionals_usd.len() > MAXIMUM_NOTIONALS
            || self
                .sweep_notionals_usd
                .iter()
                .any(|value| !value.is_finite() || *value <= 0.0 || *value > 10_000_000_000.0)
            || self
                .sweep_notionals_usd
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self.representative_quantiles.is_empty()
            || self.representative_quantiles.len() > MAXIMUM_REPRESENTATIVES
            || self
                .representative_quantiles
                .iter()
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
            || self
                .representative_quantiles
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self.stress_injections.len() > MAXIMUM_STRESSES
            || self
                .stress_injections
                .iter()
                .any(|value| value.validate().is_err())
            || self
                .stress_injections
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
            || self.stress_injections.iter().any(|stress| {
                stress
                    .start_step
                    .checked_add(stress.duration_steps)
                    .is_none_or(|end| end > maximum_steps.unwrap_or(0) + 1)
            })
        {
            Err(ScenarioError::InvalidConfig)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CONFIG_DOMAIN);
        hasher.update(&self.schema_version.to_le_bytes());
        hash_u64(&mut hasher, self.seed);
        hasher.update(&self.path_count.to_le_bytes());
        hash_u64(&mut hasher, self.step_seconds);
        hash_u64(&mut hasher, self.horizons_seconds.len() as u64);
        for value in &self.horizons_seconds {
            hash_u64(&mut hasher, *value);
        }
        hash_u64(&mut hasher, self.thresholds.len() as u64);
        for threshold in &self.thresholds {
            hash_string(&mut hasher, &threshold.id);
            hasher.update(&[threshold.direction as u8]);
            hash_f64(&mut hasher, threshold.return_fraction);
        }
        hash_f64_values(&mut hasher, &self.sweep_notionals_usd);
        hash_f64_values(&mut hasher, &self.representative_quantiles);
        hasher.update(&[u8::from(self.liquidation_amplification)]);
        hash_u64(&mut hasher, self.stress_injections.len() as u64);
        for stress in &self.stress_injections {
            hash_string(&mut hasher, &stress.id);
            hash_u64(&mut hasher, stress.start_step);
            hash_u64(&mut hasher, stress.duration_steps);
            hash_f64(&mut hasher, stress.return_shock_per_step);
            hash_f64(&mut hasher, stress.volatility_multiplier);
            hash_f64(&mut hasher, stress.liquidity_multiplier);
            hash_f64(&mut hasher, stress.jump_probability_addition);
        }
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioAssumptions {
    pub path_label: SimulationLabel,
    pub rng_algorithm: String,
    pub quantile_method: String,
    pub innovation_method: String,
    pub process_model_version: String,
    pub impact_model_version: String,
    pub options_used: bool,
    pub options_unavailable_reason: Option<String>,
    pub liquidation_amplification: bool,
    pub stress_ids: Vec<String>,
    pub model_package_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioDistribution {
    pub schema_version: u32,
    pub as_of_ns: i64,
    pub entity_id: String,
    pub path_count: u32,
    pub step_seconds: u64,
    pub horizons: Vec<HorizonSummary>,
    pub representative_paths: Vec<RepresentativePath>,
    pub assumptions: ScenarioAssumptions,
    pub condition_evidence_hash: [u8; 32],
    pub config_evidence_hash: [u8; 32],
    pub engine_evidence_hash: [u8; 32],
    pub path_digest: [u8; 32],
    pub evidence_hash: [u8; 32],
}

impl ScenarioDistribution {
    pub fn horizon(&self, horizon_seconds: u64) -> Option<&HorizonSummary> {
        self.horizons
            .iter()
            .find(|value| value.horizon_seconds == horizon_seconds)
    }

    pub fn verify(
        &self,
        engine: &ScenarioEngine,
        condition: &ScenarioCondition,
        config: &ScenarioConfig,
    ) -> Result<(), ScenarioError> {
        engine.validate()?;
        condition.validate()?;
        config.validate()?;
        self.validate_identity(engine, condition, config)?;
        let expected = engine.simulate_unchecked(condition, config)?;
        if self == &expected {
            Ok(())
        } else {
            Err(ScenarioError::InvalidResult)
        }
    }

    fn validate_identity(
        &self,
        engine: &ScenarioEngine,
        condition: &ScenarioCondition,
        config: &ScenarioConfig,
    ) -> Result<(), ScenarioError> {
        let valid_quantiles = self.horizons.iter().all(|horizon| {
            horizon.return_quantiles.windows(2).all(|pair| {
                pair[0].probability < pair[1].probability && pair[0].value <= pair[1].value
            }) && horizon.volatility_quantiles.windows(2).all(|pair| {
                pair[0].probability < pair[1].probability && pair[0].value <= pair[1].value
            })
        });
        if self.schema_version != 1
            || self.as_of_ns != condition.as_of_ns
            || self.entity_id != condition.entity_id
            || self.path_count != config.path_count
            || self.step_seconds != config.step_seconds
            || self.horizons.len() != config.horizons_seconds.len()
            || self.representative_paths.len() != config.representative_quantiles.len()
            || self
                .representative_paths
                .iter()
                .any(|path| path.label != SimulationLabel::Simulated)
            || !valid_quantiles
            || self.condition_evidence_hash != condition.evidence_hash
            || self.config_evidence_hash != config.evidence_hash
            || self.engine_evidence_hash != engine.evidence_hash
            || self.path_digest == [0; 32]
            || self.evidence_hash != self.calculate_hash()
        {
            Err(ScenarioError::InvalidResult)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let encoded = serde_json_compatible_hash(self);
        let mut hasher = blake3::Hasher::new();
        hasher.update(RESULT_DOMAIN);
        hasher.update(&encoded);
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioEngine {
    schema_version: u32,
    process: MarketProcess,
    impact: ImpactModel,
    evidence_hash: [u8; 32],
}

impl ScenarioEngine {
    pub fn try_new(process: MarketProcess, impact: ImpactModel) -> Result<Self, ScenarioError> {
        let mut result = Self {
            schema_version: 1,
            process,
            impact,
            evidence_hash: [0; 32],
        };
        result.validate_without_hash()?;
        result.evidence_hash = result.calculate_hash();
        Ok(result)
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub fn validate(&self) -> Result<(), ScenarioError> {
        self.validate_without_hash()?;
        if self.evidence_hash == self.calculate_hash() {
            Ok(())
        } else {
            Err(ScenarioError::InvalidArtifact)
        }
    }

    fn validate_without_hash(&self) -> Result<(), ScenarioError> {
        if self.schema_version == 1
            && self.process.validate().is_ok()
            && self.impact.validate().is_ok()
        {
            Ok(())
        } else {
            Err(ScenarioError::InvalidEngine)
        }
    }

    pub fn simulate(
        &self,
        condition: &ScenarioCondition,
        config: &ScenarioConfig,
    ) -> Result<ScenarioDistribution, ScenarioError> {
        self.validate()?;
        condition.validate()?;
        config.validate()?;
        if config.step_seconds != self.process.parameters().calibration_step_seconds {
            return Err(ScenarioError::InvalidConfig);
        }
        self.simulate_unchecked(condition, config)
    }

    fn simulate_unchecked(
        &self,
        condition: &ScenarioCondition,
        config: &ScenarioConfig,
    ) -> Result<ScenarioDistribution, ScenarioError> {
        let maximum_steps = config.maximum_steps()?;
        let mut rng = DeterministicRng::new(config.seed);
        let mut paths = Vec::with_capacity(config.path_count as usize);
        let mut path_hasher = blake3::Hasher::new();
        path_hasher.update(PATH_DOMAIN);
        path_hasher.update(&self.evidence_hash);
        path_hasher.update(&condition.evidence_hash);
        path_hasher.update(&config.evidence_hash);
        for path_index in 0..config.path_count {
            let path = self.simulate_path(condition, config, maximum_steps, &mut rng)?;
            hash_u64(&mut path_hasher, u64::from(path_index));
            hash_u64(&mut path_hasher, path.points.len() as u64);
            for point in &path.points {
                hash_u64(&mut path_hasher, point.elapsed_seconds);
                hash_f64(&mut path_hasher, point.simulated_price);
                hash_f64(&mut path_hasher, point.price_return);
                hash_f64(&mut path_hasher, point.volatility_annualized);
                hash_f64(&mut path_hasher, point.cumulative_liquidation_usd);
                hash_f64(&mut path_hasher, point.open_interest_usd);
            }
            paths.push(path);
        }
        let path_digest = *path_hasher.finalize().as_bytes();
        let horizons = summary::summarize_horizons(
            &paths,
            &config.horizons_seconds,
            &config.thresholds,
            &config.sweep_notionals_usd,
        )?;
        let representative_paths =
            summary::representative_paths(&paths, &config.representative_quantiles)?;
        let (options_used, options_unavailable_reason) = match &condition.options {
            OptionsCondition::Available { .. } => (true, None),
            OptionsCondition::Unavailable { reason } => (false, Some(reason.clone())),
        };
        let assumptions = ScenarioAssumptions {
            path_label: SimulationLabel::Simulated,
            rng_algorithm: DeterministicRng::ALGORITHM.to_owned(),
            quantile_method: "nist_empirical_linear_v1".to_owned(),
            innovation_method: "empirical_resampling_v1".to_owned(),
            process_model_version: self.process.model_version().to_owned(),
            impact_model_version: self.impact.model_version().to_owned(),
            options_used,
            options_unavailable_reason,
            liquidation_amplification: config.liquidation_amplification,
            stress_ids: config
                .stress_injections
                .iter()
                .map(|stress| stress.id.clone())
                .collect(),
            model_package_ids: condition.model_package_ids.clone(),
        };
        let mut result = ScenarioDistribution {
            schema_version: 1,
            as_of_ns: condition.as_of_ns,
            entity_id: condition.entity_id.clone(),
            path_count: config.path_count,
            step_seconds: config.step_seconds,
            horizons,
            representative_paths,
            assumptions,
            condition_evidence_hash: condition.evidence_hash,
            config_evidence_hash: config.evidence_hash,
            engine_evidence_hash: self.evidence_hash,
            path_digest,
            evidence_hash: [0; 32],
        };
        result.evidence_hash = result.calculate_hash();
        result.validate_identity(self, condition, config)?;
        Ok(result)
    }

    fn simulate_path(
        &self,
        condition: &ScenarioCondition,
        config: &ScenarioConfig,
        maximum_steps: u64,
        rng: &mut DeterministicRng,
    ) -> Result<RawPath, ScenarioError> {
        let mut cumulative_log_return: f64 = 0.0;
        let mut volatility =
            (condition.starting_volatility_annualized * condition.options.multiplier()).clamp(
                self.process.parameters().minimum_volatility_annualized,
                self.process.parameters().maximum_volatility_annualized,
            );
        let mut cumulative_liquidation = 0.0;
        let mut open_interest = condition.liquidity.open_interest_usd();
        let mut crossed = vec![false; config.thresholds.len()];
        let mut points = Vec::with_capacity(maximum_steps as usize);
        let mut horizons = Vec::with_capacity(config.horizons_seconds.len());
        let mut horizon_index = 0;
        for step in 1..=maximum_steps {
            let mut return_stress = 0.0;
            let mut volatility_multiplier = 1.0;
            let mut liquidity_multiplier = 1.0;
            let mut jump_probability_addition = 0.0;
            for stress in config
                .stress_injections
                .iter()
                .filter(|stress| stress.is_active(step))
            {
                return_stress += stress.return_shock_per_step;
                volatility_multiplier *= stress.volatility_multiplier;
                liquidity_multiplier *= stress.liquidity_multiplier;
                jump_probability_addition += stress.jump_probability_addition;
            }
            if ![
                return_stress,
                volatility_multiplier,
                liquidity_multiplier,
                jump_probability_addition,
            ]
            .into_iter()
            .all(f64::is_finite)
                || liquidity_multiplier <= 0.0
            {
                return Err(ScenarioError::NonFiniteArithmetic);
            }
            let process_step = self.process.step(
                rng,
                ProcessStepInput {
                    current_volatility: volatility,
                    step_seconds: config.step_seconds,
                    regime_probability: condition.regime_probability,
                    transition_probability: condition.calibrated_transition_probability,
                    cusp_instability: condition.cusp_instability,
                    systemic_return_drift: condition.systemic_return_condition
                        / maximum_steps as f64,
                    options_skew: condition.options.skew(),
                    volatility_multiplier: volatility_multiplier * (1.0 + condition.fast_stress),
                    return_stress,
                    jump_probability_addition,
                },
            )?;
            let impact = self.impact.apply_feedback(
                process_step.log_return,
                process_step.volatility_annualized,
                condition.liquidity,
                open_interest,
                liquidity_multiplier,
                config.liquidation_amplification,
            )?;
            cumulative_log_return += impact.log_return;
            cumulative_liquidation += impact.liquidated_notional;
            open_interest = impact.open_interest;
            volatility = process_step.volatility_annualized;
            let price_return = cumulative_log_return.exp() - 1.0;
            let simulated_price = condition.starting_price * cumulative_log_return.exp();
            if ![
                cumulative_log_return,
                cumulative_liquidation,
                open_interest,
                volatility,
                price_return,
                simulated_price,
            ]
            .into_iter()
            .all(f64::is_finite)
            {
                return Err(ScenarioError::NonFiniteArithmetic);
            }
            for (threshold, has_crossed) in config.thresholds.iter().zip(&mut crossed) {
                *has_crossed |= threshold.crossed(price_return);
            }
            let elapsed_seconds = step
                .checked_mul(config.step_seconds)
                .ok_or(ScenarioError::CapacityExceeded)?;
            points.push(ScenarioPathPoint {
                elapsed_seconds,
                simulated_price,
                price_return,
                volatility_annualized: volatility,
                cumulative_liquidation_usd: cumulative_liquidation,
                open_interest_usd: open_interest,
            });
            if config.horizons_seconds.get(horizon_index) == Some(&elapsed_seconds) {
                let sweep_costs = config
                    .sweep_notionals_usd
                    .iter()
                    .map(|notional| {
                        self.impact.sweep_cost_bps(
                            *notional,
                            volatility,
                            condition.liquidity,
                            liquidity_multiplier,
                        )
                    })
                    .collect::<Result<_, _>>()?;
                horizons.push(RawHorizonValue {
                    price_return,
                    volatility,
                    threshold_crossed: crossed.clone(),
                    sweep_costs,
                    cumulative_liquidation,
                    open_interest,
                });
                horizon_index += 1;
            }
        }
        if horizons.len() == config.horizons_seconds.len() {
            Ok(RawPath { points, horizons })
        } else {
            Err(ScenarioError::InvalidSummary)
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(ENGINE_DOMAIN);
        hasher.update(&self.schema_version.to_le_bytes());
        hasher.update(&self.process.evidence_hash());
        hasher.update(&self.impact.evidence_hash());
        *hasher.finalize().as_bytes()
    }
}

fn hash_options(hasher: &mut blake3::Hasher, options: &OptionsCondition) {
    match options {
        OptionsCondition::Available {
            volatility_multiplier,
            skew,
            as_known_at_ns,
            evidence_hash,
        } => {
            hasher.update(&[1]);
            hash_f64(hasher, *volatility_multiplier);
            hash_f64(hasher, *skew);
            hasher.update(&as_known_at_ns.to_le_bytes());
            hasher.update(evidence_hash);
        }
        OptionsCondition::Unavailable { reason } => {
            hasher.update(&[0]);
            hash_string(hasher, reason);
        }
    }
}

fn hash_f64_values(hasher: &mut blake3::Hasher, values: &[f64]) {
    hash_u64(hasher, values.len() as u64);
    for value in values {
        hash_f64(hasher, *value);
    }
}

fn serde_json_compatible_hash(result: &ScenarioDistribution) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&result.schema_version.to_le_bytes());
    hasher.update(&result.as_of_ns.to_le_bytes());
    hash_string(&mut hasher, &result.entity_id);
    hasher.update(&result.path_count.to_le_bytes());
    hash_u64(&mut hasher, result.step_seconds);
    hash_u64(&mut hasher, result.horizons.len() as u64);
    for horizon in &result.horizons {
        hash_u64(&mut hasher, horizon.horizon_seconds);
        hash_u64(&mut hasher, horizon.return_quantiles.len() as u64);
        for quantile in &horizon.return_quantiles {
            hash_f64(&mut hasher, quantile.probability);
            hash_f64(&mut hasher, quantile.value);
        }
        hash_u64(&mut hasher, horizon.volatility_quantiles.len() as u64);
        for quantile in &horizon.volatility_quantiles {
            hash_f64(&mut hasher, quantile.probability);
            hash_f64(&mut hasher, quantile.value);
        }
        hash_u64(&mut hasher, horizon.threshold_probabilities.len() as u64);
        for threshold in &horizon.threshold_probabilities {
            hash_string(&mut hasher, &threshold.threshold_id);
            hasher.update(&[threshold.direction as u8]);
            hash_f64(&mut hasher, threshold.threshold_return);
            hasher.update(&threshold.crossing_count.to_le_bytes());
            hash_f64(&mut hasher, threshold.probability);
        }
        hash_u64(&mut hasher, horizon.sweep_costs.len() as u64);
        for cost in &horizon.sweep_costs {
            hash_f64(&mut hasher, cost.notional_usd);
            hash_f64(&mut hasher, cost.expected_bps);
            hash_f64(&mut hasher, cost.tail_bps);
        }
        for value in [
            horizon.liquidation_notional_range.minimum,
            horizon.liquidation_notional_range.lower,
            horizon.liquidation_notional_range.median,
            horizon.liquidation_notional_range.upper,
            horizon.liquidation_notional_range.maximum,
            horizon.open_interest_range.minimum,
            horizon.open_interest_range.lower,
            horizon.open_interest_range.median,
            horizon.open_interest_range.upper,
            horizon.open_interest_range.maximum,
        ] {
            hash_f64(&mut hasher, value);
        }
    }
    hash_u64(&mut hasher, result.representative_paths.len() as u64);
    for path in &result.representative_paths {
        hasher.update(&[path.label as u8]);
        hash_f64(&mut hasher, path.selection_quantile);
        hasher.update(&path.source_path_index.to_le_bytes());
        hash_u64(&mut hasher, path.points.len() as u64);
        for point in &path.points {
            hash_u64(&mut hasher, point.elapsed_seconds);
            hash_f64(&mut hasher, point.simulated_price);
            hash_f64(&mut hasher, point.price_return);
            hash_f64(&mut hasher, point.volatility_annualized);
            hash_f64(&mut hasher, point.cumulative_liquidation_usd);
            hash_f64(&mut hasher, point.open_interest_usd);
        }
    }
    hasher.update(&[result.assumptions.path_label as u8]);
    for value in [
        &result.assumptions.rng_algorithm,
        &result.assumptions.quantile_method,
        &result.assumptions.innovation_method,
        &result.assumptions.process_model_version,
        &result.assumptions.impact_model_version,
    ] {
        hash_string(&mut hasher, value);
    }
    hasher.update(&[u8::from(result.assumptions.options_used)]);
    match &result.assumptions.options_unavailable_reason {
        Some(value) => {
            hasher.update(&[1]);
            hash_string(&mut hasher, value);
        }
        None => {
            hasher.update(&[0]);
        }
    };
    hasher.update(&[u8::from(result.assumptions.liquidation_amplification)]);
    for values in [
        &result.assumptions.stress_ids,
        &result.assumptions.model_package_ids,
    ] {
        hash_u64(&mut hasher, values.len() as u64);
        for value in values {
            hash_string(&mut hasher, value);
        }
    }
    hasher.update(&result.condition_evidence_hash);
    hasher.update(&result.config_evidence_hash);
    hasher.update(&result.engine_evidence_hash);
    hasher.update(&result.path_digest);
    *hasher.finalize().as_bytes()
}

pub(crate) fn identifier_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

pub(crate) fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hash_u64(hasher, value.len() as u64);
    hasher.update(value.as_bytes());
}

pub(crate) fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}

pub(crate) fn hash_f64(hasher: &mut blake3::Hasher, value: f64) {
    hasher.update(&value.to_bits().to_le_bytes());
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ScenarioError {
    #[error("empirical innovations are invalid or outside capacity")]
    InvalidInnovations,
    #[error("market-process parameters or identity are invalid")]
    InvalidProcess,
    #[error("liquidity state is invalid")]
    InvalidLiquidity,
    #[error("impact model is invalid")]
    InvalidImpactModel,
    #[error("options condition is invalid")]
    InvalidOptionsCondition,
    #[error("scenario condition is invalid or future-known")]
    InvalidCondition,
    #[error("event threshold is invalid")]
    InvalidThreshold,
    #[error("stress injection is invalid")]
    InvalidStress,
    #[error("scenario configuration is invalid")]
    InvalidConfig,
    #[error("scenario engine is invalid")]
    InvalidEngine,
    #[error("scenario summary is invalid")]
    InvalidSummary,
    #[error("scenario result is invalid or semantically inconsistent")]
    InvalidResult,
    #[error("serialized artifact identity is invalid")]
    InvalidArtifact,
    #[error("scenario work or allocation exceeds the declared bound")]
    CapacityExceeded,
    #[error("scenario arithmetic became non-finite")]
    NonFiniteArithmetic,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (ScenarioEngine, ScenarioCondition, ScenarioConfig) {
        let innovations = EmpiricalInnovations::try_new(
            vec![-2.0, -1.0, 0.0, 1.0, 2.0],
            vec![-0.05, 0.04],
            [1; 32],
        )
        .unwrap();
        let process = MarketProcess::try_new(
            ProcessParameters {
                calibration_step_seconds: 60,
                long_run_volatility_annualized: 0.60,
                volatility_persistence: 0.90,
                volatility_of_volatility: 0.05,
                base_jump_probability_per_step: 0.01,
                regime_jump_multiplier: 1.0,
                transition_jump_multiplier: 1.0,
                cusp_jump_multiplier: 1.0,
                systemic_correlation: 0.20,
                minimum_volatility_annualized: 0.05,
                maximum_volatility_annualized: 3.0,
                maximum_absolute_step_return: 0.25,
            },
            innovations,
            "process-test-v1",
            [2; 32],
        )
        .unwrap();
        let impact = ImpactModel::try_new(
            ImpactParameters {
                square_root_impact_bps: 5.0,
                fragility_multiplier: 1.0,
                volatility_multiplier: 0.5,
                maximum_sweep_cost_bps: 1_000.0,
                liquidation_trigger_return: 0.02,
                liquidation_sensitivity: 0.10,
                liquidation_price_feedback: 0.25,
            },
            "impact-test-v1",
            [3; 32],
        )
        .unwrap();
        let engine = ScenarioEngine::try_new(process, impact).unwrap();
        let condition = ScenarioCondition::try_new(ScenarioConditionInput {
            as_of_ns: 1_000,
            as_known_at_ns: 900,
            entity_id: "btc".to_owned(),
            starting_price: 100.0,
            starting_volatility_annualized: 0.60,
            regime_probability: 0.50,
            calibrated_transition_probability: 0.20,
            cusp_instability: 0.40,
            fast_stress: 0.10,
            systemic_return_condition: 0.0,
            liquidity: LiquiditySnapshot::try_new(5_000_000.0, 0.25, 100_000_000.0, 900, [4; 32])
                .unwrap(),
            options: OptionsCondition::unavailable("not_configured").unwrap(),
            forecast_evidence_hash: [5; 32],
            applicability_evidence_hash: [6; 32],
            source_evidence_hash: [7; 32],
            model_package_ids: vec!["forecast-test-v1".to_owned()],
        })
        .unwrap();
        let config = ScenarioConfig::try_new(ScenarioConfigInput {
            seed: 42,
            path_count: 32,
            step_seconds: 60,
            horizons_seconds: vec![300],
            thresholds: vec![
                EventThreshold::try_new("down", CrossingDirection::Below, -0.05).unwrap(),
            ],
            sweep_notionals_usd: vec![100_000.0],
            representative_quantiles: vec![0.50],
            liquidation_amplification: true,
            stress_injections: vec![],
        })
        .unwrap();
        (engine, condition, config)
    }

    #[test]
    fn verification_rederives_semantics_after_a_valid_checksum_is_recomputed() {
        let (engine, condition, config) = fixture();
        let mut result = engine.simulate(&condition, &config).unwrap();
        let probability = &mut result.horizons[0].threshold_probabilities[0].probability;
        *probability = if *probability == 0.25 { 0.50 } else { 0.25 };
        result.evidence_hash = result.calculate_hash();

        assert!(
            result
                .validate_identity(&engine, &condition, &config)
                .is_ok()
        );
        assert_eq!(
            result.verify(&engine, &condition, &config),
            Err(ScenarioError::InvalidResult)
        );
    }

    #[test]
    fn unknown_serialized_fields_and_stale_checksums_are_rejected() {
        let (engine, condition, config) = fixture();
        let result = engine.simulate(&condition, &config).unwrap();
        let mut encoded = serde_json::to_value(&result).unwrap();
        encoded
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_owned(), serde_json::json!(true));
        assert!(serde_json::from_value::<ScenarioDistribution>(encoded).is_err());

        let mut encoded = serde_json::to_value(&result).unwrap();
        encoded["path_count"] = serde_json::json!(31);
        let stale: ScenarioDistribution = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            stale.verify(&engine, &condition, &config),
            Err(ScenarioError::InvalidResult)
        );
    }
}
