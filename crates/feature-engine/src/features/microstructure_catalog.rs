//! Closed registry catalogue for Task 5 market-microstructure outputs.

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
const FIFTEEN_MINUTES: u64 = 15 * ONE_MINUTE;
const ONE_DAY: u64 = 24 * 60 * ONE_MINUTE;
const FIVE_SECONDS: u64 = 5 * ONE_SECOND;
pub(crate) const BOOK_SHAPE_LEVEL_COUNT_V1: usize = 3;

const BOOK_INPUTS: &[&str] = &[
    "instrument.definition",
    "orderbook.trusted_l2",
    "quality.trusted_book_state",
];
const BOOK_HISTORY_INPUTS: &[&str] = &[
    "instrument.definition",
    "orderbook.trusted_l2",
    "quality.book_evidence_policy",
];
const BOOK_USD_INPUTS: &[&str] = &[
    "instrument.definition",
    "orderbook.trusted_l2",
    "quality.trusted_book_state",
    "reference.quote_to_usd",
];
const FLOW_INPUTS: &[&str] = &["aggressor.authority", "normalized.trades"];
const BOOK_POLICY_INPUTS: &[&str] = &[
    "instrument.definition",
    "orderbook.trusted_l2",
    "policy.microstructure_thresholds",
    "quality.trusted_book_state",
];
const FLOW_POLICY_INPUTS: &[&str] = &[
    "aggressor.authority",
    "normalized.trades",
    "policy.microstructure_thresholds",
];
const BOOK_FLOW_POLICY_INPUTS: &[&str] = &[
    "aggressor.authority",
    "normalized.trades",
    "orderbook.trusted_l2",
    "policy.microstructure_thresholds",
    "quality.book_evidence_policy",
];
const LIFECYCLE_INPUTS: &[&str] = &[
    "aggressor.authority",
    "lifecycle.certified_l3",
    "normalized.trades",
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum RecipeWindow {
    Snapshot,
    Flow,
    Recovery,
}

impl RecipeWindow {
    const fn identity(self) -> &'static str {
        match self {
            Self::Snapshot => "tumbling_1s",
            Self::Flow => "rolling_1m",
            Self::Recovery => "rolling_15m",
        }
    }

    const fn extent(self) -> u64 {
        match self {
            Self::Snapshot => ONE_SECOND,
            Self::Flow => ONE_MINUTE,
            Self::Recovery => FIFTEEN_MINUTES,
        }
    }

    fn definition(self) -> Result<WindowDefinition, RegistryError> {
        WindowDefinition::try_new_time(
            WindowId::new(self.identity())?,
            match self {
                Self::Snapshot => WindowKind::Tumbling,
                Self::Flow | Self::Recovery => WindowKind::Sliding,
            },
            DurationNanos::new(self.extent()),
            match self {
                Self::Snapshot => None,
                Self::Flow | Self::Recovery => Some(DurationNanos::new(ONE_SECOND)),
            },
        )
    }
}

/// One immutable Task 5 output recipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Task5FeatureRecipe {
    id: &'static str,
    formula: &'static str,
    value_type: FeatureValueType,
    inputs: &'static [&'static str],
    window: RecipeWindow,
}

impl Task5FeatureRecipe {
    pub const fn id(self) -> &'static str {
        self.id
    }

    pub const fn value_type(self) -> FeatureValueType {
        self.value_type
    }

    pub const fn formula_identity(self) -> &'static str {
        self.formula
    }

    pub fn formula_hash(self) -> FormulaHash {
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/task5-microstructure-formula/v1");
        hash_bytes(&mut hasher, self.id.as_bytes());
        hash_bytes(&mut hasher, b"1.0.0");
        hash_bytes(&mut hasher, self.formula.as_bytes());
        hash_bytes(&mut hasher, self.window.identity().as_bytes());
        hasher.update(&self.window.extent().to_be_bytes());
        hasher.update(&value_type_tag(self.value_type).to_be_bytes());
        for input in self.inputs {
            hash_bytes(&mut hasher, input.as_bytes());
        }
        hasher.update(&(super::orderbook::MAX_BOOK_SHAPE_LEVELS as u64).to_be_bytes());
        hasher.update(&(super::orderbook::MAX_RECOVERY_OBSERVATIONS as u64).to_be_bytes());
        hasher.update(&(super::orderflow::MAX_FLOW_TRADES as u64).to_be_bytes());
        FormulaHash::new(*hasher.finalize().as_bytes())
            .expect("domain-separated recipe hash is nonzero")
    }

    pub(crate) fn definition(self) -> Result<FeatureDefinition, RegistryError> {
        let version = Version::new(1, 0, 0);
        FeatureDefinition::try_new(FeatureDefinitionInput {
            id: FeatureId::new(self.id)?,
            version: version.clone(),
            status: FeatureStatus::Required,
            consumption_role: FeatureConsumptionRole::ModelEligible,
            value_type: self.value_type,
            entities: EntityScope::Instrument,
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
                QualityScore::from_millionths(900_000)?,
                QualityScore::from_millionths(1_000_000)?,
                vec![SourceHealthState::Healthy],
            )?,
            formula_hash: self.formula_hash(),
            documentation: FeatureDocumentation::new(format!(
                "docs/data-dictionary/features.md#{}",
                self.id.replace('_', "-")
            ))?,
        })
    }

    pub(crate) fn window_id(self) -> Result<WindowId, RegistryError> {
        WindowId::new(self.window.identity())
    }
}

macro_rules! recipe {
    ($id:literal, $formula:literal, $value_type:ident, $inputs:ident, $window:ident) => {
        Task5FeatureRecipe {
            id: $id,
            formula: $formula,
            value_type: FeatureValueType::$value_type,
            inputs: $inputs,
            window: RecipeWindow::$window,
        }
    };
}

const TASK_FIVE_RECIPES: [Task5FeatureRecipe; 77] = [
    recipe!(
        "absolute_spread",
        "ask-best-bid;exact",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "relative_spread",
        "absolute-spread/same-venue-midpoint",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_depth_1bps",
        "sum-bid-quantity-within-inclusive-1bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_depth_2bps",
        "sum-bid-quantity-within-inclusive-2bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_depth_5bps",
        "sum-bid-quantity-within-inclusive-5bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_depth_10bps",
        "sum-bid-quantity-within-inclusive-10bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_depth_25bps",
        "sum-bid-quantity-within-inclusive-25bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_depth_50bps",
        "sum-bid-quantity-within-inclusive-50bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_depth_100bps",
        "sum-bid-quantity-within-inclusive-100bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_depth_1bps",
        "sum-ask-quantity-within-inclusive-1bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_depth_2bps",
        "sum-ask-quantity-within-inclusive-2bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_depth_5bps",
        "sum-ask-quantity-within-inclusive-5bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_depth_10bps",
        "sum-ask-quantity-within-inclusive-10bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_depth_25bps",
        "sum-ask-quantity-within-inclusive-25bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_depth_50bps",
        "sum-ask-quantity-within-inclusive-50bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_depth_100bps",
        "sum-ask-quantity-within-inclusive-100bps",
        FixedDecimal,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "book_imbalance_1bps",
        "(bid-depth-ask-depth)/(bid-depth+ask-depth);1bps",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "book_imbalance_2bps",
        "(bid-depth-ask-depth)/(bid-depth+ask-depth);2bps",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "book_imbalance_5bps",
        "(bid-depth-ask-depth)/(bid-depth+ask-depth);5bps",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "book_imbalance_10bps",
        "(bid-depth-ask-depth)/(bid-depth+ask-depth);10bps",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "book_imbalance_25bps",
        "(bid-depth-ask-depth)/(bid-depth+ask-depth);25bps",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "book_imbalance_50bps",
        "(bid-depth-ask-depth)/(bid-depth+ask-depth);50bps",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "book_imbalance_100bps",
        "(bid-depth-ask-depth)/(bid-depth+ask-depth);100bps",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "weighted_midpoint",
        "quantity-weighted-best-bid-ask-midpoint",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "microprice",
        "(ask-price*bid-qty+bid-price*ask-qty)/(bid-qty+ask-qty)",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_book_slope",
        "quadratic-cumulative-depth;bid-linear-coefficient;first-3-displayed-levels",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_book_slope",
        "quadratic-cumulative-depth;ask-linear-coefficient;first-3-displayed-levels",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_book_convexity",
        "quadratic-cumulative-depth;bid-second-derivative;first-3-displayed-levels",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_book_convexity",
        "quadratic-cumulative-depth;ask-second-derivative;first-3-displayed-levels",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_liquidity_wall_distance",
        "nearest-bid-level-with-quote-notional-at-least-threshold;distance-bps",
        Float64,
        BOOK_POLICY_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_liquidity_wall_distance",
        "nearest-ask-level-with-quote-notional-at-least-threshold;distance-bps",
        Float64,
        BOOK_POLICY_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_price_level_gap_density",
        "missing-ticks/inside-to-outer-tick-span;bid",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_price_level_gap_density",
        "missing-ticks/inside-to-outer-tick-span;ask",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "expected_buy_sweep_cost_usd_10000",
        "consume-asks-to-10000-usd-using-point-in-time-quote-usd-conversion;relative-vwap-cost",
        Float64,
        BOOK_USD_INPUTS,
        Snapshot
    ),
    recipe!(
        "expected_sell_sweep_cost_usd_10000",
        "consume-bids-to-10000-usd-using-point-in-time-quote-usd-conversion;relative-vwap-cost",
        Float64,
        BOOK_USD_INPUTS,
        Snapshot
    ),
    recipe!(
        "maximum_executable_buy_quote_notional",
        "sum-ask-notional-within-inclusive-configured-slippage",
        FixedDecimal,
        BOOK_POLICY_INPUTS,
        Snapshot
    ),
    recipe!(
        "maximum_executable_sell_quote_notional",
        "sum-bid-notional-within-inclusive-configured-slippage",
        FixedDecimal,
        BOOK_POLICY_INPUTS,
        Snapshot
    ),
    recipe!(
        "book_quote_age_ms",
        "evaluation-time-minus-book-last-update-time;milliseconds",
        Integer,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_stale_side_duration_ns",
        "evaluation-time-minus-last-observed-bid-change",
        Integer,
        BOOK_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "ask_stale_side_duration_ns",
        "evaluation-time-minus-last-observed-ask-change",
        Integer,
        BOOK_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "bid_active_price_levels",
        "count-positive-displayed-bid-levels",
        Integer,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "ask_active_price_levels",
        "count-positive-displayed-ask-levels",
        Integer,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "local_book_entropy",
        "normalized-shannon-entropy-over-displayed-quantity",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "depth_concentration_by_level",
        "displayed-quantity-herfindahl-index",
        Float64,
        BOOK_INPUTS,
        Snapshot
    ),
    recipe!(
        "bid_depth_change",
        "current-minus-prior-exact-bid-depth;same-policy-session-band",
        FixedDecimal,
        BOOK_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "ask_depth_change",
        "current-minus-prior-exact-ask-depth;same-policy-session-band",
        FixedDecimal,
        BOOK_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "absolute_spread_change",
        "current-minus-prior-exact-absolute-spread;same-policy-session",
        FixedDecimal,
        BOOK_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "displayed_depth_replenishment_fraction",
        "(later-depth-post-depth)/(pre-depth-post-depth);passive-side-proxy;point-in-time-recovery-policy-v1",
        Float64,
        BOOK_FLOW_POLICY_INPUTS,
        Recovery
    ),
    recipe!(
        "displayed_depth_recovery_time_ns",
        "first-time-displayed-depth-recovers-100pct-within-15m;point-in-time-recovery-policy-v1",
        Integer,
        BOOK_FLOW_POLICY_INPUTS,
        Recovery
    ),
    recipe!(
        "book_half_life_ns",
        "first-time-displayed-depth-recovers-50pct-within-15m;point-in-time-recovery-policy-v1",
        Integer,
        BOOK_FLOW_POLICY_INPUTS,
        Recovery
    ),
    recipe!(
        "aggressive_buy_count",
        "count-authoritative-buy-aggressor-trades",
        Integer,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "aggressive_sell_count",
        "count-authoritative-sell-aggressor-trades",
        Integer,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "aggressive_buy_quantity",
        "sum-authoritative-buy-base-quantity",
        FixedDecimal,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "aggressive_sell_quantity",
        "sum-authoritative-sell-base-quantity",
        FixedDecimal,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "aggressive_buy_notional",
        "sum-authoritative-buy-quote-notional",
        FixedDecimal,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "aggressive_sell_notional",
        "sum-authoritative-sell-quote-notional",
        FixedDecimal,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "signed_trade_count_imbalance",
        "(buy-count-sell-count)/(buy-count+sell-count)",
        Float64,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "signed_trade_quantity_imbalance",
        "(buy-quantity-sell-quantity)/(buy-quantity+sell-quantity)",
        Float64,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "signed_trade_notional_imbalance",
        "(buy-notional-sell-notional)/(buy-notional+sell-notional)",
        Float64,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "top_of_book_ofi",
        "cont-kukanov-stoikov-best-quote-ofi;buy-pressure-positive",
        FixedDecimal,
        BOOK_HISTORY_INPUTS,
        Flow
    ),
    recipe!(
        "placement_rate_per_second",
        "certified-l3-place-count/window-seconds",
        Float64,
        LIFECYCLE_INPUTS,
        Flow
    ),
    recipe!(
        "cancellation_rate_per_second",
        "certified-l3-cancel-count/window-seconds",
        Float64,
        LIFECYCLE_INPUTS,
        Flow
    ),
    recipe!(
        "modification_rate_per_second",
        "certified-l3-modify-count/window-seconds",
        Float64,
        LIFECYCLE_INPUTS,
        Flow
    ),
    recipe!(
        "cancellation_to_trade_ratio",
        "certified-l3-cancel-count/aggressive-trade-count",
        Float64,
        LIFECYCLE_INPUTS,
        Flow
    ),
    recipe!(
        "quote_to_trade_ratio",
        "certified-l3-place-cancel-modify-count/aggressive-trade-count",
        Float64,
        LIFECYCLE_INPUTS,
        Flow
    ),
    recipe!(
        "trade_intensity_per_second",
        "authoritative-trade-count/window-seconds",
        Float64,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "trade_interarrival_coefficient_of_variation",
        "population-standard-deviation-interarrival/mean-interarrival",
        Float64,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "signed_volume_at_price",
        "sum(sign-aggressor*base-quantity)-by-exact-price",
        FixedDecimalMap,
        FLOW_INPUTS,
        Flow
    ),
    recipe!(
        "trade_shock_midprice_impact",
        "side-signed-mid-response-minus-pre-mid-over-pre-mid;point-in-time-threshold-policy-v1",
        Float64,
        BOOK_FLOW_POLICY_INPUTS,
        Flow
    ),
    recipe!(
        "trade_effective_spread",
        "quantity-weighted-2*side*(trade-price-pre-mid)/pre-mid",
        Float64,
        BOOK_FLOW_POLICY_INPUTS,
        Flow
    ),
    recipe!(
        "trade_realized_spread",
        "quantity-weighted-2*side*(trade-price-response-mid)/pre-mid",
        Float64,
        BOOK_FLOW_POLICY_INPUTS,
        Flow
    ),
    recipe!(
        "trade_adverse_selection_proxy",
        "effective-spread-minus-realized-spread",
        Float64,
        BOOK_FLOW_POLICY_INPUTS,
        Flow
    ),
    recipe!(
        "trade_print_sweep_direction",
        "(qualifying-buy-episodes-qualifying-sell-episodes)/all-qualifying-episodes",
        Float64,
        FLOW_POLICY_INPUTS,
        Flow
    ),
    recipe!(
        "large_trade_cluster_count",
        "count-time-clusters-with-at-least-two-fixed-threshold-large-trades",
        Integer,
        FLOW_POLICY_INPUTS,
        Flow
    ),
    recipe!(
        "clustered_large_trade_count",
        "count-prints-in-qualifying-large-trade-clusters",
        Integer,
        FLOW_POLICY_INPUTS,
        Flow
    ),
    recipe!(
        "clustered_large_trade_notional",
        "sum-exact-quote-notional-in-qualifying-large-trade-clusters",
        FixedDecimal,
        FLOW_POLICY_INPUTS,
        Flow
    ),
    recipe!(
        "large_trade_cluster_activity_rate",
        "qualifying-large-trade-cluster-count/window-seconds",
        Float64,
        FLOW_POLICY_INPUTS,
        Flow
    ),
];

pub const fn task_five_recipes() -> &'static [Task5FeatureRecipe] {
    &TASK_FIVE_RECIPES
}

pub(crate) fn task_five_recipe(id: &str) -> Option<Task5FeatureRecipe> {
    TASK_FIVE_RECIPES
        .iter()
        .copied()
        .find(|recipe| recipe.id == id)
}

pub fn task_five_definitions() -> Result<Vec<FeatureDefinition>, RegistryError> {
    TASK_FIVE_RECIPES
        .into_iter()
        .map(Task5FeatureRecipe::definition)
        .collect()
}

const fn value_type_tag(value_type: FeatureValueType) -> u64 {
    match value_type {
        FeatureValueType::FixedDecimal => 1,
        FeatureValueType::FixedDecimalMap => 2,
        FeatureValueType::Float64 => 3,
        FeatureValueType::Integer => 4,
        FeatureValueType::Boolean => 5,
    }
}

fn hash_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}
