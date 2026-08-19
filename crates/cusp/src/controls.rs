use std::collections::{BTreeMap, BTreeSet};

use domain::AssetId;
use numerics::is_positive_definite;
use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ControlFeatureKey, ControlSchema, ControlTarget, ControlWhitening, Controls,
    MAX_CONTROL_FEATURES,
};

const MAX_ASSET_OVERRIDES: usize = 1_024;
const COVARIANCE_TOLERANCE: f64 = 1e-12;

#[derive(Clone, Debug, Error, PartialEq)]
pub enum ControlError {
    #[error("invalid control-map identifier")]
    InvalidIdentifier,
    #[error("invalid control-map semantic version")]
    InvalidVersion,
    #[error("invalid feature identity")]
    InvalidFeatureIdentity,
    #[error("feature is absent or not eligible for model consumption")]
    FeatureNotModelEligible,
    #[error("control schema has an invalid feature count")]
    InvalidFeatureCount,
    #[error("control schema has an invalid group count")]
    InvalidGroupCount,
    #[error("control schema contains a duplicate feature")]
    DuplicateFeature,
    #[error("control schema contains a duplicate feature group")]
    DuplicateGroup,
    #[error("control feature references an unknown group")]
    UnknownFeatureGroup,
    #[error("control feature normalization is invalid")]
    InvalidNormalization,
    #[error("control feature sparse penalty is invalid")]
    InvalidSparsePenalty,
    #[error("control input or coefficient is nonfinite")]
    NonFiniteValue,
    #[error("control vector does not exactly match the packaged schema")]
    FeatureSetMismatch,
    #[error("required control feature {feature_id} is missing")]
    RequiredFeatureMissing { feature_id: String },
    #[error("coefficient targets an undeclared control for {feature_id}")]
    CoefficientTargetMismatch { feature_id: String },
    #[error("coefficient violates its group sign constraint for {feature_id}")]
    SignConstraintViolation { feature_id: String },
    #[error("linear control contains duplicate coefficients")]
    DuplicateCoefficient,
    #[error("linear control exceeds the bounded asset-override capacity")]
    AssetOverrideCapacity,
    #[error("linear control contains a duplicate asset override")]
    DuplicateAssetOverride,
    #[error("normalization hash must be nonzero")]
    ZeroNormalizationHash,
    #[error("control map and schema versions differ")]
    VersionMismatch,
    #[error("feature covariance is malformed, unbounded, or not positive definite")]
    InvalidFeatureCovariance,
    #[error("residual or propagated control covariance is not positive definite")]
    InvalidControlCovariance,
    #[error("control evaluation produced an unrepresentable result")]
    NonFiniteEvaluation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMissingReason {
    Stale,
    InsufficientHistory,
    WindowNotFinal,
    QualityRejected,
    SourceUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum ControlDatum {
    Present(f64),
    Missing(ControlMissingReason),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlValue {
    pub key: ControlFeatureKey,
    pub datum: ControlDatum,
}

impl ControlValue {
    pub fn present(key: ControlFeatureKey, value: f64) -> Result<Self, ControlError> {
        if !value.is_finite() {
            return Err(ControlError::NonFiniteValue);
        }
        Ok(Self {
            key,
            datum: ControlDatum::Present(value),
        })
    }

    pub const fn missing(key: ControlFeatureKey, reason: ControlMissingReason) -> Self {
        Self {
            key,
            datum: ControlDatum::Missing(reason),
        }
    }
}

/// Bounded exact feature vector. Missingness is data, never a fabricated zero.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlVector {
    values: Vec<ControlValue>,
}

impl ControlVector {
    pub fn try_new(mut values: Vec<ControlValue>) -> Result<Self, ControlError> {
        if values.is_empty() || values.len() > MAX_CONTROL_FEATURES {
            return Err(ControlError::InvalidFeatureCount);
        }
        if values.iter().any(
            |value| matches!(value.datum, ControlDatum::Present(number) if !number.is_finite()),
        ) {
            return Err(ControlError::NonFiniteValue);
        }
        values.sort_by(|left, right| left.key.cmp(&right.key));
        if values.windows(2).any(|pair| pair[0].key == pair[1].key) {
            return Err(ControlError::DuplicateFeature);
        }
        Ok(Self { values })
    }

    pub fn values(&self) -> &[ControlValue] {
        &self.values
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureCoefficient {
    pub key: ControlFeatureKey,
    pub coefficient: f64,
}

impl FeatureCoefficient {
    pub fn try_new(key: ControlFeatureKey, coefficient: f64) -> Result<Self, ControlError> {
        if !coefficient.is_finite() {
            return Err(ControlError::NonFiniteValue);
        }
        Ok(Self { key, coefficient })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawLinearControl", into = "RawLinearControl")]
pub struct LinearControl {
    pub intercept: f64,
    pub shared: Vec<FeatureCoefficient>,
    pub by_asset: BTreeMap<AssetId, Vec<FeatureCoefficient>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLinearControl {
    intercept: f64,
    shared: Vec<FeatureCoefficient>,
    by_asset: Vec<AssetCoefficients>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetCoefficients {
    asset: AssetId,
    coefficients: Vec<FeatureCoefficient>,
}

impl LinearControl {
    pub fn try_new(
        intercept: f64,
        shared: Vec<FeatureCoefficient>,
        by_asset: BTreeMap<AssetId, Vec<FeatureCoefficient>>,
    ) -> Result<Self, ControlError> {
        let mut control = Self {
            intercept,
            shared,
            by_asset,
        };
        control.validate()?;
        Ok(control)
    }

    fn validate(&mut self) -> Result<(), ControlError> {
        if !self.intercept.is_finite() {
            return Err(ControlError::NonFiniteValue);
        }
        if self.by_asset.len() > MAX_ASSET_OVERRIDES {
            return Err(ControlError::AssetOverrideCapacity);
        }
        validate_coefficients(&mut self.shared)?;
        for coefficients in self.by_asset.values_mut() {
            validate_coefficients(coefficients)?;
        }
        Ok(())
    }

    fn coefficient(&self, asset: &AssetId, key: &ControlFeatureKey) -> f64 {
        coefficient_in(&self.shared, key)
            + self
                .by_asset
                .get(asset)
                .map_or(0.0, |coefficients| coefficient_in(coefficients, key))
    }
}

impl TryFrom<RawLinearControl> for LinearControl {
    type Error = ControlError;

    fn try_from(raw: RawLinearControl) -> Result<Self, Self::Error> {
        let mut by_asset = BTreeMap::new();
        for entry in raw.by_asset {
            if by_asset.insert(entry.asset, entry.coefficients).is_some() {
                return Err(ControlError::DuplicateAssetOverride);
            }
        }
        Self::try_new(raw.intercept, raw.shared, by_asset)
    }
}

impl From<LinearControl> for RawLinearControl {
    fn from(control: LinearControl) -> Self {
        Self {
            intercept: control.intercept,
            shared: control.shared,
            by_asset: control
                .by_asset
                .into_iter()
                .map(|(asset, coefficients)| AssetCoefficients {
                    asset,
                    coefficients,
                })
                .collect(),
        }
    }
}

fn validate_coefficients(coefficients: &mut [FeatureCoefficient]) -> Result<(), ControlError> {
    if coefficients.len() > MAX_CONTROL_FEATURES
        || coefficients
            .iter()
            .any(|coefficient| !coefficient.coefficient.is_finite())
    {
        return Err(ControlError::NonFiniteValue);
    }
    coefficients.sort_by(|left, right| left.key.cmp(&right.key));
    if coefficients
        .windows(2)
        .any(|pair| pair[0].key == pair[1].key)
    {
        return Err(ControlError::DuplicateCoefficient);
    }
    Ok(())
}

fn coefficient_in(coefficients: &[FeatureCoefficient], key: &ControlFeatureKey) -> f64 {
    coefficients
        .binary_search_by(|coefficient| coefficient.key.cmp(key))
        .ok()
        .map_or(0.0, |index| coefficients[index].coefficient)
}

/// Positive-definite residual or propagated covariance for `(alpha, beta)`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawControlCovariance", into = "RawControlCovariance")]
pub struct ControlCovariance {
    pub alpha_variance: f64,
    pub alpha_beta_covariance: f64,
    pub beta_variance: f64,
}

impl ControlCovariance {
    pub fn try_new(
        alpha_variance: f64,
        alpha_beta_covariance: f64,
        beta_variance: f64,
    ) -> Result<Self, ControlError> {
        ControlWhitening::from_covariance(alpha_variance, alpha_beta_covariance, beta_variance)
            .map_err(|_| ControlError::InvalidControlCovariance)?;
        Ok(Self {
            alpha_variance,
            alpha_beta_covariance,
            beta_variance,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControlCovariance {
    alpha_variance: f64,
    alpha_beta_covariance: f64,
    beta_variance: f64,
}

impl TryFrom<RawControlCovariance> for ControlCovariance {
    type Error = ControlError;

    fn try_from(raw: RawControlCovariance) -> Result<Self, Self::Error> {
        Self::try_new(
            raw.alpha_variance,
            raw.alpha_beta_covariance,
            raw.beta_variance,
        )
    }
}

impl From<ControlCovariance> for RawControlCovariance {
    fn from(covariance: ControlCovariance) -> Self {
        Self {
            alpha_variance: covariance.alpha_variance,
            alpha_beta_covariance: covariance.alpha_beta_covariance,
            beta_variance: covariance.beta_variance,
        }
    }
}

/// Strict positive-definite covariance in exact schema feature order.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureCovariance {
    order: Vec<ControlFeatureKey>,
    matrix: Vec<Vec<f64>>,
}

impl FeatureCovariance {
    pub fn try_new(
        order: Vec<ControlFeatureKey>,
        matrix: Vec<Vec<f64>>,
    ) -> Result<Self, ControlError> {
        if order.is_empty()
            || order.len() > MAX_CONTROL_FEATURES
            || matrix.len() != order.len()
            || matrix.iter().any(|row| row.len() != order.len())
            || matrix.iter().flatten().any(|value| !value.is_finite())
        {
            return Err(ControlError::InvalidFeatureCovariance);
        }
        let unique = order.iter().collect::<BTreeSet<_>>();
        if unique.len() != order.len() {
            return Err(ControlError::InvalidFeatureCovariance);
        }
        let scale = matrix
            .iter()
            .enumerate()
            .map(|(index, row)| row[index])
            .fold(0.0_f64, f64::max);
        if !scale.is_finite() || scale <= 0.0 {
            return Err(ControlError::InvalidFeatureCovariance);
        }
        let normalized = matrix
            .iter()
            .map(|row| row.iter().map(|value| value / scale).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if !is_positive_definite(&normalized, COVARIANCE_TOLERANCE)
            .map_err(|_| ControlError::InvalidFeatureCovariance)?
        {
            return Err(ControlError::InvalidFeatureCovariance);
        }
        Ok(Self { order, matrix })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FeatureSensitivity {
    pub key: ControlFeatureKey,
    pub alpha: f64,
    pub beta: f64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MissingControlFeature {
    pub key: ControlFeatureKey,
    pub reason: ControlMissingReason,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ControlEvaluation {
    pub controls: Controls,
    pub propagated_covariance: Option<ControlCovariance>,
    pub sensitivities: Vec<FeatureSensitivity>,
    pub missing_optional: Vec<MissingControlFeature>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlMapInput {
    pub version: Version,
    pub schema: ControlSchema,
    pub alpha: LinearControl,
    pub beta: LinearControl,
    pub normalization_hash: [u8; 32],
    pub residual_covariance: ControlCovariance,
}

/// Validated hierarchical alpha/beta mapping packaged with training evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ControlMapInput", into = "ControlMapInput")]
pub struct ControlMap {
    version: Version,
    schema: ControlSchema,
    alpha: LinearControl,
    beta: LinearControl,
    normalization_hash: [u8; 32],
    residual_covariance: ControlCovariance,
}

impl ControlMap {
    pub fn try_new(mut input: ControlMapInput) -> Result<Self, ControlError> {
        if input.version.major == 0 || input.version.to_string().len() > 128 {
            return Err(ControlError::InvalidVersion);
        }
        if input.version != input.schema.version {
            return Err(ControlError::VersionMismatch);
        }
        if input.normalization_hash.iter().all(|byte| *byte == 0) {
            return Err(ControlError::ZeroNormalizationHash);
        }
        input.schema.validate()?;
        input.alpha.validate()?;
        input.beta.validate()?;
        ControlCovariance::try_new(
            input.residual_covariance.alpha_variance,
            input.residual_covariance.alpha_beta_covariance,
            input.residual_covariance.beta_variance,
        )?;
        validate_mapping(&input.schema, &input.alpha, ControlTarget::Alpha)?;
        validate_mapping(&input.schema, &input.beta, ControlTarget::Beta)?;
        let penalty =
            penalty_for(&input.schema, &input.alpha) + penalty_for(&input.schema, &input.beta);
        if !penalty.is_finite() {
            return Err(ControlError::NonFiniteEvaluation);
        }
        Ok(Self {
            version: input.version,
            schema: input.schema,
            alpha: input.alpha,
            beta: input.beta,
            normalization_hash: input.normalization_hash,
            residual_covariance: input.residual_covariance,
        })
    }

    pub fn into_input(self) -> ControlMapInput {
        self.into()
    }

    pub fn evaluate(
        &self,
        asset: &AssetId,
        features: &ControlVector,
    ) -> Result<Controls, ControlError> {
        self.evaluate_detailed(asset, features, None)
            .map(|evaluation| evaluation.controls)
    }

    pub fn evaluate_detailed(
        &self,
        asset: &AssetId,
        features: &ControlVector,
        feature_covariance: Option<&FeatureCovariance>,
    ) -> Result<ControlEvaluation, ControlError> {
        if features.values.len() != self.schema.features.len()
            || features
                .values
                .iter()
                .zip(&self.schema.features)
                .any(|(value, feature)| value.key != feature.key)
        {
            return Err(ControlError::FeatureSetMismatch);
        }
        if feature_covariance.is_some_and(|covariance| {
            covariance.order.len() != self.schema.features.len()
                || covariance
                    .order
                    .iter()
                    .zip(&self.schema.features)
                    .any(|(key, feature)| key != &feature.key)
        }) {
            return Err(ControlError::FeatureSetMismatch);
        }

        let mut normalized = Vec::with_capacity(self.schema.features.len());
        let mut present = Vec::with_capacity(self.schema.features.len());
        let mut missing_optional = Vec::new();
        for (value, feature) in features.values.iter().zip(&self.schema.features) {
            match value.datum {
                ControlDatum::Present(raw) => {
                    let transformed =
                        (raw - feature.normalization_center) / feature.normalization_scale;
                    if !transformed.is_finite() {
                        return Err(ControlError::NonFiniteEvaluation);
                    }
                    normalized.push(transformed);
                    present.push(true);
                }
                ControlDatum::Missing(_) if feature.required => {
                    return Err(ControlError::RequiredFeatureMissing {
                        feature_id: feature.key.id().to_owned(),
                    });
                }
                ControlDatum::Missing(reason) => {
                    normalized.push(0.0);
                    present.push(false);
                    missing_optional.push(MissingControlFeature {
                        key: feature.key.clone(),
                        reason,
                    });
                }
            }
        }

        let alpha = evaluate_linear(&self.alpha, asset, &self.schema, normalized.as_slice())?;
        let beta = evaluate_linear(&self.beta, asset, &self.schema, normalized.as_slice())?;
        let controls =
            Controls::try_new(alpha, beta).map_err(|_| ControlError::NonFiniteEvaluation)?;
        let sensitivities = self
            .schema
            .features
            .iter()
            .map(|feature| FeatureSensitivity {
                key: feature.key.clone(),
                alpha: self.alpha.coefficient(asset, &feature.key) / feature.normalization_scale,
                beta: self.beta.coefficient(asset, &feature.key) / feature.normalization_scale,
            })
            .collect::<Vec<_>>();
        if sensitivities
            .iter()
            .any(|value| !value.alpha.is_finite() || !value.beta.is_finite())
        {
            return Err(ControlError::NonFiniteEvaluation);
        }
        let propagated_covariance = feature_covariance
            .map(|covariance| self.propagate_covariance(&sensitivities, &present, covariance))
            .transpose()?;
        Ok(ControlEvaluation {
            controls,
            propagated_covariance,
            sensitivities,
            missing_optional,
        })
    }

    pub fn sparse_penalty(&self) -> f64 {
        penalty_for(&self.schema, &self.alpha) + penalty_for(&self.schema, &self.beta)
    }

    fn propagate_covariance(
        &self,
        sensitivities: &[FeatureSensitivity],
        present: &[bool],
        covariance: &FeatureCovariance,
    ) -> Result<ControlCovariance, ControlError> {
        let mut alpha_variance = self.residual_covariance.alpha_variance;
        let mut alpha_beta_covariance = self.residual_covariance.alpha_beta_covariance;
        let mut beta_variance = self.residual_covariance.beta_variance;
        for row in 0..sensitivities.len() {
            let row_alpha = if present[row] {
                sensitivities[row].alpha
            } else {
                0.0
            };
            let row_beta = if present[row] {
                sensitivities[row].beta
            } else {
                0.0
            };
            for column in 0..sensitivities.len() {
                let column_alpha = if present[column] {
                    sensitivities[column].alpha
                } else {
                    0.0
                };
                let column_beta = if present[column] {
                    sensitivities[column].beta
                } else {
                    0.0
                };
                let value = covariance.matrix[row][column];
                alpha_variance += row_alpha * value * column_alpha;
                alpha_beta_covariance += row_alpha * value * column_beta;
                beta_variance += row_beta * value * column_beta;
            }
        }
        if !alpha_variance.is_finite()
            || !alpha_beta_covariance.is_finite()
            || !beta_variance.is_finite()
        {
            return Err(ControlError::NonFiniteEvaluation);
        }
        ControlCovariance::try_new(alpha_variance, alpha_beta_covariance, beta_variance)
    }
}

impl TryFrom<ControlMapInput> for ControlMap {
    type Error = ControlError;

    fn try_from(input: ControlMapInput) -> Result<Self, Self::Error> {
        Self::try_new(input)
    }
}

impl From<ControlMap> for ControlMapInput {
    fn from(map: ControlMap) -> Self {
        Self {
            version: map.version,
            schema: map.schema,
            alpha: map.alpha,
            beta: map.beta,
            normalization_hash: map.normalization_hash,
            residual_covariance: map.residual_covariance,
        }
    }
}

fn evaluate_linear(
    control: &LinearControl,
    asset: &AssetId,
    schema: &ControlSchema,
    normalized: &[f64],
) -> Result<f64, ControlError> {
    let value = schema.features.iter().zip(normalized).fold(
        control.intercept,
        |accumulator, (feature, value)| {
            control
                .coefficient(asset, &feature.key)
                .mul_add(*value, accumulator)
        },
    );
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ControlError::NonFiniteEvaluation)
    }
}

fn validate_mapping(
    schema: &ControlSchema,
    control: &LinearControl,
    target: ControlTarget,
) -> Result<(), ControlError> {
    for coefficients in std::iter::once(&control.shared).chain(control.by_asset.values()) {
        for coefficient in coefficients {
            let feature = schema.feature(&coefficient.key).ok_or_else(|| {
                ControlError::CoefficientTargetMismatch {
                    feature_id: coefficient.key.id().to_owned(),
                }
            })?;
            let permitted = match target {
                ControlTarget::Alpha => feature.target.permits_alpha(),
                ControlTarget::Beta => feature.target.permits_beta(),
                ControlTarget::Neither | ControlTarget::Both => false,
            };
            if !permitted {
                return Err(ControlError::CoefficientTargetMismatch {
                    feature_id: coefficient.key.id().to_owned(),
                });
            }
            let group = schema
                .group(&feature.group_id)
                .ok_or(ControlError::UnknownFeatureGroup)?;
            let sign = match target {
                ControlTarget::Alpha => group.alpha_sign,
                ControlTarget::Beta => group.beta_sign,
                ControlTarget::Neither | ControlTarget::Both => {
                    return Err(ControlError::CoefficientTargetMismatch {
                        feature_id: coefficient.key.id().to_owned(),
                    });
                }
            };
            if !sign.permits(coefficient.coefficient) {
                return Err(ControlError::SignConstraintViolation {
                    feature_id: coefficient.key.id().to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn penalty_for(schema: &ControlSchema, control: &LinearControl) -> f64 {
    std::iter::once(&control.shared)
        .chain(control.by_asset.values())
        .flat_map(|coefficients| coefficients.iter())
        .filter_map(|coefficient| {
            schema
                .feature(&coefficient.key)
                .and_then(|feature| schema.group(&feature.group_id))
                .map(|group| group.sparse_penalty * coefficient.coefficient.abs())
        })
        .sum()
}
