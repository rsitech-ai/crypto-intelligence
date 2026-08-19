//! Bounded Laplace uncertainty for converged offline cusp fits.

use domain::AssetId;
use nalgebra::{DMatrix, DVector, linalg::SymmetricEigen};
use serde::Serialize;
use statrs::distribution::{ContinuousCDF, Normal};
use thiserror::Error;

use crate::{
    CoefficientSign, ControlError, ControlVector, Controls,
    fit::{EstimatorRole, FitError, FitResult, ParameterKey},
};

const MAX_POSTERIOR_SAMPLES: usize = 10_000;
const MAX_SAMPLE_VALUES: usize = 1_000_000;
const MIN_POSTERIOR_SAMPLES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum UncertaintyQuality {
    ProductionCandidate,
    Experimental,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UncertaintyMethod {
    LaplaceNormal,
    BlockedBootstrapPercentile,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LaplaceConfig {
    eigenvalue_floor: f64,
    candidate_max_regularization: f64,
    maximum_regularization: f64,
    candidate_max_condition_number: f64,
    maximum_condition_number: f64,
    active_threshold: f64,
    ambiguity_ratio: f64,
}

impl LaplaceConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        eigenvalue_floor: f64,
        candidate_max_regularization: f64,
        maximum_regularization: f64,
        candidate_max_condition_number: f64,
        maximum_condition_number: f64,
        active_threshold: f64,
        ambiguity_ratio: f64,
    ) -> Result<Self, UncertaintyError> {
        let config = Self {
            eigenvalue_floor,
            candidate_max_regularization,
            maximum_regularization,
            candidate_max_condition_number,
            maximum_condition_number,
            active_threshold,
            ambiguity_ratio,
        };
        config.validate()?;
        Ok(config)
    }

    pub const fn fixture() -> Self {
        Self {
            eigenvalue_floor: 1.0e-8,
            candidate_max_regularization: 1.0e-6,
            maximum_regularization: 1.0e-2,
            candidate_max_condition_number: 1.0e10,
            maximum_condition_number: 1.0e14,
            active_threshold: 1.0e-8,
            ambiguity_ratio: 4.0,
        }
    }

    fn validate(self) -> Result<(), UncertaintyError> {
        if !self.eigenvalue_floor.is_finite()
            || self.eigenvalue_floor <= 0.0
            || !self.candidate_max_regularization.is_finite()
            || self.candidate_max_regularization < 0.0
            || !self.maximum_regularization.is_finite()
            || self.maximum_regularization < self.candidate_max_regularization
            || !self.candidate_max_condition_number.is_finite()
            || self.candidate_max_condition_number < 1.0
            || !self.maximum_condition_number.is_finite()
            || self.maximum_condition_number < self.candidate_max_condition_number
            || !self.active_threshold.is_finite()
            || self.active_threshold <= 0.0
            || !self.ambiguity_ratio.is_finite()
            || self.ambiguity_ratio <= 1.0
        {
            return Err(UncertaintyError::InvalidConfig);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaplaceDiagnostics {
    active_parameters: usize,
    inactive_parameters: usize,
    effective_observations: usize,
    raw_minimum_eigenvalue: f64,
    raw_maximum_eigenvalue: f64,
    regularization_added: f64,
    condition_number: f64,
    quality: UncertaintyQuality,
}

impl LaplaceDiagnostics {
    pub const fn active_parameters(&self) -> usize {
        self.active_parameters
    }

    pub const fn inactive_parameters(&self) -> usize {
        self.inactive_parameters
    }

    pub const fn effective_observations(&self) -> usize {
        self.effective_observations
    }

    pub const fn raw_minimum_eigenvalue(&self) -> f64 {
        self.raw_minimum_eigenvalue
    }

    pub const fn raw_maximum_eigenvalue(&self) -> f64 {
        self.raw_maximum_eigenvalue
    }

    pub const fn regularization_added(&self) -> f64 {
        self.regularization_added
    }

    pub const fn condition_number(&self) -> f64 {
        self.condition_number
    }

    pub const fn quality(&self) -> UncertaintyQuality {
        self.quality
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaplaceApproximation {
    mode: Vec<f64>,
    parameter_keys: Vec<ParameterKey>,
    active_parameter_indices: Vec<usize>,
    inactive_parameter_indices: Vec<usize>,
    parameter_signs: Vec<CoefficientSign>,
    covariance: DMatrix<f64>,
    diagnostics: LaplaceDiagnostics,
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
    effective_observations: usize,
    evidence_digest: [u8; 32],
}

impl LaplaceApproximation {
    pub fn from_fit(fit: &FitResult, config: LaplaceConfig) -> Result<Self, UncertaintyError> {
        config.validate()?;
        if !fit.diagnostics().converged() {
            return Err(UncertaintyError::NonConvergedFit);
        }
        let mode = fit.parameter_values().to_vec();
        let parameter_keys = fit.parameter_keys().to_vec();
        let penalized = fit.penalized_parameter_mask();
        let parameter_signs = fit.parameter_signs();
        if mode.len() != parameter_keys.len()
            || mode.len() != penalized.len()
            || mode.len() != parameter_signs.len()
            || mode.is_empty()
        {
            return Err(UncertaintyError::Dimension);
        }
        let (active_parameter_indices, inactive_parameter_indices) =
            classify_active_set(&mode, &penalized, config)?;
        let information_scale = fit.effective_observations() as f64;
        if !information_scale.is_finite() || information_scale <= 0.0 {
            return Err(UncertaintyError::Dimension);
        }
        let mut full_hessian = fit.analytic_hessian()?;
        for value in full_hessian.iter_mut().flatten() {
            *value *= information_scale;
            if !value.is_finite() {
                return Err(UncertaintyError::NonFinite);
            }
        }
        let active_hessian = active_parameter_indices
            .iter()
            .map(|row| {
                active_parameter_indices
                    .iter()
                    .map(|column| full_hessian[*row][*column])
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let (covariance, mut diagnostics) = regularized_covariance(
            &active_hessian,
            inactive_parameter_indices.len(),
            fit.effective_observations(),
            config,
        )?;
        if fit.role() != EstimatorRole::ProductionCandidate
            && diagnostics.quality == UncertaintyQuality::ProductionCandidate
        {
            diagnostics.quality = UncertaintyQuality::Experimental;
        }
        let evidence_digest = laplace_evidence_digest(
            config,
            fit,
            &mode,
            &parameter_keys,
            &active_parameter_indices,
            &covariance,
            &diagnostics,
        );
        Ok(Self {
            mode,
            parameter_keys,
            active_parameter_indices,
            inactive_parameter_indices,
            parameter_signs,
            covariance,
            diagnostics,
            dataset_manifest_hash: fit.dataset_manifest_hash(),
            training_fold_hash: fit.training_fold_hash(),
            effective_observations: fit.effective_observations(),
            evidence_digest,
        })
    }

    pub fn mode(&self) -> &[f64] {
        &self.mode
    }

    pub fn active_parameter_indices(&self) -> &[usize] {
        &self.active_parameter_indices
    }

    pub fn parameter_keys(&self) -> &[ParameterKey] {
        &self.parameter_keys
    }

    pub fn inactive_parameter_indices(&self) -> &[usize] {
        &self.inactive_parameter_indices
    }

    pub const fn diagnostics(&self) -> &LaplaceDiagnostics {
        &self.diagnostics
    }

    pub const fn quality(&self) -> UncertaintyQuality {
        self.diagnostics.quality
    }

    pub const fn dataset_manifest_hash(&self) -> [u8; 32] {
        self.dataset_manifest_hash
    }

    pub const fn training_fold_hash(&self) -> [u8; 32] {
        self.training_fold_hash
    }

    pub const fn evidence_digest(&self) -> [u8; 32] {
        self.evidence_digest
    }

    pub fn covariance_is_symmetric(&self, tolerance: f64) -> bool {
        tolerance.is_finite()
            && tolerance > 0.0
            && (0..self.covariance.nrows()).all(|row| {
                (0..row).all(|column| {
                    let scale = 1.0_f64
                        .max(self.covariance[(row, column)].abs())
                        .max(self.covariance[(column, row)].abs());
                    (self.covariance[(row, column)] - self.covariance[(column, row)]).abs()
                        <= tolerance * scale
                })
            })
    }

    pub fn minimum_covariance_eigenvalue(&self) -> f64 {
        SymmetricEigen::new(self.covariance.clone())
            .eigenvalues
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
    }

    pub fn parameter_interval(
        &self,
        parameter_index: usize,
        mass: f64,
    ) -> Result<IntervalEstimate, UncertaintyError> {
        validate_interval_mass(mass)?;
        let estimate = *self
            .mode
            .get(parameter_index)
            .ok_or(UncertaintyError::Dimension)?;
        let variance = match self
            .active_parameter_indices
            .iter()
            .position(|index| *index == parameter_index)
        {
            Some(active_index) => self.covariance[(active_index, active_index)],
            None if self.inactive_parameter_indices.contains(&parameter_index) => 0.0,
            None => return Err(UncertaintyError::Dimension),
        };
        if !variance.is_finite() || variance < 0.0 {
            return Err(UncertaintyError::NonFinite);
        }
        let normal = Normal::standard();
        let quantile = normal.inverse_cdf(0.5 + mass / 2.0);
        let radius = quantile * variance.sqrt();
        let (lower, upper) = match self.parameter_signs[parameter_index] {
            CoefficientSign::Any => (estimate - radius, estimate + radius),
            CoefficientSign::NonNegative => ((estimate - radius).max(0.0), estimate + radius),
            CoefficientSign::NonPositive => (estimate - radius, (estimate + radius).min(0.0)),
        };
        IntervalEstimate::try_new(IntervalInput {
            lower,
            estimate,
            upper,
            method: UncertaintyMethod::LaplaceNormal,
            seed: None,
            effective_samples: self.effective_observations,
            failed_refits: 0,
            regularization_added: self.diagnostics.regularization_added,
        })
    }

    pub fn sample_parameters(
        &self,
        seed: u64,
        sample_count: usize,
    ) -> Result<PosteriorParameterSamples, UncertaintyError> {
        validate_sample_capacity(
            sample_count,
            self.mode.len(),
            self.active_parameter_indices.len(),
        )?;
        let cholesky = self
            .covariance
            .clone()
            .cholesky()
            .ok_or(UncertaintyError::SingularHessian)?;
        let lower = cholesky.l();
        let mut normal = DeterministicNormal::new(seed);
        let mut samples = Vec::with_capacity(sample_count);
        let maximum_attempts = sample_count
            .checked_mul(256)
            .ok_or(UncertaintyError::SampleCapacity)?;
        let mut attempts = 0_usize;
        while samples.len() < sample_count {
            attempts = attempts
                .checked_add(1)
                .ok_or(UncertaintyError::SampleCapacity)?;
            if attempts > maximum_attempts {
                return Err(UncertaintyError::ConstraintSampling);
            }
            let standard = DVector::from_iterator(
                self.active_parameter_indices.len(),
                (0..self.active_parameter_indices.len()).map(|_| normal.next()),
            );
            let offset = &lower * standard;
            let mut sample = self.mode.clone();
            for (active_index, parameter_index) in self.active_parameter_indices.iter().enumerate()
            {
                sample[*parameter_index] += offset[active_index];
            }
            if sample.iter().any(|value| !value.is_finite()) {
                return Err(UncertaintyError::NonFinite);
            }
            if sample
                .iter()
                .zip(&self.parameter_signs)
                .all(|(value, sign)| match sign {
                    CoefficientSign::Any => true,
                    CoefficientSign::NonNegative => *value >= 0.0,
                    CoefficientSign::NonPositive => *value <= 0.0,
                })
            {
                samples.push(sample);
            }
        }
        let sample_digest = digest_parameter_samples(
            b"cusp-laplace-parameter-samples-v1",
            seed,
            self.dataset_manifest_hash,
            self.training_fold_hash,
            &samples,
        );
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cusp-laplace-parameter-result-v1");
        hasher.update(&self.evidence_digest);
        hasher.update(&sample_digest);
        let digest = *hasher.finalize().as_bytes();
        Ok(PosteriorParameterSamples {
            seed,
            dataset_manifest_hash: self.dataset_manifest_hash,
            training_fold_hash: self.training_fold_hash,
            laplace_evidence_digest: self.evidence_digest,
            mode: self.mode.clone(),
            parameter_keys: self.parameter_keys.clone(),
            quality: self.quality(),
            samples,
            digest,
        })
    }

    pub fn sample_controls(
        &self,
        fit: &FitResult,
        asset: &AssetId,
        features: &ControlVector,
        seed: u64,
        sample_count: usize,
    ) -> Result<PosteriorControlSamples, UncertaintyError> {
        if fit.dataset_manifest_hash() != self.dataset_manifest_hash
            || fit.training_fold_hash() != self.training_fold_hash
            || fit.parameter_values() != self.mode
            || fit.parameter_keys() != self.parameter_keys
        {
            return Err(UncertaintyError::EvidenceMismatch);
        }
        let parameter_samples = self.sample_parameters(seed, sample_count)?;
        let samples = parameter_samples
            .samples
            .iter()
            .map(|parameters| {
                fit.control_map_for_parameters(parameters)?
                    .evaluate(asset, features)
                    .map_err(UncertaintyError::Control)
            })
            .collect::<Result<Vec<_>, UncertaintyError>>()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"cusp-laplace-control-samples-v1");
        hasher.update(&self.evidence_digest);
        hasher.update(&seed.to_le_bytes());
        hasher.update(&self.dataset_manifest_hash);
        hasher.update(&self.training_fold_hash);
        for sample in &samples {
            hasher.update(&sample.alpha.to_bits().to_le_bytes());
            hasher.update(&sample.beta.to_bits().to_le_bytes());
        }
        Ok(PosteriorControlSamples {
            seed,
            samples,
            digest: *hasher.finalize().as_bytes(),
        })
    }
}

fn classify_active_set(
    mode: &[f64],
    penalized: &[bool],
    config: LaplaceConfig,
) -> Result<(Vec<usize>, Vec<usize>), UncertaintyError> {
    if mode.len() != penalized.len() || mode.is_empty() {
        return Err(UncertaintyError::Dimension);
    }
    let mut active_parameter_indices = Vec::new();
    let mut inactive_parameter_indices = Vec::new();
    let active_limit = config.active_threshold * config.ambiguity_ratio;
    for (index, (value, is_penalized)) in mode.iter().zip(penalized).enumerate() {
        if !value.is_finite() {
            return Err(UncertaintyError::NonFinite);
        }
        if !is_penalized || value.abs() >= active_limit {
            active_parameter_indices.push(index);
        } else if *value == 0.0 {
            inactive_parameter_indices.push(index);
        } else {
            return Err(UncertaintyError::AmbiguousActiveSet);
        }
    }
    Ok((active_parameter_indices, inactive_parameter_indices))
}

fn laplace_evidence_digest(
    config: LaplaceConfig,
    fit: &FitResult,
    mode: &[f64],
    parameter_keys: &[ParameterKey],
    active_parameter_indices: &[usize],
    covariance: &DMatrix<f64>,
    diagnostics: &LaplaceDiagnostics,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cusp-laplace-approximation-v1");
    hasher.update(&fit.dataset_manifest_hash());
    hasher.update(&fit.training_fold_hash());
    for value in [
        config.eigenvalue_floor,
        config.candidate_max_regularization,
        config.maximum_regularization,
        config.candidate_max_condition_number,
        config.maximum_condition_number,
        config.active_threshold,
        config.ambiguity_ratio,
        diagnostics.raw_minimum_eigenvalue,
        diagnostics.raw_maximum_eigenvalue,
        diagnostics.regularization_added,
        diagnostics.condition_number,
    ] {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    hasher.update(&(diagnostics.active_parameters as u64).to_le_bytes());
    hasher.update(&(diagnostics.inactive_parameters as u64).to_le_bytes());
    hasher.update(&(diagnostics.effective_observations as u64).to_le_bytes());
    hasher.update(&[match diagnostics.quality {
        UncertaintyQuality::ProductionCandidate => 1,
        UncertaintyQuality::Experimental => 2,
        UncertaintyQuality::Unavailable => 3,
    }]);
    for value in mode {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    hash_parameter_keys(&mut hasher, parameter_keys);
    for index in active_parameter_indices {
        hasher.update(&(*index as u64).to_le_bytes());
    }
    for value in covariance.iter() {
        hasher.update(&value.to_bits().to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn regularized_covariance(
    hessian: &[Vec<f64>],
    inactive_parameters: usize,
    effective_observations: usize,
    config: LaplaceConfig,
) -> Result<(DMatrix<f64>, LaplaceDiagnostics), UncertaintyError> {
    let dimension = hessian.len();
    if dimension == 0
        || hessian.iter().any(|row| row.len() != dimension)
        || hessian.iter().flatten().any(|value| !value.is_finite())
    {
        return Err(UncertaintyError::Dimension);
    }
    let matrix = DMatrix::from_fn(dimension, dimension, |row, column| {
        0.5 * (hessian[row][column] + hessian[column][row])
    });
    let eigen = SymmetricEigen::new(matrix.clone());
    let minimum = eigen
        .eigenvalues
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    let maximum = eigen
        .eigenvalues
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if !minimum.is_finite() || !maximum.is_finite() {
        return Err(UncertaintyError::NonFinite);
    }
    let regularization_added = (config.eigenvalue_floor - minimum).max(0.0);
    if !regularization_added.is_finite() || regularization_added > config.maximum_regularization {
        return Err(UncertaintyError::ExcessiveRegularization);
    }
    let regularized = matrix + DMatrix::identity(dimension, dimension) * regularization_added;
    let regularized_eigen = SymmetricEigen::new(regularized.clone());
    let regularized_minimum = regularized_eigen
        .eigenvalues
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    let regularized_maximum = regularized_eigen
        .eigenvalues
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let condition_number = regularized_maximum / regularized_minimum;
    if !regularized_minimum.is_finite()
        || regularized_minimum <= 0.0
        || !condition_number.is_finite()
        || condition_number > config.maximum_condition_number
    {
        return Err(UncertaintyError::IllConditioned);
    }
    let covariance = regularized
        .cholesky()
        .ok_or(UncertaintyError::SingularHessian)?
        .inverse();
    if covariance.iter().any(|value| !value.is_finite()) {
        return Err(UncertaintyError::NonFinite);
    }
    let quality = if regularization_added <= config.candidate_max_regularization
        && condition_number <= config.candidate_max_condition_number
    {
        UncertaintyQuality::ProductionCandidate
    } else {
        UncertaintyQuality::Experimental
    };
    Ok((
        covariance,
        LaplaceDiagnostics {
            active_parameters: dimension,
            inactive_parameters,
            effective_observations,
            raw_minimum_eigenvalue: minimum,
            raw_maximum_eigenvalue: maximum,
            regularization_added,
            condition_number,
            quality,
        },
    ))
}

fn validate_sample_capacity(
    sample_count: usize,
    total_dimension: usize,
    active_dimension: usize,
) -> Result<(), UncertaintyError> {
    if !(MIN_POSTERIOR_SAMPLES..=MAX_POSTERIOR_SAMPLES).contains(&sample_count)
        || total_dimension == 0
        || active_dimension == 0
        || sample_count
            .checked_mul(total_dimension)
            .is_none_or(|work| work > MAX_SAMPLE_VALUES)
    {
        return Err(UncertaintyError::SampleCapacity);
    }
    Ok(())
}

fn validate_interval_mass(mass: f64) -> Result<(), UncertaintyError> {
    if mass.is_finite() && (0.5..0.999).contains(&mass) {
        Ok(())
    } else {
        Err(UncertaintyError::InvalidInterval)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct IntervalInput {
    pub lower: f64,
    pub estimate: f64,
    pub upper: f64,
    pub method: UncertaintyMethod,
    pub seed: Option<u64>,
    pub effective_samples: usize,
    pub failed_refits: usize,
    pub regularization_added: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IntervalEstimate {
    lower: f64,
    estimate: f64,
    upper: f64,
    method: UncertaintyMethod,
    seed: Option<u64>,
    effective_samples: usize,
    failed_refits: usize,
    regularization_added: f64,
}

impl IntervalEstimate {
    pub(crate) fn try_new(input: IntervalInput) -> Result<Self, UncertaintyError> {
        if [
            input.lower,
            input.estimate,
            input.upper,
            input.regularization_added,
        ]
        .iter()
        .any(|value| !value.is_finite())
            || input.lower > input.upper
            || input.effective_samples == 0
            || input.regularization_added < 0.0
        {
            return Err(UncertaintyError::InvalidInterval);
        }
        Ok(Self {
            lower: input.lower,
            estimate: input.estimate,
            upper: input.upper,
            method: input.method,
            seed: input.seed,
            effective_samples: input.effective_samples,
            failed_refits: input.failed_refits,
            regularization_added: input.regularization_added,
        })
    }

    pub const fn lower(&self) -> f64 {
        self.lower
    }

    pub const fn estimate(&self) -> f64 {
        self.estimate
    }

    pub const fn upper(&self) -> f64 {
        self.upper
    }

    pub const fn method(&self) -> UncertaintyMethod {
        self.method
    }

    pub const fn seed(&self) -> Option<u64> {
        self.seed
    }

    pub const fn effective_samples(&self) -> usize {
        self.effective_samples
    }

    pub const fn failed_refits(&self) -> usize {
        self.failed_refits
    }

    pub const fn regularization_added(&self) -> f64 {
        self.regularization_added
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControlIntervals {
    alpha: IntervalEstimate,
    beta: IntervalEstimate,
}

impl ControlIntervals {
    pub(crate) const fn new(alpha: IntervalEstimate, beta: IntervalEstimate) -> Self {
        Self { alpha, beta }
    }

    pub const fn alpha(&self) -> &IntervalEstimate {
        &self.alpha
    }

    pub const fn beta(&self) -> &IntervalEstimate {
        &self.beta
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PosteriorParameterSamples {
    seed: u64,
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
    laplace_evidence_digest: [u8; 32],
    mode: Vec<f64>,
    parameter_keys: Vec<ParameterKey>,
    quality: UncertaintyQuality,
    samples: Vec<Vec<f64>>,
    digest: [u8; 32],
}

impl PosteriorParameterSamples {
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    pub const fn dataset_manifest_hash(&self) -> [u8; 32] {
        self.dataset_manifest_hash
    }

    pub const fn training_fold_hash(&self) -> [u8; 32] {
        self.training_fold_hash
    }

    pub const fn laplace_evidence_digest(&self) -> [u8; 32] {
        self.laplace_evidence_digest
    }

    pub fn mode(&self) -> &[f64] {
        &self.mode
    }

    pub fn parameter_keys(&self) -> &[ParameterKey] {
        &self.parameter_keys
    }

    pub const fn quality(&self) -> UncertaintyQuality {
        self.quality
    }

    pub fn samples(&self) -> &[Vec<f64>] {
        &self.samples
    }

    pub const fn effective_samples(&self) -> usize {
        self.samples.len()
    }

    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PosteriorControlSamples {
    seed: u64,
    samples: Vec<Controls>,
    digest: [u8; 32],
}

impl PosteriorControlSamples {
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    pub fn samples(&self) -> &[Controls] {
        &self.samples
    }

    pub const fn effective_samples(&self) -> usize {
        self.samples.len()
    }

    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

pub(crate) fn percentile_interval(
    mut values: Vec<f64>,
    estimate: f64,
    mass: f64,
    seed: u64,
    failed_refits: usize,
) -> Result<IntervalEstimate, UncertaintyError> {
    validate_interval_mass(mass)?;
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return Err(UncertaintyError::InvalidInterval);
    }
    values.sort_by(f64::total_cmp);
    let tail = (1.0 - mass) / 2.0;
    let lower_index = ((values.len() - 1) as f64 * tail).floor() as usize;
    let upper_index = ((values.len() - 1) as f64 * (1.0 - tail)).ceil() as usize;
    IntervalEstimate::try_new(IntervalInput {
        lower: values[lower_index],
        estimate,
        upper: values[upper_index],
        method: UncertaintyMethod::BlockedBootstrapPercentile,
        seed: Some(seed),
        effective_samples: values.len(),
        failed_refits,
        regularization_added: 0.0,
    })
}

pub(crate) fn digest_parameter_samples(
    domain: &[u8],
    seed: u64,
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
    samples: &[Vec<f64>],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&seed.to_le_bytes());
    hasher.update(&dataset_manifest_hash);
    hasher.update(&training_fold_hash);
    hasher.update(&(samples.len() as u64).to_le_bytes());
    for sample in samples {
        hasher.update(&(sample.len() as u64).to_le_bytes());
        for value in sample {
            hasher.update(&value.to_bits().to_le_bytes());
        }
    }
    *hasher.finalize().as_bytes()
}

pub(crate) fn hash_parameter_keys(hasher: &mut blake3::Hasher, keys: &[ParameterKey]) {
    hasher.update(&(keys.len() as u64).to_le_bytes());
    for key in keys {
        match key {
            ParameterKey::AlphaIntercept => {
                hasher.update(&[1]);
            }
            ParameterKey::BetaIntercept => {
                hasher.update(&[2]);
            }
            ParameterKey::AlphaShared(feature) => {
                hasher.update(&[3]);
                hash_feature_key(hasher, feature);
            }
            ParameterKey::BetaShared(feature) => {
                hasher.update(&[4]);
                hash_feature_key(hasher, feature);
            }
            ParameterKey::AlphaAsset { asset, feature } => {
                hasher.update(&[5]);
                hash_asset(hasher, asset);
                hash_feature_key(hasher, feature);
            }
            ParameterKey::BetaAsset { asset, feature } => {
                hasher.update(&[6]);
                hash_asset(hasher, asset);
                hash_feature_key(hasher, feature);
            }
        }
    }
}

fn hash_feature_key(hasher: &mut blake3::Hasher, key: &crate::ControlFeatureKey) {
    hash_bytes(hasher, key.id().as_bytes());
    hash_bytes(hasher, key.version().to_string().as_bytes());
}

fn hash_asset(hasher: &mut blake3::Hasher, asset: &AssetId) {
    hasher.update(&[asset.namespace() as u8]);
    hash_bytes(hasher, asset.chain_id().as_bytes());
    hash_bytes(hasher, asset.contract_or_mint().as_bytes());
    hash_bytes(hasher, asset.canonical_symbol().as_bytes());
    hasher.update(&asset.generation().to_le_bytes());
}

fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

pub(crate) fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub(crate) struct DeterministicNormal {
    state: u64,
    spare: Option<f64>,
}

impl DeterministicNormal {
    pub(crate) const fn new(seed: u64) -> Self {
        Self {
            state: seed,
            spare: None,
        }
    }

    pub(crate) fn next(&mut self) -> f64 {
        if let Some(value) = self.spare.take() {
            return value;
        }
        let first = self.uniform_open();
        let second = self.uniform_open();
        let radius = (-2.0 * first.ln()).sqrt();
        let angle = std::f64::consts::TAU * second;
        self.spare = Some(radius * angle.sin());
        radius * angle.cos()
    }

    fn uniform_open(&mut self) -> f64 {
        self.state = splitmix64(self.state);
        let mantissa = self.state >> 11;
        (mantissa as f64 + 0.5) / ((1_u64 << 53) as f64)
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum UncertaintyError {
    #[error("invalid uncertainty configuration")]
    InvalidConfig,
    #[error("fit must converge before uncertainty is estimated")]
    NonConvergedFit,
    #[error("uncertainty matrix or parameter dimension is invalid")]
    Dimension,
    #[error("uncertainty input or result is nonfinite")]
    NonFinite,
    #[error("elastic-net active set is ambiguous at the declared threshold")]
    AmbiguousActiveSet,
    #[error("Hessian requires excessive diagonal regularization")]
    ExcessiveRegularization,
    #[error("regularized Hessian is ill conditioned")]
    IllConditioned,
    #[error("regularized Hessian is singular")]
    SingularHessian,
    #[error("posterior sample capacity is invalid")]
    SampleCapacity,
    #[error("bounded posterior sampling could not satisfy coefficient constraints")]
    ConstraintSampling,
    #[error("uncertainty interval is invalid")]
    InvalidInterval,
    #[error("fit and uncertainty evidence do not match")]
    EvidenceMismatch,
    #[error(transparent)]
    Fit(#[from] FitError),
    #[error(transparent)]
    Control(#[from] ControlError),
}

#[cfg(test)]
mod tests {
    use super::{
        LaplaceConfig, UncertaintyError, UncertaintyMethod, classify_active_set,
        percentile_interval,
    };

    #[test]
    fn active_set_accepts_only_exact_penalized_zero_as_inactive() {
        let config = LaplaceConfig::fixture();
        let (active, inactive) =
            classify_active_set(&[1.0e-12, 0.0], &[false, true], config).expect("exact active set");
        assert_eq!(active, [0]);
        assert_eq!(inactive, [1]);
        assert_eq!(
            classify_active_set(&[1.0e-12], &[true], config),
            Err(UncertaintyError::AmbiguousActiveSet)
        );
    }

    #[test]
    fn percentile_interval_preserves_visible_bootstrap_bias() {
        let interval =
            percentile_interval(vec![10.0, 11.0, 12.0], 0.0, 0.90, 7, 2).expect("interval");
        assert_eq!(interval.lower(), 10.0);
        assert_eq!(interval.estimate(), 0.0);
        assert_eq!(interval.upper(), 12.0);
        assert_eq!(
            interval.method(),
            UncertaintyMethod::BlockedBootstrapPercentile
        );
    }
}
