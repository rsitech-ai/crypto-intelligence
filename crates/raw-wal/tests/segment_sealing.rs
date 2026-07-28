use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::symlink,
    sync::{Arc, Barrier},
};

use raw_wal::{
    RecoveryError,
    frame::{self, RecordMetadata},
    prologue::{MAX_STREAMS, SegmentMetadata, StreamDescriptor},
    recovery::MAX_RECOVERABLE_SEGMENT_LENGTH,
    seal::{
        MAX_MANIFEST_LENGTH, ManifestError, SealingError, decode_manifest, manifest_path_for,
        pending_manifest_path_for, seal_v2_segment, seal_v2_segment_after, sealed_path_for,
        verify_sealed_v2_segment,
    },
    segment::Segment,
};

fn segment_metadata() -> SegmentMetadata {
    segment_metadata_with_id([0x5A; 16])
}

fn segment_metadata_with_id(segment_id: [u8; 16]) -> SegmentMetadata {
    SegmentMetadata::new(
        segment_id,
        1_721_234_567_000_000_000,
        "market-schema-v2",
        "installation-test",
        "build-test",
        vec![
            StreamDescriptor::new(9, "kraken", "spot-xbtusd").expect("descriptor must be valid"),
            StreamDescriptor::new(7, "binance", "spot-btcusdt").expect("descriptor must be valid"),
        ],
    )
    .expect("segment metadata must be valid")
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

fn active_path(directory: &std::path::Path) -> std::path::PathBuf {
    active_path_with_identity(directory, 1, [0x5A; 16])
}

fn active_path_with_identity(
    directory: &std::path::Path,
    ordinal: u64,
    segment_id: [u8; 16],
) -> std::path::PathBuf {
    directory.join(format!(
        "{ordinal:020}-{}.active.wal",
        hex::encode(segment_id)
    ))
}

fn recompute_canonical_manifest_body_digest(mut encoded: Vec<u8>) -> Vec<u8> {
    const PREFIX: &[u8] = b"{\"body\":";
    const DIGEST_MARKER: &[u8] = b",\"manifest_body_blake3\":\"";
    assert!(encoded.starts_with(PREFIX));
    let marker_offset = encoded
        .windows(DIGEST_MARKER.len())
        .rposition(|window| window == DIGEST_MARKER)
        .expect("manifest digest marker must exist");
    let digest = hex::encode(blake3::hash(&encoded[PREFIX.len()..marker_offset]).as_bytes());
    let digest_start = marker_offset + DIGEST_MARKER.len();
    encoded[digest_start..digest_start + digest.len()].copy_from_slice(digest.as_bytes());
    encoded
}

fn manifest_with_segment_length(
    encoded: &[u8],
    original_length: u64,
    replacement_length: u64,
) -> Vec<u8> {
    let mut replaced = String::from_utf8(encoded.to_vec()).expect("manifest must be UTF-8");
    replaced = replaced.replacen(
        &format!("\"segment_bytes\":{original_length}"),
        &format!("\"segment_bytes\":{replacement_length}"),
        1,
    );
    replaced = replaced.replacen(
        &format!("\"end_offset\":{original_length}"),
        &format!("\"end_offset\":{replacement_length}"),
        1,
    );
    recompute_canonical_manifest_body_digest(replaced.into_bytes())
}

#[test]
fn clean_segment_seals_to_a_canonical_self_digested_manifest_and_verifies_read_only() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    let first;
    let second;
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        first = segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("first record must append");
        second = segment
            .append_record_synced(record_metadata(9, 1), b"two")
            .expect("second record must append");
    }

    let sealed = seal_v2_segment(&path, 1_721_234_568_000_000_000).expect("segment must seal");
    let manifest = sealed.manifest().clone();
    let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
    let manifest_path = manifest_path_for(&sealed_path).expect("manifest path must derive");
    assert_eq!(sealed.compression_job().source_display_path(), sealed_path);
    assert_eq!(
        sealed.compression_job().manifest_display_path(),
        manifest_path
    );
    assert_eq!(
        sealed.compression_job().destination_display_path(),
        sealed_path.with_file_name(format!(
            "{}.zst",
            sealed_path
                .file_name()
                .expect("sealed path must have a file name")
                .to_string_lossy()
        ))
    );
    assert_eq!(
        sealed.compression_job().expected_uncompressed_bytes(),
        manifest.segment_length()
    );
    assert_eq!(
        sealed.compression_job().expected_uncompressed_blake3(),
        manifest.segment_blake3()
    );
    assert_eq!(manifest.segment_id(), first.segment_id());
    assert_eq!(manifest.record_count(), 2);
    assert_eq!(manifest.first_record_offset(), first.frame_offset());
    assert_eq!(manifest.next_offset(), second.next_offset());
    assert_eq!(manifest.segment_length(), second.next_offset());
    assert_eq!(manifest.streams(), segment_metadata().streams());
    assert_ne!(manifest.segment_blake3(), &[0_u8; 32]);
    assert_ne!(manifest.manifest_blake3(), &[0_u8; 32]);

    let manifest_bytes = fs::read(&manifest_path).expect("manifest bytes must read");
    assert_eq!(
        decode_manifest(&manifest_bytes).expect("manifest must decode"),
        manifest
    );
    assert_eq!(
        manifest.encode().expect("manifest must encode"),
        manifest_bytes
    );

    assert!(!path.exists());
    let segment_before = fs::read(&sealed_path).expect("segment bytes must read");
    let manifest_before = manifest_bytes.clone();
    assert_eq!(
        verify_sealed_v2_segment(&sealed_path).expect("sealed segment must verify"),
        manifest
    );
    assert_eq!(
        fs::read(&sealed_path).expect("verification must leave segment bytes"),
        segment_before
    );
    assert_eq!(
        fs::read(&manifest_path).expect("verification must leave manifest bytes"),
        manifest_before
    );
}

#[test]
fn empty_segment_can_seal_at_its_creation_timestamp() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    let metadata = segment_metadata();
    let created_wall_time_ns = metadata.created_wall_time_ns();
    drop(Segment::create_v2(&path, metadata).expect("empty segment must create"));

    let sealed =
        seal_v2_segment(&path, created_wall_time_ns).expect("empty segment must seal immediately");
    let manifest = sealed.manifest();
    assert_eq!(manifest.record_count(), 0);
    assert_eq!(manifest.first_record_offset(), manifest.next_offset());
    assert!(manifest.sequence_ranges().is_empty());
    assert_eq!(manifest.min_receive_wall_time_ns(), None);
    assert_eq!(manifest.max_receive_wall_time_ns(), None);
    verify_sealed_v2_segment(&sealed.compression_job().source_display_path())
        .expect("empty sealed segment must verify");
}

#[test]
fn successor_manifest_binds_the_verified_predecessor_identity_and_digest() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let first_path = active_path_with_identity(directory.path(), 1, [0x5A; 16]);
    {
        let mut segment = Segment::create_v2(&first_path, segment_metadata_with_id([0x5A; 16]))
            .expect("first segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("first record must append");
    }
    let first =
        seal_v2_segment(&first_path, 1_721_234_568_000_000_000).expect("first segment must seal");

    let second_path = active_path_with_identity(directory.path(), 2, [0x6B; 16]);
    {
        let mut segment = Segment::create_v2(&second_path, segment_metadata_with_id([0x6B; 16]))
            .expect("second segment must create");
        segment
            .append_record_synced(record_metadata(7, 2), b"two")
            .expect("second record must append");
    }
    let second = seal_v2_segment_after(
        &second_path,
        first.manifest().as_predecessor(),
        1_721_234_569_000_000_000,
    )
    .expect("successor segment must seal");
    let predecessor = second
        .manifest()
        .predecessor()
        .expect("successor must bind its predecessor");
    assert_eq!(predecessor.segment_id(), first.manifest().segment_id());
    assert_eq!(
        predecessor.segment_blake3(),
        first.manifest().segment_blake3()
    );

    let second_sealed_path = sealed_path_for(&second_path).expect("second sealed path must derive");
    assert_eq!(
        verify_sealed_v2_segment(&second_sealed_path)
            .expect("successor must verify")
            .predecessor(),
        Some(predecessor)
    );
}

#[test]
fn invalid_predecessor_contract_fails_before_renaming_the_active_segment() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let second_path = active_path_with_identity(directory.path(), 2, [0x6B; 16]);
    drop(
        Segment::create_v2(&second_path, segment_metadata_with_id([0x6B; 16]))
            .expect("second segment must create"),
    );
    let active_before = fs::read(&second_path).expect("active bytes must read");

    assert!(matches!(
        seal_v2_segment(&second_path, 1_721_234_568_000_000_000),
        Err(SealingError::Manifest(ManifestError::InvalidField(
            "predecessor"
        )))
    ));
    assert_eq!(
        fs::read(&second_path).expect("active bytes must remain"),
        active_before
    );
    assert!(
        !sealed_path_for(&second_path)
            .expect("sealed path must derive")
            .exists()
    );

    let first_path = active_path(directory.path());
    drop(Segment::create_v2(&first_path, segment_metadata()).expect("first segment must create"));
    let predecessor = raw_wal::seal::SegmentPredecessor::new([0x11; 16], [0x22; 32])
        .expect("predecessor must be valid");
    assert!(matches!(
        seal_v2_segment_after(&first_path, predecessor, 1_721_234_568_000_000_000),
        Err(SealingError::Manifest(ManifestError::InvalidField(
            "predecessor"
        )))
    ));
    assert!(first_path.exists());
}

#[test]
fn sealing_rejects_an_incomplete_tail_until_active_recovery_repairs_it() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    let first;
    let second;
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        first = segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("first record must append");
        second = segment
            .append_record_synced(record_metadata(7, 2), b"two")
            .expect("second record must append");
    }
    OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("segment must open")
        .set_len(second.next_offset() - 3)
        .expect("tail must truncate");

    let truncated = fs::read(&path).expect("truncated active bytes must read");
    assert!(seal_v2_segment(&path, 1_721_234_568_000_000_000).is_err());
    assert_eq!(
        fs::read(&path).expect("sealing must not repair an active tail"),
        truncated
    );
    let mut segment = Segment::open_v2(&path).expect("active segment must reopen");
    let recovery = segment
        .recover()
        .expect("active startup recovery must repair the tail");
    assert_eq!(
        recovery.truncated_bytes(),
        second.next_offset() - first.next_offset() - 3
    );
    drop(segment);

    let sealed = seal_v2_segment(&path, 1_721_234_568_000_000_000)
        .expect("recovered active segment must seal");
    let manifest = sealed.manifest();
    let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
    assert_eq!(manifest.record_count(), 1);
    assert_eq!(manifest.next_offset(), first.next_offset());
    assert_eq!(
        fs::metadata(&sealed_path)
            .expect("segment must exist")
            .len(),
        first.next_offset()
    );
    verify_sealed_v2_segment(&sealed_path).expect("repaired sealed segment must verify");
}

#[test]
fn complete_corruption_refuses_sealing_and_preserves_every_byte() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    let length = fs::metadata(&path)
        .expect("segment metadata must read")
        .len();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .expect("segment must open");
    file.seek(SeekFrom::Start(length - 1))
        .expect("checksum byte must seek");
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).expect("checksum byte must read");
    file.seek(SeekFrom::Start(length - 1))
        .expect("checksum byte must seek");
    file.write_all(&[byte[0] ^ 0xFF])
        .expect("checksum byte must mutate");
    file.sync_all().expect("mutation must sync");
    drop(file);
    let corrupt = fs::read(&path).expect("corrupt bytes must read");

    assert!(matches!(
        seal_v2_segment(&path, 1_721_234_568_000_000_000),
        Err(SealingError::Recovery(RecoveryError::Corruption { .. }))
    ));
    let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
    assert!(
        !manifest_path_for(&sealed_path)
            .expect("manifest path must derive")
            .exists()
    );
    assert_eq!(fs::read(&path).expect("segment bytes must remain"), corrupt);
}

#[test]
fn existing_manifest_refuses_sealing_without_touching_either_file() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
    let manifest_path = manifest_path_for(&sealed_path).expect("manifest path must derive");
    fs::write(&manifest_path, b"sentinel").expect("sentinel manifest must write");
    let segment_before = fs::read(&path).expect("segment bytes must read");

    assert!(matches!(
        seal_v2_segment(&path, 1_721_234_568_000_000_000),
        Err(SealingError::ManifestExists)
    ));
    assert_eq!(
        fs::read(&path).expect("segment bytes must remain"),
        segment_before
    );
    assert_eq!(
        fs::read(&manifest_path).expect("manifest bytes must remain"),
        b"sentinel"
    );
}

#[test]
fn existing_sealed_target_refuses_sealing_without_touching_either_file() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
    fs::write(&sealed_path, b"sentinel").expect("sentinel segment must write");
    let active_before = fs::read(&path).expect("active bytes must read");

    assert!(matches!(
        seal_v2_segment(&path, 1_721_234_568_000_000_000),
        Err(SealingError::SealedExists)
    ));
    assert_eq!(
        fs::read(&path).expect("active bytes must remain"),
        active_before
    );
    assert_eq!(
        fs::read(&sealed_path).expect("sealed bytes must remain"),
        b"sentinel"
    );
}

#[test]
fn second_seal_is_idempotent_and_preserves_the_published_segment_and_manifest() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    seal_v2_segment(&path, 1_721_234_568_000_000_000).expect("first seal must succeed");
    let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
    let manifest_path = manifest_path_for(&sealed_path).expect("manifest path must derive");
    let segment_before = fs::read(&sealed_path).expect("segment bytes must read");
    let manifest_before = fs::read(&manifest_path).expect("manifest bytes must read");

    let repeated = seal_v2_segment(&path, 1_721_234_569_000_000_000)
        .expect("repeated seal must recover the published result");
    assert_eq!(
        repeated.manifest(),
        &decode_manifest(&manifest_before).expect("published manifest must decode")
    );
    assert_eq!(
        fs::read(&sealed_path).expect("segment bytes must remain"),
        segment_before
    );
    assert_eq!(
        fs::read(&manifest_path).expect("manifest bytes must remain"),
        manifest_before
    );
}

#[test]
fn orphan_sealed_segment_reports_a_missing_manifest_without_mutation() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
    fs::rename(&path, &sealed_path).expect("fixture must simulate rename-before-manifest crash");
    let orphan_before = fs::read(&sealed_path).expect("orphan bytes must read");

    assert!(matches!(
        verify_sealed_v2_segment(&sealed_path),
        Err(SealingError::ManifestMissing)
    ));
    assert_eq!(
        fs::read(&sealed_path).expect("orphan bytes must remain"),
        orphan_before
    );
    assert!(
        !manifest_path_for(&sealed_path)
            .expect("manifest path must derive")
            .exists()
    );
}

#[test]
fn pending_manifest_resumes_both_before_and_after_the_active_rename() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let reference_directory = directory.path().join("reference");
    fs::create_dir(&reference_directory).expect("reference directory must create");
    let reference_active = active_path(&reference_directory);
    {
        let mut segment = Segment::create_v2(&reference_active, segment_metadata())
            .expect("reference segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("reference record must append");
    }
    let reference =
        seal_v2_segment(&reference_active, 1_721_234_568_000_000_000).expect("reference must seal");
    let reference_manifest = fs::read(reference.compression_job().manifest_display_path())
        .expect("manifest bytes must read");

    for rename_before_resume in [false, true] {
        let case_directory = directory.path().join(if rename_before_resume {
            "after-rename"
        } else {
            "before-rename"
        });
        fs::create_dir(&case_directory).expect("case directory must create");
        let active = active_path(&case_directory);
        {
            let mut segment =
                Segment::create_v2(&active, segment_metadata()).expect("case segment must create");
            segment
                .append_record_synced(record_metadata(7, 1), b"one")
                .expect("case record must append");
        }
        let sealed = sealed_path_for(&active).expect("sealed path must derive");
        let pending = pending_manifest_path_for(&sealed).expect("pending path must derive");
        fs::write(&pending, &reference_manifest).expect("pending manifest must persist");
        if rename_before_resume {
            fs::rename(&active, &sealed).expect("fixture must simulate the active rename");
        }

        let resumed = seal_v2_segment(&active, 1_721_234_599_000_000_000)
            .expect("pending seal must resume idempotently");
        assert_eq!(resumed.manifest(), reference.manifest());
        assert!(!active.exists());
        assert!(!pending.exists());
        verify_sealed_v2_segment(&sealed).expect("resumed seal must verify");
    }
}

#[test]
fn symlinks_and_hardlinks_are_rejected_without_consuming_the_active_path() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let real = directory.path().join("real-segment");
    drop(Segment::create_v2(&real, segment_metadata()).expect("real segment must create"));
    let linked_active = active_path(directory.path());
    symlink(&real, &linked_active).expect("active symlink must create");

    assert!(matches!(
        seal_v2_segment(&linked_active, 1_721_234_568_000_000_000),
        Err(SealingError::UnsafeFile)
    ));
    assert!(
        fs::symlink_metadata(&linked_active)
            .expect("active symlink must remain")
            .file_type()
            .is_symlink()
    );
    assert!(real.exists());

    fs::remove_file(&linked_active).expect("test symlink must remove");
    fs::hard_link(&real, &linked_active).expect("active hardlink must create");
    assert!(matches!(
        seal_v2_segment(&linked_active, 1_721_234_568_000_000_000),
        Err(SealingError::UnsafeFile)
    ));
    assert!(linked_active.exists());
    assert!(real.exists());
}

#[test]
fn sealed_verification_rejects_a_symlink_instead_of_following_its_target() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let active = active_path(directory.path());
    drop(Segment::create_v2(&active, segment_metadata()).expect("segment must create"));
    seal_v2_segment(&active, 1_721_234_568_000_000_000).expect("segment must seal");
    let sealed = sealed_path_for(&active).expect("sealed path must derive");
    let relocated = directory.path().join("relocated.wal");
    fs::rename(&sealed, &relocated).expect("sealed segment must relocate for fixture");
    symlink(&relocated, &sealed).expect("sealed symlink must create");

    assert!(matches!(
        verify_sealed_v2_segment(&sealed),
        Err(SealingError::UnsafeFile)
    ));
    assert!(relocated.exists());
    assert!(
        fs::symlink_metadata(&sealed)
            .expect("sealed symlink must remain")
            .file_type()
            .is_symlink()
    );
}

#[test]
fn concurrent_sealers_publish_exactly_one_manifest() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let path = path.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            seal_v2_segment(&path, 1_721_234_568_000_000_000)
        }));
    }
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("sealer must not panic"))
        .collect();

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
    verify_sealed_v2_segment(&sealed_path).expect("winning manifest must verify");
}

#[test]
fn segment_or_manifest_mutation_fails_read_only_verification_without_repair() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let segment_path = active_path(directory.path());
    {
        let mut segment =
            Segment::create_v2(&segment_path, segment_metadata()).expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    seal_v2_segment(&segment_path, 1_721_234_568_000_000_000).expect("segment must seal");
    let sealed_segment_path = sealed_path_for(&segment_path).expect("sealed path must derive");
    let manifest_path = manifest_path_for(&sealed_segment_path).expect("manifest path must derive");
    let manifest_before = fs::read(&manifest_path).expect("manifest bytes must read");
    let segment_length = fs::metadata(&sealed_segment_path)
        .expect("segment metadata must read")
        .len();
    OpenOptions::new()
        .write(true)
        .open(&sealed_segment_path)
        .expect("segment must open for mutation")
        .set_len(segment_length - 1)
        .expect("sealed segment must truncate for test");
    let truncated = fs::read(&sealed_segment_path).expect("truncated bytes must read");

    assert!(verify_sealed_v2_segment(&sealed_segment_path).is_err());
    assert_eq!(
        fs::read(&sealed_segment_path).expect("verification must not repair"),
        truncated
    );
    assert_eq!(
        fs::read(&manifest_path).expect("manifest must remain"),
        manifest_before
    );

    let manifest_directory = directory.path().join("manifest");
    fs::create_dir(&manifest_directory).expect("manifest fixture directory must create");
    let manifest_segment_path = active_path(&manifest_directory);
    {
        let mut segment = Segment::create_v2(&manifest_segment_path, segment_metadata())
            .expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    seal_v2_segment(&manifest_segment_path, 1_721_234_568_000_000_000).expect("segment must seal");
    let sealed_manifest_segment_path =
        sealed_path_for(&manifest_segment_path).expect("sealed path must derive");
    let manifest_path =
        manifest_path_for(&sealed_manifest_segment_path).expect("manifest path must derive");
    let mut manifest_bytes = fs::read(&manifest_path).expect("manifest bytes must read");
    let mutation_offset = manifest_bytes
        .iter()
        .position(|byte| *byte == b'b')
        .expect("manifest fixture must contain a mutable byte");
    manifest_bytes[mutation_offset] = b'c';
    fs::write(&manifest_path, &manifest_bytes).expect("manifest must mutate");
    let segment_before = fs::read(&sealed_manifest_segment_path).expect("segment bytes must read");

    assert!(verify_sealed_v2_segment(&sealed_manifest_segment_path).is_err());
    assert_eq!(
        fs::read(&sealed_manifest_segment_path).expect("segment must remain"),
        segment_before
    );
    assert_eq!(
        fs::read(&manifest_path).expect("manifest mutation must remain"),
        manifest_bytes
    );
}

#[test]
fn maximum_supported_stream_and_sequence_cardinality_seals_within_the_manifest_bound() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    let streams = (1..=MAX_STREAMS)
        .map(|stream_id| {
            StreamDescriptor::new(
                u32::try_from(stream_id).expect("stream ID must fit"),
                "s",
                format!("t{stream_id}"),
            )
            .expect("descriptor must be valid")
        })
        .collect();
    let metadata = SegmentMetadata::new(
        [0x5A; 16],
        1_721_234_567_000_000_000,
        "s",
        "i",
        "b",
        streams,
    )
    .expect("maximum metadata must be valid");
    drop(Segment::create_v2(&path, metadata).expect("maximum segment must create"));

    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("maximum segment must reopen");
    for stream_id in 1..=MAX_STREAMS {
        let stream_id = u32::try_from(stream_id).expect("stream ID must fit");
        file.write_all(
            &frame::encode(
                RecordMetadata {
                    flags: 0,
                    stream_id,
                    connection_epoch: u64::MAX,
                    record_sequence: u64::MAX,
                    receive_wall_time_ns: i64::MAX,
                    receive_monotonic_time_ns: u64::MAX,
                },
                b"x",
            )
            .expect("maximum frame must encode"),
        )
        .expect("maximum frame must append");
    }
    file.sync_all().expect("maximum fixture must sync");
    drop(file);

    let sealed =
        seal_v2_segment(&path, 1_721_234_568_000_000_000).expect("maximum segment must seal");
    let encoded = sealed
        .manifest()
        .encode()
        .expect("maximum manifest must encode");
    assert!(
        encoded.len() > 256 * 1024,
        "fixture must cover the rejected legacy manifest bound"
    );
    assert!(encoded.len() <= MAX_MANIFEST_LENGTH);
    assert_eq!(sealed.manifest().streams().len(), MAX_STREAMS);
    assert_eq!(sealed.manifest().sequence_ranges().len(), MAX_STREAMS);
    verify_sealed_v2_segment(&sealed.compression_job().source_display_path())
        .expect("maximum sealed segment must verify");
}

#[test]
fn segmented_length_bound_accepts_the_last_byte_and_rejects_the_next_before_hashing() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let active = active_path(directory.path());
    drop(Segment::create_v2(&active, segment_metadata()).expect("segment must create"));
    seal_v2_segment(&active, 1_721_234_568_000_000_000).expect("segment must seal");
    let sealed = sealed_path_for(&active).expect("sealed path must derive");
    let manifest_path = manifest_path_for(&sealed).expect("manifest path must derive");

    let original = fs::read(&manifest_path).expect("manifest must read");
    let original_manifest = decode_manifest(&original).expect("manifest must decode");
    let maximum_length = MAX_RECOVERABLE_SEGMENT_LENGTH
        .checked_add(original_manifest.prologue_length())
        .expect("maximum segmented length must fit");
    OpenOptions::new()
        .write(true)
        .open(&sealed)
        .expect("sealed segment must reopen")
        .set_len(maximum_length)
        .expect("last accepted sparse byte must extend");
    fs::write(
        &manifest_path,
        manifest_with_segment_length(
            &original,
            original_manifest.segment_length(),
            maximum_length,
        ),
    )
    .expect("last accepted manifest must write");
    assert!(matches!(
        verify_sealed_v2_segment(&sealed),
        Err(SealingError::Recovery(_))
    ));

    let oversized_length = maximum_length + 1;
    OpenOptions::new()
        .write(true)
        .open(&sealed)
        .expect("sealed segment must reopen")
        .set_len(oversized_length)
        .expect("first rejected sparse byte must extend");
    fs::write(
        &manifest_path,
        manifest_with_segment_length(
            &original,
            original_manifest.segment_length(),
            oversized_length,
        ),
    )
    .expect("oversized manifest must write");
    let oversized_result = verify_sealed_v2_segment(&sealed);
    assert!(
        matches!(
            oversized_result,
            Err(SealingError::Manifest(ManifestError::InvalidField(
                "offsets"
            )))
        ),
        "unexpected oversized result: {oversized_result:?}"
    );
}

#[test]
fn manifest_decoder_rejects_unknown_fields_noncanonical_json_and_bad_self_digest() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let path = active_path(directory.path());
    {
        let mut segment =
            Segment::create_v2(&path, segment_metadata()).expect("segment must create");
        segment
            .append_record_synced(record_metadata(7, 1), b"one")
            .expect("record must append");
    }
    let sealed = seal_v2_segment(&path, 1_721_234_568_000_000_000).expect("segment must seal");
    let manifest = sealed.manifest();
    let canonical = manifest.encode().expect("manifest must encode");

    let mut spaced = Vec::with_capacity(canonical.len() + 1);
    spaced.push(b' ');
    spaced.extend_from_slice(&canonical);
    assert!(matches!(
        decode_manifest(&spaced),
        Err(ManifestError::NonCanonical)
    ));

    let mut value: serde_json::Value =
        serde_json::from_slice(&canonical).expect("canonical manifest must be JSON");
    value
        .as_object_mut()
        .expect("manifest must be an object")
        .insert("unknown".into(), serde_json::Value::Bool(true));
    let unknown = serde_json::to_vec(&value).expect("mutated JSON must encode");
    assert!(matches!(
        decode_manifest(&unknown),
        Err(ManifestError::Json(_))
    ));

    let ascending_streams = "\"streams\":[{\"stream_id\":7,\"source_name\":\"binance\",\"stream_name\":\"spot-btcusdt\"},{\"stream_id\":9,\"source_name\":\"kraken\",\"stream_name\":\"spot-xbtusd\"}]";
    let descending_streams = "\"streams\":[{\"stream_id\":9,\"source_name\":\"kraken\",\"stream_name\":\"spot-xbtusd\"},{\"stream_id\":7,\"source_name\":\"binance\",\"stream_name\":\"spot-btcusdt\"}]";
    let reordered = String::from_utf8(canonical.clone())
        .expect("manifest must be UTF-8")
        .replacen(ascending_streams, descending_streams, 1);
    assert!(reordered.contains(descending_streams));
    let reordered = recompute_canonical_manifest_body_digest(reordered.into_bytes());
    let reordered_result = decode_manifest(&reordered);
    assert!(
        matches!(
            reordered_result,
            Err(ManifestError::InvalidField("streams"))
        ),
        "unexpected reordered result: {reordered_result:?}"
    );

    let mut digest_mutated = canonical.clone();
    let digest_position = digest_mutated
        .windows(64)
        .rposition(|window| window.iter().all(u8::is_ascii_hexdigit))
        .expect("manifest must contain a hex digest");
    digest_mutated[digest_position] = if digest_mutated[digest_position] == b'0' {
        b'1'
    } else {
        b'0'
    };
    assert!(matches!(
        decode_manifest(&digest_mutated),
        Err(ManifestError::DigestMismatch)
    ));
    assert_eq!(
        decode_manifest(&canonical)
            .expect("canonical manifest must decode")
            .encode()
            .expect("decoded manifest must re-encode"),
        canonical
    );
}

#[test]
fn legacy_and_record_only_v2_files_cannot_be_sealed_as_segmented_v2() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    for (name, bytes) in [
        ("legacy.wal", b"legacy".to_vec()),
        (
            "record-only.wal",
            raw_wal::frame::encode(record_metadata(7, 1), b"one")
                .expect("record-only frame must encode"),
        ),
    ] {
        let fixture_directory = directory.path().join(name);
        fs::create_dir(&fixture_directory).expect("fixture directory must create");
        let path = active_path(&fixture_directory);
        fs::write(&path, &bytes).expect("fixture must write");
        assert!(seal_v2_segment(&path, 1_721_234_568_000_000_000).is_err());
        let sealed_path = sealed_path_for(&path).expect("sealed path must derive");
        assert!(
            !manifest_path_for(&sealed_path)
                .expect("manifest path must derive")
                .exists()
        );
        assert_eq!(fs::read(&path).expect("fixture bytes must remain"), bytes);
    }
}
