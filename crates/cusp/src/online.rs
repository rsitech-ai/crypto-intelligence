//! Bounded online structural inference from one frozen offline cusp fit.

use domain::AssetId;
use feature_registry::{QualityRequirement, QualityScore};
use quality::SourceHealthState;
use serde::Serialize;
use thiserror::Error;

use crate::{
    ClassifiedRoot, ControlCovariance, ControlError, ControlMap, ControlVector, Controls,
    CuspError, EquilibriumSet, EquilibriumTopology, FeatureCovariance, FeatureSensitivity,
    FoldDistance, MissingControlFeature, Stability, analyze_equilibria,
    branch_tracker::{
        BranchId, BranchObservation, BranchTracker, BranchTrackerConfig, BranchTrackerError,
        HysteresisState,
    },
    evidence::{EvidenceBuilder, control_missing_reason_tag},
    fit::{FitError, FitResult},
    fold_distance::{ControlWhitening, nearest_fold},
    uncertainty::{DeterministicNormal, PosteriorParameterSamples, UncertaintyQuality, splitmix64},
};

const MAX_ONLINE_POSTERIOR_SAMPLES: usize = 2_048;
const MIN_ONLINE_POSTERIOR_SAMPLES: usize = 32;
pub const ONLINE_CONFIG_SCHEMA_VERSION: u32 = 1;
pub const CUSP_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

/// Approved structural inference cadence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuralCadence {
    FiveMinutes,
    FifteenMinutes,
}

impl StructuralCadence {
    pub const fn nanoseconds(self) -> i64 {
        match self {
            Self::FiveMinutes => 300_000_000_000,
            Self::FifteenMinutes => 900_000_000_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OnlineConfigInput {
    pub schema_version: u32,
    pub cadence: StructuralCadence,
    pub feature_uncertainty_seed: u64,
    pub branch: BranchTrackerConfig,
    pub production_quality: QualityRequirement,
    pub research_quality: QualityRequirement,
}

/// Versioned online policy. Task 10 still controls production inclusion.
#[derive(Clone, Debug, PartialEq)]
pub struct OnlineConfig {
    schema_version: u32,
    cadence: StructuralCadence,
    feature_uncertainty_seed: u64,
    branch: BranchTrackerConfig,
    production_quality: QualityRequirement,
    research_quality: QualityRequirement,
}

impl OnlineConfig {
    pub fn try_new(input: OnlineConfigInput) -> Result<Self, OnlineError> {
        if input.schema_version != ONLINE_CONFIG_SCHEMA_VERSION
            || input.production_quality.minimum_score() < input.research_quality.minimum_score()
            || input.production_quality.minimum_coverage()
                < input.research_quality.minimum_coverage()
            || input
                .production_quality
                .allowed_source_states()
                .iter()
                .any(|state| !input.research_quality.allows(*state))
        {
            return Err(OnlineError::InvalidConfig);
        }
        Ok(Self {
            schema_version: input.schema_version,
            cadence: input.cadence,
            feature_uncertainty_seed: input.feature_uncertainty_seed,
            branch: input.branch,
            production_quality: input.production_quality,
            research_quality: input.research_quality,
        })
    }

    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn cadence(&self) -> StructuralCadence {
        self.cadence
    }

    pub const fn feature_uncertainty_seed(&self) -> u64 {
        self.feature_uncertainty_seed
    }

    pub const fn branch(&self) -> BranchTrackerConfig {
        self.branch
    }
}

/// Point-in-time quality evidence supplied independently of the cusp model.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct StructuralQuality {
    score: QualityScore,
    coverage: QualityScore,
    source_state: SourceHealthState,
}

impl StructuralQuality {
    pub const fn new(
        score: QualityScore,
        coverage: QualityScore,
        source_state: SourceHealthState,
    ) -> Self {
        Self {
            score,
            coverage,
            source_state,
        }
    }

    pub const fn score(self) -> QualityScore {
        self.score
    }

    pub const fn coverage(self) -> QualityScore {
        self.coverage
    }

    pub const fn source_state(self) -> SourceHealthState {
        self.source_state
    }

    fn meets(self, requirement: &QualityRequirement) -> bool {
        self.score >= requirement.minimum_score()
            && self.coverage >= requirement.minimum_coverage()
            && requirement.allows(self.source_state)
    }
}

/// Exact current observation consumed by one structural update.
#[derive(Clone, Debug, PartialEq)]
pub struct StructuralInput {
    as_of_ns: i64,
    normalized_state: f64,
    asset: AssetId,
    features: ControlVector,
    feature_covariance: Option<FeatureCovariance>,
    quality: StructuralQuality,
}

impl StructuralInput {
    pub fn try_new(
        as_of_ns: i64,
        normalized_state: f64,
        asset: AssetId,
        features: ControlVector,
        feature_covariance: Option<FeatureCovariance>,
        quality: StructuralQuality,
    ) -> Result<Self, OnlineError> {
        if as_of_ns <= 0 || !normalized_state.is_finite() {
            return Err(OnlineError::InvalidInput);
        }
        Ok(Self {
            as_of_ns,
            normalized_state,
            asset,
            features,
            feature_covariance,
            quality,
        })
    }

    pub const fn as_of_ns(&self) -> i64 {
        self.as_of_ns
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailabilityReason {
    QualityRejected,
    RequiredFeatureMissing,
}

/// Research-surface availability; this is not the Task 10 production gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum SnapshotAvailability {
    ResearchAvailable,
    ResearchDegraded,
    Unavailable(UnavailabilityReason),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BranchStructuralState {
    pub branch: BranchId,
    pub root: f64,
    pub barrier: Option<f64>,
    pub restoring_force: f64,
}

/// One coefficient-plus-feature uncertainty draw after structural analysis.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StructuralSample {
    pub controls: Controls,
    pub discriminant: f64,
    pub inside_cusp: bool,
    pub fold_distance: f64,
    pub topology: EquilibriumTopology,
    pub equilibria: EquilibriumSet,
    pub branches: Vec<BranchStructuralState>,
}

/// Complete quality-aware structural output for one cadence point.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CuspSnapshot {
    schema_version: u32,
    as_of_ns: i64,
    asset: AssetId,
    normalized_state: f64,
    model_evidence_hash: [u8; 32],
    quality: StructuralQuality,
    controls: Option<Controls>,
    cusp_region_probability: Option<f64>,
    signed_discriminant: Option<f64>,
    standardized_discriminant: Option<f64>,
    fold_distance: Option<FoldDistance>,
    equilibria: Option<EquilibriumSet>,
    most_likely_branch: Option<BranchId>,
    branch_probabilities: Vec<(BranchId, f64)>,
    minimum_barrier: Option<f64>,
    restoring_force: Option<f64>,
    hysteresis: Option<HysteresisState>,
    sensitivities: Vec<FeatureSensitivity>,
    missing_optional: Vec<MissingControlFeature>,
    posterior_samples: Vec<StructuralSample>,
    uncertainty_quality: UncertaintyQuality,
    availability: SnapshotAvailability,
    evidence_hash: [u8; 32],
}

impl CuspSnapshot {
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn as_of_ns(&self) -> i64 {
        self.as_of_ns
    }

    pub const fn asset(&self) -> &AssetId {
        &self.asset
    }

    pub const fn normalized_state(&self) -> f64 {
        self.normalized_state
    }

    pub const fn model_evidence_hash(&self) -> [u8; 32] {
        self.model_evidence_hash
    }

    pub const fn quality(&self) -> StructuralQuality {
        self.quality
    }

    pub const fn controls(&self) -> Option<Controls> {
        self.controls
    }

    pub const fn cusp_region_probability(&self) -> Option<f64> {
        self.cusp_region_probability
    }

    pub const fn signed_discriminant(&self) -> Option<f64> {
        self.signed_discriminant
    }

    pub const fn standardized_discriminant(&self) -> Option<f64> {
        self.standardized_discriminant
    }

    pub const fn fold_distance(&self) -> Option<&FoldDistance> {
        self.fold_distance.as_ref()
    }

    pub const fn equilibria(&self) -> Option<&EquilibriumSet> {
        self.equilibria.as_ref()
    }

    pub const fn most_likely_branch(&self) -> Option<BranchId> {
        self.most_likely_branch
    }

    pub fn branch_probabilities(&self) -> &[(BranchId, f64)] {
        &self.branch_probabilities
    }

    pub const fn minimum_barrier(&self) -> Option<f64> {
        self.minimum_barrier
    }

    pub const fn restoring_force(&self) -> Option<f64> {
        self.restoring_force
    }

    pub const fn hysteresis(&self) -> Option<HysteresisState> {
        self.hysteresis
    }

    pub fn sensitivities(&self) -> &[FeatureSensitivity] {
        &self.sensitivities
    }

    pub fn missing_optional(&self) -> &[MissingControlFeature] {
        &self.missing_optional
    }

    pub fn posterior_samples(&self) -> &[StructuralSample] {
        &self.posterior_samples
    }

    pub const fn uncertainty_quality(&self) -> UncertaintyQuality {
        self.uncertainty_quality
    }

    pub const fn availability(&self) -> SnapshotAvailability {
        self.availability
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

/// Frozen online engine. No update path owns an optimizer or fit method.
#[derive(Clone, Debug, PartialEq)]
pub struct CuspEngine {
    fit: FitResult,
    parameter_samples: PosteriorParameterSamples,
    sample_maps: Vec<ControlMap>,
    whitening_covariance: ControlCovariance,
    whitening: ControlWhitening,
    config: OnlineConfig,
    branch_tracker: BranchTracker,
    last_as_of_ns: Option<i64>,
    model_evidence_hash: [u8; 32],
}

impl CuspEngine {
    pub fn try_new(
        fit: FitResult,
        parameter_samples: PosteriorParameterSamples,
        whitening_covariance: ControlCovariance,
        config: OnlineConfig,
        initial_branch: Option<BranchId>,
    ) -> Result<Self, OnlineError> {
        let sample_count = parameter_samples.samples().len();
        if !(MIN_ONLINE_POSTERIOR_SAMPLES..=MAX_ONLINE_POSTERIOR_SAMPLES).contains(&sample_count) {
            return Err(OnlineError::SampleCapacity);
        }
        if !fit.diagnostics().converged()
            || parameter_samples.dataset_manifest_hash() != fit.dataset_manifest_hash()
            || parameter_samples.training_fold_hash() != fit.training_fold_hash()
            || parameter_samples.mode() != fit.parameter_values()
            || parameter_samples.parameter_keys() != fit.parameter_keys()
            || parameter_samples.quality() == UncertaintyQuality::Unavailable
            || parameter_samples.digest().iter().all(|byte| *byte == 0)
            || parameter_samples
                .laplace_evidence_digest()
                .iter()
                .all(|byte| *byte == 0)
        {
            return Err(OnlineError::EvidenceMismatch);
        }
        let whitening = ControlWhitening::from_covariance(
            whitening_covariance.alpha_variance,
            whitening_covariance.alpha_beta_covariance,
            whitening_covariance.beta_variance,
        )?;
        let sample_maps = parameter_samples
            .samples()
            .iter()
            .map(|parameters| fit.control_map_for_parameters(parameters))
            .collect::<Result<Vec<_>, FitError>>()?;
        let branch_tracker = BranchTracker::try_new(config.branch, initial_branch)?;
        let model_evidence_hash =
            frozen_model_evidence(&fit, &parameter_samples, whitening_covariance, &config)?;
        Ok(Self {
            fit,
            parameter_samples,
            sample_maps,
            whitening_covariance,
            whitening,
            config,
            branch_tracker,
            last_as_of_ns: None,
            model_evidence_hash,
        })
    }

    pub fn frozen_parameters(&self) -> &[f64] {
        self.fit.parameter_values()
    }

    pub const fn model_evidence_hash(&self) -> [u8; 32] {
        self.model_evidence_hash
    }

    pub fn update(&mut self, input: StructuralInput) -> Result<CuspSnapshot, OnlineError> {
        self.validate_cadence(input.as_of_ns)?;
        let prior_branch_evidence = self.branch_tracker.evidence_digest();
        if !input.quality.meets(&self.config.research_quality) {
            return self.unavailable(
                input,
                UnavailabilityReason::QualityRejected,
                prior_branch_evidence,
            );
        }

        let central = match self.fit.control_map().evaluate_detailed(
            &input.asset,
            &input.features,
            input.feature_covariance.as_ref(),
        ) {
            Ok(evaluation) => evaluation,
            Err(ControlError::RequiredFeatureMissing { .. }) => {
                return self.unavailable(
                    input,
                    UnavailabilityReason::RequiredFeatureMissing,
                    prior_branch_evidence,
                );
            }
            Err(error) => return Err(OnlineError::Control(error)),
        };
        let availability = if self.parameter_samples.quality()
            == UncertaintyQuality::ProductionCandidate
            && input.quality.meets(&self.config.production_quality)
            && input.feature_covariance.is_some()
            && central.missing_optional.is_empty()
        {
            SnapshotAvailability::ResearchAvailable
        } else {
            SnapshotAvailability::ResearchDegraded
        };

        let central_equilibria = analyze_equilibria(central.controls)?;
        let central_fold = nearest_fold(central.controls, &self.whitening)?;
        let seed = feature_noise_seed(
            self.config.feature_uncertainty_seed,
            self.parameter_samples.seed(),
            input.as_of_ns,
            &input.asset,
        );
        let mut normal = DeterministicNormal::new(seed);
        let mut posterior_samples = Vec::with_capacity(self.sample_maps.len());
        let mut observations = Vec::with_capacity(self.sample_maps.len());
        for map in &self.sample_maps {
            let evaluation = map.evaluate_detailed(
                &input.asset,
                &input.features,
                input.feature_covariance.as_ref(),
            )?;
            let covariance = evaluation
                .propagated_covariance
                .unwrap_or_else(|| map.residual_covariance());
            let sampled_controls =
                sample_correlated_controls(evaluation.controls, covariance, &mut normal)?;
            let (structural, observation) = structural_sample(
                sampled_controls,
                &self.whitening,
                self.branch_tracker.most_likely(),
            )?;
            observations.push(observation);
            posterior_samples.push(structural);
        }
        let mut next_branch_tracker = self.branch_tracker.clone();
        let branch =
            next_branch_tracker.update(input.as_of_ns, input.normalized_state, &observations)?;
        let discriminants = posterior_samples
            .iter()
            .map(|sample| sample.discriminant)
            .collect::<Vec<_>>();
        let cusp_region_probability = posterior_samples
            .iter()
            .filter(|sample| sample.inside_cusp)
            .count() as f64
            / posterior_samples.len() as f64;
        let signed_discriminant = central.controls.checked_discriminant()?;
        let standardized_discriminant =
            standardized_value(signed_discriminant, discriminants.as_slice());
        let central_branches =
            branch_states(&central_equilibria, Some(branch.most_likely_branch()))?;
        let selected =
            selected_branch_state(central_branches.as_slice(), branch.most_likely_branch());

        let mut snapshot = CuspSnapshot {
            schema_version: CUSP_SNAPSHOT_SCHEMA_VERSION,
            as_of_ns: input.as_of_ns,
            asset: input.asset.clone(),
            normalized_state: input.normalized_state,
            model_evidence_hash: self.model_evidence_hash,
            quality: input.quality,
            controls: Some(central.controls),
            cusp_region_probability: Some(cusp_region_probability),
            signed_discriminant: Some(signed_discriminant),
            standardized_discriminant,
            fold_distance: Some(central_fold),
            equilibria: Some(central_equilibria),
            most_likely_branch: Some(branch.most_likely_branch()),
            branch_probabilities: branch.probabilities().to_vec(),
            minimum_barrier: selected.and_then(|value| value.barrier),
            restoring_force: selected.map(|value| value.restoring_force),
            hysteresis: Some(branch.hysteresis()),
            sensitivities: central.sensitivities,
            missing_optional: central.missing_optional,
            posterior_samples,
            uncertainty_quality: self.parameter_samples.quality(),
            availability,
            evidence_hash: [0; 32],
        };
        snapshot.evidence_hash = self.snapshot_evidence(
            prior_branch_evidence,
            next_branch_tracker.evidence_digest(),
            &input,
            &snapshot,
        );
        self.branch_tracker = next_branch_tracker;
        self.last_as_of_ns = Some(input.as_of_ns);
        Ok(snapshot)
    }

    fn validate_cadence(&self, as_of_ns: i64) -> Result<(), OnlineError> {
        let cadence = self.config.cadence.nanoseconds();
        if as_of_ns <= 0
            || as_of_ns % cadence != 0
            || self
                .last_as_of_ns
                .is_some_and(|previous| as_of_ns - previous < cadence)
        {
            Err(OnlineError::Cadence)
        } else {
            Ok(())
        }
    }

    fn unavailable(
        &mut self,
        input: StructuralInput,
        reason: UnavailabilityReason,
        prior_branch_evidence: [u8; 32],
    ) -> Result<CuspSnapshot, OnlineError> {
        let mut snapshot = CuspSnapshot {
            schema_version: CUSP_SNAPSHOT_SCHEMA_VERSION,
            as_of_ns: input.as_of_ns,
            asset: input.asset.clone(),
            normalized_state: input.normalized_state,
            model_evidence_hash: self.model_evidence_hash,
            quality: input.quality,
            controls: None,
            cusp_region_probability: None,
            signed_discriminant: None,
            standardized_discriminant: None,
            fold_distance: None,
            equilibria: None,
            most_likely_branch: None,
            branch_probabilities: Vec::new(),
            minimum_barrier: None,
            restoring_force: None,
            hysteresis: None,
            sensitivities: Vec::new(),
            missing_optional: Vec::new(),
            posterior_samples: Vec::new(),
            uncertainty_quality: self.parameter_samples.quality(),
            availability: SnapshotAvailability::Unavailable(reason),
            evidence_hash: [0; 32],
        };
        snapshot.evidence_hash = self.snapshot_evidence(
            prior_branch_evidence,
            self.branch_tracker.evidence_digest(),
            &input,
            &snapshot,
        );
        self.last_as_of_ns = Some(input.as_of_ns);
        Ok(snapshot)
    }
}

fn frozen_model_evidence(
    fit: &FitResult,
    parameter_samples: &PosteriorParameterSamples,
    whitening_covariance: ControlCovariance,
    config: &OnlineConfig,
) -> Result<[u8; 32], OnlineError> {
    let mut evidence = EvidenceBuilder::new(b"cusp-frozen-online-model-v1");
    hash_config(&mut evidence, config);
    let control_map =
        serde_json::to_vec(fit.control_map()).map_err(|_| OnlineError::EvidenceEncoding)?;
    evidence.bytes(&control_map);
    evidence.digest(fit.dataset_manifest_hash());
    evidence.digest(fit.training_fold_hash());
    evidence.digest(parameter_samples.laplace_evidence_digest());
    evidence.digest(parameter_samples.digest());
    evidence.usize(parameter_samples.samples().len());
    evidence.f64(whitening_covariance.alpha_variance);
    evidence.f64(whitening_covariance.alpha_beta_covariance);
    evidence.f64(whitening_covariance.beta_variance);
    Ok(evidence.finish())
}

fn sample_correlated_controls(
    center: Controls,
    covariance: ControlCovariance,
    normal: &mut DeterministicNormal,
) -> Result<Controls, OnlineError> {
    let alpha_scale = covariance.alpha_variance.sqrt();
    let beta_loading = covariance.alpha_beta_covariance / alpha_scale;
    let beta_residual_variance = beta_loading.mul_add(-beta_loading, covariance.beta_variance);
    if !alpha_scale.is_finite()
        || alpha_scale <= 0.0
        || !beta_loading.is_finite()
        || !beta_residual_variance.is_finite()
        || beta_residual_variance <= 0.0
    {
        return Err(OnlineError::NonFinite);
    }
    let first = normal.next();
    let second = normal.next();
    Controls::try_new(
        alpha_scale.mul_add(first, center.alpha),
        beta_residual_variance
            .sqrt()
            .mul_add(second, beta_loading.mul_add(first, center.beta)),
    )
    .map_err(OnlineError::Cusp)
}

fn structural_sample(
    controls: Controls,
    whitening: &ControlWhitening,
    preferred_branch: Option<BranchId>,
) -> Result<(StructuralSample, BranchObservation), OnlineError> {
    let equilibria = analyze_equilibria(controls)?;
    let discriminant = controls.checked_discriminant()?;
    let fold_distance = nearest_fold(controls, whitening)?.distance;
    let branches = branch_states(&equilibria, preferred_branch)?;
    let observation = branch_observation(&equilibria)?;
    Ok((
        StructuralSample {
            controls,
            discriminant,
            inside_cusp: controls.inside_cusp(),
            fold_distance,
            topology: equilibria.topology,
            equilibria,
            branches,
        },
        observation,
    ))
}

fn branch_states(
    equilibria: &EquilibriumSet,
    preferred_branch: Option<BranchId>,
) -> Result<Vec<BranchStructuralState>, OnlineError> {
    let stable = equilibria
        .roots
        .iter()
        .filter(|root| root.stability == Stability::Stable)
        .collect::<Vec<_>>();
    stable.iter().try_fold(Vec::new(), |mut states, root| {
        for branch in physical_branches(root, stable.as_slice(), preferred_branch)? {
            let barrier = equilibria
                .barriers
                .iter()
                .find(|barrier| barrier.stable_root == root.equilibrium.value)
                .map(|barrier| barrier.height);
            states.push(BranchStructuralState {
                branch,
                root: root.equilibrium.value,
                barrier,
                restoring_force: root.restoring_force.ok_or(OnlineError::NoStableBranch)?,
            });
        }
        Ok(states)
    })
}

fn selected_branch_state(
    states: &[BranchStructuralState],
    selected: BranchId,
) -> Option<&BranchStructuralState> {
    states.iter().find(|candidate| candidate.branch == selected)
}

fn physical_branches(
    root: &ClassifiedRoot,
    stable: &[&ClassifiedRoot],
    preferred_branch: Option<BranchId>,
) -> Result<Vec<BranchId>, OnlineError> {
    match stable {
        [lower, upper] => Ok(vec![if root.equilibrium.value == lower.equilibrium.value {
            BranchId::Lower
        } else if root.equilibrium.value == upper.equilibrium.value {
            BranchId::Upper
        } else {
            return Err(OnlineError::NoStableBranch);
        }]),
        [_] if root.equilibrium.value < 0.0 => Ok(vec![BranchId::Lower]),
        [_] if root.equilibrium.value > 0.0 => Ok(vec![BranchId::Upper]),
        [_] => Ok(preferred_branch.map_or_else(
            || vec![BranchId::Lower, BranchId::Upper],
            |branch| vec![branch],
        )),
        _ => Err(OnlineError::NoStableBranch),
    }
}

fn branch_observation(equilibria: &EquilibriumSet) -> Result<BranchObservation, OnlineError> {
    let mut roots = equilibria
        .roots
        .iter()
        .filter(|root| root.stability == Stability::Stable)
        .map(|root| root.equilibrium.value)
        .collect::<Vec<_>>();
    if roots.is_empty() && equilibria.topology == EquilibriumTopology::Critical {
        roots.extend(equilibria.roots.iter().map(|root| root.equilibrium.value));
    }
    BranchObservation::try_new(roots).map_err(OnlineError::Branch)
}

fn standardized_value(value: f64, samples: &[f64]) -> Option<f64> {
    if samples.len() < 2 || samples.iter().any(|sample| !sample.is_finite()) {
        return None;
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / (samples.len() - 1) as f64;
    let standard_deviation = variance.sqrt();
    let standardized = value / standard_deviation;
    (standard_deviation.is_finite() && standard_deviation > 0.0 && standardized.is_finite())
        .then_some(standardized)
}

fn feature_noise_seed(base_seed: u64, sample_seed: u64, as_of_ns: i64, asset: &AssetId) -> u64 {
    let mut evidence = EvidenceBuilder::new(b"cusp-online-feature-noise-v1");
    evidence.u64(base_seed);
    evidence.u64(sample_seed);
    evidence.i64(as_of_ns);
    evidence.asset(asset);
    let digest = evidence.finish();
    let mut prefix = [0_u8; 8];
    prefix.copy_from_slice(&digest[..8]);
    splitmix64(u64::from_le_bytes(prefix))
}

impl CuspEngine {
    fn snapshot_evidence(
        &self,
        prior_branch_evidence: [u8; 32],
        posterior_branch_evidence: [u8; 32],
        input: &StructuralInput,
        snapshot: &CuspSnapshot,
    ) -> [u8; 32] {
        let mut evidence = EvidenceBuilder::new(b"cusp-online-snapshot-v1");
        hash_config(&mut evidence, &self.config);
        evidence.digest(self.fit.dataset_manifest_hash());
        evidence.digest(self.fit.training_fold_hash());
        evidence.digest(self.parameter_samples.laplace_evidence_digest());
        evidence.digest(self.parameter_samples.digest());
        evidence.usize(self.parameter_samples.samples().len());
        evidence.f64(self.whitening_covariance.alpha_variance);
        evidence.f64(self.whitening_covariance.alpha_beta_covariance);
        evidence.f64(self.whitening_covariance.beta_variance);
        evidence.digest(prior_branch_evidence);
        evidence.digest(posterior_branch_evidence);
        evidence.i64(input.as_of_ns);
        evidence.f64(input.normalized_state);
        evidence.asset(&input.asset);
        evidence.control_vector(&input.features);
        hash_quality(&mut evidence, input.quality);
        match &input.feature_covariance {
            Some(covariance) => {
                evidence.boolean(true);
                evidence.usize(covariance.order().len());
                for key in covariance.order() {
                    evidence.feature_key(key);
                }
                for row in covariance.matrix() {
                    for value in row {
                        evidence.f64(*value);
                    }
                }
            }
            None => evidence.boolean(false),
        }
        hash_snapshot_outputs(&mut evidence, snapshot);
        evidence.finish()
    }
}

fn hash_config(evidence: &mut EvidenceBuilder, config: &OnlineConfig) {
    evidence.u32(config.schema_version);
    evidence.i64(config.cadence.nanoseconds());
    evidence.u64(config.feature_uncertainty_seed);
    evidence.f64(config.branch.emission_scale());
    evidence.f64(config.branch.transition_probability());
    evidence.f64(config.branch.switch_probability());
    hash_requirement(evidence, &config.production_quality);
    hash_requirement(evidence, &config.research_quality);
}

fn hash_requirement(evidence: &mut EvidenceBuilder, requirement: &QualityRequirement) {
    evidence.u32(requirement.minimum_score().millionths());
    evidence.u32(requirement.minimum_coverage().millionths());
    evidence.usize(requirement.allowed_source_states().len());
    for state in requirement.allowed_source_states() {
        evidence.u8(source_state_tag(*state));
    }
}

fn hash_quality(evidence: &mut EvidenceBuilder, quality: StructuralQuality) {
    evidence.u32(quality.score.millionths());
    evidence.u32(quality.coverage.millionths());
    evidence.u8(source_state_tag(quality.source_state));
}

fn hash_snapshot_outputs(evidence: &mut EvidenceBuilder, snapshot: &CuspSnapshot) {
    evidence.u32(snapshot.schema_version);
    evidence.digest(snapshot.model_evidence_hash);
    hash_quality(evidence, snapshot.quality);
    evidence.u8(availability_tag(snapshot.availability));
    hash_optional_controls(evidence, snapshot.controls);
    hash_optional_f64(evidence, snapshot.cusp_region_probability);
    hash_optional_f64(evidence, snapshot.signed_discriminant);
    hash_optional_f64(evidence, snapshot.standardized_discriminant);
    match &snapshot.fold_distance {
        Some(fold) => {
            evidence.boolean(true);
            evidence.f64(fold.distance);
            evidence.f64(fold.nearest.alpha);
            evidence.f64(fold.nearest.beta);
            evidence.f64(fold.fold_parameter);
            evidence.boolean(fold.diagnostics.converged);
            evidence.u32(fold.diagnostics.iterations);
            evidence.u32(fold.diagnostics.evaluations);
            evidence.f64(fold.diagnostics.objective);
            evidence.u32(fold.diagnostics.schema_version);
            evidence.bytes(fold.diagnostics.method.as_bytes());
            hash_optional_f64(evidence, fold.diagnostics.gradient_norm);
            hash_optional_f64(evidence, fold.diagnostics.condition_number);
            evidence.f64(fold.diagnostics.requested_tolerance);
            evidence.bytes(fold.diagnostics.termination.as_bytes());
            hash_optional_u64(evidence, fold.diagnostics.deterministic_seed);
            evidence.f64(fold.covariance_conditioning.minimum_eigenvalue);
            evidence.f64(fold.covariance_conditioning.maximum_eigenvalue);
            evidence.f64(fold.covariance_conditioning.condition_number);
        }
        None => evidence.boolean(false),
    }
    match &snapshot.equilibria {
        Some(equilibria) => {
            evidence.boolean(true);
            hash_equilibria(evidence, equilibria);
        }
        None => evidence.boolean(false),
    }
    evidence.u8(snapshot.most_likely_branch.map_or(0, branch_tag));
    evidence.usize(snapshot.branch_probabilities.len());
    for (branch, probability) in &snapshot.branch_probabilities {
        evidence.u8(branch_tag(*branch));
        evidence.f64(*probability);
    }
    hash_optional_f64(evidence, snapshot.minimum_barrier);
    hash_optional_f64(evidence, snapshot.restoring_force);
    evidence.u8(snapshot.hysteresis.map_or(0, hysteresis_tag));
    evidence.usize(snapshot.sensitivities.len());
    for sensitivity in &snapshot.sensitivities {
        evidence.feature_key(&sensitivity.key);
        evidence.f64(sensitivity.alpha);
        evidence.f64(sensitivity.beta);
    }
    evidence.usize(snapshot.missing_optional.len());
    for missing in &snapshot.missing_optional {
        evidence.feature_key(&missing.key);
        evidence.u8(control_missing_reason_tag(missing.reason));
    }
    evidence.usize(snapshot.posterior_samples.len());
    for sample in &snapshot.posterior_samples {
        evidence.f64(sample.controls.alpha);
        evidence.f64(sample.controls.beta);
        evidence.f64(sample.discriminant);
        evidence.boolean(sample.inside_cusp);
        evidence.f64(sample.fold_distance);
        evidence.u8(topology_tag(sample.topology));
        hash_equilibria(evidence, &sample.equilibria);
        evidence.usize(sample.branches.len());
        for branch in &sample.branches {
            evidence.u8(branch_tag(branch.branch));
            evidence.f64(branch.root);
            hash_optional_f64(evidence, branch.barrier);
            evidence.f64(branch.restoring_force);
        }
    }
    evidence.u8(uncertainty_quality_tag(snapshot.uncertainty_quality));
}

fn hash_equilibria(evidence: &mut EvidenceBuilder, equilibria: &EquilibriumSet) {
    evidence.f64(equilibria.controls.alpha);
    evidence.f64(equilibria.controls.beta);
    evidence.u8(topology_tag(equilibria.topology));
    evidence.usize(equilibria.roots.len());
    for root in &equilibria.roots {
        evidence.f64(root.equilibrium.value);
        evidence.u8(root.equilibrium.multiplicity);
        evidence.f64(root.equilibrium.residual);
        evidence.f64(root.equilibrium.condition_proxy);
        evidence.u8(stability_tag(root.stability));
        evidence.f64(root.hessian);
        hash_optional_f64(evidence, root.restoring_force);
    }
    evidence.usize(equilibria.barriers.len());
    for barrier in &equilibria.barriers {
        evidence.f64(barrier.stable_root);
        evidence.f64(barrier.unstable_root);
        evidence.f64(barrier.height);
    }
}

fn hash_optional_controls(evidence: &mut EvidenceBuilder, controls: Option<Controls>) {
    match controls {
        Some(value) => {
            evidence.boolean(true);
            evidence.f64(value.alpha);
            evidence.f64(value.beta);
        }
        None => evidence.boolean(false),
    }
}

fn hash_optional_f64(evidence: &mut EvidenceBuilder, value: Option<f64>) {
    match value {
        Some(number) => {
            evidence.boolean(true);
            evidence.f64(number);
        }
        None => evidence.boolean(false),
    }
}

fn hash_optional_u64(evidence: &mut EvidenceBuilder, value: Option<u64>) {
    match value {
        Some(number) => {
            evidence.boolean(true);
            evidence.u64(number);
        }
        None => evidence.boolean(false),
    }
}

const fn source_state_tag(state: SourceHealthState) -> u8 {
    match state {
        SourceHealthState::Healthy => 1,
        SourceHealthState::Degraded => 2,
        SourceHealthState::Unhealthy => 3,
        SourceHealthState::Quarantined => 4,
        SourceHealthState::Recovering => 5,
    }
}

const fn branch_tag(branch: BranchId) -> u8 {
    match branch {
        BranchId::Lower => 1,
        BranchId::Upper => 2,
    }
}

const fn hysteresis_tag(state: HysteresisState) -> u8 {
    match state {
        HysteresisState::FollowingLower => 1,
        HysteresisState::FollowingUpper => 2,
        HysteresisState::JumpedLowerToUpper => 3,
        HysteresisState::JumpedUpperToLower => 4,
    }
}

const fn topology_tag(topology: EquilibriumTopology) -> u8 {
    match topology {
        EquilibriumTopology::OneStable => 1,
        EquilibriumTopology::ThreeBranches => 2,
        EquilibriumTopology::Fold => 3,
        EquilibriumTopology::Critical => 4,
    }
}

const fn stability_tag(stability: Stability) -> u8 {
    match stability {
        Stability::Stable => 1,
        Stability::Unstable => 2,
        Stability::NeutralAtTolerance => 3,
    }
}

const fn uncertainty_quality_tag(quality: UncertaintyQuality) -> u8 {
    match quality {
        UncertaintyQuality::ProductionCandidate => 1,
        UncertaintyQuality::Experimental => 2,
        UncertaintyQuality::Unavailable => 3,
    }
}

const fn availability_tag(availability: SnapshotAvailability) -> u8 {
    match availability {
        SnapshotAvailability::ResearchAvailable => 1,
        SnapshotAvailability::ResearchDegraded => 2,
        SnapshotAvailability::Unavailable(UnavailabilityReason::QualityRejected) => 3,
        SnapshotAvailability::Unavailable(UnavailabilityReason::RequiredFeatureMissing) => 4,
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum OnlineError {
    #[error("invalid online cusp configuration")]
    InvalidConfig,
    #[error("online cusp input is invalid")]
    InvalidInput,
    #[error("online cusp update violates the declared structural cadence")]
    Cadence,
    #[error("online cusp posterior sample capacity is invalid")]
    SampleCapacity,
    #[error("online cusp fit and posterior evidence do not match")]
    EvidenceMismatch,
    #[error("online cusp frozen-model evidence could not be encoded")]
    EvidenceEncoding,
    #[error("online cusp posterior contains no stable branch evidence")]
    NoStableBranch,
    #[error("online cusp inference produced a nonfinite value")]
    NonFinite,
    #[error(transparent)]
    Branch(#[from] BranchTrackerError),
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error(transparent)]
    Cusp(#[from] CuspError),
    #[error(transparent)]
    Fit(#[from] FitError),
}

#[cfg(test)]
mod tests {
    use super::{BranchId, BranchStructuralState, selected_branch_state};

    #[test]
    fn central_metrics_are_not_taken_from_the_opposite_physical_branch() {
        let upper = BranchStructuralState {
            branch: BranchId::Upper,
            root: 1.0,
            barrier: None,
            restoring_force: 3.0,
        };
        assert!(selected_branch_state(&[upper], BranchId::Lower).is_none());
    }
}
