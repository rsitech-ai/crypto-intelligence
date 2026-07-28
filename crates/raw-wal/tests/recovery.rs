use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use raw_wal::{
    RecoveryError,
    frame::{
        CHECKSUM_LENGTH, HEADER_LENGTH, LEGACY_SCHEMA_VERSION, MAGIC, RecordMetadata,
        SCHEMA_VERSION, encode,
    },
    prologue::{SegmentMetadata, StreamDescriptor},
    recovery::{MAX_COLLECTED_RECORDS, MAX_RECOVERABLE_SEGMENT_LENGTH},
    segment::{Segment, SegmentError},
};

fn metadata(sequence: u64) -> RecordMetadata {
    RecordMetadata {
        flags: 0,
        stream_id: 7,
        connection_epoch: 11,
        record_sequence: sequence,
        receive_wall_time_ns: 1_721_234_567_000_000_000 + sequence as i64,
        receive_monotonic_time_ns: 9_000_000 + sequence,
    }
}

fn segment_metadata() -> SegmentMetadata {
    SegmentMetadata::new(
        [1_u8; 16],
        1_721_234_567_000_000_000,
        "market-schema-v2",
        "installation-test",
        "build-test",
        vec![
            StreamDescriptor::new(7, "binance", "spot-btcusdt")
                .expect("stream descriptor must be valid"),
        ],
    )
    .expect("segment metadata must be valid")
}

fn create_v2(path: &Path) -> Segment {
    Segment::create_v2(path, segment_metadata()).expect("v2 segment must create")
}

fn legacy_frame(payload: &[u8]) -> Vec<u8> {
    let payload_length = u32::try_from(payload.len()).expect("test payload length must fit");
    let mut encoded = Vec::with_capacity(MAGIC.len() + 2 + 4 + payload.len() + CHECKSUM_LENGTH);
    encoded.extend_from_slice(&MAGIC);
    encoded.extend_from_slice(&LEGACY_SCHEMA_VERSION.to_be_bytes());
    encoded.extend_from_slice(&payload_length.to_be_bytes());
    encoded.extend_from_slice(payload);
    encoded.extend_from_slice(&crc32c::crc32c(payload).to_be_bytes());
    encoded
}

#[test]
fn schema_v2_layout_round_trips_required_metadata_and_protects_the_complete_frame() {
    let metadata = metadata(41);
    let encoded = encode(metadata, b"123456789").expect("known record must encode");

    assert_eq!(&encoded[..MAGIC.len()], &MAGIC);
    assert_eq!(
        &encoded[MAGIC.len()..MAGIC.len() + 2],
        &SCHEMA_VERSION.to_be_bytes()
    );
    assert_eq!(
        &encoded[MAGIC.len() + 2..MAGIC.len() + 4],
        &(HEADER_LENGTH as u16).to_be_bytes()
    );
    assert_eq!(&encoded[12..16], &metadata.flags.to_be_bytes());
    assert_eq!(&encoded[16..20], &metadata.stream_id.to_be_bytes());
    assert_eq!(&encoded[20..28], &metadata.connection_epoch.to_be_bytes());
    assert_eq!(&encoded[28..36], &metadata.record_sequence.to_be_bytes());
    assert_eq!(
        &encoded[36..44],
        &metadata.receive_wall_time_ns.to_be_bytes()
    );
    assert_eq!(
        &encoded[44..52],
        &metadata.receive_monotonic_time_ns.to_be_bytes()
    );
    assert_eq!(&encoded[52..56], &9_u32.to_be_bytes());
    assert_eq!(
        &encoded[56..HEADER_LENGTH],
        &crc32c::crc32c(&encoded[..56]).to_be_bytes()
    );
    assert_eq!(&encoded[HEADER_LENGTH..HEADER_LENGTH + 9], b"123456789");
    assert_eq!(
        &encoded[HEADER_LENGTH + 9..],
        &crc32c::crc32c(&encoded[..HEADER_LENGTH + 9]).to_be_bytes()
    );

    let decoded = raw_wal::frame::decode(&encoded).expect("v2 frame must decode");
    assert_eq!(decoded.metadata(), Some(metadata));
    assert_eq!(decoded.payload(), b"123456789");
    assert_eq!(decoded.encoded_length(), encoded.len());
}

#[test]
fn legacy_v1_remains_readable_and_refuses_a_mixed_v2_append() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    fs::write(&path, legacy_frame(b"legacy")).expect("legacy fixture must write");

    let mut segment = Segment::open(&path).expect("segment must open");
    assert!(matches!(
        segment.append_record_synced(metadata(1), b"current"),
        Err(SegmentError::PrologueRequired)
    ));
    let report = segment.recover().expect("legacy segment must recover");

    assert_eq!(report.records().len(), 1);
    assert_eq!(report.records()[0].metadata(), None);
    assert_eq!(report.records()[0].payload(), b"legacy");
    assert_eq!(
        fs::read(&path).expect("legacy bytes must remain"),
        legacy_frame(b"legacy")
    );
}

#[test]
fn v2_segment_refuses_legacy_append_and_recovery_rejects_mixed_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let v2_length;
    {
        let mut segment = create_v2(&path);
        v2_length = segment
            .append_record_synced(metadata(1), b"current")
            .expect("v2 frame must append")
            .next_offset();
        assert!(matches!(
            segment.append_synced(b"legacy"),
            Err(SegmentError::FormatMismatch)
        ));
    }
    assert_eq!(
        fs::metadata(&path).expect("v2 segment must exist").len(),
        v2_length
    );

    let mixed_path = directory.path().join("mixed.wal");
    let legacy = legacy_frame(b"legacy");
    let mut mixed = legacy.clone();
    mixed.extend_from_slice(&encode(metadata(1), b"current").expect("v2 frame must encode"));
    fs::write(&mixed_path, &mixed).expect("mixed fixture must write");
    let mut segment = Segment::open(&mixed_path).expect("mixed fixture must open");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == legacy.len() as u64
    ));
    assert_eq!(fs::read(&mixed_path).expect("mixed bytes must read"), mixed);
}

#[test]
fn truncated_v2_payload_tail_recovers_only_complete_frames_and_is_idempotent() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_length;
    let second_offset;
    {
        let mut segment = create_v2(&path);
        first_length = segment
            .append_record_synced(metadata(1), b"one")
            .expect("first frame must append")
            .next_offset();
        second_offset = segment
            .append_record_synced(metadata(2), b"two")
            .expect("second frame must append");
    }
    let original_length = second_offset.next_offset();
    let second_length = second_offset.next_offset() - second_offset.frame_offset();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("segment must reopen")
        .set_len(original_length - 3)
        .expect("test tail must truncate");

    let mut segment = Segment::open_v2(&path).expect("segment must reopen");
    let first_recovery = segment
        .recover()
        .expect("incomplete final v2 frame must repair");
    assert_eq!(first_recovery.records().len(), 1);
    assert_eq!(first_recovery.records()[0].payload(), b"one");
    assert_eq!(first_recovery.truncated_bytes(), second_length - 3);
    assert_eq!(
        fs::metadata(&path).expect("segment must exist").len(),
        first_length
    );

    drop(segment);
    let mut segment = Segment::open_v2(&path).expect("repaired segment must reopen");
    let second_recovery = segment.recover().expect("second recovery must be clean");
    assert_eq!(second_recovery.records(), first_recovery.records());
    assert_eq!(second_recovery.truncated_bytes(), 0);
}

#[test]
fn every_incomplete_v2_header_payload_and_checksum_prefix_repairs_to_prior_boundary() {
    let first = encode(metadata(1), b"one").expect("first frame must encode");
    let second = encode(metadata(2), b"second").expect("second frame must encode");

    for partial_length in 1..second.len() {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let path = directory.path().join("market.wal");
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second[..partial_length]);
        fs::write(&path, bytes).expect("partial segment must write");

        let mut segment = Segment::open(&path).expect("segment must reopen");
        let report = segment
            .recover()
            .unwrap_or_else(|error| panic!("prefix length {partial_length} must repair: {error}"));
        assert_eq!(report.records().len(), 1);
        assert_eq!(report.records()[0].payload(), b"one");
        assert_eq!(report.truncated_bytes(), partial_length as u64);
        assert_eq!(
            fs::metadata(&path).expect("segment must remain").len(),
            first.len() as u64
        );
    }
}

#[test]
fn checksum_corruption_in_the_final_v2_frame_preserves_all_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_length;
    {
        let mut segment = create_v2(&path);
        first_length = segment
            .append_record_synced(metadata(1), b"one")
            .expect("first frame must append")
            .next_offset();
        segment
            .append_record_synced(metadata(2), b"two")
            .expect("second frame must append");
    }
    let file_length = fs::metadata(&path).expect("segment must exist").len();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("segment must reopen");
    file.seek(SeekFrom::Start(file_length - 1))
        .expect("checksum byte must be addressable");
    let mut original_byte = [0_u8; 1];
    file.read_exact(&mut original_byte)
        .expect("checksum byte must read");
    file.seek(SeekFrom::Start(file_length - 1))
        .expect("checksum byte must be addressable");
    file.write_all(&[original_byte[0] ^ 0xFF])
        .expect("checksum must mutate");
    file.sync_data().expect("mutation must sync");
    drop(file);
    let original = fs::read(&path).expect("corrupt bytes must read");

    let mut segment = Segment::open_v2(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == first_length
    ));
    assert_eq!(fs::read(&path).expect("segment must read"), original);
}

#[test]
fn authenticated_inflated_length_before_a_later_frame_fails_without_truncation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first = encode(metadata(1), b"one").expect("first frame must encode");
    let mut second = encode(metadata(2), b"two").expect("second frame must encode");
    let third = encode(metadata(3), b"three").expect("third frame must encode");
    second[52..56].copy_from_slice(&1024_u32.to_be_bytes());
    let header_checksum = crc32c::crc32c(&second[..56]);
    second[56..HEADER_LENGTH].copy_from_slice(&header_checksum.to_be_bytes());
    let mut bytes = first.clone();
    bytes.extend_from_slice(&second);
    bytes.extend_from_slice(&third);
    fs::write(&path, &bytes).expect("inflated fixture must write");

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == first.len() as u64
    ));
    assert_eq!(fs::read(&path).expect("segment must read"), bytes);
}

#[test]
fn invalid_header_checksum_at_eof_is_corruption_not_a_tail_repair() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let frame_offset = {
        let mut segment = create_v2(&path);
        segment
            .append_record_synced(metadata(1), b"one")
            .expect("frame must append")
            .frame_offset()
    };
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("segment must reopen");
    file.seek(SeekFrom::Start(frame_offset + 56))
        .expect("header checksum byte must be addressable");
    let mut original_byte = [0_u8; 1];
    file.read_exact(&mut original_byte)
        .expect("header checksum byte must read");
    file.seek(SeekFrom::Start(frame_offset + 56))
        .expect("header checksum byte must be addressable");
    file.write_all(&[original_byte[0] ^ 0xFF])
        .expect("header checksum must mutate");
    file.sync_data().expect("mutation must sync");
    drop(file);
    let original = fs::read(&path).expect("corrupt bytes must read");

    let mut segment = Segment::open_v2(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == frame_offset
    ));
    assert_eq!(fs::read(&path).expect("segment must read"), original);
}

#[test]
fn metadata_corruption_before_a_later_valid_frame_fails_without_truncation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let second_offset;
    {
        let mut segment = create_v2(&path);
        segment
            .append_record_synced(metadata(1), b"one")
            .expect("first frame must append");
        second_offset = segment
            .append_record_synced(metadata(2), b"two")
            .expect("second frame must append");
        segment
            .append_record_synced(metadata(3), b"three")
            .expect("third frame must append");
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("segment must reopen");
    file.seek(SeekFrom::Start(second_offset.frame_offset() + 30))
        .expect("record-sequence byte must be addressable");
    file.write_all(&[0xFF]).expect("metadata must mutate");
    file.sync_data().expect("mutation must sync");
    drop(file);
    let original = fs::read(&path).expect("corrupt bytes must read");

    let mut segment = Segment::open_v2(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == second_offset.frame_offset()
    ));
    assert_eq!(fs::read(&path).expect("segment must read"), original);
}

#[test]
fn non_monotonic_sequence_for_one_stream_epoch_fails_without_changing_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first = encode(metadata(2), b"two").expect("first frame must encode");
    let second = encode(metadata(1), b"one").expect("second frame must encode");
    let first_length = first.len() as u64;
    let mut original = first;
    original.extend_from_slice(&second);
    fs::write(&path, &original).expect("out-of-order fixture must write");

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::SequenceRegression {
            offset,
            previous: 2,
            current: 1,
            ..
        }) if offset == first_length
    ));
    assert_eq!(fs::read(&path).expect("segment must read"), original);
}

#[test]
fn oversized_active_segment_is_rejected_before_reading_and_preserved() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .expect("segment fixture must open");
    file.set_len(MAX_RECOVERABLE_SEGMENT_LENGTH + 1)
        .expect("sparse oversized fixture must create");
    drop(file);

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::SegmentTooLarge { actual, maximum })
            if actual == MAX_RECOVERABLE_SEGMENT_LENGTH + 1
                && maximum == MAX_RECOVERABLE_SEGMENT_LENGTH
    ));
    assert_eq!(
        fs::metadata(&path).expect("segment must remain").len(),
        MAX_RECOVERABLE_SEGMENT_LENGTH + 1
    );
}

#[test]
fn second_segment_open_fails_while_the_first_owner_is_alive() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first = Segment::open(&path).expect("first owner must acquire the segment");

    assert!(matches!(
        Segment::open(&path),
        Err(SegmentError::AlreadyOpen)
    ));

    drop(first);
    Segment::open(&path).expect("segment must reopen after the first owner drops");
}

#[test]
fn sequence_regression_is_rejected_before_the_append_is_acknowledged() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let mut segment = create_v2(&path);
    let first_length = segment
        .append_record_synced(metadata(2), b"two")
        .expect("first frame must append")
        .next_offset();
    drop(segment);
    let mut segment = Segment::open_v2(&path).expect("existing v2 segment must reopen");

    assert!(matches!(
        segment.append_record_synced(metadata(1), b"one"),
        Err(SegmentError::SequenceRegression {
            stream_id: 7,
            connection_epoch: 11,
            previous: 2,
            current: 1,
        })
    ));
    assert_eq!(
        fs::metadata(&path).expect("segment must remain").len(),
        first_length
    );
    let report = segment.recover().expect("acknowledged prefix must recover");
    assert_eq!(report.records().len(), 1);
    assert_eq!(report.records()[0].payload(), b"two");
}

#[test]
fn collected_recovery_is_bounded_while_streaming_recovery_reads_the_same_segment() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let frame_count = MAX_COLLECTED_RECORDS + 1;
    let mut bytes = Vec::with_capacity(frame_count * (HEADER_LENGTH + CHECKSUM_LENGTH));
    for sequence in 1..=frame_count as u64 {
        bytes.extend_from_slice(
            &encode(metadata(sequence), b"").expect("ordered empty frame must encode"),
        );
    }
    fs::write(&path, &bytes).expect("bounded collection fixture must write");

    let mut segment = Segment::open(&path).expect("segment must open");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::CollectionLimit {
            maximum_records,
            ..
        }) if maximum_records == MAX_COLLECTED_RECORDS
    ));
    assert_eq!(fs::read(&path).expect("segment must remain"), bytes);

    let mut streamed = 0_u64;
    let summary = segment
        .recover_with(|_| streamed += 1)
        .expect("streaming recovery must handle the valid segment");
    assert_eq!(streamed, frame_count as u64);
    assert_eq!(summary.record_count(), frame_count as u64);
}

#[test]
fn unsupported_future_version_is_not_misclassified_as_corruption() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let mut bytes = encode(metadata(1), b"future").expect("v2 frame must encode");
    let future_version = SCHEMA_VERSION + 1;
    bytes[MAGIC.len()..MAGIC.len() + 2].copy_from_slice(&future_version.to_be_bytes());
    fs::write(&path, &bytes).expect("future fixture must write");

    let mut segment = Segment::open(&path).expect("future fixture must open");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::UnsupportedFormat {
            offset: 0,
            version,
        }) if version == future_version
    ));
    assert_eq!(fs::read(&path).expect("future bytes must remain"), bytes);
}

#[test]
fn streaming_recovery_visits_records_without_returning_a_payload_collection() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let mut segment = create_v2(&path);
    for sequence in 1..=1_024 {
        segment
            .append_record_synced(metadata(sequence), b"x")
            .expect("ordered frame must append");
    }

    let mut count = 0_u64;
    let summary = segment
        .recover_with(|record| {
            assert_eq!(record.payload(), b"x");
            count += 1;
        })
        .expect("streaming recovery must succeed");
    assert_eq!(count, 1_024);
    assert_eq!(summary.record_count(), 1_024);
    assert_eq!(summary.truncated_bytes(), 0);
}
