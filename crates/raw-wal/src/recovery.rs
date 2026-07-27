//! Fail-closed WAL recovery with final-tail repair only.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};

use thiserror::Error;

use crate::frame::{
    CHECKSUM_LENGTH, HEADER_LENGTH, MAGIC, MAX_PAYLOAD_LENGTH, SCHEMA_VERSION, decode,
};

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
            Err(_) if is_unambiguous_eof_tail(&bytes[offset..]) => {
                let truncated_bytes = (bytes.len() - offset) as u64;
                file.set_len(offset as u64)?;
                file.seek(SeekFrom::End(0))?;
                file.sync_data()?;
                return Ok(RecoveryReport {
                    records,
                    truncated_bytes,
                });
            }
            Err(_) => {
                return Err(RecoveryError::Corruption {
                    offset: offset as u64,
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

fn is_unambiguous_eof_tail(bytes: &[u8]) -> bool {
    if bytes.len() < HEADER_LENGTH || bytes[..MAGIC.len()] != MAGIC {
        return false;
    }
    let version = u16::from_be_bytes(
        bytes[MAGIC.len()..MAGIC.len() + size_of::<u16>()]
            .try_into()
            .expect("validated frame header contains a complete version"),
    );
    if version != SCHEMA_VERSION {
        return false;
    }
    let payload_length = u32::from_be_bytes(
        bytes[MAGIC.len() + size_of::<u16>()..HEADER_LENGTH]
            .try_into()
            .expect("validated frame header contains a complete length"),
    ) as usize;
    if payload_length > MAX_PAYLOAD_LENGTH {
        return false;
    }
    let Some(encoded_length) = HEADER_LENGTH
        .checked_add(payload_length)
        .and_then(|length| length.checked_add(CHECKSUM_LENGTH))
    else {
        return false;
    };

    bytes.len() <= encoded_length
}
