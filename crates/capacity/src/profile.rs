//! Versioned hardware, workload, and admission-policy inputs.

use serde::{Deserialize, Serialize};

use crate::CapacityError;

pub const PARTS_PER_MILLION: u64 = 1_000_000;
const MINIMUM_CONSERVATIVE_CAPACITY_UNCERTAINTY_PPM: u32 = 250_000;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    ConservativeDefault,
    Measured,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityEvidence {
    pub schema_version: u32,
    pub level: EvidenceLevel,
    pub capacity_uncertainty_ppm: u32,
    pub sample_count: u32,
    pub observed_at_unix_seconds: u64,
    pub measurement_blake3: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HardwareProfile {
    pub schema_version: u32,
    pub apple_silicon: bool,
    pub logical_cpus: u32,
    pub memory_bytes: u64,
    pub free_disk_bytes: u64,
    pub sustained_inbound_bytes_per_second: u64,
    pub burst_inbound_bytes_per_second: u64,
    pub sustained_disk_write_bytes_per_second: u64,
    pub burst_disk_write_bytes_per_second: u64,
    pub sustained_normalized_events_per_second: u64,
    pub burst_normalized_events_per_second: u64,
    pub evidence: CapacityEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadProfile {
    pub schema_version: u32,
    pub tier_a_instruments: u32,
    pub tier_b_instruments: u32,
    pub tier_c_instruments: u32,
    pub tier_a_retention_days: u32,
    pub tier_b_retention_days: u32,
    pub tier_c_retention_days: u32,
    pub sustained_events_per_second: u64,
    pub burst_events_per_second: u64,
    pub demand_uncertainty_ppm: u32,
    pub raw_bytes_per_event: u64,
    pub normalized_bytes_per_event: u64,
    pub wal_write_amplification_ppm: u32,
    pub parquet_write_amplification_ppm: u32,
    pub feature_cpu_nanos_per_event: u64,
    pub tier_a_book_memory_bytes: u64,
    pub tier_b_book_memory_bytes: u64,
    pub tier_c_book_memory_bytes: u64,
    pub feature_memory_bytes_per_instrument: u64,
    pub model_memory_bytes: u64,
    pub ingestion_queue_memory_bytes: u64,
    pub rpc_inflight_memory_bytes: u64,
    pub runtime_memory_bytes: u64,
    pub concurrent_replay: bool,
    pub replay_event_count: u64,
    pub replay_events_per_second: u64,
    pub replay_cpu_multiplier_ppm: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityPolicy {
    pub schema_version: u32,
    pub memory_reserve_ppm: u32,
    pub disk_reserve_ppm: u32,
    pub cpu_reserve_ppm: u32,
    pub minimum_sustained_headroom_ppm: u32,
    pub minimum_burst_headroom_ppm: u32,
    pub minimum_sustained_bandwidth_headroom_ppm: u32,
    pub minimum_burst_bandwidth_headroom_ppm: u32,
    pub minimum_sustained_write_headroom_ppm: u32,
    pub minimum_burst_write_headroom_ppm: u32,
}

impl Default for CapacityPolicy {
    fn default() -> Self {
        Self {
            schema_version: 1,
            memory_reserve_ppm: 300_000,
            disk_reserve_ppm: 200_000,
            cpu_reserve_ppm: 250_000,
            minimum_sustained_headroom_ppm: 1_250_000,
            minimum_burst_headroom_ppm: 1_250_000,
            minimum_sustained_bandwidth_headroom_ppm: 1_250_000,
            minimum_burst_bandwidth_headroom_ppm: 1_250_000,
            minimum_sustained_write_headroom_ppm: 1_250_000,
            minimum_burst_write_headroom_ppm: 1_250_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityInput {
    pub schema_version: u32,
    pub hardware: HardwareProfile,
    pub workload: WorkloadProfile,
    #[serde(default)]
    pub policy: CapacityPolicy,
}

impl CapacityInput {
    pub fn new(hardware: HardwareProfile, workload: WorkloadProfile) -> Self {
        Self {
            schema_version: 1,
            hardware,
            workload,
            policy: CapacityPolicy::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), CapacityError> {
        for (valid, field) in [
            (self.schema_version == 1, InputField::InputSchemaVersion),
            (
                self.hardware.schema_version == 1,
                InputField::HardwareSchemaVersion,
            ),
            (
                self.hardware.evidence.schema_version == 1,
                InputField::EvidenceSchemaVersion,
            ),
            (
                self.workload.schema_version == 1,
                InputField::WorkloadSchemaVersion,
            ),
            (
                self.policy.schema_version == 1,
                InputField::PolicySchemaVersion,
            ),
            (self.hardware.logical_cpus > 0, InputField::LogicalCpus),
            (self.hardware.memory_bytes > 0, InputField::MemoryBytes),
            (self.hardware.free_disk_bytes > 0, InputField::FreeDiskBytes),
            (
                self.hardware.sustained_inbound_bytes_per_second > 0,
                InputField::SustainedInboundBandwidth,
            ),
            (
                self.hardware.burst_inbound_bytes_per_second
                    >= self.hardware.sustained_inbound_bytes_per_second,
                InputField::BurstInboundBandwidth,
            ),
            (
                self.hardware.sustained_disk_write_bytes_per_second > 0,
                InputField::SustainedDiskWrite,
            ),
            (
                self.hardware.burst_disk_write_bytes_per_second
                    >= self.hardware.sustained_disk_write_bytes_per_second,
                InputField::BurstDiskWrite,
            ),
            (
                self.hardware.sustained_normalized_events_per_second > 0,
                InputField::SustainedEventCapacity,
            ),
            (
                self.hardware.burst_normalized_events_per_second
                    >= self.hardware.sustained_normalized_events_per_second,
                InputField::BurstEventCapacity,
            ),
            (
                self.hardware.evidence.capacity_uncertainty_ppm < PARTS_PER_MILLION as u32,
                InputField::CapacityUncertainty,
            ),
            (
                valid_evidence(self.hardware.evidence),
                InputField::EvidenceProvenance,
            ),
            (
                self.workload.tier_a_instruments > 0,
                InputField::TierAInstruments,
            ),
            (
                self.workload.tier_a_retention_days > 0,
                InputField::TierARetentionDays,
            ),
            (
                (self.workload.tier_b_instruments == 0 && self.workload.tier_b_retention_days == 0)
                    || (self.workload.tier_b_instruments > 0
                        && self.workload.tier_b_retention_days > 0),
                InputField::TierBRetentionDays,
            ),
            (
                (self.workload.tier_c_instruments == 0 && self.workload.tier_c_retention_days == 0)
                    || (self.workload.tier_c_instruments > 0
                        && self.workload.tier_c_retention_days > 0),
                InputField::TierCRetentionDays,
            ),
            (
                self.workload.sustained_events_per_second > 0,
                InputField::SustainedEvents,
            ),
            (
                self.workload.burst_events_per_second >= self.workload.sustained_events_per_second,
                InputField::BurstEvents,
            ),
            (
                valid_headroom(self.workload.demand_uncertainty_ppm),
                InputField::DemandUncertainty,
            ),
            (
                self.workload.raw_bytes_per_event > 0,
                InputField::RawBytesPerEvent,
            ),
            (
                self.workload.normalized_bytes_per_event > 0,
                InputField::NormalizedBytesPerEvent,
            ),
            (
                valid_amplification(self.workload.wal_write_amplification_ppm),
                InputField::WalAmplification,
            ),
            (
                valid_amplification(self.workload.parquet_write_amplification_ppm),
                InputField::ParquetAmplification,
            ),
            (
                self.workload.feature_cpu_nanos_per_event > 0,
                InputField::FeatureCpu,
            ),
            (
                self.workload.tier_a_book_memory_bytes > 0,
                InputField::TierABookMemory,
            ),
            (
                self.workload.tier_b_instruments == 0 || self.workload.tier_b_book_memory_bytes > 0,
                InputField::TierBBookMemory,
            ),
            (
                self.workload.tier_c_instruments == 0 || self.workload.tier_c_book_memory_bytes > 0,
                InputField::TierCBookMemory,
            ),
            (
                self.workload.feature_memory_bytes_per_instrument > 0,
                InputField::FeatureMemory,
            ),
            (
                (!self.workload.concurrent_replay
                    && self.workload.replay_event_count == 0
                    && self.workload.replay_events_per_second == 0)
                    || (self.workload.concurrent_replay
                        && self.workload.replay_event_count > 0
                        && self.workload.replay_events_per_second > 0
                        && valid_amplification(self.workload.replay_cpu_multiplier_ppm)),
                InputField::ReplayMultiplier,
            ),
            (
                valid_reserve(self.policy.memory_reserve_ppm),
                InputField::MemoryReserve,
            ),
            (
                valid_reserve(self.policy.disk_reserve_ppm),
                InputField::DiskReserve,
            ),
            (
                valid_reserve(self.policy.cpu_reserve_ppm),
                InputField::CpuReserve,
            ),
            (
                valid_headroom(self.policy.minimum_sustained_headroom_ppm),
                InputField::SustainedHeadroom,
            ),
            (
                valid_headroom(self.policy.minimum_burst_headroom_ppm),
                InputField::BurstHeadroom,
            ),
            (
                valid_headroom(self.policy.minimum_sustained_bandwidth_headroom_ppm),
                InputField::SustainedBandwidthHeadroom,
            ),
            (
                valid_headroom(self.policy.minimum_burst_bandwidth_headroom_ppm),
                InputField::BurstBandwidthHeadroom,
            ),
            (
                valid_headroom(self.policy.minimum_sustained_write_headroom_ppm),
                InputField::SustainedWriteHeadroom,
            ),
            (
                valid_headroom(self.policy.minimum_burst_write_headroom_ppm),
                InputField::BurstWriteHeadroom,
            ),
        ] {
            if !valid {
                return Err(CapacityError::InvalidInput { field });
            }
        }
        Ok(())
    }
}

fn valid_evidence(evidence: CapacityEvidence) -> bool {
    match evidence.level {
        EvidenceLevel::ConservativeDefault => {
            evidence.capacity_uncertainty_ppm >= MINIMUM_CONSERVATIVE_CAPACITY_UNCERTAINTY_PPM
                && evidence.sample_count == 0
                && evidence.observed_at_unix_seconds == 0
                && evidence.measurement_blake3 == [0; 32]
        }
        EvidenceLevel::Measured => {
            evidence.sample_count > 0
                && evidence.observed_at_unix_seconds > 0
                && evidence.measurement_blake3 != [0; 32]
        }
    }
}

const fn valid_amplification(value: u32) -> bool {
    value >= PARTS_PER_MILLION as u32 && value <= 10_000_000
}

const fn valid_reserve(value: u32) -> bool {
    value < PARTS_PER_MILLION as u32
}

const fn valid_headroom(value: u32) -> bool {
    value >= PARTS_PER_MILLION as u32 && value <= 10_000_000
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputField {
    InputSchemaVersion,
    HardwareSchemaVersion,
    EvidenceSchemaVersion,
    WorkloadSchemaVersion,
    PolicySchemaVersion,
    LogicalCpus,
    MemoryBytes,
    FreeDiskBytes,
    SustainedInboundBandwidth,
    BurstInboundBandwidth,
    SustainedDiskWrite,
    BurstDiskWrite,
    SustainedEventCapacity,
    BurstEventCapacity,
    CapacityUncertainty,
    EvidenceProvenance,
    TierAInstruments,
    TierARetentionDays,
    TierBRetentionDays,
    TierCRetentionDays,
    SustainedEvents,
    BurstEvents,
    DemandUncertainty,
    RawBytesPerEvent,
    NormalizedBytesPerEvent,
    WalAmplification,
    ParquetAmplification,
    FeatureCpu,
    TierABookMemory,
    TierBBookMemory,
    TierCBookMemory,
    FeatureMemory,
    ReplayMultiplier,
    MemoryReserve,
    DiskReserve,
    CpuReserve,
    SustainedHeadroom,
    BurstHeadroom,
    SustainedBandwidthHeadroom,
    BurstBandwidthHeadroom,
    SustainedWriteHeadroom,
    BurstWriteHeadroom,
}
