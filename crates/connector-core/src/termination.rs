//! Total connector termination causes returned to the supervisor.

use std::num::NonZeroU64;

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorTermination {
    RequestedShutdown {
        command_id: NonZeroU64,
    },
    RecoverableDisconnect(RecoverableDisconnect),
    CapacityRejected(CapacityRejection),
    Quarantined(QuarantineReason),
    SupervisorQuarantine {
        command_id: NonZeroU64,
        reason: QuarantineReason,
    },
    SupervisorCommand {
        command_id: NonZeroU64,
        kind: SupervisorCommandKind,
    },
    LocalWalFailure(WalRejectionReason),
    LocalWalWorkerLost,
    SupervisorLost,
    LocalOutputFailed,
    Cancelled,
    Fatal(FatalConnectorError),
}

use crate::raw_capture::WalRejectionReason;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorCommandKind {
    Resynchronize,
    Recover,
    RenewConnection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoverableDisconnect {
    RemoteClosed,
    ConnectionExpired,
    TransportUnavailable,
    ProtocolReset,
    RateLimited { retry_after_ms: Option<u32> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapacityRejection {
    RawCapture,
    NormalizedOutput,
    LifecycleOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuarantineReason {
    SchemaViolation,
    RepeatedSchemaViolation,
    RepeatedSequenceViolation,
    IntegrityViolation,
    SupervisorPolicy,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FatalConnectorError {
    #[error("source schema is incompatible")]
    SchemaIncompatible,
    #[error("authentication or subscription configuration is invalid")]
    InvalidConfiguration,
    #[error("source protocol contract is unsupported")]
    UnsupportedProtocol,
}
