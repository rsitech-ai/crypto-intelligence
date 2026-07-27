//! Versioned WAL frame encoding with a Castagnoli CRC32C.

use thiserror::Error;

pub const MAGIC: [u8; 8] = *b"CMTIWAL1";
pub const SCHEMA_VERSION: u16 = 1;
pub const HEADER_LENGTH: usize = MAGIC.len() + size_of::<u16>() + size_of::<u32>();
pub const CHECKSUM_LENGTH: usize = size_of::<u32>();
pub const MAX_PAYLOAD_LENGTH: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum FrameError {
    #[error("WAL payload exceeds the maximum frame length")]
    PayloadTooLarge,
    #[error("incomplete WAL frame")]
    Incomplete,
    #[error("invalid WAL frame magic")]
    InvalidMagic,
    #[error("unsupported WAL frame schema version {0}")]
    UnsupportedSchema(u16),
    #[error("WAL frame checksum mismatch")]
    ChecksumMismatch,
}

pub struct DecodedFrame<'a> {
    payload: &'a [u8],
    encoded_length: usize,
}

impl<'a> DecodedFrame<'a> {
    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }

    pub const fn encoded_length(&self) -> usize {
        self.encoded_length
    }
}

pub fn encode(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_PAYLOAD_LENGTH {
        return Err(FrameError::PayloadTooLarge);
    }
    let payload_length = u32::try_from(payload.len()).map_err(|_| FrameError::PayloadTooLarge)?;
    let mut encoded = Vec::with_capacity(HEADER_LENGTH + payload.len() + CHECKSUM_LENGTH);
    encoded.extend_from_slice(&MAGIC);
    encoded.extend_from_slice(&SCHEMA_VERSION.to_be_bytes());
    encoded.extend_from_slice(&payload_length.to_be_bytes());
    encoded.extend_from_slice(payload);
    encoded.extend_from_slice(&crc32c::crc32c(payload).to_be_bytes());
    Ok(encoded)
}

pub fn decode(bytes: &[u8]) -> Result<DecodedFrame<'_>, FrameError> {
    if bytes.len() < HEADER_LENGTH {
        return Err(FrameError::Incomplete);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(FrameError::InvalidMagic);
    }
    let version = u16::from_be_bytes(
        bytes[MAGIC.len()..MAGIC.len() + size_of::<u16>()]
            .try_into()
            .expect("fixed frame version range is in bounds"),
    );
    if version != SCHEMA_VERSION {
        return Err(FrameError::UnsupportedSchema(version));
    }
    let payload_length = u32::from_be_bytes(
        bytes[MAGIC.len() + size_of::<u16>()..HEADER_LENGTH]
            .try_into()
            .expect("fixed frame length range is in bounds"),
    ) as usize;
    if payload_length > MAX_PAYLOAD_LENGTH {
        return Err(FrameError::PayloadTooLarge);
    }
    let encoded_length = HEADER_LENGTH
        .checked_add(payload_length)
        .and_then(|length| length.checked_add(CHECKSUM_LENGTH))
        .ok_or(FrameError::PayloadTooLarge)?;
    if bytes.len() < encoded_length {
        return Err(FrameError::Incomplete);
    }
    let payload = &bytes[HEADER_LENGTH..HEADER_LENGTH + payload_length];
    let expected = u32::from_be_bytes(
        bytes[HEADER_LENGTH + payload_length..encoded_length]
            .try_into()
            .expect("fixed checksum range is in bounds"),
    );
    if crc32c::crc32c(payload) != expected {
        return Err(FrameError::ChecksumMismatch);
    }
    Ok(DecodedFrame {
        payload,
        encoded_length,
    })
}
