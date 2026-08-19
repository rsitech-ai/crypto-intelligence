use super::{FitDiagnostics, FitError, FitProblem, FitTermination, penalty};

#[derive(Clone, Debug)]
pub(super) struct OptimizationResult {
    pub parameters: Vec<f64>,
    pub diagnostics: FitDiagnostics,
}

pub(super) fn optimize(problem: &FitProblem) -> Result<OptimizationResult, FitError> {
    let mut parameters = problem.initial_parameters();
    let mut evaluations = 0_u64;
    let mut work = problem.preflight_work_units;
    let work_per_evaluation = problem.evaluation_work_units()?;
    let mut evaluate = |point: &[f64]| {
        work = work
            .checked_add(work_per_evaluation)
            .ok_or(FitError::WorkCapacity)?;
        if work > problem.config.optimizer.work_limit {
            return Err(FitError::WorkCapacity);
        }
        evaluations = evaluations.checked_add(1).ok_or(FitError::WorkCapacity)?;
        problem.smooth_objective(point)
    };

    let initial_smooth = evaluate(&parameters)?;
    let mut smooth_value = initial_smooth.value;
    let mut gradient = initial_smooth.gradient;
    let mut objective = penalty::composite_value(
        smooth_value,
        &parameters,
        &problem.layout,
        problem.config.penalty,
    )?;
    if !problem.condition_estimate.is_finite()
        || problem.condition_estimate > problem.config.optimizer.max_condition_number
    {
        return Ok(OptimizationResult {
            parameters,
            diagnostics: FitDiagnostics {
                estimator: problem.config.estimator,
                role: problem.config.estimator.role(),
                objective,
                proximal_gradient_norm: f64::INFINITY,
                iterations: 0,
                evaluations,
                backtracking_steps: 0,
                condition_estimate: problem.condition_estimate,
                converged: false,
                termination: FitTermination::IllConditioned,
            },
        });
    }

    let mut proximal_gradient_norm = f64::INFINITY;
    let mut iterations = 0_u32;
    let mut backtracking_steps = 0_u64;
    let mut next_step = problem.config.optimizer.initial_step;
    let mut termination = FitTermination::IterationLimit;
    let mut converged = false;

    'iterations: for iteration in 1..=problem.config.optimizer.max_iterations {
        let mut step = next_step;
        let mut accepted = None;
        for _ in 0..problem.config.optimizer.max_backtracking {
            let candidate = penalty::proximal_step(
                &parameters,
                &gradient,
                step,
                &problem.layout,
                problem.config.penalty,
            )?;
            let delta = candidate
                .iter()
                .zip(&parameters)
                .map(|(next, current)| next - current)
                .collect::<Vec<_>>();
            let candidate_smooth = match evaluate(&candidate) {
                Ok(value) => value,
                Err(FitError::WorkCapacity) => {
                    termination = FitTermination::WorkLimit;
                    break 'iterations;
                }
                Err(FitError::NonFiniteObjective | FitError::IntegrationBoundary) => {
                    backtracking_steps = backtracking_steps
                        .checked_add(1)
                        .ok_or(FitError::WorkCapacity)?;
                    step *= 0.5;
                    if step < problem.config.optimizer.minimum_step {
                        break;
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            let squared_delta = delta.iter().map(|value| value * value).sum::<f64>();
            let gradient_dot_delta = gradient
                .iter()
                .zip(&delta)
                .map(|(derivative, change)| derivative * change)
                .sum::<f64>();
            let majorizer = smooth_value + gradient_dot_delta + squared_delta / (2.0 * step);
            if candidate_smooth.value <= majorizer + 1.0e-12 {
                accepted = Some((candidate, candidate_smooth, squared_delta, step));
                break;
            }
            backtracking_steps = backtracking_steps
                .checked_add(1)
                .ok_or(FitError::WorkCapacity)?;
            step *= 0.5;
            if step < problem.config.optimizer.minimum_step {
                break;
            }
        }

        let Some((candidate, candidate_smooth, squared_delta, accepted_step)) = accepted else {
            if termination != FitTermination::WorkLimit {
                termination = FitTermination::LineSearchFailure;
            }
            break;
        };
        let candidate_objective = penalty::composite_value(
            candidate_smooth.value,
            &candidate,
            &problem.layout,
            problem.config.penalty,
        )?;
        if candidate_objective > objective + 1.0e-10 {
            termination = FitTermination::LineSearchFailure;
            break;
        }

        proximal_gradient_norm = squared_delta.sqrt() / accepted_step;
        let improvement = (objective - candidate_objective).abs();
        parameters = candidate;
        objective = candidate_objective;
        smooth_value = candidate_smooth.value;
        gradient = candidate_smooth.gradient;
        iterations = iteration;
        let parameter_scale = parameters
            .iter()
            .map(|value| value * value)
            .sum::<f64>()
            .sqrt()
            .max(1.0);
        let objective_scale = objective.abs().max(1.0);
        if proximal_gradient_norm <= problem.config.optimizer.tolerance * parameter_scale
            && improvement <= problem.config.optimizer.tolerance * objective_scale
        {
            converged = true;
            termination = FitTermination::Converged;
            break;
        }
        next_step = (accepted_step * 1.25).min(problem.config.optimizer.initial_step);
    }

    Ok(OptimizationResult {
        parameters,
        diagnostics: FitDiagnostics {
            estimator: problem.config.estimator,
            role: problem.config.estimator.role(),
            objective,
            proximal_gradient_norm,
            iterations,
            evaluations,
            backtracking_steps,
            condition_estimate: problem.condition_estimate,
            converged,
            termination,
        },
    })
}
