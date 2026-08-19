//! Descriptive cross-venue calculations.
//!
//! These functions intentionally do not claim executability. Executable
//! dispersion requires authenticated depth-walk, fee, settlement, lot-size,
//! contract-conversion, and latency evidence that a vector of prices cannot
//! provide.

use consolidated_market::{FairPrice, VenueExclusionReason};
use domain::SourceId;
use feature_registry::FiniteF64;
use fixed_decimal::{FixedDecimal, Price};

use super::FeatureComputationError;

pub const MAX_CROSS_VENUE_OBSERVATIONS: usize = 4_096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VenueDeviation {
    source: SourceId,
    relative_to_fair: FiniteF64,
    reference_depth_share: FiniteF64,
}

impl VenueDeviation {
    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn relative_to_fair(&self) -> FiniteF64 {
        self.relative_to_fair
    }

    pub const fn reference_depth_share(&self) -> FiniteF64 {
        self.reference_depth_share
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossVenueSnapshot {
    venues: Vec<VenueDeviation>,
    relative_mad: FiniteF64,
    relative_range: FiniteF64,
    depth_concentration: FiniteF64,
    healthy_venue_fraction: FiniteF64,
    stale_quote_count: usize,
    lineage_digest: [u8; 32],
}

impl CrossVenueSnapshot {
    /// Derives descriptive, non-executable metrics from catalog-bound
    /// consolidated book state.
    pub fn try_from_fair_price(fair_price: &FairPrice) -> Result<Self, FeatureComputationError> {
        let lineage = fair_price.lineage();
        let eligible_sources = lineage
            .eligible_sources()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        let included = lineage.included();
        if eligible_sources.len() < 2
            || included.len() < 2
            || included.iter().any(|venue| {
                venue.book_provenance().is_none()
                    || !venue.instrument_provenance().is_catalog_bound()
                    || !eligible_sources.contains(venue.source())
            })
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let fair = fair_price.price().value().to_f64_lossy_for_analysis();
        if !fair.is_finite() || fair <= 0.0 {
            return Err(FeatureComputationError::InvalidInput);
        }
        let depth_values = included
            .iter()
            .map(|venue| venue.adjusted_depth_reference().value())
            .collect::<Vec<_>>();
        let total_depth = depth_values
            .iter()
            .map(|depth| depth.to_f64_lossy_for_analysis())
            .sum::<f64>();
        if !total_depth.is_finite() || total_depth <= 0.0 {
            return Err(FeatureComputationError::ZeroDenominator);
        }
        let mut absolute_deviations = Vec::with_capacity(included.len());
        let mut adjusted_prices = Vec::with_capacity(included.len());
        let mut venues = Vec::with_capacity(included.len());
        for (venue, depth) in included.iter().zip(&depth_values) {
            let adjusted_price = venue.adjusted_price().value().to_f64_lossy_for_analysis();
            let relative = adjusted_price / fair - 1.0;
            adjusted_prices.push(adjusted_price);
            absolute_deviations.push(relative.abs());
            venues.push(VenueDeviation {
                source: venue.source().clone(),
                relative_to_fair: finite(relative)?,
                reference_depth_share: finite(depth.to_f64_lossy_for_analysis() / total_depth)?,
            });
        }
        absolute_deviations.sort_by(f64::total_cmp);
        adjusted_prices.sort_by(f64::total_cmp);
        let median_adjusted_price = median_sorted(&adjusted_prices);
        if !median_adjusted_price.is_finite() || median_adjusted_price <= 0.0 {
            return Err(FeatureComputationError::InvalidInput);
        }
        let relative_range = (adjusted_prices[adjusted_prices.len() - 1] - adjusted_prices[0])
            / median_adjusted_price;
        venues.sort_by_key(|venue| venue.source.to_string());
        let healthy_count = included
            .iter()
            .filter(|venue| venue.source_health() == quality::SourceHealthState::Healthy)
            .count();
        let stale_quote_count = lineage
            .excluded()
            .iter()
            .filter(|venue| venue.reason() == VenueExclusionReason::QuoteStale)
            .count();
        Ok(Self {
            venues,
            relative_mad: finite(median_sorted(&absolute_deviations))?,
            relative_range: finite(relative_range)?,
            depth_concentration: venue_depth_concentration(&depth_values)?,
            healthy_venue_fraction: finite(healthy_count as f64 / eligible_sources.len() as f64)?,
            stale_quote_count,
            lineage_digest: *lineage.digest(),
        })
    }

    pub fn venues(&self) -> &[VenueDeviation] {
        &self.venues
    }

    pub const fn relative_mad(&self) -> FiniteF64 {
        self.relative_mad
    }

    pub const fn relative_range(&self) -> FiniteF64 {
        self.relative_range
    }

    pub const fn depth_concentration(&self) -> FiniteF64 {
        self.depth_concentration
    }

    pub const fn healthy_venue_fraction(&self) -> FiniteF64 {
        self.healthy_venue_fraction
    }

    pub const fn stale_quote_count(&self) -> usize {
        self.stale_quote_count
    }

    pub const fn lineage_digest(&self) -> [u8; 32] {
        self.lineage_digest
    }

    pub const fn is_executable(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispersionClassification {
    IndicativeMidpriceOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndicativePriceDispersion {
    relative_range: FiniteF64,
    relative_mad: FiniteF64,
}

impl IndicativePriceDispersion {
    pub const fn classification(self) -> DispersionClassification {
        DispersionClassification::IndicativeMidpriceOnly
    }

    pub const fn is_executable(self) -> bool {
        false
    }

    pub const fn relative_range(self) -> FiniteF64 {
        self.relative_range
    }

    pub const fn relative_mad(self) -> FiniteF64 {
        self.relative_mad
    }
}

pub fn indicative_price_dispersion(
    prices: &[Price],
) -> Result<IndicativePriceDispersion, FeatureComputationError> {
    if prices.len() > MAX_CROSS_VENUE_OBSERVATIONS {
        return Err(FeatureComputationError::CapacityExceeded);
    }
    if prices.len() < 2 || prices.iter().any(|price| !price.value().is_positive()) {
        return Err(FeatureComputationError::InvalidInput);
    }
    let mut values = prices
        .iter()
        .map(|price| price.value().to_f64_lossy_for_analysis())
        .collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    let median = median_sorted(&values);
    if !median.is_finite() || median <= 0.0 {
        return Err(FeatureComputationError::InvalidInput);
    }
    let mut deviations = values
        .iter()
        .map(|value| (value - median).abs())
        .collect::<Vec<_>>();
    deviations.sort_by(f64::total_cmp);
    let range = (values[values.len() - 1] - values[0]) / median;
    let mad = median_sorted(&deviations) / median;
    Ok(IndicativePriceDispersion {
        relative_range: finite(range)?,
        relative_mad: finite(mad)?,
    })
}

pub fn venue_depth_concentration(
    depths: &[FixedDecimal],
) -> Result<FiniteF64, FeatureComputationError> {
    if depths.len() > MAX_CROSS_VENUE_OBSERVATIONS {
        return Err(FeatureComputationError::CapacityExceeded);
    }
    if depths.len() < 2 || depths.iter().any(|depth| depth.is_negative()) {
        return Err(FeatureComputationError::InvalidInput);
    }
    let values = depths
        .iter()
        .map(|depth| depth.to_f64_lossy_for_analysis())
        .collect::<Vec<_>>();
    let total = values.iter().sum::<f64>();
    if !total.is_finite() || total <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    finite(
        values
            .iter()
            .map(|depth| {
                let share = depth / total;
                share * share
            })
            .sum(),
    )
}

fn median_sorted(values: &[f64]) -> f64 {
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn finite(value: f64) -> Result<FiniteF64, FeatureComputationError> {
    FiniteF64::new(value).map_err(|_| FeatureComputationError::AnalyticalUnavailable)
}
