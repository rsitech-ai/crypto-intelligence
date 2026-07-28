//! Append-only catalog record, revision, correction, and bound types.

use std::num::{NonZeroU32, NonZeroU64, NonZeroUsize};

use domain::{InstrumentDefinition, UnixNanos};
use serde::Serialize;

use crate::RegistryError;

pub const MAXIMUM_CATALOG_RECORDS: usize = 1_000_000;
pub const MAXIMUM_DEFINITIONS: usize = 500_000;
pub const MAXIMUM_SYMBOL_KEYS: usize = 250_000;
pub const MAXIMUM_HISTORY_PER_SYMBOL: usize = 4_096;
pub const MAXIMUM_CORRECTIONS_PER_INSTRUMENT: usize = 1_024;
pub const MAXIMUM_BATCH_RECORDS: usize = 64;
pub const MAXIMUM_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
pub const MAXIMUM_SOURCE_REFERENCE_BYTES: usize = 256;
pub const MAXIMUM_CORRECTION_REASON_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CatalogRevision(NonZeroU64);

impl CatalogRevision {
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    pub(crate) fn next(previous: Option<Self>) -> Result<Self, RegistryError> {
        let value = previous.map_or(1, Self::get);
        let value = if previous.is_some() {
            value
                .checked_add(1)
                .ok_or(RegistryError::RevisionExhausted)?
        } else {
            value
        };
        Self::new(value).ok_or(RegistryError::RevisionExhausted)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RevisionMetadata {
    known_at: UnixNanos,
    source_reference: String,
}

impl RevisionMetadata {
    pub fn try_new(
        known_at: UnixNanos,
        source_reference: impl Into<String>,
    ) -> Result<Self, RegistryError> {
        let source_reference = source_reference.into();
        if !valid_bounded_text(&source_reference, MAXIMUM_SOURCE_REFERENCE_BYTES) {
            return Err(RegistryError::InvalidRevisionMetadata);
        }
        Ok(Self {
            known_at,
            source_reference,
        })
    }

    pub const fn known_at(&self) -> UnixNanos {
        self.known_at
    }

    pub fn source_reference(&self) -> &str {
        &self.source_reference
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistryLimitsInput {
    pub maximum_records: NonZeroUsize,
    pub maximum_definitions: NonZeroUsize,
    pub maximum_symbol_keys: NonZeroUsize,
    pub maximum_history_per_symbol: NonZeroUsize,
    pub maximum_corrections_per_instrument: NonZeroUsize,
    pub maximum_batch_records: NonZeroUsize,
    pub maximum_snapshot_bytes: NonZeroUsize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistryLimits {
    input: RegistryLimitsInput,
}

impl RegistryLimits {
    pub fn try_new(input: RegistryLimitsInput) -> Result<Self, RegistryError> {
        let records = input.maximum_records.get();
        let definitions = input.maximum_definitions.get();
        let symbols = input.maximum_symbol_keys.get();
        let history = input.maximum_history_per_symbol.get();
        let corrections = input.maximum_corrections_per_instrument.get();
        let batch = input.maximum_batch_records.get();
        let snapshot_bytes = input.maximum_snapshot_bytes.get();
        if records > MAXIMUM_CATALOG_RECORDS
            || definitions > MAXIMUM_DEFINITIONS
            || definitions > records
            || symbols > MAXIMUM_SYMBOL_KEYS
            || symbols > definitions
            || history > MAXIMUM_HISTORY_PER_SYMBOL
            || history > definitions
            || corrections > MAXIMUM_CORRECTIONS_PER_INSTRUMENT
            || corrections > records
            || batch > MAXIMUM_BATCH_RECORDS
            || batch > records
            || !(1_024..=MAXIMUM_SNAPSHOT_BYTES).contains(&snapshot_bytes)
        {
            return Err(RegistryError::InvalidLimits);
        }
        Ok(Self { input })
    }

    pub const fn maximum_records(self) -> usize {
        self.input.maximum_records.get()
    }

    pub const fn maximum_definitions(self) -> usize {
        self.input.maximum_definitions.get()
    }

    pub const fn maximum_symbol_keys(self) -> usize {
        self.input.maximum_symbol_keys.get()
    }

    pub const fn maximum_history_per_symbol(self) -> usize {
        self.input.maximum_history_per_symbol.get()
    }

    pub const fn maximum_corrections_per_instrument(self) -> usize {
        self.input.maximum_corrections_per_instrument.get()
    }

    pub const fn maximum_batch_records(self) -> usize {
        self.input.maximum_batch_records.get()
    }

    pub const fn maximum_snapshot_bytes(self) -> usize {
        self.input.maximum_snapshot_bytes.get()
    }
}

impl Default for RegistryLimits {
    fn default() -> Self {
        Self::try_new(RegistryLimitsInput {
            maximum_records: NonZeroUsize::new(100_000).expect("constant is nonzero"),
            maximum_definitions: NonZeroUsize::new(50_000).expect("constant is nonzero"),
            maximum_symbol_keys: NonZeroUsize::new(25_000).expect("constant is nonzero"),
            maximum_history_per_symbol: NonZeroUsize::new(1_024).expect("constant is nonzero"),
            maximum_corrections_per_instrument: NonZeroUsize::new(256)
                .expect("constant is nonzero"),
            maximum_batch_records: NonZeroUsize::new(64).expect("constant is nonzero"),
            maximum_snapshot_bytes: NonZeroUsize::new(64 * 1024 * 1024)
                .expect("constant is nonzero"),
        })
        .expect("default registry limits are valid")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorrectionInput {
    supersedes_revision: CatalogRevision,
    prior_definition_hash: [u8; 32],
    replacement: InstrumentDefinition,
    metadata: RevisionMetadata,
    reason: String,
}

impl CorrectionInput {
    pub fn try_new(
        supersedes_revision: CatalogRevision,
        prior_definition_hash: [u8; 32],
        replacement: InstrumentDefinition,
        metadata: RevisionMetadata,
        reason: impl Into<String>,
    ) -> Result<Self, RegistryError> {
        let reason = reason.into();
        if !valid_bounded_text(&reason, MAXIMUM_CORRECTION_REASON_BYTES) {
            return Err(RegistryError::InvalidCorrectionReason);
        }
        Ok(Self {
            supersedes_revision,
            prior_definition_hash,
            replacement,
            metadata,
            reason,
        })
    }

    pub const fn supersedes_revision(&self) -> CatalogRevision {
        self.supersedes_revision
    }

    pub const fn prior_definition_hash(&self) -> &[u8; 32] {
        &self.prior_definition_hash
    }

    pub const fn replacement(&self) -> &InstrumentDefinition {
        &self.replacement
    }

    pub const fn metadata(&self) -> &RevisionMetadata {
        &self.metadata
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogMutation {
    Definition {
        definition: InstrumentDefinition,
        metadata: RevisionMetadata,
    },
    Correction(CorrectionInput),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DefinitionRecord {
    catalog_revision: CatalogRevision,
    metadata: RevisionMetadata,
    definition_hash: [u8; 32],
    definition: InstrumentDefinition,
}

impl DefinitionRecord {
    pub(crate) const fn new(
        catalog_revision: CatalogRevision,
        metadata: RevisionMetadata,
        definition_hash: [u8; 32],
        definition: InstrumentDefinition,
    ) -> Self {
        Self {
            catalog_revision,
            metadata,
            definition_hash,
            definition,
        }
    }

    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }

    pub const fn metadata(&self) -> &RevisionMetadata {
        &self.metadata
    }

    pub const fn definition_hash(&self) -> &[u8; 32] {
        &self.definition_hash
    }

    pub const fn definition(&self) -> &InstrumentDefinition {
        &self.definition
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CorrectionRecord {
    catalog_revision: CatalogRevision,
    correction_version: NonZeroU32,
    supersedes_revision: CatalogRevision,
    metadata: RevisionMetadata,
    prior_definition_hash: [u8; 32],
    definition_hash: [u8; 32],
    correction_id: [u8; 32],
    reason: String,
    replacement: InstrumentDefinition,
}

impl CorrectionRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) const fn new(
        catalog_revision: CatalogRevision,
        correction_version: NonZeroU32,
        supersedes_revision: CatalogRevision,
        metadata: RevisionMetadata,
        prior_definition_hash: [u8; 32],
        definition_hash: [u8; 32],
        correction_id: [u8; 32],
        reason: String,
        replacement: InstrumentDefinition,
    ) -> Self {
        Self {
            catalog_revision,
            correction_version,
            supersedes_revision,
            metadata,
            prior_definition_hash,
            definition_hash,
            correction_id,
            reason,
            replacement,
        }
    }

    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }

    pub const fn correction_version(&self) -> NonZeroU32 {
        self.correction_version
    }

    pub const fn supersedes_revision(&self) -> CatalogRevision {
        self.supersedes_revision
    }

    pub const fn metadata(&self) -> &RevisionMetadata {
        &self.metadata
    }

    pub const fn prior_definition_hash(&self) -> &[u8; 32] {
        &self.prior_definition_hash
    }

    pub const fn definition_hash(&self) -> &[u8; 32] {
        &self.definition_hash
    }

    pub const fn correction_id(&self) -> &[u8; 32] {
        &self.correction_id
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub const fn replacement(&self) -> &InstrumentDefinition {
        &self.replacement
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum CatalogRecord {
    Definition(DefinitionRecord),
    Correction(CorrectionRecord),
}

impl CatalogRecord {
    pub const fn catalog_revision(&self) -> CatalogRevision {
        match self {
            Self::Definition(record) => record.catalog_revision(),
            Self::Correction(record) => record.catalog_revision(),
        }
    }

    pub const fn metadata(&self) -> &RevisionMetadata {
        match self {
            Self::Definition(record) => record.metadata(),
            Self::Correction(record) => record.metadata(),
        }
    }

    pub const fn definition(&self) -> &InstrumentDefinition {
        match self {
            Self::Definition(record) => record.definition(),
            Self::Correction(record) => record.replacement(),
        }
    }

    pub const fn definition_hash(&self) -> &[u8; 32] {
        match self {
            Self::Definition(record) => record.definition_hash(),
            Self::Correction(record) => record.definition_hash(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendOutcome {
    Appended { revision: CatalogRevision },
    AlreadyPresent { revision: CatalogRevision },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BatchOutcome {
    previous_revision: Option<CatalogRevision>,
    committed_revision: Option<CatalogRevision>,
    appended_records: usize,
    idempotent_records: usize,
}

impl BatchOutcome {
    pub(crate) const fn new(
        previous_revision: Option<CatalogRevision>,
        committed_revision: Option<CatalogRevision>,
        appended_records: usize,
        idempotent_records: usize,
    ) -> Self {
        Self {
            previous_revision,
            committed_revision,
            appended_records,
            idempotent_records,
        }
    }

    pub const fn previous_revision(self) -> Option<CatalogRevision> {
        self.previous_revision
    }

    pub const fn committed_revision(self) -> Option<CatalogRevision> {
        self.committed_revision
    }

    pub const fn appended_records(self) -> usize {
        self.appended_records
    }

    pub const fn idempotent_records(self) -> usize {
        self.idempotent_records
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DefinitionRevision {
    revision: CatalogRevision,
    definition_hash: [u8; 32],
}

impl DefinitionRevision {
    pub(crate) const fn new(revision: CatalogRevision, definition_hash: [u8; 32]) -> Self {
        Self {
            revision,
            definition_hash,
        }
    }

    pub const fn revision(self) -> CatalogRevision {
        self.revision
    }

    pub const fn definition_hash(&self) -> &[u8; 32] {
        &self.definition_hash
    }
}

pub(crate) fn valid_bounded_text(value: &str, maximum_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum_bytes
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
