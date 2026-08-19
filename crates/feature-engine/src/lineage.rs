//! Canonical lineage DAG identity for materialized feature observations.

use feature_registry::{CodeRevision, FeatureId, FormulaHash, SourceCoverage};
use semver::Version;
use serde::Serialize;

const MAX_INPUT_EVENT_HASHES: usize = 4_096;
const SOURCE_COVERAGE_DOMAIN: &[u8] = b"cmti:feature-source-coverage:v1\0";
const FEATURE_LINEAGE_DOMAIN: &[u8] = b"cmti:feature-lineage:v1\0";

/// Complete validated inputs required to derive one feature lineage identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureLineageInput {
    pub feature_id: FeatureId,
    pub feature_version: Version,
    pub formula_hash: FormulaHash,
    pub input_event_hashes: Vec<[u8; 32]>,
    pub source_coverage: SourceCoverage,
    pub code_commit: CodeRevision,
}

/// Canonically ordered, caller-independent feature lineage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FeatureLineage {
    feature_id: FeatureId,
    feature_version: Version,
    formula_hash: FormulaHash,
    input_event_hashes: Vec<[u8; 32]>,
    source_coverage_hash: [u8; 32],
    code_commit: CodeRevision,
}

impl FeatureLineage {
    pub fn try_new(mut input: FeatureLineageInput) -> Result<Self, LineageError> {
        if input.feature_version.major == 0
            || !input.feature_version.pre.is_empty()
            || !input.feature_version.build.is_empty()
            || input.input_event_hashes.is_empty()
            || input.input_event_hashes.len() > MAX_INPUT_EVENT_HASHES
            || input.input_event_hashes.contains(&[0; 32])
        {
            return Err(LineageError::InvalidInput);
        }
        input.input_event_hashes.sort_unstable();
        if input
            .input_event_hashes
            .windows(2)
            .any(|pair| pair[0] == pair[1])
        {
            return Err(LineageError::DuplicateInputEvent);
        }

        let source_coverage_bytes =
            serde_json::to_vec(&input.source_coverage).map_err(|_| LineageError::Serialization)?;
        let mut source_hasher = blake3::Hasher::new();
        source_hasher.update(SOURCE_COVERAGE_DOMAIN);
        source_hasher.update(&source_coverage_bytes);

        Ok(Self {
            feature_id: input.feature_id,
            feature_version: input.feature_version,
            formula_hash: input.formula_hash,
            input_event_hashes: input.input_event_hashes,
            source_coverage_hash: *source_hasher.finalize().as_bytes(),
            code_commit: input.code_commit,
        })
    }

    pub fn feature_id(&self) -> &FeatureId {
        &self.feature_id
    }

    pub const fn feature_version(&self) -> &Version {
        &self.feature_version
    }

    pub const fn formula_hash(&self) -> FormulaHash {
        self.formula_hash
    }

    pub fn input_event_hashes(&self) -> &[[u8; 32]] {
        &self.input_event_hashes
    }

    pub const fn source_coverage_hash(&self) -> [u8; 32] {
        self.source_coverage_hash
    }

    pub fn code_commit(&self) -> &CodeRevision {
        &self.code_commit
    }

    pub fn digest(&self) -> Result<[u8; 32], LineageError> {
        let bytes = serde_json::to_vec(self).map_err(|_| LineageError::Serialization)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(FEATURE_LINEAGE_DOMAIN);
        hasher.update(&bytes);
        Ok(*hasher.finalize().as_bytes())
    }
}

/// Fail-closed lineage construction error.
#[derive(Clone, Copy, Debug, Eq, thiserror::Error, PartialEq)]
pub enum LineageError {
    #[error("feature lineage input is invalid or exceeds its bound")]
    InvalidInput,
    #[error("feature lineage contains a duplicate input event")]
    DuplicateInputEvent,
    #[error("feature lineage could not be serialized canonically")]
    Serialization,
}
