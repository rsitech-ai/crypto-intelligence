use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
};

use raw_wal::{
    frame::{
        CHECKSUM_LENGTH, LEGACY_HEADER_LENGTH, LEGACY_SCHEMA_VERSION, MAGIC, MAX_PAYLOAD_LENGTH,
        encode_legacy,
    },
    recovery::RecoveryError,
    segment::Segment,
};

#[test]
fn frame_layout_uses_versioned_big_endian_crc32c() {
    let frame = encode_legacy(b"123456789").expect("known payload must encode");

    assert_eq!(&frame[..MAGIC.len()], &MAGIC);
    assert_eq!(
        &frame[MAGIC.len()..MAGIC.len() + 2],
        &LEGACY_SCHEMA_VERSION.to_be_bytes()
    );
    assert_eq!(
        &frame[MAGIC.len() + 2..LEGACY_HEADER_LENGTH],
        &9_u32.to_be_bytes()
    );
    assert_eq!(
        &frame[LEGACY_HEADER_LENGTH..LEGACY_HEADER_LENGTH + 9],
        b"123456789"
    );
    assert_eq!(
        &frame[LEGACY_HEADER_LENGTH + 9..],
        &0xe306_9283_u32.to_be_bytes()
    );
}

#[test]
fn append_syncs_each_frame_and_recovery_returns_payloads_in_order() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let mut segment = Segment::open(&path).expect("segment must open");

    segment
        .append_synced(br#"{"sequence":100}"#)
        .expect("first frame must persist");
    segment
        .append_synced(br#"{"sequence":101}"#)
        .expect("second frame must persist");

    let report = segment.recover().expect("synced frames must recover");
    assert_eq!(report.records().len(), 2);
    assert_eq!(report.records()[0].payload(), br#"{"sequence":100}"#);
    assert_eq!(report.records()[1].payload(), br#"{"sequence":101}"#);
    assert_eq!(report.truncated_bytes(), 0);
}

#[test]
fn declared_frame_extending_past_eof_fails_without_truncation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_frame_length;
    {
        let mut segment = Segment::open(&path).expect("segment must open");
        first_frame_length = segment
            .append_synced(b"first")
            .expect("first frame must persist");
        segment
            .append_synced(b"second")
            .expect("second frame must persist");
    }
    let original_length = fs::metadata(&path)
        .expect("segment metadata must exist")
        .len();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("segment must reopen")
        .set_len(original_length - 3)
        .expect("test tail must truncate");

    let original = fs::read(&path).expect("incomplete segment bytes must read");
    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == first_frame_length
    ));
    assert_eq!(
        fs::read(&path).expect("failed recovery must preserve bytes"),
        original,
        "an untrusted declared length extending beyond EOF must not authorize truncation"
    );
}

#[test]
fn every_valid_partial_final_header_prefix_is_repaired() {
    let second = encode_legacy(b"second").expect("second frame must encode");
    for partial_length in 1..LEGACY_HEADER_LENGTH {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let path = directory.path().join("market.wal");
        let first_frame_length;
        {
            let mut segment = Segment::open(&path).expect("segment must open");
            first_frame_length = segment
                .append_synced(b"first")
                .expect("first frame must persist");
        }
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("segment must open for partial header")
            .write_all(&second[..partial_length])
            .expect("partial header must write");

        let mut segment = Segment::open(&path).expect("segment must reopen");
        let report = segment
            .recover()
            .expect("exact partial header prefix must be repairable");
        assert_eq!(report.records().len(), 1);
        assert_eq!(report.records()[0].payload(), b"first");
        assert_eq!(report.truncated_bytes(), partial_length as u64);
        assert_eq!(
            fs::metadata(&path)
                .expect("repaired segment metadata must exist")
                .len(),
            first_frame_length,
            "partial header length {partial_length} must truncate to the prior frame"
        );
    }
}

#[test]
fn arbitrary_short_final_junk_fails_without_truncation() {
    for junk in [
        b"X".as_slice(),
        b"CMTX".as_slice(),
        b"CMTIWAL1\x01".as_slice(),
        b"CMTIWAL1\x00\x01\xff".as_slice(),
    ] {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let path = directory.path().join("market.wal");
        let first_frame_length;
        {
            let mut segment = Segment::open(&path).expect("segment must open");
            first_frame_length = segment
                .append_synced(b"first")
                .expect("first frame must persist");
        }
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("segment must open for junk suffix")
            .write_all(junk)
            .expect("junk suffix must write");
        let original = fs::read(&path).expect("junk segment bytes must read");

        let mut segment = Segment::open(&path).expect("segment must reopen");
        assert!(matches!(
            segment.recover(),
            Err(RecoveryError::Corruption { offset, .. }) if offset == first_frame_length
        ));
        assert_eq!(
            fs::read(&path).expect("failed recovery must preserve junk bytes"),
            original
        );
    }
}

#[test]
fn inflated_length_before_valid_frame_fails_without_truncation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_frame_length;
    {
        let mut segment = Segment::open(&path).expect("segment must open");
        first_frame_length = segment
            .append_synced(b"first")
            .expect("first frame must persist");
        segment
            .append_synced(b"second")
            .expect("second frame must persist");
        segment
            .append_synced(b"third")
            .expect("third frame must persist");
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("segment must reopen for length mutation");
    file.seek(SeekFrom::Start(
        first_frame_length + MAGIC.len() as u64 + size_of::<u16>() as u64,
    ))
    .expect("test mutation must seek");
    file.write_all(
        &u32::try_from(MAX_PAYLOAD_LENGTH)
            .expect("maximum payload length must fit the frame field")
            .to_be_bytes(),
    )
    .expect("in-range inflated length must write");
    file.sync_data().expect("test mutation must sync");
    drop(file);
    let original = fs::read(&path).expect("mutated segment bytes must read");

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == first_frame_length
    ));
    assert_eq!(
        fs::read(&path).expect("failed recovery must preserve bytes"),
        original
    );
}

#[test]
fn length_claim_consuming_a_later_valid_frame_fails_without_truncation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_frame_length;
    {
        let mut segment = Segment::open(&path).expect("segment must open");
        first_frame_length = segment
            .append_synced(b"first")
            .expect("first frame must persist");
        segment
            .append_synced(b"second")
            .expect("second frame must persist");
        segment
            .append_synced(b"third")
            .expect("third frame must persist");
    }
    let original_length = fs::metadata(&path)
        .expect("segment metadata must exist")
        .len();
    let claimed_payload_length = usize::try_from(original_length - first_frame_length)
        .expect("test file length must fit usize")
        - LEGACY_HEADER_LENGTH
        - CHECKSUM_LENGTH;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("segment must reopen for length mutation");
    file.seek(SeekFrom::Start(
        first_frame_length + MAGIC.len() as u64 + size_of::<u16>() as u64,
    ))
    .expect("test mutation must seek");
    file.write_all(
        &u32::try_from(claimed_payload_length)
            .expect("combined frame extent must fit the frame field")
            .to_be_bytes(),
    )
    .expect("exact-extent length must write");
    file.sync_data().expect("test mutation must sync");
    drop(file);
    let original = fs::read(&path).expect("mutated segment bytes must read");

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == first_frame_length
    ));
    assert_eq!(
        fs::read(&path).expect("failed recovery must preserve bytes"),
        original,
        "an unprotected length that consumes a later frame must not authorize truncation"
    );
}

#[test]
fn checksum_corrupt_final_frame_fails_without_truncation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_frame_length;
    {
        let mut segment = Segment::open(&path).expect("segment must open");
        first_frame_length = segment
            .append_synced(b"first")
            .expect("first frame must persist");
        segment
            .append_synced(b"second")
            .expect("second frame must persist");
    }
    let original_length = fs::metadata(&path)
        .expect("segment metadata must exist")
        .len();
    flip_byte(&path, original_length - 1);
    let original = fs::read(&path).expect("corrupt segment bytes must read");

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == first_frame_length
    ));
    assert_eq!(
        fs::read(&path).expect("failed recovery must preserve bytes"),
        original,
        "a checksum failure cannot prove a final frame boundary"
    );
}

#[test]
fn corruption_before_a_later_valid_frame_fails_closed_without_truncation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_frame_length;
    {
        let mut segment = Segment::open(&path).expect("segment must open");
        first_frame_length = segment
            .append_synced(b"first")
            .expect("first frame must persist");
        segment
            .append_synced(b"second")
            .expect("second frame must persist");
        segment
            .append_synced(b"third")
            .expect("third frame must persist");
    }
    let original = fs::read(&path).expect("segment bytes must read");
    flip_byte(&path, first_frame_length + LEGACY_HEADER_LENGTH as u64);

    let mut segment = Segment::open(&path).expect("segment must reopen");
    let error = segment
        .recover()
        .expect_err("mid-file corruption must fail closed");

    assert!(matches!(error, RecoveryError::Corruption { .. }));
    assert_eq!(
        fs::read(&path)
            .expect("failed recovery must preserve bytes")
            .len(),
        original.len()
    );
}

#[test]
fn corrupt_frame_followed_by_another_corrupt_frame_fails_without_truncation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_frame_length;
    let second_frame_length;
    {
        let mut segment = Segment::open(&path).expect("segment must open");
        first_frame_length = segment
            .append_synced(b"first")
            .expect("first frame must persist");
        second_frame_length = segment
            .append_synced(b"second")
            .expect("second frame must persist");
        segment
            .append_synced(b"third")
            .expect("third frame must persist");
    }
    flip_byte(&path, first_frame_length + second_frame_length - 1);
    let original = fs::read(&path).expect("segment bytes must read");
    flip_byte(&path, original.len() as u64 - 1);
    let original = fs::read(&path).expect("mutated segment bytes must read");

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset, .. }) if offset == first_frame_length
    ));
    assert_eq!(
        fs::read(&path).expect("failed recovery must preserve bytes"),
        original
    );
}

#[test]
fn damaged_final_frame_header_fails_without_truncation() {
    for damaged_field in ["magic", "schema", "length"] {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let path = directory.path().join("market.wal");
        let first_frame_length;
        {
            let mut segment = Segment::open(&path).expect("segment must open");
            first_frame_length = segment
                .append_synced(b"first")
                .expect("first frame must persist");
            segment
                .append_synced(b"second")
                .expect("second frame must persist");
        }

        let field_offset = match damaged_field {
            "magic" => first_frame_length,
            "schema" => first_frame_length + MAGIC.len() as u64,
            "length" => first_frame_length + MAGIC.len() as u64 + size_of::<u16>() as u64,
            _ => unreachable!("test field list is exhaustive"),
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("segment must reopen for header mutation");
        file.seek(SeekFrom::Start(field_offset))
            .expect("test mutation must seek");
        match damaged_field {
            "magic" => file.write_all(b"X").expect("magic must corrupt"),
            "schema" => file
                .write_all(&LEGACY_SCHEMA_VERSION.wrapping_add(2).to_be_bytes())
                .expect("schema must corrupt"),
            "length" => file
                .write_all(&u32::MAX.to_be_bytes())
                .expect("length must corrupt"),
            _ => unreachable!("test field list is exhaustive"),
        }
        file.sync_data().expect("test mutation must sync");
        drop(file);
        let original = fs::read(&path).expect("mutated segment bytes must read");

        let mut segment = Segment::open(&path).expect("segment must reopen");
        let error = segment
            .recover()
            .expect_err("damaged header must fail closed");
        let expected_error = match damaged_field {
            "schema" => matches!(
                error,
                RecoveryError::UnsupportedFormat { offset, .. }
                    if offset == first_frame_length
            ),
            _ => matches!(
                error,
                RecoveryError::Corruption { offset, .. } if offset == first_frame_length
            ),
        };
        assert!(expected_error, "{damaged_field} damage must fail closed");
        assert_eq!(
            fs::read(&path).expect("failed recovery must preserve bytes"),
            original,
            "{damaged_field} damage must not mutate the WAL"
        );
    }
}

#[test]
fn wrong_magic_or_schema_before_a_valid_frame_fails_closed() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = directory.path().join("market.wal");
    let first_frame_length;
    {
        let mut segment = Segment::open(&path).expect("segment must open");
        first_frame_length = segment
            .append_synced(b"first")
            .expect("first frame must persist");
        segment
            .append_synced(b"second")
            .expect("second frame must persist");
    }

    flip_byte(&path, 0);
    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset: 0, .. })
    ));
    drop(segment);

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("segment must reopen");
    file.seek(SeekFrom::Start(0)).expect("test file must seek");
    file.write_all(&MAGIC).expect("magic must restore");
    file.seek(SeekFrom::Start(MAGIC.len() as u64))
        .expect("test file must seek");
    file.write_all(&LEGACY_SCHEMA_VERSION.wrapping_add(2).to_be_bytes())
        .expect("schema must corrupt");
    file.sync_data().expect("test mutation must sync");
    drop(file);

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::UnsupportedFormat { offset: 0, .. })
    ));
    assert!(first_frame_length > LEGACY_HEADER_LENGTH as u64);
}

fn flip_byte(path: &std::path::Path, offset: u64) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("segment must reopen for mutation");
    file.seek(SeekFrom::Start(offset))
        .expect("test mutation must seek");
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte)
        .expect("test mutation byte must read");
    byte[0] ^= 0x80;
    file.seek(SeekFrom::Start(offset))
        .expect("test mutation must seek");
    file.write_all(&byte)
        .expect("test mutation byte must write");
    file.sync_data().expect("test mutation must sync");
}
