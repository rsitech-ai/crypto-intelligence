//! Private channel wrappers that preserve FIFO, item, byte, and invalidation policy.

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    io::{self, Write},
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    sync::{Arc, Mutex},
};

use domain::{InstrumentId, SourceId};
use event_envelope::{EventEnvelope, EventType};
use thiserror::Error;
use tokio::sync::{
    OwnedSemaphorePermit, Semaphore,
    mpsc::{self, error::TrySendError},
    watch,
};

use crate::{
    lifecycle::LifecycleEvent, raw_capture::DurableRawReference, termination::QuarantineReason,
};

pub const MAX_LOSS_SAMPLING_STREAMS: usize = 4_096;
const MAX_ACCEPTED_COMMAND_HISTORY: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorCommand {
    Shutdown {
        id: NonZeroU64,
    },
    Resynchronize {
        id: NonZeroU64,
        reason: ResynchronizationReason,
    },
    Quarantine {
        id: NonZeroU64,
        reason: QuarantineReason,
    },
    Recover {
        id: NonZeroU64,
    },
    RenewConnection {
        id: NonZeroU64,
    },
}

impl ConnectorCommand {
    pub const fn id(self) -> NonZeroU64 {
        match self {
            Self::Shutdown { id }
            | Self::Resynchronize { id, .. }
            | Self::Quarantine { id, .. }
            | Self::Recover { id }
            | Self::RenewConnection { id } => id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResynchronizationReason {
    SequenceGap,
    ChecksumMismatch,
    RequiredDeltaOverflow,
    FreshSnapshotRequested,
}

/// Factory for a bounded FIFO command path.
pub struct ConnectorCommandChannel;

impl ConnectorCommandChannel {
    pub fn bounded(
        capacity: usize,
        control: CancellationSender,
    ) -> Result<(ConnectorCommandSender, ConnectorCommandReceiver), ChannelError> {
        if !valid_item_capacity(capacity) {
            return Err(ChannelError::InvalidCapacity);
        }
        {
            let mut state = control
                .state
                .lock()
                .map_err(|_| ChannelError::StatePoisoned)?;
            if state.attached {
                return Err(ChannelError::ControlAlreadyAttached);
            }
            state.attached = true;
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            ConnectorCommandSender {
                sender,
                control: control.clone(),
            },
            ConnectorCommandReceiver {
                receiver,
                control: CancellationReceiver {
                    receiver: control.sender.subscribe(),
                    state: Arc::clone(&control.state),
                },
                command_history: CommandIdHistory::default(),
            },
        ))
    }
}

pub struct ConnectorCommandSender {
    sender: mpsc::Sender<ConnectorCommand>,
    control: CancellationSender,
}

impl ConnectorCommandSender {
    pub async fn send(&mut self, command: ConnectorCommand) -> Result<(), ChannelError> {
        if matches!(
            command,
            ConnectorCommand::Shutdown { .. } | ConnectorCommand::Quarantine { .. }
        ) {
            if self.sender.is_closed() {
                return Err(ChannelError::ReceiverClosed);
            }
            return self.control.accept_terminal_command(command);
        }
        let permit = self
            .sender
            .reserve()
            .await
            .map_err(|_| ChannelError::ReceiverClosed)?;
        self.control.accept_ordinary_command(command)?;
        permit.send(command);
        Ok(())
    }

    pub fn try_send(&mut self, command: ConnectorCommand) -> Result<(), ChannelError> {
        if matches!(
            command,
            ConnectorCommand::Shutdown { .. } | ConnectorCommand::Quarantine { .. }
        ) {
            if self.sender.is_closed() {
                return Err(ChannelError::ReceiverClosed);
            }
            return self.control.accept_terminal_command(command);
        }
        let permit = self.sender.try_reserve().map_err(|error| match error {
            TrySendError::Full(_) => ChannelError::CapacityRejected,
            TrySendError::Closed(_) => ChannelError::ReceiverClosed,
        })?;
        self.control.accept_ordinary_command(command)?;
        permit.send(command);
        Ok(())
    }
}

pub struct ConnectorCommandReceiver {
    receiver: mpsc::Receiver<ConnectorCommand>,
    control: CancellationReceiver,
    command_history: CommandIdHistory,
}

impl ConnectorCommandReceiver {
    pub async fn recv(&mut self) -> Result<Option<ConnectorCommand>, ChannelError> {
        let command = tokio::select! {
            biased;
            interrupt = self.control.interrupted() => {
                match interrupt {
                    ContextInterrupt::RequestedShutdown { command_id } => {
                        ConnectorCommand::Shutdown { id: command_id }
                    }
                    ContextInterrupt::RequestedQuarantine { command_id, reason } => {
                        ConnectorCommand::Quarantine {
                            id: command_id,
                            reason,
                        }
                    }
                    ContextInterrupt::Cancelled => return Err(ChannelError::Cancelled),
                }
            }
            command = self.receiver.recv() => {
                let Some(command) = command else {
                    return Ok(None);
                };
                command
            }
        };
        self.command_history.accept(
            command.id(),
            matches!(command, ConnectorCommand::Quarantine { .. }),
        )?;
        Ok(Some(command))
    }

    pub fn interruption_receiver(&self) -> CancellationReceiver {
        self.control.clone()
    }
}

/// Monotonic cancellation is intentionally latest-value state, not a command.
pub struct CancellationChannel;

impl CancellationChannel {
    pub fn channel() -> (CancellationSender, CancellationReceiver) {
        let (sender, receiver) = watch::channel(false);
        let state = Arc::new(Mutex::new(ControlState::default()));
        (
            CancellationSender {
                sender,
                state: Arc::clone(&state),
            },
            CancellationReceiver { receiver, state },
        )
    }
}

#[derive(Default)]
struct ControlState {
    attached: bool,
    command_history: CommandIdHistory,
    interrupt: Option<ContextInterrupt>,
}

#[derive(Default)]
struct CommandIdHistory {
    high_water: Option<NonZeroU64>,
    retired_high_water: Option<NonZeroU64>,
    recent: BTreeSet<NonZeroU64>,
    order: VecDeque<NonZeroU64>,
}

impl CommandIdHistory {
    fn validate(
        &self,
        id: NonZeroU64,
        allow_unused_below_high_water: bool,
    ) -> Result<(), ChannelError> {
        let historically_accepted = self.recent.contains(&id)
            || self.retired_high_water.is_some_and(|retired| id <= retired);
        let out_of_order = !allow_unused_below_high_water
            && self.high_water.is_some_and(|high_water| id <= high_water);
        if historically_accepted || out_of_order {
            Err(ChannelError::CommandSequenceRegression)
        } else {
            Ok(())
        }
    }

    fn accept(
        &mut self,
        id: NonZeroU64,
        allow_unused_below_high_water: bool,
    ) -> Result<(), ChannelError> {
        self.validate(id, allow_unused_below_high_water)?;
        self.high_water = Some(self.high_water.map_or(id, |high_water| high_water.max(id)));
        self.recent.insert(id);
        self.order.push_back(id);
        if self.order.len() > MAX_ACCEPTED_COMMAND_HISTORY {
            let retired = self
                .order
                .pop_front()
                .expect("history exceeds its nonzero bound");
            self.recent.remove(&retired);
            self.retired_high_water = Some(
                self.retired_high_water
                    .map_or(retired, |high_water| high_water.max(retired)),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextInterrupt {
    Cancelled,
    RequestedShutdown {
        command_id: NonZeroU64,
    },
    RequestedQuarantine {
        command_id: NonZeroU64,
        reason: QuarantineReason,
    },
}

#[derive(Clone)]
pub struct CancellationSender {
    sender: watch::Sender<bool>,
    state: Arc<Mutex<ControlState>>,
}

impl CancellationSender {
    pub fn cancel(&self) -> bool {
        let installed = self.state.lock().ok().is_some_and(|mut state| {
            if state.interrupt.is_some() {
                false
            } else {
                state.interrupt = Some(ContextInterrupt::Cancelled);
                true
            }
        });
        if installed {
            self.sender.send_replace(true);
        }
        installed
    }

    fn accept_ordinary_command(&self, command: ConnectorCommand) -> Result<(), ChannelError> {
        let mut state = self.state.lock().map_err(|_| ChannelError::StatePoisoned)?;
        if state.interrupt.is_some() {
            return Err(ChannelError::AlreadyTerminated);
        }
        state.command_history.accept(command.id(), false)?;
        Ok(())
    }

    fn accept_terminal_command(&self, command: ConnectorCommand) -> Result<(), ChannelError> {
        let interrupt = match command {
            ConnectorCommand::Shutdown { id } => {
                ContextInterrupt::RequestedShutdown { command_id: id }
            }
            ConnectorCommand::Quarantine { id, reason } => ContextInterrupt::RequestedQuarantine {
                command_id: id,
                reason,
            },
            _ => return Err(ChannelError::InvalidTerminalCommand),
        };
        {
            let mut state = self.state.lock().map_err(|_| ChannelError::StatePoisoned)?;
            if state.interrupt.is_some() {
                return Err(ChannelError::AlreadyTerminated);
            }
            state.command_history.accept(
                command.id(),
                matches!(command, ConnectorCommand::Quarantine { .. }),
            )?;
            state.interrupt = Some(interrupt);
        }
        self.sender.send_replace(true);
        Ok(())
    }
}

#[derive(Clone)]
pub struct CancellationReceiver {
    receiver: watch::Receiver<bool>,
    state: Arc<Mutex<ControlState>>,
}

impl CancellationReceiver {
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    pub fn current_interrupt(&self) -> Option<ContextInterrupt> {
        self.state.lock().ok().and_then(|state| state.interrupt)
    }

    pub async fn interrupted(&mut self) -> ContextInterrupt {
        if let Some(interrupt) = self.current_interrupt() {
            return interrupt;
        }
        while self.receiver.changed().await.is_ok() {
            if let Some(interrupt) = self.current_interrupt() {
                return interrupt;
            }
        }
        if let Some(interrupt) = self.current_interrupt() {
            return interrupt;
        }
        std::future::pending().await
    }
}

/// Normalized event linked to an acknowledged raw WAL record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedOutput {
    raw: DurableRawReference,
    event: EventEnvelope,
}

impl NormalizedOutput {
    pub fn try_new(raw: DurableRawReference, event: EventEnvelope) -> Result<Self, ChannelError> {
        let metadata = event.metadata().as_unchecked();
        if &metadata.source != raw.source() {
            return Err(ChannelError::RawSourceMismatch);
        }
        if metadata.connection_epoch != raw.connection_epoch().get() {
            return Err(ChannelError::RawConnectionEpochMismatch);
        }
        if &metadata.raw_payload_hash != raw.payload_hash() {
            return Err(ChannelError::RawPayloadHashMismatch);
        }
        Ok(Self { raw, event })
    }

    pub const fn raw(&self) -> &DurableRawReference {
        &self.raw
    }

    pub const fn event(&self) -> &EventEnvelope {
        &self.event
    }
}

pub struct BoundedNormalizedChannel {
    sink: BoundedNormalizedSink,
    receiver: BoundedNormalizedReceiver,
}

impl BoundedNormalizedChannel {
    pub fn new(item_capacity: usize, byte_capacity: usize) -> Result<Self, ChannelError> {
        Self::with_registry_and_loss_policies(
            item_capacity,
            byte_capacity,
            BookStreamRegistry::empty(),
            LossPolicyRegistry::empty(),
        )
    }

    pub fn with_book_streams(
        item_capacity_per_lane: usize,
        byte_capacity_per_lane: usize,
        maximum_book_streams: NonZeroUsize,
        admitted_book_streams: impl IntoIterator<Item = BookStreamKey>,
    ) -> Result<Self, ChannelError> {
        Self::with_registry_and_loss_policies(
            item_capacity_per_lane,
            byte_capacity_per_lane,
            BookStreamRegistry::try_new(maximum_book_streams, admitted_book_streams)?,
            LossPolicyRegistry::empty(),
        )
    }

    pub fn with_loss_policies(
        item_capacity_per_lane: usize,
        byte_capacity_per_lane: usize,
        loss_policies: LossPolicyRegistry,
    ) -> Result<Self, ChannelError> {
        Self::with_registry_and_loss_policies(
            item_capacity_per_lane,
            byte_capacity_per_lane,
            BookStreamRegistry::empty(),
            loss_policies,
        )
    }

    pub fn with_book_streams_and_loss_policies(
        item_capacity_per_lane: usize,
        byte_capacity_per_lane: usize,
        maximum_book_streams: NonZeroUsize,
        admitted_book_streams: impl IntoIterator<Item = BookStreamKey>,
        loss_policies: LossPolicyRegistry,
    ) -> Result<Self, ChannelError> {
        Self::with_registry_and_loss_policies(
            item_capacity_per_lane,
            byte_capacity_per_lane,
            BookStreamRegistry::try_new(maximum_book_streams, admitted_book_streams)?,
            loss_policies,
        )
    }

    fn with_registry_and_loss_policies(
        item_capacity_per_lane: usize,
        byte_capacity_per_lane: usize,
        registry: BookStreamRegistry,
        loss_policies: LossPolicyRegistry,
    ) -> Result<Self, ChannelError> {
        let byte_capacity =
            u32::try_from(byte_capacity_per_lane).map_err(|_| ChannelError::InvalidCapacity)?;
        if !valid_item_capacity(item_capacity_per_lane) || byte_capacity == 0 {
            return Err(ChannelError::InvalidCapacity);
        }
        let (integrity_sender, integrity_receiver) = mpsc::channel(item_capacity_per_lane);
        let (core_sender, core_receiver) = mpsc::channel(item_capacity_per_lane);
        let (derivatives_sender, derivatives_receiver) = mpsc::channel(item_capacity_per_lane);
        let (auxiliary_sender, auxiliary_receiver) = mpsc::channel(item_capacity_per_lane);
        let (ui_sender, ui_receiver) = mpsc::channel(item_capacity_per_lane);
        let book_streams = Arc::new(Mutex::new(registry));
        Ok(Self {
            sink: BoundedNormalizedSink {
                integrity_sender,
                core_sender,
                derivatives_sender,
                auxiliary_sender,
                ui_sender,
                integrity_byte_budget: Arc::new(Semaphore::new(
                    usize::try_from(byte_capacity).map_err(|_| ChannelError::InvalidCapacity)?,
                )),
                core_byte_budget: Arc::new(Semaphore::new(
                    usize::try_from(byte_capacity).map_err(|_| ChannelError::InvalidCapacity)?,
                )),
                derivatives_byte_budget: Arc::new(Semaphore::new(
                    usize::try_from(byte_capacity).map_err(|_| ChannelError::InvalidCapacity)?,
                )),
                auxiliary_byte_budget: Arc::new(Semaphore::new(
                    usize::try_from(byte_capacity).map_err(|_| ChannelError::InvalidCapacity)?,
                )),
                ui_byte_budget: Arc::new(Semaphore::new(
                    usize::try_from(byte_capacity).map_err(|_| ChannelError::InvalidCapacity)?,
                )),
                byte_capacity,
                book_streams: Arc::clone(&book_streams),
                loss_policies,
                sampling_state: Arc::new(Mutex::new(HashMap::new())),
            },
            receiver: BoundedNormalizedReceiver {
                integrity_receiver,
                core_receiver,
                derivatives_receiver,
                auxiliary_receiver,
                ui_receiver,
                book_streams,
            },
        })
    }

    pub fn split(self) -> (BoundedNormalizedSink, BoundedNormalizedReceiver) {
        (self.sink, self.receiver)
    }
}

#[derive(Clone)]
pub struct BoundedNormalizedSink {
    integrity_sender: mpsc::Sender<QueuedNormalizedOutput>,
    core_sender: mpsc::Sender<QueuedNormalizedOutput>,
    derivatives_sender: mpsc::Sender<QueuedNormalizedOutput>,
    auxiliary_sender: mpsc::Sender<QueuedNormalizedOutput>,
    ui_sender: mpsc::Sender<QueuedNormalizedOutput>,
    integrity_byte_budget: Arc<Semaphore>,
    core_byte_budget: Arc<Semaphore>,
    derivatives_byte_budget: Arc<Semaphore>,
    auxiliary_byte_budget: Arc<Semaphore>,
    ui_byte_budget: Arc<Semaphore>,
    byte_capacity: u32,
    book_streams: Arc<Mutex<BookStreamRegistry>>,
    loss_policies: LossPolicyRegistry,
    sampling_state: Arc<Mutex<HashMap<LossStreamKey, u64>>>,
}

impl BoundedNormalizedSink {
    pub async fn send(&self, output: NormalizedOutput) -> Result<(), ChannelError> {
        let priority = output.event.event_type().priority();
        let required = required_book_stream(&output);
        if let Some((key, _)) = &required {
            self.ensure_registered(key)?;
        }
        if output.event.event_type() == EventType::BookDelta
            && let Some((key, _)) = &required
            && self.is_invalidated(key)?
        {
            return Err(ChannelError::ResynchronizationRequired);
        }
        let encoded_len = encoded_length(output.event())?;
        if encoded_len > self.byte_capacity {
            if let Some((key, sequence)) = required {
                self.invalidate(key, sequence)?;
                return Err(ChannelError::ResynchronizationRequired);
            }
            return Err(ChannelError::CapacityRejected);
        }
        let (sender, byte_budget) = self.lane(priority);
        let permit = Arc::clone(byte_budget)
            .acquire_many_owned(encoded_len)
            .await
            .map_err(|_| ChannelError::ReceiverClosed)?;
        sender
            .send(QueuedNormalizedOutput {
                output,
                _byte_permit: permit,
            })
            .await
            .map_err(|_| ChannelError::ReceiverClosed)
    }

    pub fn try_send(&self, output: NormalizedOutput) -> Result<(), ChannelError> {
        self.try_send_inner(output)
    }

    pub fn try_send_lossy(
        &self,
        output: NormalizedOutput,
        policy: SamplingPolicy,
    ) -> Result<DeliveryOutcome, LossyPublishError> {
        let event_type = output.event().event_type();
        if !matches!(
            event_type.priority(),
            EventPriority::Auxiliary | EventPriority::UiAggregate
        ) {
            return Err(LossyPublishError::new(
                ChannelError::RequiredEventCannotBeLossy,
                output,
            ));
        }
        if self.loss_policies.get(event_type) != Some(policy) {
            return Err(LossyPublishError::new(
                ChannelError::UndeclaredLossPolicy,
                output,
            ));
        }
        let metadata = output.event().metadata().as_unchecked();
        let sequence_number = metadata.sequence_number;
        let receive_monotonic_ns = metadata.receive_monotonic_ns;
        let stream = LossStreamKey::from_output(&output);
        let SamplingPolicy::SampleEvery { interval_ms } = policy;
        let interval_ns = u64::from(interval_ms.get()).saturating_mul(1_000_000);
        let mut sampling_state = self
            .sampling_state
            .lock()
            .map_err(|_| LossyPublishError::new(ChannelError::StatePoisoned, output.clone()))?;
        if !sampling_state.contains_key(&stream)
            && sampling_state.len() >= MAX_LOSS_SAMPLING_STREAMS
        {
            return Err(LossyPublishError::new(
                ChannelError::SamplingRegistryCapacityExceeded,
                output,
            ));
        }
        if sampling_state.get(&stream).is_some_and(|previous| {
            receive_monotonic_ns >= *previous && receive_monotonic_ns - *previous < interval_ns
        }) {
            return Ok(DeliveryOutcome::Dropped(Box::new(DroppedOutput::new(
                output,
                LossAuditRecord {
                    stream,
                    policy,
                    sequence_number,
                    reason: LossReason::SamplingWindow,
                },
            ))));
        }
        let encoded_len = encoded_length(output.event())
            .map_err(|error| LossyPublishError::new(error, output.clone()))?;
        if encoded_len > self.byte_capacity {
            return Ok(DeliveryOutcome::Dropped(Box::new(DroppedOutput::new(
                output,
                LossAuditRecord {
                    stream,
                    policy,
                    sequence_number,
                    reason: LossReason::ByteCapacity,
                },
            ))));
        }
        let priority = event_type.priority();
        let (sender, byte_budget) = self.lane(priority);
        let permit = match Arc::clone(byte_budget).try_acquire_many_owned(encoded_len) {
            Ok(permit) => permit,
            Err(_) => {
                return Ok(DeliveryOutcome::Dropped(Box::new(DroppedOutput::new(
                    output,
                    LossAuditRecord {
                        stream,
                        policy,
                        sequence_number,
                        reason: LossReason::ByteCapacity,
                    },
                ))));
            }
        };
        match sender.try_send(QueuedNormalizedOutput {
            output,
            _byte_permit: permit,
        }) {
            Ok(()) => {
                sampling_state.insert(stream, receive_monotonic_ns);
                Ok(DeliveryOutcome::Queued)
            }
            Err(TrySendError::Full(queued)) => {
                Ok(DeliveryOutcome::Dropped(Box::new(DroppedOutput::new(
                    queued.into_output(),
                    LossAuditRecord {
                        stream,
                        policy,
                        sequence_number,
                        reason: LossReason::ItemCapacity,
                    },
                ))))
            }
            Err(TrySendError::Closed(queued)) => Err(LossyPublishError::new(
                ChannelError::ReceiverClosed,
                queued.into_output(),
            )),
        }
    }

    fn try_send_inner(&self, output: NormalizedOutput) -> Result<(), ChannelError> {
        let priority = output.event.event_type().priority();
        let required = required_book_stream(&output);
        if let Some((key, _)) = &required {
            self.ensure_registered(key)?;
        }
        if output.event.event_type() == EventType::BookDelta
            && let Some((key, _)) = &required
            && self.is_invalidated(key)?
        {
            return Err(ChannelError::ResynchronizationRequired);
        }
        let encoded_len = encoded_length(output.event())?;
        if encoded_len > self.byte_capacity {
            if let Some((key, sequence)) = required {
                self.invalidate(key, sequence)?;
                return Err(ChannelError::ResynchronizationRequired);
            }
            return Err(ChannelError::CapacityRejected);
        }
        let (sender, byte_budget) = self.lane(priority);
        let permit = Arc::clone(byte_budget)
            .try_acquire_many_owned(encoded_len)
            .map_err(|_| {
                if let Some((key, sequence)) = required.clone() {
                    let _ = self.invalidate(key, sequence);
                    ChannelError::ResynchronizationRequired
                } else {
                    ChannelError::CapacityRejected
                }
            })?;
        sender
            .try_send(QueuedNormalizedOutput {
                output,
                _byte_permit: permit,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => {
                    if let Some((key, sequence)) = required {
                        if self.invalidate(key, sequence).is_err() {
                            return ChannelError::StatePoisoned;
                        }
                        ChannelError::ResynchronizationRequired
                    } else {
                        ChannelError::CapacityRejected
                    }
                }
                TrySendError::Closed(_) => ChannelError::ReceiverClosed,
            })
    }

    fn lane(
        &self,
        priority: EventPriority,
    ) -> (&mpsc::Sender<QueuedNormalizedOutput>, &Arc<Semaphore>) {
        match priority {
            EventPriority::Integrity => (&self.integrity_sender, &self.integrity_byte_budget),
            EventPriority::MarketCore => (&self.core_sender, &self.core_byte_budget),
            EventPriority::DerivativesState => {
                (&self.derivatives_sender, &self.derivatives_byte_budget)
            }
            EventPriority::Auxiliary => (&self.auxiliary_sender, &self.auxiliary_byte_budget),
            EventPriority::UiAggregate => (&self.ui_sender, &self.ui_byte_budget),
        }
    }

    fn is_invalidated(&self, key: &BookStreamKey) -> Result<bool, ChannelError> {
        Ok(self
            .book_streams
            .lock()
            .map_err(|_| ChannelError::StatePoisoned)?
            .required_sequence(key)?
            .is_some())
    }

    fn ensure_registered(&self, key: &BookStreamKey) -> Result<(), ChannelError> {
        self.book_streams
            .lock()
            .map_err(|_| ChannelError::StatePoisoned)?
            .required_sequence(key)
            .map(|_| ())
    }

    fn invalidate(&self, key: BookStreamKey, sequence: u64) -> Result<(), ChannelError> {
        let mut registry = self
            .book_streams
            .lock()
            .map_err(|_| ChannelError::StatePoisoned)?;
        registry.invalidate(&key, sequence)
    }
}

pub struct BoundedNormalizedReceiver {
    integrity_receiver: mpsc::Receiver<QueuedNormalizedOutput>,
    core_receiver: mpsc::Receiver<QueuedNormalizedOutput>,
    derivatives_receiver: mpsc::Receiver<QueuedNormalizedOutput>,
    auxiliary_receiver: mpsc::Receiver<QueuedNormalizedOutput>,
    ui_receiver: mpsc::Receiver<QueuedNormalizedOutput>,
    book_streams: Arc<Mutex<BookStreamRegistry>>,
}

impl BoundedNormalizedReceiver {
    pub async fn recv(&mut self) -> Option<NormalizedOutput> {
        loop {
            if let Ok(output) = self.integrity_receiver.try_recv() {
                return Some(output.into_output());
            }
            if let Ok(output) = self.core_receiver.try_recv() {
                return Some(output.into_output());
            }
            if let Ok(output) = self.derivatives_receiver.try_recv() {
                return Some(output.into_output());
            }
            if let Ok(output) = self.auxiliary_receiver.try_recv() {
                return Some(output.into_output());
            }
            if let Ok(output) = self.ui_receiver.try_recv() {
                return Some(output.into_output());
            }
            if self.integrity_receiver.is_closed()
                && self.core_receiver.is_closed()
                && self.derivatives_receiver.is_closed()
                && self.auxiliary_receiver.is_closed()
                && self.ui_receiver.is_closed()
            {
                return None;
            }
            tokio::select! {
                biased;
                output = self.integrity_receiver.recv(), if !self.integrity_receiver.is_closed() => {
                    if let Some(output) = output {
                        return Some(output.into_output());
                    }
                }
                output = self.core_receiver.recv(), if !self.core_receiver.is_closed() => {
                    if let Some(output) = output {
                        return Some(output.into_output());
                    }
                }
                output = self.derivatives_receiver.recv(), if !self.derivatives_receiver.is_closed() => {
                    if let Some(output) = output {
                        return Some(output.into_output());
                    }
                }
                output = self.auxiliary_receiver.recv(), if !self.auxiliary_receiver.is_closed() => {
                    if let Some(output) = output {
                        return Some(output.into_output());
                    }
                }
                output = self.ui_receiver.recv(), if !self.ui_receiver.is_closed() => {
                    if let Some(output) = output {
                        return Some(output.into_output());
                    }
                }
            }
        }
    }

    /// Receives an output while preserving its channel-issued delivery provenance.
    pub async fn recv_for_apply(&mut self) -> Option<DeliveredNormalizedOutput> {
        self.recv().await.map(|output| DeliveredNormalizedOutput {
            output: Box::new(output),
            registry: Arc::clone(&self.book_streams),
        })
    }

    /// Clear one invalidation only after applying a matching delivered snapshot.
    pub fn accept_resynchronizing_snapshot(
        &self,
        applied: AppliedSnapshot,
    ) -> Result<(), ChannelError> {
        if !Arc::ptr_eq(&self.book_streams, &applied.registry) {
            return Err(ChannelError::SnapshotDoesNotSatisfyInvalidation);
        }
        let snapshot = *applied.output;
        if snapshot.event().event_type() != EventType::BookSnapshot {
            return Err(ChannelError::SnapshotDoesNotSatisfyInvalidation);
        }
        let (key, snapshot_sequence) = required_book_stream(&snapshot)
            .ok_or(ChannelError::SnapshotDoesNotSatisfyInvalidation)?;
        let mut registry = self
            .book_streams
            .lock()
            .map_err(|_| ChannelError::StatePoisoned)?;
        let required_sequence = registry
            .required_sequence(&key)?
            .ok_or(ChannelError::SnapshotDoesNotSatisfyInvalidation)?;
        if snapshot_sequence < required_sequence {
            return Err(ChannelError::SnapshotDoesNotSatisfyInvalidation);
        }
        registry.mark_synchronized(&key)?;
        Ok(())
    }
}

/// One output proven to have been delivered by this normalized receiver.
#[must_use = "the delivered output must be applied or explicitly discarded"]
#[derive(Debug)]
pub struct DeliveredNormalizedOutput {
    output: Box<NormalizedOutput>,
    registry: Arc<Mutex<BookStreamRegistry>>,
}

impl DeliveredNormalizedOutput {
    pub fn output(&self) -> &NormalizedOutput {
        self.output.as_ref()
    }

    pub fn into_output(self) -> NormalizedOutput {
        *self.output
    }

    /// Confirms the downstream consumer applied this delivered snapshot.
    pub fn confirm_snapshot_applied(self) -> Result<AppliedSnapshot, Self> {
        if self.output.event().event_type() == EventType::BookSnapshot {
            Ok(AppliedSnapshot {
                output: self.output,
                registry: self.registry,
            })
        } else {
            Err(self)
        }
    }
}

/// Linear evidence that a receiver-delivered book snapshot was applied.
pub struct AppliedSnapshot {
    output: Box<NormalizedOutput>,
    registry: Arc<Mutex<BookStreamRegistry>>,
}

/// Normative section 29.3 delivery priority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventPriority {
    Integrity = 1,
    MarketCore = 2,
    DerivativesState = 3,
    Auxiliary = 4,
    UiAggregate = 5,
}

impl EventPriority {
    pub fn for_event(event_type: EventType) -> Self {
        event_type.priority()
    }
}

trait EventPriorityExt {
    fn priority(self) -> EventPriority;
}

impl EventPriorityExt for EventType {
    fn priority(self) -> EventPriority {
        match self {
            Self::InstrumentDefinition | Self::BookSnapshot | Self::BookDelta => {
                EventPriority::Integrity
            }
            Self::Trade | Self::MarkIndexObservation => EventPriority::MarketCore,
            Self::FundingObservation
            | Self::OpenInterestObservation
            | Self::LiquidationObservation
            | Self::FutureBasisObservation
            | Self::OptionTrade => EventPriority::DerivativesState,
            Self::TopOfBook
            | Self::OptionTicker
            | Self::VenueStatus
            | Self::ChainBlock
            | Self::ChainTransactionAggregate
            | Self::ChainMempoolObservation
            | Self::ChainMetric
            | Self::ExternalMarketObservation
            | Self::StructuredEvent => EventPriority::Auxiliary,
            Self::DataQualityObservation | Self::PredictionRecord | Self::AlertEvent => {
                EventPriority::UiAggregate
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplingPolicy {
    SampleEvery { interval_ms: NonZeroU32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LossPolicyRegistry(HashMap<EventType, SamplingPolicy>);

impl LossPolicyRegistry {
    const MAX_POLICIES: usize = 16;

    pub fn empty() -> Self {
        Self(HashMap::new())
    }

    pub fn try_new(
        policies: impl IntoIterator<Item = (EventType, SamplingPolicy)>,
    ) -> Result<Self, ChannelError> {
        let mut declared = HashMap::new();
        for (event_type, policy) in policies {
            if !matches!(
                event_type.priority(),
                EventPriority::Auxiliary | EventPriority::UiAggregate
            ) || declared.insert(event_type, policy).is_some()
                || declared.len() > Self::MAX_POLICIES
            {
                return Err(ChannelError::InvalidLossPolicy);
            }
        }
        Ok(Self(declared))
    }

    pub fn get(&self, event_type: EventType) -> Option<SamplingPolicy> {
        self.0.get(&event_type).copied()
    }
}

#[must_use = "delivery outcomes must be persisted or otherwise accounted for"]
#[derive(Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    Queued,
    Dropped(Box<DroppedOutput>),
}

#[must_use = "the audit record must be persisted before the original output is discarded"]
#[derive(Debug, Eq, PartialEq)]
pub struct DroppedOutput {
    output: Box<NormalizedOutput>,
    audit: LossAuditRecord,
}

impl DroppedOutput {
    fn new(output: NormalizedOutput, audit: LossAuditRecord) -> Self {
        Self {
            output: Box::new(output),
            audit,
        }
    }

    pub const fn audit(&self) -> &LossAuditRecord {
        &self.audit
    }

    pub fn output(&self) -> &NormalizedOutput {
        self.output.as_ref()
    }

    pub fn into_parts(self) -> (NormalizedOutput, LossAuditRecord) {
        (*self.output, self.audit)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LossAuditRecord {
    pub stream: LossStreamKey,
    pub policy: SamplingPolicy,
    pub sequence_number: Option<u64>,
    pub reason: LossReason,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LossReason {
    SamplingWindow,
    ItemCapacity,
    ByteCapacity,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LossStreamKey {
    source: SourceId,
    event_type: EventType,
    instrument: Option<InstrumentId>,
    connection_epoch: u64,
    subscription_epoch: u64,
}

impl LossStreamKey {
    fn from_output(output: &NormalizedOutput) -> Self {
        let metadata = output.event().metadata().as_unchecked();
        Self {
            source: metadata.source.clone(),
            event_type: output.event().event_type(),
            instrument: metadata.instrument_id.clone(),
            connection_epoch: metadata.connection_epoch,
            subscription_epoch: metadata.subscription_epoch,
        }
    }

    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn event_type(&self) -> EventType {
        self.event_type
    }

    pub const fn instrument(&self) -> Option<&InstrumentId> {
        self.instrument.as_ref()
    }

    pub const fn connection_epoch(&self) -> u64 {
        self.connection_epoch
    }

    pub const fn subscription_epoch(&self) -> u64 {
        self.subscription_epoch
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct LossyPublishError {
    error: ChannelError,
    output: Box<NormalizedOutput>,
}

impl LossyPublishError {
    fn new(error: ChannelError, output: NormalizedOutput) -> Self {
        Self {
            error,
            output: Box::new(output),
        }
    }

    pub const fn error(&self) -> ChannelError {
        self.error
    }

    pub fn output(&self) -> &NormalizedOutput {
        self.output.as_ref()
    }

    pub fn into_parts(self) -> (ChannelError, NormalizedOutput) {
        (self.error, *self.output)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BookStreamKey {
    source: SourceId,
    instrument: InstrumentId,
    subscription_epoch: u64,
}

impl BookStreamKey {
    pub fn new(source: SourceId, instrument: InstrumentId, subscription_epoch: NonZeroU64) -> Self {
        Self {
            source,
            instrument,
            subscription_epoch: subscription_epoch.get(),
        }
    }
}

#[derive(Debug)]
struct BookStreamRegistry {
    states: HashMap<BookStreamKey, Option<u64>>,
}

impl BookStreamRegistry {
    fn empty() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    fn try_new(
        maximum: NonZeroUsize,
        admitted: impl IntoIterator<Item = BookStreamKey>,
    ) -> Result<Self, ChannelError> {
        let mut states = HashMap::with_capacity(maximum.get());
        for key in admitted {
            if states.len() >= maximum.get() || states.insert(key, None).is_some() {
                return Err(ChannelError::InvalidBookStreamRegistry);
            }
        }
        Ok(Self { states })
    }

    fn required_sequence(&self, key: &BookStreamKey) -> Result<Option<u64>, ChannelError> {
        self.states
            .get(key)
            .copied()
            .ok_or(ChannelError::UnknownBookStream)
    }

    fn invalidate(&mut self, key: &BookStreamKey, sequence: u64) -> Result<(), ChannelError> {
        let state = self
            .states
            .get_mut(key)
            .ok_or(ChannelError::UnknownBookStream)?;
        *state = Some(state.unwrap_or(0).max(sequence));
        Ok(())
    }

    fn mark_synchronized(&mut self, key: &BookStreamKey) -> Result<(), ChannelError> {
        let state = self
            .states
            .get_mut(key)
            .ok_or(ChannelError::UnknownBookStream)?;
        *state = None;
        Ok(())
    }
}

fn required_book_stream(output: &NormalizedOutput) -> Option<(BookStreamKey, u64)> {
    if !matches!(
        output.event().event_type(),
        EventType::InstrumentDefinition | EventType::BookSnapshot | EventType::BookDelta
    ) {
        return None;
    }
    let metadata = output.event().metadata().as_unchecked();
    Some((
        BookStreamKey {
            source: metadata.source.clone(),
            instrument: metadata.instrument_id.clone()?,
            subscription_epoch: metadata.subscription_epoch,
        },
        metadata.sequence_number.unwrap_or(0),
    ))
}

struct QueuedNormalizedOutput {
    output: NormalizedOutput,
    _byte_permit: OwnedSemaphorePermit,
}

impl QueuedNormalizedOutput {
    fn into_output(self) -> NormalizedOutput {
        self.output
    }
}

pub struct LifecycleChannel;

impl LifecycleChannel {
    pub fn bounded(capacity: usize) -> Result<(LifecycleSink, LifecycleReceiver), ChannelError> {
        if !valid_item_capacity(capacity) {
            return Err(ChannelError::InvalidCapacity);
        }
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((LifecycleSink { sender }, LifecycleReceiver { receiver }))
    }
}

fn valid_item_capacity(capacity: usize) -> bool {
    capacity != 0 && capacity <= Semaphore::MAX_PERMITS
}

fn encoded_length(event: &EventEnvelope) -> Result<u32, ChannelError> {
    let mut counter = EncodedLengthCounter::default();
    serde_json::to_writer(&mut counter, event).map_err(|_| ChannelError::EventEncodingFailed)?;
    u32::try_from(counter.length).map_err(|_| ChannelError::CapacityRejected)
}

#[derive(Default)]
struct EncodedLengthCounter {
    length: usize,
}

impl Write for EncodedLengthCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.length = self
            .length
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("encoded event length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct LifecycleSink {
    sender: mpsc::Sender<LifecycleEvent>,
}

impl LifecycleSink {
    pub async fn send(&self, event: LifecycleEvent) -> Result<(), ChannelError> {
        self.sender
            .send(event)
            .await
            .map_err(|_| ChannelError::ReceiverClosed)
    }
}

pub struct LifecycleReceiver {
    receiver: mpsc::Receiver<LifecycleEvent>,
}

impl LifecycleReceiver {
    pub async fn recv(&mut self) -> Option<LifecycleEvent> {
        self.receiver.recv().await
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ChannelError {
    #[error("channel capacity must be nonzero and fit the runtime")]
    InvalidCapacity,
    #[error("channel capacity is exhausted")]
    CapacityRejected,
    #[error("channel receiver is closed")]
    ReceiverClosed,
    #[error("connector command identifier is duplicated or out of order")]
    CommandSequenceRegression,
    #[error("required book stream is invalid until resynchronization")]
    ResynchronizationRequired,
    #[error("normalized event source does not match its raw receipt")]
    RawSourceMismatch,
    #[error("normalized event connection epoch does not match its raw receipt")]
    RawConnectionEpochMismatch,
    #[error("normalized event raw hash does not match its durable receipt")]
    RawPayloadHashMismatch,
    #[error("normalized event could not be encoded for byte accounting")]
    EventEncodingFailed,
    #[error("normalized channel invalidation state is poisoned")]
    StatePoisoned,
    #[error("normalized channel invalidation registry is full")]
    InvalidationCapacityExceeded,
    #[error("book stream is not in the admitted subscription registry")]
    UnknownBookStream,
    #[error("book stream registry is duplicated or exceeds its bound")]
    InvalidBookStreamRegistry,
    #[error("snapshot does not satisfy the matching invalidated book stream")]
    SnapshotDoesNotSatisfyInvalidation,
    #[error("loss policy is invalid or exceeds its bound")]
    InvalidLossPolicy,
    #[error("loss sampling stream registry capacity is exhausted")]
    SamplingRegistryCapacityExceeded,
    #[error("required integrity traffic cannot be declared lossy")]
    RequiredEventCannotBeLossy,
    #[error("lossy delivery was not declared with the exact policy")]
    UndeclaredLossPolicy,
    #[error("control plane is already terminal")]
    AlreadyTerminated,
    #[error("only shutdown is a terminal command")]
    InvalidTerminalCommand,
    #[error("control plane was cancelled")]
    Cancelled,
    #[error("control plane is already attached to a command channel")]
    ControlAlreadyAttached,
}
