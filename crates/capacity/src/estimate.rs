//! Checked integer capacity estimates.

use serde::{Deserialize, Serialize};

use crate::{
    CapacityError,
    profile::{CapacityInput, PARTS_PER_MILLION},
};

const SECONDS_PER_DAY: u64 = 86_400;
const NANOS_PER_SECOND: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityEstimate {
    pub schema_version: u32,
    pub inbound_bytes_per_second: u64,
    pub burst_inbound_bytes_per_second: u64,
    pub raw_bytes_per_day: u64,
    pub normalized_bytes_per_day: u64,
    pub wal_write_bytes_per_second: u64,
    pub parquet_write_bytes_per_second: u64,
    pub total_write_bytes_per_second: u64,
    pub burst_wal_write_bytes_per_second: u64,
    pub burst_parquet_write_bytes_per_second: u64,
    pub burst_total_write_bytes_per_second: u64,
    pub retained_bytes_per_day: u64,
    pub feature_cpu_cores_ppm: u64,
    pub replay_cpu_cores_ppm: u64,
    pub total_cpu_cores_ppm: u64,
    pub usable_cpu_cores_ppm: u64,
    pub replay_event_count: u64,
    pub replay_read_bytes: u64,
    pub replay_cpu_core_seconds: u64,
    pub replay_duration_seconds: u64,
    pub book_memory_bytes: u64,
    pub feature_memory_bytes: u64,
    pub model_memory_bytes: u64,
    pub ingestion_queue_memory_bytes: u64,
    pub rpc_inflight_memory_bytes: u64,
    pub runtime_memory_bytes: u64,
    pub peak_memory_bytes: u64,
    pub usable_memory_bytes: u64,
    pub usable_disk_bytes: u64,
    pub retention_days_supported: u64,
    pub capacity_uncertainty_ppm: u32,
    pub demand_uncertainty_ppm: u32,
    pub sustained_event_headroom_ppm: u64,
    pub burst_event_headroom_ppm: u64,
    pub sustained_bandwidth_headroom_ppm: u64,
    pub burst_bandwidth_headroom_ppm: u64,
    pub sustained_write_headroom_ppm: u64,
    pub burst_write_headroom_ppm: u64,
}

impl CapacityEstimate {
    pub fn calculate(input: &CapacityInput) -> Result<Self, CapacityError> {
        input.validate()?;
        let hardware = input.hardware;
        let workload = input.workload;
        let policy = input.policy;

        let sustained_events_per_second = checked_ppm(
            workload.sustained_events_per_second,
            u64::from(workload.demand_uncertainty_ppm),
        )?;
        let burst_events_per_second = checked_ppm(
            workload.burst_events_per_second,
            u64::from(workload.demand_uncertainty_ppm),
        )?;
        let inbound_bytes_per_second =
            checked_mul(sustained_events_per_second, workload.raw_bytes_per_event)?;
        let burst_inbound_bytes_per_second =
            checked_mul(burst_events_per_second, workload.raw_bytes_per_event)?;
        let normalized_bytes_per_second = checked_mul(
            sustained_events_per_second,
            workload.normalized_bytes_per_event,
        )?;
        let burst_normalized_bytes_per_second =
            checked_mul(burst_events_per_second, workload.normalized_bytes_per_event)?;
        let raw_bytes_per_day = checked_mul(inbound_bytes_per_second, SECONDS_PER_DAY)?;
        let normalized_bytes_per_day = checked_mul(normalized_bytes_per_second, SECONDS_PER_DAY)?;
        let wal_write_bytes_per_second = checked_ppm(
            inbound_bytes_per_second,
            u64::from(workload.wal_write_amplification_ppm),
        )?;
        let parquet_write_bytes_per_second = checked_ppm(
            normalized_bytes_per_second,
            u64::from(workload.parquet_write_amplification_ppm),
        )?;
        let total_write_bytes_per_second = wal_write_bytes_per_second
            .checked_add(parquet_write_bytes_per_second)
            .ok_or(CapacityError::Overflow)?;
        let burst_wal_write_bytes_per_second = checked_ppm(
            burst_inbound_bytes_per_second,
            u64::from(workload.wal_write_amplification_ppm),
        )?;
        let burst_parquet_write_bytes_per_second = checked_ppm(
            burst_normalized_bytes_per_second,
            u64::from(workload.parquet_write_amplification_ppm),
        )?;
        let burst_total_write_bytes_per_second = burst_wal_write_bytes_per_second
            .checked_add(burst_parquet_write_bytes_per_second)
            .ok_or(CapacityError::Overflow)?;
        let retained_bytes_per_day = checked_mul(total_write_bytes_per_second, SECONDS_PER_DAY)?;

        let feature_cpu_nanos_per_second = checked_mul(
            sustained_events_per_second,
            workload.feature_cpu_nanos_per_event,
        )?;
        let feature_cpu_cores_ppm =
            checked_ratio_ceil(feature_cpu_nanos_per_second, NANOS_PER_SECOND)?;
        let replay_cpu_cores_ppm = if workload.concurrent_replay {
            let replay_events_per_second = checked_ppm(
                workload.replay_events_per_second,
                u64::from(workload.demand_uncertainty_ppm),
            )?;
            let replay_cpu_nanos_per_second = checked_ppm(
                checked_mul(
                    replay_events_per_second,
                    workload.feature_cpu_nanos_per_event,
                )?,
                u64::from(workload.replay_cpu_multiplier_ppm),
            )?;
            checked_ratio_ceil(replay_cpu_nanos_per_second, NANOS_PER_SECOND)?
        } else {
            0
        };
        let replay_read_bytes = if workload.concurrent_replay {
            let replay_bytes_per_event = workload
                .raw_bytes_per_event
                .checked_add(workload.normalized_bytes_per_event)
                .ok_or(CapacityError::Overflow)?;
            checked_mul(workload.replay_event_count, replay_bytes_per_event)?
        } else {
            0
        };
        let replay_cpu_core_seconds = if workload.concurrent_replay {
            let total_replay_cpu_nanos = checked_ppm(
                checked_mul(
                    workload.replay_event_count,
                    workload.feature_cpu_nanos_per_event,
                )?,
                u64::from(workload.replay_cpu_multiplier_ppm),
            )?;
            checked_div_ceil(total_replay_cpu_nanos, NANOS_PER_SECOND)?
        } else {
            0
        };
        let replay_duration_seconds = if workload.concurrent_replay {
            checked_div_ceil(
                workload.replay_event_count,
                workload.replay_events_per_second,
            )?
        } else {
            0
        };
        let total_cpu_cores_ppm = feature_cpu_cores_ppm
            .checked_add(replay_cpu_cores_ppm)
            .ok_or(CapacityError::Overflow)?;
        let total_cpu_capacity_ppm =
            checked_mul(u64::from(hardware.logical_cpus), PARTS_PER_MILLION)?;
        let uncertainty_adjusted_cpu = checked_budget(
            total_cpu_capacity_ppm,
            u64::from(hardware.evidence.capacity_uncertainty_ppm),
        )?;
        let usable_cpu_cores_ppm =
            checked_budget(uncertainty_adjusted_cpu, u64::from(policy.cpu_reserve_ppm))?;

        let tier_a_book_memory = checked_mul(
            u64::from(workload.tier_a_instruments),
            workload.tier_a_book_memory_bytes,
        )?;
        let tier_b_book_memory = checked_mul(
            u64::from(workload.tier_b_instruments),
            workload.tier_b_book_memory_bytes,
        )?;
        let tier_c_book_memory = checked_mul(
            u64::from(workload.tier_c_instruments),
            workload.tier_c_book_memory_bytes,
        )?;
        let total_instruments = u64::from(workload.tier_a_instruments)
            .checked_add(u64::from(workload.tier_b_instruments))
            .and_then(|value| value.checked_add(u64::from(workload.tier_c_instruments)))
            .ok_or(CapacityError::Overflow)?;
        let feature_memory = checked_mul(
            total_instruments,
            workload.feature_memory_bytes_per_instrument,
        )?;
        let book_memory_bytes = tier_a_book_memory
            .checked_add(tier_b_book_memory)
            .and_then(|value| value.checked_add(tier_c_book_memory))
            .ok_or(CapacityError::Overflow)?;
        let peak_memory_bytes = book_memory_bytes
            .checked_add(feature_memory)
            .and_then(|value| value.checked_add(workload.model_memory_bytes))
            .and_then(|value| value.checked_add(workload.ingestion_queue_memory_bytes))
            .and_then(|value| value.checked_add(workload.rpc_inflight_memory_bytes))
            .and_then(|value| value.checked_add(workload.runtime_memory_bytes))
            .ok_or(CapacityError::Overflow)?;
        let usable_memory_bytes =
            checked_budget(hardware.memory_bytes, u64::from(policy.memory_reserve_ppm))?;
        let usable_disk_bytes =
            checked_budget(hardware.free_disk_bytes, u64::from(policy.disk_reserve_ppm))?;
        let retention_days_supported = usable_disk_bytes / retained_bytes_per_day;

        let effective_sustained_event_capacity = checked_budget(
            hardware.sustained_normalized_events_per_second,
            u64::from(hardware.evidence.capacity_uncertainty_ppm),
        )?;
        let effective_burst_event_capacity = checked_budget(
            hardware.burst_normalized_events_per_second,
            u64::from(hardware.evidence.capacity_uncertainty_ppm),
        )?;
        let effective_sustained_bandwidth = checked_budget(
            hardware.sustained_inbound_bytes_per_second,
            u64::from(hardware.evidence.capacity_uncertainty_ppm),
        )?;
        let effective_burst_bandwidth = checked_budget(
            hardware.burst_inbound_bytes_per_second,
            u64::from(hardware.evidence.capacity_uncertainty_ppm),
        )?;
        let effective_sustained_write = checked_budget(
            hardware.sustained_disk_write_bytes_per_second,
            u64::from(hardware.evidence.capacity_uncertainty_ppm),
        )?;
        let effective_burst_write = checked_budget(
            hardware.burst_disk_write_bytes_per_second,
            u64::from(hardware.evidence.capacity_uncertainty_ppm),
        )?;

        Ok(Self {
            schema_version: 1,
            inbound_bytes_per_second,
            burst_inbound_bytes_per_second,
            raw_bytes_per_day,
            normalized_bytes_per_day,
            wal_write_bytes_per_second,
            parquet_write_bytes_per_second,
            total_write_bytes_per_second,
            burst_wal_write_bytes_per_second,
            burst_parquet_write_bytes_per_second,
            burst_total_write_bytes_per_second,
            retained_bytes_per_day,
            feature_cpu_cores_ppm,
            replay_cpu_cores_ppm,
            total_cpu_cores_ppm,
            usable_cpu_cores_ppm,
            replay_event_count: workload.replay_event_count,
            replay_read_bytes,
            replay_cpu_core_seconds,
            replay_duration_seconds,
            book_memory_bytes,
            feature_memory_bytes: feature_memory,
            model_memory_bytes: workload.model_memory_bytes,
            ingestion_queue_memory_bytes: workload.ingestion_queue_memory_bytes,
            rpc_inflight_memory_bytes: workload.rpc_inflight_memory_bytes,
            runtime_memory_bytes: workload.runtime_memory_bytes,
            peak_memory_bytes,
            usable_memory_bytes,
            usable_disk_bytes,
            retention_days_supported,
            capacity_uncertainty_ppm: hardware.evidence.capacity_uncertainty_ppm,
            demand_uncertainty_ppm: workload.demand_uncertainty_ppm,
            sustained_event_headroom_ppm: checked_ratio(
                effective_sustained_event_capacity,
                sustained_events_per_second,
            )?,
            burst_event_headroom_ppm: checked_ratio(
                effective_burst_event_capacity,
                burst_events_per_second,
            )?,
            sustained_bandwidth_headroom_ppm: checked_ratio(
                effective_sustained_bandwidth,
                inbound_bytes_per_second,
            )?,
            burst_bandwidth_headroom_ppm: checked_ratio(
                effective_burst_bandwidth,
                burst_inbound_bytes_per_second,
            )?,
            sustained_write_headroom_ppm: checked_ratio(
                effective_sustained_write,
                total_write_bytes_per_second,
            )?,
            burst_write_headroom_ppm: checked_ratio(
                effective_burst_write,
                burst_total_write_bytes_per_second,
            )?,
        })
    }
}

fn checked_mul(left: u64, right: u64) -> Result<u64, CapacityError> {
    left.checked_mul(right).ok_or(CapacityError::Overflow)
}

fn checked_ppm(value: u64, ppm: u64) -> Result<u64, CapacityError> {
    checked_mul(value, ppm)?
        .checked_add(PARTS_PER_MILLION - 1)
        .ok_or(CapacityError::Overflow)
        .map(|numerator| numerator / PARTS_PER_MILLION)
}

fn checked_ratio(capacity: u64, demand: u64) -> Result<u64, CapacityError> {
    checked_mul(capacity, PARTS_PER_MILLION).map(|value| value / demand)
}

fn checked_ratio_ceil(demand: u64, unit: u64) -> Result<u64, CapacityError> {
    checked_div_ceil(checked_mul(demand, PARTS_PER_MILLION)?, unit)
}

fn checked_div_ceil(value: u64, divisor: u64) -> Result<u64, CapacityError> {
    value
        .checked_add(divisor - 1)
        .ok_or(CapacityError::Overflow)
        .map(|numerator| numerator / divisor)
}

fn checked_budget(total: u64, reserve_ppm: u64) -> Result<u64, CapacityError> {
    checked_mul(total, PARTS_PER_MILLION - reserve_ppm).map(|value| value / PARTS_PER_MILLION)
}
