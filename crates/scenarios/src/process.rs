//! Versioned deterministic market-process mechanics.

use serde::{Deserialize, Serialize};

use crate::{ScenarioError, hash_f64, hash_string, hash_u64, identifier_is_valid};

const INNOVATION_DOMAIN: &[u8] = b"cmti:scenario-innovations:v1\0";
const PROCESS_DOMAIN: &[u8] = b"cmti:scenario-process:v1\0";
const MAXIMUM_INNOVATIONS: usize = 65_536;
const SECONDS_PER_YEAR: f64 = 365.25 * 86_400.0;

/// Empirical standardized innovations and observed jump returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmpiricalInnovations {
    standardized: Vec<f64>,
    jumps: Vec<f64>,
    source_evidence_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl EmpiricalInnovations {
    /// Centers and sample-standardizes an empirical innovation vector.
    pub fn try_new(
        values: Vec<f64>,
        jumps: Vec<f64>,
        source_evidence_hash: [u8; 32],
    ) -> Result<Self, ScenarioError> {
        if !(5..=MAXIMUM_INNOVATIONS).contains(&values.len())
            || jumps.len() > MAXIMUM_INNOVATIONS
            || !values.iter().chain(&jumps).all(|value| value.is_finite())
            || jumps.iter().any(|value| value.abs() > 0.50)
            || source_evidence_hash == [0; 32]
        {
            return Err(ScenarioError::InvalidInnovations);
        }
        let mean = compensated_sum(&values) / values.len() as f64;
        let centered: Vec<f64> = values.iter().map(|value| value - mean).collect();
        let variance = compensated_squared_sum(&centered) / (values.len() - 1) as f64;
        let scale = variance.sqrt();
        if !scale.is_finite() || scale <= f64::EPSILON {
            return Err(ScenarioError::InvalidInnovations);
        }
        let standardized: Vec<f64> = centered.into_iter().map(|value| value / scale).collect();
        if !standardized.iter().all(|value| value.is_finite()) {
            return Err(ScenarioError::NonFiniteArithmetic);
        }
        let mut result = Self {
            standardized,
            jumps,
            source_evidence_hash,
            evidence_hash: [0; 32],
        };
        result.evidence_hash = result.calculate_hash();
        result.validate()?;
        Ok(result)
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub fn validate(&self) -> Result<(), ScenarioError> {
        let mean = compensated_sum(&self.standardized) / self.standardized.len().max(1) as f64;
        let variance = if self.standardized.len() > 1 {
            compensated_squared_deviation_sum(&self.standardized, mean)
                / (self.standardized.len() - 1) as f64
        } else {
            f64::NAN
        };
        if !(5..=MAXIMUM_INNOVATIONS).contains(&self.standardized.len())
            || self.jumps.len() > MAXIMUM_INNOVATIONS
            || !self
                .standardized
                .iter()
                .chain(&self.jumps)
                .all(|value| value.is_finite())
            || self.jumps.iter().any(|value| value.abs() > 0.50)
            || !mean.is_finite()
            || mean.abs() > 1e-10
            || !variance.is_finite()
            || (variance - 1.0).abs() > 1e-10
            || self.source_evidence_hash == [0; 32]
            || self.evidence_hash != self.calculate_hash()
        {
            Err(ScenarioError::InvalidInnovations)
        } else {
            Ok(())
        }
    }

    fn sample(&self, rng: &mut DeterministicRng) -> f64 {
        self.standardized[rng.index(self.standardized.len())]
    }

    fn sample_jump(&self, rng: &mut DeterministicRng) -> f64 {
        if self.jumps.is_empty() {
            0.0
        } else {
            self.jumps[rng.index(self.jumps.len())]
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(INNOVATION_DOMAIN);
        hasher.update(&self.source_evidence_hash);
        hash_u64(&mut hasher, self.standardized.len() as u64);
        for value in &self.standardized {
            hash_f64(&mut hasher, *value);
        }
        hash_u64(&mut hasher, self.jumps.len() as u64);
        for value in &self.jumps {
            hash_f64(&mut hasher, *value);
        }
        *hasher.finalize().as_bytes()
    }
}

/// Frozen stochastic-process parameters.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessParameters {
    pub calibration_step_seconds: u64,
    pub long_run_volatility_annualized: f64,
    pub volatility_persistence: f64,
    pub volatility_of_volatility: f64,
    pub base_jump_probability_per_step: f64,
    pub regime_jump_multiplier: f64,
    pub transition_jump_multiplier: f64,
    pub cusp_jump_multiplier: f64,
    pub systemic_correlation: f64,
    pub minimum_volatility_annualized: f64,
    pub maximum_volatility_annualized: f64,
    pub maximum_absolute_step_return: f64,
}

impl ProcessParameters {
    pub fn validate(self) -> Result<(), ScenarioError> {
        let finite = [
            self.long_run_volatility_annualized,
            self.volatility_persistence,
            self.volatility_of_volatility,
            self.base_jump_probability_per_step,
            self.regime_jump_multiplier,
            self.transition_jump_multiplier,
            self.cusp_jump_multiplier,
            self.systemic_correlation,
            self.minimum_volatility_annualized,
            self.maximum_volatility_annualized,
            self.maximum_absolute_step_return,
        ]
        .into_iter()
        .all(f64::is_finite);
        if finite
            && self.calibration_step_seconds > 0
            && self.long_run_volatility_annualized > 0.0
            && (0.0..1.0).contains(&self.volatility_persistence)
            && (0.0..=2.0).contains(&self.volatility_of_volatility)
            && (0.0..=0.50).contains(&self.base_jump_probability_per_step)
            && (0.0..=10.0).contains(&self.regime_jump_multiplier)
            && (0.0..=10.0).contains(&self.transition_jump_multiplier)
            && (0.0..=10.0).contains(&self.cusp_jump_multiplier)
            && (-0.99..=0.99).contains(&self.systemic_correlation)
            && self.minimum_volatility_annualized > 0.0
            && self.minimum_volatility_annualized <= self.long_run_volatility_annualized
            && self.long_run_volatility_annualized <= self.maximum_volatility_annualized
            && self.maximum_volatility_annualized <= 10.0
            && (0.0..=0.50).contains(&self.maximum_absolute_step_return)
            && self.maximum_absolute_step_return > 0.0
        {
            Ok(())
        } else {
            Err(ScenarioError::InvalidProcess)
        }
    }
}

/// A frozen process model with explicit empirical innovation evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketProcess {
    schema_version: u32,
    parameters: ProcessParameters,
    innovations: EmpiricalInnovations,
    model_version: String,
    model_evidence_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl MarketProcess {
    pub fn try_new(
        parameters: ProcessParameters,
        innovations: EmpiricalInnovations,
        model_version: impl Into<String>,
        model_evidence_hash: [u8; 32],
    ) -> Result<Self, ScenarioError> {
        let mut result = Self {
            schema_version: 1,
            parameters,
            innovations,
            model_version: model_version.into(),
            model_evidence_hash,
            evidence_hash: [0; 32],
        };
        result.validate_without_hash()?;
        result.evidence_hash = result.calculate_hash();
        Ok(result)
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub fn model_version(&self) -> &str {
        &self.model_version
    }

    pub(crate) const fn parameters(&self) -> ProcessParameters {
        self.parameters
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
            && self.parameters.validate().is_ok()
            && self.innovations.validate().is_ok()
            && identifier_is_valid(&self.model_version)
            && self.model_evidence_hash != [0; 32]
        {
            Ok(())
        } else {
            Err(ScenarioError::InvalidProcess)
        }
    }

    pub(crate) fn step(
        &self,
        rng: &mut DeterministicRng,
        input: ProcessStepInput,
    ) -> Result<ProcessStep, ScenarioError> {
        let parameters = self.parameters;
        if input.step_seconds != parameters.calibration_step_seconds {
            return Err(ScenarioError::InvalidConfig);
        }
        let idiosyncratic = self.innovations.sample(rng);
        let systemic = self.innovations.sample(rng);
        let independent_weight = (1.0 - parameters.systemic_correlation.powi(2)).sqrt();
        let innovation =
            independent_weight * idiosyncratic + parameters.systemic_correlation * systemic;
        let jump_probability = (parameters.base_jump_probability_per_step
            * (1.0
                + parameters.regime_jump_multiplier * input.regime_probability
                + parameters.transition_jump_multiplier * input.transition_probability
                + parameters.cusp_jump_multiplier * input.cusp_instability)
            + input.jump_probability_addition)
            .clamp(0.0, 1.0);
        let jump = if rng.unit_interval() < jump_probability {
            self.innovations.sample_jump(rng)
        } else {
            0.0
        };
        let step_scale = (input.step_seconds as f64 / SECONDS_PER_YEAR).sqrt();
        let effective_volatility = (input.current_volatility * input.volatility_multiplier).clamp(
            parameters.minimum_volatility_annualized,
            parameters.maximum_volatility_annualized,
        );
        let skew_adjustment = if innovation < 0.0 {
            1.0 - input.options_skew * 0.25
        } else {
            1.0 + input.options_skew * 0.25
        }
        .clamp(0.5, 1.5);
        let raw_return = effective_volatility * step_scale * innovation * skew_adjustment
            + jump
            + input.systemic_return_drift
            + input.return_stress;
        if !raw_return.is_finite() {
            return Err(ScenarioError::NonFiniteArithmetic);
        }
        let step_return = raw_return.clamp(
            -parameters.maximum_absolute_step_return,
            parameters.maximum_absolute_step_return,
        );
        let volatility_shock = parameters.volatility_of_volatility
            * parameters.long_run_volatility_annualized
            * step_scale
            * idiosyncratic;
        let next_volatility = (parameters.long_run_volatility_annualized
            + parameters.volatility_persistence
                * (input.current_volatility - parameters.long_run_volatility_annualized)
            + volatility_shock)
            .clamp(
                parameters.minimum_volatility_annualized,
                parameters.maximum_volatility_annualized,
            );
        if next_volatility.is_finite() {
            Ok(ProcessStep {
                log_return: step_return,
                volatility_annualized: next_volatility,
            })
        } else {
            Err(ScenarioError::NonFiniteArithmetic)
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(PROCESS_DOMAIN);
        hasher.update(&self.schema_version.to_le_bytes());
        hash_u64(&mut hasher, self.parameters.calibration_step_seconds);
        for value in [
            self.parameters.long_run_volatility_annualized,
            self.parameters.volatility_persistence,
            self.parameters.volatility_of_volatility,
            self.parameters.base_jump_probability_per_step,
            self.parameters.regime_jump_multiplier,
            self.parameters.transition_jump_multiplier,
            self.parameters.cusp_jump_multiplier,
            self.parameters.systemic_correlation,
            self.parameters.minimum_volatility_annualized,
            self.parameters.maximum_volatility_annualized,
            self.parameters.maximum_absolute_step_return,
        ] {
            hash_f64(&mut hasher, value);
        }
        hasher.update(&self.innovations.evidence_hash());
        hash_string(&mut hasher, &self.model_version);
        hasher.update(&self.model_evidence_hash);
        *hasher.finalize().as_bytes()
    }
}

pub(crate) struct ProcessStepInput {
    pub current_volatility: f64,
    pub step_seconds: u64,
    pub regime_probability: f64,
    pub transition_probability: f64,
    pub cusp_instability: f64,
    pub systemic_return_drift: f64,
    pub options_skew: f64,
    pub volatility_multiplier: f64,
    pub return_stress: f64,
    pub jump_probability_addition: f64,
}

pub(crate) struct ProcessStep {
    pub log_return: f64,
    pub volatility_annualized: f64,
}

/// SplitMix64 stream frozen as `splitmix64-v1` for deterministic local replay.
pub(crate) struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    pub const ALGORITHM: &'static str = "splitmix64-v1";

    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, length: usize) -> usize {
        (self.next_u64() % length as u64) as usize
    }

    fn unit_interval(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1_u64 << 53) as f64)
    }
}

fn compensated_sum(values: &[f64]) -> f64 {
    let mut sum = 0.0;
    let mut correction = 0.0;
    for value in values {
        let adjusted = value - correction;
        let next = sum + adjusted;
        correction = (next - sum) - adjusted;
        sum = next;
    }
    sum
}

fn compensated_squared_sum(values: &[f64]) -> f64 {
    let mut sum = 0.0;
    let mut correction = 0.0;
    for value in values {
        let squared = value * value;
        let adjusted = squared - correction;
        let next = sum + adjusted;
        correction = (next - sum) - adjusted;
        sum = next;
    }
    sum
}

fn compensated_squared_deviation_sum(values: &[f64], mean: f64) -> f64 {
    let mut sum = 0.0;
    let mut correction = 0.0;
    for value in values {
        let deviation = value - mean;
        let squared = deviation * deviation;
        let adjusted = squared - correction;
        let next = sum + adjusted;
        correction = (next - sum) - adjusted;
        sum = next;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::DeterministicRng;

    #[test]
    fn splitmix64_v1_stream_is_frozen() {
        let mut rng = DeterministicRng::new(0);
        assert_eq!(rng.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(rng.next_u64(), 0x6e78_9e6a_a1b9_65f4);
        assert_eq!(rng.next_u64(), 0x06c4_5d18_8009_454f);
    }

    #[test]
    fn unit_interval_is_half_open() {
        let mut rng = DeterministicRng::new(u64::MAX);
        for _ in 0..10_000 {
            let value = rng.unit_interval();
            assert!((0.0..1.0).contains(&value));
        }
    }
}
