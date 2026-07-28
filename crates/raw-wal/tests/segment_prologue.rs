use raw_wal::prologue::{
    CHECKSUM_LENGTH, FIXED_HEADER_LENGTH, FORMAT_VERSION, MAGIC, MAX_IDENTIFIER_LENGTH,
    MAX_STREAMS, PrologueError, SegmentMetadata, StreamDescriptor, decode, encode,
};
use raw_wal::{
    RecoveryError,
    frame::{RecordMetadata, encode as encode_record},
    segment::{Segment, SegmentError},
};

fn metadata() -> SegmentMetadata {
    SegmentMetadata::new(
        [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E,
            0x0F, 0x10,
        ],
        1_721_234_567_000_000_000,
        "market-schema-v2",
        "installation-01",
        "build-abc123",
        vec![
            StreamDescriptor::new(7, "binance", "spot-btcusdt")
                .expect("first descriptor must be valid"),
            StreamDescriptor::new(9, "kraken", "spot-xbtusd")
                .expect("second descriptor must be valid"),
        ],
    )
    .expect("metadata fixture must be valid")
}

#[test]
fn prologue_has_exact_big_endian_layout_and_round_trips_all_metadata() {
    let metadata = metadata();
    let encoded = encode(&metadata).expect("valid metadata must encode");
    let header_length =
        u16::from_be_bytes(encoded[10..12].try_into().expect("length field must exist")) as usize;

    assert_eq!(&encoded[..MAGIC.len()], &MAGIC);
    assert_eq!(&encoded[8..10], &FORMAT_VERSION.to_be_bytes());
    assert_eq!(header_length, encoded.len());
    assert_eq!(&encoded[12..16], &0_u32.to_be_bytes());
    assert_eq!(&encoded[16..32], metadata.segment_id());
    assert_eq!(
        &encoded[32..40],
        &metadata.created_wall_time_ns().to_be_bytes()
    );
    assert_eq!(
        &encoded[40..42],
        &u16::try_from(metadata.schema_id().len())
            .expect("fixture length must fit")
            .to_be_bytes()
    );
    assert_eq!(
        &encoded[42..44],
        &u16::try_from(metadata.installation_id().len())
            .expect("fixture length must fit")
            .to_be_bytes()
    );
    assert_eq!(
        &encoded[44..46],
        &u16::try_from(metadata.build_id().len())
            .expect("fixture length must fit")
            .to_be_bytes()
    );
    assert_eq!(&encoded[46..48], &2_u16.to_be_bytes());
    assert_eq!(
        &encoded[encoded.len() - CHECKSUM_LENGTH..],
        &crc32c::crc32c(&encoded[..encoded.len() - CHECKSUM_LENGTH]).to_be_bytes()
    );

    let decoded = decode(&encoded).expect("valid prologue must decode");
    assert_eq!(decoded.metadata(), &metadata);
    assert_eq!(decoded.encoded_length(), encoded.len());
}

#[test]
fn prologue_crc_covers_fixed_metadata_strings_and_stream_table() {
    let encoded = encode(&metadata()).expect("valid metadata must encode");
    for offset in [
        16,
        33,
        FIXED_HEADER_LENGTH,
        encoded.len() - CHECKSUM_LENGTH - 1,
    ] {
        let mut mutated = encoded.clone();
        mutated[offset] ^= 0x80;
        assert!(
            matches!(decode(&mutated), Err(PrologueError::ChecksumMismatch)),
            "mutation at byte {offset} must fail the prologue CRC"
        );
    }
}

#[test]
fn every_truncated_prologue_prefix_fails_without_guessing_a_boundary() {
    let encoded = encode(&metadata()).expect("valid metadata must encode");
    for length in 0..encoded.len() {
        assert!(
            matches!(decode(&encoded[..length]), Err(PrologueError::Incomplete)),
            "prefix length {length} must remain incomplete"
        );
    }
}

#[test]
fn constructors_reject_ambiguous_or_unbounded_metadata() {
    assert!(matches!(
        StreamDescriptor::new(0, "binance", "spot"),
        Err(PrologueError::InvalidStreamId)
    ));
    assert!(matches!(
        StreamDescriptor::new(1, "binance\n", "spot"),
        Err(PrologueError::InvalidIdentifier {
            field: "source_name"
        })
    ));
    assert!(matches!(
        SegmentMetadata::new([0_u8; 16], 1, "schema", "installation", "build", vec![]),
        Err(PrologueError::InvalidSegmentId)
    ));
    assert!(matches!(
        SegmentMetadata::new([1_u8; 16], 0, "schema", "installation", "build", vec![]),
        Err(PrologueError::InvalidCreatedTime)
    ));
    assert!(matches!(
        SegmentMetadata::new(
            [1_u8; 16],
            1,
            "schema",
            "installation",
            "build",
            vec![
                StreamDescriptor::new(7, "binance", "one").expect("descriptor must be valid"),
                StreamDescriptor::new(7, "binance", "two").expect("descriptor must be valid"),
            ]
        ),
        Err(PrologueError::DuplicateStreamId(7))
    ));
    assert!(matches!(
        SegmentMetadata::new(
            [1_u8; 16],
            1,
            "x".repeat(MAX_IDENTIFIER_LENGTH + 1),
            "installation",
            "build",
            vec![]
        ),
        Err(PrologueError::IdentifierTooLong {
            field: "schema_id",
            ..
        })
    ));

    let too_many_streams = (1..=MAX_STREAMS + 1)
        .map(|stream_id| {
            StreamDescriptor::new(
                u32::try_from(stream_id).expect("bounded stream ID must fit"),
                "s",
                "t",
            )
            .expect("descriptor must be valid")
        })
        .collect();
    assert!(matches!(
        SegmentMetadata::new(
            [1_u8; 16],
            1,
            "schema",
            "installation",
            "build",
            too_many_streams
        ),
        Err(PrologueError::TooManyStreams {
            actual,
            maximum
        }) if actual == MAX_STREAMS + 1 && maximum == MAX_STREAMS
    ));
}

#[test]
fn constructor_canonicalizes_stream_order_and_decoder_rejects_noncanonical_wire_order() {
    let metadata = SegmentMetadata::new(
        [1_u8; 16],
        1,
        "s",
        "i",
        "b",
        vec![
            StreamDescriptor::new(9, "x", "y").expect("descriptor must be valid"),
            StreamDescriptor::new(7, "x", "y").expect("descriptor must be valid"),
        ],
    )
    .expect("metadata must canonicalize");
    assert_eq!(metadata.streams()[0].stream_id(), 7);
    assert_eq!(metadata.streams()[1].stream_id(), 9);
    assert!(metadata.contains_stream(7));
    assert!(!metadata.contains_stream(8));

    let mut noncanonical = encode(&metadata).expect("canonical metadata must encode");
    let stream_table_offset = FIXED_HEADER_LENGTH
        + metadata.schema_id().len()
        + metadata.installation_id().len()
        + metadata.build_id().len();
    let descriptor_length = 10;
    let first = noncanonical[stream_table_offset..stream_table_offset + descriptor_length].to_vec();
    let second = noncanonical
        [stream_table_offset + descriptor_length..stream_table_offset + 2 * descriptor_length]
        .to_vec();
    noncanonical[stream_table_offset..stream_table_offset + descriptor_length]
        .copy_from_slice(&second);
    noncanonical
        [stream_table_offset + descriptor_length..stream_table_offset + 2 * descriptor_length]
        .copy_from_slice(&first);
    let checksum_offset = noncanonical.len() - CHECKSUM_LENGTH;
    let checksum = crc32c::crc32c(&noncanonical[..checksum_offset]);
    noncanonical[checksum_offset..].copy_from_slice(&checksum.to_be_bytes());

    assert!(matches!(
        decode(&noncanonical),
        Err(PrologueError::NonCanonicalStreamOrder)
    ));
}

#[test]
fn unsupported_version_flags_and_header_length_remain_typed() {
    let encoded = encode(&metadata()).expect("valid metadata must encode");

    let mut version = encoded.clone();
    version[8..10].copy_from_slice(&(FORMAT_VERSION + 1).to_be_bytes());
    assert!(matches!(
        decode(&version),
        Err(PrologueError::UnsupportedVersion(value)) if value == FORMAT_VERSION + 1
    ));

    let mut flags = encoded.clone();
    flags[12..16].copy_from_slice(&1_u32.to_be_bytes());
    assert!(matches!(
        decode(&flags),
        Err(PrologueError::UnsupportedFlags(1))
    ));

    let mut length = encoded;
    length[10..12].copy_from_slice(
        &u16::try_from(FIXED_HEADER_LENGTH - 1)
            .expect("fixed header length must fit")
            .to_be_bytes(),
    );
    assert!(matches!(
        decode(&length),
        Err(PrologueError::InvalidHeaderLength(_))
    ));
}

fn record_metadata(stream_id: u32, sequence: u64) -> RecordMetadata {
    RecordMetadata {
        flags: 0,
        stream_id,
        connection_epoch: 3,
        record_sequence: sequence,
        receive_wall_time_ns: 1_721_234_567_000_000_000 + sequence as i64,
        receive_monotonic_time_ns: 9_000 + sequence,
    }
}

#[test]
fn segmented_v2_create_append_reopen_and_recover_preserve_durable_offsets() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let metadata = metadata();
    let prologue_length = encode(&metadata).expect("prologue must encode").len() as u64;
    let first_offset;
    let second_offset;
    {
        let mut segment =
            Segment::create_v2(&path, metadata.clone()).expect("v2 segment must create");
        assert_eq!(segment.segment_metadata(), Some(&metadata));
        first_offset = segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("first record must append");
        second_offset = segment
            .append_record_synced(record_metadata(7, 2), b"two")
            .expect("second record must append");
    }

    assert_eq!(first_offset.segment_id(), metadata.segment_id());
    assert_eq!(first_offset.frame_offset(), prologue_length);
    assert_eq!(first_offset.next_offset(), second_offset.frame_offset());
    assert!(second_offset.next_offset() > second_offset.frame_offset());

    let mut reopened = Segment::open_v2(&path).expect("v2 segment must reopen");
    assert_eq!(reopened.segment_metadata(), Some(&metadata));
    let report = reopened.recover().expect("v2 segment must recover");
    assert_eq!(report.records().len(), 2);
    assert_eq!(report.records()[0].payload(), b"one");
    assert_eq!(report.records()[0].offset(), first_offset.frame_offset());
    assert_eq!(report.records()[1].payload(), b"two");
    assert_eq!(report.records()[1].offset(), second_offset.frame_offset());
    assert_eq!(
        report.summary().last_valid_offset(),
        second_offset.next_offset()
    );
}

#[test]
fn segmented_writer_rejects_undeclared_stream_before_writing_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let mut segment = Segment::create_v2(&path, metadata()).expect("v2 segment must create");
    let before = std::fs::read(&path).expect("prologue bytes must read");

    assert!(matches!(
        segment.append_record_synced(record_metadata(99, 1), b"unknown"),
        Err(SegmentError::UndeclaredStream(99))
    ));
    assert_eq!(
        std::fs::read(&path).expect("segment bytes must remain"),
        before
    );
}

#[test]
fn segmented_recovery_rejects_a_valid_frame_for_an_undeclared_stream_before_visiting_it() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let segment = Segment::create_v2(&path, metadata()).expect("v2 segment must create");
    let frame_offset = std::fs::metadata(&path)
        .expect("segment metadata must read")
        .len();
    drop(segment);

    let unknown = encode_record(record_metadata(99, 1), b"unknown")
        .expect("unknown-stream frame must encode");
    let mut bytes = std::fs::read(&path).expect("prologue bytes must read");
    bytes.extend_from_slice(&unknown);
    std::fs::write(&path, &bytes).expect("unknown-stream fixture must write");

    let mut segment = Segment::open_v2(&path).expect("prologue must remain valid");
    let mut visited = 0_u64;
    assert!(matches!(
        segment.recover_with(|_| visited += 1),
        Err(RecoveryError::UndeclaredStream {
            offset,
            stream_id: 99
        }) if offset == frame_offset
    ));
    assert_eq!(visited, 0);
    assert_eq!(
        std::fs::read(&path).expect("fixture bytes must remain"),
        bytes
    );
}

#[test]
fn v2_creation_rejects_every_existing_path_without_changing_its_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");

    for (name, bytes) in [("empty", Vec::new()), ("occupied", b"sentinel".to_vec())] {
        let path = directory.path().join(format!("{name}.wal"));
        std::fs::write(&path, &bytes).expect("existing fixture must write");
        assert!(matches!(
            Segment::create_v2(&path, metadata()),
            Err(SegmentError::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            std::fs::read(&path).expect("existing fixture must remain"),
            bytes
        );
        assert!(
            std::fs::read_dir(directory.path())
                .expect("directory must read")
                .all(|entry| !entry
                    .expect("directory entry must read")
                    .file_name()
                    .to_string_lossy()
                    .contains(".create-")),
            "failed no-replace installation must clean its temporary file"
        );
    }
}

#[test]
fn concurrent_v2_creation_installs_exactly_one_complete_segment_without_temp_residue() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let path = path.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            Segment::create_v2(&path, metadata())
        }));
    }
    barrier.wait();

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("creator thread must not panic"))
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| {
                matches!(
                    result,
                    Err(SegmentError::Io(error))
                        if error.kind() == std::io::ErrorKind::AlreadyExists
                )
            })
            .count(),
        1
    );
    drop(results);

    let entries: Vec<_> = std::fs::read_dir(directory.path())
        .expect("directory must read")
        .map(|entry| entry.expect("directory entry must read").file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("market.wal")]);
    let mut segment = Segment::open_v2(&path).expect("installed segment must be complete");
    assert_eq!(
        segment
            .recover()
            .expect("empty segment must recover")
            .summary()
            .record_count(),
        0
    );
}

#[test]
fn maximum_unsorted_source_table_is_canonical_and_recoverable_with_logarithmic_lookup() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("maximum.wal");
    let streams = (1..=MAX_STREAMS)
        .rev()
        .map(|stream_id| {
            StreamDescriptor::new(
                u32::try_from(stream_id).expect("bounded stream ID must fit"),
                "s",
                "t",
            )
            .expect("descriptor must be valid")
        })
        .collect();
    let metadata = SegmentMetadata::new([2_u8; 16], 2, "schema", "installation", "build", streams)
        .expect("maximum table must remain within the header bound");
    assert_eq!(
        metadata
            .streams()
            .first()
            .expect("table must not be empty")
            .stream_id(),
        1
    );
    assert_eq!(
        metadata
            .streams()
            .last()
            .expect("table must not be empty")
            .stream_id(),
        u32::try_from(MAX_STREAMS).expect("maximum stream count must fit")
    );

    let mut segment = Segment::create_v2(&path, metadata).expect("maximum segment must create");
    segment
        .append_record_synced(
            record_metadata(
                u32::try_from(MAX_STREAMS).expect("maximum stream count must fit"),
                1,
            ),
            b"one",
        )
        .expect("declared maximum stream must append");
    drop(segment);

    let mut segment = Segment::open_v2(&path).expect("maximum segment must reopen");
    assert_eq!(
        segment
            .recover()
            .expect("maximum segment must recover")
            .records()[0]
            .payload(),
        b"one"
    );
}

#[test]
fn corrupt_or_partial_prologue_fails_open_without_mutating_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let encoded = encode(&metadata()).expect("prologue must encode");

    for (name, bytes) in [
        ("partial", encoded[..encoded.len() - 1].to_vec()),
        ("checksum", {
            let mut corrupt = encoded.clone();
            let last = corrupt.last_mut().expect("checksum byte must exist");
            *last ^= 0x80;
            corrupt
        }),
    ] {
        let path = directory.path().join(format!("{name}.wal"));
        std::fs::write(&path, &bytes).expect("prologue fixture must write");
        assert!(matches!(
            Segment::open_v2(&path),
            Err(SegmentError::Prologue(_))
        ));
        assert_eq!(
            std::fs::read(&path).expect("fixture bytes must remain"),
            bytes
        );
    }
}

#[test]
fn corrupt_prologue_before_valid_records_never_truncates_or_repairs_the_file() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    {
        let mut segment = Segment::create_v2(&path, metadata()).expect("v2 segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    let mut corrupt = std::fs::read(&path).expect("segment bytes must read");
    corrupt[20] ^= 0x80;
    std::fs::write(&path, &corrupt).expect("corrupt fixture must write");

    assert!(matches!(
        Segment::open_v2(&path),
        Err(SegmentError::Prologue(PrologueError::ChecksumMismatch))
    ));
    assert_eq!(
        std::fs::read(&path).expect("corrupt segment must remain"),
        corrupt
    );
}

#[test]
fn provisional_record_only_v2_remains_recoverable_but_is_not_segmented_appendable() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("provisional.wal");
    let bytes = encode_record(record_metadata(7, 1), b"legacy-v2")
        .expect("record-only v2 fixture must encode");
    std::fs::write(&path, &bytes).expect("record-only fixture must write");

    let mut segment = Segment::open(&path).expect("record-only v2 must open generically");
    let report = segment
        .recover()
        .expect("record-only v2 must remain recoverable");
    assert_eq!(report.records()[0].payload(), b"legacy-v2");
    drop(segment);

    assert!(matches!(
        Segment::open_v2(&path),
        Err(SegmentError::PrologueRequired)
    ));
    assert_eq!(
        std::fs::read(&path).expect("record-only bytes must remain"),
        bytes
    );
}
