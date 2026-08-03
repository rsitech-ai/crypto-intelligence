//! Versioned discrete hazard bucket and forecast-horizon definitions.

use crate::HazardError;

const BUCKET_SPEC_DOMAIN: &[u8] = b"cmti:hazard-bucket-spec:v1\0";

/// Immutable Phase 2 Task 14 endpoint-only contract.
const V1_EDGES_SECONDS: [u64; 4] = [900, 3_600, 14_400, 86_400];

/// Phase 4 production grid: 1m to 15m, 5m to 1h, 15m to 4h, then 1h to 24h.
const PRODUCTION_V2_EDGES_SECONDS: [u64; 56] = [
    60, 120, 180, 240, 300, 360, 420, 480, 540, 600, 660, 720, 780, 840, 900, 1_200, 1_500, 1_800,
    2_100, 2_400, 2_700, 3_000, 3_300, 3_600, 4_500, 5_400, 6_300, 7_200, 8_100, 9_000, 9_900,
    10_800, 11_700, 12_600, 13_500, 14_400, 18_000, 21_600, 25_200, 28_800, 32_400, 36_000, 39_600,
    43_200, 46_800, 50_400, 54_000, 57_600, 61_200, 64_800, 68_400, 72_000, 75_600, 79_200, 82_800,
    86_400,
];

const FORECAST_HORIZONS_SECONDS: [u64; 4] = [900, 3_600, 14_400, 86_400];

/// Production policy for a requested horizon between modeled bucket edges.
///
/// No within-bucket event-time distribution is part of the model contract, so
/// production refuses interpolation rather than inventing one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HorizonInterpolationPolicy {
    ExactBucketEdgeOnly,
}

/// Event offsets equal to an edge belong to the bucket ending at that edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BucketBoundaryConvention {
    InclusiveEnd,
}

/// An immutable versioned sequence of inclusive bucket end times.
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

    /// Returns the Phase 4 production bucket grid without mutating v1 model
    /// semantics or artifact identity.
    #[must_use]
    pub const fn production_v2() -> Self {
        Self {
            version: 2,
            edges_seconds: &PRODUCTION_V2_EDGES_SECONDS,
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

    /// Returns the four public forecast horizons, distinct from model bucket
    /// resolution.
    #[must_use]
    pub const fn forecast_horizons_seconds(self) -> &'static [u64] {
        &FORECAST_HORIZONS_SECONDS
    }

    #[must_use]
    pub const fn horizon_policy(self) -> HorizonInterpolationPolicy {
        HorizonInterpolationPolicy::ExactBucketEdgeOnly
    }

    #[must_use]
    pub const fn boundary_convention(self) -> BucketBoundaryConvention {
        BucketBoundaryConvention::InclusiveEnd
    }

    /// Canonical identity for every behavior-bearing bucket and horizon field.
    #[must_use]
    pub fn fingerprint(self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(BUCKET_SPEC_DOMAIN);
        hash_u64(&mut hasher, u64::from(self.version));
        hash_u64(
            &mut hasher,
            u64::try_from(self.edges_seconds.len()).unwrap_or(u64::MAX),
        );
        for edge in self.edges_seconds {
            hash_u64(&mut hasher, *edge);
        }
        hash_u64(
            &mut hasher,
            u64::try_from(self.forecast_horizons_seconds().len()).unwrap_or(u64::MAX),
        );
        for horizon in self.forecast_horizons_seconds() {
            hash_u64(&mut hasher, *horizon);
        }
        hasher.update(&[match self.boundary_convention() {
            BucketBoundaryConvention::InclusiveEnd => 1,
        }]);
        hasher.update(&[match self.horizon_policy() {
            HorizonInterpolationPolicy::ExactBucketEdgeOnly => 1,
        }]);
        *hasher.finalize().as_bytes()
    }

    /// Resolves an exact modeled bucket edge for a supported product horizon.
    pub fn bucket_index_for_horizon(self, horizon_seconds: u64) -> Result<usize, HazardError> {
        if !self.forecast_horizons_seconds().contains(&horizon_seconds) {
            return Err(HazardError::UnsupportedHorizon);
        }
        self.edges_seconds
            .binary_search(&horizon_seconds)
            .map_err(|_| HazardError::UnsupportedHorizon)
    }
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}
