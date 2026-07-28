use crate::MAX_SCALE;
use thiserror::Error;

/// Failures produced by fixed-decimal parsing and checked arithmetic.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DecimalError {
    #[error("decimal text is not canonical")]
    InvalidSyntax,
    #[error("decimal scale exceeds {MAX_SCALE}")]
    ScaleTooLarge,
    #[error("fixed-decimal arithmetic overflow")]
    ArithmeticOverflow,
    #[error("rescaling would lose precision")]
    PrecisionLoss,
    #[error("{0} must be positive")]
    NonPositive(&'static str),
    #[error("{0} must not be negative")]
    Negative(&'static str),
}
