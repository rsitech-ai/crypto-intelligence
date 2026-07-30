use cusp::{AlternativeControls, Controls, CuspError, CuspState, Potential};

const REFERENCE_TOLERANCE: f64 = 1e-10;
const FINITE_DIFFERENCE_STEP: f64 = 1e-4;

#[test]
fn potential_derivatives_match_the_approved_product_convention() {
    let controls = Controls {
        alpha: 2.0,
        beta: 3.0,
    };
    let state = 1.5_f64;
    let expected_value =
        0.25 * state.powi(4) - 0.5 * controls.beta * state.powi(2) - controls.alpha * state;
    assert!((Potential::value(state, controls) - expected_value).abs() < f64::EPSILON);
    assert!(
        (Potential::gradient(state, controls)
            - (state.powi(3) - controls.beta * state - controls.alpha))
            .abs()
            < f64::EPSILON
    );
    assert!(
        (Potential::hessian(state, controls) - (3.0 * state.powi(2) - controls.beta)).abs()
            < f64::EPSILON
    );
}

#[test]
fn analytic_derivatives_match_independent_finite_differences() {
    for (state, controls) in [
        (
            -2.0,
            Controls {
                alpha: -0.75,
                beta: 1.25,
            },
        ),
        (
            0.25,
            Controls {
                alpha: 2.0,
                beta: -3.0,
            },
        ),
        (
            1.5,
            Controls {
                alpha: -2.0,
                beta: 3.0,
            },
        ),
    ] {
        let numeric_gradient = five_point_derivative(
            |point| Potential::value(point, controls),
            state,
            FINITE_DIFFERENCE_STEP,
        );
        assert!(
            (numeric_gradient - Potential::gradient(state, controls)).abs() < REFERENCE_TOLERANCE
        );

        let numeric_hessian = five_point_derivative(
            |point| Potential::gradient(point, controls),
            state,
            FINITE_DIFFERENCE_STEP,
        );
        assert!(
            (numeric_hessian - Potential::hessian(state, controls)).abs() < REFERENCE_TOLERANCE
        );
    }
}

fn five_point_derivative(function: impl Fn(f64) -> f64, point: f64, step: f64) -> f64 {
    (-function(point + 2.0 * step) + 8.0 * function(point + step) - 8.0 * function(point - step)
        + function(point - 2.0 * step))
        / (12.0 * step)
}

#[test]
fn cusp_region_is_strict_and_fold_boundaries_are_outside() {
    assert!(
        Controls {
            alpha: 0.0,
            beta: 1.0
        }
        .inside_cusp()
    );
    assert!(
        !Controls {
            alpha: 0.0,
            beta: -1.0
        }
        .inside_cusp()
    );
    for alpha in [-2.0, 2.0] {
        let fold = Controls { alpha, beta: 3.0 };
        assert_eq!(fold.checked_discriminant().expect("fold"), 0.0);
        assert!(!fold.inside_cusp());
    }
}

#[test]
fn alternative_research_convention_requires_an_explicit_sign_mapping() {
    let product = Controls {
        alpha: 1.25,
        beta: 2.5,
    };
    let alternative = AlternativeControls::from(product);
    assert_eq!(alternative.a, -product.beta);
    assert_eq!(alternative.b, -product.alpha);
    assert_eq!(Controls::from(alternative), product);

    for state in [-1.0_f64, 0.0, 2.0] {
        let alternative_value =
            0.25 * state.powi(4) + 0.5 * alternative.a * state.powi(2) + alternative.b * state;
        let alternative_gradient = state.powi(3) + alternative.a * state + alternative.b;
        assert_eq!(Potential::value(state, product), alternative_value);
        assert_eq!(Potential::gradient(state, product), alternative_gradient);
    }
}

#[test]
fn checked_boundaries_reject_nonfinite_inputs_and_derived_overflow() {
    assert!(matches!(
        Controls::try_new(f64::NAN, 1.0),
        Err(CuspError::NonFiniteInput)
    ));
    let overflowing = Controls {
        alpha: f64::MAX,
        beta: f64::MAX,
    };
    assert!(matches!(
        overflowing.checked_discriminant(),
        Err(CuspError::NonFiniteDerivedValue)
    ));
    assert!(!overflowing.inside_cusp());
    assert!(matches!(
        Potential::checked_value(f64::MAX, Controls::default()),
        Err(CuspError::NonFiniteDerivedValue)
    ));
}

#[test]
fn cusp_state_is_derived_and_deserialization_revalidates_it() {
    let state = CuspState::evaluate(
        -0.5,
        Controls {
            alpha: 0.25,
            beta: 2.0,
        },
    )
    .expect("valid mathematical state");
    assert_eq!(state.schema_version, 1);
    assert_eq!(
        state.potential,
        Potential::value(state.normalized_state, state.controls)
    );
    assert_eq!(
        state.gradient,
        Potential::gradient(state.normalized_state, state.controls)
    );
    assert_eq!(
        state.hessian,
        Potential::hessian(state.normalized_state, state.controls)
    );
    assert_eq!(
        state.discriminant,
        state.controls.checked_discriminant().expect("discriminant")
    );
    assert_eq!(state.inside_cusp, state.controls.inside_cusp());

    let encoded = serde_json::to_vec(&state).expect("serialize");
    let decoded: CuspState = serde_json::from_slice(&encoded).expect("validated round trip");
    assert_eq!(decoded, state);

    let mut forged = serde_json::to_value(state).expect("value");
    forged["gradient"] = serde_json::json!(999.0);
    assert!(serde_json::from_value::<CuspState>(forged).is_err());
}
