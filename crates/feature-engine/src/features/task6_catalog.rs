//! Closed registry catalogue for Task 6 cross-venue, derivatives, and quality outputs.

use blake3::Hasher;
use feature_registry::{
    DurationNanos, EntityScope, EventTimePolicy, FeatureConsumptionRole, FeatureDefinition,
    FeatureDefinitionInput, FeatureDocumentation, FeatureId, FeatureStatus, FeatureValueType,
    FormulaHash, InputRequirement, MissingnessPolicy, NormalizationKind, NormalizationPolicy,
    QualityRequirement, QualityScore, RegistryError, WindowDefinition, WindowId, WindowKind,
};
use quality::SourceHealthState;
use semver::Version;

const ONE_SECOND: u64 = 1_000_000_000;
const ONE_MINUTE: u64 = 60 * ONE_SECOND;
const ONE_HOUR: u64 = 60 * ONE_MINUTE;
const ONE_DAY: u64 = 24 * ONE_HOUR;
const FIVE_SECONDS: u64 = 5 * ONE_SECOND;
const MODEL_MINIMUM_SCORE: u32 = 900_000;
const MODEL_MINIMUM_COVERAGE: u32 = 900_000;

const CROSS_VENUE_INPUTS: &[&str] = &[
    "consolidated.catalog_bound_book_state",
    "consolidated.eligible_source_universe",
];
const CROSS_VENUE_HISTORY_INPUTS: &[&str] = &[
    "consolidated.catalog_bound_book_state",
    "consolidated.eligible_source_universe",
    "window.finalization",
];
const VENUE_VOLUME_INPUTS: &[&str] = &[
    "connector.trade_normalization_receipt",
    "consolidated.eligible_source_universe",
    "window.finalization",
];
const EXECUTABLE_INPUTS: &[&str] = &[
    "execution.authenticated_depth_walk",
    "execution.contract_conversion",
    "execution.fee_schedule",
    "execution.latency_buffer",
    "execution.lot_and_tick",
    "execution.settlement_compatibility",
];
const DERIVATIVE_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "instrument.definition",
];
const DERIVATIVE_HISTORY_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "instrument.definition",
    "window.finalization",
];
const REALIZED_FUNDING_INPUTS: &[&str] = &[
    "connector.realized_funding_receipt",
    "instrument.definition",
    "window.finalization",
];
const CROSS_VENUE_FUNDING_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "consolidated.eligible_source_universe",
    "instrument.definition",
    "window.finalization",
];
const OPEN_INTEREST_USD_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "consolidated.point_in_time_quote_usd_conversion",
    "instrument.definition",
];
const OPEN_INTEREST_PRICE_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "consolidated.aligned_price_return",
    "instrument.definition",
    "window.finalization",
];
const BASIS_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "consolidated.aligned_reference_price",
    "instrument.definition",
];
const DATED_BASIS_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "consolidated.aligned_reference_price",
    "instrument.point_in_time_expiry",
];
const CURVE_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "consolidated.aligned_reference_price",
    "instrument.point_in_time_maturity_set",
    "window.finalization",
];
const LIQUIDATION_INPUTS: &[&str] = &[
    "connector.liquidation_normalization_receipt",
    "instrument.definition",
    "window.finalization",
];
const LIQUIDATION_VOLUME_INPUTS: &[&str] = &[
    "connector.liquidation_normalization_receipt",
    "connector.trade_normalization_receipt",
    "instrument.definition",
    "window.finalization",
];
const LIQUIDATION_OI_INPUTS: &[&str] = &[
    "connector.derivative_normalization_receipt",
    "connector.liquidation_normalization_receipt",
    "instrument.definition",
    "window.finalization",
];
const INSURANCE_INPUTS: &[&str] = &["connector.certified_insurance_fund_receipt"];
const ADL_INPUTS: &[&str] = &["connector.certified_adl_receipt"];
const LIQUIDATION_COMPLETENESS_INPUTS: &[&str] = &[
    "connector.liquidation_normalization_receipt",
    "window.finalization",
];
const QUALITY_INPUTS: &[&str] = &["collector.quality_gate_receipt"];
const CONSOLIDATED_QUALITY_INPUTS: &[&str] = &[
    "consolidated.catalog_bound_book_state",
    "consolidated.eligible_source_universe",
];
const CONSOLIDATED_QUALITY_FLOW_INPUTS: &[&str] = &[
    "consolidated.catalog_bound_book_state",
    "consolidated.eligible_source_universe",
    "window.finalization",
];
const STORAGE_PRESSURE_INPUTS: &[&str] = &["collector.storage_pressure_receipt"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecipeWindow {
    Snapshot,
    Flow,
    History,
}

impl RecipeWindow {
    const fn id(self) -> &'static str {
        match self {
            Self::Snapshot => "tumbling_1s",
            Self::Flow => "rolling_1m",
            Self::History => "rolling_1h",
        }
    }

    const fn extent(self) -> u64 {
        match self {
            Self::Snapshot => ONE_SECOND,
            Self::Flow => ONE_MINUTE,
            Self::History => ONE_HOUR,
        }
    }

    fn definition(self) -> Result<WindowDefinition, RegistryError> {
        WindowDefinition::try_new_time(
            WindowId::new(self.id())?,
            match self {
                Self::Snapshot => WindowKind::Tumbling,
                Self::Flow | Self::History => WindowKind::Sliding,
            },
            DurationNanos::new(self.extent()),
            match self {
                Self::Snapshot => None,
                Self::Flow | Self::History => Some(DurationNanos::new(ONE_SECOND)),
            },
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Task6FeatureRecipe {
    id: &'static str,
    formula: &'static str,
    value_type: FeatureValueType,
    entity: EntityScope,
    role: FeatureConsumptionRole,
    status: FeatureStatus,
    inputs: &'static [&'static str],
    window: RecipeWindow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Task6ComputationAvailability {
    Implemented,
    ExplicitlyUnavailable,
}

impl Task6FeatureRecipe {
    pub const fn id(self) -> &'static str {
        self.id
    }

    pub const fn formula_identity(self) -> &'static str {
        self.formula
    }

    pub const fn value_type(self) -> FeatureValueType {
        self.value_type
    }

    pub const fn entity(self) -> EntityScope {
        self.entity
    }

    pub const fn consumption_role(self) -> FeatureConsumptionRole {
        self.role
    }

    pub const fn status(self) -> FeatureStatus {
        self.status
    }

    pub fn computation_availability(self) -> Task6ComputationAvailability {
        match self.id {
            "venue_midprice_deviation"
            | "cross_venue_median_absolute_dispersion"
            | "indicative_cross_venue_price_range"
            | "venue_depth_share"
            | "venue_concentration_index"
            | "healthy_venue_fraction"
            | "predicted_funding_rate"
            | "funding_rate_change"
            | "open_interest_native"
            | "open_interest_relative_change"
            | "mark_index_divergence"
            | "liquidation_observed_count"
            | "liquidation_observed_notional"
            | "liquidation_observed_velocity"
            | "liquidation_source_completeness_flag"
            | "source_latency_ns"
            | "feed_jitter_ns"
            | "clock_skew_estimate_ns"
            | "sequence_gap_count"
            | "checksum_failure_count"
            | "reconnect_count"
            | "recovery_count"
            | "stale_quote_duration_ns"
            | "source_coverage_fraction"
            | "feature_age_ns"
            | "cross_source_disagreement"
            | "correction_count"
            | "revision_count"
            | "raw_to_normalized_rejection_count"
            | "disk_pressure_fraction"
            | "local_processing_lag_ns"
            | "source_outage_indicator"
            | "source_health_gate"
            | "source_completeness_gate"
            | "cascade_eligibility_gate" => Task6ComputationAvailability::Implemented,
            _ => Task6ComputationAvailability::ExplicitlyUnavailable,
        }
    }

    pub const fn required_inputs(self) -> &'static [&'static str] {
        self.inputs
    }

    pub fn window_id(self) -> Result<WindowId, RegistryError> {
        WindowId::new(self.window.id())
    }

    pub fn formula_hash(self) -> FormulaHash {
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/task6-cross-venue-derivatives-formula/v1");
        hash_bytes(&mut hasher, self.id.as_bytes());
        hash_bytes(&mut hasher, b"1.0.0");
        hash_bytes(&mut hasher, self.formula.as_bytes());
        hash_bytes(&mut hasher, self.window.id().as_bytes());
        hasher.update(&self.window.extent().to_be_bytes());
        hasher.update(&[value_type_tag(self.value_type)]);
        hasher.update(&[entity_tag(self.entity)]);
        hasher.update(&[role_tag(self.role)]);
        hasher.update(&[status_tag(self.status)]);
        hasher.update(&ONE_SECOND.to_be_bytes());
        hasher.update(&FIVE_SECONDS.to_be_bytes());
        hasher.update(&ONE_DAY.to_be_bytes());
        hash_bytes(&mut hasher, b"normalization:none@1.0.0");
        let (minimum_score, minimum_coverage, allowed_health) = self.quality_contract();
        hasher.update(&minimum_score.to_be_bytes());
        hasher.update(&minimum_coverage.to_be_bytes());
        hasher.update(&(allowed_health.len() as u64).to_be_bytes());
        for state in allowed_health {
            hasher.update(&[health_tag(*state)]);
        }
        hasher.update(&[match self.computation_availability() {
            Task6ComputationAvailability::Implemented => 1,
            Task6ComputationAvailability::ExplicitlyUnavailable => 2,
        }]);
        for input in self.inputs {
            hash_bytes(&mut hasher, input.as_bytes());
            hash_bytes(&mut hasher, b"required-input-contract-v1");
        }
        hash_bytes(
            &mut hasher,
            b"completeness-is-source-bound-lineage-not-observed-source-count",
        );
        match self.computation_availability() {
            Task6ComputationAvailability::Implemented => hash_bytes(
                &mut hasher,
                b"typed-emitter-quality-is-minimum-validated-input-score;flags-and-completeness-are-lineage-bound;registry-gate-is-exact",
            ),
            Task6ComputationAvailability::ExplicitlyUnavailable => hash_bytes(
                &mut hasher,
                b"present-emission-forbidden-until-authority-and-parameter-contract-is-implemented",
            ),
        }
        FormulaHash::new(*hasher.finalize().as_bytes())
            .expect("domain-separated Task 6 formula hash is nonzero")
    }

    pub(crate) fn definition(self) -> Result<FeatureDefinition, RegistryError> {
        let version = Version::new(1, 0, 0);
        let (minimum_score, minimum_coverage, allowed_health) = self.quality_contract();
        FeatureDefinition::try_new(FeatureDefinitionInput {
            id: FeatureId::new(self.id)?,
            version: version.clone(),
            status: self.status,
            consumption_role: self.role,
            value_type: self.value_type,
            entities: self.entity,
            required_inputs: self
                .inputs
                .iter()
                .map(|input| InputRequirement::new(*input))
                .collect::<Result<Vec<_>, _>>()?,
            event_time_policy: EventTimePolicy::SourceEventTime,
            windows: vec![self.window.definition()?],
            output_resolution: DurationNanos::new(ONE_SECOND),
            allowed_lateness: DurationNanos::new(FIVE_SECONDS),
            time_to_live: DurationNanos::new(ONE_DAY),
            normalization: NormalizationPolicy::try_new(NormalizationKind::None, version)?,
            missingness: MissingnessPolicy::Explicit,
            quality_gate: QualityRequirement::try_new(
                QualityScore::from_millionths(minimum_score)?,
                QualityScore::from_millionths(minimum_coverage)?,
                allowed_health.to_vec(),
            )?,
            formula_hash: self.formula_hash(),
            documentation: FeatureDocumentation::new(format!(
                "docs/data-dictionary/features.md#{}",
                self.id.replace(['_', '.'], "-")
            ))?,
        })
    }

    fn quality_contract(self) -> (u32, u32, &'static [SourceHealthState]) {
        match self.role {
            FeatureConsumptionRole::ModelEligible => (
                MODEL_MINIMUM_SCORE,
                MODEL_MINIMUM_COVERAGE,
                &[SourceHealthState::Healthy],
            ),
            FeatureConsumptionRole::UncertaintyOnly | FeatureConsumptionRole::GatingOnly => {
                (0, 0, &SourceHealthState::ALL)
            }
        }
    }
}

macro_rules! recipe {
    ($id:literal, $formula:literal, $value:ident, $entity:ident, $role:ident, $status:ident, $inputs:ident, $window:ident) => {
        Task6FeatureRecipe {
            id: $id,
            formula: $formula,
            value_type: FeatureValueType::$value,
            entity: EntityScope::$entity,
            role: FeatureConsumptionRole::$role,
            status: FeatureStatus::$status,
            inputs: $inputs,
            window: RecipeWindow::$window,
        }
    };
}

const TASK_SIX_RECIPES: [Task6FeatureRecipe; 63] = [
    recipe!(
        "venue_midprice_deviation",
        "venue-adjusted-price/consolidated-fair-price-1",
        Float64,
        AssetSource,
        ModelEligible,
        Required,
        CROSS_VENUE_INPUTS,
        Snapshot
    ),
    recipe!(
        "cross_venue_median_absolute_dispersion",
        "median(abs(venue-relative-to-fair));healthy-catalog-bound-books",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CROSS_VENUE_INPUTS,
        Snapshot
    ),
    recipe!(
        "indicative_cross_venue_price_range",
        "(max-adjusted-price-min-adjusted-price)/median-adjusted-price;non-executable",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CROSS_VENUE_INPUTS,
        Snapshot
    ),
    recipe!(
        "executable_price_dispersion",
        "max-net-sell-minus-min-net-buy-after-fees-conversion-settlement-lot-depth-latency",
        Float64,
        Asset,
        ModelEligible,
        Required,
        EXECUTABLE_INPUTS,
        Snapshot
    ),
    recipe!(
        "spot_perpetual_disagreement",
        "aligned-perpetual-fair/spot-fair-1;same-base-reference-asof",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CROSS_VENUE_INPUTS,
        Snapshot
    ),
    recipe!(
        "venue_lead_lag",
        "point-in-time-lagged-correlation-of-aligned-venue-returns",
        Float64,
        AssetSourcePair,
        ModelEligible,
        Required,
        CROSS_VENUE_HISTORY_INPUTS,
        History
    ),
    recipe!(
        "venue_volume_share",
        "venue-observed-volume/sum-eligible-venue-observed-volume",
        Float64,
        AssetSource,
        ModelEligible,
        Required,
        VENUE_VOLUME_INPUTS,
        Flow
    ),
    recipe!(
        "venue_depth_share",
        "venue-adjusted-reference-depth/sum-included-adjusted-reference-depth",
        Float64,
        AssetSource,
        ModelEligible,
        Required,
        CROSS_VENUE_INPUTS,
        Snapshot
    ),
    recipe!(
        "venue_concentration_index",
        "sum(square(venue-depth-share))",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CROSS_VENUE_INPUTS,
        Snapshot
    ),
    recipe!(
        "stale_quote_indicator",
        "collector-stale-quote-decision",
        Boolean,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "crossed_market_indicator",
        "trusted-book-crossed-or-locked-classification",
        Boolean,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "cross_venue_liquidity_synchronization",
        "correlation-of-aligned-venue-depth-changes",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CROSS_VENUE_HISTORY_INPUTS,
        History
    ),
    recipe!(
        "healthy_venue_fraction",
        "healthy-included-venues/point-in-time-eligible-venues",
        Float64,
        Asset,
        UncertaintyOnly,
        Required,
        CROSS_VENUE_INPUTS,
        Snapshot
    ),
    recipe!(
        "local_move_classifier_input",
        "venue-return-minus-cross-venue-robust-return",
        Float64,
        AssetSource,
        ModelEligible,
        Required,
        CROSS_VENUE_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "systemic_move_classifier_input",
        "healthy-venue-confirmation-fraction-for-common-direction",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CROSS_VENUE_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "transfer_friction_flag",
        "settlement-or-transfer-constraint-prevents-cross-venue-execution",
        Boolean,
        Asset,
        GatingOnly,
        Required,
        EXECUTABLE_INPUTS,
        Snapshot
    ),
    recipe!(
        "predicted_funding_rate",
        "connector-reported-predicted-funding-rate-for-next-funding-time;not-realized",
        FixedDecimal,
        Instrument,
        ModelEligible,
        Required,
        DERIVATIVE_INPUTS,
        Snapshot
    ),
    recipe!(
        "realized_funding_rate",
        "settled-funding-payment-rate-at-event-time",
        FixedDecimal,
        Instrument,
        ModelEligible,
        Required,
        REALIZED_FUNDING_INPUTS,
        Flow
    ),
    recipe!(
        "funding_rate_change",
        "current-funding-rate-minus-prior-funding-rate",
        FixedDecimal,
        Instrument,
        ModelEligible,
        Required,
        DERIVATIVE_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "funding_rate_percentile",
        "point-in-time-empirical-cdf-of-funding-rate",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        DERIVATIVE_HISTORY_INPUTS,
        History
    ),
    recipe!(
        "funding_rate_robust_zscore",
        "(funding-median)/(1.4826*mad);zero-mad-missing",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        DERIVATIVE_HISTORY_INPUTS,
        History
    ),
    recipe!(
        "funding_rate_mad",
        "median(abs(funding-median-funding))",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        DERIVATIVE_HISTORY_INPUTS,
        History
    ),
    recipe!(
        "cross_venue_funding_dispersion",
        "median(abs(venue-funding-median-funding))",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CROSS_VENUE_FUNDING_INPUTS,
        Flow
    ),
    recipe!(
        "open_interest_native",
        "connector-reported-native-open-interest",
        FixedDecimal,
        Instrument,
        ModelEligible,
        Required,
        DERIVATIVE_INPUTS,
        Snapshot
    ),
    recipe!(
        "open_interest_usd_notional",
        "instrument-quote-notional(open-interest,valuation-price)*point-in-time-quote-usd",
        FixedDecimal,
        Instrument,
        ModelEligible,
        Required,
        OPEN_INTEREST_USD_INPUTS,
        Snapshot
    ),
    recipe!(
        "open_interest_relative_change",
        "current-native-open-interest/prior-native-open-interest-1",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        DERIVATIVE_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "open_interest_change_conditioned_on_price",
        "open-interest-relative-change*sign(aligned-price-return)",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        OPEN_INTEREST_PRICE_INPUTS,
        Flow
    ),
    recipe!(
        "mark_index_divergence",
        "mark-price/index-price-1",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        DERIVATIVE_INPUTS,
        Snapshot
    ),
    recipe!(
        "perpetual_spot_basis",
        "perpetual-price/aligned-spot-price-1",
        Float64,
        Asset,
        ModelEligible,
        Required,
        BASIS_INPUTS,
        Snapshot
    ),
    recipe!(
        "dated_future_basis",
        "dated-future-price/aligned-reference-price-1",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        DATED_BASIS_INPUTS,
        Snapshot
    ),
    recipe!(
        "annualized_dated_future_basis",
        "dated-future-basis/(seconds-to-expiry/seconds-per-365-day-year)",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        DATED_BASIS_INPUTS,
        Snapshot
    ),
    recipe!(
        "futures_curve_slope",
        "delta-annualized-basis/delta-seconds-to-expiry;at-least-two-maturities",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CURVE_INPUTS,
        Snapshot
    ),
    recipe!(
        "futures_curve_curvature",
        "second-divided-difference-of-annualized-basis;at-least-three-maturities",
        Float64,
        Asset,
        ModelEligible,
        Required,
        CURVE_INPUTS,
        Snapshot
    ),
    recipe!(
        "liquidation_observed_count",
        "count-sealed-liquidation-receipts;coverage-qualified",
        Integer,
        Instrument,
        ModelEligible,
        Required,
        LIQUIDATION_INPUTS,
        Flow
    ),
    recipe!(
        "liquidation_observed_notional",
        "sum-instrument-quote-notional(sealed-liquidation-receipts);sampled-is-lower-bound",
        FixedDecimal,
        Instrument,
        ModelEligible,
        Required,
        LIQUIDATION_INPUTS,
        Flow
    ),
    recipe!(
        "liquidation_observed_velocity",
        "observed-liquidation-count/window-seconds;coverage-qualified",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        LIQUIDATION_INPUTS,
        Flow
    ),
    recipe!(
        "liquidation_observed_acceleration",
        "current-observed-velocity-minus-prior-observed-velocity-over-elapsed-seconds",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        LIQUIDATION_INPUTS,
        Flow
    ),
    recipe!(
        "liquidation_to_volume_ratio",
        "coverage-qualified-observed-liquidation-notional/aligned-traded-notional",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        LIQUIDATION_VOLUME_INPUTS,
        Flow
    ),
    recipe!(
        "liquidation_to_open_interest_ratio",
        "coverage-qualified-observed-liquidation-notional/aligned-open-interest-notional",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        LIQUIDATION_OI_INPUTS,
        Flow
    ),
    recipe!(
        "open_interest_destruction",
        "max(-open-interest-relative-change,0)*abs(aligned-price-return)",
        Float64,
        Instrument,
        ModelEligible,
        Required,
        OPEN_INTEREST_PRICE_INPUTS,
        Flow
    ),
    recipe!(
        "insurance_fund_state",
        "connector-reported-public-reliable-insurance-fund-state",
        FixedDecimal,
        Venue,
        UncertaintyOnly,
        Optional,
        INSURANCE_INPUTS,
        Snapshot
    ),
    recipe!(
        "adl_state",
        "connector-reported-public-reliable-adl-state",
        Integer,
        Venue,
        UncertaintyOnly,
        Optional,
        ADL_INPUTS,
        Snapshot
    ),
    recipe!(
        "liquidation_source_completeness_flag",
        "connector-and-window-bound-liquidation-completeness-class",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        LIQUIDATION_COMPLETENESS_INPUTS,
        Flow
    ),
    recipe!(
        "source_latency_ns",
        "collector-receive-wall-time-minus-source-event-time;nanoseconds",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "feed_jitter_ns",
        "point-in-time-feed-latency-absolute-change;nanoseconds",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "clock_skew_estimate_ns",
        "collector-clock-estimator-source-minus-local;nanoseconds",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "sequence_gap_count",
        "collector-verified-sequence-gaps-in-window",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "checksum_failure_count",
        "collector-verified-checksum-failures-in-window",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "reconnect_count",
        "collector-verified-source-reconnects-in-window",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "recovery_count",
        "collector-verified-successful-source-recoveries-in-window",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "stale_quote_duration_ns",
        "as-known-at-minus-last-trusted-quote-event-time;zero-when-not-stale",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "source_coverage_fraction",
        "observed-required-sources/point-in-time-required-source-universe",
        Float64,
        Asset,
        UncertaintyOnly,
        Required,
        CONSOLIDATED_QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "feature_age_ns",
        "as-known-at-minus-feature-event-time-end;nanoseconds",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "cross_source_disagreement",
        "robust-normalized-dispersion-across-aligned-healthy-sources",
        Float64,
        Asset,
        UncertaintyOnly,
        Required,
        CONSOLIDATED_QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "correction_count",
        "finalized-observation-corrections-in-window",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "revision_count",
        "source-or-feature-revisions-in-window",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "raw_to_normalized_rejection_count",
        "durable-raw-records-rejected-before-normalized-publication-in-window",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "disk_pressure_fraction",
        "used-capacity/validated-local-data-volume-capacity",
        Float64,
        Global,
        UncertaintyOnly,
        Required,
        STORAGE_PRESSURE_INPUTS,
        Snapshot
    ),
    recipe!(
        "local_processing_lag_ns",
        "feature-computed-at-minus-source-event-time-end;nanoseconds",
        Integer,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "source_outage_indicator",
        "collector-source-unhealthy-or-quarantined",
        Boolean,
        Source,
        UncertaintyOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "source_health_gate",
        "collector-owned-source-health-eligibility-decision",
        Boolean,
        Source,
        GatingOnly,
        Required,
        QUALITY_INPUTS,
        Snapshot
    ),
    recipe!(
        "source_completeness_gate",
        "collector-owned-gap-free-window-completeness-decision",
        Boolean,
        Source,
        GatingOnly,
        Required,
        QUALITY_INPUTS,
        Flow
    ),
    recipe!(
        "cascade_eligibility_gate",
        "all-required-source-health-and-completeness-gates-pass",
        Boolean,
        Asset,
        GatingOnly,
        Required,
        CONSOLIDATED_QUALITY_FLOW_INPUTS,
        Flow
    ),
];

pub const fn task_six_recipes() -> &'static [Task6FeatureRecipe] {
    &TASK_SIX_RECIPES
}

pub fn task_six_recipe(id: &str) -> Option<Task6FeatureRecipe> {
    TASK_SIX_RECIPES
        .iter()
        .copied()
        .find(|recipe| recipe.id == id)
}

pub fn task_six_definitions() -> Result<Vec<FeatureDefinition>, RegistryError> {
    TASK_SIX_RECIPES
        .into_iter()
        .map(Task6FeatureRecipe::definition)
        .collect()
}

const fn value_type_tag(value: FeatureValueType) -> u8 {
    match value {
        FeatureValueType::FixedDecimal => 1,
        FeatureValueType::FixedDecimalMap => 2,
        FeatureValueType::Float64 => 3,
        FeatureValueType::Integer => 4,
        FeatureValueType::Boolean => 5,
    }
}

const fn entity_tag(value: EntityScope) -> u8 {
    match value {
        EntityScope::Instrument => 1,
        EntityScope::Asset => 2,
        EntityScope::AssetPair => 3,
        EntityScope::Venue => 4,
        EntityScope::Source => 5,
        EntityScope::Global => 6,
        EntityScope::AssetSource => 7,
        EntityScope::AssetSourcePair => 8,
    }
}

const fn role_tag(value: FeatureConsumptionRole) -> u8 {
    match value {
        FeatureConsumptionRole::ModelEligible => 1,
        FeatureConsumptionRole::UncertaintyOnly => 2,
        FeatureConsumptionRole::GatingOnly => 3,
    }
}

const fn status_tag(value: FeatureStatus) -> u8 {
    match value {
        FeatureStatus::Required => 1,
        FeatureStatus::Optional => 2,
        FeatureStatus::Experimental => 3,
    }
}

const fn health_tag(value: SourceHealthState) -> u8 {
    match value {
        SourceHealthState::Healthy => 1,
        SourceHealthState::Degraded => 2,
        SourceHealthState::Unhealthy => 3,
        SourceHealthState::Quarantined => 4,
        SourceHealthState::Recovering => 5,
    }
}

fn hash_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}
