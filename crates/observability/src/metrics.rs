//! Bounded, typed, in-process metrics and diagnostics snapshots.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::{Arc, Mutex},
};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as _, Unexpected},
};

use crate::ObservabilityError;

const MAX_METRIC_SERIES: usize = 1_024;
const HISTOGRAM_BOUNDS_SECONDS: [f64; 12] = [
    0.000_010, 0.000_025, 0.000_050, 0.000_100, 0.000_250, 0.000_500, 0.001, 0.002_500, 0.005,
    0.010, 0.050, 0.250,
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    Count,
    Bytes,
    Seconds,
    PartsPerMillion,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum MetricName {
    EventsReceived,
    IngestionBytes,
    EventsRejected,
    Reconnects,
    ParserFailures,
    SequenceGaps,
    ChecksumFailures,
    QueueDepth,
    DroppedEvents,
    WalFsyncLatency,
    EventToNormalizedLatency,
    NormalizedToFastFeatureLatency,
    FastFeatureToForecastLatency,
    ModelInferenceLatency,
    Predictions,
    Abstentions,
    StorageCompactionLatency,
    DiskBudgetBytes,
    RpcLatency,
    ClientLag,
    CpuUtilizationPpm,
    MemoryResidentBytes,
    FileDescriptorCount,
    ThreadCount,
}

impl Serialize for MetricName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for MetricName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::ALL
            .into_iter()
            .find(|metric| metric.as_str() == value)
            .ok_or_else(|| {
                D::Error::invalid_value(Unexpected::Str(&value), &"a stable transition metric name")
            })
    }
}

impl MetricName {
    pub const ALL: [Self; 24] = [
        Self::EventsReceived,
        Self::IngestionBytes,
        Self::EventsRejected,
        Self::Reconnects,
        Self::ParserFailures,
        Self::SequenceGaps,
        Self::ChecksumFailures,
        Self::QueueDepth,
        Self::DroppedEvents,
        Self::WalFsyncLatency,
        Self::EventToNormalizedLatency,
        Self::NormalizedToFastFeatureLatency,
        Self::FastFeatureToForecastLatency,
        Self::ModelInferenceLatency,
        Self::Predictions,
        Self::Abstentions,
        Self::StorageCompactionLatency,
        Self::DiskBudgetBytes,
        Self::RpcLatency,
        Self::ClientLag,
        Self::CpuUtilizationPpm,
        Self::MemoryResidentBytes,
        Self::FileDescriptorCount,
        Self::ThreadCount,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EventsReceived => "transition_ingestion_events_total",
            Self::IngestionBytes => "transition_ingestion_bytes_total",
            Self::EventsRejected => "transition_ingestion_rejected_events_total",
            Self::Reconnects => "transition_reconnects_total",
            Self::ParserFailures => "transition_parser_failures_total",
            Self::SequenceGaps => "transition_sequence_gaps_total",
            Self::ChecksumFailures => "transition_checksum_failures_total",
            Self::QueueDepth => "transition_queue_depth",
            Self::DroppedEvents => "transition_dropped_events_total",
            Self::WalFsyncLatency => "transition_wal_fsync_seconds",
            Self::EventToNormalizedLatency => "transition_event_to_normalized_seconds",
            Self::NormalizedToFastFeatureLatency => "transition_normalized_to_fast_feature_seconds",
            Self::FastFeatureToForecastLatency => "transition_fast_feature_to_forecast_seconds",
            Self::ModelInferenceLatency => "transition_model_inference_seconds",
            Self::Predictions => "transition_predictions_total",
            Self::Abstentions => "transition_abstention_total",
            Self::StorageCompactionLatency => "transition_storage_compaction_seconds",
            Self::DiskBudgetBytes => "transition_disk_budget_bytes",
            Self::RpcLatency => "transition_rpc_seconds",
            Self::ClientLag => "transition_client_lag_seconds",
            Self::CpuUtilizationPpm => "transition_cpu_utilization_ppm",
            Self::MemoryResidentBytes => "transition_memory_resident_bytes",
            Self::FileDescriptorCount => "transition_file_descriptor_count",
            Self::ThreadCount => "transition_thread_count",
        }
    }

    pub const fn kind(self) -> MetricKind {
        match self {
            Self::EventsReceived
            | Self::IngestionBytes
            | Self::EventsRejected
            | Self::Reconnects
            | Self::ParserFailures
            | Self::SequenceGaps
            | Self::ChecksumFailures
            | Self::DroppedEvents
            | Self::Predictions
            | Self::Abstentions => MetricKind::Counter,
            Self::QueueDepth
            | Self::DiskBudgetBytes
            | Self::CpuUtilizationPpm
            | Self::MemoryResidentBytes
            | Self::FileDescriptorCount
            | Self::ThreadCount => MetricKind::Gauge,
            Self::WalFsyncLatency
            | Self::EventToNormalizedLatency
            | Self::NormalizedToFastFeatureLatency
            | Self::FastFeatureToForecastLatency
            | Self::ModelInferenceLatency
            | Self::StorageCompactionLatency
            | Self::RpcLatency
            | Self::ClientLag => MetricKind::Histogram,
        }
    }

    pub const fn unit(self) -> MetricUnit {
        match self {
            Self::IngestionBytes | Self::DiskBudgetBytes | Self::MemoryResidentBytes => {
                MetricUnit::Bytes
            }
            Self::CpuUtilizationPpm => MetricUnit::PartsPerMillion,
            Self::WalFsyncLatency
            | Self::EventToNormalizedLatency
            | Self::NormalizedToFastFeatureLatency
            | Self::FastFeatureToForecastLatency
            | Self::ModelInferenceLatency
            | Self::StorageCompactionLatency
            | Self::RpcLatency
            | Self::ClientLag => MetricUnit::Seconds,
            _ => MetricUnit::Count,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    Ingestion,
    OrderBook,
    FeatureEngine,
    ModelEngine,
    Rpc,
    Alerting,
    Storage,
    Replay,
    System,
}

impl Component {
    pub const ALL: [Self; 9] = [
        Self::Ingestion,
        Self::OrderBook,
        Self::FeatureEngine,
        Self::ModelEngine,
        Self::Rpc,
        Self::Alerting,
        Self::Storage,
        Self::Replay,
        Self::System,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingestion => "ingestion",
            Self::OrderBook => "order_book",
            Self::FeatureEngine => "feature_engine",
            Self::ModelEngine => "model_engine",
            Self::Rpc => "rpc",
            Self::Alerting => "alerting",
            Self::Storage => "storage",
            Self::Replay => "replay",
            Self::System => "system",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Venue {
    Binance,
    Bybit,
    Kraken,
    Deribit,
    Other,
    NotApplicable,
}

impl Venue {
    pub const ALL: [Self; 6] = [
        Self::Binance,
        Self::Bybit,
        Self::Kraken,
        Self::Deribit,
        Self::Other,
        Self::NotApplicable,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Bybit => "bybit",
            Self::Kraken => "kraken",
            Self::Deribit => "deribit",
            Self::Other => "other",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Success,
    Rejected,
    Timeout,
    Degraded,
    Abstained,
    NotApplicable,
}

impl Outcome {
    pub const ALL: [Self; 6] = [
        Self::Success,
        Self::Rejected,
        Self::Timeout,
        Self::Degraded,
        Self::Abstained,
        Self::NotApplicable,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Rejected => "rejected",
            Self::Timeout => "timeout",
            Self::Degraded => "degraded",
            Self::Abstained => "abstained",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct MetricKey {
    pub name: MetricName,
    pub component: Component,
    pub venue: Venue,
    pub outcome: Outcome,
}

#[derive(Clone, Debug)]
enum StoredMetric {
    Counter(u64),
    Gauge(i64),
    Histogram {
        counts: [u64; 13],
        sum: f64,
        count: u64,
    },
}

#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Mutex<BTreeMap<MetricKey, StoredMetric>>>,
}

impl Metrics {
    pub fn increment(&self, key: MetricKey, amount: u64) -> Result<(), ObservabilityError> {
        if key.name.kind() != MetricKind::Counter {
            return Err(ObservabilityError::MetricType);
        }
        let mut metrics = self.lock()?;
        match bounded_entry(&mut metrics, key)? {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                StoredMetric::Counter(value) => {
                    *value = value.saturating_add(amount);
                    Ok(())
                }
                _ => Err(ObservabilityError::MetricType),
            },
            Entry::Vacant(entry) => {
                entry.insert(StoredMetric::Counter(amount));
                Ok(())
            }
        }
    }

    pub fn counter(&self, key: MetricKey) -> Result<u64, ObservabilityError> {
        if key.name.kind() != MetricKind::Counter {
            return Err(ObservabilityError::MetricType);
        }
        let metrics = self.lock()?;
        match metrics.get(&key) {
            Some(StoredMetric::Counter(value)) => Ok(*value),
            Some(_) => Err(ObservabilityError::MetricType),
            None => Ok(0),
        }
    }

    pub fn gauge(&self, key: MetricKey, value: i64) -> Result<(), ObservabilityError> {
        if key.name.kind() != MetricKind::Gauge {
            return Err(ObservabilityError::MetricType);
        }
        let mut metrics = self.lock()?;
        match bounded_entry(&mut metrics, key)? {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                StoredMetric::Gauge(stored) => {
                    *stored = value;
                    Ok(())
                }
                _ => Err(ObservabilityError::MetricType),
            },
            Entry::Vacant(entry) => {
                entry.insert(StoredMetric::Gauge(value));
                Ok(())
            }
        }
    }

    pub fn gauge_value(&self, key: MetricKey) -> Result<i64, ObservabilityError> {
        if key.name.kind() != MetricKind::Gauge {
            return Err(ObservabilityError::MetricType);
        }
        let metrics = self.lock()?;
        match metrics.get(&key) {
            Some(StoredMetric::Gauge(value)) => Ok(*value),
            Some(_) => Err(ObservabilityError::MetricType),
            None => Ok(0),
        }
    }

    pub fn observe_seconds(&self, key: MetricKey, value: f64) -> Result<(), ObservabilityError> {
        if key.name.kind() != MetricKind::Histogram || key.name.unit() != MetricUnit::Seconds {
            return Err(ObservabilityError::MetricType);
        }
        if !value.is_finite() || value.is_sign_negative() {
            return Err(ObservabilityError::InvalidObservation);
        }
        let mut metrics = self.lock()?;
        match bounded_entry(&mut metrics, key)? {
            Entry::Occupied(mut entry) => match entry.get_mut() {
                StoredMetric::Histogram { counts, sum, count } => {
                    let new_sum = *sum + value;
                    if !new_sum.is_finite() {
                        return Err(ObservabilityError::InvalidObservation);
                    }
                    let bucket = histogram_bucket(value);
                    counts[bucket] = counts[bucket].saturating_add(1);
                    *sum = new_sum;
                    *count = count.saturating_add(1);
                    Ok(())
                }
                _ => Err(ObservabilityError::MetricType),
            },
            Entry::Vacant(entry) => {
                let mut counts = [0; 13];
                counts[histogram_bucket(value)] = 1;
                entry.insert(StoredMetric::Histogram {
                    counts,
                    sum: value,
                    count: 1,
                });
                Ok(())
            }
        }
    }

    pub fn snapshot(
        &self,
        captured_at_unix_nanos: i64,
    ) -> Result<DiagnosticsSnapshot, ObservabilityError> {
        if captured_at_unix_nanos <= 0 {
            return Err(ObservabilityError::InvalidTimestamp);
        }
        let metrics = self.lock()?;
        let series = metrics
            .iter()
            .map(|(key, value)| MetricSeries {
                key: *key,
                value: match value {
                    StoredMetric::Counter(value) => MetricSnapshotValue::Counter { value: *value },
                    StoredMetric::Gauge(value) => MetricSnapshotValue::Gauge { value: *value },
                    StoredMetric::Histogram { counts, sum, count } => {
                        MetricSnapshotValue::Histogram {
                            bounds: HISTOGRAM_BOUNDS_SECONDS,
                            counts: Box::new(*counts),
                            sum: *sum,
                            count: *count,
                        }
                    }
                },
            })
            .collect();
        Ok(DiagnosticsSnapshot {
            schema_version: 1,
            captured_at_unix_nanos,
            series,
        })
    }

    fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<MetricKey, StoredMetric>>, ObservabilityError>
    {
        self.inner.lock().map_err(|_| ObservabilityError::Poisoned)
    }
}

fn bounded_entry(
    metrics: &mut BTreeMap<MetricKey, StoredMetric>,
    key: MetricKey,
) -> Result<Entry<'_, MetricKey, StoredMetric>, ObservabilityError> {
    if !metrics.contains_key(&key) && metrics.len() >= MAX_METRIC_SERIES {
        return Err(ObservabilityError::SeriesCapacity);
    }
    Ok(metrics.entry(key))
}

fn histogram_bucket(value: f64) -> usize {
    HISTOGRAM_BOUNDS_SECONDS
        .iter()
        .position(|bound| value <= *bound)
        .unwrap_or(HISTOGRAM_BOUNDS_SECONDS.len())
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetricSnapshotValue {
    Counter {
        value: u64,
    },
    Gauge {
        value: i64,
    },
    Histogram {
        bounds: [f64; 12],
        counts: Box<[u64; 13]>,
        sum: f64,
        count: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MetricSeries {
    pub key: MetricKey,
    pub value: MetricSnapshotValue,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiagnosticsSnapshot {
    schema_version: u32,
    captured_at_unix_nanos: i64,
    series: Vec<MetricSeries>,
}

impl DiagnosticsSnapshot {
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn captured_at_unix_nanos(&self) -> i64 {
        self.captured_at_unix_nanos
    }

    pub fn series(&self) -> &[MetricSeries] {
        &self.series
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SloObservation {
    pub metric: MetricName,
    pub observed: f64,
    pub target: f64,
    pub sample_count: u64,
}

impl SloObservation {
    pub fn passes(self) -> bool {
        self.sample_count > 0
            && self.observed.is_finite()
            && !self.observed.is_sign_negative()
            && self.target.is_finite()
            && !self.target.is_sign_negative()
            && self.observed <= self.target
    }
}
