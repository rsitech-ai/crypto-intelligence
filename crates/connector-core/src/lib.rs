//! Fail-closed contracts shared by native market-data connectors.
//!
//! This crate owns no sockets and performs no retries. It defines the bounded
//! capture, publication, lifecycle, command, and termination boundaries that a
//! later supervisor wires to venue-specific implementations.

mod capabilities;
mod channel;
mod derivative_authority;
mod lifecycle;
mod parser;
mod raw_capture;
mod termination;

pub use capabilities::*;
pub use channel::*;
pub use derivative_authority::*;
pub use lifecycle::*;
pub use parser::*;
pub use raw_capture::*;
pub use termination::*;

use std::{future::Future, num::NonZeroU64, pin::Pin};

use domain::{SourceId, UnixNanos};

/// Heap-allocated future required for heterogeneous connector supervision.
pub type ConnectorFuture = Pin<Box<dyn Future<Output = ConnectorTermination> + Send + 'static>>;

/// Dyn-compatible source adapter boundary.
pub trait MarketDataConnector: Send + 'static {
    fn source_id(&self) -> &SourceId;
    fn capabilities(&self) -> &ConnectorCapabilities;
    fn run(self: Box<Self>, context: ConnectorContext) -> ConnectorFuture;
}

/// Policy-preserving resources supplied to one connector connection epoch.
pub struct ConnectorContext {
    source_id: SourceId,
    connection_epoch: NonZeroU64,
    commands: ConnectorCommandReceiver,
    raw_capture: DurableRawCaptureClient,
    normalized: BoundedNormalizedSink,
    lifecycle: LifecycleSink,
    lifecycle_tracker: LifecycleTracker,
    cancellation: CancellationReceiver,
}

impl ConnectorContext {
    pub fn new(
        source_id: SourceId,
        connection_epoch: NonZeroU64,
        commands: ConnectorCommandReceiver,
        raw_capture: DurableRawCaptureClient,
        normalized: BoundedNormalizedSink,
        lifecycle: LifecycleSink,
    ) -> Self {
        let cancellation = commands.interruption_receiver();
        Self {
            source_id: source_id.clone(),
            connection_epoch,
            commands,
            raw_capture,
            normalized,
            lifecycle,
            lifecycle_tracker: LifecycleTracker::new(source_id, connection_epoch),
            cancellation,
        }
    }

    pub const fn connection_epoch(&self) -> NonZeroU64 {
        self.connection_epoch
    }

    pub const fn raw_capture_stream_id(&self) -> std::num::NonZeroU32 {
        self.raw_capture.stream_id()
    }

    pub async fn next_command(&mut self) -> Result<ConnectorCommand, ConnectorTermination> {
        match self.commands.recv().await {
            Ok(Some(ConnectorCommand::Shutdown { id })) => {
                Err(ConnectorTermination::RequestedShutdown { command_id: id })
            }
            Ok(Some(ConnectorCommand::Quarantine { id, reason })) => {
                Err(ConnectorTermination::SupervisorQuarantine {
                    command_id: id,
                    reason,
                })
            }
            Ok(Some(command)) => Ok(command),
            Ok(None) => Err(ConnectorTermination::SupervisorLost),
            Err(ChannelError::Cancelled) => Err(ConnectorTermination::Cancelled),
            Err(_) => Err(ConnectorTermination::Fatal(
                FatalConnectorError::InvalidConfiguration,
            )),
        }
    }

    pub async fn capture_raw(
        &mut self,
        capture: RawCapture,
    ) -> Result<DurableRawReference, ConnectorTermination> {
        if capture.source() != &self.source_id
            || capture.connection_epoch() != self.connection_epoch
        {
            return Err(ConnectorTermination::Fatal(
                FatalConnectorError::InvalidConfiguration,
            ));
        }
        if let Some(interrupt) = self.cancellation.current_interrupt() {
            return Err(map_interrupt(interrupt));
        }
        tokio::select! {
            biased;
            interrupt = self.cancellation.interrupted() => {
                Err(map_interrupt(interrupt))
            }
            command = self.commands.recv() => {
                Err(map_control_result(command))
            }
            receipt = self.raw_capture.submit(capture) => {
                receipt.map_err(map_raw_capture_error)
            }
        }
    }

    pub async fn publish_normalized(
        &mut self,
        output: NormalizedOutput,
    ) -> Result<(), ConnectorTermination> {
        if output.raw().source() != &self.source_id
            || output.raw().connection_epoch() != self.connection_epoch
        {
            return Err(ConnectorTermination::Fatal(
                FatalConnectorError::InvalidConfiguration,
            ));
        }
        if let Some(interrupt) = self.cancellation.current_interrupt() {
            return Err(map_interrupt(interrupt));
        }
        tokio::select! {
            biased;
            interrupt = self.cancellation.interrupted() => {
                Err(map_interrupt(interrupt))
            }
            command = self.commands.recv() => {
                Err(map_control_result(command))
            }
            result = self.normalized.send(output) => {
                result.map_err(map_normalized_error)
            }
        }
    }

    pub async fn publish_lifecycle(
        &mut self,
        to: ConnectorState,
        cause: LifecycleCause,
        observed_at: UnixNanos,
    ) -> Result<(), ConnectorTermination> {
        let prepared = self
            .lifecycle_tracker
            .prepare(to, cause, observed_at)
            .map_err(|_| ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration))?;
        if let Some(interrupt) = self.cancellation.current_interrupt() {
            return Err(map_interrupt(interrupt));
        }
        tokio::select! {
            biased;
            interrupt = self.cancellation.interrupted() => {
                Err(map_interrupt(interrupt))
            }
            command = self.commands.recv() => {
                Err(map_control_result(command))
            }
            result = self.lifecycle.send(prepared.event().clone()) => {
                result.map_err(|_| ConnectorTermination::LocalOutputFailed)?;
                self.lifecycle_tracker.commit(prepared).map_err(|_| {
                    ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration)
                })
            }
        }
    }

    pub const fn lifecycle_state(&self) -> ConnectorState {
        self.lifecycle_tracker.state()
    }
}

fn map_interrupt(interrupt: ContextInterrupt) -> ConnectorTermination {
    match interrupt {
        ContextInterrupt::Cancelled => ConnectorTermination::Cancelled,
        ContextInterrupt::RequestedShutdown { command_id } => {
            ConnectorTermination::RequestedShutdown { command_id }
        }
        ContextInterrupt::RequestedQuarantine { command_id, reason } => {
            ConnectorTermination::SupervisorQuarantine { command_id, reason }
        }
    }
}

fn map_control_result(
    result: Result<Option<ConnectorCommand>, ChannelError>,
) -> ConnectorTermination {
    match result {
        Ok(Some(ConnectorCommand::Shutdown { id })) => {
            ConnectorTermination::RequestedShutdown { command_id: id }
        }
        Ok(Some(ConnectorCommand::Quarantine { id, reason })) => {
            ConnectorTermination::SupervisorQuarantine {
                command_id: id,
                reason,
            }
        }
        Ok(Some(ConnectorCommand::Resynchronize { id, .. })) => {
            ConnectorTermination::SupervisorCommand {
                command_id: id,
                kind: SupervisorCommandKind::Resynchronize,
            }
        }
        Ok(Some(ConnectorCommand::Recover { id })) => ConnectorTermination::SupervisorCommand {
            command_id: id,
            kind: SupervisorCommandKind::Recover,
        },
        Ok(Some(ConnectorCommand::RenewConnection { id })) => {
            ConnectorTermination::SupervisorCommand {
                command_id: id,
                kind: SupervisorCommandKind::RenewConnection,
            }
        }
        Ok(None) => ConnectorTermination::SupervisorLost,
        Err(_) => ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration),
    }
}

fn map_raw_capture_error(error: RawCaptureError) -> ConnectorTermination {
    match error {
        RawCaptureError::CapacityRejected => {
            ConnectorTermination::CapacityRejected(CapacityRejection::RawCapture)
        }
        RawCaptureError::ChannelClosed
        | RawCaptureError::WorkerLost
        | RawCaptureError::ReceiptAbandoned => ConnectorTermination::LocalWalWorkerLost,
        RawCaptureError::WalRejected(WalRejectionReason::CapacityExhausted) => {
            ConnectorTermination::CapacityRejected(CapacityRejection::RawCapture)
        }
        RawCaptureError::WalRejected(
            reason @ (WalRejectionReason::StorageUnavailable
            | WalRejectionReason::DirectoryIdentityViolation
            | WalRejectionReason::Corruption),
        ) => ConnectorTermination::LocalWalFailure(reason),
        RawCaptureError::WalRejected(WalRejectionReason::CaptureContractMismatch) => {
            ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration)
        }
        RawCaptureError::WalRejected(WalRejectionReason::SourceIntegrityViolation) => {
            ConnectorTermination::Quarantined(QuarantineReason::IntegrityViolation)
        }
        RawCaptureError::WalProofMismatch => {
            ConnectorTermination::Quarantined(QuarantineReason::IntegrityViolation)
        }
        RawCaptureError::InvalidPayloadSize => {
            ConnectorTermination::Fatal(FatalConnectorError::SchemaIncompatible)
        }
        RawCaptureError::InvalidCapacity
        | RawCaptureError::CaptureSourceMismatch
        | RawCaptureError::CaptureStreamMismatch => {
            ConnectorTermination::Fatal(FatalConnectorError::InvalidConfiguration)
        }
    }
}

fn map_normalized_error(error: ChannelError) -> ConnectorTermination {
    match error {
        ChannelError::CapacityRejected | ChannelError::ResynchronizationRequired => {
            ConnectorTermination::CapacityRejected(CapacityRejection::NormalizedOutput)
        }
        ChannelError::ReceiverClosed => ConnectorTermination::LocalOutputFailed,
        ChannelError::DerivativeRawAuthorityMismatch => {
            ConnectorTermination::Quarantined(QuarantineReason::IntegrityViolation)
        }
        ChannelError::InvalidCapacity
        | ChannelError::CommandSequenceRegression
        | ChannelError::RawSourceMismatch
        | ChannelError::RawConnectionEpochMismatch
        | ChannelError::RawPayloadHashMismatch
        | ChannelError::RawReceiveWallTimeMismatch
        | ChannelError::RawReceiveMonotonicTimeMismatch
        | ChannelError::DerivativeAuthorityRequired
        | ChannelError::EventEncodingFailed
        | ChannelError::StatePoisoned
        | ChannelError::InvalidationCapacityExceeded
        | ChannelError::UnknownBookStream
        | ChannelError::InvalidBookStreamRegistry
        | ChannelError::SnapshotDoesNotSatisfyInvalidation
        | ChannelError::InvalidLossPolicy
        | ChannelError::SamplingRegistryCapacityExceeded
        | ChannelError::RequiredEventCannotBeLossy
        | ChannelError::UndeclaredLossPolicy
        | ChannelError::AlreadyTerminated
        | ChannelError::InvalidTerminalCommand
        | ChannelError::Cancelled
        | ChannelError::ControlAlreadyAttached => {
            ConnectorTermination::Fatal(FatalConnectorError::SchemaIncompatible)
        }
    }
}
