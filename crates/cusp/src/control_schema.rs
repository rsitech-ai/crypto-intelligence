use std::collections::BTreeSet;

use feature_registry::{FeatureId, FeatureRegistry, FeatureValueType, MissingnessPolicy};
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::ControlError;

pub const MAX_CONTROL_FEATURES: usize = 64;
pub const MAX_CONTROL_GROUPS: usize = 32;
const MAX_IDENTIFIER_LENGTH: usize = 128;

/// Exact feature identity consumed by one control-map package.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "RawControlFeatureKey", into = "RawControlFeatureKey")]
pub struct ControlFeatureKey {
    id: String,
    version: Version,
}

impl ControlFeatureKey {
    pub fn try_new(id: impl Into<String>, version: Version) -> Result<Self, ControlError> {
        let id = id.into();
        FeatureId::new(id.clone()).map_err(|_| ControlError::InvalidFeatureIdentity)?;
        validate_version(&version)?;
        Ok(Self { id, version })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn version(&self) -> &Version {
        &self.version
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControlFeatureKey {
    id: String,
    version: Version,
}

impl TryFrom<RawControlFeatureKey> for ControlFeatureKey {
    type Error = ControlError;

    fn try_from(raw: RawControlFeatureKey) -> Result<Self, Self::Error> {
        Self::try_new(raw.id, raw.version)
    }
}

impl From<ControlFeatureKey> for RawControlFeatureKey {
    fn from(key: ControlFeatureKey) -> Self {
        Self {
            id: key.id,
            version: key.version,
        }
    }
}

/// Controls a feature is permitted to influence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlTarget {
    Neither,
    Alpha,
    Beta,
    Both,
}

impl ControlTarget {
    pub const fn permits_alpha(self) -> bool {
        matches!(self, Self::Alpha | Self::Both)
    }

    pub const fn permits_beta(self) -> bool {
        matches!(self, Self::Beta | Self::Both)
    }
}

/// Optional fitted-coefficient sign constraint for one feature group.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoefficientSign {
    Any,
    NonNegative,
    NonPositive,
}

impl CoefficientSign {
    pub(crate) fn permits(self, coefficient: f64) -> bool {
        match self {
            Self::Any => true,
            Self::NonNegative => coefficient >= 0.0,
            Self::NonPositive => coefficient <= 0.0,
        }
    }
}

/// Constraint and sparse-penalty policy shared by related features.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureGroup {
    pub id: String,
    pub alpha_sign: CoefficientSign,
    pub beta_sign: CoefficientSign,
    pub sparse_penalty: f64,
}

impl FeatureGroup {
    pub fn try_new(
        id: impl Into<String>,
        alpha_sign: CoefficientSign,
        beta_sign: CoefficientSign,
        sparse_penalty: f64,
    ) -> Result<Self, ControlError> {
        let group = Self {
            id: id.into(),
            alpha_sign,
            beta_sign,
            sparse_penalty,
        };
        group.validate()?;
        Ok(group)
    }

    fn validate(&self) -> Result<(), ControlError> {
        validate_identifier(&self.id)?;
        if !self.sparse_penalty.is_finite() || self.sparse_penalty < 0.0 {
            return Err(ControlError::InvalidSparsePenalty);
        }
        Ok(())
    }
}

/// Training-fitted normalization and target policy for one feature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlFeature {
    pub key: ControlFeatureKey,
    pub required: bool,
    pub group_id: String,
    pub target: ControlTarget,
    pub normalization_center: f64,
    pub normalization_scale: f64,
}

impl ControlFeature {
    pub fn try_new(
        key: ControlFeatureKey,
        required: bool,
        group_id: impl Into<String>,
        target: ControlTarget,
        normalization_center: f64,
        normalization_scale: f64,
    ) -> Result<Self, ControlError> {
        let feature = Self {
            key,
            required,
            group_id: group_id.into(),
            target,
            normalization_center,
            normalization_scale,
        };
        feature.validate()?;
        Ok(feature)
    }

    fn validate(&self) -> Result<(), ControlError> {
        validate_identifier(&self.group_id)?;
        if !self.normalization_center.is_finite()
            || !self.normalization_scale.is_finite()
            || self.normalization_scale <= 0.0
        {
            return Err(ControlError::InvalidNormalization);
        }
        ControlFeatureKey::try_new(self.key.id.clone(), self.key.version.clone())?;
        Ok(())
    }
}

/// Versioned, bounded schema governing the alpha/beta design vector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ControlSchemaInput", into = "ControlSchemaInput")]
pub struct ControlSchema {
    pub version: Version,
    pub groups: Vec<FeatureGroup>,
    pub features: Vec<ControlFeature>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlSchemaInput {
    pub version: Version,
    pub groups: Vec<FeatureGroup>,
    pub features: Vec<ControlFeature>,
}

impl ControlSchema {
    pub fn try_new(
        version: Version,
        groups: Vec<FeatureGroup>,
        features: Vec<ControlFeature>,
    ) -> Result<Self, ControlError> {
        Self::try_from(ControlSchemaInput {
            version,
            groups,
            features,
        })
    }

    pub fn validate_against_registry(
        &self,
        registry: &FeatureRegistry,
    ) -> Result<(), ControlError> {
        let mut validated = self.clone();
        validated.validate()?;
        for feature in &self.features {
            let id = FeatureId::new(feature.key.id.clone())
                .map_err(|_| ControlError::InvalidFeatureIdentity)?;
            let definition = registry
                .ensure_model_input(&id, &feature.key.version)
                .map_err(|_| ControlError::FeatureNotModelEligible)?;
            if definition.missingness() != MissingnessPolicy::Explicit
                || !matches!(
                    definition.value_type(),
                    FeatureValueType::FixedDecimal
                        | FeatureValueType::Float64
                        | FeatureValueType::Integer
                )
            {
                return Err(ControlError::FeatureNotModelEligible);
            }
        }
        Ok(())
    }

    pub fn feature(&self, key: &ControlFeatureKey) -> Option<&ControlFeature> {
        self.features
            .binary_search_by(|feature| feature.key.cmp(key))
            .ok()
            .map(|index| &self.features[index])
    }

    pub(crate) fn group(&self, id: &str) -> Option<&FeatureGroup> {
        self.groups
            .binary_search_by(|group| group.id.as_str().cmp(id))
            .ok()
            .map(|index| &self.groups[index])
    }

    pub(crate) fn validate(&mut self) -> Result<(), ControlError> {
        validate_version(&self.version)?;
        if self.groups.is_empty() || self.groups.len() > MAX_CONTROL_GROUPS {
            return Err(ControlError::InvalidGroupCount);
        }
        if self.features.is_empty() || self.features.len() > MAX_CONTROL_FEATURES {
            return Err(ControlError::InvalidFeatureCount);
        }
        for group in &self.groups {
            group.validate()?;
        }
        self.groups.sort_by(|left, right| left.id.cmp(&right.id));
        if self.groups.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(ControlError::DuplicateGroup);
        }
        for feature in &self.features {
            feature.validate()?;
        }
        self.features
            .sort_by(|left, right| left.key.cmp(&right.key));
        if self
            .features
            .windows(2)
            .any(|pair| pair[0].key == pair[1].key)
        {
            return Err(ControlError::DuplicateFeature);
        }
        let groups = self
            .groups
            .iter()
            .map(|group| group.id.as_str())
            .collect::<BTreeSet<_>>();
        if self
            .features
            .iter()
            .any(|feature| !groups.contains(feature.group_id.as_str()))
        {
            return Err(ControlError::UnknownFeatureGroup);
        }
        Ok(())
    }
}

impl TryFrom<ControlSchemaInput> for ControlSchema {
    type Error = ControlError;

    fn try_from(input: ControlSchemaInput) -> Result<Self, Self::Error> {
        let mut schema = Self {
            version: input.version,
            groups: input.groups,
            features: input.features,
        };
        schema.validate()?;
        Ok(schema)
    }
}

impl From<ControlSchema> for ControlSchemaInput {
    fn from(schema: ControlSchema) -> Self {
        Self {
            version: schema.version,
            groups: schema.groups,
            features: schema.features,
        }
    }
}

fn validate_version(version: &Version) -> Result<(), ControlError> {
    if version.major == 0 || version.to_string().len() > MAX_IDENTIFIER_LENGTH {
        Err(ControlError::InvalidVersion)
    } else {
        Ok(())
    }
}

fn validate_identifier(value: &str) -> Result<(), ControlError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_LENGTH
        || !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || (index > 0 && matches!(byte, b'_' | b'-'))
        })
    {
        Err(ControlError::InvalidIdentifier)
    } else {
        Ok(())
    }
}
