use serde::{Deserialize, Serialize};

use crate::{NumericalError, OptimizationDiagnostics, validate_tolerance};

const MAXIMUM_ITERATIONS: u32 = 100_000;
const GOLDEN_SECTION: f64 = 0.381_966_011_250_105_1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScalarOptimum {
    pub argmin: f64,
    pub value: f64,
    pub diagnostics: OptimizationDiagnostics,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScalarRoot {
    pub root: f64,
    pub residual: f64,
    pub diagnostics: OptimizationDiagnostics,
}

pub fn brent_minimize<F>(
    mut objective: F,
    left: f64,
    right: f64,
    tolerance: f64,
    maximum_iterations: u32,
) -> Result<ScalarOptimum, NumericalError>
where
    F: FnMut(f64) -> f64,
{
    validate_search(left, right, tolerance, maximum_iterations)?;
    let mut evaluations = 0_u32;
    let mut evaluate = |point: f64| {
        evaluations = evaluations
            .checked_add(1)
            .ok_or(NumericalError::InvalidIterationLimit)?;
        let value = objective(point);
        if value.is_finite() {
            Ok(value)
        } else {
            Err(NumericalError::NonFiniteEvaluation)
        }
    };

    let mut lower = left;
    let mut upper = right;
    let left_value = evaluate(left)?;
    let right_value = evaluate(right)?;
    let mut current = lower + GOLDEN_SECTION * (upper - lower);
    let mut previous = current;
    let mut second_previous = current;
    let mut current_value = evaluate(current)?;
    let mut previous_value = current_value;
    let mut second_previous_value = current_value;
    let mut step = 0.0_f64;
    let mut prior_step = 0.0_f64;

    for iteration in 1..=maximum_iterations {
        let midpoint = 0.5 * (lower + upper);
        let effective_tolerance = f64::EPSILON.sqrt() * current.abs() + tolerance;
        let double_tolerance = 2.0 * effective_tolerance;
        if (current - midpoint).abs() <= double_tolerance - 0.5 * (upper - lower) {
            let (argmin, value) = [
                (left, left_value),
                (current, current_value),
                (right, right_value),
            ]
            .into_iter()
            .min_by(|left, right| left.1.total_cmp(&right.1))
            .ok_or(NumericalError::NonFiniteEvaluation)?;
            return Ok(ScalarOptimum {
                argmin,
                value,
                diagnostics: diagnostics(
                    "brent-minimize-v1",
                    iteration,
                    evaluations,
                    value,
                    tolerance,
                    "interval_tolerance",
                ),
            });
        }

        let mut used_parabolic_step = false;
        if prior_step.abs() > effective_tolerance {
            let first = (current - previous)
                * (current - previous)
                * (current_value - second_previous_value);
            let second = (current - second_previous)
                * (current - second_previous)
                * (current_value - previous_value);
            let mut numerator = first - second;
            let mut denominator = 2.0
                * ((current - previous) * (current_value - second_previous_value)
                    - (current - second_previous) * (current_value - previous_value));
            if denominator > 0.0 {
                numerator = -numerator;
            } else {
                denominator = -denominator;
            }
            let old_prior_step = prior_step;
            prior_step = step;
            if denominator > 0.0
                && numerator.abs() < 0.5 * denominator * old_prior_step.abs()
                && numerator > denominator * (lower - current)
                && numerator < denominator * (upper - current)
            {
                step = numerator / denominator;
                let candidate = current + step;
                if candidate - lower < double_tolerance || upper - candidate < double_tolerance {
                    step = effective_tolerance.copysign(midpoint - current);
                }
                used_parabolic_step = true;
            }
        }
        if !used_parabolic_step {
            prior_step = if current < midpoint {
                upper - current
            } else {
                lower - current
            };
            step = GOLDEN_SECTION * prior_step;
        }
        let direction = if step == 0.0 {
            midpoint - current
        } else {
            step
        };
        let candidate = current
            + if step.abs() >= effective_tolerance {
                step
            } else {
                effective_tolerance.copysign(direction)
            };
        let candidate_value = evaluate(candidate)?;

        if candidate_value <= current_value {
            if candidate < current {
                upper = current;
            } else {
                lower = current;
            }
            second_previous = previous;
            second_previous_value = previous_value;
            previous = current;
            previous_value = current_value;
            current = candidate;
            current_value = candidate_value;
        } else {
            if candidate < current {
                lower = candidate;
            } else {
                upper = candidate;
            }
            if candidate_value <= previous_value || previous == current {
                second_previous = previous;
                second_previous_value = previous_value;
                previous = candidate;
                previous_value = candidate_value;
            } else if candidate_value <= second_previous_value
                || second_previous == current
                || second_previous == previous
            {
                second_previous = candidate;
                second_previous_value = candidate_value;
            }
        }
    }
    Err(NumericalError::NonConverged)
}

pub fn brent_root<F>(
    mut function: F,
    left: f64,
    right: f64,
    tolerance: f64,
    maximum_iterations: u32,
) -> Result<ScalarRoot, NumericalError>
where
    F: FnMut(f64) -> f64,
{
    validate_search(left, right, tolerance, maximum_iterations)?;
    let mut evaluations = 0_u32;
    let mut evaluate = |point: f64| {
        evaluations = evaluations
            .checked_add(1)
            .ok_or(NumericalError::InvalidIterationLimit)?;
        let value = function(point);
        if value.is_finite() {
            Ok(value)
        } else {
            Err(NumericalError::NonFiniteEvaluation)
        }
    };

    let mut a = left;
    let mut b = right;
    let mut function_a = evaluate(a)?;
    let mut function_b = evaluate(b)?;
    if function_a == 0.0 {
        return root_result(a, function_a, 0, evaluations, tolerance);
    }
    if function_b == 0.0 {
        return root_result(b, function_b, 0, evaluations, tolerance);
    }
    if !opposite_sign(function_a, function_b) {
        return Err(NumericalError::InvalidBracket);
    }
    let mut c = b;
    let mut function_c = function_b;
    let mut step = b - a;
    let mut prior_step = step;

    for iteration in 1..=maximum_iterations {
        if same_sign(function_b, function_c) {
            c = a;
            function_c = function_a;
            step = b - a;
            prior_step = step;
        }
        if function_c.abs() < function_b.abs() {
            a = b;
            b = c;
            c = a;
            function_a = function_b;
            function_b = function_c;
            function_c = function_a;
        }
        let effective_tolerance = 2.0 * f64::EPSILON * b.abs() + 0.5 * tolerance;
        let midpoint = 0.5 * (c - b);
        if midpoint.abs() <= effective_tolerance || function_b == 0.0 {
            return root_result(b, function_b, iteration, evaluations, tolerance);
        }
        if prior_step.abs() >= effective_tolerance && function_a.abs() > function_b.abs() {
            let ratio = function_b / function_a;
            let (mut numerator, mut denominator) = if a == c {
                (2.0 * midpoint * ratio, 1.0 - ratio)
            } else {
                let first_ratio = function_a / function_c;
                let second_ratio = function_b / function_c;
                (
                    ratio
                        * (2.0 * midpoint * first_ratio * (first_ratio - second_ratio)
                            - (b - a) * (second_ratio - 1.0)),
                    (first_ratio - 1.0) * (second_ratio - 1.0) * (ratio - 1.0),
                )
            };
            if numerator > 0.0 {
                denominator = -denominator;
            }
            numerator = numerator.abs();
            let interval_bound =
                3.0 * midpoint * denominator - (effective_tolerance * denominator).abs();
            let history_bound = (prior_step * denominator).abs();
            if denominator != 0.0 && 2.0 * numerator < interval_bound.min(history_bound) {
                prior_step = step;
                step = numerator / denominator;
            } else {
                step = midpoint;
                prior_step = step;
            }
        } else {
            step = midpoint;
            prior_step = step;
        }
        a = b;
        function_a = function_b;
        b += if step.abs() > effective_tolerance {
            step
        } else {
            effective_tolerance.copysign(midpoint)
        };
        function_b = evaluate(b)?;
    }
    Err(NumericalError::NonConverged)
}

fn root_result(
    root: f64,
    residual: f64,
    iterations: u32,
    evaluations: u32,
    tolerance: f64,
) -> Result<ScalarRoot, NumericalError> {
    if !root.is_finite() || !residual.is_finite() {
        return Err(NumericalError::NonFiniteEvaluation);
    }
    Ok(ScalarRoot {
        root,
        residual,
        diagnostics: diagnostics(
            "brent-root-v1",
            iterations,
            evaluations,
            residual.abs(),
            tolerance,
            "bracket_tolerance",
        ),
    })
}

fn validate_search(
    left: f64,
    right: f64,
    tolerance: f64,
    maximum_iterations: u32,
) -> Result<(), NumericalError> {
    if !left.is_finite() || !right.is_finite() {
        return Err(NumericalError::NonFiniteInput);
    }
    if left >= right {
        return Err(NumericalError::InvalidInterval);
    }
    validate_tolerance(tolerance)?;
    if maximum_iterations == 0 || maximum_iterations > MAXIMUM_ITERATIONS {
        return Err(NumericalError::InvalidIterationLimit);
    }
    Ok(())
}

fn diagnostics(
    method: &str,
    iterations: u32,
    evaluations: u32,
    objective: f64,
    tolerance: f64,
    termination: &str,
) -> OptimizationDiagnostics {
    OptimizationDiagnostics {
        schema_version: 1,
        method: method.to_owned(),
        converged: true,
        iterations,
        evaluations,
        objective,
        gradient_norm: None,
        condition_number: None,
        requested_tolerance: tolerance,
        termination: termination.to_owned(),
        deterministic_seed: None,
    }
}

fn same_sign(left: f64, right: f64) -> bool {
    (left.is_sign_positive() && right.is_sign_positive())
        || (left.is_sign_negative() && right.is_sign_negative())
}

fn opposite_sign(left: f64, right: f64) -> bool {
    !same_sign(left, right)
}
