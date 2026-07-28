use domain::InstrumentId;
use fixed_decimal::{Price, Quantity};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub(crate) const MAX_BOOK_LEVELS_PER_SIDE: usize = 100_000;
pub(crate) const MAX_BUFFERED_DELTAS: usize = 100_000;
pub(crate) const MAX_BUFFERED_LEVEL_UPDATES: usize = 1_000_000;
pub(crate) const MAX_L3_ORDERS: usize = 1_000_000;
const QUALITY_WINDOW_NS: u64 = 3_600_000_000_000;
const QUALITY_BUCKETS: usize = 3_601;
const MAX_LATENCY_SAMPLES: usize = 4_096;

/// Synchronization lifecycle for one single-writer instrument shard.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookState {
    Disconnected,
    Buffering,
    AwaitingSnapshot,
    Replaying,
    Synchronized,
    Untrusted,
}

/// Observable outcome of a snapshot or delta application.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyResult {
    Applied,
    Duplicate,
    GapDetected,
    ChecksumMismatch,
    SnapshotRequired,
}

/// Explicit top-of-book market classification.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookClassification {
    Normal,
    Locked,
    Crossed,
    OneSided,
}

/// Sequence semantics selected by a connector.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SequencePolicy {
    /// Every applied message starts exactly at the next expected sequence.
    ExactNext,
    /// Every applicable message range contains the next expected sequence.
    /// This matches Binance Spot `[U, u]` continuation semantics.
    RangeContainsNext,
    /// The first replayed range contains the next expected sequence; every
    /// subsequent message also names the previous final sequence.
    PreviousFinal,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStrategy {
    ExternalBuffered,
    StreamSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumPolicy {
    Disabled,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookSession {
    pub connection_epoch: u64,
    pub subscription_epoch: u64,
    pub instrument_generation: u32,
}

impl BookSession {
    pub(crate) const fn is_valid(self) -> bool {
        self.connection_epoch != 0
            && self.subscription_epoch != 0
            && self.instrument_generation != 0
    }

    pub(crate) const fn is_strictly_after(self, previous: Self) -> bool {
        self.connection_epoch > previous.connection_epoch
            || self.connection_epoch == previous.connection_epoch
                && self.subscription_epoch > previous.subscription_epoch
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BookConfig {
    pub instrument: InstrumentId,
    pub price_tick: Price,
    pub quantity_step: Quantity,
    pub max_levels_per_side: usize,
    pub max_buffered_deltas: usize,
    pub max_buffered_level_updates: usize,
    pub sequence_policy: SequencePolicy,
    pub checksum_policy: ChecksumPolicy,
    pub max_l3_orders: Option<usize>,
    pub max_l3_levels_per_side: Option<usize>,
}

impl BookConfig {
    pub(crate) fn validate(&self) -> bool {
        self.price_tick.value().is_positive()
            && self.quantity_step.value().is_positive()
            && (1..=MAX_BOOK_LEVELS_PER_SIDE).contains(&self.max_levels_per_side)
            && (1..=MAX_BUFFERED_DELTAS).contains(&self.max_buffered_deltas)
            && (1..=MAX_BUFFERED_LEVEL_UPDATES).contains(&self.max_buffered_level_updates)
            && self
                .max_l3_orders
                .is_none_or(|maximum| (1..=MAX_L3_ORDERS).contains(&maximum))
            && self.max_l3_orders.is_some() == self.max_l3_levels_per_side.is_some()
            && self
                .max_l3_levels_per_side
                .is_none_or(|depth| (1..=MAX_BOOK_LEVELS_PER_SIDE).contains(&depth))
    }

    pub fn instrument(&self) -> &InstrumentId {
        &self.instrument
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumStatus {
    NotSupported,
    Pending,
    Valid,
    Mismatch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLatencyPercentiles {
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    pub p99_ms: Option<u64>,
    pub sample_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookQuality {
    pub health_state: BookState,
    pub last_source_sequence: u64,
    pub last_update_age_ms: u64,
    pub checksum_status: ChecksumStatus,
    pub resync_count_1h: u64,
    pub missing_sequence_count_1h: u64,
    pub checksum_failure_count_1h: u64,
    pub crossed_state_count_1h: u64,
    pub source_latency_percentiles: SourceLatencyPercentiles,
    pub trusted_depth_levels: usize,
    pub quality_score_ppm: u32,
}

impl BookQuality {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn score(
        health_state: BookState,
        last_source_sequence: u64,
        last_update_age_ms: u64,
        checksum_status: ChecksumStatus,
        resync_count_1h: u64,
        missing_sequence_count_1h: u64,
        checksum_failure_count_1h: u64,
        crossed_state_count_1h: u64,
        source_latency_percentiles: SourceLatencyPercentiles,
        trusted_depth_levels: usize,
    ) -> Self {
        let penalty = missing_sequence_count_1h
            .saturating_mul(250_000)
            .saturating_add(checksum_failure_count_1h.saturating_mul(300_000))
            .saturating_add(crossed_state_count_1h.saturating_mul(50_000))
            .min(1_000_000);
        Self {
            health_state,
            last_source_sequence,
            last_update_age_ms,
            checksum_status,
            resync_count_1h,
            missing_sequence_count_1h,
            checksum_failure_count_1h,
            crossed_state_count_1h,
            source_latency_percentiles,
            trusted_depth_levels,
            quality_score_ppm: u32::try_from(1_000_000 - penalty).unwrap_or(0),
        }
    }

    pub fn quality_score_0_to_1(&self) -> f64 {
        f64::from(self.quality_score_ppm) / 1_000_000.0
    }
}

#[derive(Clone, Copy, Debug)]
struct CountBucket {
    epoch_second: u64,
    count: u64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct HourlyCounter {
    buckets: VecDeque<CountBucket>,
}

impl HourlyCounter {
    pub(crate) fn record(&mut self, now_monotonic_ns: u64) {
        let epoch_second = now_monotonic_ns / 1_000_000_000;
        self.prune(epoch_second);
        if let Some(bucket) = self
            .buckets
            .back_mut()
            .filter(|bucket| bucket.epoch_second == epoch_second)
        {
            bucket.count = bucket.count.saturating_add(1);
        } else {
            if self.buckets.len() == QUALITY_BUCKETS {
                self.buckets.pop_front();
            }
            self.buckets.push_back(CountBucket {
                epoch_second,
                count: 1,
            });
        }
    }

    pub(crate) fn count(&self, now_monotonic_ns: u64) -> u64 {
        let current_second = now_monotonic_ns / 1_000_000_000;
        self.buckets
            .iter()
            .filter(|bucket| {
                current_second >= bucket.epoch_second
                    && current_second.saturating_sub(bucket.epoch_second) < 3_600
            })
            .fold(0_u64, |total, bucket| total.saturating_add(bucket.count))
    }

    fn prune(&mut self, current_second: u64) {
        while self.buckets.front().is_some_and(|bucket| {
            current_second >= bucket.epoch_second
                && current_second.saturating_sub(bucket.epoch_second) >= 3_600
        }) {
            self.buckets.pop_front();
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LatencyWindow {
    samples: VecDeque<(u64, u64)>,
}

impl LatencyWindow {
    pub(crate) fn record(&mut self, now_monotonic_ns: u64, latency_ms: u64) {
        self.prune(now_monotonic_ns);
        if self.samples.len() == MAX_LATENCY_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now_monotonic_ns, latency_ms));
    }

    pub(crate) fn percentiles(&self, now_monotonic_ns: u64) -> SourceLatencyPercentiles {
        let cutoff = now_monotonic_ns.saturating_sub(QUALITY_WINDOW_NS);
        let mut values = self
            .samples
            .iter()
            .filter_map(|(observed_at, latency)| (*observed_at >= cutoff).then_some(*latency))
            .collect::<Vec<_>>();
        values.sort_unstable();
        SourceLatencyPercentiles {
            p50_ms: percentile(&values, 50),
            p95_ms: percentile(&values, 95),
            p99_ms: percentile(&values, 99),
            sample_count: values.len(),
        }
    }

    fn prune(&mut self, now_monotonic_ns: u64) {
        let cutoff = now_monotonic_ns.saturating_sub(QUALITY_WINDOW_NS);
        while self
            .samples
            .front()
            .is_some_and(|(observed_at, _)| *observed_at < cutoff)
        {
            self.samples.pop_front();
        }
    }
}

fn percentile(values: &[u64], percentile: usize) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let rank = values.len().saturating_mul(percentile).saturating_add(99) / 100;
    values.get(rank.saturating_sub(1)).copied()
}
