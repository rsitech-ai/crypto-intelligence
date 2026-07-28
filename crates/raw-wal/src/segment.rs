//! Exclusively owned WAL segment lifecycle.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

use rustix::fs::FlockOperation;
use thiserror::Error;

use crate::{
    frame::{
        self, FrameError, LEGACY_SCHEMA_VERSION, MAGIC, RecordMetadata, SCHEMA_VERSION, WalFormat,
    },
    recovery::{
        self, MAX_TRACKED_STREAM_EPOCHS, RecoveredRecord, RecoveryError, RecoveryReport,
        RecoverySummary,
    },
};

#[derive(Debug, Error)]
pub enum SegmentError {
    #[error("WAL frame encoding failed: {0}")]
    Frame(#[from] FrameError),
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

pub struct Segment {
    file: File,
    v2_sequences: Option<BTreeMap<(u32, u64), u64>>,
    poisoned: bool,
}

impl Segment {
    pub fn open(path: &Path) -> Result<Self, SegmentError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        Self::from_file(file)
    }

    pub fn from_file(file: File) -> Result<Self, SegmentError> {
        acquire_exclusive_lock(&file)?;
        Ok(Self {
            file,
            v2_sequences: None,
            poisoned: false,
        })
    }

    /// Writes one complete legacy-v1 frame and waits for `sync_data` before returning.
    pub fn append_synced(&mut self, payload: &[u8]) -> Result<u64, SegmentError> {
        self.ensure_writable()?;
        if self
            .format()?
            .is_some_and(|format| format != WalFormat::LegacyV1)
        {
            return Err(SegmentError::FormatMismatch);
        }
        let encoded = frame::encode_legacy(payload)?;
        self.append_encoded(&encoded)
    }

    /// Writes one ordered schema-v2 frame and waits for `sync_data` before returning.
    pub fn append_record_synced(
        &mut self,
        metadata: RecordMetadata,
        payload: &[u8],
    ) -> Result<u64, SegmentError> {
        self.ensure_writable()?;
        if self.format()?.is_some_and(|format| format != WalFormat::V2) {
            return Err(SegmentError::FormatMismatch);
        }
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
        let written = self.append_encoded(&encoded)?;
        self.v2_sequences
            .as_mut()
            .expect("v2 sequence state must be initialized")
            .insert(key, metadata.record_sequence);
        Ok(written)
    }

    pub fn recover(&mut self) -> Result<RecoveryReport, RecoveryError> {
        let report = recovery::recover(&mut self.file)?;
        self.update_sequence_state_from_owned(report.records(), report.summary());
        Ok(report)
    }

    pub fn recover_with(
        &mut self,
        mut visitor: impl FnMut(RecoveredRecord<'_>),
    ) -> Result<RecoverySummary, RecoveryError> {
        let mut sequences = BTreeMap::new();
        let summary = recovery::recover_with(&mut self.file, |record| {
            if let Some(metadata) = record.metadata() {
                sequences.insert(
                    (metadata.stream_id, metadata.connection_epoch),
                    metadata.record_sequence,
                );
            }
            visitor(record);
        })?;
        self.v2_sequences = (summary.format() == Some(WalFormat::V2)).then_some(sequences);
        Ok(summary)
    }

    pub fn sync(&mut self) -> Result<(), io::Error> {
        self.file.sync_data()
    }

    fn append_encoded(&mut self, encoded: &[u8]) -> Result<u64, SegmentError> {
        let result = (|| -> io::Result<u64> {
            self.file.seek(SeekFrom::End(0))?;
            self.file.write_all(encoded)?;
            self.file.sync_data()?;
            Ok(encoded.len() as u64)
        })();
        match result {
            Ok(written) => Ok(written),
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
        let summary = recovery::recover_with(&mut self.file, |record| {
            if let Some(metadata) = record.metadata() {
                sequences.insert(
                    (metadata.stream_id, metadata.connection_epoch),
                    metadata.record_sequence,
                );
            }
        })?;
        if summary
            .format()
            .is_some_and(|format| format != WalFormat::V2)
        {
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

    fn format(&mut self) -> Result<Option<WalFormat>, SegmentError> {
        let length = self.file.metadata()?.len();
        if length == 0 {
            return Ok(None);
        }
        let prefix_length = MAGIC.len() + size_of::<u16>();
        if length < prefix_length as u64 {
            return Err(SegmentError::InvalidFormat);
        }
        let mut prefix = [0_u8; MAGIC.len() + size_of::<u16>()];
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut prefix)?;
        if prefix[..MAGIC.len()] != MAGIC {
            return Err(SegmentError::InvalidFormat);
        }
        let version = u16::from_be_bytes(
            prefix[MAGIC.len()..]
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
