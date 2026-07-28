use std::{
    num::{NonZeroU32, NonZeroU64},
    pin::Pin,
    task::{Context, Poll},
};

use connector_core::{
    BookStreamKey, BoundedNormalizedChannel, CancellationChannel, CapabilityError,
    CapacityRejection, ChannelError, ChecksumSupport, Completeness, CompletenessProfile,
    ConnectionLifetime, ConnectorCapabilities, ConnectorCapabilitiesInput, ConnectorCommand,
    ConnectorCommandChannel, ConnectorContext, ConnectorFuture, ConnectorState,
    ConnectorTermination, DeliveryOutcome, DurableRawCaptureChannel, DurableRawReference,
    EventPriority, FatalConnectorError, FundingFields, LifecycleCause, LifecycleChannel,
    LifecycleError, LifecycleTracker, LossPolicyRegistry, LossReason, MAX_RAW_CAPTURE_BYTES,
    MarketDataConnector, MarketType, NormalizedOutput, OpenInterestFields, OptionsFields,
    ParseRejection, ParseRejectionReason, RateLimitAlgorithm, RateLimitExceededAction,
    RateLimitMetric, RateLimitRule, RateLimitRuleInput, RateLimitScope, RateLimitSource,
    RateLimitTransport, RateLimitValue, RawCapture, RawCaptureError, RecoverableDisconnect,
    RecoveryMethod, RedistributionClass, ResynchronizationReason, SamplingPolicy,
    SequenceSemantics, SnapshotMethod, SourceTimestampPrecision, StreamClass,
    SupervisorCommandKind, TradeSemantics, WalRejectionReason,
};
use domain::{InstrumentId, ProductType, SourceId, SourceKind, UnixNanos, VenueId};
use event_envelope::{
    BookDelta, BookSnapshot, EventEnvelope, EventType, QualityFlags, SnapshotKind,
    UncheckedEventMetadata, UncheckedEventPayload, VenueState, VenueStatus,
};
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter, WalAppendAuthority, WalAppendProof},
    prologue::{SegmentMetadata, StreamDescriptor},
};

fn source() -> SourceId {
    SourceId::new(SourceKind::Exchange, "binance", 1).expect("fixture source")
}

fn make_raw_channel(
    authority: WalAppendAuthority,
    item_capacity: usize,
    byte_capacity: usize,
) -> Result<DurableRawCaptureChannel, RawCaptureError> {
    DurableRawCaptureChannel::new(
        authority,
        source(),
        NonZeroU32::new(7).expect("stream"),
        item_capacity,
        byte_capacity,
    )
}

fn capabilities_input() -> ConnectorCapabilitiesInput {
    ConnectorCapabilitiesInput {
        venue: VenueId::new("binance").expect("venue"),
        connector_version: "1.0.0".to_owned(),
        market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
        trade_semantics: TradeSemantics::AggregateAndIndividual,
        book_depths: vec![5, 100, 1_000],
        book_sequence_semantics: vec![
            connector_core::BookSequenceRule::new(
                MarketType::Spot,
                SequenceSemantics::InclusiveRange,
            ),
            connector_core::BookSequenceRule::new(
                MarketType::LinearPerpetual,
                SequenceSemantics::InclusiveRangeWithPreviousFinal,
            ),
        ],
        checksum_support: ChecksumSupport::NotSupported,
        stream_completeness: CompletenessProfile::binance(),
        liquidation_completeness: Completeness::SampledLatestPerSymbolWindow {
            window_ms: NonZeroU32::new(1_000).expect("nonzero"),
        },
        funding_fields: FundingFields::from_bits(
            FundingFields::RATE.bits() | FundingFields::NEXT_FUNDING_TIME.bits(),
        )
        .expect("known funding fields"),
        open_interest_fields: OpenInterestFields::VALUE,
        options_fields: OptionsFields::NONE,
        connection_lifetime: ConnectionLifetime::Finite {
            lifetime_ms: NonZeroU32::new(86_400_000).expect("nonzero"),
            renewal_margin_ms: NonZeroU32::new(300_000).expect("nonzero"),
        },
        rate_limit_model: vec![
            RateLimitRule::try_new(RateLimitRuleInput {
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
            })
            .expect("bounded REST rule"),
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketControl,
                scope: RateLimitScope::Connection,
                metric: RateLimitMetric::ControlMessages,
                applies_to: "spot_stream_control".to_owned(),
                market_types: vec![MarketType::Spot],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: NonZeroU32::new(1_000).expect("nonzero"),
                },
                limit: RateLimitValue::Fixed(NonZeroU32::new(5).expect("nonzero")),
                request_cost: NonZeroU32::new(1),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::DisconnectThenBanOnRepeatedViolation,
            })
            .expect("bounded rate rule"),
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketConnection,
                scope: RateLimitScope::Ip,
                metric: RateLimitMetric::ConnectionAttempts,
                applies_to: "spot_stream_connections".to_owned(),
                market_types: vec![MarketType::Spot],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: NonZeroU32::new(300_000).expect("nonzero"),
                },
                limit: RateLimitValue::Fixed(NonZeroU32::new(300).expect("nonzero")),
                request_cost: NonZeroU32::new(1),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoff,
            })
            .expect("bounded connection rule"),
            RateLimitRule::try_new(RateLimitRuleInput {
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
            })
            .expect("USD-M REST rule"),
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketControl,
                scope: RateLimitScope::Connection,
                metric: RateLimitMetric::ControlMessages,
                applies_to: "usd_m_stream_control".to_owned(),
                market_types: vec![MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: NonZeroU32::new(1_000).expect("nonzero"),
                },
                limit: RateLimitValue::Fixed(NonZeroU32::new(10).expect("nonzero")),
                request_cost: NonZeroU32::new(1),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::Disconnect,
            })
            .expect("USD-M stream rule"),
        ],
        snapshot_method: SnapshotMethod::RestThenBufferedDeltas,
        recovery_method: RecoveryMethod::ReconnectAndResnapshot,
        source_timestamp_precision: SourceTimestampPrecision::Milliseconds,
        known_limitations: vec!["liquidations are sampled per symbol".to_owned()],
        terms_reference: "https://www.binance.com/en/terms".to_owned(),
        redistribution_class: RedistributionClass::ReviewedRestricted,
    }
}

fn bybit_capabilities_input() -> ConnectorCapabilitiesInput {
    ConnectorCapabilitiesInput {
        venue: VenueId::new("bybit").expect("venue"),
        connector_version: "1.0.0".to_owned(),
        market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
        trade_semantics: TradeSemantics::Individual,
        book_depths: vec![1, 50, 200, 1_000],
        book_sequence_semantics: vec![
            connector_core::BookSequenceRule::new(
                MarketType::Spot,
                SequenceSemantics::SnapshotResettingUpdateId {
                    reset_update_id: NonZeroU64::MIN,
                    overwrite_on_every_snapshot: true,
                },
            ),
            connector_core::BookSequenceRule::new(
                MarketType::LinearPerpetual,
                SequenceSemantics::SnapshotResettingUpdateId {
                    reset_update_id: NonZeroU64::MIN,
                    overwrite_on_every_snapshot: true,
                },
            ),
        ],
        checksum_support: ChecksumSupport::NotSupported,
        stream_completeness: CompletenessProfile::bybit(),
        liquidation_completeness: Completeness::VenueReportedAll {
            push_cadence_ms: NonZeroU32::new(500).expect("nonzero"),
            delivery_uncertainty: true,
        },
        funding_fields: FundingFields::from_bits(
            FundingFields::RATE.bits() | FundingFields::NEXT_FUNDING_TIME.bits(),
        )
        .expect("funding fields"),
        open_interest_fields: OpenInterestFields::VALUE,
        options_fields: OptionsFields::NONE,
        connection_lifetime: ConnectionLifetime::VenueUnspecified,
        rate_limit_model: vec![
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::Rest,
                scope: RateLimitScope::Uid,
                metric: RateLimitMetric::Requests,
                applies_to: "v5_market_endpoint_default".to_owned(),
                market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::RollingWindow {
                    interval_ms: NonZeroU32::new(1_000).expect("nonzero"),
                },
                limit: RateLimitValue::Fixed(NonZeroU32::new(10).expect("nonzero")),
                request_cost: NonZeroU32::new(1),
                source: RateLimitSource::ResponseHeaders,
                on_exceeded: RateLimitExceededAction::RetryAtResetHeader,
            })
            .expect("rolling endpoint rule"),
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketConnection,
                scope: RateLimitScope::IpAndVenueDomain,
                metric: RateLimitMetric::ConnectionAttempts,
                applies_to: "public_stream_connections".to_owned(),
                market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: NonZeroU32::new(300_000).expect("nonzero"),
                },
                limit: RateLimitValue::Fixed(NonZeroU32::new(500).expect("nonzero")),
                request_cost: NonZeroU32::new(1),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoff,
            })
            .expect("connection attempt rule"),
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::WebSocketConnection,
                scope: RateLimitScope::IpAndMarketCategory,
                metric: RateLimitMetric::ConcurrentConnections,
                applies_to: "public_stream_market_category".to_owned(),
                market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::ConcurrentGauge,
                limit: RateLimitValue::Fixed(NonZeroU32::new(1_000).expect("nonzero")),
                request_cost: None,
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoff,
            })
            .expect("concurrent connection rule"),
            RateLimitRule::try_new(RateLimitRuleInput {
                transport: RateLimitTransport::Rest,
                scope: RateLimitScope::IpAndVenueDomain,
                metric: RateLimitMetric::Requests,
                applies_to: "global_http_ip_domain".to_owned(),
                market_types: vec![MarketType::Spot, MarketType::LinearPerpetual],
                algorithm: RateLimitAlgorithm::FixedWindow {
                    interval_ms: NonZeroU32::new(5_000).expect("nonzero"),
                },
                limit: RateLimitValue::Fixed(NonZeroU32::new(600).expect("nonzero")),
                request_cost: NonZeroU32::new(1),
                source: RateLimitSource::StaticDocumentation,
                on_exceeded: RateLimitExceededAction::RejectAndBackoffAtLeast {
                    minimum_ms: NonZeroU32::new(600_000).expect("nonzero"),
                },
            })
            .expect("global HTTP IP rule"),
        ],
        snapshot_method: SnapshotMethod::WebSocketSnapshot,
        recovery_method: RecoveryMethod::ReconnectAndResnapshot,
        source_timestamp_precision: SourceTimestampPrecision::Milliseconds,
        known_limitations: vec![
            "delivery completeness remains subject to transport loss".to_owned(),
        ],
        terms_reference: "https://www.bybit.com/en/help-center/bybitHC_Article?id=000001258"
            .to_owned(),
        redistribution_class: RedistributionClass::ReviewedRestricted,
    }
}

#[test]
fn normative_lifecycle_has_exact_states_and_transition_table() {
    let states = ConnectorState::ALL;
    assert_eq!(states.len(), 9);
    assert_eq!(
        states,
        [
            ConnectorState::Stopped,
            ConnectorState::Connecting,
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::Synchronizing,
            ConnectorState::Healthy,
            ConnectorState::Degraded,
            ConnectorState::BackingOff,
            ConnectorState::Recovering,
            ConnectorState::Quarantined,
        ]
    );

    let allowed = [
        (ConnectorState::Stopped, ConnectorState::Connecting),
        (
            ConnectorState::Connecting,
            ConnectorState::AuthenticatingOrSubscribing,
        ),
        (ConnectorState::Connecting, ConnectorState::BackingOff),
        (
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::Synchronizing,
        ),
        (
            ConnectorState::AuthenticatingOrSubscribing,
            ConnectorState::BackingOff,
        ),
        (ConnectorState::Synchronizing, ConnectorState::Healthy),
        (ConnectorState::Synchronizing, ConnectorState::Degraded),
        (ConnectorState::Synchronizing, ConnectorState::BackingOff),
        (ConnectorState::Synchronizing, ConnectorState::Recovering),
        (ConnectorState::Healthy, ConnectorState::Degraded),
        (ConnectorState::Healthy, ConnectorState::Recovering),
        (ConnectorState::Healthy, ConnectorState::BackingOff),
        (ConnectorState::Degraded, ConnectorState::Healthy),
        (ConnectorState::Degraded, ConnectorState::Recovering),
        (ConnectorState::Degraded, ConnectorState::BackingOff),
        (ConnectorState::BackingOff, ConnectorState::Connecting),
        (ConnectorState::Recovering, ConnectorState::Synchronizing),
        (ConnectorState::Recovering, ConnectorState::BackingOff),
        (ConnectorState::Quarantined, ConnectorState::Recovering),
    ];

    for from in states {
        for to in states {
            let expected = from != to
                && (allowed.contains(&(from, to))
                    || (to == ConnectorState::Stopped && from != ConnectorState::Stopped)
                    || (to == ConnectorState::Quarantined
                        && from != ConnectorState::Stopped
                        && from != ConnectorState::Quarantined));
            assert_eq!(
                from.transition_to(to).is_ok(),
                expected,
                "unexpected transition decision: {from:?} -> {to:?}"
            );
        }
    }
    assert_eq!(
        ConnectorState::Healthy.transition_to(ConnectorState::Connecting),
        Err(LifecycleError::InvalidTransition {
            from: ConnectorState::Healthy,
            to: ConnectorState::Connecting,
        })
    );
}

#[test]
fn every_allowed_lifecycle_transition_has_a_compatible_cause() {
    for from in ConnectorState::ALL {
        for to in ConnectorState::ALL {
            if from.transition_to(to).is_ok() {
                assert!(
                    LifecycleCause::ALL
                        .into_iter()
                        .any(|cause| cause.is_compatible(from, to)),
                    "allowed transition has no cause: {from:?} -> {to:?}"
                );
            }
        }
    }
    assert!(
        LifecycleCause::ResynchronizationStarted
            .is_compatible(ConnectorState::Recovering, ConnectorState::Synchronizing)
    );
    assert!(
        !LifecycleCause::SnapshotApplied
            .is_compatible(ConnectorState::Recovering, ConnectorState::Healthy)
    );
}

#[test]
fn lifecycle_tracker_enforces_cause_attribution_and_consecutive_history() {
    let epoch = NonZeroU64::new(3).expect("epoch");
    let mut tracker = LifecycleTracker::new(source(), epoch);
    assert!(matches!(
        tracker.prepare(
            ConnectorState::Connecting,
            LifecycleCause::Shutdown,
            UnixNanos::new(1)
        ),
        Err(LifecycleError::IncompatibleCause)
    ));
    assert_eq!(tracker.state(), ConnectorState::Stopped);

    let startup = tracker
        .prepare(
            ConnectorState::Connecting,
            LifecycleCause::Startup,
            UnixNanos::new(2),
        )
        .expect("startup transition");
    assert_eq!(startup.event().source(), &source());
    assert_eq!(startup.event().connection_epoch(), epoch);
    assert_eq!(startup.event().sequence().get(), 1);
    tracker.commit(startup).expect("commit startup");

    let subscribed = tracker
        .prepare(
            ConnectorState::AuthenticatingOrSubscribing,
            LifecycleCause::SubscriptionAccepted,
            UnixNanos::new(3),
        )
        .expect("subscription transition");
    assert_eq!(subscribed.event().from(), ConnectorState::Connecting);
    assert_eq!(subscribed.event().sequence().get(), 2);
    tracker.commit(subscribed).expect("commit subscription");
    assert_eq!(tracker.state(), ConnectorState::AuthenticatingOrSubscribing);

    let mut local = LifecycleTracker::new(source(), epoch);
    let foreign_source = SourceId::new(SourceKind::Exchange, "kraken", 1).expect("foreign source");
    let foreign = LifecycleTracker::new(foreign_source, NonZeroU64::new(4).expect("foreign epoch"))
        .prepare(
            ConnectorState::Connecting,
            LifecycleCause::Startup,
            UnixNanos::new(4),
        )
        .expect("foreign transition");
    assert!(matches!(
        local.commit(foreign),
        Err(LifecycleError::InvalidTransition { .. })
    ));
    assert_eq!(local.state(), ConnectorState::Stopped);
    let startup = local
        .prepare(
            ConnectorState::Connecting,
            LifecycleCause::Startup,
            UnixNanos::new(5),
        )
        .expect("local startup");
    local.commit(startup).expect("local commit");
    assert!(matches!(
        local.prepare(
            ConnectorState::Stopped,
            LifecycleCause::SupervisorCommand,
            UnixNanos::new(6)
        ),
        Err(LifecycleError::IncompatibleCause)
    ));
}

#[test]
fn venue_liquidation_completeness_preserves_exact_semantics() {
    assert_eq!(
        CompletenessProfile::binance().get(StreamClass::Liquidations),
        Completeness::SampledLatestPerSymbolWindow {
            window_ms: NonZeroU32::new(1_000).expect("nonzero")
        }
    );
    assert_eq!(
        CompletenessProfile::bybit().get(StreamClass::Liquidations),
        Completeness::VenueReportedAll {
            push_cadence_ms: NonZeroU32::new(500).expect("nonzero"),
            delivery_uncertainty: true,
        }
    );
}

#[test]
fn venue_capabilities_fail_closed_on_official_semantic_contradictions() {
    let binance =
        ConnectorCapabilities::try_new(capabilities_input()).expect("valid Binance profile");
    assert!(binance.rate_limit_model().iter().any(|rule| {
        rule.applies_to() == "spot_stream_control"
            && rule.on_exceeded() == RateLimitExceededAction::DisconnectThenBanOnRepeatedViolation
    }));
    let bybit =
        ConnectorCapabilities::try_new(bybit_capabilities_input()).expect("valid Bybit profile");
    assert!(bybit.rate_limit_model().iter().any(|rule| {
        rule.algorithm() == RateLimitAlgorithm::ConcurrentGauge
            && rule.metric() == RateLimitMetric::ConcurrentConnections
            && rule.limit() == RateLimitValue::Fixed(NonZeroU32::new(1_000).expect("nonzero"))
    }));
    assert!(bybit.rate_limit_model().iter().any(|rule| {
        rule.applies_to() == "global_http_ip_domain"
            && rule.on_exceeded()
                == (RateLimitExceededAction::RejectAndBackoffAtLeast {
                    minimum_ms: NonZeroU32::new(600_000).expect("nonzero"),
                })
    }));
    assert!(bybit.rate_limit_model().iter().any(|rule| {
        matches!(
            rule.algorithm(),
            RateLimitAlgorithm::RollingWindow { interval_ms }
                if interval_ms == NonZeroU32::new(1_000).expect("nonzero")
        ) && rule.scope() == RateLimitScope::Uid
    }));

    let mut invalid = capabilities_input();
    invalid.connection_lifetime = ConnectionLifetime::Unlimited;
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid.snapshot_method = SnapshotMethod::WebSocketSnapshot;
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid.book_sequence_semantics = vec![
        connector_core::BookSequenceRule::new(
            MarketType::Spot,
            SequenceSemantics::MonotonicUpdateId,
        ),
        connector_core::BookSequenceRule::new(
            MarketType::LinearPerpetual,
            SequenceSemantics::InclusiveRangeWithPreviousFinal,
        ),
    ];
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = bybit_capabilities_input();
    invalid.book_sequence_semantics = vec![
        connector_core::BookSequenceRule::new(
            MarketType::Spot,
            SequenceSemantics::MonotonicUpdateId,
        ),
        connector_core::BookSequenceRule::new(
            MarketType::LinearPerpetual,
            SequenceSemantics::MonotonicUpdateId,
        ),
    ];
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = bybit_capabilities_input();
    invalid.liquidation_completeness = Completeness::SampledLatestPerSymbolWindow {
        window_ms: NonZeroU32::new(1_000).expect("nonzero"),
    };
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid.stream_completeness = invalid
        .stream_completeness
        .with(StreamClass::Funding, Completeness::NotSupported);
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid.open_interest_fields = OpenInterestFields::NONE;
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid.market_types = vec![MarketType::Spot];
    invalid.book_sequence_semantics = vec![connector_core::BookSequenceRule::new(
        MarketType::Spot,
        SequenceSemantics::InclusiveRange,
    )];
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::InvalidRateLimitModel)
    );

    let mut invalid = capabilities_input();
    invalid.stream_completeness = invalid
        .stream_completeness
        .with(StreamClass::BookSnapshots, Completeness::NotSupported)
        .with(StreamClass::BookDeltas, Completeness::NotSupported);
    invalid.snapshot_method = SnapshotMethod::NotApplicable;
    invalid.book_sequence_semantics.clear();
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = bybit_capabilities_input();
    invalid.book_depths = vec![1, 50, 200, 500];
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = bybit_capabilities_input();
    invalid
        .rate_limit_model
        .retain(|rule| rule.applies_to() != "global_http_ip_domain");
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid
        .rate_limit_model
        .retain(|rule| !rule.market_types().contains(&MarketType::LinearPerpetual));
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = bybit_capabilities_input();
    invalid.rate_limit_model.retain(|rule| {
        !matches!(
            rule.applies_to(),
            "v5_market_endpoint_default" | "public_stream_connections"
        )
    });
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid.rate_limit_model.retain(|rule| {
        !matches!(
            rule.applies_to(),
            "spot_stream_control" | "usd_m_stream_control"
        )
    });
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid.rate_limit_model[1] = RateLimitRule::try_new(RateLimitRuleInput {
        transport: RateLimitTransport::WebSocketControl,
        scope: RateLimitScope::Connection,
        metric: RateLimitMetric::ControlMessages,
        applies_to: "spot_stream_control".to_owned(),
        market_types: vec![MarketType::LinearPerpetual],
        algorithm: RateLimitAlgorithm::FixedWindow {
            interval_ms: NonZeroU32::new(1_000).expect("nonzero"),
        },
        limit: RateLimitValue::Fixed(NonZeroU32::new(5).expect("nonzero")),
        request_cost: NonZeroU32::new(1),
        source: RateLimitSource::StaticDocumentation,
        on_exceeded: RateLimitExceededAction::DisconnectThenBanOnRepeatedViolation,
    })
    .expect("individually valid but incorrectly scoped rule");
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = bybit_capabilities_input();
    invalid.rate_limit_model[0] = RateLimitRule::try_new(RateLimitRuleInput {
        transport: RateLimitTransport::Rest,
        scope: RateLimitScope::Uid,
        metric: RateLimitMetric::Requests,
        applies_to: "v5_market_endpoint_default".to_owned(),
        market_types: vec![MarketType::Spot],
        algorithm: RateLimitAlgorithm::RollingWindow {
            interval_ms: NonZeroU32::new(1_000).expect("nonzero"),
        },
        limit: RateLimitValue::Fixed(NonZeroU32::new(10).expect("nonzero")),
        request_cost: NonZeroU32::new(1),
        source: RateLimitSource::ResponseHeaders,
        on_exceeded: RateLimitExceededAction::RetryAtResetHeader,
    })
    .expect("individually valid but narrowed rule");
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );
}

#[test]
fn capabilities_require_every_field_and_fail_closed_on_bounds() {
    let capabilities =
        ConnectorCapabilities::try_new(capabilities_input()).expect("complete capabilities");
    assert_eq!(
        capabilities.venue(),
        &VenueId::new("binance").expect("venue")
    );
    assert_eq!(capabilities.market_types().len(), 2);
    assert_eq!(
        capabilities.completeness().get(StreamClass::Liquidations),
        CompletenessProfile::binance().get(StreamClass::Liquidations)
    );

    let mut invalid = capabilities_input();
    invalid.known_limitations = vec!["x".repeat(257)];
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::InvalidKnownLimitation)
    );

    let mut invalid = capabilities_input();
    invalid.terms_reference = "http://example.com/terms".to_owned();
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::InvalidTermsReference)
    );

    let mut invalid = capabilities_input();
    invalid.connection_lifetime = ConnectionLifetime::Finite {
        lifetime_ms: NonZeroU32::new(300_000).expect("nonzero"),
        renewal_margin_ms: NonZeroU32::new(300_000).expect("nonzero"),
    };
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::InvalidConnectionRenewal)
    );

    let mut invalid = capabilities_input();
    invalid.market_types.push(MarketType::Spot);
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::InvalidMarketTypes)
    );

    let mut invalid = capabilities_input();
    invalid
        .rate_limit_model
        .push(invalid.rate_limit_model[0].clone());
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::InvalidRateLimitModel)
    );

    let mut invalid = capabilities_input();
    invalid.rate_limit_model[0] = RateLimitRule::try_new(RateLimitRuleInput {
        transport: RateLimitTransport::Rest,
        scope: RateLimitScope::Ip,
        metric: RateLimitMetric::RequestWeight,
        applies_to: "spot_rest".to_owned(),
        market_types: vec![MarketType::Option],
        algorithm: RateLimitAlgorithm::VenueAdvertisedDynamic,
        limit: RateLimitValue::VenueAdvertised,
        request_cost: None,
        source: RateLimitSource::VenueMetadataAndHeaders,
        on_exceeded: RateLimitExceededAction::RetryAfterHeaderThenTemporaryBan,
    })
    .expect("individually valid foreign-market rule");
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::InvalidRateLimitModel)
    );

    let mut invalid = capabilities_input();
    invalid.liquidation_completeness = Completeness::NotSupported;
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid
        .rate_limit_model
        .retain(|rule| rule.transport() != RateLimitTransport::Rest);
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );

    let mut invalid = capabilities_input();
    invalid.options_fields = OptionsFields::DELTA;
    assert_eq!(
        ConnectorCapabilities::try_new(invalid),
        Err(CapabilityError::ContradictoryCapability)
    );
}

#[test]
fn validated_capabilities_serialize_every_normative_field() {
    let capabilities =
        ConnectorCapabilities::try_new(capabilities_input()).expect("complete capabilities");
    let object = serde_json::to_value(capabilities)
        .expect("capabilities serialize")
        .as_object()
        .expect("capabilities object")
        .clone();
    let actual = object
        .keys()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let expected = [
        "venue",
        "connector_version",
        "market_types",
        "trade_semantics",
        "book_depths",
        "book_sequence_semantics",
        "checksum_support",
        "stream_completeness",
        "liquidation_completeness",
        "funding_fields",
        "open_interest_fields",
        "options_fields",
        "connection_lifetime",
        "rate_limit_model",
        "snapshot_method",
        "recovery_method",
        "source_timestamp_precision",
        "known_limitations",
        "terms_reference",
        "redistribution_class",
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actual, expected);
    let completeness = object["stream_completeness"]
        .as_object()
        .expect("completeness is a named fixed map");
    assert_eq!(
        completeness
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        [
            "instrument_definitions",
            "trades",
            "top_of_book",
            "book_snapshots",
            "book_deltas",
            "mark_and_index",
            "funding",
            "open_interest",
            "liquidations",
            "option_metadata",
            "option_ticker",
            "option_trades",
            "option_books",
            "venue_status",
        ]
        .into_iter()
        .collect()
    );
}

#[tokio::test]
async fn discrete_commands_are_bounded_fifo_and_not_latest_only() {
    let (control, _interrupt) = CancellationChannel::channel();
    let (mut sender, mut receiver) =
        ConnectorCommandChannel::bounded(2, control).expect("bounded command channel");
    sender
        .send(ConnectorCommand::Resynchronize {
            id: NonZeroU64::new(1).expect("nonzero"),
            reason: ResynchronizationReason::SequenceGap,
        })
        .await
        .expect("first command");
    sender
        .send(ConnectorCommand::RenewConnection {
            id: NonZeroU64::new(2).expect("nonzero"),
        })
        .await
        .expect("second command");

    assert!(matches!(
        receiver.recv().await,
        Ok(Some(ConnectorCommand::Resynchronize { id, .. })) if id.get() == 1
    ));
    assert!(matches!(
        receiver.recv().await,
        Ok(Some(ConnectorCommand::RenewConnection { id })) if id.get() == 2
    ));
}

#[tokio::test]
async fn duplicate_or_out_of_order_command_ids_fail_closed() {
    let (control, _interrupt) = CancellationChannel::channel();
    let (mut sender, mut receiver) =
        ConnectorCommandChannel::bounded(2, control).expect("bounded command channel");
    sender
        .send(ConnectorCommand::Recover {
            id: NonZeroU64::new(2).expect("nonzero"),
        })
        .await
        .expect("first command");
    assert_eq!(
        sender
            .send(ConnectorCommand::Shutdown {
                id: NonZeroU64::new(1).expect("nonzero"),
            })
            .await,
        Err(ChannelError::CommandSequenceRegression)
    );
    assert!(matches!(
        receiver.recv().await,
        Ok(Some(ConnectorCommand::Recover { .. }))
    ));

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut sender, _receiver) =
        ConnectorCommandChannel::bounded(2, control).expect("bounded command channel");
    sender
        .try_send(ConnectorCommand::Recover {
            id: NonZeroU64::new(2).expect("nonzero"),
        })
        .expect("first command");
    sender
        .try_send(ConnectorCommand::RenewConnection {
            id: NonZeroU64::new(3).expect("nonzero"),
        })
        .expect("second command");
    assert_eq!(
        sender.try_send(ConnectorCommand::Quarantine {
            id: NonZeroU64::new(2).expect("historical duplicate"),
            reason: connector_core::QuarantineReason::SupervisorPolicy,
        }),
        Err(ChannelError::CommandSequenceRegression)
    );

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut sender, mut receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("bounded command channel");
    for id in 1..=4_097 {
        sender
            .try_send(ConnectorCommand::Recover {
                id: NonZeroU64::new(id).expect("nonzero"),
            })
            .expect("monotonic command");
        assert!(matches!(
            receiver.recv().await,
            Ok(Some(ConnectorCommand::Recover { id: received })) if received.get() == id
        ));
    }
    assert_eq!(
        sender.try_send(ConnectorCommand::Quarantine {
            id: NonZeroU64::new(1).expect("retired historical duplicate"),
            reason: connector_core::QuarantineReason::SupervisorPolicy,
        }),
        Err(ChannelError::CommandSequenceRegression)
    );
}

struct WalProofFixture {
    _directory: tempfile::TempDir,
    authority: WalAppendAuthority,
    proof: WalAppendProof,
}

fn wal_proof(payload: &[u8]) -> WalProofFixture {
    wal_proof_for_capture(payload, &source(), 3)
}

fn wal_proof_for_capture(
    payload: &[u8],
    stream_source: &SourceId,
    connection_epoch: u64,
) -> WalProofFixture {
    let directory = tempfile::tempdir().expect("temporary WAL");
    let mut segment_id = [0_u8; 16];
    segment_id.copy_from_slice(&blake3::hash(payload).as_bytes()[..16]);
    let metadata = SegmentMetadata::new(
        segment_id,
        1,
        "schema",
        "installation",
        "build",
        vec![
            StreamDescriptor::new(
                7,
                connector_core::wal_stream_source_identity(stream_source),
                "fixture",
            )
            .expect("stream"),
        ],
    )
    .expect("segment metadata");
    let mut writer =
        SegmentedWalWriter::create(directory.path(), metadata, RotationPolicy::default(), 1)
            .expect("writer");
    let authority = writer.append_authority();
    let (proof, compression_job) = writer
        .append(
            RecordMetadata {
                flags: 0,
                stream_id: 7,
                connection_epoch,
                record_sequence: 1,
                receive_wall_time_ns: 2,
                receive_monotonic_time_ns: 2,
            },
            payload,
            2,
            2,
        )
        .expect("durable append")
        .into_parts();
    assert!(compression_job.is_none(), "fixture append does not rotate");
    WalProofFixture {
        _directory: directory,
        authority,
        proof,
    }
}

async fn acknowledged_receipt(
    receipt_source: SourceId,
    epoch: NonZeroU64,
    payload: &[u8],
) -> DurableRawReference {
    let wal = wal_proof_for_capture(payload, &receipt_source, epoch.get());
    let channel = DurableRawCaptureChannel::new(
        wal.authority.clone(),
        receipt_source.clone(),
        NonZeroU32::new(7).expect("stream"),
        1,
        128,
    )
    .expect("bounded raw channel");
    let (client, mut worker) = channel.split();
    let capture = RawCapture::try_new(
        receipt_source,
        NonZeroU32::new(7).expect("stream"),
        epoch,
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        payload.to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let submit = tokio::spawn(async move { client.submit(capture).await });
    let pending = worker.recv().await.expect("worker request");
    pending.acknowledge(wal.proof).expect("acknowledge");
    submit.await.expect("join").expect("receipt")
}

fn event(
    event_source: SourceId,
    epoch: u64,
    raw_payload_hash: [u8; 32],
    book_delta: bool,
) -> EventEnvelope {
    if !book_delta {
        return status_event(event_source, epoch, 1, 2, None, raw_payload_hash);
    }
    let venue = VenueId::new("binance").expect("fixture venue");
    let (instrument_id, snapshot_kind, sequence_number, previous_sequence_number, payload) =
        if book_delta {
            (
                Some(InstrumentId::new(venue.clone(), "BTCUSDT", 1).expect("fixture instrument")),
                SnapshotKind::Delta,
                Some(2),
                Some(1),
                UncheckedEventPayload::BookDelta(BookDelta {
                    bids: Vec::new(),
                    asks: Vec::new(),
                    first_sequence: 2,
                    last_sequence: 2,
                }),
            )
        } else {
            (
                None,
                SnapshotKind::NotApplicable,
                Some(1),
                None,
                UncheckedEventPayload::VenueStatus(VenueStatus {
                    state: VenueState::Operational,
                    observed_at: UnixNanos::new(2),
                    message: None,
                }),
            )
        };
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 1,
            source: event_source,
            venue: Some(venue),
            instrument_id,
            exchange_timestamp: Some(UnixNanos::new(1)),
            exchange_transaction_timestamp: None,
            receive_wall_timestamp: UnixNanos::new(2),
            receive_monotonic_ns: 2,
            normalization_timestamp: UnixNanos::new(3),
            connection_started_at: UnixNanos::new(1),
            sequence_number,
            previous_sequence_number,
            connection_epoch: epoch,
            subscription_epoch: 1,
            snapshot_kind,
            source_checksum: None,
            raw_payload_hash,
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "ingestion-1".to_owned(),
            quality_score_ppm: 1_000_000,
            quality_flags: QualityFlags::NONE,
        },
        payload,
    )
    .expect("fixture event")
}

fn status_event(
    event_source: SourceId,
    epoch: u64,
    subscription_epoch: u64,
    receive_monotonic_ns: u64,
    symbol: Option<&str>,
    raw_payload_hash: [u8; 32],
) -> EventEnvelope {
    let venue = VenueId::new("binance").expect("fixture venue");
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 1,
            source: event_source,
            venue: Some(venue.clone()),
            instrument_id: symbol
                .map(|symbol| InstrumentId::new(venue, symbol, 1).expect("fixture instrument")),
            exchange_timestamp: Some(UnixNanos::new(1)),
            exchange_transaction_timestamp: None,
            receive_wall_timestamp: UnixNanos::new(2),
            receive_monotonic_ns,
            normalization_timestamp: UnixNanos::new(3),
            connection_started_at: UnixNanos::new(1),
            sequence_number: Some(1),
            previous_sequence_number: None,
            connection_epoch: epoch,
            subscription_epoch,
            snapshot_kind: SnapshotKind::NotApplicable,
            source_checksum: None,
            raw_payload_hash,
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "ingestion-1".to_owned(),
            quality_score_ppm: 1_000_000,
            quality_flags: QualityFlags::NONE,
        },
        UncheckedEventPayload::VenueStatus(VenueStatus {
            state: VenueState::Operational,
            observed_at: UnixNanos::new(2),
            message: None,
        }),
    )
    .expect("fixture status event")
}

fn book_event(
    symbol: &str,
    snapshot: bool,
    sequence: u64,
    raw_payload_hash: [u8; 32],
) -> EventEnvelope {
    let venue = VenueId::new("binance").expect("fixture venue");
    let payload = if snapshot {
        UncheckedEventPayload::BookSnapshot(BookSnapshot {
            bids: Vec::new(),
            asks: Vec::new(),
            last_sequence: sequence,
        })
    } else {
        UncheckedEventPayload::BookDelta(BookDelta {
            bids: Vec::new(),
            asks: Vec::new(),
            first_sequence: sequence,
            last_sequence: sequence,
        })
    };
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 1,
            source: source(),
            venue: Some(venue.clone()),
            instrument_id: Some(InstrumentId::new(venue, symbol, 1).expect("fixture instrument")),
            exchange_timestamp: Some(UnixNanos::new(1)),
            exchange_transaction_timestamp: None,
            receive_wall_timestamp: UnixNanos::new(2),
            receive_monotonic_ns: 2,
            normalization_timestamp: UnixNanos::new(3),
            connection_started_at: UnixNanos::new(1),
            sequence_number: Some(sequence),
            previous_sequence_number: if snapshot {
                None
            } else {
                sequence.checked_sub(1)
            },
            connection_epoch: 3,
            subscription_epoch: 1,
            snapshot_kind: if snapshot {
                SnapshotKind::Snapshot
            } else {
                SnapshotKind::Delta
            },
            source_checksum: None,
            raw_payload_hash,
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "ingestion-1".to_owned(),
            quality_score_ppm: 1_000_000,
            quality_flags: QualityFlags::NONE,
        },
        payload,
    )
    .expect("fixture book event")
}

fn product_book_event(
    product_type: ProductType,
    snapshot: bool,
    sequence: u64,
    raw_payload_hash: [u8; 32],
) -> EventEnvelope {
    let venue = VenueId::new("binance").expect("fixture venue");
    let payload = if snapshot {
        UncheckedEventPayload::BookSnapshot(BookSnapshot {
            bids: Vec::new(),
            asks: Vec::new(),
            last_sequence: sequence,
        })
    } else {
        UncheckedEventPayload::BookDelta(BookDelta {
            bids: Vec::new(),
            asks: Vec::new(),
            first_sequence: sequence,
            last_sequence: sequence,
        })
    };
    EventEnvelope::new(
        UncheckedEventMetadata {
            schema_version: 3,
            source: source(),
            venue: Some(venue.clone()),
            instrument_id: Some(
                InstrumentId::new_for_product(venue, "BTCUSDT", product_type, 1)
                    .expect("fixture instrument"),
            ),
            exchange_timestamp: Some(UnixNanos::new(1)),
            exchange_transaction_timestamp: None,
            receive_wall_timestamp: UnixNanos::new(2),
            receive_monotonic_ns: 2,
            normalization_timestamp: UnixNanos::new(3),
            connection_started_at: UnixNanos::new(1),
            sequence_number: Some(sequence),
            previous_sequence_number: if snapshot {
                None
            } else {
                sequence.checked_sub(1)
            },
            connection_epoch: 3,
            subscription_epoch: 1,
            snapshot_kind: if snapshot {
                SnapshotKind::Snapshot
            } else {
                SnapshotKind::Delta
            },
            source_checksum: None,
            raw_payload_hash,
            parser_version: "parser-v1".to_owned(),
            normalizer_version: "normalizer-v1".to_owned(),
            ingestion_instance: "ingestion-1".to_owned(),
            quality_score_ppm: 1_000_000,
            quality_flags: QualityFlags::NONE,
        },
        payload,
    )
    .expect("fixture product-aware book event")
}

#[tokio::test]
async fn raw_capture_is_not_acknowledged_until_the_wal_worker_supplies_a_position() {
    let wal = wal_proof(b"raw");
    let channel = make_raw_channel(wal.authority.clone(), 1, 64).expect("bounded raw channel");
    let (client, mut worker) = channel.split();
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let submit = tokio::spawn(async move { client.submit(capture).await });
    let pending = worker.recv().await.expect("worker request");
    assert!(
        !submit.is_finished(),
        "receipt must wait for durable WAL ack"
    );
    pending
        .acknowledge(wal.proof)
        .expect("worker acknowledgment");
    let receipt = submit
        .await
        .expect("join")
        .expect("durable receipt must arrive");
    assert_eq!(receipt.source(), &source());
    assert_eq!(receipt.payload_hash(), blake3::hash(b"raw").as_bytes());
    assert_eq!(receipt.receive_wall_time(), UnixNanos::new(2));
    assert_eq!(receipt.receive_monotonic_ns(), 2);
}

#[tokio::test]
async fn identical_record_from_a_distinct_wal_cannot_acknowledge_a_pending_capture() {
    let expected = wal_proof(b"raw");
    let channel = make_raw_channel(expected.authority.clone(), 1, 64).expect("bounded raw channel");
    let (client, mut worker) = channel.split();
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let submit = tokio::spawn(async move { client.submit(capture).await });
    let pending = worker.recv().await.expect("worker request");
    let unrelated = wal_proof(b"raw");
    assert_eq!(
        pending.acknowledge(unrelated.proof),
        Err(RawCaptureError::WalProofMismatch)
    );
    assert_eq!(
        submit.await.expect("join"),
        Err(RawCaptureError::WalProofMismatch)
    );
}

#[tokio::test]
async fn wal_stream_source_cannot_acknowledge_capture_for_another_source() {
    for foreign in [
        SourceId::new(SourceKind::Exchange, "binance", 2)
            .expect("same name with another generation"),
        SourceId::new(SourceKind::Chain, "binance", 1).expect("same name with another kind"),
    ] {
        let wal = wal_proof(b"raw");
        let channel = DurableRawCaptureChannel::new(
            wal.authority.clone(),
            foreign.clone(),
            NonZeroU32::new(7).expect("stream"),
            2,
            64,
        )
        .expect("bounded raw channel");
        let (client, mut worker) = channel.split();
        let capture = RawCapture::try_new(
            foreign,
            NonZeroU32::new(7).expect("stream"),
            NonZeroU64::new(3).expect("epoch"),
            NonZeroU64::new(1).expect("sequence"),
            UnixNanos::new(2),
            2,
            b"raw".to_vec().into_boxed_slice(),
        )
        .expect("capture");
        let submit = tokio::spawn(async move { client.submit(capture).await });
        let pending = worker.recv().await.expect("worker request");
        assert_eq!(
            pending.acknowledge(wal.proof),
            Err(RawCaptureError::WalProofMismatch)
        );
        assert_eq!(
            submit.await.expect("join"),
            Err(RawCaptureError::WalProofMismatch)
        );
    }
}

#[tokio::test]
async fn append_proof_from_a_removed_wal_cannot_be_acknowledged() {
    let wal = wal_proof(b"raw");
    let channel = make_raw_channel(wal.authority.clone(), 1, 64).expect("bounded raw channel");
    let (client, mut worker) = channel.split();
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let submit = tokio::spawn(async move { client.submit(capture).await });
    let pending = worker.recv().await.expect("worker request");
    let WalProofFixture {
        _directory,
        authority: _,
        proof,
    } = wal;
    _directory.close().expect("remove WAL directory");
    assert_eq!(
        pending.acknowledge(proof),
        Err(RawCaptureError::WalProofMismatch)
    );
    assert_eq!(
        submit.await.expect("join"),
        Err(RawCaptureError::WalProofMismatch)
    );
}

#[tokio::test]
async fn raw_capture_enforces_item_and_byte_budgets_independently() {
    let wal_authority = wal_proof(b"capacity-test").authority;
    assert!(make_raw_channel(wal_authority.clone(), 0, 16).is_err());
    assert!(make_raw_channel(wal_authority.clone(), 1, 0).is_err());

    let channel = make_raw_channel(wal_authority.clone(), 2, 3).expect("bounded raw channel");
    let (client, _worker) = channel.split();
    let first = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"abc".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let second = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(2).expect("sequence"),
        UnixNanos::new(3),
        3,
        b"d".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let pending = client.try_submit(first).expect("first item fits");
    assert!(client.try_submit(second).is_err(), "byte budget must bind");
    drop(pending);

    let too_small = make_raw_channel(wal_authority, 1, 2).expect("bounded raw channel");
    let (client, _worker) = too_small.split();
    let oversized_for_budget = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(3).expect("sequence"),
        UnixNanos::new(4),
        4,
        b"abc".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    assert_eq!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            client.submit(oversized_for_budget)
        )
        .await
        .expect("oversized submission must not wait forever"),
        Err(RawCaptureError::CapacityRejected)
    );
}

#[test]
fn bounded_channel_constructors_reject_runtime_oversize_without_panicking() {
    let oversized = tokio::sync::Semaphore::MAX_PERMITS
        .checked_add(1)
        .expect("runtime maximum fits usize");
    let wal_authority = wal_proof(b"oversize-capacity").authority;
    assert!(make_raw_channel(wal_authority, oversized, 1).is_err());
    assert!(BoundedNormalizedChannel::new(oversized, 1).is_err());
    assert!(LifecycleChannel::bounded(oversized).is_err());
    let (control, _interrupt) = CancellationChannel::channel();
    assert!(ConnectorCommandChannel::bounded(oversized, control).is_err());
}

#[test]
fn raw_capture_rejects_unbounded_payloads_before_parsing_and_redacts_debug() {
    assert_eq!(
        RawCapture::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            NonZeroU64::new(3).expect("epoch"),
            NonZeroU64::new(1).expect("sequence"),
            UnixNanos::new(2),
            2,
            Box::new([]),
        ),
        Err(RawCaptureError::InvalidPayloadSize)
    );
    assert_eq!(
        RawCapture::try_new(
            source(),
            NonZeroU32::new(7).expect("stream"),
            NonZeroU64::new(3).expect("epoch"),
            NonZeroU64::new(1).expect("sequence"),
            UnixNanos::new(2),
            2,
            vec![0; MAX_RAW_CAPTURE_BYTES + 1].into_boxed_slice(),
        ),
        Err(RawCaptureError::InvalidPayloadSize)
    );
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"secret-provider-body".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    assert!(!format!("{capture:?}").contains("secret-provider-body"));
}

#[tokio::test]
async fn wal_rejection_produces_no_durable_reference_or_normalized_event() {
    let wal_authority = wal_proof(b"rejection-test").authority;
    let raw_channel = make_raw_channel(wal_authority, 1, 64).expect("bounded raw channel");
    let (client, mut worker) = raw_channel.split();
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let submit = tokio::spawn(async move { client.submit(capture).await });
    worker
        .recv()
        .await
        .expect("worker request")
        .reject(WalRejectionReason::StorageUnavailable)
        .expect("rejection delivery");
    assert_eq!(
        submit.await.expect("join"),
        Err(RawCaptureError::WalRejected(
            WalRejectionReason::StorageUnavailable
        ))
    );
}

async fn mapped_wal_rejection(reason: WalRejectionReason) -> ConnectorTermination {
    let epoch = NonZeroU64::new(3).expect("epoch");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel =
        make_raw_channel(wal_proof(b"typed-wal-error").authority, 1, 64).expect("raw");
    let (raw_client, mut worker) = raw_channel.split();
    let normalized = BoundedNormalizedChannel::new(1, 16_384).expect("normalized");
    let (normalized_sink, _normalized_receiver) = normalized.split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        epoch,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        epoch,
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let task = tokio::spawn(async move { context.capture_raw(capture).await });
    worker
        .recv()
        .await
        .expect("worker request")
        .reject(reason)
        .expect("deliver rejection");
    task.await
        .expect("join")
        .expect_err("WAL rejection terminates connector")
}

#[tokio::test]
async fn wal_rejection_causes_preserve_supervisor_semantics() {
    assert_eq!(
        mapped_wal_rejection(WalRejectionReason::CapacityExhausted).await,
        ConnectorTermination::CapacityRejected(CapacityRejection::RawCapture)
    );
    for reason in [
        WalRejectionReason::StorageUnavailable,
        WalRejectionReason::DirectoryIdentityViolation,
        WalRejectionReason::Corruption,
    ] {
        assert_eq!(
            mapped_wal_rejection(reason).await,
            ConnectorTermination::LocalWalFailure(reason)
        );
    }
    assert_eq!(
        mapped_wal_rejection(WalRejectionReason::CaptureContractMismatch).await,
        ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration)
    );
    assert_eq!(
        mapped_wal_rejection(WalRejectionReason::SourceIntegrityViolation).await,
        ConnectorTermination::Quarantined(connector_core::QuarantineReason::IntegrityViolation)
    );
}

#[tokio::test]
async fn normalized_output_requires_matching_raw_source_epoch_and_hash() {
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"raw").await;
    let matching = event(source(), 3, *blake3::hash(b"raw").as_bytes(), false);
    NormalizedOutput::try_new(receipt.clone(), matching).expect("matching linkage");

    let other_source = SourceId::new(SourceKind::Exchange, "bybit", 1).expect("other source");
    assert_eq!(
        NormalizedOutput::try_new(
            receipt.clone(),
            event(other_source, 3, *blake3::hash(b"raw").as_bytes(), false)
        ),
        Err(ChannelError::RawSourceMismatch)
    );
    assert_eq!(
        NormalizedOutput::try_new(
            receipt.clone(),
            event(source(), 4, *blake3::hash(b"raw").as_bytes(), false)
        ),
        Err(ChannelError::RawConnectionEpochMismatch)
    );
    assert_eq!(
        NormalizedOutput::try_new(receipt, event(source(), 3, [8; 32], false)),
        Err(ChannelError::RawPayloadHashMismatch)
    );
}

#[tokio::test]
async fn parse_rejection_retains_only_typed_reason_and_durable_link() {
    let receipt = acknowledged_receipt(
        source(),
        NonZeroU64::new(3).expect("epoch"),
        b"secret-provider-body",
    )
    .await;
    let rejection = ParseRejection::new(receipt.clone(), ParseRejectionReason::SchemaMismatch);
    assert_eq!(rejection.raw(), &receipt);
    assert_eq!(rejection.reason(), ParseRejectionReason::SchemaMismatch);
    let debug = format!("{rejection:?}");
    assert!(!debug.contains("secret-provider-body"));
}

#[tokio::test]
async fn required_book_delta_overflow_stays_invalid_until_resynchronized() {
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"raw").await;
    let venue = VenueId::new("binance").expect("venue");
    let channel = BoundedNormalizedChannel::with_book_streams(
        1,
        16_384,
        std::num::NonZeroUsize::new(2).expect("nonzero"),
        ["BTCUSDT", "ETHUSDT"].map(|symbol| {
            BookStreamKey::new(
                source(),
                InstrumentId::new(venue.clone(), symbol, 1).expect("instrument"),
                NonZeroU64::new(1).expect("subscription epoch"),
            )
        }),
    )
    .expect("bounded output");
    let (sink, mut receiver) = channel.split();
    let btc_delta = NormalizedOutput::try_new(
        receipt.clone(),
        book_event("BTCUSDT", false, 2, *blake3::hash(b"raw").as_bytes()),
    )
    .expect("delta output");
    let eth_delta = NormalizedOutput::try_new(
        receipt.clone(),
        book_event("ETHUSDT", false, 2, *blake3::hash(b"raw").as_bytes()),
    )
    .expect("other delta output");
    sink.try_send(eth_delta.clone())
        .expect("required lane is filled independently");
    assert_eq!(
        sink.try_send(btc_delta.clone()),
        Err(ChannelError::ResynchronizationRequired)
    );
    receiver.recv().await.expect("drain first item");
    sink.try_send(eth_delta)
        .expect("another instrument remains independent");
    receiver.recv().await.expect("drain other instrument");
    assert_eq!(
        sink.try_send(btc_delta.clone()),
        Err(ChannelError::ResynchronizationRequired),
        "later deltas must remain blocked"
    );

    let unrelated_snapshot = NormalizedOutput::try_new(
        receipt.clone(),
        book_event("ETHUSDT", true, 2, *blake3::hash(b"raw").as_bytes()),
    )
    .expect("unrelated snapshot");
    sink.try_send(unrelated_snapshot)
        .expect("unrelated snapshot is delivered normally");
    let unrelated_snapshot = receiver
        .recv_for_apply()
        .await
        .expect("unrelated snapshot")
        .confirm_snapshot_applied()
        .expect("snapshot token");
    assert_eq!(
        receiver.accept_resynchronizing_snapshot(unrelated_snapshot),
        Err(ChannelError::SnapshotDoesNotSatisfyInvalidation)
    );
    let stale_snapshot = NormalizedOutput::try_new(
        receipt.clone(),
        book_event("BTCUSDT", true, 1, *blake3::hash(b"raw").as_bytes()),
    )
    .expect("stale snapshot");
    sink.try_send(stale_snapshot)
        .expect("stale snapshot is delivered for validation");
    let stale_snapshot = receiver
        .recv_for_apply()
        .await
        .expect("stale snapshot")
        .confirm_snapshot_applied()
        .expect("snapshot token");
    assert_eq!(
        receiver.accept_resynchronizing_snapshot(stale_snapshot),
        Err(ChannelError::SnapshotDoesNotSatisfyInvalidation)
    );

    let fresh_snapshot = NormalizedOutput::try_new(
        receipt,
        book_event("BTCUSDT", true, 2, *blake3::hash(b"raw").as_bytes()),
    )
    .expect("fresh snapshot");
    sink.try_send(fresh_snapshot)
        .expect("matching snapshot may pass while invalidated");
    let applied_snapshot = receiver
        .recv_for_apply()
        .await
        .expect("matching snapshot")
        .confirm_snapshot_applied()
        .expect("snapshot token");
    receiver
        .accept_resynchronizing_snapshot(applied_snapshot)
        .expect("matching applied snapshot clears only this stream");
    sink.try_send(btc_delta)
        .expect("fresh snapshot re-enables deltas");
}

#[tokio::test]
async fn same_symbol_spot_and_perpetual_book_streams_do_not_share_loss_state() {
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"raw").await;
    let venue = VenueId::new("binance").expect("venue");
    let subscription = NonZeroU64::new(1).expect("subscription");
    let channel = BoundedNormalizedChannel::with_book_streams(
        1,
        16_384,
        std::num::NonZeroUsize::new(2).expect("nonzero"),
        [ProductType::Spot, ProductType::Perpetual].map(|product| {
            BookStreamKey::new(
                source(),
                InstrumentId::new_for_product(venue.clone(), "BTCUSDT", product, 1)
                    .expect("instrument"),
                subscription,
            )
        }),
    )
    .expect("product-aware channel");
    let (sink, mut receiver) = channel.split();
    let raw_hash = *blake3::hash(b"raw").as_bytes();
    let spot_delta = NormalizedOutput::try_new(
        receipt.clone(),
        product_book_event(ProductType::Spot, false, 2, raw_hash),
    )
    .expect("spot delta");
    let perpetual_delta = NormalizedOutput::try_new(
        receipt,
        product_book_event(ProductType::Perpetual, false, 2, raw_hash),
    )
    .expect("perpetual delta");

    sink.try_send(spot_delta.clone())
        .expect("fill required lane");
    assert_eq!(
        sink.try_send(spot_delta),
        Err(ChannelError::ResynchronizationRequired)
    );
    receiver.recv().await.expect("drain spot event");
    sink.try_send(perpetual_delta)
        .expect("perpetual stream remains independently trusted");
}

#[tokio::test]
async fn snapshot_from_another_receiver_cannot_clear_invalidation() {
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"raw").await;
    let venue = VenueId::new("binance").expect("venue");
    let key = BookStreamKey::new(
        source(),
        InstrumentId::new(venue, "BTCUSDT", 1).expect("instrument"),
        NonZeroU64::new(1).expect("subscription epoch"),
    );
    let first = BoundedNormalizedChannel::with_book_streams(
        1,
        16_384,
        std::num::NonZeroUsize::new(1).expect("nonzero"),
        [key.clone()],
    )
    .expect("first channel");
    let second = BoundedNormalizedChannel::with_book_streams(
        1,
        16_384,
        std::num::NonZeroUsize::new(1).expect("nonzero"),
        [key],
    )
    .expect("second channel");
    let (first_sink, first_receiver) = first.split();
    let (second_sink, mut second_receiver) = second.split();
    let delta = NormalizedOutput::try_new(
        receipt.clone(),
        book_event("BTCUSDT", false, 2, *blake3::hash(b"raw").as_bytes()),
    )
    .expect("delta");
    first_sink.try_send(delta.clone()).expect("fill lane");
    assert_eq!(
        first_sink.try_send(delta.clone()),
        Err(ChannelError::ResynchronizationRequired)
    );

    let snapshot = NormalizedOutput::try_new(
        receipt,
        book_event("BTCUSDT", true, 2, *blake3::hash(b"raw").as_bytes()),
    )
    .expect("snapshot");
    second_sink.try_send(snapshot).expect("deliver on second");
    let applied = second_receiver
        .recv_for_apply()
        .await
        .expect("delivered")
        .confirm_snapshot_applied()
        .expect("snapshot");
    assert_eq!(
        first_receiver.accept_resynchronizing_snapshot(applied),
        Err(ChannelError::SnapshotDoesNotSatisfyInvalidation)
    );
    assert_eq!(
        first_sink.try_send(delta),
        Err(ChannelError::ResynchronizationRequired)
    );
}

#[tokio::test]
async fn priority_lanes_isolate_integrity_and_loss_requires_a_recorded_policy() {
    assert_eq!(
        EventPriority::for_event(EventType::BookSnapshot),
        EventPriority::Integrity
    );
    assert_eq!(
        EventPriority::for_event(EventType::Trade),
        EventPriority::MarketCore
    );
    assert_eq!(
        EventPriority::for_event(EventType::FundingObservation),
        EventPriority::DerivativesState
    );
    assert_eq!(
        EventPriority::for_event(EventType::VenueStatus),
        EventPriority::Auxiliary
    );
    assert_eq!(
        EventPriority::for_event(EventType::AlertEvent),
        EventPriority::UiAggregate
    );

    let policy = SamplingPolicy::SampleEvery {
        interval_ms: NonZeroU32::new(1_000).expect("interval"),
    };
    assert_eq!(
        LossPolicyRegistry::try_new([(EventType::BookDelta, policy)]),
        Err(ChannelError::InvalidLossPolicy)
    );
    let policies =
        LossPolicyRegistry::try_new([(EventType::VenueStatus, policy)]).expect("loss policy");
    let venue = VenueId::new("binance").expect("venue");
    let channel = BoundedNormalizedChannel::with_book_streams_and_loss_policies(
        1,
        16_384,
        std::num::NonZeroUsize::new(1).expect("nonzero"),
        [BookStreamKey::new(
            source(),
            InstrumentId::new(venue, "BTCUSDT", 1).expect("instrument"),
            NonZeroU64::new(1).expect("subscription epoch"),
        )],
        policies,
    )
    .expect("prioritized channel");
    let (sink, mut receiver) = channel.split();
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"raw").await;
    let status = NormalizedOutput::try_new(
        receipt.clone(),
        event(source(), 3, *blake3::hash(b"raw").as_bytes(), false),
    )
    .expect("status");
    let delta = NormalizedOutput::try_new(
        receipt,
        book_event("BTCUSDT", false, 2, *blake3::hash(b"raw").as_bytes()),
    )
    .expect("book delta");

    assert_eq!(
        sink.try_send_lossy(status.clone(), policy),
        Ok(DeliveryOutcome::Queued)
    );
    sink.try_send(delta.clone())
        .expect("integrity lane remains independently available");
    assert_eq!(
        receiver.recv().await.expect("priority output").event(),
        delta.event(),
        "ready integrity traffic must be received before auxiliary traffic"
    );
    let dropped = sink
        .try_send_lossy(status, policy)
        .expect("declared loss policy");
    let DeliveryOutcome::Dropped(dropped) = dropped else {
        panic!("second status must be sampled");
    };
    assert_eq!(dropped.audit().stream.event_type(), EventType::VenueStatus);
    assert_eq!(dropped.audit().policy, policy);
    assert_eq!(dropped.audit().sequence_number, Some(1));
    assert_eq!(dropped.audit().reason, LossReason::SamplingWindow);
    assert_eq!(
        receiver
            .recv()
            .await
            .expect("queued auxiliary output")
            .event()
            .event_type(),
        EventType::VenueStatus
    );
}

#[tokio::test]
async fn lossy_capacity_outcomes_are_auditable_and_retain_the_original_output() {
    let policy = SamplingPolicy::SampleEvery {
        interval_ms: NonZeroU32::new(1_000).expect("interval"),
    };
    let policies =
        LossPolicyRegistry::try_new([(EventType::VenueStatus, policy)]).expect("loss policy");
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"loss").await;
    let first = NormalizedOutput::try_new(
        receipt.clone(),
        status_event(
            source(),
            3,
            1,
            2,
            Some("BTCUSDT"),
            *blake3::hash(b"loss").as_bytes(),
        ),
    )
    .expect("first");
    let second = NormalizedOutput::try_new(
        receipt.clone(),
        status_event(
            source(),
            3,
            1,
            2,
            Some("ETHUSDT"),
            *blake3::hash(b"loss").as_bytes(),
        ),
    )
    .expect("second");
    let (sink, mut receiver) =
        BoundedNormalizedChannel::with_loss_policies(1, 16_384, policies.clone())
            .expect("item-bounded channel")
            .split();
    assert_eq!(
        sink.try_send_lossy(first, policy),
        Ok(DeliveryOutcome::Queued)
    );
    let DeliveryOutcome::Dropped(dropped) = sink
        .try_send_lossy(second.clone(), policy)
        .expect("auditable item overflow")
    else {
        panic!("item overflow must be returned as a drop");
    };
    assert_eq!(dropped.audit().reason, LossReason::ItemCapacity);
    let (returned, audit) = dropped.into_parts();
    assert_eq!(returned, second);
    assert_eq!(audit.stream.event_type(), EventType::VenueStatus);
    receiver.recv().await.expect("drain first");

    let oversized = NormalizedOutput::try_new(
        receipt.clone(),
        status_event(
            source(),
            3,
            1,
            2,
            Some("SOLUSDT"),
            *blake3::hash(b"loss").as_bytes(),
        ),
    )
    .expect("oversized");
    let (sink, _receiver) = BoundedNormalizedChannel::with_loss_policies(1, 1, policies.clone())
        .expect("byte-bounded channel")
        .split();
    let DeliveryOutcome::Dropped(dropped) = sink
        .try_send_lossy(oversized.clone(), policy)
        .expect("auditable byte overflow")
    else {
        panic!("byte overflow must be returned as a drop");
    };
    assert_eq!(dropped.audit().reason, LossReason::ByteCapacity);
    assert_eq!(dropped.output(), &oversized);

    let closed = NormalizedOutput::try_new(
        receipt,
        status_event(
            source(),
            3,
            1,
            2,
            Some("XRPUSDT"),
            *blake3::hash(b"loss").as_bytes(),
        ),
    )
    .expect("closed");
    let (sink, receiver) = BoundedNormalizedChannel::with_loss_policies(1, 16_384, policies)
        .expect("closed channel")
        .split();
    drop(receiver);
    let error = sink
        .try_send_lossy(closed.clone(), policy)
        .expect_err("closed audit path must return the original");
    assert_eq!(error.error(), ChannelError::ReceiverClosed);
    assert_eq!(error.output(), &closed);
}

#[tokio::test]
async fn sampling_is_scoped_per_stream_and_has_a_hard_registry_bound() {
    let policy = SamplingPolicy::SampleEvery {
        interval_ms: NonZeroU32::new(1_000).expect("interval"),
    };
    let policies =
        LossPolicyRegistry::try_new([(EventType::VenueStatus, policy)]).expect("loss policy");
    let receipt =
        acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"selectors").await;
    let (sink, mut receiver) =
        BoundedNormalizedChannel::with_loss_policies(2, 16_384, policies.clone())
            .expect("selector channel")
            .split();
    for symbol in ["BTCUSDT", "ETHUSDT"] {
        let output = NormalizedOutput::try_new(
            receipt.clone(),
            status_event(
                source(),
                3,
                1,
                2,
                Some(symbol),
                *blake3::hash(b"selectors").as_bytes(),
            ),
        )
        .expect("selector output");
        assert_eq!(
            sink.try_send_lossy(output, policy),
            Ok(DeliveryOutcome::Queued),
            "another instrument must not inherit the sampling window"
        );
    }
    receiver.recv().await.expect("BTC");
    receiver.recv().await.expect("ETH");

    let next_epoch = NonZeroU64::new(4).expect("epoch");
    let next_receipt = acknowledged_receipt(source(), next_epoch, b"next-selector").await;
    let next_output = NormalizedOutput::try_new(
        next_receipt,
        status_event(
            source(),
            next_epoch.get(),
            1,
            2,
            Some("BTCUSDT"),
            *blake3::hash(b"next-selector").as_bytes(),
        ),
    )
    .expect("next epoch");
    assert_eq!(
        sink.try_send_lossy(next_output, policy),
        Ok(DeliveryOutcome::Queued),
        "a new connection epoch must start a new sampling window"
    );
    receiver.recv().await.expect("next epoch");

    let (bounded_sink, mut bounded_receiver) =
        BoundedNormalizedChannel::with_loss_policies(1, 16_384, policies)
            .expect("bounded selector registry")
            .split();
    for index in 0..connector_core::MAX_LOSS_SAMPLING_STREAMS {
        let symbol = format!("S{index}");
        let output = NormalizedOutput::try_new(
            receipt.clone(),
            status_event(
                source(),
                3,
                1,
                2,
                Some(&symbol),
                *blake3::hash(b"selectors").as_bytes(),
            ),
        )
        .expect("bounded selector");
        assert_eq!(
            bounded_sink.try_send_lossy(output, policy),
            Ok(DeliveryOutcome::Queued)
        );
        bounded_receiver.recv().await.expect("drain selector");
    }
    let overflow = NormalizedOutput::try_new(
        receipt,
        status_event(
            source(),
            3,
            1,
            2,
            Some("OVERFLOW"),
            *blake3::hash(b"selectors").as_bytes(),
        ),
    )
    .expect("overflow selector");
    let error = bounded_sink
        .try_send_lossy(overflow.clone(), policy)
        .expect_err("selector registry must be bounded");
    assert_eq!(
        error.error(),
        ChannelError::SamplingRegistryCapacityExceeded
    );
    assert_eq!(error.output(), &overflow);
}

#[tokio::test]
async fn supervisor_and_output_closure_are_typed_terminations() {
    let (control, _interrupt) = CancellationChannel::channel();
    let (command_sender, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    drop(command_sender);
    let wal_authority = wal_proof(b"closure-test").authority;
    let raw_channel = make_raw_channel(wal_authority, 1, 64).expect("raw channel");
    let (raw_client, _raw_worker) = raw_channel.split();
    let normalized_channel = BoundedNormalizedChannel::new(1, 16_384).expect("normalized");
    let (normalized_sink, _normalized_receiver) = normalized_channel.split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        NonZeroU64::new(3).expect("epoch"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    assert_eq!(
        context.next_command().await,
        Err(ConnectorTermination::SupervisorLost)
    );

    let (control, _interrupt) = CancellationChannel::channel();
    let (_live_command_sender, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("live commands");
    let wal_authority = wal_proof(b"output-closure-test").authority;
    let raw_channel = make_raw_channel(wal_authority, 1, 64).expect("raw channel");
    let (raw_client, _raw_worker) = raw_channel.split();
    let normalized_channel = BoundedNormalizedChannel::new(1, 16_384).expect("normalized");
    let (normalized_sink, normalized_receiver) = normalized_channel.split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        NonZeroU64::new(3).expect("epoch"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    drop(normalized_receiver);
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"raw").await;
    let output = NormalizedOutput::try_new(
        receipt,
        event(source(), 3, *blake3::hash(b"raw").as_bytes(), false),
    )
    .expect("output");
    assert_eq!(
        context.publish_normalized(output).await,
        Err(ConnectorTermination::LocalOutputFailed)
    );
}

#[tokio::test]
async fn raw_worker_loss_is_not_reported_as_supervisor_loss() {
    let epoch = NonZeroU64::new(3).expect("epoch");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel =
        make_raw_channel(wal_proof(b"worker-loss").authority, 1, 64).expect("raw channel");
    let (raw_client, raw_worker) = raw_channel.split();
    drop(raw_worker);
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(1, 16_384)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        epoch,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        epoch,
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    assert_eq!(
        context.capture_raw(capture).await,
        Err(ConnectorTermination::LocalWalWorkerLost)
    );
}

#[tokio::test]
async fn abandoned_pending_wal_receipt_is_a_local_wal_failure() {
    let epoch = NonZeroU64::new(3).expect("epoch");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel =
        make_raw_channel(wal_proof(b"abandoned-receipt").authority, 1, 64).expect("raw channel");
    let (raw_client, mut raw_worker) = raw_channel.split();
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(1, 16_384)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        epoch,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        epoch,
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let task = tokio::spawn(async move { context.capture_raw(capture).await });
    let pending = raw_worker.recv().await.expect("pending receipt");
    drop(pending);
    assert_eq!(
        task.await.expect("join"),
        Err(ConnectorTermination::LocalWalWorkerLost)
    );
}

#[tokio::test]
async fn cancellation_remains_responsive_while_output_is_backpressured() {
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"raw").await;
    let output = NormalizedOutput::try_new(
        receipt,
        event(source(), 3, *blake3::hash(b"raw").as_bytes(), false),
    )
    .expect("output");
    let (cancellation_sender, _interrupt) = CancellationChannel::channel();
    let (_command_sender, command_receiver) =
        ConnectorCommandChannel::bounded(1, cancellation_sender.clone()).expect("commands");
    let wal_authority = wal_proof(b"cancellation-test").authority;
    let raw_channel = make_raw_channel(wal_authority, 1, 64).expect("raw channel");
    let (raw_client, _raw_worker) = raw_channel.split();
    let normalized_channel = BoundedNormalizedChannel::new(1, 16_384).expect("normalized");
    let (normalized_sink, _normalized_receiver) = normalized_channel.split();
    normalized_sink
        .try_send(output.clone())
        .expect("fill output channel");
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        NonZeroU64::new(3).expect("epoch"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let publish = tokio::spawn(async move { context.publish_normalized(output).await });
    tokio::task::yield_now().await;
    assert!(!publish.is_finished(), "publication must be backpressured");
    assert!(cancellation_sender.cancel());
    assert_eq!(
        publish.await.expect("join"),
        Err(ConnectorTermination::Cancelled)
    );
}

#[tokio::test]
async fn shutdown_interrupts_every_blocked_context_path_with_its_command_id() {
    let epoch = NonZeroU64::new(3).expect("epoch");
    let receipt = acknowledged_receipt(source(), epoch, b"raw").await;
    let output = NormalizedOutput::try_new(
        receipt,
        event(source(), 3, *blake3::hash(b"raw").as_bytes(), false),
    )
    .expect("output");

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel =
        make_raw_channel(wal_proof(b"shutdown-normalized").authority, 1, 64).expect("raw");
    let (raw_client, _raw_worker) = raw_channel.split();
    let normalized = BoundedNormalizedChannel::new(1, 16_384).expect("normalized");
    let (normalized_sink, _normalized_receiver) = normalized.split();
    normalized_sink
        .try_send(output.clone())
        .expect("fill auxiliary lane");
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        epoch,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let blocked = tokio::spawn(async move { context.publish_normalized(output).await });
    tokio::task::yield_now().await;
    commands
        .send(ConnectorCommand::Shutdown {
            id: NonZeroU64::new(41).expect("id"),
        })
        .await
        .expect("terminal bypass");
    assert_eq!(
        blocked.await.expect("join"),
        Err(ConnectorTermination::RequestedShutdown {
            command_id: NonZeroU64::new(41).expect("id")
        })
    );

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel = make_raw_channel(wal_proof(b"shutdown-raw").authority, 1, 64).expect("raw");
    let (raw_client, mut raw_worker) = raw_channel.split();
    let normalized = BoundedNormalizedChannel::new(1, 16_384).expect("normalized");
    let (normalized_sink, _normalized_receiver) = normalized.split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        epoch,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        epoch,
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    let blocked = tokio::spawn(async move { context.capture_raw(capture).await });
    let pending = raw_worker.recv().await.expect("capture reached worker");
    commands
        .send(ConnectorCommand::Shutdown {
            id: NonZeroU64::new(42).expect("id"),
        })
        .await
        .expect("terminal bypass");
    assert_eq!(
        blocked.await.expect("join"),
        Err(ConnectorTermination::RequestedShutdown {
            command_id: NonZeroU64::new(42).expect("id")
        })
    );
    drop(pending);

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel =
        make_raw_channel(wal_proof(b"shutdown-lifecycle").authority, 1, 64).expect("raw");
    let (raw_client, _raw_worker) = raw_channel.split();
    let normalized = BoundedNormalizedChannel::new(1, 16_384).expect("normalized");
    let (normalized_sink, _normalized_receiver) = normalized.split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut filler = LifecycleTracker::new(source(), epoch);
    let prepared = filler
        .prepare(
            ConnectorState::Connecting,
            LifecycleCause::Startup,
            UnixNanos::new(1),
        )
        .expect("prepared");
    lifecycle_sink
        .send(prepared.event().clone())
        .await
        .expect("fill lifecycle lane");
    filler.commit(prepared).expect("commit filler");
    let mut context = ConnectorContext::new(
        source(),
        epoch,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let blocked = tokio::spawn(async move {
        let result = context
            .publish_lifecycle(
                ConnectorState::Connecting,
                LifecycleCause::Startup,
                UnixNanos::new(2),
            )
            .await;
        (result, context.lifecycle_state())
    });
    tokio::task::yield_now().await;
    commands
        .send(ConnectorCommand::Shutdown {
            id: NonZeroU64::new(43).expect("id"),
        })
        .await
        .expect("terminal bypass");
    assert_eq!(
        blocked.await.expect("join"),
        (
            Err(ConnectorTermination::RequestedShutdown {
                command_id: NonZeroU64::new(43).expect("id")
            }),
            ConnectorState::Stopped
        ),
        "interrupted lifecycle publication must not advance tracker state"
    );
}

#[tokio::test]
async fn terminal_shutdown_bypasses_a_full_fifo_and_first_interrupt_wins() {
    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, mut receiver) =
        ConnectorCommandChannel::bounded(1, control.clone()).expect("commands");
    commands
        .try_send(ConnectorCommand::Recover {
            id: NonZeroU64::new(1).expect("id"),
        })
        .expect("fill ordinary FIFO");
    commands
        .try_send(ConnectorCommand::Shutdown {
            id: NonZeroU64::new(2).expect("id"),
        })
        .expect("terminal command bypasses FIFO capacity");
    assert_eq!(
        receiver.recv().await,
        Ok(Some(ConnectorCommand::Shutdown {
            id: NonZeroU64::new(2).expect("id")
        }))
    );

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, _receiver) =
        ConnectorCommandChannel::bounded(1, control.clone()).expect("commands");
    assert!(control.cancel());
    assert_eq!(
        commands.try_send(ConnectorCommand::Shutdown {
            id: NonZeroU64::new(1).expect("id")
        }),
        Err(ChannelError::AlreadyTerminated)
    );

    let (control, _interrupt) = CancellationChannel::channel();
    let (_commands, _receiver) =
        ConnectorCommandChannel::bounded(1, control.clone()).expect("first attachment");
    assert!(matches!(
        ConnectorCommandChannel::bounded(1, control),
        Err(ChannelError::ControlAlreadyAttached)
    ));
}

#[tokio::test]
async fn quarantine_is_an_urgent_first_wins_terminal_interrupt() {
    let quarantine = ConnectorCommand::Quarantine {
        id: NonZeroU64::new(2).expect("id"),
        reason: connector_core::QuarantineReason::SupervisorPolicy,
    };
    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, mut receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    commands
        .try_send(ConnectorCommand::Recover {
            id: NonZeroU64::new(1).expect("id"),
        })
        .expect("fill FIFO");
    commands
        .try_send(quarantine)
        .expect("quarantine bypasses FIFO");
    assert_eq!(receiver.recv().await, Ok(Some(quarantine)));

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel =
        make_raw_channel(wal_proof(b"quarantine-command").authority, 1, 64).expect("raw");
    let (raw_client, _raw_worker) = raw_channel.split();
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(1, 16_384)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        NonZeroU64::new(3).expect("epoch"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    commands.send(quarantine).await.expect("quarantine");
    assert_eq!(
        context.next_command().await,
        Err(ConnectorTermination::SupervisorQuarantine {
            command_id: NonZeroU64::new(2).expect("id"),
            reason: connector_core::QuarantineReason::SupervisorPolicy,
        })
    );

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, _receiver) =
        ConnectorCommandChannel::bounded(1, control.clone()).expect("commands");
    assert!(control.cancel());
    assert_eq!(
        commands.try_send(quarantine),
        Err(ChannelError::AlreadyTerminated)
    );

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, _receiver) =
        ConnectorCommandChannel::bounded(1, control.clone()).expect("commands");
    commands.try_send(quarantine).expect("quarantine wins");
    assert!(!control.cancel());
    assert_eq!(
        commands.try_send(ConnectorCommand::Shutdown {
            id: NonZeroU64::new(3).expect("id")
        }),
        Err(ChannelError::AlreadyTerminated)
    );

    let (control, _interrupt) = CancellationChannel::channel();
    let (mut commands, _receiver) = ConnectorCommandChannel::bounded(2, control).expect("commands");
    commands
        .try_send(ConnectorCommand::Recover {
            id: NonZeroU64::new(3).expect("id"),
        })
        .expect("ordinary command");
    assert_eq!(
        commands.try_send(ConnectorCommand::Quarantine {
            id: NonZeroU64::new(3).expect("duplicate id"),
            reason: connector_core::QuarantineReason::SupervisorPolicy,
        }),
        Err(ChannelError::CommandSequenceRegression)
    );

    for _ in 0..128 {
        let (control, _interrupt) = CancellationChannel::channel();
        let (commands, mut receiver) =
            ConnectorCommandChannel::bounded(2, control).expect("commands");
        let commands = std::sync::Arc::new(tokio::sync::Mutex::new(commands));
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let ordinary_commands = std::sync::Arc::clone(&commands);
        let ordinary_barrier = std::sync::Arc::clone(&barrier);
        let ordinary = tokio::spawn(async move {
            ordinary_barrier.wait().await;
            ordinary_commands
                .lock()
                .await
                .try_send(ConnectorCommand::Recover {
                    id: NonZeroU64::new(3).expect("id"),
                })
        });
        let quarantine_commands = std::sync::Arc::clone(&commands);
        let quarantine_barrier = std::sync::Arc::clone(&barrier);
        let urgent = tokio::spawn(async move {
            quarantine_barrier.wait().await;
            quarantine_commands
                .lock()
                .await
                .try_send(ConnectorCommand::Quarantine {
                    id: NonZeroU64::new(2).expect("id"),
                    reason: connector_core::QuarantineReason::SupervisorPolicy,
                })
        });
        let ordinary_result = ordinary.await.expect("ordinary join");
        assert!(matches!(
            ordinary_result,
            Ok(()) | Err(ChannelError::AlreadyTerminated)
        ));
        assert_eq!(urgent.await.expect("urgent join"), Ok(()));
        assert!(matches!(
            receiver.recv().await,
            Ok(Some(ConnectorCommand::Quarantine { id, .. })) if id.get() == 2
        ));
    }
}

#[tokio::test]
async fn context_rejects_capture_from_a_different_connection_epoch() {
    let (control, _interrupt) = CancellationChannel::channel();
    let (_command_sender, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let wal_authority = wal_proof(b"epoch-test").authority;
    let raw_channel = make_raw_channel(wal_authority, 1, 64).expect("raw channel");
    let (raw_client, _raw_worker) = raw_channel.split();
    let normalized_channel = BoundedNormalizedChannel::new(1, 16_384).expect("normalized");
    let (normalized_sink, _normalized_receiver) = normalized_channel.split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        NonZeroU64::new(3).expect("epoch"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let capture = RawCapture::try_new(
        source(),
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(4).expect("wrong epoch"),
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    assert_eq!(
        context.capture_raw(capture).await,
        Err(ConnectorTermination::Fatal(
            FatalConnectorError::InvalidConfiguration
        ))
    );
}

#[tokio::test]
async fn context_rejects_capture_from_a_different_source() {
    let (control, _interrupt) = CancellationChannel::channel();
    let (_command_sender, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel =
        make_raw_channel(wal_proof(b"source-test").authority, 1, 64).expect("raw channel");
    let (raw_client, _raw_worker) = raw_channel.split();
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(1, 16_384)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        NonZeroU64::new(3).expect("epoch"),
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );
    let foreign = SourceId::new(SourceKind::Exchange, "kraken", 1).expect("foreign source fixture");
    let capture = RawCapture::try_new(
        foreign,
        NonZeroU32::new(7).expect("stream"),
        NonZeroU64::new(3).expect("epoch"),
        NonZeroU64::new(1).expect("sequence"),
        UnixNanos::new(2),
        2,
        b"raw".to_vec().into_boxed_slice(),
    )
    .expect("capture");
    assert_eq!(
        context.capture_raw(capture).await,
        Err(ConnectorTermination::Fatal(
            FatalConnectorError::InvalidConfiguration
        ))
    );
}

#[tokio::test]
async fn context_rejects_normalized_output_from_another_source_or_epoch() {
    let epoch = NonZeroU64::new(3).expect("epoch");
    let (control, _interrupt) = CancellationChannel::channel();
    let (_command_sender, command_receiver) =
        ConnectorCommandChannel::bounded(1, control).expect("commands");
    let raw_channel =
        make_raw_channel(wal_proof(b"context-normalized").authority, 1, 64).expect("raw channel");
    let (raw_client, _raw_worker) = raw_channel.split();
    let (normalized_sink, _normalized_receiver) = BoundedNormalizedChannel::new(2, 16_384)
        .expect("normalized")
        .split();
    let (lifecycle_sink, _lifecycle_receiver) = LifecycleChannel::bounded(1).expect("lifecycle");
    let mut context = ConnectorContext::new(
        source(),
        epoch,
        command_receiver,
        raw_client,
        normalized_sink,
        lifecycle_sink,
    );

    let foreign = SourceId::new(SourceKind::Exchange, "kraken", 1).expect("foreign source fixture");
    let foreign_receipt = acknowledged_receipt(foreign.clone(), epoch, b"foreign").await;
    let foreign_output = NormalizedOutput::try_new(
        foreign_receipt,
        status_event(
            foreign,
            epoch.get(),
            1,
            2,
            None,
            *blake3::hash(b"foreign").as_bytes(),
        ),
    )
    .expect("foreign output");
    assert_eq!(
        context.publish_normalized(foreign_output).await,
        Err(ConnectorTermination::Fatal(
            FatalConnectorError::InvalidConfiguration
        ))
    );

    let next_epoch = NonZeroU64::new(4).expect("next epoch");
    let epoch_receipt = acknowledged_receipt(source(), next_epoch, b"next-epoch").await;
    let epoch_output = NormalizedOutput::try_new(
        epoch_receipt,
        status_event(
            source(),
            next_epoch.get(),
            1,
            2,
            None,
            *blake3::hash(b"next-epoch").as_bytes(),
        ),
    )
    .expect("next epoch output");
    assert_eq!(
        context.publish_normalized(epoch_output).await,
        Err(ConnectorTermination::Fatal(
            FatalConnectorError::InvalidConfiguration
        ))
    );
}

#[tokio::test]
async fn normalized_output_larger_than_total_byte_budget_fails_without_waiting() {
    let receipt = acknowledged_receipt(source(), NonZeroU64::new(3).expect("epoch"), b"raw").await;
    let output = NormalizedOutput::try_new(
        receipt,
        event(source(), 3, *blake3::hash(b"raw").as_bytes(), false),
    )
    .expect("output");
    let channel = BoundedNormalizedChannel::new(1, 1).expect("tiny output budget");
    let (sink, _receiver) = channel.split();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_millis(100), sink.send(output))
            .await
            .expect("oversized output must not wait forever"),
        Err(ChannelError::CapacityRejected)
    );
}

struct ReadyConnector {
    source: SourceId,
    capabilities: ConnectorCapabilities,
}

impl MarketDataConnector for ReadyConnector {
    fn source_id(&self) -> &SourceId {
        &self.source
    }

    fn capabilities(&self) -> &ConnectorCapabilities {
        &self.capabilities
    }

    fn run(self: Box<Self>, _context: ConnectorContext) -> ConnectorFuture {
        Box::pin(async { ConnectorTermination::Fatal(FatalConnectorError::SchemaIncompatible) })
    }
}

#[test]
fn connector_trait_is_dyn_compatible_and_retry_free() {
    let connector: Box<dyn MarketDataConnector> = Box::new(ReadyConnector {
        source: source(),
        capabilities: ConnectorCapabilities::try_new(capabilities_input())
            .expect("complete capabilities"),
    });
    assert_eq!(connector.source_id(), &source());
    assert_eq!(
        connector.capabilities().venue(),
        &VenueId::new("binance").expect("venue")
    );

    fn assert_send_future(future: ConnectorFuture) -> Pin<Box<dyn FutureProbe + Send + 'static>> {
        Box::pin(FutureWrapper(future))
    }

    drop(assert_send_future(Box::pin(async {
        ConnectorTermination::Fatal(FatalConnectorError::SchemaIncompatible)
    })));
}

#[test]
fn supervisor_can_distinguish_every_termination_class() {
    let terminations = [
        ConnectorTermination::RequestedShutdown {
            command_id: NonZeroU64::new(1).expect("nonzero"),
        },
        ConnectorTermination::RecoverableDisconnect(RecoverableDisconnect::RemoteClosed),
        ConnectorTermination::CapacityRejected(connector_core::CapacityRejection::RawCapture),
        ConnectorTermination::Quarantined(connector_core::QuarantineReason::IntegrityViolation),
        ConnectorTermination::SupervisorQuarantine {
            command_id: NonZeroU64::new(2).expect("nonzero"),
            reason: connector_core::QuarantineReason::SupervisorPolicy,
        },
        ConnectorTermination::SupervisorCommand {
            command_id: NonZeroU64::new(3).expect("nonzero"),
            kind: SupervisorCommandKind::Resynchronize,
        },
        ConnectorTermination::LocalWalFailure(WalRejectionReason::StorageUnavailable),
        ConnectorTermination::SupervisorLost,
        ConnectorTermination::LocalOutputFailed,
        ConnectorTermination::Cancelled,
        ConnectorTermination::Fatal(FatalConnectorError::SchemaIncompatible),
    ];
    for (index, left) in terminations.iter().enumerate() {
        for (other_index, right) in terminations.iter().enumerate() {
            assert_eq!(left == right, index == other_index);
        }
    }
}

trait FutureProbe: std::future::Future<Output = ConnectorTermination> {}
impl<T> FutureProbe for T where T: std::future::Future<Output = ConnectorTermination> {}

struct FutureWrapper(ConnectorFuture);

impl std::future::Future for FutureWrapper {
    type Output = ConnectorTermination;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(context)
    }
}
