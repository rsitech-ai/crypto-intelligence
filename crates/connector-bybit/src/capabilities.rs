use std::num::{NonZeroU32, NonZeroU64};

use connector_core::{
    BookSequenceRule, CapabilityError, ChecksumSupport, Completeness, CompletenessProfile,
    ConnectionLifetime, ConnectorCapabilities, ConnectorCapabilitiesInput, FundingFields,
    MarketType, OpenInterestFields, OptionsFields, RateLimitAlgorithm, RateLimitExceededAction,
    RateLimitMetric, RateLimitRule, RateLimitRuleInput, RateLimitScope, RateLimitSource,
    RateLimitTransport, RateLimitValue, RecoveryMethod, RedistributionClass, SequenceSemantics,
    SnapshotMethod, SourceTimestampPrecision, TradeSemantics,
};
use domain::VenueId;

/// Connector-owned, validated declaration of the supported Bybit V5 surface.
pub fn bybit_capabilities() -> Result<ConnectorCapabilities, CapabilityError> {
    let resetting = SequenceSemantics::SnapshotResettingUpdateId {
        reset_update_id: NonZeroU64::MIN,
        overwrite_on_every_snapshot: true,
    };
    ConnectorCapabilities::try_new(ConnectorCapabilitiesInput {
        venue: VenueId::new("bybit").map_err(|_| CapabilityError::InvalidVenue)?,
        connector_version: "1.0.0".to_owned(),
        market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
        trade_semantics: TradeSemantics::Individual,
        book_depths: vec![1, 50, 200, 1_000],
        book_sequence_semantics: vec![
            BookSequenceRule::new(MarketType::Spot, resetting),
            BookSequenceRule::new(MarketType::LinearPerpetual, resetting),
        ],
        checksum_support: ChecksumSupport::NotSupported,
        stream_completeness: CompletenessProfile::bybit(),
        liquidation_completeness: Completeness::VenueReportedAll {
            push_cadence_ms: nonzero(500),
            delivery_uncertainty: true,
        },
        funding_fields: FundingFields::from_bits(
            FundingFields::RATE.bits() | FundingFields::NEXT_FUNDING_TIME.bits(),
        )?,
        open_interest_fields: OpenInterestFields::VALUE,
        options_fields: OptionsFields::NONE,
        connection_lifetime: ConnectionLifetime::VenueUnspecified,
        rate_limit_model: vec![
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::Rest,
                scope: RateLimitScope::Uid,
                metric: RateLimitMetric::Requests,
                applies_to: "v5_market_endpoint_default".to_owned(),
                market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::RollingWindow {
                    interval_ms: nonzero(1_000),
                },
                limit: RateLimitValue::Fixed(nonzero(10)),
                request_cost: Some(nonzero(1)),
                source: RateLimitSource::ResponseHeaders,
                on_exceeded: RateLimitExceededAction::RetryAtResetHeader,
            })?,
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketConnection,
                scope: RateLimitScope::IpAndVenueDomain,
                metric: RateLimitMetric::ConnectionAttempts,
                applies_to: "public_stream_connections".to_owned(),
                market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: nonzero(300_000),
                },
                limit: RateLimitValue::Fixed(nonzero(500)),
                request_cost: Some(nonzero(1)),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoff,
            })?,
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketConnection,
                scope: RateLimitScope::IpAndMarketCategory,
                metric: RateLimitMetric::ConcurrentConnections,
                applies_to: "public_stream_market_category".to_owned(),
                market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::ConcurrentGauge,
                limit: RateLimitValue::Fixed(nonzero(1_000)),
                request_cost: None,
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoff,
            })?,
            rate_rule(RateLimitRuleInput {
                transport: RateLimitTransport::Rest,
                scope: RateLimitScope::IpAndVenueDomain,
                metric: RateLimitMetric::Requests,
                applies_to: "global_http_ip_domain".to_owned(),
                market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: nonzero(5_000),
                },
                limit: RateLimitValue::Fixed(nonzero(600)),
                request_cost: Some(nonzero(1)),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoffAtLeast {
                    minimum_ms: nonzero(600_000),
                },
            })?,
        ],
        snapshot_method: SnapshotMethod::WebSocketSnapshot,
        recovery_method: RecoveryMethod::ReconnectAndResnapshot,
        source_timestamp_precision: SourceTimestampPrecision::Milliseconds,
        known_limitations: vec![
            "delivery completeness remains subject to transport loss".to_owned(),
            "linear instrument discovery requires cursor pagination beyond the default page"
                .to_owned(),
        ],
        terms_reference: "https://www.bybit.com/en/help-center/bybitHC_Article?id=000001258"
            .to_owned(),
        redistribution_class: RedistributionClass::ReviewedRestricted,
    })
}

fn rate_rule(input: RateLimitRuleInput) -> Result<RateLimitRule, CapabilityError> {
    RateLimitRule::try_new(input)
}

const fn nonzero(value: u32) -> NonZeroU32 {
    match NonZeroU32::new(value) {
        Some(value) => value,
        None => panic!("Bybit capability constant must be nonzero"),
    }
}
