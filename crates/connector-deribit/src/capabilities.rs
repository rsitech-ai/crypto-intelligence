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

pub fn deribit_capabilities() -> Result<ConnectorCapabilities, CapabilityError> {
    let complete = Completeness::VenueReportedComplete {
        delivery_uncertainty: true,
    };
    let completeness = CompletenessProfile::new([Completeness::NotSupported; 14])
        .with(StreamClass::InstrumentDefinitions, complete)
        .with(StreamClass::Trades, complete)
        .with(StreamClass::TopOfBook, complete)
        .with(StreamClass::BookSnapshots, complete)
        .with(StreamClass::BookDeltas, complete)
        .with(StreamClass::MarkAndIndex, complete)
        .with(StreamClass::Funding, complete)
        .with(StreamClass::OpenInterest, complete)
        .with(StreamClass::OptionMetadata, complete)
        .with(StreamClass::OptionTicker, complete)
        .with(StreamClass::OptionTrades, complete)
        .with(StreamClass::OptionBooks, complete)
        .with(StreamClass::VenueStatus, complete);
    ConnectorCapabilities::try_new(ConnectorCapabilitiesInput {
        venue: VenueId::new("deribit").map_err(|_| CapabilityError::InvalidVenue)?,
        connector_version: "1.0.0".to_owned(),
        market_types: vec![
            MarketType::LinearPerpetual,
            MarketType::InversePerpetual,
            MarketType::Future,
            MarketType::Option,
        ],
        trade_semantics: TradeSemantics::Individual,
        book_depths: vec![1, 5, 10, 20, 50, 100, 1_000, 10_000],
        book_sequence_semantics: vec![
            BookSequenceRule::new(
                MarketType::LinearPerpetual,
                SequenceSemantics::PreviousAndCurrent,
            ),
            BookSequenceRule::new(
                MarketType::InversePerpetual,
                SequenceSemantics::PreviousAndCurrent,
            ),
            BookSequenceRule::new(MarketType::Future, SequenceSemantics::PreviousAndCurrent),
            BookSequenceRule::new(MarketType::Option, SequenceSemantics::PreviousAndCurrent),
        ],
        checksum_support: ChecksumSupport::NotSupported,
        stream_completeness: completeness,
        liquidation_completeness: Completeness::NotSupported,
        funding_fields: FundingFields::RATE,
        open_interest_fields: OpenInterestFields::VALUE,
        options_fields: OptionsFields::from_bits(
            OptionsFields::IMPLIED_VOLATILITY.bits()
                | OptionsFields::DELTA.bits()
                | OptionsFields::GAMMA.bits()
                | OptionsFields::VEGA.bits()
                | OptionsFields::THETA.bits()
                | OptionsFields::OPEN_INTEREST.bits()
                | OptionsFields::MARK_PRICE.bits()
                | OptionsFields::INDEX_PRICE.bits()
                | OptionsFields::RHO.bits(),
        )?,
        connection_lifetime: ConnectionLifetime::VenueUnspecified,
        rate_limit_model: vec![
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketControl,
                scope: RateLimitScope::Connection,
                metric: RateLimitMetric::Requests,
                applies_to: "public_subscribe".to_owned(),
                market_types: all_markets(),
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: nonzero(3_000),
                },
                limit: RateLimitValue::Fixed(nonzero(10)),
                request_cost: Some(nonzero(1)),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::Disconnect,
            })?,
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::Rest,
                scope: RateLimitScope::Ip,
                metric: RateLimitMetric::Requests,
                applies_to: "public_get_instruments_sustained".to_owned(),
                market_types: all_markets(),
                algorithm: RateLimitAlgorithm::RollingWindow {
                    interval_ms: nonzero(1_000),
                },
                limit: RateLimitValue::Fixed(nonzero(1)),
                request_cost: Some(nonzero(1)),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoff,
            })?,
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketConnection,
                scope: RateLimitScope::Ip,
                metric: RateLimitMetric::ConcurrentConnections,
                applies_to: "public_stream_connections".to_owned(),
                market_types: all_markets(),
                algorithm: RateLimitAlgorithm::ConcurrentGauge,
                limit: RateLimitValue::Fixed(nonzero(32)),
                request_cost: None,
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoff,
            })?,
        ],
        snapshot_method: SnapshotMethod::WebSocketSnapshot,
        recovery_method: RecoveryMethod::ReconnectAndResnapshot,
        source_timestamp_precision: SourceTimestampPrecision::Milliseconds,
        known_limitations: vec![
            "Retained certification uses public 100ms-compatible subscription schemas; raw channels require authorization".to_owned(),
            "Deribit does not publish a book checksum; continuity relies on prev_change_id and replacement snapshots".to_owned(),
            "Delivery-price history is a REST observation and is not part of the live book session".to_owned(),
            "The capability model encodes conservative sustained request limits; Deribit's continuous credit refill and burst pools require transport-level enforcement".to_owned(),
            "Deribit's ungrouped book has no source depth limit; this adapter intentionally rejects snapshots above 10,000 levels per side".to_owned(),
        ],
        terms_reference: "https://www.deribit.com/pages/information/terms-of-service".to_owned(),
        redistribution_class: RedistributionClass::ReviewedRestricted,
    })
}

fn all_markets() -> Vec<MarketType> {
    vec![
        MarketType::LinearPerpetual,
        MarketType::InversePerpetual,
        MarketType::Future,
        MarketType::Option,
    ]
}

const fn nonzero(value: u32) -> NonZeroU32 {
    match NonZeroU32::new(value) {
        Some(value) => value,
        None => panic!("Deribit capability constant must be nonzero"),
    }
}
