use std::{fs, path::PathBuf};

use cusp::{Controls, CuspError, real_equilibria};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootFixture {
    schema_version: u32,
    artifact_id: String,
    oracle: FixtureOracle,
    cases: Vec<RootCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureOracle {
    method: String,
    decimal_precision_digits: u32,
    implementation_independent: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootCase {
    id: String,
    alpha: String,
    beta: String,
    relative_value_tolerance: String,
    maximum_scaled_residual: String,
    roots: Vec<ExpectedRoot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedRoot {
    value: String,
    multiplicity: u8,
}

#[test]
fn committed_high_precision_cases_cover_regions_folds_and_extreme_scales() {
    let fixture = load_fixture();
    assert_eq!(fixture.schema_version, 1);
    assert_eq!(fixture.artifact_id, "fixtures/numerical/cusp_roots.json");
    assert!(fixture.oracle.implementation_independent);
    assert!(fixture.oracle.decimal_precision_digits >= 100);
    assert!(fixture.oracle.method.contains("Decimal Newton"));
    assert!(fixture.cases.len() >= 10);

    for case in fixture.cases {
        let controls = Controls::try_new(parse(&case.alpha), parse(&case.beta)).expect("controls");
        let expected_scale = case
            .roots
            .iter()
            .map(|root| parse(&root.value).abs())
            .fold(0.0_f64, f64::max)
            .max(f64::MIN_POSITIVE);
        let value_tolerance = parse(&case.relative_value_tolerance) * expected_scale;
        let maximum_scaled_residual = parse(&case.maximum_scaled_residual);
        let roots = real_equilibria(controls).unwrap_or_else(|error| {
            panic!("fixture {} failed: {error}", case.id);
        });

        assert_eq!(roots.len(), case.roots.len(), "fixture {}", case.id);
        assert!(
            roots.windows(2).all(|pair| pair[0].value < pair[1].value),
            "fixture {} roots are not strictly sorted",
            case.id
        );
        for (actual, expected) in roots.iter().zip(&case.roots) {
            let expected_value = parse(&expected.value);
            assert!(
                (actual.value - expected_value).abs() <= value_tolerance,
                "fixture {} expected {}, got {}",
                case.id,
                expected_value,
                actual.value
            );
            assert_eq!(
                actual.multiplicity, expected.multiplicity,
                "fixture {} multiplicity",
                case.id
            );
            assert!(actual.value.is_finite(), "fixture {} value", case.id);
            assert!(
                actual.residual <= maximum_scaled_residual,
                "fixture {} residual {}",
                case.id,
                actual.residual
            );
            assert!(
                actual.condition_proxy.is_finite() && actual.condition_proxy >= 0.0,
                "fixture {} condition proxy",
                case.id
            );
        }
        verify_vieta_if_all_roots_are_real(controls, &roots, &case.id);
    }
}

#[test]
fn sign_equivalent_controls_produce_negated_reverse_ordered_roots() {
    for controls in [
        Controls {
            alpha: 0.75,
            beta: 3.0,
        },
        Controls {
            alpha: 4.0,
            beta: -2.0,
        },
    ] {
        let positive = real_equilibria(controls).expect("positive controls");
        let negative = real_equilibria(Controls {
            alpha: -controls.alpha,
            beta: controls.beta,
        })
        .expect("negative controls");
        assert_eq!(positive.len(), negative.len());
        for (left, right) in positive.iter().zip(negative.iter().rev()) {
            assert!((left.value + right.value).abs() < 1e-12);
            assert_eq!(left.multiplicity, right.multiplicity);
        }
    }
}

#[test]
fn triple_root_and_invalid_controls_are_explicit() {
    let roots = real_equilibria(Controls::default()).expect("triple root");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].value, 0.0);
    assert_eq!(roots[0].multiplicity, 3);
    assert_eq!(roots[0].residual, 0.0);

    assert!(matches!(
        real_equilibria(Controls {
            alpha: f64::NAN,
            beta: 1.0,
        }),
        Err(CuspError::NonFiniteInput)
    ));
}

#[test]
fn finite_boundary_grid_never_produces_nonfinite_or_unsorted_metadata() {
    let minimum_subnormal = f64::from_bits(1);
    for controls in [
        Controls {
            alpha: minimum_subnormal,
            beta: 0.0,
        },
        Controls {
            alpha: 0.0,
            beta: minimum_subnormal,
        },
        Controls {
            alpha: 0.0,
            beta: -minimum_subnormal,
        },
        Controls {
            alpha: f64::MAX,
            beta: 0.0,
        },
        Controls {
            alpha: -f64::MAX,
            beta: 0.0,
        },
        Controls {
            alpha: 0.0,
            beta: f64::MAX,
        },
        Controls {
            alpha: 0.0,
            beta: -f64::MAX,
        },
        Controls {
            alpha: f64::MAX,
            beta: f64::MAX,
        },
        Controls {
            alpha: -f64::MAX,
            beta: f64::MAX,
        },
        Controls {
            alpha: 1e-300,
            beta: 1e300,
        },
        Controls {
            alpha: 1e300,
            beta: 1e-300,
        },
    ] {
        let roots = real_equilibria(controls).expect("finite controls");
        assert!(!roots.is_empty());
        assert!(roots.windows(2).all(|pair| pair[0].value < pair[1].value));
        assert!(roots.iter().all(|root| {
            root.value.is_finite()
                && root.residual.is_finite()
                && root.residual <= 5e-13
                && root.condition_proxy.is_finite()
                && root.condition_proxy >= 0.0
        }));
    }
}

fn load_fixture() -> RootFixture {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/numerical/cusp_roots.json");
    let bytes = fs::read(path).expect("read cusp root fixture");
    serde_json::from_slice(&bytes).expect("parse cusp root fixture")
}

fn verify_vieta_if_all_roots_are_real(
    controls: Controls,
    roots: &[cusp::EquilibriumRoot],
    case_id: &str,
) {
    let expanded: Vec<f64> = roots
        .iter()
        .flat_map(|root| std::iter::repeat_n(root.value, usize::from(root.multiplicity)))
        .collect();
    if expanded.len() != 3 {
        return;
    }
    let scale = controls.beta.abs().sqrt().max(controls.alpha.abs().cbrt());
    let normalized: Vec<f64> = expanded.iter().map(|root| root / scale).collect();
    let normalized_beta = (controls.beta / scale) / scale;
    let normalized_alpha = ((controls.alpha / scale) / scale) / scale;
    let sum = normalized.iter().sum::<f64>();
    let pair_sum = normalized[0] * normalized[1]
        + normalized[0] * normalized[2]
        + normalized[1] * normalized[2];
    let product = normalized.iter().product::<f64>();
    assert!(sum.abs() < 5e-12, "fixture {case_id} root sum {sum}");
    assert!(
        (pair_sum + normalized_beta).abs() < 5e-12,
        "fixture {case_id} pair sum {pair_sum}"
    );
    assert!(
        (product - normalized_alpha).abs() < 5e-12,
        "fixture {case_id} product {product}"
    );
}

fn parse(value: &str) -> f64 {
    value.parse::<f64>().expect("fixture decimal")
}
