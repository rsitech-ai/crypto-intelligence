//! Typed, fail-closed data-quality state and lineage.

pub mod source_health;

pub use source_health::{
    PreparedQualityTransition, QualityCause, QualityError, QualityEvent, SourceHealthState,
    SourceHealthTracker,
};
