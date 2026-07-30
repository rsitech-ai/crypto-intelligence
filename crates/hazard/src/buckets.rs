//! Versioned discrete hazard bucket definitions.

/// The Task 14 v1 forecast horizons: 15 minutes, 1 hour, 4 hours, and 24 hours.
const V1_EDGES_SECONDS: [u64; 4] = [900, 3_600, 14_400, 86_400];

/// An immutable versioned sequence of right-open bucket end times.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BucketSpec {
    version: u32,
    edges_seconds: &'static [u64],
}

impl BucketSpec {
    /// Returns the initial Task 14 bucket contract.
    #[must_use]
    pub const fn v1() -> Self {
        Self {
            version: 1,
            edges_seconds: &V1_EDGES_SECONDS,
        }
    }

    #[must_use]
    pub const fn version(self) -> u32 {
        self.version
    }

    #[must_use]
    pub const fn edges_seconds(self) -> &'static [u64] {
        self.edges_seconds
    }

    #[must_use]
    pub const fn len(self) -> usize {
        self.edges_seconds.len()
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.edges_seconds.is_empty()
    }
}
