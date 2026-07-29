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
use domain::{InstrumentId, ProductType, SourceId, SourceKind, UnixNanos};
use instrument_registry::CatalogSnapshot;
use orderbook::{ApplyResult, BookConfig, BookSession, BookState};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{
    BybitBookSyncError, BybitBookSynchronizer, BybitInput, BybitMarket, BybitMessage,
    MAX_NATIVE_PAYLOAD_BYTES, NormalizationContext, capabilities, normalize_native_message,
    parse_durable_native_message,
};

const BYBIT_SOURCE: &str = "bybit";
const MAX_INGESTION_INSTANCE_BYTES: usize = 256;
const MAX_SESSION_CHANNEL_RECORDS: usize = 64;
const MAX_BOOK_BINDINGS: usize = 1_024;
const DEFAULT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(600);

/// One bounded provider record whose receive timestamps are fixed before WAL capture.
#[derive(Clone, Eq, PartialEq)]
pub struct BybitSessionRecord {
    input: BybitInput,
    stream_id: NonZeroU32,
    record_sequence: NonZeroU64,
    receive_wall_time: UnixNanos,
    receive_monotonic_ns: u64,
    payload: Box<[u8]>,
}

impl fmt::Debug for BybitSessionRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BybitSessionRecord")
            .field("input", &self.input)
            .field("stream_id", &self.stream_id)
            .field("record_sequence", &self.record_sequence)
            .field("receive_wall_time", &self.receive_wall_time)
            .field("receive_monotonic_ns", &self.receive_monotonic_ns)
            .field("payload_length", &self.payload.len())
            .field("payload", &"<redacted>")
            .finish()
    }
}

impl BybitSessionRecord {
    pub fn try_new(
        input: BybitInput,
        stream_id: NonZeroU32,
        record_sequence: NonZeroU64,
        receive_wall_time: UnixNanos,
        receive_monotonic_ns: u64,
        payload: Box<[u8]>,
    ) -> Result<Self, BybitSessionError> {
        if payload.is_empty() || payload.len() > MAX_NATIVE_PAYLOAD_BYTES {
            return Err(BybitSessionError::InvalidRecord);
        }
        Ok(Self {
            input,
            stream_id,
            record_sequence,
            receive_wall_time,
            receive_monotonic_ns,
            payload,
        })
    }
}

#[derive(Clone)]
pub struct BybitSessionSender {
    sender: mpsc::Sender<BybitSessionRecord>,
}

impl BybitSessionSender {
    pub async fn send(&self, record: BybitSessionRecord) -> Result<(), BybitSessionError> {
        self.sender
            .send(record)
            .await
            .map_err(|_| BybitSessionError::ChannelClosed)
    }

    pub fn try_send(&self, record: BybitSessionRecord) -> Result<(), BybitSessionError> {
        self.sender.try_send(record).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => BybitSessionError::CapacityRejected,
            mpsc::error::TrySendError::Closed(_) => BybitSessionError::ChannelClosed,
        })
    }
}

pub struct BybitSessionReceiver {
    receiver: mpsc::Receiver<BybitSessionRecord>,
}

pub fn bounded_session_channel(
    capacity: usize,
) -> Result<(BybitSessionSender, BybitSessionReceiver), BybitSessionError> {
    if !(1..=MAX_SESSION_CHANNEL_RECORDS).contains(&capacity) {
        return Err(BybitSessionError::InvalidCapacity);
    }
    let (sender, receiver) = mpsc::channel(capacity);
    Ok((
        BybitSessionSender { sender },
        BybitSessionReceiver { receiver },
    ))
}

#[derive(Clone)]
pub struct BybitConfig {
    source: SourceId,
    stream_id: NonZeroU32,
    catalog: Arc<CatalogSnapshot>,
    subscription_epoch: NonZeroU64,
    connection_started_at: UnixNanos,
    ingestion_instance: String,
    heartbeat_timeout: Duration,
    books: Vec<BybitBookBinding>,
}

#[derive(Clone)]
struct BybitBookBinding {
    market: BybitMarket,
    config: BookConfig,
}

impl BybitConfig {
    pub fn try_new(
        source: SourceId,
        stream_id: NonZeroU32,
        catalog: Arc<CatalogSnapshot>,
        subscription_epoch: NonZeroU64,
        connection_started_at: UnixNanos,
        ingestion_instance: impl Into<String>,
    ) -> Result<Self, BybitSessionError> {
        let ingestion_instance = ingestion_instance.into();
        if source.kind() != SourceKind::Exchange
            || source.name() != BYBIT_SOURCE
            || !catalog.verify_integrity()
            || ingestion_instance.is_empty()
            || ingestion_instance.len() > MAX_INGESTION_INSTANCE_BYTES
            || ingestion_instance.trim() != ingestion_instance
            || ingestion_instance.chars().any(char::is_control)
        {
            return Err(BybitSessionError::InvalidConfig);
        }
        Ok(Self {
            source,
            stream_id,
            catalog,
            subscription_epoch,
            connection_started_at,
            ingestion_instance,
            heartbeat_timeout: DEFAULT_HEARTBEAT_TIMEOUT,
            books: Vec::new(),
        })
    }

    pub fn with_heartbeat_timeout(
        mut self,
        heartbeat_timeout: Duration,
    ) -> Result<Self, BybitSessionError> {
        if heartbeat_timeout.is_zero() || heartbeat_timeout > MAX_HEARTBEAT_TIMEOUT {
            return Err(BybitSessionError::InvalidConfig);
        }
        self.heartbeat_timeout = heartbeat_timeout;
        Ok(self)
    }

    pub fn with_book(
        mut self,
        market: BybitMarket,
        config: BookConfig,
    ) -> Result<Self, BybitSessionError> {
        BybitBookSynchronizer::try_new(market, config.clone())
            .map_err(|_| BybitSessionError::InvalidConfig)?;
        if self
            .books
            .iter()
            .any(|binding| binding.config.instrument == config.instrument)
            || self.books.len() >= MAX_BOOK_BINDINGS
        {
            return Err(BybitSessionError::InvalidConfig);
        }
        self.books.push(BybitBookBinding { market, config });
        Ok(self)
    }
}

pub struct BybitConnector {
    config: BybitConfig,
    capabilities: ConnectorCapabilities,
    input: BybitSessionReceiver,
}

impl BybitConnector {
    pub fn try_new(
        config: BybitConfig,
        input: BybitSessionReceiver,
    ) -> Result<Self, BybitSessionError> {
        let capabilities = capabilities().map_err(|_| BybitSessionError::InvalidConfig)?;
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
            (ConnectorState::Connecting, LifecycleCause::Startup),
            (
                ConnectorState::AuthenticatingOrSubscribing,
                LifecycleCause::SubscriptionAccepted,
            ),
            (
                ConnectorState::Synchronizing,
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
        let heartbeat = tokio::time::sleep(self.config.heartbeat_timeout);
        tokio::pin!(heartbeat);
        loop {
            let record = tokio::select! {
                () = &mut heartbeat => {
                    disconnect_books(&mut books);
                    let observed_at = elapsed_observed_at(
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
                    ).await {
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
                            ConnectorState::BackingOff,
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
                disconnect_books(&mut books);
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
                    disconnect_books(&mut books);
                    return ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration);
                }
            };
            let reference = match context.capture_raw(capture).await {
                Ok(reference) => reference,
                Err(termination) => {
                    disconnect_books(&mut books);
                    return termination;
                }
            };
            let parsed = match parse_durable_native_message(record.input, &payload, &reference) {
                Ok(parsed) => parsed,
                Err(_) => {
                    disconnect_books(&mut books);
                    return schema_termination(&mut context, last_observed_at).await;
                }
            };

            let publication = match apply_book_message(
                &mut books,
                parsed.message(),
                record.receive_monotonic_ns,
                &mut context,
                reference.receive_wall_time(),
            )
            .await
            {
                Ok(publication) => publication,
                Err(termination) => {
                    disconnect_books(&mut books);
                    return termination;
                }
            };
            if publication == BookPublication::Reconnect {
                disconnect_books(&mut books);
                return ConnectorTermination::RecoverableDisconnect(
                    RecoverableDisconnect::ProtocolReset,
                );
            }

            let normalization = match NormalizationContext::try_new(
                &self.config.catalog,
                &reference,
                reference.receive_wall_time(),
                self.config.connection_started_at,
                self.config.subscription_epoch,
                &self.config.ingestion_instance,
            ) {
                Ok(normalization) => normalization,
                Err(_) => {
                    disconnect_books(&mut books);
                    return ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration);
                }
            };
            let events = match normalize_native_message(parsed, &normalization) {
                Ok(events) => events,
                Err(_) => {
                    disconnect_books(&mut books);
                    return schema_termination(&mut context, last_observed_at).await;
                }
            };
            if publication == BookPublication::Suppress {
                continue;
            }
            for event in events {
                let output = match NormalizedOutput::try_new(reference.clone(), event) {
                    Ok(output) => output,
                    Err(_) => {
                        disconnect_books(&mut books);
                        return ConnectorTermination::Quarantined(
                            QuarantineReason::IntegrityViolation,
                        );
                    }
                };
                if let Err(termination) = context.publish_normalized(output).await {
                    disconnect_books(&mut books);
                    return termination;
                }
            }
        }
    }

    fn start_books(
        &self,
        connection_epoch: NonZeroU64,
    ) -> Result<Vec<(InstrumentId, BybitBookSynchronizer)>, ConnectorTermination> {
        let mut books = Vec::with_capacity(self.config.books.len());
        for binding in &self.config.books {
            let instrument = binding.config.instrument.clone();
            let mut synchronizer =
                BybitBookSynchronizer::try_new(binding.market, binding.config.clone()).map_err(
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

impl MarketDataConnector for BybitConnector {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BookPublication {
    Publish,
    Suppress,
    Reconnect,
}

async fn apply_book_message(
    books: &mut [(InstrumentId, BybitBookSynchronizer)],
    message: &BybitMessage,
    now_monotonic_ns: u64,
    context: &mut ConnectorContext,
    observed_at: UnixNanos,
) -> Result<BookPublication, ConnectorTermination> {
    let BybitMessage::OrderBook(message) = message else {
        return Ok(BookPublication::Publish);
    };
    let expected_product = match message.market {
        BybitMarket::Spot => ProductType::Spot,
        BybitMarket::LinearPerpetual => ProductType::Perpetual,
    };
    let synchronizer = books
        .iter_mut()
        .find_map(|(instrument, synchronizer)| {
            (instrument.product_type() == expected_product
                && instrument.venue_symbol() == message.symbol)
                .then_some(synchronizer)
        })
        .ok_or(ConnectorTermination::Fatal(
            FatalConnectorError::InvalidConfiguration,
        ))?;
    let outcome = match synchronizer.apply_message(message, now_monotonic_ns) {
        Ok(outcome) => outcome,
        Err(BybitBookSyncError::CrossSequenceRegression) => {
            context
                .publish_lifecycle(
                    ConnectorState::BackingOff,
                    LifecycleCause::RecoverableDisconnect,
                    observed_at,
                )
                .await?;
            return Ok(BookPublication::Reconnect);
        }
        Err(
            BybitBookSyncError::InvalidConfig
            | BybitBookSyncError::WrongMessage
            | BybitBookSyncError::SessionNotStarted
            | BybitBookSyncError::Book(_),
        ) => {
            return Err(ConnectorTermination::Quarantined(
                QuarantineReason::IntegrityViolation,
            ));
        }
    };
    let book_state = synchronizer.state();
    match outcome {
        ApplyResult::GapDetected | ApplyResult::ChecksumMismatch => {
            match context.lifecycle_state() {
                ConnectorState::Healthy => {
                    context
                        .publish_lifecycle(
                            ConnectorState::Degraded,
                            LifecycleCause::SourceStale,
                            observed_at,
                        )
                        .await?;
                    context
                        .publish_lifecycle(
                            ConnectorState::Recovering,
                            LifecycleCause::SequenceGap,
                            observed_at,
                        )
                        .await?;
                }
                ConnectorState::Synchronizing | ConnectorState::Degraded => {
                    context
                        .publish_lifecycle(
                            ConnectorState::Recovering,
                            LifecycleCause::SequenceGap,
                            observed_at,
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
            Ok(BookPublication::Suppress)
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
                        observed_at,
                    )
                    .await?;
            }
            if context.lifecycle_state() == ConnectorState::Synchronizing {
                context
                    .publish_lifecycle(
                        ConnectorState::Healthy,
                        LifecycleCause::SnapshotApplied,
                        observed_at,
                    )
                    .await?;
            }
            Ok(BookPublication::Publish)
        }
        ApplyResult::Applied => Ok(BookPublication::Publish),
        ApplyResult::Duplicate | ApplyResult::SnapshotRequired => Ok(BookPublication::Suppress),
    }
}

async fn schema_termination(
    context: &mut ConnectorContext,
    observed_at: UnixNanos,
) -> ConnectorTermination {
    if let Err(termination) = context
        .publish_lifecycle(
            ConnectorState::Quarantined,
            LifecycleCause::SchemaViolation,
            observed_at,
        )
        .await
    {
        return termination;
    }
    ConnectorTermination::Quarantined(QuarantineReason::SchemaViolation)
}

fn elapsed_observed_at(started_at: UnixNanos, duration: Duration) -> UnixNanos {
    let additional_ns = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
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

fn disconnect_books(books: &mut [(InstrumentId, BybitBookSynchronizer)]) {
    for (_, synchronizer) in books {
        synchronizer.disconnect();
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum BybitSessionError {
    #[error("Bybit session configuration is invalid")]
    InvalidConfig,
    #[error("Bybit session channel capacity is invalid")]
    InvalidCapacity,
    #[error("Bybit session record is invalid")]
    InvalidRecord,
    #[error("Bybit session channel capacity is exhausted")]
    CapacityRejected,
    #[error("Bybit session channel is closed")]
    ChannelClosed,
}
