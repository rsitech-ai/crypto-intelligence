//! Point-in-time training data and discrete risk-set expansion.

use std::collections::BTreeSet;

use crate::{BucketSpec, HazardError};

const NANOS_PER_SECOND: i64 = 1_000_000_000;
const MAXIMUM_FEATURES: usize = 256;
const MAXIMUM_CAUSES: usize = 32;
const MAXIMUM_SAMPLES: usize = 1_000_000;
const MAXIMUM_DESIGN_ROWS: usize = 4_000_000;
const EVIDENCE_DOMAIN: &[u8] = b"cmti:hazard-training-evidence:v1\0";

/// Availability at the supervised-model boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputQuality {
    Available,
    Degraded,
    Unavailable,
}

/// The first modeled event or the fully observed right-censoring boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HazardOutcome {
    Event {
        offset_seconds: u64,
        cause_index: usize,
    },
    RightCensored {
        observed_seconds: u64,
    },
}

/// Ordered feature contract used by fitting and prediction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureSchema {
    version: u32,
    feature_ids: Vec<String>,
}

impl FeatureSchema {
    pub fn try_new(version: u32, feature_ids: Vec<String>) -> Result<Self, HazardError> {
        if version == 0 || feature_ids.is_empty() || feature_ids.len() > MAXIMUM_FEATURES {
            return Err(HazardError::InvalidFeatureSchema);
        }
        let mut unique = BTreeSet::new();
        if feature_ids
            .iter()
            .any(|identifier| !valid_identifier(identifier) || !unique.insert(identifier.as_str()))
        {
            return Err(HazardError::InvalidFeatureSchema);
        }
        Ok(Self {
            version,
            feature_ids,
        })
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub fn feature_ids(&self) -> &[String] {
        &self.feature_ids
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.feature_ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.feature_ids.is_empty()
    }
}

/// Untrusted boundary input for one supervised example.
#[derive(Clone, Debug, PartialEq)]
pub struct HazardSampleInput {
    pub id: u64,
    pub origin_time_ns: i64,
    pub feature_as_known_at_ns: i64,
    pub outcome_known_at_ns: i64,
    pub features: Vec<f64>,
    pub quality: InputQuality,
    pub feature_lineage: [u8; 32],
    pub label_lineage: [u8; 32],
    pub outcome: HazardOutcome,
}

/// Complete input for one immutable training artifact.
#[derive(Clone, Debug, PartialEq)]
pub struct HazardTrainingSetInput {
    pub feature_schema: FeatureSchema,
    pub bucket_spec: BucketSpec,
    pub cause_ids: Vec<String>,
    pub training_cutoff_ns: i64,
    pub samples: Vec<HazardSampleInput>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HazardSample {
    id: u64,
    origin_time_ns: i64,
    feature_as_known_at_ns: i64,
    outcome_known_at_ns: i64,
    features: Vec<f64>,
    quality: InputQuality,
    feature_lineage: [u8; 32],
    label_lineage: [u8; 32],
    outcome: HazardOutcome,
}

/// Validated, ordered, point-in-time training data.
#[derive(Clone, Debug, PartialEq)]
pub struct HazardTrainingSet {
    feature_schema: FeatureSchema,
    bucket_spec: BucketSpec,
    cause_ids: Vec<String>,
    training_cutoff_ns: i64,
    samples: Vec<HazardSample>,
    evidence_id: [u8; 32],
}

impl HazardTrainingSet {
    pub fn try_new(input: HazardTrainingSetInput) -> Result<Self, HazardError> {
        validate_header(&input)?;
        let mut seen_ids = BTreeSet::new();
        let mut previous_origin = None;
        let mut samples = Vec::with_capacity(input.samples.len());
        for sample in input.samples {
            validate_sample(
                &sample,
                &input.feature_schema,
                input.bucket_spec,
                input.cause_ids.len(),
                input.training_cutoff_ns,
                previous_origin,
            )?;
            if !seen_ids.insert(sample.id) {
                return Err(HazardError::DuplicateSample);
            }
            previous_origin = Some(sample.origin_time_ns);
            samples.push(HazardSample {
                id: sample.id,
                origin_time_ns: sample.origin_time_ns,
                feature_as_known_at_ns: sample.feature_as_known_at_ns,
                outcome_known_at_ns: sample.outcome_known_at_ns,
                features: sample.features,
                quality: sample.quality,
                feature_lineage: sample.feature_lineage,
                label_lineage: sample.label_lineage,
                outcome: sample.outcome,
            });
        }

        let evidence_id = evidence_id(
            &input.feature_schema,
            input.bucket_spec,
            &input.cause_ids,
            input.training_cutoff_ns,
            &samples,
        );
        Ok(Self {
            feature_schema: input.feature_schema,
            bucket_spec: input.bucket_spec,
            cause_ids: input.cause_ids,
            training_cutoff_ns: input.training_cutoff_ns,
            samples,
            evidence_id,
        })
    }

    #[must_use]
    pub const fn feature_schema(&self) -> &FeatureSchema {
        &self.feature_schema
    }

    #[must_use]
    pub const fn bucket_spec(&self) -> BucketSpec {
        self.bucket_spec
    }

    #[must_use]
    pub fn cause_ids(&self) -> &[String] {
        &self.cause_ids
    }

    #[must_use]
    pub const fn training_cutoff_ns(&self) -> i64 {
        self.training_cutoff_ns
    }

    #[must_use]
    pub const fn evidence_id(&self) -> [u8; 32] {
        self.evidence_id
    }

    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    pub(crate) fn samples(&self) -> &[HazardSample] {
        &self.samples
    }
}

impl HazardSample {
    pub(crate) fn features(&self) -> &[f64] {
        &self.features
    }
}

/// One augmented person-bucket target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BucketTarget {
    Cause(usize),
    Survival,
}

/// A bounded design row referencing one validated sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesignRow {
    sample_index: usize,
    sample_id: u64,
    bucket_index: usize,
    target: BucketTarget,
}

impl DesignRow {
    #[must_use]
    pub const fn sample_id(self) -> u64 {
        self.sample_id
    }

    #[must_use]
    pub const fn bucket_index(self) -> usize {
        self.bucket_index
    }

    #[must_use]
    pub const fn target(self) -> BucketTarget {
        self.target
    }

    pub(crate) const fn sample_index(self) -> usize {
        self.sample_index
    }
}

/// Expands each sample only over buckets in which it is observed at risk.
pub fn augment_design(data: &HazardTrainingSet) -> Result<Vec<DesignRow>, HazardError> {
    let row_count = data.samples.iter().try_fold(0_usize, |total, sample| {
        total
            .checked_add(observed_bucket_count(sample.outcome, data.bucket_spec)?)
            .ok_or(HazardError::DesignCapacity)
    })?;
    if row_count == 0 || row_count > MAXIMUM_DESIGN_ROWS {
        return Err(HazardError::DesignCapacity);
    }

    let mut rows = Vec::with_capacity(row_count);
    for (sample_index, sample) in data.samples.iter().enumerate() {
        match sample.outcome {
            HazardOutcome::Event {
                offset_seconds,
                cause_index,
            } => {
                let event_bucket = bucket_for_event(data.bucket_spec, offset_seconds)?;
                for bucket_index in 0..event_bucket {
                    rows.push(DesignRow {
                        sample_index,
                        sample_id: sample.id,
                        bucket_index,
                        target: BucketTarget::Survival,
                    });
                }
                rows.push(DesignRow {
                    sample_index,
                    sample_id: sample.id,
                    bucket_index: event_bucket,
                    target: BucketTarget::Cause(cause_index),
                });
            }
            HazardOutcome::RightCensored { observed_seconds } => {
                for bucket_index in 0..fully_observed_buckets(data.bucket_spec, observed_seconds) {
                    rows.push(DesignRow {
                        sample_index,
                        sample_id: sample.id,
                        bucket_index,
                        target: BucketTarget::Survival,
                    });
                }
            }
        }
    }
    Ok(rows)
}

fn validate_header(input: &HazardTrainingSetInput) -> Result<(), HazardError> {
    if input.training_cutoff_ns <= 0
        || input.samples.is_empty()
        || input.samples.len() > MAXIMUM_SAMPLES
        || input.cause_ids.len() < 2
        || input.cause_ids.len() > MAXIMUM_CAUSES
        || input.bucket_spec.is_empty()
    {
        return Err(HazardError::InvalidTrainingSet);
    }
    let mut unique = BTreeSet::new();
    if input
        .cause_ids
        .iter()
        .any(|identifier| !valid_identifier(identifier) || !unique.insert(identifier.as_str()))
    {
        return Err(HazardError::InvalidCauseSchema);
    }
    Ok(())
}

fn validate_sample(
    sample: &HazardSampleInput,
    feature_schema: &FeatureSchema,
    bucket_spec: BucketSpec,
    cause_count: usize,
    training_cutoff_ns: i64,
    previous_origin: Option<i64>,
) -> Result<(), HazardError> {
    if sample.id == 0
        || sample.origin_time_ns <= 0
        || sample.feature_as_known_at_ns <= 0
        || sample.feature_as_known_at_ns > sample.origin_time_ns
        || sample.outcome_known_at_ns < sample.origin_time_ns
        || sample.outcome_known_at_ns > training_cutoff_ns
        || sample.features.len() != feature_schema.len()
        || sample.features.iter().any(|value| !value.is_finite())
        || sample.quality != InputQuality::Available
        || sample.feature_lineage == [0; 32]
        || sample.label_lineage == [0; 32]
        || previous_origin.is_some_and(|previous| sample.origin_time_ns <= previous)
    {
        return Err(HazardError::InvalidSample);
    }

    let observed_seconds = match sample.outcome {
        HazardOutcome::Event {
            offset_seconds,
            cause_index,
        } => {
            if cause_index >= cause_count {
                return Err(HazardError::InvalidOutcome);
            }
            bucket_for_event(bucket_spec, offset_seconds)?;
            offset_seconds
        }
        HazardOutcome::RightCensored { observed_seconds } => {
            if fully_observed_buckets(bucket_spec, observed_seconds) == 0 {
                return Err(HazardError::InvalidOutcome);
            }
            observed_seconds
        }
    };
    let observation_end_ns = duration_end_ns(sample.origin_time_ns, observed_seconds)?;
    if sample.outcome_known_at_ns < observation_end_ns {
        return Err(HazardError::OutcomeKnownTooEarly);
    }
    Ok(())
}

fn bucket_for_event(bucket_spec: BucketSpec, offset_seconds: u64) -> Result<usize, HazardError> {
    if offset_seconds == 0 {
        return Err(HazardError::InvalidOutcome);
    }
    bucket_spec
        .edges_seconds()
        .iter()
        .position(|edge| offset_seconds <= *edge)
        .ok_or(HazardError::InvalidOutcome)
}

fn fully_observed_buckets(bucket_spec: BucketSpec, observed_seconds: u64) -> usize {
    bucket_spec
        .edges_seconds()
        .partition_point(|edge| *edge <= observed_seconds)
}

fn observed_bucket_count(
    outcome: HazardOutcome,
    bucket_spec: BucketSpec,
) -> Result<usize, HazardError> {
    match outcome {
        HazardOutcome::Event { offset_seconds, .. } => {
            bucket_for_event(bucket_spec, offset_seconds)
                .and_then(|index| index.checked_add(1).ok_or(HazardError::DesignCapacity))
        }
        HazardOutcome::RightCensored { observed_seconds } => {
            Ok(fully_observed_buckets(bucket_spec, observed_seconds))
        }
    }
}

fn duration_end_ns(origin_time_ns: i64, seconds: u64) -> Result<i64, HazardError> {
    i64::try_from(seconds)
        .ok()
        .and_then(|value| value.checked_mul(NANOS_PER_SECOND))
        .and_then(|duration| origin_time_ns.checked_add(duration))
        .ok_or(HazardError::TimeOverflow)
}

fn valid_identifier(identifier: &str) -> bool {
    !identifier.is_empty()
        && identifier.len() <= 128
        && identifier.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
}

fn evidence_id(
    feature_schema: &FeatureSchema,
    bucket_spec: BucketSpec,
    cause_ids: &[String],
    training_cutoff_ns: i64,
    samples: &[HazardSample],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(EVIDENCE_DOMAIN);
    hash_u64(&mut hasher, u64::from(feature_schema.version));
    hash_strings(&mut hasher, &feature_schema.feature_ids);
    hash_u64(&mut hasher, u64::from(bucket_spec.version()));
    hash_u64(
        &mut hasher,
        u64::try_from(bucket_spec.len()).unwrap_or(u64::MAX),
    );
    for edge in bucket_spec.edges_seconds() {
        hash_u64(&mut hasher, *edge);
    }
    hash_strings(&mut hasher, cause_ids);
    hasher.update(&training_cutoff_ns.to_le_bytes());
    hash_u64(
        &mut hasher,
        u64::try_from(samples.len()).unwrap_or(u64::MAX),
    );
    for sample in samples {
        hash_u64(&mut hasher, sample.id);
        hasher.update(&sample.origin_time_ns.to_le_bytes());
        hasher.update(&sample.feature_as_known_at_ns.to_le_bytes());
        hasher.update(&sample.outcome_known_at_ns.to_le_bytes());
        hasher.update(&[match sample.quality {
            InputQuality::Available => 0,
            InputQuality::Degraded => 1,
            InputQuality::Unavailable => 2,
        }]);
        hasher.update(&sample.feature_lineage);
        hasher.update(&sample.label_lineage);
        hash_u64(
            &mut hasher,
            u64::try_from(sample.features.len()).unwrap_or(u64::MAX),
        );
        for value in &sample.features {
            hasher.update(&value.to_bits().to_le_bytes());
        }
        match sample.outcome {
            HazardOutcome::Event {
                offset_seconds,
                cause_index,
            } => {
                hasher.update(&[0]);
                hash_u64(&mut hasher, offset_seconds);
                hash_u64(&mut hasher, u64::try_from(cause_index).unwrap_or(u64::MAX));
            }
            HazardOutcome::RightCensored { observed_seconds } => {
                hasher.update(&[1]);
                hash_u64(&mut hasher, observed_seconds);
            }
        }
    }
    *hasher.finalize().as_bytes()
}

fn hash_strings(hasher: &mut blake3::Hasher, values: &[String]) {
    hash_u64(hasher, u64::try_from(values.len()).unwrap_or(u64::MAX));
    for value in values {
        hash_u64(hasher, u64::try_from(value.len()).unwrap_or(u64::MAX));
        hasher.update(value.as_bytes());
    }
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}
