use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
};

use raw_wal::{
    frame::{HEADER_LENGTH, MAGIC, SCHEMA_VERSION, encode},
    recovery::RecoveryError,
    segment::Segment,
};

#[test]
fn frame_layout_uses_versioned_big_endian_crc32c() {
    let frame = encode(b"123456789").expect("known payload must encode");

    assert_eq!(&frame[..MAGIC.len()], &MAGIC);
    assert_eq!(
        &frame[MAGIC.len()..MAGIC.len() + 2],
        &SCHEMA_VERSION.to_be_bytes()
    );
    assert_eq!(&frame[MAGIC.len() + 2..HEADER_LENGTH], &9_u32.to_be_bytes());
    assert_eq!(&frame[HEADER_LENGTH..HEADER_LENGTH + 9], b"123456789");
    assert_eq!(&frame[HEADER_LENGTH + 9..], &0xe306_9283_u32.to_be_bytes());
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
    assert_eq!(
        report.records(),
        [
            br#"{"sequence":100}"#.as_slice(),
            br#"{"sequence":101}"#.as_slice()
        ]
    );
    assert_eq!(report.truncated_bytes(), 0);
}

#[test]
fn recovery_truncates_an_incomplete_final_frame_only() {
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

    let mut segment = Segment::open(&path).expect("segment must reopen");
    let report = segment
        .recover()
        .expect("incomplete final frame must be recoverable");

    assert_eq!(report.records(), [b"first".as_slice()]);
    assert_eq!(
        report.truncated_bytes(),
        original_length - 3 - first_frame_length
    );
    assert_eq!(
        fs::metadata(&path)
            .expect("recovered segment metadata must exist")
            .len(),
        first_frame_length
    );
}

#[test]
fn recovery_truncates_a_checksum_corrupt_final_frame_only() {
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

    let mut segment = Segment::open(&path).expect("segment must reopen");
    let report = segment
        .recover()
        .expect("corrupt final frame must be recoverable");

    assert_eq!(report.records(), [b"first".as_slice()]);
    assert_eq!(
        report.truncated_bytes(),
        original_length - first_frame_length
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
    flip_byte(&path, first_frame_length + HEADER_LENGTH as u64);

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
        Err(RecoveryError::Corruption { offset: 0 })
    ));

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("segment must reopen");
    file.seek(SeekFrom::Start(0)).expect("test file must seek");
    file.write_all(&MAGIC).expect("magic must restore");
    file.seek(SeekFrom::Start(MAGIC.len() as u64))
        .expect("test file must seek");
    file.write_all(&SCHEMA_VERSION.wrapping_add(1).to_be_bytes())
        .expect("schema must corrupt");
    file.sync_data().expect("test mutation must sync");
    drop(file);

    let mut segment = Segment::open(&path).expect("segment must reopen");
    assert!(matches!(
        segment.recover(),
        Err(RecoveryError::Corruption { offset: 0 })
    ));
    assert!(first_frame_length > HEADER_LENGTH as u64);
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
