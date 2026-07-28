use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    sync::{Arc, Barrier},
    thread,
};

use raw_wal::{
    frame::{self, RecordMetadata},
    manager::{InventoryError, ManagerError, RotationPolicy, SegmentedWalWriter},
    prologue::{self, SegmentMetadata, StreamDescriptor},
    recovery::TARGET_SEGMENT_LENGTH,
    seal::{
        manifest_path_for, pending_manifest_path_for, seal_v2_segment, seal_v2_segment_after,
        sealed_path_for, verify_sealed_v2_segment,
    },
    segment::Segment,
};

const CREATED_WALL_NS: i64 = 1_721_234_567_000_000_000;

#[test]
fn rotation_policy_never_allows_an_active_segment_beyond_the_recovery_target() {
    assert!(
        RotationPolicy::new(TARGET_SEGMENT_LENGTH, 1)
            .expect("the recovery target must remain an accepted rotation boundary")
            .max_segment_bytes()
            == TARGET_SEGMENT_LENGTH
    );
    assert!(matches!(
        RotationPolicy::new(TARGET_SEGMENT_LENGTH + 1, 1),
        Err(ManagerError::InvalidPolicy)
    ));
}

fn metadata() -> SegmentMetadata {
    SegmentMetadata::new(
        [0x31; 16],
        CREATED_WALL_NS,
        "market-schema-v2",
        "installation-test",
        "build-test",
        vec![
            StreamDescriptor::new(7, "binance", "spot-btcusdt").expect("descriptor must be valid"),
        ],
    )
    .expect("segment metadata must be valid")
}

fn record(sequence: u64) -> RecordMetadata {
    RecordMetadata {
        flags: 0,
        stream_id: 7,
        connection_epoch: 4,
        record_sequence: sequence,
        receive_wall_time_ns: CREATED_WALL_NS + sequence as i64,
        receive_monotonic_time_ns: 1_000 + sequence,
    }
}

fn segment_limit_for_one(payload: &[u8]) -> u64 {
    u64::try_from(
        prologue::encode(&metadata())
            .expect("prologue must encode")
            .len(),
    )
    .expect("prologue length must fit")
        + u64::try_from(
            frame::encode(record(1), payload)
                .expect("frame must encode")
                .len(),
        )
        .expect("frame length must fit")
}

fn reference_pending_manifest() -> Vec<u8> {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
        .expect("writer must create");
    writer
        .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
        .expect("first append must succeed");
    let rotated = writer
        .poll_rotation(300_000_000_011, CREATED_WALL_NS + 2)
        .expect("poll must succeed")
        .expect("poll must rotate");
    fs::read(rotated.manifest_display_path()).expect("reference manifest must read")
}

#[test]
fn byte_rotation_happens_before_append_and_returns_cross_segment_position_and_job() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let payload = b"one";
    let policy = RotationPolicy::new(segment_limit_for_one(payload), 300_000_000_000)
        .expect("policy must be valid");
    let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
        .expect("writer must create");

    let first = writer
        .append(record(1), payload, 11, CREATED_WALL_NS + 1)
        .expect("first append must succeed");
    assert_eq!(first.position().segment_ordinal(), 1);
    assert_eq!(first.position().segment_id(), &[0x31; 16]);
    assert!(first.compression_job().is_none());
    let first_next_offset = first.position().next_offset();

    let second = writer
        .append(record(2), payload, 12, CREATED_WALL_NS + 2)
        .expect("second append must rotate then succeed");
    assert_eq!(second.position().segment_ordinal(), 2);
    assert_ne!(
        second.position().segment_id(),
        first.position().segment_id()
    );
    assert!(second.position().frame_offset() < first_next_offset);
    let job = second
        .compression_job()
        .expect("rotation must return a compression job");
    assert!(job.manifest_display_path().exists());
    assert!(job.source_display_path().exists());
    assert!(!job.destination_display_path().exists());

    let first_manifest =
        verify_sealed_v2_segment(&job.source_display_path()).expect("first segment must verify");
    assert_eq!(first_manifest.segment_ordinal(), 1);
    assert!(first_manifest.predecessor().is_none());
    assert_eq!(writer.active_ordinal(), 2);
    assert_eq!(writer.active_record_count(), 1);
}

#[test]
fn capability_open_or_create_recovers_existing_chain_instead_of_reinitializing() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::default();
    let mut writer = SegmentedWalWriter::open_or_create_in(
        File::open(directory.path()).expect("directory capability must open"),
        directory.path(),
        metadata(),
        policy,
        10,
        CREATED_WALL_NS,
    )
    .expect("empty capability directory must create a chain");
    writer
        .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
        .expect("first append must succeed");
    let first_length = writer.active_length();
    drop(writer);

    let replacement_initial_metadata = SegmentMetadata::new(
        [0x41; 16],
        CREATED_WALL_NS + 1_000,
        "replacement-schema",
        "replacement-installation",
        "replacement-build",
        vec![
            StreamDescriptor::new(7, "binance", "spot-btcusdt").expect("descriptor must be valid"),
        ],
    )
    .expect("replacement metadata must be valid");
    let recovered = SegmentedWalWriter::open_or_create_in(
        File::open(directory.path()).expect("directory capability must reopen"),
        directory.path(),
        replacement_initial_metadata,
        policy,
        20,
        CREATED_WALL_NS + 1_000,
    )
    .expect("existing capability directory must recover");

    assert_eq!(recovered.active_ordinal(), 2);
    assert_eq!(recovered.active_record_count(), 0);
    assert!(recovered.active_length() < first_length);
}

#[test]
fn recovery_conservatively_seals_a_nonempty_active_segment_once() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::default();
    {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("record must append");
    }

    let first = SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100)
        .expect("nonempty active segment must recover");
    assert_eq!(first.active_ordinal(), 2);
    assert_eq!(first.active_record_count(), 0);
    drop(first);

    let second = SegmentedWalWriter::recover(directory.path(), policy, 30, CREATED_WALL_NS + 200)
        .expect("empty successor must recover without another rotation");
    assert_eq!(second.active_ordinal(), 2);
    assert_eq!(second.active_record_count(), 0);
}

#[test]
fn validated_replay_visits_sealed_then_active_records_in_chain_order() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        for (sequence, payload) in [(1, b"one".as_slice()), (2, b"two"), (3, b"tri")] {
            writer
                .append(
                    record(sequence),
                    payload,
                    10 + sequence,
                    CREATED_WALL_NS + sequence as i64,
                )
                .expect("append must succeed");
        }
    }

    let mut payloads = Vec::new();
    let recovered = SegmentedWalWriter::recover_with_replay(
        directory.path(),
        policy,
        20,
        CREATED_WALL_NS + 100,
        |record| payloads.push(record.payload().to_vec()),
    )
    .expect("validated records must replay during recovery");

    assert_eq!(
        payloads,
        [b"one".to_vec(), b"two".to_vec(), b"tri".to_vec()]
    );
    assert_eq!(recovered.active_record_count(), 0);
}

#[test]
fn manager_sync_is_explicit_and_keeps_records_recoverable() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::default();
    {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"durable", 11, CREATED_WALL_NS + 1)
            .expect("append must succeed");
        writer.sync().expect("explicit sync must succeed");
    }

    let mut recovered =
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100)
            .expect("chain must recover");
    let mut payloads = Vec::new();
    recovered
        .visit_records(|record| payloads.push(record.payload().to_vec()))
        .expect("validated records must replay");

    assert_eq!(payloads, [b"durable".to_vec()]);
}

#[test]
fn monotonic_idle_poll_rotates_exactly_at_age_boundary_despite_wall_clock_jumps() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(1024 * 1024, 5).expect("policy must be valid");
    let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 100)
        .expect("writer must create");
    writer
        .append(record(1), b"one", 101, i64::MAX)
        .expect("append must succeed");

    assert!(
        writer
            .poll_rotation(104, CREATED_WALL_NS - 10_000)
            .expect("pre-boundary poll must succeed")
            .is_none()
    );
    let job = writer
        .poll_rotation(105, CREATED_WALL_NS - 20_000)
        .expect("boundary poll must succeed")
        .expect("boundary poll must rotate");
    assert!(job.manifest_display_path().exists());
    assert_eq!(writer.active_ordinal(), 2);
    assert_eq!(writer.active_record_count(), 0);
    assert!(
        writer
            .poll_rotation(1_000, CREATED_WALL_NS)
            .expect("empty poll must succeed")
            .is_none(),
        "empty active segments must not rotate repeatedly"
    );
}

#[test]
fn sequence_regression_across_rotation_is_rejected_before_writing_successor_bytes() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
        .expect("writer must create");
    writer
        .append(record(10), b"one", 11, CREATED_WALL_NS + 1)
        .expect("first append must succeed");
    writer
        .poll_rotation(300_000_000_011, CREATED_WALL_NS + 2)
        .expect("poll must succeed")
        .expect("poll must rotate");
    let length_before = writer.active_length();

    assert!(matches!(
        writer.append(record(10), b"two", 300_000_000_012, CREATED_WALL_NS + 3),
        Err(ManagerError::SequenceRegression {
            stream_id: 7,
            connection_epoch: 4,
            previous: 10,
            current: 10,
        })
    ));
    assert_eq!(writer.active_length(), length_before);
    assert_eq!(writer.active_record_count(), 0);
}

#[test]
fn an_invalid_frame_is_rejected_before_policy_rotation_mutates_the_chain() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
        .expect("writer must create");
    writer
        .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
        .expect("first append must succeed");
    let active_before = writer
        .active_path()
        .expect("active path must remain anchored")
        .to_owned();
    let length_before = writer.active_length();
    let mut invalid = record(2);
    invalid.flags = 1;

    assert!(matches!(
        writer.append(invalid, b"two", 12, CREATED_WALL_NS + 2),
        Err(ManagerError::Frame(_))
    ));
    assert_eq!(writer.active_ordinal(), 1);
    assert_eq!(
        writer
            .active_path()
            .expect("active path must remain anchored"),
        active_before
    );
    assert_eq!(writer.active_length(), length_before);
    assert_eq!(writer.active_record_count(), 1);
    assert_eq!(
        fs::read_dir(directory.path())
            .expect("directory must read")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".seal.json"))
            .count(),
        0
    );

    let mut undeclared = record(2);
    undeclared.stream_id = 99;
    assert!(matches!(
        writer.append(undeclared, b"two", 12, CREATED_WALL_NS + 2),
        Err(ManagerError::Segment(
            raw_wal::segment::SegmentError::UndeclaredStream(99)
        ))
    ));
    assert_eq!(writer.active_ordinal(), 1);
    assert_eq!(
        writer
            .active_path()
            .expect("active path must remain anchored"),
        active_before
    );
    assert_eq!(writer.active_length(), length_before);
    assert_eq!(writer.active_record_count(), 1);
}

#[test]
fn one_oversized_first_frame_is_allowed_then_the_next_append_rotates_first() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let prologue_length = u64::try_from(
        prologue::encode(&metadata())
            .expect("prologue must encode")
            .len(),
    )
    .expect("prologue length must fit");
    let policy = RotationPolicy::new(prologue_length + 1, 300_000_000_000)
        .expect("small policy must be valid");
    let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
        .expect("writer must create");

    let first = writer
        .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
        .expect("first oversized frame must be accepted");
    assert_eq!(first.position().segment_ordinal(), 1);
    assert!(first.compression_job().is_none());
    let second = writer
        .append(record(2), b"two", 12, CREATED_WALL_NS + 2)
        .expect("second append must rotate first");
    assert_eq!(second.position().segment_ordinal(), 2);
    assert!(second.compression_job().is_some());
}

#[test]
fn recovering_a_clean_chain_restores_active_position_and_global_sequence_state() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("first append must succeed");
        writer
            .append(record(2), b"two", 12, CREATED_WALL_NS + 2)
            .expect("second append must rotate");
    }

    let mut recovered =
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100)
            .expect("clean chain must recover");
    assert_eq!(recovered.active_ordinal(), 3);
    assert_eq!(recovered.active_record_count(), 0);
    let length_before = recovered.active_length();
    assert!(matches!(
        recovered.append(record(2), b"duplicate", 21, CREATED_WALL_NS + 3),
        Err(ManagerError::SequenceRegression { .. })
    ));
    assert_eq!(recovered.active_length(), length_before);
    recovered
        .append(record(3), b"three", 22, CREATED_WALL_NS + 4)
        .expect("next global sequence must append");

    let names: Vec<_> = fs::read_dir(directory.path())
        .expect("directory must read")
        .map(|entry| {
            entry
                .expect("entry must read")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(names.iter().any(|name| name.ends_with(".seal.json")));
    assert_eq!(
        names
            .iter()
            .filter(|name| name.ends_with(".active.wal"))
            .count(),
        1
    );
}

#[test]
fn a_second_live_manager_is_rejected_by_the_directory_lock() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::default();
    let _writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
        .expect("first writer must create");

    assert!(matches!(
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100),
        Err(ManagerError::AlreadyOpen)
    ));
}

#[test]
fn live_writer_fails_closed_after_directory_rename_and_path_replacement() {
    let parent = tempfile::tempdir().expect("temporary parent must exist");
    let original = parent.path().join("wal");
    let moved = parent.path().join("wal-moved");
    fs::create_dir(&original).expect("managed directory must create");
    let policy = RotationPolicy::default();
    let mut first = SegmentedWalWriter::create(&original, metadata(), policy, 10)
        .expect("first writer must create");
    first
        .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
        .expect("first append must succeed");
    let first_name = first
        .active_path()
        .expect("first path must initially be anchored")
        .file_name()
        .expect("first active filename must exist")
        .to_owned();
    fs::rename(&original, &moved).expect("managed directory must move");
    fs::create_dir(&original).expect("replacement directory must create");
    let second = SegmentedWalWriter::create(&original, metadata(), policy, 20)
        .expect("replacement directory is a distinct managed inode");

    assert!(matches!(
        first.active_path(),
        Err(ManagerError::DirectoryReplaced)
    ));
    assert!(matches!(
        first.append(record(2), b"two", 21, CREATED_WALL_NS + 2),
        Err(ManagerError::DirectoryReplaced)
    ));
    assert!(matches!(
        first.poll_rotation(300_000_000_011, CREATED_WALL_NS + 3),
        Err(ManagerError::DirectoryReplaced)
    ));
    assert!(moved.join(first_name).exists());
    assert!(
        second
            .active_path()
            .expect("replacement writer path must remain anchored")
            .starts_with(&original)
    );
    assert!(matches!(
        SegmentedWalWriter::recover(&moved, policy, 30, CREATED_WALL_NS + 100),
        Err(ManagerError::AlreadyOpen)
    ));
}

#[test]
fn concurrent_directory_replacement_never_turns_a_durable_append_into_an_error() {
    let parent = tempfile::tempdir().expect("temporary parent must exist");
    let original = parent.path().join("wal");
    let moved = parent.path().join("wal-moved");
    fs::create_dir(&original).expect("managed directory must create");
    let policy = RotationPolicy::default();
    let writer =
        SegmentedWalWriter::create(&original, metadata(), policy, 10).expect("writer must create");
    let active_name = writer
        .active_path()
        .expect("active path must initially be anchored")
        .file_name()
        .expect("active filename must exist")
        .to_owned();
    let active_before = fs::metadata(original.join(&active_name))
        .expect("active metadata must read")
        .len();
    let barrier = Arc::new(Barrier::new(2));
    let append_barrier = Arc::clone(&barrier);
    let append_thread = thread::spawn(move || {
        let mut writer = writer;
        let payload = vec![0x5a; frame::MAX_PAYLOAD_LENGTH];
        append_barrier.wait();
        let result = writer.append(record(1), &payload, 11, CREATED_WALL_NS + 1);
        (writer, result)
    });

    barrier.wait();
    fs::rename(&original, &moved).expect("managed directory must move");
    fs::create_dir(&original).expect("replacement directory must create");
    let (writer, result) = append_thread.join().expect("append thread must finish");
    let moved_active = moved.join(&active_name);
    let active_after = fs::metadata(&moved_active)
        .expect("moved active metadata must read")
        .len();

    assert!(
        fs::read_dir(&original)
            .expect("replacement directory must read")
            .next()
            .is_none(),
        "the replacement directory must never receive manager artifacts"
    );
    match result {
        Ok(outcome) => {
            assert!(active_after > active_before);
            assert_eq!(outcome.position().next_offset(), active_after);
            assert_eq!(writer.active_length(), active_after);
            assert_eq!(writer.active_record_count(), 1);
        }
        Err(ManagerError::DirectoryReplaced) => {
            assert_eq!(active_after, active_before);
            assert_eq!(writer.active_length(), active_before);
            assert_eq!(writer.active_record_count(), 0);
        }
        Err(error) => panic!("unexpected append result during directory replacement: {error}"),
    }
}

#[test]
fn concurrent_directory_replacement_never_redirects_rotation_artifacts() {
    let parent = tempfile::tempdir().expect("temporary parent must exist");
    let original = parent.path().join("wal");
    let moved = parent.path().join("wal-moved");
    fs::create_dir(&original).expect("managed directory must create");
    let policy = RotationPolicy::new(64 * 1024 * 1024, 5).expect("policy must be valid");
    let mut writer =
        SegmentedWalWriter::create(&original, metadata(), policy, 10).expect("writer must create");
    let payload = vec![0x5a; frame::MAX_PAYLOAD_LENGTH];
    writer
        .append(record(1), &payload, 11, CREATED_WALL_NS + 1)
        .expect("large append must succeed");
    let first_active_name = writer
        .active_path()
        .expect("active path must initially be anchored")
        .file_name()
        .expect("active filename must exist")
        .to_owned();
    let barrier = Arc::new(Barrier::new(2));
    let rotation_barrier = Arc::clone(&barrier);
    let rotation_thread = thread::spawn(move || {
        let mut writer = writer;
        rotation_barrier.wait();
        let result = writer.poll_rotation(15, CREATED_WALL_NS + 2);
        (writer, result)
    });

    barrier.wait();
    fs::rename(&original, &moved).expect("managed directory must move");
    fs::create_dir(&original).expect("replacement directory must create");
    let (writer, result) = rotation_thread.join().expect("rotation thread must finish");

    assert!(
        fs::read_dir(&original)
            .expect("replacement directory must read")
            .next()
            .is_none(),
        "the replacement directory must never receive rotation artifacts"
    );
    match result {
        Ok(Some(job)) => {
            assert!(
                job.source_display_path().exists() && job.manifest_display_path().exists(),
                "a successful handoff must never expose stale full paths"
            );
            let source_name = job.source_name();
            let manifest_name = job.manifest_name();
            assert!(moved.join(source_name).exists());
            assert!(moved.join(manifest_name).exists());
            assert_eq!(writer.active_ordinal(), 2);
            assert_eq!(
                fs::read_dir(&moved)
                    .expect("moved directory must read")
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".active.wal"))
                    .count(),
                1
            );
        }
        Err(ManagerError::DirectoryReplacedAfterRotation { sealed_ordinal: 1 }) => {
            assert_eq!(writer.active_ordinal(), 2);
            assert_eq!(
                fs::read_dir(&moved)
                    .expect("moved directory must read")
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".active.wal"))
                    .count(),
                1
            );
            assert_eq!(
                fs::read_dir(&moved)
                    .expect("moved directory must read")
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".seal.json"))
                    .count(),
                1
            );
            drop(writer);
            let recovered = SegmentedWalWriter::recover(&moved, policy, 20, CREATED_WALL_NS + 100)
                .expect("typed committed rotation must recover at the moved path");
            let mut recovered_jobs = Vec::new();
            recovered
                .visit_pending_compression_jobs(|job| recovered_jobs.push(job))
                .expect("moved recovery path must remain anchored");
            assert_eq!(recovered_jobs.len(), 1);
            assert!(recovered_jobs[0].source_display_path().exists());
            assert!(recovered_jobs[0].manifest_display_path().exists());
            assert!(recovered_jobs[0].source_display_path().starts_with(&moved));
        }
        Err(ManagerError::DirectoryReplaced) => {
            assert!(moved.join(first_active_name).exists());
            assert_eq!(writer.active_ordinal(), 1);
        }
        Ok(None) => panic!("age-due rotation unexpectedly returned no job"),
        Err(error) => panic!("unexpected rotation result during directory replacement: {error}"),
    }
}

#[test]
fn concurrent_directory_replacement_never_exposes_stale_recovery_jobs() {
    let parent = tempfile::tempdir().expect("temporary parent must exist");
    let original = parent.path().join("wal");
    let moved = parent.path().join("wal-moved");
    fs::create_dir(&original).expect("managed directory must create");
    let policy = RotationPolicy::new(64 * 1024 * 1024, 5).expect("policy must be valid");
    {
        let mut writer = SegmentedWalWriter::create(&original, metadata(), policy, 10)
            .expect("writer must create");
        let payload = vec![0x5a; frame::MAX_PAYLOAD_LENGTH];
        writer
            .append(record(1), &payload, 11, CREATED_WALL_NS + 1)
            .expect("large append must succeed");
        writer
            .poll_rotation(15, CREATED_WALL_NS + 2)
            .expect("fixture rotation must succeed")
            .expect("fixture rotation must return a job");
    }
    let barrier = Arc::new(Barrier::new(2));
    let recovery_barrier = Arc::clone(&barrier);
    let recovery_path = original.clone();
    let recovery_thread = thread::spawn(move || {
        recovery_barrier.wait();
        SegmentedWalWriter::recover(&recovery_path, policy, 20, CREATED_WALL_NS + 100)
    });

    barrier.wait();
    fs::rename(&original, &moved).expect("managed directory must move");
    fs::create_dir(&original).expect("replacement directory must create");
    match recovery_thread.join().expect("recovery thread must finish") {
        Ok(recovered) => assert!(matches!(
            recovered.pending_compression_job_count(),
            Err(ManagerError::DirectoryReplaced)
        )),
        Err(ManagerError::DirectoryReplacedAfterRecovery | ManagerError::DirectoryReplaced) => {}
        Err(ManagerError::Inventory(InventoryError::MissingActiveSegment)) => {
            // Recovery began after the replacement was installed and exposed no job.
        }
        Err(error) => panic!("unexpected recovery result during directory replacement: {error}"),
    }
    assert!(
        fs::read_dir(&original)
            .expect("replacement directory must read")
            .next()
            .is_none(),
        "the replacement directory must never receive recovery artifacts"
    );
    assert_eq!(
        fs::read_dir(&moved)
            .expect("moved directory must read")
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".wal") && !name.ends_with(".active.wal")
            })
            .count(),
        1
    );
}

#[test]
fn recovery_rejects_unknown_symlink_and_hardlink_artifacts_without_mutation() {
    let policy = RotationPolicy::default();

    let unknown_directory = tempfile::tempdir().expect("temporary directory must exist");
    let unknown_active = {
        let writer = SegmentedWalWriter::create(unknown_directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    fs::write(
        unknown_directory.path().join("operator-note.txt"),
        b"do not consume",
    )
    .expect("unknown artifact must write");
    assert!(matches!(
        SegmentedWalWriter::recover(unknown_directory.path(), policy, 20, CREATED_WALL_NS + 100,),
        Err(ManagerError::Inventory(InventoryError::UnknownArtifact))
    ));
    assert!(unknown_active.exists());

    let symlink_directory = tempfile::tempdir().expect("temporary directory must exist");
    let symlink_active = {
        let writer = SegmentedWalWriter::create(symlink_directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    let symlink_target = symlink_directory.path().join("outside-contract");
    fs::rename(&symlink_active, &symlink_target).expect("active must move for fixture");
    symlink(&symlink_target, &symlink_active).expect("active symlink must create");
    assert!(
        SegmentedWalWriter::recover(symlink_directory.path(), policy, 20, CREATED_WALL_NS + 100,)
            .is_err()
    );
    assert!(
        fs::symlink_metadata(&symlink_active)
            .expect("symlink must remain")
            .file_type()
            .is_symlink()
    );

    let hardlink_directory = tempfile::tempdir().expect("temporary directory must exist");
    let hardlink_active = {
        let writer = SegmentedWalWriter::create(hardlink_directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    let hardlink =
        hardlink_directory
            .path()
            .join(format!("{:020}-{}.active.wal", 2, hex::encode([0x41; 16])));
    fs::hard_link(&hardlink_active, &hardlink).expect("hardlink fixture must create");
    assert!(matches!(
        SegmentedWalWriter::recover(hardlink_directory.path(), policy, 20, CREATED_WALL_NS + 100,),
        Err(ManagerError::Inventory(InventoryError::UnsafeArtifact))
    ));
    assert!(hardlink_active.exists());
    assert!(hardlink.exists());
}

#[test]
fn recovery_creates_the_deterministic_successor_after_a_durable_seal() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    let missing_successor = {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("append must succeed");
        writer
            .poll_rotation(300_000_000_011, CREATED_WALL_NS + 2)
            .expect("poll must succeed")
            .expect("poll must rotate");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    fs::remove_file(&missing_successor).expect("fixture must remove unacknowledged successor");

    let mut recovered =
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100)
            .expect("sealed chain without active must recover");
    assert_eq!(recovered.active_ordinal(), 2);
    assert_eq!(
        recovered
            .active_path()
            .expect("active path must remain anchored"),
        missing_successor
    );
    assert!(missing_successor.exists());
    let mut jobs = Vec::new();
    recovered
        .visit_pending_compression_jobs(|job| jobs.push(job))
        .expect("recovery path must remain anchored");
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].source_display_path().exists());
    assert!(jobs[0].manifest_display_path().exists());
    recovered
        .append(record(2), b"two", 21, CREATED_WALL_NS + 3)
        .expect("recovered successor must accept the next global sequence");
}

#[test]
fn recovery_resumes_pending_seals_before_and_after_the_active_rename() {
    let reference_manifest = reference_pending_manifest();
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");

    for rename_before_recovery in [false, true] {
        let directory = tempfile::tempdir().expect("temporary directory must exist");
        let active = {
            let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
                .expect("writer must create");
            writer
                .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
                .expect("append must succeed");
            writer
                .active_path()
                .expect("active path must remain anchored")
                .to_owned()
        };
        let sealed = sealed_path_for(&active).expect("sealed path must derive");
        let pending = pending_manifest_path_for(&sealed).expect("pending path must derive");
        fs::write(&pending, &reference_manifest).expect("pending state must persist");
        fs::set_permissions(&pending, fs::Permissions::from_mode(0o600))
            .expect("pending state must be private");
        if rename_before_recovery {
            fs::rename(&active, &sealed).expect("fixture must simulate active rename");
        }

        let mut replayed = Vec::new();
        let mut recovered = SegmentedWalWriter::recover_with_replay(
            directory.path(),
            policy,
            20,
            CREATED_WALL_NS + 100,
            |record| replayed.push(record.payload().to_vec()),
        )
        .expect("pending transition must recover");
        assert_eq!(
            replayed,
            vec![b"one".to_vec()],
            "pending seal recovery must replay the finalized segment exactly once"
        );
        assert_eq!(recovered.active_ordinal(), 2);
        assert!(!active.exists());
        assert!(!pending.exists());
        assert!(sealed.exists());
        let manifest = manifest_path_for(&sealed).expect("manifest path must derive");
        assert!(manifest.exists());
        verify_sealed_v2_segment(&sealed).expect("resumed seal must verify");
        let mut jobs = Vec::new();
        recovered
            .visit_pending_compression_jobs(|job| jobs.push(job))
            .expect("recovery path must remain anchored");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].source_display_path(), sealed);
        assert_eq!(jobs[0].manifest_display_path(), manifest);
        recovered
            .append(record(2), b"two", 21, CREATED_WALL_NS + 3)
            .expect("recovered successor must accept next sequence");
    }
}

#[test]
fn recovery_rejects_pending_cross_segment_sequence_regression_before_publication() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    let second_active = {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("first append must succeed");
        writer
            .poll_rotation(300_000_000_011, CREATED_WALL_NS + 2)
            .expect("poll must succeed")
            .expect("poll must rotate");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    let first_sealed = fs::read_dir(directory.path())
        .expect("directory must read")
        .map(|entry| entry.expect("entry must read").path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("00000000000000000001-") && name.ends_with(".wal")
                })
        })
        .expect("first sealed segment must exist");
    let first_manifest =
        verify_sealed_v2_segment(&first_sealed).expect("first sealed segment must verify");
    {
        let mut second = Segment::open_v2(&second_active).expect("second active must open");
        second
            .append_record_synced(record(1), b"duplicate")
            .expect("manual fixture append must succeed within the empty segment");
    }
    let second = seal_v2_segment_after(
        &second_active,
        first_manifest.as_predecessor(),
        CREATED_WALL_NS + 3,
    )
    .expect("second fixture segment must seal");
    let second_sealed = second.compression_job().source_display_path();
    let final_manifest = second.compression_job().manifest_display_path();
    let pending =
        pending_manifest_path_for(&second_sealed).expect("pending manifest path must derive");
    fs::rename(&final_manifest, &pending).expect("fixture must simulate unpublished manifest");

    assert!(matches!(
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100),
        Err(ManagerError::Inventory(
            InventoryError::CrossSegmentSequenceRegression
        ))
    ));
    assert!(second_sealed.exists());
    assert!(pending.exists());
    assert!(!final_manifest.exists());
}

#[test]
fn recovery_repairs_only_an_incomplete_final_active_frame_and_resumes() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::default();
    let (active, valid_length) = {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("append must succeed");
        (
            writer
                .active_path()
                .expect("active path must remain anchored")
                .to_owned(),
            writer.active_length(),
        )
    };
    let torn = frame::encode(record(2), b"two").expect("fixture frame must encode");
    OpenOptions::new()
        .append(true)
        .open(&active)
        .expect("active must open for crash fixture")
        .write_all(&torn[..10])
        .expect("partial final frame must write");
    assert!(
        fs::metadata(&active)
            .expect("active metadata must read")
            .len()
            > valid_length
    );

    let mut recovered =
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100)
            .expect("incomplete final frame must recover");
    assert!(recovered.active_length() < valid_length);
    assert_eq!(
        fs::metadata(sealed_path_for(&active).expect("sealed path must derive"))
            .expect("repaired and sealed segment metadata must read")
            .len(),
        valid_length
    );
    assert!(!active.exists());
    assert_eq!(recovered.active_record_count(), 0);
    recovered
        .append(record(2), b"two", 21, CREATED_WALL_NS + 2)
        .expect("recovered writer must accept the missing next record");
}

#[test]
fn recovery_does_not_repair_a_tail_when_the_active_chain_sequence_is_invalid() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    let second_active = {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(10), b"one", 11, CREATED_WALL_NS + 1)
            .expect("first append must succeed");
        writer
            .poll_rotation(300_000_000_011, CREATED_WALL_NS + 2)
            .expect("poll must succeed")
            .expect("poll must rotate");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    {
        let mut second = Segment::open_v2(&second_active).expect("second active must open");
        second
            .append_record_synced(record(5), b"regressed")
            .expect("manual within-segment fixture append must succeed");
    }
    let torn = frame::encode(record(6), b"torn").expect("fixture frame must encode");
    OpenOptions::new()
        .append(true)
        .open(&second_active)
        .expect("second active must open for crash fixture")
        .write_all(&torn[..10])
        .expect("partial final frame must write");
    let bytes_before = fs::read(&second_active).expect("invalid active bytes must read");

    assert!(matches!(
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100),
        Err(ManagerError::Inventory(
            InventoryError::CrossSegmentSequenceRegression
        ))
    ));
    assert_eq!(
        fs::read(&second_active).expect("invalid active bytes must remain"),
        bytes_before
    );
}

#[test]
fn compression_handoff_remains_anchored_after_directory_move_and_replacement() {
    let parent = tempfile::tempdir().expect("temporary parent must exist");
    let original = parent.path().join("wal");
    let moved = parent.path().join("wal-moved");
    fs::create_dir(&original).expect("managed directory must create");
    let policy =
        RotationPolicy::new(segment_limit_for_one(b"one"), 5).expect("policy must be valid");
    let job = {
        let mut writer = SegmentedWalWriter::create(&original, metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("record must append");
        writer
            .poll_rotation(15, CREATED_WALL_NS + 2)
            .expect("rotation must succeed")
            .expect("nonempty segment must rotate")
    };
    let expected_source_length = job.expected_uncompressed_bytes();

    fs::rename(&original, &moved).expect("managed directory must move");
    fs::create_dir(&original).expect("replacement directory must create");
    fs::write(original.join(job.source_name()), b"replacement")
        .expect("replacement source fixture must write");
    fs::write(original.join(job.manifest_name()), b"replacement")
        .expect("replacement manifest fixture must write");

    let mut source = job
        .open_source()
        .expect("capability must still open the moved original source");
    let mut source_bytes = Vec::new();
    source
        .read_to_end(&mut source_bytes)
        .expect("capability-owned source must read");
    assert_eq!(
        u64::try_from(source_bytes.len()).expect("source length must fit"),
        expected_source_length
    );
    assert_ne!(source_bytes, b"replacement");

    let mut manifest = job
        .open_manifest()
        .expect("capability must still open the moved original manifest");
    let mut manifest_bytes = Vec::new();
    manifest
        .read_to_end(&mut manifest_bytes)
        .expect("capability-owned manifest must read");
    assert!(manifest_bytes.starts_with(b"{"));

    let moved_source = moved.join(job.source_name());
    let displaced_source = moved.join("displaced.wal");
    fs::rename(&moved_source, &displaced_source).expect("original source must move aside");
    fs::write(&moved_source, b"same-directory replacement")
        .expect("same-directory replacement must write");
    assert!(matches!(
        job.open_source(),
        Err(raw_wal::seal::SealingError::UnsafeFile)
    ));
}

#[test]
fn rejected_directory_inventory_is_not_mutated_by_lock_acquisition() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let unknown = directory.path().join("operator-note.txt");
    fs::write(&unknown, b"retain").expect("unknown artifact must write");
    let entries_before = fs::read_dir(directory.path())
        .expect("directory must read")
        .map(|entry| entry.expect("entry must read").file_name())
        .collect::<Vec<_>>();

    assert!(matches!(
        SegmentedWalWriter::recover(
            directory.path(),
            RotationPolicy::default(),
            20,
            CREATED_WALL_NS + 100,
        ),
        Err(ManagerError::Inventory(InventoryError::UnknownArtifact))
    ));
    let entries_after = fs::read_dir(directory.path())
        .expect("directory must read")
        .map(|entry| entry.expect("entry must read").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries_after, entries_before);
    assert_eq!(
        fs::read(&unknown).expect("unknown bytes must remain"),
        b"retain"
    );
}

#[test]
fn recovery_removes_only_exact_private_protocol_temporary_files() {
    let successor_directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    let missing_successor = {
        let mut writer =
            SegmentedWalWriter::create(successor_directory.path(), metadata(), policy, 10)
                .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("append must succeed");
        writer
            .poll_rotation(300_000_000_011, CREATED_WALL_NS + 2)
            .expect("poll must succeed")
            .expect("poll must rotate");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    fs::remove_file(&missing_successor).expect("fixture must remove successor");
    let successor_temp = missing_successor.with_file_name(format!(
        ".{}.create-123-0.tmp",
        missing_successor
            .file_name()
            .and_then(|name| name.to_str())
            .expect("successor filename must be UTF-8")
    ));
    fs::write(&successor_temp, b"unpublished").expect("successor temp must write");
    fs::set_permissions(&successor_temp, fs::Permissions::from_mode(0o600))
        .expect("successor temp must be private");
    let recovered = SegmentedWalWriter::recover(
        successor_directory.path(),
        policy,
        20,
        CREATED_WALL_NS + 100,
    )
    .expect("successor temp state must recover");
    assert_eq!(
        recovered
            .active_path()
            .expect("active path must remain anchored"),
        missing_successor
    );
    assert!(!successor_temp.exists());
    drop(recovered);

    let pending_directory = tempfile::tempdir().expect("temporary directory must exist");
    let active = {
        let mut writer =
            SegmentedWalWriter::create(pending_directory.path(), metadata(), policy, 10)
                .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("append must succeed");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    let sealed = sealed_path_for(&active).expect("sealed path must derive");
    let pending = pending_manifest_path_for(&sealed).expect("pending path must derive");
    let pending_temp = pending.with_file_name(format!(
        ".{}.create-123-0.tmp",
        pending
            .file_name()
            .and_then(|name| name.to_str())
            .expect("pending filename must be UTF-8")
    ));
    fs::write(&pending_temp, b"unpublished").expect("pending temp must write");
    fs::set_permissions(&pending_temp, fs::Permissions::from_mode(0o600))
        .expect("pending temp must be private");
    let recovered =
        SegmentedWalWriter::recover(pending_directory.path(), policy, 20, CREATED_WALL_NS + 100)
            .expect("pending temp state must recover");
    assert_eq!(recovered.active_ordinal(), 2);
    assert_ne!(
        recovered
            .active_path()
            .expect("recovered successor must remain anchored"),
        active
    );
    assert!(!active.exists());
    assert!(!pending_temp.exists());
}

#[test]
fn recovery_rejects_multiple_active_segments() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::default();
    {
        let _writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
    }
    let extra_metadata = SegmentMetadata::new(
        [0x41; 16],
        CREATED_WALL_NS,
        "market-schema-v2",
        "installation-test",
        "build-test",
        vec![
            StreamDescriptor::new(7, "binance", "spot-btcusdt").expect("descriptor must be valid"),
        ],
    )
    .expect("metadata must be valid");
    let extra_path = directory.path().join(format!(
        "{:020}-{}.active.wal",
        2,
        hex::encode(extra_metadata.segment_id())
    ));
    drop(Segment::create_v2(&extra_path, extra_metadata).expect("extra segment must create"));

    assert!(matches!(
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100),
        Err(ManagerError::Inventory(
            InventoryError::MultipleActiveSegments
        ))
    ));
}

#[test]
fn manager_created_lifecycle_files_are_owner_only() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("append must succeed");
        writer
            .poll_rotation(300_000_000_011, CREATED_WALL_NS + 2)
            .expect("poll must succeed")
            .expect("poll must rotate");
    }

    for entry in fs::read_dir(directory.path()).expect("directory must read") {
        let path = entry.expect("entry must read").path();
        let mode = fs::metadata(&path)
            .expect("managed artifact metadata must read")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "managed artifact must not grant group or other access: {}",
            path.display()
        );
    }
}

#[test]
fn recovery_rejects_a_deterministic_active_id_with_changed_segment_metadata() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let policy = RotationPolicy::new(segment_limit_for_one(b"one"), 300_000_000_000)
        .expect("policy must be valid");
    let active = {
        let mut writer = SegmentedWalWriter::create(directory.path(), metadata(), policy, 10)
            .expect("writer must create");
        writer
            .append(record(1), b"one", 11, CREATED_WALL_NS + 1)
            .expect("append must succeed");
        writer
            .poll_rotation(300_000_000_011, CREATED_WALL_NS + 2)
            .expect("poll must succeed")
            .expect("poll must rotate");
        writer
            .active_path()
            .expect("active path must remain anchored")
            .to_owned()
    };
    let active_name = active
        .file_name()
        .and_then(|name| name.to_str())
        .expect("active filename must be UTF-8");
    let encoded_id = active_name
        .strip_suffix(".active.wal")
        .and_then(|base| base.split_once('-').map(|(_, id)| id))
        .expect("active identity must parse");
    let segment_id: [u8; 16] = hex::decode(encoded_id)
        .expect("active segment ID must decode")
        .try_into()
        .expect("active segment ID must be 16 bytes");
    fs::remove_file(&active).expect("fixture must replace active segment");
    let changed = SegmentMetadata::new(
        segment_id,
        CREATED_WALL_NS + 2,
        "changed-schema",
        "installation-test",
        "build-test",
        vec![
            StreamDescriptor::new(7, "binance", "spot-btcusdt").expect("descriptor must be valid"),
        ],
    )
    .expect("changed metadata must be structurally valid");
    drop(Segment::create_v2(&active, changed).expect("replacement active must create"));

    assert!(matches!(
        SegmentedWalWriter::recover(directory.path(), policy, 20, CREATED_WALL_NS + 100),
        Err(ManagerError::Inventory(
            InventoryError::SegmentMetadataMismatch { ordinal: 2 }
        ))
    ));
}

#[test]
fn recovery_rejects_a_predecessor_bound_but_nondeterministic_segment_id() {
    let directory = tempfile::tempdir().expect("temporary directory must exist");
    let first_active =
        directory
            .path()
            .join(format!("{:020}-{}.active.wal", 1, hex::encode([0x31; 16])));
    {
        let mut first =
            Segment::create_v2(&first_active, metadata()).expect("first segment must create");
        first
            .append_record_synced(record(1), b"one")
            .expect("first record must append");
    }
    let first =
        seal_v2_segment(&first_active, CREATED_WALL_NS + 2).expect("first segment must seal");
    let arbitrary_id = [0x41; 16];
    let second_active = directory.path().join(format!(
        "{:020}-{}.active.wal",
        2,
        hex::encode(arbitrary_id)
    ));
    let second_metadata = SegmentMetadata::new(
        arbitrary_id,
        CREATED_WALL_NS + 2,
        "market-schema-v2",
        "installation-test",
        "build-test",
        vec![
            StreamDescriptor::new(7, "binance", "spot-btcusdt").expect("descriptor must be valid"),
        ],
    )
    .expect("second metadata must be valid");
    {
        let mut second = Segment::create_v2(&second_active, second_metadata)
            .expect("second segment must create");
        second
            .append_record_synced(record(2), b"two")
            .expect("second record must append");
    }
    seal_v2_segment_after(
        &second_active,
        first.manifest().as_predecessor(),
        CREATED_WALL_NS + 3,
    )
    .expect("predecessor-bound second segment must seal");

    assert!(matches!(
        SegmentedWalWriter::recover(
            directory.path(),
            RotationPolicy::default(),
            20,
            CREATED_WALL_NS + 100,
        ),
        Err(ManagerError::Inventory(
            InventoryError::SegmentIdentityMismatch { ordinal: 2 }
        ))
    ));
}
