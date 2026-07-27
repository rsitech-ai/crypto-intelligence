use std::{
    collections::BTreeMap,
    fs::File,
    io::Write,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const REDACTED: &str = "[REDACTED]";
const HISTOGRAM_BOUNDS: [u64; 12] = [
    10, 25, 50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 50_000, 250_000,
];
const MAX_LOG_LINE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricName {
    EventsReceived,
    EventsRejected,
    SequenceGaps,
    ChecksumFailures,
    Reconnects,
    QueueDepth,
    WalLatencyMicros,
    InferenceLatencyMicros,
    RpcLatencyMicros,
    AlertsEmitted,
    Abstentions,
    DroppedLogLines,
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
        sum: u64,
        count: u64,
    },
}

#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Mutex<BTreeMap<MetricKey, StoredMetric>>>,
}

impl Metrics {
    pub fn increment(&self, key: MetricKey, amount: u64) -> Result<(), ObservabilityError> {
        let mut metrics = self
            .inner
            .lock()
            .map_err(|_| ObservabilityError::Poisoned)?;
        match metrics.entry(key).or_insert(StoredMetric::Counter(0)) {
            StoredMetric::Counter(value) => {
                *value = value.saturating_add(amount);
                Ok(())
            }
            _ => Err(ObservabilityError::MetricType),
        }
    }

    pub fn counter(&self, key: MetricKey) -> Result<u64, ObservabilityError> {
        let metrics = self
            .inner
            .lock()
            .map_err(|_| ObservabilityError::Poisoned)?;
        match metrics.get(&key) {
            Some(StoredMetric::Counter(value)) => Ok(*value),
            Some(_) => Err(ObservabilityError::MetricType),
            None => Ok(0),
        }
    }

    pub fn gauge(&self, key: MetricKey, value: i64) -> Result<(), ObservabilityError> {
        let mut metrics = self
            .inner
            .lock()
            .map_err(|_| ObservabilityError::Poisoned)?;
        match metrics.entry(key).or_insert(StoredMetric::Gauge(value)) {
            StoredMetric::Gauge(stored) => {
                *stored = value;
                Ok(())
            }
            _ => Err(ObservabilityError::MetricType),
        }
    }

    pub fn observe(&self, key: MetricKey, value: u64) -> Result<(), ObservabilityError> {
        let mut metrics = self
            .inner
            .lock()
            .map_err(|_| ObservabilityError::Poisoned)?;
        match metrics.entry(key).or_insert(StoredMetric::Histogram {
            counts: [0; 13],
            sum: 0,
            count: 0,
        }) {
            StoredMetric::Histogram { counts, sum, count } => {
                let bucket = HISTOGRAM_BOUNDS
                    .iter()
                    .position(|bound| value <= *bound)
                    .unwrap_or(HISTOGRAM_BOUNDS.len());
                counts[bucket] = counts[bucket].saturating_add(1);
                *sum = sum.saturating_add(value);
                *count = count.saturating_add(1);
                Ok(())
            }
            _ => Err(ObservabilityError::MetricType),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Redactor;

impl Redactor {
    pub fn field(self, key: &str, value: &str) -> String {
        if sensitive_key(key) || sensitive_value(value) {
            REDACTED.to_owned()
        } else {
            sanitize(value)
        }
    }

    pub fn json(self, value: &Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        if sensitive_key(key) {
                            (key.clone(), Value::String(REDACTED.to_owned()))
                        } else {
                            (key.clone(), self.json(value))
                        }
                    })
                    .collect(),
            ),
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .take(1_024)
                    .map(|value| self.json(value))
                    .collect(),
            ),
            Value::String(value) => Value::String(self.field("value", value)),
            value => value.clone(),
        }
    }
}

pub struct LocalJsonLog {
    file: Mutex<File>,
    redactor: Redactor,
}

impl LocalJsonLog {
    pub fn from_file(file: File) -> Self {
        Self {
            file: Mutex::new(file),
            redactor: Redactor,
        }
    }

    pub fn write_event(&self, event: &Value) -> Result<(), ObservabilityError> {
        let sanitized = self.redactor.json(event);
        let encoded = serde_json::to_vec(&sanitized)?;
        if encoded.len() > MAX_LOG_LINE_BYTES {
            return Err(ObservabilityError::LogLineTooLarge);
        }
        let mut file = self.file.lock().map_err(|_| ObservabilityError::Poisoned)?;
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    pub fn sync(&self) -> Result<(), ObservabilityError> {
        self.file
            .lock()
            .map_err(|_| ObservabilityError::Poisoned)?
            .sync_data()?;
        Ok(())
    }

    pub fn shutdown(self) -> Result<(), ObservabilityError> {
        self.sync()
    }
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "password",
        "secret",
        "token",
        "authorization",
        "cookie",
        "api_key",
        "private_key",
        "credential",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

fn sensitive_value(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    lowercase.contains("bearer ")
        || lowercase.contains("password=")
        || lowercase.contains("api_key=")
        || lowercase.starts_with("keychain://")
        || contains_json_secret(value)
}

fn contains_json_secret(value: &str) -> bool {
    serde_json::from_str::<Value>(value)
        .ok()
        .is_some_and(|parsed| match parsed {
            Value::Object(ref map) => {
                map.keys().any(|key| sensitive_key(key))
                    || map
                        .values()
                        .any(|nested| contains_json_secret(&nested.to_string()))
            }
            Value::Array(ref values) => values
                .iter()
                .any(|nested| contains_json_secret(&nested.to_string())),
            _ => false,
        })
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(4_096)
        .collect()
}

#[derive(Debug, Error)]
pub enum ObservabilityError {
    #[error("metric type mismatch")]
    MetricType,
    #[error("observability state is poisoned")]
    Poisoned,
    #[error("structured log line exceeds the local bound")]
    LogLineTooLarge,
    #[error("structured log serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("local log I/O failed: {0}")]
    Io(#[from] std::io::Error),
}
