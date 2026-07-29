//! Robust, bounded, point-in-time quote-currency conversion evidence.

use std::{cmp::Ordering, collections::HashSet};

use domain::{AssetId, SourceId, SourceKind, UnixNanos, VenueId};
use feature_registry::DurationNanos;
use fixed_decimal::{FixedDecimal, Notional, Price};
use quality::SourceHealthState;

use crate::{
    ConsolidatedError, PolicyId, Ppm,
    fair_price::{capped_normalized_weights, compare_nonnegative_ratio},
};

const MAXIMUM_REFERENCE_VENUES: usize = 64;
const PPM_DENOMINATOR: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum StablecoinDislocationState {
    Normal = 1,
    TransientNoise = 2,
    VenueLocal = 3,
    SustainedMarketWide = 4,
    LiquidityFailure = 5,
    RedemptionOrReserveEvent = 6,
    InsufficientEvidence = 7,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PriceInterval {
    lower: Price,
    upper: Price,
}

impl PriceInterval {
    pub fn try_new(lower: Price, upper: Price) -> Result<Self, ConsolidatedError> {
        if lower > upper {
            return Err(ConsolidatedError::InvalidConversionReference);
        }
        Ok(Self { lower, upper })
    }

    pub const fn lower(self) -> Price {
        self.lower
    }

    pub const fn upper(self) -> Price {
        self.upper
    }
}

#[derive(Clone, Debug)]
pub struct StablecoinReferenceConfigInput {
    pub policy_id: PolicyId,
    pub quote_asset: AssetId,
    pub reference_asset: AssetId,
    pub nominal_rate: Price,
    pub minimum_sources: usize,
    pub maximum_sources: usize,
    pub quote_ttl: DurationNanos,
    pub depth_cap_reference: Notional,
    pub minimum_quality: Ppm,
    pub minimum_freshness: Ppm,
    pub maximum_venue_weight: Ppm,
    pub material_deviation: Ppm,
}

#[derive(Clone, Debug)]
struct StablecoinReferenceConfig {
    policy_id: PolicyId,
    quote_asset: AssetId,
    reference_asset: AssetId,
    nominal_rate: Price,
    minimum_sources: usize,
    maximum_sources: usize,
    quote_ttl: i64,
    depth_cap_reference: Notional,
    minimum_quality: Ppm,
    minimum_freshness: Ppm,
    maximum_venue_weight: Ppm,
    material_deviation: Ppm,
}

#[derive(Clone, Debug)]
pub struct StablecoinVenueQuoteInput {
    pub source: SourceId,
    pub venue: VenueId,
    pub quote_asset: AssetId,
    pub reference_asset: AssetId,
    pub bid: Price,
    pub ask: Price,
    pub executable_depth_reference: Notional,
    pub quality: Ppm,
    pub freshness: Ppm,
    pub source_health: SourceHealthState,
    pub event_time: UnixNanos,
    pub as_known_at: UnixNanos,
    pub persistent: bool,
    pub liquidity_failure: bool,
    pub redemption_or_reserve_event: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StablecoinVenueQuote {
    source: SourceId,
    venue: VenueId,
    quote_asset: AssetId,
    reference_asset: AssetId,
    bid: Price,
    ask: Price,
    midpoint: Price,
    executable_depth_reference: Notional,
    quality: Ppm,
    freshness: Ppm,
    source_health: SourceHealthState,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    persistent: bool,
    liquidity_failure: bool,
    redemption_or_reserve_event: bool,
}

impl StablecoinVenueQuote {
    pub fn try_new(input: StablecoinVenueQuoteInput) -> Result<Self, ConsolidatedError> {
        if input.source.kind() != SourceKind::Exchange
            || input.quote_asset == input.reference_asset
            || input.bid > input.ask
            || input.executable_depth_reference.value().is_zero()
            || input.event_time.value() <= 0
            || input.as_known_at < input.event_time
        {
            return Err(ConsolidatedError::InvalidStablecoinQuote);
        }
        let midpoint = average_price(input.bid, input.ask)?;
        Ok(Self {
            source: input.source,
            venue: input.venue,
            quote_asset: input.quote_asset,
            reference_asset: input.reference_asset,
            bid: input.bid,
            ask: input.ask,
            midpoint,
            executable_depth_reference: input.executable_depth_reference,
            quality: input.quality,
            freshness: input.freshness,
            source_health: input.source_health,
            event_time: input.event_time,
            as_known_at: input.as_known_at,
            persistent: input.persistent,
            liquidity_failure: input.liquidity_failure,
            redemption_or_reserve_event: input.redemption_or_reserve_event,
        })
    }

    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn venue(&self) -> &VenueId {
        &self.venue
    }

    pub const fn quote_asset(&self) -> &AssetId {
        &self.quote_asset
    }

    pub const fn reference_asset(&self) -> &AssetId {
        &self.reference_asset
    }

    pub const fn bid(&self) -> Price {
        self.bid
    }

    pub const fn ask(&self) -> Price {
        self.ask
    }

    pub const fn executable_depth_reference(&self) -> Notional {
        self.executable_depth_reference
    }

    pub const fn quality(&self) -> Ppm {
        self.quality
    }

    pub const fn freshness(&self) -> Ppm {
        self.freshness
    }

    pub const fn source_health(&self) -> SourceHealthState {
        self.source_health
    }

    pub const fn event_time(&self) -> UnixNanos {
        self.event_time
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn persistent(&self) -> bool {
        self.persistent
    }

    pub const fn liquidity_failure(&self) -> bool {
        self.liquidity_failure
    }

    pub const fn redemption_or_reserve_event(&self) -> bool {
        self.redemption_or_reserve_event
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum StablecoinExclusionReason {
    SourceUnhealthy = 1,
    FutureObservation = 2,
    QuoteStale = 3,
    QualityBelowMinimum = 4,
    FreshnessBelowMinimum = 5,
    AssetMismatch = 6,
    ZeroWeight = 7,
    UnconfirmedOutlier = 8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StablecoinExcludedVenue {
    quote: StablecoinVenueQuote,
    reason: StablecoinExclusionReason,
}

impl StablecoinExcludedVenue {
    pub const fn source(&self) -> &SourceId {
        &self.quote.source
    }

    pub const fn reason(&self) -> StablecoinExclusionReason {
        self.reason
    }

    pub const fn venue(&self) -> &VenueId {
        &self.quote.venue
    }

    pub const fn quote_asset(&self) -> &AssetId {
        &self.quote.quote_asset
    }

    pub const fn reference_asset(&self) -> &AssetId {
        &self.quote.reference_asset
    }

    pub const fn bid(&self) -> Price {
        self.quote.bid
    }

    pub const fn ask(&self) -> Price {
        self.quote.ask
    }

    pub const fn executable_depth_reference(&self) -> Notional {
        self.quote.executable_depth_reference
    }

    pub const fn quality(&self) -> Ppm {
        self.quote.quality
    }

    pub const fn freshness(&self) -> Ppm {
        self.quote.freshness
    }

    pub const fn source_health(&self) -> SourceHealthState {
        self.quote.source_health
    }

    pub const fn event_time(&self) -> UnixNanos {
        self.quote.event_time
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.quote.as_known_at
    }

    pub const fn persistent(&self) -> bool {
        self.quote.persistent
    }

    pub const fn liquidity_failure(&self) -> bool {
        self.quote.liquidity_failure
    }

    pub const fn redemption_or_reserve_event(&self) -> bool {
        self.quote.redemption_or_reserve_event
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StablecoinReferenceOutcome {
    Available(Box<QuoteConversionReference>),
    Abstained {
        state: StablecoinDislocationState,
        as_of: UnixNanos,
        considered_sources: Vec<SourceId>,
        considered_venues: Vec<VenueId>,
        considered: Vec<StablecoinVenueQuote>,
        excluded: Vec<StablecoinExcludedVenue>,
        evidence_digest: [u8; 32],
    },
}

#[derive(Clone, Debug)]
pub struct StablecoinReferenceEstimator {
    config: StablecoinReferenceConfig,
}

impl StablecoinReferenceEstimator {
    pub fn try_new(input: StablecoinReferenceConfigInput) -> Result<Self, ConsolidatedError> {
        let quote_ttl =
            i64::try_from(input.quote_ttl.value()).map_err(|_| ConsolidatedError::InvalidConfig)?;
        if input.quote_asset == input.reference_asset
            || input.minimum_sources < 2
            || input.minimum_sources > input.maximum_sources
            || input.maximum_sources > MAXIMUM_REFERENCE_VENUES
            || quote_ttl <= 0
            || input.depth_cap_reference.value().is_zero()
            || input.minimum_quality.value() == 0
            || input.minimum_freshness.value() == 0
            || input.maximum_venue_weight.value() == 0
            || input.maximum_venue_weight.value() > 500_000
            || input.material_deviation.value() == 0
            || (input.minimum_sources as u64)
                .checked_mul(u64::from(input.maximum_venue_weight.value()))
                .is_none_or(|capacity| capacity < u64::from(PPM_DENOMINATOR))
        {
            return Err(ConsolidatedError::InvalidConfig);
        }
        Ok(Self {
            config: StablecoinReferenceConfig {
                policy_id: input.policy_id,
                quote_asset: input.quote_asset,
                reference_asset: input.reference_asset,
                nominal_rate: input.nominal_rate,
                minimum_sources: input.minimum_sources,
                maximum_sources: input.maximum_sources,
                quote_ttl,
                depth_cap_reference: input.depth_cap_reference,
                minimum_quality: input.minimum_quality,
                minimum_freshness: input.minimum_freshness,
                maximum_venue_weight: input.maximum_venue_weight,
                material_deviation: input.material_deviation,
            },
        })
    }

    pub fn estimate(
        &self,
        as_of: UnixNanos,
        quotes: &[StablecoinVenueQuote],
    ) -> Result<StablecoinReferenceOutcome, ConsolidatedError> {
        if as_of.value() <= 0 || quotes.is_empty() || quotes.len() > self.config.maximum_sources {
            return Err(ConsolidatedError::InvalidInputSet);
        }
        let mut sources = HashSet::with_capacity(quotes.len());
        let mut venues = HashSet::with_capacity(quotes.len());
        for quote in quotes {
            if !sources.insert(quote.source.clone()) || !venues.insert(quote.venue.clone()) {
                return Err(ConsolidatedError::DuplicateVenueIdentity);
            }
        }
        let mut ordered: Vec<_> = quotes.iter().collect();
        ordered.sort_by(|left, right| stablecoin_quote_key(left).cmp(&stablecoin_quote_key(right)));
        let mut eligible = Vec::new();
        let mut excluded = Vec::new();
        for quote in ordered {
            if let Some(reason) = self.exclusion(as_of, quote)? {
                excluded.push(StablecoinExcludedVenue {
                    quote: quote.clone(),
                    reason,
                });
            } else {
                eligible.push(quote);
            }
        }
        if eligible.len() < self.config.minimum_sources {
            let considered_sources = source_ids(&eligible);
            let considered_venues = venue_ids(&eligible);
            let considered = considered_quotes(&eligible);
            let evidence_digest =
                stablecoin_evidence_digest(&self.config, as_of, &eligible, &[], &excluded);
            return Ok(StablecoinReferenceOutcome::Abstained {
                state: StablecoinDislocationState::InsufficientEvidence,
                as_of,
                considered_sources,
                considered_venues,
                considered,
                excluded,
                evidence_digest,
            });
        }

        let median = ordinary_median(eligible.iter().map(|quote| quote.midpoint).collect())?;
        let mut outlier_retained = Vec::with_capacity(eligible.len());
        for quote in eligible {
            if deviation_exceeds(quote.midpoint, median, self.config.material_deviation)? {
                excluded.push(StablecoinExcludedVenue {
                    quote: quote.clone(),
                    reason: StablecoinExclusionReason::UnconfirmedOutlier,
                });
            } else {
                outlier_retained.push(quote);
            }
        }
        let eligible = outlier_retained;
        if eligible.len() < self.config.minimum_sources {
            let considered_sources = source_ids(&eligible);
            let considered_venues = venue_ids(&eligible);
            let considered = considered_quotes(&eligible);
            let evidence_digest =
                stablecoin_evidence_digest(&self.config, as_of, &eligible, &[], &excluded);
            return Ok(StablecoinReferenceOutcome::Abstained {
                state: StablecoinDislocationState::InsufficientEvidence,
                as_of,
                considered_sources,
                considered_venues,
                considered,
                excluded,
                evidence_digest,
            });
        }

        let mut raw = Vec::with_capacity(eligible.len());
        for quote in &eligible {
            let depth = ratio_ppm(
                quote.executable_depth_reference.value(),
                self.config.depth_cap_reference.value(),
            )?;
            let weight = u64::from(depth.value())
                .checked_mul(u64::from(quote.quality.value()))
                .and_then(|value| value.checked_mul(u64::from(quote.freshness.value())))
                .ok_or(ConsolidatedError::Arithmetic)?;
            raw.push(weight);
        }
        let mut retained = Vec::with_capacity(eligible.len());
        let mut retained_raw = Vec::with_capacity(eligible.len());
        for (quote, weight) in eligible.into_iter().zip(raw) {
            if weight == 0 {
                excluded.push(StablecoinExcludedVenue {
                    quote: quote.clone(),
                    reason: StablecoinExclusionReason::ZeroWeight,
                });
            } else {
                retained.push(quote);
                retained_raw.push(weight);
            }
        }
        if retained.len() < self.config.minimum_sources {
            let considered_sources = source_ids(&retained);
            let considered_venues = venue_ids(&retained);
            let considered = considered_quotes(&retained);
            let evidence_digest =
                stablecoin_evidence_digest(&self.config, as_of, &retained, &[], &excluded);
            return Ok(StablecoinReferenceOutcome::Abstained {
                state: StablecoinDislocationState::InsufficientEvidence,
                as_of,
                considered_sources,
                considered_venues,
                considered,
                excluded,
                evidence_digest,
            });
        }

        let weights =
            capped_normalized_weights(&retained_raw, self.config.maximum_venue_weight.value())?;
        let dislocation = self.classify(&retained)?;
        let estimate = stablecoin_weighted_median(
            retained
                .iter()
                .zip(&weights)
                .map(|(quote, weight)| (quote.midpoint, *weight, &quote.source))
                .collect(),
        )?;
        let lower = stablecoin_weighted_median(
            retained
                .iter()
                .zip(&weights)
                .map(|(quote, weight)| (quote.bid, *weight, &quote.source))
                .collect(),
        )?;
        let upper = stablecoin_weighted_median(
            retained
                .iter()
                .zip(&weights)
                .map(|(quote, weight)| (quote.ask, *weight, &quote.source))
                .collect(),
        )?;
        let mut source_ids: Vec<_> = retained.iter().map(|quote| quote.source.clone()).collect();
        source_ids
            .sort_by(|left, right| stablecoin_source_key(left).cmp(&stablecoin_source_key(right)));
        let mut venue_ids: Vec<_> = retained.iter().map(|quote| quote.venue.clone()).collect();
        venue_ids.sort();
        let executable_depth_reference = retained
            .iter()
            .try_fold(FixedDecimal::new(0, 0)?, |total, quote| {
                total.checked_add(quote.executable_depth_reference.value())
            })?;
        let event_time = retained
            .iter()
            .map(|quote| quote.event_time)
            .max()
            .ok_or(ConsolidatedError::Arithmetic)?;
        let as_known_at = retained
            .iter()
            .map(|quote| quote.as_known_at)
            .max()
            .ok_or(ConsolidatedError::Arithmetic)?;
        let quality = retained
            .iter()
            .map(|quote| quote.quality)
            .min()
            .ok_or(ConsolidatedError::Arithmetic)?;
        let freshness = retained
            .iter()
            .map(|quote| quote.freshness)
            .min()
            .ok_or(ConsolidatedError::Arithmetic)?;
        let evidence_digest =
            stablecoin_evidence_digest(&self.config, as_of, &retained, &weights, &excluded);
        Ok(StablecoinReferenceOutcome::Available(Box::new(
            QuoteConversionReference {
                policy_id: self.config.policy_id.clone(),
                quote_asset: self.config.quote_asset.clone(),
                reference_asset: self.config.reference_asset.clone(),
                estimate,
                interval: PriceInterval::try_new(lower, upper)?,
                source_ids,
                venue_ids,
                quality,
                freshness,
                event_time,
                as_known_at,
                dislocation,
                executable_depth_reference: Notional::new(executable_depth_reference)?,
                evidence_digest,
            },
        )))
    }

    fn exclusion(
        &self,
        as_of: UnixNanos,
        quote: &StablecoinVenueQuote,
    ) -> Result<Option<StablecoinExclusionReason>, ConsolidatedError> {
        let reason = if quote.quote_asset != self.config.quote_asset
            || quote.reference_asset != self.config.reference_asset
        {
            Some(StablecoinExclusionReason::AssetMismatch)
        } else if quote.source_health != SourceHealthState::Healthy {
            Some(StablecoinExclusionReason::SourceUnhealthy)
        } else if quote.event_time > as_of || quote.as_known_at > as_of {
            Some(StablecoinExclusionReason::FutureObservation)
        } else if as_of
            .value()
            .checked_sub(quote.event_time.value())
            .ok_or(ConsolidatedError::Arithmetic)?
            > self.config.quote_ttl
        {
            Some(StablecoinExclusionReason::QuoteStale)
        } else if quote.quality < self.config.minimum_quality {
            Some(StablecoinExclusionReason::QualityBelowMinimum)
        } else if quote.freshness < self.config.minimum_freshness {
            Some(StablecoinExclusionReason::FreshnessBelowMinimum)
        } else {
            None
        };
        Ok(reason)
    }

    fn classify(
        &self,
        quotes: &[&StablecoinVenueQuote],
    ) -> Result<StablecoinDislocationState, ConsolidatedError> {
        if quotes
            .iter()
            .filter(|quote| quote.redemption_or_reserve_event)
            .count()
            >= self.config.minimum_sources
        {
            return Ok(StablecoinDislocationState::RedemptionOrReserveEvent);
        }
        if quotes
            .iter()
            .filter(|quote| quote.liquidity_failure)
            .count()
            >= self.config.minimum_sources
        {
            return Ok(StablecoinDislocationState::LiquidityFailure);
        }
        let mut deviating = 0_usize;
        let mut persistent_deviations = 0_usize;
        for quote in quotes {
            if deviation_exceeds(
                quote.midpoint,
                self.config.nominal_rate,
                self.config.material_deviation,
            )? {
                deviating += 1;
                persistent_deviations += usize::from(quote.persistent);
            }
        }
        Ok(match deviating {
            0 => StablecoinDislocationState::Normal,
            1 => StablecoinDislocationState::VenueLocal,
            _ if persistent_deviations >= self.config.minimum_sources => {
                StablecoinDislocationState::SustainedMarketWide
            }
            _ => StablecoinDislocationState::TransientNoise,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuoteConversionReference {
    policy_id: PolicyId,
    quote_asset: AssetId,
    reference_asset: AssetId,
    estimate: Price,
    interval: PriceInterval,
    source_ids: Vec<SourceId>,
    venue_ids: Vec<VenueId>,
    quality: Ppm,
    freshness: Ppm,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    dislocation: StablecoinDislocationState,
    executable_depth_reference: Notional,
    evidence_digest: [u8; 32],
}

impl QuoteConversionReference {
    pub const fn policy_id(&self) -> &PolicyId {
        &self.policy_id
    }

    pub const fn quote_asset(&self) -> &AssetId {
        &self.quote_asset
    }

    pub const fn reference_asset(&self) -> &AssetId {
        &self.reference_asset
    }

    pub const fn estimate(&self) -> Price {
        self.estimate
    }

    pub const fn interval(&self) -> PriceInterval {
        self.interval
    }

    pub fn source_ids(&self) -> &[SourceId] {
        &self.source_ids
    }

    pub fn venue_ids(&self) -> &[VenueId] {
        &self.venue_ids
    }

    pub fn distinct_healthy_sources(&self) -> u16 {
        u16::try_from(self.source_ids.len()).unwrap_or(u16::MAX)
    }

    pub const fn quality(&self) -> Ppm {
        self.quality
    }

    pub const fn freshness(&self) -> Ppm {
        self.freshness
    }

    pub const fn event_time(&self) -> UnixNanos {
        self.event_time
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn dislocation(&self) -> StablecoinDislocationState {
        self.dislocation
    }

    pub const fn executable_depth_reference(&self) -> Notional {
        self.executable_depth_reference
    }

    pub const fn evidence_digest(&self) -> &[u8; 32] {
        &self.evidence_digest
    }
}

fn ratio_ppm(numerator: FixedDecimal, denominator: FixedDecimal) -> Result<Ppm, ConsolidatedError> {
    if numerator.is_negative() || denominator.is_zero() || denominator.is_negative() {
        return Err(ConsolidatedError::Arithmetic);
    }
    if numerator >= denominator {
        return Ppm::new(PPM_DENOMINATOR);
    }
    let mut low = 0_u32;
    let mut high = PPM_DENOMINATOR;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if compare_nonnegative_ratio(numerator, denominator, middle, PPM_DENOMINATOR)?
            != Ordering::Less
        {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ppm::new(low)
}

fn stablecoin_weighted_median(
    mut values: Vec<(Price, u32, &SourceId)>,
) -> Result<Price, ConsolidatedError> {
    values.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| stablecoin_source_key(left.2).cmp(&stablecoin_source_key(right.2)))
    });
    let total: u64 = values.iter().map(|value| u64::from(value.1)).sum();
    let mut cumulative = 0_u64;
    for (index, (price, weight, _)) in values.iter().enumerate() {
        cumulative = cumulative
            .checked_add(u64::from(*weight))
            .ok_or(ConsolidatedError::Arithmetic)?;
        match cumulative
            .checked_mul(2)
            .ok_or(ConsolidatedError::Arithmetic)?
            .cmp(&total)
        {
            std::cmp::Ordering::Greater => return Ok(*price),
            std::cmp::Ordering::Equal if index + 1 < values.len() => {
                return average_price(*price, values[index + 1].0);
            }
            std::cmp::Ordering::Equal => return Ok(*price),
            std::cmp::Ordering::Less => {}
        }
    }
    Err(ConsolidatedError::Arithmetic)
}

fn ordinary_median(mut values: Vec<Price>) -> Result<Price, ConsolidatedError> {
    values.sort();
    let middle = values.len() / 2;
    if values.len() % 2 == 1 {
        Ok(values[middle])
    } else {
        average_price(values[middle - 1], values[middle])
    }
}

fn source_ids(quotes: &[&StablecoinVenueQuote]) -> Vec<SourceId> {
    let mut sources: Vec<_> = quotes.iter().map(|quote| quote.source.clone()).collect();
    sources.sort_by(|left, right| stablecoin_source_key(left).cmp(&stablecoin_source_key(right)));
    sources
}

fn venue_ids(quotes: &[&StablecoinVenueQuote]) -> Vec<VenueId> {
    let mut venues: Vec<_> = quotes.iter().map(|quote| quote.venue.clone()).collect();
    venues.sort();
    venues
}

fn considered_quotes(quotes: &[&StablecoinVenueQuote]) -> Vec<StablecoinVenueQuote> {
    quotes.iter().map(|quote| (*quote).clone()).collect()
}

fn average_price(left: Price, right: Price) -> Result<Price, ConsolidatedError> {
    let (lower, upper) = if left <= right {
        (left, right)
    } else {
        (right, left)
    };
    let half_span = upper
        .value()
        .checked_sub(lower.value())?
        .checked_div_exact(FixedDecimal::new(2, 0)?)?;
    Ok(Price::new(lower.value().checked_add(half_span)?)?)
}

fn deviation_exceeds(
    value: Price,
    center: Price,
    threshold: Ppm,
) -> Result<bool, ConsolidatedError> {
    let difference = if value >= center {
        value.value().checked_sub(center.value())?
    } else {
        center.value().checked_sub(value.value())?
    };
    Ok(compare_nonnegative_ratio(
        difference,
        center.value(),
        threshold.value(),
        PPM_DENOMINATOR,
    )? == Ordering::Greater)
}

fn stablecoin_source_key(source: &SourceId) -> (u8, &str, u32) {
    (source.kind() as u8, source.name(), source.generation())
}

fn stablecoin_quote_key(quote: &StablecoinVenueQuote) -> (&str, u8, &str, u32) {
    (
        quote.venue.as_str(),
        quote.source.kind() as u8,
        quote.source.name(),
        quote.source.generation(),
    )
}

fn stablecoin_evidence_digest(
    config: &StablecoinReferenceConfig,
    as_of: UnixNanos,
    included: &[&StablecoinVenueQuote],
    weights: &[u32],
    excluded: &[StablecoinExcludedVenue],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crypto-intelligence/stablecoin-reference/v1");
    hash_bytes(&mut hasher, config.policy_id.as_str().as_bytes());
    hash_asset(&mut hasher, &config.quote_asset);
    hash_asset(&mut hasher, &config.reference_asset);
    hash_decimal(&mut hasher, config.nominal_rate.value());
    hasher.update(&(config.minimum_sources as u64).to_be_bytes());
    hasher.update(&(config.maximum_sources as u64).to_be_bytes());
    hasher.update(&config.quote_ttl.to_be_bytes());
    hash_decimal(&mut hasher, config.depth_cap_reference.value());
    hasher.update(&config.minimum_quality.value().to_be_bytes());
    hasher.update(&config.minimum_freshness.value().to_be_bytes());
    hasher.update(&config.maximum_venue_weight.value().to_be_bytes());
    hasher.update(&config.material_deviation.value().to_be_bytes());
    hasher.update(&as_of.value().to_be_bytes());
    hasher.update(&(included.len() as u64).to_be_bytes());
    for (index, quote) in included.iter().enumerate() {
        hasher.update(&[1]);
        hash_stablecoin_quote(&mut hasher, quote);
        match weights.get(index) {
            Some(weight) => {
                hasher.update(&[1]);
                hasher.update(&weight.to_be_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
    }
    hasher.update(&(excluded.len() as u64).to_be_bytes());
    for entry in excluded {
        hasher.update(&[2]);
        hash_stablecoin_quote(&mut hasher, &entry.quote);
        hasher.update(&[entry.reason as u8]);
    }
    *hasher.finalize().as_bytes()
}

fn hash_stablecoin_quote(hasher: &mut blake3::Hasher, quote: &StablecoinVenueQuote) {
    hash_source(hasher, &quote.source);
    hash_bytes(hasher, quote.venue.as_str().as_bytes());
    hash_asset(hasher, &quote.quote_asset);
    hash_asset(hasher, &quote.reference_asset);
    hash_decimal(hasher, quote.bid.value());
    hash_decimal(hasher, quote.ask.value());
    hash_decimal(hasher, quote.midpoint.value());
    hash_decimal(hasher, quote.executable_depth_reference.value());
    hasher.update(&quote.quality.value().to_be_bytes());
    hasher.update(&quote.freshness.value().to_be_bytes());
    hash_bytes(hasher, quote.source_health.as_str().as_bytes());
    hasher.update(&quote.event_time.value().to_be_bytes());
    hasher.update(&quote.as_known_at.value().to_be_bytes());
    hasher.update(&[
        u8::from(quote.persistent),
        u8::from(quote.liquidity_failure),
        u8::from(quote.redemption_or_reserve_event),
    ]);
}

fn hash_asset(hasher: &mut blake3::Hasher, asset: &AssetId) {
    hasher.update(&[asset.namespace() as u8]);
    hash_bytes(hasher, asset.chain_id().as_bytes());
    hash_bytes(hasher, asset.contract_or_mint().as_bytes());
    hash_bytes(hasher, asset.canonical_symbol().as_bytes());
    hasher.update(&asset.generation().to_be_bytes());
}

fn hash_source(hasher: &mut blake3::Hasher, source: &SourceId) {
    hasher.update(&[source.kind() as u8]);
    hash_bytes(hasher, source.name().as_bytes());
    hasher.update(&source.generation().to_be_bytes());
}

fn hash_decimal(hasher: &mut blake3::Hasher, value: FixedDecimal) {
    hasher.update(&value.mantissa().to_be_bytes());
    hasher.update(&value.scale().to_be_bytes());
}

fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value);
}
