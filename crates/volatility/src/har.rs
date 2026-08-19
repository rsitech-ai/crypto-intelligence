//! Ridge-stabilized heterogeneous autoregressive realized-volatility model.

use crate::ForecastError;

const MAXIMUM_TRAINING_ROWS: usize = 65_536;
const MINIMUM_TRAINING_ROWS: usize = 5;
const NORMAL_INTERVAL_MULTIPLIER: f64 = 1.96;
const MODEL_HASH_DOMAIN: &[u8] = b"cmti:har-rv-model:v1\0";
const DIMENSION: usize = 4;
const PREDICTORS: usize = 3;

/// One immutable HAR-RV training row whose target knowledge time is explicit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HarRvObservation {
    observation_id: u64,
    origin_time_ns: i64,
    predictors_as_known_at_ns: i64,
    target_known_at_ns: i64,
    daily: f64,
    weekly: f64,
    monthly: f64,
    target: f64,
    lineage_hash: [u8; 32],
}

impl HarRvObservation {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        observation_id: u64,
        origin_time_ns: i64,
        predictors_as_known_at_ns: i64,
        target_known_at_ns: i64,
        daily: f64,
        weekly: f64,
        monthly: f64,
        target: f64,
        lineage_hash: [u8; 32],
    ) -> Result<Self, ForecastError> {
        if observation_id == 0
            || origin_time_ns <= 0
            || predictors_as_known_at_ns <= 0
            || predictors_as_known_at_ns > origin_time_ns
            || target_known_at_ns <= origin_time_ns
            || [daily, weekly, monthly, target]
                .iter()
                .any(|value| !value.is_finite() || *value < 0.0)
            || lineage_hash == [0; 32]
        {
            return Err(ForecastError::InvalidObservation);
        }
        Ok(Self {
            observation_id,
            origin_time_ns,
            predictors_as_known_at_ns,
            target_known_at_ns,
            daily: canonical_zero(daily),
            weekly: canonical_zero(weekly),
            monthly: canonical_zero(monthly),
            target: canonical_zero(target),
            lineage_hash,
        })
    }

    pub const fn observation_id(self) -> u64 {
        self.observation_id
    }

    pub const fn origin_time_ns(self) -> i64 {
        self.origin_time_ns
    }

    pub const fn predictors_as_known_at_ns(self) -> i64 {
        self.predictors_as_known_at_ns
    }

    pub const fn target_known_at_ns(self) -> i64 {
        self.target_known_at_ns
    }

    pub const fn daily(self) -> f64 {
        self.daily
    }

    pub const fn weekly(self) -> f64 {
        self.weekly
    }

    pub const fn monthly(self) -> f64 {
        self.monthly
    }

    pub const fn target(self) -> f64 {
        self.target
    }

    pub const fn lineage_hash(self) -> [u8; 32] {
        self.lineage_hash
    }

    const fn predictors(self) -> [f64; PREDICTORS] {
        [self.daily, self.weekly, self.monthly]
    }
}

/// Predictor location and scale fitted exclusively from training rows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PredictorNormalization {
    pub means: [f64; PREDICTORS],
    pub scales: [f64; PREDICTORS],
    pub count: u64,
}

/// Frozen HAR-RV model.
#[derive(Clone, Debug, PartialEq)]
pub struct HarRvModel {
    coefficients: [f64; DIMENSION],
    normalization: PredictorNormalization,
    residual_rmse: f64,
    ridge: f64,
    fit_rows: usize,
    fit_cutoff_ns: i64,
    model_id: [u8; 32],
}

impl HarRvModel {
    pub fn fit(
        observations: &[HarRvObservation],
        fit_cutoff_ns: i64,
        ridge: f64,
    ) -> Result<Self, ForecastError> {
        if observations.len() < MINIMUM_TRAINING_ROWS {
            return Err(ForecastError::InsufficientHistory);
        }
        if observations.len() > MAXIMUM_TRAINING_ROWS {
            return Err(ForecastError::CapacityExceeded);
        }
        if fit_cutoff_ns <= 0 || !ridge.is_finite() || ridge <= 0.0 {
            return Err(ForecastError::InvalidConfiguration);
        }

        let mut canonical = observations.iter().collect::<Vec<_>>();
        canonical.sort_by_key(|row| row.observation_id);
        for pair in canonical.windows(2) {
            if pair[0].observation_id == pair[1].observation_id {
                return Err(ForecastError::DuplicateObservation {
                    observation_id: pair[0].observation_id,
                });
            }
            if pair[0].origin_time_ns >= pair[1].origin_time_ns {
                return Err(ForecastError::NonMonotonicObservations);
            }
        }
        for row in &canonical {
            if row.origin_time_ns > fit_cutoff_ns {
                return Err(ForecastError::OriginAfterFitCutoff {
                    observation_id: row.observation_id,
                });
            }
            if row.target_known_at_ns > fit_cutoff_ns {
                return Err(ForecastError::OutcomeKnownAfterFitCutoff {
                    observation_id: row.observation_id,
                });
            }
        }

        let normalization = normalization(&canonical)?;
        let mut gram = [[0.0; DIMENSION]; DIMENSION];
        let mut response = [0.0; DIMENSION];
        for row in &canonical {
            let design = design_row(row.predictors(), normalization)?;
            for left in 0..DIMENSION {
                response[left] = finite(response[left] + design[left] * row.target)?;
                for right in 0..DIMENSION {
                    gram[left][right] = finite(gram[left][right] + design[left] * design[right])?;
                }
            }
        }
        for (index, diagonal) in gram.iter_mut().enumerate().skip(1) {
            diagonal[index] = finite(diagonal[index] + ridge)?;
        }
        let coefficients = solve(gram, response)?;
        let residual_sum = canonical.iter().try_fold(0.0, |sum, row| {
            let prediction = raw_prediction(coefficients, normalization, row.predictors())?;
            let residual = row.target - prediction;
            finite(sum + residual * residual)
        })?;
        let residual_rmse = finite((residual_sum / canonical.len() as f64).sqrt())?;
        let model_id = hash_model(
            &canonical,
            fit_cutoff_ns,
            ridge,
            normalization,
            coefficients,
            residual_rmse,
        );

        Ok(Self {
            coefficients,
            normalization,
            residual_rmse: canonical_zero(residual_rmse),
            ridge,
            fit_rows: canonical.len(),
            fit_cutoff_ns,
            model_id,
        })
    }

    pub const fn coefficients(&self) -> [f64; DIMENSION] {
        self.coefficients
    }

    pub const fn normalization(&self) -> PredictorNormalization {
        self.normalization
    }

    pub const fn residual_rmse(&self) -> f64 {
        self.residual_rmse
    }

    pub const fn ridge(&self) -> f64 {
        self.ridge
    }

    pub const fn fit_rows(&self) -> usize {
        self.fit_rows
    }

    pub const fn fit_cutoff_ns(&self) -> i64 {
        self.fit_cutoff_ns
    }

    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    pub fn forecast(
        &self,
        daily: f64,
        weekly: f64,
        monthly: f64,
    ) -> Result<VolatilityForecast, ForecastError> {
        let predictors = [daily, weekly, monthly];
        if predictors
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err(ForecastError::InvalidVolatility);
        }
        let raw = raw_prediction(self.coefficients, self.normalization, predictors)?;
        let volatility = canonical_zero(raw.max(0.0));
        let lower =
            canonical_zero((volatility - NORMAL_INTERVAL_MULTIPLIER * self.residual_rmse).max(0.0));
        let upper = finite(volatility + NORMAL_INTERVAL_MULTIPLIER * self.residual_rmse)?;
        Ok(VolatilityForecast {
            volatility,
            lower,
            upper: canonical_zero(upper),
            residual_rmse: self.residual_rmse,
            fit_rows: self.fit_rows as u64,
            model_id: self.model_id,
        })
    }
}

/// HAR-RV point forecast and residual interval in realized-volatility units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolatilityForecast {
    pub volatility: f64,
    pub lower: f64,
    pub upper: f64,
    pub residual_rmse: f64,
    pub fit_rows: u64,
    pub model_id: [u8; 32],
}

fn normalization(
    observations: &[&HarRvObservation],
) -> Result<PredictorNormalization, ForecastError> {
    let count = observations.len() as f64;
    let mut means = [0.0; PREDICTORS];
    for row in observations {
        for (mean, value) in means.iter_mut().zip(row.predictors()) {
            *mean = finite(*mean + value)?;
        }
    }
    for mean in &mut means {
        *mean = finite(*mean / count)?;
    }
    let mut scales = [0.0; PREDICTORS];
    for row in observations {
        for ((scale, value), mean) in scales.iter_mut().zip(row.predictors()).zip(means) {
            let deviation = value - mean;
            *scale = finite(*scale + deviation * deviation)?;
        }
    }
    for scale in &mut scales {
        *scale = finite((*scale / count).sqrt())?;
        if *scale <= f64::EPSILON {
            *scale = 1.0;
        }
    }
    Ok(PredictorNormalization {
        means,
        scales,
        count: observations.len() as u64,
    })
}

fn design_row(
    predictors: [f64; PREDICTORS],
    normalization: PredictorNormalization,
) -> Result<[f64; DIMENSION], ForecastError> {
    let mut row = [1.0; DIMENSION];
    for index in 0..PREDICTORS {
        row[index + 1] =
            finite((predictors[index] - normalization.means[index]) / normalization.scales[index])?;
    }
    Ok(row)
}

fn raw_prediction(
    coefficients: [f64; DIMENSION],
    normalization: PredictorNormalization,
    predictors: [f64; PREDICTORS],
) -> Result<f64, ForecastError> {
    design_row(predictors, normalization)?
        .into_iter()
        .zip(coefficients)
        .try_fold(0.0, |sum, (value, coefficient)| {
            finite(sum + value * coefficient)
        })
}

fn solve(
    mut matrix: [[f64; DIMENSION]; DIMENSION],
    mut response: [f64; DIMENSION],
) -> Result<[f64; DIMENSION], ForecastError> {
    for pivot in 0..DIMENSION {
        let pivot_row = (pivot..DIMENSION)
            .max_by(|left, right| {
                matrix[*left][pivot]
                    .abs()
                    .total_cmp(&matrix[*right][pivot].abs())
            })
            .ok_or(ForecastError::NumericalFailure)?;
        if matrix[pivot_row][pivot].abs() <= f64::EPSILON {
            return Err(ForecastError::NumericalFailure);
        }
        if pivot_row != pivot {
            matrix.swap(pivot, pivot_row);
            response.swap(pivot, pivot_row);
        }
        let divisor = matrix[pivot][pivot];
        for value in &mut matrix[pivot][pivot..] {
            *value = finite(*value / divisor)?;
        }
        response[pivot] = finite(response[pivot] / divisor)?;
        let normalized_pivot = matrix[pivot];
        for row in 0..DIMENSION {
            if row == pivot {
                continue;
            }
            let factor = matrix[row][pivot];
            for (value, pivot_value) in matrix[row][pivot..]
                .iter_mut()
                .zip(&normalized_pivot[pivot..])
            {
                *value = finite(*value - factor * pivot_value)?;
            }
            response[row] = finite(response[row] - factor * response[pivot])?;
        }
    }
    Ok(response.map(canonical_zero))
}

fn hash_model(
    observations: &[&HarRvObservation],
    fit_cutoff_ns: i64,
    ridge: f64,
    normalization: PredictorNormalization,
    coefficients: [f64; DIMENSION],
    residual_rmse: f64,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(MODEL_HASH_DOMAIN);
    hasher.update(&fit_cutoff_ns.to_le_bytes());
    hasher.update(&ridge.to_bits().to_le_bytes());
    hasher.update(&(observations.len() as u64).to_le_bytes());
    for value in normalization
        .means
        .into_iter()
        .chain(normalization.scales)
        .chain(coefficients)
        .chain([residual_rmse])
    {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    for row in observations {
        hasher.update(&row.observation_id.to_le_bytes());
        hasher.update(&row.origin_time_ns.to_le_bytes());
        hasher.update(&row.predictors_as_known_at_ns.to_le_bytes());
        hasher.update(&row.target_known_at_ns.to_le_bytes());
        for value in [row.daily, row.weekly, row.monthly, row.target] {
            hasher.update(&value.to_bits().to_le_bytes());
        }
        hasher.update(&row.lineage_hash);
    }
    *hasher.finalize().as_bytes()
}

fn finite(value: f64) -> Result<f64, ForecastError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ForecastError::NumericalFailure)
    }
}

const fn canonical_zero(value: f64) -> f64 {
    if value == 0.0 { 0.0 } else { value }
}
