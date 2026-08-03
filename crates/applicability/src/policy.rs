//! Deterministic availability and abstention policy.

use serde::{Deserialize, Serialize};

use crate::ApplicabilityError;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Available,
    Degraded,
    Experimental,
    OutOfDistribution,
    InsufficientData,
    SourceUnhealthy,
    ModelIncompatible,
}

impl Availability {
    pub const ALL: [Self; 7] = [
        Self::Available,
        Self::Degraded,
        Self::Experimental,
        Self::OutOfDistribution,
        Self::InsufficientData,
        Self::SourceUnhealthy,
        Self::ModelIncompatible,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Degraded => "degraded",
            Self::Experimental => "experimental",
            Self::OutOfDistribution => "out_of_distribution",
            Self::InsufficientData => "insufficient_data",
            Self::SourceUnhealthy => "source_unhealthy",
            Self::ModelIncompatible => "model_incompatible",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilityThresholds {
    pub mahalanobis_degraded: f64,
    pub mahalanobis_ood: f64,
    pub nearest_neighbor_degraded: f64,
    pub nearest_neighbor_ood: f64,
    pub residual_experimental: f64,
    pub residual_ood: f64,
    pub coefficient_drift_experimental: f64,
    pub coefficient_drift_incompatible: f64,
    pub quality_available: f64,
    pub quality_degraded: f64,
    pub critical_quality_available: f64,
    pub critical_quality_degraded: f64,
    pub model_age_experimental_ns: i64,
    pub model_age_incompatible_ns: i64,
}

impl Default for ApplicabilityThresholds {
    fn default() -> Self {
        Self {
            mahalanobis_degraded: 9.0,
            mahalanobis_ood: 25.0,
            nearest_neighbor_degraded: 3.0,
            nearest_neighbor_ood: 6.0,
            residual_experimental: 2.0,
            residual_ood: 4.0,
            coefficient_drift_experimental: 0.15,
            coefficient_drift_incompatible: 0.30,
            quality_available: 0.90,
            quality_degraded: 0.70,
            critical_quality_available: 0.90,
            critical_quality_degraded: 0.60,
            model_age_experimental_ns: 30 * 86_400 * 1_000_000_000,
            model_age_incompatible_ns: 90 * 86_400 * 1_000_000_000,
        }
    }
}

impl ApplicabilityThresholds {
    pub fn validate(self) -> Result<(), ApplicabilityError> {
        if self.mahalanobis_degraded.is_finite()
            && self.mahalanobis_ood.is_finite()
            && 0.0 < self.mahalanobis_degraded
            && self.mahalanobis_degraded < self.mahalanobis_ood
            && self.nearest_neighbor_degraded.is_finite()
            && self.nearest_neighbor_ood.is_finite()
            && 0.0 < self.nearest_neighbor_degraded
            && self.nearest_neighbor_degraded < self.nearest_neighbor_ood
            && self.residual_experimental.is_finite()
            && self.residual_ood.is_finite()
            && 0.0 < self.residual_experimental
            && self.residual_experimental < self.residual_ood
            && self.coefficient_drift_experimental.is_finite()
            && self.coefficient_drift_incompatible.is_finite()
            && 0.0 < self.coefficient_drift_experimental
            && self.coefficient_drift_experimental < self.coefficient_drift_incompatible
            && self.quality_available.is_finite()
            && self.quality_degraded.is_finite()
            && 0.0 <= self.quality_degraded
            && self.quality_degraded < self.quality_available
            && self.quality_available <= 1.0
            && self.critical_quality_available.is_finite()
            && self.critical_quality_degraded.is_finite()
            && 0.0 <= self.critical_quality_degraded
            && self.critical_quality_degraded < self.critical_quality_available
            && self.critical_quality_available <= 1.0
            && self.model_age_experimental_ns > 0
            && self.model_age_experimental_ns < self.model_age_incompatible_ns
        {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidPolicy)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PolicySignals {
    pub model_incompatible: bool,
    pub source_unhealthy: bool,
    pub insufficient_data: bool,
    pub out_of_distribution: bool,
    pub experimental: bool,
    pub degraded: bool,
}

impl PolicySignals {
    pub const fn healthy() -> Self {
        Self {
            model_incompatible: false,
            source_unhealthy: false,
            insufficient_data: false,
            out_of_distribution: false,
            experimental: false,
            degraded: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyDecision {
    pub availability: Availability,
    pub active_signals: Vec<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilityPolicy {
    thresholds: ApplicabilityThresholds,
}

impl ApplicabilityPolicy {
    pub fn try_new(thresholds: ApplicabilityThresholds) -> Result<Self, ApplicabilityError> {
        thresholds.validate()?;
        Ok(Self { thresholds })
    }

    pub const fn thresholds(self) -> ApplicabilityThresholds {
        self.thresholds
    }

    pub fn decide(self, signals: PolicySignals) -> PolicyDecision {
        let mut active_signals = Vec::new();
        for (active, reason) in [
            (signals.model_incompatible, "model_incompatible"),
            (signals.source_unhealthy, "source_unhealthy"),
            (signals.insufficient_data, "insufficient_data"),
            (signals.out_of_distribution, "out_of_distribution"),
            (signals.experimental, "experimental"),
            (signals.degraded, "degraded"),
        ] {
            if active {
                active_signals.push(reason);
            }
        }
        let availability = if signals.model_incompatible {
            Availability::ModelIncompatible
        } else if signals.source_unhealthy {
            Availability::SourceUnhealthy
        } else if signals.insufficient_data {
            Availability::InsufficientData
        } else if signals.out_of_distribution {
            Availability::OutOfDistribution
        } else if signals.experimental {
            Availability::Experimental
        } else if signals.degraded {
            Availability::Degraded
        } else {
            Availability::Available
        };
        PolicyDecision {
            availability,
            active_signals,
        }
    }
}
