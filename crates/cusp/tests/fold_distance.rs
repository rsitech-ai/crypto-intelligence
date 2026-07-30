use cusp::{ControlWhitening, Controls, CuspError, fold_point, nearest_fold};

#[test]
fn exact_fold_has_zero_distance_and_recovers_both_parameters() {
    for parameter in [-2.0_f64, -1.0, 0.0, 1.0, 2.0] {
        let controls = fold_point(parameter).expect("finite fold");
        let result = nearest_fold(controls, &ControlWhitening::identity()).expect("fold distance");
        assert!(result.distance.abs() < 1e-10);
        assert!((result.fold_parameter - parameter).abs() < 1e-8);
        assert_controls_close(result.nearest, controls, 1e-9);
        assert!(result.diagnostics.converged);
        assert_eq!(result.diagnostics.method, "fold-grid-brent-v1");
    }
}

#[test]
fn signed_distance_is_negative_inside_and_positive_outside_under_whitening() {
    for whitening in [
        ControlWhitening::identity(),
        ControlWhitening::diagonal(4.0, 0.25).expect("diagonal covariance"),
        ControlWhitening::from_covariance(4.0, 0.5, 1.0).expect("correlated covariance"),
    ] {
        let inside = nearest_fold(
            Controls {
                alpha: 0.0,
                beta: 1.0,
            },
            &whitening,
        )
        .expect("inside");
        let outside = nearest_fold(
            Controls {
                alpha: 0.0,
                beta: -1.0,
            },
            &whitening,
        )
        .expect("outside");
        assert!(inside.distance.is_sign_negative());
        assert!(outside.distance.is_sign_positive());
        assert!(inside.distance.abs() > 0.0);
        assert!(outside.distance > 0.0);
    }
}

#[test]
fn alpha_reflection_preserves_distance_and_reflects_the_nearest_fold() {
    let whitening = ControlWhitening::diagonal(2.0, 0.5).expect("whitening");
    let left = nearest_fold(
        Controls {
            alpha: -0.75,
            beta: 1.5,
        },
        &whitening,
    )
    .expect("left");
    let right = nearest_fold(
        Controls {
            alpha: 0.75,
            beta: 1.5,
        },
        &whitening,
    )
    .expect("right");
    assert_relative(left.distance, right.distance, 2e-10);
    assert_relative(left.fold_parameter, -right.fold_parameter, 2e-9);
    assert_relative(left.nearest.alpha, -right.nearest.alpha, 2e-9);
    assert_relative(left.nearest.beta, right.nearest.beta, 2e-9);
}

#[test]
fn state_and_covariance_rescaling_preserve_whitened_distance() {
    let baseline_controls = Controls {
        alpha: 0.4,
        beta: 1.2,
    };
    let baseline_whitening =
        ControlWhitening::from_covariance(2.0, 0.25, 0.5).expect("baseline covariance");
    let baseline = nearest_fold(baseline_controls, &baseline_whitening).expect("baseline");

    for state_scale in [1e-20_f64, 1e20_f64] {
        let alpha_scale = state_scale.powi(3);
        let beta_scale = state_scale.powi(2);
        let scaled = nearest_fold(
            Controls {
                alpha: baseline_controls.alpha * alpha_scale,
                beta: baseline_controls.beta * beta_scale,
            },
            &ControlWhitening::from_covariance(
                2.0 * alpha_scale.powi(2),
                0.25 * alpha_scale * beta_scale,
                0.5 * beta_scale.powi(2),
            )
            .expect("scaled covariance"),
        )
        .expect("scaled distance");
        assert_relative(scaled.distance, baseline.distance, 2e-8);
        assert_relative(
            scaled.fold_parameter,
            baseline.fold_parameter * state_scale,
            2e-8,
        );
    }
}

#[test]
fn optimizer_matches_an_independent_dense_reference_across_both_fold_branches() {
    let whitening = ControlWhitening::from_covariance(1.5, -0.4, 0.75).expect("covariance");
    for controls in [
        Controls {
            alpha: -1.2,
            beta: 2.0,
        },
        Controls {
            alpha: 0.9,
            beta: 0.2,
        },
        Controls {
            alpha: 0.1,
            beta: -1.5,
        },
    ] {
        let result = nearest_fold(controls, &whitening).expect("optimized distance");
        let reference = dense_reference(controls, &whitening, -3.0, 3.0, 120_000);
        assert!(
            result.distance.abs() <= reference + 2e-7,
            "optimized {}, dense reference {}",
            result.distance.abs(),
            reference
        );
        assert_controls_close(
            result.nearest,
            fold_point(result.fold_parameter).expect("reported fold"),
            1e-12,
        );
    }
}

#[test]
fn singular_invalid_and_nonfinite_inputs_fail_closed() {
    assert!(matches!(
        ControlWhitening::from_covariance(1.0, 1.0, 1.0),
        Err(CuspError::InvalidWhitening)
    ));
    assert!(matches!(
        ControlWhitening::diagonal(0.0, 1.0),
        Err(CuspError::InvalidWhitening)
    ));
    assert!(matches!(
        nearest_fold(
            Controls {
                alpha: f64::NAN,
                beta: 1.0,
            },
            &ControlWhitening::identity()
        ),
        Err(CuspError::NonFiniteInput)
    ));
    assert!(fold_point(f64::MAX).is_err());

    let whitening = ControlWhitening::from_covariance(2.0, 0.25, 0.5).expect("valid covariance");
    let encoded = serde_json::to_vec(&whitening).expect("serialize whitening");
    let decoded: ControlWhitening =
        serde_json::from_slice(&encoded).expect("validated whitening round trip");
    assert_eq!(decoded, whitening);
    let singular = serde_json::json!({
        "alpha_variance": 1.0,
        "alpha_beta_covariance": 1.0,
        "beta_variance": 1.0
    });
    assert!(serde_json::from_value::<ControlWhitening>(singular).is_err());
}

fn dense_reference(
    controls: Controls,
    whitening: &ControlWhitening,
    left: f64,
    right: f64,
    intervals: usize,
) -> f64 {
    (0..=intervals)
        .map(|index| left + (right - left) * index as f64 / intervals as f64)
        .map(|parameter| {
            whitening
                .distance(controls, fold_point(parameter).expect("reference fold"))
                .expect("reference distance")
        })
        .fold(f64::INFINITY, f64::min)
}

fn assert_controls_close(actual: Controls, expected: Controls, tolerance: f64) {
    assert!((actual.alpha - expected.alpha).abs() <= tolerance);
    assert!((actual.beta - expected.beta).abs() <= tolerance);
}

fn assert_relative(actual: f64, expected: f64, tolerance: f64) {
    let scale = actual.abs().max(expected.abs()).max(f64::MIN_POSITIVE);
    assert!(
        (actual - expected).abs() <= tolerance * scale,
        "expected {expected}, got {actual}"
    );
}
