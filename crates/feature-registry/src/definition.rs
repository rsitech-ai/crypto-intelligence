//! Validated, versioned feature definitions.

use std::num::NonZeroU64;

use domain::UnixNanos;
use fixed_decimal::FixedDecimal;
use quality::SourceHealthState;
use semver::Version;
use serde::Serialize;

use crate::RegistryError;

const MAX_IDENTIFIER_LENGTH: usize = 128;
const MAX_DOCUMENTATION_LENGTH: usize = 256;
const MAX_VERSION_LENGTH: usize = 128;
const MAX_INPUTS: usize = 64;
const MAX_WINDOWS: usize = 64;

/// Stable feature identifier.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct FeatureId(String);

impl FeatureId {
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        validate_feature_identifier(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn documentation_anchor(&self) -> String {
        self.0.replace('_', "-")
    }
}

/// Stable identifier for one declared feature window.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct WindowId(String);

impl WindowId {
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        validate_identifier(&value, "window_id")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Nanosecond duration used by feature contracts.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DurationNanos(u64);

impl DurationNanos {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

/// Release classification for a feature contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureStatus {
    Required,
    Optional,
    Experimental,
}

/// Serialized value representation produced by a feature.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureValueType {
    FixedDecimal,
    Float64,
    Integer,
    Boolean,
}

/// Canonical entity family accepted by a definition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityScope {
    Instrument,
    Asset,
    Venue,
    Source,
    Global,
}

/// Input dependency declared for audit and lineage.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InputRequirement {
    id: String,
}

impl InputRequirement {
    pub fn new(id: impl Into<String>) -> Result<Self, RegistryError> {
        let id = id.into();
        validate_identifier(&id, "input_id")?;
        Ok(Self { id })
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Timestamp selection policy for economic event time.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTimePolicy {
    SourceEventTime,
    ReceiveTime,
}

/// Declared window family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    Tumbling,
    Sliding,
    SessionAligned,
    ExponentiallyWeighted,
    EventCount,
    Volume,
    Notional,
}

/// Bounded window declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WindowDefinition {
    id: WindowId,
    kind: WindowKind,
    parameter: WindowParameter,
}

/// Semantically typed window parameter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum WindowParameter {
    Time {
        extent: DurationNanos,
        advance: Option<DurationNanos>,
    },
    SessionAligned {
        extent: DurationNanos,
        utc_anchor: UnixNanos,
    },
    ExponentiallyWeighted {
        half_life: DurationNanos,
    },
    EventCount {
        events: NonZeroU64,
        advance: Option<NonZeroU64>,
    },
    Threshold {
        threshold: FixedDecimal,
    },
}

impl WindowDefinition {
    pub fn try_new_time(
        id: WindowId,
        kind: WindowKind,
        extent: DurationNanos,
        advance: Option<DurationNanos>,
    ) -> Result<Self, RegistryError> {
        if extent.is_zero()
            || extent.value() > i64::MAX as u64
            || advance.is_some_and(|advance| advance.value() > i64::MAX as u64)
        {
            return Err(RegistryError::InvalidWindow);
        }
        match kind {
            WindowKind::Sliding => {
                let advance = advance.ok_or(RegistryError::InvalidWindow)?;
                if advance.is_zero() || advance > extent {
                    return Err(RegistryError::InvalidWindow);
                }
            }
            WindowKind::Tumbling => {
                if advance.is_some() {
                    return Err(RegistryError::InvalidWindow);
                }
            }
            WindowKind::SessionAligned
            | WindowKind::ExponentiallyWeighted
            | WindowKind::EventCount
            | WindowKind::Volume
            | WindowKind::Notional => {
                return Err(RegistryError::InvalidWindow);
            }
        }
        Ok(Self {
            id,
            kind,
            parameter: WindowParameter::Time { extent, advance },
        })
    }

    pub fn try_new_session_aligned(
        id: WindowId,
        extent: DurationNanos,
        utc_anchor: UnixNanos,
    ) -> Result<Self, RegistryError> {
        if extent.is_zero() || extent.value() > i64::MAX as u64 || utc_anchor.value() <= 0 {
            return Err(RegistryError::InvalidWindow);
        }
        Ok(Self {
            id,
            kind: WindowKind::SessionAligned,
            parameter: WindowParameter::SessionAligned { extent, utc_anchor },
        })
    }

    pub fn try_new_exponentially_weighted(
        id: WindowId,
        half_life: DurationNanos,
    ) -> Result<Self, RegistryError> {
        if half_life.is_zero() || half_life.value() > i64::MAX as u64 {
            return Err(RegistryError::InvalidWindow);
        }
        Ok(Self {
            id,
            kind: WindowKind::ExponentiallyWeighted,
            parameter: WindowParameter::ExponentiallyWeighted { half_life },
        })
    }

    pub fn try_new_event_count(
        id: WindowId,
        events: NonZeroU64,
        advance: Option<NonZeroU64>,
    ) -> Result<Self, RegistryError> {
        if advance.is_some_and(|advance| advance > events) {
            return Err(RegistryError::InvalidWindow);
        }
        Ok(Self {
            id,
            kind: WindowKind::EventCount,
            parameter: WindowParameter::EventCount { events, advance },
        })
    }

    pub fn try_new_threshold(
        id: WindowId,
        kind: WindowKind,
        threshold: FixedDecimal,
    ) -> Result<Self, RegistryError> {
        if !matches!(kind, WindowKind::Volume | WindowKind::Notional) || !threshold.is_positive() {
            return Err(RegistryError::InvalidWindow);
        }
        Ok(Self {
            id,
            kind,
            parameter: WindowParameter::Threshold { threshold },
        })
    }

    pub const fn id(&self) -> &WindowId {
        &self.id
    }

    pub const fn kind(&self) -> WindowKind {
        self.kind
    }

    pub const fn parameter(&self) -> &WindowParameter {
        &self.parameter
    }
}

/// Leakage-safe normalization family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizationKind {
    None,
    RobustRolling,
    ExpandingQuantile,
    TrainingFoldZScore,
    Log,
    SignedLog,
    VolatilityScaled,
    AssetRelative,
    CrossSectionalRank,
    TimeOfWeekSeasonal,
    LiquidityBucket,
}

/// Versioned normalization policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NormalizationPolicy {
    kind: NormalizationKind,
    version: Version,
}

impl NormalizationPolicy {
    pub fn try_new(kind: NormalizationKind, version: Version) -> Result<Self, RegistryError> {
        validate_version(&version, "normalization_version")?;
        Ok(Self { kind, version })
    }

    pub const fn kind(&self) -> NormalizationKind {
        self.kind
    }

    pub const fn version(&self) -> &Version {
        &self.version
    }
}

/// Required handling when an observation is unavailable.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingnessPolicy {
    Explicit,
    DeterministicTrainingFoldImputation,
    Abstain,
}

/// Exact scalar quality score in millionths.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct QualityScore(u32);

impl QualityScore {
    pub const MAX_MILLIONTHS: u32 = 1_000_000;

    pub fn from_millionths(value: u32) -> Result<Self, RegistryError> {
        if value > Self::MAX_MILLIONTHS {
            Err(RegistryError::InvalidQualityScore)
        } else {
            Ok(Self(value))
        }
    }

    pub const fn millionths(self) -> u32 {
        self.0
    }
}

/// Minimum score and acceptable source states for a usable observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QualityRequirement {
    minimum_score: QualityScore,
    minimum_coverage: QualityScore,
    allowed_source_states: Vec<SourceHealthState>,
}

impl QualityRequirement {
    pub fn try_new(
        minimum_score: QualityScore,
        minimum_coverage: QualityScore,
        mut allowed_source_states: Vec<SourceHealthState>,
    ) -> Result<Self, RegistryError> {
        if allowed_source_states.len() > SourceHealthState::ALL.len() {
            return Err(RegistryError::InvalidQualityGate);
        }
        allowed_source_states.sort_by_key(|state| state.as_str());
        allowed_source_states.dedup();
        if allowed_source_states.is_empty() {
            return Err(RegistryError::InvalidQualityGate);
        }
        Ok(Self {
            minimum_score,
            minimum_coverage,
            allowed_source_states,
        })
    }

    pub const fn minimum_score(&self) -> QualityScore {
        self.minimum_score
    }

    pub const fn minimum_coverage(&self) -> QualityScore {
        self.minimum_coverage
    }

    pub fn allows(&self, state: SourceHealthState) -> bool {
        self.allowed_source_states.contains(&state)
    }

    pub fn allowed_source_states(&self) -> &[SourceHealthState] {
        &self.allowed_source_states
    }
}

/// Nonzero digest of the implemented formula.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FormulaHash([u8; 32]);

impl FormulaHash {
    pub fn new(value: [u8; 32]) -> Result<Self, RegistryError> {
        ensure_nonzero_digest(value, "formula_hash").map(Self)
    }

    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }
}

impl Serialize for FormulaHash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&encode_hex(&self.0))
    }
}

/// Durable local documentation reference for a feature.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FeatureDocumentation(String);

impl FeatureDocumentation {
    pub fn new(value: impl Into<String>) -> Result<Self, RegistryError> {
        let value = value.into();
        let Some(anchor) = value.strip_prefix("docs/data-dictionary/features.md#") else {
            return Err(RegistryError::InvalidDocumentation);
        };
        if value.len() > MAX_DOCUMENTATION_LENGTH
            || anchor.is_empty()
            || !anchor.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            return Err(RegistryError::InvalidDocumentation);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn anchor(&self) -> &str {
        self.0.split_once('#').map_or("", |(_, anchor)| anchor)
    }
}

/// Construction input for a complete definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureDefinitionInput {
    pub id: FeatureId,
    pub version: Version,
    pub status: FeatureStatus,
    pub value_type: FeatureValueType,
    pub entities: EntityScope,
    pub required_inputs: Vec<InputRequirement>,
    pub event_time_policy: EventTimePolicy,
    pub windows: Vec<WindowDefinition>,
    pub output_resolution: DurationNanos,
    pub allowed_lateness: DurationNanos,
    pub time_to_live: DurationNanos,
    pub normalization: NormalizationPolicy,
    pub missingness: MissingnessPolicy,
    pub quality_gate: QualityRequirement,
    pub formula_hash: FormulaHash,
    pub documentation: FeatureDocumentation,
}

/// Immutable feature definition stored in the registry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FeatureDefinition {
    id: FeatureId,
    version: Version,
    status: FeatureStatus,
    value_type: FeatureValueType,
    entities: EntityScope,
    required_inputs: Vec<InputRequirement>,
    event_time_policy: EventTimePolicy,
    windows: Vec<WindowDefinition>,
    output_resolution: DurationNanos,
    allowed_lateness: DurationNanos,
    time_to_live: DurationNanos,
    normalization: NormalizationPolicy,
    missingness: MissingnessPolicy,
    quality_gate: QualityRequirement,
    formula_hash: FormulaHash,
    documentation: FeatureDocumentation,
}

impl FeatureDefinition {
    pub fn try_new(mut input: FeatureDefinitionInput) -> Result<Self, RegistryError> {
        validate_version(&input.version, "feature_version")?;
        if input.documentation.anchor() != input.id.documentation_anchor() {
            return Err(RegistryError::DocumentationFeatureMismatch);
        }
        validate_duration("output_resolution", input.output_resolution, false)?;
        validate_duration("allowed_lateness", input.allowed_lateness, true)?;
        validate_duration("time_to_live", input.time_to_live, false)?;
        if input.time_to_live < input.output_resolution
            || input.allowed_lateness > input.time_to_live
        {
            return Err(RegistryError::InvalidDefinition {
                field: "duration_relationship",
            });
        }
        if input.required_inputs.is_empty() || input.required_inputs.len() > MAX_INPUTS {
            return Err(RegistryError::InvalidDefinition {
                field: "required_inputs",
            });
        }
        input
            .required_inputs
            .sort_by(|left, right| left.id.cmp(&right.id));
        if input
            .required_inputs
            .windows(2)
            .any(|pair| pair[0].id == pair[1].id)
        {
            return Err(RegistryError::DuplicateInput);
        }
        if input.windows.is_empty() || input.windows.len() > MAX_WINDOWS {
            return Err(RegistryError::InvalidDefinition { field: "windows" });
        }
        input.windows.sort_by(|left, right| left.id.cmp(&right.id));
        if input
            .windows
            .windows(2)
            .any(|pair| pair[0].id == pair[1].id)
        {
            return Err(RegistryError::DuplicateWindow);
        }
        Ok(Self {
            id: input.id,
            version: input.version,
            status: input.status,
            value_type: input.value_type,
            entities: input.entities,
            required_inputs: input.required_inputs,
            event_time_policy: input.event_time_policy,
            windows: input.windows,
            output_resolution: input.output_resolution,
            allowed_lateness: input.allowed_lateness,
            time_to_live: input.time_to_live,
            normalization: input.normalization,
            missingness: input.missingness,
            quality_gate: input.quality_gate,
            formula_hash: input.formula_hash,
            documentation: input.documentation,
        })
    }

    pub const fn id(&self) -> &FeatureId {
        &self.id
    }

    pub const fn version(&self) -> &Version {
        &self.version
    }

    pub const fn status(&self) -> FeatureStatus {
        self.status
    }

    pub const fn value_type(&self) -> FeatureValueType {
        self.value_type
    }

    pub const fn entities(&self) -> EntityScope {
        self.entities
    }

    pub fn required_inputs(&self) -> &[InputRequirement] {
        &self.required_inputs
    }

    pub const fn event_time_policy(&self) -> EventTimePolicy {
        self.event_time_policy
    }

    pub fn windows(&self) -> &[WindowDefinition] {
        &self.windows
    }

    pub const fn output_resolution(&self) -> DurationNanos {
        self.output_resolution
    }

    pub const fn allowed_lateness(&self) -> DurationNanos {
        self.allowed_lateness
    }

    pub const fn time_to_live(&self) -> DurationNanos {
        self.time_to_live
    }

    pub const fn normalization(&self) -> &NormalizationPolicy {
        &self.normalization
    }

    pub const fn missingness(&self) -> MissingnessPolicy {
        self.missingness
    }

    pub const fn quality_gate(&self) -> &QualityRequirement {
        &self.quality_gate
    }

    pub const fn formula_hash(&self) -> FormulaHash {
        self.formula_hash
    }

    pub const fn documentation(&self) -> &FeatureDocumentation {
        &self.documentation
    }

    pub fn window(&self, id: &WindowId) -> Option<&WindowDefinition> {
        self.windows.iter().find(|window| window.id() == id)
    }
}

fn validate_identifier(value: &str, field: &'static str) -> Result<(), RegistryError> {
    let starts_alphanumeric = value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric);
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_LENGTH
        || !starts_alphanumeric
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
    {
        Err(RegistryError::InvalidIdentifier { field })
    } else {
        Ok(())
    }
}

pub(crate) fn validate_version(
    version: &Version,
    field: &'static str,
) -> Result<(), RegistryError> {
    let core_length = decimal_digits(version.major)
        + decimal_digits(version.minor)
        + decimal_digits(version.patch)
        + 2;
    let prerelease_length = usize::from(!version.pre.is_empty()) + version.pre.as_str().len();
    let build_length = usize::from(!version.build.is_empty()) + version.build.as_str().len();
    if core_length
        .checked_add(prerelease_length)
        .and_then(|length| length.checked_add(build_length))
        .is_none_or(|length| length > MAX_VERSION_LENGTH)
    {
        Err(RegistryError::InvalidVersion { field })
    } else {
        Ok(())
    }
}

fn validate_feature_identifier(value: &str) -> Result<(), RegistryError> {
    let starts_alphanumeric = value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric);
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_LENGTH
        || !starts_alphanumeric
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Err(RegistryError::InvalidIdentifier {
            field: "feature_id",
        })
    } else {
        Ok(())
    }
}

fn decimal_digits(mut value: u64) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn validate_duration(
    field: &'static str,
    duration: DurationNanos,
    zero_allowed: bool,
) -> Result<(), RegistryError> {
    if (!zero_allowed && duration.is_zero()) || duration.value() > i64::MAX as u64 {
        Err(RegistryError::InvalidDefinition { field })
    } else {
        Ok(())
    }
}

pub(crate) fn ensure_nonzero_digest(
    value: [u8; 32],
    field: &'static str,
) -> Result<[u8; 32], RegistryError> {
    if value == [0; 32] {
        Err(RegistryError::ZeroDigest { field })
    } else {
        Ok(value)
    }
}

pub(crate) fn encode_hex(value: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(64);
    for byte in value {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}
