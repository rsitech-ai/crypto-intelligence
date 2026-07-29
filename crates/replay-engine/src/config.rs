use std::{
    collections::BTreeSet,
    fmt,
    fs::{self, File, OpenOptions},
    io::Read,
    num::{NonZeroU32, NonZeroU64},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
};

use domain::{SourceId, SourceKind, UnixNanos};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::ReplayError;

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_INPUT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_TOTAL_INPUT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RECORDS: usize = 4_096;
const MAX_LEVELS_PER_SIDE: usize = 100_000;
const MAX_BUFFERED_DELTAS: usize = 100_000;
const MAX_BUFFERED_LEVEL_UPDATES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReplayRecordKind {
    SpotDepthSnapshot,
    SpotWebSocket,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedReplayDigest {
    pub(crate) normalized_event_hash: [u8; 32],
    pub(crate) book_state_hash: [u8; 32],
    pub(crate) quality_incident_hash: [u8; 32],
    pub(crate) replayed_records: usize,
    pub(crate) normalized_events: usize,
    pub(crate) quality_incidents: usize,
    pub(crate) silent_integrity_failures: u64,
    pub(crate) logical_elapsed_ns: u64,
}

impl ExpectedReplayDigest {
    pub fn normalized_event_hash_hex(&self) -> String {
        hex::encode(self.normalized_event_hash)
    }

    pub fn book_state_hash_hex(&self) -> String {
        hex::encode(self.book_state_hash)
    }

    pub fn quality_incident_hash_hex(&self) -> String {
        hex::encode(self.quality_incident_hash)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ReplayRecord {
    pub(crate) stream_id: NonZeroU32,
    pub(crate) stream_name: String,
    pub(crate) kind: ReplayRecordKind,
    pub(crate) record_sequence: NonZeroU64,
    pub(crate) receive_wall_time: UnixNanos,
    pub(crate) receive_monotonic_ns: u64,
    pub(crate) payload: Box<[u8]>,
}

impl fmt::Debug for ReplayRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplayRecord")
            .field("stream_id", &self.stream_id)
            .field("stream_name", &self.stream_name)
            .field("kind", &self.kind)
            .field("record_sequence", &self.record_sequence)
            .field("receive_wall_time", &self.receive_wall_time)
            .field("receive_monotonic_ns", &self.receive_monotonic_ns)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayConfig {
    pub(crate) deterministic_seed: u64,
    pub(crate) wal_segment_id: [u8; 16],
    pub(crate) source: SourceId,
    pub(crate) connection_epoch: NonZeroU64,
    pub(crate) subscription_epoch: NonZeroU64,
    pub(crate) connection_started_at: UnixNanos,
    pub(crate) normalization_offset_ns: i64,
    pub(crate) max_levels_per_side: usize,
    pub(crate) max_buffered_deltas: usize,
    pub(crate) max_buffered_level_updates: usize,
    pub(crate) stop_at_receive_monotonic_ns: Option<u64>,
    pub(crate) records: Vec<ReplayRecord>,
    expected: ExpectedReplayDigest,
}

impl ReplayConfig {
    pub fn from_manifest(path: impl AsRef<Path>) -> Result<Self, ReplayError> {
        let workspace_root = approved_workspace_root()?;
        let manifest_path = approved_manifest_path(
            &workspace_root,
            path.as_ref(),
            Path::new("fixtures/golden-replays"),
        )?;
        let manifest_bytes = read_bounded_regular(&manifest_path, MAX_MANIFEST_BYTES)?;
        let manifest_text = std::str::from_utf8(&manifest_bytes)
            .map_err(|_| ReplayError::InvalidManifest("utf8"))?;
        let raw = toml::from_str::<RawReplayManifest>(manifest_text)?;
        let artifact_id = manifest_path
            .strip_prefix(&workspace_root)
            .map_err(|_| ReplayError::UnapprovedPath("manifest root"))?
            .to_str()
            .ok_or(ReplayError::InvalidManifest("artifact id"))?;
        Self::from_raw(raw, &workspace_root, artifact_id)
    }

    pub const fn expected(&self) -> &ExpectedReplayDigest {
        &self.expected
    }

    fn from_raw(
        raw: RawReplayManifest,
        workspace_root: &Path,
        expected_artifact_id: &str,
    ) -> Result<Self, ReplayError> {
        if raw.schema_version != 1
            || raw.artifact_id != expected_artifact_id
            || !raw.artifact_id.starts_with("fixtures/golden-replays/")
            || raw.status != "implemented"
            || !raw.local_only
            || raw.venue != "binance"
            || raw.market != "spot"
            || raw.source_name != "binance"
            || raw.source_generation != 1
            || raw.deterministic_seed == 0
        {
            return Err(ReplayError::InvalidManifest("identity"));
        }
        if raw.max_records == 0
            || raw.max_records > MAX_RECORDS
            || raw.records.len() != raw.max_records
            || raw.max_total_input_bytes == 0
            || raw.max_total_input_bytes > MAX_TOTAL_INPUT_BYTES
            || !(1..=MAX_LEVELS_PER_SIDE).contains(&raw.max_levels_per_side)
            || !(1..=MAX_BUFFERED_DELTAS).contains(&raw.max_buffered_deltas)
            || !(1..=MAX_BUFFERED_LEVEL_UPDATES).contains(&raw.max_buffered_level_updates)
        {
            return Err(ReplayError::InvalidManifest("bounds"));
        }
        let wal_segment_id = decode_array::<16>(&raw.wal_segment_id_hex, "wal segment id")?;
        let source = SourceId::new(
            SourceKind::Exchange,
            &raw.source_name,
            raw.source_generation,
        )
        .map_err(|_| ReplayError::InvalidManifest("source"))?;
        let connection_epoch = NonZeroU64::new(raw.connection_epoch)
            .ok_or(ReplayError::InvalidManifest("connection epoch"))?;
        let subscription_epoch = NonZeroU64::new(raw.subscription_epoch)
            .ok_or(ReplayError::InvalidManifest("subscription epoch"))?;
        if raw.connection_started_at_unix_nanos <= 0 || raw.normalization_offset_ns <= 0 {
            return Err(ReplayError::InvalidManifest("time policy"));
        }
        let connection_started_at = UnixNanos::new(raw.connection_started_at_unix_nanos);

        let mut total_bytes = 0_u64;
        let mut previous_monotonic = None;
        let mut previous_sequence = None;
        let mut streams = BTreeSet::new();
        let mut records = Vec::with_capacity(raw.records.len());
        for record in raw.records {
            if record.stream_name.is_empty()
                || record.stream_name.len() > 256
                || record.stream_name.chars().any(char::is_control)
                || record.receive_wall_time_ns < raw.connection_started_at_unix_nanos
                || record.receive_monotonic_ns == 0
                || previous_monotonic
                    .is_some_and(|previous| record.receive_monotonic_ns <= previous)
                || previous_sequence.is_some_and(|previous| record.record_sequence <= previous)
            {
                return Err(ReplayError::InvalidManifest("record ordering"));
            }
            let stream_id = NonZeroU32::new(record.stream_id)
                .ok_or(ReplayError::InvalidManifest("stream id"))?;
            let record_sequence = NonZeroU64::new(record.record_sequence)
                .ok_or(ReplayError::InvalidManifest("record sequence"))?;
            streams.insert((record.stream_id, record.stream_name.clone()));
            let path = approved_regular_path(
                workspace_root,
                Path::new(&record.path),
                Path::new("fixtures/exchanges/binance"),
            )?;
            let payload = read_bounded_regular(&path, MAX_INPUT_BYTES)?;
            if sha256(&payload) != decode_array::<32>(&record.sha256, "input sha256")? {
                return Err(ReplayError::InputHashMismatch);
            }
            total_bytes = total_bytes
                .checked_add(u64::try_from(payload.len()).map_err(|_| ReplayError::Arithmetic)?)
                .ok_or(ReplayError::Arithmetic)?;
            if total_bytes > raw.max_total_input_bytes {
                return Err(ReplayError::InvalidManifest("total input bytes"));
            }
            previous_monotonic = Some(record.receive_monotonic_ns);
            previous_sequence = Some(record.record_sequence);
            records.push(ReplayRecord {
                stream_id,
                stream_name: record.stream_name,
                kind: record.kind,
                record_sequence,
                receive_wall_time: UnixNanos::new(record.receive_wall_time_ns),
                receive_monotonic_ns: record.receive_monotonic_ns,
                payload: payload.into_boxed_slice(),
            });
        }
        if streams.len() != 1 {
            return Err(ReplayError::InvalidManifest("stream identity"));
        }
        if raw
            .stop_at_receive_monotonic_ns
            .is_some_and(|stop| stop < records[0].receive_monotonic_ns)
        {
            return Err(ReplayError::InvalidManifest("stop boundary"));
        }
        let normalized_event_hash =
            decode_array::<32>(&raw.expected.normalized_event_hash, "expected event hash")?;
        let book_state_hash =
            decode_array::<32>(&raw.expected.book_state_hash, "expected book hash")?;
        let quality_incident_hash =
            decode_array::<32>(&raw.expected.quality_incident_hash, "expected quality hash")?;
        if normalized_event_hash == [0; 32]
            || book_state_hash == [0; 32]
            || quality_incident_hash == [0; 32]
        {
            return Err(ReplayError::InvalidManifest("expected digest"));
        }

        Ok(Self {
            deterministic_seed: raw.deterministic_seed,
            wal_segment_id,
            source,
            connection_epoch,
            subscription_epoch,
            connection_started_at,
            normalization_offset_ns: raw.normalization_offset_ns,
            max_levels_per_side: raw.max_levels_per_side,
            max_buffered_deltas: raw.max_buffered_deltas,
            max_buffered_level_updates: raw.max_buffered_level_updates,
            stop_at_receive_monotonic_ns: raw.stop_at_receive_monotonic_ns,
            records,
            expected: ExpectedReplayDigest {
                normalized_event_hash,
                book_state_hash,
                quality_incident_hash,
                replayed_records: raw.expected.replayed_records,
                normalized_events: raw.expected.normalized_events,
                quality_incidents: raw.expected.quality_incidents,
                silent_integrity_failures: raw.expected.silent_integrity_failures,
                logical_elapsed_ns: raw.expected.logical_elapsed_ns,
            },
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReplayManifest {
    schema_version: u32,
    artifact_id: String,
    status: String,
    local_only: bool,
    venue: String,
    market: String,
    source_name: String,
    source_generation: u32,
    deterministic_seed: u64,
    wal_segment_id_hex: String,
    connection_epoch: u64,
    subscription_epoch: u64,
    connection_started_at_unix_nanos: i64,
    normalization_offset_ns: i64,
    max_records: usize,
    max_total_input_bytes: u64,
    max_levels_per_side: usize,
    max_buffered_deltas: usize,
    max_buffered_level_updates: usize,
    stop_at_receive_monotonic_ns: Option<u64>,
    expected: RawExpectedDigest,
    #[serde(rename = "record")]
    records: Vec<RawReplayRecord>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExpectedDigest {
    normalized_event_hash: String,
    book_state_hash: String,
    quality_incident_hash: String,
    replayed_records: usize,
    normalized_events: usize,
    quality_incidents: usize,
    silent_integrity_failures: u64,
    logical_elapsed_ns: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReplayRecord {
    stream_id: u32,
    stream_name: String,
    kind: ReplayRecordKind,
    path: String,
    sha256: String,
    record_sequence: u64,
    receive_wall_time_ns: i64,
    receive_monotonic_ns: u64,
}

fn approved_workspace_root() -> Result<PathBuf, ReplayError> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    if !root.join("Cargo.toml").is_file() {
        return Err(ReplayError::UnapprovedPath("workspace root"));
    }
    Ok(root)
}

fn approved_regular_path(
    workspace_root: &Path,
    requested: &Path,
    required_prefix: &Path,
) -> Result<PathBuf, ReplayError> {
    if requested.as_os_str().is_empty()
        || requested.is_absolute()
        || requested
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ReplayError::UnapprovedPath("path shape"));
    }
    let candidate = workspace_root.join(requested);
    let metadata = fs::symlink_metadata(&candidate)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ReplayError::UnapprovedPath("regular file"));
    }
    let canonical = candidate.canonicalize()?;
    let approved_prefix = workspace_root.join(required_prefix).canonicalize()?;
    if !canonical.starts_with(&approved_prefix) {
        return Err(ReplayError::UnapprovedPath("prefix"));
    }
    Ok(canonical)
}

fn approved_manifest_path(
    workspace_root: &Path,
    requested: &Path,
    required_prefix: &Path,
) -> Result<PathBuf, ReplayError> {
    if !requested.is_absolute() {
        return approved_regular_path(workspace_root, requested, required_prefix);
    }
    let metadata = fs::symlink_metadata(requested)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ReplayError::UnapprovedPath("regular file"));
    }
    let canonical = requested.canonicalize()?;
    let approved_prefix = workspace_root.join(required_prefix).canonicalize()?;
    if !canonical.starts_with(&approved_prefix) {
        return Err(ReplayError::UnapprovedPath("prefix"));
    }
    Ok(canonical)
}

fn read_bounded_regular(path: &Path, maximum: u64) -> Result<Vec<u8>, ReplayError> {
    let mut file = open_nofollow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(ReplayError::InvalidManifest("file size"));
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| ReplayError::Arithmetic)?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref().take(maximum + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_err(|_| ReplayError::Arithmetic)? > maximum {
        return Err(ReplayError::InvalidManifest("file size"));
    }
    Ok(bytes)
}

fn open_nofollow(path: &Path) -> Result<File, ReplayError> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?)
}

fn decode_array<const N: usize>(
    encoded: &str,
    field: &'static str,
) -> Result<[u8; N], ReplayError> {
    if encoded.len() != N * 2
        || encoded
            .bytes()
            .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
    {
        return Err(ReplayError::InvalidManifest(field));
    }
    let mut bytes = [0_u8; N];
    hex::decode_to_slice(encoded, &mut bytes).map_err(|_| ReplayError::InvalidManifest(field))?;
    Ok(bytes)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTIFACT_ID: &str = "fixtures/golden-replays/market-foundation/manifest.toml";

    fn raw_manifest() -> RawReplayManifest {
        toml::from_str(include_str!(
            "../../../fixtures/golden-replays/market-foundation/manifest.toml"
        ))
        .expect("checked-in manifest")
    }

    #[test]
    fn rejects_a_tampered_input_hash_before_replay() {
        let mut raw = raw_manifest();
        raw.records[0].sha256 =
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned();

        let error = ReplayConfig::from_raw(
            raw,
            &approved_workspace_root().expect("workspace"),
            ARTIFACT_ID,
        )
        .expect_err("tampered input must fail");

        assert!(matches!(error, ReplayError::InputHashMismatch));
    }

    #[test]
    fn rejects_uninitialized_expected_hashes() {
        let mut raw = raw_manifest();
        raw.expected.book_state_hash =
            "0000000000000000000000000000000000000000000000000000000000000000".to_owned();

        let error = ReplayConfig::from_raw(
            raw,
            &approved_workspace_root().expect("workspace"),
            ARTIFACT_ID,
        )
        .expect_err("zero expected digest must fail");

        assert!(matches!(
            error,
            ReplayError::InvalidManifest("expected digest")
        ));
    }

    #[test]
    fn rejects_an_unsupported_source_generation_at_the_manifest_boundary() {
        let mut raw = raw_manifest();
        raw.source_generation = 2;

        let error = ReplayConfig::from_raw(
            raw,
            &approved_workspace_root().expect("workspace"),
            ARTIFACT_ID,
        )
        .expect_err("unsupported generation must fail");

        assert!(matches!(error, ReplayError::InvalidManifest("identity")));
    }

    #[test]
    fn debug_output_reports_payload_lengths_without_payload_bytes() {
        let config = ReplayConfig::from_manifest(ARTIFACT_ID).expect("manifest");
        let rendered = format!("{config:?}");

        assert!(!rendered.contains("payload: ["));
        assert!(rendered.contains("payload_len:"));
    }
}
