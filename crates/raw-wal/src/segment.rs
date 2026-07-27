//! Owned WAL segment lifecycle.

use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    path::Path,
};

use thiserror::Error;

use crate::{
    frame::{self, FrameError},
    recovery::{self, RecoveryError, RecoveryReport},
};

#[derive(Debug, Error)]
pub enum SegmentError {
    #[error("WAL frame encoding failed: {0}")]
    Frame(#[from] FrameError),
    #[error("WAL I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub struct Segment {
    file: File,
}

impl Segment {
    pub fn open(path: &Path) -> Result<Self, io::Error> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        Ok(Self { file })
    }

    pub const fn from_file(file: File) -> Self {
        Self { file }
    }

    /// Writes one complete frame and waits for `sync_data` before returning.
    pub fn append_synced(&mut self, payload: &[u8]) -> Result<u64, SegmentError> {
        let encoded = frame::encode(payload)?;
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&encoded)?;
        self.file.sync_data()?;
        Ok(encoded.len() as u64)
    }

    pub fn recover(&mut self) -> Result<RecoveryReport, RecoveryError> {
        recovery::recover(&mut self.file)
    }

    pub fn sync(&mut self) -> Result<(), io::Error> {
        self.file.sync_data()
    }
}
