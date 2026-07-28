//! Immutable catalog snapshots, exact resolution, and canonical hashing.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use domain::{AssetId, InstrumentDefinition, InstrumentId, UnixNanos, VenueId};
use serde::Serialize;

use crate::{
    CatalogRecord, CatalogRevision, RegistryLimits, ResolveError, history::MAXIMUM_SNAPSHOT_BYTES,
};

const SNAPSHOT_SCHEMA_VERSION: u32 = 1;
const DEFINITION_HASH_DOMAIN: &[u8] = b"cmti:instrument-definition:v1\0";
const CORRECTION_ID_DOMAIN: &[u8] = b"cmti:instrument-correction:v1\0";
const CATALOG_HASH_DOMAIN: &[u8] = b"cmti:instrument-catalog:v1\0";
const HISTORY_HASH_DOMAIN: &[u8] = b"cmti:instrument-history:v1\0";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogSnapshotEntry {
    definition_revision: CatalogRevision,
    definition_hash: [u8; 32],
    definition: InstrumentDefinition,
}

impl CatalogSnapshotEntry {
    pub const fn definition_revision(&self) -> CatalogRevision {
        self.definition_revision
    }

    pub const fn definition_hash(&self) -> &[u8; 32] {
        &self.definition_hash
    }

    pub const fn definition(&self) -> &InstrumentDefinition {
        &self.definition
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CatalogSnapshot {
    schema_version: u32,
    catalog_revision: CatalogRevision,
    as_known_at: UnixNanos,
    records: Vec<CatalogRecord>,
    entries: Vec<CatalogSnapshotEntry>,
    catalog_digest: [u8; 32],
    history_digest: [u8; 32],
    #[serde(skip)]
    by_symbol: BTreeMap<(VenueId, String), Vec<usize>>,
    #[serde(skip)]
    by_id: HashMap<InstrumentId, usize>,
    #[serde(skip)]
    venues: BTreeSet<VenueId>,
}

impl PartialEq for CatalogSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.catalog_revision == other.catalog_revision
            && self.as_known_at == other.as_known_at
            && self.records == other.records
            && self.entries == other.entries
            && self.catalog_digest == other.catalog_digest
            && self.history_digest == other.history_digest
    }
}

impl Eq for CatalogSnapshot {}

impl CatalogSnapshot {
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn catalog_revision(&self) -> CatalogRevision {
        self.catalog_revision
    }

    pub const fn as_known_at(&self) -> UnixNanos {
        self.as_known_at
    }

    pub fn records(&self) -> &[CatalogRecord] {
        &self.records
    }

    pub fn entries(&self) -> &[CatalogSnapshotEntry] {
        &self.entries
    }

    pub const fn catalog_digest(&self) -> &[u8; 32] {
        &self.catalog_digest
    }

    pub const fn history_digest(&self) -> &[u8; 32] {
        &self.history_digest
    }

    pub fn resolve(
        &self,
        venue: &VenueId,
        symbol: &str,
        event_time: UnixNanos,
    ) -> Result<ResolvedInstrument<'_>, ResolveError> {
        if !self.venues.contains(venue) {
            return Err(ResolveError::UnknownVenue);
        }
        let normalized =
            InstrumentId::new(venue.clone(), symbol, 1).map_err(ResolveError::InvalidIdentity)?;
        let key = (venue.clone(), normalized.venue_symbol().to_owned());
        let indices = self
            .by_symbol
            .get(&key)
            .ok_or(ResolveError::UnknownSymbol)?;
        let mut matches = indices
            .iter()
            .copied()
            .filter(|index| listed_at(self.entries[*index].definition(), event_time));
        let Some(index) = matches.next() else {
            return Err(ResolveError::NotListedAtTime);
        };
        if matches.next().is_some() {
            return Err(ResolveError::AmbiguousHistory);
        }
        Ok(self.resolved(index))
    }

    pub fn resolve_id(
        &self,
        id: &InstrumentId,
        event_time: UnixNanos,
    ) -> Result<ResolvedInstrument<'_>, ResolveError> {
        let index = self
            .by_id
            .get(id)
            .copied()
            .ok_or(ResolveError::UnknownInstrument)?;
        if !listed_at(self.entries[index].definition(), event_time) {
            return Err(ResolveError::NotListedAtTime);
        }
        Ok(self.resolved(index))
    }

    pub fn verify_integrity(&self) -> bool {
        let Ok((catalog_digest, history_digest)) = snapshot_digests(
            self.catalog_revision,
            self.as_known_at,
            &self.records,
            &self.entries,
            MAXIMUM_SNAPSHOT_BYTES,
        ) else {
            return false;
        };
        catalog_digest == self.catalog_digest && history_digest == self.history_digest
    }

    fn resolved(&self, index: usize) -> ResolvedInstrument<'_> {
        let entry = &self.entries[index];
        ResolvedInstrument {
            definition: entry.definition(),
            definition_revision: entry.definition_revision(),
            catalog_revision: self.catalog_revision,
            as_known_at: self.as_known_at,
            catalog_digest: &self.catalog_digest,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedInstrument<'a> {
    definition: &'a InstrumentDefinition,
    definition_revision: CatalogRevision,
    catalog_revision: CatalogRevision,
    as_known_at: UnixNanos,
    catalog_digest: &'a [u8; 32],
}

impl<'a> ResolvedInstrument<'a> {
    pub const fn definition(self) -> &'a InstrumentDefinition {
        self.definition
    }

    pub const fn definition_revision(self) -> CatalogRevision {
        self.definition_revision
    }

    pub const fn catalog_revision(self) -> CatalogRevision {
        self.catalog_revision
    }

    pub const fn as_known_at(self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn catalog_digest(self) -> &'a [u8; 32] {
        self.catalog_digest
    }
}

pub(crate) fn build_snapshot(
    records: Vec<CatalogRecord>,
    as_known_at: UnixNanos,
    limits: RegistryLimits,
) -> Result<Arc<CatalogSnapshot>, ResolveError> {
    let catalog_revision = records
        .last()
        .map(CatalogRecord::catalog_revision)
        .ok_or(ResolveError::CatalogEmpty)?;
    let mut effective = HashMap::<InstrumentId, CatalogSnapshotEntry>::new();
    for record in &records {
        let entry = CatalogSnapshotEntry {
            definition_revision: record.catalog_revision(),
            definition_hash: *record.definition_hash(),
            definition: record.definition().clone(),
        };
        effective.insert(entry.definition.id().clone(), entry);
    }
    let mut entries = effective.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| definition_order(left.definition(), right.definition()));

    let mut by_symbol = BTreeMap::<(VenueId, String), Vec<usize>>::new();
    let mut by_id = HashMap::with_capacity(entries.len());
    let mut venues = BTreeSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let id = entry.definition().id();
        venues.insert(id.venue().clone());
        by_symbol
            .entry((id.venue().clone(), id.venue_symbol().to_owned()))
            .or_default()
            .push(index);
        by_id.insert(id.clone(), index);
    }
    if by_symbol.values().any(|indices| {
        indices.windows(2).any(|pair| {
            intervals_overlap(entries[pair[0]].definition(), entries[pair[1]].definition())
        })
    }) {
        return Err(ResolveError::AmbiguousHistory);
    }

    let (catalog_digest, history_digest) = snapshot_digests(
        catalog_revision,
        as_known_at,
        &records,
        &entries,
        limits.maximum_snapshot_bytes(),
    )?;
    let snapshot = SelfContainedSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        catalog_revision,
        as_known_at,
        records,
        entries,
        catalog_digest,
        history_digest,
        by_symbol,
        by_id,
        venues,
    };
    Ok(Arc::new(snapshot.into_snapshot()))
}

struct SelfContainedSnapshot {
    schema_version: u32,
    catalog_revision: CatalogRevision,
    as_known_at: UnixNanos,
    records: Vec<CatalogRecord>,
    entries: Vec<CatalogSnapshotEntry>,
    catalog_digest: [u8; 32],
    history_digest: [u8; 32],
    by_symbol: BTreeMap<(VenueId, String), Vec<usize>>,
    by_id: HashMap<InstrumentId, usize>,
    venues: BTreeSet<VenueId>,
}

impl SelfContainedSnapshot {
    fn into_snapshot(self) -> CatalogSnapshot {
        CatalogSnapshot {
            schema_version: self.schema_version,
            catalog_revision: self.catalog_revision,
            as_known_at: self.as_known_at,
            records: self.records,
            entries: self.entries,
            catalog_digest: self.catalog_digest,
            history_digest: self.history_digest,
            by_symbol: self.by_symbol,
            by_id: self.by_id,
            venues: self.venues,
        }
    }
}

pub(crate) fn definition_hash(
    definition: &InstrumentDefinition,
) -> Result<[u8; 32], RegistryLimitError> {
    let mut writer = CanonicalWriter::new(MAXIMUM_SNAPSHOT_BYTES);
    encode_definition(&mut writer, definition)?;
    Ok(domain_hash(DEFINITION_HASH_DOMAIN, &writer.finish()?))
}

pub(crate) fn correction_id(
    supersedes_revision: CatalogRevision,
    prior_definition_hash: &[u8; 32],
    definition_hash: &[u8; 32],
    known_at: UnixNanos,
    source_reference: &str,
    reason: &str,
) -> Result<[u8; 32], RegistryLimitError> {
    let mut writer = CanonicalWriter::new(MAXIMUM_SNAPSHOT_BYTES);
    writer.u64(supersedes_revision.get())?;
    writer.bytes(prior_definition_hash)?;
    writer.bytes(definition_hash)?;
    writer.i64(known_at.value())?;
    writer.string(source_reference)?;
    writer.string(reason)?;
    Ok(domain_hash(CORRECTION_ID_DOMAIN, &writer.finish()?))
}

pub(crate) fn listed_at(definition: &InstrumentDefinition, event_time: UnixNanos) -> bool {
    event_time >= definition.listing_time()
        && definition
            .delisting_time()
            .is_none_or(|delisting| event_time < delisting)
}

pub(crate) fn intervals_overlap(left: &InstrumentDefinition, right: &InstrumentDefinition) -> bool {
    let left_before_right_end = right
        .delisting_time()
        .is_none_or(|right_end| left.listing_time() < right_end);
    let right_before_left_end = left
        .delisting_time()
        .is_none_or(|left_end| right.listing_time() < left_end);
    left_before_right_end && right_before_left_end
}

pub(crate) fn definition_order(
    left: &InstrumentDefinition,
    right: &InstrumentDefinition,
) -> std::cmp::Ordering {
    left.id()
        .venue()
        .cmp(right.id().venue())
        .then_with(|| left.id().venue_symbol().cmp(right.id().venue_symbol()))
        .then_with(|| left.listing_time().cmp(&right.listing_time()))
        .then_with(|| left.id().generation().cmp(&right.id().generation()))
}

pub(crate) fn same_economic_identity(
    left: &InstrumentDefinition,
    right: &InstrumentDefinition,
) -> bool {
    left.id() == right.id()
        && left.product_type() == right.product_type()
        && left.base_asset() == right.base_asset()
        && left.quote_asset() == right.quote_asset()
        && left.settlement_asset() == right.settlement_asset()
        && left.contract_multiplier() == right.contract_multiplier()
        && left.contract_value_unit() == right.contract_value_unit()
        && left.contract_kind() == right.contract_kind()
        && left.expiry_time() == right.expiry_time()
        && left.strike() == right.strike()
        && left.option_side() == right.option_side()
        && left.price_tick() == right.price_tick()
        && left.quantity_step() == right.quantity_step()
}

fn snapshot_digests(
    catalog_revision: CatalogRevision,
    as_known_at: UnixNanos,
    records: &[CatalogRecord],
    entries: &[CatalogSnapshotEntry],
    maximum_bytes: usize,
) -> Result<([u8; 32], [u8; 32]), ResolveError> {
    let mut catalog = CanonicalWriter::new(maximum_bytes);
    catalog
        .u32(SNAPSHOT_SCHEMA_VERSION)
        .and_then(|()| catalog.u64(catalog_revision.get()))
        .and_then(|()| catalog.i64(as_known_at.value()))
        .and_then(|()| catalog.usize(entries.len()))
        .map_err(|_| ResolveError::SnapshotCapacityExceeded)?;
    for entry in entries {
        encode_definition(&mut catalog, entry.definition())
            .map_err(|_| ResolveError::SnapshotCapacityExceeded)?;
    }
    let catalog_bytes = catalog
        .finish()
        .map_err(|_| ResolveError::SnapshotCapacityExceeded)?;

    let remaining_bytes = maximum_bytes
        .checked_sub(catalog_bytes.len())
        .ok_or(ResolveError::SnapshotCapacityExceeded)?;
    let mut history = CanonicalWriter::new(remaining_bytes);
    history
        .u32(SNAPSHOT_SCHEMA_VERSION)
        .and_then(|()| history.u64(catalog_revision.get()))
        .and_then(|()| history.i64(as_known_at.value()))
        .and_then(|()| history.usize(records.len()))
        .map_err(|_| ResolveError::SnapshotCapacityExceeded)?;
    for record in records {
        encode_record(&mut history, record).map_err(|_| ResolveError::SnapshotCapacityExceeded)?;
    }
    let history_bytes = history
        .finish()
        .map_err(|_| ResolveError::SnapshotCapacityExceeded)?;
    let catalog_digest = domain_hash(CATALOG_HASH_DOMAIN, &catalog_bytes);
    let history_digest = domain_hash(HISTORY_HASH_DOMAIN, &history_bytes);
    Ok((catalog_digest, history_digest))
}

fn encode_record(
    writer: &mut CanonicalWriter,
    record: &CatalogRecord,
) -> Result<(), RegistryLimitError> {
    match record {
        CatalogRecord::Definition(record) => {
            writer.u8(0)?;
            writer.u64(record.catalog_revision().get())?;
            encode_metadata(writer, record.metadata())?;
            writer.bytes(record.definition_hash())?;
            encode_definition(writer, record.definition())
        }
        CatalogRecord::Correction(record) => {
            writer.u8(1)?;
            writer.u64(record.catalog_revision().get())?;
            writer.u32(record.correction_version().get())?;
            writer.u64(record.supersedes_revision().get())?;
            encode_metadata(writer, record.metadata())?;
            writer.bytes(record.prior_definition_hash())?;
            writer.bytes(record.definition_hash())?;
            writer.bytes(record.correction_id())?;
            writer.string(record.reason())?;
            encode_definition(writer, record.replacement())
        }
    }
}

fn encode_metadata(
    writer: &mut CanonicalWriter,
    metadata: &crate::RevisionMetadata,
) -> Result<(), RegistryLimitError> {
    writer.i64(metadata.known_at().value())?;
    writer.string(metadata.source_reference())
}

fn encode_definition(
    writer: &mut CanonicalWriter,
    definition: &InstrumentDefinition,
) -> Result<(), RegistryLimitError> {
    encode_instrument_id(writer, definition.id())?;
    writer.u8(definition.product_type() as u8)?;
    encode_asset_id(writer, definition.base_asset())?;
    encode_asset_id(writer, definition.quote_asset())?;
    encode_asset_id(writer, definition.settlement_asset())?;
    encode_decimal(
        writer,
        definition.contract_multiplier().mantissa(),
        definition.contract_multiplier().scale(),
    )?;
    writer.u8(definition.contract_value_unit() as u8)?;
    writer.u8(definition.contract_kind() as u8)?;
    encode_optional_time(writer, definition.expiry_time())?;
    match definition.strike() {
        Some(value) => {
            writer.u8(1)?;
            encode_decimal(writer, value.value().mantissa(), value.value().scale())?;
        }
        None => writer.u8(0)?,
    }
    match definition.option_side() {
        Some(value) => {
            writer.u8(1)?;
            writer.u8(value as u8)?;
        }
        None => writer.u8(0)?,
    }
    encode_decimal(
        writer,
        definition.price_tick().value().mantissa(),
        definition.price_tick().value().scale(),
    )?;
    encode_decimal(
        writer,
        definition.quantity_step().value().mantissa(),
        definition.quantity_step().value().scale(),
    )?;
    writer.i64(definition.listing_time().value())?;
    encode_optional_time(writer, definition.delisting_time())
}

fn encode_instrument_id(
    writer: &mut CanonicalWriter,
    id: &InstrumentId,
) -> Result<(), RegistryLimitError> {
    writer.string(id.venue().as_str())?;
    writer.string(id.venue_symbol())?;
    writer.u32(id.generation())
}

fn encode_asset_id(writer: &mut CanonicalWriter, id: &AssetId) -> Result<(), RegistryLimitError> {
    writer.u8(id.namespace() as u8)?;
    writer.string(id.chain_id())?;
    writer.string(id.contract_or_mint())?;
    writer.string(id.canonical_symbol())?;
    writer.u32(id.generation())
}

fn encode_optional_time(
    writer: &mut CanonicalWriter,
    value: Option<UnixNanos>,
) -> Result<(), RegistryLimitError> {
    match value {
        Some(value) => {
            writer.u8(1)?;
            writer.i64(value.value())
        }
        None => writer.u8(0),
    }
}

fn encode_decimal(
    writer: &mut CanonicalWriter,
    mantissa: i128,
    scale: u32,
) -> Result<(), RegistryLimitError> {
    writer.bytes(&mantissa.to_be_bytes())?;
    writer.u32(scale)
}

fn domain_hash(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RegistryLimitError;

struct CanonicalWriter {
    bytes: Vec<u8>,
    maximum_bytes: usize,
}

impl CanonicalWriter {
    fn new(maximum_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum_bytes,
        }
    }

    fn u8(&mut self, value: u8) -> Result<(), RegistryLimitError> {
        self.bytes(&[value])
    }

    fn u32(&mut self, value: u32) -> Result<(), RegistryLimitError> {
        self.bytes(&value.to_be_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), RegistryLimitError> {
        self.bytes(&value.to_be_bytes())
    }

    fn i64(&mut self, value: i64) -> Result<(), RegistryLimitError> {
        self.bytes(&value.to_be_bytes())
    }

    fn usize(&mut self, value: usize) -> Result<(), RegistryLimitError> {
        let value = u64::try_from(value).map_err(|_| RegistryLimitError)?;
        self.u64(value)
    }

    fn string(&mut self, value: &str) -> Result<(), RegistryLimitError> {
        self.usize(value.len())?;
        self.bytes(value.as_bytes())
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), RegistryLimitError> {
        let next = self
            .bytes
            .len()
            .checked_add(value.len())
            .ok_or(RegistryLimitError)?;
        if next > self.maximum_bytes {
            return Err(RegistryLimitError);
        }
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>, RegistryLimitError> {
        if self.bytes.len() > self.maximum_bytes {
            Err(RegistryLimitError)
        } else {
            Ok(self.bytes)
        }
    }
}
