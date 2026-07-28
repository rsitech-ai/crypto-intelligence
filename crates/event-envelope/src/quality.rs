//! Forward-compatible source and record quality signals.

use serde::{Deserialize, Serialize};

pub const MAX_QUALITY_SCORE_PPM: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QualityFlags(u64);

impl QualityFlags {
    pub const NONE: Self = Self(0);
    pub const STALE: Self = Self(1 << 0);
    pub const PARTIAL: Self = Self(1 << 1);
    pub const CLOCK_UNCERTAIN: Self = Self(1 << 2);
    pub const SOURCE_DEGRADED: Self = Self(1 << 3);

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }
}
