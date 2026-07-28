//! Two-phase, byte-and-item bounded raw capture acknowledgement.

use std::{
    fmt,
    num::{NonZeroU32, NonZeroU64},
    sync::Arc,
};

use domain::{SourceId, UnixNanos};
use raw_wal::manager::{
    VerifiedRecoveredRecord, WalAppendAuthority, WalAppendProof, WalIdentity, WalPosition,
};
use thiserror::Error;
use tokio::sync::{
    OwnedSemaphorePermit, Semaphore,
    mpsc::{self, error::TrySendError},
    oneshot,
};

pub const MAX_RAW_CAPTURE_BYTES: usize = 16 * 1024 * 1024;

/// One pre-parse provider payload and its capture-plane identity.
#[derive(Eq, PartialEq)]
pub struct RawCapture {
    source: SourceId,
    stream_id: NonZeroU32,
    connection_epoch: NonZeroU64,
    record_sequence: NonZeroU64,
    receive_wall_time: UnixNanos,
    receive_monotonic_ns: u64,
    payload: Box<[u8]>,
}

impl fmt::Debug for RawCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawCapture")
            .field("source", &self.source)
            .field("stream_id", &self.stream_id)
            .field("connection_epoch", &self.connection_epoch)
            .field("record_sequence", &self.record_sequence)
            .field("receive_wall_time", &self.receive_wall_time)
            .field("receive_monotonic_ns", &self.receive_monotonic_ns)
            .field("payload_length", &self.payload.len())
            .field("payload", &"<redacted>")
            .finish()
    }
}

impl RawCapture {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        source: SourceId,
        stream_id: NonZeroU32,
        connection_epoch: NonZeroU64,
        record_sequence: NonZeroU64,
        receive_wall_time: UnixNanos,
        receive_monotonic_ns: u64,
        payload: Box<[u8]>,
    ) -> Result<Self, RawCaptureError> {
        if payload.is_empty() || payload.len() > MAX_RAW_CAPTURE_BYTES {
            return Err(RawCaptureError::InvalidPayloadSize);
        }
        Ok(Self {
            source,
            stream_id,
            connection_epoch,
            record_sequence,
            receive_wall_time,
            receive_monotonic_ns,
            payload,
        })
    }

    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn stream_id(&self) -> NonZeroU32 {
        self.stream_id
    }

    pub const fn connection_epoch(&self) -> NonZeroU64 {
        self.connection_epoch
    }

    pub const fn record_sequence(&self) -> NonZeroU64 {
        self.record_sequence
    }

    pub const fn receive_wall_time(&self) -> UnixNanos {
        self.receive_wall_time
    }

    pub const fn receive_monotonic_ns(&self) -> u64 {
        self.receive_monotonic_ns
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// Canonical generation-aware source identity stored in WAL stream descriptors.
pub fn wal_stream_source_identity(source: &SourceId) -> String {
    format!(
        "source-v1:{}:{}:{}",
        source.kind() as u8,
        source.name(),
        source.generation()
    )
}

/// Receipt that can only be constructed by acknowledging a raw worker request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableRawReference {
    wal_identity: WalIdentity,
    position: WalPosition,
    payload_hash: [u8; 32],
    source: SourceId,
    stream_id: NonZeroU32,
    connection_epoch: NonZeroU64,
    record_sequence: NonZeroU64,
    receive_wall_time: UnixNanos,
    receive_monotonic_ns: u64,
}

impl DurableRawReference {
    pub fn try_from_recovered(
        source: SourceId,
        record: &VerifiedRecoveredRecord<'_>,
    ) -> Result<Self, RawCaptureError> {
        let metadata = record.metadata();
        let stream_id =
            NonZeroU32::new(metadata.stream_id).ok_or(RawCaptureError::WalProofMismatch)?;
        let connection_epoch =
            NonZeroU64::new(metadata.connection_epoch).ok_or(RawCaptureError::WalProofMismatch)?;
        let record_sequence =
            NonZeroU64::new(metadata.record_sequence).ok_or(RawCaptureError::WalProofMismatch)?;
        if metadata.flags != 0
            || record.stream_source_name() != wal_stream_source_identity(&source)
            || record.payload().is_empty()
            || record.payload().len() > MAX_RAW_CAPTURE_BYTES
            || record.payload_hash() != blake3::hash(record.payload()).as_bytes()
        {
            return Err(RawCaptureError::WalProofMismatch);
        }
        Ok(Self {
            wal_identity: record.wal_identity(),
            position: record.position(),
            payload_hash: *record.payload_hash(),
            source,
            stream_id,
            connection_epoch,
            record_sequence,
            receive_wall_time: UnixNanos::new(metadata.receive_wall_time_ns),
            receive_monotonic_ns: metadata.receive_monotonic_time_ns,
        })
    }

    pub const fn wal_identity(&self) -> WalIdentity {
        self.wal_identity
    }

    pub const fn position(&self) -> WalPosition {
        self.position
    }

    pub const fn payload_hash(&self) -> &[u8; 32] {
        &self.payload_hash
    }

    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn stream_id(&self) -> NonZeroU32 {
        self.stream_id
    }

    pub const fn connection_epoch(&self) -> NonZeroU64 {
        self.connection_epoch
    }

    pub const fn record_sequence(&self) -> NonZeroU64 {
        self.record_sequence
    }

    pub const fn receive_wall_time(&self) -> UnixNanos {
        self.receive_wall_time
    }

    pub const fn receive_monotonic_ns(&self) -> u64 {
        self.receive_monotonic_ns
    }
}

/// Owner used to construct the client/worker halves exactly once.
pub struct DurableRawCaptureChannel {
    client: DurableRawCaptureClient,
    worker: DurableRawCaptureReceiver,
}

impl DurableRawCaptureChannel {
    pub fn new(
        wal_authority: WalAppendAuthority,
        source: SourceId,
        stream_id: NonZeroU32,
        item_capacity: usize,
        byte_capacity: usize,
    ) -> Result<Self, RawCaptureError> {
        let byte_capacity =
            u32::try_from(byte_capacity).map_err(|_| RawCaptureError::InvalidCapacity)?;
        if item_capacity == 0 || item_capacity > Semaphore::MAX_PERMITS || byte_capacity == 0 {
            return Err(RawCaptureError::InvalidCapacity);
        }
        let (sender, receiver) = mpsc::channel(item_capacity);
        let byte_budget = Arc::new(Semaphore::new(
            usize::try_from(byte_capacity).map_err(|_| RawCaptureError::InvalidCapacity)?,
        ));
        Ok(Self {
            client: DurableRawCaptureClient {
                wal_authority,
                source,
                stream_id,
                sender,
                byte_budget,
                byte_capacity,
            },
            worker: DurableRawCaptureReceiver { receiver },
        })
    }

    pub fn split(self) -> (DurableRawCaptureClient, DurableRawCaptureReceiver) {
        (self.client, self.worker)
    }
}

#[derive(Clone)]
pub struct DurableRawCaptureClient {
    wal_authority: WalAppendAuthority,
    source: SourceId,
    stream_id: NonZeroU32,
    sender: mpsc::Sender<PendingRawCapture>,
    byte_budget: Arc<Semaphore>,
    byte_capacity: u32,
}

impl DurableRawCaptureClient {
    pub const fn stream_id(&self) -> NonZeroU32 {
        self.stream_id
    }

    pub async fn submit(
        &self,
        capture: RawCapture,
    ) -> Result<DurableRawReference, RawCaptureError> {
        self.validate_capture_binding(&capture)?;
        let payload_len = u32::try_from(capture.payload.len())
            .map_err(|_| RawCaptureError::InvalidPayloadSize)?;
        if payload_len > self.byte_capacity {
            return Err(RawCaptureError::CapacityRejected);
        }
        let permit = Arc::clone(&self.byte_budget)
            .acquire_many_owned(payload_len)
            .await
            .map_err(|_| RawCaptureError::ChannelClosed)?;
        let receipt = self.send_with_permit(capture, permit).await?;
        receipt.wait().await
    }

    pub fn try_submit(&self, capture: RawCapture) -> Result<RawReceiptFuture, RawCaptureError> {
        self.validate_capture_binding(&capture)?;
        let payload_len = u32::try_from(capture.payload.len())
            .map_err(|_| RawCaptureError::InvalidPayloadSize)?;
        if payload_len > self.byte_capacity {
            return Err(RawCaptureError::CapacityRejected);
        }
        let permit = Arc::clone(&self.byte_budget)
            .try_acquire_many_owned(payload_len)
            .map_err(|_| RawCaptureError::CapacityRejected)?;
        let (receipt_sender, receipt_receiver) = oneshot::channel();
        let request = PendingRawCapture {
            wal_authority: self.wal_authority.clone(),
            capture,
            receipt: receipt_sender,
            _byte_permit: permit,
        };
        self.sender.try_send(request).map_err(|error| match error {
            TrySendError::Full(_) => RawCaptureError::CapacityRejected,
            TrySendError::Closed(_) => RawCaptureError::ChannelClosed,
        })?;
        Ok(RawReceiptFuture {
            receiver: receipt_receiver,
        })
    }

    async fn send_with_permit(
        &self,
        capture: RawCapture,
        permit: OwnedSemaphorePermit,
    ) -> Result<RawReceiptFuture, RawCaptureError> {
        let (receipt_sender, receipt_receiver) = oneshot::channel();
        self.sender
            .send(PendingRawCapture {
                wal_authority: self.wal_authority.clone(),
                capture,
                receipt: receipt_sender,
                _byte_permit: permit,
            })
            .await
            .map_err(|_| RawCaptureError::ChannelClosed)?;
        Ok(RawReceiptFuture {
            receiver: receipt_receiver,
        })
    }

    fn validate_capture_binding(&self, capture: &RawCapture) -> Result<(), RawCaptureError> {
        if capture.source() != &self.source {
            return Err(RawCaptureError::CaptureSourceMismatch);
        }
        if capture.stream_id() != self.stream_id {
            return Err(RawCaptureError::CaptureStreamMismatch);
        }
        Ok(())
    }
}

pub struct DurableRawCaptureReceiver {
    receiver: mpsc::Receiver<PendingRawCapture>,
}

impl DurableRawCaptureReceiver {
    pub async fn recv(&mut self) -> Option<PendingRawCapture> {
        self.receiver.recv().await
    }
}

/// Worker-side request. Dropping it rejects the connector's pending receipt.
pub struct PendingRawCapture {
    wal_authority: WalAppendAuthority,
    capture: RawCapture,
    receipt: oneshot::Sender<Result<DurableRawReference, RawCaptureError>>,
    _byte_permit: OwnedSemaphorePermit,
}

impl PendingRawCapture {
    pub const fn capture(&self) -> &RawCapture {
        &self.capture
    }

    pub fn acknowledge(self, proof: WalAppendProof) -> Result<(), RawCaptureError> {
        let metadata = proof.metadata();
        let payload_hash = *blake3::hash(self.capture.payload()).as_bytes();
        if !self.wal_authority.validates(&proof)
            || metadata.flags != 0
            || proof.stream_source_name() != wal_stream_source_identity(&self.capture.source)
            || metadata.stream_id != self.capture.stream_id.get()
            || metadata.connection_epoch != self.capture.connection_epoch.get()
            || metadata.record_sequence != self.capture.record_sequence.get()
            || metadata.receive_wall_time_ns != self.capture.receive_wall_time.value()
            || metadata.receive_monotonic_time_ns != self.capture.receive_monotonic_ns
            || proof.payload_hash() != &payload_hash
        {
            self.receipt
                .send(Err(RawCaptureError::WalProofMismatch))
                .map_err(|_| RawCaptureError::ReceiptAbandoned)?;
            return Err(RawCaptureError::WalProofMismatch);
        }
        let reference = DurableRawReference {
            wal_identity: proof.wal_identity(),
            position: proof.position(),
            payload_hash,
            source: self.capture.source,
            stream_id: self.capture.stream_id,
            connection_epoch: self.capture.connection_epoch,
            record_sequence: self.capture.record_sequence,
            receive_wall_time: self.capture.receive_wall_time,
            receive_monotonic_ns: self.capture.receive_monotonic_ns,
        };
        self.receipt
            .send(Ok(reference))
            .map_err(|_| RawCaptureError::ReceiptAbandoned)
    }

    pub fn reject(self, reason: WalRejectionReason) -> Result<(), RawCaptureError> {
        self.receipt
            .send(Err(RawCaptureError::WalRejected(reason)))
            .map_err(|_| RawCaptureError::ReceiptAbandoned)
    }
}

/// Awaitable durable receipt returned by nonblocking submission.
pub struct RawReceiptFuture {
    receiver: oneshot::Receiver<Result<DurableRawReference, RawCaptureError>>,
}

impl RawReceiptFuture {
    pub async fn wait(self) -> Result<DurableRawReference, RawCaptureError> {
        self.receiver
            .await
            .map_err(|_| RawCaptureError::WorkerLost)?
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum RawCaptureError {
    #[error("raw capture payload is empty or exceeds the maximum")]
    InvalidPayloadSize,
    #[error("raw capture source does not match the channel binding")]
    CaptureSourceMismatch,
    #[error("raw capture stream does not match the channel binding")]
    CaptureStreamMismatch,
    #[error("raw capture channel capacity is invalid")]
    InvalidCapacity,
    #[error("raw capture capacity is exhausted")]
    CapacityRejected,
    #[error("raw capture channel is closed")]
    ChannelClosed,
    #[error("WAL rejected the raw capture: {0:?}")]
    WalRejected(WalRejectionReason),
    #[error("WAL append proof does not match the pending raw capture")]
    WalProofMismatch,
    #[error("raw capture worker stopped before acknowledging")]
    WorkerLost,
    #[error("connector abandoned the raw receipt")]
    ReceiptAbandoned,
}

/// Bounded reason supplied by the WAL worker when persistence cannot complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalRejectionReason {
    CapacityExhausted,
    StorageUnavailable,
    DirectoryIdentityViolation,
    Corruption,
    CaptureContractMismatch,
    SourceIntegrityViolation,
}
