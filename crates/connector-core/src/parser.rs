//! Typed parser rejection records linked to durable raw capture.

use crate::DurableRawReference;

/// Bounded parse-failure classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseRejectionReason {
    InvalidJson,
    SchemaMismatch,
    NumericOutOfRange,
    CollectionLimitExceeded,
    UnsupportedMessage,
}

/// A parse rejection never retains or exposes the provider payload itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseRejection {
    raw: DurableRawReference,
    reason: ParseRejectionReason,
}

impl ParseRejection {
    pub const fn new(raw: DurableRawReference, reason: ParseRejectionReason) -> Self {
        Self { raw, reason }
    }

    pub const fn raw(&self) -> &DurableRawReference {
        &self.raw
    }

    pub const fn reason(&self) -> ParseRejectionReason {
        self.reason
    }
}
