//! Bounded point-in-time price and return feature calculations.

use std::collections::BTreeSet;

use blake3::Hasher;
use consolidated_market::FairPrice;
use domain::{AssetId, InstrumentDefinition, SourceId, SourceKind, UnixNanos};
use event_envelope::{EventEnvelope, UncheckedEventPayload};
use feature_registry::{FeatureEntity, QualityScore, SourceCoverage, SourceCoverageEntry};
use fixed_decimal::{Price, Quantity};

use super::{FeatureComputationError, Task4FeatureKind, finite_analytical};
use crate::TimeWindow;

pub const MAX_PRICE_OBSERVATIONS: usize = 65_536;
const MAX_HORIZONS: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceObservationInput {
    pub entity: FeatureEntity,
    pub price: Price,
    pub event_time: UnixNanos,
    pub as_known_at: UnixNanos,
    pub finalized: bool,
    pub lineage_hash: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceObservation {
    entity: FeatureEntity,
    price: Price,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    finalized: bool,
    lineage_hash: [u8; 32],
    consolidated_evidence: Option<ConsolidatedPriceEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConsolidatedPriceEvidence {
    source_coverage: SourceCoverage,
    quality_score: QualityScore,
}

impl PriceObservation {
    pub fn try_new(input: PriceObservationInput) -> Result<Self, FeatureComputationError> {
        if input.event_time.value() <= 0
            || input.as_known_at < input.event_time
            || input.lineage_hash == [0; 32]
        {
            return Err(FeatureComputationError::InvalidInput);
        }
        Ok(Self {
            entity: input.entity,
            price: input.price,
            event_time: input.event_time,
            as_known_at: input.as_known_at,
            finalized: input.finalized,
            lineage_hash: input.lineage_hash,
            consolidated_evidence: None,
        })
    }

    pub fn from_consolidated(fair_price: &FairPrice) -> Result<Self, FeatureComputationError> {
        let event_time = fair_price.event_time();
        let as_known_at = fair_price.lineage().as_of();
        if event_time.value() <= 0 || as_known_at < event_time {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        let source_coverage = source_coverage_from_fair_price(fair_price)?;
        let quality_score =
            QualityScore::from_millionths(fair_price.minimum_included_quality().value())
                .map_err(|_| FeatureComputationError::InvalidInput)?;
        Ok(Self {
            entity: FeatureEntity::Asset(fair_price.base_asset().clone()),
            price: fair_price.price(),
            event_time,
            as_known_at,
            finalized: false,
            lineage_hash: *fair_price.lineage().digest(),
            consolidated_evidence: Some(ConsolidatedPriceEvidence {
                source_coverage,
                quality_score,
            }),
        })
    }

    pub const fn price(&self) -> Price {
        self.price
    }

    pub const fn entity(&self) -> &FeatureEntity {
        &self.entity
    }

    pub const fn event_time(&self) -> UnixNanos {
        self.event_time
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn is_finalized(&self) -> bool {
        self.finalized
    }

    pub const fn lineage_hash(&self) -> &[u8; 32] {
        &self.lineage_hash
    }

    fn consolidated_evidence(&self) -> Option<&ConsolidatedPriceEvidence> {
        self.consolidated_evidence.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceWindow {
    time_window: TimeWindow,
    entity: FeatureEntity,
    as_of: UnixNanos,
    observations: Vec<PriceObservation>,
    input_lineage_digest: [u8; 32],
    finalization: Option<crate::FinalizationDecision>,
    source_coverage: Option<SourceCoverage>,
    quality_score: Option<QualityScore>,
}

impl PriceWindow {
    pub fn try_new(
        time_window: TimeWindow,
        entity: FeatureEntity,
        as_of: UnixNanos,
        observations: Vec<PriceObservation>,
    ) -> Result<Self, FeatureComputationError> {
        if observations.len() > MAX_PRICE_OBSERVATIONS {
            return Err(FeatureComputationError::CapacityExceeded);
        }
        if observations.is_empty() {
            return Err(FeatureComputationError::InsufficientHistory);
        }
        if as_of.value() <= 0 {
            return Err(FeatureComputationError::InvalidInput);
        }
        if observations.iter().any(|observation| {
            observation.event_time < time_window.start()
                || observation.event_time >= time_window.end()
        }) {
            return Err(FeatureComputationError::OutsideWindow);
        }
        if observations
            .iter()
            .any(|observation| observation.entity != entity)
        {
            return Err(FeatureComputationError::MixedEntity);
        }
        if observations
            .iter()
            .any(|observation| observation.as_known_at > as_of)
        {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        if observations
            .windows(2)
            .any(|pair| pair[0].event_time >= pair[1].event_time)
        {
            return Err(FeatureComputationError::NonMonotonicTime);
        }
        let mut unique_lineage = BTreeSet::new();
        if observations
            .iter()
            .any(|observation| !unique_lineage.insert(observation.lineage_hash))
        {
            return Err(FeatureComputationError::DuplicateLineage);
        }
        let input_lineage_digest =
            price_window_lineage_digest(time_window, &entity, as_of, &observations);
        Ok(Self {
            time_window,
            entity,
            as_of,
            observations,
            input_lineage_digest,
            finalization: None,
            source_coverage: None,
            quality_score: None,
        })
    }

    pub fn try_from_finalization(
        entity: FeatureEntity,
        tracker: &crate::WatermarkTracker,
        finalization: crate::FinalizationDecision,
        observations: Vec<PriceObservation>,
    ) -> Result<Self, FeatureComputationError> {
        if observations.len() > MAX_PRICE_OBSERVATIONS {
            return Err(FeatureComputationError::CapacityExceeded);
        }
        if observations.is_empty() {
            return Err(FeatureComputationError::InsufficientHistory);
        }
        let as_of = observations
            .iter()
            .map(PriceObservation::as_known_at)
            .max()
            .ok_or(FeatureComputationError::InsufficientHistory)?;
        let time_window = finalization.window();
        if !tracker.validates_decision(finalization, &entity) {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let mut window = Self::try_new(time_window, entity, as_of, observations)?;
        let (source_coverage, quality_score) =
            aggregate_consolidated_evidence(&window.observations)?;
        if !finalization.matches_required_sources(source_coverage.expected_sources()) {
            return Err(FeatureComputationError::UntrustedInput);
        }
        window.finalization = Some(finalization);
        window.source_coverage = Some(source_coverage);
        window.quality_score = Some(quality_score);
        Ok(window)
    }

    pub const fn time_window(&self) -> TimeWindow {
        self.time_window
    }

    pub const fn entity(&self) -> &FeatureEntity {
        &self.entity
    }

    pub const fn as_of(&self) -> UnixNanos {
        self.as_of
    }

    pub fn observations(&self) -> &[PriceObservation] {
        &self.observations
    }

    pub const fn input_lineage_digest(&self) -> [u8; 32] {
        self.input_lineage_digest
    }

    pub const fn finalization(&self) -> Option<crate::FinalizationDecision> {
        self.finalization
    }

    pub const fn source_coverage(&self) -> Option<&SourceCoverage> {
        self.source_coverage.as_ref()
    }

    pub const fn quality_score(&self) -> Option<QualityScore> {
        self.quality_score
    }

    pub fn validates_consolidated_closing_observation(
        &self,
        closing: &PriceObservation,
    ) -> Result<(), FeatureComputationError> {
        if closing.entity != self.entity
            || closing.event_time != self.time_window.end()
            || closing.as_known_at < self.as_of
        {
            return Err(FeatureComputationError::InvalidInput);
        }
        let closing_evidence = closing
            .consolidated_evidence()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        if Some(&closing_evidence.source_coverage) != self.source_coverage.as_ref() {
            return Err(FeatureComputationError::EligibleUniverseChanged);
        }
        Ok(())
    }

    pub fn closing_lineage_digest(
        &self,
        closing: &PriceObservation,
    ) -> Result<[u8; 32], FeatureComputationError> {
        self.validates_consolidated_closing_observation(closing)?;
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/price-window-with-close/v1");
        hasher.update(&self.input_lineage_digest);
        hasher.update(
            &self
                .finalization
                .ok_or(FeatureComputationError::UntrustedInput)?
                .evidence_digest(),
        );
        hasher.update(closing.lineage_hash());
        hasher.update(&closing.event_time.value().to_be_bytes());
        let price = closing.price.to_string();
        hasher.update(&(price.len() as u64).to_be_bytes());
        hasher.update(price.as_bytes());
        Ok(*hasher.finalize().as_bytes())
    }

    pub fn finalized_lineage_digest(&self) -> Result<[u8; 32], FeatureComputationError> {
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/finalized-price-window/v1");
        hasher.update(&self.input_lineage_digest);
        hasher.update(
            &self
                .finalization
                .ok_or(FeatureComputationError::UntrustedInput)?
                .evidence_digest(),
        );
        Ok(*hasher.finalize().as_bytes())
    }

    pub fn quality_with_closing(
        &self,
        closing: &PriceObservation,
    ) -> Result<QualityScore, FeatureComputationError> {
        self.validates_consolidated_closing_observation(closing)?;
        let window_quality = self
            .quality_score
            .ok_or(FeatureComputationError::UntrustedInput)?;
        let closing_quality = closing
            .consolidated_evidence()
            .ok_or(FeatureComputationError::UntrustedInput)?
            .quality_score;
        Ok(window_quality.min(closing_quality))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TradeObservation {
    asset: AssetId,
    price: Price,
    quantity: Quantity,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    source: SourceId,
    quality_score: QualityScore,
    lineage_hash: [u8; 32],
}

impl TradeObservation {
    /// Derives a feature-safe trade from a fully validated normalized envelope
    /// and the exact instrument definition used to interpret it.
    pub fn try_from_event(
        event: &EventEnvelope,
        instrument: &InstrumentDefinition,
    ) -> Result<Self, FeatureComputationError> {
        event
            .verify()
            .map_err(|_| FeatureComputationError::UntrustedInput)?;
        let metadata = event.metadata().as_unchecked();
        if metadata.instrument_id.as_ref() != Some(instrument.id()) {
            return Err(FeatureComputationError::MixedEntity);
        }
        if metadata.source.kind() != SourceKind::Exchange
            || metadata.source.name() != instrument.id().venue().as_str()
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let UncheckedEventPayload::Trade(trade) = event.payload().as_unchecked() else {
            return Err(FeatureComputationError::InvalidInput);
        };
        let event_time = metadata
            .exchange_transaction_timestamp
            .or(metadata.exchange_timestamp)
            .ok_or(FeatureComputationError::InvalidInput)?;
        if event_time < instrument.listing_time()
            || instrument
                .delisting_time()
                .is_some_and(|delisting| event_time >= delisting)
            || metadata.normalization_timestamp < event_time
        {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        Ok(Self {
            asset: instrument.base_asset().clone(),
            price: trade.price,
            quantity: trade.quantity,
            event_time,
            as_known_at: metadata.normalization_timestamp,
            source: metadata.source.clone(),
            quality_score: QualityScore::from_millionths(metadata.quality_score_ppm)
                .map_err(|_| FeatureComputationError::InvalidInput)?,
            lineage_hash: *event.id().as_bytes(),
        })
    }

    pub const fn event_time(&self) -> UnixNanos {
        self.event_time
    }
}

/// Finalized, bounded normalized-trade evidence for a VWAP recipe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedTradeWindow {
    time_window: TimeWindow,
    asset: AssetId,
    observations: Vec<TradeObservation>,
    as_known_at: UnixNanos,
    source_coverage: SourceCoverage,
    quality_score: QualityScore,
    lineage_digest: [u8; 32],
}

impl FinalizedTradeWindow {
    pub fn try_from_finalization(
        asset: AssetId,
        tracker: &crate::WatermarkTracker,
        decision: crate::FinalizationDecision,
        observations: Vec<TradeObservation>,
    ) -> Result<Self, FeatureComputationError> {
        if observations.is_empty() {
            return Err(FeatureComputationError::InsufficientHistory);
        }
        if observations.len() > MAX_PRICE_OBSERVATIONS {
            return Err(FeatureComputationError::CapacityExceeded);
        }
        let entity = FeatureEntity::Asset(asset.clone());
        if !tracker.validates_decision(decision, &entity)
            || decision.state() != crate::Finalization::Final
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        if observations
            .iter()
            .any(|observation| observation.asset != asset)
        {
            return Err(FeatureComputationError::MixedEntity);
        }
        if observations.iter().any(|observation| {
            observation.event_time < decision.window().start()
                || observation.event_time >= decision.window().end()
        }) {
            return Err(FeatureComputationError::OutsideWindow);
        }
        if observations
            .windows(2)
            .any(|pair| pair[0].event_time >= pair[1].event_time)
        {
            return Err(FeatureComputationError::NonMonotonicTime);
        }
        let mut unique_lineage = BTreeSet::new();
        if observations
            .iter()
            .any(|observation| !unique_lineage.insert(observation.lineage_hash))
        {
            return Err(FeatureComputationError::DuplicateLineage);
        }
        let tracker_coverage = tracker
            .coverage_for_decision(decision)
            .ok_or(FeatureComputationError::UntrustedInput)?;
        let mut contributing_sources = Vec::new();
        for observation in &observations {
            if !contributing_sources.contains(&observation.source) {
                contributing_sources.push(observation.source.clone());
            }
        }
        let observed_sources = tracker_coverage
            .entries()
            .iter()
            .filter(|entry| contributing_sources.contains(entry.source()))
            .cloned()
            .collect();
        let source_coverage = SourceCoverage::try_new_partial(
            tracker_coverage.expected_sources().to_vec(),
            observed_sources,
        )
        .map_err(|_| FeatureComputationError::UntrustedInput)?;
        if contributing_sources
            .iter()
            .any(|source| !source_coverage.expected_sources().contains(source))
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let as_known_at = observations
            .iter()
            .map(|observation| observation.as_known_at)
            .max()
            .ok_or(FeatureComputationError::InsufficientHistory)?;
        let quality_score = observations
            .iter()
            .map(|observation| observation.quality_score)
            .min()
            .ok_or(FeatureComputationError::InsufficientHistory)?;
        let mut hasher = Hasher::new();
        hasher.update(b"crypto-intelligence/finalized-trade-window/v1");
        hasher.update(&decision.evidence_digest());
        hasher.update(&decision.window().start().value().to_be_bytes());
        hasher.update(&decision.window().end().value().to_be_bytes());
        super::hash_entity(&mut hasher, &entity);
        hasher.update(&(observations.len() as u64).to_be_bytes());
        for observation in &observations {
            hasher.update(&observation.event_time.value().to_be_bytes());
            hasher.update(&observation.as_known_at.value().to_be_bytes());
            hasher.update(&observation.lineage_hash);
            let price = observation.price.to_string();
            hasher.update(&(price.len() as u64).to_be_bytes());
            hasher.update(price.as_bytes());
            let quantity = observation.quantity.to_string();
            hasher.update(&(quantity.len() as u64).to_be_bytes());
            hasher.update(quantity.as_bytes());
        }
        Ok(Self {
            time_window: decision.window(),
            asset,
            observations,
            as_known_at,
            source_coverage,
            quality_score,
            lineage_digest: *hasher.finalize().as_bytes(),
        })
    }

    pub const fn time_window(&self) -> TimeWindow {
        self.time_window
    }

    fn secondary_evidence(&self) -> super::SecondaryEvidence {
        super::SecondaryEvidence::new(
            self.as_known_at,
            self.lineage_digest,
            self.observations.len(),
            self.quality_score,
        )
        .with_source_coverage(self.source_coverage.clone())
    }
}

/// Emits rolling or UTC-session VWAP distance only when normalized trades and
/// consolidated fair prices share the same finalized window and asset.
pub fn compute_vwap_distance_feature(
    kind: Task4FeatureKind,
    trades: &FinalizedTradeWindow,
    fair_prices: &PriceWindow,
    closing: &PriceObservation,
    fair_price_tracker: &crate::WatermarkTracker,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    if !matches!(
        kind,
        Task4FeatureKind::RollingVwapDistance | Task4FeatureKind::AnchoredVwapDistance
    ) || trades.time_window != fair_prices.time_window()
        || fair_prices.entity() != &FeatureEntity::Asset(trades.asset.clone())
    {
        return Err(FeatureComputationError::MixedEntity);
    }
    fair_prices.validates_consolidated_closing_observation(closing)?;
    let prices_and_quantities = trades
        .observations
        .iter()
        .map(|observation| (observation.price, observation.quantity))
        .collect::<Vec<_>>();
    let value = vwap_distance(closing.price(), &prices_and_quantities)?;
    super::ComputedAnalyticalFeature::from_price_window_recipe_with_secondary(
        kind,
        fair_prices,
        Some(closing),
        fair_price_tracker,
        value,
        Some(trades.secondary_evidence()),
    )
}

/// Compares the latest normalized trade in the finalized window with the
/// robust consolidated fair price at the same window boundary.
pub fn compute_consolidated_fair_price_distance(
    trades: &FinalizedTradeWindow,
    fair_prices: &PriceWindow,
    closing: &PriceObservation,
    fair_price_tracker: &crate::WatermarkTracker,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    if trades.time_window != fair_prices.time_window()
        || fair_prices.entity() != &FeatureEntity::Asset(trades.asset.clone())
    {
        return Err(FeatureComputationError::MixedEntity);
    }
    let latest_trade = trades
        .observations
        .last()
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    let value = relative_distance(latest_trade.price, closing.price())?;
    super::ComputedAnalyticalFeature::from_price_window_recipe_with_secondary(
        Task4FeatureKind::ConsolidatedFairPriceDistance,
        fair_prices,
        Some(closing),
        fair_price_tracker,
        value,
        Some(trades.secondary_evidence()),
    )
}

/// Computes the opening gap of a finalized interval against the immediately
/// preceding finalized interval while binding both decisions into lineage.
pub fn compute_finalized_interval_gap_feature(
    previous: (&PriceWindow, &PriceObservation, &crate::WatermarkTracker),
    current: (&PriceWindow, &crate::WatermarkTracker),
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    if previous.0.entity() != current.0.entity()
        || previous.0.time_window().end() != current.0.time_window().start()
    {
        return Err(FeatureComputationError::NonMonotonicTime);
    }
    let previous_decision = previous
        .0
        .finalization()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    if !previous
        .2
        .validates_decision(previous_decision, previous.0.entity())
        || previous_decision.state() != crate::Finalization::Final
    {
        return Err(FeatureComputationError::UntrustedInput);
    }
    previous
        .0
        .validates_consolidated_closing_observation(previous.1)?;
    let current_open = current
        .0
        .observations()
        .first()
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    let value = relative_distance(current_open.price(), previous.1.price())?;
    let secondary = super::SecondaryEvidence::new(
        previous.1.as_known_at().max(previous.0.as_of()),
        previous.0.closing_lineage_digest(previous.1)?,
        previous.0.observations().len() + 1,
        previous.0.quality_with_closing(previous.1)?,
    )
    .with_source_coverage(
        previous
            .0
            .source_coverage()
            .cloned()
            .ok_or(FeatureComputationError::UntrustedInput)?,
    );
    super::ComputedAnalyticalFeature::from_price_window_recipe_with_secondary(
        Task4FeatureKind::FinalizedIntervalGap,
        current.0,
        None,
        current.1,
        value,
        Some(secondary),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HorizonReturn {
    horizon: usize,
    value: f64,
}

impl HorizonReturn {
    pub const fn horizon(self) -> usize {
        self.horizon
    }

    pub const fn value(self) -> f64 {
        self.value
    }
}

pub fn log_return(current: Price, previous: Price) -> Result<f64, FeatureComputationError> {
    let current = analytical_price(current)?;
    let previous = analytical_price(previous)?;
    finite_analytical((current / previous).ln())
}

pub fn relative_distance(value: Price, reference: Price) -> Result<f64, FeatureComputationError> {
    let value = analytical_price(value)?;
    let reference = analytical_price(reference)?;
    finite_analytical(value / reference - 1.0)
}

pub fn cumulative_return(window: &PriceWindow) -> Result<f64, FeatureComputationError> {
    require_price_history(window, 2)?;
    relative_distance(
        window
            .observations
            .last()
            .expect("history is nonempty")
            .price,
        window
            .observations
            .first()
            .expect("history is nonempty")
            .price,
    )
}

pub fn high_low_range(window: &PriceWindow) -> Result<f64, FeatureComputationError> {
    require_price_history(window, 2)?;
    let mut high = window.observations[0].price;
    let mut low = high;
    for observation in &window.observations[1..] {
        high = high.max(observation.price);
        low = low.min(observation.price);
    }
    relative_distance(high, low)
}

pub fn open_close_range(window: &PriceWindow) -> Result<f64, FeatureComputationError> {
    require_price_history(window, 2)?;
    relative_distance(
        window
            .observations
            .last()
            .expect("history is nonempty")
            .price,
        window
            .observations
            .first()
            .expect("history is nonempty")
            .price,
    )
}

pub fn vwap_distance(
    value: Price,
    prices_and_quantities: &[(Price, Quantity)],
) -> Result<f64, FeatureComputationError> {
    if prices_and_quantities.is_empty() || prices_and_quantities.len() > MAX_PRICE_OBSERVATIONS {
        return Err(if prices_and_quantities.is_empty() {
            FeatureComputationError::InsufficientHistory
        } else {
            FeatureComputationError::CapacityExceeded
        });
    }
    let mut notional = 0.0;
    let mut quantity = 0.0;
    for (price, size) in prices_and_quantities {
        if size.value().is_zero() {
            continue;
        }
        let analytical_price = analytical_price(*price)?;
        let analytical_size = size.value().to_f64_lossy_for_analysis();
        if !analytical_size.is_finite() || analytical_size <= 0.0 {
            return Err(FeatureComputationError::InvalidInput);
        }
        notional = analytical_price.mul_add(analytical_size, notional);
        quantity += analytical_size;
        if !notional.is_finite() || !quantity.is_finite() {
            return Err(FeatureComputationError::InvalidInput);
        }
    }
    if quantity <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let vwap = finite_analytical(notional / quantity)?;
    finite_analytical(analytical_price(value)? / vwap - 1.0)
}

pub fn consecutive_log_returns(window: &PriceWindow) -> Result<Vec<f64>, FeatureComputationError> {
    require_price_history(window, 2)?;
    window
        .observations
        .windows(2)
        .map(|pair| log_return(pair[1].price, pair[0].price))
        .collect()
}

pub fn multi_horizon_log_returns(
    window: &PriceWindow,
    horizons: &[usize],
) -> Result<Vec<HorizonReturn>, FeatureComputationError> {
    if horizons.is_empty() || horizons.len() > MAX_HORIZONS {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let latest = window
        .observations
        .last()
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    horizons
        .iter()
        .map(|horizon| {
            if *horizon == 0 || *horizon >= window.observations.len() {
                return Err(FeatureComputationError::InsufficientHistory);
            }
            let previous = &window.observations[window.observations.len() - 1 - *horizon];
            Ok(HorizonReturn {
                horizon: *horizon,
                value: log_return(latest.price, previous.price)?,
            })
        })
        .collect()
}

pub fn maximum_drawdown(window: &PriceWindow) -> Result<f64, FeatureComputationError> {
    require_price_history(window, 2)?;
    let mut peak = analytical_price(window.observations[0].price)?;
    let mut maximum = 0.0_f64;
    for observation in &window.observations[1..] {
        let value = analytical_price(observation.price)?;
        peak = peak.max(value);
        maximum = maximum.max((peak - value) / peak);
    }
    finite_analytical(maximum)
}

pub fn maximum_run_up(window: &PriceWindow) -> Result<f64, FeatureComputationError> {
    require_price_history(window, 2)?;
    let mut trough = analytical_price(window.observations[0].price)?;
    let mut maximum = 0.0_f64;
    for observation in &window.observations[1..] {
        let value = analytical_price(observation.price)?;
        trough = trough.min(value);
        maximum = maximum.max((value - trough) / trough);
    }
    finite_analytical(maximum)
}

/// Least-squares slope for values sampled at equal intervals.
///
/// Use [`time_weighted_trend_slope`] when event-time spacing is irregular.
pub fn trend_slope(values: &[f64]) -> Result<f64, FeatureComputationError> {
    validate_finite_history(values, 2)?;
    let count = values.len() as f64;
    let x_mean = (count - 1.0) / 2.0;
    let y_mean = values.iter().sum::<f64>() / count;
    let (numerator, denominator) =
        values
            .iter()
            .enumerate()
            .fold((0.0, 0.0), |(numerator, denominator), (index, value)| {
                let centered = index as f64 - x_mean;
                (
                    numerator + centered * (value - y_mean),
                    denominator + centered * centered,
                )
            });
    if denominator <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    finite_analytical(numerator / denominator)
}

pub fn time_weighted_trend_slope(window: &PriceWindow) -> Result<f64, FeatureComputationError> {
    require_price_history(window, 2)?;
    let origin = window.observations[0].event_time.value();
    let times = window
        .observations
        .iter()
        .map(|observation| {
            observation
                .event_time
                .value()
                .checked_sub(origin)
                .map(|value| value as f64)
                .ok_or(FeatureComputationError::InvalidInput)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let values = window
        .observations
        .iter()
        .map(|observation| analytical_price(observation.price))
        .collect::<Result<Vec<_>, _>>()?;
    regression_slope(&times, &values)
}

pub fn time_weighted_trend_acceleration(
    window: &PriceWindow,
) -> Result<f64, FeatureComputationError> {
    require_price_history(window, 3)?;
    let mut midpoint_times = Vec::with_capacity(window.observations.len() - 1);
    let mut interval_slopes = Vec::with_capacity(window.observations.len() - 1);
    let origin = window.observations[0].event_time.value();
    for pair in window.observations.windows(2) {
        let left_time = pair[0].event_time.value();
        let right_time = pair[1].event_time.value();
        let elapsed = right_time
            .checked_sub(left_time)
            .ok_or(FeatureComputationError::InvalidInput)?;
        if elapsed <= 0 {
            return Err(FeatureComputationError::NonMonotonicTime);
        }
        let price_change = analytical_price(pair[1].price)? - analytical_price(pair[0].price)?;
        midpoint_times.push(
            left_time
                .checked_sub(origin)
                .and_then(|left| left.checked_add(elapsed / 2))
                .ok_or(FeatureComputationError::InvalidInput)? as f64,
        );
        interval_slopes.push(finite_analytical(price_change / elapsed as f64)?);
    }
    regression_slope(&midpoint_times, &interval_slopes)
}

/// Mean second difference for values sampled at equal intervals.
///
/// Use [`time_weighted_trend_acceleration`] when event-time spacing is irregular.
pub fn trend_acceleration(values: &[f64]) -> Result<f64, FeatureComputationError> {
    validate_finite_history(values, 3)?;
    let mut total = 0.0;
    for triple in values.windows(3) {
        total += triple[2] - 2.0 * triple[1] + triple[0];
    }
    finite_analytical(total / (values.len() - 2) as f64)
}

pub fn return_autocorrelation(returns: &[f64], lag: usize) -> Result<f64, FeatureComputationError> {
    validate_finite_history(returns, 3)?;
    if lag == 0 || lag >= returns.len() - 1 {
        return Err(FeatureComputationError::InvalidParameter);
    }
    correlation(&returns[..returns.len() - lag], &returns[lag..])
}

pub fn variance_ratio(returns: &[f64], horizon: usize) -> Result<f64, FeatureComputationError> {
    validate_finite_history(returns, 3)?;
    if horizon < 2 || horizon >= returns.len() {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let one_period = population_variance(returns)?;
    if one_period <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let mut aggregated = Vec::with_capacity(returns.len() - horizon + 1);
    let mut rolling = returns[..horizon].iter().sum::<f64>();
    aggregated.push(rolling);
    for index in horizon..returns.len() {
        rolling += returns[index] - returns[index - horizon];
        aggregated.push(finite_analytical(rolling)?);
    }
    let aggregated_variance = population_variance(&aggregated)?;
    finite_analytical(aggregated_variance / (horizon as f64 * one_period))
}

pub fn rolling_skewness(values: &[f64]) -> Result<f64, FeatureComputationError> {
    validate_finite_history(values, 3)?;
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64;
    if variance <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let third = values
        .iter()
        .map(|value| (value - mean).powi(3))
        .sum::<f64>()
        / values.len() as f64;
    finite_analytical(third / variance.powf(1.5))
}

pub fn rolling_excess_kurtosis(values: &[f64]) -> Result<f64, FeatureComputationError> {
    validate_finite_history(values, 4)?;
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / values.len() as f64;
    if variance <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let fourth = values
        .iter()
        .map(|value| (value - mean).powi(4))
        .sum::<f64>()
        / values.len() as f64;
    finite_analytical(fourth / variance.powi(2) - 3.0)
}

pub fn return_reversal(returns: &[f64], threshold: f64) -> Result<f64, FeatureComputationError> {
    validate_finite_history(returns, 2)?;
    if !threshold.is_finite() || threshold <= 0.0 {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let mut total = 0.0;
    let mut count = 0_usize;
    for pair in returns.windows(2) {
        if pair[0].abs() >= threshold {
            total += -pair[0].signum() * pair[1];
            count += 1;
        }
    }
    if count == 0 {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    finite_analytical(total / count as f64)
}

pub fn finalized_interval_gap(
    previous_close: &PriceObservation,
    current_open: &PriceObservation,
) -> Result<f64, FeatureComputationError> {
    if !previous_close.finalized || !current_open.finalized {
        return Err(FeatureComputationError::WindowNotFinal);
    }
    if current_open.entity != previous_close.entity
        || current_open.event_time <= previous_close.event_time
        || current_open.as_known_at < previous_close.as_known_at
    {
        return Err(FeatureComputationError::NonMonotonicTime);
    }
    relative_distance(current_open.price, previous_close.price)
}

/// Computes one price-path or return-distribution catalogue recipe from an
/// exact, finalized consolidated-price grid.
pub fn compute_price_path_feature(
    kind: Task4FeatureKind,
    window: &PriceWindow,
    closing: &PriceObservation,
    tracker: &crate::WatermarkTracker,
) -> Result<super::ComputedAnalyticalFeature, FeatureComputationError> {
    let result = (|| {
        super::volatility::validate_sampling(
            window,
            closing,
            super::volatility::realized_volatility_v2_policy(),
        )?;
        let prices = complete_analytical_prices(window, closing)?;
        let returns = complete_log_returns(&prices)?;
        match kind {
            Task4FeatureKind::LogReturn => finite_analytical(
                (prices
                    .last()
                    .expect("validated complete price path is nonempty")
                    / prices[0])
                    .ln(),
            ),
            Task4FeatureKind::CumulativeReturn | Task4FeatureKind::OpenCloseRange => {
                finite_analytical(
                    prices
                        .last()
                        .expect("validated complete price path is nonempty")
                        / prices[0]
                        - 1.0,
                )
            }
            Task4FeatureKind::HighLowRange => {
                let (low, high) = prices
                    .iter()
                    .copied()
                    .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), price| {
                        (low.min(price), high.max(price))
                    });
                finite_analytical(high / low - 1.0)
            }
            Task4FeatureKind::TrendSlope => complete_time_weighted_trend_slope(window, closing),
            Task4FeatureKind::TrendAcceleration => {
                complete_time_weighted_trend_acceleration(window, closing)
            }
            Task4FeatureKind::MaximumDrawdown => maximum_drawdown_values(&prices),
            Task4FeatureKind::MaximumRunUp => maximum_run_up_values(&prices),
            Task4FeatureKind::ReturnAutocorrelation => return_autocorrelation(&returns, 1),
            Task4FeatureKind::VarianceRatio => variance_ratio(&returns, 2),
            Task4FeatureKind::RollingSkewness => rolling_skewness(&returns),
            Task4FeatureKind::RollingExcessKurtosis => rolling_excess_kurtosis(&returns),
            Task4FeatureKind::ReturnReversal => return_reversal(&returns, 0.05),
            _ => Err(FeatureComputationError::InvalidParameter),
        }
    })();
    super::ComputedAnalyticalFeature::from_price_window_analytical(
        kind,
        window,
        Some(closing),
        tracker,
        super::AnalyticalFeature::from_result(result)?,
    )
}

fn complete_analytical_prices(
    window: &PriceWindow,
    closing: &PriceObservation,
) -> Result<Vec<f64>, FeatureComputationError> {
    window.validates_consolidated_closing_observation(closing)?;
    let mut prices = Vec::with_capacity(window.observations.len() + 1);
    for observation in &window.observations {
        prices.push(analytical_price(observation.price)?);
    }
    prices.push(analytical_price(closing.price)?);
    Ok(prices)
}

fn complete_log_returns(prices: &[f64]) -> Result<Vec<f64>, FeatureComputationError> {
    validate_finite_history(prices, 2)?;
    prices
        .windows(2)
        .map(|pair| finite_analytical((pair[1] / pair[0]).ln()))
        .collect()
}

fn complete_time_weighted_trend_slope(
    window: &PriceWindow,
    closing: &PriceObservation,
) -> Result<f64, FeatureComputationError> {
    let prices = complete_analytical_prices(window, closing)?;
    let origin = window
        .observations
        .first()
        .ok_or(FeatureComputationError::InsufficientHistory)?
        .event_time
        .value();
    let mut times = Vec::with_capacity(prices.len());
    for observation in &window.observations {
        times.push(
            observation
                .event_time
                .value()
                .checked_sub(origin)
                .ok_or(FeatureComputationError::InvalidInput)? as f64,
        );
    }
    times.push(
        closing
            .event_time
            .value()
            .checked_sub(origin)
            .ok_or(FeatureComputationError::InvalidInput)? as f64,
    );
    regression_slope(&times, &prices)
}

fn complete_time_weighted_trend_acceleration(
    window: &PriceWindow,
    closing: &PriceObservation,
) -> Result<f64, FeatureComputationError> {
    let prices = complete_analytical_prices(window, closing)?;
    let mut times = window
        .observations
        .iter()
        .map(|observation| observation.event_time.value())
        .collect::<Vec<_>>();
    times.push(closing.event_time.value());
    let origin = times[0];
    let mut midpoint_times = Vec::with_capacity(prices.len() - 1);
    let mut interval_slopes = Vec::with_capacity(prices.len() - 1);
    for (time_pair, price_pair) in times.windows(2).zip(prices.windows(2)) {
        let elapsed = time_pair[1]
            .checked_sub(time_pair[0])
            .ok_or(FeatureComputationError::InvalidInput)?;
        if elapsed <= 0 {
            return Err(FeatureComputationError::NonMonotonicTime);
        }
        midpoint_times.push(
            time_pair[0]
                .checked_sub(origin)
                .and_then(|left| left.checked_add(elapsed / 2))
                .ok_or(FeatureComputationError::InvalidInput)? as f64,
        );
        interval_slopes.push(finite_analytical(
            (price_pair[1] - price_pair[0]) / elapsed as f64,
        )?);
    }
    regression_slope(&midpoint_times, &interval_slopes)
}

fn maximum_drawdown_values(prices: &[f64]) -> Result<f64, FeatureComputationError> {
    validate_finite_history(prices, 2)?;
    let mut peak = prices[0];
    let mut maximum = 0.0_f64;
    for value in &prices[1..] {
        peak = peak.max(*value);
        maximum = maximum.max((peak - value) / peak);
    }
    finite_analytical(maximum)
}

fn maximum_run_up_values(prices: &[f64]) -> Result<f64, FeatureComputationError> {
    validate_finite_history(prices, 2)?;
    let mut trough = prices[0];
    let mut maximum = 0.0_f64;
    for value in &prices[1..] {
        trough = trough.min(*value);
        maximum = maximum.max((value - trough) / trough);
    }
    finite_analytical(maximum)
}

fn analytical_price(price: Price) -> Result<f64, FeatureComputationError> {
    let value = price.value().to_f64_lossy_for_analysis();
    if !value.is_finite() || value <= 0.0 {
        Err(FeatureComputationError::InvalidInput)
    } else {
        Ok(value)
    }
}

fn require_price_history(
    window: &PriceWindow,
    minimum: usize,
) -> Result<(), FeatureComputationError> {
    if window.observations.len() < minimum {
        Err(FeatureComputationError::InsufficientHistory)
    } else {
        Ok(())
    }
}

fn validate_finite_history(values: &[f64], minimum: usize) -> Result<(), FeatureComputationError> {
    if values.len() > MAX_PRICE_OBSERVATIONS {
        return Err(FeatureComputationError::CapacityExceeded);
    }
    if values.len() < minimum {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    if values.iter().any(|value| !value.is_finite()) {
        return Err(FeatureComputationError::InvalidInput);
    }
    Ok(())
}

fn population_variance(values: &[f64]) -> Result<f64, FeatureComputationError> {
    validate_finite_history(values, 2)?;
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    finite_analytical(
        values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / values.len() as f64,
    )
}

fn correlation(left: &[f64], right: &[f64]) -> Result<f64, FeatureComputationError> {
    if left.len() != right.len() {
        return Err(FeatureComputationError::LengthMismatch);
    }
    validate_finite_history(left, 2)?;
    validate_finite_history(right, 2)?;
    let left_mean = left.iter().sum::<f64>() / left.len() as f64;
    let right_mean = right.iter().sum::<f64>() / right.len() as f64;
    let (covariance, left_variance, right_variance) = left.iter().zip(right).fold(
        (0.0, 0.0, 0.0),
        |(covariance, left_variance, right_variance), (left, right)| {
            let left = left - left_mean;
            let right = right - right_mean;
            (
                covariance + left * right,
                left_variance + left * left,
                right_variance + right * right,
            )
        },
    );
    if left_variance <= 0.0 || right_variance <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    finite_analytical(covariance / (left_variance * right_variance).sqrt())
}

fn regression_slope(x: &[f64], y: &[f64]) -> Result<f64, FeatureComputationError> {
    if x.len() != y.len() {
        return Err(FeatureComputationError::LengthMismatch);
    }
    validate_finite_history(x, 2)?;
    validate_finite_history(y, 2)?;
    let count = x.len() as f64;
    let x_mean = x.iter().sum::<f64>() / count;
    let y_mean = y.iter().sum::<f64>() / count;
    let (numerator, denominator) =
        x.iter()
            .zip(y)
            .fold((0.0, 0.0), |(numerator, denominator), (x, y)| {
                let centered = x - x_mean;
                (
                    numerator + centered * (y - y_mean),
                    denominator + centered * centered,
                )
            });
    if denominator <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    finite_analytical(numerator / denominator)
}

fn price_window_lineage_digest(
    time_window: TimeWindow,
    entity: &FeatureEntity,
    as_of: UnixNanos,
    observations: &[PriceObservation],
) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/price-window-input/v1");
    hasher.update(&time_window.start().value().to_be_bytes());
    hasher.update(&time_window.end().value().to_be_bytes());
    hasher.update(&as_of.value().to_be_bytes());
    super::hash_entity(&mut hasher, entity);
    hasher.update(&(observations.len() as u64).to_be_bytes());
    for observation in observations {
        hasher.update(&observation.event_time.value().to_be_bytes());
        hasher.update(&observation.as_known_at.value().to_be_bytes());
        hasher.update(&[u8::from(observation.finalized)]);
        let price = observation.price.to_string();
        hasher.update(&(price.len() as u64).to_be_bytes());
        hasher.update(price.as_bytes());
        hasher.update(observation.lineage_hash());
    }
    *hasher.finalize().as_bytes()
}

fn source_coverage_from_fair_price(
    fair_price: &FairPrice,
) -> Result<SourceCoverage, FeatureComputationError> {
    let lineage = fair_price.lineage();
    let expected_sources = lineage
        .eligible_sources()
        .ok_or(FeatureComputationError::UntrustedInput)?
        .to_vec();
    let mut observed = Vec::with_capacity(lineage.included().len());
    for included in lineage.included() {
        observed.push(SourceCoverageEntry::new(
            included.source().clone(),
            included.source_health(),
        ));
    }
    SourceCoverage::try_new_partial(expected_sources, observed)
        .map_err(|_| FeatureComputationError::InvalidInput)
}

fn aggregate_consolidated_evidence(
    observations: &[PriceObservation],
) -> Result<(SourceCoverage, QualityScore), FeatureComputationError> {
    let first = observations
        .first()
        .and_then(PriceObservation::consolidated_evidence)
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let mut minimum_quality = first.quality_score;
    for observation in &observations[1..] {
        let evidence = observation
            .consolidated_evidence()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        if evidence.source_coverage != first.source_coverage {
            return Err(FeatureComputationError::EligibleUniverseChanged);
        }
        minimum_quality = minimum_quality.min(evidence.quality_score);
    }
    Ok((first.source_coverage.clone(), minimum_quality))
}
