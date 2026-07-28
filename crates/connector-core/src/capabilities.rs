//! Validated, venue-specific connector capability declarations.

use std::{
    collections::BTreeSet,
    num::{NonZeroU32, NonZeroU64},
};

use domain::VenueId;
use serde::ser::SerializeMap as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_ID_LEN: usize = 96;
const MAX_VERSION_LEN: usize = 64;
const MAX_BOOK_DEPTHS: usize = 16;
const MAX_RATE_LIMIT_RULES: usize = 16;
const MAX_KNOWN_LIMITATIONS: usize = 16;
const MAX_LIMITATION_LEN: usize = 256;

/// Market forms declared by a connector.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketType {
    Spot,
    LinearPerpetual,
    InversePerpetual,
    Future,
    Option,
}

/// Whether the venue publishes individual, aggregate, or both trade forms.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeSemantics {
    Individual,
    Aggregate,
    AggregateAndIndividual,
}

/// Venue sequence semantics used by the local integrity state machine.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SequenceSemantics {
    None,
    MonotonicUpdateId,
    InclusiveRange,
    InclusiveRangeWithPreviousFinal,
    PreviousAndCurrent,
    SnapshotResettingUpdateId {
        reset_update_id: NonZeroU64,
        overwrite_on_every_snapshot: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct BookSequenceRule {
    market_type: MarketType,
    semantics: SequenceSemantics,
}

impl BookSequenceRule {
    pub const fn new(market_type: MarketType, semantics: SequenceSemantics) -> Self {
        Self {
            market_type,
            semantics,
        }
    }

    pub const fn market_type(self) -> MarketType {
        self.market_type
    }

    pub const fn semantics(self) -> SequenceSemantics {
        self.semantics
    }
}

/// Checksum algorithm plus its normative canonicalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumSupport {
    NotSupported,
    Crc32DecimalBook,
    Crc32cCanonicalBytes,
}

/// Fixed set of stream classes whose completeness must be declared explicitly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum StreamClass {
    InstrumentDefinitions = 0,
    Trades = 1,
    TopOfBook = 2,
    BookSnapshots = 3,
    BookDeltas = 4,
    MarkAndIndex = 5,
    Funding = 6,
    OpenInterest = 7,
    Liquidations = 8,
    OptionMetadata = 9,
    OptionTicker = 10,
    OptionTrades = 11,
    OptionBooks = 12,
    VenueStatus = 13,
}

impl StreamClass {
    pub const ALL: [Self; 14] = [
        Self::InstrumentDefinitions,
        Self::Trades,
        Self::TopOfBook,
        Self::BookSnapshots,
        Self::BookDeltas,
        Self::MarkAndIndex,
        Self::Funding,
        Self::OpenInterest,
        Self::Liquidations,
        Self::OptionMetadata,
        Self::OptionTicker,
        Self::OptionTrades,
        Self::OptionBooks,
        Self::VenueStatus,
    ];
    const COUNT: usize = Self::ALL.len();

    const fn index(self) -> usize {
        self as usize
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::InstrumentDefinitions => "instrument_definitions",
            Self::Trades => "trades",
            Self::TopOfBook => "top_of_book",
            Self::BookSnapshots => "book_snapshots",
            Self::BookDeltas => "book_deltas",
            Self::MarkAndIndex => "mark_and_index",
            Self::Funding => "funding",
            Self::OpenInterest => "open_interest",
            Self::Liquidations => "liquidations",
            Self::OptionMetadata => "option_metadata",
            Self::OptionTicker => "option_ticker",
            Self::OptionTrades => "option_trades",
            Self::OptionBooks => "option_books",
            Self::VenueStatus => "venue_status",
        }
    }
}

/// Truthful delivery completeness for one source stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    NotSupported,
    VenueReportedComplete {
        delivery_uncertainty: bool,
    },
    VenueReportedAll {
        push_cadence_ms: NonZeroU32,
        delivery_uncertainty: bool,
    },
    SampledLatestPerSymbolWindow {
        window_ms: NonZeroU32,
    },
    Partial {
        reason: CompletenessReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletenessReason {
    VenueSampling,
    SubscriptionScope,
    LicensingRestriction,
    HistoricalGap,
}

/// Fixed-size completeness declarations for every source stream class.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletenessProfile([Completeness; StreamClass::COUNT]);

impl CompletenessProfile {
    pub const fn new(values: [Completeness; StreamClass::COUNT]) -> Self {
        Self(values)
    }

    pub const fn get(&self, stream: StreamClass) -> Completeness {
        self.0[stream.index()]
    }

    pub fn with(mut self, stream: StreamClass, completeness: Completeness) -> Self {
        self.0[stream.index()] = completeness;
        self
    }

    /// Initial Binance contract, preserving sampled liquidation semantics.
    pub fn binance() -> Self {
        let mut values = [Completeness::NotSupported; StreamClass::COUNT];
        values[StreamClass::InstrumentDefinitions.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::Trades.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::TopOfBook.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::BookSnapshots.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::BookDeltas.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::MarkAndIndex.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::Funding.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::OpenInterest.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::Liquidations.index()] = Completeness::SampledLatestPerSymbolWindow {
            window_ms: NonZeroU32::new(1_000).expect("1000 is nonzero"),
        };
        values[StreamClass::VenueStatus.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        Self(values)
    }

    /// Initial Bybit contract, retaining push cadence and delivery uncertainty.
    pub fn bybit() -> Self {
        let mut values = [Completeness::NotSupported; StreamClass::COUNT];
        values[StreamClass::InstrumentDefinitions.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::Trades.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::TopOfBook.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::BookSnapshots.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::BookDeltas.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::MarkAndIndex.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::Funding.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::OpenInterest.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        values[StreamClass::Liquidations.index()] = Completeness::VenueReportedAll {
            push_cadence_ms: NonZeroU32::new(500).expect("500 is nonzero"),
            delivery_uncertainty: true,
        };
        values[StreamClass::VenueStatus.index()] = Completeness::VenueReportedComplete {
            delivery_uncertainty: true,
        };
        Self(values)
    }
}

impl Serialize for CompletenessProfile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(StreamClass::COUNT))?;
        for stream in StreamClass::ALL {
            map.serialize_entry(stream.as_str(), &self.get(stream))?;
        }
        map.end()
    }
}

macro_rules! field_set {
    ($name:ident, $known:expr) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(u32);

        impl $name {
            pub const NONE: Self = Self(0);

            pub const fn from_bits(bits: u32) -> Result<Self, CapabilityError> {
                if bits & !$known == 0 {
                    Ok(Self(bits))
                } else {
                    Err(CapabilityError::UnknownFieldBit)
                }
            }

            pub const fn bits(self) -> u32 {
                self.0
            }
        }
    };
}

field_set!(FundingFields, 0b0000_0111_u32);
impl FundingFields {
    pub const RATE: Self = Self(0b0000_0001);
    pub const NEXT_FUNDING_TIME: Self = Self(0b0000_0010);
    pub const INTERVAL: Self = Self(0b0000_0100);
}

field_set!(OpenInterestFields, 0b0000_0011_u32);
impl OpenInterestFields {
    pub const VALUE: Self = Self(0b0000_0001);
    pub const NOTIONAL: Self = Self(0b0000_0010);
}

field_set!(OptionsFields, 0b0001_1111_1111_u32);
impl OptionsFields {
    pub const IMPLIED_VOLATILITY: Self = Self(0b0000_0001);
    pub const DELTA: Self = Self(0b0000_0010);
    pub const GAMMA: Self = Self(0b0000_0100);
    pub const VEGA: Self = Self(0b0000_1000);
    pub const THETA: Self = Self(0b0001_0000);
    pub const OPEN_INTEREST: Self = Self(0b0010_0000);
    pub const MARK_PRICE: Self = Self(0b0100_0000);
    pub const INDEX_PRICE: Self = Self(0b1000_0000);
    pub const FUNDING_RATE: Self = Self(0b0001_0000_0000);
}

/// One bounded rate-limit rule.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitTransport {
    Rest,
    WebSocketControl,
    WebSocketConnection,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    Ip,
    Uid,
    Connection,
    VenueDomain,
    MarketCategory,
    IpAndVenueDomain,
    IpAndMarketCategory,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitMetric {
    Requests,
    RequestWeight,
    Orders,
    ControlMessages,
    ConnectionAttempts,
    ConcurrentConnections,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RateLimitAlgorithm {
    FixedWindow { interval_ms: NonZeroU32 },
    RollingWindow { interval_ms: NonZeroU32 },
    ConcurrentGauge,
    VenueAdvertisedDynamic,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum RateLimitValue {
    Fixed(NonZeroU32),
    VenueAdvertised,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitSource {
    StaticDocumentation,
    VenueMetadataAndHeaders,
    ResponseHeaders,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitExceededAction {
    RejectAndBackoff,
    RejectAndBackoffAtLeast { minimum_ms: NonZeroU32 },
    RetryAtResetHeader,
    RetryAfterHeaderThenTemporaryBan,
    Disconnect,
    DisconnectThenBanOnRepeatedViolation,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RateLimitRuleInput {
    pub transport: RateLimitTransport,
    pub scope: RateLimitScope,
    pub metric: RateLimitMetric,
    pub applies_to: String,
    pub market_types: Vec<MarketType>,
    pub algorithm: RateLimitAlgorithm,
    pub limit: RateLimitValue,
    pub request_cost: Option<NonZeroU32>,
    pub source: RateLimitSource,
    pub on_exceeded: RateLimitExceededAction,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RateLimitRule {
    input: RateLimitRuleInput,
}

impl RateLimitRule {
    pub fn try_new(input: RateLimitRuleInput) -> Result<Self, CapabilityError> {
        if !valid_identifier(&input.applies_to, MAX_ID_LEN)
            || input.market_types.is_empty()
            || input.market_types.len() > MarketType::ALL_MAX
            || input
                .market_types
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .len()
                != input.market_types.len()
            || (matches!(input.algorithm, RateLimitAlgorithm::VenueAdvertisedDynamic)
                != matches!(input.limit, RateLimitValue::VenueAdvertised))
            || ((input.algorithm == RateLimitAlgorithm::ConcurrentGauge)
                != (input.metric == RateLimitMetric::ConcurrentConnections))
            || (matches!(
                input.algorithm,
                RateLimitAlgorithm::FixedWindow { .. } | RateLimitAlgorithm::RollingWindow { .. }
            ) && input.request_cost.is_none())
            || (matches!(
                input.algorithm,
                RateLimitAlgorithm::ConcurrentGauge | RateLimitAlgorithm::VenueAdvertisedDynamic
            ) && input.request_cost.is_some())
            || (input.algorithm == RateLimitAlgorithm::VenueAdvertisedDynamic
                && input.source != RateLimitSource::VenueMetadataAndHeaders)
        {
            return Err(CapabilityError::InvalidRateLimitRule);
        }
        Ok(Self { input })
    }

    pub const fn transport(&self) -> RateLimitTransport {
        self.input.transport
    }

    pub const fn scope(&self) -> RateLimitScope {
        self.input.scope
    }

    pub const fn metric(&self) -> RateLimitMetric {
        self.input.metric
    }

    pub fn applies_to(&self) -> &str {
        &self.input.applies_to
    }

    pub fn market_types(&self) -> &[MarketType] {
        &self.input.market_types
    }

    pub const fn algorithm(&self) -> RateLimitAlgorithm {
        self.input.algorithm
    }

    pub const fn limit(&self) -> RateLimitValue {
        self.input.limit
    }

    pub const fn request_cost(&self) -> Option<NonZeroU32> {
        self.input.request_cost
    }

    pub const fn source(&self) -> RateLimitSource {
        self.input.source
    }

    pub const fn on_exceeded(&self) -> RateLimitExceededAction {
        self.input.on_exceeded
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotMethod {
    NotApplicable,
    WebSocketSnapshot,
    RestThenBufferedDeltas,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryMethod {
    Reconnect,
    ReconnectAndResnapshot,
    RenewSubscription,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConnectionLifetime {
    Finite {
        lifetime_ms: NonZeroU32,
        renewal_margin_ms: NonZeroU32,
    },
    Unlimited,
    VenueUnspecified,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceTimestampPrecision {
    Seconds,
    Milliseconds,
    Microseconds,
    Nanoseconds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedistributionClass {
    ReviewedPermitted,
    ReviewedRestricted,
    InternalOnly,
}

/// Complete constructor input. Adding a mandatory spec field is a compile-time
/// change for every connector preset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConnectorCapabilitiesInput {
    pub venue: VenueId,
    pub connector_version: String,
    pub market_types: Vec<MarketType>,
    pub trade_semantics: TradeSemantics,
    pub book_depths: Vec<u32>,
    pub book_sequence_semantics: Vec<BookSequenceRule>,
    pub checksum_support: ChecksumSupport,
    pub stream_completeness: CompletenessProfile,
    pub liquidation_completeness: Completeness,
    pub funding_fields: FundingFields,
    pub open_interest_fields: OpenInterestFields,
    pub options_fields: OptionsFields,
    pub connection_lifetime: ConnectionLifetime,
    pub rate_limit_model: Vec<RateLimitRule>,
    pub snapshot_method: SnapshotMethod,
    pub recovery_method: RecoveryMethod,
    pub source_timestamp_precision: SourceTimestampPrecision,
    pub known_limitations: Vec<String>,
    pub terms_reference: String,
    pub redistribution_class: RedistributionClass,
}

/// Validated source capability record required by production specification 9.2.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ConnectorCapabilities {
    input: ConnectorCapabilitiesInput,
}

impl ConnectorCapabilities {
    pub fn try_new(mut input: ConnectorCapabilitiesInput) -> Result<Self, CapabilityError> {
        if !valid_identifier(&input.connector_version, MAX_VERSION_LEN) {
            return Err(CapabilityError::InvalidConnectorVersion);
        }
        validate_unique_bounded(&input.market_types, 1, MarketType::ALL_MAX)?;
        validate_depths(&mut input.book_depths)?;
        validate_connection_lifetime(input.connection_lifetime)?;
        if input.rate_limit_model.is_empty() || input.rate_limit_model.len() > MAX_RATE_LIMIT_RULES
        {
            return Err(CapabilityError::InvalidRateLimitModel);
        }
        if input.rate_limit_model.iter().collect::<BTreeSet<_>>().len()
            != input.rate_limit_model.len()
            || input.rate_limit_model.iter().any(|rule| {
                rule.market_types()
                    .iter()
                    .any(|market| !input.market_types.contains(market))
            })
        {
            return Err(CapabilityError::InvalidRateLimitModel);
        }
        if input.known_limitations.len() > MAX_KNOWN_LIMITATIONS
            || input
                .known_limitations
                .iter()
                .any(|value| !valid_text(value, MAX_LIMITATION_LEN))
            || input
                .known_limitations
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != input.known_limitations.len()
        {
            return Err(CapabilityError::InvalidKnownLimitation);
        }
        if !valid_https_reference(&input.terms_reference) {
            return Err(CapabilityError::InvalidTermsReference);
        }
        validate_consistency(&input)?;
        Ok(Self { input })
    }

    pub fn venue(&self) -> &VenueId {
        &self.input.venue
    }

    pub fn connector_version(&self) -> &str {
        &self.input.connector_version
    }

    pub fn market_types(&self) -> &[MarketType] {
        &self.input.market_types
    }

    pub const fn trade_semantics(&self) -> TradeSemantics {
        self.input.trade_semantics
    }

    pub fn book_depths(&self) -> &[u32] {
        &self.input.book_depths
    }

    pub fn book_sequence_semantics(&self) -> &[BookSequenceRule] {
        &self.input.book_sequence_semantics
    }

    pub const fn checksum_support(&self) -> ChecksumSupport {
        self.input.checksum_support
    }

    pub const fn completeness(&self) -> &CompletenessProfile {
        &self.input.stream_completeness
    }

    pub const fn liquidation_completeness(&self) -> Completeness {
        self.input.liquidation_completeness
    }

    pub const fn funding_fields(&self) -> FundingFields {
        self.input.funding_fields
    }

    pub const fn open_interest_fields(&self) -> OpenInterestFields {
        self.input.open_interest_fields
    }

    pub const fn options_fields(&self) -> OptionsFields {
        self.input.options_fields
    }

    pub const fn connection_lifetime(&self) -> ConnectionLifetime {
        self.input.connection_lifetime
    }

    pub fn rate_limit_model(&self) -> &[RateLimitRule] {
        &self.input.rate_limit_model
    }

    pub const fn snapshot_method(&self) -> SnapshotMethod {
        self.input.snapshot_method
    }

    pub const fn recovery_method(&self) -> RecoveryMethod {
        self.input.recovery_method
    }

    pub const fn source_timestamp_precision(&self) -> SourceTimestampPrecision {
        self.input.source_timestamp_precision
    }

    pub fn known_limitations(&self) -> &[String] {
        &self.input.known_limitations
    }

    pub fn terms_reference(&self) -> &str {
        &self.input.terms_reference
    }

    pub const fn redistribution_class(&self) -> RedistributionClass {
        self.input.redistribution_class
    }
}

impl MarketType {
    const ALL_MAX: usize = 5;
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CapabilityError {
    #[error("invalid venue identifier")]
    InvalidVenue,
    #[error("invalid connector version")]
    InvalidConnectorVersion,
    #[error("market type declaration is missing, duplicated, or too large")]
    InvalidMarketTypes,
    #[error("book depth declaration is missing, duplicated, or too large")]
    InvalidBookDepths,
    #[error("connection renewal margin must be positive and below the lifetime")]
    InvalidConnectionRenewal,
    #[error("rate limit model is missing or too large")]
    InvalidRateLimitModel,
    #[error("invalid rate limit rule")]
    InvalidRateLimitRule,
    #[error("known limitation is invalid or exceeds its bound")]
    InvalidKnownLimitation,
    #[error("terms reference must be a bounded HTTPS URL")]
    InvalidTermsReference,
    #[error("field bitset contains an unknown bit")]
    UnknownFieldBit,
    #[error("capability fields contradict one another")]
    ContradictoryCapability,
}

fn validate_connection_lifetime(lifetime: ConnectionLifetime) -> Result<(), CapabilityError> {
    if let ConnectionLifetime::Finite {
        lifetime_ms,
        renewal_margin_ms,
    } = lifetime
        && renewal_margin_ms >= lifetime_ms
    {
        return Err(CapabilityError::InvalidConnectionRenewal);
    }
    Ok(())
}

fn validate_consistency(input: &ConnectorCapabilitiesInput) -> Result<(), CapabilityError> {
    let completeness = &input.stream_completeness;
    if input.liquidation_completeness != completeness.get(StreamClass::Liquidations) {
        return Err(CapabilityError::ContradictoryCapability);
    }
    let supports_books = completeness.get(StreamClass::BookSnapshots) != Completeness::NotSupported
        || completeness.get(StreamClass::BookDeltas) != Completeness::NotSupported;
    if supports_books
        && (input.snapshot_method == SnapshotMethod::NotApplicable
            || input.recovery_method != RecoveryMethod::ReconnectAndResnapshot)
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    let sequence_markets = input
        .book_sequence_semantics
        .iter()
        .map(|rule| rule.market_type())
        .collect::<BTreeSet<_>>();
    if supports_books
        && (input.book_sequence_semantics.len() != input.market_types.len()
            || sequence_markets.len() != input.book_sequence_semantics.len()
            || input
                .market_types
                .iter()
                .any(|market| !sequence_markets.contains(market))
            || input
                .book_sequence_semantics
                .iter()
                .any(|rule| rule.semantics() == SequenceSemantics::None))
        || !supports_books && !input.book_sequence_semantics.is_empty()
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    if input.snapshot_method == SnapshotMethod::RestThenBufferedDeltas
        && !input
            .rate_limit_model
            .iter()
            .any(|rule| rule.transport() == RateLimitTransport::Rest)
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    if matches!(input.connection_lifetime, ConnectionLifetime::Finite { .. })
        && !input
            .rate_limit_model
            .iter()
            .any(|rule| rule.transport() == RateLimitTransport::WebSocketConnection)
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    let semantic_rate_keys = input
        .rate_limit_model
        .iter()
        .map(|rule| {
            (
                rule.transport(),
                rule.scope(),
                rule.metric(),
                rule.applies_to(),
                rule.market_types(),
            )
        })
        .collect::<BTreeSet<_>>();
    if semantic_rate_keys.len() != input.rate_limit_model.len() {
        return Err(CapabilityError::ContradictoryCapability);
    }
    let option_market = input.market_types.contains(&MarketType::Option);
    let option_stream = [
        StreamClass::OptionMetadata,
        StreamClass::OptionTicker,
        StreamClass::OptionTrades,
        StreamClass::OptionBooks,
    ]
    .into_iter()
    .any(|stream| completeness.get(stream) != Completeness::NotSupported);
    if option_market != (input.options_fields != OptionsFields::NONE || option_stream) {
        return Err(CapabilityError::ContradictoryCapability);
    }
    let derivatives_market = input.market_types.iter().any(|market| {
        matches!(
            market,
            MarketType::LinearPerpetual
                | MarketType::InversePerpetual
                | MarketType::Future
                | MarketType::Option
        )
    });
    if !derivatives_market
        && (input.funding_fields != FundingFields::NONE
            || input.open_interest_fields != OpenInterestFields::NONE)
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    let funding_supported = completeness.get(StreamClass::Funding) != Completeness::NotSupported;
    let open_interest_supported =
        completeness.get(StreamClass::OpenInterest) != Completeness::NotSupported;
    if funding_supported != (input.funding_fields != FundingFields::NONE)
        || open_interest_supported != (input.open_interest_fields != OpenInterestFields::NONE)
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    match input.venue.as_str() {
        "binance" => validate_binance_profile(input, completeness)?,
        "bybit" => validate_bybit_profile(input, completeness)?,
        _ => {}
    }
    Ok(())
}

fn validate_binance_profile(
    input: &ConnectorCapabilitiesInput,
    completeness: &CompletenessProfile,
) -> Result<(), CapabilityError> {
    validate_initial_venue_surface(input, completeness)?;
    if input.liquidation_completeness
        != (Completeness::SampledLatestPerSymbolWindow {
            window_ms: NonZeroU32::new(1_000).expect("1000 is nonzero"),
        })
        || !matches!(input.connection_lifetime, ConnectionLifetime::Finite { .. })
        || input.snapshot_method != SnapshotMethod::RestThenBufferedDeltas
        || sequence_for(input, MarketType::Spot) != Some(SequenceSemantics::InclusiveRange)
        || sequence_for(input, MarketType::LinearPerpetual)
            != Some(SequenceSemantics::InclusiveRangeWithPreviousFinal)
        || !rate_limits_cover_all_markets(input)
        || !required_rate_rules_present(input, &binance_required_rate_rules())
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    Ok(())
}

fn validate_bybit_profile(
    input: &ConnectorCapabilitiesInput,
    completeness: &CompletenessProfile,
) -> Result<(), CapabilityError> {
    validate_initial_venue_surface(input, completeness)?;
    let bybit_sequence = SequenceSemantics::SnapshotResettingUpdateId {
        reset_update_id: NonZeroU64::MIN,
        overwrite_on_every_snapshot: true,
    };
    if input.liquidation_completeness
        != (Completeness::VenueReportedAll {
            push_cadence_ms: NonZeroU32::new(500).expect("500 is nonzero"),
            delivery_uncertainty: true,
        })
        || input.snapshot_method != SnapshotMethod::WebSocketSnapshot
        || sequence_for(input, MarketType::Spot) != Some(bybit_sequence)
        || sequence_for(input, MarketType::LinearPerpetual) != Some(bybit_sequence)
        || ![1, 50, 200, 1_000]
            .into_iter()
            .all(|depth| input.book_depths.contains(&depth))
        || !rate_limits_cover_all_markets(input)
        || !required_rate_rules_present(input, &bybit_required_rate_rules())
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    Ok(())
}

fn validate_initial_venue_surface(
    input: &ConnectorCapabilitiesInput,
    completeness: &CompletenessProfile,
) -> Result<(), CapabilityError> {
    let required_markets = [MarketType::Spot, MarketType::LinearPerpetual];
    let required_streams = [
        StreamClass::InstrumentDefinitions,
        StreamClass::Trades,
        StreamClass::TopOfBook,
        StreamClass::BookSnapshots,
        StreamClass::BookDeltas,
        StreamClass::MarkAndIndex,
        StreamClass::Funding,
        StreamClass::OpenInterest,
        StreamClass::Liquidations,
        StreamClass::VenueStatus,
    ];
    if required_markets
        .into_iter()
        .any(|market| !input.market_types.contains(&market))
        || required_streams
            .into_iter()
            .any(|stream| completeness.get(stream) == Completeness::NotSupported)
    {
        return Err(CapabilityError::ContradictoryCapability);
    }
    Ok(())
}

fn sequence_for(
    input: &ConnectorCapabilitiesInput,
    market: MarketType,
) -> Option<SequenceSemantics> {
    input
        .book_sequence_semantics
        .iter()
        .find(|rule| rule.market_type() == market)
        .map(|rule| rule.semantics())
}

fn rate_limits_cover_all_markets(input: &ConnectorCapabilitiesInput) -> bool {
    input.market_types.iter().all(|market| {
        input
            .rate_limit_model
            .iter()
            .any(|rule| rule.market_types().contains(market))
    })
}

#[derive(Clone, Copy)]
struct RequiredRateRule {
    applies_to: &'static str,
    market_types: &'static [MarketType],
    transport: RateLimitTransport,
    scope: RateLimitScope,
    metric: RateLimitMetric,
    algorithm: RateLimitAlgorithm,
    limit: RateLimitValue,
    request_cost: Option<NonZeroU32>,
    source: RateLimitSource,
    action: RateLimitExceededAction,
}

fn required_rate_rules_present(
    input: &ConnectorCapabilitiesInput,
    required: &[RequiredRateRule],
) -> bool {
    required.iter().all(|expected| {
        input.rate_limit_model.iter().any(|rule| {
            rule.applies_to() == expected.applies_to
                && canonical_market_set(rule.market_types())
                    == canonical_market_set(expected.market_types)
                && rule.transport() == expected.transport
                && rule.scope() == expected.scope
                && rule.metric() == expected.metric
                && rule.algorithm() == expected.algorithm
                && rule.limit() == expected.limit
                && rule.request_cost() == expected.request_cost
                && rule.source() == expected.source
                && rule.on_exceeded() == expected.action
        })
    })
}

fn canonical_market_set(markets: &[MarketType]) -> BTreeSet<MarketType> {
    markets.iter().copied().collect()
}

fn binance_required_rate_rules() -> [RequiredRateRule; 5] {
    [
        RequiredRateRule {
            applies_to: "spot_rest",
            market_types: &[MarketType::Spot],
            transport: RateLimitTransport::Rest,
            scope: RateLimitScope::Ip,
            metric: RateLimitMetric::RequestWeight,
            algorithm: RateLimitAlgorithm::VenueAdvertisedDynamic,
            limit: RateLimitValue::VenueAdvertised,
            request_cost: None,
            source: RateLimitSource::VenueMetadataAndHeaders,
            action: RateLimitExceededAction::RetryAfterHeaderThenTemporaryBan,
        },
        RequiredRateRule {
            applies_to: "spot_stream_control",
            market_types: &[MarketType::Spot],
            transport: RateLimitTransport::WebSocketControl,
            scope: RateLimitScope::Connection,
            metric: RateLimitMetric::ControlMessages,
            algorithm: RateLimitAlgorithm::FixedWindow {
                interval_ms: NonZeroU32::new(1_000).expect("nonzero"),
            },
            limit: RateLimitValue::Fixed(NonZeroU32::new(5).expect("nonzero")),
            request_cost: NonZeroU32::new(1),
            source: RateLimitSource::StaticDocumentation,
            action: RateLimitExceededAction::DisconnectThenBanOnRepeatedViolation,
        },
        RequiredRateRule {
            applies_to: "spot_stream_connections",
            market_types: &[MarketType::Spot],
            transport: RateLimitTransport::WebSocketConnection,
            scope: RateLimitScope::Ip,
            metric: RateLimitMetric::ConnectionAttempts,
            algorithm: RateLimitAlgorithm::FixedWindow {
                interval_ms: NonZeroU32::new(300_000).expect("nonzero"),
            },
            limit: RateLimitValue::Fixed(NonZeroU32::new(300).expect("nonzero")),
            request_cost: NonZeroU32::new(1),
            source: RateLimitSource::StaticDocumentation,
            action: RateLimitExceededAction::RejectAndBackoff,
        },
        RequiredRateRule {
            applies_to: "usd_m_rest",
            market_types: &[MarketType::LinearPerpetual],
            transport: RateLimitTransport::Rest,
            scope: RateLimitScope::Ip,
            metric: RateLimitMetric::RequestWeight,
            algorithm: RateLimitAlgorithm::VenueAdvertisedDynamic,
            limit: RateLimitValue::VenueAdvertised,
            request_cost: None,
            source: RateLimitSource::VenueMetadataAndHeaders,
            action: RateLimitExceededAction::RetryAfterHeaderThenTemporaryBan,
        },
        RequiredRateRule {
            applies_to: "usd_m_stream_control",
            market_types: &[MarketType::LinearPerpetual],
            transport: RateLimitTransport::WebSocketControl,
            scope: RateLimitScope::Connection,
            metric: RateLimitMetric::ControlMessages,
            algorithm: RateLimitAlgorithm::FixedWindow {
                interval_ms: NonZeroU32::new(1_000).expect("nonzero"),
            },
            limit: RateLimitValue::Fixed(NonZeroU32::new(10).expect("nonzero")),
            request_cost: NonZeroU32::new(1),
            source: RateLimitSource::StaticDocumentation,
            action: RateLimitExceededAction::Disconnect,
        },
    ]
}

fn bybit_required_rate_rules() -> [RequiredRateRule; 4] {
    [
        RequiredRateRule {
            applies_to: "v5_market_endpoint_default",
            market_types: &[MarketType::Spot, MarketType::LinearPerpetual],
            transport: RateLimitTransport::Rest,
            scope: RateLimitScope::Uid,
            metric: RateLimitMetric::Requests,
            algorithm: RateLimitAlgorithm::RollingWindow {
                interval_ms: NonZeroU32::new(1_000).expect("nonzero"),
            },
            limit: RateLimitValue::Fixed(NonZeroU32::new(10).expect("nonzero")),
            request_cost: NonZeroU32::new(1),
            source: RateLimitSource::ResponseHeaders,
            action: RateLimitExceededAction::RetryAtResetHeader,
        },
        RequiredRateRule {
            applies_to: "public_stream_connections",
            market_types: &[MarketType::Spot, MarketType::LinearPerpetual],
            transport: RateLimitTransport::WebSocketConnection,
            scope: RateLimitScope::IpAndVenueDomain,
            metric: RateLimitMetric::ConnectionAttempts,
            algorithm: RateLimitAlgorithm::FixedWindow {
                interval_ms: NonZeroU32::new(300_000).expect("nonzero"),
            },
            limit: RateLimitValue::Fixed(NonZeroU32::new(500).expect("nonzero")),
            request_cost: NonZeroU32::new(1),
            source: RateLimitSource::StaticDocumentation,
            action: RateLimitExceededAction::RejectAndBackoff,
        },
        RequiredRateRule {
            applies_to: "public_stream_market_category",
            market_types: &[MarketType::Spot, MarketType::LinearPerpetual],
            transport: RateLimitTransport::WebSocketConnection,
            scope: RateLimitScope::IpAndMarketCategory,
            metric: RateLimitMetric::ConcurrentConnections,
            algorithm: RateLimitAlgorithm::ConcurrentGauge,
            limit: RateLimitValue::Fixed(NonZeroU32::new(1_000).expect("nonzero")),
            request_cost: None,
            source: RateLimitSource::StaticDocumentation,
            action: RateLimitExceededAction::RejectAndBackoff,
        },
        RequiredRateRule {
            applies_to: "global_http_ip_domain",
            market_types: &[MarketType::Spot, MarketType::LinearPerpetual],
            transport: RateLimitTransport::Rest,
            scope: RateLimitScope::IpAndVenueDomain,
            metric: RateLimitMetric::Requests,
            algorithm: RateLimitAlgorithm::FixedWindow {
                interval_ms: NonZeroU32::new(5_000).expect("nonzero"),
            },
            limit: RateLimitValue::Fixed(NonZeroU32::new(600).expect("nonzero")),
            request_cost: NonZeroU32::new(1),
            source: RateLimitSource::StaticDocumentation,
            action: RateLimitExceededAction::RejectAndBackoffAtLeast {
                minimum_ms: NonZeroU32::new(600_000).expect("nonzero"),
            },
        },
    ]
}

fn validate_unique_bounded<T: Copy + Ord>(
    values: &[T],
    minimum: usize,
    maximum: usize,
) -> Result<(), CapabilityError> {
    if values.len() < minimum
        || values.len() > maximum
        || values.iter().copied().collect::<BTreeSet<_>>().len() != values.len()
    {
        Err(CapabilityError::InvalidMarketTypes)
    } else {
        Ok(())
    }
}

fn validate_depths(depths: &mut [u32]) -> Result<(), CapabilityError> {
    if depths.is_empty()
        || depths.len() > MAX_BOOK_DEPTHS
        || depths.contains(&0)
        || depths.iter().copied().collect::<BTreeSet<_>>().len() != depths.len()
    {
        return Err(CapabilityError::InvalidBookDepths);
    }
    depths.sort_unstable();
    Ok(())
}

fn valid_identifier(value: &str, maximum: usize) -> bool {
    valid_text(value, maximum)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_https_reference(value: &str) -> bool {
    let authority_and_path = value.strip_prefix("https://");
    valid_text(value, MAX_LIMITATION_LEN)
        && authority_and_path.is_some_and(|suffix| {
            suffix
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
        })
        && !value.chars().any(char::is_whitespace)
}
