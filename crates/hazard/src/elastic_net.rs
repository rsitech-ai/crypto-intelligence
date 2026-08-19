//! Deterministic elastic-net multinomial hazard fitting.

use crate::incidence::CauseHorizonProbability;
use crate::{
    BucketProbability, BucketSpec, CumulativeIncidence, FeatureSchema, HazardError,
    HazardTrainingSet, InputQuality, augment_design, cumulative_incidence_for_spec,
    softmax_with_survival,
};
use crate::{BucketTarget, DesignRow};

const MAXIMUM_ITERATIONS: u32 = 100_000;
const MAXIMUM_BACKTRACKING: u32 = 128;
const MAXIMUM_PATH_LENGTH: usize = 32;
const MAXIMUM_WORK_LIMIT: u64 = 100_000_000_000;
const MAXIMUM_PATH_WORK_LIMIT: u64 = 500_000_000;
const MAXIMUM_MINIMUM_BUCKET_SUPPORT: u32 = 1_000_000;
const NORMALIZATION_FLOOR: f64 = 1.0e-12;
const MODEL_DOMAIN: &[u8] = b"cmti:elastic-net-hazard-model:v2\0";
const PREDICTION_DOMAIN: &[u8] = b"cmti:elastic-net-hazard-prediction:v2\0";
const CANDIDATE_DOMAIN: &[u8] = b"cmti:elastic-net-hazard-candidate:v2\0";

/// Untrusted deterministic optimizer configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct HazardConfigInput {
    pub lambda: f64,
    pub alpha: f64,
    pub max_iterations: u32,
    pub tolerance: f64,
    pub initial_step: f64,
    pub minimum_step: f64,
    pub max_backtracking: u32,
    pub work_limit: u64,
    pub minimum_at_risk_rows_per_bucket: u32,
}

/// Validated elastic-net and optimizer controls.
#[derive(Clone, Debug, PartialEq)]
pub struct HazardConfig {
    lambda: f64,
    alpha: f64,
    max_iterations: u32,
    tolerance: f64,
    initial_step: f64,
    minimum_step: f64,
    max_backtracking: u32,
    work_limit: u64,
    minimum_at_risk_rows_per_bucket: u32,
}

impl HazardConfig {
    pub fn try_new(input: HazardConfigInput) -> Result<Self, HazardError> {
        if !input.lambda.is_finite()
            || input.lambda < 0.0
            || !input.alpha.is_finite()
            || !(0.0..=1.0).contains(&input.alpha)
            || input.max_iterations == 0
            || input.max_iterations > MAXIMUM_ITERATIONS
            || !input.tolerance.is_finite()
            || input.tolerance <= 0.0
            || input.tolerance > 0.1
            || !input.initial_step.is_finite()
            || input.initial_step <= 0.0
            || !input.minimum_step.is_finite()
            || input.minimum_step <= 0.0
            || input.minimum_step > input.initial_step
            || input.max_backtracking == 0
            || input.max_backtracking > MAXIMUM_BACKTRACKING
            || input.work_limit == 0
            || input.work_limit > MAXIMUM_WORK_LIMIT
            || input.minimum_at_risk_rows_per_bucket == 0
            || input.minimum_at_risk_rows_per_bucket > MAXIMUM_MINIMUM_BUCKET_SUPPORT
            || !(input.lambda * input.alpha).is_finite()
            || !(input.lambda * (1.0 - input.alpha)).is_finite()
            || !(input.initial_step * input.lambda * input.alpha).is_finite()
        {
            return Err(HazardError::InvalidConfig);
        }
        Ok(Self {
            lambda: input.lambda,
            alpha: input.alpha,
            max_iterations: input.max_iterations,
            tolerance: input.tolerance,
            initial_step: input.initial_step,
            minimum_step: input.minimum_step,
            max_backtracking: input.max_backtracking,
            work_limit: input.work_limit,
            minimum_at_risk_rows_per_bucket: input.minimum_at_risk_rows_per_bucket,
        })
    }

    #[must_use]
    pub const fn lambda(&self) -> f64 {
        self.lambda
    }

    #[must_use]
    pub const fn alpha(&self) -> f64 {
        self.alpha
    }

    #[must_use]
    pub const fn max_iterations(&self) -> u32 {
        self.max_iterations
    }

    #[must_use]
    pub const fn tolerance(&self) -> f64 {
        self.tolerance
    }

    #[must_use]
    pub const fn initial_step(&self) -> f64 {
        self.initial_step
    }

    #[must_use]
    pub const fn minimum_step(&self) -> f64 {
        self.minimum_step
    }

    #[must_use]
    pub const fn max_backtracking(&self) -> u32 {
        self.max_backtracking
    }

    #[must_use]
    pub const fn work_limit(&self) -> u64 {
        self.work_limit
    }

    #[must_use]
    pub const fn minimum_at_risk_rows_per_bucket(&self) -> u32 {
        self.minimum_at_risk_rows_per_bucket
    }
}

impl From<HazardConfig> for HazardConfigInput {
    fn from(config: HazardConfig) -> Self {
        Self {
            lambda: config.lambda,
            alpha: config.alpha,
            max_iterations: config.max_iterations,
            tolerance: config.tolerance,
            initial_step: config.initial_step,
            minimum_step: config.minimum_step,
            max_backtracking: config.max_backtracking,
            work_limit: config.work_limit,
            minimum_at_risk_rows_per_bucket: config.minimum_at_risk_rows_per_bucket,
        }
    }
}

/// Training-only feature normalization.
#[derive(Clone, Debug, PartialEq)]
pub struct NormalizationStats {
    means: Vec<f64>,
    scales: Vec<f64>,
    constant_features: Vec<bool>,
}

impl NormalizationStats {
    #[must_use]
    pub fn means(&self) -> &[f64] {
        &self.means
    }

    #[must_use]
    pub fn scales(&self) -> &[f64] {
        &self.scales
    }

    #[must_use]
    pub fn constant_features(&self) -> &[bool] {
        &self.constant_features
    }
}

/// Honest deterministic optimization evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct FitDiagnostics {
    objective: f64,
    proximal_gradient_norm: f64,
    iterations: u32,
    condition_estimate: f64,
    converged: bool,
    backtracking_steps: u64,
}

impl FitDiagnostics {
    #[must_use]
    pub const fn objective(&self) -> f64 {
        self.objective
    }

    #[must_use]
    pub const fn proximal_gradient_norm(&self) -> f64 {
        self.proximal_gradient_norm
    }

    #[must_use]
    pub const fn iterations(&self) -> u32 {
        self.iterations
    }

    #[must_use]
    pub const fn condition_estimate(&self) -> f64 {
        self.condition_estimate
    }

    #[must_use]
    pub const fn converged(&self) -> bool {
        self.converged
    }

    #[must_use]
    pub const fn backtracking_steps(&self) -> u64 {
        self.backtracking_steps
    }
}

/// Point-in-time inference input whose feature order is explicit.
#[derive(Clone, Debug, PartialEq)]
pub struct HazardPredictionInput {
    pub feature_schema: FeatureSchema,
    pub origin_time_ns: i64,
    pub as_known_at_ns: i64,
    pub features: Vec<f64>,
    pub quality: InputQuality,
    pub feature_lineage: [u8; 32],
}

/// Model-bound conditional hazards and cumulative incidence.
#[derive(Clone, Debug, PartialEq)]
pub struct HazardPrediction {
    buckets: Vec<BucketProbability>,
    cumulative_incidence: CumulativeIncidence,
    cause_ids: Vec<String>,
    bucket_spec: BucketSpec,
    model_id: [u8; 32],
    evidence_id: [u8; 32],
}

/// Self-describing model forecast at one exact supported horizon.
#[derive(Clone, Debug, PartialEq)]
pub struct HazardHorizonForecast {
    horizon_seconds: u64,
    bucket_spec: BucketSpec,
    causes: Vec<CauseHorizonProbability>,
    survival: f64,
    model_id: [u8; 32],
    evidence_id: [u8; 32],
}

impl HazardHorizonForecast {
    #[must_use]
    pub const fn horizon_seconds(&self) -> u64 {
        self.horizon_seconds
    }

    #[must_use]
    pub const fn bucket_spec(&self) -> BucketSpec {
        self.bucket_spec
    }

    #[must_use]
    pub fn causes(&self) -> &[CauseHorizonProbability] {
        &self.causes
    }

    #[must_use]
    pub const fn survival(&self) -> f64 {
        self.survival
    }

    #[must_use]
    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    #[must_use]
    pub const fn evidence_id(&self) -> [u8; 32] {
        self.evidence_id
    }
}

impl HazardPrediction {
    #[must_use]
    pub fn buckets(&self) -> &[BucketProbability] {
        &self.buckets
    }

    #[must_use]
    pub const fn cumulative_incidence(&self) -> &CumulativeIncidence {
        &self.cumulative_incidence
    }

    #[must_use]
    pub fn cause_ids(&self) -> &[String] {
        &self.cause_ids
    }

    #[must_use]
    pub const fn bucket_spec(&self) -> BucketSpec {
        self.bucket_spec
    }

    #[must_use]
    pub const fn bucket_edges_seconds(&self) -> &'static [u64] {
        self.bucket_spec.edges_seconds()
    }

    #[must_use]
    pub const fn forecast_horizons_seconds(&self) -> &'static [u64] {
        self.bucket_spec.forecast_horizons_seconds()
    }

    pub fn at_horizon(&self, horizon_seconds: u64) -> Result<HazardHorizonForecast, HazardError> {
        let incidence = self.cumulative_incidence.at_horizon(horizon_seconds)?;
        Ok(HazardHorizonForecast {
            horizon_seconds: incidence.horizon_seconds(),
            bucket_spec: incidence.bucket_spec(),
            causes: incidence.causes().to_vec(),
            survival: incidence.survival(),
            model_id: self.model_id,
            evidence_id: self.evidence_id,
        })
    }

    #[must_use]
    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    #[must_use]
    pub const fn evidence_id(&self) -> [u8; 32] {
        self.evidence_id
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Parameters {
    intercepts: Vec<f64>,
    coefficients: Vec<f64>,
}

impl Parameters {
    fn zeros(bucket_count: usize, cause_count: usize, feature_count: usize) -> Self {
        Self {
            intercepts: vec![0.0; bucket_count * cause_count],
            coefficients: vec![0.0; cause_count * feature_count],
        }
    }

    fn squared_norm(&self) -> f64 {
        self.intercepts
            .iter()
            .chain(&self.coefficients)
            .map(|value| value * value)
            .sum()
    }
}

/// Fitted Task 14 multinomial hazard baseline.
#[derive(Clone, Debug, PartialEq)]
pub struct CompetingRiskHazard {
    feature_schema: FeatureSchema,
    bucket_spec: crate::BucketSpec,
    cause_ids: Vec<String>,
    config: HazardConfig,
    normalization: NormalizationStats,
    intercepts: Vec<f64>,
    coefficients: Vec<f64>,
    diagnostics: FitDiagnostics,
    training_evidence_id: [u8; 32],
    model_id: [u8; 32],
}

impl CompetingRiskHazard {
    pub fn fit(data: &HazardTrainingSet, config: HazardConfig) -> Result<Self, HazardError> {
        let rows = augment_design(data)?;
        validate_bucket_support(
            &rows,
            data.bucket_spec().len(),
            config.minimum_at_risk_rows_per_bucket,
        )?;
        preflight_work(data, &rows, &config)?;
        let normalization = fit_normalization(data)?;
        let normalized = normalize_training(data, &normalization)?;
        let condition_estimate = condition_estimate(
            &normalized,
            &rows,
            data.bucket_spec().len(),
            data.feature_schema().len(),
        )?;
        let cause_count = data.cause_ids().len();
        let feature_count = data.feature_schema().len();
        let mut parameters =
            Parameters::zeros(data.bucket_spec().len(), cause_count, feature_count);
        let mut work_budget = WorkBudget::new(config.work_limit);
        let (mut smooth, mut gradient) = smooth_loss_gradient(
            &parameters,
            &normalized,
            &rows,
            cause_count,
            feature_count,
            &config,
            &mut work_budget,
        )?;
        let mut objective = composite_objective(smooth, &parameters, &config)?;
        let mut proximal_gradient_norm = f64::INFINITY;
        let mut iterations = 0_u32;
        let mut converged = false;
        let mut total_backtracking = 0_u64;
        let mut next_step = config.initial_step;

        for iteration in 1..=config.max_iterations {
            let mut step = next_step;
            let mut accepted = None;
            for _ in 0..config.max_backtracking {
                let candidate = proximal_step(&parameters, &gradient, step, &config)?;
                let delta = parameter_delta(&candidate, &parameters);
                let candidate_smooth = smooth_loss_only(
                    &candidate,
                    &normalized,
                    &rows,
                    cause_count,
                    feature_count,
                    &config,
                    &mut work_budget,
                )?;
                let majorizer = smooth
                    + gradient_dot_delta(&gradient, &delta)
                    + delta.squared_norm() / (2.0 * step);
                if candidate_smooth <= majorizer + 1.0e-12 {
                    accepted = Some((candidate, candidate_smooth, delta));
                    break;
                }
                total_backtracking = total_backtracking
                    .checked_add(1)
                    .ok_or(HazardError::WorkCapacity)?;
                step *= 0.5;
                if step < config.minimum_step {
                    break;
                }
            }
            let Some((candidate, candidate_smooth, delta)) = accepted else {
                return Err(HazardError::OptimizationFailure);
            };

            proximal_gradient_norm = delta.squared_norm().sqrt() / step;
            let candidate_objective = composite_objective(candidate_smooth, &candidate, &config)?;
            if candidate_objective > objective + 1.0e-10 {
                return Err(HazardError::ObjectiveRegression);
            }
            let improvement = (objective - candidate_objective).abs();
            parameters = candidate;
            objective = candidate_objective;
            iterations = iteration;
            let parameter_scale = parameters.squared_norm().sqrt().max(1.0);
            let objective_scale = objective.abs().max(1.0);
            if proximal_gradient_norm <= config.tolerance * parameter_scale
                && improvement <= config.tolerance * objective_scale
            {
                converged = true;
                break;
            }
            let evaluated = smooth_loss_gradient(
                &parameters,
                &normalized,
                &rows,
                cause_count,
                feature_count,
                &config,
                &mut work_budget,
            )?;
            smooth = evaluated.0;
            gradient = evaluated.1;
            next_step = (step * 1.25).min(config.initial_step);
        }

        let diagnostics = FitDiagnostics {
            objective,
            proximal_gradient_norm,
            iterations,
            condition_estimate,
            converged,
            backtracking_steps: total_backtracking,
        };
        let model_id = model_id(data, &config, &normalization, &parameters, &diagnostics);
        Ok(Self {
            feature_schema: data.feature_schema().clone(),
            bucket_spec: data.bucket_spec(),
            cause_ids: data.cause_ids().to_vec(),
            config,
            normalization,
            intercepts: parameters.intercepts,
            coefficients: parameters.coefficients,
            diagnostics,
            training_evidence_id: data.evidence_id(),
            model_id,
        })
    }

    pub fn predict(&self, input: HazardPredictionInput) -> Result<HazardPrediction, HazardError> {
        validate_prediction_input(&input, &self.feature_schema)?;
        let normalized = normalize_values(&input.features, &self.normalization)?;
        let cause_count = self.cause_ids.len();
        let feature_count = self.feature_schema.len();
        let mut buckets = Vec::with_capacity(self.bucket_spec.len());
        for bucket_index in 0..self.bucket_spec.len() {
            let mut logits = Vec::with_capacity(cause_count);
            for cause_index in 0..cause_count {
                let coefficient_offset = cause_index * feature_count;
                let mut logit = self.intercepts[bucket_index * cause_count + cause_index];
                for (feature_index, value) in normalized.iter().enumerate() {
                    logit += self.coefficients[coefficient_offset + feature_index] * value;
                }
                if !logit.is_finite() {
                    return Err(HazardError::InvalidLogits);
                }
                logits.push(logit);
            }
            buckets.push(softmax_with_survival(&logits)?);
        }
        let incidence = cumulative_incidence_for_spec(self.bucket_spec, &self.cause_ids, &buckets)?;
        let evidence_id = prediction_id(self.model_id, &input);
        Ok(HazardPrediction {
            buckets,
            cumulative_incidence: incidence,
            cause_ids: self.cause_ids.clone(),
            bucket_spec: self.bucket_spec,
            model_id: self.model_id,
            evidence_id,
        })
    }

    #[must_use]
    pub const fn diagnostics(&self) -> &FitDiagnostics {
        &self.diagnostics
    }

    #[must_use]
    pub const fn feature_schema(&self) -> &FeatureSchema {
        &self.feature_schema
    }

    #[must_use]
    pub const fn bucket_spec(&self) -> crate::BucketSpec {
        self.bucket_spec
    }

    #[must_use]
    pub fn cause_ids(&self) -> &[String] {
        &self.cause_ids
    }

    #[must_use]
    pub const fn config(&self) -> &HazardConfig {
        &self.config
    }

    #[must_use]
    pub const fn normalization(&self) -> &NormalizationStats {
        &self.normalization
    }

    #[must_use]
    pub fn coefficients(&self) -> &[f64] {
        &self.coefficients
    }

    #[must_use]
    pub fn intercepts(&self) -> &[f64] {
        &self.intercepts
    }

    #[must_use]
    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    #[must_use]
    pub const fn training_evidence_id(&self) -> [u8; 32] {
        self.training_evidence_id
    }

    #[must_use]
    pub fn coefficient_l1_norm(&self) -> f64 {
        self.coefficients.iter().map(|value| value.abs()).sum()
    }
}

/// Auditable promotion wrapper for a converged research model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateHazardArtifact {
    model_id: [u8; 32],
    training_evidence_id: [u8; 32],
    artifact_id: [u8; 32],
}

impl CandidateHazardArtifact {
    pub fn try_new(model: &CompetingRiskHazard) -> Result<Self, HazardError> {
        if !model.diagnostics.converged {
            return Err(HazardError::NonConvergedCandidate);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(CANDIDATE_DOMAIN);
        hasher.update(&model.model_id);
        hasher.update(&model.training_evidence_id);
        Ok(Self {
            model_id: model.model_id,
            training_evidence_id: model.training_evidence_id,
            artifact_id: *hasher.finalize().as_bytes(),
        })
    }

    #[must_use]
    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    #[must_use]
    pub const fn training_evidence_id(&self) -> [u8; 32] {
        self.training_evidence_id
    }

    #[must_use]
    pub const fn artifact_id(&self) -> [u8; 32] {
        self.artifact_id
    }
}

/// Fits a bounded predeclared regularization path without consulting held-out data.
pub fn fit_regularization_path(
    data: &HazardTrainingSet,
    configs: &[HazardConfig],
) -> Result<Vec<CompetingRiskHazard>, HazardError> {
    if configs.is_empty() || configs.len() > MAXIMUM_PATH_LENGTH {
        return Err(HazardError::InvalidRegularizationPath);
    }
    let rows = augment_design(data)?;
    let aggregate_work = configs.iter().try_fold(0_u64, |total, config| {
        total
            .checked_add(estimated_work(data, &rows, config)?)
            .ok_or(HazardError::WorkCapacity)
    })?;
    if aggregate_work > MAXIMUM_PATH_WORK_LIMIT {
        return Err(HazardError::WorkCapacity);
    }
    configs
        .iter()
        .cloned()
        .map(|config| CompetingRiskHazard::fit(data, config))
        .collect()
}

#[derive(Clone, Debug)]
struct WorkBudget {
    remaining: u64,
}

impl WorkBudget {
    const fn new(limit: u64) -> Self {
        Self { remaining: limit }
    }

    fn charge(&mut self, units: u64) -> Result<(), HazardError> {
        self.remaining = self
            .remaining
            .checked_sub(units)
            .ok_or(HazardError::WorkCapacity)?;
        Ok(())
    }
}

fn preflight_work(
    data: &HazardTrainingSet,
    rows: &[DesignRow],
    config: &HazardConfig,
) -> Result<(), HazardError> {
    let units = estimated_work(data, rows, config)?;
    if units > config.work_limit {
        return Err(HazardError::WorkCapacity);
    }
    Ok(())
}

fn estimated_work(
    data: &HazardTrainingSet,
    rows: &[DesignRow],
    config: &HazardConfig,
) -> Result<u64, HazardError> {
    checked_u64(rows.len())?
        .checked_mul(checked_u64(data.cause_ids().len())?)
        .and_then(|value| value.checked_mul(checked_u64(data.feature_schema().len() + 1).ok()?))
        .and_then(|value| value.checked_mul(u64::from(config.max_iterations)))
        .ok_or(HazardError::WorkCapacity)
}

fn checked_u64(value: usize) -> Result<u64, HazardError> {
    u64::try_from(value).map_err(|_| HazardError::WorkCapacity)
}

fn fit_normalization(data: &HazardTrainingSet) -> Result<NormalizationStats, HazardError> {
    let feature_count = data.feature_schema().len();
    let mut means = vec![0.0; feature_count];
    let mut squared_deviations = vec![0.0; feature_count];
    let mut count = 0.0_f64;
    for sample in data.samples() {
        count += 1.0;
        for feature_index in 0..feature_count {
            let value = sample.features()[feature_index];
            let delta = value - means[feature_index];
            means[feature_index] += delta / count;
            let next_delta = value - means[feature_index];
            squared_deviations[feature_index] += delta * next_delta;
            if !means[feature_index].is_finite() || !squared_deviations[feature_index].is_finite() {
                return Err(HazardError::NonFiniteOptimization);
            }
        }
    }
    let mut scales = Vec::with_capacity(feature_count);
    let mut constant_features = Vec::with_capacity(feature_count);
    for squared_deviation in squared_deviations {
        let scale = (squared_deviation / count).sqrt();
        if !scale.is_finite() {
            return Err(HazardError::NonFiniteOptimization);
        }
        let constant = scale <= NORMALIZATION_FLOOR;
        scales.push(if constant { 1.0 } else { scale });
        constant_features.push(constant);
    }
    Ok(NormalizationStats {
        means,
        scales,
        constant_features,
    })
}

fn normalize_training(
    data: &HazardTrainingSet,
    normalization: &NormalizationStats,
) -> Result<Vec<Vec<f64>>, HazardError> {
    data.samples()
        .iter()
        .map(|sample| normalize_values(sample.features(), normalization))
        .collect()
}

fn normalize_values(
    values: &[f64],
    normalization: &NormalizationStats,
) -> Result<Vec<f64>, HazardError> {
    if values.len() != normalization.means.len() || values.iter().any(|value| !value.is_finite()) {
        return Err(HazardError::InvalidPrediction);
    }
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let normalized = if normalization.constant_features[index] {
                0.0
            } else {
                (*value - normalization.means[index]) / normalization.scales[index]
            };
            if normalized.is_finite() {
                Ok(normalized)
            } else {
                Err(HazardError::InvalidPrediction)
            }
        })
        .collect()
}

fn smooth_loss_gradient(
    parameters: &Parameters,
    normalized: &[Vec<f64>],
    rows: &[DesignRow],
    cause_count: usize,
    feature_count: usize,
    config: &HazardConfig,
    work_budget: &mut WorkBudget,
) -> Result<(f64, Parameters), HazardError> {
    charge_evaluation(work_budget, rows.len(), cause_count, feature_count)?;
    let mut gradient = Parameters::zeros(
        parameters.intercepts.len() / cause_count,
        cause_count,
        feature_count,
    );
    let mut loss = 0.0;
    let row_scale = 1.0 / rows.len() as f64;
    let mut logits = vec![0.0; cause_count];
    let mut probabilities = vec![0.0; cause_count];
    for row in rows {
        fill_logits(
            &mut logits,
            parameters,
            &normalized[row.sample_index()],
            row.bucket_index(),
            cause_count,
            feature_count,
        )?;
        let (log_denominator, _) = probabilities_from_logits(&logits, &mut probabilities)?;
        loss += match row.target() {
            BucketTarget::Cause(cause_index) => log_denominator - logits[cause_index],
            BucketTarget::Survival => log_denominator,
        };
        for (cause_index, probability) in probabilities.iter().copied().enumerate() {
            let target = usize::from(row.target() == BucketTarget::Cause(cause_index)) as f64;
            let delta = (probability - target) * row_scale;
            gradient.intercepts[row.bucket_index() * cause_count + cause_index] += delta;
            let coefficient_offset = cause_index * feature_count;
            for (feature_index, feature) in normalized[row.sample_index()].iter().enumerate() {
                gradient.coefficients[coefficient_offset + feature_index] += delta * feature;
            }
        }
    }
    loss *= row_scale;
    let l2_weight = config.lambda * (1.0 - config.alpha);
    for (gradient_value, coefficient) in gradient
        .coefficients
        .iter_mut()
        .zip(&parameters.coefficients)
    {
        loss += 0.5 * l2_weight * coefficient * coefficient;
        *gradient_value += l2_weight * coefficient;
    }
    if !loss.is_finite()
        || gradient
            .intercepts
            .iter()
            .chain(&gradient.coefficients)
            .any(|value| !value.is_finite())
    {
        return Err(HazardError::NonFiniteOptimization);
    }
    Ok((loss, gradient))
}

fn smooth_loss_only(
    parameters: &Parameters,
    normalized: &[Vec<f64>],
    rows: &[DesignRow],
    cause_count: usize,
    feature_count: usize,
    config: &HazardConfig,
    work_budget: &mut WorkBudget,
) -> Result<f64, HazardError> {
    charge_evaluation(work_budget, rows.len(), cause_count, feature_count)?;
    let mut loss = 0.0;
    let row_scale = 1.0 / rows.len() as f64;
    let mut logits = vec![0.0; cause_count];
    let mut probabilities = vec![0.0; cause_count];
    for row in rows {
        fill_logits(
            &mut logits,
            parameters,
            &normalized[row.sample_index()],
            row.bucket_index(),
            cause_count,
            feature_count,
        )?;
        let (log_denominator, _) = probabilities_from_logits(&logits, &mut probabilities)?;
        loss += match row.target() {
            BucketTarget::Cause(cause_index) => log_denominator - logits[cause_index],
            BucketTarget::Survival => log_denominator,
        };
    }
    loss *= row_scale;
    let l2_weight = config.lambda * (1.0 - config.alpha);
    loss += 0.5
        * l2_weight
        * parameters
            .coefficients
            .iter()
            .map(|value| value * value)
            .sum::<f64>();
    if !loss.is_finite() {
        return Err(HazardError::NonFiniteOptimization);
    }
    Ok(loss)
}

fn charge_evaluation(
    work_budget: &mut WorkBudget,
    row_count: usize,
    cause_count: usize,
    feature_count: usize,
) -> Result<(), HazardError> {
    let units = checked_u64(row_count)?
        .checked_mul(checked_u64(cause_count)?)
        .and_then(|value| value.checked_mul(checked_u64(feature_count + 1).ok()?))
        .ok_or(HazardError::WorkCapacity)?;
    work_budget.charge(units)
}

fn fill_logits(
    logits: &mut [f64],
    parameters: &Parameters,
    features: &[f64],
    bucket_index: usize,
    cause_count: usize,
    feature_count: usize,
) -> Result<(), HazardError> {
    for (cause_index, logit) in logits.iter_mut().enumerate() {
        let mut value = parameters.intercepts[bucket_index * cause_count + cause_index];
        let coefficient_offset = cause_index * feature_count;
        for (feature_index, feature) in features.iter().enumerate() {
            value += parameters.coefficients[coefficient_offset + feature_index] * feature;
        }
        if !value.is_finite() {
            return Err(HazardError::NonFiniteOptimization);
        }
        *logit = value;
    }
    Ok(())
}

fn probabilities_from_logits(
    logits: &[f64],
    probabilities: &mut [f64],
) -> Result<(f64, f64), HazardError> {
    let maximum = logits
        .iter()
        .copied()
        .fold(0.0_f64, |current, value| current.max(value));
    let survival_weight = (-maximum).exp();
    let mut denominator = survival_weight;
    for (probability, logit) in probabilities.iter_mut().zip(logits) {
        *probability = (*logit - maximum).exp();
        denominator += *probability;
    }
    if !denominator.is_finite() || denominator <= 0.0 {
        return Err(HazardError::NonFiniteOptimization);
    }
    for probability in probabilities {
        *probability /= denominator;
    }
    let log_denominator = maximum + denominator.ln();
    if !log_denominator.is_finite() {
        return Err(HazardError::NonFiniteOptimization);
    }
    Ok((log_denominator, survival_weight / denominator))
}

fn proximal_step(
    parameters: &Parameters,
    gradient: &Parameters,
    step: f64,
    config: &HazardConfig,
) -> Result<Parameters, HazardError> {
    let intercepts = parameters
        .intercepts
        .iter()
        .zip(&gradient.intercepts)
        .map(|(value, derivative)| value - step * derivative)
        .collect::<Vec<_>>();
    let threshold = step * config.lambda * config.alpha;
    let coefficients = parameters
        .coefficients
        .iter()
        .zip(&gradient.coefficients)
        .map(|(value, derivative)| soft_threshold(value - step * derivative, threshold))
        .collect::<Vec<_>>();
    if intercepts
        .iter()
        .chain(&coefficients)
        .any(|value| !value.is_finite())
    {
        return Err(HazardError::NonFiniteOptimization);
    }
    Ok(Parameters {
        intercepts,
        coefficients,
    })
}

fn soft_threshold(value: f64, threshold: f64) -> f64 {
    if value > threshold {
        value - threshold
    } else if value < -threshold {
        value + threshold
    } else {
        0.0
    }
}

fn parameter_delta(candidate: &Parameters, current: &Parameters) -> Parameters {
    Parameters {
        intercepts: candidate
            .intercepts
            .iter()
            .zip(&current.intercepts)
            .map(|(next, previous)| next - previous)
            .collect(),
        coefficients: candidate
            .coefficients
            .iter()
            .zip(&current.coefficients)
            .map(|(next, previous)| next - previous)
            .collect(),
    }
}

fn gradient_dot_delta(gradient: &Parameters, delta: &Parameters) -> f64 {
    gradient
        .intercepts
        .iter()
        .zip(&delta.intercepts)
        .chain(gradient.coefficients.iter().zip(&delta.coefficients))
        .map(|(left, right)| left * right)
        .sum()
}

fn composite_objective(
    smooth: f64,
    parameters: &Parameters,
    config: &HazardConfig,
) -> Result<f64, HazardError> {
    let l1 = config.lambda
        * config.alpha
        * parameters
            .coefficients
            .iter()
            .map(|value| value.abs())
            .sum::<f64>();
    let objective = smooth + l1;
    if objective.is_finite() {
        Ok(objective)
    } else {
        Err(HazardError::NonFiniteOptimization)
    }
}

fn condition_estimate(
    normalized: &[Vec<f64>],
    rows: &[DesignRow],
    bucket_count: usize,
    feature_count: usize,
) -> Result<f64, HazardError> {
    let mut diagonals = vec![0.0; bucket_count + feature_count];
    for row in rows {
        diagonals[row.bucket_index()] += 1.0;
        for feature_index in 0..feature_count {
            let value = normalized[row.sample_index()][feature_index];
            diagonals[bucket_count + feature_index] += value * value;
        }
    }
    let mut minimum = f64::INFINITY;
    let mut maximum = 0.0_f64;
    for value in diagonals
        .into_iter()
        .filter(|value| *value > NORMALIZATION_FLOOR)
    {
        minimum = minimum.min(value);
        maximum = maximum.max(value);
    }
    let estimate = if minimum.is_finite() {
        maximum / minimum
    } else {
        1.0
    };
    if estimate.is_finite() && estimate >= 1.0 {
        Ok(estimate)
    } else {
        Err(HazardError::NonFiniteOptimization)
    }
}

fn validate_prediction_input(
    input: &HazardPredictionInput,
    expected_schema: &FeatureSchema,
) -> Result<(), HazardError> {
    if &input.feature_schema != expected_schema
        || input.origin_time_ns <= 0
        || input.as_known_at_ns <= 0
        || input.as_known_at_ns > input.origin_time_ns
        || input.features.len() != expected_schema.len()
        || input.features.iter().any(|value| !value.is_finite())
        || input.quality != InputQuality::Available
        || input.feature_lineage == [0; 32]
    {
        return Err(HazardError::InvalidPrediction);
    }
    Ok(())
}

fn validate_bucket_support(
    rows: &[DesignRow],
    bucket_count: usize,
    minimum_at_risk_rows_per_bucket: u32,
) -> Result<(), HazardError> {
    let mut support = vec![0_u32; bucket_count];
    for row in rows {
        let count = support
            .get_mut(row.bucket_index())
            .ok_or(HazardError::InvalidTrainingSet)?;
        *count = count
            .checked_add(1)
            .ok_or(HazardError::InsufficientBucketSupport)?;
    }
    if support
        .iter()
        .any(|count| *count < minimum_at_risk_rows_per_bucket)
    {
        return Err(HazardError::InsufficientBucketSupport);
    }
    Ok(())
}

fn model_id(
    data: &HazardTrainingSet,
    config: &HazardConfig,
    normalization: &NormalizationStats,
    parameters: &Parameters,
    diagnostics: &FitDiagnostics,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(MODEL_DOMAIN);
    hasher.update(&data.evidence_id());
    hash_f64(&mut hasher, config.lambda);
    hash_f64(&mut hasher, config.alpha);
    hash_u64(&mut hasher, u64::from(config.max_iterations));
    hash_f64(&mut hasher, config.tolerance);
    hash_f64(&mut hasher, config.initial_step);
    hash_f64(&mut hasher, config.minimum_step);
    hash_u64(&mut hasher, u64::from(config.max_backtracking));
    hash_u64(&mut hasher, config.work_limit);
    hash_u64(
        &mut hasher,
        u64::from(config.minimum_at_risk_rows_per_bucket),
    );
    hash_f64_slice(&mut hasher, &normalization.means);
    hash_f64_slice(&mut hasher, &normalization.scales);
    for constant in &normalization.constant_features {
        hasher.update(&[u8::from(*constant)]);
    }
    hash_f64_slice(&mut hasher, &parameters.intercepts);
    hash_f64_slice(&mut hasher, &parameters.coefficients);
    hash_f64(&mut hasher, diagnostics.objective);
    hash_f64(&mut hasher, diagnostics.proximal_gradient_norm);
    hash_u64(&mut hasher, u64::from(diagnostics.iterations));
    hash_f64(&mut hasher, diagnostics.condition_estimate);
    hasher.update(&[u8::from(diagnostics.converged)]);
    hash_u64(&mut hasher, diagnostics.backtracking_steps);
    *hasher.finalize().as_bytes()
}

fn prediction_id(model_id: [u8; 32], input: &HazardPredictionInput) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PREDICTION_DOMAIN);
    hasher.update(&model_id);
    hash_u64(&mut hasher, u64::from(input.feature_schema.version()));
    for identifier in input.feature_schema.feature_ids() {
        hash_u64(
            &mut hasher,
            u64::try_from(identifier.len()).unwrap_or(u64::MAX),
        );
        hasher.update(identifier.as_bytes());
    }
    hasher.update(&input.origin_time_ns.to_le_bytes());
    hasher.update(&input.as_known_at_ns.to_le_bytes());
    hasher.update(&input.feature_lineage);
    hash_f64_slice(&mut hasher, &input.features);
    *hasher.finalize().as_bytes()
}

fn hash_f64_slice(hasher: &mut blake3::Hasher, values: &[f64]) {
    hash_u64(hasher, u64::try_from(values.len()).unwrap_or(u64::MAX));
    for value in values {
        hash_f64(hasher, *value);
    }
}

fn hash_f64(hasher: &mut blake3::Hasher, value: f64) {
    hasher.update(&value.to_bits().to_le_bytes());
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_le_bytes());
}
