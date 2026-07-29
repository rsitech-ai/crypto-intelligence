//! Point-in-time derivatives and leverage calculations.

use std::collections::BTreeSet;

use connector_core::{
    Completeness, CompletenessReason, DerivativeNormalizationReceipt, DerivativeStream,
};
use domain::{InstrumentDefinition, UnixNanos};
use event_envelope::{EventType, QualityFlags, UncheckedEventPayload};
use feature_registry::FiniteF64;
use fixed_decimal::FixedDecimal;

use crate::{Finalization, FinalizationDecision, WatermarkTracker};

use super::FeatureComputationError;

#[cfg(test)]
const SECONDS_PER_YEAR: f64 = 365.0 * 86_400.0;
pub const MAX_DERIVATIVE_WINDOW_OBSERVATIONS: usize = 100_000;

#[cfg(test)]
fn annualized_basis(
    future: fixed_decimal::Price,
    spot: fixed_decimal::Price,
    seconds_to_expiry: i64,
) -> Result<FiniteF64, FeatureComputationError> {
    if !future.value().is_positive() || !spot.value().is_positive() || seconds_to_expiry <= 0 {
        return Err(FeatureComputationError::InvalidInput);
    }
    let future = future.value().to_f64_lossy_for_analysis();
    let spot = spot.value().to_f64_lossy_for_analysis();
    finite((future / spot - 1.0) / (seconds_to_expiry as f64 / SECONDS_PER_YEAR))
}

#[cfg(test)]
fn open_interest_destruction(
    price_return: f64,
    open_interest_change: f64,
) -> Result<FiniteF64, FeatureComputationError> {
    if !price_return.is_finite() || !open_interest_change.is_finite() {
        return Err(FeatureComputationError::InvalidInput);
    }
    finite((-open_interest_change).max(0.0) * price_return.abs())
}

pub fn mark_index_divergence(
    receipt: &DerivativeNormalizationReceipt,
) -> Result<FiniteF64, FeatureComputationError> {
    validate_receipt_stream(receipt, DerivativeStream::MarkIndex)?;
    let UncheckedEventPayload::MarkIndexObservation(observation) =
        receipt.event().payload().as_unchecked()
    else {
        return Err(FeatureComputationError::UntrustedInput);
    };
    let mark = observation.mark_price.value().to_f64_lossy_for_analysis();
    let index = observation.index_price.value().to_f64_lossy_for_analysis();
    if index <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    finite(mark / index - 1.0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PredictedFundingRate {
    rate: FixedDecimal,
    observed_at: UnixNanos,
    next_funding_time: UnixNanos,
}

impl PredictedFundingRate {
    pub const fn rate(self) -> FixedDecimal {
        self.rate
    }

    pub const fn observed_at(self) -> UnixNanos {
        self.observed_at
    }

    pub const fn next_funding_time(self) -> UnixNanos {
        self.next_funding_time
    }
}

pub fn predicted_funding_rate(
    receipt: &DerivativeNormalizationReceipt,
) -> Result<PredictedFundingRate, FeatureComputationError> {
    validate_receipt_stream(receipt, DerivativeStream::Funding)?;
    let UncheckedEventPayload::FundingObservation(observation) =
        receipt.event().payload().as_unchecked()
    else {
        return Err(FeatureComputationError::UntrustedInput);
    };
    let next_funding_time = observation
        .next_funding_time
        .ok_or(FeatureComputationError::SourceNotSupported)?;
    Ok(PredictedFundingRate {
        rate: observation.funding_rate.value(),
        observed_at: observation.observed_at,
        next_funding_time,
    })
}

pub fn funding_change(
    previous: &DerivativeNormalizationReceipt,
    current: &DerivativeNormalizationReceipt,
) -> Result<FixedDecimal, FeatureComputationError> {
    validate_receipt_pair(previous, current, DerivativeStream::Funding)?;
    let UncheckedEventPayload::FundingObservation(previous_observation) =
        previous.event().payload().as_unchecked()
    else {
        return Err(FeatureComputationError::UntrustedInput);
    };
    let UncheckedEventPayload::FundingObservation(current_observation) =
        current.event().payload().as_unchecked()
    else {
        return Err(FeatureComputationError::UntrustedInput);
    };
    if current_observation.observed_at <= previous_observation.observed_at {
        return Err(FeatureComputationError::NonMonotonicTime);
    }
    current_observation
        .funding_rate
        .value()
        .checked_sub(previous_observation.funding_rate.value())
        .map_err(|_| FeatureComputationError::CapacityExceeded)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpenInterestChange {
    native_change: FixedDecimal,
    current_quote_notional: Option<FixedDecimal>,
    relative_change: FiniteF64,
}

impl OpenInterestChange {
    pub const fn native_change(self) -> FixedDecimal {
        self.native_change
    }

    pub const fn current_quote_notional(self) -> Option<FixedDecimal> {
        self.current_quote_notional
    }

    pub const fn relative_change(self) -> FiniteF64 {
        self.relative_change
    }
}

pub fn open_interest_change(
    previous: &DerivativeNormalizationReceipt,
    current: &DerivativeNormalizationReceipt,
    instrument: &InstrumentDefinition,
) -> Result<OpenInterestChange, FeatureComputationError> {
    validate_receipt_pair(previous, current, DerivativeStream::OpenInterest)?;
    if previous.instrument_definition() != instrument
        || current.instrument_definition() != instrument
    {
        return Err(FeatureComputationError::UntrustedInput);
    }
    let previous_metadata = previous.event().metadata().as_unchecked();
    let current_metadata = current.event().metadata().as_unchecked();
    if previous_metadata.instrument_id.as_ref() != Some(instrument.id())
        || current_metadata.instrument_id.as_ref() != Some(instrument.id())
    {
        return Err(FeatureComputationError::MixedEntity);
    }
    let UncheckedEventPayload::OpenInterestObservation(previous_observation) =
        previous.event().payload().as_unchecked()
    else {
        return Err(FeatureComputationError::UntrustedInput);
    };
    let UncheckedEventPayload::OpenInterestObservation(current_observation) =
        current.event().payload().as_unchecked()
    else {
        return Err(FeatureComputationError::UntrustedInput);
    };
    if current_observation.observed_at <= previous_observation.observed_at {
        return Err(FeatureComputationError::NonMonotonicTime);
    }
    let previous_quantity = previous_observation.quantity.value();
    let current_quantity = current_observation.quantity.value();
    if previous_quantity.is_zero() {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let native_change = current_quantity
        .checked_sub(previous_quantity)
        .map_err(|_| FeatureComputationError::CapacityExceeded)?;
    let current_quote_notional = current_observation
        .quote_notional
        .map(fixed_decimal::Notional::value);
    let relative_change = current_quantity.to_f64_lossy_for_analysis()
        / previous_quantity.to_f64_lossy_for_analysis()
        - 1.0;
    Ok(OpenInterestChange {
        native_change,
        current_quote_notional,
        relative_change: finite(relative_change)?,
    })
}

#[cfg(test)]
mod tests {
    use super::{annualized_basis, open_interest_destruction};
    use fixed_decimal::{FixedDecimal, Price};

    fn price(value: &str) -> Price {
        Price::new(FixedDecimal::parse_canonical(value).expect("decimal")).expect("price")
    }

    #[test]
    fn pure_annualized_basis_kernel_rejects_expired_contracts() {
        let value =
            annualized_basis(price("101"), price("100"), 30 * 86_400).expect("valid dated basis");
        assert!((value.value() - 0.121_666_666_666_666_77).abs() < 1e-12);
        assert!(annualized_basis(price("101"), price("100"), 0).is_err());
    }

    #[test]
    fn pure_open_interest_destruction_kernel_requires_a_contraction() {
        let destruction =
            open_interest_destruction(-0.04, -0.25).expect("finite contraction and price move");
        assert!((destruction.value() - 0.01).abs() < f64::EPSILON);
        assert_eq!(
            open_interest_destruction(0.04, 0.25)
                .expect("growth is valid")
                .value(),
            0.0
        );
        assert!(open_interest_destruction(f64::NAN, -0.25).is_err());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiquidationCoverage {
    Complete,
    VenueReportedCompleteWithDeliveryUncertainty,
    VenueReportedAll {
        push_cadence_ms: u32,
        delivery_uncertainty: bool,
    },
    SampledLargestPerSymbolWindow {
        window_ms: u32,
    },
    Partial {
        reason: CompletenessReason,
    },
}

impl LiquidationCoverage {
    pub const fn is_complete(self) -> bool {
        matches!(
            self,
            Self::Complete
                | Self::VenueReportedAll {
                    delivery_uncertainty: false,
                    ..
                }
        )
    }

    pub const fn is_sampled(self) -> bool {
        matches!(self, Self::SampledLargestPerSymbolWindow { .. })
    }

    pub const fn sampling_window_ms(self) -> Option<u32> {
        match self {
            Self::SampledLargestPerSymbolWindow { window_ms } => Some(window_ms),
            _ => None,
        }
    }

    fn from_connector(value: Completeness) -> Result<Self, FeatureComputationError> {
        match value {
            Completeness::NotSupported => Err(FeatureComputationError::SourceNotSupported),
            Completeness::VenueReportedComplete {
                delivery_uncertainty: false,
            } => Ok(Self::Complete),
            Completeness::VenueReportedComplete {
                delivery_uncertainty: true,
            } => Ok(Self::VenueReportedCompleteWithDeliveryUncertainty),
            Completeness::VenueReportedAll {
                push_cadence_ms,
                delivery_uncertainty,
            } => Ok(Self::VenueReportedAll {
                push_cadence_ms: push_cadence_ms.get(),
                delivery_uncertainty,
            }),
            Completeness::SampledLargestPerSymbolWindow { window_ms } => {
                Ok(Self::SampledLargestPerSymbolWindow {
                    window_ms: window_ms.get(),
                })
            }
            Completeness::Partial { reason } => Ok(Self::Partial { reason }),
        }
    }

    fn downgrade_for_partial_record(self) -> Self {
        if self.is_complete() {
            Self::Partial {
                reason: CompletenessReason::HistoricalGap,
            }
        } else {
            self
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiquidationNotionalInterpretation {
    CompleteObservedFlow,
    ObservedLowerBound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiquidationVelocity {
    observed_count: usize,
    observed_quote_notional: FixedDecimal,
    observed_count_per_second: FiniteF64,
    coverage: LiquidationCoverage,
    notional_interpretation: LiquidationNotionalInterpretation,
}

impl LiquidationVelocity {
    pub const fn observed_count(self) -> usize {
        self.observed_count
    }

    pub const fn observed_quote_notional(self) -> FixedDecimal {
        self.observed_quote_notional
    }

    pub const fn observed_count_per_second(self) -> FiniteF64 {
        self.observed_count_per_second
    }

    pub const fn coverage(self) -> LiquidationCoverage {
        self.coverage
    }

    pub const fn notional_interpretation(self) -> LiquidationNotionalInterpretation {
        self.notional_interpretation
    }
}

pub fn liquidation_velocity(
    receipts: &[DerivativeNormalizationReceipt],
    instrument: &InstrumentDefinition,
    tracker: &WatermarkTracker,
    decision: FinalizationDecision,
) -> Result<LiquidationVelocity, FeatureComputationError> {
    if receipts.is_empty() {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    if receipts.len() > MAX_DERIVATIVE_WINDOW_OBSERVATIONS {
        return Err(FeatureComputationError::CapacityExceeded);
    }
    let window = decision.window();
    let entity = feature_registry::FeatureEntity::Instrument(instrument.id().clone());
    if decision.state() != Finalization::Final
        || !decision.matches_entity(&entity)
        || tracker.coverage_for_decision(decision).is_none()
    {
        return Err(FeatureComputationError::UntrustedInput);
    }
    let required_source = receipts[0].event().metadata().as_unchecked().source.clone();
    if !decision.matches_required_sources(std::slice::from_ref(&required_source)) {
        return Err(FeatureComputationError::UntrustedInput);
    }
    let duration_ns = window
        .end()
        .value()
        .checked_sub(window.start().value())
        .ok_or(FeatureComputationError::InvalidInput)?;
    if duration_ns <= 0 {
        return Err(FeatureComputationError::InvalidInput);
    }

    let first_completeness = receipts[0].completeness();
    let first_connector_version = receipts[0].connector_version();
    let first_catalog_authority = receipts[0].catalog_authority();
    let mut coverage = LiquidationCoverage::from_connector(first_completeness)?;
    let mut observed_quote_notional =
        FixedDecimal::new(0, 0).map_err(|_| FeatureComputationError::InvalidInput)?;
    let mut expected_source = None;
    let mut lineage = BTreeSet::new();

    for receipt in receipts {
        if receipt.stream() != DerivativeStream::Liquidation
            || receipt.completeness() != first_completeness
            || receipt.connector_version() != first_connector_version
            || receipt.catalog_authority() != first_catalog_authority
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let event = receipt.event();
        event
            .verify()
            .map_err(|_| FeatureComputationError::UntrustedInput)?;
        if !lineage.insert(*event.id().as_bytes()) {
            return Err(FeatureComputationError::DuplicateLineage);
        }
        if event.event_type() != EventType::LiquidationObservation {
            return Err(FeatureComputationError::InvalidInput);
        }
        let metadata = event.metadata().as_unchecked();
        if receipt.instrument_definition() != instrument
            || metadata.instrument_id.as_ref() != Some(instrument.id())
        {
            return Err(FeatureComputationError::MixedEntity);
        }
        if expected_source
            .as_ref()
            .is_some_and(|source| source != &metadata.source)
        {
            return Err(FeatureComputationError::MixedEntity);
        }
        expected_source.get_or_insert_with(|| metadata.source.clone());
        let event_time = metadata
            .exchange_transaction_timestamp
            .or(metadata.exchange_timestamp)
            .ok_or(FeatureComputationError::InvalidInput)?;
        if event_time < window.start() || event_time >= window.end() {
            return Err(FeatureComputationError::OutsideWindow);
        }
        if metadata.quality_flags.bits() & QualityFlags::PARTIAL.bits() != 0 {
            coverage = coverage.downgrade_for_partial_record();
        }
        let UncheckedEventPayload::LiquidationObservation(observation) =
            event.payload().as_unchecked()
        else {
            return Err(FeatureComputationError::InvalidInput);
        };
        let notional = instrument
            .quote_notional(observation.price, observation.quantity)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        observed_quote_notional = observed_quote_notional
            .checked_add(notional.value())
            .map_err(|_| FeatureComputationError::CapacityExceeded)?;
    }

    let count_per_second = receipts.len() as f64 / (duration_ns as f64 / 1_000_000_000.0);
    if coverage.is_complete() {
        return Err(FeatureComputationError::UntrustedInput);
    }
    Ok(LiquidationVelocity {
        observed_count: receipts.len(),
        observed_quote_notional,
        observed_count_per_second: finite(count_per_second)?,
        coverage,
        notional_interpretation: LiquidationNotionalInterpretation::ObservedLowerBound,
    })
}

fn finite(value: f64) -> Result<FiniteF64, FeatureComputationError> {
    FiniteF64::new(value).map_err(|_| FeatureComputationError::AnalyticalUnavailable)
}

fn validate_receipt_stream(
    receipt: &DerivativeNormalizationReceipt,
    expected: DerivativeStream,
) -> Result<(), FeatureComputationError> {
    if receipt.stream() != expected
        || receipt.connector_version().is_empty()
        || receipt.catalog_digest() == &[0; 32]
        || receipt.definition_hash() == &[0; 32]
        || receipt.catalog_as_known_at()
            > receipt
                .event()
                .metadata()
                .as_unchecked()
                .normalization_timestamp
        || receipt
            .event()
            .metadata()
            .as_unchecked()
            .instrument_id
            .as_ref()
            != Some(receipt.instrument_definition().id())
        || receipt.event().verify().is_err()
    {
        return Err(FeatureComputationError::UntrustedInput);
    }
    Ok(())
}

fn validate_receipt_pair(
    previous: &DerivativeNormalizationReceipt,
    current: &DerivativeNormalizationReceipt,
    expected: DerivativeStream,
) -> Result<(), FeatureComputationError> {
    validate_receipt_stream(previous, expected)?;
    validate_receipt_stream(current, expected)?;
    let previous_metadata = previous.event().metadata().as_unchecked();
    let current_metadata = current.event().metadata().as_unchecked();
    if previous.connector_version() != current.connector_version()
        || previous.completeness() != current.completeness()
        || previous.catalog_authority() != current.catalog_authority()
        || previous.instrument_definition() != current.instrument_definition()
        || previous_metadata.source != current_metadata.source
        || previous_metadata.instrument_id != current_metadata.instrument_id
    {
        return Err(FeatureComputationError::MixedEntity);
    }
    Ok(())
}
