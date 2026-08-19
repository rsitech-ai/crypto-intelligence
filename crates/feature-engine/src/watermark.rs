//! Deterministic, generation-aware event-time watermark tracking.

use std::{cmp::Ordering, collections::BTreeMap, fmt};

use domain::{SourceId, UnixNanos};
use feature_registry::{DurationNanos, FeatureEntity, SourceCoverage, SourceCoverageEntry};
use quality::SourceHealthState;
use serde::{Deserialize, Deserializer, Serialize, de::Visitor};
use thiserror::Error;

const MAX_PARTITION_ID_LENGTH: usize = 96;
const MAX_PARTITIONS: usize = 128;
const POLICY_HASH_DOMAIN: &[u8] = b"crypto-intelligence/watermark-policy/v1";
const REQUIRED_SOURCE_HASH_DOMAIN: &[u8] = b"crypto-intelligence/watermark-required-sources/v1";

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PartitionId(String);

impl PartitionId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, WatermarkError> {
        let value = value.as_ref();
        Self::validate(value)?;
        Ok(Self(value.to_owned()))
    }

    fn validate(value: &str) -> Result<(), WatermarkError> {
        let valid = !value.is_empty()
            && value.len() <= MAX_PARTITION_ID_LENGTH
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            });
        if !valid {
            return Err(WatermarkError::InvalidPartitionId);
        }
        Ok(())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Ord for PartitionId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for PartitionId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'de> Deserialize<'de> for PartitionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(PartitionIdVisitor)
    }
}

struct PartitionIdVisitor;

impl Visitor<'_> for PartitionIdVisitor {
    type Value = PartitionId;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded lowercase partition identifier")
    }

    fn visit_borrowed_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        PartitionId::new(value).map_err(E::custom)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        PartitionId::new(value).map_err(E::custom)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct WatermarkKey {
    source: SourceId,
    partition: PartitionId,
}

impl WatermarkKey {
    pub const fn new(source: SourceId, partition: PartitionId) -> Self {
        Self { source, partition }
    }

    pub const fn source(&self) -> &SourceId {
        &self.source
    }

    pub const fn partition(&self) -> &PartitionId {
        &self.partition
    }
}

impl Ord for WatermarkKey {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.source.kind() as u8,
            self.source.name(),
            self.source.generation(),
            &self.partition,
        )
            .cmp(&(
                other.source.kind() as u8,
                other.source.name(),
                other.source.generation(),
                &other.partition,
            ))
    }
}

impl PartialOrd for WatermarkKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PartitionRole {
    Required,
    Optional,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionConfig {
    key: WatermarkKey,
    role: PartitionRole,
}

impl PartitionConfig {
    pub const fn required(key: WatermarkKey) -> Self {
        Self {
            key,
            role: PartitionRole::Required,
        }
    }

    pub const fn optional(key: WatermarkKey) -> Self {
        Self {
            key,
            role: PartitionRole::Optional,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatermarkUpdate {
    event_time: UnixNanos,
    as_known_at: UnixNanos,
    health: SourceHealthState,
}

impl WatermarkUpdate {
    pub const fn new(
        event_time: UnixNanos,
        as_known_at: UnixNanos,
        health: SourceHealthState,
    ) -> Self {
        Self {
            event_time,
            as_known_at,
            health,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Finalization {
    Provisional,
    Final,
    Invalid,
}

/// Window-bound evidence produced by the watermark and source-health gate.
///
/// Fields are private so a caller cannot fabricate permission to emit a final
/// or invalid revision without evaluating the configured tracker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalizationDecision {
    window: crate::TimeWindow,
    state: Finalization,
    watermark: Option<UnixNanos>,
    as_known_at: UnixNanos,
    entity_digest: Option<[u8; 32]>,
    required_source_count: usize,
    required_source_digest: [u8; 32],
    evaluation_sequence: u64,
    policy_id: WatermarkPolicyId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorrectionDecision {
    window: crate::TimeWindow,
    source: WatermarkKey,
    corrected_event_time: UnixNanos,
    evaluation_sequence: u64,
    policy_id: WatermarkPolicyId,
}

impl CorrectionDecision {
    pub const fn window(&self) -> crate::TimeWindow {
        self.window
    }

    pub const fn source(&self) -> &WatermarkKey {
        &self.source
    }

    pub const fn corrected_event_time(&self) -> UnixNanos {
        self.corrected_event_time
    }

    pub const fn evaluation_sequence(&self) -> u64 {
        self.evaluation_sequence
    }

    pub(crate) const fn policy_id(&self) -> WatermarkPolicyId {
        self.policy_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WatermarkPolicyId([u8; 32]);

impl FinalizationDecision {
    pub const fn window(self) -> crate::TimeWindow {
        self.window
    }

    pub const fn state(self) -> Finalization {
        self.state
    }

    /// Lowest raw watermark among the required partitions at evaluation time.
    pub const fn watermark(self) -> Option<UnixNanos> {
        self.watermark
    }

    /// Earliest point in processing time at which this exact decision existed.
    pub const fn as_known_at(self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn evaluation_sequence(self) -> u64 {
        self.evaluation_sequence
    }

    pub(crate) fn matches_required_sources(self, sources: &[SourceId]) -> bool {
        self.required_source_count == sources.len()
            && self.required_source_digest == fingerprint_sources(sources.iter())
    }

    pub(crate) fn matches_entity(self, entity: &FeatureEntity) -> bool {
        self.entity_digest == Some(fingerprint_entity(entity))
    }

    pub(crate) fn evidence_digest(self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"crypto-intelligence/finalization-decision/v1");
        hasher.update(&self.window.start().value().to_be_bytes());
        hasher.update(&self.window.end().value().to_be_bytes());
        hasher.update(&[match self.state {
            Finalization::Provisional => 1,
            Finalization::Final => 2,
            Finalization::Invalid => 3,
        }]);
        match self.watermark {
            Some(watermark) => {
                hasher.update(&[1]);
                hasher.update(&watermark.value().to_be_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        hasher.update(&self.as_known_at.value().to_be_bytes());
        match self.entity_digest {
            Some(digest) => {
                hasher.update(&[1]);
                hasher.update(&digest);
            }
            None => {
                hasher.update(&[0]);
            }
        }
        hasher.update(&(self.required_source_count as u64).to_be_bytes());
        hasher.update(&self.required_source_digest);
        hasher.update(&self.evaluation_sequence.to_be_bytes());
        hasher.update(&self.policy_id.0);
        *hasher.finalize().as_bytes()
    }

    pub(crate) const fn policy_id(self) -> WatermarkPolicyId {
        self.policy_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PartitionState {
    role: PartitionRole,
    watermark: Option<UnixNanos>,
    as_known_at: Option<UnixNanos>,
    health: Option<SourceHealthState>,
}

#[derive(Clone, Debug)]
pub struct WatermarkTracker {
    partitions: BTreeMap<WatermarkKey, PartitionState>,
    allowed_lateness: DurationNanos,
    allowed_health: Vec<SourceHealthState>,
    update_sequence: u64,
    policy_id: WatermarkPolicyId,
    entity_digest: Option<[u8; 32]>,
    required_source_count: usize,
    required_source_digest: [u8; 32],
}

impl WatermarkTracker {
    pub fn try_new(
        configs: Vec<PartitionConfig>,
        allowed_lateness: DurationNanos,
        allowed_health: Vec<SourceHealthState>,
    ) -> Result<Self, WatermarkError> {
        if configs.is_empty()
            || configs.len() > MAX_PARTITIONS
            || allowed_health.is_empty()
            || allowed_health.len() > SourceHealthState::ALL.len()
            || allowed_lateness.value() > i64::MAX as u64
        {
            return Err(WatermarkError::InvalidTracker);
        }
        if !configs
            .iter()
            .any(|config| config.role == PartitionRole::Required)
        {
            return Err(WatermarkError::InvalidTracker);
        }

        let mut unique_health = Vec::with_capacity(allowed_health.len());
        for state in allowed_health {
            if unique_health.contains(&state) {
                return Err(WatermarkError::InvalidTracker);
            }
            unique_health.push(state);
        }
        unique_health.sort_by_key(|state| health_rank(*state));

        let mut partitions = BTreeMap::new();
        for config in configs {
            let state = PartitionState {
                role: config.role,
                watermark: None,
                as_known_at: None,
                health: None,
            };
            if partitions.insert(config.key, state).is_some() {
                return Err(WatermarkError::DuplicatePartition);
            }
        }

        let policy_id = fingerprint_policy(&partitions, allowed_lateness, &unique_health, None);
        let required_sources = canonical_required_sources(&partitions);
        let required_source_count = required_sources.len();
        let required_source_digest = fingerprint_sources(required_sources);
        Ok(Self {
            partitions,
            allowed_lateness,
            allowed_health: unique_health,
            update_sequence: 0,
            policy_id,
            entity_digest: None,
            required_source_count,
            required_source_digest,
        })
    }

    /// Creates a tracker whose decisions are scoped to one canonical entity.
    pub fn try_new_for_entity(
        configs: Vec<PartitionConfig>,
        allowed_lateness: DurationNanos,
        allowed_health: Vec<SourceHealthState>,
        entity: FeatureEntity,
    ) -> Result<Self, WatermarkError> {
        let mut tracker = Self::try_new(configs, allowed_lateness, allowed_health)?;
        let entity_digest = fingerprint_entity(&entity);
        tracker.entity_digest = Some(entity_digest);
        tracker.policy_id = fingerprint_policy(
            &tracker.partitions,
            tracker.allowed_lateness,
            &tracker.allowed_health,
            Some(&entity),
        );
        Ok(tracker)
    }

    pub fn advance(
        &mut self,
        key: &WatermarkKey,
        update: WatermarkUpdate,
    ) -> Result<(), WatermarkError> {
        if update.event_time.value() <= 0 || update.as_known_at < update.event_time {
            return Err(WatermarkError::InvalidWatermark);
        }
        let state = self
            .partitions
            .get_mut(key)
            .ok_or(WatermarkError::UnknownPartition)?;
        if let Some(current) = state.watermark
            && update.event_time < current
        {
            return Err(WatermarkError::Regression {
                current,
                attempted: update.event_time,
            });
        }
        if state
            .as_known_at
            .is_some_and(|current| update.as_known_at < current)
        {
            return Err(WatermarkError::InvalidWatermark);
        }
        let next_sequence = self
            .update_sequence
            .checked_add(1)
            .ok_or(WatermarkError::ArithmeticOverflow)?;
        state.watermark = Some(update.event_time);
        state.as_known_at = Some(update.as_known_at);
        state.health = Some(update.health);
        self.update_sequence = next_sequence;
        Ok(())
    }

    pub fn watermark(&self, key: &WatermarkKey) -> Option<UnixNanos> {
        self.partitions.get(key).and_then(|state| state.watermark)
    }

    pub(crate) fn coverage_for_decision(
        &self,
        decision: FinalizationDecision,
    ) -> Option<SourceCoverage> {
        if decision.policy_id != self.policy_id
            || decision.evaluation_sequence != self.update_sequence
        {
            return None;
        }
        let expected = canonical_required_sources(&self.partitions);
        if !decision.matches_required_sources(
            &expected
                .iter()
                .map(|source| (*source).clone())
                .collect::<Vec<_>>(),
        ) {
            return None;
        }
        let mut observed = Vec::with_capacity(expected.len());
        for source in &expected {
            let mut health = None;
            let mut complete = true;
            for (key, state) in &self.partitions {
                if state.role != PartitionRole::Required || key.source() != *source {
                    continue;
                }
                let Some(partition_health) = state.health else {
                    complete = false;
                    break;
                };
                health = Some(health.map_or(partition_health, |current| {
                    worse_health(current, partition_health)
                }));
            }
            if complete && let Some(health) = health {
                observed.push(SourceCoverageEntry::new((*source).clone(), health));
            }
        }
        SourceCoverage::try_new_partial(expected.into_iter().cloned().collect(), observed).ok()
    }

    pub(crate) fn validates_decision(
        &self,
        decision: FinalizationDecision,
        entity: &FeatureEntity,
    ) -> bool {
        decision.policy_id == self.policy_id
            && decision.evaluation_sequence == self.update_sequence
            && decision.matches_entity(entity)
    }

    pub fn decision(&self, window: crate::TimeWindow) -> FinalizationDecision {
        let as_known_at = self
            .latest_as_known_at()
            .map_or(window.end(), |latest| latest.max(window.end()));
        FinalizationDecision {
            window,
            state: self.evaluate(window),
            watermark: self.minimum_required_watermark(),
            as_known_at,
            entity_digest: self.entity_digest,
            required_source_count: self.required_source_count,
            required_source_digest: self.required_source_digest,
            evaluation_sequence: self.update_sequence,
            policy_id: self.policy_id,
        }
    }

    /// Records fresh, exact-window evidence that a source corrected an event.
    pub fn correction(
        &mut self,
        source: &WatermarkKey,
        window: crate::TimeWindow,
        corrected_event_time: UnixNanos,
    ) -> Result<CorrectionDecision, WatermarkError> {
        if corrected_event_time < window.start() || corrected_event_time >= window.end() {
            return Err(WatermarkError::InvalidCorrectionTime);
        }
        let source_state = self
            .partitions
            .get(source)
            .ok_or(WatermarkError::UnknownPartition)?;
        if source_state.health.is_none()
            || !source_state
                .health
                .is_some_and(|health| self.allowed_health.contains(&health))
            || self.evaluate(window) != Finalization::Final
        {
            return Err(WatermarkError::InvalidCorrectionState);
        }
        let evaluation_sequence = self
            .update_sequence
            .checked_add(1)
            .ok_or(WatermarkError::ArithmeticOverflow)?;
        self.update_sequence = evaluation_sequence;
        Ok(CorrectionDecision {
            window,
            source: source.clone(),
            corrected_event_time,
            evaluation_sequence,
            policy_id: self.policy_id,
        })
    }

    pub(crate) const fn policy_id(&self) -> WatermarkPolicyId {
        self.policy_id
    }

    pub(crate) fn effective_frontier(&self) -> Option<UnixNanos> {
        let lateness = i64::try_from(self.allowed_lateness.value()).ok()?;
        let mut minimum = None;
        for state in self
            .partitions
            .values()
            .filter(|state| state.role == PartitionRole::Required)
        {
            let (Some(watermark), Some(health)) = (state.watermark, state.health) else {
                return None;
            };
            if !self.allowed_health.contains(&health) {
                return None;
            }
            minimum = Some(minimum.map_or(watermark, |current: UnixNanos| current.min(watermark)));
        }
        let frontier = minimum?.value().checked_sub(lateness)?;
        Some(UnixNanos::new(frontier.max(0)))
    }

    fn minimum_required_watermark(&self) -> Option<UnixNanos> {
        self.partitions
            .values()
            .filter(|state| state.role == PartitionRole::Required)
            .try_fold(None, |minimum, state| {
                let watermark = state.watermark?;
                Some(Some(minimum.map_or(watermark, |current: UnixNanos| {
                    current.min(watermark)
                })))
            })
            .flatten()
    }

    fn latest_as_known_at(&self) -> Option<UnixNanos> {
        self.partitions
            .values()
            .filter_map(|state| state.as_known_at)
            .max()
    }

    fn evaluate(&self, window: crate::TimeWindow) -> Finalization {
        let Ok(lateness) = i64::try_from(self.allowed_lateness.value()) else {
            return Finalization::Invalid;
        };
        let Some(threshold) = window.end().value().checked_add(lateness) else {
            return Finalization::Invalid;
        };

        let mut complete = true;
        for state in self
            .partitions
            .values()
            .filter(|state| state.role == PartitionRole::Required)
        {
            match (state.watermark, state.health) {
                (_, Some(health)) if !self.allowed_health.contains(&health) => {
                    return Finalization::Invalid;
                }
                (Some(watermark), Some(_)) if watermark.value() >= threshold => {}
                _ => complete = false,
            }
        }
        if complete {
            Finalization::Final
        } else {
            Finalization::Provisional
        }
    }
}

fn fingerprint_policy(
    partitions: &BTreeMap<WatermarkKey, PartitionState>,
    allowed_lateness: DurationNanos,
    allowed_health: &[SourceHealthState],
    entity: Option<&FeatureEntity>,
) -> WatermarkPolicyId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(POLICY_HASH_DOMAIN);
    hasher.update(&allowed_lateness.value().to_be_bytes());
    for (key, state) in partitions {
        hasher.update(&[key.source.kind() as u8]);
        hash_bounded_bytes(&mut hasher, key.source.name().as_bytes());
        hasher.update(&key.source.generation().to_be_bytes());
        hash_bounded_bytes(&mut hasher, key.partition.as_str().as_bytes());
        hasher.update(&[match state.role {
            PartitionRole::Required => 1,
            PartitionRole::Optional => 2,
        }]);
    }
    for health in allowed_health {
        hasher.update(&[health_rank(*health)]);
    }
    match entity {
        Some(entity) => {
            hasher.update(&[1]);
            hasher.update(&fingerprint_entity(entity));
        }
        None => {
            hasher.update(&[0]);
        }
    }
    WatermarkPolicyId(*hasher.finalize().as_bytes())
}

fn fingerprint_entity(entity: &FeatureEntity) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crypto-intelligence/watermark-entity/v1");
    crate::features::hash_entity(&mut hasher, entity);
    *hasher.finalize().as_bytes()
}

fn canonical_required_sources(
    partitions: &BTreeMap<WatermarkKey, PartitionState>,
) -> Vec<&SourceId> {
    let mut sources = Vec::new();
    for (key, state) in partitions {
        if state.role == PartitionRole::Required
            && sources
                .last()
                .is_none_or(|previous: &&SourceId| *previous != key.source())
        {
            sources.push(key.source());
        }
    }
    sources
}

fn fingerprint_sources<'a>(sources: impl IntoIterator<Item = &'a SourceId>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(REQUIRED_SOURCE_HASH_DOMAIN);
    for source in sources {
        hasher.update(&[source.kind() as u8]);
        hash_bounded_bytes(&mut hasher, source.name().as_bytes());
        hasher.update(&source.generation().to_be_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn hash_bounded_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

const fn health_rank(state: SourceHealthState) -> u8 {
    match state {
        SourceHealthState::Healthy => 0,
        SourceHealthState::Degraded => 1,
        SourceHealthState::Unhealthy => 2,
        SourceHealthState::Quarantined => 3,
        SourceHealthState::Recovering => 4,
    }
}

const fn worse_health(left: SourceHealthState, right: SourceHealthState) -> SourceHealthState {
    if health_severity(left) >= health_severity(right) {
        left
    } else {
        right
    }
}

const fn health_severity(state: SourceHealthState) -> u8 {
    match state {
        SourceHealthState::Healthy => 0,
        SourceHealthState::Recovering => 1,
        SourceHealthState::Degraded => 2,
        SourceHealthState::Unhealthy => 3,
        SourceHealthState::Quarantined => 4,
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WatermarkError {
    #[error("invalid partition identifier")]
    InvalidPartitionId,
    #[error("invalid watermark tracker configuration")]
    InvalidTracker,
    #[error("duplicate partition configuration")]
    DuplicatePartition,
    #[error("unknown watermark partition")]
    UnknownPartition,
    #[error("watermark timestamp must be positive")]
    InvalidWatermark,
    #[error("watermark update sequence overflow")]
    ArithmeticOverflow,
    #[error("corrected event time is outside the exact target window")]
    InvalidCorrectionTime,
    #[error("correction source or current watermark state is not eligible")]
    InvalidCorrectionState,
    #[error("watermark regressed from {current:?} to {attempted:?}")]
    Regression {
        current: UnixNanos,
        attempted: UnixNanos,
    },
}
