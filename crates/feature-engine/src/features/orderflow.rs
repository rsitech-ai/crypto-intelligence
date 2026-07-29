//! Bounded deterministic order-flow observations with explicit side authority.

use domain::{InstrumentId, ProductType, SourceId, SourceKind, UnixNanos};
use event_envelope::{EventEnvelope, Side, UncheckedEventPayload};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{BookSession, BookSnapshotView};
use std::collections::{BTreeMap, BTreeSet};

use super::{BookEvidencePolicy, BookStateEvidence, FeatureComputationError};

pub const MAX_FLOW_TRADES: usize = 100_000;
const MAX_RULE_ID_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AggressorSide {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleSemantics {
    AggregateL2,
    CertifiedL3,
    LicenseRestrictedL3,
}

pub const fn require_order_lifecycle_semantics(
    semantics: LifecycleSemantics,
) -> Result<(), FeatureComputationError> {
    match semantics {
        LifecycleSemantics::AggregateL2 => Err(FeatureComputationError::SourceNotSupported),
        LifecycleSemantics::CertifiedL3 => Ok(()),
        LifecycleSemantics::LicenseRestrictedL3 => {
            Err(FeatureComputationError::PrivacyOrLicenseRestriction)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleAction {
    Place,
    Cancel,
    Modify,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleCapability {
    schema_id: String,
    schema_version: u32,
    certification_id: String,
    certification_version: u32,
    license_authority_id: String,
    license_authority_version: u32,
}

impl LifecycleCapability {
    pub fn new(
        schema_id: impl Into<String>,
        schema_version: u32,
        certification_id: impl Into<String>,
        certification_version: u32,
        license_authority_id: impl Into<String>,
        license_authority_version: u32,
    ) -> Result<Self, FeatureComputationError> {
        let schema_id = schema_id.into();
        let certification_id = certification_id.into();
        let license_authority_id = license_authority_id.into();
        if schema_version == 0
            || certification_version == 0
            || license_authority_version == 0
            || !valid_bounded_identity(&schema_id)
            || !valid_bounded_identity(&certification_id)
            || !valid_bounded_identity(&license_authority_id)
        {
            return Err(FeatureComputationError::InvalidParameter);
        }
        Ok(Self {
            schema_id,
            schema_version,
            certification_id,
            certification_version,
            license_authority_id,
            license_authority_version,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleObservation {
    instrument: InstrumentId,
    session: BookSession,
    source_sequence: u64,
    order_id: String,
    capability: LifecycleCapability,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    lineage: [u8; 32],
    action: LifecycleAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleObservationInput {
    pub instrument: InstrumentId,
    pub session: BookSession,
    pub source_sequence: u64,
    pub order_id: String,
    pub capability: LifecycleCapability,
    pub event_time: UnixNanos,
    pub as_known_at: UnixNanos,
    pub lineage: [u8; 32],
    pub action: LifecycleAction,
}

impl LifecycleObservation {
    pub fn try_new(input: LifecycleObservationInput) -> Result<Self, FeatureComputationError> {
        if input.source_sequence == 0
            || !valid_bounded_identity(&input.order_id)
            || input.as_known_at < input.event_time
            || input.lineage == [0; 32]
        {
            return Err(FeatureComputationError::InvalidInput);
        }
        Ok(Self {
            instrument: input.instrument,
            session: input.session,
            source_sequence: input.source_sequence,
            order_id: input.order_id,
            capability: input.capability,
            event_time: input.event_time,
            as_known_at: input.as_known_at,
            lineage: input.lineage,
            action: input.action,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LifecycleWindow {
    observations: Vec<LifecycleObservation>,
    instrument: InstrumentId,
    session: BookSession,
    capability: LifecycleCapability,
    start: UnixNanos,
    end: UnixNanos,
    evaluation_time: UnixNanos,
}

impl LifecycleWindow {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mut observations: Vec<LifecycleObservation>,
        capacity: usize,
        start: UnixNanos,
        end: UnixNanos,
        evaluation_time: UnixNanos,
    ) -> Result<Self, FeatureComputationError> {
        validate_finalized_window(capacity, start, end, evaluation_time)?;
        if observations.is_empty() {
            return Err(FeatureComputationError::InsufficientHistory);
        }
        if observations.len() > capacity {
            return Err(FeatureComputationError::CapacityExceeded);
        }
        observations.sort_by_key(|observation| observation.source_sequence);
        let instrument = observations[0].instrument.clone();
        let session = observations[0].session;
        let capability = observations[0].capability.clone();
        let mut lineages = BTreeSet::new();
        for (index, observation) in observations.iter().enumerate() {
            if observation.instrument != instrument {
                return Err(FeatureComputationError::MixedEntity);
            }
            if observation.session != session || observation.capability != capability {
                return Err(FeatureComputationError::SourceHealthChanged);
            }
            if observation.event_time < start || observation.event_time >= end {
                return Err(FeatureComputationError::OutsideWindow);
            }
            if observation.as_known_at > evaluation_time {
                return Err(FeatureComputationError::FutureKnowledge);
            }
            if !lineages.insert(observation.lineage) {
                return Err(FeatureComputationError::DuplicateLineage);
            }
            if index > 0 && observation.as_known_at < observations[index - 1].as_known_at {
                return Err(FeatureComputationError::NonMonotonicTime);
            }
            if index > 0
                && observation.source_sequence
                    != observations[index - 1]
                        .source_sequence
                        .checked_add(1)
                        .ok_or(FeatureComputationError::SequenceGap)?
            {
                return Err(FeatureComputationError::SequenceGap);
            }
            if index > 0 && observation.event_time < observations[index - 1].event_time {
                return Err(FeatureComputationError::NonMonotonicTime);
            }
        }
        Ok(Self {
            observations,
            instrument,
            session,
            capability,
            start,
            end,
            evaluation_time,
        })
    }

    pub fn observed_empty(
        instrument: InstrumentId,
        session: BookSession,
        capability: LifecycleCapability,
        capacity: usize,
        start: UnixNanos,
        end: UnixNanos,
        evaluation_time: UnixNanos,
    ) -> Result<Self, FeatureComputationError> {
        validate_finalized_window(capacity, start, end, evaluation_time)?;
        Ok(Self {
            observations: Vec::new(),
            instrument,
            session,
            capability,
            start,
            end,
            evaluation_time,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleMetrics {
    placement_rate_per_second_bits: u64,
    cancellation_rate_per_second_bits: u64,
    modification_rate_per_second_bits: u64,
    cancellation_to_trade_ratio_bits: u64,
    quote_to_trade_ratio_bits: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleRates {
    placement_rate_per_second_bits: u64,
    cancellation_rate_per_second_bits: u64,
    modification_rate_per_second_bits: u64,
}

impl LifecycleRates {
    pub fn placement_rate_per_second(self) -> f64 {
        f64::from_bits(self.placement_rate_per_second_bits)
    }

    pub fn cancellation_rate_per_second(self) -> f64 {
        f64::from_bits(self.cancellation_rate_per_second_bits)
    }

    pub fn modification_rate_per_second(self) -> f64 {
        f64::from_bits(self.modification_rate_per_second_bits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleRatios {
    cancellation_to_trade_ratio_bits: u64,
    quote_to_trade_ratio_bits: u64,
}

impl LifecycleRatios {
    pub fn cancellation_to_trade_ratio(self) -> f64 {
        f64::from_bits(self.cancellation_to_trade_ratio_bits)
    }

    pub fn quote_to_trade_ratio(self) -> f64 {
        f64::from_bits(self.quote_to_trade_ratio_bits)
    }
}

impl LifecycleMetrics {
    pub fn placement_rate_per_second(self) -> f64 {
        f64::from_bits(self.placement_rate_per_second_bits)
    }

    pub fn cancellation_rate_per_second(self) -> f64 {
        f64::from_bits(self.cancellation_rate_per_second_bits)
    }

    pub fn modification_rate_per_second(self) -> f64 {
        f64::from_bits(self.modification_rate_per_second_bits)
    }

    pub fn cancellation_to_trade_ratio(self) -> f64 {
        f64::from_bits(self.cancellation_to_trade_ratio_bits)
    }

    pub fn quote_to_trade_ratio(self) -> f64 {
        f64::from_bits(self.quote_to_trade_ratio_bits)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct AggressorAuthority {
    rule_id: String,
    rule_version: u32,
    inferred: bool,
}

impl AggressorAuthority {
    pub fn supplied(
        rule_id: impl Into<String>,
        rule_version: u32,
    ) -> Result<Self, FeatureComputationError> {
        Self::new(rule_id, rule_version, false)
    }

    pub fn inferred(
        rule_id: impl Into<String>,
        rule_version: u32,
    ) -> Result<Self, FeatureComputationError> {
        Self::new(rule_id, rule_version, true)
    }

    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub const fn rule_version(&self) -> u32 {
        self.rule_version
    }

    pub const fn is_inferred(&self) -> bool {
        self.inferred
    }

    fn new(
        rule_id: impl Into<String>,
        rule_version: u32,
        inferred: bool,
    ) -> Result<Self, FeatureComputationError> {
        let rule_id = rule_id.into();
        if rule_version == 0
            || rule_id.is_empty()
            || rule_id.len() > MAX_RULE_ID_BYTES
            || rule_id.trim() != rule_id
            || !rule_id.is_ascii()
            || rule_id.chars().any(char::is_control)
        {
            return Err(FeatureComputationError::InvalidParameter);
        }
        Ok(Self {
            rule_id,
            rule_version,
            inferred,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlowTradeObservation {
    instrument: InstrumentId,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    lineage: [u8; 32],
    price: Price,
    quantity: Quantity,
    side: AggressorSide,
    authority: AggressorAuthority,
    authenticated: bool,
    source: Option<SourceId>,
    quality_score_ppm: u32,
}

impl FlowTradeObservation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        instrument: InstrumentId,
        event_time: UnixNanos,
        as_known_at: UnixNanos,
        lineage: [u8; 32],
        price: Price,
        quantity: Quantity,
        side: AggressorSide,
        authority: AggressorAuthority,
    ) -> Result<Self, FeatureComputationError> {
        if as_known_at < event_time || lineage == [0; 32] || quantity.value().is_zero() {
            return Err(FeatureComputationError::InvalidInput);
        }
        Ok(Self {
            instrument,
            event_time,
            as_known_at,
            lineage,
            price,
            quantity,
            side,
            authority,
            authenticated: false,
            source: None,
            quality_score_ppm: 0,
        })
    }

    fn try_from_authenticated_event(
        event: &EventEnvelope,
        authority: AggressorAuthority,
    ) -> Result<Self, FeatureComputationError> {
        event
            .verify()
            .map_err(|_| FeatureComputationError::UntrustedInput)?;
        let metadata = event.metadata().as_unchecked();
        let instrument = metadata
            .instrument_id
            .clone()
            .ok_or(FeatureComputationError::MixedEntity)?;
        if metadata.source.kind() != SourceKind::Exchange
            || metadata.source.name() != instrument.venue().as_str()
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
        if metadata.normalization_timestamp < event_time {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        let side = match trade.side {
            Side::Buy => AggressorSide::Buy,
            Side::Sell => AggressorSide::Sell,
            Side::Unknown => return Err(FeatureComputationError::SourceNotSupported),
        };
        let mut observation = Self::new(
            instrument,
            event_time,
            metadata.normalization_timestamp,
            *event.id().as_bytes(),
            trade.price,
            trade.quantity,
            side,
            authority,
        )?;
        observation.authenticated = true;
        observation.source = Some(metadata.source.clone());
        observation.quality_score_ppm = metadata.quality_score_ppm;
        Ok(observation)
    }

    pub fn try_from_binance_receipt(
        receipt: &connector_binance::BinanceTradeNormalizationReceipt,
    ) -> Result<Self, FeatureComputationError> {
        Self::try_from_authenticated_event(
            receipt.event(),
            AggressorAuthority::supplied("binance-buyer-maker-inversion", 1)?,
        )
    }

    pub const fn instrument(&self) -> &InstrumentId {
        &self.instrument
    }

    pub const fn event_time(&self) -> UnixNanos {
        self.event_time
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn lineage(&self) -> [u8; 32] {
        self.lineage
    }

    pub const fn price(&self) -> Price {
        self.price
    }

    pub const fn quantity(&self) -> Quantity {
        self.quantity
    }

    pub const fn side(&self) -> AggressorSide {
        self.side
    }

    pub const fn authority(&self) -> &AggressorAuthority {
        &self.authority
    }

    pub const fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    pub const fn source(&self) -> Option<&SourceId> {
        self.source.as_ref()
    }

    pub const fn quality_score_ppm(&self) -> u32 {
        self.quality_score_ppm
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TradeFlowWindow {
    observations: Vec<FlowTradeObservation>,
    instrument: InstrumentId,
    authorities: BTreeSet<AggressorAuthority>,
    start: UnixNanos,
    end: UnixNanos,
    evaluation_time: UnixNanos,
}

impl TradeFlowWindow {
    pub fn new(
        mut observations: Vec<FlowTradeObservation>,
        capacity: usize,
        start: UnixNanos,
        end: UnixNanos,
        evaluation_time: UnixNanos,
    ) -> Result<Self, FeatureComputationError> {
        validate_finalized_window(capacity, start, end, evaluation_time)?;
        if observations.is_empty() {
            return Err(FeatureComputationError::InsufficientHistory);
        }
        if observations.len() > capacity {
            return Err(FeatureComputationError::CapacityExceeded);
        }
        observations.sort_by_key(|observation| (observation.event_time, observation.lineage));
        let instrument = observations[0].instrument.clone();
        let mut lineages = BTreeSet::new();
        let mut authorities = BTreeSet::new();
        for (index, observation) in observations.iter().enumerate() {
            if observation.instrument != instrument {
                return Err(FeatureComputationError::MixedEntity);
            }
            if observation.event_time < start || observation.event_time >= end {
                return Err(FeatureComputationError::OutsideWindow);
            }
            if observation.as_known_at > evaluation_time {
                return Err(FeatureComputationError::FutureKnowledge);
            }
            if !lineages.insert(observation.lineage) {
                return Err(FeatureComputationError::DuplicateLineage);
            }
            if index > 0 && observation.as_known_at < observations[index - 1].as_known_at {
                return Err(FeatureComputationError::NonMonotonicTime);
            }
            authorities.insert(observation.authority.clone());
        }
        Ok(Self {
            observations,
            instrument,
            authorities,
            start,
            end,
            evaluation_time,
        })
    }

    pub fn observed_empty(
        instrument: InstrumentId,
        capacity: usize,
        start: UnixNanos,
        end: UnixNanos,
        evaluation_time: UnixNanos,
    ) -> Result<Self, FeatureComputationError> {
        validate_finalized_window(capacity, start, end, evaluation_time)?;
        Ok(Self {
            observations: Vec::new(),
            instrument,
            authorities: BTreeSet::new(),
            start,
            end,
            evaluation_time,
        })
    }

    pub fn observations(&self) -> &[FlowTradeObservation] {
        &self.observations
    }

    pub const fn authorities(&self) -> &BTreeSet<AggressorAuthority> {
        &self.authorities
    }

    pub const fn evaluation_time(&self) -> UnixNanos {
        self.evaluation_time
    }

    pub const fn start(&self) -> UnixNanos {
        self.start
    }

    pub const fn end(&self) -> UnixNanos {
        self.end
    }

    pub const fn instrument(&self) -> &InstrumentId {
        &self.instrument
    }

    pub fn is_authenticated(&self) -> bool {
        !self.observations.is_empty()
            && self
                .observations
                .iter()
                .all(FlowTradeObservation::is_authenticated)
    }

    pub fn sources(&self) -> Vec<SourceId> {
        let mut sources = BTreeMap::new();
        for source in self
            .observations
            .iter()
            .filter_map(|observation| observation.source.as_ref())
        {
            sources
                .entry((source.kind() as u8, source.name(), source.generation()))
                .or_insert(source);
        }
        sources.into_values().cloned().collect()
    }

    pub fn minimum_quality_score_ppm(&self) -> Option<u32> {
        self.observations
            .iter()
            .map(|observation| observation.quality_score_ppm)
            .min()
    }

    pub fn maximum_as_known_at(&self) -> Option<UnixNanos> {
        self.observations
            .iter()
            .map(FlowTradeObservation::as_known_at)
            .max()
    }

    pub fn canonical_evidence_digest(&self) -> Result<[u8; 32], FeatureComputationError> {
        if !self.is_authenticated() {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crypto-intelligence/trade-flow-window-evidence/v1");
        hasher.update(&self.start.value().to_be_bytes());
        hasher.update(&self.end.value().to_be_bytes());
        hasher.update(&self.evaluation_time.value().to_be_bytes());
        for observation in &self.observations {
            hasher.update(&observation.lineage);
            hasher.update(&observation.event_time.value().to_be_bytes());
            hasher.update(&observation.as_known_at.value().to_be_bytes());
            hash_identity(&mut hasher, observation.price.to_string().as_bytes());
            hash_identity(&mut hasher, observation.quantity.to_string().as_bytes());
            hasher.update(&[match observation.side {
                AggressorSide::Buy => 1,
                AggressorSide::Sell => 2,
            }]);
            hash_identity(&mut hasher, observation.authority.rule_id.as_bytes());
            hasher.update(&observation.authority.rule_version.to_be_bytes());
            hasher.update(&[u8::from(observation.authority.inferred)]);
            let source = observation
                .source
                .as_ref()
                .ok_or(FeatureComputationError::UntrustedInput)?;
            hash_identity(&mut hasher, source.to_string().as_bytes());
            hasher.update(&observation.quality_score_ppm.to_be_bytes());
        }
        Ok(*hasher.finalize().as_bytes())
    }

    fn duration_ns(&self) -> Result<u64, FeatureComputationError> {
        elapsed_between(self.start, self.end)
    }
}

fn hash_identity(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

pub fn certified_lifecycle_metrics(
    lifecycle: &LifecycleWindow,
    trades: &TradeFlowWindow,
) -> Result<LifecycleMetrics, FeatureComputationError> {
    let rates = certified_lifecycle_rates(lifecycle)?;
    let ratios = certified_lifecycle_ratios(lifecycle, trades)?;
    Ok(LifecycleMetrics {
        placement_rate_per_second_bits: rates.placement_rate_per_second_bits,
        cancellation_rate_per_second_bits: rates.cancellation_rate_per_second_bits,
        modification_rate_per_second_bits: rates.modification_rate_per_second_bits,
        cancellation_to_trade_ratio_bits: ratios.cancellation_to_trade_ratio_bits,
        quote_to_trade_ratio_bits: ratios.quote_to_trade_ratio_bits,
    })
}

pub fn certified_lifecycle_rates(
    lifecycle: &LifecycleWindow,
) -> Result<LifecycleRates, FeatureComputationError> {
    let (placements, cancellations, modifications) = lifecycle_action_counts(lifecycle)?;
    let duration_ns = elapsed_between(lifecycle.start, lifecycle.end)?;
    let per_second = |count: usize| count as f64 * 1_000_000_000.0 / duration_ns as f64;
    Ok(LifecycleRates {
        placement_rate_per_second_bits: finite(per_second(placements))?.to_bits(),
        cancellation_rate_per_second_bits: finite(per_second(cancellations))?.to_bits(),
        modification_rate_per_second_bits: finite(per_second(modifications))?.to_bits(),
    })
}

pub fn certified_lifecycle_ratios(
    lifecycle: &LifecycleWindow,
    trades: &TradeFlowWindow,
) -> Result<LifecycleRatios, FeatureComputationError> {
    validate_lifecycle_trade_pair(lifecycle, trades)?;
    let (placements, cancellations, modifications) = lifecycle_action_counts(lifecycle)?;
    let quote_events = placements
        .checked_add(cancellations)
        .and_then(|count| count.checked_add(modifications))
        .ok_or(FeatureComputationError::CapacityExceeded)?;
    let trade_count = trades.observations.len();
    if trade_count == 0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    Ok(LifecycleRatios {
        cancellation_to_trade_ratio_bits: finite(cancellations as f64 / trade_count as f64)?
            .to_bits(),
        quote_to_trade_ratio_bits: finite(quote_events as f64 / trade_count as f64)?.to_bits(),
    })
}

fn validate_lifecycle_trade_pair(
    lifecycle: &LifecycleWindow,
    trades: &TradeFlowWindow,
) -> Result<(), FeatureComputationError> {
    if lifecycle.evaluation_time != trades.evaluation_time {
        return Err(FeatureComputationError::SourceHealthChanged);
    }
    if lifecycle.instrument != trades.instrument {
        return Err(FeatureComputationError::MixedEntity);
    }
    if lifecycle.start != trades.start || lifecycle.end != trades.end {
        return Err(FeatureComputationError::OutsideWindow);
    }
    let mut seen_lineages = lifecycle
        .observations
        .iter()
        .map(|observation| observation.lineage)
        .collect::<BTreeSet<_>>();
    for trade in &trades.observations {
        if trade.instrument != lifecycle.instrument {
            return Err(FeatureComputationError::MixedEntity);
        }
        if trade.event_time < lifecycle.start || trade.event_time >= lifecycle.end {
            return Err(FeatureComputationError::OutsideWindow);
        }
        if !seen_lineages.insert(trade.lineage) {
            return Err(FeatureComputationError::DuplicateLineage);
        }
    }
    Ok(())
}

fn lifecycle_action_counts(
    lifecycle: &LifecycleWindow,
) -> Result<(usize, usize, usize), FeatureComputationError> {
    let mut placements = 0_usize;
    let mut cancellations = 0_usize;
    let mut modifications = 0_usize;
    for observation in &lifecycle.observations {
        let count = match observation.action {
            LifecycleAction::Place => &mut placements,
            LifecycleAction::Cancel => &mut cancellations,
            LifecycleAction::Modify => &mut modifications,
        };
        *count = count
            .checked_add(1)
            .ok_or(FeatureComputationError::CapacityExceeded)?;
    }
    Ok((placements, cancellations, modifications))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopOfBookObservation {
    instrument: InstrumentId,
    session: BookSession,
    evidence_policy: BookEvidencePolicy,
    last_source_sequence: u64,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    lineage: [u8; 32],
    bid_price: Price,
    bid_quantity: Quantity,
    ask_price: Price,
    ask_quantity: Quantity,
    authenticated: bool,
}

impl TopOfBookObservation {
    pub fn from_snapshot(
        book: &BookSnapshotView,
        evidence_policy: BookEvidencePolicy,
        event_time: UnixNanos,
        as_known_at: UnixNanos,
        lineage: [u8; 32],
    ) -> Result<Self, FeatureComputationError> {
        super::orderbook::require_normal_book(book)?;
        if as_known_at < event_time || lineage == [0; 32] {
            return Err(FeatureComputationError::InvalidInput);
        }
        let bid = book
            .best_bid()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        let ask = book
            .best_ask()
            .ok_or(FeatureComputationError::UntrustedInput)?;
        Ok(Self {
            instrument: book.instrument().clone(),
            session: book.session(),
            evidence_policy,
            last_source_sequence: book.last_source_sequence(),
            event_time,
            as_known_at,
            lineage,
            bid_price: bid.price,
            bid_quantity: bid.quantity,
            ask_price: ask.price,
            ask_quantity: ask.quantity,
            authenticated: false,
        })
    }

    pub fn from_authenticated_snapshot(
        book: &BookSnapshotView,
        evidence_policy: BookEvidencePolicy,
        evidence: &BookStateEvidence,
    ) -> Result<Self, FeatureComputationError> {
        if !evidence.matches(book) {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let mut observation = Self::from_snapshot(
            book,
            evidence_policy,
            evidence.event_time(),
            evidence.as_known_at(),
            evidence.lineage(),
        )?;
        observation.authenticated = true;
        Ok(observation)
    }

    pub const fn event_time(&self) -> UnixNanos {
        self.event_time
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn lineage(&self) -> [u8; 32] {
        self.lineage
    }

    pub const fn is_authenticated(&self) -> bool {
        self.authenticated
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopOfBookWindow {
    observations: Vec<TopOfBookObservation>,
    evaluation_time: UnixNanos,
}

impl TopOfBookWindow {
    pub fn new(
        mut observations: Vec<TopOfBookObservation>,
        capacity: usize,
        evaluation_time: UnixNanos,
    ) -> Result<Self, FeatureComputationError> {
        if capacity == 0 || capacity > MAX_FLOW_TRADES {
            return Err(FeatureComputationError::InvalidParameter);
        }
        if observations.len() < 2 {
            return Err(FeatureComputationError::InsufficientHistory);
        }
        if observations.len() > capacity {
            return Err(FeatureComputationError::CapacityExceeded);
        }
        observations.sort_by_key(|observation| (observation.event_time, observation.lineage));
        let instrument = observations[0].instrument.clone();
        let session = observations[0].session;
        let mut lineages = BTreeSet::new();
        for (index, observation) in observations.iter().enumerate() {
            if observation.instrument != instrument {
                return Err(FeatureComputationError::MixedEntity);
            }
            if observation.session != session {
                return Err(FeatureComputationError::SourceHealthChanged);
            }
            if observation.evidence_policy != observations[0].evidence_policy {
                return Err(FeatureComputationError::SourceHealthChanged);
            }
            if observation.as_known_at > evaluation_time {
                return Err(FeatureComputationError::FutureKnowledge);
            }
            if !lineages.insert(observation.lineage) {
                return Err(FeatureComputationError::DuplicateLineage);
            }
            if index > 0
                && (observation.last_source_sequence
                    <= observations[index - 1].last_source_sequence
                    || observation.as_known_at < observations[index - 1].as_known_at)
            {
                return Err(FeatureComputationError::NonMonotonicTime);
            }
        }
        Ok(Self {
            observations,
            evaluation_time,
        })
    }

    pub fn observations(&self) -> &[TopOfBookObservation] {
        &self.observations
    }

    pub const fn evaluation_time(&self) -> UnixNanos {
        self.evaluation_time
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopOfBookSideStaleness {
    bid_quote_age_ns: u64,
    ask_quote_age_ns: u64,
    bid_stale_duration_ns: u64,
    ask_stale_duration_ns: u64,
}

impl TopOfBookSideStaleness {
    pub const fn bid_quote_age_ns(self) -> u64 {
        self.bid_quote_age_ns
    }

    pub const fn ask_quote_age_ns(self) -> u64 {
        self.ask_quote_age_ns
    }

    pub const fn bid_stale_duration_ns(self) -> u64 {
        self.bid_stale_duration_ns
    }

    pub const fn ask_stale_duration_ns(self) -> u64 {
        self.ask_stale_duration_ns
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BookQuoteSide {
    Bid,
    Ask,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuoteSideStaleness {
    quote_age_ns: u64,
    stale_duration_ns: u64,
}

impl QuoteSideStaleness {
    pub const fn quote_age_ns(self) -> u64 {
        self.quote_age_ns
    }

    pub const fn stale_duration_ns(self) -> u64 {
        self.stale_duration_ns
    }
}

pub fn top_of_book_side_staleness(
    window: &TopOfBookWindow,
    as_of_event_time: UnixNanos,
    stale_after_ns: u64,
) -> Result<TopOfBookSideStaleness, FeatureComputationError> {
    let bid = top_of_book_staleness_for_side(
        window,
        BookQuoteSide::Bid,
        as_of_event_time,
        stale_after_ns,
    )?;
    let ask = top_of_book_staleness_for_side(
        window,
        BookQuoteSide::Ask,
        as_of_event_time,
        stale_after_ns,
    )?;
    Ok(TopOfBookSideStaleness {
        bid_quote_age_ns: bid.quote_age_ns,
        ask_quote_age_ns: ask.quote_age_ns,
        bid_stale_duration_ns: bid.stale_duration_ns,
        ask_stale_duration_ns: ask.stale_duration_ns,
    })
}

pub fn top_of_book_staleness_for_side(
    window: &TopOfBookWindow,
    side: BookQuoteSide,
    as_of_event_time: UnixNanos,
    stale_after_ns: u64,
) -> Result<QuoteSideStaleness, FeatureComputationError> {
    if stale_after_ns == 0 {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let last = window
        .observations
        .last()
        .expect("top-of-book windows contain at least two observations");
    if as_of_event_time < last.event_time || as_of_event_time > window.evaluation_time {
        return Err(FeatureComputationError::FutureKnowledge);
    }
    let mut last_change = None;
    for pair in window.observations.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        let changed = match side {
            BookQuoteSide::Bid => {
                current.bid_price != previous.bid_price
                    || current.bid_quantity != previous.bid_quantity
            }
            BookQuoteSide::Ask => {
                current.ask_price != previous.ask_price
                    || current.ask_quantity != previous.ask_quantity
            }
        };
        if changed {
            last_change = Some(current.event_time);
        }
    }
    let quote_age_ns = elapsed_between(
        last_change.ok_or(FeatureComputationError::InsufficientHistory)?,
        as_of_event_time,
    )?;
    Ok(QuoteSideStaleness {
        quote_age_ns,
        stale_duration_ns: quote_age_ns.saturating_sub(stale_after_ns),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TradeShockThreshold {
    QuoteNotional(FixedDecimal),
    BaseQuantity(Quantity),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TradeShockWeighting {
    BaseQuantity,
    QuoteNotional,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TradeShock {
    instrument: InstrumentId,
    side: AggressorSide,
    threshold: TradeShockThreshold,
    observations: Vec<FlowTradeObservation>,
    start_event_time: UnixNanos,
    end_event_time: UnixNanos,
    availability_at: UnixNanos,
    evaluation_time: UnixNanos,
    realized_base_quantity: FixedDecimal,
    realized_quote_notional: FixedDecimal,
}

impl TradeShock {
    pub fn from_prefix(
        window: &TradeFlowWindow,
        threshold: TradeShockThreshold,
    ) -> Result<Self, FeatureComputationError> {
        require_spot_quote_notional(&window.instrument)?;
        let threshold_is_valid = match threshold {
            TradeShockThreshold::QuoteNotional(value) => value.is_positive(),
            TradeShockThreshold::BaseQuantity(value) => value.value().is_positive(),
        };
        if !threshold_is_valid {
            return Err(FeatureComputationError::InvalidParameter);
        }
        let first = window
            .observations
            .first()
            .ok_or(FeatureComputationError::InsufficientHistory)?;
        let side = first.side;
        let mut observations = Vec::new();
        let mut realized_base_quantity = zero();
        let mut realized_quote_notional = zero();
        for observation in &window.observations {
            if observation.side != side {
                break;
            }
            realized_base_quantity = realized_base_quantity
                .checked_add(observation.quantity.value())
                .map_err(|_| FeatureComputationError::InvalidInput)?;
            let notional = observation
                .price
                .value()
                .checked_mul(observation.quantity.value())
                .map_err(|_| FeatureComputationError::InvalidInput)?;
            realized_quote_notional = realized_quote_notional
                .checked_add(notional)
                .map_err(|_| FeatureComputationError::InvalidInput)?;
            observations.push(observation.clone());
            let reached = match threshold {
                TradeShockThreshold::QuoteNotional(target) => realized_quote_notional >= target,
                TradeShockThreshold::BaseQuantity(target) => {
                    realized_base_quantity >= target.value()
                }
            };
            if reached {
                let last = observations
                    .last()
                    .expect("a reached shock prefix contains at least one trade");
                return Ok(Self {
                    instrument: first.instrument.clone(),
                    side,
                    threshold,
                    start_event_time: first.event_time,
                    end_event_time: last.event_time,
                    availability_at: last.as_known_at,
                    evaluation_time: window.evaluation_time,
                    observations,
                    realized_base_quantity,
                    realized_quote_notional,
                });
            }
        }
        Err(FeatureComputationError::BelowLiquidityThreshold)
    }

    pub const fn side(&self) -> AggressorSide {
        self.side
    }

    pub const fn threshold(&self) -> TradeShockThreshold {
        self.threshold
    }

    pub const fn realized_base_quantity(&self) -> FixedDecimal {
        self.realized_base_quantity
    }

    pub const fn realized_quote_notional(&self) -> FixedDecimal {
        self.realized_quote_notional
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TradeShockResponse {
    signed_impact_bits: u64,
    effective_spread_bits: u64,
    realized_spread_bits: u64,
    adverse_selection_proxy_bits: u64,
}

impl TradeShockResponse {
    pub fn signed_impact(self) -> f64 {
        f64::from_bits(self.signed_impact_bits)
    }

    pub fn effective_spread(self) -> f64 {
        f64::from_bits(self.effective_spread_bits)
    }

    pub fn realized_spread(self) -> f64 {
        f64::from_bits(self.realized_spread_bits)
    }

    pub fn adverse_selection_proxy(self) -> f64 {
        f64::from_bits(self.adverse_selection_proxy_bits)
    }
}

pub fn trade_shock_response(
    shock: &TradeShock,
    quotes: &TopOfBookWindow,
    max_pre_age_ns: u64,
    response_horizon_ns: u64,
    response_tolerance_ns: u64,
    weighting: TradeShockWeighting,
) -> Result<TradeShockResponse, FeatureComputationError> {
    if max_pre_age_ns == 0 || quotes.evaluation_time != shock.evaluation_time {
        return Err(FeatureComputationError::InvalidParameter);
    }
    if quotes.observations[0].instrument != shock.instrument {
        return Err(FeatureComputationError::MixedEntity);
    }
    let shock_lineages = shock
        .observations
        .iter()
        .map(|observation| observation.lineage)
        .collect::<BTreeSet<_>>();
    if quotes
        .observations
        .iter()
        .any(|observation| shock_lineages.contains(&observation.lineage))
    {
        return Err(FeatureComputationError::DuplicateLineage);
    }
    let pre = quotes
        .observations
        .iter()
        .rev()
        .find(|observation| {
            observation.event_time <= shock.start_event_time
                && observation.as_known_at <= shock.availability_at
                && elapsed_between(observation.event_time, shock.start_event_time)
                    .is_ok_and(|age| age <= max_pre_age_ns)
        })
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    let response_start = add_ns(shock.end_event_time, response_horizon_ns)?;
    let response_end = add_ns(response_start, response_tolerance_ns)?;
    let response = quotes
        .observations
        .iter()
        .find(|observation| {
            observation.event_time >= response_start && observation.event_time <= response_end
        })
        .ok_or(FeatureComputationError::InsufficientHistory)?;
    let pre_midpoint_numerator = midpoint_numerator(pre)?;
    let response_midpoint_numerator = midpoint_numerator(response)?;
    let midpoint_change = response_midpoint_numerator
        .checked_sub(pre_midpoint_numerator)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let signed_midpoint_change = apply_side_sign(midpoint_change, shock.side)?;
    let signed_impact = finite(
        signed_midpoint_change.to_f64_lossy_for_analysis()
            / pre_midpoint_numerator.to_f64_lossy_for_analysis(),
    )?;

    let mut total_weight = zero();
    let mut effective_numerator = zero();
    let mut realized_numerator = zero();
    let two = FixedDecimal::new(2, 0).expect("two is a valid fixed decimal");
    for trade in &shock.observations {
        let weight = match weighting {
            TradeShockWeighting::BaseQuantity => trade.quantity.value(),
            TradeShockWeighting::QuoteNotional => trade
                .price
                .value()
                .checked_mul(trade.quantity.value())
                .map_err(|_| FeatureComputationError::InvalidInput)?,
        };
        total_weight = total_weight
            .checked_add(weight)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        let twice_price = trade
            .price
            .value()
            .checked_mul(two)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        let effective_component = twice_price
            .checked_sub(pre_midpoint_numerator)
            .and_then(|value| value.checked_mul(two))
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        let realized_component = twice_price
            .checked_sub(response_midpoint_numerator)
            .and_then(|value| value.checked_mul(two))
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        effective_numerator = effective_numerator
            .checked_add(
                apply_side_sign(effective_component, shock.side)?
                    .checked_mul(weight)
                    .map_err(|_| FeatureComputationError::InvalidInput)?,
            )
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        realized_numerator = realized_numerator
            .checked_add(
                apply_side_sign(realized_component, shock.side)?
                    .checked_mul(weight)
                    .map_err(|_| FeatureComputationError::InvalidInput)?,
            )
            .map_err(|_| FeatureComputationError::InvalidInput)?;
    }
    let denominator = pre_midpoint_numerator
        .checked_mul(total_weight)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    if denominator.is_zero() {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let effective_spread = finite(
        effective_numerator.to_f64_lossy_for_analysis() / denominator.to_f64_lossy_for_analysis(),
    )?;
    let realized_spread = finite(
        realized_numerator.to_f64_lossy_for_analysis() / denominator.to_f64_lossy_for_analysis(),
    )?;
    let adverse_selection = finite(effective_spread - realized_spread)?;
    Ok(TradeShockResponse {
        signed_impact_bits: signed_impact.to_bits(),
        effective_spread_bits: effective_spread.to_bits(),
        realized_spread_bits: realized_spread.to_bits(),
        adverse_selection_proxy_bits: adverse_selection.to_bits(),
    })
}

fn midpoint_numerator(
    observation: &TopOfBookObservation,
) -> Result<FixedDecimal, FeatureComputationError> {
    observation
        .bid_price
        .value()
        .checked_add(observation.ask_price.value())
        .map_err(|_| FeatureComputationError::InvalidInput)
}

fn apply_side_sign(
    value: FixedDecimal,
    side: AggressorSide,
) -> Result<FixedDecimal, FeatureComputationError> {
    match side {
        AggressorSide::Buy => Ok(value),
        AggressorSide::Sell => value
            .checked_neg()
            .map_err(|_| FeatureComputationError::InvalidInput),
    }
}

fn elapsed_between(start: UnixNanos, end: UnixNanos) -> Result<u64, FeatureComputationError> {
    end.value()
        .checked_sub(start.value())
        .and_then(|elapsed| u64::try_from(elapsed).ok())
        .ok_or(FeatureComputationError::NonMonotonicTime)
}

fn add_ns(time: UnixNanos, offset_ns: u64) -> Result<UnixNanos, FeatureComputationError> {
    let offset = i64::try_from(offset_ns).map_err(|_| FeatureComputationError::InvalidParameter)?;
    time.value()
        .checked_add(offset)
        .map(UnixNanos::new)
        .ok_or(FeatureComputationError::InvalidParameter)
}

fn valid_bounded_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RULE_ID_BYTES
        && value.trim() == value
        && value.is_ascii()
        && !value.chars().any(char::is_control)
}

fn validate_finalized_window(
    capacity: usize,
    start: UnixNanos,
    end: UnixNanos,
    evaluation_time: UnixNanos,
) -> Result<(), FeatureComputationError> {
    if capacity == 0 || capacity > MAX_FLOW_TRADES || start >= end {
        return Err(FeatureComputationError::InvalidParameter);
    }
    if evaluation_time < end {
        return Err(FeatureComputationError::WindowNotFinal);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AggressiveTradeTotals {
    buy_count: usize,
    sell_count: usize,
    buy_quantity: Quantity,
    sell_quantity: Quantity,
    buy_notional: FixedDecimal,
    sell_notional: FixedDecimal,
    signed_count_imbalance_bits: u64,
    signed_quantity_imbalance_bits: u64,
    signed_notional_imbalance_bits: u64,
    inferred_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AggressiveTradeSums {
    buy_count: usize,
    sell_count: usize,
    buy_quantity: Quantity,
    sell_quantity: Quantity,
    buy_notional: FixedDecimal,
    sell_notional: FixedDecimal,
    inferred_count: usize,
}

impl AggressiveTradeSums {
    pub const fn buy_count(self) -> usize {
        self.buy_count
    }

    pub const fn sell_count(self) -> usize {
        self.sell_count
    }

    pub const fn buy_quantity(self) -> Quantity {
        self.buy_quantity
    }

    pub const fn sell_quantity(self) -> Quantity {
        self.sell_quantity
    }

    pub const fn buy_notional(self) -> FixedDecimal {
        self.buy_notional
    }

    pub const fn sell_notional(self) -> FixedDecimal {
        self.sell_notional
    }

    pub const fn inferred_count(self) -> usize {
        self.inferred_count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AggressiveTradeImbalances {
    signed_count_imbalance_bits: u64,
    signed_quantity_imbalance_bits: u64,
    signed_notional_imbalance_bits: u64,
}

impl AggressiveTradeImbalances {
    pub fn signed_count_imbalance(self) -> f64 {
        f64::from_bits(self.signed_count_imbalance_bits)
    }

    pub fn signed_quantity_imbalance(self) -> f64 {
        f64::from_bits(self.signed_quantity_imbalance_bits)
    }

    pub fn signed_notional_imbalance(self) -> f64 {
        f64::from_bits(self.signed_notional_imbalance_bits)
    }
}

impl AggressiveTradeTotals {
    pub const fn buy_count(self) -> usize {
        self.buy_count
    }

    pub const fn sell_count(self) -> usize {
        self.sell_count
    }

    pub const fn buy_quantity(self) -> Quantity {
        self.buy_quantity
    }

    pub const fn sell_quantity(self) -> Quantity {
        self.sell_quantity
    }

    pub const fn buy_notional(self) -> FixedDecimal {
        self.buy_notional
    }

    pub const fn sell_notional(self) -> FixedDecimal {
        self.sell_notional
    }

    pub fn signed_count_imbalance(self) -> f64 {
        f64::from_bits(self.signed_count_imbalance_bits)
    }

    pub fn signed_quantity_imbalance(self) -> f64 {
        f64::from_bits(self.signed_quantity_imbalance_bits)
    }

    pub fn signed_notional_imbalance(self) -> f64 {
        f64::from_bits(self.signed_notional_imbalance_bits)
    }

    pub const fn inferred_count(self) -> usize {
        self.inferred_count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TradeClusterStatistics {
    longest_same_side_run: usize,
    largest_large_trade_cluster: usize,
    large_trade_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepEqualPricePolicy {
    Allow,
    Reject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TradePrintSweepDirection {
    buy_episode_count: usize,
    sell_episode_count: usize,
    direction_bits: u64,
}

impl TradePrintSweepDirection {
    pub const fn buy_episode_count(self) -> usize {
        self.buy_episode_count
    }

    pub const fn sell_episode_count(self) -> usize {
        self.sell_episode_count
    }

    pub fn direction(self) -> f64 {
        f64::from_bits(self.direction_bits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LargeTradeClusterActivity {
    cluster_count: usize,
    clustered_large_count: usize,
    clustered_large_notional: FixedDecimal,
    cluster_activity_rate_per_second_bits: u64,
}

impl LargeTradeClusterActivity {
    pub const fn cluster_count(self) -> usize {
        self.cluster_count
    }

    pub const fn clustered_large_count(self) -> usize {
        self.clustered_large_count
    }

    pub const fn clustered_large_notional(self) -> FixedDecimal {
        self.clustered_large_notional
    }

    pub fn cluster_activity_rate_per_second(self) -> f64 {
        f64::from_bits(self.cluster_activity_rate_per_second_bits)
    }
}

impl TradeClusterStatistics {
    pub const fn longest_same_side_run(self) -> usize {
        self.longest_same_side_run
    }

    pub const fn largest_large_trade_cluster(self) -> usize {
        self.largest_large_trade_cluster
    }

    pub const fn large_trade_count(self) -> usize {
        self.large_trade_count
    }
}

pub fn aggressive_trade_totals(
    window: &TradeFlowWindow,
) -> Result<AggressiveTradeTotals, FeatureComputationError> {
    let sums = aggressive_trade_sums(window)?;
    let imbalances = aggressive_trade_imbalances(window)?;
    Ok(AggressiveTradeTotals {
        buy_count: sums.buy_count,
        sell_count: sums.sell_count,
        buy_quantity: sums.buy_quantity,
        sell_quantity: sums.sell_quantity,
        buy_notional: sums.buy_notional,
        sell_notional: sums.sell_notional,
        signed_count_imbalance_bits: imbalances.signed_count_imbalance_bits,
        signed_quantity_imbalance_bits: imbalances.signed_quantity_imbalance_bits,
        signed_notional_imbalance_bits: imbalances.signed_notional_imbalance_bits,
        inferred_count: sums.inferred_count,
    })
}

pub fn aggressive_trade_sums(
    window: &TradeFlowWindow,
) -> Result<AggressiveTradeSums, FeatureComputationError> {
    require_spot_quote_notional(&window.instrument)?;
    let mut buy_count = 0_usize;
    let mut sell_count = 0_usize;
    let mut buy_quantity = zero();
    let mut sell_quantity = zero();
    let mut buy_notional = zero();
    let mut sell_notional = zero();
    let mut inferred_count = 0_usize;

    for observation in window.observations() {
        let notional = observation
            .price
            .value()
            .checked_mul(observation.quantity.value())
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        let (count, quantity, total_notional) = match observation.side {
            AggressorSide::Buy => (&mut buy_count, &mut buy_quantity, &mut buy_notional),
            AggressorSide::Sell => (&mut sell_count, &mut sell_quantity, &mut sell_notional),
        };
        *count = count
            .checked_add(1)
            .ok_or(FeatureComputationError::CapacityExceeded)?;
        *quantity = quantity
            .checked_add(observation.quantity.value())
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        *total_notional = total_notional
            .checked_add(notional)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        if observation.authority.is_inferred() {
            inferred_count = inferred_count
                .checked_add(1)
                .ok_or(FeatureComputationError::CapacityExceeded)?;
        }
    }

    Ok(AggressiveTradeSums {
        buy_count,
        sell_count,
        buy_quantity: Quantity::new(buy_quantity)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
        sell_quantity: Quantity::new(sell_quantity)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
        buy_notional,
        sell_notional,
        inferred_count,
    })
}

pub fn aggressive_trade_imbalances(
    window: &TradeFlowWindow,
) -> Result<AggressiveTradeImbalances, FeatureComputationError> {
    let sums = aggressive_trade_sums(window)?;
    let total_count = sums
        .buy_count
        .checked_add(sums.sell_count)
        .ok_or(FeatureComputationError::CapacityExceeded)?;
    let total_quantity = sums
        .buy_quantity
        .value()
        .checked_add(sums.sell_quantity.value())
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let total_notional = sums
        .buy_notional
        .checked_add(sums.sell_notional)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    if total_count == 0 || total_quantity.is_zero() || total_notional.is_zero() {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let count_imbalance =
        finite((sums.buy_count as f64 - sums.sell_count as f64) / total_count as f64)?;
    let signed_quantity = sums
        .buy_quantity
        .value()
        .checked_sub(sums.sell_quantity.value())
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let quantity_imbalance = finite(
        signed_quantity.to_f64_lossy_for_analysis() / total_quantity.to_f64_lossy_for_analysis(),
    )?;
    let signed_notional = sums
        .buy_notional
        .checked_sub(sums.sell_notional)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let notional_imbalance = finite(
        signed_notional.to_f64_lossy_for_analysis() / total_notional.to_f64_lossy_for_analysis(),
    )?;
    Ok(AggressiveTradeImbalances {
        signed_count_imbalance_bits: count_imbalance.to_bits(),
        signed_quantity_imbalance_bits: quantity_imbalance.to_bits(),
        signed_notional_imbalance_bits: notional_imbalance.to_bits(),
    })
}

pub fn trade_intensity_per_second(
    window: &TradeFlowWindow,
) -> Result<f64, FeatureComputationError> {
    let duration_ns = window
        .end
        .value()
        .checked_sub(window.start.value())
        .and_then(|duration| u64::try_from(duration).ok())
        .filter(|duration| *duration > 0)
        .ok_or(FeatureComputationError::InvalidParameter)?;
    finite(window.observations.len() as f64 * 1_000_000_000.0 / duration_ns as f64)
}

pub fn interarrival_coefficient_of_variation(
    window: &TradeFlowWindow,
) -> Result<f64, FeatureComputationError> {
    if window.observations.len() < 3 {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    let intervals = window
        .observations
        .windows(2)
        .map(|pair| {
            pair[1]
                .event_time
                .value()
                .checked_sub(pair[0].event_time.value())
                .ok_or(FeatureComputationError::InvalidInput)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mean = intervals.iter().map(|value| *value as f64).sum::<f64>() / intervals.len() as f64;
    if !mean.is_finite() || mean <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let variance = intervals
        .iter()
        .map(|value| {
            let centered = *value as f64 - mean;
            centered * centered
        })
        .sum::<f64>()
        / intervals.len() as f64;
    let coefficient = variance.sqrt() / mean;
    if coefficient.is_finite() {
        Ok(coefficient)
    } else {
        Err(FeatureComputationError::AnalyticalUnavailable)
    }
}

pub fn signed_volume_at_price(
    window: &TradeFlowWindow,
) -> Result<BTreeMap<Price, FixedDecimal>, FeatureComputationError> {
    let mut signed = BTreeMap::new();
    for observation in window.observations() {
        let quantity = match observation.side {
            AggressorSide::Buy => observation.quantity.value(),
            AggressorSide::Sell => observation
                .quantity
                .value()
                .checked_neg()
                .map_err(|_| FeatureComputationError::InvalidInput)?,
        };
        let previous = signed.get(&observation.price).copied().unwrap_or_else(zero);
        let total = previous
            .checked_add(quantity)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        signed.insert(observation.price, total);
    }
    Ok(signed)
}

pub fn top_of_book_ofi(window: &TopOfBookWindow) -> Result<FixedDecimal, FeatureComputationError> {
    window
        .observations()
        .windows(2)
        .try_fold(zero(), |total, pair| {
            let previous = &pair[0];
            let current = &pair[1];
            let mut increment = zero();
            if current.bid_price >= previous.bid_price {
                increment = increment
                    .checked_add(current.bid_quantity.value())
                    .map_err(|_| FeatureComputationError::InvalidInput)?;
            }
            if current.bid_price <= previous.bid_price {
                increment = increment
                    .checked_sub(previous.bid_quantity.value())
                    .map_err(|_| FeatureComputationError::InvalidInput)?;
            }
            if current.ask_price <= previous.ask_price {
                increment = increment
                    .checked_sub(current.ask_quantity.value())
                    .map_err(|_| FeatureComputationError::InvalidInput)?;
            }
            if current.ask_price >= previous.ask_price {
                increment = increment
                    .checked_add(previous.ask_quantity.value())
                    .map_err(|_| FeatureComputationError::InvalidInput)?;
            }
            total
                .checked_add(increment)
                .map_err(|_| FeatureComputationError::InvalidInput)
        })
}

pub fn trade_cluster_statistics(
    window: &TradeFlowWindow,
    large_trade_notional_threshold: FixedDecimal,
    max_cluster_gap_ns: u64,
) -> Result<TradeClusterStatistics, FeatureComputationError> {
    require_spot_quote_notional(&window.instrument)?;
    if !large_trade_notional_threshold.is_positive() || max_cluster_gap_ns == 0 {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let mut longest_same_side_run = 0_usize;
    let mut current_same_side_run = 0_usize;
    let mut previous_side = None;
    let mut large_trade_count = 0_usize;
    let mut largest_large_trade_cluster = 0_usize;
    let mut current_large_trade_cluster = 0_usize;
    let mut previous_large_event_time = None;

    for observation in window.observations() {
        if previous_side == Some(observation.side) {
            current_same_side_run = current_same_side_run
                .checked_add(1)
                .ok_or(FeatureComputationError::CapacityExceeded)?;
        } else {
            current_same_side_run = 1;
            previous_side = Some(observation.side);
        }
        longest_same_side_run = longest_same_side_run.max(current_same_side_run);

        let notional = observation
            .price
            .value()
            .checked_mul(observation.quantity.value())
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        if notional >= large_trade_notional_threshold {
            large_trade_count = large_trade_count
                .checked_add(1)
                .ok_or(FeatureComputationError::CapacityExceeded)?;
            let joins_previous_cluster =
                previous_large_event_time.is_some_and(|previous: UnixNanos| {
                    observation
                        .event_time
                        .value()
                        .checked_sub(previous.value())
                        .and_then(|gap| u64::try_from(gap).ok())
                        .is_some_and(|gap| gap <= max_cluster_gap_ns)
                });
            current_large_trade_cluster = if joins_previous_cluster {
                current_large_trade_cluster
                    .checked_add(1)
                    .ok_or(FeatureComputationError::CapacityExceeded)?
            } else {
                1
            };
            largest_large_trade_cluster =
                largest_large_trade_cluster.max(current_large_trade_cluster);
            previous_large_event_time = Some(observation.event_time);
        }
    }

    Ok(TradeClusterStatistics {
        longest_same_side_run,
        largest_large_trade_cluster,
        large_trade_count,
    })
}

pub fn trade_print_sweep_direction(
    window: &TradeFlowWindow,
    max_intertrade_gap_ns: u64,
    min_trade_count: usize,
    min_distinct_prices: usize,
    min_quote_notional: FixedDecimal,
    equal_price_policy: SweepEqualPricePolicy,
) -> Result<TradePrintSweepDirection, FeatureComputationError> {
    require_spot_quote_notional(&window.instrument)?;
    if max_intertrade_gap_ns == 0
        || !(2..=MAX_FLOW_TRADES).contains(&min_trade_count)
        || !(2..=MAX_FLOW_TRADES).contains(&min_distinct_prices)
        || !min_quote_notional.is_positive()
    {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let mut buy_episode_count = 0_usize;
    let mut sell_episode_count = 0_usize;
    let mut episode_start = 0_usize;
    for index in 1..=window.observations.len() {
        let continues = index < window.observations.len()
            && window.observations[index].side == window.observations[index - 1].side
            && elapsed_between(
                window.observations[index - 1].event_time,
                window.observations[index].event_time,
            )
            .is_ok_and(|gap| gap <= max_intertrade_gap_ns);
        if continues {
            continue;
        }
        let episode = &window.observations[episode_start..index];
        if qualifies_trade_print_sweep(
            episode,
            min_trade_count,
            min_distinct_prices,
            min_quote_notional,
            equal_price_policy,
        )? {
            let count = match episode[0].side {
                AggressorSide::Buy => &mut buy_episode_count,
                AggressorSide::Sell => &mut sell_episode_count,
            };
            *count = count
                .checked_add(1)
                .ok_or(FeatureComputationError::CapacityExceeded)?;
        }
        episode_start = index;
    }
    let total = buy_episode_count
        .checked_add(sell_episode_count)
        .ok_or(FeatureComputationError::CapacityExceeded)?;
    if total == 0 {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    let direction = finite((buy_episode_count as f64 - sell_episode_count as f64) / total as f64)?;
    Ok(TradePrintSweepDirection {
        buy_episode_count,
        sell_episode_count,
        direction_bits: direction.to_bits(),
    })
}

fn qualifies_trade_print_sweep(
    episode: &[FlowTradeObservation],
    min_trade_count: usize,
    min_distinct_prices: usize,
    min_quote_notional: FixedDecimal,
    equal_price_policy: SweepEqualPricePolicy,
) -> Result<bool, FeatureComputationError> {
    if episode.len() < min_trade_count {
        return Ok(false);
    }
    let distinct_prices = episode
        .iter()
        .map(|observation| observation.price)
        .collect::<BTreeSet<_>>();
    if distinct_prices.len() < min_distinct_prices {
        return Ok(false);
    }
    let mut quote_notional = zero();
    for observation in episode {
        quote_notional = quote_notional
            .checked_add(
                observation
                    .price
                    .value()
                    .checked_mul(observation.quantity.value())
                    .map_err(|_| FeatureComputationError::InvalidInput)?,
            )
            .map_err(|_| FeatureComputationError::InvalidInput)?;
    }
    if quote_notional < min_quote_notional {
        return Ok(false);
    }
    Ok(episode.windows(2).all(|pair| {
        let previous = pair[0].price;
        let current = pair[1].price;
        match (pair[0].side, equal_price_policy) {
            (AggressorSide::Buy, SweepEqualPricePolicy::Allow) => current >= previous,
            (AggressorSide::Buy, SweepEqualPricePolicy::Reject) => current > previous,
            (AggressorSide::Sell, SweepEqualPricePolicy::Allow) => current <= previous,
            (AggressorSide::Sell, SweepEqualPricePolicy::Reject) => current < previous,
        }
    }))
}

pub fn large_trade_cluster_activity(
    window: &TradeFlowWindow,
    large_quote_notional: FixedDecimal,
    cluster_gap_ns: u64,
    min_cluster_trades: usize,
) -> Result<LargeTradeClusterActivity, FeatureComputationError> {
    require_spot_quote_notional(&window.instrument)?;
    if !large_quote_notional.is_positive()
        || cluster_gap_ns == 0
        || !(2..=MAX_FLOW_TRADES).contains(&min_cluster_trades)
    {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let mut cluster_count = 0_usize;
    let mut clustered_large_count = 0_usize;
    let mut clustered_large_notional = zero();
    let mut current_count = 0_usize;
    let mut current_notional = zero();
    let mut previous_large_time = None;

    for observation in window.observations() {
        let notional = observation
            .price
            .value()
            .checked_mul(observation.quantity.value())
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        if notional < large_quote_notional {
            continue;
        }
        let joins_current = previous_large_time.is_some_and(|previous: UnixNanos| {
            observation
                .event_time
                .value()
                .checked_sub(previous.value())
                .and_then(|gap| u64::try_from(gap).ok())
                .is_some_and(|gap| gap <= cluster_gap_ns)
        });
        if !joins_current && current_count > 0 {
            finalize_large_cluster(
                current_count,
                current_notional,
                min_cluster_trades,
                &mut cluster_count,
                &mut clustered_large_count,
                &mut clustered_large_notional,
            )?;
            current_count = 0;
            current_notional = zero();
        }
        current_count = current_count
            .checked_add(1)
            .ok_or(FeatureComputationError::CapacityExceeded)?;
        current_notional = current_notional
            .checked_add(notional)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        previous_large_time = Some(observation.event_time);
    }
    if current_count > 0 {
        finalize_large_cluster(
            current_count,
            current_notional,
            min_cluster_trades,
            &mut cluster_count,
            &mut clustered_large_count,
            &mut clustered_large_notional,
        )?;
    }
    let rate = finite(cluster_count as f64 * 1_000_000_000.0 / window.duration_ns()? as f64)?;
    Ok(LargeTradeClusterActivity {
        cluster_count,
        clustered_large_count,
        clustered_large_notional,
        cluster_activity_rate_per_second_bits: rate.to_bits(),
    })
}

fn finalize_large_cluster(
    current_count: usize,
    current_notional: FixedDecimal,
    min_cluster_trades: usize,
    cluster_count: &mut usize,
    clustered_large_count: &mut usize,
    clustered_large_notional: &mut FixedDecimal,
) -> Result<(), FeatureComputationError> {
    if current_count < min_cluster_trades {
        return Ok(());
    }
    *cluster_count = cluster_count
        .checked_add(1)
        .ok_or(FeatureComputationError::CapacityExceeded)?;
    *clustered_large_count = clustered_large_count
        .checked_add(current_count)
        .ok_or(FeatureComputationError::CapacityExceeded)?;
    *clustered_large_notional = clustered_large_notional
        .checked_add(current_notional)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    Ok(())
}

fn zero() -> FixedDecimal {
    FixedDecimal::new(0, 0).expect("zero is valid")
}

fn require_spot_quote_notional(instrument: &InstrumentId) -> Result<(), FeatureComputationError> {
    if instrument.product_type() == ProductType::Spot {
        Ok(())
    } else {
        Err(FeatureComputationError::SourceNotSupported)
    }
}

fn finite(value: f64) -> Result<f64, FeatureComputationError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(FeatureComputationError::AnalyticalUnavailable)
    }
}
