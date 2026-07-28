//! Bounded, checksummed identity prologue for segmented schema-v2 WAL files.

use std::{collections::BTreeSet, str};

use thiserror::Error;

pub const MAGIC: [u8; 8] = *b"CMTISEG2";
pub const FORMAT_VERSION: u16 = 2;
pub const SUPPORTED_FLAGS: u32 = 0;
pub const CHECKSUM_LENGTH: usize = size_of::<u32>();
pub const FIXED_HEADER_LENGTH: usize = MAGIC.len()
    + size_of::<u16>()
    + size_of::<u16>()
    + size_of::<u32>()
    + 16
    + size_of::<i64>()
    + size_of::<u16>()
    + size_of::<u16>()
    + size_of::<u16>()
    + size_of::<u16>();
pub const MAX_HEADER_LENGTH: usize = u16::MAX as usize;
pub const MAX_IDENTIFIER_LENGTH: usize = 1_024;
pub const MAX_STREAMS: usize = 4_096;

const STREAM_FIXED_LENGTH: usize = size_of::<u32>() + size_of::<u16>() + size_of::<u16>();

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PrologueError {
    #[error("incomplete WAL segment prologue")]
    Incomplete,
    #[error("invalid WAL segment prologue magic")]
    InvalidMagic,
    #[error("unsupported WAL segment prologue version {0}")]
    UnsupportedVersion(u16),
    #[error("invalid WAL segment prologue header length {0}")]
    InvalidHeaderLength(u16),
    #[error("unsupported WAL segment prologue flags 0x{0:08x}")]
    UnsupportedFlags(u32),
    #[error("WAL segment prologue checksum mismatch")]
    ChecksumMismatch,
    #[error("WAL segment ID must be nonzero")]
    InvalidSegmentId,
    #[error("WAL segment creation time must be positive")]
    InvalidCreatedTime,
    #[error("WAL stream ID must be nonzero")]
    InvalidStreamId,
    #[error("WAL segment requires at least one stream descriptor")]
    MissingStreams,
    #[error("WAL segment contains duplicate stream ID {0}")]
    DuplicateStreamId(u32),
    #[error("WAL segment stream descriptors are not in canonical ascending stream-ID order")]
    NonCanonicalStreamOrder,
    #[error("WAL segment contains too many streams: {actual}, maximum {maximum}")]
    TooManyStreams { actual: usize, maximum: usize },
    #[error("WAL prologue identifier {field} is invalid")]
    InvalidIdentifier { field: &'static str },
    #[error("WAL prologue identifier {field} is {actual} bytes, maximum {maximum}")]
    IdentifierTooLong {
        field: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("WAL segment prologue exceeds the maximum header length")]
    HeaderTooLarge,
    #[error("WAL segment prologue contains invalid UTF-8")]
    InvalidUtf8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamDescriptor {
    stream_id: u32,
    source_name: String,
    stream_name: String,
}

impl StreamDescriptor {
    pub fn new(
        stream_id: u32,
        source_name: impl Into<String>,
        stream_name: impl Into<String>,
    ) -> Result<Self, PrologueError> {
        if stream_id == 0 {
            return Err(PrologueError::InvalidStreamId);
        }
        let source_name = source_name.into();
        let stream_name = stream_name.into();
        validate_identifier("source_name", &source_name)?;
        validate_identifier("stream_name", &stream_name)?;
        Ok(Self {
            stream_id,
            source_name,
            stream_name,
        })
    }

    pub const fn stream_id(&self) -> u32 {
        self.stream_id
    }

    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    pub fn stream_name(&self) -> &str {
        &self.stream_name
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentMetadata {
    segment_id: [u8; 16],
    created_wall_time_ns: i64,
    schema_id: String,
    installation_id: String,
    build_id: String,
    streams: Vec<StreamDescriptor>,
}

impl SegmentMetadata {
    pub fn new(
        segment_id: [u8; 16],
        created_wall_time_ns: i64,
        schema_id: impl Into<String>,
        installation_id: impl Into<String>,
        build_id: impl Into<String>,
        mut streams: Vec<StreamDescriptor>,
    ) -> Result<Self, PrologueError> {
        if segment_id == [0_u8; 16] {
            return Err(PrologueError::InvalidSegmentId);
        }
        if created_wall_time_ns <= 0 {
            return Err(PrologueError::InvalidCreatedTime);
        }
        let schema_id = schema_id.into();
        let installation_id = installation_id.into();
        let build_id = build_id.into();
        validate_identifier("schema_id", &schema_id)?;
        validate_identifier("installation_id", &installation_id)?;
        validate_identifier("build_id", &build_id)?;
        if streams.is_empty() {
            return Err(PrologueError::MissingStreams);
        }
        if streams.len() > MAX_STREAMS {
            return Err(PrologueError::TooManyStreams {
                actual: streams.len(),
                maximum: MAX_STREAMS,
            });
        }
        let mut stream_ids = BTreeSet::new();
        for stream in &streams {
            if !stream_ids.insert(stream.stream_id) {
                return Err(PrologueError::DuplicateStreamId(stream.stream_id));
            }
        }
        streams.sort_unstable_by_key(StreamDescriptor::stream_id);
        let metadata = Self {
            segment_id,
            created_wall_time_ns,
            schema_id,
            installation_id,
            build_id,
            streams,
        };
        encoded_length(&metadata)?;
        Ok(metadata)
    }

    pub const fn segment_id(&self) -> &[u8; 16] {
        &self.segment_id
    }

    pub const fn created_wall_time_ns(&self) -> i64 {
        self.created_wall_time_ns
    }

    pub fn schema_id(&self) -> &str {
        &self.schema_id
    }

    pub fn installation_id(&self) -> &str {
        &self.installation_id
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    pub fn streams(&self) -> &[StreamDescriptor] {
        &self.streams
    }

    pub fn contains_stream(&self, stream_id: u32) -> bool {
        self.streams
            .binary_search_by_key(&stream_id, StreamDescriptor::stream_id)
            .is_ok()
    }
}

pub struct DecodedPrologue {
    metadata: SegmentMetadata,
    encoded_length: usize,
}

impl DecodedPrologue {
    pub const fn metadata(&self) -> &SegmentMetadata {
        &self.metadata
    }

    pub const fn encoded_length(&self) -> usize {
        self.encoded_length
    }

    pub fn into_metadata(self) -> SegmentMetadata {
        self.metadata
    }
}

pub fn encode(metadata: &SegmentMetadata) -> Result<Vec<u8>, PrologueError> {
    let header_length = encoded_length(metadata)?;
    let header_length_u16 =
        u16::try_from(header_length).map_err(|_| PrologueError::HeaderTooLarge)?;
    let stream_count =
        u16::try_from(metadata.streams.len()).map_err(|_| PrologueError::TooManyStreams {
            actual: metadata.streams.len(),
            maximum: MAX_STREAMS,
        })?;
    let mut encoded = Vec::with_capacity(header_length);
    encoded.extend_from_slice(&MAGIC);
    encoded.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
    encoded.extend_from_slice(&header_length_u16.to_be_bytes());
    encoded.extend_from_slice(&SUPPORTED_FLAGS.to_be_bytes());
    encoded.extend_from_slice(&metadata.segment_id);
    encoded.extend_from_slice(&metadata.created_wall_time_ns.to_be_bytes());
    push_length(&mut encoded, metadata.schema_id.len())?;
    push_length(&mut encoded, metadata.installation_id.len())?;
    push_length(&mut encoded, metadata.build_id.len())?;
    encoded.extend_from_slice(&stream_count.to_be_bytes());
    encoded.extend_from_slice(metadata.schema_id.as_bytes());
    encoded.extend_from_slice(metadata.installation_id.as_bytes());
    encoded.extend_from_slice(metadata.build_id.as_bytes());
    for stream in &metadata.streams {
        encoded.extend_from_slice(&stream.stream_id.to_be_bytes());
        push_length(&mut encoded, stream.source_name.len())?;
        push_length(&mut encoded, stream.stream_name.len())?;
        encoded.extend_from_slice(stream.source_name.as_bytes());
        encoded.extend_from_slice(stream.stream_name.as_bytes());
    }
    encoded.extend_from_slice(&crc32c::crc32c(&encoded).to_be_bytes());
    debug_assert_eq!(encoded.len(), header_length);
    Ok(encoded)
}

pub fn decode(bytes: &[u8]) -> Result<DecodedPrologue, PrologueError> {
    if bytes.len() < FIXED_HEADER_LENGTH {
        return Err(PrologueError::Incomplete);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(PrologueError::InvalidMagic);
    }
    let version = read_u16(bytes, 8);
    if version != FORMAT_VERSION {
        return Err(PrologueError::UnsupportedVersion(version));
    }
    let header_length_u16 = read_u16(bytes, 10);
    let header_length = usize::from(header_length_u16);
    if !(FIXED_HEADER_LENGTH + CHECKSUM_LENGTH..=MAX_HEADER_LENGTH).contains(&header_length) {
        return Err(PrologueError::InvalidHeaderLength(header_length_u16));
    }
    let flags = read_u32(bytes, 12);
    if flags != SUPPORTED_FLAGS {
        return Err(PrologueError::UnsupportedFlags(flags));
    }
    if bytes.len() < header_length {
        return Err(PrologueError::Incomplete);
    }
    let checksum_offset = header_length - CHECKSUM_LENGTH;
    let expected = read_u32(bytes, checksum_offset);
    if crc32c::crc32c(&bytes[..checksum_offset]) != expected {
        return Err(PrologueError::ChecksumMismatch);
    }

    let segment_id = bytes[16..32]
        .try_into()
        .expect("fixed segment ID range is in bounds");
    let created_wall_time_ns = i64::from_be_bytes(
        bytes[32..40]
            .try_into()
            .expect("fixed creation-time range is in bounds"),
    );
    let schema_length = usize::from(read_u16(bytes, 40));
    let installation_length = usize::from(read_u16(bytes, 42));
    let build_length = usize::from(read_u16(bytes, 44));
    let stream_count = usize::from(read_u16(bytes, 46));
    if stream_count > MAX_STREAMS {
        return Err(PrologueError::TooManyStreams {
            actual: stream_count,
            maximum: MAX_STREAMS,
        });
    }

    let mut cursor = FIXED_HEADER_LENGTH;
    let schema_id = read_identifier(bytes, &mut cursor, checksum_offset, schema_length)?;
    let installation_id =
        read_identifier(bytes, &mut cursor, checksum_offset, installation_length)?;
    let build_id = read_identifier(bytes, &mut cursor, checksum_offset, build_length)?;
    let mut streams = Vec::new();
    let mut stream_ids = BTreeSet::new();
    streams
        .try_reserve_exact(stream_count)
        .map_err(|_| PrologueError::HeaderTooLarge)?;
    for _ in 0..stream_count {
        let fixed_end = cursor
            .checked_add(STREAM_FIXED_LENGTH)
            .ok_or(PrologueError::HeaderTooLarge)?;
        if fixed_end > checksum_offset {
            return Err(PrologueError::InvalidHeaderLength(header_length_u16));
        }
        let stream_id = read_u32(bytes, cursor);
        let source_length = usize::from(read_u16(bytes, cursor + size_of::<u32>()));
        let stream_length = usize::from(read_u16(
            bytes,
            cursor + size_of::<u32>() + size_of::<u16>(),
        ));
        cursor = fixed_end;
        let source_name = read_identifier(bytes, &mut cursor, checksum_offset, source_length)?;
        let stream_name = read_identifier(bytes, &mut cursor, checksum_offset, stream_length)?;
        if !stream_ids.insert(stream_id) {
            return Err(PrologueError::DuplicateStreamId(stream_id));
        }
        streams.push(StreamDescriptor::new(stream_id, source_name, stream_name)?);
    }
    if streams
        .windows(2)
        .any(|pair| pair[0].stream_id > pair[1].stream_id)
    {
        return Err(PrologueError::NonCanonicalStreamOrder);
    }
    if cursor != checksum_offset {
        return Err(PrologueError::InvalidHeaderLength(header_length_u16));
    }
    let metadata = SegmentMetadata::new(
        segment_id,
        created_wall_time_ns,
        schema_id,
        installation_id,
        build_id,
        streams,
    )?;
    Ok(DecodedPrologue {
        metadata,
        encoded_length: header_length,
    })
}

pub(crate) fn declared_header_length(bytes: &[u8]) -> Result<usize, PrologueError> {
    if bytes.len() < 12 {
        return Err(PrologueError::Incomplete);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(PrologueError::InvalidMagic);
    }
    let version = read_u16(bytes, 8);
    if version != FORMAT_VERSION {
        return Err(PrologueError::UnsupportedVersion(version));
    }
    let header_length_u16 = read_u16(bytes, 10);
    let header_length = usize::from(header_length_u16);
    if !(FIXED_HEADER_LENGTH + CHECKSUM_LENGTH..=MAX_HEADER_LENGTH).contains(&header_length) {
        return Err(PrologueError::InvalidHeaderLength(header_length_u16));
    }
    Ok(header_length)
}

fn encoded_length(metadata: &SegmentMetadata) -> Result<usize, PrologueError> {
    let mut length = FIXED_HEADER_LENGTH
        .checked_add(metadata.schema_id.len())
        .and_then(|value| value.checked_add(metadata.installation_id.len()))
        .and_then(|value| value.checked_add(metadata.build_id.len()))
        .ok_or(PrologueError::HeaderTooLarge)?;
    for stream in &metadata.streams {
        length = length
            .checked_add(STREAM_FIXED_LENGTH)
            .and_then(|value| value.checked_add(stream.source_name.len()))
            .and_then(|value| value.checked_add(stream.stream_name.len()))
            .ok_or(PrologueError::HeaderTooLarge)?;
    }
    length = length
        .checked_add(CHECKSUM_LENGTH)
        .ok_or(PrologueError::HeaderTooLarge)?;
    if length > MAX_HEADER_LENGTH {
        return Err(PrologueError::HeaderTooLarge);
    }
    Ok(length)
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), PrologueError> {
    if value.len() > MAX_IDENTIFIER_LENGTH {
        return Err(PrologueError::IdentifierTooLong {
            field,
            actual: value.len(),
            maximum: MAX_IDENTIFIER_LENGTH,
        });
    }
    if value.is_empty()
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return Err(PrologueError::InvalidIdentifier { field });
    }
    Ok(())
}

fn push_length(encoded: &mut Vec<u8>, length: usize) -> Result<(), PrologueError> {
    encoded.extend_from_slice(
        &u16::try_from(length)
            .map_err(|_| PrologueError::HeaderTooLarge)?
            .to_be_bytes(),
    );
    Ok(())
}

fn read_identifier(
    bytes: &[u8],
    cursor: &mut usize,
    limit: usize,
    length: usize,
) -> Result<String, PrologueError> {
    if length > MAX_IDENTIFIER_LENGTH {
        return Err(PrologueError::IdentifierTooLong {
            field: "encoded_identifier",
            actual: length,
            maximum: MAX_IDENTIFIER_LENGTH,
        });
    }
    let end = cursor
        .checked_add(length)
        .ok_or(PrologueError::HeaderTooLarge)?;
    if end > limit {
        return Err(PrologueError::InvalidHeaderLength(
            u16::try_from(limit + CHECKSUM_LENGTH).unwrap_or(u16::MAX),
        ));
    }
    let value = str::from_utf8(&bytes[*cursor..end])
        .map_err(|_| PrologueError::InvalidUtf8)?
        .to_owned();
    *cursor = end;
    Ok(value)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(
        bytes[offset..offset + size_of::<u16>()]
            .try_into()
            .expect("validated u16 range is in bounds"),
    )
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        bytes[offset..offset + size_of::<u32>()]
            .try_into()
            .expect("validated u32 range is in bounds"),
    )
}
