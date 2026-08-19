//! Pure, deterministic fair-price estimation and venue inclusion lineage.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
};

use domain::{
    AssetId, InstrumentDefinition, InstrumentId, ProductType, SourceId, SourceKind, UnixNanos,
};
use feature_registry::DurationNanos;
use fixed_decimal::{DecimalError, FixedDecimal, Notional, Price, Quantity};
use instrument_registry::{CatalogRevision, CatalogSnapshot};
use num_bigint::BigInt;
use orderbook::{
    BookClassification, BookError, BookSession, BookSnapshotView, BookState, ChecksumStatus,
    OrderBookEngine,
};
use quality::SourceHealthState;
use thiserror::Error;

use crate::{PriceInterval, QuoteConversionReference, StablecoinDislocationState};

const MAX_POLICY_ID_LENGTH: usize = 96;
const PPM_DENOMINATOR: u32 = 1_000_000;
const LINEAGE_DOMAIN: &[u8] = b"crypto-intelligence/consolidated-lineage/v1";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PolicyId(String);

impl PolicyId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ConsolidatedError> {
        let value = value.as_ref();
        if value.is_empty()
            || value.len() > MAX_POLICY_ID_LENGTH
            || !value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
        {
            return Err(ConsolidatedError::InvalidPolicyId);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Ppm(u32);

impl Ppm {
    pub fn new(value: u32) -> Result<Self, ConsolidatedError> {
        if value > PPM_DENOMINATOR {
            Err(ConsolidatedError::InvalidPpm)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum CandidatePriceKind {
    Midpoint = 1,
    Microprice = 2,
    RobustTrade = 3,
    MarkOrIndex = 4,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum MetadataStatus {
    Current = 1,
    Stale = 2,
    Ambiguous = 3,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum VenueTradingState {
    Normal = 1,
    Suspended = 2,
    Auction = 3,
    Maintenance = 4,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum VenueExclusionReason {
    SourceUnhealthy = 1,
    QuoteStale = 2,
    FutureObservation = 3,
    MetadataStale = 4,
    MetadataAmbiguous = 5,
    UnversionedInstrument = 6,
    VenueSuspended = 7,
    VenueAuction = 8,
    VenueMaintenance = 9,
    ClockUnhealthy = 10,
    ParserUnhealthy = 11,
    QualityBelowMinimum = 12,
    FreshnessBelowMinimum = 13,
    ConversionUnavailable = 14,
    ConversionStale = 15,
    ConversionInsufficientCoverage = 16,
    ConversionUncertain = 17,
    ConversionDislocated = 18,
    AssetMismatch = 19,
    ProductMismatch = 20,
    UnsupportedCandidateKind = 21,
    UnconfirmedOutlier = 22,
    ZeroWeight = 23,
    InsufficientConsolidatedCoverage = 24,
    ConversionInconsistent = 25,
    BookDegraded = 26,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AbstentionReason {
    InsufficientHealthyVenues,
}

#[derive(Clone, Debug)]
pub struct EstimatorConfigInput {
    pub policy_id: PolicyId,
    pub base_asset: AssetId,
    pub reference_asset: AssetId,
    pub product_type: ProductType,
    pub minimum_venues: usize,
    pub maximum_venues: usize,
    pub quote_ttl: DurationNanos,
    pub conversion_ttl: DurationNanos,
    pub depth_cap_reference: Notional,
    pub minimum_quality: Ppm,
    pub minimum_freshness: Ppm,
    pub maximum_venue_weight: Ppm,
    pub outlier_threshold: Ppm,
    pub minimum_conversion_sources: u16,
    pub maximum_conversion_interval_width: Ppm,
    pub require_catalog_provenance: bool,
}

#[derive(Clone, Debug)]
struct EstimatorConfig {
    policy_id: PolicyId,
    base_asset: AssetId,
    reference_asset: AssetId,
    product_type: ProductType,
    minimum_venues: usize,
    maximum_venues: usize,
    quote_ttl: i64,
    conversion_ttl: i64,
    depth_cap_reference: Notional,
    minimum_quality: Ppm,
    minimum_freshness: Ppm,
    maximum_venue_weight: Ppm,
    outlier_threshold: Ppm,
    minimum_conversion_sources: u16,
    maximum_conversion_interval_width: Ppm,
    require_catalog_provenance: bool,
    eligible_sources: Option<Box<[SourceId]>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstrumentProvenance {
    definition_revision: Option<CatalogRevision>,
    catalog_revision: Option<CatalogRevision>,
    catalog_as_known_at: Option<UnixNanos>,
    catalog_digest: Option<[u8; 32]>,
}

impl InstrumentProvenance {
    pub const fn direct_definition() -> Self {
        Self {
            definition_revision: None,
            catalog_revision: None,
            catalog_as_known_at: None,
            catalog_digest: None,
        }
    }

    const fn catalog(
        definition_revision: CatalogRevision,
        catalog_revision: CatalogRevision,
        catalog_as_known_at: UnixNanos,
        catalog_digest: [u8; 32],
    ) -> Self {
        Self {
            definition_revision: Some(definition_revision),
            catalog_revision: Some(catalog_revision),
            catalog_as_known_at: Some(catalog_as_known_at),
            catalog_digest: Some(catalog_digest),
        }
    }

    pub const fn is_catalog_bound(&self) -> bool {
        self.definition_revision.is_some()
            && self.catalog_revision.is_some()
            && self.catalog_as_known_at.is_some()
            && self.catalog_digest.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct VenueQuoteInput {
    pub source: SourceId,
    pub instrument: InstrumentDefinition,
    pub candidate_kind: CandidatePriceKind,
    pub bid: Price,
    pub ask: Price,
    pub executable_quantity: Quantity,
    pub quality: Ppm,
    pub freshness: Ppm,
    pub source_health: SourceHealthState,
    pub metadata_status: MetadataStatus,
    pub venue_status: VenueTradingState,
    pub clock_healthy: bool,
    pub parser_healthy: bool,
    pub book_healthy: bool,
    pub event_time: UnixNanos,
    pub as_known_at: UnixNanos,
    pub conversion: Option<QuoteConversionReference>,
    pub instrument_provenance: InstrumentProvenance,
}

#[derive(Clone, Debug)]
struct TrustedBookInput<'a> {
    source: SourceId,
    instrument_id: &'a InstrumentId,
    catalog: &'a VerifiedCatalogSnapshot<'a>,
    snapshot: &'a BookSnapshotView,
    candidate_kind: CandidatePriceKind,
    depth_levels: usize,
    source_health: SourceHealthState,
    metadata_status: MetadataStatus,
    venue_status: VenueTradingState,
    clock_healthy: bool,
    parser_healthy: bool,
    book_healthy: bool,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    conversion: Option<QuoteConversionReference>,
}

pub struct TrustedBookAdmissionInput<'a> {
    pub source: SourceId,
    pub engine: &'a OrderBookEngine,
    pub catalog: &'a VerifiedCatalogSnapshot<'a>,
    pub now_monotonic_ns: u64,
    pub candidate_kind: CandidatePriceKind,
    pub depth_levels: usize,
    pub source_health: SourceHealthState,
    pub metadata_status: MetadataStatus,
    pub venue_status: VenueTradingState,
    pub clock_healthy: bool,
    pub parser_healthy: bool,
    pub event_time: UnixNanos,
    pub as_known_at: UnixNanos,
    pub conversion: Option<QuoteConversionReference>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookAdmissionExclusion {
    source: SourceId,
    instrument: InstrumentId,
    state: BookState,
    error: BookError,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
}

impl BookAdmissionExclusion {
    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn instrument(&self) -> &InstrumentId {
        &self.instrument
    }

    pub const fn state(&self) -> BookState {
        self.state
    }

    pub const fn error(&self) -> &BookError {
        &self.error
    }

    pub const fn event_time(&self) -> UnixNanos {
        self.event_time
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TrustedBookAdmission {
    Accepted(Box<VenueQuote>),
    Excluded(BookAdmissionExclusion),
}

#[derive(Clone, Copy, Debug)]
pub struct VerifiedCatalogSnapshot<'a> {
    snapshot: &'a CatalogSnapshot,
}

impl<'a> VerifiedCatalogSnapshot<'a> {
    pub fn try_new(snapshot: &'a CatalogSnapshot) -> Result<Self, ConsolidatedError> {
        if !snapshot.verify_integrity() {
            return Err(ConsolidatedError::CatalogIntegrity);
        }
        Ok(Self { snapshot })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookProvenance {
    session: BookSession,
    source_sequence: u64,
    checksum_status: ChecksumStatus,
    classification: BookClassification,
    updated_monotonic_ns: u64,
    age_ms: u64,
    depth_levels: usize,
}

impl BookProvenance {
    pub const fn session(&self) -> BookSession {
        self.session
    }

    pub const fn source_sequence(&self) -> u64 {
        self.source_sequence
    }

    pub const fn checksum_status(&self) -> ChecksumStatus {
        self.checksum_status
    }

    pub const fn classification(&self) -> BookClassification {
        self.classification
    }

    pub const fn age_ms(&self) -> u64 {
        self.age_ms
    }

    pub const fn updated_monotonic_ns(&self) -> u64 {
        self.updated_monotonic_ns
    }

    pub const fn depth_levels(&self) -> usize {
        self.depth_levels
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VenueQuote {
    source: SourceId,
    instrument: InstrumentDefinition,
    candidate_kind: CandidatePriceKind,
    bid: Price,
    ask: Price,
    midpoint: Price,
    executable_depth_quote: Notional,
    quality: Ppm,
    freshness: Ppm,
    source_health: SourceHealthState,
    metadata_status: MetadataStatus,
    venue_status: VenueTradingState,
    clock_healthy: bool,
    parser_healthy: bool,
    book_healthy: bool,
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    conversion: Option<QuoteConversionReference>,
    instrument_provenance: InstrumentProvenance,
    book_provenance: Option<BookProvenance>,
}

impl VenueQuote {
    pub fn try_new(input: VenueQuoteInput) -> Result<Self, ConsolidatedError> {
        if input.source.kind() != SourceKind::Exchange
            || input.source.name() != input.instrument.id().venue().as_str()
            || input.bid > input.ask
            || input.event_time.value() <= 0
            || input.as_known_at < input.event_time
            || input.executable_quantity.value().is_zero()
        {
            return Err(ConsolidatedError::InvalidQuote);
        }
        let midpoint = average_price(input.bid, input.ask)?;
        let executable_depth_quote = input
            .instrument
            .quote_notional(midpoint, input.executable_quantity)
            .map_err(|_| ConsolidatedError::InvalidQuote)?;
        Self::from_validated_parts(input, midpoint, executable_depth_quote)
    }

    fn from_trusted_book_with_ttl(
        input: TrustedBookInput<'_>,
        quote_ttl_ns: i64,
    ) -> Result<Self, ConsolidatedError> {
        if input.catalog.snapshot.as_known_at() > input.as_known_at
            || input.depth_levels == 0
            || quote_ttl_ns <= 0
            || input.snapshot.quality().health_state != BookState::Synchronized
            || input.snapshot.instrument() != input.instrument_id
            || input.snapshot.classification() != BookClassification::Normal
            || !matches!(
                input.snapshot.quality().checksum_status,
                ChecksumStatus::NotSupported | ChecksumStatus::Valid
            )
            || input.snapshot.session().instrument_generation != input.instrument_id.generation()
            || input.snapshot.quality().trusted_depth_levels
                < input
                    .depth_levels
                    .checked_mul(2)
                    .ok_or(ConsolidatedError::Arithmetic)?
        {
            return Err(ConsolidatedError::UntrustedBook);
        }
        let resolved = input
            .catalog
            .snapshot
            .resolve_id(input.instrument_id, input.event_time)
            .map_err(|_| ConsolidatedError::InstrumentResolution)?;
        let bids = input.snapshot.bids();
        let asks = input.snapshot.asks();
        if bids.len() < input.depth_levels || asks.len() < input.depth_levels {
            return Err(ConsolidatedError::UntrustedBook);
        }
        let instrument = resolved.definition().clone();
        let bid = bids[0].price;
        let ask = asks[0].price;
        let midpoint = average_price(bid, ask)?;
        let bid_depth = side_quote_notional(&instrument, &bids[..input.depth_levels])?;
        let ask_depth = side_quote_notional(&instrument, &asks[..input.depth_levels])?;
        let executable_depth_quote = std::cmp::min(bid_depth, ask_depth);
        let age_ms = input.snapshot.quality().last_update_age_ms;
        let age_ns = u128::from(age_ms)
            .checked_mul(1_000_000)
            .ok_or(ConsolidatedError::Arithmetic)?;
        let quote_ttl_ns =
            u128::try_from(quote_ttl_ns).map_err(|_| ConsolidatedError::Arithmetic)?;
        let freshness = if age_ns >= quote_ttl_ns {
            Ppm(0)
        } else {
            let consumed = u32::try_from(
                age_ns
                    .checked_mul(u128::from(PPM_DENOMINATOR))
                    .ok_or(ConsolidatedError::Arithmetic)?
                    / quote_ttl_ns,
            )
            .map_err(|_| ConsolidatedError::Arithmetic)?;
            Ppm(PPM_DENOMINATOR
                .checked_sub(consumed)
                .ok_or(ConsolidatedError::Arithmetic)?)
        };
        let quote = VenueQuoteInput {
            source: input.source,
            instrument,
            candidate_kind: input.candidate_kind,
            bid,
            ask,
            executable_quantity: Quantity::new(FixedDecimal::new(1, 0)?)?,
            quality: Ppm::new(input.snapshot.quality().quality_score_ppm)?,
            freshness,
            source_health: input.source_health,
            metadata_status: input.metadata_status,
            venue_status: input.venue_status,
            clock_healthy: input.clock_healthy,
            parser_healthy: input.parser_healthy,
            book_healthy: input.book_healthy,
            event_time: input.event_time,
            as_known_at: input.as_known_at,
            conversion: input.conversion,
            instrument_provenance: InstrumentProvenance::catalog(
                resolved.definition_revision(),
                resolved.catalog_revision(),
                resolved.as_known_at(),
                *resolved.catalog_digest(),
            ),
        };
        let mut quote = Self::from_validated_parts(quote, midpoint, executable_depth_quote)?;
        quote.book_provenance = Some(BookProvenance {
            session: input.snapshot.session(),
            source_sequence: input.snapshot.last_source_sequence(),
            checksum_status: input.snapshot.quality().checksum_status,
            classification: input.snapshot.classification(),
            updated_monotonic_ns: input.snapshot.updated_monotonic_ns(),
            age_ms: input.snapshot.quality().last_update_age_ms,
            depth_levels: input.depth_levels,
        });
        Ok(quote)
    }

    fn from_validated_parts(
        input: VenueQuoteInput,
        midpoint: Price,
        executable_depth_quote: Notional,
    ) -> Result<Self, ConsolidatedError> {
        if input.source.kind() != SourceKind::Exchange
            || input.source.name() != input.instrument.id().venue().as_str()
            || input.bid > input.ask
            || input.event_time.value() <= 0
            || input.as_known_at < input.event_time
            || input.event_time < input.instrument.listing_time()
            || input
                .instrument
                .delisting_time()
                .is_some_and(|delisting| input.event_time >= delisting)
            || executable_depth_quote.value().is_zero()
        {
            return Err(ConsolidatedError::InvalidQuote);
        }
        Ok(Self {
            source: input.source,
            instrument: input.instrument,
            candidate_kind: input.candidate_kind,
            bid: input.bid,
            ask: input.ask,
            midpoint,
            executable_depth_quote,
            quality: input.quality,
            freshness: input.freshness,
            source_health: input.source_health,
            metadata_status: input.metadata_status,
            venue_status: input.venue_status,
            clock_healthy: input.clock_healthy,
            parser_healthy: input.parser_healthy,
            book_healthy: input.book_healthy,
            event_time: input.event_time,
            as_known_at: input.as_known_at,
            conversion: input.conversion,
            instrument_provenance: input.instrument_provenance,
            book_provenance: None,
        })
    }

    pub const fn midpoint(&self) -> Price {
        self.midpoint
    }

    pub const fn candidate_kind(&self) -> CandidatePriceKind {
        self.candidate_kind
    }

    pub const fn bid(&self) -> Price {
        self.bid
    }

    pub const fn ask(&self) -> Price {
        self.ask
    }

    pub const fn executable_depth_quote(&self) -> Notional {
        self.executable_depth_quote
    }

    pub const fn instrument_provenance(&self) -> &InstrumentProvenance {
        &self.instrument_provenance
    }

    pub const fn book_provenance(&self) -> Option<&BookProvenance> {
        self.book_provenance.as_ref()
    }
}

fn side_quote_notional(
    instrument: &InstrumentDefinition,
    levels: &[event_envelope::BookLevel],
) -> Result<Notional, ConsolidatedError> {
    let mut total = FixedDecimal::new(0, 0)?;
    for level in levels {
        let notional = instrument
            .quote_notional(level.price, level.quantity)
            .map_err(|_| ConsolidatedError::InvalidQuote)?;
        total = total.checked_add(notional.value())?;
    }
    Ok(Notional::new(total)?)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncludedVenue {
    quote: VenueQuote,
    adjusted_price: Price,
    adjusted_interval: PriceInterval,
    adjusted_depth: Notional,
    raw_weight: u64,
    effective_weight: Ppm,
}

impl IncludedVenue {
    pub const fn source(&self) -> &SourceId {
        &self.quote.source
    }

    pub const fn source_health(&self) -> SourceHealthState {
        self.quote.source_health
    }

    pub const fn quality(&self) -> Ppm {
        self.quote.quality
    }

    pub const fn raw_weight(&self) -> u64 {
        self.raw_weight
    }

    pub const fn adjusted_price(&self) -> Price {
        self.adjusted_price
    }

    pub const fn adjusted_interval(&self) -> PriceInterval {
        self.adjusted_interval
    }

    pub const fn adjusted_depth_reference(&self) -> Notional {
        self.adjusted_depth
    }

    pub const fn book_provenance(&self) -> Option<&BookProvenance> {
        self.quote.book_provenance.as_ref()
    }

    pub const fn instrument_provenance(&self) -> &InstrumentProvenance {
        &self.quote.instrument_provenance
    }

    pub const fn effective_weight(&self) -> Ppm {
        self.effective_weight
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExcludedVenue {
    quote: VenueQuote,
    reason: VenueExclusionReason,
}

impl ExcludedVenue {
    pub const fn source(&self) -> &SourceId {
        &self.quote.source
    }

    pub const fn reason(&self) -> VenueExclusionReason {
        self.reason
    }

    pub const fn book_provenance(&self) -> Option<&BookProvenance> {
        self.quote.book_provenance.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsolidatedLineage {
    policy_id: PolicyId,
    as_of: UnixNanos,
    eligible_sources: Option<Box<[SourceId]>>,
    included: Vec<IncludedVenue>,
    excluded: Vec<ExcludedVenue>,
    book_admission_excluded: Vec<BookAdmissionExclusion>,
    digest: [u8; 32],
}

impl ConsolidatedLineage {
    pub const fn policy_id(&self) -> &PolicyId {
        &self.policy_id
    }

    pub const fn as_of(&self) -> UnixNanos {
        self.as_of
    }

    /// Canonical source universe fixed by the estimator policy.
    pub fn eligible_sources(&self) -> Option<&[SourceId]> {
        self.eligible_sources.as_deref()
    }

    pub fn decision(&self, source: &SourceId) -> Option<&VenueExclusionReason> {
        self.excluded
            .iter()
            .find(|entry| &entry.quote.source == source)
            .map(|entry| &entry.reason)
    }

    pub fn excluded_count(&self) -> usize {
        self.excluded.len()
    }

    /// Canonical, bounded exclusion records ready for a feature or audit sink.
    ///
    /// This crate does not itself claim durable audit persistence.
    pub fn excluded(&self) -> &[ExcludedVenue] {
        &self.excluded
    }

    pub fn book_admission_excluded(&self) -> &[BookAdmissionExclusion] {
        &self.book_admission_excluded
    }

    pub fn included(&self) -> &[IncludedVenue] {
        &self.included
    }

    pub fn input_count(&self) -> usize {
        self.included.len() + self.excluded.len() + self.book_admission_excluded.len()
    }

    pub const fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FairPrice {
    price: Price,
    interval: PriceInterval,
    minimum_included_quality: Ppm,
    minimum_included_freshness: Ppm,
    lineage: ConsolidatedLineage,
}

impl FairPrice {
    pub fn base_asset(&self) -> &AssetId {
        self.lineage
            .included
            .first()
            .expect("available fair price always has an included venue")
            .quote
            .instrument
            .base_asset()
    }

    pub const fn price(&self) -> Price {
        self.price
    }

    /// Latest source-event time contributing to this consolidated estimate.
    pub fn event_time(&self) -> UnixNanos {
        self.lineage
            .included
            .iter()
            .map(|entry| entry.quote.event_time)
            .max()
            .expect("available fair price always has an included venue")
    }

    pub const fn interval(&self) -> PriceInterval {
        self.interval
    }

    pub const fn lineage(&self) -> &ConsolidatedLineage {
        &self.lineage
    }

    pub const fn minimum_included_quality(&self) -> Ppm {
        self.minimum_included_quality
    }

    pub const fn minimum_included_freshness(&self) -> Ppm {
        self.minimum_included_freshness
    }

    pub const fn is_executable(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsolidatedOutcome {
    Available(FairPrice),
    Abstained {
        reason: AbstentionReason,
        lineage: ConsolidatedLineage,
    },
}

#[derive(Clone, Debug)]
pub struct FairPriceEstimator {
    config: EstimatorConfig,
}

impl FairPriceEstimator {
    pub fn try_new(input: EstimatorConfigInput) -> Result<Self, ConsolidatedError> {
        let quote_ttl =
            i64::try_from(input.quote_ttl.value()).map_err(|_| ConsolidatedError::InvalidConfig)?;
        let conversion_ttl = i64::try_from(input.conversion_ttl.value())
            .map_err(|_| ConsolidatedError::InvalidConfig)?;
        if input.minimum_venues < 2
            || input.minimum_venues > input.maximum_venues
            || input.maximum_venues > 64
            || !matches!(
                input.product_type,
                ProductType::Spot | ProductType::Perpetual
            )
            || quote_ttl <= 0
            || conversion_ttl <= 0
            || input.depth_cap_reference.value().is_zero()
            || input.minimum_quality.value() == 0
            || input.minimum_freshness.value() == 0
            || input.maximum_venue_weight.value() == 0
            || input.maximum_venue_weight.value() > 500_000
            || input.outlier_threshold.value() == 0
            || input.minimum_conversion_sources < 2
            || input.maximum_conversion_interval_width.value() == 0
            || (input.minimum_venues as u64)
                .checked_mul(u64::from(input.maximum_venue_weight.value()))
                .is_none_or(|capacity| capacity < u64::from(PPM_DENOMINATOR))
        {
            return Err(ConsolidatedError::InvalidConfig);
        }
        Ok(Self {
            config: EstimatorConfig {
                policy_id: input.policy_id,
                base_asset: input.base_asset,
                reference_asset: input.reference_asset,
                product_type: input.product_type,
                minimum_venues: input.minimum_venues,
                maximum_venues: input.maximum_venues,
                quote_ttl,
                conversion_ttl,
                depth_cap_reference: input.depth_cap_reference,
                minimum_quality: input.minimum_quality,
                minimum_freshness: input.minimum_freshness,
                maximum_venue_weight: input.maximum_venue_weight,
                outlier_threshold: input.outlier_threshold,
                minimum_conversion_sources: input.minimum_conversion_sources,
                maximum_conversion_interval_width: input.maximum_conversion_interval_width,
                require_catalog_provenance: input.require_catalog_provenance,
                eligible_sources: None,
            },
        })
    }

    /// Creates an estimator with an authoritative coverage denominator.
    pub fn try_new_with_eligible_sources(
        input: EstimatorConfigInput,
        mut eligible_sources: Vec<SourceId>,
    ) -> Result<Self, ConsolidatedError> {
        let mut estimator = Self::try_new(input)?;
        if eligible_sources.len() < estimator.config.minimum_venues
            || eligible_sources.len() > estimator.config.maximum_venues
            || eligible_sources
                .iter()
                .any(|source| source.kind() != SourceKind::Exchange)
        {
            return Err(ConsolidatedError::InvalidConfig);
        }
        eligible_sources.sort_by(compare_source);
        if eligible_sources.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(ConsolidatedError::InvalidConfig);
        }
        estimator.config.eligible_sources = Some(eligible_sources.into_boxed_slice());
        Ok(estimator)
    }

    pub fn admit_trusted_book(
        &self,
        input: TrustedBookAdmissionInput<'_>,
    ) -> Result<TrustedBookAdmission, ConsolidatedError> {
        if input.source.kind() != SourceKind::Exchange
            || input.source.name() != input.engine.instrument().venue().as_str()
            || input.event_time.value() <= 0
            || input.as_known_at < input.event_time
        {
            return Err(ConsolidatedError::InvalidQuote);
        }
        let snapshot = match input.engine.snapshot_at(input.now_monotonic_ns) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Ok(TrustedBookAdmission::Excluded(BookAdmissionExclusion {
                    source: input.source,
                    instrument: input.engine.instrument().clone(),
                    state: input.engine.state(),
                    error,
                    event_time: input.event_time,
                    as_known_at: input.as_known_at,
                }));
            }
        };
        let source = input.source.clone();
        let instrument = input.engine.instrument().clone();
        let state = input.engine.state();
        let event_time = input.event_time;
        let as_known_at = input.as_known_at;
        let quote = match VenueQuote::from_trusted_book_with_ttl(
            TrustedBookInput {
                source: input.source,
                instrument_id: input.engine.instrument(),
                catalog: input.catalog,
                snapshot: &snapshot,
                candidate_kind: input.candidate_kind,
                depth_levels: input.depth_levels,
                source_health: input.source_health,
                metadata_status: input.metadata_status,
                venue_status: input.venue_status,
                clock_healthy: input.clock_healthy,
                parser_healthy: input.parser_healthy,
                book_healthy: true,
                event_time: input.event_time,
                as_known_at: input.as_known_at,
                conversion: input.conversion,
            },
            self.config.quote_ttl,
        ) {
            Ok(quote) => quote,
            Err(ConsolidatedError::UntrustedBook) => {
                return Ok(TrustedBookAdmission::Excluded(BookAdmissionExclusion {
                    source,
                    instrument,
                    state,
                    error: BookError::Untrusted,
                    event_time,
                    as_known_at,
                }));
            }
            Err(error) => return Err(error),
        };
        Ok(TrustedBookAdmission::Accepted(Box::new(quote)))
    }

    pub fn estimate(
        &self,
        as_of: UnixNanos,
        quotes: &[VenueQuote],
    ) -> Result<ConsolidatedOutcome, ConsolidatedError> {
        self.estimate_internal(as_of, quotes, Vec::new())
    }

    pub fn estimate_admissions(
        &self,
        as_of: UnixNanos,
        admissions: &[TrustedBookAdmission],
    ) -> Result<ConsolidatedOutcome, ConsolidatedError> {
        if admissions.is_empty() || admissions.len() > self.config.maximum_venues {
            return Err(ConsolidatedError::InvalidInputSet);
        }
        let mut quotes = Vec::new();
        let mut book_excluded = Vec::new();
        for admission in admissions {
            match admission {
                TrustedBookAdmission::Accepted(quote) => quotes.push((**quote).clone()),
                TrustedBookAdmission::Excluded(excluded) => book_excluded.push(excluded.clone()),
            }
        }
        self.estimate_internal(as_of, &quotes, book_excluded)
    }

    fn estimate_internal(
        &self,
        as_of: UnixNanos,
        quotes: &[VenueQuote],
        book_admission_excluded: Vec<BookAdmissionExclusion>,
    ) -> Result<ConsolidatedOutcome, ConsolidatedError> {
        if as_of.value() <= 0
            || quotes.len() + book_admission_excluded.len() > self.config.maximum_venues
            || quotes.is_empty() && book_admission_excluded.is_empty()
        {
            return Err(ConsolidatedError::InvalidInputSet);
        }
        let mut unique_sources =
            HashSet::with_capacity(quotes.len() + book_admission_excluded.len());
        let mut unique_venues =
            HashSet::with_capacity(quotes.len() + book_admission_excluded.len());
        for excluded in &book_admission_excluded {
            if !self.is_configured_source(&excluded.source) {
                return Err(ConsolidatedError::InvalidInputSet);
            }
            if !unique_sources.insert(excluded.source.clone())
                || !unique_venues.insert(excluded.instrument.venue().clone())
            {
                return Err(ConsolidatedError::DuplicateVenueIdentity);
            }
        }
        for quote in quotes {
            if !self.is_configured_source(&quote.source) {
                return Err(ConsolidatedError::InvalidInputSet);
            }
            if !unique_sources.insert(quote.source.clone())
                || !unique_venues.insert(quote.instrument.id().venue().clone())
            {
                return Err(ConsolidatedError::DuplicateVenueIdentity);
            }
        }
        let mut ordered: Vec<_> = quotes.iter().collect();
        ordered.sort_by(|left, right| canonical_quote_key(left).cmp(&canonical_quote_key(right)));
        let mut excluded = Vec::new();
        let mut eligible = Vec::new();
        for quote in ordered {
            if let Some(reason) = self.preliminary_exclusion(as_of, quote)? {
                excluded.push(excluded_venue(quote, reason));
                continue;
            }
            if quote.instrument.base_asset() != &self.config.base_asset {
                excluded.push(excluded_venue(quote, VenueExclusionReason::AssetMismatch));
                continue;
            }
            if quote.instrument.product_type() != self.config.product_type {
                excluded.push(excluded_venue(quote, VenueExclusionReason::ProductMismatch));
                continue;
            }
            if quote.candidate_kind != CandidatePriceKind::Midpoint {
                excluded.push(excluded_venue(
                    quote,
                    VenueExclusionReason::UnsupportedCandidateKind,
                ));
                continue;
            }
            eligible.push(quote);
        }
        let mut candidates = Vec::new();
        for quote in eligible {
            match self.adjusted_candidate(as_of, quote)? {
                Ok(candidate) => candidates.push(candidate),
                Err(reason) => excluded.push(excluded_venue(quote, reason)),
            }
        }

        let mut conversion_digests = HashMap::<AssetId, [u8; 32]>::new();
        let mut inconsistent_conversion_assets = HashSet::<AssetId>::new();
        for candidate in &candidates {
            if let Some(reference) = candidate.quote.conversion.as_ref() {
                match conversion_digests.get(candidate.quote.instrument.quote_asset()) {
                    Some(digest) if digest != reference.evidence_digest() => {
                        inconsistent_conversion_assets
                            .insert(candidate.quote.instrument.quote_asset().clone());
                    }
                    Some(_) => {}
                    None => {
                        conversion_digests.insert(
                            candidate.quote.instrument.quote_asset().clone(),
                            *reference.evidence_digest(),
                        );
                    }
                }
            }
        }
        if !inconsistent_conversion_assets.is_empty() {
            let mut consistent_candidates = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                if inconsistent_conversion_assets.contains(candidate.quote.instrument.quote_asset())
                {
                    excluded.push(excluded_venue(
                        candidate.quote,
                        VenueExclusionReason::ConversionInconsistent,
                    ));
                } else {
                    consistent_candidates.push(candidate);
                }
            }
            candidates = consistent_candidates;
        }

        if candidates.len() >= 2 {
            let median =
                ordinary_median(candidates.iter().map(|candidate| candidate.price).collect())?;
            let mut retained = Vec::with_capacity(candidates.len());
            for candidate in candidates {
                if deviation_exceeds(candidate.price, median, self.config.outlier_threshold)? {
                    excluded.push(excluded_venue(
                        candidate.quote,
                        VenueExclusionReason::UnconfirmedOutlier,
                    ));
                } else {
                    retained.push(candidate);
                }
            }
            candidates = retained;
        }

        if candidates.len() < self.config.minimum_venues {
            exclude_remaining_candidates(&mut excluded, candidates);
            let lineage = build_lineage(
                &self.config,
                as_of,
                &[],
                excluded,
                book_admission_excluded.clone(),
            );
            return Ok(ConsolidatedOutcome::Abstained {
                reason: AbstentionReason::InsufficientHealthyVenues,
                lineage,
            });
        }

        let mut raw_weights = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            let depth = ratio_ppm(
                candidate.depth_reference.value(),
                self.config.depth_cap_reference.value(),
            )?;
            let raw = u64::from(depth.value())
                .checked_mul(u64::from(candidate.quote.quality.value()))
                .and_then(|value| value.checked_mul(u64::from(candidate.quote.freshness.value())))
                .ok_or(ConsolidatedError::Arithmetic)?;
            if raw == 0 {
                excluded.push(excluded_venue(
                    candidate.quote,
                    VenueExclusionReason::ZeroWeight,
                ));
            }
            raw_weights.push(raw);
        }
        if raw_weights.contains(&0) {
            let mut retained_candidates = Vec::new();
            let mut retained_weights = Vec::new();
            for (candidate, raw) in candidates.into_iter().zip(raw_weights) {
                if raw != 0 {
                    retained_candidates.push(candidate);
                    retained_weights.push(raw);
                }
            }
            candidates = retained_candidates;
            raw_weights = retained_weights;
        }
        if candidates.len() < self.config.minimum_venues {
            exclude_remaining_candidates(&mut excluded, candidates);
            let lineage = build_lineage(
                &self.config,
                as_of,
                &[],
                excluded,
                book_admission_excluded.clone(),
            );
            return Ok(ConsolidatedOutcome::Abstained {
                reason: AbstentionReason::InsufficientHealthyVenues,
                lineage,
            });
        }

        let effective =
            capped_normalized_weights(&raw_weights, self.config.maximum_venue_weight.value())?;
        let price = weighted_median(
            candidates
                .iter()
                .zip(&effective)
                .map(|(candidate, weight)| {
                    (
                        candidate.price,
                        *weight,
                        canonical_quote_key(candidate.quote),
                    )
                })
                .collect(),
        )?;
        let lower = weighted_median(
            candidates
                .iter()
                .zip(&effective)
                .map(|(candidate, weight)| {
                    (
                        candidate.interval.lower(),
                        *weight,
                        canonical_quote_key(candidate.quote),
                    )
                })
                .collect(),
        )?;
        let upper = weighted_median(
            candidates
                .iter()
                .zip(&effective)
                .map(|(candidate, weight)| {
                    (
                        candidate.interval.upper(),
                        *weight,
                        canonical_quote_key(candidate.quote),
                    )
                })
                .collect(),
        )?;
        let included: Vec<_> = candidates
            .iter()
            .zip(raw_weights)
            .zip(effective)
            .map(|((candidate, raw_weight), weight)| IncludedVenue {
                quote: candidate.quote.clone(),
                adjusted_price: candidate.price,
                adjusted_interval: candidate.interval,
                adjusted_depth: candidate.depth_reference,
                raw_weight,
                effective_weight: Ppm(weight),
            })
            .collect();
        let lineage = build_lineage(
            &self.config,
            as_of,
            &included,
            excluded,
            book_admission_excluded,
        );
        let minimum_included_quality = included
            .iter()
            .map(|entry| entry.quote.quality)
            .min()
            .ok_or(ConsolidatedError::Arithmetic)?;
        let minimum_included_freshness = included
            .iter()
            .map(|entry| entry.quote.freshness)
            .min()
            .ok_or(ConsolidatedError::Arithmetic)?;
        Ok(ConsolidatedOutcome::Available(FairPrice {
            price,
            interval: PriceInterval::try_new(lower, upper)?,
            minimum_included_quality,
            minimum_included_freshness,
            lineage,
        }))
    }

    fn preliminary_exclusion(
        &self,
        as_of: UnixNanos,
        quote: &VenueQuote,
    ) -> Result<Option<VenueExclusionReason>, ConsolidatedError> {
        let reason = if quote.source_health != SourceHealthState::Healthy {
            Some(VenueExclusionReason::SourceUnhealthy)
        } else if quote.as_known_at > as_of || quote.event_time > as_of {
            Some(VenueExclusionReason::FutureObservation)
        } else if as_of
            .value()
            .checked_sub(quote.event_time.value())
            .ok_or(ConsolidatedError::Arithmetic)?
            > self.config.quote_ttl
        {
            Some(VenueExclusionReason::QuoteStale)
        } else if quote.metadata_status == MetadataStatus::Stale {
            Some(VenueExclusionReason::MetadataStale)
        } else if quote.metadata_status == MetadataStatus::Ambiguous {
            Some(VenueExclusionReason::MetadataAmbiguous)
        } else if self.config.require_catalog_provenance
            && !quote.instrument_provenance.is_catalog_bound()
        {
            Some(VenueExclusionReason::UnversionedInstrument)
        } else if quote.venue_status == VenueTradingState::Suspended {
            Some(VenueExclusionReason::VenueSuspended)
        } else if quote.venue_status == VenueTradingState::Auction {
            Some(VenueExclusionReason::VenueAuction)
        } else if quote.venue_status == VenueTradingState::Maintenance {
            Some(VenueExclusionReason::VenueMaintenance)
        } else if !quote.clock_healthy {
            Some(VenueExclusionReason::ClockUnhealthy)
        } else if !quote.parser_healthy {
            Some(VenueExclusionReason::ParserUnhealthy)
        } else if !quote.book_healthy {
            Some(VenueExclusionReason::BookDegraded)
        } else if quote.quality < self.config.minimum_quality {
            Some(VenueExclusionReason::QualityBelowMinimum)
        } else if quote.freshness < self.config.minimum_freshness {
            Some(VenueExclusionReason::FreshnessBelowMinimum)
        } else {
            None
        };
        Ok(reason)
    }

    fn is_configured_source(&self, source: &SourceId) -> bool {
        self.config
            .eligible_sources
            .as_deref()
            .is_none_or(|sources| sources.contains(source))
    }

    fn adjusted_candidate<'a>(
        &self,
        as_of: UnixNanos,
        quote: &'a VenueQuote,
    ) -> Result<Result<AdjustedCandidate<'a>, VenueExclusionReason>, ConsolidatedError> {
        if quote.instrument.quote_asset() == &self.config.reference_asset {
            if quote.conversion.is_some() {
                return Err(ConsolidatedError::InvalidQuote);
            }
            return Ok(Ok(AdjustedCandidate {
                quote,
                price: quote.midpoint,
                interval: PriceInterval::try_new(quote.midpoint, quote.midpoint)?,
                depth_reference: quote.executable_depth_quote,
            }));
        }
        let Some(reference) = quote.conversion.as_ref() else {
            return Ok(Err(VenueExclusionReason::ConversionUnavailable));
        };
        if reference.quote_asset() != quote.instrument.quote_asset()
            || reference.reference_asset() != &self.config.reference_asset
        {
            return Ok(Err(VenueExclusionReason::ConversionUnavailable));
        }
        if reference.as_known_at() > as_of {
            return Ok(Err(VenueExclusionReason::ConversionStale));
        }
        let reference_age = as_of
            .value()
            .checked_sub(reference.event_time().value())
            .ok_or(ConsolidatedError::Arithmetic)?;
        if reference_age > self.config.conversion_ttl {
            return Ok(Err(VenueExclusionReason::ConversionStale));
        }
        if reference.distinct_healthy_sources() < self.config.minimum_conversion_sources
            || reference.quality() < self.config.minimum_quality
            || reference.freshness() < self.config.minimum_freshness
        {
            return Ok(Err(VenueExclusionReason::ConversionInsufficientCoverage));
        }
        if matches!(
            reference.dislocation(),
            StablecoinDislocationState::LiquidityFailure
                | StablecoinDislocationState::RedemptionOrReserveEvent
                | StablecoinDislocationState::InsufficientEvidence
        ) {
            return Ok(Err(VenueExclusionReason::ConversionDislocated));
        }
        let interval_width = reference
            .interval()
            .upper()
            .value()
            .checked_sub(reference.interval().lower().value())?;
        if ratio_ppm(interval_width, reference.estimate().value())?
            > self.config.maximum_conversion_interval_width
        {
            return Ok(Err(VenueExclusionReason::ConversionUncertain));
        }
        let price = Price::new(
            quote
                .midpoint
                .value()
                .checked_mul(reference.estimate().value())?,
        )?;
        let lower = Price::new(
            quote
                .midpoint
                .value()
                .checked_mul(reference.interval().lower().value())?,
        )?;
        let upper = Price::new(
            quote
                .midpoint
                .value()
                .checked_mul(reference.interval().upper().value())?,
        )?;
        let depth_reference = Notional::new(
            quote
                .executable_depth_quote
                .value()
                .checked_mul(reference.estimate().value())?,
        )?;
        Ok(Ok(AdjustedCandidate {
            quote,
            price,
            interval: PriceInterval::try_new(lower, upper)?,
            depth_reference,
        }))
    }
}

#[derive(Clone, Debug)]
struct AdjustedCandidate<'a> {
    quote: &'a VenueQuote,
    price: Price,
    interval: PriceInterval,
    depth_reference: Notional,
}

type CanonicalQuoteKey<'a> = (&'a str, u8, &'a str, u32, u32);
type WeightedPrice<'a> = (Price, u32, CanonicalQuoteKey<'a>);

fn canonical_quote_key(quote: &VenueQuote) -> CanonicalQuoteKey<'_> {
    (
        quote.instrument.id().venue().as_str(),
        quote.instrument.id().product_type() as u8,
        quote.instrument.id().venue_symbol(),
        quote.instrument.id().generation(),
        quote.source.generation(),
    )
}

fn admission_key(excluded: &BookAdmissionExclusion) -> (&str, u8, &str, u32, u32) {
    (
        excluded.instrument.venue().as_str(),
        excluded.instrument.product_type() as u8,
        excluded.instrument.venue_symbol(),
        excluded.instrument.generation(),
        excluded.source.generation(),
    )
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

fn ratio_ppm(numerator: FixedDecimal, denominator: FixedDecimal) -> Result<Ppm, ConsolidatedError> {
    if numerator.is_negative() || denominator.is_zero() || denominator.is_negative() {
        return Err(ConsolidatedError::Arithmetic);
    }
    if numerator >= denominator {
        return Ok(Ppm(PPM_DENOMINATOR));
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
    Ok(Ppm(low))
}

pub(crate) fn compare_nonnegative_ratio(
    numerator: FixedDecimal,
    denominator: FixedDecimal,
    rhs_numerator: u32,
    rhs_denominator: u32,
) -> Result<Ordering, ConsolidatedError> {
    if numerator.is_negative()
        || denominator.is_zero()
        || denominator.is_negative()
        || rhs_denominator == 0
    {
        return Err(ConsolidatedError::Arithmetic);
    }
    let left = BigInt::from(numerator.mantissa())
        * BigInt::from(rhs_denominator)
        * BigInt::from(10_u8).pow(denominator.scale());
    let right = BigInt::from(denominator.mantissa())
        * BigInt::from(rhs_numerator)
        * BigInt::from(10_u8).pow(numerator.scale());
    Ok(left.cmp(&right))
}

pub(crate) fn capped_normalized_weights(
    raw: &[u64],
    cap: u32,
) -> Result<Vec<u32>, ConsolidatedError> {
    if raw.is_empty()
        || raw.contains(&0)
        || raw.len() > PPM_DENOMINATOR as usize
        || (raw.len() as u64)
            .checked_mul(u64::from(cap))
            .is_none_or(|capacity| capacity < u64::from(PPM_DENOMINATOR))
    {
        return Err(ConsolidatedError::InvalidConfig);
    }
    let positive_floor = 1_u32;
    let residual_cap = cap
        .checked_sub(positive_floor)
        .ok_or(ConsolidatedError::InvalidConfig)?;
    let mut assigned = vec![0_u32; raw.len()];
    let mut active: Vec<usize> = (0..raw.len()).collect();
    let mut remaining = PPM_DENOMINATOR
        .checked_sub(u32::try_from(raw.len()).map_err(|_| ConsolidatedError::Arithmetic)?)
        .ok_or(ConsolidatedError::InvalidConfig)?;
    loop {
        let total: u128 = active.iter().map(|index| u128::from(raw[*index])).sum();
        if total == 0 {
            return Err(ConsolidatedError::Arithmetic);
        }
        let mut capped = Vec::new();
        for index in &active {
            let allocation = u128::from(remaining) * u128::from(raw[*index]) / total;
            if allocation > u128::from(residual_cap) {
                capped.push(*index);
            }
        }
        if capped.is_empty() {
            let mut remainders = Vec::with_capacity(active.len());
            let mut used = 0_u32;
            for index in &active {
                let numerator = u128::from(remaining) * u128::from(raw[*index]);
                let allocation =
                    u32::try_from(numerator / total).map_err(|_| ConsolidatedError::Arithmetic)?;
                assigned[*index] = allocation;
                used = used
                    .checked_add(allocation)
                    .ok_or(ConsolidatedError::Arithmetic)?;
                remainders.push((*index, numerator % total));
            }
            let mut residual = remaining
                .checked_sub(used)
                .ok_or(ConsolidatedError::Arithmetic)?;
            remainders
                .sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
            for (index, _) in remainders {
                if residual == 0 {
                    break;
                }
                if assigned[index] < residual_cap {
                    assigned[index] += 1;
                    residual -= 1;
                }
            }
            if residual != 0 {
                return Err(ConsolidatedError::InvalidConfig);
            }
            break;
        }
        capped.sort_unstable();
        for index in &capped {
            assigned[*index] = residual_cap;
            remaining = remaining
                .checked_sub(residual_cap)
                .ok_or(ConsolidatedError::InvalidConfig)?;
        }
        active.retain(|index| !capped.contains(index));
        if active.is_empty() {
            if remaining != 0 {
                return Err(ConsolidatedError::InvalidConfig);
            }
            break;
        }
    }
    for weight in &mut assigned {
        *weight = weight
            .checked_add(positive_floor)
            .ok_or(ConsolidatedError::Arithmetic)?;
    }
    if assigned.iter().copied().map(u64::from).sum::<u64>() != u64::from(PPM_DENOMINATOR)
        || assigned.iter().any(|weight| *weight > cap)
    {
        return Err(ConsolidatedError::Arithmetic);
    }
    Ok(assigned)
}

fn weighted_median(mut values: Vec<WeightedPrice<'_>>) -> Result<Price, ConsolidatedError> {
    values.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.2.cmp(&right.2)));
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

fn excluded_venue(quote: &VenueQuote, reason: VenueExclusionReason) -> ExcludedVenue {
    ExcludedVenue {
        quote: quote.clone(),
        reason,
    }
}

fn exclude_remaining_candidates(
    excluded: &mut Vec<ExcludedVenue>,
    candidates: Vec<AdjustedCandidate<'_>>,
) {
    excluded.extend(candidates.into_iter().map(|candidate| {
        excluded_venue(
            candidate.quote,
            VenueExclusionReason::InsufficientConsolidatedCoverage,
        )
    }));
}

fn build_lineage(
    config: &EstimatorConfig,
    as_of: UnixNanos,
    included: &[IncludedVenue],
    mut excluded: Vec<ExcludedVenue>,
    mut book_admission_excluded: Vec<BookAdmissionExclusion>,
) -> ConsolidatedLineage {
    let mut included = included.to_vec();
    included.sort_by(|left, right| {
        canonical_quote_key(&left.quote).cmp(&canonical_quote_key(&right.quote))
    });
    excluded.sort_by(|left, right| {
        canonical_quote_key(&left.quote).cmp(&canonical_quote_key(&right.quote))
    });
    book_admission_excluded.sort_by(|left, right| admission_key(left).cmp(&admission_key(right)));
    let mut hasher = blake3::Hasher::new();
    hasher.update(LINEAGE_DOMAIN);
    hash_config(&mut hasher, config);
    hasher.update(&as_of.value().to_be_bytes());
    for entry in &included {
        hasher.update(&[1]);
        hash_quote(&mut hasher, &entry.quote);
        hash_decimal(&mut hasher, entry.adjusted_price.value());
        hash_decimal(&mut hasher, entry.adjusted_interval.lower().value());
        hash_decimal(&mut hasher, entry.adjusted_interval.upper().value());
        hash_decimal(&mut hasher, entry.adjusted_depth.value());
        hasher.update(&entry.raw_weight.to_be_bytes());
        hasher.update(&entry.effective_weight.value().to_be_bytes());
    }
    for entry in &excluded {
        hasher.update(&[2]);
        hash_quote(&mut hasher, &entry.quote);
        hasher.update(&[entry.reason as u8]);
    }
    for entry in &book_admission_excluded {
        hasher.update(&[3]);
        hash_source(&mut hasher, &entry.source);
        hash_instrument_id(&mut hasher, &entry.instrument);
        hasher.update(&[book_state_tag(entry.state)]);
        hasher.update(&[book_error_tag(&entry.error)]);
        hasher.update(&entry.event_time.value().to_be_bytes());
        hasher.update(&entry.as_known_at.value().to_be_bytes());
    }
    ConsolidatedLineage {
        policy_id: config.policy_id.clone(),
        as_of,
        eligible_sources: config.eligible_sources.clone(),
        included,
        excluded,
        book_admission_excluded,
        digest: *hasher.finalize().as_bytes(),
    }
}

fn hash_config(hasher: &mut blake3::Hasher, config: &EstimatorConfig) {
    hash_bytes(hasher, config.policy_id.as_str().as_bytes());
    hash_asset(hasher, &config.base_asset);
    hash_asset(hasher, &config.reference_asset);
    hasher.update(&[config.product_type as u8]);
    hasher.update(&(config.minimum_venues as u64).to_be_bytes());
    hasher.update(&(config.maximum_venues as u64).to_be_bytes());
    hasher.update(&config.quote_ttl.to_be_bytes());
    hasher.update(&config.conversion_ttl.to_be_bytes());
    hash_decimal(hasher, config.depth_cap_reference.value());
    hasher.update(&config.minimum_quality.value().to_be_bytes());
    hasher.update(&config.minimum_freshness.value().to_be_bytes());
    hasher.update(&config.maximum_venue_weight.value().to_be_bytes());
    hasher.update(&config.outlier_threshold.value().to_be_bytes());
    hasher.update(&config.minimum_conversion_sources.to_be_bytes());
    hasher.update(
        &config
            .maximum_conversion_interval_width
            .value()
            .to_be_bytes(),
    );
    hasher.update(&[u8::from(config.require_catalog_provenance)]);
    match &config.eligible_sources {
        Some(sources) => {
            hasher.update(&[1]);
            hasher.update(&(sources.len() as u64).to_be_bytes());
            for source in sources {
                hash_source(hasher, source);
            }
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn compare_source(left: &SourceId, right: &SourceId) -> std::cmp::Ordering {
    (left.kind() as u8)
        .cmp(&(right.kind() as u8))
        .then_with(|| left.name().cmp(right.name()))
        .then_with(|| left.generation().cmp(&right.generation()))
}

fn hash_quote(hasher: &mut blake3::Hasher, quote: &VenueQuote) {
    hash_source(hasher, &quote.source);
    hash_instrument(hasher, &quote.instrument);
    hasher.update(&[quote.candidate_kind as u8]);
    hash_decimal(hasher, quote.bid.value());
    hash_decimal(hasher, quote.ask.value());
    hash_decimal(hasher, quote.midpoint.value());
    hash_decimal(hasher, quote.executable_depth_quote.value());
    hasher.update(&quote.quality.value().to_be_bytes());
    hasher.update(&quote.freshness.value().to_be_bytes());
    hash_bytes(hasher, quote.source_health.as_str().as_bytes());
    hasher.update(&[quote.metadata_status as u8]);
    hasher.update(&[quote.venue_status as u8]);
    hasher.update(&[u8::from(quote.clock_healthy)]);
    hasher.update(&[u8::from(quote.parser_healthy)]);
    hasher.update(&[u8::from(quote.book_healthy)]);
    hasher.update(&quote.event_time.value().to_be_bytes());
    hasher.update(&quote.as_known_at.value().to_be_bytes());
    hash_conversion(hasher, quote.conversion.as_ref());
    hash_instrument_provenance(hasher, &quote.instrument_provenance);
    hash_book_provenance(hasher, quote.book_provenance.as_ref());
}

fn hash_instrument(hasher: &mut blake3::Hasher, instrument: &InstrumentDefinition) {
    let id = instrument.id();
    hash_instrument_id(hasher, id);
    hash_asset(hasher, instrument.base_asset());
    hash_asset(hasher, instrument.quote_asset());
    hash_asset(hasher, instrument.settlement_asset());
    hash_decimal(hasher, instrument.contract_multiplier());
    hasher.update(&[instrument.contract_value_unit() as u8]);
    hasher.update(&[instrument.contract_kind() as u8]);
    hash_optional_time(hasher, instrument.expiry_time());
    hash_optional_price(hasher, instrument.strike());
    match instrument.option_side() {
        Some(side) => hasher.update(&[1, side as u8]),
        None => hasher.update(&[0]),
    };
    hash_decimal(hasher, instrument.price_tick().value());
    hash_decimal(hasher, instrument.quantity_step().value());
    hasher.update(&instrument.listing_time().value().to_be_bytes());
    hash_optional_time(hasher, instrument.delisting_time());
}

fn hash_instrument_id(hasher: &mut blake3::Hasher, id: &InstrumentId) {
    hash_bytes(hasher, id.venue().as_str().as_bytes());
    hash_bytes(hasher, id.venue_symbol().as_bytes());
    hasher.update(&[id.product_type() as u8]);
    hasher.update(&id.generation().to_be_bytes());
}

fn hash_asset(hasher: &mut blake3::Hasher, asset: &AssetId) {
    hasher.update(&[asset.namespace() as u8]);
    hash_bytes(hasher, asset.chain_id().as_bytes());
    hash_bytes(hasher, asset.contract_or_mint().as_bytes());
    hash_bytes(hasher, asset.canonical_symbol().as_bytes());
    hasher.update(&asset.generation().to_be_bytes());
}

fn hash_conversion(hasher: &mut blake3::Hasher, conversion: Option<&QuoteConversionReference>) {
    let Some(conversion) = conversion else {
        hasher.update(&[0]);
        return;
    };
    hasher.update(&[1]);
    hash_bytes(hasher, conversion.policy_id().as_str().as_bytes());
    hash_asset(hasher, conversion.quote_asset());
    hash_asset(hasher, conversion.reference_asset());
    hash_decimal(hasher, conversion.estimate().value());
    hash_decimal(hasher, conversion.interval().lower().value());
    hash_decimal(hasher, conversion.interval().upper().value());
    hasher.update(&conversion.distinct_healthy_sources().to_be_bytes());
    for source in conversion.source_ids() {
        hash_source(hasher, source);
    }
    for venue in conversion.venue_ids() {
        hash_bytes(hasher, venue.as_str().as_bytes());
    }
    hasher.update(&conversion.quality().value().to_be_bytes());
    hasher.update(&conversion.freshness().value().to_be_bytes());
    hasher.update(&conversion.event_time().value().to_be_bytes());
    hasher.update(&conversion.as_known_at().value().to_be_bytes());
    hasher.update(&[conversion.dislocation() as u8]);
    hash_decimal(hasher, conversion.executable_depth_reference().value());
    hasher.update(conversion.evidence_digest());
}

fn hash_instrument_provenance(hasher: &mut blake3::Hasher, provenance: &InstrumentProvenance) {
    match (
        provenance.definition_revision,
        provenance.catalog_revision,
        provenance.catalog_as_known_at,
        provenance.catalog_digest,
    ) {
        (None, None, None, None) => {
            hasher.update(&[0]);
        }
        (
            Some(definition_revision),
            Some(catalog_revision),
            Some(catalog_as_known_at),
            Some(catalog_digest),
        ) => {
            hasher.update(&[1]);
            hasher.update(&definition_revision.get().to_be_bytes());
            hasher.update(&catalog_revision.get().to_be_bytes());
            hasher.update(&catalog_as_known_at.value().to_be_bytes());
            hasher.update(&catalog_digest);
        }
        _ => {
            unreachable!("instrument provenance is constructed atomically");
        }
    }
}

fn hash_book_provenance(hasher: &mut blake3::Hasher, provenance: Option<&BookProvenance>) {
    let Some(provenance) = provenance else {
        hasher.update(&[0]);
        return;
    };
    hasher.update(&[1]);
    hasher.update(&provenance.session.connection_epoch.to_be_bytes());
    hasher.update(&provenance.session.subscription_epoch.to_be_bytes());
    hasher.update(&provenance.session.instrument_generation.to_be_bytes());
    hasher.update(&provenance.source_sequence.to_be_bytes());
    hasher.update(&[checksum_status_tag(provenance.checksum_status)]);
    hasher.update(&[book_classification_tag(provenance.classification)]);
    hasher.update(&provenance.updated_monotonic_ns.to_be_bytes());
    hasher.update(&provenance.age_ms.to_be_bytes());
    hasher.update(&(provenance.depth_levels as u64).to_be_bytes());
}

const fn checksum_status_tag(status: ChecksumStatus) -> u8 {
    match status {
        ChecksumStatus::NotSupported => 1,
        ChecksumStatus::Pending => 2,
        ChecksumStatus::Valid => 3,
        ChecksumStatus::Mismatch => 4,
    }
}

const fn book_classification_tag(classification: BookClassification) -> u8 {
    match classification {
        BookClassification::Normal => 1,
        BookClassification::Locked => 2,
        BookClassification::Crossed => 3,
        BookClassification::OneSided => 4,
    }
}

const fn book_state_tag(state: BookState) -> u8 {
    match state {
        BookState::Disconnected => 1,
        BookState::Buffering => 2,
        BookState::AwaitingSnapshot => 3,
        BookState::Replaying => 4,
        BookState::Synchronized => 5,
        BookState::Untrusted => 6,
    }
}

const fn book_error_tag(error: &BookError) -> u8 {
    match error {
        BookError::InvalidConfig => 1,
        BookError::Untrusted => 2,
        BookError::InvalidEpoch => 3,
        BookError::StaleInstrumentGeneration => 4,
        BookError::WrongInstrument => 5,
        BookError::InvalidSequence => 6,
        BookError::SequenceOverflow => 7,
        BookError::BufferCapacity => 8,
        BookError::Capacity => 9,
        BookError::MisalignedLevel => 10,
        BookError::AmbiguousDelta => 11,
        BookError::InvalidBook => 12,
        BookError::ChecksumRequired => 13,
        BookError::InvalidChecksumInput => 14,
        BookError::InvalidEvent => 15,
        BookError::ChecksumSourceUnavailable => 16,
        BookError::MonotonicTimeRegression => 17,
        BookError::L3Disabled => 18,
        BookError::L3SnapshotRequired => 19,
        BookError::InvalidL3Order => 20,
        BookError::AmbiguousL3Order => 21,
        BookError::UnknownL3Order => 22,
        BookError::SequenceGap => 23,
        BookError::ChecksumMismatch => 24,
        BookError::SnapshotRequired => 25,
        BookError::InvalidTransition => 26,
    }
}

fn hash_optional_time(hasher: &mut blake3::Hasher, time: Option<UnixNanos>) {
    match time {
        Some(time) => {
            hasher.update(&[1]);
            hasher.update(&time.value().to_be_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn hash_optional_price(hasher: &mut blake3::Hasher, price: Option<Price>) {
    match price {
        Some(price) => {
            hasher.update(&[1]);
            hash_decimal(hasher, price.value());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn hash_decimal(hasher: &mut blake3::Hasher, value: FixedDecimal) {
    hasher.update(&value.mantissa().to_be_bytes());
    hasher.update(&value.scale().to_be_bytes());
}

fn hash_source(hasher: &mut blake3::Hasher, source: &SourceId) {
    hasher.update(&[source.kind() as u8]);
    hash_bytes(hasher, source.name().as_bytes());
    hasher.update(&source.generation().to_be_bytes());
}

fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value);
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ConsolidatedError {
    #[error("invalid policy identifier")]
    InvalidPolicyId,
    #[error("parts per million must be in inclusive range 0..=1_000_000")]
    InvalidPpm,
    #[error("invalid estimator configuration")]
    InvalidConfig,
    #[error("invalid venue quote")]
    InvalidQuote,
    #[error("invalid quote conversion reference")]
    InvalidConversionReference,
    #[error("invalid stablecoin venue quote")]
    InvalidStablecoinQuote,
    #[error("invalid or oversized input set")]
    InvalidInputSet,
    #[error("duplicate source or venue identity")]
    DuplicateVenueIdentity,
    #[error("trusted order-book snapshot failed validation")]
    UntrustedBook,
    #[error("instrument could not be resolved from the point-in-time catalog")]
    InstrumentResolution,
    #[error("instrument catalog integrity verification failed")]
    CatalogIntegrity,
    #[error("consolidated arithmetic failed")]
    Arithmetic,
    #[error("fixed-decimal failure: {0}")]
    Decimal(#[from] DecimalError),
}
