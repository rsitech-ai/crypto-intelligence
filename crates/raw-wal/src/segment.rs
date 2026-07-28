//! Exclusively owned WAL segment lifecycle.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{FileExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::fs::{AtFlags, CWD, FlockOperation, Mode, OFlags, RenameFlags};
use thiserror::Error;

use crate::{
    frame::{
        self, FrameError, LEGACY_SCHEMA_VERSION, MAGIC as RECORD_MAGIC, RecordMetadata,
        SCHEMA_VERSION, WalFormat,
    },
    prologue::{self, PrologueError, SegmentMetadata},
    recovery::{
        self, MAX_TRACKED_STREAM_EPOCHS, RecoveredRecord, RecoveryError, RecoveryReport,
        RecoverySummary,
    },
};

const TEMP_CREATE_ATTEMPTS: u64 = 128;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum SegmentError {
    #[error("WAL frame encoding failed: {0}")]
    Frame(#[from] FrameError),
    #[error("WAL segment prologue failed validation: {0}")]
    Prologue(#[from] PrologueError),
    #[error("WAL recovery failed: {0}")]
    Recovery(#[from] RecoveryError),
    #[error("WAL I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("WAL segment already has a live owner")]
    AlreadyOpen,
    #[error("WAL segment format does not match the requested append format")]
    FormatMismatch,
    #[error("WAL segment has an invalid or incomplete format prefix")]
    InvalidFormat,
    #[error("segmented WAL v2 requires a valid segment prologue")]
    PrologueRequired,
    #[error("WAL record references undeclared stream ID {0}")]
    UndeclaredStream(u32),
    #[error(
        "WAL record sequence regressed for stream {stream_id} epoch {connection_epoch}: previous {previous}, current {current}"
    )]
    SequenceRegression {
        stream_id: u32,
        connection_epoch: u64,
        previous: u64,
        current: u64,
    },
    #[error("WAL exceeds the maximum {maximum} tracked stream/epoch pairs")]
    StreamEpochLimit { maximum: usize },
    #[error("WAL segment must be reopened after an append or sync failure")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurableOffset {
    segment_id: [u8; 16],
    frame_offset: u64,
    next_offset: u64,
}

impl DurableOffset {
    pub const fn segment_id(&self) -> &[u8; 16] {
        &self.segment_id
    }

    pub const fn frame_offset(&self) -> u64 {
        self.frame_offset
    }

    pub const fn next_offset(&self) -> u64 {
        self.next_offset
    }
}

pub struct Segment {
    file: File,
    segment_metadata: Option<SegmentMetadata>,
    record_start_offset: u64,
    v2_sequences: Option<BTreeMap<(u32, u64), u64>>,
    poisoned: bool,
}

impl Segment {
    pub(crate) fn file_identity(&self) -> Result<(u64, u64), SegmentError> {
        let stat = rustix::fs::fstat(&self.file).map_err(io::Error::from)?;
        let device = u64::try_from(stat.st_dev)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative device identity"))?;
        Ok((device, stat.st_ino))
    }

    pub(crate) fn prologue_identity(&self) -> Result<(u64, [u8; 32]), SegmentError> {
        let length = usize::try_from(self.record_start_offset)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "prologue length overflow"))?;
        let mut encoded = vec![0_u8; length];
        self.file.read_exact_at(&mut encoded, 0)?;
        Ok((self.record_start_offset, *blake3::hash(&encoded).as_bytes()))
    }

    pub fn open(path: &Path) -> Result<Self, SegmentError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        Self::from_file(file)
    }

    pub fn create_v2(path: &Path, metadata: SegmentMetadata) -> Result<Self, SegmentError> {
        let encoded = prologue::encode(&metadata)?;
        let parent = usable_parent(path)?;
        let mut temporary = create_temporary_segment(parent, path)?;
        let file = temporary.file_mut();
        acquire_exclusive_lock(file)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        rustix::fs::renameat_with(CWD, temporary.path(), CWD, path, RenameFlags::NOREPLACE)
            .map_err(io::Error::from)?;
        let file = temporary.into_file();
        File::open(parent)?.sync_all()?;
        Ok(Self {
            file,
            segment_metadata: Some(metadata),
            record_start_offset: encoded.len() as u64,
            v2_sequences: Some(BTreeMap::new()),
            poisoned: false,
        })
    }

    pub(crate) fn create_v2_at(
        directory: &File,
        file_name: &std::ffi::OsStr,
        metadata: SegmentMetadata,
    ) -> Result<Self, SegmentError> {
        let encoded = prologue::encode(&metadata)?;
        let (temporary_name, mut file) = create_temporary_segment_at(directory, file_name)?;
        let result = (|| -> Result<(), SegmentError> {
            acquire_exclusive_lock(&file)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            rustix::fs::renameat_with(
                directory,
                &temporary_name,
                directory,
                file_name,
                RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from)?;
            directory.sync_all()?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = rustix::fs::unlinkat(directory, &temporary_name, AtFlags::empty());
            return Err(error);
        }
        Ok(Self {
            file,
            segment_metadata: Some(metadata),
            record_start_offset: encoded.len() as u64,
            v2_sequences: Some(BTreeMap::new()),
            poisoned: false,
        })
    }

    pub fn open_v2(path: &Path) -> Result<Self, SegmentError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::open_v2_file(file)
    }

    pub(crate) fn open_v2_file(file: File) -> Result<Self, SegmentError> {
        let mut segment = Self::from_file(file)?;
        if !segment.has_prologue_magic()? {
            return Err(SegmentError::PrologueRequired);
        }
        segment.load_prologue()?;
        Ok(segment)
    }

    pub fn from_file(file: File) -> Result<Self, SegmentError> {
        acquire_exclusive_lock(&file)?;
        Ok(Self {
            file,
            segment_metadata: None,
            record_start_offset: 0,
            v2_sequences: None,
            poisoned: false,
        })
    }

    pub const fn segment_metadata(&self) -> Option<&SegmentMetadata> {
        self.segment_metadata.as_ref()
    }

    /// Writes one complete legacy-v1 frame and waits for `sync_data` before returning.
    pub fn append_synced(&mut self, payload: &[u8]) -> Result<u64, SegmentError> {
        self.ensure_writable()?;
        self.ensure_prologue_loaded_if_present()?;
        if self.segment_metadata.is_some()
            || self
                .record_only_format()?
                .is_some_and(|format| format != WalFormat::LegacyV1)
        {
            return Err(SegmentError::FormatMismatch);
        }
        let encoded = frame::encode_legacy(payload)?;
        let (frame_offset, next_offset) = self.append_encoded_range(&encoded)?;
        Ok(next_offset - frame_offset)
    }

    /// Writes one ordered segmented-v2 frame and waits for `sync_data` before returning.
    pub fn append_record_synced(
        &mut self,
        metadata: RecordMetadata,
        payload: &[u8],
    ) -> Result<DurableOffset, SegmentError> {
        self.ensure_writable()?;
        self.ensure_prologue_loaded_if_present()?;
        let segment_metadata = self
            .segment_metadata
            .as_ref()
            .ok_or(SegmentError::PrologueRequired)?;
        if !segment_metadata.contains_stream(metadata.stream_id) {
            return Err(SegmentError::UndeclaredStream(metadata.stream_id));
        }
        let segment_id = *segment_metadata.segment_id();
        self.ensure_v2_sequences()?;
        let key = (metadata.stream_id, metadata.connection_epoch);
        let sequences = self
            .v2_sequences
            .as_ref()
            .expect("v2 sequence state must be initialized");
        if !sequences.contains_key(&key) && sequences.len() >= MAX_TRACKED_STREAM_EPOCHS {
            return Err(SegmentError::StreamEpochLimit {
                maximum: MAX_TRACKED_STREAM_EPOCHS,
            });
        }
        if let Some(previous) = sequences.get(&key).copied()
            && metadata.record_sequence <= previous
        {
            return Err(SegmentError::SequenceRegression {
                stream_id: metadata.stream_id,
                connection_epoch: metadata.connection_epoch,
                previous,
                current: metadata.record_sequence,
            });
        }

        let encoded = frame::encode(metadata, payload)?;
        let (frame_offset, next_offset) = self.append_encoded_range(&encoded)?;
        self.v2_sequences
            .as_mut()
            .expect("v2 sequence state must be initialized")
            .insert(key, metadata.record_sequence);
        Ok(DurableOffset {
            segment_id,
            frame_offset,
            next_offset,
        })
    }

    pub fn recover(&mut self) -> Result<RecoveryReport, RecoveryError> {
        self.ensure_prologue_for_recovery()?;
        let report = if self.segment_metadata.is_some() {
            recovery::recover_from(
                &mut self.file,
                self.record_start_offset,
                Some(WalFormat::V2),
                self.segment_metadata.as_ref(),
            )?
        } else {
            recovery::recover(&mut self.file)?
        };
        self.update_sequence_state_from_owned(report.records(), report.summary());
        Ok(report)
    }

    pub fn recover_with(
        &mut self,
        mut visitor: impl FnMut(RecoveredRecord<'_>),
    ) -> Result<RecoverySummary, RecoveryError> {
        self.ensure_prologue_for_recovery()?;
        let mut sequences = BTreeMap::new();
        let mut visit = |record: RecoveredRecord<'_>| {
            if let Some(metadata) = record.metadata() {
                sequences.insert(
                    (metadata.stream_id, metadata.connection_epoch),
                    metadata.record_sequence,
                );
            }
            visitor(record);
        };
        let summary = if self.segment_metadata.is_some() {
            recovery::recover_with_from(
                &mut self.file,
                self.record_start_offset,
                Some(WalFormat::V2),
                self.segment_metadata.as_ref(),
                &mut visit,
            )?
        } else {
            recovery::recover_with(&mut self.file, &mut visit)?
        };
        self.v2_sequences = (summary.format() == Some(WalFormat::V2)).then_some(sequences);
        Ok(summary)
    }

    pub fn sync(&mut self) -> Result<(), io::Error> {
        match self.file.sync_data() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    pub(crate) fn sync_all(&mut self) -> Result<(), io::Error> {
        match self.file.sync_all() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.poisoned = true;
                Err(error)
            }
        }
    }

    pub(crate) fn verify_without_repair(
        &mut self,
        visitor: impl FnMut(RecoveredRecord<'_>),
    ) -> Result<RecoverySummary, RecoveryError> {
        self.ensure_prologue_for_recovery()?;
        let metadata = self
            .segment_metadata
            .as_ref()
            .ok_or_else(|| RecoveryError::Io(io::Error::other(SegmentError::PrologueRequired)))?;
        recovery::verify_with_from(
            &mut self.file,
            self.record_start_offset,
            Some(WalFormat::V2),
            Some(metadata),
            visitor,
        )
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) const fn file(&self) -> &File {
        &self.file
    }

    fn append_encoded_range(&mut self, encoded: &[u8]) -> Result<(u64, u64), SegmentError> {
        let result = (|| -> io::Result<(u64, u64)> {
            let frame_offset = self.file.seek(SeekFrom::End(0))?;
            self.file.write_all(encoded)?;
            self.file.sync_data()?;
            let next_offset = frame_offset
                .checked_add(encoded.len() as u64)
                .ok_or_else(|| io::Error::other("WAL offset overflow"))?;
            Ok((frame_offset, next_offset))
        })();
        match result {
            Ok(offsets) => Ok(offsets),
            Err(error) => {
                self.poisoned = true;
                Err(SegmentError::Io(error))
            }
        }
    }

    fn ensure_writable(&self) -> Result<(), SegmentError> {
        if self.poisoned {
            Err(SegmentError::Poisoned)
        } else {
            Ok(())
        }
    }

    fn ensure_v2_sequences(&mut self) -> Result<(), SegmentError> {
        if self.v2_sequences.is_some() {
            return Ok(());
        }
        let mut sequences = BTreeMap::new();
        let summary = recovery::recover_with_from(
            &mut self.file,
            self.record_start_offset,
            Some(WalFormat::V2),
            self.segment_metadata.as_ref(),
            |record| {
                if let Some(metadata) = record.metadata() {
                    sequences.insert(
                        (metadata.stream_id, metadata.connection_epoch),
                        metadata.record_sequence,
                    );
                }
            },
        )?;
        if summary.format() != Some(WalFormat::V2) {
            return Err(SegmentError::FormatMismatch);
        }
        self.v2_sequences = Some(sequences);
        Ok(())
    }

    fn update_sequence_state_from_owned(
        &mut self,
        records: &[recovery::RecoveredRecordOwned],
        summary: RecoverySummary,
    ) {
        if summary.format() != Some(WalFormat::V2) {
            self.v2_sequences = None;
            return;
        }
        let sequences = records
            .iter()
            .filter_map(|record| record.metadata())
            .map(|metadata| {
                (
                    (metadata.stream_id, metadata.connection_epoch),
                    metadata.record_sequence,
                )
            })
            .collect();
        self.v2_sequences = Some(sequences);
    }

    fn ensure_prologue_loaded_if_present(&mut self) -> Result<(), SegmentError> {
        if self.segment_metadata.is_none() && self.has_prologue_magic()? {
            self.load_prologue()?;
        }
        Ok(())
    }

    fn ensure_prologue_for_recovery(&mut self) -> Result<(), RecoveryError> {
        self.ensure_prologue_loaded_if_present()
            .map_err(segment_error_to_recovery)
    }

    fn load_prologue(&mut self) -> Result<(), SegmentError> {
        let file_length = self.file.metadata()?.len();
        let prefix_length = 12_usize;
        let available_prefix = usize::try_from(file_length.min(prefix_length as u64))
            .expect("bounded prologue prefix fits usize");
        let mut prefix = vec![0_u8; available_prefix];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut prefix)?;
        let header_length = prologue::declared_header_length(&prefix)?;
        if file_length < header_length as u64 {
            return Err(SegmentError::Prologue(PrologueError::Incomplete));
        }
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(header_length)
            .map_err(|_| PrologueError::HeaderTooLarge)?;
        encoded.resize(header_length, 0);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut encoded)?;
        let decoded = prologue::decode(&encoded)?;
        self.record_start_offset = decoded.encoded_length() as u64;
        self.segment_metadata = Some(decoded.into_metadata());
        self.v2_sequences = None;
        Ok(())
    }

    fn has_prologue_magic(&mut self) -> Result<bool, io::Error> {
        if self.file.metadata()?.len() < prologue::MAGIC.len() as u64 {
            return Ok(false);
        }
        let mut magic = [0_u8; prologue::MAGIC.len()];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut magic)?;
        Ok(magic == prologue::MAGIC)
    }

    fn record_only_format(&mut self) -> Result<Option<WalFormat>, SegmentError> {
        let length = self.file.metadata()?.len();
        if length == 0 {
            return Ok(None);
        }
        let prefix_length = RECORD_MAGIC.len() + size_of::<u16>();
        if length < prefix_length as u64 {
            return Err(SegmentError::InvalidFormat);
        }
        let mut prefix = [0_u8; RECORD_MAGIC.len() + size_of::<u16>()];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut prefix)?;
        if prefix[..RECORD_MAGIC.len()] != RECORD_MAGIC {
            return Err(SegmentError::InvalidFormat);
        }
        let version = u16::from_be_bytes(
            prefix[RECORD_MAGIC.len()..]
                .try_into()
                .expect("fixed WAL version prefix is in bounds"),
        );
        match version {
            LEGACY_SCHEMA_VERSION => Ok(Some(WalFormat::LegacyV1)),
            SCHEMA_VERSION => Ok(Some(WalFormat::V2)),
            _ => Err(SegmentError::InvalidFormat),
        }
    }
}

struct TemporarySegment {
    path: PathBuf,
    file: Option<File>,
}

impl TemporarySegment {
    fn path(&self) -> &Path {
        &self.path
    }

    fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("temporary segment file must remain present")
    }

    fn into_file(mut self) -> File {
        self.file
            .take()
            .expect("temporary segment file must remain present")
    }
}

impl Drop for TemporarySegment {
    fn drop(&mut self) {
        if self.file.is_some() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn usable_parent(path: &Path) -> Result<&Path, io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL path has no parent"))?;
    Ok(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })
}

fn create_temporary_segment(
    parent: &Path,
    final_path: &Path,
) -> Result<TemporarySegment, io::Error> {
    let file_name = final_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "WAL path has no file name"))?;
    let process_id = std::process::id();
    let first_sequence = TEMP_SEQUENCE.fetch_add(TEMP_CREATE_ATTEMPTS, Ordering::Relaxed);
    for attempt in 0..TEMP_CREATE_ATTEMPTS {
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(
            ".create-{process_id}-{}.tmp",
            first_sequence.wrapping_add(attempt)
        ));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&temporary_path)
        {
            Ok(file) => {
                return Ok(TemporarySegment {
                    path: temporary_path,
                    file: Some(file),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary WAL segment path",
    ))
}

fn create_temporary_segment_at(
    directory: &File,
    final_name: &std::ffi::OsStr,
) -> Result<(OsString, File), io::Error> {
    let process_id = std::process::id();
    let first_sequence = TEMP_SEQUENCE.fetch_add(TEMP_CREATE_ATTEMPTS, Ordering::Relaxed);
    for attempt in 0..TEMP_CREATE_ATTEMPTS {
        let mut temporary_name = OsString::from(".");
        temporary_name.push(final_name);
        temporary_name.push(format!(
            ".create-{process_id}-{}.tmp",
            first_sequence.wrapping_add(attempt)
        ));
        match rustix::fs::openat(
            directory,
            &temporary_name,
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(file) => return Ok((temporary_name, File::from(file))),
            Err(error) if error == rustix::io::Errno::EXIST => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary WAL segment name",
    ))
}

fn acquire_exclusive_lock(file: &File) -> Result<(), SegmentError> {
    match rustix::fs::flock(file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(()),
        Err(error) => {
            let error = io::Error::from(error);
            if error.kind() == io::ErrorKind::WouldBlock {
                Err(SegmentError::AlreadyOpen)
            } else {
                Err(SegmentError::Io(error))
            }
        }
    }
}

fn segment_error_to_recovery(error: SegmentError) -> RecoveryError {
    match error {
        SegmentError::Prologue(error) => RecoveryError::Prologue(error),
        SegmentError::Io(error) => RecoveryError::Io(error),
        SegmentError::Recovery(error) => error,
        _ => RecoveryError::Io(io::Error::other(error)),
    }
}
