use std::{
    fmt,
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
    time::Duration,
};

use connector_core::{
    ConnectorCapabilities, ConnectorCommand, ConnectorContext, ConnectorFuture, ConnectorState,
    ConnectorTermination, FatalConnectorError, LifecycleCause, MarketDataConnector,
    NormalizedOutput, QuarantineReason, RawCapture, RecoverableDisconnect, SupervisorCommandKind,
};
use domain::{InstrumentId, SourceId, SourceKind, UnixNanos};
use event_envelope::UncheckedEventPayload;
use instrument_registry::CatalogSnapshot;
use orderbook::{ApplyResult, BookConfig, BookSession, BookState};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    BinanceBookSynchronizer, BinanceInput, BinanceMarket, MAX_NATIVE_PAYLOAD_BYTES,
    NormalizationContext, binance_capabilities, normalize_depth_snapshot, normalize_native_message,
    parse_durable_depth_snapshot, parse_durable_native_message,
};

const BINANCE_SOURCE: &str = "binance";
const MAX_INGESTION_INSTANCE_BYTES: usize = 256;
const MAX_SESSION_CHANNEL_RECORDS: usize = 64;
const MAX_BOOK_BINDINGS: usize = 1_024;
const DEFAULT_RENEWAL_AFTER: Duration = Duration::from_millis(86_100_000);
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BinanceSessionRoute {
    Native(BinanceInput),
    DepthSnapshot {
        market: BinanceMarket,
        symbol: String,
    },
}

/// One bounded transport record with collector timestamps fixed before WAL capture.
#[derive(Clone, Eq, PartialEq)]
pub struct BinanceSessionRecord {
    route: BinanceSessionRoute,
    stream_id: NonZeroU32,
    record_sequence: NonZeroU64,
    receive_wall_time: UnixNanos,
    receive_monotonic_ns: u64,
    payload: Box<[u8]>,
}

impl fmt::Debug for BinanceSessionRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BinanceSessionRecord")
            .field("route", &self.route)
            .field("stream_id", &self.stream_id)
            .field("record_sequence", &self.record_sequence)
            .field("receive_wall_time", &self.receive_wall_time)
            .field("receive_monotonic_ns", &self.receive_monotonic_ns)
            .field("payload_length", &self.payload.len())
            .field("payload", &"<redacted>")
            .finish()
    }
}

impl BinanceSessionRecord {
    pub fn try_new(
        route: BinanceSessionRoute,
        stream_id: NonZeroU32,
        record_sequence: NonZeroU64,
        receive_wall_time: UnixNanos,
        receive_monotonic_ns: u64,
        payload: Box<[u8]>,
    ) -> Result<Self, BinanceSessionError> {
        if payload.is_empty() || payload.len() > MAX_NATIVE_PAYLOAD_BYTES {
            return Err(BinanceSessionError::InvalidRecord);
        }
        if let BinanceSessionRoute::DepthSnapshot { symbol, .. } = &route
            && (symbol.is_empty() || symbol.len() > 96)
        {
            return Err(BinanceSessionError::InvalidRecord);
        }
        Ok(Self {
            route,
            stream_id,
            record_sequence,
            receive_wall_time,
            receive_monotonic_ns,
            payload,
        })
    }
}

#[derive(Clone)]
pub struct BinanceSessionSender {
    sender: mpsc::Sender<BinanceSessionRecord>,
}

impl BinanceSessionSender {
    pub async fn send(&self, record: BinanceSessionRecord) -> Result<(), BinanceSessionError> {
        self.sender
            .send(record)
            .await
            .map_err(|_| BinanceSessionError::ChannelClosed)
    }

    pub fn try_send(&self, record: BinanceSessionRecord) -> Result<(), BinanceSessionError> {
        self.sender.try_send(record).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => BinanceSessionError::CapacityRejected,
            mpsc::error::TrySendError::Closed(_) => BinanceSessionError::ChannelClosed,
        })
    }
}

pub struct BinanceSessionReceiver {
    receiver: mpsc::Receiver<BinanceSessionRecord>,
}

pub fn bounded_session_channel(
    capacity: usize,
) -> Result<(BinanceSessionSender, BinanceSessionReceiver), BinanceSessionError> {
    if !(1..=MAX_SESSION_CHANNEL_RECORDS).contains(&capacity) {
        return Err(BinanceSessionError::InvalidCapacity);
    }
    let (sender, receiver) = mpsc::channel(capacity);
    Ok((
        BinanceSessionSender { sender },
        BinanceSessionReceiver { receiver },
    ))
}

#[derive(Clone)]
pub struct BinanceConfig {
    source: SourceId,
    stream_id: NonZeroU32,
    catalog: Arc<CatalogSnapshot>,
    subscription_epoch: NonZeroU64,
    connection_started_at: UnixNanos,
    ingestion_instance: String,
    renewal_after: Duration,
    heartbeat_timeout: Duration,
    books: Vec<BinanceBookBinding>,
}

#[derive(Clone)]
struct BinanceBookBinding {
    market: BinanceMarket,
    config: BookConfig,
}

impl BinanceConfig {
    pub fn try_new(
        source: SourceId,
        stream_id: NonZeroU32,
        catalog: Arc<CatalogSnapshot>,
        subscription_epoch: NonZeroU64,
        connection_started_at: UnixNanos,
        ingestion_instance: impl Into<String>,
    ) -> Result<Self, BinanceSessionError> {
        let ingestion_instance = ingestion_instance.into();
        if source.kind() != SourceKind::Exchange
            || source.name() != BINANCE_SOURCE
            || !catalog.verify_integrity()
            || ingestion_instance.is_empty()
            || ingestion_instance.len() > MAX_INGESTION_INSTANCE_BYTES
            || ingestion_instance.trim() != ingestion_instance
            || ingestion_instance.chars().any(char::is_control)
        {
            return Err(BinanceSessionError::InvalidConfig);
        }
        Ok(Self {
            source,
            stream_id,
            catalog,
            subscription_epoch,
            connection_started_at,
            ingestion_instance,
            renewal_after: DEFAULT_RENEWAL_AFTER,
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
            books: Vec::new(),
        })
    }

    pub fn with_renewal_after(
        mut self,
        renewal_after: Duration,
    ) -> Result<Self, BinanceSessionError> {
        if renewal_after.is_zero() || renewal_after > DEFAULT_RENEWAL_AFTER {
            return Err(BinanceSessionError::InvalidConfig);
        }
        self.renewal_after = renewal_after;
        Ok(self)
    }

    pub fn with_heartbeat_timeout(
        mut self,
        heartbeat_timeout: Duration,
    ) -> Result<Self, BinanceSessionError> {
        if heartbeat_timeout.is_zero() || heartbeat_timeout > MAX_HEARTBEAT_TIMEOUT {
            return Err(BinanceSessionError::InvalidConfig);
        }
        self.heartbeat_timeout = heartbeat_timeout;
        Ok(self)
    }

    pub fn with_book(
        mut self,
        market: BinanceMarket,
        config: BookConfig,
    ) -> Result<Self, BinanceSessionError> {
        BinanceBookSynchronizer::try_new(market, config.clone())
            .map_err(|_| BinanceSessionError::InvalidConfig)?;
        if self
            .books
            .iter()
            .any(|binding| binding.config.instrument == config.instrument)
            || self.books.len() >= MAX_BOOK_BINDINGS
        {
            return Err(BinanceSessionError::InvalidConfig);
        }
        self.books.push(BinanceBookBinding { market, config });
        Ok(self)
    }
}

pub struct BinanceConnector {
    config: BinanceConfig,
    capabilities: ConnectorCapabilities,
    input: BinanceSessionReceiver,
}

impl BinanceConnector {
    pub fn try_new(
        config: BinanceConfig,
        input: BinanceSessionReceiver,
    ) -> Result<Self, BinanceSessionError> {
        let capabilities =
            binance_capabilities().map_err(|_| BinanceSessionError::InvalidConfig)?;
        Ok(Self {
            config,
            capabilities,
            input,
        })
    }

    async fn run_session(mut self, mut context: ConnectorContext) -> ConnectorTermination {
        if context.raw_capture_stream_id() != self.config.stream_id {
            return ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration);
        }
        for (state, cause) in [
            (
                connector_core::ConnectorState::Connecting,
                LifecycleCause::Startup,
            ),
            (
                connector_core::ConnectorState::AuthenticatingOrSubscribing,
                LifecycleCause::SubscriptionAccepted,
            ),
            (
                connector_core::ConnectorState::Synchronizing,
                LifecycleCause::SubscriptionAccepted,
            ),
        ] {
            if let Err(termination) = context
                .publish_lifecycle(state, cause, self.config.connection_started_at)
                .await
            {
                return termination;
            }
        }

        let mut books = match self.start_books(context.connection_epoch()) {
            Ok(books) => books,
            Err(termination) => return termination,
        };
        if books.is_empty()
            && let Err(termination) = context
                .publish_lifecycle(
                    ConnectorState::Healthy,
                    LifecycleCause::SynchronizationNotRequired,
                    self.config.connection_started_at,
                )
                .await
        {
            return termination;
        }
        let mut last_record_sequence = 0_u64;
        let mut last_observed_at = self.config.connection_started_at;
        let renewal = tokio::time::sleep(self.config.renewal_after);
        tokio::pin!(renewal);
        let heartbeat = tokio::time::sleep(self.config.heartbeat_timeout);
        tokio::pin!(heartbeat);
        loop {
            let record = tokio::select! {
                () = &mut renewal => {
                    disconnect_books(&mut books);
                    let observed_at = renewal_observed_at(
                        self.config.connection_started_at,
                        self.config.renewal_after,
                    );
                    if let Err(termination) = context.publish_lifecycle(
                        connector_core::ConnectorState::BackingOff,
                        LifecycleCause::RecoverableDisconnect,
                        observed_at,
                    ).await {
                        return termination;
                    }
                    return ConnectorTermination::RecoverableDisconnect(
                        RecoverableDisconnect::ConnectionExpired,
                    );
                }
                () = &mut heartbeat => {
                    disconnect_books(&mut books);
                    let observed_at = renewal_observed_at(
                        last_observed_at,
                        self.config.heartbeat_timeout,
                    );
                    if matches!(
                        context.lifecycle_state(),
                        ConnectorState::Healthy | ConnectorState::Synchronizing
                    ) && let Err(termination) = context.publish_lifecycle(
                            ConnectorState::Degraded,
                            LifecycleCause::SourceStale,
                            observed_at,
                        ).await
                    {
                        return termination;
                    }
                    if let Err(termination) = context.publish_lifecycle(
                        ConnectorState::BackingOff,
                        LifecycleCause::RecoverableDisconnect,
                        observed_at,
                    ).await {
                        return termination;
                    }
                    return ConnectorTermination::RecoverableDisconnect(
                        RecoverableDisconnect::TransportUnavailable,
                    );
                }
                command = context.next_command() => {
                    disconnect_books(&mut books);
                    return match command {
                        Err(termination) => termination,
                        Ok(command) => command_termination(command),
                    };
                }
                record = self.input.receiver.recv() => {
                    let Some(record) = record else {
                        disconnect_books(&mut books);
                        if let Err(termination) = context.publish_lifecycle(
                            connector_core::ConnectorState::BackingOff,
                            LifecycleCause::RecoverableDisconnect,
                            last_observed_at,
                        ).await {
                            return termination;
                        }
                        return ConnectorTermination::RecoverableDisconnect(
                            RecoverableDisconnect::RemoteClosed,
                        );
                    };
                    record
                }
            };
            last_observed_at = record.receive_wall_time;
            heartbeat
                .as_mut()
                .reset(tokio::time::Instant::now() + self.config.heartbeat_timeout);
            if record.stream_id != self.config.stream_id
                || record.record_sequence.get() <= last_record_sequence
                || record.receive_wall_time < self.config.connection_started_at
            {
                return ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration);
            }
            last_record_sequence = record.record_sequence.get();

            let payload = record.payload.clone();
            let capture = match RawCapture::try_new(
                self.config.source.clone(),
                record.stream_id,
                context.connection_epoch(),
                record.record_sequence,
                record.receive_wall_time,
                record.receive_monotonic_ns,
                record.payload,
            ) {
                Ok(capture) => capture,
                Err(_) => {
                    return ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration);
                }
            };
            let reference = match context.capture_raw(capture).await {
                Ok(reference) => reference,
                Err(termination) => return termination,
            };
            let normalization = match NormalizationContext::try_new(
                &self.config.catalog,
                &reference,
                reference.receive_wall_time(),
                self.config.connection_started_at,
                self.config.subscription_epoch,
                &self.config.ingestion_instance,
            ) {
                Ok(context) => context,
                Err(_) => {
                    return ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration);
                }
            };
            let events = match record.route {
                BinanceSessionRoute::Native(route) => {
                    let parsed = match parse_durable_native_message(route, &payload, &reference) {
                        Ok(parsed) => parsed,
                        Err(_) => return schema_termination(&mut context, last_observed_at).await,
                    };
                    match normalize_native_message(parsed, &normalization) {
                        Ok(events) => events,
                        Err(_) => return schema_termination(&mut context, last_observed_at).await,
                    }
                }
                BinanceSessionRoute::DepthSnapshot { market, symbol } => {
                    let parsed =
                        match parse_durable_depth_snapshot(market, &symbol, &payload, &reference) {
                            Ok(parsed) => parsed,
                            Err(_) => {
                                return schema_termination(&mut context, last_observed_at).await;
                            }
                        };
                    match normalize_depth_snapshot(parsed, &normalization) {
                        Ok(event) => vec![event],
                        Err(_) => return schema_termination(&mut context, last_observed_at).await,
                    }
                }
            };
            for event in events {
                let output = match NormalizedOutput::try_new(reference.clone(), event) {
                    Ok(output) => output,
                    Err(_) => {
                        return ConnectorTermination::Quarantined(
                            QuarantineReason::IntegrityViolation,
                        );
                    }
                };
                if let Err(termination) = apply_book_output(&mut books, &output, &mut context).await
                {
                    return termination;
                }
                if let Err(termination) = context.publish_normalized(output).await {
                    return termination;
                }
            }
        }
    }

    fn start_books(
        &self,
        connection_epoch: NonZeroU64,
    ) -> Result<Vec<(InstrumentId, BinanceBookSynchronizer)>, ConnectorTermination> {
        let mut books = Vec::with_capacity(self.config.books.len());
        for binding in &self.config.books {
            let instrument = binding.config.instrument.clone();
            let mut synchronizer =
                BinanceBookSynchronizer::try_new(binding.market, binding.config.clone()).map_err(
                    |_| ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration),
                )?;
            synchronizer
                .start_session(BookSession {
                    connection_epoch: connection_epoch.get(),
                    subscription_epoch: self.config.subscription_epoch.get(),
                    instrument_generation: instrument.generation(),
                })
                .map_err(|_| {
                    ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration)
                })?;
            books.push((instrument, synchronizer));
        }
        Ok(books)
    }
}

impl MarketDataConnector for BinanceConnector {
    fn source_id(&self) -> &SourceId {
        &self.config.source
    }

    fn capabilities(&self) -> &ConnectorCapabilities {
        &self.capabilities
    }

    fn run(self: Box<Self>, context: ConnectorContext) -> ConnectorFuture {
        Box::pin(async move { self.run_session(context).await })
    }
}

async fn schema_termination(
    context: &mut ConnectorContext,
    observed_at: UnixNanos,
) -> ConnectorTermination {
    if let Err(termination) = context
        .publish_lifecycle(
            connector_core::ConnectorState::Quarantined,
            LifecycleCause::SchemaViolation,
            observed_at,
        )
        .await
    {
        return termination;
    }
    ConnectorTermination::Quarantined(QuarantineReason::SchemaViolation)
}

fn renewal_observed_at(started_at: UnixNanos, renewal_after: Duration) -> UnixNanos {
    let additional_ns = i64::try_from(renewal_after.as_nanos()).unwrap_or(i64::MAX);
    UnixNanos::new(started_at.value().saturating_add(additional_ns))
}

fn command_termination(command: ConnectorCommand) -> ConnectorTermination {
    match command {
        ConnectorCommand::Shutdown { id } => {
            ConnectorTermination::RequestedShutdown { command_id: id }
        }
        ConnectorCommand::Quarantine { id, reason } => ConnectorTermination::SupervisorQuarantine {
            command_id: id,
            reason,
        },
        ConnectorCommand::Resynchronize { id, .. } => ConnectorTermination::SupervisorCommand {
            command_id: id,
            kind: SupervisorCommandKind::Resynchronize,
        },
        ConnectorCommand::Recover { id } => ConnectorTermination::SupervisorCommand {
            command_id: id,
            kind: SupervisorCommandKind::Recover,
        },
        ConnectorCommand::RenewConnection { id } => ConnectorTermination::SupervisorCommand {
            command_id: id,
            kind: SupervisorCommandKind::RenewConnection,
        },
    }
}

async fn apply_book_output(
    books: &mut [(InstrumentId, BinanceBookSynchronizer)],
    output: &NormalizedOutput,
    context: &mut ConnectorContext,
) -> Result<(), ConnectorTermination> {
    if !matches!(
        output.event().payload().as_unchecked(),
        UncheckedEventPayload::BookSnapshot(_) | UncheckedEventPayload::BookDelta(_)
    ) {
        return Ok(());
    }
    let instrument = output
        .event()
        .metadata()
        .as_unchecked()
        .instrument_id
        .as_ref()
        .ok_or(ConnectorTermination::Quarantined(
            QuarantineReason::IntegrityViolation,
        ))?;
    let (outcome, book_state) = {
        let synchronizer = books
            .iter_mut()
            .find_map(|(configured, synchronizer)| {
                (configured == instrument).then_some(synchronizer)
            })
            .ok_or(ConnectorTermination::Fatal(
                FatalConnectorError::InvalidConfiguration,
            ))?;
        let outcome = match synchronizer.apply_output(output) {
            Ok(outcome) => outcome,
            Err(_) if synchronizer.state() == BookState::Buffering => ApplyResult::GapDetected,
            Err(_) => {
                return Err(ConnectorTermination::Quarantined(
                    QuarantineReason::IntegrityViolation,
                ));
            }
        };
        (outcome, synchronizer.state())
    };
    match outcome {
        ApplyResult::GapDetected | ApplyResult::ChecksumMismatch => {
            match context.lifecycle_state() {
                ConnectorState::Healthy => {
                    context
                        .publish_lifecycle(
                            ConnectorState::Degraded,
                            LifecycleCause::SourceStale,
                            output.raw().receive_wall_time(),
                        )
                        .await?;
                    context
                        .publish_lifecycle(
                            ConnectorState::Recovering,
                            LifecycleCause::SequenceGap,
                            output.raw().receive_wall_time(),
                        )
                        .await?;
                }
                ConnectorState::Synchronizing | ConnectorState::Degraded => {
                    context
                        .publish_lifecycle(
                            ConnectorState::Recovering,
                            LifecycleCause::SequenceGap,
                            output.raw().receive_wall_time(),
                        )
                        .await?;
                }
                ConnectorState::Recovering => {}
                _ => {
                    return Err(ConnectorTermination::Fatal(
                        FatalConnectorError::InvalidConfiguration,
                    ));
                }
            }
        }
        ApplyResult::Applied
            if book_state == BookState::Synchronized
                && books
                    .iter()
                    .all(|(_, synchronizer)| synchronizer.state() == BookState::Synchronized) =>
        {
            if context.lifecycle_state() == ConnectorState::Recovering {
                context
                    .publish_lifecycle(
                        ConnectorState::Synchronizing,
                        LifecycleCause::ResynchronizationStarted,
                        output.raw().receive_wall_time(),
                    )
                    .await?;
            }
            if context.lifecycle_state() == ConnectorState::Synchronizing {
                context
                    .publish_lifecycle(
                        ConnectorState::Healthy,
                        LifecycleCause::SnapshotApplied,
                        output.raw().receive_wall_time(),
                    )
                    .await?;
            }
        }
        ApplyResult::Applied | ApplyResult::Duplicate | ApplyResult::SnapshotRequired => {}
    }
    Ok(())
}

fn disconnect_books(books: &mut [(InstrumentId, BinanceBookSynchronizer)]) {
    for (_, synchronizer) in books {
        synchronizer.disconnect();
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BinanceSessionError {
    #[error("Binance session configuration is invalid")]
    InvalidConfig,
    #[error("Binance session channel capacity is invalid")]
    InvalidCapacity,
    #[error("Binance session record is invalid")]
    InvalidRecord,
    #[error("Binance session channel capacity is exhausted")]
    CapacityRejected,
    #[error("Binance session channel is closed")]
    ChannelClosed,
}
