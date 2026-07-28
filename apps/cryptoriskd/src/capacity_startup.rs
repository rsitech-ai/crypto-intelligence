//! Fail-closed admission for the currently implemented foundation workload.

use std::thread;

use capacity::{
    AdmissionDecision, AdmissionReason, CapacityEvidence, CapacityInput, EvidenceLevel,
    HardwareProfile, WorkloadProfile, admit,
};
use config::{AppConfig, CoverageTier, ValidatedDirectory};
use thiserror::Error;

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
const CONSERVATIVE_SUSTAINED_INBOUND_BYTES_PER_SECOND: u64 = 10 * MIB;
const CONSERVATIVE_BURST_INBOUND_BYTES_PER_SECOND: u64 = 50 * MIB;
const CONSERVATIVE_DISK_WRITE_BYTES_PER_SECOND: u64 = 50 * MIB;
const CONSERVATIVE_BURST_DISK_WRITE_BYTES_PER_SECOND: u64 = 100 * MIB;
const CONSERVATIVE_SUSTAINED_EVENTS_PER_SECOND: u64 = 50_000;
const CONSERVATIVE_BURST_EVENTS_PER_SECOND: u64 = 250_000;
const CONSERVATIVE_CAPACITY_UNCERTAINTY_PPM: u32 = 500_000;
const FOUNDATION_DEMAND_UNCERTAINTY_PPM: u32 = 1_250_000;
const FOUNDATION_EVENTS_PER_SECOND: u64 = 1;
const FOUNDATION_RAW_BYTES_PER_EVENT: u64 = 512;
const FOUNDATION_NORMALIZED_BYTES_PER_EVENT: u64 = 256;
const FOUNDATION_FEATURE_CPU_NANOS_PER_EVENT: u64 = 50_000;
const FOUNDATION_TIER_A_BOOK_MEMORY_BYTES: u64 = 256 * MIB;
const FOUNDATION_TIER_B_BOOK_MEMORY_BYTES: u64 = 64 * MIB;
const FOUNDATION_TIER_C_BOOK_MEMORY_BYTES: u64 = 16 * MIB;
const FOUNDATION_FEATURE_MEMORY_BYTES_PER_INSTRUMENT: u64 = 32 * MIB;
const FOUNDATION_MAX_INGESTION_MESSAGE_BYTES: u64 = MIB;
const FOUNDATION_RUNTIME_MEMORY_BYTES_PER_THREAD: u64 = 2 * MIB;

pub fn enforce_detected_foundation_capacity(
    config: &AppConfig,
    data_root: &ValidatedDirectory,
) -> Result<(), CapacityStartupError> {
    let filesystem = rustix::fs::fstatvfs(data_root.as_fd())
        .map_err(|source| CapacityStartupError::Probe(std::io::Error::from(source)))?;
    let fragment_size = if filesystem.f_frsize == 0 {
        filesystem.f_bsize
    } else {
        filesystem.f_frsize
    };
    let free_disk_bytes = filesystem
        .f_bavail
        .checked_mul(fragment_size)
        .ok_or(CapacityStartupError::ProbeOverflow)?;
    let logical_cpus = thread::available_parallelism()
        .map_err(CapacityStartupError::Probe)?
        .get();
    let logical_cpus =
        u32::try_from(logical_cpus).map_err(|_| CapacityStartupError::ProbeOverflow)?;
    let configured_memory_bytes = config
        .max_memory_gib()
        .checked_mul(GIB)
        .ok_or(CapacityStartupError::ProbeOverflow)?;
    let memory_bytes =
        configured_memory_bytes.min(detected_physical_memory_bytes(configured_memory_bytes)?);
    enforce_foundation_capacity(
        config,
        HardwareProfile {
            schema_version: 1,
            apple_silicon: cfg!(all(target_os = "macos", target_arch = "aarch64")),
            logical_cpus,
            memory_bytes,
            free_disk_bytes,
            sustained_inbound_bytes_per_second: CONSERVATIVE_SUSTAINED_INBOUND_BYTES_PER_SECOND,
            burst_inbound_bytes_per_second: CONSERVATIVE_BURST_INBOUND_BYTES_PER_SECOND,
            sustained_disk_write_bytes_per_second: CONSERVATIVE_DISK_WRITE_BYTES_PER_SECOND,
            burst_disk_write_bytes_per_second: CONSERVATIVE_BURST_DISK_WRITE_BYTES_PER_SECOND,
            sustained_normalized_events_per_second: CONSERVATIVE_SUSTAINED_EVENTS_PER_SECOND,
            burst_normalized_events_per_second: CONSERVATIVE_BURST_EVENTS_PER_SECOND,
            evidence: CapacityEvidence {
                schema_version: 1,
                level: EvidenceLevel::ConservativeDefault,
                capacity_uncertainty_ppm: CONSERVATIVE_CAPACITY_UNCERTAINTY_PPM,
                sample_count: 0,
                observed_at_unix_seconds: 0,
                measurement_blake3: [0; 32],
            },
        },
    )
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn detected_physical_memory_bytes(
    _configured_memory_bytes: u64,
) -> Result<u64, CapacityStartupError> {
    let name = c"hw.memsize";
    let mut value = 0_u64;
    let mut length = std::mem::size_of::<u64>();
    // SAFETY: `name` is a static NUL-terminated C string, `value` is writable
    // for exactly `length` bytes, and both setter arguments are null/zero, so
    // this call can only read the named scalar system value.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &raw mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || length != std::mem::size_of::<u64>() || value == 0 {
        return Err(CapacityStartupError::InvalidProbe);
    }
    Ok(value)
}

#[cfg(not(target_os = "macos"))]
const fn detected_physical_memory_bytes(
    configured_memory_bytes: u64,
) -> Result<u64, CapacityStartupError> {
    Ok(configured_memory_bytes)
}

pub fn enforce_foundation_capacity(
    config: &AppConfig,
    hardware: HardwareProfile,
) -> Result<(), CapacityStartupError> {
    let mut tier_a_instruments = 0_u32;
    let mut tier_b_instruments = 0_u32;
    let mut tier_c_instruments = 0_u32;
    let mut tier_a_retention_days = 0_u32;
    let mut tier_b_retention_days = 0_u32;
    let mut tier_c_retention_days = 0_u32;
    for venue in config.venues().iter().filter(|venue| venue.enabled()) {
        match venue.coverage_tier() {
            CoverageTier::A => {
                tier_a_instruments = tier_a_instruments
                    .checked_add(venue.instrument_budget())
                    .ok_or(CapacityStartupError::InputOverflow)?;
                tier_a_retention_days = tier_a_retention_days.max(venue.raw_retention_days());
            }
            CoverageTier::B => {
                tier_b_instruments = tier_b_instruments
                    .checked_add(venue.instrument_budget())
                    .ok_or(CapacityStartupError::InputOverflow)?;
                tier_b_retention_days = tier_b_retention_days.max(venue.raw_retention_days());
            }
            CoverageTier::C => {
                tier_c_instruments = tier_c_instruments
                    .checked_add(venue.instrument_budget())
                    .ok_or(CapacityStartupError::InputOverflow)?;
                tier_c_retention_days = tier_c_retention_days.max(venue.raw_retention_days());
            }
        }
    }
    let ingestion_queue_memory_bytes = u64::try_from(config.ingestion_queue_capacity())
        .map_err(|_| CapacityStartupError::InputOverflow)?
        .checked_mul(FOUNDATION_MAX_INGESTION_MESSAGE_BYTES)
        .ok_or(CapacityStartupError::InputOverflow)?;
    let rpc_inflight_memory_bytes = u64::try_from(config.maximum_request_bytes())
        .map_err(|_| CapacityStartupError::InputOverflow)?
        .checked_mul(
            u64::try_from(config.maximum_concurrent_requests())
                .map_err(|_| CapacityStartupError::InputOverflow)?,
        )
        .ok_or(CapacityStartupError::InputOverflow)?;
    let runtime_memory_bytes = u64::try_from(config.runtime_threads())
        .map_err(|_| CapacityStartupError::InputOverflow)?
        .checked_mul(FOUNDATION_RUNTIME_MEMORY_BYTES_PER_THREAD)
        .ok_or(CapacityStartupError::InputOverflow)?;
    let decision = admit(&CapacityInput::new(
        hardware,
        WorkloadProfile {
            schema_version: 1,
            tier_a_instruments,
            tier_b_instruments,
            tier_c_instruments,
            tier_a_retention_days,
            tier_b_retention_days,
            tier_c_retention_days,
            sustained_events_per_second: FOUNDATION_EVENTS_PER_SECOND,
            burst_events_per_second: FOUNDATION_EVENTS_PER_SECOND,
            demand_uncertainty_ppm: FOUNDATION_DEMAND_UNCERTAINTY_PPM,
            raw_bytes_per_event: FOUNDATION_RAW_BYTES_PER_EVENT,
            normalized_bytes_per_event: FOUNDATION_NORMALIZED_BYTES_PER_EVENT,
            wal_write_amplification_ppm: 1_100_000,
            parquet_write_amplification_ppm: 1_200_000,
            feature_cpu_nanos_per_event: FOUNDATION_FEATURE_CPU_NANOS_PER_EVENT,
            tier_a_book_memory_bytes: FOUNDATION_TIER_A_BOOK_MEMORY_BYTES,
            tier_b_book_memory_bytes: FOUNDATION_TIER_B_BOOK_MEMORY_BYTES,
            tier_c_book_memory_bytes: FOUNDATION_TIER_C_BOOK_MEMORY_BYTES,
            feature_memory_bytes_per_instrument: FOUNDATION_FEATURE_MEMORY_BYTES_PER_INSTRUMENT,
            model_memory_bytes: 0,
            ingestion_queue_memory_bytes,
            rpc_inflight_memory_bytes,
            runtime_memory_bytes,
            concurrent_replay: false,
            replay_event_count: 0,
            replay_events_per_second: 0,
            replay_cpu_multiplier_ppm: 1_000_000,
        },
    ))?;
    match decision {
        AdmissionDecision::EvidenceRequired { .. } => Ok(()),
        AdmissionDecision::Rejected { reasons, .. } => {
            Err(CapacityStartupError::Rejected { reasons })
        }
    }
}

#[derive(Debug, Error)]
pub enum CapacityStartupError {
    #[error("local hardware/filesystem capacity could not be measured")]
    Probe(#[source] std::io::Error),
    #[error("local hardware/filesystem capacity measurement overflowed")]
    ProbeOverflow,
    #[error("local hardware capacity measurement was malformed")]
    InvalidProbe,
    #[error("configured coverage could not be represented")]
    InputOverflow,
    #[error("configured foundation workload exceeds local capacity: {reasons:?}")]
    Rejected { reasons: Vec<AdmissionReason> },
    #[error("configured foundation workload is not a valid capacity input: {0}")]
    Capacity(#[from] capacity::CapacityError),
}
