use cusp::{Controls, CuspError, EquilibriumTopology, Potential, Stability, analyze_equilibria};

#[test]
fn three_root_region_has_two_stable_branches_and_positive_adjacent_barriers() {
    let set = analyze_equilibria(Controls {
        alpha: 0.0,
        beta: 3.0,
    })
    .expect("three-root analysis");

    assert_eq!(set.topology, EquilibriumTopology::ThreeBranches);
    assert_eq!(set.roots.len(), 3);
    assert_eq!(
        set.roots
            .iter()
            .filter(|root| root.stability == Stability::Stable)
            .count(),
        2
    );
    assert_eq!(
        set.roots
            .iter()
            .filter(|root| root.stability == Stability::Unstable)
            .count(),
        1
    );
    assert_eq!(set.barriers.len(), 2);
    assert!(set.barriers.iter().all(|barrier| barrier.height > 0.0));
    assert!(
        set.roots
            .iter()
            .filter(|root| root.stability == Stability::Stable)
            .all(|root| root.restoring_force.is_some_and(|force| force > 0.0))
    );
    assert!(
        set.roots
            .iter()
            .filter(|root| root.stability != Stability::Stable)
            .all(|root| root.restoring_force.is_none())
    );
    assert_close(set.minimum_barrier().expect("minimum barrier"), 2.25, 1e-13);
}

#[test]
fn barrier_tends_to_zero_near_fold_and_is_absent_at_or_outside_it() {
    let far = analyze_equilibria(Controls {
        alpha: 0.0,
        beta: 3.0,
    })
    .expect("far from fold")
    .minimum_barrier()
    .expect("far barrier");
    for sign in [-1.0_f64, 1.0_f64] {
        let near = analyze_equilibria(Controls {
            alpha: sign * 1.99,
            beta: 3.0,
        })
        .expect("near fold");
        assert_eq!(near.topology, EquilibriumTopology::ThreeBranches);
        assert!(near.minimum_barrier().is_some_and(|height| height < far));

        let fold = analyze_equilibria(Controls {
            alpha: sign * 2.0,
            beta: 3.0,
        })
        .expect("exact fold");
        assert_eq!(fold.topology, EquilibriumTopology::Fold);
        assert!(fold.barriers.is_empty());
        assert!(fold.minimum_barrier().is_none());
        assert_eq!(
            fold.roots
                .iter()
                .filter(|root| root.stability == Stability::NeutralAtTolerance)
                .count(),
            1
        );

        let outside = analyze_equilibria(Controls {
            alpha: sign * 2.01,
            beta: 3.0,
        })
        .expect("outside fold");
        assert_eq!(outside.topology, EquilibriumTopology::OneStable);
        assert!(outside.barriers.is_empty());
        assert!(outside.minimum_barrier().is_none());
    }
}

#[test]
fn barriers_match_independent_potential_differences_for_asymmetric_controls() {
    let controls = Controls {
        alpha: 0.5,
        beta: 3.0,
    };
    let set = analyze_equilibria(controls).expect("asymmetric analysis");
    assert_eq!(set.topology, EquilibriumTopology::ThreeBranches);

    for barrier in &set.barriers {
        let expected = Potential::checked_value(barrier.unstable_root, controls)
            .expect("unstable potential")
            - Potential::checked_value(barrier.stable_root, controls).expect("stable potential");
        assert!(expected > 0.0);
        assert_close(barrier.height, expected, 2e-14);
    }
}

#[test]
fn topology_and_restoring_force_are_explicit_for_single_and_critical_roots() {
    let one = analyze_equilibria(Controls {
        alpha: 4.0,
        beta: -2.0,
    })
    .expect("one-root analysis");
    assert_eq!(one.topology, EquilibriumTopology::OneStable);
    assert_eq!(one.roots.len(), 1);
    assert_eq!(one.roots[0].stability, Stability::Stable);
    assert!(
        one.roots[0]
            .restoring_force
            .is_some_and(|force| force > 0.0)
    );
    assert!(one.barriers.is_empty());

    let critical = analyze_equilibria(Controls::default()).expect("critical analysis");
    assert_eq!(critical.topology, EquilibriumTopology::Critical);
    assert_eq!(critical.roots.len(), 1);
    assert_eq!(critical.roots[0].equilibrium.multiplicity, 3);
    assert_eq!(critical.roots[0].stability, Stability::NeutralAtTolerance);
    assert_eq!(critical.roots[0].hessian, 0.0);
    assert!(critical.roots[0].restoring_force.is_none());
    assert!(critical.barriers.is_empty());
}

#[test]
fn state_rescaling_preserves_topology_and_expected_physical_units() {
    let baseline = analyze_equilibria(Controls {
        alpha: 0.0,
        beta: 3.0,
    })
    .expect("baseline");

    for state_scale in [1e-50_f64, 1e50_f64] {
        let scaled = analyze_equilibria(Controls {
            alpha: 0.0,
            beta: 3.0 * state_scale.powi(2),
        })
        .expect("scaled analysis");
        assert_eq!(scaled.topology, EquilibriumTopology::ThreeBranches);

        for (base_root, scaled_root) in baseline.roots.iter().zip(&scaled.roots) {
            assert_relative(
                scaled_root.equilibrium.value,
                base_root.equilibrium.value * state_scale,
                2e-14,
            );
            assert_relative(
                scaled_root.hessian,
                base_root.hessian * state_scale.powi(2),
                2e-14,
            );
        }
        for (base_barrier, scaled_barrier) in baseline.barriers.iter().zip(&scaled.barriers) {
            assert_relative(
                scaled_barrier.height,
                base_barrier.height * state_scale.powi(4),
                3e-14,
            );
        }
    }
}

#[test]
fn exact_fold_topology_is_stable_across_representable_state_scales() {
    for state_scale in [1e-75_f64, 1e-25_f64, 1.0_f64, 1e25_f64, 1e75_f64] {
        for sign in [-1.0_f64, 1.0_f64] {
            let fold = analyze_equilibria(Controls {
                alpha: sign * 2.0 * state_scale.powi(3),
                beta: 3.0 * state_scale.powi(2),
            })
            .expect("scaled fold");
            assert_eq!(fold.topology, EquilibriumTopology::Fold);
            assert_eq!(fold.roots.len(), 2);
            assert!(fold.barriers.is_empty());
        }
    }
}

#[test]
fn deterministic_control_grid_preserves_topology_and_barrier_invariants() {
    for beta in [1e-6_f64, 1e-2_f64, 1.0_f64, 1e2_f64, 1e6_f64] {
        let fold_alpha = 2.0 * (beta / 3.0).sqrt().powi(3);
        for fold_ratio in [-1.01_f64, -1.0, -0.99, -0.5, 0.0, 0.5, 0.99, 1.0, 1.01] {
            let controls = Controls {
                alpha: fold_ratio * fold_alpha,
                beta,
            };
            let set = analyze_equilibria(controls).expect("grid analysis");
            let expected_topology = if fold_ratio.abs() < 1.0 {
                EquilibriumTopology::ThreeBranches
            } else if fold_ratio.abs() == 1.0 {
                EquilibriumTopology::Fold
            } else {
                EquilibriumTopology::OneStable
            };
            assert_eq!(set.topology, expected_topology);
            assert!(
                set.roots
                    .windows(2)
                    .all(|pair| { pair[0].equilibrium.value < pair[1].equilibrium.value })
            );

            if expected_topology == EquilibriumTopology::ThreeBranches {
                assert_eq!(set.barriers.len(), 2);
                for barrier in &set.barriers {
                    let independent = Potential::value(barrier.unstable_root, controls)
                        - Potential::value(barrier.stable_root, controls);
                    assert!(independent > 0.0);
                    assert_relative(barrier.height, independent, 2e-11);
                }
            } else {
                assert!(set.barriers.is_empty());
            }
        }
    }
}

#[test]
fn invalid_or_unrepresentable_derived_values_fail_closed() {
    assert!(matches!(
        analyze_equilibria(Controls {
            alpha: f64::NAN,
            beta: 1.0,
        }),
        Err(CuspError::NonFiniteInput)
    ));
    assert!(matches!(
        analyze_equilibria(Controls {
            alpha: 0.0,
            beta: f64::MAX,
        }),
        Err(CuspError::NonFiniteDerivedValue)
    ));
}

fn assert_close(actual: f64, expected: f64, absolute_tolerance: f64) {
    assert!(
        (actual - expected).abs() <= absolute_tolerance,
        "expected {expected}, got {actual}"
    );
}

fn assert_relative(actual: f64, expected: f64, relative_tolerance: f64) {
    let scale = expected.abs().max(f64::MIN_POSITIVE);
    assert!(
        (actual - expected).abs() <= relative_tolerance * scale,
        "expected {expected}, got {actual}"
    );
}
