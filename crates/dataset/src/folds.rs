//! Deterministic purged nested walk-forward folds.

use serde::Serialize;

use crate::DatasetError;

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const DAY_SECONDS: u64 = 86_400;
const MAXIMUM_SAMPLES: usize = 10_000_000;
const MAXIMUM_FOLDS: usize = 10_000;
const FOLD_HASH_DOMAIN: &[u8] = b"cmti:walk-forward-fold:v1\0";

/// One time-ordered model sample with its complete outcome and knowledge boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sample {
    id: u64,
    origin_time_ns: i64,
    outcome_end_ns: i64,
    as_known_at_ns: i64,
}

impl Sample {
    pub fn try_new(
        id: u64,
        origin_time_ns: i64,
        outcome_end_ns: i64,
        as_known_at_ns: i64,
    ) -> Result<Self, DatasetError> {
        if id == 0
            || origin_time_ns <= 0
            || outcome_end_ns < origin_time_ns
            || as_known_at_ns < outcome_end_ns
        {
            return Err(DatasetError::InvalidSample);
        }
        Ok(Self {
            id,
            origin_time_ns,
            outcome_end_ns,
            as_known_at_ns,
        })
    }

    pub fn daily_fixture(count: usize) -> Result<Vec<Self>, DatasetError> {
        if count == 0 || count > MAXIMUM_SAMPLES {
            return Err(DatasetError::SampleCapacity);
        }
        let day_ns = seconds_to_ns(DAY_SECONDS)?;
        (0..count)
            .map(|index| {
                let id = u64::try_from(index)
                    .ok()
                    .and_then(|value| value.checked_add(1))
                    .ok_or(DatasetError::TimeOverflow)?;
                let origin_time_ns = i64::try_from(id)
                    .ok()
                    .and_then(|value| value.checked_mul(day_ns))
                    .ok_or(DatasetError::TimeOverflow)?;
                let outcome_end_ns = origin_time_ns
                    .checked_add(day_ns)
                    .ok_or(DatasetError::TimeOverflow)?;
                Self::try_new(id, origin_time_ns, outcome_end_ns, outcome_end_ns)
            })
            .collect()
    }

    pub const fn id(self) -> u64 {
        self.id
    }

    pub const fn origin_time_ns(self) -> i64 {
        self.origin_time_ns
    }

    pub const fn outcome_end_ns(self) -> i64 {
        self.outcome_end_ns
    }

    pub const fn as_known_at_ns(self) -> i64 {
        self.as_known_at_ns
    }

    pub fn with_outcome_end_ns(self, outcome_end_ns: i64) -> Result<Self, DatasetError> {
        Self::try_new(
            self.id,
            self.origin_time_ns,
            outcome_end_ns,
            self.as_known_at_ns.max(outcome_end_ns),
        )
    }
}

/// Half-open time range for one immutable fold role.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TimeRange {
    start_ns: i64,
    end_ns: i64,
}

impl TimeRange {
    fn try_new(start_ns: i64, end_ns: i64) -> Result<Self, DatasetError> {
        if start_ns <= 0 || start_ns >= end_ns {
            return Err(DatasetError::InvalidFold);
        }
        Ok(Self { start_ns, end_ns })
    }

    pub const fn start_ns(self) -> i64 {
        self.start_ns
    }

    pub const fn end_ns(self) -> i64 {
        self.end_ns
    }
}

/// Inner time fold used only for feature and hyperparameter selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InnerFold {
    training: TimeRange,
    validation: TimeRange,
    purge_embargo_ns: i64,
    fold_hash: [u8; 32],
}

impl InnerFold {
    pub const fn training(&self) -> TimeRange {
        self.training
    }

    pub const fn validation(&self) -> TimeRange {
        self.validation
    }

    pub const fn purge_embargo_ns(&self) -> i64 {
        self.purge_embargo_ns
    }

    pub const fn fold_hash(&self) -> [u8; 32] {
        self.fold_hash
    }

    pub fn training_samples<'a>(
        &self,
        samples: &'a [Sample],
    ) -> Result<Vec<&'a Sample>, DatasetError> {
        select_samples(samples, self.training)
    }

    pub fn validation_samples<'a>(
        &self,
        samples: &'a [Sample],
    ) -> Result<Vec<&'a Sample>, DatasetError> {
        select_samples(samples, self.validation)
    }
}

/// One outer fold with explicit fit, calibration, and untouched test roles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OuterFold {
    training: TimeRange,
    inner_folds: Vec<InnerFold>,
    calibration: TimeRange,
    test: TimeRange,
    purge_embargo_ns: i64,
    fold_hash: [u8; 32],
}

impl OuterFold {
    pub const fn training(&self) -> TimeRange {
        self.training
    }

    pub fn inner_folds(&self) -> &[InnerFold] {
        &self.inner_folds
    }

    pub const fn calibration(&self) -> TimeRange {
        self.calibration
    }

    pub const fn test(&self) -> TimeRange {
        self.test
    }

    pub const fn purge_embargo_ns(&self) -> i64 {
        self.purge_embargo_ns
    }

    pub const fn fold_hash(&self) -> [u8; 32] {
        self.fold_hash
    }

    pub fn training_samples<'a>(
        &self,
        samples: &'a [Sample],
    ) -> Result<Vec<&'a Sample>, DatasetError> {
        select_samples(samples, self.training)
    }

    pub fn calibration_samples<'a>(
        &self,
        samples: &'a [Sample],
    ) -> Result<Vec<&'a Sample>, DatasetError> {
        select_samples(samples, self.calibration)
    }

    pub fn test_samples<'a>(&self, samples: &'a [Sample]) -> Result<Vec<&'a Sample>, DatasetError> {
        select_samples(samples, self.test)
    }
}

/// Deterministic nested walk-forward geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FoldBuilder {
    purge_embargo_ns: i64,
    initial_training_ns: i64,
    minimum_inner_training_ns: i64,
    inner_validation_ns: i64,
    calibration_ns: i64,
    test_ns: i64,
    step_ns: i64,
}

/// Explicit durations for every walk-forward role and origin advance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WalkForwardSchedule {
    pub initial_training_seconds: u64,
    pub minimum_inner_training_seconds: u64,
    pub inner_validation_seconds: u64,
    pub calibration_seconds: u64,
    pub test_seconds: u64,
    pub step_seconds: u64,
}

impl WalkForwardSchedule {
    /// Reference schedule used by the phase-two production evaluation contract.
    pub const fn reference() -> Self {
        Self {
            initial_training_seconds: 180 * DAY_SECONDS,
            minimum_inner_training_seconds: 60 * DAY_SECONDS,
            inner_validation_seconds: 30 * DAY_SECONDS,
            calibration_seconds: 30 * DAY_SECONDS,
            test_seconds: 30 * DAY_SECONDS,
            step_seconds: 30 * DAY_SECONDS,
        }
    }
}

impl FoldBuilder {
    pub fn try_new(
        maximum_outcome_horizon_seconds: u64,
        maximum_publication_lag_seconds: u64,
    ) -> Result<Self, DatasetError> {
        Self::try_with_schedule(
            maximum_outcome_horizon_seconds,
            maximum_publication_lag_seconds,
            WalkForwardSchedule::reference(),
        )
    }

    /// Constructs fold geometry from an explicit, validated time schedule.
    pub fn try_with_schedule(
        maximum_outcome_horizon_seconds: u64,
        maximum_publication_lag_seconds: u64,
        schedule: WalkForwardSchedule,
    ) -> Result<Self, DatasetError> {
        if maximum_outcome_horizon_seconds == 0 {
            return Err(DatasetError::InvalidFoldConfiguration);
        }
        if [
            schedule.initial_training_seconds,
            schedule.minimum_inner_training_seconds,
            schedule.inner_validation_seconds,
            schedule.calibration_seconds,
            schedule.test_seconds,
            schedule.step_seconds,
        ]
        .contains(&0)
            || schedule.minimum_inner_training_seconds >= schedule.initial_training_seconds
        {
            return Err(DatasetError::InvalidFoldConfiguration);
        }
        let purge_embargo_seconds = maximum_outcome_horizon_seconds
            .checked_add(maximum_publication_lag_seconds)
            .ok_or(DatasetError::TimeOverflow)?;
        let inner_history_seconds = schedule
            .minimum_inner_training_seconds
            .checked_add(purge_embargo_seconds)
            .and_then(|value| value.checked_add(schedule.inner_validation_seconds))
            .ok_or(DatasetError::TimeOverflow)?;
        if inner_history_seconds > schedule.initial_training_seconds {
            return Err(DatasetError::InvalidFoldConfiguration);
        }
        Ok(Self {
            purge_embargo_ns: seconds_to_ns(purge_embargo_seconds)?,
            initial_training_ns: seconds_to_ns(schedule.initial_training_seconds)?,
            minimum_inner_training_ns: seconds_to_ns(schedule.minimum_inner_training_seconds)?,
            inner_validation_ns: seconds_to_ns(schedule.inner_validation_seconds)?,
            calibration_ns: seconds_to_ns(schedule.calibration_seconds)?,
            test_ns: seconds_to_ns(schedule.test_seconds)?,
            step_ns: seconds_to_ns(schedule.step_seconds)?,
        })
    }

    pub fn outer_folds(&self, samples: &[Sample]) -> Result<Vec<OuterFold>, DatasetError> {
        validate_samples(samples)?;
        let first_origin = samples[0].origin_time_ns;
        let coverage_end = samples
            .last()
            .ok_or(DatasetError::InsufficientHistory)?
            .origin_time_ns
            .checked_add(1)
            .ok_or(DatasetError::TimeOverflow)?;
        let mut training_start = first_origin;
        let mut folds = Vec::new();
        loop {
            let training_end = checked_add(training_start, self.initial_training_ns)?;
            let calibration_start = checked_add(training_end, self.purge_embargo_ns)?;
            let calibration_end = checked_add(calibration_start, self.calibration_ns)?;
            let test_start = checked_add(calibration_end, self.purge_embargo_ns)?;
            let test_end = checked_add(test_start, self.test_ns)?;
            if test_end > coverage_end {
                break;
            }
            let training = TimeRange::try_new(training_start, training_end)?;
            let calibration = TimeRange::try_new(calibration_start, calibration_end)?;
            let test = TimeRange::try_new(test_start, test_end)?;
            let inner_folds = self.build_inner_folds(training)?;
            if inner_folds.is_empty() {
                return Err(DatasetError::InsufficientHistory);
            }
            require_eligible_samples(samples, training, &inner_folds, calibration, test)?;
            let fold_hash = hash_outer_fold(
                training,
                &inner_folds,
                calibration,
                test,
                self.purge_embargo_ns,
            )?;
            if folds.len() == MAXIMUM_FOLDS {
                return Err(DatasetError::FoldCapacity);
            }
            folds.push(OuterFold {
                training,
                inner_folds,
                calibration,
                test,
                purge_embargo_ns: self.purge_embargo_ns,
                fold_hash,
            });
            training_start = checked_add(training_start, self.step_ns)?;
        }
        if folds.is_empty() {
            Err(DatasetError::InsufficientHistory)
        } else {
            Ok(folds)
        }
    }

    fn build_inner_folds(&self, outer_training: TimeRange) -> Result<Vec<InnerFold>, DatasetError> {
        let mut validation_start = checked_add(
            checked_add(outer_training.start_ns, self.minimum_inner_training_ns)?,
            self.purge_embargo_ns,
        )?;
        let mut folds = Vec::new();
        loop {
            let validation_end = checked_add(validation_start, self.inner_validation_ns)?;
            if validation_end > outer_training.end_ns {
                break;
            }
            let training_end = validation_start
                .checked_sub(self.purge_embargo_ns)
                .ok_or(DatasetError::TimeOverflow)?;
            let training = TimeRange::try_new(outer_training.start_ns, training_end)?;
            let validation = TimeRange::try_new(validation_start, validation_end)?;
            let fold_hash = hash_inner_fold(training, validation, self.purge_embargo_ns)?;
            folds.push(InnerFold {
                training,
                validation,
                purge_embargo_ns: self.purge_embargo_ns,
                fold_hash,
            });
            validation_start = checked_add(validation_start, self.step_ns)?;
        }
        Ok(folds)
    }
}

fn validate_samples(samples: &[Sample]) -> Result<(), DatasetError> {
    if samples.is_empty() {
        return Err(DatasetError::InsufficientHistory);
    }
    if samples.len() > MAXIMUM_SAMPLES {
        return Err(DatasetError::SampleCapacity);
    }
    if samples
        .windows(2)
        .any(|pair| pair[0].origin_time_ns >= pair[1].origin_time_ns || pair[0].id >= pair[1].id)
    {
        return Err(DatasetError::NonMonotonicSamples);
    }
    Ok(())
}

fn select_samples(samples: &[Sample], range: TimeRange) -> Result<Vec<&Sample>, DatasetError> {
    validate_samples(samples)?;
    Ok(samples_in_range(samples, range)
        .iter()
        .filter(|sample| {
            sample.outcome_end_ns <= range.end_ns && sample.as_known_at_ns <= range.end_ns
        })
        .collect())
}

fn require_eligible_samples(
    samples: &[Sample],
    training: TimeRange,
    inner_folds: &[InnerFold],
    calibration: TimeRange,
    test: TimeRange,
) -> Result<(), DatasetError> {
    if !has_eligible_sample(samples, training)
        || !has_eligible_sample(samples, calibration)
        || !has_eligible_sample(samples, test)
        || inner_folds.iter().any(|inner| {
            !has_eligible_sample(samples, inner.training)
                || !has_eligible_sample(samples, inner.validation)
        })
    {
        return Err(DatasetError::InsufficientHistory);
    }
    Ok(())
}

fn has_eligible_sample(samples: &[Sample], range: TimeRange) -> bool {
    samples_in_range(samples, range).iter().any(|sample| {
        sample.outcome_end_ns <= range.end_ns && sample.as_known_at_ns <= range.end_ns
    })
}

fn samples_in_range(samples: &[Sample], range: TimeRange) -> &[Sample] {
    let start = samples.partition_point(|sample| sample.origin_time_ns < range.start_ns);
    let end = samples.partition_point(|sample| sample.origin_time_ns < range.end_ns);
    &samples[start..end]
}

fn seconds_to_ns(seconds: u64) -> Result<i64, DatasetError> {
    seconds
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(DatasetError::TimeOverflow)
}

fn checked_add(left: i64, right: i64) -> Result<i64, DatasetError> {
    left.checked_add(right).ok_or(DatasetError::TimeOverflow)
}

#[derive(Serialize)]
struct InnerFoldHashWire {
    schema_version: u32,
    training: TimeRange,
    validation: TimeRange,
    purge_embargo_ns: i64,
}

#[derive(Serialize)]
struct OuterFoldHashWire<'a> {
    schema_version: u32,
    training: TimeRange,
    inner_fold_hashes: Vec<[u8; 32]>,
    calibration: TimeRange,
    test: TimeRange,
    purge_embargo_ns: i64,
    role_order: &'a [&'a str],
}

fn hash_inner_fold(
    training: TimeRange,
    validation: TimeRange,
    purge_embargo_ns: i64,
) -> Result<[u8; 32], DatasetError> {
    hash_fold(&InnerFoldHashWire {
        schema_version: 1,
        training,
        validation,
        purge_embargo_ns,
    })
}

fn hash_outer_fold(
    training: TimeRange,
    inner_folds: &[InnerFold],
    calibration: TimeRange,
    test: TimeRange,
    purge_embargo_ns: i64,
) -> Result<[u8; 32], DatasetError> {
    hash_fold(&OuterFoldHashWire {
        schema_version: 1,
        training,
        inner_fold_hashes: inner_folds.iter().map(InnerFold::fold_hash).collect(),
        calibration,
        test,
        purge_embargo_ns,
        role_order: &["selection", "outer_fit", "calibration", "untouched_test"],
    })
}

fn hash_fold(value: &impl Serialize) -> Result<[u8; 32], DatasetError> {
    let bytes = serde_json::to_vec(value).map_err(|_| DatasetError::FoldSerialization)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(FOLD_HASH_DOMAIN);
    hasher.update(&bytes);
    Ok(*hasher.finalize().as_bytes())
}
