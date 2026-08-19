//! Untouched-fold module-removal ablations for the initial meta-stacker.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    EnsembleError, MetaStacker, ModuleKind, StackerConfig, StackerTrainingSet, hash_string,
    hash_u64, stacker::score_model,
};

const ABLATION_DOMAIN: &[u8] = b"cmti:meta-stacker-module-ablation:v1\0";

/// Whether a module was refit away or was already ineligible in the full model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AblationStatus {
    Evaluated,
    AlreadyLocked,
}

impl AblationStatus {
    const fn identity_tag(self) -> u8 {
        match self {
            Self::Evaluated => 1,
            Self::AlreadyLocked => 2,
        }
    }
}

/// Untouched-fold metric comparison after removing every column for one module.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleAblation {
    module_kind: ModuleKind,
    status: AblationStatus,
    full_model_id: [u8; 32],
    ablated_model_id: [u8; 32],
    full_log_loss: f64,
    ablated_log_loss: f64,
    full_brier_score: f64,
    ablated_brier_score: f64,
    removed_columns: Vec<AblatedColumn>,
}

/// Exact post-refit coefficients for one removed module column.
#[derive(Clone, Debug, PartialEq)]
pub struct AblatedColumn {
    column_id: String,
    primary_weight: f64,
    missingness_weight: f64,
}

impl AblatedColumn {
    #[must_use]
    pub fn column_id(&self) -> &str {
        &self.column_id
    }

    #[must_use]
    pub const fn primary_weight(&self) -> f64 {
        self.primary_weight
    }

    #[must_use]
    pub const fn missingness_weight(&self) -> f64 {
        self.missingness_weight
    }
}

impl ModuleAblation {
    #[must_use]
    pub const fn module_kind(&self) -> ModuleKind {
        self.module_kind
    }

    #[must_use]
    pub const fn status(&self) -> AblationStatus {
        self.status
    }

    #[must_use]
    pub const fn full_model_id(&self) -> [u8; 32] {
        self.full_model_id
    }

    #[must_use]
    pub const fn ablated_model_id(&self) -> [u8; 32] {
        self.ablated_model_id
    }

    #[must_use]
    pub const fn full_log_loss(&self) -> f64 {
        self.full_log_loss
    }

    #[must_use]
    pub const fn ablated_log_loss(&self) -> f64 {
        self.ablated_log_loss
    }

    #[must_use]
    pub fn log_loss_delta(&self) -> f64 {
        self.ablated_log_loss - self.full_log_loss
    }

    #[must_use]
    pub const fn full_brier_score(&self) -> f64 {
        self.full_brier_score
    }

    #[must_use]
    pub const fn ablated_brier_score(&self) -> f64 {
        self.ablated_brier_score
    }

    #[must_use]
    pub fn brier_score_delta(&self) -> f64 {
        self.ablated_brier_score - self.full_brier_score
    }

    #[must_use]
    pub fn removed_columns(&self) -> &[AblatedColumn] {
        &self.removed_columns
    }
}

/// Canonical full-versus-module-removed evaluation on disjoint OOF folds.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleAblationReport {
    training_evidence_hash: [u8; 32],
    evaluation_evidence_hash: [u8; 32],
    full_model_id: [u8; 32],
    modules: BTreeMap<ModuleKind, ModuleAblation>,
    evidence_hash: [u8; 32],
}

impl ModuleAblationReport {
    pub fn run(
        training: &StackerTrainingSet,
        evaluation: &StackerTrainingSet,
        config: StackerConfig,
    ) -> Result<Self, EnsembleError> {
        validate_ablation_sets(training, evaluation)?;
        let full = MetaStacker::fit(training, config.clone())?;
        let full_score = score_model(&full, evaluation)?;
        let module_kinds = training
            .columns()
            .iter()
            .map(crate::MatrixColumn::module_kind)
            .collect::<BTreeSet<_>>();
        let mut modules = BTreeMap::new();
        for module_kind in &module_kinds {
            let already_locked = config.is_additionally_locked(*module_kind)
                || training
                    .columns()
                    .iter()
                    .filter(|column| column.module_kind() == *module_kind)
                    .all(|column| training.matrix_constraints().is_weight_locked(column.id()));
            let (status, ablated_model_id, ablated_score, removed_columns) = if already_locked {
                (
                    AblationStatus::AlreadyLocked,
                    full.model_id(),
                    full_score,
                    removed_columns(&full, training, *module_kind),
                )
            } else {
                let reduced =
                    MetaStacker::fit(training, config.with_additional_lock(*module_kind))?;
                let score = score_model(&reduced, evaluation)?;
                let removed_columns = removed_columns(&reduced, training, *module_kind);
                (
                    AblationStatus::Evaluated,
                    reduced.model_id(),
                    score,
                    removed_columns,
                )
            };
            modules.insert(
                *module_kind,
                ModuleAblation {
                    module_kind: *module_kind,
                    status,
                    full_model_id: full.model_id(),
                    ablated_model_id,
                    full_log_loss: full_score.log_loss,
                    ablated_log_loss: ablated_score.log_loss,
                    full_brier_score: full_score.brier_score,
                    ablated_brier_score: ablated_score.brier_score,
                    removed_columns,
                },
            );
        }
        let evidence_hash = calculate_ablation_hash(
            training.evidence_hash(),
            evaluation.evidence_hash(),
            full.model_id(),
            &modules,
        );
        Ok(Self {
            training_evidence_hash: training.evidence_hash(),
            evaluation_evidence_hash: evaluation.evidence_hash(),
            full_model_id: full.model_id(),
            modules,
            evidence_hash,
        })
    }

    #[must_use]
    pub fn module(&self, module_kind: ModuleKind) -> Option<&ModuleAblation> {
        self.modules.get(&module_kind)
    }

    #[must_use]
    pub const fn training_evidence_hash(&self) -> [u8; 32] {
        self.training_evidence_hash
    }

    #[must_use]
    pub const fn evaluation_evidence_hash(&self) -> [u8; 32] {
        self.evaluation_evidence_hash
    }

    #[must_use]
    pub const fn full_model_id(&self) -> [u8; 32] {
        self.full_model_id
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

fn removed_columns(
    model: &MetaStacker,
    training: &StackerTrainingSet,
    module_kind: ModuleKind,
) -> Vec<AblatedColumn> {
    training
        .columns()
        .iter()
        .enumerate()
        .filter(|(_, column)| column.module_kind() == module_kind)
        .map(|(index, column)| AblatedColumn {
            column_id: column.id().to_owned(),
            primary_weight: model.weights()[index],
            missingness_weight: model.missingness_weights()[index],
        })
        .collect()
}

fn validate_ablation_sets(
    training: &StackerTrainingSet,
    evaluation: &StackerTrainingSet,
) -> Result<(), EnsembleError> {
    if training.schema_hash() != evaluation.schema_hash()
        || training.columns() != evaluation.columns()
        || training.target_schema() != evaluation.target_schema()
        || training.matrix_constraints().locked_columns()
            != evaluation.matrix_constraints().locked_columns()
    {
        return Err(EnsembleError::AblationIncompatible);
    }
    let training_folds = training.fold_hashes().iter().collect::<BTreeSet<_>>();
    if evaluation
        .fold_hashes()
        .iter()
        .any(|fold_hash| training_folds.contains(fold_hash))
    {
        return Err(EnsembleError::AblationOverlap);
    }
    let training_rows = training
        .examples()
        .iter()
        .map(|example| example.row_id)
        .collect::<BTreeSet<_>>();
    if evaluation
        .examples()
        .iter()
        .any(|example| training_rows.contains(&example.row_id))
    {
        return Err(EnsembleError::AblationOverlap);
    }
    let latest_training_knowledge = training
        .examples()
        .iter()
        .map(|example| example.known_at_ns)
        .max()
        .ok_or(EnsembleError::AblationIncompatible)?;
    let earliest_evaluation_origin = evaluation
        .examples()
        .iter()
        .map(|example| example.origin_ns)
        .min()
        .ok_or(EnsembleError::AblationIncompatible)?;
    if earliest_evaluation_origin < latest_training_knowledge {
        return Err(EnsembleError::AblationChronology);
    }
    Ok(())
}

fn calculate_ablation_hash(
    training_hash: [u8; 32],
    evaluation_hash: [u8; 32],
    full_model_id: [u8; 32],
    modules: &BTreeMap<ModuleKind, ModuleAblation>,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ABLATION_DOMAIN);
    hasher.update(&training_hash);
    hasher.update(&evaluation_hash);
    hasher.update(&full_model_id);
    hash_u64(
        &mut hasher,
        u64::try_from(modules.len()).unwrap_or(u64::MAX),
    );
    for (module_kind, ablation) in modules {
        hasher.update(&[module_kind.identity_tag(), ablation.status.identity_tag()]);
        hasher.update(&ablation.full_model_id);
        hasher.update(&ablation.ablated_model_id);
        hasher.update(&ablation.full_log_loss.to_bits().to_le_bytes());
        hasher.update(&ablation.ablated_log_loss.to_bits().to_le_bytes());
        hasher.update(&ablation.full_brier_score.to_bits().to_le_bytes());
        hasher.update(&ablation.ablated_brier_score.to_bits().to_le_bytes());
        hash_u64(
            &mut hasher,
            u64::try_from(ablation.removed_columns.len()).unwrap_or(u64::MAX),
        );
        for column in &ablation.removed_columns {
            hash_string(&mut hasher, &column.column_id);
            hasher.update(&column.primary_weight.to_bits().to_le_bytes());
            hasher.update(&column.missingness_weight.to_bits().to_le_bytes());
        }
        hash_string(&mut hasher, module_kind.as_str());
    }
    *hasher.finalize().as_bytes()
}
