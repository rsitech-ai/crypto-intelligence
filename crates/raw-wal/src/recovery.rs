//! Fail-closed WAL recovery with final-tail repair only.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};

use thiserror::Error;

use crate::frame::{MAGIC, decode};

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("WAL I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("WAL corruption before the final frame at byte offset {offset}")]
    Corruption { offset: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryReport {
    records: Vec<Vec<u8>>,
    truncated_bytes: u64,
}

impl RecoveryReport {
    pub fn records(&self) -> &[Vec<u8>] {
        &self.records
    }

    pub const fn truncated_bytes(&self) -> u64 {
        self.truncated_bytes
    }
}

pub fn recover(file: &mut File) -> Result<RecoveryReport, RecoveryError> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    let mut records = Vec::new();
    let mut offset = 0_usize;
    while offset < bytes.len() {
        match decode(&bytes[offset..]) {
            Ok(frame) => {
                records.push(frame.payload().to_vec());
                offset += frame.encoded_length();
            }
            Err(_) if contains_later_valid_frame(&bytes, offset) => {
                return Err(RecoveryError::Corruption {
                    offset: offset as u64,
                });
            }
            Err(_) => {
                let truncated_bytes = (bytes.len() - offset) as u64;
                file.set_len(offset as u64)?;
                file.seek(SeekFrom::End(0))?;
                file.sync_data()?;
                return Ok(RecoveryReport {
                    records,
                    truncated_bytes,
                });
            }
        }
    }

    file.seek(SeekFrom::End(0))?;
    Ok(RecoveryReport {
        records,
        truncated_bytes: 0,
    })
}

fn contains_later_valid_frame(bytes: &[u8], failed_offset: usize) -> bool {
    let search_start = failed_offset.saturating_add(1);
    bytes
        .get(search_start..)
        .into_iter()
        .flat_map(|tail| tail.windows(MAGIC.len()).enumerate())
        .filter(|(_, candidate)| *candidate == MAGIC)
        .map(|(relative, _)| search_start + relative)
        .any(|candidate| decode(&bytes[candidate..]).is_ok())
}
