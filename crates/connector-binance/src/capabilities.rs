use std::num::NonZeroU32;

use connector_core::{
    BookSequenceRule, CapabilityError, ChecksumSupport, Completeness, CompletenessProfile,
    ConnectionLifetime, ConnectorCapabilities, ConnectorCapabilitiesInput, FundingFields,
    MarketType, OpenInterestFields, OptionsFields, RateLimitAlgorithm, RateLimitExceededAction,
    RateLimitMetric, RateLimitRule, RateLimitRuleInput, RateLimitScope, RateLimitSource,
    RateLimitTransport, RateLimitValue, RecoveryMethod, RedistributionClass, SequenceSemantics,
    SnapshotMethod, SourceTimestampPrecision, TradeSemantics,
};
use domain::VenueId;

/// Connector-owned, validated declaration of the supported Binance surface.
pub fn binance_capabilities() -> Result<ConnectorCapabilities, CapabilityError> {
    ConnectorCapabilities::try_new(ConnectorCapabilitiesInput {
        venue: VenueId::new("binance").map_err(|_| CapabilityError::InvalidVenue)?,
        connector_version: "1.0.0".to_owned(),
        market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
        trade_semantics: TradeSemantics::Aggregate,
        book_depths: vec![5, 100, 1_000],
        book_sequence_semantics: vec![
            BookSequenceRule::new(MarketType::Spot, SequenceSemantics::InclusiveRange),
            BookSequenceRule::new(
                MarketType::LinearPerpetual,
                SequenceSemantics::InclusiveRangeWithPreviousFinal,
            ),
        ],
        checksum_support: ChecksumSupport::NotSupported,
        stream_completeness: CompletenessProfile::binance(),
        liquidation_completeness: Completeness::SampledLargestPerSymbolWindow {
            window_ms: nonzero(1_000),
        },
        funding_fields: FundingFields::from_bits(
            FundingFields::RATE.bits() | FundingFields::NEXT_FUNDING_TIME.bits(),
        )?,
        open_interest_fields: OpenInterestFields::VALUE,
        options_fields: OptionsFields::NONE,
        connection_lifetime: ConnectionLifetime::Finite {
            lifetime_ms: nonzero(86_400_000),
            renewal_margin_ms: nonzero(300_000),
        },
        rate_limit_model: vec![
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::Rest,
                scope: RateLimitScope::Ip,
                metric: RateLimitMetric::RequestWeight,
                applies_to: "spot_rest".to_owned(),
                market_types: vec![MarketType::Spot],
                algorithm: RateLimitAlgorithm::VenueAdvertisedDynamic,
                limit: RateLimitValue::VenueAdvertised,
                request_cost: None,
                source: RateLimitSource::VenueMetadataAndHeaders,
                on_exceeded: RateLimitExceededAction::RetryAfterHeaderThenTemporaryBan,
            })?,
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketControl,
                scope: RateLimitScope::Connection,
                metric: RateLimitMetric::ControlMessages,
                applies_to: "spot_stream_control".to_owned(),
                market_types: vec![MarketType::Spot],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: nonzero(1_000),
                },
                limit: RateLimitValue::Fixed(nonzero(5)),
                request_cost: Some(nonzero(1)),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::DisconnectThenBanOnRepeatedViolation,
            })?,
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketConnection,
                scope: RateLimitScope::Ip,
                metric: RateLimitMetric::ConnectionAttempts,
                applies_to: "spot_stream_connections".to_owned(),
                market_types: vec![MarketType::Spot],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: nonzero(300_000),
                },
                limit: RateLimitValue::Fixed(nonzero(300)),
                request_cost: Some(nonzero(1)),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoff,
            })?,
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::Rest,
                scope: RateLimitScope::Ip,
                metric: RateLimitMetric::RequestWeight,
                applies_to: "usd_m_rest".to_owned(),
                market_types: vec![MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::VenueAdvertisedDynamic,
                limit: RateLimitValue::VenueAdvertised,
                request_cost: None,
                source: RateLimitSource::VenueMetadataAndHeaders,
                on_exceeded: RateLimitExceededAction::RetryAfterHeaderThenTemporaryBan,
            })?,
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketControl,
                scope: RateLimitScope::Connection,
                metric: RateLimitMetric::ControlMessages,
                applies_to: "usd_m_stream_control".to_owned(),
                market_types: vec![MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: nonzero(1_000),
                },
                limit: RateLimitValue::Fixed(nonzero(10)),
                request_cost: Some(nonzero(1)),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::Disconnect,
            })?,
        ],
        snapshot_method: SnapshotMethod::RestThenBufferedDeltas,
        recovery_method: RecoveryMethod::ReconnectAndResnapshot,
        source_timestamp_precision: SourceTimestampPrecision::Milliseconds,
        known_limitations: vec![
            "USD-M liquidations expose only the largest order per symbol per 1000 ms".to_owned(),
            "delivery completeness remains subject to transport loss".to_owned(),
        ],
        terms_reference: "https://www.binance.com/en/terms".to_owned(),
        redistribution_class: RedistributionClass::ReviewedRestricted,
    })
}

fn rate_rule(input: RateLimitRuleInput) -> Result<RateLimitRule, CapabilityError> {
    RateLimitRule::try_new(input)
}

const fn nonzero(value: u32) -> NonZeroU32 {
    match NonZeroU32::new(value) {
        Some(value) => value,
        None => panic!("Binance capability constant must be nonzero"),
    }
}
