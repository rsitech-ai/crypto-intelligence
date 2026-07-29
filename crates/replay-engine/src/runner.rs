use std::collections::BTreeMap;

use connector_binance::{
    BinanceBookSynchronizer, BinanceInput, BinanceMarket, NormalizationContext,
    normalize_depth_snapshot, normalize_native_message, parse_durable_depth_snapshot,
    parse_durable_native_message,
};
use connector_core::{DurableRawReference, NormalizedOutput, wal_stream_source_identity};
use domain::{
    AssetId, AssetNamespace, ContractKind, ContractValueUnit, InstrumentDefinition,
    InstrumentDefinitionInput, InstrumentId, ProductType, UnixNanos, VenueId,
};
use event_envelope::{BookLevel, EventType};
use fixed_decimal::{FixedDecimal, Price, Quantity};
use instrument_registry::{CatalogSnapshot, InstrumentRegistry, RevisionMetadata};
use orderbook::{
    ApplyResult, BookClassification, BookConfig, BookQuality, BookSession, ChecksumPolicy,
    SequencePolicy,
};
use quality::QualityCause;
use raw_wal::{
    frame::RecordMetadata,
    manager::{RotationPolicy, SegmentedWalWriter},
    prologue::{SegmentMetadata, StreamDescriptor},
};
use serde::Serialize;

use crate::{
    ReplayConfig, ReplayError, ReplayRecordKind,
    config::{ExpectedReplayDigest, ReplayRecord},
};

const REPLAY_DOMAIN_EVENTS: &[u8] = b"cmti:replay:events:v1\0";
const REPLAY_DOMAIN_BOOK: &[u8] = b"cmti:replay:book:v1\0";
const REPLAY_DOMAIN_QUALITY: &[u8] = b"cmti:replay:quality:v1\0";
const GOLDEN_INSTRUMENT_GENERATION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayDigest {
    normalized_event_hash: [u8; 32],
    book_state_hash: [u8; 32],
    quality_incident_hash: [u8; 32],
    replayed_records: usize,
    normalized_events: usize,
    quality_incidents: usize,
    silent_integrity_failures: u64,
    logical_elapsed_ns: u64,
}

impl ReplayDigest {
    pub fn normalized_event_hash_hex(&self) -> String {
        hex::encode(self.normalized_event_hash)
    }

    pub fn book_state_hash_hex(&self) -> String {
        hex::encode(self.book_state_hash)
    }

    pub fn quality_incident_hash_hex(&self) -> String {
        hex::encode(self.quality_incident_hash)
    }

    pub const fn replayed_records(&self) -> usize {
        self.replayed_records
    }

    pub const fn normalized_events(&self) -> usize {
        self.normalized_events
    }

    pub const fn quality_incidents(&self) -> usize {
        self.quality_incidents
    }

    pub const fn silent_integrity_failures(&self) -> u64 {
        self.silent_integrity_failures
    }

    pub const fn logical_elapsed_ns(&self) -> u64 {
        self.logical_elapsed_ns
    }

    pub fn matches_expected(&self, expected: &ExpectedReplayDigest) -> bool {
        self.normalized_event_hash == expected.normalized_event_hash
            && self.book_state_hash == expected.book_state_hash
            && self.quality_incident_hash == expected.quality_incident_hash
            && self.replayed_records == expected.replayed_records
            && self.normalized_events == expected.normalized_events
            && self.quality_incidents == expected.quality_incidents
            && self.silent_integrity_failures == expected.silent_integrity_failures
            && self.logical_elapsed_ns == expected.logical_elapsed_ns
    }
}

pub struct ReplayRunner;

impl ReplayRunner {
    pub fn run_to_digest(config: &ReplayConfig) -> Result<ReplayDigest, ReplayError> {
        let directory = tempfile::tempdir()?;
        materialize_wal(config, directory.path())?;
        let last = config
            .records
            .last()
            .ok_or(ReplayError::InvalidManifest("records"))?;
        let replay_now = last
            .receive_monotonic_ns
            .checked_add(2)
            .ok_or(ReplayError::Arithmetic)?;
        let replay_wall = last
            .receive_wall_time
            .value()
            .checked_add(2)
            .ok_or(ReplayError::Arithmetic)?;
        let recovered = SegmentedWalWriter::recover(
            directory.path(),
            RotationPolicy::default(),
            replay_now,
            replay_wall,
        )?;
        let catalog = catalog()?;
        let instrument = instrument()?;
        let mut book = BinanceBookSynchronizer::try_new(
            BinanceMarket::Spot,
            BookConfig {
                instrument,
                price_tick: price("0.01")?,
                quantity_step: quantity("0.001")?,
                max_levels_per_side: config.max_levels_per_side,
                max_buffered_deltas: config.max_buffered_deltas,
                max_buffered_level_updates: config.max_buffered_level_updates,
                sequence_policy: SequencePolicy::RangeContainsNext,
                checksum_policy: ChecksumPolicy::Disabled,
                max_l3_orders: None,
                max_l3_levels_per_side: None,
            },
        )?;
        book.start_session(BookSession {
            connection_epoch: config.connection_epoch.get(),
            subscription_epoch: config.subscription_epoch.get(),
            instrument_generation: GOLDEN_INSTRUMENT_GENERATION,
        })?;

        let records_by_sequence = config
            .records
            .iter()
            .map(|record| (record.record_sequence.get(), record))
            .collect::<BTreeMap<_, _>>();
        let mut event_entries = Vec::with_capacity(config.records.len());
        let mut incident_entries = Vec::new();
        let mut replayed_records = 0_usize;
        let mut first_monotonic = None;
        let mut last_monotonic = None;
        let mut failure = None;
        recovered.visit_verified_records(|record| {
            if failure.is_some() {
                return;
            }
            let result = replay_verified_record(
                config,
                &catalog,
                &records_by_sequence,
                &mut book,
                record,
                &mut event_entries,
                &mut incident_entries,
            );
            match result {
                Ok(ReplayRecordOutcome::Skipped) => {}
                Ok(ReplayRecordOutcome::Applied {
                    receive_monotonic_ns,
                }) => {
                    replayed_records += 1;
                    first_monotonic.get_or_insert(receive_monotonic_ns);
                    last_monotonic = Some(receive_monotonic_ns);
                }
                Err(error) => failure = Some(error),
            }
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        let snapshot = book.snapshot().map_err(|_| ReplayError::MissingBook)?;
        let book_bytes = serde_json::to_vec(&CanonicalBook {
            bids: snapshot.bids(),
            asks: snapshot.asks(),
            last_source_sequence: snapshot.last_source_sequence(),
            session: snapshot.session(),
            classification: snapshot.classification(),
            updated_monotonic_ns: snapshot.updated_monotonic_ns(),
            quality: snapshot.quality(),
        })?;
        let logical_elapsed_ns = last_monotonic
            .zip(first_monotonic)
            .map_or(0, |(last, first)| last.saturating_sub(first));
        Ok(ReplayDigest {
            normalized_event_hash: hash_entries(
                REPLAY_DOMAIN_EVENTS,
                config.deterministic_seed,
                &event_entries,
            )?,
            book_state_hash: hash_entries(
                REPLAY_DOMAIN_BOOK,
                config.deterministic_seed,
                &[book_bytes],
            )?,
            quality_incident_hash: hash_entries(
                REPLAY_DOMAIN_QUALITY,
                config.deterministic_seed,
                &incident_entries,
            )?,
            replayed_records,
            normalized_events: event_entries.len(),
            quality_incidents: incident_entries.len(),
            silent_integrity_failures: 0,
            logical_elapsed_ns,
        })
    }
}

fn materialize_wal(config: &ReplayConfig, directory: &std::path::Path) -> Result<(), ReplayError> {
    let stream = config
        .records
        .first()
        .ok_or(ReplayError::InvalidManifest("records"))?;
    let descriptor = StreamDescriptor::new(
        stream.stream_id.get(),
        wal_stream_source_identity(&config.source),
        stream.stream_name.clone(),
    )?;
    let first = config
        .records
        .first()
        .ok_or(ReplayError::InvalidManifest("records"))?;
    let metadata = SegmentMetadata::new(
        config.wal_segment_id,
        first.receive_wall_time.value(),
        "cmti-market-replay-v1",
        format!("replay-seed-{:016x}", config.deterministic_seed),
        "replay-engine-v1",
        vec![descriptor],
    )?;
    let mut writer = SegmentedWalWriter::create(
        directory,
        metadata,
        RotationPolicy::default(),
        first.receive_monotonic_ns,
    )?;
    for record in &config.records {
        let append = writer.append(
            RecordMetadata {
                flags: 0,
                stream_id: record.stream_id.get(),
                connection_epoch: config.connection_epoch.get(),
                record_sequence: record.record_sequence.get(),
                receive_wall_time_ns: record.receive_wall_time.value(),
                receive_monotonic_time_ns: record.receive_monotonic_ns,
            },
            &record.payload,
            record.receive_monotonic_ns,
            record.receive_wall_time.value(),
        )?;
        let (_, compression) = append.into_parts();
        if compression.is_some() {
            return Err(ReplayError::InvalidManifest("unexpected rotation"));
        }
    }
    let seal_monotonic = config
        .records
        .last()
        .ok_or(ReplayError::InvalidManifest("records"))?
        .receive_monotonic_ns
        .checked_add(1)
        .ok_or(ReplayError::Arithmetic)?;
    let seal_wall = config
        .records
        .last()
        .ok_or(ReplayError::InvalidManifest("records"))?
        .receive_wall_time
        .value()
        .checked_add(1)
        .ok_or(ReplayError::Arithmetic)?;
    writer.seal_active(seal_monotonic, seal_wall)?;
    drop(writer);
    Ok(())
}

enum ReplayRecordOutcome {
    Applied { receive_monotonic_ns: u64 },
    Skipped,
}

#[allow(clippy::too_many_arguments)]
fn replay_verified_record(
    config: &ReplayConfig,
    catalog: &CatalogSnapshot,
    records_by_sequence: &BTreeMap<u64, &ReplayRecord>,
    book: &mut BinanceBookSynchronizer,
    recovered: raw_wal::manager::VerifiedRecoveredRecord<'_>,
    event_entries: &mut Vec<Vec<u8>>,
    incident_entries: &mut Vec<Vec<u8>>,
) -> Result<ReplayRecordOutcome, ReplayError> {
    let metadata = recovered.metadata();
    let declared = records_by_sequence
        .get(&metadata.record_sequence)
        .ok_or(ReplayError::RecordMismatch)?;
    if metadata.stream_id != declared.stream_id.get()
        || recovered.stream_name() != declared.stream_name
        || recovered.payload() != declared.payload.as_ref()
        || metadata.receive_wall_time_ns != declared.receive_wall_time.value()
        || metadata.receive_monotonic_time_ns != declared.receive_monotonic_ns
    {
        return Err(ReplayError::RecordMismatch);
    }
    if config
        .stop_at_receive_monotonic_ns
        .is_some_and(|stop| metadata.receive_monotonic_time_ns > stop)
    {
        return Ok(ReplayRecordOutcome::Skipped);
    }
    let raw = DurableRawReference::try_from_recovered(config.source.clone(), &recovered)?;
    let normalization_time = raw
        .receive_wall_time()
        .value()
        .checked_add(config.normalization_offset_ns)
        .ok_or(ReplayError::Arithmetic)?;
    let context = NormalizationContext::try_new(
        catalog,
        &raw,
        UnixNanos::new(normalization_time),
        config.connection_started_at,
        config.subscription_epoch,
        "golden-market-replay-v1",
    )?;
    let events = match declared.kind {
        ReplayRecordKind::SpotDepthSnapshot => {
            let snapshot = parse_durable_depth_snapshot(
                BinanceMarket::Spot,
                "BTCUSDT",
                recovered.payload(),
                &raw,
            )?;
            vec![normalize_depth_snapshot(snapshot, &context)?]
        }
        ReplayRecordKind::SpotWebSocket => {
            let parsed = parse_durable_native_message(
                BinanceInput::SpotWebSocket,
                recovered.payload(),
                &raw,
            )?;
            normalize_native_message(parsed, &context)?
        }
    };
    for event in events {
        event.verify().map_err(|_| ReplayError::RecordMismatch)?;
        let serialized = serde_json::to_vec(&event)?;
        if matches!(
            event.event_type(),
            EventType::BookSnapshot | EventType::BookDelta
        ) {
            let output = NormalizedOutput::try_new(raw.clone(), event)?;
            let result = book.apply_output(&output)?;
            if matches!(
                result,
                ApplyResult::GapDetected
                    | ApplyResult::ChecksumMismatch
                    | ApplyResult::SnapshotRequired
            ) {
                incident_entries.push(serde_json::to_vec(&ReplayIncident {
                    record_sequence: metadata.record_sequence,
                    cause: QualityCause::SequenceIntegrityFailed.as_str(),
                    outcome: result,
                })?);
            }
        }
        event_entries.push(serialized);
    }
    Ok(ReplayRecordOutcome::Applied {
        receive_monotonic_ns: metadata.receive_monotonic_time_ns,
    })
}

#[derive(Serialize)]
struct ReplayIncident<'a> {
    record_sequence: u64,
    cause: &'a str,
    outcome: ApplyResult,
}

#[derive(Serialize)]
struct CanonicalBook<'a> {
    bids: &'a [BookLevel],
    asks: &'a [BookLevel],
    last_source_sequence: u64,
    session: BookSession,
    classification: BookClassification,
    updated_monotonic_ns: u64,
    quality: &'a BookQuality,
}

fn hash_entries(domain: &[u8], seed: u64, entries: &[Vec<u8>]) -> Result<[u8; 32], ReplayError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&seed.to_be_bytes());
    hasher.update(
        &u64::try_from(entries.len())
            .map_err(|_| ReplayError::Arithmetic)?
            .to_be_bytes(),
    );
    for entry in entries {
        hasher.update(
            &u64::try_from(entry.len())
                .map_err(|_| ReplayError::Arithmetic)?
                .to_be_bytes(),
        );
        hasher.update(entry);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn catalog() -> Result<std::sync::Arc<CatalogSnapshot>, ReplayError> {
    let mut registry = InstrumentRegistry::new();
    registry.append_definition(
        definition()?,
        RevisionMetadata::try_new(UnixNanos::new(1), "golden-market-replay-v1")
            .map_err(|_| ReplayError::InvalidManifest("revision metadata"))?,
    )?;
    Ok(registry.snapshot()?)
}

fn definition() -> Result<InstrumentDefinition, ReplayError> {
    let quote = AssetId::new(AssetNamespace::Synthetic, "", "", "USDT", 1)?;
    Ok(InstrumentDefinition::new(InstrumentDefinitionInput {
        id: instrument()?,
        product_type: ProductType::Spot,
        base_asset: AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1)?,
        quote_asset: quote.clone(),
        settlement_asset: quote,
        contract_multiplier: decimal("1")?,
        contract_value_unit: ContractValueUnit::Base,
        contract_kind: ContractKind::None,
        expiry_time: None,
        strike: None,
        option_side: None,
        price_tick: price("0.01")?,
        quantity_step: quantity("0.001")?,
        listing_time: UnixNanos::new(1),
        delisting_time: None,
    })?)
}

fn instrument() -> Result<InstrumentId, ReplayError> {
    Ok(InstrumentId::new_for_product(
        VenueId::new("binance")?,
        "BTCUSDT",
        ProductType::Spot,
        GOLDEN_INSTRUMENT_GENERATION,
    )?)
}

fn decimal(value: &str) -> Result<FixedDecimal, ReplayError> {
    FixedDecimal::parse_canonical(value).map_err(|_| ReplayError::InvalidManifest("decimal"))
}

fn price(value: &str) -> Result<Price, ReplayError> {
    Price::new(decimal(value)?).map_err(ReplayError::from)
}

fn quantity(value: &str) -> Result<Quantity, ReplayError> {
    Quantity::new(decimal(value)?).map_err(ReplayError::from)
}
