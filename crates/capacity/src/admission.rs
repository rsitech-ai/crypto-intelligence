//! Evidence-aware capacity admission and explicit downgrade proposals.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{CapacityError, CapacityEstimate, profile::CapacityInput};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionReason {
    AppleSiliconRequired,
    MemoryBudgetExceeded,
    RetentionBudgetExceeded,
    CpuBudgetExceeded,
    SustainedEventHeadroomInsufficient,
    BurstEventHeadroomInsufficient,
    SustainedBandwidthHeadroomInsufficient,
    BurstBandwidthHeadroomInsufficient,
    SustainedWriteHeadroomInsufficient,
    BurstWriteHeadroomInsufficient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRequirement {
    RepositoryVerifiedCapacityProfile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityQualityImpact {
    pub schema_version: u32,
    pub tier_a_instruments_preserved: u32,
    pub tier_a_retention_days_preserved: u32,
    pub removed_tier_b_instruments: u32,
    pub removed_tier_c_instruments: u32,
    pub removed_tier_b_retention_days: u32,
    pub removed_tier_c_retention_days: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DowngradeProposal {
    pub schema_version: u32,
    pub tier_a_instruments: u32,
    pub tier_b_instruments: u32,
    pub tier_c_instruments: u32,
    pub tier_a_retention_days: u32,
    pub tier_b_retention_days: u32,
    pub tier_c_retention_days: u32,
    pub quality_impact: CapacityQualityImpact,
    pub automatically_applied: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AdmissionDecision {
    EvidenceRequired {
        estimate: CapacityEstimate,
        required: EvidenceRequirement,
    },
    Rejected {
        estimate: CapacityEstimate,
        reasons: Vec<AdmissionReason>,
        downgrade: Option<DowngradeProposal>,
    },
}

pub fn admit(input: &CapacityInput) -> Result<AdmissionDecision, CapacityError> {
    let estimate = CapacityEstimate::calculate(input)?;
    let reasons = reasons(input, &estimate);
    if !reasons.is_empty() {
        return Ok(AdmissionDecision::Rejected {
            estimate,
            reasons: reasons.into_iter().collect(),
            downgrade: downgrade(input)?,
        });
    }
    // Schema v1 has no repository-owned certification registry. A caller may
    // declare conservative or measured observations, but cannot self-assert
    // that those observations passed the release certification gate.
    Ok(AdmissionDecision::EvidenceRequired {
        estimate,
        required: EvidenceRequirement::RepositoryVerifiedCapacityProfile,
    })
}

fn reasons(input: &CapacityInput, estimate: &CapacityEstimate) -> BTreeSet<AdmissionReason> {
    let mut reasons = BTreeSet::new();
    if !input.hardware.apple_silicon {
        reasons.insert(AdmissionReason::AppleSiliconRequired);
    }
    if estimate.peak_memory_bytes > estimate.usable_memory_bytes {
        reasons.insert(AdmissionReason::MemoryBudgetExceeded);
    }
    let required_retention_days = input
        .workload
        .tier_a_retention_days
        .max(input.workload.tier_b_retention_days)
        .max(input.workload.tier_c_retention_days);
    if estimate.retention_days_supported < u64::from(required_retention_days) {
        reasons.insert(AdmissionReason::RetentionBudgetExceeded);
    }
    if estimate.total_cpu_cores_ppm > estimate.usable_cpu_cores_ppm {
        reasons.insert(AdmissionReason::CpuBudgetExceeded);
    }
    if estimate.sustained_event_headroom_ppm
        < u64::from(input.policy.minimum_sustained_headroom_ppm)
    {
        reasons.insert(AdmissionReason::SustainedEventHeadroomInsufficient);
    }
    if estimate.burst_event_headroom_ppm < u64::from(input.policy.minimum_burst_headroom_ppm) {
        reasons.insert(AdmissionReason::BurstEventHeadroomInsufficient);
    }
    if estimate.sustained_bandwidth_headroom_ppm
        < u64::from(input.policy.minimum_sustained_bandwidth_headroom_ppm)
    {
        reasons.insert(AdmissionReason::SustainedBandwidthHeadroomInsufficient);
    }
    if estimate.burst_bandwidth_headroom_ppm
        < u64::from(input.policy.minimum_burst_bandwidth_headroom_ppm)
    {
        reasons.insert(AdmissionReason::BurstBandwidthHeadroomInsufficient);
    }
    if estimate.sustained_write_headroom_ppm
        < u64::from(input.policy.minimum_sustained_write_headroom_ppm)
    {
        reasons.insert(AdmissionReason::SustainedWriteHeadroomInsufficient);
    }
    if estimate.burst_write_headroom_ppm < u64::from(input.policy.minimum_burst_write_headroom_ppm)
    {
        reasons.insert(AdmissionReason::BurstWriteHeadroomInsufficient);
    }
    reasons
}

fn downgrade(input: &CapacityInput) -> Result<Option<DowngradeProposal>, CapacityError> {
    if input.workload.tier_b_instruments == 0 && input.workload.tier_c_instruments == 0 {
        return Ok(None);
    }
    let mut baseline = *input;
    baseline.workload.tier_b_instruments = 0;
    baseline.workload.tier_c_instruments = 0;
    baseline.workload.tier_b_retention_days = 0;
    baseline.workload.tier_c_retention_days = 0;
    let estimate = CapacityEstimate::calculate(&baseline)?;
    if !reasons(&baseline, &estimate).is_empty() {
        return Ok(None);
    }
    Ok(Some(DowngradeProposal {
        schema_version: 1,
        tier_a_instruments: input.workload.tier_a_instruments,
        tier_b_instruments: 0,
        tier_c_instruments: 0,
        tier_a_retention_days: input.workload.tier_a_retention_days,
        tier_b_retention_days: 0,
        tier_c_retention_days: 0,
        quality_impact: CapacityQualityImpact {
            schema_version: 1,
            tier_a_instruments_preserved: input.workload.tier_a_instruments,
            tier_a_retention_days_preserved: input.workload.tier_a_retention_days,
            removed_tier_b_instruments: input.workload.tier_b_instruments,
            removed_tier_c_instruments: input.workload.tier_c_instruments,
            removed_tier_b_retention_days: input.workload.tier_b_retention_days,
            removed_tier_c_retention_days: input.workload.tier_c_retention_days,
        },
        automatically_applied: false,
    }))
}
