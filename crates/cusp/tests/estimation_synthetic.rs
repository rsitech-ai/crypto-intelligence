use cusp::{
    CoefficientSign, ControlCovariance, ControlFeature, ControlFeatureKey, ControlSchema,
    ControlTarget, ControlValue, ControlVector, Controls, FeatureGroup, Stability,
    analyze_equilibria,
    fit::{
        EstimatorKind, EstimatorRole, FitConfig, FitConfigInput, FitDataset, FitProblem, FitRow,
        FitTermination, OptimizerConfig, PenaltyConfig, fit,
    },
};
use domain::{AssetId, AssetNamespace};
use numerics::central_gradient;
use semver::Version;

const TRUE_ALPHA_COEFFICIENT: f64 = 0.8;
const TRUE_BETA_COEFFICIENT: f64 = 0.55;

#[test]
fn transition_estimator_recovers_synthetic_control_direction() {
    let data = transition_data(false, false, false);
    let result = fit(&data, FitConfig::fixture(EstimatorKind::StudentTTransition))
        .expect("bounded transition fit");

    assert!(
        result.diagnostics().converged(),
        "{:?}",
        result.diagnostics()
    );
    assert_eq!(result.role(), EstimatorRole::ProductionCandidate);
    let map = result.control_map().clone().into_input();
    assert!(coefficient(&map.alpha.shared, "alpha_signal") > 0.0);
    assert!(coefficient(&map.beta.shared, "beta_signal") > 0.0);
    assert_relative(
        coefficient(&map.alpha.shared, "alpha_signal"),
        TRUE_ALPHA_COEFFICIENT,
        0.25,
    );
    assert_relative(
        coefficient(&map.beta.shared, "beta_signal"),
        TRUE_BETA_COEFFICIENT,
        0.25,
    );
    let artifact = result
        .into_candidate_artifact()
        .expect("converged transition candidate");
    assert_eq!(artifact.dataset_manifest_hash(), [23; 32]);
    assert_eq!(artifact.training_fold_hash(), [29; 32]);
}

#[test]
fn transition_analytic_gradient_matches_independent_finite_differences() {
    let data = transition_data(false, false, false);
    let problem = FitProblem::try_new(&data, FitConfig::fixture(EstimatorKind::StudentTTransition))
        .expect("fit problem");
    let mut parameters = problem.initial_parameters();
    for (index, parameter) in parameters.iter_mut().enumerate() {
        *parameter = 0.03 * (index + 1) as f64;
    }
    let analytic = problem
        .smooth_objective(&parameters)
        .expect("analytic objective");
    let numeric = central_gradient(
        |point| {
            problem
                .smooth_objective(point)
                .map_or(f64::NAN, |objective| objective.value())
        },
        &parameters,
        1.0e-6,
    )
    .expect("finite-difference gradient");

    assert_eq!(numeric.len(), analytic.gradient().len());
    for (actual, expected) in analytic.gradient().iter().zip(numeric) {
        assert_relative(*actual, expected, 2.0e-5);
    }
}

#[test]
fn stationary_analytic_gradient_matches_independent_finite_differences() {
    let data = stationary_data();
    let problem = FitProblem::try_new(&data, FitConfig::fixture(EstimatorKind::StationaryDensity))
        .expect("stationary fit problem");
    let mut parameters = problem.initial_parameters();
    for (index, parameter) in parameters.iter_mut().enumerate() {
        *parameter = 0.02 * (index + 1) as f64;
    }
    let analytic = problem
        .smooth_objective(&parameters)
        .expect("analytic stationary objective");
    let numeric = central_gradient(
        |point| {
            problem
                .smooth_objective(point)
                .map_or(f64::NAN, |objective| objective.value())
        },
        &parameters,
        1.0e-6,
    )
    .expect("stationary finite-difference gradient");

    for (actual, expected) in analytic.gradient().iter().zip(numeric) {
        assert_relative(*actual, expected, 3.0e-5);
    }
}

#[test]
fn student_t_fit_retains_direction_under_declared_heavy_tail_outliers() {
    let clean = fit(
        &transition_data(false, false, false),
        FitConfig::fixture(EstimatorKind::StudentTTransition),
    )
    .expect("clean fit");
    let contaminated = fit(
        &transition_data(true, false, false),
        FitConfig::fixture(EstimatorKind::StudentTTransition),
    )
    .expect("heavy-tail fit");
    assert!(clean.diagnostics().converged());
    assert!(contaminated.diagnostics().converged());

    let clean_map = clean.control_map().clone().into_input();
    let contaminated_map = contaminated.control_map().clone().into_input();
    let clean_alpha = coefficient(&clean_map.alpha.shared, "alpha_signal");
    let contaminated_alpha = coefficient(&contaminated_map.alpha.shared, "alpha_signal");
    assert!(contaminated_alpha > 0.0);
    assert_relative(contaminated_alpha, clean_alpha, 0.35);
}

#[test]
fn transition_fit_is_consistent_across_declared_time_step_scaling() {
    let short = fit(
        &transition_data_with_delta_time(false, false, false, 0.04),
        FitConfig::fixture(EstimatorKind::StudentTTransition),
    )
    .expect("short-step fit");
    let long = fit(
        &transition_data_with_delta_time(false, false, false, 0.16),
        FitConfig::fixture(EstimatorKind::StudentTTransition),
    )
    .expect("long-step fit");
    assert!(short.diagnostics().converged());
    assert!(long.diagnostics().converged());

    let short_map = short.control_map().clone().into_input();
    let long_map = long.control_map().clone().into_input();
    assert_relative(
        coefficient(&short_map.alpha.shared, "alpha_signal"),
        coefficient(&long_map.alpha.shared, "alpha_signal"),
        0.12,
    );
    assert_relative(
        coefficient(&short_map.beta.shared, "beta_signal"),
        coefficient(&long_map.beta.shared, "beta_signal"),
        0.12,
    );
}

#[test]
fn stationary_density_fit_is_a_converged_research_comparator() {
    let result = fit(
        &stationary_data(),
        FitConfig::fixture(EstimatorKind::StationaryDensity),
    )
    .expect("stationary comparator fit");

    assert!(
        result.diagnostics().converged(),
        "{:?}",
        result.diagnostics()
    );
    assert_eq!(result.role(), EstimatorRole::ResearchComparator);
    assert!(result.diagnostics().objective().is_finite());
    assert!(result.into_candidate_artifact().is_err());
}

#[test]
fn stronger_hierarchical_penalty_shrinks_asset_deviations() {
    let data = transition_data(false, true, false);
    let weak = fit(
        &data,
        FitConfig::fixture(EstimatorKind::StudentTTransition)
            .with_penalty(PenaltyConfig::try_new(0.002, 0.5, 1.5).expect("weak penalty")),
    )
    .expect("weakly penalized fit");
    let strong = fit(
        &data,
        FitConfig::fixture(EstimatorKind::StudentTTransition)
            .with_penalty(PenaltyConfig::try_new(0.08, 0.8, 4.0).expect("strong penalty")),
    )
    .expect("strongly penalized fit");
    assert!(weak.diagnostics().converged());
    assert!(strong.diagnostics().converged());

    let weak_norm = asset_coefficient_l1(weak.control_map());
    let strong_norm = asset_coefficient_l1(strong.control_map());
    assert!(weak_norm > 0.0);
    assert!(
        strong_norm < weak_norm,
        "weak={weak_norm}, strong={strong_norm}"
    );
}

#[test]
fn nonconverged_and_ill_conditioned_fits_cannot_be_packaged() {
    let limited = fit(
        &transition_data(false, false, false),
        FitConfig::fixture(EstimatorKind::StudentTTransition).with_max_iterations(1),
    )
    .expect("inspectable bounded fit");
    assert!(!limited.diagnostics().converged());
    assert_eq!(
        limited.diagnostics().termination(),
        FitTermination::IterationLimit
    );
    assert!(limited.into_candidate_artifact().is_err());

    let ill_conditioned = fit(
        &transition_data(false, false, true),
        FitConfig::fixture(EstimatorKind::StudentTTransition).with_max_condition_number(1.0e6),
    )
    .expect("inspectable ill-conditioned fit");
    assert!(!ill_conditioned.diagnostics().converged());
    assert_eq!(
        ill_conditioned.diagnostics().termination(),
        FitTermination::IllConditioned
    );
    assert!(
        ill_conditioned
            .diagnostics()
            .condition_estimate()
            .is_infinite()
    );
    assert!(ill_conditioned.into_candidate_artifact().is_err());

    let base = FitConfig::fixture(EstimatorKind::StudentTTransition);
    let optimizer = base.optimizer();
    let work_limited_config = FitConfig::try_new(FitConfigInput {
        estimator: base.estimator(),
        degrees_of_freedom: base.degrees_of_freedom(),
        penalty: base.penalty(),
        optimizer: OptimizerConfig::try_new(
            optimizer.max_iterations(),
            optimizer.tolerance(),
            optimizer.initial_step(),
            optimizer.minimum_step(),
            optimizer.max_backtracking(),
            optimizer.max_condition_number(),
            7_000,
        )
        .expect("bounded optimizer"),
        integration_bound: base.integration_bound(),
        integration_intervals: base.integration_intervals(),
    })
    .expect("work-limited config");
    let work_limited = fit(&transition_data(false, false, false), work_limited_config)
        .expect("inspectable work-limited fit");
    assert_eq!(
        work_limited.diagnostics().termination(),
        FitTermination::WorkLimit
    );
    assert!(work_limited.into_candidate_artifact().is_err());
}

#[test]
fn repeated_fit_is_deterministic_and_boundaries_reject_invalid_rows() {
    let data = transition_data(false, true, false);
    let config = FitConfig::fixture(EstimatorKind::StudentTTransition);
    let first = fit(&data, config.clone()).expect("first fit");
    let second = fit(&data, config).expect("second fit");
    assert_eq!(first.diagnostics(), second.diagnostics());
    assert_eq!(first.control_map(), second.control_map());

    let row = FitRow::try_new(bitcoin(), vector(0.0, 0.0), 0.0, 0.0, 0.0, 1.0, 1.0);
    assert!(row.is_err());

    let optimizer = FitConfig::fixture(EstimatorKind::StudentTTransition).optimizer();
    assert!(
        OptimizerConfig::try_new(
            u32::MAX,
            optimizer.tolerance(),
            optimizer.initial_step(),
            optimizer.minimum_step(),
            optimizer.max_backtracking(),
            optimizer.max_condition_number(),
            optimizer.work_limit(),
        )
        .is_err()
    );
    assert!(
        OptimizerConfig::try_new(
            optimizer.max_iterations(),
            optimizer.tolerance(),
            optimizer.initial_step(),
            optimizer.minimum_step(),
            u32::MAX,
            optimizer.max_condition_number(),
            optimizer.work_limit(),
        )
        .is_err()
    );
    assert!(
        OptimizerConfig::try_new(
            optimizer.max_iterations(),
            optimizer.tolerance(),
            optimizer.initial_step(),
            optimizer.minimum_step(),
            optimizer.max_backtracking(),
            optimizer.max_condition_number(),
            u64::MAX,
        )
        .is_err()
    );
}

fn transition_data(contaminated: bool, two_assets: bool, ill_conditioned: bool) -> FitDataset {
    transition_data_with_delta_time(contaminated, two_assets, ill_conditioned, 0.08)
}

fn transition_data_with_delta_time(
    contaminated: bool,
    two_assets: bool,
    ill_conditioned: bool,
    delta_time: f64,
) -> FitDataset {
    let mut rows = Vec::new();
    for index in 0..240_usize {
        let asset = if two_assets && index % 2 == 1 {
            ethereum()
        } else {
            bitcoin()
        };
        let alpha_feature = centered_cycle(index, 17, 8.0);
        let beta_feature = if ill_conditioned {
            alpha_feature
        } else {
            centered_cycle(index * 7 + 3, 19, 9.0)
        };
        let state = if ill_conditioned {
            1.0
        } else {
            centered_cycle(index * 13 + 5, 23, 7.0)
        };
        let asset_alpha = if asset == ethereum() { 0.24 } else { 0.0 };
        let asset_beta = if asset == ethereum() { 0.16 } else { 0.0 };
        let alpha = 0.12 + (TRUE_ALPHA_COEFFICIENT + asset_alpha) * alpha_feature;
        let beta = 0.28 + (TRUE_BETA_COEFFICIENT + asset_beta) * beta_feature;
        let innovation_scale = 0.12;
        let base_noise = [
            -1.2, -0.8, -0.45, -0.2, 0.0, 0.15, 0.4, 0.7, 1.05, 0.35, -0.3,
        ][index % 11];
        let outlier = if contaminated && index % 47 == 0 {
            if index % 2 == 0 { 10.0 } else { -9.0 }
        } else {
            0.0
        };
        let drift = alpha + beta * state - state.powi(3);
        let delta_state =
            drift * delta_time + innovation_scale * delta_time.sqrt() * (base_noise + outlier);
        rows.push(
            FitRow::try_new(
                asset,
                vector(alpha_feature, beta_feature),
                state,
                delta_state,
                delta_time,
                innovation_scale,
                1.0,
            )
            .expect("synthetic transition row"),
        );
    }
    dataset(rows)
}

fn stationary_data() -> FitDataset {
    let offsets = [-0.55, -0.30, -0.12, 0.0, 0.10, 0.27, 0.48];
    let mut rows = Vec::new();
    for index in 0..224_usize {
        let alpha_feature = if index % 2 == 0 { -0.75 } else { 0.75 };
        let beta_feature = centered_cycle(index * 5 + 2, 13, 6.0);
        let controls = Controls::try_new(0.55 * alpha_feature, 0.75 + 0.18 * beta_feature)
            .expect("finite controls");
        let equilibrium = analyze_equilibria(controls)
            .expect("equilibrium")
            .roots
            .into_iter()
            .filter(|root| root.stability == Stability::Stable)
            .min_by(|left, right| {
                left.equilibrium
                    .value
                    .abs()
                    .total_cmp(&right.equilibrium.value.abs())
            })
            .expect("stable root")
            .equilibrium
            .value;
        let state = equilibrium + offsets[index % offsets.len()];
        rows.push(
            FitRow::try_new(
                bitcoin(),
                vector(alpha_feature, beta_feature),
                state,
                0.0,
                1.0,
                1.0,
                1.0,
            )
            .expect("stationary row"),
        );
    }
    dataset(rows)
}

fn dataset(rows: Vec<FitRow>) -> FitDataset {
    FitDataset::try_new(
        control_schema(),
        [17; 32],
        [23; 32],
        [29; 32],
        ControlCovariance::try_new(0.4, 0.05, 0.6).expect("residual covariance"),
        rows,
    )
    .expect("fit dataset")
}

fn control_schema() -> ControlSchema {
    ControlSchema::try_new(
        Version::new(1, 0, 0),
        vec![
            FeatureGroup::try_new(
                "alpha_group",
                CoefficientSign::NonNegative,
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

fn ethereum() -> AssetId {
    AssetId::new(AssetNamespace::Native, "ethereum", "", "ETH", 1).expect("ethereum")
}

fn centered_cycle(index: usize, modulus: usize, denominator: f64) -> f64 {
    (index % modulus) as f64 / denominator - (modulus / 2) as f64 / denominator
}

fn coefficient(coefficients: &[cusp::FeatureCoefficient], id: &str) -> f64 {
    coefficients
        .iter()
        .find(|coefficient| coefficient.key.id() == id)
        .map_or(0.0, |coefficient| coefficient.coefficient)
}

fn asset_coefficient_l1(map: &cusp::ControlMap) -> f64 {
    let input = map.clone().into_input();
    input
        .alpha
        .by_asset
        .values()
        .chain(input.beta.by_asset.values())
        .flatten()
        .map(|coefficient| coefficient.coefficient.abs())
        .sum()
}

fn assert_relative(actual: f64, expected: f64, relative_tolerance: f64) {
    let scale = actual.abs().max(expected.abs()).max(1.0);
    assert!(
        (actual - expected).abs() <= relative_tolerance * scale,
        "expected {expected}, got {actual}"
    );
}
