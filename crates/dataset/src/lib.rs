//! Leakage-safe point-in-time dataset construction and walk-forward folds.

pub mod folds;
pub mod manifest;

use std::collections::{BTreeMap, BTreeSet};

use labels::LabelOutcome;
use semver::Version;
use thiserror::Error;

pub use folds::{FoldBuilder, InnerFold, OuterFold, Sample, TimeRange, WalkForwardSchedule};
pub use manifest::{CorrectionPolicy, DatasetManifest, DatasetManifestInput};

const MAXIMUM_FEATURES_PER_ROW: usize = 4_096;
const MAXIMUM_LABELS_PER_ROW: usize = 256;
const ONE_MILLION: u32 = 1_000_000;
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// One materialized feature selected at its exact point-in-time revision.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureValue {
    pub feature_id: String,
    pub feature_version: Version,
    pub value: Option<f64>,
    pub event_time_end_ns: i64,
    pub as_known_at_ns: i64,
    pub quality_score_millionths: u32,
    pub revision: u32,
    pub lineage_hash: [u8; 32],
}

/// One materialized event outcome whose evidence is bounded by the dataset cutoff.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventLabel {
    pub label_id: String,
    pub label_version: Version,
    pub definition_hash: [u8; 32],
    pub origin_time_ns: i64,
    pub horizon_seconds: u64,
    pub outcome: LabelOutcome,
    pub outcome_known_at_ns: i64,
    pub revision: u32,
    pub source_range_hash: [u8; 32],
}

/// Point-in-time membership and instrument-generation authority for one row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UniverseMembership {
    pub valid_from_ns: i64,
    pub valid_to_ns: Option<i64>,
    pub as_known_at_ns: i64,
    pub revision: u32,
    pub instrument_definition_hash: [u8; 32],
    pub lineage_hash: [u8; 32],
}

/// Complete bounded input for one point-in-time dataset row.
#[derive(Clone, Debug, PartialEq)]
pub struct JoinInput {
    pub asset: String,
    pub instrument_generation: u32,
    pub origin_time_ns: i64,
    pub dataset_cutoff_ns: i64,
    pub universe: UniverseMembership,
    pub features: Vec<FeatureValue>,
    pub labels: Vec<EventLabel>,
    pub minimum_quality_millionths: u32,
}

/// Validated dataset row. Evidence remains available; missing values remain `None`.
#[derive(Clone, Debug, PartialEq)]
pub struct DatasetRow {
    asset: String,
    instrument_generation: u32,
    origin_time_ns: i64,
    dataset_cutoff_ns: i64,
    universe: UniverseMembership,
    features: BTreeMap<String, Option<f64>>,
    feature_evidence: Vec<FeatureValue>,
    labels: Vec<EventLabel>,
    quality_score_millionths: u32,
}

impl DatasetRow {
    pub fn asset(&self) -> &str {
        &self.asset
    }

    pub const fn instrument_generation(&self) -> u32 {
        self.instrument_generation
    }

    pub const fn origin_time_ns(&self) -> i64 {
        self.origin_time_ns
    }

    pub const fn dataset_cutoff_ns(&self) -> i64 {
        self.dataset_cutoff_ns
    }

    pub const fn universe(&self) -> &UniverseMembership {
        &self.universe
    }

    pub const fn features(&self) -> &BTreeMap<String, Option<f64>> {
        &self.features
    }

    pub fn feature_evidence(&self) -> &[FeatureValue] {
        &self.feature_evidence
    }

    pub fn labels(&self) -> &[EventLabel] {
        &self.labels
    }

    pub const fn quality_score_millionths(&self) -> u32 {
        self.quality_score_millionths
    }
}

/// Materializes one row without selecting any evidence unavailable at its boundary.
pub fn join_point_in_time(mut input: JoinInput) -> Result<DatasetRow, DatasetError> {
    validate_join_header(&input)?;
    validate_universe(&input)?;
    if input.features.is_empty() || input.features.len() > MAXIMUM_FEATURES_PER_ROW {
        return Err(DatasetError::FeatureCapacity);
    }
    if input.labels.is_empty() || input.labels.len() > MAXIMUM_LABELS_PER_ROW {
        return Err(DatasetError::LabelCapacity);
    }

    input
        .features
        .sort_by(|left, right| left.feature_id.cmp(&right.feature_id));
    let mut values = BTreeMap::new();
    let mut quality = ONE_MILLION;
    for feature in &input.features {
        validate_feature(
            feature,
            input.origin_time_ns,
            input.minimum_quality_millionths,
        )?;
        if values
            .insert(feature.feature_id.clone(), feature.value)
            .is_some()
        {
            return Err(DatasetError::DuplicateFeature {
                feature_id: feature.feature_id.clone(),
            });
        }
        quality = quality.min(feature.quality_score_millionths);
    }

    input.labels.sort_by(|left, right| {
        left.label_id
            .cmp(&right.label_id)
            .then_with(|| left.label_version.cmp(&right.label_version))
    });
    let mut label_identities = BTreeSet::new();
    for label in &input.labels {
        validate_label(label, input.origin_time_ns, input.dataset_cutoff_ns)?;
        if !label_identities.insert(label.label_id.clone()) {
            return Err(DatasetError::DuplicateLabel {
                label_id: label.label_id.clone(),
            });
        }
    }

    Ok(DatasetRow {
        asset: input.asset,
        instrument_generation: input.instrument_generation,
        origin_time_ns: input.origin_time_ns,
        dataset_cutoff_ns: input.dataset_cutoff_ns,
        universe: input.universe,
        features: values,
        feature_evidence: input.features,
        labels: input.labels,
        quality_score_millionths: quality,
    })
}

fn validate_join_header(input: &JoinInput) -> Result<(), DatasetError> {
    if !valid_identifier(&input.asset)
        || input.instrument_generation == 0
        || input.origin_time_ns <= 0
        || input.dataset_cutoff_ns < input.origin_time_ns
        || input.minimum_quality_millionths > ONE_MILLION
    {
        return Err(DatasetError::InvalidRow);
    }
    Ok(())
}

fn validate_universe(input: &JoinInput) -> Result<(), DatasetError> {
    let universe = &input.universe;
    if universe.revision == 0
        || universe.instrument_definition_hash == [0; 32]
        || universe.lineage_hash == [0; 32]
        || universe.valid_from_ns <= 0
        || universe.as_known_at_ns <= 0
        || universe
            .valid_to_ns
            .is_some_and(|valid_to| valid_to <= universe.valid_from_ns)
    {
        return Err(DatasetError::InvalidUniverse);
    }
    if universe.as_known_at_ns > input.origin_time_ns {
        return Err(DatasetError::UniverseKnownAfterOrigin);
    }
    if input.origin_time_ns < universe.valid_from_ns
        || universe
            .valid_to_ns
            .is_some_and(|valid_to| input.origin_time_ns >= valid_to)
    {
        return Err(DatasetError::OutsideUniverse);
    }
    Ok(())
}

fn validate_feature(
    feature: &FeatureValue,
    origin_time_ns: i64,
    minimum_quality_millionths: u32,
) -> Result<(), DatasetError> {
    if !valid_identifier(&feature.feature_id)
        || !valid_version(&feature.feature_version)
        || feature.event_time_end_ns <= 0
        || feature.as_known_at_ns < feature.event_time_end_ns
        || feature.quality_score_millionths > ONE_MILLION
        || feature.revision == 0
        || feature.lineage_hash == [0; 32]
        || feature.value.is_some_and(|value| !value.is_finite())
    {
        return Err(DatasetError::InvalidFeature {
            feature_id: feature.feature_id.clone(),
        });
    }
    if feature.event_time_end_ns > origin_time_ns {
        return Err(DatasetError::FeatureEventAfterOrigin {
            feature_id: feature.feature_id.clone(),
        });
    }
    if feature.as_known_at_ns > origin_time_ns {
        return Err(DatasetError::FeatureKnownAfterOrigin {
            feature_id: feature.feature_id.clone(),
        });
    }
    if feature.quality_score_millionths < minimum_quality_millionths {
        return Err(DatasetError::FeatureQualityBelowRequirement {
            feature_id: feature.feature_id.clone(),
        });
    }
    Ok(())
}

fn validate_label(
    label: &EventLabel,
    origin_time_ns: i64,
    dataset_cutoff_ns: i64,
) -> Result<(), DatasetError> {
    if !valid_identifier(&label.label_id)
        || !valid_version(&label.label_version)
        || label.definition_hash == [0; 32]
        || label.source_range_hash == [0; 32]
        || label.horizon_seconds == 0
        || label.revision == 0
        || label.outcome_known_at_ns < label.origin_time_ns
    {
        return Err(DatasetError::InvalidLabel {
            label_id: label.label_id.clone(),
        });
    }
    if label.origin_time_ns != origin_time_ns {
        return Err(DatasetError::LabelOriginMismatch {
            label_id: label.label_id.clone(),
        });
    }
    if label.outcome_known_at_ns > dataset_cutoff_ns {
        return Err(DatasetError::LabelKnownAfterCutoff {
            label_id: label.label_id.clone(),
        });
    }
    let horizon_ns = label
        .horizon_seconds
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(DatasetError::TimeOverflow)?;
    let horizon_end_ns = label
        .origin_time_ns
        .checked_add(horizon_ns)
        .ok_or(DatasetError::TimeOverflow)?;
    match label.outcome {
        LabelOutcome::Occurred { offset_seconds } => {
            if offset_seconds == 0 || offset_seconds > label.horizon_seconds {
                return Err(DatasetError::InvalidLabel {
                    label_id: label.label_id.clone(),
                });
            }
            let occurrence_ns = offset_seconds
                .checked_mul(NANOS_PER_SECOND)
                .and_then(|value| i64::try_from(value).ok())
                .and_then(|value| label.origin_time_ns.checked_add(value))
                .ok_or(DatasetError::TimeOverflow)?;
            if label.outcome_known_at_ns < occurrence_ns {
                return Err(DatasetError::InvalidLabel {
                    label_id: label.label_id.clone(),
                });
            }
        }
        LabelOutcome::NotOccurred => {
            if label.outcome_known_at_ns < horizon_end_ns {
                return Err(DatasetError::InvalidLabel {
                    label_id: label.label_id.clone(),
                });
            }
        }
        LabelOutcome::Censored { observed_seconds } => {
            if observed_seconds >= label.horizon_seconds {
                return Err(DatasetError::InvalidLabel {
                    label_id: label.label_id.clone(),
                });
            }
            let observed_ns = observed_seconds
                .checked_mul(NANOS_PER_SECOND)
                .and_then(|value| i64::try_from(value).ok())
                .and_then(|value| label.origin_time_ns.checked_add(value))
                .ok_or(DatasetError::TimeOverflow)?;
            if label.outcome_known_at_ns < observed_ns {
                return Err(DatasetError::InvalidLabel {
                    label_id: label.label_id.clone(),
                });
            }
        }
        LabelOutcome::Excluded(_) => {}
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_version(value: &Version) -> bool {
    value.major > 0 && value.pre.is_empty() && value.build.is_empty()
}

/// Fail-closed dataset construction error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DatasetError {
    #[error("dataset manifest is invalid or exceeds a bound")]
    InvalidManifest,
    #[error("dataset manifest could not be serialized canonically")]
    ManifestSerialization,
    #[error("dataset row header is invalid")]
    InvalidRow,
    #[error("universe membership is invalid")]
    InvalidUniverse,
    #[error("universe membership was not known at the prediction origin")]
    UniverseKnownAfterOrigin,
    #[error("prediction origin is outside the point-in-time universe")]
    OutsideUniverse,
    #[error("feature capacity is empty or exceeded")]
    FeatureCapacity,
    #[error("label capacity is empty or exceeded")]
    LabelCapacity,
    #[error("invalid feature evidence for {feature_id}")]
    InvalidFeature { feature_id: String },
    #[error("feature event time exceeds the prediction origin for {feature_id}")]
    FeatureEventAfterOrigin { feature_id: String },
    #[error("feature knowledge time exceeds the prediction origin for {feature_id}")]
    FeatureKnownAfterOrigin { feature_id: String },
    #[error("feature quality is below the row requirement for {feature_id}")]
    FeatureQualityBelowRequirement { feature_id: String },
    #[error("duplicate feature identity {feature_id}")]
    DuplicateFeature { feature_id: String },
    #[error("invalid label evidence for {label_id}")]
    InvalidLabel { label_id: String },
    #[error("label origin does not match the row for {label_id}")]
    LabelOriginMismatch { label_id: String },
    #[error("label outcome was not known by the dataset cutoff for {label_id}")]
    LabelKnownAfterCutoff { label_id: String },
    #[error("duplicate label identity {label_id}")]
    DuplicateLabel { label_id: String },
    #[error("sample is invalid")]
    InvalidSample,
    #[error("sample capacity is empty or exceeded")]
    SampleCapacity,
    #[error("samples are not strictly time ordered and unique")]
    NonMonotonicSamples,
    #[error("history is insufficient for the requested walk-forward geometry")]
    InsufficientHistory,
    #[error("fold configuration is invalid")]
    InvalidFoldConfiguration,
    #[error("fold range is invalid")]
    InvalidFold,
    #[error("fold capacity was exceeded")]
    FoldCapacity,
    #[error("fold identity could not be serialized canonically")]
    FoldSerialization,
    #[error("time arithmetic overflowed")]
    TimeOverflow,
}
