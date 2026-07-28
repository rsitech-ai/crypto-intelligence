//! Fail-closed WAL recovery with bounded streaming reads and final-tail repair only.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};

use thiserror::Error;

use crate::frame::{
    self, CHECKSUM_LENGTH, FrameError, HEADER_LENGTH, LEGACY_HEADER_LENGTH, LEGACY_SCHEMA_VERSION,
    MAGIC, MAX_PAYLOAD_LENGTH, RecordMetadata, SCHEMA_VERSION, WalFormat, decode,
};
use crate::prologue::{PrologueError, SegmentMetadata};

pub const TARGET_SEGMENT_LENGTH: u64 = 256 * 1024 * 1024;
pub const MAX_RECOVERABLE_SEGMENT_LENGTH: u64 = TARGET_SEGMENT_LENGTH
    + HEADER_LENGTH as u64
    + MAX_PAYLOAD_LENGTH as u64
    + CHECKSUM_LENGTH as u64;
pub const MAX_COLLECTED_RECORDS: usize = 65_536;
pub const MAX_COLLECTED_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_TRACKED_STREAM_EPOCHS: usize = 4_096;

const SEARCH_BUFFER_LENGTH: usize = 64 * 1024;
const MAX_MAGIC_CANDIDATES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorruptionKind {
    InvalidMagic,
    InvalidHeaderLength,
    UnsupportedFlags,
    HeaderChecksumMismatch,
    FrameChecksumMismatch,
    PayloadTooLarge,
    IncompleteLegacyFrame,
    InvalidPartialTail,
    MixedFormat,
}

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("WAL I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("WAL segment prologue failed validation: {0}")]
    Prologue(#[from] PrologueError),
    #[error("WAL corruption at byte offset {offset}: {kind:?}")]
    Corruption { offset: u64, kind: CorruptionKind },
    #[error("unsupported WAL format version {version} at byte offset {offset}")]
    UnsupportedFormat { offset: u64, version: u16 },
    #[error(
        "WAL record sequence regressed at byte offset {offset} for stream {stream_id} epoch {connection_epoch}: previous {previous}, current {current}"
    )]
    SequenceRegression {
        offset: u64,
        stream_id: u32,
        connection_epoch: u64,
        previous: u64,
        current: u64,
    },
    #[error("WAL segment is {actual} bytes, exceeding the recovery maximum {maximum}")]
    SegmentTooLarge { actual: u64, maximum: u64 },
    #[error("WAL recovery cannot reserve {requested} bytes safely")]
    Capacity { requested: u64 },
    #[error(
        "collected WAL recovery exceeds {maximum_records} records or {maximum_payload_bytes} payload bytes"
    )]
    CollectionLimit {
        maximum_records: usize,
        maximum_payload_bytes: usize,
    },
    #[error("WAL exceeds the maximum {maximum} tracked stream/epoch pairs")]
    StreamEpochLimit { maximum: usize },
    #[error("WAL record at byte offset {offset} references undeclared stream ID {stream_id}")]
    UndeclaredStream { offset: u64, stream_id: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoveredRecord<'a> {
    metadata: Option<RecordMetadata>,
    payload: &'a [u8],
    offset: u64,
    next_offset: u64,
}

impl<'a> RecoveredRecord<'a> {
    pub const fn metadata(&self) -> Option<RecordMetadata> {
        self.metadata
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn next_offset(&self) -> u64 {
        self.next_offset
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveredRecordOwned {
    metadata: Option<RecordMetadata>,
    payload: Vec<u8>,
    offset: u64,
    next_offset: u64,
}

impl RecoveredRecordOwned {
    pub const fn metadata(&self) -> Option<RecordMetadata> {
        self.metadata
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn next_offset(&self) -> u64 {
        self.next_offset
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecoverySummary {
    format: Option<WalFormat>,
    record_count: u64,
    last_valid_offset: u64,
    truncated_bytes: u64,
}

impl RecoverySummary {
    pub const fn format(&self) -> Option<WalFormat> {
        self.format
    }

    pub const fn record_count(&self) -> u64 {
        self.record_count
    }

    pub const fn last_valid_offset(&self) -> u64 {
        self.last_valid_offset
    }

    pub const fn truncated_bytes(&self) -> u64 {
        self.truncated_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    records: Vec<RecoveredRecordOwned>,
    summary: RecoverySummary,
}

impl RecoveryReport {
    pub fn records(&self) -> &[RecoveredRecordOwned] {
        &self.records
    }

    pub const fn summary(&self) -> RecoverySummary {
        self.summary
    }

    pub const fn truncated_bytes(&self) -> u64 {
        self.summary.truncated_bytes()
    }
}

pub fn recover(file: &mut File) -> Result<RecoveryReport, RecoveryError> {
    recover_from(file, 0, None, None)
}

pub(crate) fn recover_from(
    file: &mut File,
    record_start_offset: u64,
    expected_format: Option<WalFormat>,
    segment_metadata: Option<&SegmentMetadata>,
) -> Result<RecoveryReport, RecoveryError> {
    let mut records = Vec::new();
    let mut payload_bytes = 0_usize;
    let mut collection_exceeded = false;
    let summary = recover_with_from(
        file,
        record_start_offset,
        expected_format,
        segment_metadata,
        |record| {
            let next_payload_bytes = payload_bytes.checked_add(record.payload().len());
            if collection_exceeded
                || records.len() >= MAX_COLLECTED_RECORDS
                || next_payload_bytes.is_none()
                || next_payload_bytes.is_some_and(|bytes| bytes > MAX_COLLECTED_PAYLOAD_BYTES)
            {
                collection_exceeded = true;
                return;
            }
            payload_bytes = next_payload_bytes.expect("checked payload length must be present");
            records.push(RecoveredRecordOwned {
                metadata: record.metadata(),
                payload: record.payload().to_vec(),
                offset: record.offset(),
                next_offset: record.next_offset(),
            });
        },
    )?;
    if collection_exceeded {
        return Err(RecoveryError::CollectionLimit {
            maximum_records: MAX_COLLECTED_RECORDS,
            maximum_payload_bytes: MAX_COLLECTED_PAYLOAD_BYTES,
        });
    }
    Ok(RecoveryReport { records, summary })
}

pub fn recover_with(
    file: &mut File,
    visitor: impl FnMut(RecoveredRecord<'_>),
) -> Result<RecoverySummary, RecoveryError> {
    recover_with_from(file, 0, None, None, visitor)
}

pub(crate) fn recover_with_from(
    file: &mut File,
    record_start_offset: u64,
    expected_format: Option<WalFormat>,
    segment_metadata: Option<&SegmentMetadata>,
    mut visitor: impl FnMut(RecoveredRecord<'_>),
) -> Result<RecoverySummary, RecoveryError> {
    let actual_length = file.metadata()?.len();
    let maximum_length = MAX_RECOVERABLE_SEGMENT_LENGTH
        .checked_add(record_start_offset)
        .ok_or(RecoveryError::Capacity {
            requested: u64::MAX,
        })?;
    if actual_length > maximum_length {
        return Err(RecoveryError::SegmentTooLarge {
            actual: actual_length,
            maximum: maximum_length,
        });
    }
    if record_start_offset > actual_length {
        return Err(corruption(
            record_start_offset,
            CorruptionKind::InvalidPartialTail,
        ));
    }

    let mut frame_bytes = Vec::new();
    let mut header_bytes = [0_u8; HEADER_LENGTH];
    let mut last_sequences = BTreeMap::new();
    let mut segment_format = expected_format;
    let mut record_count = 0_u64;
    let mut offset = record_start_offset;

    while offset < actual_length {
        let remaining = actual_length - offset;
        let prefix_length = (MAGIC.len() + size_of::<u16>()) as u64;
        if remaining < prefix_length {
            let tail = read_tail(file, offset, remaining)?;
            if valid_common_prefix(&tail, segment_format) {
                return repair_tail(file, segment_format, record_count, offset, actual_length);
            }
            return Err(corruption(offset, CorruptionKind::InvalidPartialTail));
        }

        let mut prefix = [0_u8; MAGIC.len() + size_of::<u16>()];
        read_exact_at(file, offset, &mut prefix)?;
        if prefix[..MAGIC.len()] != MAGIC {
            return Err(corruption(offset, CorruptionKind::InvalidMagic));
        }
        let version = u16::from_be_bytes(
            prefix[MAGIC.len()..]
                .try_into()
                .expect("fixed version prefix is in bounds"),
        );
        let format = match version {
            LEGACY_SCHEMA_VERSION => WalFormat::LegacyV1,
            SCHEMA_VERSION => WalFormat::V2,
            unsupported => {
                return Err(RecoveryError::UnsupportedFormat {
                    offset,
                    version: unsupported,
                });
            }
        };
        if segment_format.is_some_and(|current| current != format) {
            return Err(corruption(offset, CorruptionKind::MixedFormat));
        }
        segment_format.get_or_insert(format);

        let header_length = match format {
            WalFormat::LegacyV1 => LEGACY_HEADER_LENGTH,
            WalFormat::V2 => HEADER_LENGTH,
        };
        if remaining < header_length as u64 {
            let tail = read_tail(file, offset, remaining)?;
            if is_repairable_partial_header(&tail, format) {
                return repair_tail(file, segment_format, record_count, offset, actual_length);
            }
            return Err(corruption(offset, CorruptionKind::InvalidPartialTail));
        }

        let header = &mut header_bytes[..header_length];
        read_exact_at(file, offset, header)?;
        let payload_length = match format {
            WalFormat::LegacyV1 => frame::decode_legacy_header(header)
                .map_err(|error| map_frame_error(offset, error))?,
            WalFormat::V2 => {
                frame::decode_v2_header(header)
                    .map_err(|error| map_frame_error(offset, error))?
                    .1
            }
        };
        let encoded_length = header_length
            .checked_add(payload_length)
            .and_then(|length| length.checked_add(CHECKSUM_LENGTH))
            .ok_or_else(|| corruption(offset, CorruptionKind::PayloadTooLarge))?;
        let next_offset = offset
            .checked_add(
                u64::try_from(encoded_length).map_err(|_| RecoveryError::Capacity {
                    requested: u64::MAX,
                })?,
            )
            .ok_or_else(|| corruption(offset, CorruptionKind::PayloadTooLarge))?;

        if next_offset > actual_length {
            if format == WalFormat::V2
                && !contains_later_valid_frame(file, offset + 1, actual_length)?
            {
                return repair_tail(file, segment_format, record_count, offset, actual_length);
            }
            return Err(corruption(offset, CorruptionKind::IncompleteLegacyFrame));
        }

        frame_bytes.clear();
        frame_bytes
            .try_reserve_exact(encoded_length)
            .map_err(|_| RecoveryError::Capacity {
                requested: encoded_length as u64,
            })?;
        frame_bytes.resize(encoded_length, 0);
        read_exact_at(file, offset, &mut frame_bytes)?;
        let decoded = decode(&frame_bytes).map_err(|error| map_frame_error(offset, error))?;
        if decoded.format() != format {
            return Err(corruption(offset, CorruptionKind::MixedFormat));
        }
        if let Some(metadata) = decoded.metadata() {
            if segment_metadata.is_some_and(|segment| !segment.contains_stream(metadata.stream_id))
            {
                return Err(RecoveryError::UndeclaredStream {
                    offset,
                    stream_id: metadata.stream_id,
                });
            }
            validate_sequence(&mut last_sequences, metadata, offset)?;
        }

        record_count = record_count.checked_add(1).ok_or(RecoveryError::Capacity {
            requested: u64::MAX,
        })?;
        visitor(RecoveredRecord {
            metadata: decoded.metadata(),
            payload: decoded.payload(),
            offset,
            next_offset,
        });
        offset = next_offset;
    }

    file.seek(SeekFrom::End(0))?;
    Ok(RecoverySummary {
        format: segment_format,
        record_count,
        last_valid_offset: offset,
        truncated_bytes: 0,
    })
}

fn validate_sequence(
    last_sequences: &mut BTreeMap<(u32, u64), u64>,
    metadata: RecordMetadata,
    offset: u64,
) -> Result<(), RecoveryError> {
    let key = (metadata.stream_id, metadata.connection_epoch);
    if !last_sequences.contains_key(&key) && last_sequences.len() >= MAX_TRACKED_STREAM_EPOCHS {
        return Err(RecoveryError::StreamEpochLimit {
            maximum: MAX_TRACKED_STREAM_EPOCHS,
        });
    }
    if let Some(previous) = last_sequences.insert(key, metadata.record_sequence)
        && metadata.record_sequence <= previous
    {
        return Err(RecoveryError::SequenceRegression {
            offset,
            stream_id: metadata.stream_id,
            connection_epoch: metadata.connection_epoch,
            previous,
            current: metadata.record_sequence,
        });
    }
    Ok(())
}

fn repair_tail(
    file: &mut File,
    format: Option<WalFormat>,
    record_count: u64,
    last_valid_offset: u64,
    actual_length: u64,
) -> Result<RecoverySummary, RecoveryError> {
    let truncated_bytes = actual_length - last_valid_offset;
    file.set_len(last_valid_offset)?;
    file.seek(SeekFrom::End(0))?;
    file.sync_all()?;
    Ok(RecoverySummary {
        format,
        record_count,
        last_valid_offset,
        truncated_bytes,
    })
}

fn read_tail(file: &mut File, offset: u64, length: u64) -> Result<Vec<u8>, RecoveryError> {
    let length =
        usize::try_from(length).map_err(|_| RecoveryError::Capacity { requested: length })?;
    let mut bytes = vec![0_u8; length];
    read_exact_at(file, offset, &mut bytes)?;
    Ok(bytes)
}

fn read_exact_at(file: &mut File, offset: u64, bytes: &mut [u8]) -> Result<(), RecoveryError> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(bytes)?;
    Ok(())
}

fn map_frame_error(offset: u64, error: FrameError) -> RecoveryError {
    match error {
        FrameError::UnsupportedSchema(version) => {
            RecoveryError::UnsupportedFormat { offset, version }
        }
        FrameError::InvalidMagic => corruption(offset, CorruptionKind::InvalidMagic),
        FrameError::InvalidHeaderLength(_) => {
            corruption(offset, CorruptionKind::InvalidHeaderLength)
        }
        FrameError::UnsupportedFlags(_) => corruption(offset, CorruptionKind::UnsupportedFlags),
        FrameError::HeaderChecksumMismatch => {
            corruption(offset, CorruptionKind::HeaderChecksumMismatch)
        }
        FrameError::FrameChecksumMismatch => {
            corruption(offset, CorruptionKind::FrameChecksumMismatch)
        }
        FrameError::PayloadTooLarge => corruption(offset, CorruptionKind::PayloadTooLarge),
        FrameError::Incomplete => corruption(offset, CorruptionKind::InvalidPartialTail),
    }
}

const fn corruption(offset: u64, kind: CorruptionKind) -> RecoveryError {
    RecoveryError::Corruption { offset, kind }
}

fn is_repairable_partial_header(bytes: &[u8], format: WalFormat) -> bool {
    match format {
        WalFormat::LegacyV1 => is_valid_partial_legacy_header_prefix(bytes),
        WalFormat::V2 => frame::is_valid_partial_header_prefix(bytes),
    }
}

fn valid_common_prefix(bytes: &[u8], segment_format: Option<WalFormat>) -> bool {
    let legacy_prefix = [
        MAGIC.as_slice(),
        LEGACY_SCHEMA_VERSION.to_be_bytes().as_slice(),
    ]
    .concat();
    let current_prefix = [MAGIC.as_slice(), SCHEMA_VERSION.to_be_bytes().as_slice()].concat();
    match segment_format {
        Some(WalFormat::LegacyV1) => bytes == &legacy_prefix[..bytes.len()],
        Some(WalFormat::V2) => bytes == &current_prefix[..bytes.len()],
        None => bytes == &legacy_prefix[..bytes.len()] || bytes == &current_prefix[..bytes.len()],
    }
}

fn is_valid_partial_legacy_header_prefix(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() >= LEGACY_HEADER_LENGTH {
        return false;
    }
    let fixed_header = [
        MAGIC.as_slice(),
        LEGACY_SCHEMA_VERSION.to_be_bytes().as_slice(),
    ]
    .concat();
    let fixed_prefix_length = bytes.len().min(fixed_header.len());
    if bytes[..fixed_prefix_length] != fixed_header[..fixed_prefix_length] {
        return false;
    }
    if bytes.len() <= fixed_header.len() {
        return true;
    }

    let length_prefix = &bytes[fixed_header.len()..];
    let mut minimum_declared_length = [0_u8; size_of::<u32>()];
    minimum_declared_length[..length_prefix.len()].copy_from_slice(length_prefix);
    u32::from_be_bytes(minimum_declared_length) as usize <= MAX_PAYLOAD_LENGTH
}

fn contains_later_valid_frame(
    file: &mut File,
    start: u64,
    end: u64,
) -> Result<bool, RecoveryError> {
    let mut scan_offset = start;
    let mut carry = Vec::new();
    let mut candidate_count = 0_usize;
    while scan_offset < end {
        let read_length = usize::try_from((end - scan_offset).min(SEARCH_BUFFER_LENGTH as u64))
            .expect("bounded search chunk fits usize");
        let mut chunk = vec![0_u8; read_length];
        read_exact_at(file, scan_offset, &mut chunk)?;
        let carry_length = carry.len();
        carry.extend_from_slice(&chunk);
        let base_offset = scan_offset - carry_length as u64;
        for (relative, window) in carry.windows(MAGIC.len()).enumerate() {
            if window != MAGIC {
                continue;
            }
            let candidate_offset = base_offset + relative as u64;
            if candidate_offset < start {
                continue;
            }
            candidate_count += 1;
            if candidate_count > MAX_MAGIC_CANDIDATES {
                return Ok(true);
            }
            if valid_frame_at(file, candidate_offset, end)? {
                return Ok(true);
            }
        }
        let retained = carry.len().min(MAGIC.len() - 1);
        carry = carry[carry.len() - retained..].to_vec();
        scan_offset += read_length as u64;
    }
    Ok(false)
}

fn valid_frame_at(file: &mut File, offset: u64, end: u64) -> Result<bool, RecoveryError> {
    let prefix_length = MAGIC.len() + size_of::<u16>();
    if end - offset < prefix_length as u64 {
        return Ok(false);
    }
    let mut prefix = [0_u8; MAGIC.len() + size_of::<u16>()];
    read_exact_at(file, offset, &mut prefix)?;
    if prefix[..MAGIC.len()] != MAGIC {
        return Ok(false);
    }
    let version = u16::from_be_bytes(
        prefix[MAGIC.len()..]
            .try_into()
            .expect("fixed version prefix is in bounds"),
    );
    let header_length = match version {
        LEGACY_SCHEMA_VERSION => LEGACY_HEADER_LENGTH,
        SCHEMA_VERSION => HEADER_LENGTH,
        _ => return Ok(false),
    };
    if end - offset < header_length as u64 {
        return Ok(false);
    }
    let mut header = vec![0_u8; header_length];
    read_exact_at(file, offset, &mut header)?;
    let payload_length = match version {
        LEGACY_SCHEMA_VERSION => match frame::decode_legacy_header(&header) {
            Ok(length) => length,
            Err(_) => return Ok(false),
        },
        SCHEMA_VERSION => match frame::decode_v2_header(&header) {
            Ok((_, length)) => length,
            Err(_) => return Ok(false),
        },
        _ => return Ok(false),
    };
    let Some(encoded_length) = header_length
        .checked_add(payload_length)
        .and_then(|length| length.checked_add(CHECKSUM_LENGTH))
    else {
        return Ok(false);
    };
    if end - offset < encoded_length as u64 {
        return Ok(false);
    }
    let mut encoded = Vec::new();
    if encoded.try_reserve_exact(encoded_length).is_err() {
        return Ok(true);
    }
    encoded.resize(encoded_length, 0);
    read_exact_at(file, offset, &mut encoded)?;
    Ok(decode(&encoded).is_ok())
}
