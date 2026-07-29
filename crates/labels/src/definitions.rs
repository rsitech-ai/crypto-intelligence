//! Validated, versioned market-transition label definitions.

use semver::Version;
use serde::Serialize;

use crate::LabelError;

const MAXIMUM_HORIZONS: usize = 64;
const MAXIMUM_HORIZON_SECONDS: u64 = 315_360_000;
const PRODUCTION_V1_HORIZONS_SECONDS: [u64; 4] = [900, 3_600, 14_400, 86_400];
const DEFINITION_HASH_DOMAIN: &[u8] = b"cmti:label-definition:v2\0";
const ONE_MILLION: u32 = 1_000_000;

/// Closed market-transition outcome family.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EventType {
    Downside,
    Upside,
    VolatilityExplosion,
    LiquidityVacuum,
    LiquidationCascade,
}

impl EventType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Downside => "downside",
            Self::Upside => "upside",
            Self::VolatilityExplosion => "volatility_explosion",
            Self::LiquidityVacuum => "liquidity_vacuum",
            Self::LiquidationCascade => "liquidation_cascade",
        }
    }
}

/// Whether a definition is the sealed v1 production contract or a research variant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefinitionScope {
    ProductionV1,
    Research,
}

impl DefinitionScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ProductionV1 => "production_v1",
            Self::Research => "research",
        }
    }
}

/// Policy for simultaneous or overlapping event families.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlapPolicy {
    AllowCompeting,
    EarliestWins,
    ExcludeTies,
}

impl OverlapPolicy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::AllowCompeting => "allow_competing",
            Self::EarliestWins => "earliest_wins",
            Self::ExcludeTies => "exclude_ties",
        }
    }
}

/// Downside or upside log-return threshold authority.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PriceThreshold {
    AbsoluteFraction(f64),
    VolatilityScaled {
        multiple: f64,
    },
    /// Requires the stricter of the absolute floor and volatility-scaled move.
    Combined {
        absolute_floor_fraction: f64,
        volatility_multiple: f64,
    },
}

/// Realized-volatility estimator declared by a volatility label.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VolatilityEstimator {
    LogReturnRms,
    RealizedVariance,
}

impl VolatilityEstimator {
    const fn as_str(self) -> &'static str {
        match self {
            Self::LogReturnRms => "log_return_rms",
            Self::RealizedVariance => "realized_variance",
        }
    }
}

/// Whether jumps are part of or separated from the volatility estimate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JumpTreatment {
    Included,
    Separated,
}

impl JumpTreatment {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Included => "included",
            Self::Separated => "separated",
        }
    }
}

/// Event-specific, closed label semantics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LabelRule {
    Downside {
        threshold: PriceThreshold,
    },
    Upside {
        threshold: PriceThreshold,
    },
    VolatilityExplosion {
        estimator: VolatilityEstimator,
        sampling_resolution_seconds: u64,
        reference_distribution_hash: [u8; 32],
        percentile_millionths: u32,
        reference_threshold: f64,
        minimum_multiple: f64,
        jump_treatment: JumpTreatment,
    },
    /// A systemic liquidity vacuum corroborated by multiple healthy sources.
    LiquidityVacuum {
        minimum_spread_multiple: f64,
        maximum_depth_fraction: f64,
        minimum_cancellation_multiple: f64,
        minimum_sweep_cost_multiple: f64,
        minimum_resiliency_multiple: f64,
        minimum_corroborating_sources: u32,
    },
    LiquidationCascade {
        price_threshold: PriceThreshold,
        minimum_liquidation_notional: f64,
        minimum_open_interest_drop_fraction: f64,
        minimum_spread_multiple: f64,
        maximum_depth_fraction: f64,
        minimum_corroborating_sources: u32,
        minimum_confidence_millionths: u32,
    },
}

impl LabelRule {
    pub const fn event_type(self) -> EventType {
        match self {
            Self::Downside { .. } => EventType::Downside,
            Self::Upside { .. } => EventType::Upside,
            Self::VolatilityExplosion { .. } => EventType::VolatilityExplosion,
            Self::LiquidityVacuum { .. } => EventType::LiquidityVacuum,
            Self::LiquidationCascade { .. } => EventType::LiquidationCascade,
        }
    }

    fn validate(self) -> Result<(), LabelError> {
        match self {
            Self::Downside { threshold } | Self::Upside { threshold } => {
                validate_price_threshold(threshold)
            }
            Self::VolatilityExplosion {
                sampling_resolution_seconds,
                reference_distribution_hash,
                percentile_millionths,
                reference_threshold,
                minimum_multiple,
                ..
            } => {
                if sampling_resolution_seconds == 0
                    || sampling_resolution_seconds > 86_400
                    || reference_distribution_hash == [0; 32]
                    || !(500_000..1_000_000).contains(&percentile_millionths)
                {
                    return Err(LabelError::InvalidDefinition);
                }
                validate_positive_bounded(reference_threshold, 100.0)?;
                validate_at_least_one_bounded(minimum_multiple, 100.0)
            }
            Self::LiquidityVacuum {
                minimum_spread_multiple,
                maximum_depth_fraction,
                minimum_cancellation_multiple,
                minimum_sweep_cost_multiple,
                minimum_resiliency_multiple,
                minimum_corroborating_sources,
            } => {
                validate_at_least_one_bounded(minimum_spread_multiple, 1_000.0)?;
                validate_fraction(maximum_depth_fraction)?;
                validate_at_least_one_bounded(minimum_cancellation_multiple, 1_000.0)?;
                validate_at_least_one_bounded(minimum_sweep_cost_multiple, 1_000.0)?;
                validate_at_least_one_bounded(minimum_resiliency_multiple, 1_000.0)?;
                validate_source_count(minimum_corroborating_sources)
            }
            Self::LiquidationCascade {
                price_threshold,
                minimum_liquidation_notional,
                minimum_open_interest_drop_fraction,
                minimum_spread_multiple,
                maximum_depth_fraction,
                minimum_corroborating_sources,
                minimum_confidence_millionths,
            } => {
                validate_price_threshold(price_threshold)?;
                validate_positive_bounded(minimum_liquidation_notional, 1.0e18)?;
                validate_fraction(minimum_open_interest_drop_fraction)?;
                validate_at_least_one_bounded(minimum_spread_multiple, 1_000.0)?;
                validate_fraction(maximum_depth_fraction)?;
                validate_source_count(minimum_corroborating_sources)?;
                validate_millionths(minimum_confidence_millionths)
            }
        }
    }
}

/// Construction input for a canonical definition.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelDefinitionInput {
    pub id: String,
    pub version: Version,
    pub scope: DefinitionScope,
    pub rule: LabelRule,
    pub horizons_seconds: Vec<u64>,
    pub minimum_duration_seconds: u64,
    pub minimum_quality_millionths: u32,
    pub minimum_source_coverage_millionths: u32,
    pub overlap_policy: OverlapPolicy,
}

/// Validated semantic identity for one label family.
#[derive(Clone, Debug, PartialEq)]
pub struct LabelDefinition {
    id: String,
    version: Version,
    scope: DefinitionScope,
    rule: LabelRule,
    horizons_seconds: Vec<u64>,
    minimum_duration_seconds: u64,
    minimum_quality_millionths: u32,
    minimum_source_coverage_millionths: u32,
    overlap_policy: OverlapPolicy,
    definition_hash: [u8; 32],
}

impl LabelDefinition {
    pub fn try_new(mut input: LabelDefinitionInput) -> Result<Self, LabelError> {
        if !valid_identifier(&input.id)
            || input.version.major == 0
            || !input.version.pre.is_empty()
            || !input.version.build.is_empty()
            || input.horizons_seconds.is_empty()
            || input.horizons_seconds.len() > MAXIMUM_HORIZONS
        {
            return Err(LabelError::InvalidDefinition);
        }
        input.rule.validate()?;
        validate_millionths(input.minimum_quality_millionths)?;
        validate_millionths(input.minimum_source_coverage_millionths)?;
        input.horizons_seconds.sort_unstable();
        let maximum_horizon = input
            .horizons_seconds
            .last()
            .copied()
            .ok_or(LabelError::InvalidDefinition)?;
        if input.horizons_seconds[0] == 0
            || maximum_horizon > MAXIMUM_HORIZON_SECONDS
            || input
                .horizons_seconds
                .windows(2)
                .any(|pair| pair[0] == pair[1])
            || input.minimum_duration_seconds > input.horizons_seconds[0]
            || (input.scope == DefinitionScope::ProductionV1
                && input.horizons_seconds != PRODUCTION_V1_HORIZONS_SECONDS)
        {
            return Err(LabelError::InvalidDefinition);
        }

        let definition_hash = hash_definition(&input)?;
        Ok(Self {
            id: input.id,
            version: input.version,
            scope: input.scope,
            rule: input.rule,
            horizons_seconds: input.horizons_seconds,
            minimum_duration_seconds: input.minimum_duration_seconds,
            minimum_quality_millionths: input.minimum_quality_millionths,
            minimum_source_coverage_millionths: input.minimum_source_coverage_millionths,
            overlap_policy: input.overlap_policy,
            definition_hash,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn version(&self) -> &Version {
        &self.version
    }

    pub const fn scope(&self) -> DefinitionScope {
        self.scope
    }

    pub const fn event_type(&self) -> EventType {
        self.rule.event_type()
    }

    pub const fn rule(&self) -> LabelRule {
        self.rule
    }

    pub fn horizons_seconds(&self) -> &[u64] {
        &self.horizons_seconds
    }

    pub const fn minimum_duration_seconds(&self) -> u64 {
        self.minimum_duration_seconds
    }

    pub const fn minimum_quality_millionths(&self) -> u32 {
        self.minimum_quality_millionths
    }

    pub const fn minimum_source_coverage_millionths(&self) -> u32 {
        self.minimum_source_coverage_millionths
    }

    pub const fn overlap_policy(&self) -> OverlapPolicy {
        self.overlap_policy
    }

    pub const fn definition_hash(&self) -> [u8; 32] {
        self.definition_hash
    }
}

fn validate_price_threshold(threshold: PriceThreshold) -> Result<(), LabelError> {
    match threshold {
        PriceThreshold::AbsoluteFraction(value) => validate_fraction(value),
        PriceThreshold::VolatilityScaled { multiple } => validate_positive_bounded(multiple, 100.0),
        PriceThreshold::Combined {
            absolute_floor_fraction,
            volatility_multiple,
        } => {
            validate_fraction(absolute_floor_fraction)?;
            validate_positive_bounded(volatility_multiple, 100.0)
        }
    }
}

fn validate_fraction(value: f64) -> Result<(), LabelError> {
    if value.is_finite() && value > 0.0 && value < 1.0 {
        Ok(())
    } else {
        Err(LabelError::InvalidDefinition)
    }
}

fn validate_positive_bounded(value: f64, maximum: f64) -> Result<(), LabelError> {
    if value.is_finite() && value > 0.0 && value <= maximum {
        Ok(())
    } else {
        Err(LabelError::InvalidDefinition)
    }
}

fn validate_at_least_one_bounded(value: f64, maximum: f64) -> Result<(), LabelError> {
    if value.is_finite() && value >= 1.0 && value <= maximum {
        Ok(())
    } else {
        Err(LabelError::InvalidDefinition)
    }
}

fn validate_source_count(value: u32) -> Result<(), LabelError> {
    if (2..=64).contains(&value) {
        Ok(())
    } else {
        Err(LabelError::InvalidDefinition)
    }
}

fn validate_millionths(value: u32) -> Result<(), LabelError> {
    if (1..=ONE_MILLION).contains(&value) {
        Ok(())
    } else {
        Err(LabelError::InvalidDefinition)
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[derive(Serialize)]
struct DefinitionHashWire<'a> {
    schema_version: u32,
    id: &'a str,
    version: String,
    scope: &'static str,
    event_type: &'static str,
    rule: RuleHashWire,
    horizons_seconds: &'a [u64],
    minimum_duration_seconds: u64,
    minimum_quality_millionths: u32,
    minimum_source_coverage_millionths: u32,
    overlap_policy: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RuleHashWire {
    Downside {
        threshold: ThresholdHashWire,
    },
    Upside {
        threshold: ThresholdHashWire,
    },
    VolatilityExplosion {
        estimator: &'static str,
        sampling_resolution_seconds: u64,
        reference_distribution_hash: [u8; 32],
        percentile_millionths: u32,
        reference_threshold_bits: u64,
        minimum_multiple_bits: u64,
        jump_treatment: &'static str,
    },
    LiquidityVacuum {
        minimum_spread_multiple_bits: u64,
        maximum_depth_fraction_bits: u64,
        minimum_cancellation_multiple_bits: u64,
        minimum_sweep_cost_multiple_bits: u64,
        minimum_resiliency_multiple_bits: u64,
        minimum_corroborating_sources: u32,
    },
    LiquidationCascade {
        price_threshold: ThresholdHashWire,
        minimum_liquidation_notional_bits: u64,
        minimum_open_interest_drop_fraction_bits: u64,
        minimum_spread_multiple_bits: u64,
        maximum_depth_fraction_bits: u64,
        minimum_corroborating_sources: u32,
        minimum_confidence_millionths: u32,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ThresholdHashWire {
    AbsoluteFraction {
        value_bits: u64,
    },
    VolatilityScaled {
        multiple_bits: u64,
    },
    Combined {
        absolute_floor_fraction_bits: u64,
        volatility_multiple_bits: u64,
    },
}

fn hash_definition(input: &LabelDefinitionInput) -> Result<[u8; 32], LabelError> {
    let wire = DefinitionHashWire {
        schema_version: 2,
        id: &input.id,
        version: input.version.to_string(),
        scope: input.scope.as_str(),
        event_type: input.rule.event_type().as_str(),
        rule: RuleHashWire::from(input.rule),
        horizons_seconds: &input.horizons_seconds,
        minimum_duration_seconds: input.minimum_duration_seconds,
        minimum_quality_millionths: input.minimum_quality_millionths,
        minimum_source_coverage_millionths: input.minimum_source_coverage_millionths,
        overlap_policy: input.overlap_policy.as_str(),
    };
    let bytes = serde_json::to_vec(&wire).map_err(|_| LabelError::Serialization)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(DEFINITION_HASH_DOMAIN);
    hasher.update(&bytes);
    Ok(*hasher.finalize().as_bytes())
}

impl From<PriceThreshold> for ThresholdHashWire {
    fn from(value: PriceThreshold) -> Self {
        match value {
            PriceThreshold::AbsoluteFraction(value) => Self::AbsoluteFraction {
                value_bits: value.to_bits(),
            },
            PriceThreshold::VolatilityScaled { multiple } => Self::VolatilityScaled {
                multiple_bits: multiple.to_bits(),
            },
            PriceThreshold::Combined {
                absolute_floor_fraction,
                volatility_multiple,
            } => Self::Combined {
                absolute_floor_fraction_bits: absolute_floor_fraction.to_bits(),
                volatility_multiple_bits: volatility_multiple.to_bits(),
            },
        }
    }
}

impl From<LabelRule> for RuleHashWire {
    fn from(value: LabelRule) -> Self {
        match value {
            LabelRule::Downside { threshold } => Self::Downside {
                threshold: threshold.into(),
            },
            LabelRule::Upside { threshold } => Self::Upside {
                threshold: threshold.into(),
            },
            LabelRule::VolatilityExplosion {
                estimator,
                sampling_resolution_seconds,
                reference_distribution_hash,
                percentile_millionths,
                reference_threshold,
                minimum_multiple,
                jump_treatment,
            } => Self::VolatilityExplosion {
                estimator: estimator.as_str(),
                sampling_resolution_seconds,
                reference_distribution_hash,
                percentile_millionths,
                reference_threshold_bits: reference_threshold.to_bits(),
                minimum_multiple_bits: minimum_multiple.to_bits(),
                jump_treatment: jump_treatment.as_str(),
            },
            LabelRule::LiquidityVacuum {
                minimum_spread_multiple,
                maximum_depth_fraction,
                minimum_cancellation_multiple,
                minimum_sweep_cost_multiple,
                minimum_resiliency_multiple,
                minimum_corroborating_sources,
            } => Self::LiquidityVacuum {
                minimum_spread_multiple_bits: minimum_spread_multiple.to_bits(),
                maximum_depth_fraction_bits: maximum_depth_fraction.to_bits(),
                minimum_cancellation_multiple_bits: minimum_cancellation_multiple.to_bits(),
                minimum_sweep_cost_multiple_bits: minimum_sweep_cost_multiple.to_bits(),
                minimum_resiliency_multiple_bits: minimum_resiliency_multiple.to_bits(),
                minimum_corroborating_sources,
            },
            LabelRule::LiquidationCascade {
                price_threshold,
                minimum_liquidation_notional,
                minimum_open_interest_drop_fraction,
                minimum_spread_multiple,
                maximum_depth_fraction,
                minimum_corroborating_sources,
                minimum_confidence_millionths,
            } => Self::LiquidationCascade {
                price_threshold: price_threshold.into(),
                minimum_liquidation_notional_bits: minimum_liquidation_notional.to_bits(),
                minimum_open_interest_drop_fraction_bits: minimum_open_interest_drop_fraction
                    .to_bits(),
                minimum_spread_multiple_bits: minimum_spread_multiple.to_bits(),
                maximum_depth_fraction_bits: maximum_depth_fraction.to_bits(),
                minimum_corroborating_sources,
                minimum_confidence_millionths,
            },
        }
    }
}
