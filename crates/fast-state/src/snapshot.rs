//! Immutable missing-aware fast-state feature snapshots.

use std::sync::Arc;

use feature_registry::{
    FeatureDatum, FeatureEntity, FeatureObservation, FeatureRegistry, FeatureValue, FinalityState,
    MissingnessReason, QualityScore, SourceCoverage,
};
use quality::SourceHealthState;

use crate::{FastStateError, TickEvent, TickKind};

const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const SNAPSHOT_HASH_DOMAIN: &[u8] = b"crypto-intelligence/fast-state-snapshot/v1";
const FEATURE_SCHEMA_HASH_DOMAIN: &[u8] = b"crypto-intelligence/fast-state-feature-schema/v1";
const LINEAGE_HASH_DOMAIN: &[u8] = b"crypto-intelligence/fast-state-lineage/v1";
const INTERNAL_INTERVAL_NS: i64 = 100_000_000;
const MAX_FEATURES: usize = 4_096;
const HEALTHY_SCORE_MILLIONTHS: u32 = 900_000;
const DEGRADED_SCORE_MILLIONTHS: u32 = 700_000;

/// Trusted in-process handoff from the feature engine.
///
/// Registry validation below enforces contract shape and model eligibility. It
/// does not authenticate observation origin or recompute source authority, so
/// callers must pass observations emitted by the feature engine rather than
/// caller-assembled substitutes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FastStateSnapshotInput {
    pub as_of_event_time_ns: i64,
    pub observations: Vec<FeatureObservation>,
}

/// Deterministically derived summary; the input has no independent health label.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SnapshotHealth {
    Healthy,
    Degraded,
    Unavailable,
}

/// Dense values and missingness as one missing-aware immutable view.
#[derive(Clone, Copy, Debug)]
pub struct MaskedFeatureVector<'a> {
    values: &'a [f64],
    missingness_mask: &'a [u64],
}

impl<'a> MaskedFeatureVector<'a> {
    pub fn len(self) -> usize {
        self.values.len()
    }

    pub fn is_empty(self) -> bool {
        self.values.is_empty()
    }

    pub fn feature(self, index: usize) -> Option<Option<f64>> {
        let value = *self.values.get(index)?;
        Some(if is_missing(self.missingness_mask, index) {
            None
        } else {
            Some(value)
        })
    }

    pub fn iter(self) -> impl DoubleEndedIterator<Item = Option<f64>> + ExactSizeIterator + 'a {
        (0..self.values.len()).map(move |index| {
            if is_missing(self.missingness_mask, index) {
                None
            } else {
                Some(self.values[index])
            }
        })
    }
}

/// Complete immutable fast-state input consumed by later inference.
#[derive(Clone, Debug, PartialEq)]
pub struct FastStateSnapshot {
    publication_sequence: u64,
    entity: FeatureEntity,
    as_of_event_time_ns: i64,
    as_known_at_ns: i64,
    source_watermark_ns: Option<i64>,
    observations: Arc<[FeatureObservation]>,
    feature_values: Arc<[f64]>,
    missingness_mask: Arc<[u64]>,
    missingness_reasons: Arc<[Option<MissingnessReason>]>,
    missing_feature_count: usize,
    minimum_source_coverage_millionths: u32,
    minimum_quality_score: QualityScore,
    health: SnapshotHealth,
    feature_schema_hash: [u8; 32],
    lineage_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl FastStateSnapshot {
    /// Freeze one authoritative snapshot at an emitted publication boundary.
    ///
    /// This is the handoff between the lossless scheduler queue and immutable
    /// inference state. UI delivery remains a separate, explicitly lossy copy.
    pub fn try_for_published_tick(
        registry: &FeatureRegistry,
        tick: TickEvent,
        input: FastStateSnapshotInput,
    ) -> Result<Self, FastStateError> {
        if tick.kind() != TickKind::Published1S {
            return Err(FastStateError::PublicationRequiresPublishedTick);
        }
        if tick.scheduled_at_ns() != input.as_of_event_time_ns {
            return Err(FastStateError::PublicationTimeMismatch);
        }
        Self::build(registry, tick.sequence(), input)
    }

    /// Build a later-known correction of an already published event snapshot.
    ///
    /// A correction retains the consumed publication tick identity and must
    /// advance at least one observation by exactly one revision without
    /// regressing any observation knowledge or finality timestamp.
    pub fn try_correction(
        registry: &FeatureRegistry,
        previous: &Self,
        input: FastStateSnapshotInput,
    ) -> Result<Self, FastStateError> {
        if input.as_of_event_time_ns != previous.as_of_event_time_ns {
            return Err(FastStateError::SnapshotCorrectionMismatch);
        }
        let next = Self::build(registry, previous.publication_sequence, input)?;
        if !is_monotonic_correction(previous, &next) {
            return Err(FastStateError::SnapshotCorrectionMismatch);
        }
        Ok(next)
    }

    fn build(
        registry: &FeatureRegistry,
        publication_sequence: u64,
        mut input: FastStateSnapshotInput,
    ) -> Result<Self, FastStateError> {
        if input.as_of_event_time_ns <= 0 || input.as_of_event_time_ns % INTERNAL_INTERVAL_NS != 0 {
            return Err(FastStateError::InvalidSnapshotTime);
        }
        if input.observations.is_empty() || input.observations.len() > MAX_FEATURES {
            return Err(FastStateError::InvalidFeatureCount);
        }
        input.observations.sort_by(compare_observations);
        if input
            .observations
            .windows(2)
            .any(|pair| same_feature_key(&pair[0], &pair[1]))
        {
            return Err(FastStateError::DuplicateFeatureObservation);
        }
        let mut has_expired_observation = false;
        for observation in &input.observations {
            registry.validate_model_input_observation(observation)?;
            if observation.event_time_end().value() > input.as_of_event_time_ns {
                return Err(FastStateError::InvalidSnapshotTime);
            }
            let definition = registry
                .get(observation.feature_id(), observation.feature_version())
                .ok_or(feature_registry::RegistryError::UnknownDefinition)?;
            let age_ns =
                u64::try_from(input.as_of_event_time_ns - observation.event_time_end().value())
                    .map_err(|_| FastStateError::InvalidSnapshotTime)?;
            has_expired_observation |= age_ns > definition.time_to_live().value();
        }

        let entity = input.observations[0].entity().clone();
        if input
            .observations
            .iter()
            .any(|observation| observation.entity() != &entity)
        {
            return Err(FastStateError::MixedSnapshotEntity);
        }

        let mut feature_values = Vec::with_capacity(input.observations.len());
        let mask_words = input
            .observations
            .len()
            .checked_add(63)
            .ok_or(FastStateError::InvalidFeatureCount)?
            / 64;
        let mut missingness_mask = vec![0_u64; mask_words];
        let mut missingness_reasons = Vec::with_capacity(input.observations.len());
        let mut missing_feature_count = 0;
        for (index, observation) in input.observations.iter().enumerate() {
            match observation.datum() {
                FeatureDatum::Present(FeatureValue::Float64(value)) => {
                    feature_values.push(value.value());
                    missingness_reasons.push(None);
                }
                FeatureDatum::Missing(reason) => {
                    feature_values.push(0.0);
                    missingness_mask[index / 64] |= 1_u64 << (index % 64);
                    missingness_reasons.push(Some(*reason));
                    missing_feature_count += 1;
                }
                FeatureDatum::Present(_) => return Err(FastStateError::UnsupportedFeatureValue),
            }
        }

        let as_known_at_ns = input
            .observations
            .iter()
            .map(|observation| observation.as_known_at().value())
            .max()
            .ok_or(FastStateError::InvalidFeatureCount)?;
        let source_watermark_ns = minimum_watermark(&input.observations);
        let minimum_source_coverage_millionths = input
            .observations
            .iter()
            .map(|observation| observation.source_coverage().coverage_millionths())
            .min()
            .ok_or(FastStateError::InvalidFeatureCount)?;
        let minimum_quality_score = input
            .observations
            .iter()
            .map(FeatureObservation::quality_score)
            .min()
            .ok_or(FastStateError::InvalidFeatureCount)?;
        let health = derive_health(
            source_watermark_ns,
            &input.observations,
            missing_feature_count,
            has_expired_observation,
            minimum_source_coverage_millionths,
            minimum_quality_score,
        );
        let feature_schema_hash = calculate_feature_schema_hash(registry, &input.observations)?;
        let lineage_hash = calculate_lineage_hash(feature_schema_hash, &input.observations)?;
        let evidence_hash = calculate_evidence_hash(SnapshotEvidenceInput {
            publication_sequence,
            as_of_event_time_ns: input.as_of_event_time_ns,
            as_known_at_ns,
            source_watermark_ns,
            observations: &input.observations,
            feature_values: &feature_values,
            missingness_mask: &missingness_mask,
            missingness_reasons: &missingness_reasons,
            minimum_source_coverage_millionths,
            minimum_quality_score,
            health,
            feature_schema_hash,
            lineage_hash,
        })?;

        Ok(Self {
            publication_sequence,
            entity,
            as_of_event_time_ns: input.as_of_event_time_ns,
            as_known_at_ns,
            source_watermark_ns,
            observations: input.observations.into(),
            feature_values: feature_values.into(),
            missingness_mask: missingness_mask.into(),
            missingness_reasons: missingness_reasons.into(),
            missing_feature_count,
            minimum_source_coverage_millionths,
            minimum_quality_score,
            health,
            feature_schema_hash,
            lineage_hash,
            evidence_hash,
        })
    }

    pub const fn schema_version(&self) -> u32 {
        SNAPSHOT_SCHEMA_VERSION
    }

    /// Scheduler sequence of the consumed one-second publication tick.
    pub const fn publication_sequence(&self) -> u64 {
        self.publication_sequence
    }

    pub const fn entity(&self) -> &FeatureEntity {
        &self.entity
    }

    pub const fn as_of_event_time_ns(&self) -> i64 {
        self.as_of_event_time_ns
    }

    pub const fn as_known_at_ns(&self) -> i64 {
        self.as_known_at_ns
    }

    pub const fn source_watermark_ns(&self) -> Option<i64> {
        self.source_watermark_ns
    }

    pub fn feature_count(&self) -> usize {
        self.feature_values.len()
    }

    pub const fn missing_feature_count(&self) -> usize {
        self.missing_feature_count
    }

    pub fn feature_vector(&self) -> MaskedFeatureVector<'_> {
        MaskedFeatureVector {
            values: &self.feature_values,
            missingness_mask: &self.missingness_mask,
        }
    }

    pub fn feature(&self, index: usize) -> Option<Option<f64>> {
        self.feature_vector().feature(index)
    }

    pub fn missingness_reason(&self, index: usize) -> Option<Option<MissingnessReason>> {
        self.missingness_reasons.get(index).copied()
    }

    pub fn observation(&self, index: usize) -> Option<&FeatureObservation> {
        self.observations.get(index)
    }

    pub fn source_coverage(&self, index: usize) -> Option<&SourceCoverage> {
        self.observation(index)
            .map(FeatureObservation::source_coverage)
    }

    pub fn quality_score(&self, index: usize) -> Option<QualityScore> {
        self.observation(index)
            .map(FeatureObservation::quality_score)
    }

    pub const fn minimum_source_coverage_millionths(&self) -> u32 {
        self.minimum_source_coverage_millionths
    }

    pub const fn minimum_quality_score_millionths(&self) -> u32 {
        self.minimum_quality_score.millionths()
    }

    pub const fn health(&self) -> SnapshotHealth {
        self.health
    }

    pub const fn feature_schema_hash(&self) -> [u8; 32] {
        self.feature_schema_hash
    }

    pub const fn lineage_hash(&self) -> [u8; 32] {
        self.lineage_hash
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

/// One capacity-one UI delivery with explicit coalescing evidence.
#[derive(Clone, Debug)]
pub struct PublishedFastStateSnapshot {
    sequence: u64,
    coalesced_before_delivery: u64,
    snapshot: Arc<FastStateSnapshot>,
}

impl PublishedFastStateSnapshot {
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn coalesced_before_delivery(&self) -> u64 {
        self.coalesced_before_delivery
    }

    pub const fn snapshot(&self) -> &Arc<FastStateSnapshot> {
        &self.snapshot
    }
}

/// Capacity-one slot for explicitly lossy UI state only.
///
/// Authoritative inference/replay work uses `FastStateScheduler` and must never
/// pass through this coalescing boundary.
#[derive(Clone, Debug, Default)]
pub struct CoalescingSnapshotSlot {
    latest: Option<(u64, Arc<FastStateSnapshot>)>,
    last_accepted: Option<Arc<FastStateSnapshot>>,
    entity: Option<FeatureEntity>,
    last_sequence: u64,
    coalesced_total: u64,
    coalesced_since_delivery: u64,
}

impl CoalescingSnapshotSlot {
    pub const fn new() -> Self {
        Self {
            latest: None,
            last_accepted: None,
            entity: None,
            last_sequence: 0,
            coalesced_total: 0,
            coalesced_since_delivery: 0,
        }
    }

    pub fn publish(&mut self, snapshot: Arc<FastStateSnapshot>) -> Result<u64, FastStateError> {
        if self
            .entity
            .as_ref()
            .is_some_and(|entity| entity != snapshot.entity())
        {
            return Err(FastStateError::SnapshotEntityMismatch);
        }
        if self.last_accepted.as_ref().is_some_and(|previous| {
            snapshot.as_of_event_time_ns() < previous.as_of_event_time_ns()
                || snapshot.as_known_at_ns() <= previous.as_known_at_ns()
        }) {
            return Err(FastStateError::SnapshotRegression);
        }
        if self.last_accepted.as_ref().is_some_and(|previous| {
            snapshot.as_of_event_time_ns() == previous.as_of_event_time_ns()
                && !is_monotonic_correction(previous, &snapshot)
        }) {
            return Err(FastStateError::SnapshotCorrectionMismatch);
        }
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or(FastStateError::CounterOverflow)?;
        let (coalesced_total, coalesced_since_delivery) = if self.latest.is_some() {
            (
                self.coalesced_total
                    .checked_add(1)
                    .ok_or(FastStateError::CounterOverflow)?,
                self.coalesced_since_delivery
                    .checked_add(1)
                    .ok_or(FastStateError::CounterOverflow)?,
            )
        } else {
            (self.coalesced_total, self.coalesced_since_delivery)
        };

        self.entity.get_or_insert_with(|| snapshot.entity().clone());
        self.last_sequence = sequence;
        self.coalesced_total = coalesced_total;
        self.coalesced_since_delivery = coalesced_since_delivery;
        self.last_accepted = Some(Arc::clone(&snapshot));
        self.latest = Some((sequence, snapshot));
        Ok(sequence)
    }

    pub const fn coalesced_total(&self) -> u64 {
        self.coalesced_total
    }

    pub fn take_latest(&mut self) -> Option<PublishedFastStateSnapshot> {
        let (sequence, snapshot) = self.latest.take()?;
        let coalesced_before_delivery = self.coalesced_since_delivery;
        self.coalesced_since_delivery = 0;
        Some(PublishedFastStateSnapshot {
            sequence,
            coalesced_before_delivery,
            snapshot,
        })
    }
}

fn is_monotonic_correction(previous: &FastStateSnapshot, next: &FastStateSnapshot) -> bool {
    if previous.feature_schema_hash != next.feature_schema_hash
        || previous.entity != next.entity
        || previous.as_of_event_time_ns != next.as_of_event_time_ns
        || previous.publication_sequence != next.publication_sequence
        || next.as_known_at_ns <= previous.as_known_at_ns
        || previous.observations.len() != next.observations.len()
    {
        return false;
    }
    let mut advanced = false;
    for (previous, next) in previous.observations.iter().zip(next.observations.iter()) {
        if !same_correction_identity(previous, next) {
            return false;
        }
        if next == previous {
            continue;
        }
        if previous.revision().value().checked_add(1) != Some(next.revision().value())
            || next.finality_state() != FinalityState::Corrected
            || next.as_known_at() < previous.as_known_at()
            || next.computed_at() < previous.computed_at()
            || next.finality_as_known_at() < previous.finality_as_known_at()
        {
            return false;
        }
        advanced = true;
    }
    advanced
}

fn compare_observations(
    left: &FeatureObservation,
    right: &FeatureObservation,
) -> std::cmp::Ordering {
    left.feature_id()
        .cmp(right.feature_id())
        .then_with(|| left.feature_version().cmp(right.feature_version()))
        .then_with(|| left.window_id().cmp(right.window_id()))
}

fn same_feature_key(left: &FeatureObservation, right: &FeatureObservation) -> bool {
    left.feature_id() == right.feature_id()
        && left.feature_version() == right.feature_version()
        && left.window_id() == right.window_id()
}

fn same_correction_identity(left: &FeatureObservation, right: &FeatureObservation) -> bool {
    same_feature_key(left, right)
        && left.entity() == right.entity()
        && left.event_time_start() == right.event_time_start()
        && left.event_time_end() == right.event_time_end()
}

fn minimum_watermark(observations: &[FeatureObservation]) -> Option<i64> {
    let mut minimum = None;
    for observation in observations {
        let watermark = observation.watermark()?.value();
        minimum = Some(minimum.map_or(watermark, |value: i64| value.min(watermark)));
    }
    minimum
}

fn derive_health(
    source_watermark_ns: Option<i64>,
    observations: &[FeatureObservation],
    missing_feature_count: usize,
    has_expired_observation: bool,
    minimum_coverage_millionths: u32,
    minimum_quality_score: QualityScore,
) -> SnapshotHealth {
    let has_unavailable_source = observations.iter().any(|observation| {
        observation.finality_state() == FinalityState::Invalid
            || observation.source_coverage().entries().iter().any(|entry| {
                matches!(
                    entry.health(),
                    SourceHealthState::Unhealthy | SourceHealthState::Quarantined
                )
            })
    });
    let all_features_missing = missing_feature_count == observations.len();
    let has_unavailable_missingness = observations.iter().any(|observation| {
        matches!(
            observation.datum().missingness_reason(),
            Some(
                MissingnessReason::NotListed
                    | MissingnessReason::SourceNotSupported
                    | MissingnessReason::SourceDisconnected
                    | MissingnessReason::SequenceGap
                    | MissingnessReason::Stale
                    | MissingnessReason::PrivacyOrLicenseRestriction
                    | MissingnessReason::Unknown
            )
        )
    });
    if source_watermark_ns.is_none()
        || has_unavailable_source
        || all_features_missing
        || has_unavailable_missingness
        || has_expired_observation
        || minimum_coverage_millionths < DEGRADED_SCORE_MILLIONTHS
        || minimum_quality_score.millionths() < DEGRADED_SCORE_MILLIONTHS
    {
        return SnapshotHealth::Unavailable;
    }
    let has_degraded_source = observations.iter().any(|observation| {
        observation.source_coverage().entries().iter().any(|entry| {
            matches!(
                entry.health(),
                SourceHealthState::Degraded | SourceHealthState::Recovering
            )
        })
    });
    if missing_feature_count > 0
        || has_degraded_source
        || minimum_coverage_millionths < HEALTHY_SCORE_MILLIONTHS
        || minimum_quality_score.millionths() < HEALTHY_SCORE_MILLIONTHS
    {
        SnapshotHealth::Degraded
    } else {
        SnapshotHealth::Healthy
    }
}

fn calculate_feature_schema_hash(
    registry: &FeatureRegistry,
    observations: &[FeatureObservation],
) -> Result<[u8; 32], FastStateError> {
    let mut evidence = CanonicalEvidence::new(FEATURE_SCHEMA_HASH_DOMAIN)?;
    evidence.len(observations.len())?;
    for observation in observations {
        let definition = registry
            .get(observation.feature_id(), observation.feature_version())
            .ok_or(feature_registry::RegistryError::UnknownDefinition)?;
        evidence.bytes(
            &serde_json::to_vec(&(definition, observation.window_id()))
                .map_err(|_| FastStateError::EvidenceEncoding)?,
        )?;
    }
    Ok(evidence.finish())
}

fn calculate_lineage_hash(
    feature_schema_hash: [u8; 32],
    observations: &[FeatureObservation],
) -> Result<[u8; 32], FastStateError> {
    let mut evidence = CanonicalEvidence::new(LINEAGE_HASH_DOMAIN)?;
    evidence.bytes(&feature_schema_hash)?;
    evidence.len(observations.len())?;
    for observation in observations {
        evidence.bytes(&observation.lineage_hash().bytes())?;
    }
    Ok(evidence.finish())
}

struct SnapshotEvidenceInput<'a> {
    publication_sequence: u64,
    as_of_event_time_ns: i64,
    as_known_at_ns: i64,
    source_watermark_ns: Option<i64>,
    observations: &'a [FeatureObservation],
    feature_values: &'a [f64],
    missingness_mask: &'a [u64],
    missingness_reasons: &'a [Option<MissingnessReason>],
    minimum_source_coverage_millionths: u32,
    minimum_quality_score: QualityScore,
    health: SnapshotHealth,
    feature_schema_hash: [u8; 32],
    lineage_hash: [u8; 32],
}

fn calculate_evidence_hash(input: SnapshotEvidenceInput<'_>) -> Result<[u8; 32], FastStateError> {
    let mut evidence = CanonicalEvidence::new(SNAPSHOT_HASH_DOMAIN)?;
    evidence.u32(SNAPSHOT_SCHEMA_VERSION);
    evidence.u64(input.publication_sequence);
    evidence.i64(input.as_of_event_time_ns);
    evidence.i64(input.as_known_at_ns);
    evidence.optional_i64(input.source_watermark_ns);
    evidence.bytes(
        &serde_json::to_vec(input.observations).map_err(|_| FastStateError::EvidenceEncoding)?,
    )?;
    evidence.len(input.feature_values.len())?;
    for value in input.feature_values {
        evidence.u64(value.to_bits());
    }
    evidence.len(input.missingness_mask.len())?;
    for word in input.missingness_mask {
        evidence.u64(*word);
    }
    evidence.bytes(
        &serde_json::to_vec(input.missingness_reasons)
            .map_err(|_| FastStateError::EvidenceEncoding)?,
    )?;
    evidence.u32(input.minimum_source_coverage_millionths);
    evidence.u32(input.minimum_quality_score.millionths());
    evidence.u8(match input.health {
        SnapshotHealth::Healthy => 1,
        SnapshotHealth::Degraded => 2,
        SnapshotHealth::Unavailable => 3,
    });
    evidence.bytes(&input.feature_schema_hash)?;
    evidence.bytes(&input.lineage_hash)?;
    Ok(evidence.finish())
}

fn is_missing(mask: &[u64], index: usize) -> bool {
    mask.get(index / 64)
        .is_some_and(|word| word & (1_u64 << (index % 64)) != 0)
}

struct CanonicalEvidence {
    hasher: blake3::Hasher,
}

impl CanonicalEvidence {
    fn new(domain: &[u8]) -> Result<Self, FastStateError> {
        let mut evidence = Self {
            hasher: blake3::Hasher::new(),
        };
        evidence.bytes(domain)?;
        Ok(evidence)
    }

    fn len(&mut self, value: usize) -> Result<(), FastStateError> {
        self.u64(u64::try_from(value).map_err(|_| FastStateError::CounterOverflow)?);
        Ok(())
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), FastStateError> {
        let length = u64::try_from(value.len()).map_err(|_| FastStateError::CounterOverflow)?;
        self.hasher.update(&length.to_le_bytes());
        self.hasher.update(value);
        Ok(())
    }

    fn optional_i64(&mut self, value: Option<i64>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.i64(value);
            }
            None => self.u8(0),
        }
    }

    fn u8(&mut self, value: u8) {
        self.hasher.update(&[value]);
    }

    fn u32(&mut self, value: u32) {
        self.hasher.update(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.hasher.update(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.hasher.update(&value.to_le_bytes());
    }

    fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }
}
