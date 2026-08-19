use regime::{
    CandidateArtifact, CandidateIneligibility, EmissionObservation, EmissionQuality,
    EmissionSequence, HmmConfig, HmmError, SelectionConfig, StateId, StudentTHmm, WalkForwardFold,
};

const PROBABILITY_TOLERANCE: f64 = 1e-10;

#[test]
fn transition_stationary_and_posteriors_are_stochastic() {
    let data = three_regime_series(0, 180, 1);
    let model = StudentTHmm::fit(&data, config(3, 42, 250, 1e-7)).expect("bounded fit");

    assert!(model.diagnostics().converged, "{:?}", model.diagnostics());
    for row in model.transition_matrix() {
        assert_probability_vector(row);
    }
    assert_probability_vector(model.initial_probabilities());
    assert_probability_vector(model.stationary_probabilities());

    let filtered = model
        .filtered_probabilities(&data)
        .expect("finite filtered probabilities");
    let smoothed = model
        .smoothed_probabilities(&data)
        .expect("finite smoothed probabilities");
    assert_eq!(filtered.len(), data.len());
    assert_eq!(smoothed.len(), data.len());
    for posterior in filtered.iter().chain(&smoothed) {
        assert_probability_vector(posterior);
    }

    assert_eq!(StateId(0).to_string(), "state_0");
    assert_eq!(StateId(2).to_string(), "state_2");
}

#[test]
fn every_supported_state_count_fits_bounded_synthetic_regimes() {
    let data = six_regime_series(0, 360, 1);
    for state_count in 3..=6 {
        let model = StudentTHmm::fit(
            &data,
            config(state_count, 100 + state_count as u64, 300, 1e-7),
        )
        .expect("supported state count fits");
        assert!(model.diagnostics().converged, "{:?}", model.diagnostics());
        assert_eq!(model.emissions().len(), state_count);
        assert_eq!(model.transition_matrix().len(), state_count);
    }
}

#[test]
fn point_in_time_boundary_rejects_bad_values_order_quality_and_schema() {
    assert_eq!(
        EmissionObservation::try_new(0, 10, 10, vec![0.0], EmissionQuality::Trusted, [1; 32]),
        Err(HmmError::InvalidObservation)
    );
    assert_eq!(
        EmissionObservation::try_new(1, 10, 9, vec![0.0], EmissionQuality::Trusted, [1; 32]),
        Err(HmmError::InvalidObservation)
    );
    assert_eq!(
        EmissionObservation::try_new(1, 10, 10, vec![f64::NAN], EmissionQuality::Trusted, [1; 32]),
        Err(HmmError::NonFiniteValue)
    );
    assert_eq!(
        EmissionObservation::try_new(1, 10, 10, vec![0.0], EmissionQuality::Unavailable, [1; 32]),
        Err(HmmError::UnavailableQuality)
    );

    let first = observation(1, 10, 10, vec![0.0, 1.0], 1);
    let second = observation(2, 20, 20, vec![0.1, 1.1], 1);
    let duplicate = observation(1, 20, 20, vec![0.1, 1.1], 1);
    assert_eq!(
        EmissionSequence::try_new(feature_schema(), vec![first.clone(), duplicate]),
        Err(HmmError::NonMonotonicObservation)
    );
    assert_eq!(
        EmissionSequence::try_new(vec!["return".into()], vec![first, second]),
        Err(HmmError::DimensionMismatch)
    );
    assert_eq!(
        HmmConfig::try_new(2, 8.0, 1e-4, 0.25, 100, 1e-7, 1),
        Err(HmmError::InvalidConfiguration)
    );
    assert_eq!(
        HmmConfig::try_new(3, 2.0, 1e-4, 0.25, 100, 1e-7, 1),
        Err(HmmError::InvalidConfiguration)
    );
    assert_eq!(
        SelectionConfig::try_new(3, 7, 8.0, 1e-4, 0.25, 100, 1e-7, 1, 0.25),
        Err(HmmError::InvalidConfiguration)
    );

    let expensive = six_regime_series(0, 1_000, 1);
    assert_eq!(
        StudentTHmm::fit(&expensive, config(6, 1, 10_000, 1e-7)),
        Err(HmmError::WorkCapacity)
    );
}

#[test]
fn evidence_identity_binds_lineage_without_changing_posterior_math() {
    let left = three_regime_series(0, 150, 1);
    let right = three_regime_series(0, 150, 2);
    let degraded = quality_sequence(&left, EmissionQuality::Degraded);
    assert_ne!(left.evidence_id(), right.evidence_id());
    assert_ne!(left.evidence_id(), degraded.evidence_id());

    let model = StudentTHmm::fit(&left, config(3, 9, 250, 1e-7)).expect("bounded fit");
    let left_inference = model.infer(&left).expect("left inference");
    let right_inference = model.infer(&right).expect("right inference");
    let degraded_inference = model.infer(&degraded).expect("degraded inference");
    assert_eq!(left_inference.filtered, right_inference.filtered);
    assert_eq!(left_inference.smoothed, right_inference.smoothed);
    assert_eq!(
        left_inference.log_likelihood,
        right_inference.log_likelihood
    );
    assert_eq!(left_inference.filtered, degraded_inference.filtered);
    assert_eq!(left_inference.smoothed, degraded_inference.smoothed);
    assert_ne!(left_inference.evidence_id, right_inference.evidence_id);
    assert_ne!(left_inference.evidence_id, degraded_inference.evidence_id);

    let extreme = EmissionSequence::try_new(
        feature_schema(),
        vec![
            observation(1, 10, 10, vec![f64::MAX, 0.0], 3),
            observation(2, 20, 20, vec![f64::MAX, 0.0], 3),
        ],
    )
    .expect("finite boundary values can still overflow model arithmetic");
    assert_eq!(model.infer(&extreme), Err(HmmError::NumericalFailure));
}

#[test]
fn deterministic_seed_and_configuration_are_bound_into_model_identity() {
    let data = three_regime_series(0, 180, 1);
    let left = StudentTHmm::fit(&data, config(3, 17, 250, 1e-7)).expect("left fit");
    let repeated = StudentTHmm::fit(&data, config(3, 17, 250, 1e-7)).expect("repeat fit");
    let alternate_seed = StudentTHmm::fit(&data, config(3, 18, 250, 1e-7)).expect("alternate fit");
    let alternate_floor = StudentTHmm::fit(
        &data,
        HmmConfig::try_new(3, 8.0, 1e-3, 0.25, 250, 1e-7, 17).expect("alternate config"),
    )
    .expect("alternate floor fit");

    assert_eq!(left, repeated);
    assert_eq!(left.model_id(), repeated.model_id());
    assert_ne!(left.model_id(), alternate_seed.model_id());
    assert_ne!(left.model_id(), alternate_floor.model_id());
}

#[test]
fn forced_iteration_exhaustion_cannot_create_candidate_artifact() {
    let data = three_regime_series(0, 180, 1);
    let model = StudentTHmm::fit(&data, config(3, 5, 1, 1e-15)).expect("inspectable fit");

    assert!(!model.diagnostics().converged);
    assert_eq!(
        CandidateArtifact::try_from(model),
        Err(HmmError::NonConvergedFit)
    );
    let diagnostic_model =
        StudentTHmm::fit(&data, config(3, 5, 1, 1e-15)).expect("repeat inspectable fit");
    assert_eq!(
        diagnostic_model.describe_states(),
        Err(HmmError::NonConvergedFit)
    );
}

#[test]
fn walk_forward_selection_uses_only_inner_validation_and_refits_final_training() {
    let fold_one = WalkForwardFold::try_new(
        three_regime_series(0, 120, 1),
        three_regime_series(120, 60, 1),
    )
    .expect("first time-ordered fold");
    let fold_two = WalkForwardFold::try_new(
        three_regime_series(0, 180, 1),
        three_regime_series(180, 60, 1),
    )
    .expect("second time-ordered fold");
    let final_training = three_regime_series(0, 240, 1);
    let selection = StudentTHmm::select(
        &[fold_one.clone(), fold_two.clone()],
        &final_training,
        selection_config(3, 4, 33),
    )
    .expect("walk-forward selection");

    assert!((3..=4).contains(&selection.report().selected_state_count));
    assert_eq!(
        selection.model().training_evidence_id(),
        final_training.evidence_id()
    );
    assert!(selection.model().diagnostics().converged);
    assert_eq!(selection.report().candidates.len(), 2);
    assert!(
        selection
            .report()
            .candidates
            .iter()
            .all(|candidate| candidate.fold_scores.len() == 2)
    );
    assert!(selection.report().candidates.iter().all(|candidate| {
        candidate.ineligibility.is_none()
            && candidate.failed_fold_index.is_none()
            && candidate.fold_scores[0].training_evidence_id == fold_one.training().evidence_id()
            && candidate.fold_scores[0].validation_evidence_id
                == fold_one.validation().evidence_id()
            && candidate
                .fold_scores
                .iter()
                .all(|score| score.diagnostics.converged)
    }));

    let alternate_final_training = three_regime_series(0, 240, 9);
    let alternate_final = StudentTHmm::select(
        &[fold_one.clone(), fold_two.clone()],
        &alternate_final_training,
        selection_config(3, 4, 33),
    )
    .expect("selection with alternate final-training lineage");
    assert_eq!(selection.report(), alternate_final.report());
    assert_ne!(
        selection.model().training_evidence_id(),
        alternate_final.model().training_evidence_id()
    );
    assert_ne!(selection.selection_id(), alternate_final.selection_id());

    let shifted_validation = WalkForwardFold::try_new(
        fold_two.training().clone(),
        offset_sequence(fold_two.validation(), 4.0, 8),
    )
    .expect("numerically shifted validation fold");
    let shifted = StudentTHmm::select(
        &[fold_one.clone(), shifted_validation],
        &final_training,
        selection_config(3, 4, 33),
    )
    .expect("selection with shifted validation values");
    assert_ne!(selection.report().candidates, shifted.report().candidates);

    let changed_validation =
        WalkForwardFold::try_new(fold_two.training().clone(), three_regime_series(180, 60, 7))
            .expect("lineage-mutated validation fold");
    let changed = StudentTHmm::select(
        &[fold_one, changed_validation],
        &final_training,
        selection_config(3, 4, 33),
    )
    .expect("selection with changed evidence");
    assert_ne!(selection.selection_id(), changed.selection_id());

    let selection_id = selection.selection_id();
    let artifact = selection
        .into_candidate()
        .expect("converged selected model promotes");
    assert_eq!(artifact.selection_id(), Some(selection_id));
    let direct = CandidateArtifact::try_from(changed.model().clone())
        .expect("converged direct model promotes");
    assert_eq!(direct.selection_id(), None);
    assert_ne!(artifact.artifact_id(), direct.artifact_id());
}

#[test]
fn one_ineligible_state_count_does_not_abort_other_candidates() {
    let fold = WalkForwardFold::try_new(
        rapid_three_regime_series(0, 15, 1),
        rapid_three_regime_series(15, 15, 1),
    )
    .expect("small time-ordered fold");
    let final_training = rapid_three_regime_series(0, 30, 1);
    let config = SelectionConfig::try_new(3, 4, 8.0, 1e-4, 0.25, 500, 1e-6, 71, 0.25)
        .expect("bounded small-fold selection config");
    let selection =
        StudentTHmm::select(&[fold], &final_training, config).expect("eligible candidate remains");

    assert_eq!(selection.report().selected_state_count, 3);
    assert!(selection.report().candidates[0].eligible);
    assert_eq!(
        selection.report().candidates[1].ineligibility,
        Some(CandidateIneligibility::InsufficientData)
    );
    assert_eq!(selection.report().candidates[1].failed_fold_index, Some(0));
}

#[test]
fn fold_boundaries_fail_closed_without_future_or_schema_leakage() {
    let training = three_regime_series(0, 120, 1);
    let overlapping = three_regime_series(100, 60, 1);
    assert_eq!(
        WalkForwardFold::try_new(training.clone(), overlapping),
        Err(HmmError::OverlappingFold)
    );

    let mut other_schema = three_regime_series(120, 60, 1);
    other_schema = EmissionSequence::try_new(
        vec!["return".into(), "funding".into()],
        other_schema.observations().to_vec(),
    )
    .expect("alternate valid schema");
    assert_eq!(
        WalkForwardFold::try_new(training, other_schema),
        Err(HmmError::SchemaMismatch)
    );

    let late_known_training = sequence_with_knowledge_lag(0, 120, 1_000_000);
    let later_validation = three_regime_series(120, 60, 1);
    assert_eq!(
        WalkForwardFold::try_new(late_known_training, later_validation),
        Err(HmmError::OverlappingFold)
    );
}

#[test]
fn descriptions_are_post_hoc_and_internal_ids_remain_neutral() {
    let data = three_regime_series(0, 180, 1);
    let model = StudentTHmm::fit(&data, config(3, 81, 250, 1e-7)).expect("bounded fit");
    let descriptions = model.describe_states().expect("post-hoc descriptions");
    assert_eq!(
        descriptions,
        model
            .describe_states()
            .expect("deterministic post-hoc descriptions")
    );

    assert_eq!(descriptions.len(), 3);
    for (index, description) in descriptions.iter().enumerate() {
        assert_eq!(description.state_id, StateId(index));
        assert_eq!(description.state_id.to_string(), format!("state_{index}"));
        assert!(!description.description.is_empty());
        assert!(!description.evidence.is_empty());
        assert_eq!(description.model_id, model.model_id());
        assert_ne!(description.description_id, [0; 32]);
    }
    assert!(
        descriptions
            .iter()
            .all(|description| !description.description.contains("liquidity_stress"))
    );
}

fn assert_probability_vector(values: &[f64]) {
    assert!(!values.is_empty());
    assert!(
        values
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0 && *value <= 1.0)
    );
    assert!(
        (values.iter().sum::<f64>() - 1.0).abs() < PROBABILITY_TOLERANCE,
        "{values:?}"
    );
}

fn config(state_count: usize, seed: u64, max_iterations: usize, tolerance: f64) -> HmmConfig {
    HmmConfig::try_new(
        state_count,
        8.0,
        1e-4,
        0.25,
        max_iterations,
        tolerance,
        seed,
    )
    .expect("valid HMM config")
}

fn selection_config(minimum_states: usize, maximum_states: usize, seed: u64) -> SelectionConfig {
    SelectionConfig::try_new(
        minimum_states,
        maximum_states,
        8.0,
        1e-4,
        0.25,
        250,
        1e-7,
        seed,
        0.25,
    )
    .expect("valid selection config")
}

fn feature_schema() -> Vec<String> {
    vec!["return".into(), "realized_volatility".into()]
}

fn three_regime_series(start: usize, count: usize, lineage_byte: u8) -> EmissionSequence {
    let observations = (start..start + count)
        .map(|index| {
            let block = (index / 20) % 3;
            let within = (index % 20) as f64;
            let noise = (within - 9.5) / 100.0;
            let values = match block {
                0 => vec![-2.0 + noise, 0.25 - noise / 2.0],
                1 => vec![0.0 + noise, 1.5 + noise],
                _ => vec![2.0 + noise, -0.5 + noise / 2.0],
            };
            let id = index as u64 + 1;
            let event_time_ns = (index as i64 + 1) * 10;
            observation(id, event_time_ns, event_time_ns, values, lineage_byte)
        })
        .collect();
    EmissionSequence::try_new(feature_schema(), observations).expect("valid fixture sequence")
}

fn six_regime_series(start: usize, count: usize, lineage_byte: u8) -> EmissionSequence {
    let centers = [
        [-3.0, -1.0],
        [-2.0, 1.0],
        [-0.5, -2.0],
        [0.5, 2.0],
        [2.0, -1.0],
        [3.0, 1.0],
    ];
    let observations = (start..start + count)
        .map(|index| {
            let center = centers[(index / 20) % centers.len()];
            let noise = ((index % 20) as f64 - 9.5) / 200.0;
            let id = index as u64 + 1;
            let event_time_ns = (index as i64 + 1) * 10;
            observation(
                id,
                event_time_ns,
                event_time_ns,
                vec![center[0] + noise, center[1] - noise],
                lineage_byte,
            )
        })
        .collect();
    EmissionSequence::try_new(feature_schema(), observations).expect("valid six-regime sequence")
}

fn rapid_three_regime_series(start: usize, count: usize, lineage_byte: u8) -> EmissionSequence {
    let centers = [[-2.0, 0.0], [0.0, 2.0], [2.0, -1.0]];
    let observations = (start..start + count)
        .map(|index| {
            let center = centers[(index / 5) % centers.len()];
            let noise = ((index % 5) as f64 - 2.0) / 100.0;
            let id = index as u64 + 1;
            let event_time_ns = (index as i64 + 1) * 10;
            observation(
                id,
                event_time_ns,
                event_time_ns,
                vec![center[0] + noise, center[1] - noise],
                lineage_byte,
            )
        })
        .collect();
    EmissionSequence::try_new(feature_schema(), observations).expect("valid rapid-regime sequence")
}

fn observation(
    id: u64,
    event_time_ns: i64,
    as_known_at_ns: i64,
    values: Vec<f64>,
    lineage_byte: u8,
) -> EmissionObservation {
    EmissionObservation::try_new(
        id,
        event_time_ns,
        as_known_at_ns,
        values,
        EmissionQuality::Trusted,
        [lineage_byte; 32],
    )
    .expect("valid fixture observation")
}

fn sequence_with_knowledge_lag(
    start: usize,
    count: usize,
    final_as_known_at_ns: i64,
) -> EmissionSequence {
    let mut base = three_regime_series(start, count, 1).observations().to_vec();
    let last = base.len() - 1;
    base[last] = observation(
        base[last].id(),
        base[last].event_time_ns(),
        final_as_known_at_ns,
        base[last].values().to_vec(),
        1,
    );
    EmissionSequence::try_new(feature_schema(), base).expect("monotonic late-known sequence")
}

fn offset_sequence(sequence: &EmissionSequence, offset: f64, lineage_byte: u8) -> EmissionSequence {
    let observations = sequence
        .observations()
        .iter()
        .map(|source| {
            observation(
                source.id(),
                source.event_time_ns(),
                source.as_known_at_ns(),
                source.values().iter().map(|value| value + offset).collect(),
                lineage_byte,
            )
        })
        .collect();
    EmissionSequence::try_new(feature_schema(), observations).expect("valid offset sequence")
}

fn quality_sequence(sequence: &EmissionSequence, quality: EmissionQuality) -> EmissionSequence {
    let observations = sequence
        .observations()
        .iter()
        .map(|source| {
            EmissionObservation::try_new(
                source.id(),
                source.event_time_ns(),
                source.as_known_at_ns(),
                source.values().to_vec(),
                quality,
                source.lineage_hash(),
            )
            .expect("available fixture quality")
        })
        .collect();
    EmissionSequence::try_new(feature_schema(), observations)
        .expect("valid quality-mutated sequence")
}
