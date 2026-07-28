use std::{
    fs::{self, File},
    num::NonZeroU32,
    sync::Arc,
};

use arrow::{
    array::{Int64Array, StringArray, UInt32Array, UInt64Array},
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
#[cfg(feature = "test-support")]
use parquet::{arrow::ArrowWriter, basic::ZstdLevel, file::properties::WriterProperties};
use parquet::{
    basic::Compression,
    file::reader::{FileReader, SerializedFileReader},
};
#[cfg(feature = "test-support")]
use parquet_store::FaultPoint;
use parquet_store::{
    BatchId, CorrectionLineage, DatasetMetadata, DatasetMetadataInput, DatasetPolicy,
    DatasetPolicyInput, DatasetReader, DatasetWriter, Manifest, PartitionKey, PartitionKeyInput,
    SourceCoverage, StoreError,
};
use time::{Date, Month};

fn private_root() -> tempfile::TempDir {
    let temporary_parent = std::env::temp_dir()
        .canonicalize()
        .expect("canonical temporary parent");
    let root = tempfile::Builder::new()
        .prefix("cmti-parquet-store-")
        .tempdir_in(temporary_parent)
        .expect("temporary dataset root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))
            .expect("private dataset root");
    }
    root
}

fn policy() -> DatasetPolicy {
    DatasetPolicy::try_new(policy_input()).expect("valid explicit policy")
}

fn policy_input() -> DatasetPolicyInput {
    DatasetPolicyInput {
        maximum_batch_rows: 1_024,
        maximum_batch_columns: 64,
        maximum_batch_bytes: 16 * 1024 * 1024,
        maximum_value_bytes: 64 * 1024,
        maximum_schema_bytes: 256 * 1024,
        maximum_schema_fields: 128,
        maximum_schema_depth: 32,
        maximum_writer_memory_bytes: 64 * 1024 * 1024,
        maximum_row_group_rows: 1_000_000,
        target_row_group_bytes: 64 * 1024 * 1024,
        maximum_file_bytes: 64 * 1024 * 1024,
        maximum_partition_bytes: 64 * 1024 * 1024,
        maximum_partition_rows: 1_000_000,
        maximum_active_fragments: 64,
        maximum_footer_bytes: 1024 * 1024,
        maximum_sealed_files: 64,
        reader_batch_rows: 1_024,
        maximum_correction_depth: 64,
    }
}

fn key(dataset_version: u32) -> PartitionKey {
    PartitionKey::try_new(PartitionKeyInput {
        event_type: "book-delta".to_owned(),
        dataset_version: NonZeroU32::new(dataset_version).expect("dataset version"),
        schema_version: NonZeroU32::new(1).expect("schema version"),
        venue: "binance".to_owned(),
        instrument_generation: NonZeroU32::new(1).expect("generation"),
        utc_date: Date::from_calendar_date(1970, Month::January, 1).expect("fixture date"),
        hour: 0,
    })
    .expect("valid partition key")
}

fn coverage(first_sequence: u64, last_sequence: u64) -> SourceCoverage {
    SourceCoverage::try_new(
        "binance",
        "book-btcusdt",
        1,
        1,
        first_sequence,
        last_sequence,
    )
    .expect("source coverage")
}

fn metadata(
    source_coverage: Vec<SourceCoverage>,
    correction_lineage: Option<CorrectionLineage>,
) -> DatasetMetadata {
    DatasetMetadata::try_new(DatasetMetadataInput {
        feature_version: "normalized-event-v1".to_owned(),
        parser_version: "binance-parser-v1".to_owned(),
        normalizer_version: "cmti-normalizer-v1".to_owned(),
        code_identity: "git_sha1:a6d85071bdba0e76bf1474eac7137c770971ecd4".to_owned(),
        source_coverage,
        correction_lineage,
    })
    .expect("valid dataset metadata")
}

fn batch_id(source_coverage: SourceCoverage, marker: char) -> BatchId {
    BatchId::try_new(source_coverage, marker.to_string().repeat(64), 128, 4_096)
        .expect("batch identity")
}

#[cfg(feature = "test-support")]
fn has_temporary_entry(directory: &std::path::Path) -> bool {
    fs::read_dir(directory)
        .expect("read managed directory")
        .map(|entry| entry.expect("read managed entry").file_name())
        .any(|name| {
            let value = name.to_string_lossy();
            value.starts_with('.') && value.ends_with(".tmp")
        })
}

fn batch(times: &[i64], first_sequence: u64) -> RecordBatch {
    batch_with_dimensions(
        times,
        &times.iter().copied().map(Some).collect::<Vec<_>>(),
        &times.iter().map(|time| time + 10).collect::<Vec<_>>(),
        first_sequence,
        "book-delta",
        "binance",
        1,
    )
}

fn batch_with_dimensions(
    effective_times: &[i64],
    exchange_times: &[Option<i64>],
    receive_times: &[i64],
    first_sequence: u64,
    event_type: &str,
    venue: &str,
    generation: u32,
) -> RecordBatch {
    assert_eq!(effective_times.len(), exchange_times.len());
    assert_eq!(effective_times.len(), receive_times.len());
    let schema = Arc::new(Schema::new(vec![
        Field::new("effective_event_time_ns", DataType::Int64, false),
        Field::new("exchange_event_time_ns", DataType::Int64, true),
        Field::new("receive_wall_time_ns", DataType::Int64, false),
        Field::new("event_type", DataType::Utf8, false),
        Field::new("venue", DataType::Utf8, false),
        Field::new("instrument_generation", DataType::UInt32, false),
        Field::new("connection_epoch", DataType::UInt64, false),
        Field::new("subscription_epoch", DataType::UInt64, false),
        Field::new("wal_record_sequence", DataType::UInt64, false),
        Field::new("symbol", DataType::Utf8, false),
    ]));
    let row_count = effective_times.len();
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(effective_times.to_vec())),
            Arc::new(Int64Array::from(exchange_times.to_vec())),
            Arc::new(Int64Array::from(receive_times.to_vec())),
            Arc::new(StringArray::from_iter_values(std::iter::repeat_n(
                event_type, row_count,
            ))),
            Arc::new(StringArray::from_iter_values(std::iter::repeat_n(
                venue, row_count,
            ))),
            Arc::new(UInt32Array::from_iter_values(std::iter::repeat_n(
                generation, row_count,
            ))),
            Arc::new(UInt64Array::from_iter_values(std::iter::repeat_n(
                1, row_count,
            ))),
            Arc::new(UInt64Array::from_iter_values(std::iter::repeat_n(
                1, row_count,
            ))),
            Arc::new(UInt64Array::from_iter_values(
                first_sequence..first_sequence + u64::try_from(row_count).expect("row count"),
            )),
            Arc::new(StringArray::from_iter_values(std::iter::repeat_n(
                "BTCUSDT", row_count,
            ))),
        ],
    )
    .expect("fixture record batch")
}

#[test]
fn sealed_partition_is_compacted_real_zstd_parquet_and_immutable() {
    let root = private_root();
    let key = key(1);
    let first_coverage = coverage(100, 101);
    let second_coverage = coverage(102, 103);
    let first_metadata = metadata(vec![first_coverage.clone()], None);
    let second_metadata = metadata(vec![second_coverage.clone()], None);
    let first_id = batch_id(first_coverage, '1');
    let second_id = batch_id(second_coverage, '2');
    let mut writer = DatasetWriter::open(root.path(), policy()).expect("open dataset writer");

    let first_receipt = writer
        .write_batch(&key, &first_metadata, &first_id, &batch(&[100, 200], 100))
        .expect("write first active fragment");
    let retry_receipt = writer
        .write_batch(&key, &first_metadata, &first_id, &batch(&[100, 200], 100))
        .expect("lost acknowledgement retry");
    assert_eq!(retry_receipt, first_receipt);
    assert!(matches!(
        writer.write_batch(&key, &first_metadata, &first_id, &batch(&[100, 250], 100)),
        Err(StoreError::BatchIdentityConflict)
    ));
    writer
        .write_batch(&key, &second_metadata, &second_id, &batch(&[300, 400], 102))
        .expect("write second active fragment");

    let manifest = writer.seal(&key).expect("seal partition");
    let verified = DatasetReader::open(root.path(), policy())
        .expect("open reader")
        .verify_partition(&key)
        .expect("verify installed publication");
    assert_eq!(verified.row_count(), 4);
    assert_eq!(verified.minimum_event_time_ns(), 100);
    assert_eq!(verified.maximum_event_time_ns(), 400);
    assert_eq!(verified.file_count(), 1);
    assert_eq!(manifest.files().len(), 1);
    assert_eq!(manifest.source_fragments().len(), 2);
    assert_ne!(manifest.source_fragment_set_digest(), &[0; 32]);
    let mut source_starts = manifest
        .source_fragments()
        .iter()
        .map(|fragment| fragment.batch_id().coverage().first_sequence())
        .collect::<Vec<_>>();
    source_starts.sort_unstable();
    assert_eq!(source_starts, vec![100, 102]);

    let parquet_path = root.path().join(manifest.files()[0].relative_path());
    let reader = SerializedFileReader::new(File::open(parquet_path).expect("open sealed parquet"))
        .expect("read sealed parquet");
    assert_eq!(reader.metadata().num_row_groups(), 1);
    assert!(
        reader
            .metadata()
            .row_group(0)
            .columns()
            .iter()
            .all(|column| column.compression() == Compression::ZSTD(Default::default()))
    );
    let footer_metadata = reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .expect("required footer metadata");
    for required in [
        "cmti.schema_version",
        "cmti.feature_version",
        "cmti.code_identity",
        "cmti.source_coverage",
        "cmti.minimum_event_time_ns",
        "cmti.maximum_event_time_ns",
    ] {
        assert!(
            footer_metadata
                .iter()
                .any(|entry| entry.key == required && entry.value.is_some()),
            "missing {required}"
        );
    }

    assert!(matches!(
        writer.write_batch(
            &key,
            &metadata(vec![coverage(104, 104)], None),
            &batch_id(coverage(104, 104), '3'),
            &batch(&[500], 104)
        ),
        Err(StoreError::PartitionSealed)
    ));
    let retried = writer.seal(&key).expect("idempotent seal retry");
    assert_eq!(retried, manifest);
}

#[test]
fn active_fragments_survive_restart_and_corrections_require_verified_lineage() {
    let root = private_root();
    let original_key = key(1);
    let original_coverage = coverage(100, 101);
    {
        let mut writer = DatasetWriter::open(root.path(), policy()).expect("open original writer");
        writer
            .write_batch(
                &original_key,
                &metadata(vec![original_coverage.clone()], None),
                &batch_id(original_coverage, '1'),
                &batch(&[100, 200], 100),
            )
            .expect("durable active fragment");
    }

    let original = {
        let mut writer =
            DatasetWriter::open(root.path(), policy()).expect("reopen original writer");
        writer.seal(&original_key).expect("seal after restart")
    };
    let original_hash = hex::encode(original.manifest_hash());
    let regressive_lineage = CorrectionLineage::try_new(
        NonZeroU32::new(1).expect("base version"),
        original_hash.clone(),
        99,
        "regressive correction timestamp",
        "operator-reviewed source replay",
    )
    .expect("structurally valid correction lineage");
    {
        let regressive_coverage = coverage(100, 101);
        let mut writer =
            DatasetWriter::open(root.path(), policy()).expect("open regressive correction writer");
        assert!(matches!(
            writer.write_batch(
                &key(2),
                &metadata(vec![regressive_coverage.clone()], Some(regressive_lineage)),
                &batch_id(regressive_coverage, '3'),
                &batch(&[100, 250], 100),
            ),
            Err(StoreError::InvalidCorrectionLineage)
        ));
    }
    let skipped_lineage = CorrectionLineage::try_new(
        NonZeroU32::new(1).expect("base version"),
        original_hash.clone(),
        1_000,
        "skip an intermediate dataset version",
        "operator-reviewed source replay",
    )
    .expect("structurally valid skipped lineage");
    {
        let skipped_coverage = coverage(100, 101);
        let mut writer =
            DatasetWriter::open(root.path(), policy()).expect("open skipped correction writer");
        assert!(matches!(
            writer.write_batch(
                &key(3),
                &metadata(vec![skipped_coverage.clone()], Some(skipped_lineage)),
                &batch_id(skipped_coverage, '4'),
                &batch(&[100, 250], 100),
            ),
            Err(StoreError::InvalidCorrectionLineage)
        ));
    }
    let correction_lineage = CorrectionLineage::try_new(
        NonZeroU32::new(1).expect("base version"),
        original_hash,
        1_000,
        "replace one mis-normalized event",
        "operator-reviewed source replay",
    )
    .expect("correction lineage");
    let correction_key = key(2);
    let correction_coverage = coverage(100, 101);
    let correction = {
        let mut writer =
            DatasetWriter::open(root.path(), policy()).expect("open correction writer");
        writer
            .write_batch(
                &correction_key,
                &metadata(vec![correction_coverage.clone()], Some(correction_lineage)),
                &batch_id(correction_coverage, '5'),
                &batch(&[100, 250], 100),
            )
            .expect("write correction version");
        writer.seal(&correction_key).expect("seal correction")
    };

    assert_ne!(correction.manifest_hash(), original.manifest_hash());
    let reader = DatasetReader::open(root.path(), policy()).expect("open publication reader");
    assert_eq!(
        reader
            .verify_partition(&original_key)
            .expect("original remains verifiable")
            .row_count(),
        2
    );
    assert_eq!(
        reader
            .verify_partition(&correction_key)
            .expect("correction verifies")
            .maximum_event_time_ns(),
        250
    );
}

#[test]
fn reader_verifies_the_complete_correction_chain() {
    fn seal_version(
        root: &std::path::Path,
        version: u32,
        base: Option<&Manifest>,
        maximum_event_time_ns: i64,
    ) -> Manifest {
        let lineage = base.map(|base| {
            CorrectionLineage::try_new(
                base.partition().dataset_version(),
                hex::encode(base.manifest_hash()),
                i64::from(version) * 1_000,
                format!("publish correction version {version}"),
                "operator-reviewed source replay",
            )
            .expect("valid correction chain link")
        });
        let source_coverage = coverage(100, 101);
        let marker = char::from_digit(version % 10, 10).expect("decimal version marker");
        let mut writer = DatasetWriter::open(root, policy()).expect("open version writer");
        writer
            .write_batch(
                &key(version),
                &metadata(vec![source_coverage.clone()], lineage),
                &batch_id(source_coverage, marker),
                &batch(&[100, maximum_event_time_ns], 100),
            )
            .expect("write version");
        writer.seal(&key(version)).expect("seal version")
    }

    let missing_root = private_root();
    let missing_v1 = seal_version(missing_root.path(), 1, None, 200);
    let missing_v2 = seal_version(missing_root.path(), 2, Some(&missing_v1), 250);
    assert_eq!(missing_v2.partition().dataset_version().get(), 2);
    fs::remove_file(
        missing_root
            .path()
            .join(key(1).relative_path())
            .join("manifest.json"),
    )
    .expect("remove required base manifest");
    assert!(matches!(
        DatasetReader::open(missing_root.path(), policy())
            .expect("open missing-base reader")
            .verify_partition(&key(2)),
        Err(StoreError::InvalidCorrectionLineage)
    ));

    let wrong_root = private_root();
    let wrong_v1 = seal_version(wrong_root.path(), 1, None, 200);
    let _wrong_v2 = seal_version(wrong_root.path(), 2, Some(&wrong_v1), 250);
    let alternate_root = private_root();
    let alternate_v1 = seal_version(alternate_root.path(), 1, None, 400);
    assert_ne!(alternate_v1.manifest_hash(), wrong_v1.manifest_hash());
    for relative in [
        key(1).relative_path().join("sealed").join("data.parquet"),
        key(1).relative_path().join("manifest.json"),
    ] {
        fs::copy(
            alternate_root.path().join(&relative),
            wrong_root.path().join(&relative),
        )
        .expect("replace base with another internally valid publication");
    }
    assert!(matches!(
        DatasetReader::open(wrong_root.path(), policy())
            .expect("open wrong-base reader")
            .verify_partition(&key(2)),
        Err(StoreError::InvalidCorrectionLineage)
    ));

    let chain_root = private_root();
    let chain_v1 = seal_version(chain_root.path(), 1, None, 200);
    let chain_v2 = seal_version(chain_root.path(), 2, Some(&chain_v1), 250);
    let _chain_v3 = seal_version(chain_root.path(), 3, Some(&chain_v2), 300);
    DatasetReader::open(chain_root.path(), policy())
        .expect("open complete-chain reader")
        .verify_partition(&key(3))
        .expect("verify complete three-version chain");
    let mut shallow_policy_input = policy_input();
    shallow_policy_input.maximum_correction_depth = 1;
    assert!(matches!(
        DatasetReader::open(
            chain_root.path(),
            DatasetPolicy::try_new(shallow_policy_input).expect("shallow correction policy"),
        )
        .expect("open shallow-chain reader")
        .verify_partition(&key(3)),
        Err(StoreError::InvalidCorrectionLineage)
    ));
    fs::remove_file(
        chain_root
            .path()
            .join(key(1).relative_path())
            .join("manifest.json"),
    )
    .expect("remove indirect base manifest");
    assert!(matches!(
        DatasetReader::open(chain_root.path(), policy())
            .expect("open broken-chain reader")
            .verify_partition(&key(3)),
        Err(StoreError::InvalidCorrectionLineage)
    ));
}

#[test]
fn invalid_layout_effective_time_dimensions_and_tampering_fail_closed() {
    let root = private_root();
    assert!(matches!(
        PartitionKey::try_new(PartitionKeyInput {
            event_type: "../escape".to_owned(),
            dataset_version: NonZeroU32::new(1).expect("version"),
            schema_version: NonZeroU32::new(1).expect("schema"),
            venue: "binance".to_owned(),
            instrument_generation: NonZeroU32::new(1).expect("generation"),
            utc_date: Date::from_calendar_date(2026, Month::July, 28).expect("date"),
            hour: 17,
        }),
        Err(StoreError::InvalidPartitionKey)
    ));

    let mut writer = DatasetWriter::open(root.path(), policy()).expect("open dataset writer");
    let key = key(1);
    let source_coverage = coverage(100, 101);
    let metadata = metadata(vec![source_coverage.clone()], None);
    let id = batch_id(source_coverage, '1');
    let invalid_schema = Arc::new(Schema::new(vec![Field::new(
        "effective_event_time_ns",
        DataType::Int64,
        true,
    )]));
    let invalid_batch = RecordBatch::try_new(
        invalid_schema,
        vec![Arc::new(Int64Array::from(vec![Some(100), None]))],
    )
    .expect("invalid fixture batch");
    assert!(matches!(
        writer.write_batch(&key, &metadata, &id, &invalid_batch),
        Err(StoreError::InvalidEventTimeColumn | StoreError::SchemaMismatch)
    ));

    writer
        .write_batch(&key, &metadata, &id, &batch(&[100, 200], 100))
        .expect("write valid fragment");
    let manifest = writer.seal(&key).expect("seal partition");
    drop(writer);
    let file = root.path().join(manifest.files()[0].relative_path());
    let mut bytes = fs::read(&file).expect("read sealed file");
    bytes[4] ^= 0xff;
    fs::write(&file, bytes).expect("tamper sealed file");
    assert!(matches!(
        DatasetReader::open(root.path(), policy())
            .expect("open reader")
            .verify_partition(&key),
        Err(StoreError::Integrity | StoreError::Parquet(_))
    ));
}

#[cfg(unix)]
#[test]
fn root_and_partition_paths_must_be_private_real_directories() {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};

    let container = private_root();
    let permissive = container.path().join("permissive");
    fs::create_dir(&permissive).expect("permissive root");
    fs::set_permissions(&permissive, fs::Permissions::from_mode(0o755)).expect("permissive mode");
    assert!(matches!(
        DatasetWriter::open(&permissive, policy()),
        Err(StoreError::UnsafeRoot)
    ));

    let real_parent = container.path().join("real-parent");
    fs::create_dir(&real_parent).expect("real parent");
    let real_root = real_parent.join("root");
    fs::create_dir(&real_root).expect("real nested root");
    fs::set_permissions(&real_root, fs::Permissions::from_mode(0o700))
        .expect("private nested root mode");
    let parent_alias = container.path().join("parent-alias");
    symlink(&real_parent, &parent_alias).expect("root ancestor symlink");
    assert!(matches!(
        DatasetWriter::open(&parent_alias.join("root"), policy()),
        Err(StoreError::UnsafeRoot)
    ));

    let private = container.path().join("private");
    fs::create_dir(&private).expect("private root");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("private mode");
    let target = container.path().join("target");
    fs::create_dir(&target).expect("target");
    let link = private.join("normalized");
    symlink(&target, &link).expect("partition symlink");
    let mut writer = DatasetWriter::open(&private, policy()).expect("open private root");
    let source_coverage = coverage(100, 100);
    assert!(matches!(
        writer.write_batch(
            &key(1),
            &metadata(vec![source_coverage.clone()], None),
            &batch_id(source_coverage, '1'),
            &batch(&[100], 100)
        ),
        Err(StoreError::UnsafePath | StoreError::Io(_))
    ));
}

#[test]
fn one_writer_owns_the_root_and_policy_bounds_are_explicit() {
    let root = private_root();
    let _first = DatasetWriter::open(root.path(), policy()).expect("first writer");
    assert!(matches!(
        DatasetWriter::open(root.path(), policy()),
        Err(StoreError::AlreadyOpen)
    ));
    assert!(matches!(
        DatasetPolicy::try_new(DatasetPolicyInput {
            maximum_batch_rows: 0,
            maximum_batch_columns: 1,
            maximum_batch_bytes: 1,
            maximum_value_bytes: 1,
            maximum_schema_bytes: 1,
            maximum_schema_fields: 1,
            maximum_schema_depth: 1,
            maximum_writer_memory_bytes: 1,
            maximum_row_group_rows: 1,
            target_row_group_bytes: 1,
            maximum_file_bytes: 1,
            maximum_partition_bytes: 1,
            maximum_partition_rows: 1,
            maximum_active_fragments: 1,
            maximum_footer_bytes: 1,
            maximum_sealed_files: 1,
            reader_batch_rows: 1,
            maximum_correction_depth: 1,
        }),
        Err(StoreError::InvalidPolicy)
    ));
}

#[test]
fn every_acknowledged_active_set_remains_within_sealable_limits() {
    let row_root = private_root();
    let mut row_policy_input = policy_input();
    row_policy_input.maximum_partition_rows = 1;
    let row_policy = DatasetPolicy::try_new(row_policy_input).expect("one-row partition policy");
    let mut row_writer =
        DatasetWriter::open(row_root.path(), row_policy.clone()).expect("open bounded writer");
    let first_coverage = coverage(100, 100);
    row_writer
        .write_batch(
            &key(1),
            &metadata(vec![first_coverage.clone()], None),
            &batch_id(first_coverage, '1'),
            &batch(&[100], 100),
        )
        .expect("acknowledge one admissible row");
    let second_coverage = coverage(101, 101);
    assert!(matches!(
        row_writer.write_batch(
            &key(1),
            &metadata(vec![second_coverage.clone()], None),
            &batch_id(second_coverage, '2'),
            &batch(&[200], 101),
        ),
        Err(StoreError::CapacityExceeded)
    ));
    assert_eq!(
        row_writer
            .seal(&key(1))
            .expect("admitted row remains sealable")
            .row_count(),
        1
    );
    assert_eq!(
        DatasetReader::open(row_root.path(), row_policy)
            .expect("open bounded reader")
            .verify_partition(&key(1))
            .expect("verify bounded partition")
            .row_count(),
        1
    );

    let coverage_root = private_root();
    let mut coverage_writer =
        DatasetWriter::open(coverage_root.path(), policy()).expect("open coverage-bound writer");
    for index in 0_u64..64 {
        let source_coverage = coverage(1_000 + index, 1_000 + index);
        let marker = char::from_digit(u32::try_from(index % 10).expect("decimal marker"), 10)
            .expect("decimal marker");
        coverage_writer
            .write_batch(
                &key(1),
                &metadata(vec![source_coverage.clone()], None),
                &batch_id(source_coverage, marker),
                &batch(
                    &[100 + i64::try_from(index).expect("small event time")],
                    1_000 + index,
                ),
            )
            .expect("acknowledge bounded coverage");
    }
    let overflow_coverage = coverage(2_000, 2_000);
    assert!(matches!(
        coverage_writer.write_batch(
            &key(1),
            &metadata(vec![overflow_coverage.clone()], None),
            &batch_id(overflow_coverage, 'f'),
            &batch(&[500], 2_000),
        ),
        Err(StoreError::FragmentCapacityExceeded)
    ));
    assert_eq!(
        coverage_writer
            .seal(&key(1))
            .expect("maximum admitted coverage remains sealable")
            .row_count(),
        64
    );
}

#[cfg(feature = "test-support")]
#[test]
fn near_capacity_compaction_falls_back_to_exact_bounded_fragments() {
    let first_coverage = coverage(100, 100);
    let second_coverage = coverage(101, 101);
    let first_metadata = metadata(vec![first_coverage.clone()], None);
    let second_metadata = metadata(vec![second_coverage.clone()], None);
    let first_batch_id = batch_id(first_coverage, '1');
    let second_batch_id = batch_id(second_coverage, '2');
    let first_batch = batch(&[100], 100);
    let second_batch = batch(&[200], 101);

    let calibration_root = private_root();
    let mut calibration =
        DatasetWriter::open(calibration_root.path(), policy()).expect("open calibration writer");
    let first_size = calibration
        .write_batch(&key(1), &first_metadata, &first_batch_id, &first_batch)
        .expect("write first calibration fragment")
        .size_bytes();
    let second_size = calibration
        .write_batch(&key(1), &second_metadata, &second_batch_id, &second_batch)
        .expect("write second calibration fragment")
        .size_bytes();
    let exact_capacity = first_size
        .checked_add(second_size)
        .expect("bounded fixture sizes");
    drop(calibration);

    let boundary_root = private_root();
    let mut boundary_policy_input = policy_input();
    boundary_policy_input.maximum_file_bytes = exact_capacity;
    boundary_policy_input.maximum_partition_bytes = exact_capacity;
    boundary_policy_input.maximum_footer_bytes = boundary_policy_input
        .maximum_footer_bytes
        .min(usize::try_from(exact_capacity).expect("small fixture capacity"));
    let boundary_policy =
        DatasetPolicy::try_new(boundary_policy_input).expect("exact byte-boundary policy");
    let mut writer = DatasetWriter::open(boundary_root.path(), boundary_policy.clone())
        .expect("open boundary writer");
    let first_receipt = writer
        .write_batch(&key(1), &first_metadata, &first_batch_id, &first_batch)
        .expect("acknowledge first boundary fragment");
    let second_receipt = writer
        .write_batch(&key(1), &second_metadata, &second_batch_id, &second_batch)
        .expect("acknowledge exact-capacity fragment");
    assert_eq!(
        first_receipt.size_bytes() + second_receipt.size_bytes(),
        exact_capacity
    );
    assert_eq!(
        writer
            .write_batch(&key(1), &second_metadata, &second_batch_id, &second_batch,)
            .expect("exact retry remains admissible at the full byte cap"),
        second_receipt
    );

    writer.inject_fault_once(FaultPoint::CompactedFileExceedsCapacity);
    let manifest = writer
        .seal(&key(1))
        .expect("fallback preserves sealability at the exact byte cap");
    assert_eq!(manifest.files().len(), 2);
    assert_eq!(
        manifest
            .files()
            .iter()
            .map(|file| file.size_bytes())
            .sum::<u64>(),
        exact_capacity
    );
    let mut expected_hashes = vec![
        hex::encode(first_receipt.blake3()),
        hex::encode(second_receipt.blake3()),
    ];
    expected_hashes.sort();
    let mut sealed_hashes = manifest
        .files()
        .iter()
        .map(|file| hex::encode(file.blake3()))
        .collect::<Vec<_>>();
    sealed_hashes.sort();
    assert_eq!(sealed_hashes, expected_hashes);
    assert_eq!(
        DatasetReader::open(boundary_root.path(), boundary_policy)
            .expect("open boundary reader")
            .verify_partition(&key(1))
            .expect("verify exact fallback files")
            .row_count(),
        2
    );

    let mut byte_restricted_input = policy_input();
    byte_restricted_input.maximum_file_bytes = exact_capacity - 1;
    byte_restricted_input.maximum_partition_bytes = exact_capacity - 1;
    byte_restricted_input.maximum_footer_bytes = byte_restricted_input
        .maximum_footer_bytes
        .min(usize::try_from(exact_capacity - 1).expect("small fixture capacity"));
    assert!(matches!(
        DatasetReader::open(
            boundary_root.path(),
            DatasetPolicy::try_new(byte_restricted_input).expect("stricter byte policy"),
        )
        .expect("open byte-restricted reader")
        .verify_partition(&key(1)),
        Err(StoreError::CapacityExceeded)
    ));

    let mut row_restricted_input = policy_input();
    row_restricted_input.maximum_file_bytes = exact_capacity;
    row_restricted_input.maximum_partition_bytes = exact_capacity;
    row_restricted_input.maximum_partition_rows = 1;
    row_restricted_input.maximum_footer_bytes = row_restricted_input
        .maximum_footer_bytes
        .min(usize::try_from(exact_capacity).expect("small fixture capacity"));
    assert!(matches!(
        DatasetReader::open(
            boundary_root.path(),
            DatasetPolicy::try_new(row_restricted_input).expect("stricter row policy"),
        )
        .expect("open row-restricted reader")
        .verify_partition(&key(1)),
        Err(StoreError::CapacityExceeded)
    ));
}

#[cfg(feature = "test-support")]
#[test]
fn fallback_replays_full_semantics_before_manifest_publication() {
    let root = private_root();
    let source_coverage = coverage(100, 100);
    let metadata = metadata(vec![source_coverage.clone()], None);
    let batch_id = batch_id(source_coverage, '1');
    let mut writer = DatasetWriter::open(root.path(), policy()).expect("open fallback writer");
    let receipt = writer
        .write_batch(&key(1), &metadata, &batch_id, &batch(&[100], 100))
        .expect("write valid active fragment");
    let active_path = root.path().join(receipt.relative_path());
    let original =
        SerializedFileReader::new(File::open(&active_path).expect("open original active fragment"))
            .expect("read original Parquet metadata");
    let footer = original
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .cloned()
        .expect("required active footer");
    drop(original);

    let semantically_invalid =
        batch_with_dimensions(&[100], &[Some(99)], &[110], 100, "book-delta", "binance", 1);
    let properties = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(1).expect("valid Zstandard level"),
        ))
        .set_key_value_metadata(Some(footer))
        .build();
    let mut parquet_writer = ArrowWriter::try_new(
        File::create(&active_path).expect("replace active bytes"),
        semantically_invalid.schema(),
        Some(properties),
    )
    .expect("create readable invalid Parquet");
    parquet_writer
        .write(&semantically_invalid)
        .expect("write readable invalid batch");
    parquet_writer.close().expect("close invalid Parquet");

    writer.inject_fault_once(FaultPoint::CompactedFileExceedsCapacity);
    assert!(matches!(
        writer.seal(&key(1)),
        Err(StoreError::InvalidEventTimeColumn)
    ));
    assert!(
        !root
            .path()
            .join(key(1).relative_path())
            .join("manifest.json")
            .exists(),
        "semantic failure must not publish a manifest"
    );
}

#[test]
fn effective_time_fallback_hour_boundaries_dimensions_and_value_bounds_are_enforced() {
    let root = private_root();
    let key = key(1);
    let mut writer = DatasetWriter::open(root.path(), policy()).expect("open writer");

    let fallback_coverage = coverage(100, 100);
    writer
        .write_batch(
            &key,
            &metadata(vec![fallback_coverage.clone()], None),
            &batch_id(fallback_coverage, '1'),
            &batch_with_dimensions(&[500], &[None], &[500], 100, "book-delta", "binance", 1),
        )
        .expect("receive time is the effective-time fallback");

    let mismatch_coverage = coverage(101, 101);
    assert!(matches!(
        writer.write_batch(
            &key,
            &metadata(vec![mismatch_coverage.clone()], None),
            &batch_id(mismatch_coverage, '2'),
            &batch_with_dimensions(
                &[600],
                &[Some(599)],
                &[610],
                101,
                "book-delta",
                "binance",
                1,
            ),
        ),
        Err(StoreError::InvalidEventTimeColumn)
    ));

    let dimension_coverage = coverage(102, 102);
    assert!(matches!(
        writer.write_batch(
            &key,
            &metadata(vec![dimension_coverage.clone()], None),
            &batch_id(dimension_coverage, '3'),
            &batch_with_dimensions(&[700], &[Some(700)], &[710], 102, "trade", "binance", 1,),
        ),
        Err(StoreError::PartitionDimensionMismatch)
    ));

    let boundary_coverage = coverage(103, 103);
    assert!(matches!(
        writer.write_batch(
            &key,
            &metadata(vec![boundary_coverage.clone()], None),
            &batch_id(boundary_coverage, '4'),
            &batch_with_dimensions(
                &[3_600_000_000_000],
                &[Some(3_600_000_000_000)],
                &[3_600_000_000_010],
                103,
                "book-delta",
                "binance",
                1,
            ),
        ),
        Err(StoreError::EventTimeOutsidePartition)
    ));

    let oversized_coverage = coverage(104, 104);
    let original = batch(&[800], 104);
    let mut columns = original.columns().to_vec();
    columns[9] = Arc::new(StringArray::from(vec!["x".repeat(64 * 1024 + 1)]));
    let oversized =
        RecordBatch::try_new(original.schema(), columns).expect("oversized-value fixture");
    assert!(matches!(
        writer.write_batch(
            &key,
            &metadata(vec![oversized_coverage.clone()], None),
            &batch_id(oversized_coverage, '5'),
            &oversized,
        ),
        Err(StoreError::InvalidBatch)
    ));
}

#[test]
fn coverage_overlap_missing_correction_lineage_and_managed_hardlinks_fail_closed() {
    assert!(matches!(
        DatasetMetadata::try_new(DatasetMetadataInput {
            feature_version: "normalized-event-v1".to_owned(),
            parser_version: "binance-parser-v1".to_owned(),
            normalizer_version: "cmti-normalizer-v1".to_owned(),
            code_identity: "git_sha1:a6d85071bdba0e76bf1474eac7137c770971ecd4".to_owned(),
            source_coverage: vec![coverage(1, 3), coverage(3, 4)],
            correction_lineage: None,
        }),
        Err(StoreError::InvalidDatasetMetadata)
    ));

    let correction_root = private_root();
    let correction_coverage = coverage(100, 100);
    let mut correction_writer =
        DatasetWriter::open(correction_root.path(), policy()).expect("open correction writer");
    assert!(matches!(
        correction_writer.write_batch(
            &key(2),
            &metadata(vec![correction_coverage.clone()], None),
            &batch_id(correction_coverage, '1'),
            &batch(&[100], 100),
        ),
        Err(StoreError::InvalidCorrectionLineage)
    ));

    let overclaim_root = private_root();
    let mut overclaim_writer =
        DatasetWriter::open(overclaim_root.path(), policy()).expect("open overclaim writer");
    assert!(matches!(
        overclaim_writer.write_batch(
            &key(1),
            &metadata(vec![coverage(100, 101)], None),
            &batch_id(coverage(100, 100), '2'),
            &batch(&[100], 100),
        ),
        Err(StoreError::InvalidBatchIdentity)
    ));

    let hardlink_root = private_root();
    let hardlink_key = key(1);
    let hardlink_coverage = coverage(100, 100);
    let mut hardlink_writer =
        DatasetWriter::open(hardlink_root.path(), policy()).expect("open hardlink writer");
    let receipt = hardlink_writer
        .write_batch(
            &hardlink_key,
            &metadata(vec![hardlink_coverage.clone()], None),
            &batch_id(hardlink_coverage, '2'),
            &batch(&[100], 100),
        )
        .expect("write active fragment");
    let installed = hardlink_root.path().join(receipt.relative_path());
    let alias = installed
        .parent()
        .expect("active directory")
        .join(format!("fragment-{}.parquet", "a".repeat(64)));
    fs::hard_link(&installed, alias).expect("create managed hardlink attack fixture");
    assert!(matches!(
        hardlink_writer.seal(&hardlink_key),
        Err(StoreError::UnsafePath)
    ));

    let renamed_root = private_root();
    let renamed_key = key(1);
    let renamed_coverage = coverage(100, 100);
    let mut renamed_writer =
        DatasetWriter::open(renamed_root.path(), policy()).expect("open renamed-fragment writer");
    let receipt = renamed_writer
        .write_batch(
            &renamed_key,
            &metadata(vec![renamed_coverage.clone()], None),
            &batch_id(renamed_coverage, '3'),
            &batch(&[100], 100),
        )
        .expect("write source fragment");
    let installed = renamed_root.path().join(receipt.relative_path());
    let copied = installed
        .parent()
        .expect("active directory")
        .join(format!("fragment-{}.parquet", "b".repeat(64)));
    fs::copy(&installed, &copied).expect("copy fragment under forged identity");
    assert!(matches!(
        renamed_writer.seal(&renamed_key),
        Err(StoreError::Integrity)
    ));

    let corrupt_root = private_root();
    let corrupt_key = key(1);
    let corrupt_coverage = coverage(100, 100);
    let mut corrupt_writer =
        DatasetWriter::open(corrupt_root.path(), policy()).expect("open corrupt-fragment writer");
    let receipt = corrupt_writer
        .write_batch(
            &corrupt_key,
            &metadata(vec![corrupt_coverage.clone()], None),
            &batch_id(corrupt_coverage, '4'),
            &batch(&[100], 100),
        )
        .expect("write fragment to corrupt");
    let installed = corrupt_root.path().join(receipt.relative_path());
    let mut bytes = fs::read(&installed).expect("read active fragment");
    bytes[4] ^= 0xff;
    fs::write(installed, bytes).expect("corrupt active fragment");
    let result = corrupt_writer.seal(&corrupt_key);
    assert!(
        result.is_err(),
        "corrupted active fragment must never be published: {result:?}"
    );
}

#[test]
fn manifest_unknown_fields_and_undeclared_sealed_files_are_rejected() {
    let root = private_root();
    let key = key(1);
    let source_coverage = coverage(100, 100);
    let manifest = {
        let mut writer = DatasetWriter::open(root.path(), policy()).expect("open writer");
        writer
            .write_batch(
                &key,
                &metadata(vec![source_coverage.clone()], None),
                &batch_id(source_coverage, '1'),
                &batch(&[100], 100),
            )
            .expect("write fragment");
        writer.seal(&key).expect("seal partition")
    };
    let extra = root
        .path()
        .join(
            manifest.files()[0]
                .relative_path()
                .parent()
                .expect("sealed directory"),
        )
        .join("undeclared.parquet");
    fs::write(&extra, b"not parquet").expect("write undeclared sealed artifact");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&extra, fs::Permissions::from_mode(0o600)).expect("private file mode");
    }
    assert!(matches!(
        DatasetReader::open(root.path(), policy())
            .expect("open reader")
            .verify_partition(&key),
        Err(StoreError::Integrity)
    ));
    fs::remove_file(extra).expect("remove test-only undeclared artifact");
    let hidden = root
        .path()
        .join(
            manifest.files()[0]
                .relative_path()
                .parent()
                .expect("sealed directory"),
        )
        .join(".undeclared");
    fs::write(&hidden, b"hidden artifact").expect("write hidden sealed artifact");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o600))
            .expect("private hidden file mode");
    }
    assert!(matches!(
        DatasetReader::open(root.path(), policy())
            .expect("open reader")
            .verify_partition(&key),
        Err(StoreError::Integrity)
    ));
    fs::remove_file(hidden).expect("remove test-only hidden artifact");

    let manifest_path = root.path().join(key.relative_path()).join("manifest.json");
    let bytes = fs::read(&manifest_path).expect("read manifest");
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("manifest JSON");
    value
        .as_object_mut()
        .expect("manifest envelope")
        .insert("unknown".to_owned(), serde_json::Value::Bool(true));
    fs::write(
        &manifest_path,
        serde_json::to_vec(&value).expect("mutated manifest"),
    )
    .expect("write unknown field");
    assert!(matches!(
        DatasetReader::open(root.path(), policy())
            .expect("open reader")
            .verify_partition(&key),
        Err(StoreError::Integrity)
    ));
}

#[test]
fn compaction_is_deterministic_across_write_order_and_schema_metadata_is_rejected() {
    fn seal_in_order(reverse: bool) -> (String, String) {
        let root = private_root();
        let key = key(1);
        let first_coverage = coverage(100, 101);
        let second_coverage = coverage(102, 103);
        let entries = [
            (
                metadata(vec![first_coverage.clone()], None),
                batch_id(first_coverage, '1'),
                batch(&[100, 200], 100),
            ),
            (
                metadata(vec![second_coverage.clone()], None),
                batch_id(second_coverage, '2'),
                batch(&[300, 400], 102),
            ),
        ];
        let mut writer = DatasetWriter::open(root.path(), policy()).expect("open writer");
        let order: [usize; 2] = if reverse { [1, 0] } else { [0, 1] };
        for index in order {
            let (metadata, batch_id, batch) = &entries[index];
            writer
                .write_batch(&key, metadata, batch_id, batch)
                .expect("write deterministic fragment");
        }
        let manifest = writer.seal(&key).expect("seal deterministic partition");
        (
            hex::encode(manifest.manifest_hash()),
            hex::encode(manifest.files()[0].blake3()),
        )
    }

    assert_eq!(seal_in_order(false), seal_in_order(true));

    let root = private_root();
    let key = key(1);
    let source_coverage = coverage(100, 100);
    let input = batch(&[100], 100);
    let mut schema_metadata = std::collections::HashMap::new();
    schema_metadata.insert("caller-order-sensitive".to_owned(), "value".to_owned());
    let schema = Arc::new(Schema::new_with_metadata(
        input.schema().fields.clone(),
        schema_metadata,
    ));
    let with_metadata =
        RecordBatch::try_new(schema, input.columns().to_vec()).expect("metadata fixture");
    assert!(matches!(
        DatasetWriter::open(root.path(), policy())
            .expect("open writer")
            .write_batch(
                &key,
                &metadata(vec![source_coverage.clone()], None),
                &batch_id(source_coverage, '3'),
                &with_metadata,
            ),
        Err(StoreError::SchemaMismatch)
    ));
}

#[cfg(feature = "test-support")]
#[test]
fn durable_publication_crash_boundaries_recover_idempotently() {
    for (index, point) in [
        FaultPoint::AfterParquetCloseBeforeFileSync,
        FaultPoint::AfterFileSyncBeforeHash,
        FaultPoint::AfterFileHashBeforeRename,
        FaultPoint::AfterDataRenameBeforeDirectorySync,
    ]
    .into_iter()
    .enumerate()
    {
        let root = private_root();
        let key = key(1);
        let source_coverage = coverage(100, 100);
        let metadata = metadata(vec![source_coverage.clone()], None);
        let id = batch_id(
            source_coverage,
            char::from_digit(u32::try_from(index + 1).expect("small index"), 10)
                .expect("decimal marker"),
        );
        {
            let mut writer =
                DatasetWriter::open(root.path(), policy()).expect("open boundary writer");
            writer.inject_fault_once(point);
            assert!(matches!(
                writer.write_batch(&key, &metadata, &id, &batch(&[100], 100)),
                Err(StoreError::InjectedFault)
            ));
        }
        let active_directory = root.path().join(key.relative_path()).join("active");
        let leaves_stale_temporary = matches!(
            point,
            FaultPoint::AfterParquetCloseBeforeFileSync
                | FaultPoint::AfterFileSyncBeforeHash
                | FaultPoint::AfterFileHashBeforeRename
        );
        assert_eq!(
            has_temporary_entry(&active_directory),
            leaves_stale_temporary,
            "pre-rename write crashes must leave a recoverable temporary artifact"
        );
        let mut writer =
            DatasetWriter::open(root.path(), policy()).expect("reopen boundary writer");
        writer
            .write_batch(&key, &metadata, &id, &batch(&[100], 100))
            .expect("retry boundary batch");
        writer.seal(&key).expect("seal boundary batch");
        assert_eq!(
            DatasetReader::open(root.path(), policy())
                .expect("open boundary reader")
                .verify_partition(&key)
                .expect("verify boundary publication")
                .row_count(),
            1
        );
        assert!(!has_temporary_entry(&active_directory));
    }

    for (index, point) in [
        FaultPoint::AfterParquetCloseBeforeFileSync,
        FaultPoint::AfterFileSyncBeforeHash,
        FaultPoint::AfterFileHashBeforeRename,
        FaultPoint::AfterDataRenameBeforeDirectorySync,
        FaultPoint::AfterManifestTempSyncBeforeRename,
        FaultPoint::AfterManifestRenameBeforeDirectorySync,
    ]
    .into_iter()
    .enumerate()
    {
        let root = private_root();
        let key = key(1);
        let source_coverage = coverage(100, 100);
        {
            let mut writer =
                DatasetWriter::open(root.path(), policy()).expect("open seal-boundary writer");
            writer
                .write_batch(
                    &key,
                    &metadata(vec![source_coverage.clone()], None),
                    &batch_id(
                        source_coverage,
                        char::from_digit(u32::try_from(index + 1).expect("small index"), 10)
                            .expect("decimal marker"),
                    ),
                    &batch(&[100], 100),
                )
                .expect("write seal-boundary fragment");
            writer.inject_fault_once(point);
            assert!(matches!(writer.seal(&key), Err(StoreError::InjectedFault)));
        }
        let partition_directory = root.path().join(key.relative_path());
        let stale_directory = match point {
            FaultPoint::AfterParquetCloseBeforeFileSync
            | FaultPoint::AfterFileSyncBeforeHash
            | FaultPoint::AfterFileHashBeforeRename => Some(partition_directory.join("sealed")),
            FaultPoint::AfterManifestTempSyncBeforeRename => Some(partition_directory.clone()),
            FaultPoint::AfterDataRenameBeforeDirectorySync
            | FaultPoint::AfterManifestRenameBeforeDirectorySync
            | FaultPoint::AfterFragmentPublication
            | FaultPoint::AfterSealedDataPublication
            | FaultPoint::AfterManifestPublication
            | FaultPoint::CompactedFileExceedsCapacity => None,
        };
        if let Some(directory) = stale_directory.as_ref() {
            assert!(
                has_temporary_entry(directory),
                "pre-rename seal crash must leave a recoverable temporary artifact"
            );
        }
        let manifest = DatasetWriter::open(root.path(), policy())
            .expect("reopen seal-boundary writer")
            .seal(&key)
            .expect("recover seal boundary");
        assert_eq!(manifest.row_count(), 1);
        assert_eq!(
            DatasetReader::open(root.path(), policy())
                .expect("open seal-boundary reader")
                .verify_partition(&key)
                .expect("verify recovered seal boundary")
                .row_count(),
            1
        );
        if let Some(directory) = stale_directory.as_ref() {
            assert!(
                !has_temporary_entry(directory),
                "restart must remove the stale temporary artifact"
            );
        }
    }

    let fragment_root = private_root();
    let fragment_key = key(1);
    let fragment_coverage = coverage(100, 100);
    let fragment_metadata = metadata(vec![fragment_coverage.clone()], None);
    let fragment_id = batch_id(fragment_coverage, '1');
    let mut fragment_writer =
        DatasetWriter::open(fragment_root.path(), policy()).expect("open fragment writer");
    fragment_writer.inject_fault_once(FaultPoint::AfterFragmentPublication);
    assert!(matches!(
        fragment_writer.write_batch(
            &fragment_key,
            &fragment_metadata,
            &fragment_id,
            &batch(&[100], 100),
        ),
        Err(StoreError::InjectedFault)
    ));
    fragment_writer
        .write_batch(
            &fragment_key,
            &fragment_metadata,
            &fragment_id,
            &batch(&[100], 100),
        )
        .expect("retry after lost fragment acknowledgement");
    fragment_writer
        .seal(&fragment_key)
        .expect("seal recovered fragment");

    let data_root = private_root();
    let data_key = key(1);
    let data_coverage = coverage(100, 100);
    {
        let mut writer = DatasetWriter::open(data_root.path(), policy()).expect("open data writer");
        writer
            .write_batch(
                &data_key,
                &metadata(vec![data_coverage.clone()], None),
                &batch_id(data_coverage, '2'),
                &batch(&[100], 100),
            )
            .expect("write data fragment");
        writer.inject_fault_once(FaultPoint::AfterSealedDataPublication);
        assert!(matches!(
            writer.seal(&data_key),
            Err(StoreError::InjectedFault)
        ));
    }
    DatasetWriter::open(data_root.path(), policy())
        .expect("reopen after orphaned sealed data")
        .seal(&data_key)
        .expect("publish manifest after data-only crash");

    let manifest_root = private_root();
    let manifest_key = key(1);
    let manifest_coverage = coverage(100, 100);
    {
        let mut writer =
            DatasetWriter::open(manifest_root.path(), policy()).expect("open manifest writer");
        writer
            .write_batch(
                &manifest_key,
                &metadata(vec![manifest_coverage.clone()], None),
                &batch_id(manifest_coverage, '3'),
                &batch(&[100], 100),
            )
            .expect("write manifest fragment");
        writer.inject_fault_once(FaultPoint::AfterManifestPublication);
        assert!(matches!(
            writer.seal(&manifest_key),
            Err(StoreError::InjectedFault)
        ));
    }
    let manifest = DatasetWriter::open(manifest_root.path(), policy())
        .expect("reopen after manifest publication")
        .seal(&manifest_key)
        .expect("verify manifest and clean active fragments");
    assert_eq!(manifest.row_count(), 1);
    assert_eq!(
        DatasetReader::open(manifest_root.path(), policy())
            .expect("open reader")
            .verify_partition(&manifest_key)
            .expect("verify recovered publication")
            .row_count(),
        1
    );
}
