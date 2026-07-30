//! Deterministic offline estimators for the approved stochastic cusp convention.

mod optimizer;
mod penalty;
mod stationary;
mod transition;

use std::collections::{BTreeMap, BTreeSet};

use domain::AssetId;
use numerics::invert_matrix;
use thiserror::Error;

use crate::{
    CoefficientSign, ControlCovariance, ControlDatum, ControlFeatureKey, ControlMap,
    ControlMapInput, ControlSchema, ControlVector, FeatureCoefficient, LinearControl,
};

const MIN_FIT_ROWS: usize = 32;
const MAX_FIT_ROWS: usize = 100_000;
const MAX_FIT_ASSETS: usize = 64;
const MAX_FIT_PARAMETERS: usize = 256;
const MAX_CONDITION_WORK_UNITS: u64 = 100_000_000;
const MAX_OPTIMIZER_ITERATIONS: u32 = 100_000;
const MAX_OPTIMIZER_BACKTRACKING: u32 = 128;
const MAX_OPTIMIZER_WORK_UNITS: u64 = 1_000_000_000_000;
const MIN_STUDENT_T_DEGREES: f64 = 2.0;
const MAX_STUDENT_T_DEGREES: f64 = 100.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EstimatorKind {
    StationaryDensity,
    StudentTTransition,
}

impl EstimatorKind {
    pub const fn role(self) -> EstimatorRole {
        match self {
            Self::StationaryDensity => EstimatorRole::ResearchComparator,
            Self::StudentTTransition => EstimatorRole::ProductionCandidate,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EstimatorRole {
    ResearchComparator,
    ProductionCandidate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PenaltyConfig {
    lambda: f64,
    l1_ratio: f64,
    asset_multiplier: f64,
}

impl PenaltyConfig {
    pub fn try_new(lambda: f64, l1_ratio: f64, asset_multiplier: f64) -> Result<Self, FitError> {
        let config = Self {
            lambda,
            l1_ratio,
            asset_multiplier,
        };
        config.validate()?;
        Ok(config)
    }

    pub const fn lambda(self) -> f64 {
        self.lambda
    }

    pub const fn l1_ratio(self) -> f64 {
        self.l1_ratio
    }

    pub const fn asset_multiplier(self) -> f64 {
        self.asset_multiplier
    }

    fn validate(self) -> Result<(), FitError> {
        if !self.lambda.is_finite()
            || self.lambda < 0.0
            || !self.l1_ratio.is_finite()
            || !(0.0..=1.0).contains(&self.l1_ratio)
            || !self.asset_multiplier.is_finite()
            || self.asset_multiplier < 1.0
        {
            return Err(FitError::InvalidConfig);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OptimizerConfig {
    max_iterations: u32,
    tolerance: f64,
    initial_step: f64,
    minimum_step: f64,
    max_backtracking: u32,
    max_condition_number: f64,
    work_limit: u64,
}

impl OptimizerConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        max_iterations: u32,
        tolerance: f64,
        initial_step: f64,
        minimum_step: f64,
        max_backtracking: u32,
        max_condition_number: f64,
        work_limit: u64,
    ) -> Result<Self, FitError> {
        let config = Self {
            max_iterations,
            tolerance,
            initial_step,
            minimum_step,
            max_backtracking,
            max_condition_number,
            work_limit,
        };
        config.validate()?;
        Ok(config)
    }

    pub const fn max_iterations(self) -> u32 {
        self.max_iterations
    }

    pub const fn tolerance(self) -> f64 {
        self.tolerance
    }

    pub const fn initial_step(self) -> f64 {
        self.initial_step
    }

    pub const fn minimum_step(self) -> f64 {
        self.minimum_step
    }

    pub const fn max_backtracking(self) -> u32 {
        self.max_backtracking
    }

    pub const fn max_condition_number(self) -> f64 {
        self.max_condition_number
    }

    pub const fn work_limit(self) -> u64 {
        self.work_limit
    }

    fn validate(self) -> Result<(), FitError> {
        if self.max_iterations == 0
            || self.max_iterations > MAX_OPTIMIZER_ITERATIONS
            || !self.tolerance.is_finite()
            || self.tolerance <= 0.0
            || self.tolerance > 1.0e-2
            || !self.initial_step.is_finite()
            || self.initial_step <= 0.0
            || !self.minimum_step.is_finite()
            || self.minimum_step <= 0.0
            || self.minimum_step >= self.initial_step
            || self.max_backtracking == 0
            || self.max_backtracking > MAX_OPTIMIZER_BACKTRACKING
            || !self.max_condition_number.is_finite()
            || self.max_condition_number < 1.0
            || self.work_limit == 0
            || self.work_limit > MAX_OPTIMIZER_WORK_UNITS
        {
            return Err(FitError::InvalidConfig);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitConfig {
    estimator: EstimatorKind,
    degrees_of_freedom: f64,
    penalty: PenaltyConfig,
    optimizer: OptimizerConfig,
    integration_bound: f64,
    integration_intervals: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitConfigInput {
    pub estimator: EstimatorKind,
    pub degrees_of_freedom: f64,
    pub penalty: PenaltyConfig,
    pub optimizer: OptimizerConfig,
    pub integration_bound: f64,
    pub integration_intervals: u32,
}

impl FitConfig {
    pub fn try_new(input: FitConfigInput) -> Result<Self, FitError> {
        let config = Self {
            estimator: input.estimator,
            degrees_of_freedom: input.degrees_of_freedom,
            penalty: input.penalty,
            optimizer: input.optimizer,
            integration_bound: input.integration_bound,
            integration_intervals: input.integration_intervals,
        };
        config.validate()?;
        Ok(config)
    }

    /// Deterministic bounded configuration used by the repository contract tests.
    pub fn fixture(estimator: EstimatorKind) -> Self {
        let (maximum_iterations, tolerance, initial_step) = match estimator {
            EstimatorKind::StationaryDensity => (4_000, 1.0e-6, 0.5),
            EstimatorKind::StudentTTransition => (2_000, 1.0e-7, 0.2),
        };
        Self {
            estimator,
            degrees_of_freedom: 5.0,
            penalty: PenaltyConfig {
                lambda: 0.002,
                l1_ratio: 0.25,
                asset_multiplier: 2.0,
            },
            optimizer: OptimizerConfig {
                max_iterations: maximum_iterations,
                tolerance,
                initial_step,
                minimum_step: 1.0e-12,
                max_backtracking: 48,
                max_condition_number: 1.0e12,
                work_limit: 5_000_000_000,
            },
            integration_bound: 6.0,
            integration_intervals: 128,
        }
    }

    #[must_use]
    pub fn with_penalty(mut self, penalty: PenaltyConfig) -> Self {
        self.penalty = penalty;
        self
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: u32) -> Self {
        self.optimizer.max_iterations = max_iterations;
        self
    }

    #[must_use]
    pub fn with_max_condition_number(mut self, max_condition_number: f64) -> Self {
        self.optimizer.max_condition_number = max_condition_number;
        self
    }

    pub const fn estimator(&self) -> EstimatorKind {
        self.estimator
    }

    pub const fn degrees_of_freedom(&self) -> f64 {
        self.degrees_of_freedom
    }

    pub const fn penalty(&self) -> PenaltyConfig {
        self.penalty
    }

    pub const fn optimizer(&self) -> OptimizerConfig {
        self.optimizer
    }

    pub const fn integration_bound(&self) -> f64 {
        self.integration_bound
    }

    pub const fn integration_intervals(&self) -> u32 {
        self.integration_intervals
    }

    fn validate(&self) -> Result<(), FitError> {
        self.penalty.validate()?;
        self.optimizer.validate()?;
        if !self.degrees_of_freedom.is_finite()
            || self.degrees_of_freedom <= MIN_STUDENT_T_DEGREES
            || self.degrees_of_freedom > MAX_STUDENT_T_DEGREES
            || !self.integration_bound.is_finite()
            || self.integration_bound < 2.0
            || self.integration_bound > 20.0
            || self.integration_intervals < 32
            || self.integration_intervals > 1_024
            || !self.integration_intervals.is_multiple_of(2)
        {
            return Err(FitError::InvalidConfig);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitRow {
    asset: AssetId,
    features: ControlVector,
    event_time_ns: i64,
    state: f64,
    delta_state: f64,
    delta_time: f64,
    innovation_scale: f64,
    weight: f64,
}

impl FitRow {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        asset: AssetId,
        features: ControlVector,
        event_time_ns: i64,
        state: f64,
        delta_state: f64,
        delta_time: f64,
        innovation_scale: f64,
        weight: f64,
    ) -> Result<Self, FitError> {
        if event_time_ns <= 0
            || [state, delta_state, delta_time, innovation_scale, weight]
                .iter()
                .any(|value| !value.is_finite())
            || delta_time <= 0.0
            || innovation_scale <= 0.0
            || weight <= 0.0
        {
            return Err(FitError::InvalidRow);
        }
        Ok(Self {
            asset,
            features,
            event_time_ns,
            state,
            delta_state,
            delta_time,
            innovation_scale,
            weight,
        })
    }

    pub const fn event_time_ns(&self) -> i64 {
        self.event_time_ns
    }

    pub const fn asset(&self) -> &AssetId {
        &self.asset
    }

    pub const fn weight(&self) -> f64 {
        self.weight
    }

    pub(crate) fn with_weight_multiplier(&self, multiplier: u32) -> Result<Self, FitError> {
        if multiplier == 0 {
            return Err(FitError::InvalidRow);
        }
        Self::try_new(
            self.asset.clone(),
            self.features.clone(),
            self.event_time_ns,
            self.state,
            self.delta_state,
            self.delta_time,
            self.innovation_scale,
            self.weight * f64::from(multiplier),
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitTimeRange {
    start_ns: i64,
    end_ns: i64,
}

impl FitTimeRange {
    pub fn try_new(start_ns: i64, end_ns: i64) -> Result<Self, FitError> {
        if start_ns <= 0 || end_ns <= start_ns {
            return Err(FitError::InvalidTimeRange);
        }
        Ok(Self { start_ns, end_ns })
    }

    pub const fn start_ns(&self) -> i64 {
        self.start_ns
    }

    pub const fn end_ns(&self) -> i64 {
        self.end_ns
    }

    const fn contains(&self, event_time_ns: i64) -> bool {
        event_time_ns >= self.start_ns && event_time_ns < self.end_ns
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitDataset {
    schema: ControlSchema,
    normalization_hash: [u8; 32],
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
    training_time_range: FitTimeRange,
    residual_control_covariance: ControlCovariance,
    rows: Vec<FitRow>,
}

impl FitDataset {
    pub fn try_new(
        mut schema: ControlSchema,
        normalization_hash: [u8; 32],
        dataset_manifest_hash: [u8; 32],
        training_fold_hash: [u8; 32],
        training_time_range: FitTimeRange,
        residual_control_covariance: ControlCovariance,
        rows: Vec<FitRow>,
    ) -> Result<Self, FitError> {
        schema.validate().map_err(FitError::Control)?;
        if rows.len() < MIN_FIT_ROWS || rows.len() > MAX_FIT_ROWS {
            return Err(FitError::RowCapacity);
        }
        if normalization_hash.iter().all(|byte| *byte == 0)
            || dataset_manifest_hash.iter().all(|byte| *byte == 0)
            || training_fold_hash.iter().all(|byte| *byte == 0)
        {
            return Err(FitError::InvalidIdentity);
        }
        let assets = rows.iter().map(|row| &row.asset).collect::<BTreeSet<_>>();
        if assets.is_empty() || assets.len() > MAX_FIT_ASSETS {
            return Err(FitError::AssetCapacity);
        }
        let mut previous: Option<(&AssetId, i64)> = None;
        for row in &rows {
            if !training_time_range.contains(row.event_time_ns) {
                return Err(FitError::InvalidTimeRange);
            }
            if let Some((previous_asset, previous_time)) = previous
                && (row.event_time_ns < previous_time
                    || (row.event_time_ns == previous_time && row.asset <= *previous_asset))
            {
                return Err(FitError::InvalidRowOrder);
            }
            normalized_features(&schema, &row.features)?;
            previous = Some((&row.asset, row.event_time_ns));
        }
        Ok(Self {
            schema,
            normalization_hash,
            dataset_manifest_hash,
            training_fold_hash,
            training_time_range,
            residual_control_covariance,
            rows,
        })
    }

    pub const fn schema(&self) -> &ControlSchema {
        &self.schema
    }

    pub fn rows(&self) -> &[FitRow] {
        &self.rows
    }

    pub const fn dataset_manifest_hash(&self) -> [u8; 32] {
        self.dataset_manifest_hash
    }

    pub const fn training_fold_hash(&self) -> [u8; 32] {
        self.training_fold_hash
    }

    pub const fn training_time_range(&self) -> &FitTimeRange {
        &self.training_time_range
    }

    pub const fn residual_control_covariance(&self) -> ControlCovariance {
        self.residual_control_covariance
    }

    pub(crate) fn bootstrap_replica(
        &self,
        dataset_manifest_hash: [u8; 32],
        rows: Vec<FitRow>,
    ) -> Result<Self, FitError> {
        Self::try_new(
            self.schema.clone(),
            self.normalization_hash,
            dataset_manifest_hash,
            self.training_fold_hash,
            self.training_time_range.clone(),
            self.residual_control_covariance,
            rows,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FitTermination {
    Converged,
    IterationLimit,
    IllConditioned,
    LineSearchFailure,
    WorkLimit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitDiagnostics {
    estimator: EstimatorKind,
    role: EstimatorRole,
    objective: f64,
    proximal_gradient_norm: f64,
    iterations: u32,
    evaluations: u64,
    backtracking_steps: u64,
    condition_estimate: f64,
    converged: bool,
    termination: FitTermination,
}

impl FitDiagnostics {
    pub const fn estimator(&self) -> EstimatorKind {
        self.estimator
    }

    pub const fn role(&self) -> EstimatorRole {
        self.role
    }

    pub const fn objective(&self) -> f64 {
        self.objective
    }

    pub const fn proximal_gradient_norm(&self) -> f64 {
        self.proximal_gradient_norm
    }

    pub const fn iterations(&self) -> u32 {
        self.iterations
    }

    pub const fn evaluations(&self) -> u64 {
        self.evaluations
    }

    pub const fn backtracking_steps(&self) -> u64 {
        self.backtracking_steps
    }

    pub const fn condition_estimate(&self) -> f64 {
        self.condition_estimate
    }

    pub const fn converged(&self) -> bool {
        self.converged
    }

    pub const fn termination(&self) -> FitTermination {
        self.termination
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Objective {
    value: f64,
    gradient: Vec<f64>,
}

impl Objective {
    pub const fn value(&self) -> f64 {
        self.value
    }

    pub fn gradient(&self) -> &[f64] {
        &self.gradient
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum ParameterKey {
    AlphaIntercept,
    BetaIntercept,
    AlphaShared(ControlFeatureKey),
    BetaShared(ControlFeatureKey),
    AlphaAsset {
        asset: AssetId,
        feature: ControlFeatureKey,
    },
    BetaAsset {
        asset: AssetId,
        feature: ControlFeatureKey,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitResult {
    control_map: ControlMap,
    diagnostics: FitDiagnostics,
    reference_asset: AssetId,
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
    candidate_condition_limit: f64,
    parameters: Vec<f64>,
    parameter_keys: Vec<ParameterKey>,
    uncertainty_problem: Box<FitProblem>,
}

impl FitResult {
    pub const fn control_map(&self) -> &ControlMap {
        &self.control_map
    }

    pub const fn diagnostics(&self) -> &FitDiagnostics {
        &self.diagnostics
    }

    pub const fn role(&self) -> EstimatorRole {
        self.diagnostics.role
    }

    pub const fn reference_asset(&self) -> &AssetId {
        &self.reference_asset
    }

    pub fn parameter_values(&self) -> &[f64] {
        &self.parameters
    }

    pub fn parameter_keys(&self) -> &[ParameterKey] {
        &self.parameter_keys
    }

    pub const fn dataset_manifest_hash(&self) -> [u8; 32] {
        self.dataset_manifest_hash
    }

    pub const fn training_fold_hash(&self) -> [u8; 32] {
        self.training_fold_hash
    }

    pub const fn effective_observations(&self) -> usize {
        self.uncertainty_problem.rows.len()
    }

    pub fn analytic_hessian(&self) -> Result<Vec<Vec<f64>>, FitError> {
        self.uncertainty_problem.smooth_hessian(&self.parameters)
    }

    pub(crate) fn control_map_for_parameters(
        &self,
        parameters: &[f64],
    ) -> Result<ControlMap, FitError> {
        self.uncertainty_problem.decode_control_map(parameters)
    }

    pub(crate) fn penalized_parameter_mask(&self) -> Vec<bool> {
        self.uncertainty_problem
            .layout
            .coordinates
            .iter()
            .map(|coordinate| {
                coordinate.scope != CoordinateScope::Intercept
                    && coordinate.sparse_weight > 0.0
                    && self.uncertainty_problem.config.penalty.lambda()
                        * self.uncertainty_problem.config.penalty.l1_ratio()
                        > 0.0
            })
            .collect()
    }

    pub(crate) fn parameter_signs(&self) -> Vec<CoefficientSign> {
        self.uncertainty_problem
            .layout
            .coordinates
            .iter()
            .map(|coordinate| coordinate.sign)
            .collect()
    }

    pub fn into_candidate_artifact(self) -> Result<CandidateFitArtifact, FitError> {
        if !self.diagnostics.converged
            || self.diagnostics.role != EstimatorRole::ProductionCandidate
            || !self.diagnostics.condition_estimate.is_finite()
            || self.diagnostics.condition_estimate > self.candidate_condition_limit
        {
            return Err(FitError::CandidateIneligible);
        }
        Ok(CandidateFitArtifact {
            control_map: self.control_map,
            diagnostics: self.diagnostics,
            reference_asset: self.reference_asset,
            dataset_manifest_hash: self.dataset_manifest_hash,
            training_fold_hash: self.training_fold_hash,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CandidateFitArtifact {
    control_map: ControlMap,
    diagnostics: FitDiagnostics,
    reference_asset: AssetId,
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
}

impl CandidateFitArtifact {
    pub const fn control_map(&self) -> &ControlMap {
        &self.control_map
    }

    pub const fn diagnostics(&self) -> &FitDiagnostics {
        &self.diagnostics
    }

    pub const fn reference_asset(&self) -> &AssetId {
        &self.reference_asset
    }

    pub const fn dataset_manifest_hash(&self) -> [u8; 32] {
        self.dataset_manifest_hash
    }

    pub const fn training_fold_hash(&self) -> [u8; 32] {
        self.training_fold_hash
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FitProblem {
    dataset: FitDataset,
    config: FitConfig,
    rows: Vec<PreparedRow>,
    assets: Vec<AssetId>,
    layout: ParameterLayout,
    condition_estimate: f64,
    preflight_work_units: u64,
}

impl FitProblem {
    pub fn try_new(dataset: &FitDataset, config: FitConfig) -> Result<Self, FitError> {
        config.validate()?;
        let assets = dataset
            .rows
            .iter()
            .map(|row| row.asset.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let asset_indices = assets
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, asset)| (asset, index))
            .collect::<BTreeMap<_, _>>();
        let rows = dataset
            .rows
            .iter()
            .map(|row| {
                let asset_index = asset_indices
                    .get(&row.asset)
                    .copied()
                    .ok_or(FitError::InvalidRow)?;
                Ok(PreparedRow {
                    asset_index,
                    normalized_features: normalized_features(&dataset.schema, &row.features)?,
                    state: row.state,
                    delta_state: row.delta_state,
                    delta_time: row.delta_time,
                    innovation_scale: row.innovation_scale,
                    weight: row.weight,
                })
            })
            .collect::<Result<Vec<_>, FitError>>()?;
        let layout = ParameterLayout::build(&dataset.schema, assets.len())?;
        let row_count = u64::try_from(rows.len()).map_err(|_| FitError::WorkCapacity)?;
        let dimension =
            u64::try_from(layout.coordinates.len()).map_err(|_| FitError::WorkCapacity)?;
        let condition_work = row_count
            .checked_mul(dimension)
            .and_then(|value| value.checked_mul(dimension))
            .ok_or(FitError::WorkCapacity)?;
        if condition_work > MAX_CONDITION_WORK_UNITS || condition_work > config.optimizer.work_limit
        {
            return Err(FitError::WorkCapacity);
        }
        let condition_estimate = design_condition_estimate(&rows, &layout);
        Ok(Self {
            dataset: dataset.clone(),
            config,
            rows,
            assets,
            layout,
            condition_estimate,
            preflight_work_units: condition_work,
        })
    }

    pub fn initial_parameters(&self) -> Vec<f64> {
        vec![0.0; self.layout.coordinates.len()]
    }

    pub fn smooth_objective(&self, parameters: &[f64]) -> Result<Objective, FitError> {
        self.validate_parameters(parameters)?;
        let mut objective = match self.config.estimator {
            EstimatorKind::StationaryDensity => stationary::objective(self, parameters)?,
            EstimatorKind::StudentTTransition => transition::objective(self, parameters)?,
        };
        penalty::add_smooth_penalty(
            &mut objective,
            parameters,
            &self.layout,
            self.config.penalty,
        )?;
        validate_objective(&objective, parameters.len())?;
        Ok(objective)
    }

    pub fn smooth_hessian(&self, parameters: &[f64]) -> Result<Vec<Vec<f64>>, FitError> {
        self.validate_parameters(parameters)?;
        let mut hessian = match self.config.estimator {
            EstimatorKind::StationaryDensity => stationary::hessian(self, parameters)?,
            EstimatorKind::StudentTTransition => transition::hessian(self, parameters)?,
        };
        penalty::add_smooth_hessian(&mut hessian, &self.layout, self.config.penalty)?;
        validate_hessian(&hessian, parameters.len())?;
        Ok(hessian)
    }

    fn validate_parameters(&self, parameters: &[f64]) -> Result<(), FitError> {
        if parameters.len() != self.layout.coordinates.len()
            || parameters.iter().any(|value| !value.is_finite())
        {
            return Err(FitError::InvalidParameters);
        }
        Ok(())
    }

    fn fit(self) -> Result<FitResult, FitError> {
        let optimized = optimizer::optimize(&self)?;
        let control_map = self.decode_control_map(&optimized.parameters)?;
        let parameter_keys = self.parameter_keys()?;
        let reference_asset = self.assets[0].clone();
        let dataset_manifest_hash = self.dataset.dataset_manifest_hash;
        let training_fold_hash = self.dataset.training_fold_hash;
        let candidate_condition_limit = self.config.optimizer.max_condition_number;
        let mut uncertainty_problem = self;
        uncertainty_problem.dataset.rows.clear();
        Ok(FitResult {
            control_map,
            diagnostics: optimized.diagnostics,
            reference_asset,
            dataset_manifest_hash,
            training_fold_hash,
            candidate_condition_limit,
            parameters: optimized.parameters,
            parameter_keys,
            uncertainty_problem: Box::new(uncertainty_problem),
        })
    }

    fn parameter_keys(&self) -> Result<Vec<ParameterKey>, FitError> {
        self.layout
            .coordinates
            .iter()
            .map(|coordinate| {
                let feature = || {
                    self.dataset
                        .schema
                        .features
                        .get(coordinate.feature_index)
                        .map(|value| value.key.clone())
                        .ok_or(FitError::InvalidLayout)
                };
                Ok(match (coordinate.axis, coordinate.scope) {
                    (ControlAxis::Alpha, CoordinateScope::Intercept) => {
                        ParameterKey::AlphaIntercept
                    }
                    (ControlAxis::Beta, CoordinateScope::Intercept) => ParameterKey::BetaIntercept,
                    (ControlAxis::Alpha, CoordinateScope::Shared) => {
                        ParameterKey::AlphaShared(feature()?)
                    }
                    (ControlAxis::Beta, CoordinateScope::Shared) => {
                        ParameterKey::BetaShared(feature()?)
                    }
                    (ControlAxis::Alpha, CoordinateScope::Asset(asset_index)) => {
                        ParameterKey::AlphaAsset {
                            asset: self
                                .assets
                                .get(asset_index)
                                .cloned()
                                .ok_or(FitError::InvalidLayout)?,
                            feature: feature()?,
                        }
                    }
                    (ControlAxis::Beta, CoordinateScope::Asset(asset_index)) => {
                        ParameterKey::BetaAsset {
                            asset: self
                                .assets
                                .get(asset_index)
                                .cloned()
                                .ok_or(FitError::InvalidLayout)?,
                            feature: feature()?,
                        }
                    }
                })
            })
            .collect()
    }

    fn decode_control_map(&self, parameters: &[f64]) -> Result<ControlMap, FitError> {
        self.validate_parameters(parameters)?;
        let mut alpha_shared = Vec::new();
        let mut beta_shared = Vec::new();
        let mut alpha_assets = BTreeMap::<AssetId, Vec<FeatureCoefficient>>::new();
        let mut beta_assets = BTreeMap::<AssetId, Vec<FeatureCoefficient>>::new();
        for (index, coordinate) in self.layout.coordinates.iter().enumerate().skip(2) {
            let key = self.dataset.schema.features[coordinate.feature_index]
                .key
                .clone();
            let coefficient =
                FeatureCoefficient::try_new(key, parameters[index]).map_err(FitError::Control)?;
            let target = match (coordinate.axis, coordinate.scope) {
                (ControlAxis::Alpha, CoordinateScope::Shared) => &mut alpha_shared,
                (ControlAxis::Beta, CoordinateScope::Shared) => &mut beta_shared,
                (ControlAxis::Alpha, CoordinateScope::Asset(asset_index)) => alpha_assets
                    .entry(self.assets[asset_index].clone())
                    .or_default(),
                (ControlAxis::Beta, CoordinateScope::Asset(asset_index)) => beta_assets
                    .entry(self.assets[asset_index].clone())
                    .or_default(),
                (_, CoordinateScope::Intercept) => return Err(FitError::InvalidLayout),
            };
            target.push(coefficient);
        }
        let alpha = LinearControl::try_new(parameters[0], alpha_shared, alpha_assets)
            .map_err(FitError::Control)?;
        let beta = LinearControl::try_new(parameters[1], beta_shared, beta_assets)
            .map_err(FitError::Control)?;
        ControlMap::try_new(ControlMapInput {
            version: self.dataset.schema.version.clone(),
            schema: self.dataset.schema.clone(),
            alpha,
            beta,
            normalization_hash: self.dataset.normalization_hash,
            residual_covariance: self.dataset.residual_control_covariance,
        })
        .map_err(FitError::Control)
    }

    fn controls(&self, parameters: &[f64], row: &PreparedRow) -> Result<(f64, f64), FitError> {
        let mut alpha = parameters[0];
        let mut beta = parameters[1];
        for (index, coordinate) in self.layout.coordinates.iter().enumerate().skip(2) {
            if coordinate.applies_to(row.asset_index) {
                let feature = row.normalized_features[coordinate.feature_index];
                match coordinate.axis {
                    ControlAxis::Alpha => {
                        alpha = parameters[index].mul_add(feature, alpha);
                    }
                    ControlAxis::Beta => {
                        beta = parameters[index].mul_add(feature, beta);
                    }
                }
            }
        }
        if alpha.is_finite() && beta.is_finite() {
            Ok((alpha, beta))
        } else {
            Err(FitError::NonFiniteObjective)
        }
    }

    fn control_derivative(
        &self,
        coordinate_index: usize,
        row: &PreparedRow,
    ) -> Result<(f64, f64), FitError> {
        let coordinate = self
            .layout
            .coordinates
            .get(coordinate_index)
            .ok_or(FitError::InvalidLayout)?;
        let value = match coordinate.scope {
            CoordinateScope::Intercept => 1.0,
            CoordinateScope::Shared => row.normalized_features[coordinate.feature_index],
            CoordinateScope::Asset(asset_index) if asset_index == row.asset_index => {
                row.normalized_features[coordinate.feature_index]
            }
            CoordinateScope::Asset(_) => 0.0,
        };
        Ok(match coordinate.axis {
            ControlAxis::Alpha => (value, 0.0),
            ControlAxis::Beta => (0.0, value),
        })
    }

    fn evaluation_work_units(&self) -> Result<u64, FitError> {
        let rows = u64::try_from(self.rows.len()).map_err(|_| FitError::WorkCapacity)?;
        let parameters =
            u64::try_from(self.layout.coordinates.len() + 1).map_err(|_| FitError::WorkCapacity)?;
        let quadrature = match self.config.estimator {
            EstimatorKind::StationaryDensity => u64::from(self.config.integration_intervals + 1),
            EstimatorKind::StudentTTransition => 1,
        };
        rows.checked_mul(parameters)
            .and_then(|value| value.checked_mul(quadrature))
            .ok_or(FitError::WorkCapacity)
    }
}

pub fn fit(dataset: &FitDataset, config: FitConfig) -> Result<FitResult, FitError> {
    FitProblem::try_new(dataset, config)?.fit()
}

#[derive(Clone, Debug, PartialEq)]
struct PreparedRow {
    asset_index: usize,
    normalized_features: Vec<f64>,
    state: f64,
    delta_state: f64,
    delta_time: f64,
    innovation_scale: f64,
    weight: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlAxis {
    Alpha,
    Beta,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinateScope {
    Intercept,
    Shared,
    Asset(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Coordinate {
    axis: ControlAxis,
    scope: CoordinateScope,
    feature_index: usize,
    sign: CoefficientSign,
    sparse_weight: f64,
}

impl Coordinate {
    fn applies_to(self, asset_index: usize) -> bool {
        match self.scope {
            CoordinateScope::Intercept | CoordinateScope::Shared => true,
            CoordinateScope::Asset(expected) => expected == asset_index,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ParameterLayout {
    coordinates: Vec<Coordinate>,
}

impl ParameterLayout {
    fn build(schema: &ControlSchema, asset_count: usize) -> Result<Self, FitError> {
        let mut coordinates = vec![
            Coordinate {
                axis: ControlAxis::Alpha,
                scope: CoordinateScope::Intercept,
                feature_index: 0,
                sign: CoefficientSign::Any,
                sparse_weight: 0.0,
            },
            Coordinate {
                axis: ControlAxis::Beta,
                scope: CoordinateScope::Intercept,
                feature_index: 0,
                sign: CoefficientSign::Any,
                sparse_weight: 0.0,
            },
        ];
        for scope in std::iter::once(CoordinateScope::Shared)
            .chain((1..asset_count).map(CoordinateScope::Asset))
        {
            for (feature_index, feature) in schema.features.iter().enumerate() {
                let group = schema
                    .group(&feature.group_id)
                    .ok_or(FitError::InvalidLayout)?;
                if feature.target.permits_alpha() {
                    coordinates.push(Coordinate {
                        axis: ControlAxis::Alpha,
                        scope,
                        feature_index,
                        sign: group.alpha_sign,
                        sparse_weight: group.sparse_penalty,
                    });
                }
                if feature.target.permits_beta() {
                    coordinates.push(Coordinate {
                        axis: ControlAxis::Beta,
                        scope,
                        feature_index,
                        sign: group.beta_sign,
                        sparse_weight: group.sparse_penalty,
                    });
                }
            }
        }
        if coordinates.len() <= 2 || coordinates.len() > MAX_FIT_PARAMETERS {
            return Err(FitError::ParameterCapacity);
        }
        Ok(Self { coordinates })
    }
}

fn normalized_features(
    schema: &ControlSchema,
    vector: &ControlVector,
) -> Result<Vec<f64>, FitError> {
    if vector.values().len() != schema.features.len()
        || vector
            .values()
            .iter()
            .zip(&schema.features)
            .any(|(value, feature)| value.key != feature.key)
    {
        return Err(FitError::FeatureSetMismatch);
    }
    vector
        .values()
        .iter()
        .zip(&schema.features)
        .map(|(value, feature)| match value.datum {
            ControlDatum::Present(raw) => {
                let normalized = (raw - feature.normalization_center) / feature.normalization_scale;
                if normalized.is_finite() {
                    Ok(normalized)
                } else {
                    Err(FitError::InvalidRow)
                }
            }
            ControlDatum::Missing(_) if feature.required => Err(FitError::RequiredFeatureMissing {
                key: feature.key.clone(),
            }),
            ControlDatum::Missing(_) => Ok(0.0),
        })
        .collect()
}

fn design_condition_estimate(rows: &[PreparedRow], layout: &ParameterLayout) -> f64 {
    let dimension = layout.coordinates.len();
    let mut gram = vec![vec![0.0; dimension]; dimension];
    for row in rows {
        let design = layout
            .coordinates
            .iter()
            .map(|coordinate| {
                let feature = match coordinate.scope {
                    CoordinateScope::Intercept => 1.0,
                    CoordinateScope::Shared => row.normalized_features[coordinate.feature_index],
                    CoordinateScope::Asset(asset_index) if asset_index == row.asset_index => {
                        row.normalized_features[coordinate.feature_index]
                    }
                    CoordinateScope::Asset(_) => 0.0,
                };
                match coordinate.axis {
                    ControlAxis::Alpha => feature,
                    ControlAxis::Beta => feature * row.state,
                }
            })
            .collect::<Vec<_>>();
        for left in 0..dimension {
            for right in 0..dimension {
                gram[left][right] = design[left].mul_add(design[right], gram[left][right]);
            }
        }
    }
    let scales = (0..dimension)
        .map(|index| gram[index][index].sqrt())
        .collect::<Vec<_>>();
    if scales
        .iter()
        .any(|scale| !scale.is_finite() || *scale <= f64::EPSILON)
    {
        return f64::INFINITY;
    }
    for row in 0..dimension {
        for column in 0..dimension {
            gram[row][column] /= scales[row] * scales[column];
        }
    }
    let Ok(inverse) = invert_matrix(gram.clone()) else {
        return f64::INFINITY;
    };
    let norm = gram
        .iter()
        .map(|row| row.iter().map(|value| value.abs()).sum::<f64>())
        .fold(0.0, f64::max);
    let inverse_norm = inverse
        .iter()
        .map(|row| row.iter().map(|value| value.abs()).sum::<f64>())
        .fold(0.0, f64::max);
    let condition = norm * inverse_norm;
    if condition.is_finite() {
        condition
    } else {
        f64::INFINITY
    }
}

fn validate_objective(objective: &Objective, dimension: usize) -> Result<(), FitError> {
    if !objective.value.is_finite()
        || objective.gradient.len() != dimension
        || objective.gradient.iter().any(|value| !value.is_finite())
    {
        Err(FitError::NonFiniteObjective)
    } else {
        Ok(())
    }
}

fn validate_hessian(hessian: &[Vec<f64>], dimension: usize) -> Result<(), FitError> {
    if hessian.len() != dimension
        || hessian.iter().any(|row| row.len() != dimension)
        || hessian.iter().flatten().any(|value| !value.is_finite())
    {
        return Err(FitError::NonFiniteObjective);
    }
    for (row, values) in hessian.iter().enumerate() {
        for (column, value) in values.iter().enumerate().take(row) {
            let scale = 1.0_f64.max(value.abs()).max(hessian[column][row].abs());
            if (*value - hessian[column][row]).abs() > 1.0e-10 * scale {
                return Err(FitError::InvalidLayout);
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum FitError {
    #[error("invalid cusp fit configuration")]
    InvalidConfig,
    #[error("cusp fit row is invalid or nonfinite")]
    InvalidRow,
    #[error("cusp fit rows are not in strict point-in-time order")]
    InvalidRowOrder,
    #[error("cusp fit training time range is invalid or does not contain every row")]
    InvalidTimeRange,
    #[error("cusp fit dataset row capacity is invalid")]
    RowCapacity,
    #[error("cusp fit asset capacity is invalid")]
    AssetCapacity,
    #[error("cusp fit parameter capacity is invalid")]
    ParameterCapacity,
    #[error("cusp fit package identity is invalid")]
    InvalidIdentity,
    #[error("cusp fit feature vector does not match the schema")]
    FeatureSetMismatch,
    #[error("required cusp fit feature is missing: {key:?}")]
    RequiredFeatureMissing { key: ControlFeatureKey },
    #[error("cusp fit parameter vector is invalid")]
    InvalidParameters,
    #[error("cusp fit parameter layout is invalid")]
    InvalidLayout,
    #[error("cusp fit objective or gradient is nonfinite")]
    NonFiniteObjective,
    #[error("stationary-density quadrature does not contain the fitted mass")]
    IntegrationBoundary,
    #[error("cusp fit work capacity was exceeded")]
    WorkCapacity,
    #[error("fit is not eligible for a candidate artifact")]
    CandidateIneligible,
    #[error(transparent)]
    Control(#[from] crate::ControlError),
}
