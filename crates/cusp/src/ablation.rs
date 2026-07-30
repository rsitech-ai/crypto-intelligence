//! Predeclared walk-forward gate for cusp production-weight eligibility.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_FOLDS: usize = 256;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_OBSERVATIONS_PER_FOLD: u64 = 1_000_000_000;
const MAX_ATTEMPTED_MODEL_VARIANTS: u32 = 100_000;
const PARTS_PER_MILLION: f64 = 1_000_000.0;

pub const ABLATION_REPORT_SCHEMA_VERSION: u32 = 1;
pub const CUSP_GATE_POLICY_SCHEMA_VERSION: u32 = 1;
pub const GATE_EVALUATION_SCHEMA_VERSION: u32 = 1;

/// Required non-cusp controls for the incremental comparison.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum RequiredBaseline {
    Volatility = 1,
    Leverage = 2,
    Regime = 3,
    Microstructure = 4,
}

impl RequiredBaseline {
    const ALL: [Self; 4] = [
        Self::Volatility,
        Self::Leverage,
        Self::Regime,
        Self::Microstructure,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Volatility => "volatility",
            Self::Leverage => "leverage",
            Self::Regime => "regime",
            Self::Microstructure => "microstructure",
        }
    }
}

/// Untrusted metrics for one untouched outer test fold.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FoldMetricInput {
    pub fold_id: String,
    pub regime_id: String,
    pub test_start_ns: i64,
    pub test_end_ns: i64,
    pub observation_count: u64,
    pub positive_event_count: u32,
    pub independent_event_episode_count: u32,
    pub baseline_brier_score: f64,
    pub candidate_brier_score: f64,
    pub calibration_slope: f64,
    pub calibration_intercept: f64,
    pub expected_calibration_error: f64,
    pub alert_budget_score_delta: f64,
}

/// Validated metrics for one chronologically ordered outer test fold.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FoldMetric {
    fold_id: String,
    regime_id: String,
    test_start_ns: i64,
    test_end_ns: i64,
    observation_count: u64,
    positive_event_count: u32,
    independent_event_episode_count: u32,
    baseline_brier_score: f64,
    candidate_brier_score: f64,
    calibration_slope: f64,
    calibration_intercept: f64,
    expected_calibration_error: f64,
    alert_budget_score_delta: f64,
}

impl FoldMetric {
    fn try_new(input: FoldMetricInput) -> Result<Self, AblationError> {
        validate_identifier(&input.fold_id)?;
        validate_identifier(&input.regime_id)?;
        if input.test_start_ns <= 0 || input.test_end_ns <= input.test_start_ns {
            return Err(AblationError::InvalidFoldPeriod);
        }
        if input.observation_count == 0
            || input.observation_count > MAX_OBSERVATIONS_PER_FOLD
            || u64::from(input.positive_event_count) > input.observation_count
            || input.independent_event_episode_count > input.positive_event_count
        {
            return Err(AblationError::InvalidCount);
        }
        let finite = [
            input.baseline_brier_score,
            input.candidate_brier_score,
            input.calibration_slope,
            input.calibration_intercept,
            input.expected_calibration_error,
            input.alert_budget_score_delta,
        ]
        .iter()
        .all(|value| value.is_finite());
        if !finite {
            return Err(AblationError::NonFiniteMetric);
        }
        if !(0.0..=1.0).contains(&input.candidate_brier_score)
            || !(0.0..=1.0).contains(&input.baseline_brier_score)
            || input.baseline_brier_score == 0.0
            || !(0.0..=1.0).contains(&input.expected_calibration_error)
            || !(-1.0..=1.0).contains(&input.alert_budget_score_delta)
        {
            return Err(AblationError::MetricOutOfRange);
        }
        Ok(Self {
            fold_id: input.fold_id,
            regime_id: input.regime_id,
            test_start_ns: input.test_start_ns,
            test_end_ns: input.test_end_ns,
            observation_count: input.observation_count,
            positive_event_count: input.positive_event_count,
            independent_event_episode_count: input.independent_event_episode_count,
            baseline_brier_score: input.baseline_brier_score,
            candidate_brier_score: input.candidate_brier_score,
            calibration_slope: input.calibration_slope,
            calibration_intercept: input.calibration_intercept,
            expected_calibration_error: input.expected_calibration_error,
            alert_budget_score_delta: input.alert_budget_score_delta,
        })
    }

    pub fn fold_id(&self) -> &str {
        &self.fold_id
    }

    pub fn regime_id(&self) -> &str {
        &self.regime_id
    }

    pub const fn test_start_ns(&self) -> i64 {
        self.test_start_ns
    }

    pub const fn test_end_ns(&self) -> i64 {
        self.test_end_ns
    }

    pub const fn observation_count(&self) -> u64 {
        self.observation_count
    }

    pub const fn positive_event_count(&self) -> u32 {
        self.positive_event_count
    }

    pub const fn independent_event_episode_count(&self) -> u32 {
        self.independent_event_episode_count
    }

    pub const fn baseline_brier_score(&self) -> f64 {
        self.baseline_brier_score
    }

    pub const fn candidate_brier_score(&self) -> f64 {
        self.candidate_brier_score
    }

    pub const fn calibration_slope(&self) -> f64 {
        self.calibration_slope
    }

    pub const fn calibration_intercept(&self) -> f64 {
        self.calibration_intercept
    }

    pub const fn expected_calibration_error(&self) -> f64 {
        self.expected_calibration_error
    }

    pub const fn alert_budget_score_delta(&self) -> f64 {
        self.alert_budget_score_delta
    }

    fn brier_skill(&self) -> f64 {
        1.0 - self.candidate_brier_score / self.baseline_brier_score
    }

    fn improved(&self) -> bool {
        self.candidate_brier_score < self.baseline_brier_score
    }
}

/// Untrusted, serialized walk-forward report input.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AblationReportInput {
    pub schema_version: u32,
    pub dataset_manifest_hash: [u8; 32],
    pub candidate_model_hash: [u8; 32],
    pub baseline_model_hash: [u8; 32],
    pub required_baselines: Vec<RequiredBaseline>,
    pub folds: Vec<FoldMetricInput>,
    pub maximum_standardized_coefficient_drift: f64,
    pub sign_scaling_parity: bool,
    pub numerical_failures: u32,
    pub attempted_model_variants: u32,
}

/// Validated decision evidence. Aggregate metrics are derived, not caller supplied.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AblationReport {
    schema_version: u32,
    dataset_manifest_hash: [u8; 32],
    candidate_model_hash: [u8; 32],
    baseline_model_hash: [u8; 32],
    required_baselines: Vec<RequiredBaseline>,
    folds: Vec<FoldMetric>,
    maximum_standardized_coefficient_drift: f64,
    sign_scaling_parity: bool,
    numerical_failures: u32,
    attempted_model_variants: u32,
    evidence_hash: [u8; 32],
}

impl AblationReport {
    pub fn try_new(input: AblationReportInput) -> Result<Self, AblationError> {
        if input.schema_version != ABLATION_REPORT_SCHEMA_VERSION {
            return Err(AblationError::UnsupportedSchema {
                found: input.schema_version,
            });
        }
        if [
            input.dataset_manifest_hash,
            input.candidate_model_hash,
            input.baseline_model_hash,
        ]
        .iter()
        .any(|hash| hash.iter().all(|byte| *byte == 0))
        {
            return Err(AblationError::InvalidIdentity);
        }
        if input.candidate_model_hash == input.baseline_model_hash {
            return Err(AblationError::InvalidIdentity);
        }
        if input.folds.is_empty() || input.folds.len() > MAX_FOLDS {
            return Err(AblationError::FoldCapacity);
        }
        if !input.maximum_standardized_coefficient_drift.is_finite() {
            return Err(AblationError::NonFiniteMetric);
        }
        if input.maximum_standardized_coefficient_drift < 0.0 {
            return Err(AblationError::MetricOutOfRange);
        }
        if input.attempted_model_variants == 0
            || input.attempted_model_variants > MAX_ATTEMPTED_MODEL_VARIANTS
        {
            return Err(AblationError::InvalidCount);
        }

        let mut required_baselines = input.required_baselines;
        required_baselines.sort_unstable();
        if required_baselines.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AblationError::DuplicateBaseline);
        }

        let folds = input
            .folds
            .into_iter()
            .map(FoldMetric::try_new)
            .collect::<Result<Vec<_>, _>>()?;
        let mut fold_ids = BTreeSet::new();
        if folds
            .iter()
            .any(|fold| !fold_ids.insert(fold.fold_id.clone()))
        {
            return Err(AblationError::DuplicateFold);
        }
        if folds
            .windows(2)
            .any(|pair| pair[0].test_end_ns > pair[1].test_start_ns)
        {
            return Err(AblationError::NonChronologicalFolds);
        }

        let mut report = Self {
            schema_version: input.schema_version,
            dataset_manifest_hash: input.dataset_manifest_hash,
            candidate_model_hash: input.candidate_model_hash,
            baseline_model_hash: input.baseline_model_hash,
            required_baselines,
            folds,
            maximum_standardized_coefficient_drift: input.maximum_standardized_coefficient_drift,
            sign_scaling_parity: input.sign_scaling_parity,
            numerical_failures: input.numerical_failures,
            attempted_model_variants: input.attempted_model_variants,
            evidence_hash: [0; 32],
        };
        report.evidence_hash = report.calculate_evidence_hash();
        Ok(report)
    }

    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn dataset_manifest_hash(&self) -> [u8; 32] {
        self.dataset_manifest_hash
    }

    pub const fn candidate_model_hash(&self) -> [u8; 32] {
        self.candidate_model_hash
    }

    pub const fn baseline_model_hash(&self) -> [u8; 32] {
        self.baseline_model_hash
    }

    pub fn required_baselines(&self) -> &[RequiredBaseline] {
        &self.required_baselines
    }

    pub fn folds(&self) -> &[FoldMetric] {
        &self.folds
    }

    pub const fn maximum_standardized_coefficient_drift(&self) -> f64 {
        self.maximum_standardized_coefficient_drift
    }

    pub const fn sign_scaling_parity(&self) -> bool {
        self.sign_scaling_parity
    }

    pub const fn numerical_failures(&self) -> u32 {
        self.numerical_failures
    }

    pub const fn attempted_model_variants(&self) -> u32 {
        self.attempted_model_variants
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }

    fn aggregate_brier_skill(&self) -> f64 {
        let (candidate, baseline, observations) =
            self.folds
                .iter()
                .fold((0.0, 0.0, 0_u64), |aggregate, fold| {
                    (
                        fold.candidate_brier_score
                            .mul_add(fold.observation_count as f64, aggregate.0),
                        fold.baseline_brier_score
                            .mul_add(fold.observation_count as f64, aggregate.1),
                        aggregate.2 + fold.observation_count,
                    )
                });
        let count = observations as f64;
        1.0 - (candidate / count) / (baseline / count)
    }

    fn aggregate_alert_budget_delta(&self) -> f64 {
        let (weighted, observations) = self.folds.iter().fold((0.0, 0_u64), |aggregate, fold| {
            (
                fold.alert_budget_score_delta
                    .mul_add(fold.observation_count as f64, aggregate.0),
                aggregate.1 + fold.observation_count,
            )
        });
        weighted / observations as f64
    }

    fn calculate_evidence_hash(&self) -> [u8; 32] {
        let mut evidence = CanonicalEvidence::new(b"cusp-ablation-report-v1");
        evidence.u32(self.schema_version);
        evidence.bytes(&self.dataset_manifest_hash);
        evidence.bytes(&self.candidate_model_hash);
        evidence.bytes(&self.baseline_model_hash);
        evidence.len(self.required_baselines.len());
        for baseline in &self.required_baselines {
            evidence.u8(*baseline as u8);
        }
        evidence.len(self.folds.len());
        for fold in &self.folds {
            evidence.text(&fold.fold_id);
            evidence.text(&fold.regime_id);
            evidence.i64(fold.test_start_ns);
            evidence.i64(fold.test_end_ns);
            evidence.u64(fold.observation_count);
            evidence.u32(fold.positive_event_count);
            evidence.u32(fold.independent_event_episode_count);
            evidence.f64(fold.baseline_brier_score);
            evidence.f64(fold.candidate_brier_score);
            evidence.f64(fold.calibration_slope);
            evidence.f64(fold.calibration_intercept);
            evidence.f64(fold.expected_calibration_error);
            evidence.f64(fold.alert_budget_score_delta);
        }
        evidence.f64(self.maximum_standardized_coefficient_drift);
        evidence.boolean(self.sign_scaling_parity);
        evidence.u32(self.numerical_failures);
        evidence.u32(self.attempted_model_variants);
        evidence.finish()
    }
}

/// Immutable v1 quantitative thresholds.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct GatePolicy {
    schema_version: u32,
    minimum_improved_fold_fraction_ppm: u32,
    minimum_positive_events: u32,
    minimum_regimes: u32,
    minimum_independent_episodes: u32,
    calibration_slope_minimum_ppm: u32,
    calibration_slope_maximum_ppm: u32,
    maximum_absolute_calibration_intercept_ppm: u32,
    maximum_expected_calibration_error_ppm: u32,
    maximum_standardized_coefficient_drift_ppm: u32,
    minimum_recent_fold_brier_skill_ppm: i32,
}

impl GatePolicy {
    pub const fn schema_version(self) -> u32 {
        self.schema_version
    }

    pub const fn minimum_improved_fold_fraction_ppm(self) -> u32 {
        self.minimum_improved_fold_fraction_ppm
    }

    pub const fn minimum_positive_events(self) -> u32 {
        self.minimum_positive_events
    }

    pub const fn minimum_regimes(self) -> u32 {
        self.minimum_regimes
    }

    pub const fn minimum_independent_episodes(self) -> u32 {
        self.minimum_independent_episodes
    }

    pub const fn calibration_slope_ppm(self) -> (u32, u32) {
        (
            self.calibration_slope_minimum_ppm,
            self.calibration_slope_maximum_ppm,
        )
    }

    pub const fn maximum_absolute_calibration_intercept_ppm(self) -> u32 {
        self.maximum_absolute_calibration_intercept_ppm
    }

    pub const fn maximum_expected_calibration_error_ppm(self) -> u32 {
        self.maximum_expected_calibration_error_ppm
    }

    pub const fn maximum_standardized_coefficient_drift_ppm(self) -> u32 {
        self.maximum_standardized_coefficient_drift_ppm
    }

    pub const fn minimum_recent_fold_brier_skill_ppm(self) -> i32 {
        self.minimum_recent_fold_brier_skill_ppm
    }
}

impl Default for GatePolicy {
    fn default() -> Self {
        Self {
            schema_version: CUSP_GATE_POLICY_SCHEMA_VERSION,
            minimum_improved_fold_fraction_ppm: 700_000,
            minimum_positive_events: 50,
            minimum_regimes: 3,
            minimum_independent_episodes: 3,
            calibration_slope_minimum_ppm: 800_000,
            calibration_slope_maximum_ppm: 1_200_000,
            maximum_absolute_calibration_intercept_ppm: 100_000,
            maximum_expected_calibration_error_ppm: 30_000,
            maximum_standardized_coefficient_drift_ppm: 2_000_000,
            minimum_recent_fold_brier_skill_ppm: -100_000,
        }
    }
}

/// Mechanically derived release status. Eligibility does not assign a weight.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum GateDecision {
    EligibleForProductionWeight = 1,
    ResearchOnly = 2,
}

impl GateDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EligibleForProductionWeight => "eligible_for_production_weight",
            Self::ResearchOnly => "research_only",
        }
    }
}

/// Stable identity of each predeclared rule.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum GateCheckKind {
    RequiredBaselines = 1,
    PositivePrimaryScore = 2,
    ImprovedFoldFraction = 3,
    MinimumPositiveEvents = 4,
    IndependentEpisodes = 5,
    MultiRegimeImprovement = 6,
    RecentFoldSafety = 7,
    Calibration = 8,
    AlertBudgetImprovement = 9,
    CoefficientStability = 10,
    SignScalingParity = 11,
    NumericalReliability = 12,
}

impl GateCheckKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RequiredBaselines => "required_baselines",
            Self::PositivePrimaryScore => "positive_primary_score",
            Self::ImprovedFoldFraction => "improved_fold_fraction",
            Self::MinimumPositiveEvents => "minimum_positive_events",
            Self::IndependentEpisodes => "independent_episodes",
            Self::MultiRegimeImprovement => "multi_regime_improvement",
            Self::RecentFoldSafety => "recent_fold_safety",
            Self::Calibration => "calibration",
            Self::AlertBudgetImprovement => "alert_budget_improvement",
            Self::CoefficientStability => "coefficient_stability",
            Self::SignScalingParity => "sign_scaling_parity",
            Self::NumericalReliability => "numerical_reliability",
        }
    }
}

/// Unit for a structured gate observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum GateMetricUnit {
    Boolean = 1,
    Count = 2,
    PartsPerMillion = 3,
}

impl GateMetricUnit {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Count => "count",
            Self::PartsPerMillion => "parts_per_million",
        }
    }
}

/// One inspectable pass/fail reason.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GateCheck {
    kind: GateCheckKind,
    passed: bool,
    observed: i64,
    minimum: Option<i64>,
    maximum: Option<i64>,
    unit: GateMetricUnit,
}

impl GateCheck {
    const fn new(
        kind: GateCheckKind,
        passed: bool,
        observed: i64,
        minimum: Option<i64>,
        maximum: Option<i64>,
        unit: GateMetricUnit,
    ) -> Self {
        Self {
            kind,
            passed,
            observed,
            minimum,
            maximum,
            unit,
        }
    }

    pub const fn kind(&self) -> GateCheckKind {
        self.kind
    }

    pub const fn passed(&self) -> bool {
        self.passed
    }

    pub const fn observed(&self) -> i64 {
        self.observed
    }

    pub const fn minimum(&self) -> Option<i64> {
        self.minimum
    }

    pub const fn maximum(&self) -> Option<i64> {
        self.maximum
    }

    pub const fn unit(&self) -> GateMetricUnit {
        self.unit
    }
}

/// Complete, deterministic result of applying the immutable policy.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GateEvaluation {
    schema_version: u32,
    policy: GatePolicy,
    decision: GateDecision,
    checks: Vec<GateCheck>,
    candidate_model_hash: [u8; 32],
    report_evidence_hash: [u8; 32],
    evidence_hash: [u8; 32],
}

impl GateEvaluation {
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub const fn policy(&self) -> GatePolicy {
        self.policy
    }

    pub const fn decision(&self) -> GateDecision {
        self.decision
    }

    pub fn checks(&self) -> &[GateCheck] {
        &self.checks
    }

    pub const fn candidate_model_hash(&self) -> [u8; 32] {
        self.candidate_model_hash
    }

    /// The check list is complete and fixed by the versioned policy.
    pub fn check(&self, kind: GateCheckKind) -> &GateCheck {
        self.checks
            .iter()
            .find(|check| check.kind == kind)
            .expect("every v1 gate check is present")
    }

    pub const fn report_evidence_hash(&self) -> [u8; 32] {
        self.report_evidence_hash
    }

    pub const fn evidence_hash(&self) -> [u8; 32] {
        self.evidence_hash
    }
}

/// Immutable predeclared cusp gate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CuspGate {
    policy: GatePolicy,
}

impl CuspGate {
    pub const fn policy(self) -> GatePolicy {
        self.policy
    }

    pub fn decide(&self, report: &AblationReport) -> Result<GateDecision, GateError> {
        self.evaluate(report).map(|evaluation| evaluation.decision)
    }

    pub fn evaluate(&self, report: &AblationReport) -> Result<GateEvaluation, GateError> {
        if report.schema_version != ABLATION_REPORT_SCHEMA_VERSION
            || report.evidence_hash != report.calculate_evidence_hash()
            || self.policy.schema_version != CUSP_GATE_POLICY_SCHEMA_VERSION
        {
            return Err(GateError::EvidenceMismatch);
        }

        let required_baseline_count = RequiredBaseline::ALL
            .iter()
            .filter(|baseline| report.required_baselines.binary_search(baseline).is_ok())
            .count();
        let aggregate_brier_skill_ppm = to_ppm(report.aggregate_brier_skill())?;
        let improved_fold_count = report.folds.iter().filter(|fold| fold.improved()).count();
        let improved_fold_fraction_ppm = fraction_ppm(improved_fold_count, report.folds.len())?;
        let positive_events = report
            .folds
            .iter()
            .try_fold(0_u64, |count, fold| {
                count.checked_add(u64::from(fold.positive_event_count))
            })
            .ok_or(GateError::MetricOverflow)?;
        let independent_episodes = report
            .folds
            .iter()
            .try_fold(0_u64, |count, fold| {
                count.checked_add(u64::from(fold.independent_event_episode_count))
            })
            .ok_or(GateError::MetricOverflow)?;
        let improved_regime_count = report
            .folds
            .iter()
            .filter(|fold| fold.improved())
            .map(|fold| fold.regime_id.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let recent_fold = report.folds.last().ok_or(GateError::EvidenceMismatch)?;
        let recent_brier_skill_ppm = to_ppm(recent_fold.brier_skill())?;
        let calibration_passed = report.folds.iter().all(|fold| {
            (f64::from(self.policy.calibration_slope_minimum_ppm) / PARTS_PER_MILLION
                ..=f64::from(self.policy.calibration_slope_maximum_ppm) / PARTS_PER_MILLION)
                .contains(&fold.calibration_slope)
                && fold.calibration_intercept.abs()
                    <= f64::from(self.policy.maximum_absolute_calibration_intercept_ppm)
                        / PARTS_PER_MILLION
                && fold.expected_calibration_error
                    <= f64::from(self.policy.maximum_expected_calibration_error_ppm)
                        / PARTS_PER_MILLION
        });
        let alert_budget_delta_ppm = to_ppm(report.aggregate_alert_budget_delta())?;
        let coefficient_drift_ppm =
            to_nonnegative_ppm(report.maximum_standardized_coefficient_drift)?;

        let checks = vec![
            GateCheck::new(
                GateCheckKind::RequiredBaselines,
                required_baseline_count == RequiredBaseline::ALL.len(),
                i64::try_from(required_baseline_count).map_err(|_| GateError::MetricOverflow)?,
                Some(
                    i64::try_from(RequiredBaseline::ALL.len())
                        .map_err(|_| GateError::MetricOverflow)?,
                ),
                None,
                GateMetricUnit::Count,
            ),
            GateCheck::new(
                GateCheckKind::PositivePrimaryScore,
                aggregate_brier_skill_ppm > 0,
                aggregate_brier_skill_ppm,
                Some(1),
                None,
                GateMetricUnit::PartsPerMillion,
            ),
            GateCheck::new(
                GateCheckKind::ImprovedFoldFraction,
                improved_fold_fraction_ppm
                    >= i64::from(self.policy.minimum_improved_fold_fraction_ppm),
                improved_fold_fraction_ppm,
                Some(i64::from(self.policy.minimum_improved_fold_fraction_ppm)),
                None,
                GateMetricUnit::PartsPerMillion,
            ),
            GateCheck::new(
                GateCheckKind::MinimumPositiveEvents,
                positive_events >= u64::from(self.policy.minimum_positive_events),
                i64::try_from(positive_events).map_err(|_| GateError::MetricOverflow)?,
                Some(i64::from(self.policy.minimum_positive_events)),
                None,
                GateMetricUnit::Count,
            ),
            GateCheck::new(
                GateCheckKind::IndependentEpisodes,
                independent_episodes >= u64::from(self.policy.minimum_independent_episodes),
                i64::try_from(independent_episodes).map_err(|_| GateError::MetricOverflow)?,
                Some(i64::from(self.policy.minimum_independent_episodes)),
                None,
                GateMetricUnit::Count,
            ),
            GateCheck::new(
                GateCheckKind::MultiRegimeImprovement,
                improved_regime_count
                    >= usize::try_from(self.policy.minimum_regimes)
                        .map_err(|_| GateError::MetricOverflow)?,
                i64::try_from(improved_regime_count).map_err(|_| GateError::MetricOverflow)?,
                Some(i64::from(self.policy.minimum_regimes)),
                None,
                GateMetricUnit::Count,
            ),
            GateCheck::new(
                GateCheckKind::RecentFoldSafety,
                recent_brier_skill_ppm
                    >= i64::from(self.policy.minimum_recent_fold_brier_skill_ppm),
                recent_brier_skill_ppm,
                Some(i64::from(self.policy.minimum_recent_fold_brier_skill_ppm)),
                None,
                GateMetricUnit::PartsPerMillion,
            ),
            GateCheck::new(
                GateCheckKind::Calibration,
                calibration_passed,
                i64::from(calibration_passed),
                Some(1),
                Some(1),
                GateMetricUnit::Boolean,
            ),
            GateCheck::new(
                GateCheckKind::AlertBudgetImprovement,
                alert_budget_delta_ppm > 0,
                alert_budget_delta_ppm,
                Some(1),
                None,
                GateMetricUnit::PartsPerMillion,
            ),
            GateCheck::new(
                GateCheckKind::CoefficientStability,
                coefficient_drift_ppm
                    <= i64::from(self.policy.maximum_standardized_coefficient_drift_ppm),
                coefficient_drift_ppm,
                None,
                Some(i64::from(
                    self.policy.maximum_standardized_coefficient_drift_ppm,
                )),
                GateMetricUnit::PartsPerMillion,
            ),
            GateCheck::new(
                GateCheckKind::SignScalingParity,
                report.sign_scaling_parity,
                i64::from(report.sign_scaling_parity),
                Some(1),
                Some(1),
                GateMetricUnit::Boolean,
            ),
            GateCheck::new(
                GateCheckKind::NumericalReliability,
                report.numerical_failures == 0,
                i64::from(report.numerical_failures),
                Some(0),
                Some(0),
                GateMetricUnit::Count,
            ),
        ];
        let decision = if checks.iter().all(|check| check.passed) {
            GateDecision::EligibleForProductionWeight
        } else {
            GateDecision::ResearchOnly
        };
        let evidence_hash =
            evaluation_evidence_hash(self.policy, decision, &checks, report.evidence_hash);
        Ok(GateEvaluation {
            schema_version: GATE_EVALUATION_SCHEMA_VERSION,
            policy: self.policy,
            decision,
            checks,
            candidate_model_hash: report.candidate_model_hash,
            report_evidence_hash: report.evidence_hash,
            evidence_hash,
        })
    }
}

fn evaluation_evidence_hash(
    policy: GatePolicy,
    decision: GateDecision,
    checks: &[GateCheck],
    report_evidence_hash: [u8; 32],
) -> [u8; 32] {
    let mut evidence = CanonicalEvidence::new(b"cusp-gate-evaluation-v1");
    evidence.u32(GATE_EVALUATION_SCHEMA_VERSION);
    evidence.u32(policy.schema_version);
    evidence.u32(policy.minimum_improved_fold_fraction_ppm);
    evidence.u32(policy.minimum_positive_events);
    evidence.u32(policy.minimum_regimes);
    evidence.u32(policy.minimum_independent_episodes);
    evidence.u32(policy.calibration_slope_minimum_ppm);
    evidence.u32(policy.calibration_slope_maximum_ppm);
    evidence.u32(policy.maximum_absolute_calibration_intercept_ppm);
    evidence.u32(policy.maximum_expected_calibration_error_ppm);
    evidence.u32(policy.maximum_standardized_coefficient_drift_ppm);
    evidence.i32(policy.minimum_recent_fold_brier_skill_ppm);
    evidence.u8(decision as u8);
    evidence.bytes(&report_evidence_hash);
    evidence.len(checks.len());
    for check in checks {
        evidence.u8(check.kind as u8);
        evidence.boolean(check.passed);
        evidence.i64(check.observed);
        evidence.optional_i64(check.minimum);
        evidence.optional_i64(check.maximum);
        evidence.u8(check.unit as u8);
    }
    evidence.finish()
}

fn validate_identifier(value: &str) -> Result<(), AblationError> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || !value.is_ascii()
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        })
    {
        Err(AblationError::InvalidIdentity)
    } else {
        Ok(())
    }
}

fn fraction_ppm(numerator: usize, denominator: usize) -> Result<i64, GateError> {
    let numerator = u64::try_from(numerator).map_err(|_| GateError::MetricOverflow)?;
    let denominator = u64::try_from(denominator).map_err(|_| GateError::MetricOverflow)?;
    let scaled = numerator
        .checked_mul(1_000_000)
        .ok_or(GateError::MetricOverflow)?
        / denominator;
    i64::try_from(scaled).map_err(|_| GateError::MetricOverflow)
}

fn to_ppm(value: f64) -> Result<i64, GateError> {
    let scaled = value * PARTS_PER_MILLION;
    if scaled.is_nan() {
        return Err(GateError::MetricOverflow);
    }
    if scaled <= i64::MIN as f64 {
        Ok(i64::MIN)
    } else if scaled >= i64::MAX as f64 {
        Ok(i64::MAX)
    } else {
        Ok(scaled.round() as i64)
    }
}

fn to_nonnegative_ppm(value: f64) -> Result<i64, GateError> {
    if value < 0.0 {
        return Err(GateError::EvidenceMismatch);
    }
    to_ppm(value)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AblationError {
    #[error("unsupported ablation report schema version {found}")]
    UnsupportedSchema { found: u32 },
    #[error("ablation identity is absent or malformed")]
    InvalidIdentity,
    #[error("ablation report exceeds the bounded fold capacity")]
    FoldCapacity,
    #[error("ablation report contains a duplicate fold")]
    DuplicateFold,
    #[error("ablation report contains a duplicate baseline")]
    DuplicateBaseline,
    #[error("ablation fold test period is invalid")]
    InvalidFoldPeriod,
    #[error("ablation folds must be chronological and non-overlapping")]
    NonChronologicalFolds,
    #[error("ablation report contains a nonfinite metric")]
    NonFiniteMetric,
    #[error("ablation report contains an out-of-range metric")]
    MetricOutOfRange,
    #[error("ablation report contains an invalid count")]
    InvalidCount,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum GateError {
    #[error("ablation evidence does not match the versioned gate contract")]
    EvidenceMismatch,
    #[error("ablation gate metric overflow")]
    MetricOverflow,
}

struct CanonicalEvidence {
    hasher: blake3::Hasher,
}

impl CanonicalEvidence {
    fn new(domain: &[u8]) -> Self {
        let mut evidence = Self {
            hasher: blake3::Hasher::new(),
        };
        evidence.bytes(domain);
        evidence
    }

    fn len(&mut self, value: usize) {
        self.u64(u64::try_from(value).expect("bounded evidence length fits u64"));
    }

    fn text(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.len_raw(value.len());
        self.hasher.update(value);
    }

    fn len_raw(&mut self, value: usize) {
        self.hasher.update(
            &u64::try_from(value)
                .expect("bounded evidence length fits u64")
                .to_le_bytes(),
        );
    }

    fn boolean(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn optional_i64(&mut self, value: Option<i64>) {
        match value {
            Some(value) => {
                self.u8(1);
                self.i64(value);
            }
            None => self.u8(0),
        }
    }

    fn u8(&mut self, value: u8) {
        self.hasher.update(&[value]);
    }

    fn u32(&mut self, value: u32) {
        self.hasher.update(&value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.hasher.update(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.hasher.update(&value.to_le_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.hasher.update(&value.to_le_bytes());
    }

    fn f64(&mut self, value: f64) {
        self.hasher.update(&value.to_bits().to_le_bytes());
    }

    fn finish(self) -> [u8; 32] {
        *self.hasher.finalize().as_bytes()
    }
}
