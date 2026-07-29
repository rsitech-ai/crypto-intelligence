//! Closed registry catalogue for production price/return/volatility recipes.

use blake3::Hasher;
use feature_registry::{
    DurationNanos, EntityScope, EventTimePolicy, FeatureConsumptionRole, FeatureDefinition,
    FeatureDefinitionInput, FeatureDocumentation, FeatureId, FeatureStatus, FeatureValueType,
    FormulaHash, InputRequirement, MissingnessPolicy, NormalizationKind, NormalizationPolicy,
    QualityRequirement, QualityScore, RegistryError, WindowDefinition, WindowId, WindowKind,
};
use quality::SourceHealthState;
use semver::Version;

use crate::TimeWindow;

use super::FeatureComputationError;

const ONE_MINUTE: u64 = 60_000_000_000;
const FIVE_MINUTES: u64 = 5 * ONE_MINUTE;
const FIFTEEN_MINUTES: u64 = 15 * ONE_MINUTE;
const ONE_HOUR: u64 = 60 * ONE_MINUTE;
const ONE_DAY: u64 = 24 * ONE_HOUR;
const FIVE_SECONDS: u64 = 5_000_000_000;

/// Every required output in production specification sections 19.1 and 19.2.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Task4FeatureKind {
    LogReturn,
    CumulativeReturn,
    HighLowRange,
    OpenCloseRange,
    RollingVwapDistance,
    AnchoredVwapDistance,
    ConsolidatedFairPriceDistance,
    TrendSlope,
    TrendAcceleration,
    MaximumDrawdown,
    MaximumRunUp,
    UpsideSemivariance,
    DownsideSemivariance,
    ReturnAutocorrelation,
    VarianceRatio,
    RollingSkewness,
    RollingExcessKurtosis,
    ReturnReversal,
    FinalizedIntervalGap,
    RealizedVariance,
    RealizedVolatility,
    ExponentiallyWeightedVolatility,
    ParkinsonVolatility,
    GarmanKlassVolatility,
    BipowerVariation,
    JumpVariation,
    VolatilityOfVolatility,
    VolatilityTermRatio,
    SeasonalityAdjustedVolatility,
    RealizedCovariance,
    RealizedCorrelation,
    VolatilityForecastResidual,
}

impl Task4FeatureKind {
    pub const ALL: [Self; 32] = [
        Self::LogReturn,
        Self::CumulativeReturn,
        Self::HighLowRange,
        Self::OpenCloseRange,
        Self::RollingVwapDistance,
        Self::AnchoredVwapDistance,
        Self::ConsolidatedFairPriceDistance,
        Self::TrendSlope,
        Self::TrendAcceleration,
        Self::MaximumDrawdown,
        Self::MaximumRunUp,
        Self::UpsideSemivariance,
        Self::DownsideSemivariance,
        Self::ReturnAutocorrelation,
        Self::VarianceRatio,
        Self::RollingSkewness,
        Self::RollingExcessKurtosis,
        Self::ReturnReversal,
        Self::FinalizedIntervalGap,
        Self::RealizedVariance,
        Self::RealizedVolatility,
        Self::ExponentiallyWeightedVolatility,
        Self::ParkinsonVolatility,
        Self::GarmanKlassVolatility,
        Self::BipowerVariation,
        Self::JumpVariation,
        Self::VolatilityOfVolatility,
        Self::VolatilityTermRatio,
        Self::SeasonalityAdjustedVolatility,
        Self::RealizedCovariance,
        Self::RealizedCorrelation,
        Self::VolatilityForecastResidual,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::LogReturn => "log_return",
            Self::CumulativeReturn => "cumulative_return",
            Self::HighLowRange => "high_low_range",
            Self::OpenCloseRange => "open_close_range",
            Self::RollingVwapDistance => "rolling_vwap_distance",
            Self::AnchoredVwapDistance => "anchored_vwap_distance",
            Self::ConsolidatedFairPriceDistance => "consolidated_fair_price_distance",
            Self::TrendSlope => "trend_slope",
            Self::TrendAcceleration => "trend_acceleration",
            Self::MaximumDrawdown => "maximum_drawdown",
            Self::MaximumRunUp => "maximum_run_up",
            Self::UpsideSemivariance => "upside_semivariance",
            Self::DownsideSemivariance => "downside_semivariance",
            Self::ReturnAutocorrelation => "return_autocorrelation",
            Self::VarianceRatio => "variance_ratio",
            Self::RollingSkewness => "rolling_skewness",
            Self::RollingExcessKurtosis => "rolling_excess_kurtosis",
            Self::ReturnReversal => "return_reversal",
            Self::FinalizedIntervalGap => "finalized_interval_gap",
            Self::RealizedVariance => "realized_variance",
            Self::RealizedVolatility => "realized_volatility",
            Self::ExponentiallyWeightedVolatility => "exponentially_weighted_volatility",
            Self::ParkinsonVolatility => "parkinson_volatility",
            Self::GarmanKlassVolatility => "garman_klass_volatility",
            Self::BipowerVariation => "bipower_variation",
            Self::JumpVariation => "jump_variation",
            Self::VolatilityOfVolatility => "volatility_of_volatility",
            Self::VolatilityTermRatio => "volatility_term_ratio",
            Self::SeasonalityAdjustedVolatility => "seasonality_adjusted_volatility",
            Self::RealizedCovariance => "realized_covariance",
            Self::RealizedCorrelation => "realized_correlation",
            Self::VolatilityForecastResidual => "volatility_forecast_residual",
        }
    }

    pub const fn formula(self) -> &'static str {
        match self {
            Self::LogReturn => "ln(close_t/close_t-h)",
            Self::CumulativeReturn => "close_t/open_h-1",
            Self::HighLowRange => "high_h/low_h-1",
            Self::OpenCloseRange => "close_h/open_h-1",
            Self::RollingVwapDistance => "price/rolling-vwap-1;zero-size-ignored",
            Self::AnchoredVwapDistance => "price/utc-session-vwap-1;zero-size-ignored",
            Self::ConsolidatedFairPriceDistance => "price/consolidated-fair-price-1",
            Self::TrendSlope => "event-time-ols-price-slope",
            Self::TrendAcceleration => "event-time-ols-interval-slope-acceleration",
            Self::MaximumDrawdown => "max((running-peak-price)/running-peak)",
            Self::MaximumRunUp => "max((price-running-trough)/running-trough)",
            Self::UpsideSemivariance => "sum(return^2 where return>0)",
            Self::DownsideSemivariance => "sum(return^2 where return<0)",
            Self::ReturnAutocorrelation => "pearson(return_t,return_t-1);lag=1",
            Self::VarianceRatio => "var(overlapping-two-return-sums)/(2*var(return));horizon=2",
            Self::RollingSkewness => "population-third-central-moment/variance^1.5",
            Self::RollingExcessKurtosis => "population-fourth-central-moment/variance^2-3",
            Self::ReturnReversal => "mean(-sign(r_t)*r_t+1 where abs(r_t)>=0.05)",
            Self::FinalizedIntervalGap => "current-final-open/previous-final-close-1",
            Self::RealizedVariance => "sum(consecutive-log-return^2)",
            Self::RealizedVolatility => "sqrt(sum(consecutive-log-return^2));unannualized",
            Self::ExponentiallyWeightedVolatility => {
                "sqrt(time-aware-ewma-squared-return);half-life=5m;lookback=5m;initial=first-squared-return"
            }
            Self::ParkinsonVolatility => "sqrt(mean(log(high/low)^2)/(4*ln(2)))",
            Self::GarmanKlassVolatility => {
                "sqrt(mean(0.5*log(high/low)^2-(2*ln(2)-1)*log(close/open)^2));negative-mean-tolerance=f64-epsilon;clamp-small-negative-to-zero"
            }
            Self::BipowerVariation => "pi/2*sum(abs(r_t-1)*abs(r_t))",
            Self::JumpVariation => "max(realized-variance-bipower-variation,0)",
            Self::VolatilityOfVolatility => "sample-standard-deviation(realized-volatility)",
            Self::VolatilityTermRatio => {
                "short-horizon-volatility/long-horizon-volatility;pair=5m/15m-or-15m/1h"
            }
            Self::SeasonalityAdjustedVolatility => {
                "realized-volatility/point-in-time-time-of-week-factor;bucket-anchor=monday-00:00-utc"
            }
            Self::RealizedCovariance => "sum(aligned-log-return-products)",
            Self::RealizedCorrelation => {
                "sum(aligned-log-return-products)/sqrt(sum(left^2)*sum(right^2))"
            }
            Self::VolatilityForecastResidual => "realized-volatility-point-in-time-forecast",
        }
    }

    pub fn version(self) -> Version {
        if self == Self::RealizedVolatility {
            Version::new(2, 0, 0)
        } else {
            Version::new(1, 0, 0)
        }
    }

    pub fn formula_hash(self) -> FormulaHash {
        if self == Self::RealizedVolatility {
            return super::volatility::realized_volatility_v2_policy().formula_hash();
        }
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/task4-feature-formula/v1");
        hash_bytes(&mut hasher, self.id().as_bytes());
        hash_bytes(&mut hasher, self.version().to_string().as_bytes());
        hash_bytes(&mut hasher, self.formula().as_bytes());
        hasher.update(&ONE_MINUTE.to_be_bytes());
        hasher.update(&FIVE_MINUTES.to_be_bytes());
        hasher.update(&FIFTEEN_MINUTES.to_be_bytes());
        hasher.update(&ONE_HOUR.to_be_bytes());
        hasher.update(&(super::MAX_PRICE_OBSERVATIONS as u64).to_be_bytes());
        hasher.update(&(volatility::MAX_MEASURE_OBSERVATIONS as u64).to_be_bytes());
        FormulaHash::new(*hasher.finalize().as_bytes())
            .expect("domain-separated recipe hash is nonzero")
    }

    pub fn definition(self) -> Result<FeatureDefinition, RegistryError> {
        if self == Self::RealizedVolatility {
            return super::volatility::realized_volatility_v2_definition();
        }
        let version = self.version();
        FeatureDefinition::try_new(FeatureDefinitionInput {
            id: FeatureId::new(self.id())?,
            version: version.clone(),
            status: FeatureStatus::Required,
            consumption_role: FeatureConsumptionRole::ModelEligible,
            value_type: FeatureValueType::Float64,
            entities: if matches!(self, Self::RealizedCovariance | Self::RealizedCorrelation) {
                EntityScope::AssetPair
            } else {
                EntityScope::Asset
            },
            required_inputs: self
                .required_inputs()
                .iter()
                .map(|input| InputRequirement::new(*input))
                .collect::<Result<Vec<_>, _>>()?,
            event_time_policy: EventTimePolicy::SourceEventTime,
            windows: self.windows()?,
            output_resolution: DurationNanos::new(ONE_MINUTE),
            allowed_lateness: DurationNanos::new(FIVE_SECONDS),
            time_to_live: DurationNanos::new(ONE_DAY),
            normalization: NormalizationPolicy::try_new(NormalizationKind::None, version)?,
            missingness: MissingnessPolicy::Explicit,
            quality_gate: QualityRequirement::try_new(
                QualityScore::from_millionths(900_000)?,
                QualityScore::from_millionths(1_000_000)?,
                vec![SourceHealthState::Healthy, SourceHealthState::Recovering],
            )?,
            formula_hash: self.formula_hash(),
            documentation: FeatureDocumentation::new(format!(
                "docs/data-dictionary/features.md#{}",
                self.id().replace('_', "-")
            ))?,
        })
    }

    pub(crate) fn output_window_id(
        self,
        window: TimeWindow,
    ) -> Result<WindowId, FeatureComputationError> {
        if self == Self::ExponentiallyWeightedVolatility {
            let extent = window
                .end()
                .value()
                .checked_sub(window.start().value())
                .ok_or(FeatureComputationError::InvalidInput)?;
            if u64::try_from(extent).ok() != Some(FIVE_MINUTES) {
                return Err(FeatureComputationError::InvalidParameter);
            }
            return WindowId::new("ewma_5m").map_err(|_| FeatureComputationError::InvalidParameter);
        }
        if self == Self::AnchoredVwapDistance {
            let extent = window
                .end()
                .value()
                .checked_sub(window.start().value())
                .ok_or(FeatureComputationError::InvalidInput)?;
            if u64::try_from(extent).ok() != Some(ONE_DAY)
                || window.start().value().rem_euclid(ONE_DAY as i64) != 0
            {
                return Err(FeatureComputationError::InvalidParameter);
            }
            return WindowId::new("utc_day").map_err(|_| FeatureComputationError::InvalidParameter);
        }
        let extent = window
            .end()
            .value()
            .checked_sub(window.start().value())
            .ok_or(FeatureComputationError::InvalidInput)?;
        let id = match u64::try_from(extent).ok() {
            Some(FIVE_MINUTES) => "rolling_5m",
            Some(FIFTEEN_MINUTES) => "rolling_15m",
            Some(ONE_HOUR) => "rolling_1h",
            _ => return Err(FeatureComputationError::InvalidParameter),
        };
        WindowId::new(id).map_err(|_| FeatureComputationError::InvalidParameter)
    }

    fn required_inputs(self) -> &'static [&'static str] {
        match self {
            Self::RollingVwapDistance
            | Self::AnchoredVwapDistance
            | Self::ConsolidatedFairPriceDistance => {
                &["normalized.trades", "consolidated.fair_price"]
            }
            Self::ParkinsonVolatility | Self::GarmanKlassVolatility => &["consolidated.fair_price"],
            Self::ExponentiallyWeightedVolatility => &["feature.log_return"],
            Self::VolatilityOfVolatility => &["feature.realized_volatility"],
            Self::VolatilityTermRatio => &[
                "feature.realized_volatility.short",
                "feature.realized_volatility.long",
            ],
            Self::SeasonalityAdjustedVolatility => &[
                "feature.realized_volatility",
                "baseline.intraday_seasonality",
            ],
            Self::RealizedCovariance | Self::RealizedCorrelation => &[
                "consolidated.fair_price.left",
                "consolidated.fair_price.right",
            ],
            Self::VolatilityForecastResidual => {
                &["feature.realized_volatility", "forecast.volatility"]
            }
            Self::FinalizedIntervalGap => &["consolidated.finalized_interval"],
            _ => &["consolidated.fair_price"],
        }
    }

    fn windows(self) -> Result<Vec<WindowDefinition>, RegistryError> {
        if self == Self::ExponentiallyWeightedVolatility {
            return Ok(vec![WindowDefinition::try_new_exponentially_weighted(
                WindowId::new("ewma_5m")?,
                DurationNanos::new(FIVE_MINUTES),
            )?]);
        }
        if self == Self::AnchoredVwapDistance {
            return Ok(vec![WindowDefinition::try_new_session_aligned(
                WindowId::new("utc_day")?,
                DurationNanos::new(ONE_DAY),
                domain::UnixNanos::new(ONE_DAY as i64),
            )?]);
        }
        if self == Self::VolatilityTermRatio {
            return [("rolling_15m", FIFTEEN_MINUTES), ("rolling_1h", ONE_HOUR)]
                .into_iter()
                .map(|(id, extent)| {
                    WindowDefinition::try_new_time(
                        WindowId::new(id)?,
                        WindowKind::Sliding,
                        DurationNanos::new(extent),
                        Some(DurationNanos::new(ONE_MINUTE)),
                    )
                })
                .collect();
        }
        [
            ("rolling_5m", FIVE_MINUTES),
            ("rolling_15m", FIFTEEN_MINUTES),
            ("rolling_1h", ONE_HOUR),
        ]
        .into_iter()
        .map(|(id, extent)| {
            WindowDefinition::try_new_time(
                WindowId::new(id)?,
                WindowKind::Sliding,
                DurationNanos::new(extent),
                Some(DurationNanos::new(ONE_MINUTE)),
            )
        })
        .collect()
    }
}

pub fn task_four_definitions() -> Result<Vec<FeatureDefinition>, RegistryError> {
    Task4FeatureKind::ALL
        .into_iter()
        .map(Task4FeatureKind::definition)
        .collect()
}

fn hash_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}
