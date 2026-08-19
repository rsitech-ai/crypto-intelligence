use numerics::{
    NumericalError, brent_minimize, brent_root, central_gradient, is_positive_definite, logsumexp,
};

#[test]
fn brent_minimizer_converges_to_the_hand_derived_quadratic_minimum() {
    let result = brent_minimize(|x| (x - 2.0).powi(2), -5.0, 5.0, 1e-12, 200).expect("minimum");
    assert!((result.argmin - 2.0).abs() < 1e-9);
    assert!(result.value < 1e-18);
    assert!(result.diagnostics.converged);
    assert_eq!(result.diagnostics.method, "brent-minimize-v1");
    assert_eq!(result.diagnostics.requested_tolerance, 1e-12);
    assert!(result.diagnostics.evaluations <= 203);
}

#[test]
fn brent_minimizer_includes_both_interval_endpoints() {
    let left = brent_minimize(|x| x, -1.0, 1.0, 1e-12, 200).expect("left endpoint minimum");
    assert_eq!(left.argmin, -1.0);
    assert_eq!(left.value, -1.0);

    let right = brent_minimize(|x| -x, -1.0, 1.0, 1e-12, 200).expect("right endpoint minimum");
    assert_eq!(right.argmin, 1.0);
    assert_eq!(right.value, -1.0);
}

#[test]
fn brent_root_preserves_the_bracket_and_matches_a_known_root() {
    let result = brent_root(|x| x.powi(3) - x - 2.0, 1.0, 2.0, 1e-12, 200).expect("root");
    assert!((result.root - 1.521_379_706_804_567_6).abs() < 1e-10);
    assert!(result.residual.abs() < 1e-10);
    assert!(result.diagnostics.converged);
    assert_eq!(result.diagnostics.method, "brent-root-v1");
}

#[test]
fn brent_searches_cover_endpoint_and_varied_reference_functions() {
    let root_cases = [
        (brent_root(|x| x - 4.0, 4.0, 9.0, 1e-12, 200), 4.0),
        (brent_root(|x| 4.0 - x, 0.0, 8.0, 1e-12, 200), 4.0),
        (
            brent_root(|x| x.exp() - 2.0, 0.0, 2.0, 1e-12, 200),
            2.0_f64.ln(),
        ),
        (
            brent_root(|x| x.cos(), 1.0, 2.0, 1e-12, 200),
            std::f64::consts::FRAC_PI_2,
        ),
    ];
    for (result, expected) in root_cases {
        let result = result.expect("reference root");
        assert!((result.root - expected).abs() < 1e-10);
        assert!(result.residual.abs() < 1e-10);
        assert!(result.diagnostics.evaluations <= 202);
    }

    for expected in [-3.5, 0.0, 4.25] {
        let result = brent_minimize(|x| (x - expected).powi(2), -10.0, 10.0, 1e-12, 200)
            .expect("reference minimum");
        assert!(
            (result.argmin - expected).abs() < 1e-9,
            "expected {expected}, got {}",
            result.argmin
        );
        assert!(result.value < 1e-18);
        assert!(result.diagnostics.evaluations <= 203);
    }
}

#[test]
fn logsumexp_is_stable_at_large_logits_and_explicit_at_infinity() {
    let expected = 1000.0 + 2.0_f64.ln();
    assert!((logsumexp(&[1000.0, 1000.0]).expect("finite result") - expected).abs() < 1e-12);
    assert_eq!(
        logsumexp(&[f64::NEG_INFINITY, f64::NEG_INFINITY]).expect("zero total mass"),
        f64::NEG_INFINITY
    );
    assert_eq!(
        logsumexp(&[1.0, f64::INFINITY]).expect("dominant positive infinity"),
        f64::INFINITY
    );
}

#[test]
fn central_gradient_matches_independently_derived_partial_derivatives() {
    let gradient = central_gradient(|x| x[0].powi(3) + 2.0 * x[1].powi(2), &[2.0, -3.0], 1e-6)
        .expect("gradient");
    assert!((gradient[0] - 12.0).abs() < 1e-5);
    assert!((gradient[1] + 12.0).abs() < 1e-5);
}

#[test]
fn cholesky_pivots_distinguish_positive_definite_and_indefinite_matrices() {
    assert!(is_positive_definite(&[vec![4.0, 2.0], vec![2.0, 3.0]], 1e-12).expect("matrix"));
    assert!(
        !is_positive_definite(&[vec![1.0, 2.0], vec![2.0, 1.0]], 1e-12).expect("indefinite matrix")
    );
}

#[test]
fn invalid_nonfinite_and_unconverged_work_fails_closed() {
    assert!(matches!(
        brent_minimize(|x| x * x, 1.0, 1.0, 1e-8, 20),
        Err(NumericalError::InvalidInterval)
    ));
    assert!(matches!(
        brent_minimize(|_| f64::NAN, -1.0, 1.0, 1e-8, 20),
        Err(NumericalError::NonFiniteEvaluation)
    ));
    assert!(matches!(
        brent_root(|x| x * x + 1.0, -1.0, 1.0, 1e-8, 20),
        Err(NumericalError::InvalidBracket)
    ));
    assert!(matches!(
        brent_root(|x| x.exp() - 2.0, 0.0, 2.0, 1e-30, 1),
        Err(NumericalError::NonConverged)
    ));
    assert!(matches!(logsumexp(&[]), Err(NumericalError::EmptyInput)));
    assert!(matches!(
        logsumexp(&[0.0, f64::NAN]),
        Err(NumericalError::NonFiniteInput)
    ));
    assert!(matches!(
        central_gradient(|x| x[0], &[], 1e-6),
        Err(NumericalError::Dimension)
    ));
    assert!(matches!(
        is_positive_definite(&[vec![1.0, 2.0]], 1e-12),
        Err(NumericalError::Dimension)
    ));
    assert!(matches!(
        is_positive_definite(&[vec![1.0, f64::NAN], vec![f64::NAN, 1.0]], 1e-12),
        Err(NumericalError::NonFiniteInput)
    ));
}
