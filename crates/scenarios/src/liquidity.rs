//! Versioned liquidity, sweep-cost, and liquidation feedback mechanics.

use serde::{Deserialize, Serialize};

use crate::{ScenarioError, hash_f64, hash_string, identifier_is_valid};

const LIQUIDITY_DOMAIN: &[u8] = b"cmti:scenario-liquidity:v1\0";
const IMPACT_DOMAIN: &[u8] = b"cmti:scenario-impact:v1\0";

/// Point-in-time liquidity and open-interest state.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiquiditySnapshot {
    depth_usd: f64,
    fragility: f64,
    open_interest_usd: f64,
    as_known_at_ns: i64,
    source_evidence_hash: [u8; 32],
    identity_hash: [u8; 32],
}

impl LiquiditySnapshot {
    pub fn try_new(
        depth_usd: f64,
        fragility: f64,
        open_interest_usd: f64,
        as_known_at_ns: i64,
        source_evidence_hash: [u8; 32],
    ) -> Result<Self, ScenarioError> {
        let mut value = Self {
            depth_usd,
            fragility,
            open_interest_usd,
            as_known_at_ns,
            source_evidence_hash,
            identity_hash: [0; 32],
        };
        value.validate_without_hash()?;
        value.identity_hash = value.calculate_hash();
        Ok(value)
    }

    pub const fn depth_usd(self) -> f64 {
        self.depth_usd
    }

    pub const fn fragility(self) -> f64 {
        self.fragility
    }

    pub const fn open_interest_usd(self) -> f64 {
        self.open_interest_usd
    }

    pub const fn as_known_at_ns(self) -> i64 {
        self.as_known_at_ns
    }

    pub const fn identity_hash(self) -> [u8; 32] {
        self.identity_hash
    }

    pub fn validate(self) -> Result<(), ScenarioError> {
        self.validate_without_hash()?;
        if self.identity_hash == self.calculate_hash() {
            Ok(())
        } else {
            Err(ScenarioError::InvalidArtifact)
        }
    }

    fn validate_without_hash(self) -> Result<(), ScenarioError> {
        if self.depth_usd.is_finite()
            && self.depth_usd > 0.0
            && self.fragility.is_finite()
            && (0.0..=1.0).contains(&self.fragility)
            && self.open_interest_usd.is_finite()
            && self.open_interest_usd > 0.0
            && self.as_known_at_ns > 0
            && self.source_evidence_hash != [0; 32]
        {
            Ok(())
        } else {
            Err(ScenarioError::InvalidLiquidity)
        }
    }

    fn calculate_hash(self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(LIQUIDITY_DOMAIN);
        hash_f64(&mut hasher, self.depth_usd);
        hash_f64(&mut hasher, self.fragility);
        hash_f64(&mut hasher, self.open_interest_usd);
        hasher.update(&self.as_known_at_ns.to_le_bytes());
        hasher.update(&self.source_evidence_hash);
        *hasher.finalize().as_bytes()
    }
}

/// Frozen liquidity-impact and liquidation-feedback parameters.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactParameters {
    pub square_root_impact_bps: f64,
    pub fragility_multiplier: f64,
    pub volatility_multiplier: f64,
    pub maximum_sweep_cost_bps: f64,
    pub liquidation_trigger_return: f64,
    pub liquidation_sensitivity: f64,
    pub liquidation_price_feedback: f64,
}

impl ImpactParameters {
    pub fn validate(self) -> Result<(), ScenarioError> {
        let finite = [
            self.square_root_impact_bps,
            self.fragility_multiplier,
            self.volatility_multiplier,
            self.maximum_sweep_cost_bps,
            self.liquidation_trigger_return,
            self.liquidation_sensitivity,
            self.liquidation_price_feedback,
        ]
        .into_iter()
        .all(f64::is_finite);
        if finite
            && self.square_root_impact_bps > 0.0
            && (0.0..=20.0).contains(&self.fragility_multiplier)
            && (0.0..=10.0).contains(&self.volatility_multiplier)
            && self.maximum_sweep_cost_bps >= self.square_root_impact_bps
            && self.maximum_sweep_cost_bps <= 10_000.0
            && (0.0..=0.50).contains(&self.liquidation_trigger_return)
            && self.liquidation_trigger_return > 0.0
            && (0.0..=1.0).contains(&self.liquidation_sensitivity)
            && (0.0..=2.0).contains(&self.liquidation_price_feedback)
        {
            Ok(())
        } else {
            Err(ScenarioError::InvalidImpactModel)
        }
    }
}

/// Frozen liquidity model used by a scenario engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpactModel {
    schema_version: u32,
    parameters: ImpactParameters,
    model_version: String,
    model_evidence_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl ImpactModel {
    pub fn try_new(
        parameters: ImpactParameters,
        model_version: impl Into<String>,
        model_evidence_hash: [u8; 32],
    ) -> Result<Self, ScenarioError> {
        let mut result = Self {
            schema_version: 1,
            parameters,
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
            && identifier_is_valid(&self.model_version)
            && self.model_evidence_hash != [0; 32]
        {
            Ok(())
        } else {
            Err(ScenarioError::InvalidImpactModel)
        }
    }

    pub(crate) fn apply_feedback(
        &self,
        raw_log_return: f64,
        volatility: f64,
        snapshot: LiquiditySnapshot,
        current_open_interest: f64,
        liquidity_multiplier: f64,
        liquidation_amplification: bool,
    ) -> Result<ImpactStep, ScenarioError> {
        let effective_depth = snapshot.depth_usd * liquidity_multiplier;
        let effective_fragility =
            (snapshot.fragility + (1.0 - liquidity_multiplier)).clamp(0.0, 1.0);
        if !effective_depth.is_finite() || effective_depth <= 0.0 {
            return Err(ScenarioError::NonFiniteArithmetic);
        }
        let amplification = 1.0
            + 0.10 * self.parameters.fragility_multiplier * effective_fragility
            + 0.02 * self.parameters.volatility_multiplier * volatility;
        let impacted = raw_log_return * amplification;
        let excess_downside = (-impacted - self.parameters.liquidation_trigger_return).max(0.0);
        let liquidated_notional = if liquidation_amplification {
            (current_open_interest * self.parameters.liquidation_sensitivity * excess_downside)
                .min(current_open_interest)
        } else {
            0.0
        };
        let feedback = if current_open_interest > 0.0 {
            self.parameters.liquidation_price_feedback * liquidated_notional / current_open_interest
        } else {
            0.0
        };
        let log_return = impacted - feedback;
        let open_interest = current_open_interest - liquidated_notional;
        if [log_return, liquidated_notional, open_interest]
            .into_iter()
            .all(f64::is_finite)
            && open_interest >= 0.0
        {
            Ok(ImpactStep {
                log_return,
                liquidated_notional,
                open_interest,
            })
        } else {
            Err(ScenarioError::NonFiniteArithmetic)
        }
    }

    pub(crate) fn sweep_cost_bps(
        &self,
        notional_usd: f64,
        volatility: f64,
        snapshot: LiquiditySnapshot,
        liquidity_multiplier: f64,
    ) -> Result<f64, ScenarioError> {
        let effective_depth = snapshot.depth_usd * liquidity_multiplier;
        let effective_fragility =
            (snapshot.fragility + (1.0 - liquidity_multiplier)).clamp(0.0, 1.0);
        let participation = notional_usd / effective_depth;
        let cost = self.parameters.square_root_impact_bps
            * participation.sqrt()
            * (1.0
                + self.parameters.fragility_multiplier * effective_fragility
                + self.parameters.volatility_multiplier * volatility);
        if cost.is_finite() && cost >= 0.0 {
            Ok(cost.min(self.parameters.maximum_sweep_cost_bps))
        } else {
            Err(ScenarioError::NonFiniteArithmetic)
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(IMPACT_DOMAIN);
        hasher.update(&self.schema_version.to_le_bytes());
        for value in [
            self.parameters.square_root_impact_bps,
            self.parameters.fragility_multiplier,
            self.parameters.volatility_multiplier,
            self.parameters.maximum_sweep_cost_bps,
            self.parameters.liquidation_trigger_return,
            self.parameters.liquidation_sensitivity,
            self.parameters.liquidation_price_feedback,
        ] {
            hash_f64(&mut hasher, value);
        }
        hash_string(&mut hasher, &self.model_version);
        hasher.update(&self.model_evidence_hash);
        *hasher.finalize().as_bytes()
    }
}

pub(crate) struct ImpactStep {
    pub log_return: f64,
    pub liquidated_notional: f64,
    pub open_interest: f64,
}
