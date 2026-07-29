use std::num::NonZeroU32;

use connector_core::{
    BookSequenceRule, CapabilityError, ChecksumSupport, Completeness, CompletenessProfile,
    ConnectionLifetime, ConnectorCapabilities, ConnectorCapabilitiesInput, FundingFields,
    MarketType, OpenInterestFields, OptionsFields, RateLimitAlgorithm, RateLimitExceededAction,
    RateLimitMetric, RateLimitRule, RateLimitRuleInput, RateLimitScope, RateLimitSource,
    RateLimitTransport, RateLimitValue, RecoveryMethod, RedistributionClass, SequenceSemantics,
    SnapshotMethod, SourceTimestampPrecision, StreamClass, TradeSemantics,
};
use domain::VenueId;

pub fn kraken_capabilities() -> Result<ConnectorCapabilities, CapabilityError> {
    let complete = Completeness::VenueReportedComplete {
        delivery_uncertainty: true,
    };
    let completeness = CompletenessProfile::new([Completeness::NotSupported; 14])
        .with(StreamClass::InstrumentDefinitions, complete)
        .with(StreamClass::Trades, complete)
        .with(StreamClass::TopOfBook, complete)
        .with(StreamClass::BookSnapshots, complete)
        .with(StreamClass::BookDeltas, complete)
        .with(StreamClass::VenueStatus, complete);
    let rate_limit_model = [(10_u32, 5_u32), (100, 25), (1_000, 100)]
        .into_iter()
        .map(|(depth, cost)| l3_standard_subscription_rule(depth, cost))
        .collect::<Result<Vec<_>, _>>()?;
    ConnectorCapabilities::try_new(ConnectorCapabilitiesInput {
        venue: VenueId::new("kraken").map_err(|_| CapabilityError::InvalidVenue)?,
        connector_version: "1.0.0".to_owned(),
        market_types: vec![MarketType::Spot, MarketType::Future],
        trade_semantics: TradeSemantics::Individual,
        book_depths: vec![10, 25, 100, 500, 1_000],
        book_sequence_semantics: vec![
            BookSequenceRule::new(
                MarketType::Spot,
                SequenceSemantics::ChecksumValidatedNoSequence,
            ),
            BookSequenceRule::new(MarketType::Future, SequenceSemantics::MonotonicUpdateId),
        ],
        checksum_support: ChecksumSupport::Crc32DecimalBook,
        stream_completeness: completeness,
        liquidation_completeness: Completeness::NotSupported,
        funding_fields: FundingFields::NONE,
        open_interest_fields: OpenInterestFields::NONE,
        options_fields: OptionsFields::NONE,
        connection_lifetime: ConnectionLifetime::VenueUnspecified,
        rate_limit_model,
        snapshot_method: SnapshotMethod::WebSocketSnapshot,
        recovery_method: RecoveryMethod::ReconnectAndResnapshot,
        source_timestamp_precision: SourceTimestampPrecision::Milliseconds,
        known_limitations: vec![
            "CRC32 validation applies to Kraken Spot v2 books, not the Futures feed".to_owned(),
            "Spot v2 book continuity is checksum-based because the feed has no source sequence"
                .to_owned(),
            "L3 requires authentication and remains disabled unless a Tier A symbol is explicitly allowlisted".to_owned(),
            "Futures source timestamps are milliseconds while Spot v2 timestamps may be nanoseconds"
                .to_owned(),
        ],
        terms_reference: "https://www.kraken.com/legal".to_owned(),
        redistribution_class: RedistributionClass::ReviewedRestricted,
    })
}

fn l3_standard_subscription_rule(
    depth: u32,
    request_cost: u32,
) -> Result<RateLimitRule, CapabilityError> {
    RateLimitRule::try_new(RateLimitRuleInput {
        transport: RateLimitTransport::WebSocketControl,
        scope: RateLimitScope::Connection,
        metric: RateLimitMetric::Requests,
        applies_to: format!("spot_level3_depth_{depth}_standard_subscription_counter"),
        market_types: vec![MarketType::Spot],
        algorithm: RateLimitAlgorithm::FixedWindow {
            interval_ms: nonzero(1_000),
        },
        limit: RateLimitValue::Fixed(nonzero(200)),
        request_cost: Some(nonzero(request_cost)),
        source: RateLimitSource::StaticDocumentation,
        on_exceeded: RateLimitExceededAction::RejectAndBackoff,
    })
}

const fn nonzero(value: u32) -> NonZeroU32 {
    match NonZeroU32::new(value) {
        Some(value) => value,
        None => panic!("Kraken capability constant must be nonzero"),
    }
}
