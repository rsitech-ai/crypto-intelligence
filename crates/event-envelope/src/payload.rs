//! Transport-neutral normalized records required by the event contract.

use crate::{BookLevel, EventError, QualityFlags, Side};
use domain::{AssetId, SourceId, UnixNanos, VenueId};
use fixed_decimal::{FixedDecimal, Notional, Price, Quantity, Rate};
use serde::{Deserialize, Serialize};

const MAX_TEXT: usize = 4_096;
const MAX_COLLECTION: usize = 10_000;
const MAX_PRIORITY: u8 = 3;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopOfBook {
    pub bid: Option<BookLevel>,
    pub ask: Option<BookLevel>,
}

impl TopOfBook {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        if self.bid.is_none() && self.ask.is_none() {
            return Err(EventError::InvalidPayload("top of book"));
        }
        if self
            .bid
            .as_ref()
            .zip(self.ask.as_ref())
            .is_some_and(|(bid, ask)| bid.price >= ask.price)
            || self
                .bid
                .as_ref()
                .into_iter()
                .chain(self.ask.as_ref())
                .any(|level| level.quantity.value().is_zero())
        {
            return Err(EventError::InvalidPayload("top of book state"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingObservation {
    pub funding_rate: Rate,
    pub observed_at: UnixNanos,
    pub next_funding_time: Option<UnixNanos>,
}

impl FundingObservation {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        if self
            .next_funding_time
            .is_some_and(|next| next <= self.observed_at)
        {
            return Err(EventError::InvalidPayload("funding time"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenInterestObservation {
    pub quantity: Quantity,
    pub quote_notional: Option<Notional>,
    pub observed_at: UnixNanos,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiquidationObservation {
    pub liquidation_id: String,
    pub price: Price,
    pub quantity: Quantity,
    pub side: Side,
}

impl LiquidationObservation {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_text(&self.liquidation_id, "liquidation id")?;
        if self.quantity.value().is_zero() || self.side == Side::Unknown {
            return Err(EventError::InvalidPayload("liquidation"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarkIndexObservation {
    pub mark_price: Price,
    pub index_price: Price,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FutureBasisObservation {
    pub future_price: Price,
    pub reference_price: Price,
    pub basis_rate: Rate,
    pub annualized_basis_rate: Option<Rate>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionTicker {
    pub bid_price: Option<Price>,
    pub ask_price: Option<Price>,
    pub mark_price: Price,
    pub mark_iv: Rate,
    pub delta: Rate,
    pub open_interest: Quantity,
}

impl OptionTicker {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        if self
            .bid_price
            .zip(self.ask_price)
            .is_some_and(|(bid, ask)| bid > ask)
            || self.mark_iv.value().is_negative()
        {
            return Err(EventError::InvalidPayload("option ticker"));
        }
        validate_unit_rate(self.delta, "option delta")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionTrade {
    pub trade_id: String,
    pub price: Price,
    pub quantity: Quantity,
    pub side: Side,
    pub implied_volatility: Option<Rate>,
}

impl OptionTrade {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_text(&self.trade_id, "option trade id")?;
        if self.quantity.value().is_zero()
            || self
                .implied_volatility
                .is_some_and(|value| value.value().is_negative())
        {
            return Err(EventError::InvalidPayload("option trade"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum VenueState {
    Operational = 0,
    Degraded = 1,
    Maintenance = 2,
    Auction = 3,
    Suspended = 4,
    Offline = 5,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueStatus {
    pub state: VenueState,
    pub observed_at: UnixNanos,
    pub message: Option<String>,
}

impl VenueStatus {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_optional_text(self.message.as_deref(), "venue status message")
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum FinalityState {
    Unconfirmed = 0,
    Confirmed = 1,
    Finalized = 2,
    Reverted = 3,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainBlock {
    pub chain: AssetId,
    pub block_hash: String,
    pub parent_hash: Option<String>,
    pub height: u64,
    pub block_time: UnixNanos,
    pub confirmation_depth: u64,
    pub finality: FinalityState,
    pub transaction_count: u64,
}

impl ChainBlock {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_text(&self.block_hash, "block hash")?;
        validate_optional_text(self.parent_hash.as_deref(), "parent hash")?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainTransactionAggregate {
    pub chain: AssetId,
    pub interval_start: UnixNanos,
    pub interval_end: UnixNanos,
    pub transaction_count: u64,
    pub transferred_value: Option<Notional>,
    pub fee_total: Option<Notional>,
}

impl ChainTransactionAggregate {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_interval(
            self.interval_start,
            self.interval_end,
            "chain transaction interval",
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainMempoolObservation {
    pub chain: AssetId,
    pub observed_at: UnixNanos,
    pub transaction_count: u64,
    pub virtual_size: u64,
    pub total_fees: Option<Notional>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainMetric {
    pub chain: AssetId,
    pub metric_name: String,
    pub value: FixedDecimal,
    pub observed_at: UnixNanos,
}

impl ChainMetric {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_text(&self.metric_name, "chain metric name")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalMarketObservation {
    pub market_id: String,
    pub value: FixedDecimal,
    pub observed_at: UnixNanos,
    pub source_native_id: Option<String>,
}

impl ExternalMarketObservation {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_text(&self.market_id, "external market id")?;
        validate_optional_text(self.source_native_id.as_deref(), "source native id")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredFact {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredEvent {
    pub structured_event_id: String,
    pub event_type: String,
    pub affected_assets: Vec<AssetId>,
    pub affected_venues: Vec<VenueId>,
    pub affected_protocols: Vec<String>,
    pub source_identity: SourceId,
    pub source_reliability_ppm: u32,
    pub publication_time: UnixNanos,
    pub effective_time: Option<UnixNanos>,
    pub expected_end_time: Option<UnixNanos>,
    pub direction_prior: Rate,
    pub severity_ppm: u32,
    pub extraction_confidence_ppm: u32,
    pub human_verified: bool,
    pub source_document_hash: [u8; 32],
    pub extractor_model_id: String,
    pub extractor_prompt_version: String,
    pub extracted_facts: Vec<StructuredFact>,
}

impl StructuredEvent {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_text(&self.structured_event_id, "structured event id")?;
        validate_text(&self.event_type, "structured event type")?;
        validate_text(&self.extractor_model_id, "extractor model id")?;
        validate_text(&self.extractor_prompt_version, "extractor prompt version")?;
        validate_score(self.source_reliability_ppm, "source reliability")?;
        validate_score(self.severity_ppm, "severity")?;
        validate_score(self.extraction_confidence_ppm, "extraction confidence")?;
        validate_unit_rate(self.direction_prior, "direction prior")?;
        if self.source_document_hash == [0; 32]
            || self.affected_assets.len() > MAX_COLLECTION
            || self.affected_venues.len() > MAX_COLLECTION
            || self.affected_protocols.len() > MAX_COLLECTION
            || self.extracted_facts.len() > MAX_COLLECTION
            || self
                .effective_time
                .zip(self.expected_end_time)
                .is_some_and(|(effective, end)| end <= effective)
        {
            return Err(EventError::InvalidPayload("structured event"));
        }
        for protocol in &self.affected_protocols {
            validate_text(protocol, "affected protocol")?;
        }
        for fact in &self.extracted_facts {
            validate_text(&fact.key, "structured fact key")?;
            validate_text(&fact.value, "structured fact value")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataQualityObservation {
    pub component: String,
    pub observed_at: UnixNanos,
    pub score_ppm: u32,
    pub flags: QualityFlags,
    pub details: Option<String>,
}

impl DataQualityObservation {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_text(&self.component, "quality component")?;
        validate_score(self.score_ppm, "quality score")?;
        validate_optional_text(self.details.as_deref(), "quality details")
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AvailabilityState {
    Available = 0,
    Degraded = 1,
    Unavailable = 2,
    Abstained = 3,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PredictionRecord {
    pub forecast_id: String,
    pub issued_at: UnixNanos,
    pub entity_id: String,
    pub model_package_id: String,
    pub forecast_schema_version: u32,
    pub prediction_event_type: String,
    pub horizon_seconds: u64,
    pub calibrated_probability_ppm: u32,
    pub raw_score: Rate,
    pub base_rate_ppm: u32,
    pub lower_uncertainty_ppm: u32,
    pub upper_uncertainty_ppm: u32,
    pub availability: AvailabilityState,
    pub quality_score_ppm: u32,
    pub applicability_score_ppm: u32,
    pub scenario_summary_id: String,
    pub evidence_bundle_id: String,
    pub supersedes_forecast_id: Option<String>,
}

impl PredictionRecord {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        for (value, field) in [
            (&self.forecast_id, "forecast id"),
            (&self.entity_id, "forecast entity id"),
            (&self.model_package_id, "model package id"),
            (&self.prediction_event_type, "prediction event type"),
            (&self.scenario_summary_id, "scenario summary id"),
            (&self.evidence_bundle_id, "evidence bundle id"),
        ] {
            validate_text(value, field)?;
        }
        validate_optional_text(
            self.supersedes_forecast_id.as_deref(),
            "superseded forecast id",
        )?;
        for (value, field) in [
            (self.calibrated_probability_ppm, "calibrated probability"),
            (self.base_rate_ppm, "base rate"),
            (self.lower_uncertainty_ppm, "lower uncertainty"),
            (self.upper_uncertainty_ppm, "upper uncertainty"),
            (self.quality_score_ppm, "forecast quality"),
            (self.applicability_score_ppm, "forecast applicability"),
        ] {
            validate_score(value, field)?;
        }
        if self.forecast_schema_version == 0
            || self.horizon_seconds == 0
            || self.lower_uncertainty_ppm > self.calibrated_probability_ppm
            || self.calibrated_probability_ppm > self.upper_uncertainty_ppm
        {
            return Err(EventError::InvalidPayload("prediction record"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AlertStatus {
    Triggered = 0,
    Updated = 1,
    Recovered = 2,
    Acknowledged = 3,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertEvent {
    pub alert_id: String,
    pub rule_id: String,
    pub status: AlertStatus,
    pub priority: u8,
    pub triggered_at: UnixNanos,
    pub recovered_at: Option<UnixNanos>,
    pub forecast_id: Option<String>,
    pub message: String,
}

impl AlertEvent {
    pub(crate) fn validate(&self) -> Result<(), EventError> {
        validate_text(&self.alert_id, "alert id")?;
        validate_text(&self.rule_id, "alert rule id")?;
        validate_text(&self.message, "alert message")?;
        validate_optional_text(self.forecast_id.as_deref(), "alert forecast id")?;
        if self.priority > MAX_PRIORITY
            || self
                .recovered_at
                .is_some_and(|recovered| recovered < self.triggered_at)
            || (self.status == AlertStatus::Recovered && self.recovered_at.is_none())
            || (self.status != AlertStatus::Recovered && self.recovered_at.is_some())
        {
            return Err(EventError::InvalidPayload("alert event"));
        }
        Ok(())
    }
}

pub(crate) fn validate_score(value: u32, field: &'static str) -> Result<(), EventError> {
    if value > 1_000_000 {
        Err(EventError::InvalidPayload(field))
    } else {
        Ok(())
    }
}

fn validate_interval(
    start: UnixNanos,
    end: UnixNanos,
    field: &'static str,
) -> Result<(), EventError> {
    if end <= start {
        Err(EventError::InvalidPayload(field))
    } else {
        Ok(())
    }
}

fn validate_unit_rate(value: Rate, field: &'static str) -> Result<(), EventError> {
    let negative_one = FixedDecimal::new(-1, 0).map_err(|_| EventError::InvalidPayload(field))?;
    let one = FixedDecimal::new(1, 0).map_err(|_| EventError::InvalidPayload(field))?;
    if value.value() < negative_one || value.value() > one {
        Err(EventError::InvalidPayload(field))
    } else {
        Ok(())
    }
}

fn validate_optional_text(value: Option<&str>, field: &'static str) -> Result<(), EventError> {
    if let Some(value) = value {
        validate_text(value, field)?;
    }
    Ok(())
}

fn validate_text(value: &str, field: &'static str) -> Result<(), EventError> {
    if value.is_empty()
        || value.len() > MAX_TEXT
        || value.chars().any(char::is_control)
        || value.trim() != value
    {
        Err(EventError::InvalidPayload(field))
    } else {
        Ok(())
    }
}
