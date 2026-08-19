//! Production-weight constraints derived independently from runtime availability.

use std::collections::BTreeSet;

use cusp::ablation::{GateDecision, GateEvaluation};

use crate::{MatrixColumn, ModuleKind, hash_string, hash_u64};

const CONSTRAINT_DOMAIN: &[u8] = b"cmti:ensemble-matrix-constraints:v1\0";
const WEIGHT_CONSTRAINT_DOMAIN: &[u8] = b"cmti:stacker-weight-constraints:v1\0";

/// Explicit coefficient geometry for the initial logistic stacker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WeightConstraint {
    Unconstrained,
    NonNegative,
    Simplex,
}

impl WeightConstraint {
    pub(crate) const fn identity_tag(self) -> u8 {
        match self {
            Self::Unconstrained => 1,
            Self::NonNegative => 2,
            Self::Simplex => 3,
        }
    }
}

/// Approved deterministic treatment for reasoned missing module values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MissingValuePolicy {
    /// Learn each column mean on training folds and map missing values to that mean.
    TrainingMean,
}

impl MissingValuePolicy {
    pub(crate) const fn identity_tag(self) -> u8 {
        match self {
            Self::TrainingMean => 1,
        }
    }
}

/// Opaque evidence controlling whether Cusp columns may receive production weight.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CuspEligibilityReceipt {
    eligible: bool,
    gate_schema_version: u32,
    candidate_model_hash: [u8; 32],
    gate_evidence_hash: [u8; 32],
}

impl CuspEligibilityReceipt {
    /// Safe default when no validated Cusp ablation gate has been supplied.
    #[must_use]
    pub const fn locked_without_gate() -> Self {
        Self {
            eligible: false,
            gate_schema_version: 0,
            candidate_model_hash: [0; 32],
            gate_evidence_hash: [0; 32],
        }
    }

    #[must_use]
    pub const fn is_eligible(self) -> bool {
        self.eligible
    }

    #[must_use]
    pub const fn gate_schema_version(self) -> u32 {
        self.gate_schema_version
    }

    /// Exact Cusp `ModuleOutput::model_package_hash` evaluated by the gate.
    #[must_use]
    pub const fn candidate_model_hash(self) -> [u8; 32] {
        self.candidate_model_hash
    }

    #[must_use]
    pub const fn gate_evidence_hash(self) -> [u8; 32] {
        self.gate_evidence_hash
    }
}

impl From<&GateEvaluation> for CuspEligibilityReceipt {
    fn from(evaluation: &GateEvaluation) -> Self {
        Self {
            eligible: evaluation.decision() == GateDecision::EligibleForProductionWeight,
            gate_schema_version: evaluation.schema_version(),
            candidate_model_hash: evaluation.candidate_model_hash(),
            gate_evidence_hash: evaluation.evidence_hash(),
        }
    }
}

/// Exact matrix columns whose production weights must remain zero.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatrixConstraints {
    locked_columns: BTreeSet<String>,
    cusp_receipt: CuspEligibilityReceipt,
    evidence_hash: [u8; 32],
}

impl MatrixConstraints {
    pub(crate) fn new(columns: &[MatrixColumn], cusp_receipt: CuspEligibilityReceipt) -> Self {
        let locked_columns = columns
            .iter()
            .filter(|column| {
                column.module_kind() == ModuleKind::Cusp && !cusp_receipt.is_eligible()
            })
            .map(|column| column.id().to_owned())
            .collect();
        let evidence_hash = calculate_constraints_hash(columns, &locked_columns, cusp_receipt);
        Self {
            locked_columns,
            cusp_receipt,
            evidence_hash,
        }
    }

    #[must_use]
    pub fn is_weight_locked(&self, column_id: &str) -> bool {
        self.locked_columns.contains(column_id)
    }

    #[must_use]
    pub const fn locked_columns(&self) -> &BTreeSet<String> {
        &self.locked_columns
    }

    #[must_use]
    pub const fn cusp_receipt(&self) -> CuspEligibilityReceipt {
        self.cusp_receipt
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

/// Validated coefficient locks and projection policy retained by a fitted model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WeightConstraints {
    mode: WeightConstraint,
    primary_locked: Vec<bool>,
    missingness_locked: Vec<bool>,
    evidence_hash: [u8; 32],
}

impl WeightConstraints {
    pub(crate) fn derive(
        columns: &[MatrixColumn],
        matrix_constraints: &MatrixConstraints,
        mode: WeightConstraint,
        additional_locked_modules: &BTreeSet<ModuleKind>,
    ) -> Result<Self, crate::EnsembleError> {
        let locked = columns
            .iter()
            .map(|column| {
                matrix_constraints.is_weight_locked(column.id())
                    || additional_locked_modules.contains(&column.module_kind())
            })
            .collect::<Vec<_>>();
        if mode == WeightConstraint::Simplex && locked.iter().all(|is_locked| *is_locked) {
            return Err(crate::EnsembleError::InvalidWeightConstraints);
        }
        let evidence_hash = calculate_weight_constraints_hash(columns, mode, &locked, &locked);
        Ok(Self {
            mode,
            primary_locked: locked.clone(),
            missingness_locked: locked,
            evidence_hash,
        })
    }

    pub(crate) fn lock_constant_primary_columns(
        &mut self,
        columns: &[MatrixColumn],
        constant_columns: &[bool],
    ) -> Result<(), crate::EnsembleError> {
        if columns.len() != self.primary_locked.len()
            || constant_columns.len() != self.primary_locked.len()
        {
            return Err(crate::EnsembleError::InvalidWeightConstraints);
        }
        for (locked, constant) in self.primary_locked.iter_mut().zip(constant_columns) {
            *locked |= *constant;
        }
        if self.mode == WeightConstraint::Simplex
            && self.primary_locked.iter().all(|is_locked| *is_locked)
        {
            return Err(crate::EnsembleError::InvalidWeightConstraints);
        }
        self.evidence_hash = calculate_weight_constraints_hash(
            columns,
            self.mode,
            &self.primary_locked,
            &self.missingness_locked,
        );
        Ok(())
    }

    #[must_use]
    pub const fn mode(&self) -> WeightConstraint {
        self.mode
    }

    #[must_use]
    pub fn is_locked(&self, column_index: usize) -> Option<bool> {
        self.primary_locked.get(column_index).copied()
    }

    #[must_use]
    pub fn is_missingness_locked(&self, column_index: usize) -> Option<bool> {
        self.missingness_locked.get(column_index).copied()
    }

    #[must_use]
    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub(crate) fn missingness_locked(&self) -> &[bool] {
        &self.missingness_locked
    }

    pub(crate) fn project(&self, weights: &mut [f64]) -> Result<(), crate::EnsembleError> {
        if weights.len() != self.primary_locked.len()
            || weights.iter().any(|value| !value.is_finite())
        {
            return Err(crate::EnsembleError::InvalidWeightConstraints);
        }
        match self.mode {
            WeightConstraint::Unconstrained => {
                zero_locked(weights, &self.primary_locked);
            }
            WeightConstraint::NonNegative => {
                for (weight, locked) in weights.iter_mut().zip(&self.primary_locked) {
                    *weight = if *locked { 0.0 } else { weight.max(0.0) };
                }
            }
            WeightConstraint::Simplex => project_simplex(weights, &self.primary_locked)?,
        }
        if weights.iter().any(|value| !value.is_finite())
            || weights
                .iter()
                .zip(&self.primary_locked)
                .any(|(weight, locked)| *locked && weight.to_bits() != 0.0_f64.to_bits())
        {
            return Err(crate::EnsembleError::InvalidWeightConstraints);
        }
        Ok(())
    }
}

fn zero_locked(weights: &mut [f64], locked: &[bool]) {
    for (weight, locked) in weights.iter_mut().zip(locked) {
        if *locked {
            *weight = 0.0;
        }
    }
}

fn project_simplex(weights: &mut [f64], locked: &[bool]) -> Result<(), crate::EnsembleError> {
    let mut active = weights
        .iter()
        .zip(locked)
        .enumerate()
        .filter_map(|(index, (weight, is_locked))| (!*is_locked).then_some((index, *weight)))
        .collect::<Vec<_>>();
    if active.is_empty() {
        return Err(crate::EnsembleError::InvalidWeightConstraints);
    }
    active.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut cumulative = 0.0;
    let mut rho = None;
    for (rank, (_, value)) in active.iter().enumerate() {
        cumulative += *value;
        let denominator = (rank + 1) as f64;
        let theta = (cumulative - 1.0) / denominator;
        if *value > theta {
            rho = Some((rank + 1, cumulative));
        }
    }
    let (count, selected_sum) = rho.ok_or(crate::EnsembleError::InvalidWeightConstraints)?;
    let theta = (selected_sum - 1.0) / count as f64;
    let mut projected_sum = 0.0;
    for (weight, is_locked) in weights.iter_mut().zip(locked) {
        if *is_locked {
            *weight = 0.0;
        } else {
            *weight = (*weight - theta).max(0.0);
            projected_sum += *weight;
        }
    }
    if !projected_sum.is_finite() || projected_sum <= 0.0 {
        return Err(crate::EnsembleError::InvalidWeightConstraints);
    }
    let residual = 1.0 - projected_sum;
    if residual.abs() > 1.0e-12 {
        return Err(crate::EnsembleError::InvalidWeightConstraints);
    }
    let correction_index = weights
        .iter()
        .zip(locked)
        .enumerate()
        .filter(|(_, (_, is_locked))| !**is_locked)
        .max_by(|left, right| {
            left.1
                .0
                .total_cmp(right.1.0)
                .then_with(|| right.0.cmp(&left.0))
        })
        .map(|(index, _)| index)
        .ok_or(crate::EnsembleError::InvalidWeightConstraints)?;
    weights[correction_index] += residual;
    if weights[correction_index] < 0.0 || (weights.iter().sum::<f64>() - 1.0).abs() > 1.0e-12 {
        return Err(crate::EnsembleError::InvalidWeightConstraints);
    }
    Ok(())
}

fn calculate_weight_constraints_hash(
    columns: &[MatrixColumn],
    mode: WeightConstraint,
    primary_locked: &[bool],
    missingness_locked: &[bool],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(WEIGHT_CONSTRAINT_DOMAIN);
    hasher.update(&[mode.identity_tag()]);
    hash_u64(
        &mut hasher,
        u64::try_from(columns.len()).unwrap_or(u64::MAX),
    );
    for ((column, primary_locked), missingness_locked) in
        columns.iter().zip(primary_locked).zip(missingness_locked)
    {
        hasher.update(&[column.module_kind().identity_tag()]);
        hash_string(&mut hasher, column.id());
        hasher.update(&[u8::from(*primary_locked), u8::from(*missingness_locked)]);
    }
    *hasher.finalize().as_bytes()
}

fn calculate_constraints_hash(
    columns: &[MatrixColumn],
    locked_columns: &BTreeSet<String>,
    receipt: CuspEligibilityReceipt,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CONSTRAINT_DOMAIN);
    hasher.update(&[u8::from(receipt.eligible)]);
    hasher.update(&receipt.gate_schema_version.to_le_bytes());
    hasher.update(&receipt.candidate_model_hash);
    hasher.update(&receipt.gate_evidence_hash);
    hash_u64(
        &mut hasher,
        u64::try_from(columns.len()).unwrap_or(u64::MAX),
    );
    for column in columns {
        hash_string(&mut hasher, column.id());
        hasher.update(&[column.module_kind().identity_tag()]);
        hasher.update(&[u8::from(locked_columns.contains(column.id()))]);
    }
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use feature_registry::FeatureId;

    use super::{CuspEligibilityReceipt, MatrixConstraints, WeightConstraint, WeightConstraints};
    use crate::{EnsembleError, MatrixColumn, ModuleKind};

    fn columns() -> Vec<MatrixColumn> {
        vec![
            MatrixColumn::new(
                ModuleKind::Hazard,
                FeatureId::new("hazard").expect("feature ID"),
            ),
            MatrixColumn::new(
                ModuleKind::Volatility,
                FeatureId::new("volatility").expect("feature ID"),
            ),
            MatrixColumn::new(
                ModuleKind::Cusp,
                FeatureId::new("cusp").expect("feature ID"),
            ),
        ]
    }

    #[test]
    fn simplex_projection_is_deterministic_and_keeps_locked_coefficients_exactly_zero() {
        let columns = columns();
        let matrix =
            MatrixConstraints::new(&columns, CuspEligibilityReceipt::locked_without_gate());
        let constraints = WeightConstraints::derive(
            &columns,
            &matrix,
            WeightConstraint::Simplex,
            &BTreeSet::new(),
        )
        .expect("valid simplex constraints");
        let mut first = vec![-2.0, 4.0, 99.0];
        let mut second = first.clone();
        constraints.project(&mut first).expect("first projection");
        constraints.project(&mut second).expect("repeat projection");
        assert_eq!(first, second);
        let projected = first.clone();
        constraints
            .project(&mut first)
            .expect("idempotent projection");
        assert_eq!(first, projected);
        assert_eq!(first[2].to_bits(), 0.0_f64.to_bits());
        assert!(first.iter().all(|weight| *weight >= 0.0));
        assert!((first.iter().sum::<f64>() - 1.0).abs() <= f64::EPSILON);

        let mut tie = vec![0.5, 0.5, -1.0];
        constraints.project(&mut tie).expect("tied projection");
        assert_eq!(tie, vec![0.5, 0.5, 0.0]);
    }

    #[test]
    fn projections_reject_nonfinite_or_misaligned_vectors_and_all_locked_simplex() {
        let columns = columns();
        let matrix =
            MatrixConstraints::new(&columns, CuspEligibilityReceipt::locked_without_gate());
        let constraints = WeightConstraints::derive(
            &columns,
            &matrix,
            WeightConstraint::NonNegative,
            &BTreeSet::new(),
        )
        .expect("valid nonnegative constraints");
        assert_eq!(
            constraints.project(&mut [1.0, f64::NAN, 0.0]),
            Err(EnsembleError::InvalidWeightConstraints)
        );
        assert_eq!(
            constraints.project(&mut [1.0, 0.0]),
            Err(EnsembleError::InvalidWeightConstraints)
        );

        let all_modules = columns
            .iter()
            .map(MatrixColumn::module_kind)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            WeightConstraints::derive(&columns, &matrix, WeightConstraint::Simplex, &all_modules,),
            Err(EnsembleError::InvalidWeightConstraints)
        );
    }
}
