//! Deterministic fold-bound block bootstrap for offline cusp fits.

use domain::AssetId;
use thiserror::Error;

use crate::{
    ControlError, ControlMap, ControlVector,
    fit::{EstimatorKind, EstimatorRole, FitConfig, FitDataset, FitError, ParameterKey, fit},
    uncertainty::{
        ControlIntervals, IntervalEstimate, UncertaintyError, UncertaintyQuality,
        digest_parameter_samples, percentile_interval, splitmix64,
    },
};

const MIN_BOOTSTRAP_BLOCKS: usize = 2;
const MAX_BOOTSTRAP_BLOCKS: usize = 1_024;
const MIN_BOOTSTRAP_REFITS: u32 = 16;
const MAX_BOOTSTRAP_REFITS: u32 = 1_024;
const MAX_BOOTSTRAP_AGGREGATE_ROWS: u64 = 100_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeBlock {
    start_ns: i64,
    end_ns: i64,
}

impl TimeBlock {
    pub fn try_new(start_ns: i64, end_ns: i64) -> Result<Self, BootstrapError> {
        if start_ns <= 0 || end_ns <= start_ns {
            return Err(BootstrapError::InvalidBlock);
        }
        Ok(Self { start_ns, end_ns })
    }

    pub const fn start_ns(self) -> i64 {
        self.start_ns
    }

    pub const fn end_ns(self) -> i64 {
        self.end_ns
    }

    const fn contains(self, event_time_ns: i64) -> bool {
        event_time_ns >= self.start_ns && event_time_ns < self.end_ns
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockedBootstrapConfig {
    seed: u64,
    refits: u32,
    maximum_failed_fraction: f64,
    training_fold_hash: [u8; 32],
    blocks: Vec<TimeBlock>,
    fit_config: FitConfig,
}

impl BlockedBootstrapConfig {
    pub fn try_new(
        seed: u64,
        refits: u32,
        maximum_failed_fraction: f64,
        training_fold_hash: [u8; 32],
        mut blocks: Vec<TimeBlock>,
        fit_config: FitConfig,
    ) -> Result<Self, BootstrapError> {
        if !(MIN_BOOTSTRAP_REFITS..=MAX_BOOTSTRAP_REFITS).contains(&refits)
            || !maximum_failed_fraction.is_finite()
            || !(0.0..0.5).contains(&maximum_failed_fraction)
            || training_fold_hash.iter().all(|byte| *byte == 0)
            || !(MIN_BOOTSTRAP_BLOCKS..=MAX_BOOTSTRAP_BLOCKS).contains(&blocks.len())
        {
            return Err(BootstrapError::InvalidConfig);
        }
        blocks.sort_by_key(|block| (block.start_ns, block.end_ns));
        if blocks
            .windows(2)
            .any(|pair| pair[0].end_ns > pair[1].start_ns)
        {
            return Err(BootstrapError::OverlappingBlocks);
        }
        if blocks
            .windows(2)
            .any(|pair| pair[0].end_ns != pair[1].start_ns)
        {
            return Err(BootstrapError::NonContiguousBlocks);
        }
        Ok(Self {
            seed,
            refits,
            maximum_failed_fraction,
            training_fold_hash,
            blocks,
            fit_config,
        })
    }

    pub const fn seed(&self) -> u64 {
        self.seed
    }

    pub const fn refits(&self) -> u32 {
        self.refits
    }

    pub const fn maximum_failed_fraction(&self) -> f64 {
        self.maximum_failed_fraction
    }

    pub const fn training_fold_hash(&self) -> [u8; 32] {
        self.training_fold_hash
    }

    pub fn blocks(&self) -> &[TimeBlock] {
        &self.blocks
    }
}

#[derive(Clone, Debug)]
pub struct BlockedBootstrap {
    config: BlockedBootstrapConfig,
}

impl BlockedBootstrap {
    pub const fn new(config: BlockedBootstrapConfig) -> Self {
        Self { config }
    }

    pub fn run(&self, dataset: &FitDataset) -> Result<BootstrapResult, BootstrapError> {
        if dataset.training_fold_hash() != self.config.training_fold_hash {
            return Err(BootstrapError::EvidenceMismatch);
        }
        let training_range = dataset.training_time_range();
        if self.config.blocks.first().map(|block| block.start_ns) != Some(training_range.start_ns())
            || self.config.blocks.last().map(|block| block.end_ns) != Some(training_range.end_ns())
        {
            return Err(BootstrapError::BlockCoverage);
        }
        let assignments = assign_blocks(dataset, &self.config.blocks)?;
        let mut block_row_counts = vec![0_usize; self.config.blocks.len()];
        for assignment in &assignments {
            block_row_counts[*assignment] = block_row_counts[*assignment]
                .checked_add(1)
                .ok_or(BootstrapError::WorkCapacity)?;
        }
        if block_row_counts.contains(&0) {
            return Err(BootstrapError::EmptyBlock);
        }
        let aggregate_rows = u64::from(self.config.refits)
            .checked_mul(
                u64::try_from(dataset.rows().len()).map_err(|_| BootstrapError::WorkCapacity)?,
            )
            .ok_or(BootstrapError::WorkCapacity)?;
        if aggregate_rows > MAX_BOOTSTRAP_AGGREGATE_ROWS {
            return Err(BootstrapError::WorkCapacity);
        }

        let base_fit = fit(dataset, self.config.fit_config.clone())?;
        if !base_fit.diagnostics().converged() {
            return Err(BootstrapError::NonConvergedBaseFit);
        }
        let parameter_keys = base_fit.parameter_keys().to_vec();
        let base_parameters = base_fit.parameter_values().to_vec();
        let base_control_map = base_fit.control_map().clone();
        let production_candidate_estimator = base_fit.role() == EstimatorRole::ProductionCandidate;
        let mut parameter_samples = Vec::new();
        let mut control_maps = Vec::new();
        let mut selection_counts = Vec::with_capacity(self.config.refits as usize);
        let mut effective_samples = Vec::new();
        let mut state = self.config.seed;

        for replicate_index in 0..self.config.refits {
            let mut counts = vec![0_u32; self.config.blocks.len()];
            for _ in 0..self.config.blocks.len() {
                state = splitmix64(
                    state
                        ^ u64::from(replicate_index).rotate_left(19)
                        ^ self.config.seed.rotate_left(43),
                );
                let selected = (state % self.config.blocks.len() as u64) as usize;
                counts[selected] = counts[selected]
                    .checked_add(1)
                    .ok_or(BootstrapError::WorkCapacity)?;
            }
            let effective = kish_effective_sample_size(dataset, &assignments, &counts)?;
            let rows = dataset
                .rows()
                .iter()
                .zip(&assignments)
                .filter_map(|(row, block)| {
                    let multiplier = counts[*block];
                    (multiplier > 0).then(|| row.with_weight_multiplier(multiplier))
                })
                .collect::<Result<Vec<_>, FitError>>();
            let manifest_hash = bootstrap_manifest_hash(
                dataset.dataset_manifest_hash(),
                dataset.training_fold_hash(),
                &self.config,
                replicate_index,
                &counts,
            );
            let candidate = rows
                .and_then(|rows| dataset.bootstrap_replica(manifest_hash, rows))
                .and_then(|replica| fit(&replica, self.config.fit_config.clone()));
            if let Ok(result) = candidate
                && result.diagnostics().converged()
                && result.parameter_keys() == parameter_keys
            {
                parameter_samples.push(result.parameter_values().to_vec());
                control_maps.push(result.control_map().clone());
                effective_samples.push(effective);
            }
            selection_counts.push(counts);
        }

        let requested_refits = self.config.refits as usize;
        let successful_refits = parameter_samples.len();
        let failed_refits = requested_refits - successful_refits;
        let failed_fraction = failed_refits as f64 / requested_refits as f64;
        let quality =
            if successful_refits == 0 || failed_fraction > self.config.maximum_failed_fraction {
                UncertaintyQuality::Unavailable
            } else if failed_refits == 0 && production_candidate_estimator {
                UncertaintyQuality::ProductionCandidate
            } else {
                UncertaintyQuality::Experimental
            };
        let digest = bootstrap_digest(
            &self.config,
            dataset.dataset_manifest_hash(),
            dataset.training_fold_hash(),
            &selection_counts,
            &parameter_samples,
            failed_refits,
        );
        Ok(BootstrapResult {
            seed: self.config.seed,
            dataset_manifest_hash: dataset.dataset_manifest_hash(),
            training_fold_hash: dataset.training_fold_hash(),
            requested_refits,
            failed_refits,
            parameter_keys,
            base_parameters,
            parameter_samples,
            base_control_map,
            control_maps,
            selection_counts,
            minimum_effective_samples: effective_samples
                .iter()
                .copied()
                .reduce(f64::min)
                .unwrap_or(0.0),
            maximum_effective_samples: effective_samples.iter().copied().fold(0.0, f64::max),
            quality,
            digest,
        })
    }
}

fn assign_blocks(dataset: &FitDataset, blocks: &[TimeBlock]) -> Result<Vec<usize>, BootstrapError> {
    dataset
        .rows()
        .iter()
        .map(|row| {
            let mut matches = blocks
                .iter()
                .enumerate()
                .filter(|(_, block)| block.contains(row.event_time_ns()));
            let (index, _) = matches.next().ok_or(BootstrapError::BlockCoverage)?;
            if matches.next().is_some() {
                return Err(BootstrapError::OverlappingBlocks);
            }
            Ok(index)
        })
        .collect()
}

fn kish_effective_sample_size(
    dataset: &FitDataset,
    assignments: &[usize],
    counts: &[u32],
) -> Result<f64, BootstrapError> {
    let mut weight_sum = 0.0;
    let mut squared_weight_sum = 0.0;
    for (row, block) in dataset.rows().iter().zip(assignments) {
        let weight = row.weight() * f64::from(counts[*block]);
        weight_sum += weight;
        squared_weight_sum = weight.mul_add(weight, squared_weight_sum);
    }
    let effective = weight_sum.powi(2) / squared_weight_sum;
    if effective.is_finite() && effective > 0.0 {
        Ok(effective)
    } else {
        Err(BootstrapError::WorkCapacity)
    }
}

fn bootstrap_manifest_hash(
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
    config: &BlockedBootstrapConfig,
    replicate_index: u32,
    counts: &[u32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cusp-block-bootstrap-manifest-v1");
    hasher.update(&dataset_manifest_hash);
    hasher.update(&training_fold_hash);
    hash_bootstrap_config(&mut hasher, config);
    hasher.update(&replicate_index.to_le_bytes());
    for count in counts {
        hasher.update(&count.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn bootstrap_digest(
    config: &BlockedBootstrapConfig,
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
    selection_counts: &[Vec<u32>],
    parameter_samples: &[Vec<f64>],
    failed_refits: usize,
) -> [u8; 32] {
    let parameter_digest = digest_parameter_samples(
        b"cusp-block-bootstrap-parameters-v1",
        config.seed,
        dataset_manifest_hash,
        training_fold_hash,
        parameter_samples,
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cusp-block-bootstrap-result-v1");
    hash_bootstrap_config(&mut hasher, config);
    hasher.update(&parameter_digest);
    hasher.update(&(failed_refits as u64).to_le_bytes());
    for counts in selection_counts {
        for count in counts {
            hasher.update(&count.to_le_bytes());
        }
    }
    *hasher.finalize().as_bytes()
}

fn hash_bootstrap_config(hasher: &mut blake3::Hasher, config: &BlockedBootstrapConfig) {
    hasher.update(&config.seed.to_le_bytes());
    hasher.update(&config.refits.to_le_bytes());
    hasher.update(&config.maximum_failed_fraction.to_bits().to_le_bytes());
    hasher.update(&config.training_fold_hash);
    hasher.update(&(config.blocks.len() as u64).to_le_bytes());
    for block in &config.blocks {
        hasher.update(&block.start_ns.to_le_bytes());
        hasher.update(&block.end_ns.to_le_bytes());
    }
    let fit = &config.fit_config;
    hasher.update(&[match fit.estimator() {
        EstimatorKind::StationaryDensity => 1,
        EstimatorKind::StudentTTransition => 2,
    }]);
    hasher.update(&fit.degrees_of_freedom().to_bits().to_le_bytes());
    let penalty = fit.penalty();
    hasher.update(&penalty.lambda().to_bits().to_le_bytes());
    hasher.update(&penalty.l1_ratio().to_bits().to_le_bytes());
    hasher.update(&penalty.asset_multiplier().to_bits().to_le_bytes());
    let optimizer = fit.optimizer();
    hasher.update(&optimizer.max_iterations().to_le_bytes());
    hasher.update(&optimizer.tolerance().to_bits().to_le_bytes());
    hasher.update(&optimizer.initial_step().to_bits().to_le_bytes());
    hasher.update(&optimizer.minimum_step().to_bits().to_le_bytes());
    hasher.update(&optimizer.max_backtracking().to_le_bytes());
    hasher.update(&optimizer.max_condition_number().to_bits().to_le_bytes());
    hasher.update(&optimizer.work_limit().to_le_bytes());
    hasher.update(&fit.integration_bound().to_bits().to_le_bytes());
    hasher.update(&fit.integration_intervals().to_le_bytes());
}

#[derive(Clone, Debug, PartialEq)]
pub struct BootstrapResult {
    seed: u64,
    dataset_manifest_hash: [u8; 32],
    training_fold_hash: [u8; 32],
    requested_refits: usize,
    failed_refits: usize,
    parameter_keys: Vec<ParameterKey>,
    base_parameters: Vec<f64>,
    parameter_samples: Vec<Vec<f64>>,
    base_control_map: ControlMap,
    control_maps: Vec<ControlMap>,
    selection_counts: Vec<Vec<u32>>,
    minimum_effective_samples: f64,
    maximum_effective_samples: f64,
    quality: UncertaintyQuality,
    digest: [u8; 32],
}

impl BootstrapResult {
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    pub const fn dataset_manifest_hash(&self) -> [u8; 32] {
        self.dataset_manifest_hash
    }

    pub const fn training_fold_hash(&self) -> [u8; 32] {
        self.training_fold_hash
    }

    pub const fn requested_refits(&self) -> usize {
        self.requested_refits
    }

    pub const fn successful_refits(&self) -> usize {
        self.parameter_samples.len()
    }

    pub const fn failed_refits(&self) -> usize {
        self.failed_refits
    }

    pub fn parameter_keys(&self) -> &[ParameterKey] {
        &self.parameter_keys
    }

    pub fn parameter_samples(&self) -> &[Vec<f64>] {
        &self.parameter_samples
    }

    pub fn selection_counts(&self) -> &[Vec<u32>] {
        &self.selection_counts
    }

    pub const fn minimum_effective_samples(&self) -> f64 {
        self.minimum_effective_samples
    }

    pub const fn maximum_effective_samples(&self) -> f64 {
        self.maximum_effective_samples
    }

    pub const fn quality(&self) -> UncertaintyQuality {
        self.quality
    }

    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }

    pub fn parameter_interval(
        &self,
        parameter_index: usize,
        mass: f64,
    ) -> Result<IntervalEstimate, BootstrapError> {
        self.require_available()?;
        let estimate = *self
            .base_parameters
            .get(parameter_index)
            .ok_or(BootstrapError::Dimension)?;
        let values = self
            .parameter_samples
            .iter()
            .map(|sample| {
                sample
                    .get(parameter_index)
                    .copied()
                    .ok_or(BootstrapError::Dimension)
            })
            .collect::<Result<Vec<_>, BootstrapError>>()?;
        percentile_interval(values, estimate, mass, self.seed, self.failed_refits)
            .map_err(BootstrapError::Uncertainty)
    }

    pub fn control_interval(
        &self,
        asset: &AssetId,
        features: &ControlVector,
        mass: f64,
    ) -> Result<ControlIntervals, BootstrapError> {
        self.require_available()?;
        let estimate = self.base_control_map.evaluate(asset, features)?;
        let samples = self
            .control_maps
            .iter()
            .map(|map| map.evaluate(asset, features))
            .collect::<Result<Vec<_>, ControlError>>()?;
        let alpha = percentile_interval(
            samples.iter().map(|sample| sample.alpha).collect(),
            estimate.alpha,
            mass,
            self.seed,
            self.failed_refits,
        )?;
        let beta = percentile_interval(
            samples.iter().map(|sample| sample.beta).collect(),
            estimate.beta,
            mass,
            self.seed,
            self.failed_refits,
        )?;
        Ok(ControlIntervals::new(alpha, beta))
    }

    fn require_available(&self) -> Result<(), BootstrapError> {
        if self.quality == UncertaintyQuality::Unavailable {
            Err(BootstrapError::Unavailable)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum BootstrapError {
    #[error("blocked bootstrap configuration is invalid")]
    InvalidConfig,
    #[error("bootstrap time block is invalid")]
    InvalidBlock,
    #[error("bootstrap time blocks overlap")]
    OverlappingBlocks,
    #[error("bootstrap time blocks do not form one contiguous training-fold partition")]
    NonContiguousBlocks,
    #[error("bootstrap time blocks do not cover every training row")]
    BlockCoverage,
    #[error("bootstrap time block contains no training rows")]
    EmptyBlock,
    #[error("bootstrap training-fold evidence does not match the dataset")]
    EvidenceMismatch,
    #[error("bootstrap base fit did not converge")]
    NonConvergedBaseFit,
    #[error("bootstrap aggregate work capacity was exceeded")]
    WorkCapacity,
    #[error("bootstrap parameter dimension is invalid")]
    Dimension,
    #[error("bootstrap uncertainty is unavailable under the declared failure threshold")]
    Unavailable,
    #[error(transparent)]
    Fit(#[from] FitError),
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error(transparent)]
    Uncertainty(#[from] UncertaintyError),
}
