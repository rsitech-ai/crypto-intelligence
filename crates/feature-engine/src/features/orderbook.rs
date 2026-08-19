//! Deterministic analytical features over trusted fixed-point order books.

use blake3::Hasher;
use domain::{InstrumentDefinition, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos};
use event_envelope::{BookLevel, EventEnvelope, UncheckedEventPayload};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use orderbook::{
    BookClassification, BookEventApplyReceipt, BookSession, BookSnapshotView, BookState,
    ChecksumStatus,
};
use std::collections::BTreeSet;

use super::{AggressorSide, FeatureComputationError, FlowTradeObservation};

const BASIS_POINTS_DENOMINATOR: i128 = 10_000;
pub const MAX_DEPTH_BAND_BPS: u32 = 10_000;
pub const L2_UNAVAILABLE_AFTER_MS: u64 = 3_000;
pub const MAX_BOOK_SHAPE_LEVELS: usize = 256;
pub const MAX_RECOVERY_OBSERVATIONS: usize = 4_096;
const MAX_POLICY_ID_BYTES: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstrumentDefinitionEvidence {
    definition: InstrumentDefinition,
    source: SourceId,
    as_known_at: UnixNanos,
    lineage: [u8; 32],
    quality_score_ppm: u32,
}

impl InstrumentDefinitionEvidence {
    pub fn try_from_event(event: &EventEnvelope) -> Result<Self, FeatureComputationError> {
        event
            .verify()
            .map_err(|_| FeatureComputationError::UntrustedInput)?;
        let metadata = event.metadata().as_unchecked();
        let definition = match event.payload().as_unchecked() {
            UncheckedEventPayload::InstrumentDefinition(definition) => definition,
            _ => return Err(FeatureComputationError::InvalidInput),
        };
        if metadata.instrument_id.as_ref() != Some(definition.id())
            || metadata.source.kind() != SourceKind::Exchange
            || metadata.source.name() != definition.id().venue().as_str()
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let event_time = metadata
            .exchange_transaction_timestamp
            .or(metadata.exchange_timestamp)
            .ok_or(FeatureComputationError::InvalidInput)?;
        if metadata.normalization_timestamp < event_time {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        Ok(Self {
            definition: definition.clone(),
            source: metadata.source.clone(),
            as_known_at: metadata.normalization_timestamp,
            lineage: *event.id().as_bytes(),
            quality_score_ppm: metadata.quality_score_ppm,
        })
    }

    fn matches(&self, book: &BookSnapshotView, book_event_time: UnixNanos) -> bool {
        self.definition.id() == book.instrument()
            && self.definition.price_tick() == book.price_tick()
            && self.definition.quantity_step() == book.quantity_step()
            && self.definition.listing_time() <= book_event_time
            && self
                .definition
                .delisting_time()
                .is_none_or(|delisting| book_event_time < delisting)
    }

    pub const fn lineage(&self) -> [u8; 32] {
        self.lineage
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookStateEvidence {
    source: SourceId,
    session: BookSession,
    last_source_sequence: u64,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    lineage: [u8; 32],
    instrument_definition_lineage: Option<[u8; 32]>,
    quality_score_ppm: u32,
    state_digest: [u8; 32],
}

impl BookStateEvidence {
    pub fn try_from_event(
        book: &BookSnapshotView,
        event: &EventEnvelope,
    ) -> Result<Self, FeatureComputationError> {
        Self::try_from_authenticated_state(book, event, false)
    }

    pub fn try_from_apply_receipt(
        receipt: &BookEventApplyReceipt,
        event: &EventEnvelope,
    ) -> Result<Self, FeatureComputationError> {
        if receipt.event_id() != event.id() {
            return Err(FeatureComputationError::UntrustedInput);
        }
        Self::try_from_authenticated_state(receipt.snapshot(), event, true)
    }

    pub fn try_from_event_with_definition(
        book: &BookSnapshotView,
        event: &EventEnvelope,
        definition: &InstrumentDefinitionEvidence,
    ) -> Result<Self, FeatureComputationError> {
        Self::try_from_event(book, event)?.with_instrument_definition(definition, book)
    }

    pub fn try_from_apply_receipt_with_definition(
        receipt: &BookEventApplyReceipt,
        event: &EventEnvelope,
        definition: &InstrumentDefinitionEvidence,
    ) -> Result<Self, FeatureComputationError> {
        Self::try_from_apply_receipt(receipt, event)?
            .with_instrument_definition(definition, receipt.snapshot())
    }

    fn try_from_authenticated_state(
        book: &BookSnapshotView,
        event: &EventEnvelope,
        applied_event_receipt: bool,
    ) -> Result<Self, FeatureComputationError> {
        event
            .verify()
            .map_err(|_| FeatureComputationError::UntrustedInput)?;
        let metadata = event.metadata().as_unchecked();
        if metadata.instrument_id.as_ref() != Some(book.instrument()) {
            return Err(FeatureComputationError::MixedEntity);
        }
        if metadata.source.kind() != SourceKind::Exchange
            || metadata.source.name() != book.instrument().venue().as_str()
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let payload_sequence = match event.payload().as_unchecked() {
            UncheckedEventPayload::BookSnapshot(snapshot) => {
                if snapshot.bids.as_slice() != book.bids()
                    || snapshot.asks.as_slice() != book.asks()
                {
                    return Err(FeatureComputationError::UntrustedInput);
                }
                snapshot.last_sequence
            }
            UncheckedEventPayload::BookDelta(delta) if applied_event_receipt => delta.last_sequence,
            UncheckedEventPayload::BookDelta(_) => {
                return Err(FeatureComputationError::SourceNotSupported);
            }
            _ => return Err(FeatureComputationError::InvalidInput),
        };
        let session = book.session();
        if metadata.connection_epoch != session.connection_epoch
            || metadata.subscription_epoch != session.subscription_epoch
            || metadata.sequence_number != Some(book.last_source_sequence())
            || payload_sequence != book.last_source_sequence()
        {
            return Err(FeatureComputationError::SequenceGap);
        }
        if metadata.receive_monotonic_ns != book.updated_monotonic_ns() {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let event_time = metadata
            .exchange_transaction_timestamp
            .or(metadata.exchange_timestamp)
            .ok_or(FeatureComputationError::InvalidInput)?;
        if metadata.normalization_timestamp < event_time {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        Ok(Self {
            source: metadata.source.clone(),
            session,
            last_source_sequence: book.last_source_sequence(),
            event_time,
            as_known_at: metadata.normalization_timestamp,
            lineage: *event.id().as_bytes(),
            instrument_definition_lineage: None,
            quality_score_ppm: metadata
                .quality_score_ppm
                .min(book.quality().quality_score_ppm),
            state_digest: book_state_digest(book),
        })
    }

    fn with_instrument_definition(
        mut self,
        definition: &InstrumentDefinitionEvidence,
        book: &BookSnapshotView,
    ) -> Result<Self, FeatureComputationError> {
        if definition.source != self.source
            || definition.lineage == self.lineage
            || !definition.matches(book, self.event_time)
        {
            return Err(FeatureComputationError::UntrustedInput);
        }
        self.as_known_at = self.as_known_at.max(definition.as_known_at);
        self.quality_score_ppm = self.quality_score_ppm.min(definition.quality_score_ppm);
        self.instrument_definition_lineage = Some(definition.lineage);
        Ok(self)
    }

    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn session(&self) -> BookSession {
        self.session
    }

    pub const fn last_source_sequence(&self) -> u64 {
        self.last_source_sequence
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

    pub const fn instrument_definition_lineage(&self) -> Option<[u8; 32]> {
        self.instrument_definition_lineage
    }

    pub const fn quality_score_ppm(&self) -> u32 {
        self.quality_score_ppm
    }

    pub const fn state_digest(&self) -> [u8; 32] {
        self.state_digest
    }

    pub fn matches(&self, book: &BookSnapshotView) -> bool {
        self.session == book.session()
            && self.last_source_sequence == book.last_source_sequence()
            && self.state_digest == book_state_digest(book)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookEvidencePolicy {
    price_tick: Price,
    quality_policy_id: String,
    quality_policy_version: u32,
    ordering_authority_id: String,
    ordering_authority_version: u32,
    reference_price_policy_id: String,
    reference_price_policy_version: u32,
}

impl BookEvidencePolicy {
    pub fn new(
        price_tick: Price,
        quality_policy_id: impl Into<String>,
        quality_policy_version: u32,
        ordering_authority_id: impl Into<String>,
        ordering_authority_version: u32,
        reference_price_policy_id: impl Into<String>,
        reference_price_policy_version: u32,
    ) -> Result<Self, FeatureComputationError> {
        let quality_policy_id = quality_policy_id.into();
        let ordering_authority_id = ordering_authority_id.into();
        let reference_price_policy_id = reference_price_policy_id.into();
        if quality_policy_version == 0
            || ordering_authority_version == 0
            || reference_price_policy_version == 0
            || !valid_policy_id(&quality_policy_id)
            || !valid_policy_id(&ordering_authority_id)
            || !valid_policy_id(&reference_price_policy_id)
        {
            return Err(FeatureComputationError::InvalidParameter);
        }
        Ok(Self {
            price_tick,
            quality_policy_id,
            quality_policy_version,
            ordering_authority_id,
            ordering_authority_version,
            reference_price_policy_id,
            reference_price_policy_version,
        })
    }

    pub const fn price_tick(&self) -> Price {
        self.price_tick
    }

    pub fn quality_policy_id(&self) -> &str {
        &self.quality_policy_id
    }

    pub const fn quality_policy_version(&self) -> u32 {
        self.quality_policy_version
    }

    pub fn ordering_authority_id(&self) -> &str {
        &self.ordering_authority_id
    }

    pub const fn ordering_authority_version(&self) -> u32 {
        self.ordering_authority_version
    }

    pub fn reference_price_policy_id(&self) -> &str {
        &self.reference_price_policy_id
    }

    pub const fn reference_price_policy_version(&self) -> u32 {
        self.reference_price_policy_version
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpreadMetrics {
    absolute: FixedDecimal,
    relative_bits: u64,
}

impl SpreadMetrics {
    pub const fn absolute(self) -> FixedDecimal {
        self.absolute
    }

    pub fn relative(self) -> f64 {
        f64::from_bits(self.relative_bits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DepthBand {
    bid_quantity: Quantity,
    ask_quantity: Quantity,
    bid_notional: FixedDecimal,
    ask_notional: FixedDecimal,
}

impl DepthBand {
    pub const fn bid_quantity(self) -> Quantity {
        self.bid_quantity
    }

    pub const fn ask_quantity(self) -> Quantity {
        self.ask_quantity
    }

    pub const fn bid_notional(self) -> FixedDecimal {
        self.bid_notional
    }

    pub const fn ask_notional(self) -> FixedDecimal {
        self.ask_notional
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FullyObservedDepthBand {
    bid_quantity: Result<Quantity, FeatureComputationError>,
    ask_quantity: Result<Quantity, FeatureComputationError>,
}

impl FullyObservedDepthBand {
    pub const fn bid_quantity(self) -> Result<Quantity, FeatureComputationError> {
        self.bid_quantity
    }

    pub const fn ask_quantity(self) -> Result<Quantity, FeatureComputationError> {
        self.ask_quantity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveLevelCounts {
    bid: usize,
    ask: usize,
}

impl ActiveLevelCounts {
    pub const fn bid(self) -> usize {
        self.bid
    }

    pub const fn ask(self) -> usize {
        self.ask
    }

    pub const fn total(self) -> usize {
        self.bid + self.ask
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BookDistribution {
    normalized_entropy_bits: u64,
    concentration_bits: u64,
}

impl BookDistribution {
    pub fn normalized_entropy(self) -> f64 {
        f64::from_bits(self.normalized_entropy_bits)
    }

    pub fn concentration(self) -> f64 {
        f64::from_bits(self.concentration_bits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BookSideShape {
    slope_at_inside_bits: u64,
    convexity_bits: u64,
}

impl BookSideShape {
    pub fn slope_at_inside(self) -> f64 {
        f64::from_bits(self.slope_at_inside_bits)
    }

    pub fn convexity(self) -> f64 {
        f64::from_bits(self.convexity_bits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuadraticBookShape {
    bid: BookSideShape,
    ask: BookSideShape,
}

impl QuadraticBookShape {
    pub const fn bid(self) -> BookSideShape {
        self.bid
    }

    pub const fn ask(self) -> BookSideShape {
        self.ask
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LevelGapDensity {
    bid_bits: u64,
    ask_bits: u64,
}

impl LevelGapDensity {
    pub fn bid(self) -> f64 {
        f64::from_bits(self.bid_bits)
    }

    pub fn ask(self) -> f64 {
        f64::from_bits(self.ask_bits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiquidityWallDistances {
    bid_bits: Option<u64>,
    ask_bits: Option<u64>,
}

impl LiquidityWallDistances {
    pub fn bid(self) -> Option<f64> {
        self.bid_bits.map(f64::from_bits)
    }

    pub fn ask(self) -> Option<f64> {
        self.ask_bits.map(f64::from_bits)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookMetricObservation {
    instrument: InstrumentId,
    session: BookSession,
    evidence_policy: BookEvidencePolicy,
    last_source_sequence: u64,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    lineage: [u8; 32],
    reference_price: Price,
    band_bps: u32,
    bid_quantity: FixedDecimal,
    ask_quantity: FixedDecimal,
    absolute_spread: FixedDecimal,
    authenticated: bool,
}

impl BookMetricObservation {
    pub fn from_snapshot(
        book: &BookSnapshotView,
        evidence_policy: BookEvidencePolicy,
        reference_price: Price,
        band_bps: u32,
        event_time: UnixNanos,
        as_known_at: UnixNanos,
        lineage: [u8; 32],
    ) -> Result<Self, FeatureComputationError> {
        if as_known_at < event_time || lineage == [0; 32] {
            return Err(FeatureComputationError::InvalidInput);
        }
        let depth = depth_within_band(book, reference_price, band_bps)?;
        let spread = spread(book)?;
        Ok(Self {
            instrument: book.instrument().clone(),
            session: book.session(),
            evidence_policy,
            last_source_sequence: book.last_source_sequence(),
            event_time,
            as_known_at,
            lineage,
            reference_price,
            band_bps,
            bid_quantity: depth.bid_quantity.value(),
            ask_quantity: depth.ask_quantity.value(),
            absolute_spread: spread.absolute,
            authenticated: false,
        })
    }

    pub fn from_authenticated_snapshot(
        book: &BookSnapshotView,
        evidence_policy: BookEvidencePolicy,
        reference_price: Price,
        band_bps: u32,
        evidence: &BookStateEvidence,
    ) -> Result<Self, FeatureComputationError> {
        if !evidence.matches(book) {
            return Err(FeatureComputationError::UntrustedInput);
        }
        let mut observation = Self::from_snapshot(
            book,
            evidence_policy,
            reference_price,
            band_bps,
            evidence.event_time,
            evidence.as_known_at,
            evidence.lineage,
        )?;
        observation.authenticated = true;
        Ok(observation)
    }

    pub const fn is_authenticated(&self) -> bool {
        self.authenticated
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DepthSpreadChange {
    bid_quantity: FixedDecimal,
    ask_quantity: FixedDecimal,
    absolute_spread: FixedDecimal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisplayedDepthRecoveryEpisode {
    pre: BookMetricObservation,
    post: BookMetricObservation,
    trade: FlowTradeObservation,
    evaluation_time: UnixNanos,
    passive_post_depth: FixedDecimal,
    passive_depth_loss: FixedDecimal,
}

impl DisplayedDepthRecoveryEpisode {
    pub fn new(
        pre: BookMetricObservation,
        post: BookMetricObservation,
        trade: FlowTradeObservation,
        evaluation_time: UnixNanos,
        max_anchor_lag_ns: u64,
    ) -> Result<Self, FeatureComputationError> {
        if max_anchor_lag_ns == 0 {
            return Err(FeatureComputationError::InvalidParameter);
        }
        validate_metric_pair(&pre, &post)?;
        if trade.instrument() != &pre.instrument {
            return Err(FeatureComputationError::MixedEntity);
        }
        if pre.event_time > trade.event_time() || trade.event_time() > post.event_time {
            return Err(FeatureComputationError::NonMonotonicTime);
        }
        if pre.as_known_at > evaluation_time
            || post.as_known_at > evaluation_time
            || trade.as_known_at() > evaluation_time
        {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        if pre.lineage == trade.lineage()
            || post.lineage == trade.lineage()
            || pre.lineage == post.lineage
        {
            return Err(FeatureComputationError::DuplicateLineage);
        }
        let pre_anchor_lag = elapsed_ns(pre.event_time, trade.event_time())?;
        let post_anchor_lag = elapsed_ns(trade.event_time(), post.event_time)?;
        if pre_anchor_lag > max_anchor_lag_ns || post_anchor_lag > max_anchor_lag_ns {
            return Err(FeatureComputationError::OutsideWindow);
        }
        let passive_pre_depth = passive_depth(&pre, trade.side());
        let passive_post_depth = passive_depth(&post, trade.side());
        let passive_depth_loss = passive_pre_depth
            .checked_sub(passive_post_depth)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        if !passive_depth_loss.is_positive() {
            return Err(FeatureComputationError::ModelNotApplicable);
        }
        Ok(Self {
            pre,
            post,
            trade,
            evaluation_time,
            passive_post_depth,
            passive_depth_loss,
        })
    }

    pub fn recovery_fraction(
        &self,
        later: &BookMetricObservation,
    ) -> Result<f64, FeatureComputationError> {
        self.validate_later(later)?;
        let recovered = passive_depth(later, self.trade.side())
            .checked_sub(self.passive_post_depth)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        finite(
            recovered.to_f64_lossy_for_analysis()
                / self.passive_depth_loss.to_f64_lossy_for_analysis(),
        )
    }

    pub fn time_to_fraction_ns(
        &self,
        candidates: &[BookMetricObservation],
        capacity: usize,
        numerator: u32,
        denominator: u32,
        max_recovery_ns: u64,
    ) -> Result<u64, FeatureComputationError> {
        if capacity == 0
            || capacity > MAX_RECOVERY_OBSERVATIONS
            || numerator == 0
            || denominator == 0
            || numerator > denominator
            || max_recovery_ns == 0
        {
            return Err(FeatureComputationError::InvalidParameter);
        }
        if candidates.is_empty() {
            return Err(FeatureComputationError::InsufficientHistory);
        }
        if candidates.len() > capacity {
            return Err(FeatureComputationError::CapacityExceeded);
        }
        let numerator = FixedDecimal::new(i128::from(numerator), 0)
            .map_err(|_| FeatureComputationError::InvalidParameter)?;
        let denominator = FixedDecimal::new(i128::from(denominator), 0)
            .map_err(|_| FeatureComputationError::InvalidParameter)?;
        let target_loss = self
            .passive_depth_loss
            .checked_mul(numerator)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        let mut previous = &self.post;
        let mut lineages =
            BTreeSet::from([self.pre.lineage, self.post.lineage, self.trade.lineage()]);
        for candidate in candidates {
            validate_metric_pair(previous, candidate)?;
            if candidate.as_known_at > self.evaluation_time {
                return Err(FeatureComputationError::FutureKnowledge);
            }
            if !lineages.insert(candidate.lineage) {
                return Err(FeatureComputationError::DuplicateLineage);
            }
            let elapsed = elapsed_ns(self.post.event_time, candidate.event_time)?;
            if elapsed > max_recovery_ns {
                break;
            }
            let recovered = passive_depth(candidate, self.trade.side())
                .checked_sub(self.passive_post_depth)
                .map_err(|_| FeatureComputationError::InvalidInput)?;
            let scaled_recovered = recovered
                .checked_mul(denominator)
                .map_err(|_| FeatureComputationError::InvalidInput)?;
            if scaled_recovered >= target_loss {
                return Ok(elapsed);
            }
            previous = candidate;
        }
        Err(FeatureComputationError::InsufficientHistory)
    }

    fn validate_later(&self, later: &BookMetricObservation) -> Result<(), FeatureComputationError> {
        validate_metric_pair(&self.post, later)?;
        if later.as_known_at > self.evaluation_time {
            return Err(FeatureComputationError::FutureKnowledge);
        }
        if later.lineage == self.pre.lineage
            || later.lineage == self.post.lineage
            || later.lineage == self.trade.lineage()
        {
            return Err(FeatureComputationError::DuplicateLineage);
        }
        Ok(())
    }
}

impl DepthSpreadChange {
    pub const fn bid_quantity(self) -> FixedDecimal {
        self.bid_quantity
    }

    pub const fn ask_quantity(self) -> FixedDecimal {
        self.ask_quantity
    }

    pub const fn absolute_spread(self) -> FixedDecimal {
        self.absolute_spread
    }
}

pub fn book_depth_and_spread_change(
    previous: &BookMetricObservation,
    current: &BookMetricObservation,
) -> Result<DepthSpreadChange, FeatureComputationError> {
    validate_metric_pair(previous, current)?;
    Ok(DepthSpreadChange {
        bid_quantity: current
            .bid_quantity
            .checked_sub(previous.bid_quantity)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
        ask_quantity: current
            .ask_quantity
            .checked_sub(previous.ask_quantity)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
        absolute_spread: current
            .absolute_spread
            .checked_sub(previous.absolute_spread)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
    })
}

fn validate_metric_pair(
    previous: &BookMetricObservation,
    current: &BookMetricObservation,
) -> Result<(), FeatureComputationError> {
    if previous.instrument != current.instrument {
        return Err(FeatureComputationError::MixedEntity);
    }
    if previous.session != current.session
        || previous.evidence_policy != current.evidence_policy
        || previous.reference_price != current.reference_price
        || previous.band_bps != current.band_bps
    {
        return Err(FeatureComputationError::SourceHealthChanged);
    }
    if current.last_source_sequence <= previous.last_source_sequence
        || current.event_time < previous.event_time
        || current.as_known_at < previous.as_known_at
    {
        return Err(FeatureComputationError::NonMonotonicTime);
    }
    if current.lineage == previous.lineage {
        return Err(FeatureComputationError::DuplicateLineage);
    }
    Ok(())
}

fn passive_depth(observation: &BookMetricObservation, side: AggressorSide) -> FixedDecimal {
    match side {
        AggressorSide::Buy => observation.ask_quantity,
        AggressorSide::Sell => observation.bid_quantity,
    }
}

fn elapsed_ns(start: UnixNanos, end: UnixNanos) -> Result<u64, FeatureComputationError> {
    let elapsed = end
        .value()
        .checked_sub(start.value())
        .ok_or(FeatureComputationError::NonMonotonicTime)?;
    u64::try_from(elapsed).map_err(|_| FeatureComputationError::NonMonotonicTime)
}

fn valid_policy_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_POLICY_ID_BYTES
        && value.trim() == value
        && value.is_ascii()
        && !value.chars().any(char::is_control)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepSide {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SweepCost {
    executed_quote_notional: FixedDecimal,
    estimated_base_quantity_bits: u64,
    estimated_average_price_bits: u64,
    relative_cost_bits: u64,
}

impl SweepCost {
    pub const fn executed_quote_notional(self) -> FixedDecimal {
        self.executed_quote_notional
    }

    pub fn estimated_base_quantity(self) -> f64 {
        f64::from_bits(self.estimated_base_quantity_bits)
    }

    pub fn estimated_average_price(self) -> f64 {
        f64::from_bits(self.estimated_average_price_bits)
    }

    pub fn relative_cost(self) -> f64 {
        f64::from_bits(self.relative_cost_bits)
    }
}

pub fn spread(book: &BookSnapshotView) -> Result<SpreadMetrics, FeatureComputationError> {
    require_normal_book(book)?;
    let bid = book
        .best_bid()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let ask = book
        .best_ask()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let absolute = ask
        .price
        .value()
        .checked_sub(bid.price.value())
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let midpoint_sum = ask
        .price
        .value()
        .checked_add(bid.price.value())
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let midpoint = midpoint_sum.to_f64_lossy_for_analysis() / 2.0;
    let relative = absolute.to_f64_lossy_for_analysis() / midpoint;
    finite(relative)?;
    Ok(SpreadMetrics {
        absolute,
        relative_bits: relative.to_bits(),
    })
}

pub fn book_quote_age_ms(book: &BookSnapshotView) -> Result<u64, FeatureComputationError> {
    require_normal_book(book)?;
    Ok(book.quality().last_update_age_ms)
}

pub fn depth_within_band(
    book: &BookSnapshotView,
    reference_price: Price,
    band_bps: u32,
) -> Result<DepthBand, FeatureComputationError> {
    require_normal_book(book)?;
    require_spot_quote_notional(book.instrument())?;
    let (lower_bound, upper_bound) = scaled_band_bounds(reference_price, band_bps)?;
    let denominator = basis_points_denominator();

    let mut bid_quantity = zero();
    let mut ask_quantity = zero();
    let mut bid_notional = zero();
    let mut ask_notional = zero();

    for level in book.bids() {
        let scaled_price = level
            .price
            .value()
            .checked_mul(denominator)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        if scaled_price < lower_bound {
            break;
        }
        if scaled_price > upper_bound {
            continue;
        }
        bid_quantity = bid_quantity
            .checked_add(level.quantity.value())
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        bid_notional = bid_notional
            .checked_add(level_notional(level)?)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
    }
    for level in book.asks() {
        let scaled_price = level
            .price
            .value()
            .checked_mul(denominator)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        if scaled_price > upper_bound {
            break;
        }
        if scaled_price < lower_bound {
            continue;
        }
        ask_quantity = ask_quantity
            .checked_add(level.quantity.value())
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        ask_notional = ask_notional
            .checked_add(level_notional(level)?)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
    }

    Ok(DepthBand {
        bid_quantity: Quantity::new(bid_quantity)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
        ask_quantity: Quantity::new(ask_quantity)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
        bid_notional,
        ask_notional,
    })
}

pub fn fully_observed_depth_within_band(
    book: &BookSnapshotView,
    reference_price: Price,
    band_bps: u32,
) -> Result<FullyObservedDepthBand, FeatureComputationError> {
    require_normal_book(book)?;
    require_spot_quote_notional(book.instrument())?;
    let (lower_bound, upper_bound) = scaled_band_bounds(reference_price, band_bps)?;
    let denominator = basis_points_denominator();
    let depth = depth_within_band(book, reference_price, band_bps)?;
    let outer_bid = book
        .bids()
        .last()
        .ok_or(FeatureComputationError::UntrustedInput)?
        .price
        .value()
        .checked_mul(denominator)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let outer_ask = book
        .asks()
        .last()
        .ok_or(FeatureComputationError::UntrustedInput)?
        .price
        .value()
        .checked_mul(denominator)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    Ok(FullyObservedDepthBand {
        bid_quantity: if outer_bid <= lower_bound {
            Ok(depth.bid_quantity())
        } else {
            Err(FeatureComputationError::InsufficientHistory)
        },
        ask_quantity: if outer_ask >= upper_bound {
            Ok(depth.ask_quantity())
        } else {
            Err(FeatureComputationError::InsufficientHistory)
        },
    })
}

pub fn normalized_imbalance(
    book: &BookSnapshotView,
    reference_price: Price,
    band_bps: u32,
) -> Result<f64, FeatureComputationError> {
    let depth = depth_within_band(book, reference_price, band_bps)?;
    let bid = depth.bid_quantity.value();
    let ask = depth.ask_quantity.value();
    let total = bid
        .checked_add(ask)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    if total.is_zero() {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let difference = bid
        .checked_sub(ask)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let value = difference.to_f64_lossy_for_analysis() / total.to_f64_lossy_for_analysis();
    finite(value)
}

pub fn microprice(book: &BookSnapshotView) -> Result<f64, FeatureComputationError> {
    require_normal_book(book)?;
    let bid = book
        .best_bid()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let ask = book
        .best_ask()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let bid_quantity = bid.quantity.value();
    let ask_quantity = ask.quantity.value();
    let total_quantity = bid_quantity
        .checked_add(ask_quantity)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    if total_quantity.is_zero() {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let ask_weighted = ask
        .price
        .value()
        .checked_mul(bid_quantity)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let bid_weighted = bid
        .price
        .value()
        .checked_mul(ask_quantity)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let numerator = ask_weighted
        .checked_add(bid_weighted)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let value = numerator.to_f64_lossy_for_analysis() / total_quantity.to_f64_lossy_for_analysis();
    finite(value)
}

pub fn weighted_midpoint(book: &BookSnapshotView) -> Result<f64, FeatureComputationError> {
    require_normal_book(book)?;
    let bid = book
        .best_bid()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let ask = book
        .best_ask()
        .ok_or(FeatureComputationError::UntrustedInput)?;
    let bid_quantity = bid.quantity.value();
    let ask_quantity = ask.quantity.value();
    let total_quantity = bid_quantity
        .checked_add(ask_quantity)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    if total_quantity.is_zero() {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let bid_weighted = bid
        .price
        .value()
        .checked_mul(bid_quantity)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let ask_weighted = ask
        .price
        .value()
        .checked_mul(ask_quantity)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let numerator = bid_weighted
        .checked_add(ask_weighted)
        .map_err(|_| FeatureComputationError::InvalidInput)?;
    let value = numerator.to_f64_lossy_for_analysis() / total_quantity.to_f64_lossy_for_analysis();
    finite(value)
}

pub fn active_level_counts(
    book: &BookSnapshotView,
) -> Result<ActiveLevelCounts, FeatureComputationError> {
    require_normal_book(book)?;
    Ok(ActiveLevelCounts {
        bid: book.bids().len(),
        ask: book.asks().len(),
    })
}

pub fn book_distribution(
    book: &BookSnapshotView,
) -> Result<BookDistribution, FeatureComputationError> {
    require_normal_book(book)?;
    let level_count = book
        .bids()
        .len()
        .checked_add(book.asks().len())
        .ok_or(FeatureComputationError::CapacityExceeded)?;
    if level_count < 2 {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    let total_quantity =
        book.bids()
            .iter()
            .chain(book.asks())
            .try_fold(zero(), |total, level| {
                total
                    .checked_add(level.quantity.value())
                    .map_err(|_| FeatureComputationError::InvalidInput)
            })?;
    if total_quantity.is_zero() {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let total = total_quantity.to_f64_lossy_for_analysis();
    let (entropy, concentration) = book.bids().iter().chain(book.asks()).try_fold(
        (0.0, 0.0),
        |(entropy, concentration), level| {
            let weight = level.quantity.value().to_f64_lossy_for_analysis() / total;
            if !weight.is_finite() || weight <= 0.0 {
                return Err(FeatureComputationError::InvalidInput);
            }
            Ok((
                entropy - weight * weight.ln(),
                concentration + weight * weight,
            ))
        },
    )?;
    let normalized_entropy = entropy / (level_count as f64).ln();
    finite(normalized_entropy)?;
    finite(concentration)?;
    Ok(BookDistribution {
        normalized_entropy_bits: normalized_entropy.to_bits(),
        concentration_bits: concentration.to_bits(),
    })
}

pub fn book_shape_quadratic(
    book: &BookSnapshotView,
    price_tick: Price,
    level_count: usize,
) -> Result<QuadraticBookShape, FeatureComputationError> {
    Ok(QuadraticBookShape {
        bid: bid_book_shape_quadratic(book, price_tick, level_count)?,
        ask: ask_book_shape_quadratic(book, price_tick, level_count)?,
    })
}

pub fn bid_book_shape_quadratic(
    book: &BookSnapshotView,
    price_tick: Price,
    level_count: usize,
) -> Result<BookSideShape, FeatureComputationError> {
    require_normal_book(book)?;
    require_book_shape_level_count(level_count)?;
    if book.bids().len() < level_count {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    fit_side_shape(&book.bids()[..level_count], price_tick, true)
}

pub fn ask_book_shape_quadratic(
    book: &BookSnapshotView,
    price_tick: Price,
    level_count: usize,
) -> Result<BookSideShape, FeatureComputationError> {
    require_normal_book(book)?;
    require_book_shape_level_count(level_count)?;
    if book.asks().len() < level_count {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    fit_side_shape(&book.asks()[..level_count], price_tick, false)
}

pub fn level_gap_density(
    book: &BookSnapshotView,
    price_tick: Price,
) -> Result<LevelGapDensity, FeatureComputationError> {
    Ok(LevelGapDensity {
        bid_bits: bid_level_gap_density(book, price_tick)?.to_bits(),
        ask_bits: ask_level_gap_density(book, price_tick)?.to_bits(),
    })
}

pub fn bid_level_gap_density(
    book: &BookSnapshotView,
    price_tick: Price,
) -> Result<f64, FeatureComputationError> {
    require_normal_book(book)?;
    side_gap_density(book.bids(), price_tick, true)
}

pub fn ask_level_gap_density(
    book: &BookSnapshotView,
    price_tick: Price,
) -> Result<f64, FeatureComputationError> {
    require_normal_book(book)?;
    side_gap_density(book.asks(), price_tick, false)
}

pub fn liquidity_wall_distance_bps(
    book: &BookSnapshotView,
    reference_price: Price,
    material_quote_notional: FixedDecimal,
) -> Result<LiquidityWallDistances, FeatureComputationError> {
    require_normal_book(book)?;
    require_spot_quote_notional(book.instrument())?;
    if !material_quote_notional.is_positive() {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let bid = nearest_material_wall(book.bids(), reference_price, material_quote_notional)?;
    let ask = nearest_material_wall(book.asks(), reference_price, material_quote_notional)?;
    Ok(LiquidityWallDistances {
        bid_bits: bid.map(f64::to_bits),
        ask_bits: ask.map(f64::to_bits),
    })
}

pub fn expected_sweep_cost(
    book: &BookSnapshotView,
    side: SweepSide,
    requested_quote_notional: FixedDecimal,
    reference_price: Price,
) -> Result<SweepCost, FeatureComputationError> {
    require_normal_book(book)?;
    require_spot_quote_notional(book.instrument())?;
    if !requested_quote_notional.is_positive() {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let levels = match side {
        SweepSide::Buy => book.asks(),
        SweepSide::Sell => book.bids(),
    };
    let mut remaining = requested_quote_notional;
    let mut executed_quote_notional = zero();
    let mut estimated_base_quantity = 0.0;

    for level in levels {
        if remaining.is_zero() {
            break;
        }
        let available_quote = level_notional(level)?;
        let (base_fill, quote_fill) = if remaining >= available_quote {
            (
                level.quantity.value().to_f64_lossy_for_analysis(),
                available_quote,
            )
        } else {
            let base_fill = remaining.to_f64_lossy_for_analysis()
                / level.price.value().to_f64_lossy_for_analysis();
            (base_fill, remaining)
        };
        finite(base_fill)?;
        estimated_base_quantity += base_fill;
        finite(estimated_base_quantity)?;
        executed_quote_notional = executed_quote_notional
            .checked_add(quote_fill)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        remaining = remaining
            .checked_sub(quote_fill)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
    }
    if !remaining.is_zero() {
        return Err(FeatureComputationError::BelowLiquidityThreshold);
    }
    if estimated_base_quantity <= 0.0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let average_float =
        executed_quote_notional.to_f64_lossy_for_analysis() / estimated_base_quantity;
    finite(average_float)?;
    let reference_float = reference_price.value().to_f64_lossy_for_analysis();
    let relative_cost = match side {
        SweepSide::Buy => average_float / reference_float - 1.0,
        SweepSide::Sell => 1.0 - average_float / reference_float,
    };
    finite(relative_cost)?;

    Ok(SweepCost {
        executed_quote_notional,
        estimated_base_quantity_bits: estimated_base_quantity.to_bits(),
        estimated_average_price_bits: average_float.to_bits(),
        relative_cost_bits: relative_cost.to_bits(),
    })
}

pub fn maximum_executable_quote_notional(
    book: &BookSnapshotView,
    side: SweepSide,
    slippage_limit_bps: u32,
    reference_price: Price,
) -> Result<FixedDecimal, FeatureComputationError> {
    require_normal_book(book)?;
    require_spot_quote_notional(book.instrument())?;
    let (lower_bound, upper_bound) = scaled_price_bounds(reference_price, slippage_limit_bps)?;
    let denominator = basis_points_denominator();
    let levels = match side {
        SweepSide::Buy => book.asks(),
        SweepSide::Sell => book.bids(),
    };
    let mut executable = zero();
    for level in levels {
        let scaled_price = level
            .price
            .value()
            .checked_mul(denominator)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        let inside_limit = match side {
            SweepSide::Buy => scaled_price <= upper_bound,
            SweepSide::Sell => scaled_price >= lower_bound,
        };
        if !inside_limit {
            break;
        }
        executable = executable
            .checked_add(level_notional(level)?)
            .map_err(|_| FeatureComputationError::InvalidInput)?;
    }
    Ok(executable)
}

pub(crate) fn require_normal_book(book: &BookSnapshotView) -> Result<(), FeatureComputationError> {
    if book.quality().health_state != BookState::Synchronized
        || book.classification() != BookClassification::Normal
    {
        return Err(FeatureComputationError::UntrustedInput);
    }
    if book.quality().last_update_age_ms > L2_UNAVAILABLE_AFTER_MS {
        return Err(FeatureComputationError::Stale);
    }
    Ok(())
}

fn scaled_band_bounds(
    reference_price: Price,
    band_bps: u32,
) -> Result<(FixedDecimal, FixedDecimal), FeatureComputationError> {
    if band_bps == 0 || band_bps > MAX_DEPTH_BAND_BPS {
        return Err(FeatureComputationError::InvalidParameter);
    }
    scaled_price_bounds(reference_price, band_bps)
}

fn scaled_price_bounds(
    reference_price: Price,
    band_bps: u32,
) -> Result<(FixedDecimal, FixedDecimal), FeatureComputationError> {
    if band_bps > MAX_DEPTH_BAND_BPS {
        return Err(FeatureComputationError::InvalidParameter);
    }
    let band = i128::from(band_bps);
    let lower_multiplier = FixedDecimal::new(BASIS_POINTS_DENOMINATOR - band, 0)
        .map_err(|_| FeatureComputationError::InvalidParameter)?;
    let upper_multiplier = FixedDecimal::new(BASIS_POINTS_DENOMINATOR + band, 0)
        .map_err(|_| FeatureComputationError::InvalidParameter)?;
    Ok((
        reference_price
            .value()
            .checked_mul(lower_multiplier)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
        reference_price
            .value()
            .checked_mul(upper_multiplier)
            .map_err(|_| FeatureComputationError::InvalidInput)?,
    ))
}

fn level_notional(level: &BookLevel) -> Result<FixedDecimal, FeatureComputationError> {
    level
        .price
        .value()
        .checked_mul(level.quantity.value())
        .map_err(|_| FeatureComputationError::InvalidInput)
}

fn require_spot_quote_notional(instrument: &InstrumentId) -> Result<(), FeatureComputationError> {
    if instrument.product_type() == ProductType::Spot {
        Ok(())
    } else {
        Err(FeatureComputationError::SourceNotSupported)
    }
}

fn book_state_digest(book: &BookSnapshotView) -> [u8; 32] {
    let mut hasher = Hasher::new();
    hasher.update(b"crypto-intelligence/trusted-book-feature-state/v2");
    hash_digest_bytes(&mut hasher, book.instrument().venue().as_str().as_bytes());
    hash_digest_bytes(&mut hasher, book.instrument().venue_symbol().as_bytes());
    hasher.update(&[book.instrument().product_type() as u8]);
    hasher.update(&book.instrument().generation().to_be_bytes());
    hash_digest_bytes(&mut hasher, book.price_tick().to_string().as_bytes());
    hash_digest_bytes(&mut hasher, book.quantity_step().to_string().as_bytes());
    let session = book.session();
    hasher.update(&session.connection_epoch.to_be_bytes());
    hasher.update(&session.subscription_epoch.to_be_bytes());
    hasher.update(&session.instrument_generation.to_be_bytes());
    hasher.update(&book.last_source_sequence().to_be_bytes());
    hasher.update(&book.updated_monotonic_ns().to_be_bytes());
    hasher.update(&[match book.classification() {
        BookClassification::Normal => 1,
        BookClassification::Locked => 2,
        BookClassification::Crossed => 3,
        BookClassification::OneSided => 4,
    }]);
    for (tag, levels) in [(1_u8, book.bids()), (2_u8, book.asks())] {
        hasher.update(&[tag]);
        hasher.update(&(levels.len() as u64).to_be_bytes());
        for level in levels {
            hash_digest_bytes(&mut hasher, level.price.to_string().as_bytes());
            hash_digest_bytes(&mut hasher, level.quantity.to_string().as_bytes());
            match level.order_count {
                Some(count) => {
                    hasher.update(&[1]);
                    hasher.update(&count.to_be_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
    }
    let quality = book.quality();
    hasher.update(&[match quality.health_state {
        BookState::Disconnected => 1,
        BookState::Buffering => 2,
        BookState::AwaitingSnapshot => 3,
        BookState::Replaying => 4,
        BookState::Synchronized => 5,
        BookState::Untrusted => 6,
    }]);
    hasher.update(&quality.last_source_sequence.to_be_bytes());
    hasher.update(&quality.last_update_age_ms.to_be_bytes());
    hasher.update(&[match quality.checksum_status {
        ChecksumStatus::NotSupported => 1,
        ChecksumStatus::Pending => 2,
        ChecksumStatus::Valid => 3,
        ChecksumStatus::Mismatch => 4,
    }]);
    hasher.update(&quality.resync_count_1h.to_be_bytes());
    hasher.update(&quality.missing_sequence_count_1h.to_be_bytes());
    hasher.update(&quality.checksum_failure_count_1h.to_be_bytes());
    hasher.update(&quality.crossed_state_count_1h.to_be_bytes());
    hash_optional_u64(&mut hasher, quality.source_latency_percentiles.p50_ms);
    hash_optional_u64(&mut hasher, quality.source_latency_percentiles.p95_ms);
    hash_optional_u64(&mut hasher, quality.source_latency_percentiles.p99_ms);
    hasher.update(&(quality.source_latency_percentiles.sample_count as u64).to_be_bytes());
    hasher.update(&(quality.trusted_depth_levels as u64).to_be_bytes());
    hasher.update(&quality.quality_score_ppm.to_be_bytes());
    *hasher.finalize().as_bytes()
}

fn hash_digest_bytes(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn hash_optional_u64(hasher: &mut Hasher, value: Option<u64>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hasher.update(&value.to_be_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn side_gap_density(
    levels: &[BookLevel],
    price_tick: Price,
    descending: bool,
) -> Result<f64, FeatureComputationError> {
    if levels.len() < 2 {
        return Err(FeatureComputationError::InsufficientHistory);
    }
    let (missing, intervals) =
        levels
            .windows(2)
            .try_fold((0_u128, 0_u128), |(missing, intervals), pair| {
                let distance = if descending {
                    pair[0].price.value().checked_sub(pair[1].price.value())
                } else {
                    pair[1].price.value().checked_sub(pair[0].price.value())
                }
                .map_err(|_| FeatureComputationError::InvalidInput)?;
                let ticks = distance
                    .checked_div_exact(price_tick.value())
                    .map_err(|_| FeatureComputationError::InvalidParameter)?;
                if ticks.scale() != 0 || !ticks.is_positive() {
                    return Err(FeatureComputationError::InvalidParameter);
                }
                let ticks = u128::try_from(ticks.mantissa())
                    .map_err(|_| FeatureComputationError::InvalidParameter)?;
                Ok((
                    missing
                        .checked_add(ticks - 1)
                        .ok_or(FeatureComputationError::CapacityExceeded)?,
                    intervals
                        .checked_add(ticks)
                        .ok_or(FeatureComputationError::CapacityExceeded)?,
                ))
            })?;
    if intervals == 0 {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    finite(missing as f64 / intervals as f64)
}

fn require_book_shape_level_count(level_count: usize) -> Result<(), FeatureComputationError> {
    if (3..=MAX_BOOK_SHAPE_LEVELS).contains(&level_count) {
        Ok(())
    } else {
        Err(FeatureComputationError::InvalidParameter)
    }
}

fn nearest_material_wall(
    levels: &[BookLevel],
    reference_price: Price,
    material_quote_notional: FixedDecimal,
) -> Result<Option<f64>, FeatureComputationError> {
    let mut material_level = None;
    for level in levels {
        if level_notional(level)? >= material_quote_notional {
            material_level = Some(level);
            break;
        }
    }
    let Some(level) = material_level else {
        return Ok(None);
    };
    let distance = if level.price >= reference_price {
        level.price.value().checked_sub(reference_price.value())
    } else {
        reference_price.value().checked_sub(level.price.value())
    }
    .map_err(|_| FeatureComputationError::InvalidInput)?;
    let distance_bps = distance.to_f64_lossy_for_analysis()
        / reference_price.value().to_f64_lossy_for_analysis()
        * 10_000.0;
    finite(distance_bps).map(Some)
}

fn fit_side_shape(
    levels: &[BookLevel],
    price_tick: Price,
    descending: bool,
) -> Result<BookSideShape, FeatureComputationError> {
    let inside = levels[0].price.value();
    let mut cumulative_depth = zero();
    let mut count = 0.0;
    let mut sum_x = 0.0;
    let mut sum_x2 = 0.0;
    let mut sum_x3 = 0.0;
    let mut sum_x4 = 0.0;
    let mut sum_y = 0.0;
    let mut sum_xy = 0.0;
    let mut sum_x2y = 0.0;

    for level in levels {
        let distance = if descending {
            inside.checked_sub(level.price.value())
        } else {
            level.price.value().checked_sub(inside)
        }
        .map_err(|_| FeatureComputationError::InvalidInput)?;
        let ticks = distance
            .checked_div_exact(price_tick.value())
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        if ticks.scale() != 0 || ticks.is_negative() {
            return Err(FeatureComputationError::InvalidInput);
        }
        cumulative_depth = cumulative_depth
            .checked_add(level.quantity.value())
            .map_err(|_| FeatureComputationError::InvalidInput)?;
        let x = ticks.to_f64_lossy_for_analysis();
        let y = cumulative_depth.to_f64_lossy_for_analysis();
        let x2 = x * x;
        count += 1.0;
        sum_x += x;
        sum_x2 += x2;
        sum_x3 += x2 * x;
        sum_x4 += x2 * x2;
        sum_y += y;
        sum_xy += x * y;
        sum_x2y += x2 * y;
    }

    let determinant = determinant_3x3(
        count, sum_x, sum_x2, sum_x, sum_x2, sum_x3, sum_x2, sum_x3, sum_x4,
    );
    if !determinant.is_finite() || determinant.abs() <= f64::EPSILON {
        return Err(FeatureComputationError::ZeroDenominator);
    }
    let slope_numerator = determinant_3x3(
        count, sum_y, sum_x2, sum_x, sum_xy, sum_x3, sum_x2, sum_x2y, sum_x4,
    );
    let quadratic_numerator = determinant_3x3(
        count, sum_x, sum_y, sum_x, sum_x2, sum_xy, sum_x2, sum_x3, sum_x2y,
    );
    let slope = slope_numerator / determinant;
    let convexity = 2.0 * quadratic_numerator / determinant;
    finite(slope)?;
    finite(convexity)?;
    Ok(BookSideShape {
        slope_at_inside_bits: slope.to_bits(),
        convexity_bits: convexity.to_bits(),
    })
}

#[allow(clippy::too_many_arguments)]
fn determinant_3x3(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64, g: f64, h: f64, i: f64) -> f64 {
    a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g)
}

fn basis_points_denominator() -> FixedDecimal {
    FixedDecimal::new(BASIS_POINTS_DENOMINATOR, 0).expect("basis-point denominator is valid")
}

fn zero() -> FixedDecimal {
    FixedDecimal::new(0, 0).expect("zero is valid")
}

fn finite(value: f64) -> Result<f64, FeatureComputationError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(FeatureComputationError::AnalyticalUnavailable)
    }
}
