use cusp::{
    CoefficientSign, ControlCovariance, ControlFeature, ControlFeatureKey, ControlMissingReason,
    ControlSchema, ControlTarget, ControlValue, ControlVector, FeatureCovariance, FeatureGroup,
    branch_tracker::{
        BranchId, BranchObservation, BranchTracker, BranchTrackerConfig, HysteresisState,
    },
    fit::{EstimatorKind, FitConfig, FitDataset, FitRow, FitTimeRange, PenaltyConfig, fit},
    online::{
        CuspEngine, OnlineConfig, OnlineConfigInput, OnlineError, SnapshotAvailability,
        StructuralCadence, StructuralInput, StructuralQuality, UnavailabilityReason,
    },
    uncertainty::{LaplaceApproximation, LaplaceConfig, UncertaintyQuality},
};
use domain::{AssetId, AssetNamespace};
use feature_registry::{QualityRequirement, QualityScore};
use quality::SourceHealthState;
use semver::Version;

const BASE_TIME_NS: i64 = 1_800_000_000_000_000_000;
const STEP_NS: i64 = 60_000_000_000;

#[test]
fn physical_root_input_order_does_not_flip_the_upper_branch() {
    let mut tracker = BranchTracker::try_new(
        BranchTrackerConfig::try_new(0.5, 0.01, 0.70).expect("tracker config"),
        Some(BranchId::Upper),
    )
    .expect("tracker");
    let first = tracker
        .update(
            BASE_TIME_NS,
            0.95,
            &[
                BranchObservation::try_new(vec![-1.0, 1.0]).expect("ordered roots"),
                BranchObservation::try_new(vec![-1.1, 0.9]).expect("ordered roots"),
            ],
        )
        .expect("first update");
    let second = tracker
        .update(
            BASE_TIME_NS + 5 * STEP_NS,
            0.96,
            &[
                BranchObservation::try_new(vec![1.01, -0.99]).expect("reversed roots"),
                BranchObservation::try_new(vec![0.91, -1.09]).expect("reversed roots"),
            ],
        )
        .expect("second update");
    assert_eq!(first.most_likely_branch(), BranchId::Upper);
    assert_eq!(second.most_likely_branch(), BranchId::Upper);
    assert!(second.probability(BranchId::Upper) > 0.99);
    assert_eq!(second.hysteresis(), HysteresisState::FollowingUpper);
    assert_probability_sum(second.probabilities().as_slice());
}

#[test]
fn disappearance_at_a_true_fold_forces_a_branch_transition() {
    let mut tracker = BranchTracker::try_new(
        BranchTrackerConfig::try_new(0.4, 0.005, 0.70).expect("tracker config"),
        Some(BranchId::Lower),
    )
    .expect("tracker");
    let initial = tracker
        .update(
            BASE_TIME_NS,
            -1.0,
            &[BranchObservation::try_new(vec![-1.0, 1.0]).expect("two branches")],
        )
        .expect("initial update");
    assert_eq!(initial.most_likely_branch(), BranchId::Lower);

    let transitioned = tracker
        .update(
            BASE_TIME_NS + 5 * STEP_NS,
            1.1,
            &[BranchObservation::try_new(vec![1.2]).expect("surviving upper branch")],
        )
        .expect("fold update");
    assert_eq!(transitioned.most_likely_branch(), BranchId::Upper);
    assert_eq!(
        transitioned.hysteresis(),
        HysteresisState::JumpedLowerToUpper
    );
    assert_eq!(transitioned.probability(BranchId::Lower), 0.0);
    assert_eq!(transitioned.probability(BranchId::Upper), 1.0);
}

#[test]
fn symmetric_single_well_preserves_the_established_physical_branch() {
    let mut tracker = BranchTracker::try_new(
        BranchTrackerConfig::try_new(0.4, 0.005, 0.70).expect("tracker config"),
        Some(BranchId::Upper),
    )
    .expect("tracker");
    let update = tracker
        .update(
            BASE_TIME_NS,
            0.0,
            &[BranchObservation::try_new(vec![0.0]).expect("critical root")],
        )
        .expect("critical update");
    assert_eq!(update.most_likely_branch(), BranchId::Upper);
    assert_eq!(update.hysteresis(), HysteresisState::FollowingUpper);
    assert_eq!(update.probability(BranchId::Lower), 0.0);
    assert_eq!(update.probability(BranchId::Upper), 1.0);
}

#[test]
fn online_snapshot_is_deterministic_normalized_and_structurally_complete() {
    let (fit, samples) = fitted_posterior(64);
    let mut first_engine = CuspEngine::try_new(
        fit.clone(),
        samples.clone(),
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("whitening covariance"),
        online_config(),
        Some(BranchId::Upper),
    )
    .expect("first engine");
    let mut repeated_engine = CuspEngine::try_new(
        fit,
        samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("whitening covariance"),
        online_config(),
        Some(BranchId::Upper),
    )
    .expect("repeated engine");
    let input = available_input(BASE_TIME_NS, vector(0.25, 0.30));
    let first = first_engine.update(input.clone()).expect("first snapshot");
    let repeated = repeated_engine.update(input).expect("repeated snapshot");

    assert_eq!(first, repeated);
    assert_eq!(
        first.availability(),
        SnapshotAvailability::ResearchAvailable
    );
    assert!(first.controls().is_some());
    assert!(first.signed_discriminant().is_some());
    assert!(first.standardized_discriminant().is_some());
    assert!(first.fold_distance().is_some());
    assert!(first.equilibria().is_some());
    assert_eq!(first.posterior_samples().len(), 64);
    assert!(first.posterior_samples().iter().all(|sample| {
        sample.equilibria.controls == sample.controls
            && sample.equilibria.topology == sample.topology
            && !sample.equilibria.roots.is_empty()
    }));
    assert_eq!(
        first.uncertainty_quality(),
        UncertaintyQuality::ProductionCandidate
    );
    assert_eq!(first.sensitivities().len(), 2);
    assert!(
        first
            .cusp_region_probability()
            .is_some_and(|value| (0.0..=1.0).contains(&value))
    );
    assert_probability_sum(first.branch_probabilities());
    assert!(first.evidence_hash().iter().any(|byte| *byte != 0));
    let encoded = serde_json::to_value(&first).expect("serialize snapshot");
    assert_eq!(encoded["schema_version"], 1);
    assert_eq!(encoded["availability"]["state"], "research_available");
    assert!(encoded["availability"].get("reason").is_none());
    assert_eq!(encoded["uncertainty_quality"], "production_candidate");
    assert_eq!(encoded["most_likely_branch"], "upper");
}

#[test]
fn degraded_quality_is_research_only_and_changes_evidence() {
    let (fit, samples) = fitted_posterior(64);
    let mut available_engine = CuspEngine::try_new(
        fit.clone(),
        samples.clone(),
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("available engine");
    let mut degraded_engine = CuspEngine::try_new(
        fit,
        samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("degraded engine");
    let available = available_engine
        .update(available_input(BASE_TIME_NS, vector(0.25, 0.30)))
        .expect("available");
    let degraded = degraded_engine
        .update(
            StructuralInput::try_new(
                BASE_TIME_NS,
                0.7,
                bitcoin(),
                vector(0.25, 0.30),
                Some(feature_covariance()),
                StructuralQuality::new(
                    QualityScore::from_millionths(800_000).expect("score"),
                    QualityScore::from_millionths(850_000).expect("coverage"),
                    SourceHealthState::Degraded,
                ),
            )
            .expect("degraded input"),
        )
        .expect("degraded");
    assert_eq!(
        degraded.availability(),
        SnapshotAvailability::ResearchDegraded
    );
    assert!(degraded.controls().is_some());
    assert_ne!(available.evidence_hash(), degraded.evidence_hash());
}

#[test]
fn missing_feature_uncertainty_is_explicitly_degraded() {
    let (fit, samples) = fitted_posterior(64);
    let mut engine = CuspEngine::try_new(
        fit,
        samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("engine");
    let snapshot = engine
        .update(
            StructuralInput::try_new(
                BASE_TIME_NS,
                0.7,
                bitcoin(),
                vector(0.25, 0.30),
                None,
                StructuralQuality::new(
                    QualityScore::from_millionths(950_000).expect("score"),
                    QualityScore::from_millionths(950_000).expect("coverage"),
                    SourceHealthState::Healthy,
                ),
            )
            .expect("input"),
        )
        .expect("snapshot");
    assert_eq!(
        snapshot.availability(),
        SnapshotAvailability::ResearchDegraded
    );
    assert!(snapshot.controls().is_some());
    assert_eq!(snapshot.posterior_samples().len(), 64);
}

#[test]
fn experimental_parameter_uncertainty_cannot_be_research_available() {
    let data = transition_data();
    let fitted = fit(&data, smooth_config()).expect("converged fit");
    let experimental_config = LaplaceConfig::try_new(1.0e-8, 0.0, 1.0, 1.0, 1.0e14, 1.0e-8, 4.0)
        .expect("experimental config");
    let approximation =
        LaplaceApproximation::from_fit(&fitted, experimental_config).expect("Laplace fit");
    assert_eq!(approximation.quality(), UncertaintyQuality::Experimental);
    let samples = approximation
        .sample_parameters(73, 64)
        .expect("parameter samples");
    let mut engine = CuspEngine::try_new(
        fitted,
        samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("engine");
    let snapshot = engine
        .update(available_input(BASE_TIME_NS, vector(0.25, 0.30)))
        .expect("snapshot");
    assert_eq!(
        snapshot.uncertainty_quality(),
        UncertaintyQuality::Experimental
    );
    assert_eq!(
        snapshot.availability(),
        SnapshotAvailability::ResearchDegraded
    );
}

#[test]
fn missing_required_feature_and_rejected_quality_never_become_zero_risk() {
    let (fit, samples) = fitted_posterior(64);
    let mut missing_engine = CuspEngine::try_new(
        fit.clone(),
        samples.clone(),
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("missing engine");
    let missing = ControlVector::try_new(vec![
        ControlValue::missing(key("alpha_signal"), ControlMissingReason::QualityRejected),
        ControlValue::present(key("beta_signal"), 0.3).expect("beta"),
    ])
    .expect("missing vector");
    let unavailable = missing_engine
        .update(available_input(BASE_TIME_NS, missing))
        .expect("unavailable snapshot");
    assert_eq!(
        unavailable.availability(),
        SnapshotAvailability::Unavailable(UnavailabilityReason::RequiredFeatureMissing)
    );
    assert!(unavailable.controls().is_none());
    assert!(unavailable.cusp_region_probability().is_none());
    assert!(unavailable.fold_distance().is_none());
    assert!(unavailable.posterior_samples().is_empty());
    assert!(unavailable.branch_probabilities().is_empty());
    let encoded = serde_json::to_value(&unavailable).expect("serialize unavailable snapshot");
    assert_eq!(encoded["availability"]["state"], "unavailable");
    assert_eq!(
        encoded["availability"]["reason"],
        "required_feature_missing"
    );

    let mut quality_engine = CuspEngine::try_new(
        fit,
        samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("quality engine");
    let rejected = quality_engine
        .update(
            StructuralInput::try_new(
                BASE_TIME_NS,
                0.7,
                bitcoin(),
                vector(0.25, 0.30),
                Some(feature_covariance()),
                StructuralQuality::new(
                    QualityScore::from_millionths(600_000).expect("score"),
                    QualityScore::from_millionths(600_000).expect("coverage"),
                    SourceHealthState::Unhealthy,
                ),
            )
            .expect("rejected input"),
        )
        .expect("rejected snapshot");
    assert_eq!(
        rejected.availability(),
        SnapshotAvailability::Unavailable(UnavailabilityReason::QualityRejected)
    );
    assert!(rejected.controls().is_none());
}

#[test]
fn unavailable_updates_do_not_mutate_the_hidden_branch_posterior() {
    let (fit, samples) = fitted_posterior(64);
    let covariance = ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance");
    let mut interrupted = CuspEngine::try_new(
        fit.clone(),
        samples.clone(),
        covariance,
        online_config(),
        Some(BranchId::Upper),
    )
    .expect("interrupted engine");
    let mut uninterrupted = CuspEngine::try_new(
        fit,
        samples,
        covariance,
        online_config(),
        Some(BranchId::Upper),
    )
    .expect("uninterrupted engine");
    let rejected = StructuralInput::try_new(
        BASE_TIME_NS,
        -0.9,
        bitcoin(),
        vector(0.25, 0.30),
        Some(feature_covariance()),
        StructuralQuality::new(
            QualityScore::from_millionths(600_000).expect("score"),
            QualityScore::from_millionths(600_000).expect("coverage"),
            SourceHealthState::Unhealthy,
        ),
    )
    .expect("rejected input");
    interrupted.update(rejected).expect("unavailable snapshot");

    let as_of_ns = BASE_TIME_NS + 5 * STEP_NS;
    let after_rejection = interrupted
        .update(available_input(as_of_ns, vector(0.25, 0.30)))
        .expect("snapshot after rejection");
    let direct = uninterrupted
        .update(available_input(as_of_ns, vector(0.25, 0.30)))
        .expect("direct snapshot");
    assert_eq!(
        after_rejection.most_likely_branch(),
        direct.most_likely_branch()
    );
    assert_eq!(
        after_rejection.branch_probabilities(),
        direct.branch_probabilities()
    );
    assert_eq!(after_rejection.hysteresis(), direct.hysteresis());
    assert_eq!(
        after_rejection.posterior_samples(),
        direct.posterior_samples()
    );
}

#[test]
fn cadence_and_posterior_work_are_bounded_before_updates() {
    let (fit, samples) = fitted_posterior(64);
    let mut engine = CuspEngine::try_new(
        fit,
        samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("engine");
    assert_eq!(
        engine.update(available_input(BASE_TIME_NS + 1, vector(0.25, 0.30))),
        Err(OnlineError::Cadence)
    );
    engine
        .update(available_input(BASE_TIME_NS, vector(0.25, 0.30)))
        .expect("first cadence");
    assert_eq!(
        engine.update(available_input(
            BASE_TIME_NS + 4 * STEP_NS,
            vector(0.25, 0.30),
        )),
        Err(OnlineError::Cadence)
    );

    let (oversized_fit, oversized_samples) = fitted_posterior(2_049);
    let maximum_samples = LaplaceApproximation::from_fit(&oversized_fit, LaplaceConfig::fixture())
        .expect("maximum Laplace fit")
        .sample_parameters(73, 2_048)
        .expect("maximum samples");
    let mut maximum_engine = CuspEngine::try_new(
        oversized_fit.clone(),
        maximum_samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("maximum engine");
    let maximum_snapshot = maximum_engine
        .update(available_input(BASE_TIME_NS, vector(0.25, 0.30)))
        .expect("maximum bounded update");
    assert_eq!(maximum_snapshot.posterior_samples().len(), 2_048);
    assert!(
        maximum_snapshot
            .evidence_hash()
            .iter()
            .any(|byte| *byte != 0)
    );
    assert_eq!(
        CuspEngine::try_new(
            oversized_fit,
            oversized_samples,
            ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
            online_config(),
            None,
        ),
        Err(OnlineError::SampleCapacity)
    );
}

#[test]
fn fifteen_minute_policy_accepts_only_its_declared_cadence() {
    let (fit, samples) = fitted_posterior(64);
    let mut engine = CuspEngine::try_new(
        fit,
        samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config_at(StructuralCadence::FifteenMinutes),
        None,
    )
    .expect("engine");
    engine
        .update(available_input(BASE_TIME_NS, vector(0.25, 0.30)))
        .expect("initial update");
    assert_eq!(
        engine.update(available_input(
            BASE_TIME_NS + 5 * STEP_NS,
            vector(0.25, 0.30),
        )),
        Err(OnlineError::Cadence)
    );
    engine
        .update(available_input(
            BASE_TIME_NS + 15 * STEP_NS,
            vector(0.25, 0.30),
        ))
        .expect("fifteen-minute update");
}

#[test]
fn configuration_observation_and_input_boundaries_fail_closed() {
    let invalid_quality_order = OnlineConfig::try_new(OnlineConfigInput {
        schema_version: 1,
        cadence: StructuralCadence::FiveMinutes,
        feature_uncertainty_seed: 91,
        branch: BranchTrackerConfig::try_new(0.5, 0.01, 0.70).expect("branch config"),
        production_quality: QualityRequirement::try_new(
            QualityScore::from_millionths(700_000).expect("score"),
            QualityScore::from_millionths(700_000).expect("coverage"),
            vec![SourceHealthState::Healthy, SourceHealthState::Degraded],
        )
        .expect("production quality"),
        research_quality: QualityRequirement::try_new(
            QualityScore::from_millionths(900_000).expect("score"),
            QualityScore::from_millionths(900_000).expect("coverage"),
            vec![SourceHealthState::Healthy],
        )
        .expect("research quality"),
    });
    assert_eq!(invalid_quality_order, Err(OnlineError::InvalidConfig));
    let invalid_version = OnlineConfig::try_new(OnlineConfigInput {
        schema_version: 2,
        cadence: StructuralCadence::FiveMinutes,
        feature_uncertainty_seed: 91,
        branch: BranchTrackerConfig::try_new(0.5, 0.01, 0.70).expect("branch config"),
        production_quality: quality_requirement(900_000, 900_000),
        research_quality: quality_requirement(700_000, 700_000),
    });
    assert_eq!(invalid_version, Err(OnlineError::InvalidConfig));
    assert_eq!(
        StructuralInput::try_new(
            0,
            0.0,
            bitcoin(),
            vector(0.25, 0.30),
            None,
            StructuralQuality::new(
                QualityScore::from_millionths(950_000).expect("score"),
                QualityScore::from_millionths(950_000).expect("coverage"),
                SourceHealthState::Healthy,
            ),
        ),
        Err(OnlineError::InvalidInput)
    );
    assert!(BranchObservation::try_new(Vec::new()).is_err());
    assert!(BranchObservation::try_new(vec![0.0, 0.0]).is_err());
    assert!(BranchObservation::try_new(vec![0.5, 1.0]).is_err());
    assert!(BranchObservation::try_new(vec![f64::NAN]).is_err());
    assert!(BranchTrackerConfig::try_new(0.0, 0.01, 0.70).is_err());
    assert!(BranchTrackerConfig::try_new(0.5, 0.50, 0.70).is_err());
    assert!(BranchTrackerConfig::try_new(0.5, 0.01, 0.49).is_err());
}

#[test]
fn posterior_evidence_must_match_the_exact_frozen_fit() {
    let data = transition_data();
    let source_fit = fit(&data, smooth_config()).expect("source fit");
    let source_samples = LaplaceApproximation::from_fit(&source_fit, LaplaceConfig::fixture())
        .expect("source Laplace fit")
        .sample_parameters(73, 64)
        .expect("source samples");
    let different_fit = fit(
        &data,
        FitConfig::fixture(EstimatorKind::StudentTTransition)
            .with_penalty(PenaltyConfig::try_new(0.02, 0.0, 2.0).expect("different penalty")),
    )
    .expect("different fit");
    assert_ne!(
        source_fit.parameter_values(),
        different_fit.parameter_values()
    );
    assert_eq!(
        CuspEngine::try_new(
            different_fit,
            source_samples,
            ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
            online_config(),
            None,
        ),
        Err(OnlineError::EvidenceMismatch)
    );
}

#[test]
fn frozen_model_evidence_binds_the_complete_control_map_identity() {
    let first_fit = fit(
        &transition_data_with_normalization_hash([17; 32]),
        smooth_config(),
    )
    .expect("first fit");
    let second_fit = fit(
        &transition_data_with_normalization_hash([18; 32]),
        smooth_config(),
    )
    .expect("second fit");
    assert_eq!(first_fit.parameter_values(), second_fit.parameter_values());
    let first_samples = LaplaceApproximation::from_fit(&first_fit, LaplaceConfig::fixture())
        .expect("first Laplace fit")
        .sample_parameters(73, 64)
        .expect("first samples");
    let second_samples = LaplaceApproximation::from_fit(&second_fit, LaplaceConfig::fixture())
        .expect("second Laplace fit")
        .sample_parameters(73, 64)
        .expect("second samples");
    let first = CuspEngine::try_new(
        first_fit,
        first_samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("first engine");
    let second = CuspEngine::try_new(
        second_fit,
        second_samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("second engine");

    assert_ne!(first.model_evidence_hash(), second.model_evidence_hash());
}

#[test]
fn evidence_changes_with_time_without_online_refitting() {
    let (fit, samples) = fitted_posterior(64);
    let parameters = fit.parameter_values().to_vec();
    let mut first_engine = CuspEngine::try_new(
        fit.clone(),
        samples.clone(),
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("first engine");
    let mut second_engine = CuspEngine::try_new(
        fit,
        samples,
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("covariance"),
        online_config(),
        None,
    )
    .expect("second engine");
    let first = first_engine
        .update(available_input(BASE_TIME_NS, vector(0.25, 0.30)))
        .expect("first");
    let second = second_engine
        .update(available_input(
            BASE_TIME_NS + 5 * STEP_NS,
            vector(0.25, 0.30),
        ))
        .expect("second");
    assert_ne!(first.evidence_hash(), second.evidence_hash());
    assert_eq!(first_engine.frozen_parameters(), parameters);
    assert_eq!(second_engine.frozen_parameters(), parameters);
}

fn online_config() -> OnlineConfig {
    online_config_at(StructuralCadence::FiveMinutes)
}

fn online_config_at(cadence: StructuralCadence) -> OnlineConfig {
    OnlineConfig::try_new(OnlineConfigInput {
        schema_version: 1,
        cadence,
        feature_uncertainty_seed: 91,
        branch: BranchTrackerConfig::try_new(0.5, 0.01, 0.70).expect("branch config"),
        production_quality: QualityRequirement::try_new(
            QualityScore::from_millionths(900_000).expect("score"),
            QualityScore::from_millionths(900_000).expect("coverage"),
            vec![SourceHealthState::Healthy],
        )
        .expect("production quality"),
        research_quality: QualityRequirement::try_new(
            QualityScore::from_millionths(700_000).expect("score"),
            QualityScore::from_millionths(700_000).expect("coverage"),
            vec![SourceHealthState::Healthy, SourceHealthState::Degraded],
        )
        .expect("research quality"),
    })
    .expect("online config")
}

fn available_input(as_of_ns: i64, features: ControlVector) -> StructuralInput {
    StructuralInput::try_new(
        as_of_ns,
        0.7,
        bitcoin(),
        features,
        Some(feature_covariance()),
        StructuralQuality::new(
            QualityScore::from_millionths(950_000).expect("score"),
            QualityScore::from_millionths(950_000).expect("coverage"),
            SourceHealthState::Healthy,
        ),
    )
    .expect("available input")
}

fn quality_requirement(score: u32, coverage: u32) -> QualityRequirement {
    QualityRequirement::try_new(
        QualityScore::from_millionths(score).expect("score"),
        QualityScore::from_millionths(coverage).expect("coverage"),
        vec![SourceHealthState::Healthy],
    )
    .expect("quality requirement")
}

fn fitted_posterior(
    sample_count: usize,
) -> (
    cusp::fit::FitResult,
    cusp::uncertainty::PosteriorParameterSamples,
) {
    let data = transition_data();
    let fitted = fit(&data, smooth_config()).expect("converged fit");
    let approximation =
        LaplaceApproximation::from_fit(&fitted, LaplaceConfig::fixture()).expect("Laplace fit");
    let samples = approximation
        .sample_parameters(73, sample_count)
        .expect("parameter samples");
    (fitted, samples)
}

fn transition_data() -> FitDataset {
    transition_data_with_normalization_hash([17; 32])
}

fn transition_data_with_normalization_hash(normalization_hash: [u8; 32]) -> FitDataset {
    let mut rows = Vec::new();
    for index in 0..96_usize {
        let alpha_feature = centered_cycle(index, 17, 8.0);
        let beta_feature = centered_cycle(index * 7 + 3, 19, 9.0);
        let state = centered_cycle(index * 13 + 5, 23, 7.0);
        let alpha = 0.12 + 0.65 * alpha_feature;
        let beta = 0.55 + 0.48 * beta_feature;
        let delta_time = 0.08;
        let scale = 0.12;
        let noise = [-0.8, -0.35, 0.0, 0.25, 0.65, 0.15, -0.2][index % 7];
        let drift = alpha + beta * state - state.powi(3);
        rows.push(
            FitRow::try_new(
                bitcoin(),
                vector(alpha_feature, beta_feature),
                BASE_TIME_NS + index as i64 * STEP_NS,
                state,
                drift * delta_time + scale * delta_time.sqrt() * noise,
                delta_time,
                scale,
                1.0,
            )
            .expect("transition row"),
        );
    }
    FitDataset::try_new(
        control_schema(),
        normalization_hash,
        [23; 32],
        [29; 32],
        FitTimeRange::try_new(BASE_TIME_NS, BASE_TIME_NS + 96 * STEP_NS).expect("time range"),
        ControlCovariance::try_new(0.08, 0.01, 0.12).expect("control covariance"),
        rows,
    )
    .expect("fit dataset")
}

fn smooth_config() -> FitConfig {
    FitConfig::fixture(EstimatorKind::StudentTTransition)
        .with_penalty(PenaltyConfig::try_new(0.002, 0.0, 2.0).expect("smooth penalty"))
}

fn control_schema() -> ControlSchema {
    ControlSchema::try_new(
        Version::new(1, 0, 0),
        vec![
            FeatureGroup::try_new(
                "alpha_group",
                CoefficientSign::Any,
                CoefficientSign::Any,
                1.0,
            )
            .expect("alpha group"),
            FeatureGroup::try_new(
                "beta_group",
                CoefficientSign::Any,
                CoefficientSign::NonNegative,
                1.0,
            )
            .expect("beta group"),
        ],
        vec![
            ControlFeature::try_new(
                key("alpha_signal"),
                true,
                "alpha_group",
                ControlTarget::Alpha,
                0.0,
                1.0,
            )
            .expect("alpha feature"),
            ControlFeature::try_new(
                key("beta_signal"),
                true,
                "beta_group",
                ControlTarget::Beta,
                0.0,
                1.0,
            )
            .expect("beta feature"),
        ],
    )
    .expect("control schema")
}

fn feature_covariance() -> FeatureCovariance {
    FeatureCovariance::try_new(
        vec![key("alpha_signal"), key("beta_signal")],
        vec![vec![0.01, 0.002], vec![0.002, 0.015]],
    )
    .expect("feature covariance")
}

fn vector(alpha: f64, beta: f64) -> ControlVector {
    ControlVector::try_new(vec![
        ControlValue::present(key("alpha_signal"), alpha).expect("alpha value"),
        ControlValue::present(key("beta_signal"), beta).expect("beta value"),
    ])
    .expect("control vector")
}

fn key(id: &str) -> ControlFeatureKey {
    ControlFeatureKey::try_new(id, Version::new(1, 0, 0)).expect("feature key")
}

fn bitcoin() -> AssetId {
    AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("bitcoin")
}

fn centered_cycle(index: usize, modulus: usize, denominator: f64) -> f64 {
    (index % modulus) as f64 / denominator - (modulus / 2) as f64 / denominator
}

fn assert_probability_sum(probabilities: &[(BranchId, f64)]) {
    let sum = probabilities.iter().map(|(_, value)| value).sum::<f64>();
    assert!((sum - 1.0).abs() <= 1.0e-12, "sum={sum}");
    assert!(
        probabilities
            .iter()
            .all(|(_, value)| value.is_finite() && (0.0..=1.0).contains(value))
    );
}
