//! Frozen support and novelty checks for applicability decisions.

use crate::ApplicabilityError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const NOVELTY_DOMAIN: &[u8] = b"cmti:applicability-novelty:v1\0";
const MAXIMUM_FEATURES: usize = 256;
const MAXIMUM_CATEGORIES: usize = 256;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureRange {
    id: String,
    required: bool,
    minimum: f64,
    maximum: f64,
}

impl FeatureRange {
    pub fn try_new(
        id: impl Into<String>,
        required: bool,
        minimum: f64,
        maximum: f64,
    ) -> Result<Self, ApplicabilityError> {
        let value = Self {
            id: id.into(),
            required,
            minimum,
            maximum,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn required(&self) -> bool {
        self.required
    }

    pub const fn minimum(&self) -> f64 {
        self.minimum
    }

    pub const fn maximum(&self) -> f64 {
        self.maximum
    }

    fn validate(&self) -> Result<(), ApplicabilityError> {
        if valid_identifier(&self.id)
            && self.minimum.is_finite()
            && self.maximum.is_finite()
            && self.minimum < self.maximum
        {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidNoveltyProfile)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoricalSupport {
    field: String,
    values: Vec<String>,
}

impl CategoricalSupport {
    pub fn try_new<I, S>(field: impl Into<String>, values: I) -> Result<Self, ApplicabilityError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut values: Vec<String> = values.into_iter().map(Into::into).collect();
        values.sort();
        values.dedup();
        let value = Self {
            field: field.into(),
            values,
        };
        value.validate()?;
        Ok(value)
    }

    fn validate(&self) -> Result<(), ApplicabilityError> {
        if valid_identifier(&self.field)
            && (1..=MAXIMUM_CATEGORIES).contains(&self.values.len())
            && self.values.windows(2).all(|pair| pair[0] < pair[1])
            && self.values.iter().all(|value| valid_identifier(value))
        {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidNoveltyProfile)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoveltyProfile {
    schema_version: u32,
    features: Vec<FeatureRange>,
    categories: Vec<CategoricalSupport>,
    venues: Vec<String>,
    products: Vec<String>,
    evidence_hash: [u8; 32],
}

impl NoveltyProfile {
    pub fn try_new<VI, VS, PI, PS>(
        features: Vec<FeatureRange>,
        mut categories: Vec<CategoricalSupport>,
        venues: VI,
        products: PI,
    ) -> Result<Self, ApplicabilityError>
    where
        VI: IntoIterator<Item = VS>,
        VS: Into<String>,
        PI: IntoIterator<Item = PS>,
        PS: Into<String>,
    {
        categories.sort_by(|left, right| left.field.cmp(&right.field));
        let mut venues: Vec<String> = venues.into_iter().map(Into::into).collect();
        let mut products: Vec<String> = products.into_iter().map(Into::into).collect();
        venues.sort();
        venues.dedup();
        products.sort();
        products.dedup();
        let mut value = Self {
            schema_version: 1,
            features,
            categories,
            venues,
            products,
            evidence_hash: [0; 32],
        };
        value.validate_without_hash()?;
        value.evidence_hash = value.calculate_hash();
        Ok(value)
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    pub fn features(&self) -> &[FeatureRange] {
        &self.features
    }

    pub fn evaluate(
        &self,
        values: &[Option<f64>],
        categories: &[(&str, &str)],
        venue: &str,
        product: &str,
    ) -> Result<NoveltyDiagnostics, ApplicabilityError> {
        self.validate()?;
        if values.len() != self.features.len()
            || !valid_identifier(venue)
            || !valid_identifier(product)
        {
            return Err(ApplicabilityError::FeatureSchemaMismatch);
        }
        let provided: std::collections::BTreeMap<&str, &str> = categories.iter().copied().collect();
        if provided.len() != categories.len()
            || provided
                .iter()
                .any(|(field, value)| !valid_identifier(field) || !valid_identifier(value))
        {
            return Err(ApplicabilityError::InvalidRuntimeContext);
        }

        let mut diagnostics = NoveltyDiagnostics::default();
        for (definition, value) in self.features.iter().zip(values) {
            match value {
                None if definition.required => {
                    diagnostics.required_missing = true;
                    diagnostics
                        .reasons
                        .push(format!("required_feature_missing:{}", definition.id));
                }
                None => {
                    diagnostics.optional_missing = true;
                    diagnostics
                        .reasons
                        .push(format!("optional_feature_missing:{}", definition.id));
                }
                Some(value) if !value.is_finite() => {
                    return Err(ApplicabilityError::NonFiniteArithmetic);
                }
                Some(value) if *value < definition.minimum || *value > definition.maximum => {
                    diagnostics.out_of_range = true;
                    diagnostics
                        .reasons
                        .push(format!("feature_out_of_range:{}", definition.id));
                }
                Some(_) => {}
            }
        }
        for support in &self.categories {
            match provided.get(support.field.as_str()) {
                Some(value) if support.values.binary_search(&(*value).to_owned()).is_ok() => {}
                Some(_) => {
                    diagnostics.categorical_novelty = true;
                    diagnostics
                        .reasons
                        .push(format!("categorical_novelty:{}", support.field));
                }
                None => {
                    diagnostics.required_missing = true;
                    diagnostics
                        .reasons
                        .push(format!("required_category_missing:{}", support.field));
                }
            }
        }
        for field in provided.keys() {
            if self
                .categories
                .binary_search_by(|support| support.field.as_str().cmp(field))
                .is_err()
            {
                diagnostics.categorical_novelty = true;
                diagnostics
                    .reasons
                    .push(format!("categorical_novelty:{field}"));
            }
        }
        if self.venues.binary_search(&venue.to_owned()).is_err() {
            diagnostics.unsupported_venue = true;
            diagnostics
                .reasons
                .push(format!("unsupported_venue:{venue}"));
        }
        if self.products.binary_search(&product.to_owned()).is_err() {
            diagnostics.unsupported_product = true;
            diagnostics
                .reasons
                .push(format!("unsupported_product:{product}"));
        }
        Ok(diagnostics)
    }

    pub fn validate(&self) -> Result<(), ApplicabilityError> {
        self.validate_without_hash()?;
        if self.evidence_hash == self.calculate_hash() {
            Ok(())
        } else {
            Err(ApplicabilityError::InvalidArtifact)
        }
    }

    fn validate_without_hash(&self) -> Result<(), ApplicabilityError> {
        if self.schema_version != 1
            || !(1..=MAXIMUM_FEATURES).contains(&self.features.len())
            || self.features.iter().any(|value| value.validate().is_err())
            || self
                .features
                .iter()
                .map(|value| value.id.as_str())
                .collect::<BTreeSet<_>>()
                .len()
                != self.features.len()
            || self.categories.len() > MAXIMUM_CATEGORIES
            || self
                .categories
                .iter()
                .any(|value| value.validate().is_err())
            || self
                .categories
                .windows(2)
                .any(|pair| pair[0].field >= pair[1].field)
            || self.venues.is_empty()
            || self.products.is_empty()
            || self.venues.windows(2).any(|pair| pair[0] >= pair[1])
            || self.products.windows(2).any(|pair| pair[0] >= pair[1])
            || !self
                .venues
                .iter()
                .chain(&self.products)
                .all(|value| valid_identifier(value))
        {
            Err(ApplicabilityError::InvalidNoveltyProfile)
        } else {
            Ok(())
        }
    }

    fn calculate_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(NOVELTY_DOMAIN);
        hash_string(&mut hasher, &self.schema_version.to_string());
        hasher.update(&(self.features.len() as u64).to_le_bytes());
        for feature in &self.features {
            hash_string(&mut hasher, &feature.id);
            hasher.update(&[u8::from(feature.required)]);
            hasher.update(&feature.minimum.to_bits().to_le_bytes());
            hasher.update(&feature.maximum.to_bits().to_le_bytes());
        }
        hasher.update(&(self.categories.len() as u64).to_le_bytes());
        for category in &self.categories {
            hash_string(&mut hasher, &category.field);
            hasher.update(&(category.values.len() as u64).to_le_bytes());
            for value in &category.values {
                hash_string(&mut hasher, value);
            }
        }
        hasher.update(&(self.venues.len() as u64).to_le_bytes());
        for value in &self.venues {
            hash_string(&mut hasher, value);
        }
        hasher.update(&(self.products.len() as u64).to_le_bytes());
        for value in &self.products {
            hash_string(&mut hasher, value);
        }
        *hasher.finalize().as_bytes()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoveltyDiagnostics {
    pub required_missing: bool,
    pub optional_missing: bool,
    pub out_of_range: bool,
    pub categorical_novelty: bool,
    pub unsupported_venue: bool,
    pub unsupported_product: bool,
    pub reasons: Vec<String>,
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}

pub(crate) fn identifier_is_valid(value: &str) -> bool {
    valid_identifier(value)
}

fn hash_string(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}
