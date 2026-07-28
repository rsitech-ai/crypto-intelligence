//! Point-in-time, generation-aware instrument catalog.
//!
//! The registry is append-only. Definitions and explicit corrections are
//! published as monotonically numbered records, while readers consume
//! immutable snapshots pinned to both a catalog revision and an as-known time.

mod history;
mod resolve;

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroU32,
    sync::Arc,
};

use domain::{DomainError, InstrumentDefinition, InstrumentId, UnixNanos, VenueId};
use thiserror::Error;

pub use history::{
    AppendOutcome, BatchOutcome, CatalogMutation, CatalogRecord, CatalogRevision, CorrectionInput,
    CorrectionRecord, DefinitionRecord, DefinitionRevision, MAXIMUM_BATCH_RECORDS, RegistryLimits,
    RegistryLimitsInput, RevisionMetadata,
};
pub use resolve::{CatalogSnapshot, CatalogSnapshotEntry, ResolvedInstrument};

#[derive(Clone, Debug)]
struct EffectiveDefinition {
    definition: InstrumentDefinition,
    revision: CatalogRevision,
    definition_hash: [u8; 32],
    correction_count: u32,
}

#[derive(Clone, Debug, Default)]
struct DerivedCatalog {
    by_id: HashMap<InstrumentId, EffectiveDefinition>,
    by_symbol: BTreeMap<(VenueId, String), Vec<InstrumentId>>,
}

impl DerivedCatalog {
    fn rebuild_symbol_index(&mut self) {
        let mut by_symbol = BTreeMap::<(VenueId, String), Vec<InstrumentId>>::new();
        for definition in self.by_id.values() {
            let id = definition.definition.id();
            by_symbol
                .entry((id.venue().clone(), id.venue_symbol().to_owned()))
                .or_default()
                .push(id.clone());
        }
        for ids in by_symbol.values_mut() {
            ids.sort_by(|left, right| {
                resolve::definition_order(
                    &self.by_id[left].definition,
                    &self.by_id[right].definition,
                )
            });
        }
        self.by_symbol = by_symbol;
    }

    fn validate_symbol(&self, key: &(VenueId, String)) -> Result<(), RegistryError> {
        let Some(ids) = self.by_symbol.get(key) else {
            return Ok(());
        };
        if ids
            .windows(2)
            .any(|pair| pair[0].generation() >= pair[1].generation())
        {
            return Err(RegistryError::GenerationNotIncreasing);
        }
        if ids.windows(2).any(|pair| {
            resolve::intervals_overlap(
                &self.by_id[&pair[0]].definition,
                &self.by_id[&pair[1]].definition,
            )
        }) {
            Err(RegistryError::OverlappingListing)
        } else {
            Ok(())
        }
    }
}

/// Mutable writer for an append-only catalog.
///
/// Publication is atomic at the `append_batch` boundary. Failed validation
/// never changes the visible records or current revision.
#[derive(Clone, Debug)]
pub struct InstrumentRegistry {
    limits: RegistryLimits,
    records: Vec<CatalogRecord>,
    derived: DerivedCatalog,
    last_known_at: Option<UnixNanos>,
}

impl Default for InstrumentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentRegistry {
    pub fn new() -> Self {
        Self::with_limits(RegistryLimits::default())
    }

    pub fn with_limits(limits: RegistryLimits) -> Self {
        Self {
            limits,
            records: Vec::new(),
            derived: DerivedCatalog::default(),
            last_known_at: None,
        }
    }

    pub fn records(&self) -> &[CatalogRecord] {
        &self.records
    }

    pub fn current_revision(&self) -> Option<CatalogRevision> {
        self.records.last().map(CatalogRecord::catalog_revision)
    }

    pub fn definition_revision(&self, id: &InstrumentId) -> Option<DefinitionRevision> {
        self.derived.by_id.get(id).map(|definition| {
            DefinitionRevision::new(definition.revision, definition.definition_hash)
        })
    }

    pub fn append_definition(
        &mut self,
        definition: InstrumentDefinition,
        metadata: RevisionMetadata,
    ) -> Result<AppendOutcome, RegistryError> {
        if let Some(existing) = self.derived.by_id.get(definition.id())
            && existing.definition == definition
        {
            return Ok(AppendOutcome::AlreadyPresent {
                revision: existing.revision,
            });
        }
        let previous = self.current_revision();
        let outcome = self.append_batch(
            previous,
            vec![CatalogMutation::Definition {
                definition,
                metadata,
            }],
        )?;
        let revision = outcome
            .committed_revision()
            .ok_or(RegistryError::InternalInvariant)?;
        Ok(AppendOutcome::Appended { revision })
    }

    pub fn append_correction(
        &mut self,
        correction: CorrectionInput,
    ) -> Result<AppendOutcome, RegistryError> {
        if let Some(revision) = existing_correction_revision(&self.records, &[], &correction) {
            return Ok(AppendOutcome::AlreadyPresent { revision });
        }
        let previous = self.current_revision();
        let outcome = self.append_batch(previous, vec![CatalogMutation::Correction(correction)])?;
        let revision = outcome
            .committed_revision()
            .ok_or(RegistryError::InternalInvariant)?;
        Ok(AppendOutcome::Appended { revision })
    }

    pub fn append_batch(
        &mut self,
        expected_revision: Option<CatalogRevision>,
        mutations: Vec<CatalogMutation>,
    ) -> Result<BatchOutcome, RegistryError> {
        let actual_revision = self.current_revision();
        if expected_revision != actual_revision {
            return Err(RegistryError::RevisionConflict {
                expected: expected_revision,
                actual: actual_revision,
            });
        }
        if mutations.is_empty() {
            return Err(RegistryError::EmptyBatch);
        }
        if mutations.len() > self.limits.maximum_batch_records() {
            return Err(RegistryError::BatchCapacityExceeded);
        }

        let mut derived = self.derived.clone();
        let mut staged = Vec::with_capacity(mutations.len());
        let mut commit_revision = None;
        let mut batch_known_at = None;
        let mut idempotent_records = 0usize;

        for mutation in mutations {
            match mutation {
                CatalogMutation::Definition {
                    definition,
                    metadata,
                } => {
                    if let Some(existing) = derived.by_id.get(definition.id()) {
                        if existing.definition == definition {
                            idempotent_records = idempotent_records
                                .checked_add(1)
                                .ok_or(RegistryError::RecordCapacityExceeded)?;
                            continue;
                        }
                        if existing.definition.listing_time() != definition.listing_time()
                            || existing.definition.delisting_time() != definition.delisting_time()
                        {
                            return Err(RegistryError::GenerationNotIncreasing);
                        }
                        return Err(RegistryError::GenerationConflict);
                    }
                    validate_record_capacity(self.records.len(), staged.len(), self.limits)?;
                    validate_batch_time(
                        self.last_known_at,
                        &mut batch_known_at,
                        metadata.known_at(),
                    )?;
                    if derived.by_id.len() >= self.limits.maximum_definitions() {
                        return Err(RegistryError::DefinitionCapacityExceeded);
                    }

                    let id = definition.id().clone();
                    let key = (id.venue().clone(), id.venue_symbol().to_owned());
                    let existing_ids = derived.by_symbol.get(&key);
                    match existing_ids {
                        None if derived.by_symbol.len() >= self.limits.maximum_symbol_keys() => {
                            return Err(RegistryError::SymbolCapacityExceeded);
                        }
                        Some(existing_ids)
                            if existing_ids.len() >= self.limits.maximum_history_per_symbol() =>
                        {
                            return Err(RegistryError::HistoryCapacityExceeded);
                        }
                        None | Some(_) => {}
                    }

                    let definition_hash = resolve::definition_hash(&definition)
                        .map_err(|_| RegistryError::CanonicalEncodingCapacityExceeded)?;
                    let revision =
                        *commit_revision.get_or_insert(CatalogRevision::next(actual_revision)?);
                    derived.by_id.insert(
                        id,
                        EffectiveDefinition {
                            definition: definition.clone(),
                            revision,
                            definition_hash,
                            correction_count: 0,
                        },
                    );
                    derived.rebuild_symbol_index();
                    staged.push(CatalogRecord::Definition(DefinitionRecord::new(
                        revision,
                        metadata.clone(),
                        definition_hash,
                        definition,
                    )));
                }
                CatalogMutation::Correction(correction) => {
                    if existing_correction_revision(&self.records, &staged, &correction).is_some() {
                        idempotent_records = idempotent_records
                            .checked_add(1)
                            .ok_or(RegistryError::RecordCapacityExceeded)?;
                        continue;
                    }
                    let replacement_id = correction.replacement().id();
                    let target_id = self
                        .records
                        .iter()
                        .chain(&staged)
                        .find(|record| {
                            record.catalog_revision() == correction.supersedes_revision()
                                && record.definition_hash() == correction.prior_definition_hash()
                        })
                        .map(CatalogRecord::definition)
                        .map(InstrumentDefinition::id);
                    let Some(current) = derived.by_id.get(replacement_id) else {
                        return if target_id.is_some() {
                            Err(RegistryError::CorrectionIdentityMismatch)
                        } else {
                            Err(RegistryError::UnknownCorrectionTarget)
                        };
                    };
                    if current.revision != correction.supersedes_revision() {
                        return Err(RegistryError::StaleCorrection);
                    }
                    if current.definition.id() != correction.replacement().id() {
                        return Err(RegistryError::CorrectionIdentityMismatch);
                    }
                    if current.definition_hash != *correction.prior_definition_hash() {
                        return Err(RegistryError::CorrectionHashMismatch);
                    }
                    if current.definition == *correction.replacement() {
                        return Err(RegistryError::NoOpCorrection);
                    }
                    if !resolve::same_economic_identity(
                        &current.definition,
                        correction.replacement(),
                    ) {
                        return Err(RegistryError::CorrectionRequiresNewGeneration);
                    }
                    validate_record_capacity(self.records.len(), staged.len(), self.limits)?;
                    validate_batch_time(
                        self.last_known_at,
                        &mut batch_known_at,
                        correction.metadata().known_at(),
                    )?;
                    if current.correction_count as usize
                        >= self.limits.maximum_corrections_per_instrument()
                    {
                        return Err(RegistryError::CorrectionCapacityExceeded);
                    }

                    let definition_hash = resolve::definition_hash(correction.replacement())
                        .map_err(|_| RegistryError::CanonicalEncodingCapacityExceeded)?;
                    let correction_id = resolve::correction_id(
                        correction.supersedes_revision(),
                        correction.prior_definition_hash(),
                        &definition_hash,
                        correction.metadata().known_at(),
                        correction.metadata().source_reference(),
                        correction.reason(),
                    )
                    .map_err(|_| RegistryError::CanonicalEncodingCapacityExceeded)?;
                    let correction_version = current
                        .correction_count
                        .checked_add(1)
                        .and_then(NonZeroU32::new)
                        .ok_or(RegistryError::CorrectionVersionExhausted)?;
                    let revision =
                        *commit_revision.get_or_insert(CatalogRevision::next(actual_revision)?);
                    derived.by_id.insert(
                        replacement_id.clone(),
                        EffectiveDefinition {
                            definition: correction.replacement().clone(),
                            revision,
                            definition_hash,
                            correction_count: correction_version.get(),
                        },
                    );
                    derived.rebuild_symbol_index();
                    staged.push(CatalogRecord::Correction(CorrectionRecord::new(
                        revision,
                        correction_version,
                        correction.supersedes_revision(),
                        correction.metadata().clone(),
                        *correction.prior_definition_hash(),
                        definition_hash,
                        correction_id,
                        correction.reason().to_owned(),
                        correction.replacement().clone(),
                    )));
                }
            }
        }

        for key in derived.by_symbol.keys() {
            derived.validate_symbol(key)?;
        }

        let appended_records = staged.len();
        if appended_records > 0 {
            let mut proposed_records = self.records.clone();
            proposed_records.extend(staged.iter().cloned());
            let proposed_known_at = batch_known_at.ok_or(RegistryError::InternalInvariant)?;
            resolve::build_snapshot(proposed_records, proposed_known_at, self.limits)
                .map_err(RegistryError::from_snapshot_validation)?;
        }
        self.records.extend(staged);
        self.derived = derived;
        if batch_known_at.is_some() {
            self.last_known_at = batch_known_at;
        }
        Ok(BatchOutcome::new(
            actual_revision,
            self.current_revision(),
            appended_records,
            idempotent_records,
        ))
    }

    /// Validate and apply a batch to an isolated copy of the current registry.
    ///
    /// Callers that must durably commit external state before publication can
    /// stage once, persist `staged.records()[original.records().len()..]`, and
    /// replace the live registry only after that commit succeeds.
    pub fn stage_batch(
        &self,
        expected_revision: Option<CatalogRevision>,
        mutations: Vec<CatalogMutation>,
    ) -> Result<(Self, BatchOutcome), RegistryError> {
        let mut staged = self.clone();
        let outcome = staged.append_batch(expected_revision, mutations)?;
        Ok((staged, outcome))
    }

    pub fn snapshot(&self) -> Result<Arc<CatalogSnapshot>, ResolveError> {
        let as_known_at = self.last_known_at.ok_or(ResolveError::CatalogEmpty)?;
        resolve::build_snapshot(self.records.clone(), as_known_at, self.limits)
    }

    pub fn snapshot_as_known_at(
        &self,
        as_known_at: UnixNanos,
    ) -> Result<Arc<CatalogSnapshot>, ResolveError> {
        let records = self
            .records
            .iter()
            .take_while(|record| record.metadata().known_at() <= as_known_at)
            .cloned()
            .collect::<Vec<_>>();
        resolve::build_snapshot(records, as_known_at, self.limits)
    }

    pub fn snapshot_at_revision(
        &self,
        revision: CatalogRevision,
    ) -> Result<Arc<CatalogSnapshot>, ResolveError> {
        if self
            .current_revision()
            .is_none_or(|current| revision > current)
        {
            return Err(ResolveError::CatalogRevisionUnavailable);
        }
        let records = self
            .records
            .iter()
            .take_while(|record| record.catalog_revision() <= revision)
            .cloned()
            .collect::<Vec<_>>();
        let as_known_at = records
            .last()
            .map(CatalogRecord::metadata)
            .map(RevisionMetadata::known_at)
            .ok_or(ResolveError::CatalogEmpty)?;
        resolve::build_snapshot(records, as_known_at, self.limits)
    }
}

fn validate_batch_time(
    previous: Option<UnixNanos>,
    batch_known_at: &mut Option<UnixNanos>,
    next: UnixNanos,
) -> Result<(), RegistryError> {
    if previous.is_some_and(|previous| next < previous) {
        return Err(RegistryError::RecordTimeRegression);
    }
    match *batch_known_at {
        Some(known_at) if known_at != next => Err(RegistryError::BatchKnownAtMismatch),
        Some(_) => Ok(()),
        None => {
            *batch_known_at = Some(next);
            Ok(())
        }
    }
}

fn validate_record_capacity(
    committed: usize,
    staged: usize,
    limits: RegistryLimits,
) -> Result<(), RegistryError> {
    if committed
        .checked_add(staged)
        .is_none_or(|count| count >= limits.maximum_records())
    {
        Err(RegistryError::RecordCapacityExceeded)
    } else {
        Ok(())
    }
}

fn existing_correction_revision(
    committed: &[CatalogRecord],
    staged: &[CatalogRecord],
    input: &CorrectionInput,
) -> Option<CatalogRevision> {
    committed
        .iter()
        .chain(staged)
        .find_map(|record| match record {
            CatalogRecord::Correction(record)
                if record.supersedes_revision() == input.supersedes_revision()
                    && record.prior_definition_hash() == input.prior_definition_hash()
                    && record.replacement() == input.replacement()
                    && record.metadata() == input.metadata()
                    && record.reason() == input.reason() =>
            {
                Some(record.catalog_revision())
            }
            CatalogRecord::Definition(_) | CatalogRecord::Correction(_) => None,
        })
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RegistryError {
    #[error("registry limits are invalid")]
    InvalidLimits,
    #[error("revision metadata is invalid or exceeds its bound")]
    InvalidRevisionMetadata,
    #[error("correction reason is invalid or exceeds its bound")]
    InvalidCorrectionReason,
    #[error("batch must contain at least one mutation")]
    EmptyBatch,
    #[error("batch exceeds the configured mutation bound")]
    BatchCapacityExceeded,
    #[error("catalog record capacity exceeded")]
    RecordCapacityExceeded,
    #[error("instrument definition capacity exceeded")]
    DefinitionCapacityExceeded,
    #[error("symbol-key capacity exceeded")]
    SymbolCapacityExceeded,
    #[error("symbol generation history capacity exceeded")]
    HistoryCapacityExceeded,
    #[error("instrument correction capacity exceeded")]
    CorrectionCapacityExceeded,
    #[error("canonical encoding capacity exceeded")]
    CanonicalEncodingCapacityExceeded,
    #[error("catalog revision exhausted")]
    RevisionExhausted,
    #[error("correction version exhausted")]
    CorrectionVersionExhausted,
    #[error("same instrument generation was supplied with different content")]
    GenerationConflict,
    #[error("instrument generation must increase monotonically for a venue symbol")]
    GenerationNotIncreasing,
    #[error("instrument listing intervals overlap")]
    OverlappingListing,
    #[error("catalog record known-at time regressed")]
    RecordTimeRegression,
    #[error("every non-idempotent mutation in one batch must have the same known-at time")]
    BatchKnownAtMismatch,
    #[error("batch expected revision differs from the current revision")]
    RevisionConflict {
        expected: Option<CatalogRevision>,
        actual: Option<CatalogRevision>,
    },
    #[error("correction target does not exist")]
    UnknownCorrectionTarget,
    #[error("correction targets a stale definition revision")]
    StaleCorrection,
    #[error("correction replacement has a different instrument identity")]
    CorrectionIdentityMismatch,
    #[error("correction prior-definition hash does not match")]
    CorrectionHashMismatch,
    #[error("correction does not change the definition")]
    NoOpCorrection,
    #[error("economic metadata changes require a new instrument generation")]
    CorrectionRequiresNewGeneration,
    #[error("internal registry invariant failed")]
    InternalInvariant,
}

impl RegistryError {
    fn from_snapshot_validation(error: ResolveError) -> Self {
        match error {
            ResolveError::SnapshotCapacityExceeded => Self::CanonicalEncodingCapacityExceeded,
            ResolveError::AmbiguousHistory => Self::OverlappingListing,
            ResolveError::CatalogEmpty
            | ResolveError::CatalogRevisionUnavailable
            | ResolveError::UnknownVenue
            | ResolveError::UnknownSymbol
            | ResolveError::UnknownInstrument
            | ResolveError::NotListedAtTime
            | ResolveError::InvalidIdentity(_) => Self::InternalInvariant,
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ResolveError {
    #[error("catalog has no record at the requested cutoff")]
    CatalogEmpty,
    #[error("catalog revision is unavailable")]
    CatalogRevisionUnavailable,
    #[error("venue is unknown in this snapshot")]
    UnknownVenue,
    #[error("venue symbol is unknown in this snapshot")]
    UnknownSymbol,
    #[error("instrument generation is unknown in this snapshot")]
    UnknownInstrument,
    #[error("instrument is not listed at the requested event time")]
    NotListedAtTime,
    #[error("instrument identity is invalid: {0}")]
    InvalidIdentity(#[source] DomainError),
    #[error("snapshot contains overlapping instrument history")]
    AmbiguousHistory,
    #[error("snapshot canonical encoding exceeds its configured bound")]
    SnapshotCapacityExceeded,
}
