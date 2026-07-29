//! Validated, content-addressed training-dataset manifests.

use std::collections::{BTreeMap, BTreeSet};

use semver::Version;
use serde::Serialize;

use crate::DatasetError;

const MANIFEST_HASH_DOMAIN: &[u8] = b"cmti:dataset-manifest:v1\0";
const MAXIMUM_MANIFEST_ENTRIES: usize = 4_096;
const MAXIMUM_IDENTIFIER_BYTES: usize = 128;
const MAXIMUM_VERSION_BYTES: usize = 256;

/// How source corrections are selected when materializing the dataset.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionPolicy {
    /// Select only revisions whose knowledge time is at or before the manifest cutoff.
    AsKnownAtCutoff,
}

/// Construction input for a complete training-dataset manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatasetManifestInput {
    pub dataset_id: String,
    pub dataset_version: Version,
    pub created_at_ns: i64,
    pub as_known_at_cutoff_ns: i64,
    pub entity_universe_hash: [u8; 32],
    pub source_versions: BTreeMap<String, String>,
    pub schema_versions: BTreeMap<String, u32>,
    pub feature_versions: BTreeMap<String, Version>,
    pub label_versions: BTreeMap<String, Version>,
    pub partition_hashes: Vec<[u8; 32]>,
    pub exclusion_counts: BTreeMap<String, u64>,
    pub correction_policy: CorrectionPolicy,
    pub code_commit: String,
    pub license_manifest_hash: [u8; 32],
}

/// Immutable, canonical identity for a reproducible training dataset.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DatasetManifest {
    dataset_id: String,
    dataset_version: Version,
    created_at_ns: i64,
    as_known_at_cutoff_ns: i64,
    entity_universe_hash: [u8; 32],
    source_versions: BTreeMap<String, String>,
    schema_versions: BTreeMap<String, u32>,
    feature_versions: BTreeMap<String, Version>,
    label_versions: BTreeMap<String, Version>,
    partition_hashes: Vec<[u8; 32]>,
    exclusion_counts: BTreeMap<String, u64>,
    correction_policy: CorrectionPolicy,
    code_commit: String,
    license_manifest_hash: [u8; 32],
    manifest_hash: [u8; 32],
}

impl DatasetManifest {
    pub fn try_new(mut input: DatasetManifestInput) -> Result<Self, DatasetError> {
        validate_manifest(&input)?;
        input.partition_hashes.sort_unstable();
        let manifest_hash = hash_manifest(&input)?;
        Ok(Self {
            dataset_id: input.dataset_id,
            dataset_version: input.dataset_version,
            created_at_ns: input.created_at_ns,
            as_known_at_cutoff_ns: input.as_known_at_cutoff_ns,
            entity_universe_hash: input.entity_universe_hash,
            source_versions: input.source_versions,
            schema_versions: input.schema_versions,
            feature_versions: input.feature_versions,
            label_versions: input.label_versions,
            partition_hashes: input.partition_hashes,
            exclusion_counts: input.exclusion_counts,
            correction_policy: input.correction_policy,
            code_commit: input.code_commit,
            license_manifest_hash: input.license_manifest_hash,
            manifest_hash,
        })
    }

    pub fn dataset_id(&self) -> &str {
        &self.dataset_id
    }

    pub const fn dataset_version(&self) -> &Version {
        &self.dataset_version
    }

    pub const fn as_known_at_cutoff_ns(&self) -> i64 {
        self.as_known_at_cutoff_ns
    }

    pub const fn manifest_hash(&self) -> [u8; 32] {
        self.manifest_hash
    }

    pub fn partition_hashes(&self) -> &[[u8; 32]] {
        &self.partition_hashes
    }

    pub fn exclusion_counts(&self) -> &BTreeMap<String, u64> {
        &self.exclusion_counts
    }
}

fn validate_manifest(input: &DatasetManifestInput) -> Result<(), DatasetError> {
    if !valid_identifier(&input.dataset_id)
        || !valid_version(&input.dataset_version)
        || input.created_at_ns <= 0
        || input.as_known_at_cutoff_ns <= 0
        || input.as_known_at_cutoff_ns > input.created_at_ns
        || input.entity_universe_hash == [0; 32]
        || input.license_manifest_hash == [0; 32]
        || !valid_commit(&input.code_commit)
        || input.partition_hashes.is_empty()
        || input.partition_hashes.len() > MAXIMUM_MANIFEST_ENTRIES
        || input.partition_hashes.contains(&[0; 32])
        || input.partition_hashes.iter().collect::<BTreeSet<_>>().len()
            != input.partition_hashes.len()
        || !valid_map_size(&input.source_versions)
        || !valid_map_size(&input.schema_versions)
        || !valid_map_size(&input.feature_versions)
        || !valid_map_size(&input.label_versions)
        || input.exclusion_counts.len() > MAXIMUM_MANIFEST_ENTRIES
        || input
            .source_versions
            .iter()
            .any(|(key, value)| !valid_identifier(key) || !valid_text_version(value))
        || input
            .schema_versions
            .iter()
            .any(|(key, value)| !valid_identifier(key) || *value == 0)
        || input
            .feature_versions
            .iter()
            .chain(input.label_versions.iter())
            .any(|(key, value)| !valid_identifier(key) || !valid_version(value))
        || input
            .exclusion_counts
            .keys()
            .any(|key| !valid_identifier(key))
    {
        return Err(DatasetError::InvalidManifest);
    }
    Ok(())
}

fn valid_map_size<K, V>(map: &BTreeMap<K, V>) -> bool {
    !map.is_empty() && map.len() <= MAXIMUM_MANIFEST_ENTRIES
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_IDENTIFIER_BYTES
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_text_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAXIMUM_VERSION_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'\\' && byte != b'"')
}

fn valid_version(value: &Version) -> bool {
    value.major > 0 && value.pre.is_empty() && value.build.is_empty()
}

fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Serialize)]
struct ManifestHashWire<'a> {
    schema_version: u32,
    dataset_id: &'a str,
    dataset_version: String,
    created_at_ns: i64,
    as_known_at_cutoff_ns: i64,
    entity_universe_hash: [u8; 32],
    source_versions: &'a BTreeMap<String, String>,
    schema_versions: &'a BTreeMap<String, u32>,
    feature_versions: BTreeMap<&'a str, String>,
    label_versions: BTreeMap<&'a str, String>,
    partition_hashes: &'a [[u8; 32]],
    exclusion_counts: &'a BTreeMap<String, u64>,
    correction_policy: CorrectionPolicy,
    code_commit: &'a str,
    license_manifest_hash: [u8; 32],
}

fn hash_manifest(input: &DatasetManifestInput) -> Result<[u8; 32], DatasetError> {
    let wire = ManifestHashWire {
        schema_version: 1,
        dataset_id: &input.dataset_id,
        dataset_version: input.dataset_version.to_string(),
        created_at_ns: input.created_at_ns,
        as_known_at_cutoff_ns: input.as_known_at_cutoff_ns,
        entity_universe_hash: input.entity_universe_hash,
        source_versions: &input.source_versions,
        schema_versions: &input.schema_versions,
        feature_versions: input
            .feature_versions
            .iter()
            .map(|(key, value)| (key.as_str(), value.to_string()))
            .collect(),
        label_versions: input
            .label_versions
            .iter()
            .map(|(key, value)| (key.as_str(), value.to_string()))
            .collect(),
        partition_hashes: &input.partition_hashes,
        exclusion_counts: &input.exclusion_counts,
        correction_policy: input.correction_policy,
        code_commit: &input.code_commit,
        license_manifest_hash: input.license_manifest_hash,
    };
    let bytes = serde_json::to_vec(&wire).map_err(|_| DatasetError::ManifestSerialization)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(MANIFEST_HASH_DOMAIN);
    hasher.update(&bytes);
    Ok(*hasher.finalize().as_bytes())
}
