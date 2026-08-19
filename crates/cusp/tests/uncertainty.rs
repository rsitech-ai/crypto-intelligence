use cusp::{
    CoefficientSign, ControlCovariance, ControlFeature, ControlFeatureKey, ControlSchema,
    ControlTarget, ControlValue, ControlVector, FeatureGroup,
    bootstrap::{BlockedBootstrap, BlockedBootstrapConfig, TimeBlock},
    fit::{
        EstimatorKind, FitConfig, FitDataset, FitProblem, FitRow, FitTimeRange, PenaltyConfig, fit,
    },
    uncertainty::{LaplaceApproximation, LaplaceConfig, UncertaintyMethod, UncertaintyQuality},
};
use domain::{AssetId, AssetNamespace};
use semver::Version;

const BASE_TIME_NS: i64 = 1_800_000_000_000_000_000;
const STEP_NS: i64 = 60_000_000_000;

#[test]
fn transition_analytic_hessian_matches_gradient_finite_differences() {
    let data = transition_data(false);
    let problem =
        FitProblem::try_new(&data, smooth_config()).expect("well-conditioned transition problem");
    let point = vec![0.15, 0.25, 0.4, 0.3, 0.05, 0.04];
    let analytic = problem
        .smooth_hessian(&point)
        .expect("analytic transition Hessian");
    assert_hessian_matches_gradient(&problem, &point, &analytic, 3.0e-5);
}

#[test]
fn stationary_analytic_hessian_matches_gradient_finite_differences() {
    let data = stationary_data();
    let problem = FitProblem::try_new(&data, smooth_config_for(EstimatorKind::StationaryDensity))
        .expect("well-conditioned stationary problem");
    let point = vec![0.05, 0.6, 0.25, 0.15];
    let analytic = problem
        .smooth_hessian(&point)
        .expect("analytic stationary Hessian");
    assert_hessian_matches_gradient(&problem, &point, &analytic, 8.0e-5);
}

#[test]
fn laplace_covariance_is_symmetric_positive_definite_and_evidence_bound() {
    let data = transition_data(false);
    let fitted = fit(&data, smooth_config()).expect("converged fit");
    assert!(fitted.diagnostics().converged());
    let approximation =
        LaplaceApproximation::from_fit(&fitted, LaplaceConfig::fixture()).expect("Laplace fit");
    let alternate_config =
        LaplaceConfig::try_new(1.0e-8, 1.0e-6, 1.0e-2, 1.0e11, 1.0e14, 1.0e-8, 4.0)
            .expect("alternate evidence threshold");
    let alternate_approximation =
        LaplaceApproximation::from_fit(&fitted, alternate_config).expect("alternate Laplace fit");
    assert_ne!(
        approximation.evidence_digest(),
        alternate_approximation.evidence_digest()
    );

    assert!(approximation.covariance_is_symmetric(1.0e-12));
    assert!(approximation.minimum_covariance_eigenvalue() > 0.0);
    assert!(approximation.diagnostics().active_parameters() >= 2);
    assert_eq!(
        approximation.diagnostics().active_parameters()
            + approximation.diagnostics().inactive_parameters(),
        fitted.parameter_values().len()
    );
    assert_eq!(
        approximation.diagnostics().effective_observations(),
        data.rows().len()
    );
    assert_eq!(
        approximation.dataset_manifest_hash(),
        data.dataset_manifest_hash()
    );
    assert_eq!(
        approximation.training_fold_hash(),
        data.training_fold_hash()
    );
    assert_eq!(
        approximation.quality(),
        UncertaintyQuality::ProductionCandidate
    );
}

#[test]
fn laplace_samples_controls_and_intervals_are_deterministic_and_bounded() {
    let data = transition_data(false);
    let fitted = fit(&data, smooth_config()).expect("converged fit");
    let approximation =
        LaplaceApproximation::from_fit(&fitted, LaplaceConfig::fixture()).expect("Laplace fit");

    let first = approximation
        .sample_parameters(73, 512)
        .expect("parameter samples");
    let repeated = approximation
        .sample_parameters(73, 512)
        .expect("repeated samples");
    let alternate = approximation
        .sample_parameters(74, 512)
        .expect("alternate samples");
    assert_eq!(first.digest(), repeated.digest());
    assert_eq!(first.samples(), repeated.samples());
    assert_eq!(first.mode(), fitted.parameter_values());
    assert_eq!(first.parameter_keys(), fitted.parameter_keys());
    assert_eq!(first.quality(), approximation.quality());
    assert_ne!(first.digest(), alternate.digest());
    assert_eq!(first.effective_samples(), 512);
    assert!(approximation.sample_parameters(73, 31).is_err());

    let controls = approximation
        .sample_controls(&fitted, &bitcoin(), &vector(0.4, -0.2), 73, 512)
        .expect("control samples");
    assert_eq!(controls.effective_samples(), 512);
    assert_eq!(controls.seed(), 73);
    assert!(
        controls
            .samples()
            .iter()
            .all(|sample| { sample.alpha.is_finite() && sample.beta.is_finite() })
    );

    let interval = approximation
        .parameter_interval(0, 0.95)
        .expect("coefficient interval");
    assert_eq!(interval.method(), UncertaintyMethod::LaplaceNormal);
    assert_eq!(interval.seed(), None);
    assert_eq!(interval.effective_samples(), data.rows().len());
    assert_eq!(interval.failed_refits(), 0);
    assert!(interval.lower() <= interval.estimate());
    assert!(interval.estimate() <= interval.upper());
    assert_eq!(
        interval.regularization_added(),
        approximation.diagnostics().regularization_added()
    );

    let empirical_coverage = first
        .samples()
        .iter()
        .filter(|sample| sample[0] >= interval.lower() && sample[0] <= interval.upper())
        .count() as f64
        / first.samples().len() as f64;
    assert!(
        (0.90..=0.99).contains(&empirical_coverage),
        "coverage={empirical_coverage}"
    );
}

#[test]
fn laplace_intervals_contract_with_more_independent_observations() {
    let small_fit =
        fit(&transition_data_with_rows(false, 48), smooth_config()).expect("small converged fit");
    let large_fit =
        fit(&transition_data_with_rows(false, 192), smooth_config()).expect("large converged fit");
    let small = LaplaceApproximation::from_fit(&small_fit, LaplaceConfig::fixture())
        .expect("small Laplace fit")
        .parameter_interval(0, 0.95)
        .expect("small interval");
    let large = LaplaceApproximation::from_fit(&large_fit, LaplaceConfig::fixture())
        .expect("large Laplace fit")
        .parameter_interval(0, 0.95)
        .expect("large interval");
    let small_width = small.upper() - small.lower();
    let large_width = large.upper() - large.lower();
    assert!(
        large_width < small_width * 0.75,
        "small width={small_width}, large width={large_width}"
    );
}

#[test]
fn laplace_regularization_and_active_set_quality_are_explicit() {
    let data = transition_data(false);
    let sparse_fit = fit(
        &data,
        FitConfig::fixture(EstimatorKind::StudentTTransition)
            .with_penalty(PenaltyConfig::try_new(0.25, 1.0, 4.0).expect("sparse penalty")),
    )
    .expect("inspectable sparse fit");
    assert!(sparse_fit.diagnostics().converged());
    let approximation = LaplaceApproximation::from_fit(&sparse_fit, LaplaceConfig::fixture())
        .expect("active-set approximation");
    assert!(approximation.diagnostics().inactive_parameters() > 0);
    assert!(
        approximation
            .inactive_parameter_indices()
            .iter()
            .all(|index| sparse_fit.parameter_values()[*index] == 0.0)
    );

    let experimental_config = LaplaceConfig::try_new(1.0e-6, 0.0, 1.0, 10.0, 1.0e14, 1.0e-8, 4.0)
        .expect("experimental threshold");
    let experimental = LaplaceApproximation::from_fit(
        &fit(&data, smooth_config()).expect("fit"),
        experimental_config,
    )
    .expect("bounded regularization");
    assert_ne!(
        experimental.quality(),
        UncertaintyQuality::ProductionCandidate
    );

    assert!(LaplaceConfig::try_new(1.0e-6, 1.0, 0.5, 10.0, 5.0, 1.0e-8, 4.0).is_err());

    let stationary_fit = fit(
        &stationary_data(),
        smooth_config_for(EstimatorKind::StationaryDensity),
    )
    .expect("stationary fit");
    let stationary = LaplaceApproximation::from_fit(&stationary_fit, LaplaceConfig::fixture())
        .expect("stationary uncertainty");
    assert_eq!(stationary.quality(), UncertaintyQuality::Experimental);
}

#[test]
fn blocked_bootstrap_is_reproducible_and_never_leaves_the_bound_fold() {
    let data = transition_data(false);
    let blocks = time_blocks(4, 24);
    let config = BlockedBootstrapConfig::try_new(
        99,
        48,
        0.25,
        data.training_fold_hash(),
        blocks,
        smooth_config(),
    )
    .expect("bootstrap config");
    let first = BlockedBootstrap::new(config.clone())
        .run(&data)
        .expect("first bootstrap");
    let repeated = BlockedBootstrap::new(config)
        .run(&data)
        .expect("repeated bootstrap");
    let alternate = BlockedBootstrap::new(
        BlockedBootstrapConfig::try_new(
            100,
            48,
            0.25,
            data.training_fold_hash(),
            time_blocks(4, 24),
            smooth_config(),
        )
        .expect("alternate bootstrap"),
    )
    .run(&data)
    .expect("alternate result");
    assert_eq!(first.digest(), repeated.digest());
    assert_ne!(first.digest(), alternate.digest());
    assert_eq!(first.parameter_samples(), repeated.parameter_samples());
    assert_eq!(first.requested_refits(), 48);
    assert_eq!(
        first.successful_refits() + first.failed_refits(),
        first.requested_refits()
    );
    assert_eq!(first.training_fold_hash(), data.training_fold_hash());
    assert!(
        first
            .selection_counts()
            .iter()
            .all(|counts| counts.iter().sum::<u32>() == 4)
    );
    assert!(first.minimum_effective_samples() > 0.0);
    assert!(first.minimum_effective_samples() <= 96.0);
    assert!(first.maximum_effective_samples() >= first.minimum_effective_samples());
    assert_ne!(first.quality(), UncertaintyQuality::Unavailable);

    let interval = first
        .parameter_interval(2, 0.90)
        .expect("bootstrap interval");
    assert_eq!(
        interval.method(),
        UncertaintyMethod::BlockedBootstrapPercentile
    );
    assert_eq!(interval.seed(), Some(99));
    assert_eq!(interval.effective_samples(), first.successful_refits());
    assert_eq!(interval.failed_refits(), first.failed_refits());

    let controls = first
        .control_interval(&bitcoin(), &vector(0.4, -0.2), 0.90)
        .expect("bootstrap control intervals");
    assert_eq!(
        controls.alpha().method(),
        UncertaintyMethod::BlockedBootstrapPercentile
    );
    assert_eq!(controls.beta().seed(), Some(99));
}

#[test]
fn bootstrap_failed_refits_and_quality_thresholds_are_visible() {
    let data = transition_data(true);
    let blocks = time_blocks(4, 24);
    let permissive = BlockedBootstrapConfig::try_new(
        17,
        64,
        0.30,
        data.training_fold_hash(),
        blocks.clone(),
        smooth_config(),
    )
    .expect("permissive bootstrap");
    let result = BlockedBootstrap::new(permissive)
        .run(&data)
        .expect("inspectable bootstrap");
    assert!(result.failed_refits() > 0);
    assert!(result.successful_refits() > 0);
    assert_ne!(result.quality(), UncertaintyQuality::Unavailable);

    let strict = BlockedBootstrapConfig::try_new(
        17,
        64,
        0.01,
        data.training_fold_hash(),
        blocks,
        smooth_config(),
    )
    .expect("strict bootstrap");
    let unavailable = BlockedBootstrap::new(strict)
        .run(&data)
        .expect("diagnostic result");
    assert_eq!(unavailable.quality(), UncertaintyQuality::Unavailable);
    assert!(unavailable.parameter_interval(0, 0.90).is_err());
}

#[test]
fn bootstrap_boundaries_reject_wrong_fold_overlap_gaps_and_unbounded_work() {
    let data = transition_data(false);
    let valid_blocks = time_blocks(4, 24);
    let wrong_fold = BlockedBootstrapConfig::try_new(
        5,
        32,
        0.25,
        [88; 32],
        valid_blocks.clone(),
        smooth_config(),
    )
    .expect("structurally valid config");
    assert!(BlockedBootstrap::new(wrong_fold).run(&data).is_err());

    let overlap = vec![
        TimeBlock::try_new(BASE_TIME_NS, BASE_TIME_NS + 30 * STEP_NS).expect("first"),
        TimeBlock::try_new(BASE_TIME_NS + 20 * STEP_NS, BASE_TIME_NS + 60 * STEP_NS)
            .expect("second"),
    ];
    assert!(
        BlockedBootstrapConfig::try_new(
            5,
            32,
            0.25,
            data.training_fold_hash(),
            overlap,
            smooth_config(),
        )
        .is_err()
    );

    let incomplete = vec![
        TimeBlock::try_new(BASE_TIME_NS, BASE_TIME_NS + 24 * STEP_NS).expect("first"),
        TimeBlock::try_new(BASE_TIME_NS + 48 * STEP_NS, BASE_TIME_NS + 72 * STEP_NS)
            .expect("second"),
    ];
    assert!(
        BlockedBootstrapConfig::try_new(
            5,
            32,
            0.25,
            data.training_fold_hash(),
            incomplete,
            smooth_config(),
        )
        .is_err()
    );

    assert!(
        BlockedBootstrapConfig::try_new(
            5,
            u32::MAX,
            0.25,
            data.training_fold_hash(),
            valid_blocks,
            smooth_config(),
        )
        .is_err()
    );

    assert!(
        FitDataset::try_new(
            control_schema(),
            [17; 32],
            [23; 32],
            [29; 32],
            FitTimeRange::try_new(BASE_TIME_NS + STEP_NS, BASE_TIME_NS + 96 * STEP_NS)
                .expect("narrow range"),
            ControlCovariance::try_new(0.4, 0.05, 0.6).expect("covariance"),
            data.rows().to_vec(),
        )
        .is_err()
    );
}

fn assert_hessian_matches_gradient(
    problem: &FitProblem,
    point: &[f64],
    analytic: &[Vec<f64>],
    relative_tolerance: f64,
) {
    let step = 1.0e-5;
    for column in 0..point.len() {
        let mut left = point.to_vec();
        let mut right = point.to_vec();
        left[column] -= step;
        right[column] += step;
        let left_gradient = problem
            .smooth_objective(&left)
            .expect("left objective")
            .gradient()
            .to_vec();
        let right_gradient = problem
            .smooth_objective(&right)
            .expect("right objective")
            .gradient()
            .to_vec();
        for row in 0..point.len() {
            let numerical = (right_gradient[row] - left_gradient[row]) / (2.0 * step);
            assert_relative(analytic[row][column], numerical, relative_tolerance);
        }
    }
}

fn smooth_config() -> FitConfig {
    smooth_config_for(EstimatorKind::StudentTTransition)
}

fn smooth_config_for(estimator: EstimatorKind) -> FitConfig {
    FitConfig::fixture(estimator)
        .with_penalty(PenaltyConfig::try_new(0.002, 0.0, 2.0).expect("smooth L2 penalty"))
}

fn transition_data(asset_segregated_blocks: bool) -> FitDataset {
    transition_data_with_rows(asset_segregated_blocks, 96)
}

fn transition_data_with_rows(asset_segregated_blocks: bool, row_count: usize) -> FitDataset {
    let mut rows = Vec::new();
    for index in 0..row_count {
        let asset = if asset_segregated_blocks {
            if index < row_count / 2 {
                bitcoin()
            } else {
                ethereum()
            }
        } else if index % 2 == 0 {
            bitcoin()
        } else {
            ethereum()
        };
        let alpha_feature = centered_cycle(index, 17, 8.0);
        let beta_feature = centered_cycle(index * 7 + 3, 19, 9.0);
        let state = centered_cycle(index * 13 + 5, 23, 7.0);
        let asset_alpha = if asset == ethereum() { 0.12 } else { 0.0 };
        let asset_beta = if asset == ethereum() { 0.08 } else { 0.0 };
        let alpha = 0.12 + (0.65 + asset_alpha) * alpha_feature;
        let beta = 0.28 + (0.48 + asset_beta) * beta_feature;
        let delta_time = 0.08;
        let scale = 0.12;
        let noise = [-0.8, -0.35, 0.0, 0.25, 0.65, 0.15, -0.2][index % 7];
        let drift = alpha + beta * state - state.powi(3);
        rows.push(
            FitRow::try_new(
                asset,
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
    dataset(rows)
}

fn stationary_data() -> FitDataset {
    let offsets = [-0.45, -0.20, 0.0, 0.18, 0.38];
    let mut rows = Vec::new();
    for index in 0..80_usize {
        let alpha_feature = if index % 2 == 0 { -0.65 } else { 0.65 };
        let beta_feature = centered_cycle(index * 5 + 2, 13, 6.0);
        rows.push(
            FitRow::try_new(
                bitcoin(),
                vector(alpha_feature, beta_feature),
                BASE_TIME_NS + index as i64 * STEP_NS,
                0.45 * alpha_feature + offsets[index % offsets.len()],
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
    let start_ns = rows.first().expect("first fit row").event_time_ns();
    let end_ns = rows
        .last()
        .expect("last fit row")
        .event_time_ns()
        .checked_add(STEP_NS)
        .expect("fit range");
    FitDataset::try_new(
        control_schema(),
        [17; 32],
        [23; 32],
        [29; 32],
        FitTimeRange::try_new(start_ns, end_ns).expect("training range"),
        ControlCovariance::try_new(0.4, 0.05, 0.6).expect("residual covariance"),
        rows,
    )
    .expect("fit dataset")
}

fn time_blocks(count: usize, rows_per_block: usize) -> Vec<TimeBlock> {
    (0..count)
        .map(|index| {
            let start = BASE_TIME_NS + (index * rows_per_block) as i64 * STEP_NS;
            let end = BASE_TIME_NS + ((index + 1) * rows_per_block) as i64 * STEP_NS;
            TimeBlock::try_new(start, end).expect("time block")
        })
        .collect()
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

fn assert_relative(actual: f64, expected: f64, relative_tolerance: f64) {
    let scale = actual.abs().max(expected.abs()).max(1.0);
    assert!(
        (actual - expected).abs() <= relative_tolerance * scale,
        "expected {expected}, got {actual}"
    );
}
