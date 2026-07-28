//! Canonical generation-aware identities and instrument metadata.

mod id;
mod instrument;
mod source;

pub use id::{AssetId, AssetNamespace, VenueId};
pub use instrument::{
    ContractKind, ContractValueUnit, InstrumentDefinition, InstrumentDefinitionInput, InstrumentId,
    OptionSide, ProductType,
};
pub use source::{SourceId, SourceKind};

use fixed_decimal::DecimalError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DomainError {
    #[error("invalid {field} identity field")]
    InvalidIdentity { field: &'static str },
    #[error("identity generation must be nonzero")]
    GenerationZero,
    #[error("invalid instrument {field}")]
    InvalidInstrument { field: &'static str },
    #[error("invalid instrument lifecycle: {field}")]
    InvalidLifecycle { field: &'static str },
    #[error("decimal domain failure: {0}")]
    Decimal(#[from] DecimalError),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixNanos(i64);

impl UnixNanos {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

fn ensure_generation(generation: u32) -> Result<(), DomainError> {
    if generation == 0 {
        Err(DomainError::GenerationZero)
    } else {
        Ok(())
    }
}
