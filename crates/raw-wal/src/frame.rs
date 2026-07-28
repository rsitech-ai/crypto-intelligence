//! Versioned WAL frame encoding with a Castagnoli CRC32C.

use thiserror::Error;

pub const MAGIC: [u8; 8] = *b"CMTIWAL1";
pub const LEGACY_SCHEMA_VERSION: u16 = 1;
pub const SCHEMA_VERSION: u16 = 2;
pub const LEGACY_HEADER_LENGTH: usize = MAGIC.len() + size_of::<u16>() + size_of::<u32>();
pub const HEADER_LENGTH: usize = MAGIC.len()
    + size_of::<u16>()
    + size_of::<u16>()
    + size_of::<u32>()
    + size_of::<u32>()
    + size_of::<u64>()
    + size_of::<u64>()
    + size_of::<i64>()
    + size_of::<u64>()
    + size_of::<u32>()
    + size_of::<u32>();
pub const CHECKSUM_LENGTH: usize = size_of::<u32>();
pub const HEADER_CHECKSUM_LENGTH: usize = size_of::<u32>();
pub const MAX_PAYLOAD_LENGTH: usize = 16 * 1024 * 1024;
pub const SUPPORTED_FLAGS: u32 = 0;

const VERSION_OFFSET: usize = MAGIC.len();
const HEADER_LENGTH_OFFSET: usize = VERSION_OFFSET + size_of::<u16>();
const FLAGS_OFFSET: usize = HEADER_LENGTH_OFFSET + size_of::<u16>();
const STREAM_ID_OFFSET: usize = FLAGS_OFFSET + size_of::<u32>();
const CONNECTION_EPOCH_OFFSET: usize = STREAM_ID_OFFSET + size_of::<u32>();
const RECORD_SEQUENCE_OFFSET: usize = CONNECTION_EPOCH_OFFSET + size_of::<u64>();
const RECEIVE_WALL_TIME_OFFSET: usize = RECORD_SEQUENCE_OFFSET + size_of::<u64>();
const RECEIVE_MONOTONIC_TIME_OFFSET: usize = RECEIVE_WALL_TIME_OFFSET + size_of::<i64>();
const PAYLOAD_LENGTH_OFFSET: usize = RECEIVE_MONOTONIC_TIME_OFFSET + size_of::<u64>();
const HEADER_CHECKSUM_OFFSET: usize = PAYLOAD_LENGTH_OFFSET + size_of::<u32>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalFormat {
    LegacyV1,
    V2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordMetadata {
    pub flags: u32,
    pub stream_id: u32,
    pub connection_epoch: u64,
    pub record_sequence: u64,
    pub receive_wall_time_ns: i64,
    pub receive_monotonic_time_ns: u64,
}

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
    #[error("invalid WAL frame header length {0}")]
    InvalidHeaderLength(u16),
    #[error("unsupported WAL frame flags 0x{0:08x}")]
    UnsupportedFlags(u32),
    #[error("WAL frame header checksum mismatch")]
    HeaderChecksumMismatch,
    #[error("WAL frame checksum mismatch")]
    FrameChecksumMismatch,
}

pub struct DecodedFrame<'a> {
    format: WalFormat,
    metadata: Option<RecordMetadata>,
    payload: &'a [u8],
    encoded_length: usize,
}

impl<'a> DecodedFrame<'a> {
    pub const fn format(&self) -> WalFormat {
        self.format
    }

    pub const fn metadata(&self) -> Option<RecordMetadata> {
        self.metadata
    }

    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }

    pub const fn encoded_length(&self) -> usize {
        self.encoded_length
    }
}

pub fn encode(metadata: RecordMetadata, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if metadata.flags != SUPPORTED_FLAGS {
        return Err(FrameError::UnsupportedFlags(metadata.flags));
    }
    if payload.len() > MAX_PAYLOAD_LENGTH {
        return Err(FrameError::PayloadTooLarge);
    }
    let payload_length = u32::try_from(payload.len()).map_err(|_| FrameError::PayloadTooLarge)?;
    let header_length = u16::try_from(HEADER_LENGTH).expect("the fixed WAL header length fits u16");
    let mut encoded = Vec::with_capacity(HEADER_LENGTH + payload.len() + CHECKSUM_LENGTH);
    encoded.extend_from_slice(&MAGIC);
    encoded.extend_from_slice(&SCHEMA_VERSION.to_be_bytes());
    encoded.extend_from_slice(&header_length.to_be_bytes());
    encoded.extend_from_slice(&metadata.flags.to_be_bytes());
    encoded.extend_from_slice(&metadata.stream_id.to_be_bytes());
    encoded.extend_from_slice(&metadata.connection_epoch.to_be_bytes());
    encoded.extend_from_slice(&metadata.record_sequence.to_be_bytes());
    encoded.extend_from_slice(&metadata.receive_wall_time_ns.to_be_bytes());
    encoded.extend_from_slice(&metadata.receive_monotonic_time_ns.to_be_bytes());
    encoded.extend_from_slice(&payload_length.to_be_bytes());
    encoded.extend_from_slice(&crc32c::crc32c(&encoded).to_be_bytes());
    encoded.extend_from_slice(payload);
    encoded.extend_from_slice(&crc32c::crc32c(&encoded).to_be_bytes());
    Ok(encoded)
}

pub fn encode_legacy(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_PAYLOAD_LENGTH {
        return Err(FrameError::PayloadTooLarge);
    }
    let payload_length = u32::try_from(payload.len()).map_err(|_| FrameError::PayloadTooLarge)?;
    let mut encoded = Vec::with_capacity(LEGACY_HEADER_LENGTH + payload.len() + CHECKSUM_LENGTH);
    encoded.extend_from_slice(&MAGIC);
    encoded.extend_from_slice(&LEGACY_SCHEMA_VERSION.to_be_bytes());
    encoded.extend_from_slice(&payload_length.to_be_bytes());
    encoded.extend_from_slice(payload);
    encoded.extend_from_slice(&crc32c::crc32c(payload).to_be_bytes());
    Ok(encoded)
}

pub fn decode(bytes: &[u8]) -> Result<DecodedFrame<'_>, FrameError> {
    if bytes.len() < MAGIC.len() + size_of::<u16>() {
        return Err(FrameError::Incomplete);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(FrameError::InvalidMagic);
    }
    let version = read_u16(bytes, VERSION_OFFSET);
    match version {
        LEGACY_SCHEMA_VERSION => decode_legacy(bytes),
        SCHEMA_VERSION => decode_v2(bytes),
        unsupported => Err(FrameError::UnsupportedSchema(unsupported)),
    }
}

fn decode_legacy(bytes: &[u8]) -> Result<DecodedFrame<'_>, FrameError> {
    if bytes.len() < LEGACY_HEADER_LENGTH {
        return Err(FrameError::Incomplete);
    }
    let payload_length =
        u32::from_be_bytes(read_array(bytes, MAGIC.len() + size_of::<u16>())) as usize;
    let encoded_length = checked_encoded_length(LEGACY_HEADER_LENGTH, payload_length)?;
    if bytes.len() < encoded_length {
        return Err(FrameError::Incomplete);
    }
    let payload = &bytes[LEGACY_HEADER_LENGTH..LEGACY_HEADER_LENGTH + payload_length];
    let expected = u32::from_be_bytes(read_array(bytes, LEGACY_HEADER_LENGTH + payload_length));
    if crc32c::crc32c(payload) != expected {
        return Err(FrameError::FrameChecksumMismatch);
    }
    Ok(DecodedFrame {
        format: WalFormat::LegacyV1,
        metadata: None,
        payload,
        encoded_length,
    })
}

fn decode_v2(bytes: &[u8]) -> Result<DecodedFrame<'_>, FrameError> {
    let (metadata, payload_length) = decode_v2_header(bytes)?;
    let encoded_length = checked_encoded_length(HEADER_LENGTH, payload_length)?;
    if bytes.len() < encoded_length {
        return Err(FrameError::Incomplete);
    }
    let payload = &bytes[HEADER_LENGTH..HEADER_LENGTH + payload_length];
    let expected = u32::from_be_bytes(read_array(bytes, HEADER_LENGTH + payload_length));
    if crc32c::crc32c(&bytes[..HEADER_LENGTH + payload_length]) != expected {
        return Err(FrameError::FrameChecksumMismatch);
    }
    Ok(DecodedFrame {
        format: WalFormat::V2,
        metadata: Some(metadata),
        payload,
        encoded_length,
    })
}

pub(crate) fn decode_v2_header(bytes: &[u8]) -> Result<(RecordMetadata, usize), FrameError> {
    if bytes.len() < HEADER_LENGTH {
        return Err(FrameError::Incomplete);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(FrameError::InvalidMagic);
    }
    let version = read_u16(bytes, VERSION_OFFSET);
    if version != SCHEMA_VERSION {
        return Err(FrameError::UnsupportedSchema(version));
    }
    let header_length = read_u16(bytes, HEADER_LENGTH_OFFSET);
    if usize::from(header_length) != HEADER_LENGTH {
        return Err(FrameError::InvalidHeaderLength(header_length));
    }
    let flags = u32::from_be_bytes(read_array(bytes, FLAGS_OFFSET));
    if flags != SUPPORTED_FLAGS {
        return Err(FrameError::UnsupportedFlags(flags));
    }
    let payload_length = u32::from_be_bytes(read_array(bytes, PAYLOAD_LENGTH_OFFSET)) as usize;
    if payload_length > MAX_PAYLOAD_LENGTH {
        return Err(FrameError::PayloadTooLarge);
    }
    let expected_header_checksum = u32::from_be_bytes(read_array(bytes, HEADER_CHECKSUM_OFFSET));
    if crc32c::crc32c(&bytes[..HEADER_CHECKSUM_OFFSET]) != expected_header_checksum {
        return Err(FrameError::HeaderChecksumMismatch);
    }
    Ok((
        RecordMetadata {
            flags,
            stream_id: u32::from_be_bytes(read_array(bytes, STREAM_ID_OFFSET)),
            connection_epoch: u64::from_be_bytes(read_array(bytes, CONNECTION_EPOCH_OFFSET)),
            record_sequence: u64::from_be_bytes(read_array(bytes, RECORD_SEQUENCE_OFFSET)),
            receive_wall_time_ns: i64::from_be_bytes(read_array(bytes, RECEIVE_WALL_TIME_OFFSET)),
            receive_monotonic_time_ns: u64::from_be_bytes(read_array(
                bytes,
                RECEIVE_MONOTONIC_TIME_OFFSET,
            )),
        },
        payload_length,
    ))
}

pub(crate) fn decode_legacy_header(bytes: &[u8]) -> Result<usize, FrameError> {
    if bytes.len() < LEGACY_HEADER_LENGTH {
        return Err(FrameError::Incomplete);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(FrameError::InvalidMagic);
    }
    let version = read_u16(bytes, VERSION_OFFSET);
    if version != LEGACY_SCHEMA_VERSION {
        return Err(FrameError::UnsupportedSchema(version));
    }
    let payload_length =
        u32::from_be_bytes(read_array(bytes, MAGIC.len() + size_of::<u16>())) as usize;
    if payload_length > MAX_PAYLOAD_LENGTH {
        return Err(FrameError::PayloadTooLarge);
    }
    Ok(payload_length)
}

pub(crate) fn is_valid_partial_header_prefix(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() >= HEADER_LENGTH {
        return false;
    }

    let immutable_prefix = [
        MAGIC.as_slice(),
        SCHEMA_VERSION.to_be_bytes().as_slice(),
        u16::try_from(HEADER_LENGTH)
            .expect("the fixed WAL header length fits u16")
            .to_be_bytes()
            .as_slice(),
        SUPPORTED_FLAGS.to_be_bytes().as_slice(),
    ]
    .concat();
    let validated_length = bytes.len().min(immutable_prefix.len());
    bytes[..validated_length] == immutable_prefix[..validated_length]
}

fn checked_encoded_length(
    header_length: usize,
    payload_length: usize,
) -> Result<usize, FrameError> {
    if payload_length > MAX_PAYLOAD_LENGTH {
        return Err(FrameError::PayloadTooLarge);
    }
    header_length
        .checked_add(payload_length)
        .and_then(|length| length.checked_add(CHECKSUM_LENGTH))
        .ok_or(FrameError::PayloadTooLarge)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(read_array(bytes, offset))
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    bytes[offset..offset + N]
        .try_into()
        .expect("validated fixed frame range is in bounds")
}
