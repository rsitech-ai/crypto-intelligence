//! Production-weight constraints derived independently from runtime availability.

use std::collections::BTreeSet;

use cusp::ablation::{GateDecision, GateEvaluation};

use crate::{MatrixColumn, ModuleKind, hash_string, hash_u64};

const CONSTRAINT_DOMAIN: &[u8] = b"cmti:ensemble-matrix-constraints:v1\0";

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
