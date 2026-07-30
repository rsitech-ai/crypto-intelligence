//! Probabilistic physical-branch tracking for the stochastic cusp.

use numerics::logsumexp;
use serde::Serialize;
use thiserror::Error;

const BRANCH_COUNT: usize = 2;
const MAX_BRANCH_OBSERVATIONS: usize = 2_048;

/// Persistent identity of one stable physical equilibrium branch.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchId {
    Lower,
    Upper,
}

impl BranchId {
    const ALL: [Self; BRANCH_COUNT] = [Self::Lower, Self::Upper];

    const fn index(self) -> usize {
        match self {
            Self::Lower => 0,
            Self::Upper => 1,
        }
    }
}

/// Observable hysteresis path state for the current update.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HysteresisState {
    FollowingLower,
    FollowingUpper,
    JumpedLowerToUpper,
    JumpedUpperToLower,
}

/// Bounded branch-emission and transition policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BranchTrackerConfig {
    emission_scale: f64,
    transition_probability: f64,
    switch_probability: f64,
}

impl BranchTrackerConfig {
    pub fn try_new(
        emission_scale: f64,
        transition_probability: f64,
        switch_probability: f64,
    ) -> Result<Self, BranchTrackerError> {
        if !emission_scale.is_finite()
            || emission_scale <= 0.0
            || !transition_probability.is_finite()
            || !(0.0..0.5).contains(&transition_probability)
            || !switch_probability.is_finite()
            || !(0.5..=1.0).contains(&switch_probability)
        {
            return Err(BranchTrackerError::InvalidConfig);
        }
        Ok(Self {
            emission_scale,
            transition_probability,
            switch_probability,
        })
    }

    pub const fn emission_scale(self) -> f64 {
        self.emission_scale
    }

    pub const fn transition_probability(self) -> f64 {
        self.transition_probability
    }

    pub const fn switch_probability(self) -> f64 {
        self.switch_probability
    }
}

/// Stable roots observed under one posterior structural sample.
///
/// Construction sorts by physical state value. Callers cannot assign a
/// numerical vector index as a persistent branch identity.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchObservation {
    stable_roots: Vec<f64>,
}

impl BranchObservation {
    pub fn try_new(mut stable_roots: Vec<f64>) -> Result<Self, BranchTrackerError> {
        if stable_roots.is_empty()
            || stable_roots.len() > BRANCH_COUNT
            || stable_roots.iter().any(|root| !root.is_finite())
        {
            return Err(BranchTrackerError::InvalidObservation);
        }
        stable_roots.sort_by(f64::total_cmp);
        if stable_roots.windows(2).any(|pair| pair[0] >= pair[1])
            || matches!(stable_roots.as_slice(), [lower, upper] if *lower >= 0.0 || *upper <= 0.0)
        {
            return Err(BranchTrackerError::InvalidObservation);
        }
        Ok(Self { stable_roots })
    }

    pub fn stable_roots(&self) -> &[f64] {
        &self.stable_roots
    }

    fn root_for(&self, branch: BranchId, previous: Option<BranchId>) -> Option<f64> {
        match self.stable_roots.as_slice() {
            [lower, upper] => Some(match branch {
                BranchId::Lower => *lower,
                BranchId::Upper => *upper,
            }),
            [root] if *root < 0.0 && branch == BranchId::Lower => Some(*root),
            [root] if *root > 0.0 && branch == BranchId::Upper => Some(*root),
            [root] if *root == 0.0 && previous == Some(branch) => Some(*root),
            [root] if *root == 0.0 && previous.is_none() => Some(*root),
            [_] => None,
            _ => None,
        }
    }
}

/// Normalized branch posterior and hysteresis decision for one update.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BranchUpdate {
    as_of_ns: i64,
    most_likely_branch: BranchId,
    probabilities: [(BranchId, f64); BRANCH_COUNT],
    hysteresis: HysteresisState,
}

impl BranchUpdate {
    pub const fn as_of_ns(&self) -> i64 {
        self.as_of_ns
    }

    pub const fn most_likely_branch(&self) -> BranchId {
        self.most_likely_branch
    }

    pub const fn probabilities(&self) -> &[(BranchId, f64); BRANCH_COUNT] {
        &self.probabilities
    }

    pub const fn hysteresis(&self) -> HysteresisState {
        self.hysteresis
    }

    pub fn probability(&self, branch: BranchId) -> f64 {
        self.probabilities[branch.index()].1
    }
}

/// Stateful two-branch Bayesian filter with bounded HMM-like transitions.
#[derive(Clone, Debug, PartialEq)]
pub struct BranchTracker {
    config: BranchTrackerConfig,
    posterior: [f64; BRANCH_COUNT],
    most_likely: Option<BranchId>,
    last_roots: [Option<f64>; BRANCH_COUNT],
    last_as_of_ns: Option<i64>,
}

impl BranchTracker {
    pub fn try_new(
        config: BranchTrackerConfig,
        initial_branch: Option<BranchId>,
    ) -> Result<Self, BranchTrackerError> {
        let posterior = match initial_branch {
            Some(BranchId::Lower) => [1.0, 0.0],
            Some(BranchId::Upper) => [0.0, 1.0],
            None => [0.5, 0.5],
        };
        Ok(Self {
            config,
            posterior,
            most_likely: initial_branch,
            last_roots: [None, None],
            last_as_of_ns: None,
        })
    }

    pub const fn config(&self) -> BranchTrackerConfig {
        self.config
    }

    pub const fn most_likely(&self) -> Option<BranchId> {
        self.most_likely
    }

    pub fn probabilities(&self) -> [(BranchId, f64); BRANCH_COUNT] {
        [
            (BranchId::Lower, self.posterior[BranchId::Lower.index()]),
            (BranchId::Upper, self.posterior[BranchId::Upper.index()]),
        ]
    }

    pub(crate) fn evidence_digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        let domain = b"cusp-branch-tracker-state-v1";
        hasher.update(&(domain.len() as u64).to_le_bytes());
        hasher.update(domain);
        for probability in self.posterior {
            hasher.update(&probability.to_bits().to_le_bytes());
        }
        hasher.update(&[self.most_likely.map_or(0, |branch| match branch {
            BranchId::Lower => 1,
            BranchId::Upper => 2,
        })]);
        for root in self.last_roots {
            match root {
                Some(value) => {
                    hasher.update(&[1]);
                    hasher.update(&value.to_bits().to_le_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
        match self.last_as_of_ns {
            Some(value) => {
                hasher.update(&[1]);
                hasher.update(&value.to_le_bytes());
            }
            None => {
                hasher.update(&[0]);
            }
        }
        *hasher.finalize().as_bytes()
    }

    pub fn update(
        &mut self,
        as_of_ns: i64,
        normalized_state: f64,
        observations: &[BranchObservation],
    ) -> Result<BranchUpdate, BranchTrackerError> {
        if as_of_ns <= 0
            || !normalized_state.is_finite()
            || observations.is_empty()
            || observations.len() > MAX_BRANCH_OBSERVATIONS
            || self
                .last_as_of_ns
                .is_some_and(|previous| as_of_ns <= previous)
        {
            return Err(BranchTrackerError::InvalidObservation);
        }

        let predicted = self.predicted_posterior();
        let mut log_weights = [f64::NEG_INFINITY; BRANCH_COUNT];
        for branch in BranchId::ALL {
            let log_emissions = observations
                .iter()
                .filter_map(|observation| {
                    observation
                        .root_for(branch, self.most_likely)
                        .map(|root| self.log_emission(branch, normalized_state, root))
                })
                .collect::<Result<Vec<_>, BranchTrackerError>>()?;
            if !log_emissions.is_empty() {
                let log_mean = logsumexp(&log_emissions)
                    .map_err(|_| BranchTrackerError::Numerical)?
                    - (observations.len() as f64).ln();
                log_weights[branch.index()] = predicted[branch.index()].ln() + log_mean;
            }
        }
        let normalizer = logsumexp(&log_weights).map_err(|_| BranchTrackerError::Numerical)?;
        if !normalizer.is_finite() {
            return Err(BranchTrackerError::NoStableBranch);
        }
        let mut posterior = [0.0; BRANCH_COUNT];
        for branch in BranchId::ALL {
            posterior[branch.index()] = (log_weights[branch.index()] - normalizer).exp();
        }
        validate_probability_vector(posterior)?;

        let posterior_winner =
            if posterior[BranchId::Upper.index()] > posterior[BranchId::Lower.index()] {
                BranchId::Upper
            } else {
                BranchId::Lower
            };
        let selected = match self.most_likely {
            Some(previous)
                if previous != posterior_winner
                    && posterior[posterior_winner.index()] < self.config.switch_probability
                    && posterior[previous.index()] > 0.0 =>
            {
                previous
            }
            _ => posterior_winner,
        };
        let hysteresis = match (self.most_likely, selected) {
            (Some(BranchId::Lower), BranchId::Upper) => HysteresisState::JumpedLowerToUpper,
            (Some(BranchId::Upper), BranchId::Lower) => HysteresisState::JumpedUpperToLower,
            (_, BranchId::Lower) => HysteresisState::FollowingLower,
            (_, BranchId::Upper) => HysteresisState::FollowingUpper,
        };

        self.posterior = posterior;
        self.most_likely = Some(selected);
        self.last_as_of_ns = Some(as_of_ns);
        self.update_root_memory(observations);
        Ok(BranchUpdate {
            as_of_ns,
            most_likely_branch: selected,
            probabilities: self.probabilities(),
            hysteresis,
        })
    }

    fn predicted_posterior(&self) -> [f64; BRANCH_COUNT] {
        let transition = self.config.transition_probability;
        [
            self.posterior[0].mul_add(1.0 - transition, self.posterior[1] * transition),
            self.posterior[1].mul_add(1.0 - transition, self.posterior[0] * transition),
        ]
    }

    fn log_emission(
        &self,
        branch: BranchId,
        normalized_state: f64,
        root: f64,
    ) -> Result<f64, BranchTrackerError> {
        let state_residual = (normalized_state - root) / self.config.emission_scale;
        let continuity_residual = self.last_roots[branch.index()].map_or(0.0, |previous| {
            (root - previous) / self.config.emission_scale
        });
        let value = -0.5 * (state_residual.powi(2) + continuity_residual.powi(2));
        if value.is_finite() {
            Ok(value)
        } else {
            Err(BranchTrackerError::Numerical)
        }
    }

    fn update_root_memory(&mut self, observations: &[BranchObservation]) {
        for branch in BranchId::ALL {
            let roots = observations
                .iter()
                .filter_map(|observation| observation.root_for(branch, self.most_likely))
                .collect::<Vec<_>>();
            self.last_roots[branch.index()] = if roots.is_empty() {
                None
            } else {
                Some(roots.iter().sum::<f64>() / roots.len() as f64)
            };
        }
    }
}

fn validate_probability_vector(
    probabilities: [f64; BRANCH_COUNT],
) -> Result<(), BranchTrackerError> {
    if probabilities
        .iter()
        .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        || (probabilities.iter().sum::<f64>() - 1.0).abs() > 1.0e-12
    {
        Err(BranchTrackerError::Numerical)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum BranchTrackerError {
    #[error("invalid branch tracker configuration")]
    InvalidConfig,
    #[error("invalid or nonmonotonic branch observation")]
    InvalidObservation,
    #[error("posterior sample contains no stable physical branch")]
    NoStableBranch,
    #[error("branch posterior calculation is nonfinite or unnormalized")]
    Numerical,
}
