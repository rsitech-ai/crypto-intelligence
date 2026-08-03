//! Deterministic regularized logistic stacking over canonical OOF folds.

use std::collections::{BTreeMap, BTreeSet};

use labels::{EventType, LabelDefinition, LabelOutcome};
use nalgebra::{DMatrix, linalg::SymmetricEigen};

use crate::{
    EnsembleError, MatrixColumn, MatrixConstraints, MatrixDatum, MissingValuePolicy, ModuleKind,
    OutOfFoldMatrix, WeightConstraint, WeightConstraints, hash_string, hash_u64,
    module_output::{hash_module_datum, missingness_identity_tag},
    require_nonzero_digest, valid_identifier,
};

const TARGET_SCHEMA_VERSION: u32 = 1;
const MAXIMUM_FOLDS: usize = 10_000;
const MAXIMUM_ROWS: usize = 1_000_000;
const MAXIMUM_COLUMNS: usize = 256;
const MAXIMUM_CELLS: usize = 4_000_000;
const MAXIMUM_ITERATIONS: u32 = 100_000;
const MAXIMUM_BACKTRACKING: u32 = 128;
const MAXIMUM_WORK: u64 = 100_000_000_000;
const NORMALIZATION_FLOOR: f64 = 1.0e-12;
const NANOS_PER_SECOND: i64 = 1_000_000_000;
const TARGET_SCHEMA_DOMAIN: &[u8] = b"cmti:stacker-target-schema:v1\0";
const TARGET_SET_DOMAIN: &[u8] = b"cmti:stacker-target-set:v1\0";
const TRAINING_SET_DOMAIN: &[u8] = b"cmti:stacker-training-set:v1\0";
const MODEL_DOMAIN: &[u8] = b"cmti:meta-stacker-model:v1\0";
const PREDICTION_DOMAIN: &[u8] = b"cmti:meta-stacker-prediction:v1\0";

/// Exact binary event/horizon target fitted by one stacker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackerTargetSchema {
    version: u32,
    event_id: String,
    event_type: EventType,
    horizon_seconds: u64,
    definition_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl StackerTargetSchema {
    pub fn try_from_definition(
        definition: &LabelDefinition,
        horizon_seconds: u64,
    ) -> Result<Self, EnsembleError> {
        let version = TARGET_SCHEMA_VERSION;
        let event_id = definition.id().to_owned();
        let event_type = definition.event_type();
        let definition_hash = definition.definition_hash();
        if !valid_identifier(&event_id) || !definition.horizons_seconds().contains(&horizon_seconds)
        {
            return Err(EnsembleError::InvalidTargetSchema);
        }
        require_nonzero_digest(&definition_hash)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(TARGET_SCHEMA_DOMAIN);
        hasher.update(&version.to_le_bytes());
        hash_string(&mut hasher, &event_id);
        hasher.update(&[event_type_identity_tag(event_type)]);
        hash_u64(&mut hasher, horizon_seconds);
        hasher.update(&definition_hash);
        Ok(Self {
            version,
            event_id,
            event_type,
            horizon_seconds,
            definition_hash,
            evidence_hash: *hasher.finalize().as_bytes(),
        })
    }

    #[must_use]
    pub const fn version(&self) -> u32 {
        self.version
    }

    #[must_use]
    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    #[must_use]
    pub const fn event_type(&self) -> EventType {
        self.event_type
    }

    #[must_use]
    pub const fn horizon_seconds(&self) -> u64 {
        self.horizon_seconds
    }

    #[must_use]
    pub const fn definition_hash(&self) -> [u8; 32] {
        self.definition_hash
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

/// Untrusted target evidence for one exact matrix row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StackerTargetInput {
    pub row_id: u64,
    pub origin_ns: i64,
    pub as_known_at_ns: i64,
    pub horizon_seconds: u64,
    pub definition_hash: [u8; 32],
    pub source_range_hash: [u8; 32],
    pub outcome: LabelOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObservedBinaryOutcome {
    Occurred { offset_seconds: u64 },
    NotOccurred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BinaryTarget {
    outcome: ObservedBinaryOutcome,
    source_range_hash: [u8; 32],
}

/// Canonical exact target coverage for one OOF matrix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackerTargetSet {
    schema: StackerTargetSchema,
    targets: BTreeMap<u64, BinaryTarget>,
    matrix_evidence_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl StackerTargetSet {
    pub fn try_new(
        matrix: &OutOfFoldMatrix,
        schema: StackerTargetSchema,
        inputs: Vec<StackerTargetInput>,
    ) -> Result<Self, EnsembleError> {
        if inputs.len() > MAXIMUM_ROWS {
            return Err(EnsembleError::TargetCoverageMismatch);
        }
        let mut keyed = BTreeMap::new();
        for input in inputs {
            if keyed.insert(input.row_id, input).is_some() {
                return Err(EnsembleError::DuplicateTarget);
            }
        }
        if keyed.len() != matrix.rows().len() {
            return Err(EnsembleError::TargetCoverageMismatch);
        }
        let mut targets = BTreeMap::new();
        for row in matrix.rows() {
            let input = keyed
                .remove(&row.row_id())
                .ok_or(EnsembleError::TargetCoverageMismatch)?;
            require_nonzero_digest(&input.source_range_hash)?;
            if input.row_id == 0
                || input.origin_ns != row.origin().value()
                || input.as_known_at_ns != row.outcome_known_at().value()
                || input.as_known_at_ns < row.outcome_end().value()
                || input.horizon_seconds != schema.horizon_seconds
                || input.definition_hash != schema.definition_hash
            {
                return Err(EnsembleError::TargetMismatch);
            }
            let (outcome, required_known_at_ns) = match input.outcome {
                LabelOutcome::Occurred { offset_seconds }
                    if offset_seconds > 0 && offset_seconds <= schema.horizon_seconds =>
                {
                    (
                        ObservedBinaryOutcome::Occurred { offset_seconds },
                        checked_horizon_end(input.origin_ns, offset_seconds)?,
                    )
                }
                LabelOutcome::NotOccurred => (
                    ObservedBinaryOutcome::NotOccurred,
                    checked_horizon_end(input.origin_ns, schema.horizon_seconds)?,
                ),
                LabelOutcome::Occurred { .. } => return Err(EnsembleError::TargetMismatch),
                LabelOutcome::Censored { .. } | LabelOutcome::Excluded(_) => {
                    return Err(EnsembleError::TargetNotObserved);
                }
            };
            if input.as_known_at_ns < required_known_at_ns {
                return Err(EnsembleError::TargetMismatch);
            }
            targets.insert(
                row.row_id(),
                BinaryTarget {
                    outcome,
                    source_range_hash: input.source_range_hash,
                },
            );
        }
        if !keyed.is_empty() {
            return Err(EnsembleError::TargetCoverageMismatch);
        }
        let evidence_hash = calculate_target_set_hash(matrix, &schema, &targets);
        Ok(Self {
            schema,
            targets,
            matrix_evidence_hash: matrix.evidence_hash(),
            evidence_hash,
        })
    }

    #[must_use]
    pub const fn schema(&self) -> &StackerTargetSchema {
        &self.schema
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

/// One OOF matrix and its exact target coverage.
#[derive(Clone, Copy, Debug)]
pub struct StackerFold<'a> {
    matrix: &'a OutOfFoldMatrix,
    targets: &'a StackerTargetSet,
}

impl<'a> StackerFold<'a> {
    #[must_use]
    pub const fn new(matrix: &'a OutOfFoldMatrix, targets: &'a StackerTargetSet) -> Self {
        Self { matrix, targets }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StackerExample {
    pub(crate) row_id: u64,
    pub(crate) origin_ns: i64,
    pub(crate) known_at_ns: i64,
    pub(crate) values: Vec<MatrixDatum>,
    pub(crate) outcome: f64,
}

/// Bounded aggregate of compatible OOF folds with lineage retained.
#[derive(Clone, Debug, PartialEq)]
pub struct StackerTrainingSet {
    columns: Vec<MatrixColumn>,
    schema_hash: [u8; 32],
    matrix_constraints: MatrixConstraints,
    target_schema: StackerTargetSchema,
    fold_hashes: Vec<[u8; 32]>,
    examples: Vec<StackerExample>,
    evidence_hash: [u8; 32],
}

impl StackerTrainingSet {
    pub fn try_new(mut folds: Vec<StackerFold<'_>>) -> Result<Self, EnsembleError> {
        if folds.is_empty() || folds.len() > MAXIMUM_FOLDS {
            return Err(EnsembleError::EmptyRows);
        }
        folds.sort_by_key(|fold| fold.matrix.outer_fold_hash());
        if folds
            .windows(2)
            .any(|pair| pair[0].matrix.outer_fold_hash() == pair[1].matrix.outer_fold_hash())
        {
            return Err(EnsembleError::DuplicateStackerFold);
        }
        let first = folds[0];
        let columns = first.matrix.columns().to_vec();
        let schema_hash = first.matrix.schema().evidence_hash();
        let target_schema = first.targets.schema.clone();
        let locked_columns = first.matrix.constraints().locked_columns();
        let row_count = folds.iter().try_fold(0_usize, |count, fold| {
            count
                .checked_add(fold.matrix.rows().len())
                .ok_or(EnsembleError::RowCapacity)
        })?;
        if row_count == 0 || row_count > MAXIMUM_ROWS || columns.len() > MAXIMUM_COLUMNS {
            return Err(EnsembleError::RowCapacity);
        }
        let cell_count = row_count
            .checked_mul(columns.len())
            .ok_or(EnsembleError::CellCapacity)?;
        if cell_count > MAXIMUM_CELLS {
            return Err(EnsembleError::CellCapacity);
        }

        let mut row_ids = BTreeSet::new();
        let mut examples = Vec::with_capacity(row_count);
        let mut fold_hashes = Vec::with_capacity(folds.len());
        let mut positive = 0_usize;
        let mut negative = 0_usize;
        for fold in &folds {
            if fold.targets.matrix_evidence_hash != fold.matrix.evidence_hash()
                || fold.matrix.schema().evidence_hash() != schema_hash
                || fold.matrix.columns() != columns
                || fold.targets.schema != target_schema
                || fold.matrix.constraints().locked_columns() != locked_columns
            {
                return Err(EnsembleError::IncompatibleStackerFold);
            }
            fold_hashes.push(fold.matrix.outer_fold_hash());
            for row in fold.matrix.rows() {
                if !row_ids.insert(row.row_id()) {
                    return Err(EnsembleError::DuplicateTrainingRow);
                }
                let target = fold
                    .targets
                    .targets
                    .get(&row.row_id())
                    .ok_or(EnsembleError::TargetCoverageMismatch)?;
                let occurred = matches!(target.outcome, ObservedBinaryOutcome::Occurred { .. });
                if occurred {
                    positive += 1;
                } else {
                    negative += 1;
                }
                examples.push(StackerExample {
                    row_id: row.row_id(),
                    origin_ns: row.origin().value(),
                    known_at_ns: row.outcome_known_at().value(),
                    values: row.values().to_vec(),
                    outcome: if occurred { 1.0 } else { 0.0 },
                });
            }
        }
        if positive == 0 || negative == 0 {
            return Err(EnsembleError::InsufficientTargetVariation);
        }
        let evidence_hash = calculate_training_set_hash(&folds, &examples, schema_hash);
        Ok(Self {
            columns,
            schema_hash,
            matrix_constraints: first.matrix.constraints().clone(),
            target_schema,
            fold_hashes,
            examples,
            evidence_hash,
        })
    }

    #[must_use]
    pub fn columns(&self) -> &[MatrixColumn] {
        &self.columns
    }

    #[must_use]
    pub const fn target_schema(&self) -> &StackerTargetSchema {
        &self.target_schema
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub(crate) fn examples(&self) -> &[StackerExample] {
        &self.examples
    }

    pub(crate) fn fold_hashes(&self) -> &[[u8; 32]] {
        &self.fold_hashes
    }

    pub(crate) const fn schema_hash(&self) -> [u8; 32] {
        self.schema_hash
    }

    pub(crate) const fn matrix_constraints(&self) -> &MatrixConstraints {
        &self.matrix_constraints
    }
}

/// Untrusted deterministic optimizer configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct StackerConfigInput {
    pub l2_penalty: f64,
    pub weight_constraint: WeightConstraint,
    pub missing_value_policy: MissingValuePolicy,
    pub maximum_iterations: u32,
    pub tolerance: f64,
    pub initial_step: f64,
    pub minimum_step: f64,
    pub maximum_backtracking_steps: u32,
    pub work_limit: u64,
    pub additional_locked_modules: Vec<ModuleKind>,
}

/// Validated solver, regularization, missingness, and constraint metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct StackerConfig {
    l2_penalty: f64,
    weight_constraint: WeightConstraint,
    missing_value_policy: MissingValuePolicy,
    maximum_iterations: u32,
    tolerance: f64,
    initial_step: f64,
    minimum_step: f64,
    maximum_backtracking_steps: u32,
    work_limit: u64,
    additional_locked_modules: BTreeSet<ModuleKind>,
}

impl StackerConfig {
    pub fn try_new(input: StackerConfigInput) -> Result<Self, EnsembleError> {
        if !input.l2_penalty.is_finite()
            || input.l2_penalty < 0.0
            || input.maximum_iterations == 0
            || input.maximum_iterations > MAXIMUM_ITERATIONS
            || !input.tolerance.is_finite()
            || input.tolerance <= 0.0
            || input.tolerance > 0.1
            || !input.initial_step.is_finite()
            || input.initial_step <= 0.0
            || !input.minimum_step.is_finite()
            || input.minimum_step <= 0.0
            || input.minimum_step > input.initial_step
            || input.maximum_backtracking_steps == 0
            || input.maximum_backtracking_steps > MAXIMUM_BACKTRACKING
            || input.work_limit == 0
            || input.work_limit > MAXIMUM_WORK
        {
            return Err(EnsembleError::InvalidStackerConfig);
        }
        let additional_locked_modules = input
            .additional_locked_modules
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if additional_locked_modules.len() != input.additional_locked_modules.len() {
            return Err(EnsembleError::InvalidStackerConfig);
        }
        Ok(Self {
            l2_penalty: input.l2_penalty,
            weight_constraint: input.weight_constraint,
            missing_value_policy: input.missing_value_policy,
            maximum_iterations: input.maximum_iterations,
            tolerance: input.tolerance,
            initial_step: input.initial_step,
            minimum_step: input.minimum_step,
            maximum_backtracking_steps: input.maximum_backtracking_steps,
            work_limit: input.work_limit,
            additional_locked_modules,
        })
    }

    #[must_use]
    pub const fn l2_penalty(&self) -> f64 {
        self.l2_penalty
    }

    #[must_use]
    pub const fn weight_constraint(&self) -> WeightConstraint {
        self.weight_constraint
    }

    #[must_use]
    pub const fn missing_value_policy(&self) -> MissingValuePolicy {
        self.missing_value_policy
    }

    #[must_use]
    pub const fn maximum_iterations(&self) -> u32 {
        self.maximum_iterations
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
    pub const fn maximum_backtracking_steps(&self) -> u32 {
        self.maximum_backtracking_steps
    }

    #[must_use]
    pub const fn work_limit(&self) -> u64 {
        self.work_limit
    }

    #[must_use]
    pub const fn additional_locked_modules(&self) -> &BTreeSet<ModuleKind> {
        &self.additional_locked_modules
    }

    pub(crate) fn with_additional_lock(&self, module: ModuleKind) -> Self {
        let mut candidate = self.clone();
        candidate.additional_locked_modules.insert(module);
        candidate
    }

    pub(crate) fn is_additionally_locked(&self, module: ModuleKind) -> bool {
        self.additional_locked_modules.contains(&module)
    }
}

/// Training-only observed mean/scale and missingness transformation.
#[derive(Clone, Debug, PartialEq)]
pub struct StackerNormalization {
    means: Vec<f64>,
    scales: Vec<f64>,
    observed_counts: Vec<u64>,
    constant_columns: Vec<bool>,
    policy: MissingValuePolicy,
}

impl StackerNormalization {
    #[must_use]
    pub fn means(&self) -> &[f64] {
        &self.means
    }

    #[must_use]
    pub fn scales(&self) -> &[f64] {
        &self.scales
    }

    #[must_use]
    pub fn observed_counts(&self) -> &[u64] {
        &self.observed_counts
    }

    #[must_use]
    pub fn constant_columns(&self) -> &[bool] {
        &self.constant_columns
    }

    #[must_use]
    pub const fn policy(&self) -> MissingValuePolicy {
        self.policy
    }
}

/// Honest bounded optimization evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct StackerDiagnostics {
    objective: f64,
    projected_gradient_norm: f64,
    iterations: u32,
    evaluations: u64,
    backtracking_steps: u64,
    condition_estimate: f64,
    converged: bool,
    requested_tolerance: f64,
    termination: &'static str,
}

impl StackerDiagnostics {
    #[must_use]
    pub const fn objective(&self) -> f64 {
        self.objective
    }

    #[must_use]
    pub const fn projected_gradient_norm(&self) -> f64 {
        self.projected_gradient_norm
    }

    #[must_use]
    pub const fn iterations(&self) -> u32 {
        self.iterations
    }

    #[must_use]
    pub const fn evaluations(&self) -> u64 {
        self.evaluations
    }

    #[must_use]
    pub const fn backtracking_steps(&self) -> u64 {
        self.backtracking_steps
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
    pub const fn requested_tolerance(&self) -> f64 {
        self.requested_tolerance
    }

    #[must_use]
    pub const fn termination(&self) -> &'static str {
        self.termination
    }
}

#[derive(Clone, Debug, PartialEq)]
struct FitRow {
    values: Vec<f64>,
    missing: Vec<f64>,
    outcome: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct Parameters {
    weights: Vec<f64>,
    missing_weights: Vec<f64>,
    intercept: f64,
}

impl Parameters {
    fn squared_norm(&self) -> f64 {
        self.weights
            .iter()
            .chain(&self.missing_weights)
            .map(|value| value * value)
            .sum::<f64>()
            + self.intercept * self.intercept
    }
}

/// Inspectable fit outcome; only the converged variant contains a model.
#[derive(Clone, Debug, PartialEq)]
pub enum StackerFitOutcome {
    Converged(Box<MetaStacker>),
    NonConverged(StackerDiagnostics),
}

/// Fitted deterministic uncalibrated binary logistic meta-model.
#[derive(Clone, Debug, PartialEq)]
pub struct MetaStacker {
    column_ids: Vec<String>,
    module_kinds: Vec<ModuleKind>,
    schema_hash: [u8; 32],
    target_schema: StackerTargetSchema,
    normalization: StackerNormalization,
    weights: Vec<f64>,
    missing_weights: Vec<f64>,
    intercept: f64,
    constraints: WeightConstraints,
    config: StackerConfig,
    diagnostics: StackerDiagnostics,
    training_evidence_hash: [u8; 32],
    model_id: [u8; 32],
}

impl MetaStacker {
    pub fn fit(
        training: &StackerTrainingSet,
        config: StackerConfig,
    ) -> Result<Self, EnsembleError> {
        match Self::fit_candidate(training, config)? {
            StackerFitOutcome::Converged(model) => Ok(*model),
            StackerFitOutcome::NonConverged(_) => Err(EnsembleError::StackerNonConverged),
        }
    }

    pub fn fit_candidate(
        training: &StackerTrainingSet,
        config: StackerConfig,
    ) -> Result<StackerFitOutcome, EnsembleError> {
        let mut constraints = WeightConstraints::derive(
            &training.columns,
            &training.matrix_constraints,
            config.weight_constraint,
            &config.additional_locked_modules,
        )?;
        let mut budget = WorkBudget::new(config.work_limit);
        budget.consume(preprocessing_work_units(
            training.examples.len(),
            training.columns.len(),
        )?)?;
        let normalization = fit_normalization(training, &constraints, config.missing_value_policy)?;
        constraints
            .lock_constant_primary_columns(&training.columns, normalization.constant_columns())?;
        let rows = normalize_training(training, &normalization)?;
        let condition_estimate =
            design_condition_estimate(&rows, config.l2_penalty, &constraints, &mut budget)?;
        let positive_fraction = rows.iter().map(|row| row.outcome).sum::<f64>() / rows.len() as f64;
        if !(0.0..1.0).contains(&positive_fraction) {
            return Err(EnsembleError::InsufficientTargetVariation);
        }
        let mut parameters = Parameters {
            weights: vec![0.0; training.columns.len()],
            missing_weights: vec![0.0; training.columns.len()],
            intercept: (positive_fraction / (1.0 - positive_fraction)).ln(),
        };
        budget.consume(projection_work_units(parameters.weights.len())?)?;
        constraints.project(&mut parameters.weights)?;
        zero_locked_missing(
            &mut parameters.missing_weights,
            constraints.missingness_locked(),
        );
        let mut evaluations = 0_u64;
        let (mut objective, mut gradient) =
            objective_gradient(&rows, &parameters, config.l2_penalty, &mut budget)?;
        evaluations += 1;
        let mut projected_gradient_norm = f64::INFINITY;
        let mut total_backtracking = 0_u64;
        let mut iterations = 0_u32;
        let mut next_step = config.initial_step;

        for iteration in 1..=config.maximum_iterations {
            let mut step = next_step;
            let mut accepted = None;
            for _ in 0..config.maximum_backtracking_steps {
                let mut candidate = Parameters {
                    weights: parameters
                        .weights
                        .iter()
                        .zip(&gradient.weights)
                        .map(|(value, direction)| value - step * direction)
                        .collect(),
                    missing_weights: parameters
                        .missing_weights
                        .iter()
                        .zip(&gradient.missing_weights)
                        .map(|(value, direction)| value - step * direction)
                        .collect(),
                    intercept: parameters.intercept - step * gradient.intercept,
                };
                budget.consume(projection_work_units(candidate.weights.len())?)?;
                constraints.project(&mut candidate.weights)?;
                zero_locked_missing(
                    &mut candidate.missing_weights,
                    constraints.missingness_locked(),
                );
                let candidate_objective =
                    objective_only(&rows, &candidate, config.l2_penalty, &mut budget)?;
                evaluations = evaluations
                    .checked_add(1)
                    .ok_or(EnsembleError::StackerWorkCapacity)?;
                if candidate_objective <= objective {
                    accepted = Some((candidate, candidate_objective));
                    break;
                }
                total_backtracking = total_backtracking
                    .checked_add(1)
                    .ok_or(EnsembleError::StackerWorkCapacity)?;
                step *= 0.5;
                if step < config.minimum_step {
                    break;
                }
            }
            let Some((candidate, candidate_objective)) = accepted else {
                projected_gradient_norm = projected_gradient_mapping_norm(
                    &parameters,
                    &gradient,
                    &constraints,
                    &mut budget,
                )?;
                let parameter_scale = parameters.squared_norm().sqrt().max(1.0);
                let converged = projected_gradient_norm <= config.tolerance * parameter_scale;
                let diagnostics = StackerDiagnostics {
                    objective,
                    projected_gradient_norm,
                    iterations,
                    evaluations,
                    backtracking_steps: total_backtracking,
                    condition_estimate,
                    converged,
                    requested_tolerance: config.tolerance,
                    termination: if converged {
                        "projected_gradient_line_search_floor"
                    } else {
                        "line_search_failure"
                    },
                };
                return if converged {
                    Ok(StackerFitOutcome::Converged(Box::new(build_model(
                        training,
                        config,
                        constraints,
                        normalization,
                        parameters,
                        diagnostics,
                    ))))
                } else {
                    Ok(StackerFitOutcome::NonConverged(diagnostics))
                };
            };
            if candidate_objective > objective + 1.0e-12 {
                return Err(EnsembleError::StackerOptimizationFailure);
            }
            let improvement = objective - candidate_objective;
            parameters = candidate;
            iterations = iteration;
            let evaluated = objective_gradient(&rows, &parameters, config.l2_penalty, &mut budget)?;
            evaluations = evaluations
                .checked_add(1)
                .ok_or(EnsembleError::StackerWorkCapacity)?;
            objective = evaluated.0;
            gradient = evaluated.1;
            projected_gradient_norm =
                projected_gradient_mapping_norm(&parameters, &gradient, &constraints, &mut budget)?;
            let parameter_scale = parameters.squared_norm().sqrt().max(1.0);
            if projected_gradient_norm <= config.tolerance * parameter_scale
                && improvement <= config.tolerance * objective.abs().max(1.0)
            {
                let diagnostics = StackerDiagnostics {
                    objective,
                    projected_gradient_norm,
                    iterations,
                    evaluations,
                    backtracking_steps: total_backtracking,
                    condition_estimate,
                    converged: true,
                    requested_tolerance: config.tolerance,
                    termination: "projected_gradient_and_objective",
                };
                return Ok(StackerFitOutcome::Converged(Box::new(build_model(
                    training,
                    config,
                    constraints,
                    normalization,
                    parameters,
                    diagnostics,
                ))));
            }
            next_step = (step * 1.25).min(config.initial_step);
        }
        Ok(StackerFitOutcome::NonConverged(StackerDiagnostics {
            objective,
            projected_gradient_norm,
            iterations,
            evaluations,
            backtracking_steps: total_backtracking,
            condition_estimate,
            converged: false,
            requested_tolerance: config.tolerance,
            termination: "iteration_limit",
        }))
    }

    pub fn predict(&self, input: &ModuleVectorInput) -> Result<StackerPrediction, EnsembleError> {
        if input.schema_hash != self.schema_hash
            || input.column_ids != self.column_ids
            || input.values.len() != self.column_ids.len()
        {
            return Err(EnsembleError::StackerSchemaMismatch);
        }
        require_nonzero_digest(&input.evidence_hash)?;
        let (logit, probability) = self.predict_values(&input.values)?;
        let evidence_hash = calculate_prediction_hash(self.model_id, input, logit, probability);
        Ok(StackerPrediction {
            logit,
            probability,
            model_id: self.model_id,
            input_evidence_hash: input.evidence_hash,
            evidence_hash,
        })
    }

    pub(crate) fn predict_values(
        &self,
        values: &[MatrixDatum],
    ) -> Result<(f64, f64), EnsembleError> {
        let (normalized, missing) = normalize_values(values, &self.normalization)?;
        let mut logit = self.intercept;
        for index in 0..self.weights.len() {
            logit = self.weights[index].mul_add(normalized[index], logit);
            logit = self.missing_weights[index].mul_add(missing[index], logit);
        }
        let probability = sigmoid(logit)?;
        Ok((logit, probability))
    }

    #[must_use]
    pub fn weights(&self) -> &[f64] {
        &self.weights
    }

    #[must_use]
    pub fn column_ids(&self) -> &[String] {
        &self.column_ids
    }

    #[must_use]
    pub const fn schema_hash(&self) -> [u8; 32] {
        self.schema_hash
    }

    #[must_use]
    pub const fn target_schema(&self) -> &StackerTargetSchema {
        &self.target_schema
    }

    #[must_use]
    pub const fn intercept(&self) -> f64 {
        self.intercept
    }

    #[must_use]
    pub const fn config(&self) -> &StackerConfig {
        &self.config
    }

    #[must_use]
    pub fn missingness_weights(&self) -> &[f64] {
        &self.missing_weights
    }

    #[must_use]
    pub fn column_weight(&self, column_id: &str) -> Option<f64> {
        self.column_ids
            .iter()
            .position(|candidate| candidate == column_id)
            .map(|index| self.weights[index])
    }

    #[must_use]
    pub fn module_weight(&self, module_kind: ModuleKind) -> Option<f64> {
        self.module_kinds
            .iter()
            .zip(&self.weights)
            .filter(|(candidate, _)| **candidate == module_kind)
            .map(|(_, weight)| *weight)
            .reduce(|sum, weight| sum + weight)
    }

    #[must_use]
    pub fn coefficient_l2_norm(&self) -> f64 {
        self.weights
            .iter()
            .chain(&self.missing_weights)
            .map(|value| value * value)
            .sum::<f64>()
            .sqrt()
    }

    #[must_use]
    pub const fn diagnostics(&self) -> &StackerDiagnostics {
        &self.diagnostics
    }

    #[must_use]
    pub const fn normalization(&self) -> &StackerNormalization {
        &self.normalization
    }

    #[must_use]
    pub const fn constraints(&self) -> &WeightConstraints {
        &self.constraints
    }

    #[must_use]
    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    #[must_use]
    pub const fn training_evidence_hash(&self) -> [u8; 32] {
        self.training_evidence_hash
    }
}

/// Exact ordered inference vector; missing values retain their reasoned variants.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleVectorInput {
    pub schema_hash: [u8; 32],
    pub column_ids: Vec<String>,
    pub values: Vec<MatrixDatum>,
    pub evidence_hash: [u8; 32],
}

/// Uncalibrated logistic prediction with deterministic lineage.
#[derive(Clone, Debug, PartialEq)]
pub struct StackerPrediction {
    logit: f64,
    probability: f64,
    model_id: [u8; 32],
    input_evidence_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl StackerPrediction {
    #[must_use]
    pub const fn logit(&self) -> f64 {
        self.logit
    }

    #[must_use]
    pub const fn probability(&self) -> f64 {
        self.probability
    }

    #[must_use]
    pub const fn model_id(&self) -> [u8; 32] {
        self.model_id
    }

    #[must_use]
    pub const fn input_evidence_hash(&self) -> [u8; 32] {
        self.input_evidence_hash
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct StackerScore {
    pub(crate) log_loss: f64,
    pub(crate) brier_score: f64,
}

pub(crate) fn score_model(
    model: &MetaStacker,
    evaluation: &StackerTrainingSet,
) -> Result<StackerScore, EnsembleError> {
    let mut log_loss = 0.0;
    let mut brier = 0.0;
    for example in &evaluation.examples {
        let (logit, probability) = model.predict_values(&example.values)?;
        log_loss += softplus(logit)? - example.outcome * logit;
        let residual = probability - example.outcome;
        brier = residual.mul_add(residual, brier);
    }
    let count = evaluation.examples.len() as f64;
    let score = StackerScore {
        log_loss: log_loss / count,
        brier_score: brier / count,
    };
    if score.log_loss.is_finite() && score.brier_score.is_finite() {
        Ok(score)
    } else {
        Err(EnsembleError::StackerOptimizationFailure)
    }
}

fn fit_normalization(
    training: &StackerTrainingSet,
    constraints: &WeightConstraints,
    policy: MissingValuePolicy,
) -> Result<StackerNormalization, EnsembleError> {
    let column_count = training.columns.len();
    let mut means = vec![0.0; column_count];
    let mut squared_deltas = vec![0.0; column_count];
    let mut counts = vec![0_u64; column_count];
    for example in &training.examples {
        for (index, datum) in example.values.iter().enumerate() {
            let MatrixDatum::Present(value) = datum else {
                continue;
            };
            counts[index] = counts[index]
                .checked_add(1)
                .ok_or(EnsembleError::StackerWorkCapacity)?;
            let number = value.value();
            let delta = number - means[index];
            means[index] += delta / counts[index] as f64;
            let next_delta = number - means[index];
            squared_deltas[index] = delta.mul_add(next_delta, squared_deltas[index]);
            if !means[index].is_finite() || !squared_deltas[index].is_finite() {
                return Err(EnsembleError::StackerOptimizationFailure);
            }
        }
    }
    let mut scales = Vec::with_capacity(column_count);
    let mut constant_columns = Vec::with_capacity(column_count);
    for index in 0..column_count {
        if counts[index] == 0 {
            if constraints.missingness_locked()[index] {
                means[index] = 0.0;
                scales.push(1.0);
                constant_columns.push(true);
                continue;
            }
            return Err(EnsembleError::UnsupportedMissingColumn);
        }
        let scale = (squared_deltas[index] / counts[index] as f64).sqrt();
        if !scale.is_finite() {
            return Err(EnsembleError::StackerOptimizationFailure);
        }
        let constant = scale <= NORMALIZATION_FLOOR;
        scales.push(if constant { 1.0 } else { scale });
        constant_columns.push(constant);
    }
    Ok(StackerNormalization {
        means,
        scales,
        observed_counts: counts,
        constant_columns,
        policy,
    })
}

fn normalize_training(
    training: &StackerTrainingSet,
    normalization: &StackerNormalization,
) -> Result<Vec<FitRow>, EnsembleError> {
    training
        .examples
        .iter()
        .map(|example| {
            let (values, missing) = normalize_values(&example.values, normalization)?;
            Ok(FitRow {
                values,
                missing,
                outcome: example.outcome,
            })
        })
        .collect()
}

fn normalize_values(
    values: &[MatrixDatum],
    normalization: &StackerNormalization,
) -> Result<(Vec<f64>, Vec<f64>), EnsembleError> {
    if values.len() != normalization.means.len() {
        return Err(EnsembleError::StackerSchemaMismatch);
    }
    let mut normalized = Vec::with_capacity(values.len());
    let mut missing = Vec::with_capacity(values.len());
    for (index, datum) in values.iter().enumerate() {
        match datum {
            MatrixDatum::Present(value) => {
                let transformed =
                    (value.value() - normalization.means[index]) / normalization.scales[index];
                if !transformed.is_finite() {
                    return Err(EnsembleError::InvalidStackerPrediction);
                }
                normalized.push(transformed);
                missing.push(0.0);
            }
            MatrixDatum::Missing(_) | MatrixDatum::ModuleMissing(_) => {
                normalized.push(0.0);
                missing.push(1.0);
            }
        }
    }
    Ok((normalized, missing))
}

fn design_condition_estimate(
    rows: &[FitRow],
    l2_penalty: f64,
    constraints: &WeightConstraints,
    budget: &mut WorkBudget,
) -> Result<f64, EnsembleError> {
    let mut coordinates = Vec::new();
    for index in 0..constraints.missingness_locked().len() {
        if constraints.is_locked(index) == Some(false) {
            coordinates.push((index, false));
        }
        if !constraints.missingness_locked()[index] {
            coordinates.push((index, true));
        }
    }
    if coordinates.is_empty() {
        return Ok(1.0);
    }
    budget.consume(condition_work_units(rows.len(), coordinates.len())?)?;
    let dimension = coordinates.len();
    let mut gram = DMatrix::zeros(dimension, dimension);
    for row in rows {
        let design = coordinates
            .iter()
            .map(|(index, missing)| {
                if *missing {
                    row.missing[*index]
                } else {
                    row.values[*index]
                }
            })
            .collect::<Vec<_>>();
        for left in 0..dimension {
            for right in left..dimension {
                let value = design[left].mul_add(design[right], gram[(left, right)]);
                gram[(left, right)] = value;
                gram[(right, left)] = value;
            }
        }
    }
    let count = rows.len() as f64;
    gram /= count;
    let active = (0..dimension)
        .filter(|index| gram[(*index, *index)] > NORMALIZATION_FLOOR)
        .collect::<Vec<_>>();
    if active.is_empty() {
        return Ok(1.0);
    }
    let mut regularized = DMatrix::zeros(active.len(), active.len());
    for (row_index, source_row) in active.iter().enumerate() {
        for (column_index, source_column) in active.iter().enumerate() {
            regularized[(row_index, column_index)] = gram[(*source_row, *source_column)];
        }
        regularized[(row_index, row_index)] += l2_penalty;
    }
    let eigenvalues = SymmetricEigen::new(regularized).eigenvalues;
    if eigenvalues.iter().any(|value| !value.is_finite()) {
        return Err(EnsembleError::StackerOptimizationFailure);
    }
    let minimum = eigenvalues.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = eigenvalues.iter().copied().fold(0.0_f64, f64::max);
    if maximum <= 0.0 {
        return Ok(1.0);
    }
    if minimum <= NORMALIZATION_FLOOR * maximum.max(1.0) {
        Ok(f64::INFINITY)
    } else {
        let condition = maximum / minimum;
        if condition.is_finite() && condition >= 1.0 {
            Ok(condition)
        } else {
            Err(EnsembleError::StackerOptimizationFailure)
        }
    }
}

fn objective_gradient(
    rows: &[FitRow],
    parameters: &Parameters,
    l2_penalty: f64,
    budget: &mut WorkBudget,
) -> Result<(f64, Parameters), EnsembleError> {
    budget.consume(gradient_work_units(rows.len(), parameters.weights.len())?)?;
    let mut loss = 0.0;
    let mut gradient = Parameters {
        weights: vec![0.0; parameters.weights.len()],
        missing_weights: vec![0.0; parameters.weights.len()],
        intercept: 0.0,
    };
    for row in rows {
        let logit = linear_predictor(parameters, row)?;
        loss += softplus(logit)? - row.outcome * logit;
        let residual = sigmoid(logit)? - row.outcome;
        for index in 0..parameters.weights.len() {
            gradient.weights[index] = residual.mul_add(row.values[index], gradient.weights[index]);
            gradient.missing_weights[index] =
                residual.mul_add(row.missing[index], gradient.missing_weights[index]);
        }
        gradient.intercept += residual;
    }
    let count = rows.len() as f64;
    loss /= count;
    gradient.intercept /= count;
    for index in 0..parameters.weights.len() {
        loss += 0.5
            * l2_penalty
            * (parameters.weights[index] * parameters.weights[index]
                + parameters.missing_weights[index] * parameters.missing_weights[index]);
        gradient.weights[index] =
            gradient.weights[index] / count + l2_penalty * parameters.weights[index];
        gradient.missing_weights[index] = gradient.missing_weights[index] / count
            + l2_penalty * parameters.missing_weights[index];
    }
    if loss.is_finite()
        && gradient.intercept.is_finite()
        && gradient.weights.iter().all(|value| value.is_finite())
        && gradient
            .missing_weights
            .iter()
            .all(|value| value.is_finite())
    {
        Ok((loss, gradient))
    } else {
        Err(EnsembleError::StackerOptimizationFailure)
    }
}

fn objective_only(
    rows: &[FitRow],
    parameters: &Parameters,
    l2_penalty: f64,
    budget: &mut WorkBudget,
) -> Result<f64, EnsembleError> {
    budget.consume(objective_work_units(rows.len(), parameters.weights.len())?)?;
    let mut loss = 0.0;
    for row in rows {
        let logit = linear_predictor(parameters, row)?;
        loss += softplus(logit)? - row.outcome * logit;
    }
    loss /= rows.len() as f64;
    loss += 0.5
        * l2_penalty
        * parameters
            .weights
            .iter()
            .chain(&parameters.missing_weights)
            .map(|value| value * value)
            .sum::<f64>();
    if loss.is_finite() {
        Ok(loss)
    } else {
        Err(EnsembleError::StackerOptimizationFailure)
    }
}

fn linear_predictor(parameters: &Parameters, row: &FitRow) -> Result<f64, EnsembleError> {
    let mut result = parameters.intercept;
    for index in 0..parameters.weights.len() {
        result = parameters.weights[index].mul_add(row.values[index], result);
        result = parameters.missing_weights[index].mul_add(row.missing[index], result);
    }
    if result.is_finite() {
        Ok(result)
    } else {
        Err(EnsembleError::StackerOptimizationFailure)
    }
}

fn parameter_delta_norm(candidate: &Parameters, current: &Parameters) -> f64 {
    candidate
        .weights
        .iter()
        .zip(&current.weights)
        .chain(
            candidate
                .missing_weights
                .iter()
                .zip(&current.missing_weights),
        )
        .map(|(left, right)| (left - right) * (left - right))
        .sum::<f64>()
        .mul_add(
            1.0,
            (candidate.intercept - current.intercept) * (candidate.intercept - current.intercept),
        )
        .sqrt()
}

fn projected_gradient_mapping_norm(
    parameters: &Parameters,
    gradient: &Parameters,
    constraints: &WeightConstraints,
    budget: &mut WorkBudget,
) -> Result<f64, EnsembleError> {
    let mut projected = Parameters {
        weights: parameters
            .weights
            .iter()
            .zip(&gradient.weights)
            .map(|(value, direction)| value - direction)
            .collect(),
        missing_weights: parameters
            .missing_weights
            .iter()
            .zip(&gradient.missing_weights)
            .map(|(value, direction)| value - direction)
            .collect(),
        intercept: parameters.intercept - gradient.intercept,
    };
    budget.consume(projection_work_units(projected.weights.len())?)?;
    constraints.project(&mut projected.weights)?;
    zero_locked_missing(
        &mut projected.missing_weights,
        constraints.missingness_locked(),
    );
    let norm = parameter_delta_norm(&projected, parameters);
    if norm.is_finite() {
        Ok(norm)
    } else {
        Err(EnsembleError::StackerOptimizationFailure)
    }
}

fn zero_locked_missing(weights: &mut [f64], locked: &[bool]) {
    for (weight, is_locked) in weights.iter_mut().zip(locked) {
        if *is_locked {
            *weight = 0.0;
        }
    }
}

fn sigmoid(logit: f64) -> Result<f64, EnsembleError> {
    if !logit.is_finite() {
        return Err(EnsembleError::StackerOptimizationFailure);
    }
    let probability = if logit >= 0.0 {
        1.0 / (1.0 + (-logit).exp())
    } else {
        let exponential = logit.exp();
        exponential / (1.0 + exponential)
    };
    if probability.is_finite() && (0.0..=1.0).contains(&probability) {
        Ok(probability)
    } else {
        Err(EnsembleError::StackerOptimizationFailure)
    }
}

fn softplus(value: f64) -> Result<f64, EnsembleError> {
    if !value.is_finite() {
        return Err(EnsembleError::StackerOptimizationFailure);
    }
    Ok(if value > 0.0 {
        value + (-value).exp().ln_1p()
    } else {
        value.exp().ln_1p()
    })
}

#[derive(Clone, Copy, Debug)]
struct WorkBudget {
    used: u64,
    limit: u64,
}

impl WorkBudget {
    const fn new(limit: u64) -> Self {
        Self { used: 0, limit }
    }

    fn consume(&mut self, units: u64) -> Result<(), EnsembleError> {
        self.used = self
            .used
            .checked_add(units)
            .ok_or(EnsembleError::StackerWorkCapacity)?;
        if self.used > self.limit {
            Err(EnsembleError::StackerWorkCapacity)
        } else {
            Ok(())
        }
    }
}

fn objective_work_units(rows: usize, columns: usize) -> Result<u64, EnsembleError> {
    let per_row = columns
        .checked_mul(2)
        .and_then(|value| value.checked_add(4))
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    let row_work = rows
        .checked_mul(per_row)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    let penalty_work = columns
        .checked_mul(2)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    u64::try_from(
        row_work
            .checked_add(penalty_work)
            .ok_or(EnsembleError::StackerWorkCapacity)?,
    )
    .map_err(|_| EnsembleError::StackerWorkCapacity)
}

fn gradient_work_units(rows: usize, columns: usize) -> Result<u64, EnsembleError> {
    let per_row = columns
        .checked_mul(6)
        .and_then(|value| value.checked_add(8))
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    let row_work = rows
        .checked_mul(per_row)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    let penalty_work = columns
        .checked_mul(6)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    u64::try_from(
        row_work
            .checked_add(penalty_work)
            .ok_or(EnsembleError::StackerWorkCapacity)?,
    )
    .map_err(|_| EnsembleError::StackerWorkCapacity)
}

fn preprocessing_work_units(rows: usize, columns: usize) -> Result<u64, EnsembleError> {
    let cells = rows
        .checked_mul(columns)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    let column_work = columns
        .checked_mul(8)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    u64::try_from(
        cells
            .checked_mul(8)
            .and_then(|value| value.checked_add(column_work))
            .ok_or(EnsembleError::StackerWorkCapacity)?,
    )
    .map_err(|_| EnsembleError::StackerWorkCapacity)
}

fn projection_work_units(columns: usize) -> Result<u64, EnsembleError> {
    u64::try_from(
        columns
            .checked_mul(columns)
            .and_then(|value| value.checked_add(columns))
            .ok_or(EnsembleError::StackerWorkCapacity)?,
    )
    .map_err(|_| EnsembleError::StackerWorkCapacity)
}

fn condition_work_units(rows: usize, dimension: usize) -> Result<u64, EnsembleError> {
    let square = dimension
        .checked_mul(dimension)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    let gram = rows
        .checked_mul(square)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    let eigen = square
        .checked_mul(dimension)
        .ok_or(EnsembleError::StackerWorkCapacity)?;
    u64::try_from(
        gram.checked_add(eigen)
            .ok_or(EnsembleError::StackerWorkCapacity)?,
    )
    .map_err(|_| EnsembleError::StackerWorkCapacity)
}

fn build_model(
    training: &StackerTrainingSet,
    config: StackerConfig,
    constraints: WeightConstraints,
    normalization: StackerNormalization,
    parameters: Parameters,
    diagnostics: StackerDiagnostics,
) -> MetaStacker {
    let column_ids = training
        .columns
        .iter()
        .map(|column| column.id().to_owned())
        .collect::<Vec<_>>();
    let module_kinds = training
        .columns
        .iter()
        .map(MatrixColumn::module_kind)
        .collect::<Vec<_>>();
    let model_id = calculate_model_hash(
        training,
        &config,
        &constraints,
        &normalization,
        &parameters,
        &diagnostics,
    );
    MetaStacker {
        column_ids,
        module_kinds,
        schema_hash: training.schema_hash,
        target_schema: training.target_schema.clone(),
        normalization,
        weights: parameters.weights,
        missing_weights: parameters.missing_weights,
        intercept: parameters.intercept,
        constraints,
        config,
        diagnostics,
        training_evidence_hash: training.evidence_hash,
        model_id,
    }
}

fn calculate_target_set_hash(
    matrix: &OutOfFoldMatrix,
    schema: &StackerTargetSchema,
    targets: &BTreeMap<u64, BinaryTarget>,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TARGET_SET_DOMAIN);
    hasher.update(&matrix.evidence_hash());
    hasher.update(&schema.evidence_hash);
    hash_u64(
        &mut hasher,
        u64::try_from(targets.len()).unwrap_or(u64::MAX),
    );
    for (row_id, target) in targets {
        hash_u64(&mut hasher, *row_id);
        match target.outcome {
            ObservedBinaryOutcome::Occurred { offset_seconds } => {
                hasher.update(&[1]);
                hash_u64(&mut hasher, offset_seconds);
            }
            ObservedBinaryOutcome::NotOccurred => {
                hasher.update(&[2]);
                hash_u64(&mut hasher, 0);
            }
        }
        hasher.update(&target.source_range_hash);
    }
    *hasher.finalize().as_bytes()
}

fn checked_horizon_end(origin_ns: i64, horizon_seconds: u64) -> Result<i64, EnsembleError> {
    let horizon_ns = i64::try_from(horizon_seconds)
        .ok()
        .and_then(|seconds| seconds.checked_mul(NANOS_PER_SECOND))
        .ok_or(EnsembleError::TargetMismatch)?;
    origin_ns
        .checked_add(horizon_ns)
        .ok_or(EnsembleError::TargetMismatch)
}

const fn event_type_identity_tag(event_type: EventType) -> u8 {
    match event_type {
        EventType::Downside => 1,
        EventType::Upside => 2,
        EventType::VolatilityExplosion => 3,
        EventType::LiquidityVacuum => 4,
        EventType::LiquidationCascade => 5,
    }
}

fn calculate_training_set_hash(
    folds: &[StackerFold<'_>],
    examples: &[StackerExample],
    schema_hash: [u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(TRAINING_SET_DOMAIN);
    hasher.update(&schema_hash);
    hash_u64(&mut hasher, u64::try_from(folds.len()).unwrap_or(u64::MAX));
    for fold in folds {
        hasher.update(&fold.matrix.outer_fold_hash());
        hasher.update(&fold.matrix.evidence_hash());
        hasher.update(&fold.targets.evidence_hash);
    }
    hash_u64(
        &mut hasher,
        u64::try_from(examples.len()).unwrap_or(u64::MAX),
    );
    for example in examples {
        hash_u64(&mut hasher, example.row_id);
        hasher.update(&example.origin_ns.to_le_bytes());
        hasher.update(&example.known_at_ns.to_le_bytes());
        hasher.update(&example.outcome.to_bits().to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn calculate_model_hash(
    training: &StackerTrainingSet,
    config: &StackerConfig,
    constraints: &WeightConstraints,
    normalization: &StackerNormalization,
    parameters: &Parameters,
    diagnostics: &StackerDiagnostics,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(MODEL_DOMAIN);
    hasher.update(&training.evidence_hash);
    hasher.update(&constraints.evidence_hash());
    hasher.update(&[config.weight_constraint.identity_tag()]);
    hasher.update(&[config.missing_value_policy.identity_tag()]);
    hasher.update(&config.l2_penalty.to_bits().to_le_bytes());
    hasher.update(&config.maximum_iterations.to_le_bytes());
    hasher.update(&config.tolerance.to_bits().to_le_bytes());
    hasher.update(&config.initial_step.to_bits().to_le_bytes());
    hasher.update(&config.minimum_step.to_bits().to_le_bytes());
    hasher.update(&config.maximum_backtracking_steps.to_le_bytes());
    hash_u64(&mut hasher, config.work_limit);
    for module in &config.additional_locked_modules {
        hasher.update(&[module.identity_tag()]);
    }
    for index in 0..parameters.weights.len() {
        hasher.update(&normalization.means[index].to_bits().to_le_bytes());
        hasher.update(&normalization.scales[index].to_bits().to_le_bytes());
        hash_u64(&mut hasher, normalization.observed_counts[index]);
        hasher.update(&[u8::from(normalization.constant_columns[index])]);
        hasher.update(&parameters.weights[index].to_bits().to_le_bytes());
        hasher.update(&parameters.missing_weights[index].to_bits().to_le_bytes());
    }
    hasher.update(&parameters.intercept.to_bits().to_le_bytes());
    hash_stacker_diagnostics(&mut hasher, diagnostics);
    *hasher.finalize().as_bytes()
}

fn hash_stacker_diagnostics(hasher: &mut blake3::Hasher, diagnostics: &StackerDiagnostics) {
    hasher.update(&diagnostics.objective.to_bits().to_le_bytes());
    hasher.update(&diagnostics.projected_gradient_norm.to_bits().to_le_bytes());
    hasher.update(&diagnostics.iterations.to_le_bytes());
    hash_u64(hasher, diagnostics.evaluations);
    hash_u64(hasher, diagnostics.backtracking_steps);
    hasher.update(&diagnostics.condition_estimate.to_bits().to_le_bytes());
    hasher.update(&[u8::from(diagnostics.converged)]);
    hasher.update(&diagnostics.requested_tolerance.to_bits().to_le_bytes());
    hash_string(hasher, diagnostics.termination);
}

fn calculate_prediction_hash(
    model_id: [u8; 32],
    input: &ModuleVectorInput,
    logit: f64,
    probability: f64,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PREDICTION_DOMAIN);
    hasher.update(&model_id);
    hasher.update(&input.schema_hash);
    hasher.update(&input.evidence_hash);
    hash_u64(
        &mut hasher,
        u64::try_from(input.column_ids.len()).unwrap_or(u64::MAX),
    );
    for column_id in &input.column_ids {
        hash_string(&mut hasher, column_id);
    }
    for value in &input.values {
        match value {
            MatrixDatum::Present(value) => {
                hash_module_datum(&mut hasher, &crate::ModuleDatum::Present(*value));
            }
            MatrixDatum::Missing(reason) => {
                hash_module_datum(&mut hasher, &crate::ModuleDatum::Missing(*reason));
            }
            MatrixDatum::ModuleMissing(reason) => {
                hasher.update(&[3, missingness_identity_tag(*reason)]);
            }
        }
    }
    hasher.update(&logit.to_bits().to_le_bytes());
    hasher.update(&probability.to_bits().to_le_bytes());
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use numerics::central_gradient;

    use super::{
        FitRow, Parameters, StackerDiagnostics, WorkBudget, hash_stacker_diagnostics,
        objective_gradient, objective_only, sigmoid, softplus,
    };

    #[test]
    fn stable_logistic_primitives_remain_finite_at_extreme_logits() {
        assert_eq!(sigmoid(1_000.0).expect("positive extreme"), 1.0);
        assert_eq!(sigmoid(-1_000.0).expect("negative extreme"), 0.0);
        assert!(softplus(1_000.0).expect("positive softplus").is_finite());
        assert!(softplus(-1_000.0).expect("negative softplus").is_finite());
    }

    #[test]
    fn analytic_regularized_gradient_matches_central_difference() {
        let rows = vec![
            FitRow {
                values: vec![-1.0, 0.25],
                missing: vec![0.0, 1.0],
                outcome: 0.0,
            },
            FitRow {
                values: vec![0.5, -0.75],
                missing: vec![1.0, 0.0],
                outcome: 1.0,
            },
            FitRow {
                values: vec![1.25, 0.5],
                missing: vec![0.0, 0.0],
                outcome: 1.0,
            },
        ];
        let point = vec![0.2, -0.3, 0.1, 0.4, -0.2];
        let l2_penalty = 0.17;
        let numerical = central_gradient(
            |candidate| {
                let parameters = Parameters {
                    weights: candidate[0..2].to_vec(),
                    missing_weights: candidate[2..4].to_vec(),
                    intercept: candidate[4],
                };
                objective_only(
                    &rows,
                    &parameters,
                    l2_penalty,
                    &mut WorkBudget::new(u64::MAX),
                )
                .expect("finite objective")
            },
            &point,
            1.0e-6,
        )
        .expect("central gradient");
        let parameters = Parameters {
            weights: point[0..2].to_vec(),
            missing_weights: point[2..4].to_vec(),
            intercept: point[4],
        };
        let (_, analytic) = objective_gradient(
            &rows,
            &parameters,
            l2_penalty,
            &mut WorkBudget::new(u64::MAX),
        )
        .expect("analytic gradient");
        let analytic = [
            analytic.weights[0],
            analytic.weights[1],
            analytic.missing_weights[0],
            analytic.missing_weights[1],
            analytic.intercept,
        ];
        for (expected, actual) in numerical.iter().zip(analytic) {
            assert!((expected - actual).abs() <= 1.0e-9);
        }
    }

    #[test]
    fn diagnostic_identity_changes_with_convergence_termination() {
        let diagnostics = StackerDiagnostics {
            objective: 0.25,
            projected_gradient_norm: 1.0e-9,
            iterations: 7,
            evaluations: 19,
            backtracking_steps: 3,
            condition_estimate: 4.0,
            converged: true,
            requested_tolerance: 1.0e-8,
            termination: "projected_gradient_and_objective",
        };
        let mut alternate = diagnostics.clone();
        alternate.termination = "projected_gradient_line_search_floor";
        let mut first = blake3::Hasher::new();
        let mut second = blake3::Hasher::new();
        hash_stacker_diagnostics(&mut first, &diagnostics);
        hash_stacker_diagnostics(&mut second, &alternate);
        assert_ne!(first.finalize(), second.finalize());
    }
}
