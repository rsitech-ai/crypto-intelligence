//! Bounded, quality-aware local Cusp snapshot and history service.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, RwLock},
};

use cusp::{
    ControlMissingReason, Stability,
    ablation::{GateDecision, GateEvaluation},
    branch_tracker::{BranchId, HysteresisState},
    online::{CuspSnapshot, SnapshotAvailability, UnavailabilityReason},
    uncertainty::UncertaintyQuality,
};
use domain::{AssetId, AssetNamespace};
use thiserror::Error;
use tonic::{Request, Response, Status};

use crate::{
    auth::RequestAuthenticator,
    proto::{
        common_v1::{
            AssetId as ProtoAssetId, AssetNamespace as ProtoAssetNamespace, AvailabilityState,
            DataQualitySummary, UnixNanos,
        },
        risk_v1::{
            CuspBranch, CuspBranchProbability, CuspEquilibrium, CuspEquilibriumStability,
            CuspFeatureSensitivity, CuspFoldState, CuspGateCheck, CuspHysteresisState,
            CuspMissingFeature, CuspProductionUse, CuspServiceGetCuspHistoryRequest,
            CuspServiceGetCuspHistoryResponse, CuspServiceGetCuspStateRequest,
            CuspServiceGetCuspStateResponse, CuspState, CuspUncertaintyQuality,
            CuspWeightEligibility, cusp_service_server::CuspService as CuspServiceRpc,
        },
    },
};

pub const MAX_CUSP_HISTORY_PER_ASSET: usize = 2_048;
pub const MAX_CUSP_HISTORY_REQUEST: usize = 256;
pub const MAX_CUSP_ASSETS: usize = 4_096;
pub const MAX_CUSP_MODELS: usize = 1_024;
pub const MAX_CUSP_TOTAL_RECORDS: usize = 16_384;

const QUALITY_FLAG_SOURCE_DEGRADED: u64 = 1 << 0;
const QUALITY_FLAG_SOURCE_UNHEALTHY: u64 = 1 << 1;
const QUALITY_FLAG_SOURCE_QUARANTINED: u64 = 1 << 2;
const QUALITY_FLAG_SOURCE_RECOVERING: u64 = 1 << 3;
const QUALITY_FLAG_OPTIONAL_FEATURE_MISSING: u64 = 1 << 4;
const QUALITY_FLAG_EXPERIMENTAL_UNCERTAINTY: u64 = 1 << 5;
const QUALITY_FLAG_RESEARCH_ONLY_GATE: u64 = 1 << 6;

#[derive(Clone, Debug)]
struct CuspRecord {
    as_of_ns: i64,
    state: CuspState,
}

/// Bounded authoritative history indexed by generation-aware asset identity.
#[derive(Clone, Debug)]
pub struct CuspSnapshotStore {
    capacity_per_asset: usize,
    asset_capacity: usize,
    model_capacity: usize,
    record_capacity: usize,
    total_records: usize,
    records: BTreeMap<AssetId, VecDeque<CuspRecord>>,
    gate_by_model: BTreeMap<[u8; 32], [u8; 32]>,
}

impl CuspSnapshotStore {
    pub fn try_new(capacity_per_asset: usize) -> Result<Self, CuspServiceError> {
        Self::try_new_with_limits(capacity_per_asset, MAX_CUSP_ASSETS, MAX_CUSP_MODELS)
    }

    fn try_new_with_limits(
        capacity_per_asset: usize,
        asset_capacity: usize,
        model_capacity: usize,
    ) -> Result<Self, CuspServiceError> {
        Self::try_new_with_all_limits(
            capacity_per_asset,
            asset_capacity,
            model_capacity,
            MAX_CUSP_TOTAL_RECORDS,
        )
    }

    fn try_new_with_all_limits(
        capacity_per_asset: usize,
        asset_capacity: usize,
        model_capacity: usize,
        record_capacity: usize,
    ) -> Result<Self, CuspServiceError> {
        if capacity_per_asset == 0
            || capacity_per_asset > MAX_CUSP_HISTORY_PER_ASSET
            || asset_capacity == 0
            || asset_capacity > MAX_CUSP_ASSETS
            || model_capacity == 0
            || model_capacity > MAX_CUSP_MODELS
            || record_capacity == 0
            || record_capacity > MAX_CUSP_TOTAL_RECORDS
        {
            return Err(CuspServiceError::InvalidCapacity);
        }
        Ok(Self {
            capacity_per_asset,
            asset_capacity,
            model_capacity,
            record_capacity,
            total_records: 0,
            records: BTreeMap::new(),
            gate_by_model: BTreeMap::new(),
        })
    }

    pub fn publish(
        &mut self,
        snapshot: CuspSnapshot,
        gate: GateEvaluation,
    ) -> Result<(), CuspServiceError> {
        validate_snapshot(&snapshot)?;
        if snapshot.model_evidence_hash() != gate.candidate_model_hash()
            || gate.evidence_hash().iter().all(|byte| *byte == 0)
        {
            return Err(CuspServiceError::ModelEvidenceMismatch);
        }
        let state = snapshot_to_proto(&snapshot, &gate)?;
        let asset = snapshot.asset().clone();
        let as_of_ns = snapshot.as_of_ns();
        let model_evidence_hash = snapshot.model_evidence_hash();
        if !self.records.contains_key(&asset) && self.records.len() == self.asset_capacity {
            return Err(CuspServiceError::AssetCapacity);
        }
        if !self.gate_by_model.contains_key(&model_evidence_hash)
            && self.gate_by_model.len() == self.model_capacity
        {
            return Err(CuspServiceError::ModelCapacity);
        }
        match self.gate_by_model.get(&model_evidence_hash) {
            Some(existing) if *existing != gate.evidence_hash() => {
                return Err(CuspServiceError::GateEvidenceMismatch);
            }
            Some(_) => {}
            None => {}
        }
        if self
            .records
            .get(&asset)
            .and_then(|records| records.back())
            .is_some_and(|previous| previous.as_of_ns >= as_of_ns)
        {
            return Err(CuspServiceError::OutOfOrderSnapshot);
        }
        let asset_history_full = self
            .records
            .get(&asset)
            .is_some_and(|records| records.len() == self.capacity_per_asset);
        if asset_history_full {
            self.records
                .get_mut(&asset)
                .expect("existing full asset history")
                .pop_front();
            self.total_records -= 1;
        } else if self.total_records == self.record_capacity {
            self.evict_oldest_record();
        }
        self.gate_by_model
            .entry(model_evidence_hash)
            .or_insert_with(|| gate.evidence_hash());
        self.records
            .entry(asset)
            .or_default()
            .push_back(CuspRecord { as_of_ns, state });
        self.total_records += 1;
        Ok(())
    }

    pub fn current_state(&self, asset: &AssetId) -> Result<Option<CuspState>, CuspServiceError> {
        Ok(self.current_record(asset).map(|record| record.state))
    }

    /// Return the latest bounded window before the exclusive cursor, ordered oldest to newest.
    pub fn history_states(
        &self,
        asset: &AssetId,
        limit: usize,
        before_as_of_ns: Option<i64>,
    ) -> Result<Vec<CuspState>, CuspServiceError> {
        Ok(self
            .history_records(asset, limit, before_as_of_ns)?
            .into_iter()
            .map(|record| record.state)
            .collect())
    }

    fn current_record(&self, asset: &AssetId) -> Option<CuspRecord> {
        self.records
            .get(asset)
            .and_then(|records| records.back())
            .cloned()
    }

    fn history_records(
        &self,
        asset: &AssetId,
        limit: usize,
        before_as_of_ns: Option<i64>,
    ) -> Result<Vec<CuspRecord>, CuspServiceError> {
        if limit == 0
            || limit > MAX_CUSP_HISTORY_REQUEST
            || before_as_of_ns.is_some_and(|value| value <= 0)
        {
            return Err(CuspServiceError::InvalidHistoryRequest);
        }
        let Some(records) = self.records.get(asset) else {
            return Ok(Vec::new());
        };
        let mut selected = records
            .iter()
            .rev()
            .filter(|record| before_as_of_ns.is_none_or(|cursor| record.as_of_ns < cursor))
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        selected.reverse();
        Ok(selected)
    }

    fn evict_oldest_record(&mut self) {
        let oldest_asset = self
            .records
            .iter()
            .filter_map(|(asset, records)| records.front().map(|record| (asset, record.as_of_ns)))
            .min_by(|(left_asset, left_time), (right_asset, right_time)| {
                left_time
                    .cmp(right_time)
                    .then_with(|| left_asset.cmp(right_asset))
            })
            .map(|(asset, _)| asset.clone())
            .expect("a full record store has an oldest record");
        let empty = {
            let records = self
                .records
                .get_mut(&oldest_asset)
                .expect("oldest asset remains present");
            records.pop_front();
            records.is_empty()
        };
        if empty {
            self.records.remove(&oldest_asset);
        }
        self.total_records -= 1;
    }
}

/// Thread-safe, authenticated Tonic service implementation.
#[derive(Clone)]
pub struct LocalCuspService {
    store: Arc<RwLock<CuspSnapshotStore>>,
    request_authenticator: RequestAuthenticator,
}

impl LocalCuspService {
    pub(crate) fn new(
        store: CuspSnapshotStore,
        request_authenticator: RequestAuthenticator,
    ) -> Self {
        Self {
            store: Arc::new(RwLock::new(store)),
            request_authenticator,
        }
    }

    pub fn shared_store(&self) -> Arc<RwLock<CuspSnapshotStore>> {
        Arc::clone(&self.store)
    }
}

#[tonic::async_trait]
impl CuspServiceRpc for LocalCuspService {
    async fn get_cusp_state(
        &self,
        request: Request<CuspServiceGetCuspStateRequest>,
    ) -> Result<Response<CuspServiceGetCuspStateResponse>, Status> {
        self.request_authenticator.authenticate(&request)?;
        let asset = parse_asset(
            request
                .into_inner()
                .asset
                .ok_or_else(|| Status::invalid_argument("asset is required"))?,
        )?;
        let record = {
            self.store
                .read()
                .map_err(|_| Status::internal("cusp store lock is poisoned"))?
                .current_record(&asset)
        }
        .ok_or_else(|| Status::not_found("cusp state was not found"))?;
        Ok(Response::new(CuspServiceGetCuspStateResponse {
            cusp_state: Some(record.state),
        }))
    }

    async fn get_cusp_history(
        &self,
        request: Request<CuspServiceGetCuspHistoryRequest>,
    ) -> Result<Response<CuspServiceGetCuspHistoryResponse>, Status> {
        self.request_authenticator.authenticate(&request)?;
        let request = request.into_inner();
        let asset = parse_asset(
            request
                .asset
                .ok_or_else(|| Status::invalid_argument("asset is required"))?,
        )?;
        let limit = usize::try_from(request.limit)
            .map_err(|_| Status::invalid_argument("history limit is invalid"))?;
        let before_as_of_ns = request.before_as_of_time.map(|value| value.value);
        let records = self
            .store
            .read()
            .map_err(|_| Status::internal("cusp store lock is poisoned"))?
            .history_records(&asset, limit, before_as_of_ns)
            .map_err(status_from_service_error)?;
        Ok(Response::new(CuspServiceGetCuspHistoryResponse {
            cusp_states: records.into_iter().map(|record| record.state).collect(),
        }))
    }
}

fn validate_snapshot(snapshot: &CuspSnapshot) -> Result<(), CuspServiceError> {
    if snapshot.schema_version() == 0
        || snapshot.as_of_ns() <= 0
        || !snapshot.normalized_state().is_finite()
        || snapshot.evidence_hash().iter().all(|byte| *byte == 0)
        || snapshot.model_evidence_hash().iter().all(|byte| *byte == 0)
        || u32::try_from(snapshot.posterior_samples().len()).is_err()
    {
        return Err(CuspServiceError::InvalidSnapshot);
    }
    match snapshot.availability() {
        SnapshotAvailability::ResearchAvailable | SnapshotAvailability::ResearchDegraded => {
            if snapshot.controls().is_none()
                || snapshot.fold_distance().is_none()
                || snapshot.equilibria().is_none()
                || snapshot.cusp_region_probability().is_none()
                || snapshot.signed_discriminant().is_none()
                || snapshot.branch_probabilities().len() != 2
                || snapshot.most_likely_branch().is_none()
                || snapshot.hysteresis().is_none()
            {
                return Err(CuspServiceError::InvalidSnapshot);
            }
            let probability_sum = snapshot
                .branch_probabilities()
                .iter()
                .map(|(_, probability)| *probability)
                .sum::<f64>();
            if snapshot
                .branch_probabilities()
                .iter()
                .any(|(_, probability)| {
                    !probability.is_finite() || !(0.0..=1.0).contains(probability)
                })
                || (probability_sum - 1.0).abs() > 1.0e-12
            {
                return Err(CuspServiceError::InvalidSnapshot);
            }
        }
        SnapshotAvailability::Unavailable(_) => {
            if snapshot.controls().is_some()
                || snapshot.fold_distance().is_some()
                || snapshot.equilibria().is_some()
                || snapshot.cusp_region_probability().is_some()
                || snapshot.signed_discriminant().is_some()
                || !snapshot.branch_probabilities().is_empty()
                || snapshot.most_likely_branch().is_some()
                || snapshot.hysteresis().is_some()
                || !snapshot.posterior_samples().is_empty()
            {
                return Err(CuspServiceError::InvalidSnapshot);
            }
        }
    }
    Ok(())
}

fn snapshot_to_proto(
    snapshot: &CuspSnapshot,
    gate: &GateEvaluation,
) -> Result<CuspState, CuspServiceError> {
    let controls = snapshot.controls();
    let fold = snapshot.fold_distance();
    let quality = snapshot.quality();
    let availability = availability(snapshot, gate.decision());
    let quality_summary = quality_summary(snapshot, gate.decision());
    Ok(CuspState {
        state_id: hex::encode(snapshot.evidence_hash()),
        alpha: controls.map_or_else(String::new, |value| value.alpha.to_string()),
        beta: controls.map_or_else(String::new, |value| value.beta.to_string()),
        fold_distance: fold.map_or_else(String::new, |value| value.distance.to_string()),
        availability: availability as i32,
        asset: Some(asset_to_proto(snapshot.asset())),
        as_of_time: Some(UnixNanos {
            value: snapshot.as_of_ns(),
        }),
        normalized_state: snapshot.normalized_state().to_string(),
        cusp_region_probability: optional_number(snapshot.cusp_region_probability()),
        signed_discriminant: optional_number(snapshot.signed_discriminant()),
        standardized_discriminant: optional_number(snapshot.standardized_discriminant()),
        equilibria: snapshot.equilibria().map_or_else(Vec::new, |equilibria| {
            equilibria
                .roots
                .iter()
                .map(|root| CuspEquilibrium {
                    root: root.equilibrium.value.to_string(),
                    multiplicity: u32::from(root.equilibrium.multiplicity),
                    residual: root.equilibrium.residual.to_string(),
                    condition_proxy: root.equilibrium.condition_proxy.to_string(),
                    stability: stability(root.stability) as i32,
                    hessian: root.hessian.to_string(),
                    restoring_force: optional_number(root.restoring_force),
                })
                .collect()
        }),
        most_likely_branch: snapshot
            .most_likely_branch()
            .map_or(CuspBranch::Unspecified, branch) as i32,
        branch_probabilities: branch_probabilities(snapshot.branch_probabilities())?,
        minimum_barrier: optional_number(snapshot.minimum_barrier()),
        restoring_force: optional_number(snapshot.restoring_force()),
        hysteresis: snapshot
            .hysteresis()
            .map_or(CuspHysteresisState::Unspecified, hysteresis) as i32,
        sensitivities: snapshot
            .sensitivities()
            .iter()
            .map(|sensitivity| CuspFeatureSensitivity {
                feature_id: sensitivity.key.id().to_owned(),
                feature_version: sensitivity.key.version().to_string(),
                alpha_sensitivity: sensitivity.alpha.to_string(),
                beta_sensitivity: sensitivity.beta.to_string(),
            })
            .collect(),
        missing_optional_features: snapshot
            .missing_optional()
            .iter()
            .map(|missing| CuspMissingFeature {
                feature_id: missing.key.id().to_owned(),
                feature_version: missing.key.version().to_string(),
                reason: missing_reason(missing.reason).to_owned(),
            })
            .collect(),
        uncertainty_quality: uncertainty_quality(snapshot.uncertainty_quality()) as i32,
        evidence_blake3: snapshot.evidence_hash().to_vec(),
        weight_eligibility: match gate.decision() {
            GateDecision::EligibleForProductionWeight => {
                CuspWeightEligibility::EligibleForProductionWeight
            }
            GateDecision::ResearchOnly => CuspWeightEligibility::ResearchOnly,
        } as i32,
        production_use: CuspProductionUse::NotUsedInProductionProbability as i32,
        gate_evidence_blake3: gate.evidence_hash().to_vec(),
        quality: Some(quality_summary),
        feature_coverage_ppm: quality.coverage().millionths(),
        source_health: quality.source_state().as_str().to_owned(),
        fold: fold.map(|fold| CuspFoldState {
            distance: fold.distance.to_string(),
            nearest_alpha: fold.nearest.alpha.to_string(),
            nearest_beta: fold.nearest.beta.to_string(),
            fold_parameter: fold.fold_parameter.to_string(),
            converged: fold.diagnostics.converged,
            iterations: fold.diagnostics.iterations,
            evaluations: fold.diagnostics.evaluations,
            condition_number: fold
                .diagnostics
                .condition_number
                .map(|value| value.to_string()),
        }),
        posterior_sample_count: u32::try_from(snapshot.posterior_samples().len())
            .map_err(|_| CuspServiceError::InvalidSnapshot)?,
        schema_version: snapshot.schema_version(),
        availability_reason: availability_reason(snapshot, gate.decision()).to_owned(),
        model_evidence_blake3: snapshot.model_evidence_hash().to_vec(),
        gate_checks: gate
            .checks()
            .iter()
            .map(|check| CuspGateCheck {
                kind: check.kind().as_str().to_owned(),
                passed: check.passed(),
                observed: check.observed(),
                minimum: check.minimum(),
                maximum: check.maximum(),
                unit: check.unit().as_str().to_owned(),
            })
            .collect(),
        gate_policy_schema_version: gate.policy().schema_version(),
        gate_evaluation_schema_version: gate.schema_version(),
    })
}

fn branch_probabilities(
    probabilities: &[(BranchId, f64)],
) -> Result<Vec<CuspBranchProbability>, CuspServiceError> {
    if probabilities.is_empty() {
        return Ok(Vec::new());
    }
    let lower = probabilities
        .iter()
        .find_map(|(branch, probability)| (*branch == BranchId::Lower).then_some(*probability))
        .ok_or(CuspServiceError::InvalidSnapshot)?;
    let upper = probabilities
        .iter()
        .find_map(|(branch, probability)| (*branch == BranchId::Upper).then_some(*probability))
        .ok_or(CuspServiceError::InvalidSnapshot)?;
    if (lower + upper - 1.0).abs() > 1.0e-12 {
        return Err(CuspServiceError::InvalidSnapshot);
    }
    let lower_ppm = probability_ppm(lower)?;
    let upper_ppm = 1_000_000_u32
        .checked_sub(lower_ppm)
        .ok_or(CuspServiceError::InvalidSnapshot)?;
    Ok(vec![
        CuspBranchProbability {
            branch: CuspBranch::Lower as i32,
            probability_ppm: lower_ppm,
        },
        CuspBranchProbability {
            branch: CuspBranch::Upper as i32,
            probability_ppm: upper_ppm,
        },
    ])
}

fn probability_ppm(value: f64) -> Result<u32, CuspServiceError> {
    let scaled = (value * 1_000_000.0).round();
    if !scaled.is_finite() || !(0.0..=1_000_000.0).contains(&scaled) {
        return Err(CuspServiceError::InvalidSnapshot);
    }
    Ok(scaled as u32)
}

fn quality_summary(snapshot: &CuspSnapshot, decision: GateDecision) -> DataQualitySummary {
    let quality = snapshot.quality();
    let mut flags = 0_u64;
    let mut reasons = Vec::new();
    match quality.source_state().as_str() {
        "healthy" => {}
        "degraded" => flags |= QUALITY_FLAG_SOURCE_DEGRADED,
        "unhealthy" => flags |= QUALITY_FLAG_SOURCE_UNHEALTHY,
        "quarantined" => flags |= QUALITY_FLAG_SOURCE_QUARANTINED,
        "recovering" => flags |= QUALITY_FLAG_SOURCE_RECOVERING,
        _ => {}
    }
    if quality.source_state().as_str() != "healthy" {
        reasons.push(format!("source_health:{}", quality.source_state().as_str()));
    }
    if quality.coverage().millionths() < 1_000_000 {
        reasons.push(format!(
            "feature_coverage_ppm:{}",
            quality.coverage().millionths()
        ));
    }
    if !snapshot.missing_optional().is_empty() {
        flags |= QUALITY_FLAG_OPTIONAL_FEATURE_MISSING;
        reasons.push(format!(
            "missing_optional_features:{}",
            snapshot.missing_optional().len()
        ));
    }
    if snapshot.uncertainty_quality() != UncertaintyQuality::ProductionCandidate {
        flags |= QUALITY_FLAG_EXPERIMENTAL_UNCERTAINTY;
        reasons.push(format!(
            "uncertainty_quality:{}",
            uncertainty_quality_label(snapshot.uncertainty_quality())
        ));
    }
    if decision == GateDecision::ResearchOnly {
        flags |= QUALITY_FLAG_RESEARCH_ONLY_GATE;
        reasons.push("not_used_in_production_probability".to_owned());
    }
    DataQualitySummary {
        quality_score_ppm: quality.score().millionths(),
        quality_flags: flags,
        degradation_reasons: reasons,
    }
}

fn availability(snapshot: &CuspSnapshot, decision: GateDecision) -> AvailabilityState {
    match snapshot.availability() {
        SnapshotAvailability::Unavailable(_) => match snapshot.quality().source_state().as_str() {
            "unhealthy" | "quarantined" => AvailabilityState::SourceUnhealthy,
            _ => AvailabilityState::InsufficientData,
        },
        SnapshotAvailability::ResearchAvailable if decision == GateDecision::ResearchOnly => {
            AvailabilityState::Experimental
        }
        SnapshotAvailability::ResearchAvailable => AvailabilityState::Available,
        SnapshotAvailability::ResearchDegraded if decision == GateDecision::ResearchOnly => {
            AvailabilityState::Experimental
        }
        SnapshotAvailability::ResearchDegraded => AvailabilityState::Degraded,
    }
}

fn availability_reason(snapshot: &CuspSnapshot, decision: GateDecision) -> &'static str {
    match snapshot.availability() {
        SnapshotAvailability::Unavailable(UnavailabilityReason::QualityRejected) => {
            "quality_rejected"
        }
        SnapshotAvailability::Unavailable(UnavailabilityReason::RequiredFeatureMissing) => {
            "required_feature_missing"
        }
        SnapshotAvailability::ResearchAvailable | SnapshotAvailability::ResearchDegraded
            if decision == GateDecision::ResearchOnly =>
        {
            "ablation_gate_research_only"
        }
        SnapshotAvailability::ResearchDegraded => "research_degraded",
        SnapshotAvailability::ResearchAvailable => "",
    }
}

fn optional_number(value: Option<f64>) -> Option<String> {
    value.map(|number| number.to_string())
}

const fn branch(value: BranchId) -> CuspBranch {
    match value {
        BranchId::Lower => CuspBranch::Lower,
        BranchId::Upper => CuspBranch::Upper,
    }
}

const fn hysteresis(value: HysteresisState) -> CuspHysteresisState {
    match value {
        HysteresisState::FollowingLower => CuspHysteresisState::FollowingLower,
        HysteresisState::FollowingUpper => CuspHysteresisState::FollowingUpper,
        HysteresisState::JumpedLowerToUpper => CuspHysteresisState::JumpedLowerToUpper,
        HysteresisState::JumpedUpperToLower => CuspHysteresisState::JumpedUpperToLower,
    }
}

const fn stability(value: Stability) -> CuspEquilibriumStability {
    match value {
        Stability::Stable => CuspEquilibriumStability::Stable,
        Stability::Unstable => CuspEquilibriumStability::Unstable,
        Stability::NeutralAtTolerance => CuspEquilibriumStability::Marginal,
    }
}

const fn uncertainty_quality(value: UncertaintyQuality) -> CuspUncertaintyQuality {
    match value {
        UncertaintyQuality::ProductionCandidate => CuspUncertaintyQuality::ProductionCandidate,
        UncertaintyQuality::Experimental => CuspUncertaintyQuality::Experimental,
        UncertaintyQuality::Unavailable => CuspUncertaintyQuality::Unavailable,
    }
}

const fn uncertainty_quality_label(value: UncertaintyQuality) -> &'static str {
    match value {
        UncertaintyQuality::ProductionCandidate => "production_candidate",
        UncertaintyQuality::Experimental => "experimental",
        UncertaintyQuality::Unavailable => "unavailable",
    }
}

const fn missing_reason(reason: ControlMissingReason) -> &'static str {
    match reason {
        ControlMissingReason::Stale => "stale",
        ControlMissingReason::InsufficientHistory => "insufficient_history",
        ControlMissingReason::WindowNotFinal => "window_not_final",
        ControlMissingReason::QualityRejected => "quality_rejected",
        ControlMissingReason::SourceUnavailable => "source_unavailable",
    }
}

fn parse_asset(asset: ProtoAssetId) -> Result<AssetId, Status> {
    let namespace = match ProtoAssetNamespace::try_from(asset.namespace) {
        Ok(ProtoAssetNamespace::Native) => AssetNamespace::Native,
        Ok(ProtoAssetNamespace::Evm) => AssetNamespace::Evm,
        Ok(ProtoAssetNamespace::Solana) => AssetNamespace::Solana,
        Ok(ProtoAssetNamespace::Fiat) => AssetNamespace::Fiat,
        Ok(ProtoAssetNamespace::Synthetic) => AssetNamespace::Synthetic,
        Ok(ProtoAssetNamespace::Unspecified) | Err(_) => {
            return Err(Status::invalid_argument("asset namespace is invalid"));
        }
    };
    AssetId::new(
        namespace,
        asset.chain_id,
        asset.contract_or_mint,
        asset.canonical_symbol,
        asset.generation,
    )
    .map_err(|_| Status::invalid_argument("asset identity is invalid"))
}

fn asset_to_proto(asset: &AssetId) -> ProtoAssetId {
    let namespace = match asset.namespace() {
        AssetNamespace::Native => ProtoAssetNamespace::Native,
        AssetNamespace::Evm => ProtoAssetNamespace::Evm,
        AssetNamespace::Solana => ProtoAssetNamespace::Solana,
        AssetNamespace::Fiat => ProtoAssetNamespace::Fiat,
        AssetNamespace::Synthetic => ProtoAssetNamespace::Synthetic,
    };
    ProtoAssetId {
        namespace: namespace as i32,
        chain_id: asset.chain_id().to_owned(),
        contract_or_mint: asset.contract_or_mint().to_owned(),
        canonical_symbol: asset.canonical_symbol().to_owned(),
        generation: asset.generation(),
    }
}

fn status_from_service_error(error: CuspServiceError) -> Status {
    match error {
        CuspServiceError::InvalidHistoryRequest => {
            Status::invalid_argument("history request is invalid")
        }
        CuspServiceError::InvalidCapacity
        | CuspServiceError::AssetCapacity
        | CuspServiceError::ModelCapacity
        | CuspServiceError::InvalidSnapshot
        | CuspServiceError::ModelEvidenceMismatch
        | CuspServiceError::GateEvidenceMismatch
        | CuspServiceError::OutOfOrderSnapshot => {
            Status::failed_precondition("cusp service state is inconsistent")
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CuspServiceError {
    #[error("cusp history capacity is outside the bounded policy")]
    InvalidCapacity,
    #[error("cusp store has reached its unique-asset capacity")]
    AssetCapacity,
    #[error("cusp store has reached its frozen-model capacity")]
    ModelCapacity,
    #[error("cusp snapshot violates the service invariant")]
    InvalidSnapshot,
    #[error("cusp snapshot model evidence does not match its gate")]
    ModelEvidenceMismatch,
    #[error("one frozen model has conflicting ablation evidence")]
    GateEvidenceMismatch,
    #[error("cusp snapshots must be published in strictly increasing time order")]
    OutOfOrderSnapshot,
    #[error("cusp history request is outside the bounded policy")]
    InvalidHistoryRequest,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::OnceLock,
        time::{SystemTime, UNIX_EPOCH},
    };

    use cusp::{
        CoefficientSign, ControlCovariance, ControlFeature, ControlFeatureKey, ControlSchema,
        ControlTarget, ControlValue, ControlVector, FeatureCovariance, FeatureGroup,
        ablation::{
            ABLATION_REPORT_SCHEMA_VERSION, AblationReport, AblationReportInput, CuspGate,
            FoldMetricInput, RequiredBaseline,
        },
        branch_tracker::BranchTrackerConfig,
        fit::{
            EstimatorKind, FitConfig, FitDataset, FitResult, FitRow, FitTimeRange, PenaltyConfig,
            fit,
        },
        online::{
            CuspEngine, OnlineConfig, OnlineConfigInput, StructuralCadence, StructuralInput,
            StructuralQuality,
        },
        uncertainty::{LaplaceApproximation, LaplaceConfig, PosteriorParameterSamples},
    };
    use domain::{InstrumentId, SourceId, SourceKind, UnixNanos, VenueId};
    use feature_registry::{QualityRequirement, QualityScore};
    use fixed_decimal::{FixedDecimal, Price};
    use quality::SourceHealthState;
    use semver::Version;
    use tonic::{
        Code, Request, client::Grpc, codegen::http::uri::PathAndQuery, transport::Endpoint,
    };
    use zeroize::Zeroizing;

    use super::*;
    use crate::{
        auth::{Clock, SessionAuthenticator, SessionSecret, insert_authentication_metadata},
        proto::market_v1::SnapshotHealth,
        server::{LoopbackServer, MarketSnapshot},
        session::SessionDescriptor,
    };

    const BASE_TIME_NS: i64 = 1_800_000_000_000_000_000;
    const FIVE_MINUTES_NS: i64 = 300_000_000_000;
    const AUTH_TIME_SECONDS: i64 = 1_800_000_000;

    struct FixedClock;

    impl Clock for FixedClock {
        fn unix_seconds(&self) -> i64 {
            AUTH_TIME_SECONDS
        }
    }

    #[test]
    fn research_only_snapshot_is_explicit_and_never_claims_production_use() {
        let mut engine = engine();
        let snapshot = engine
            .update(available_input(BASE_TIME_NS))
            .expect("online snapshot");
        let gate = gate(engine.model_evidence_hash(), false);
        let mut store = CuspSnapshotStore::try_new(4).expect("store");
        store.publish(snapshot, gate).expect("publish");

        let state = store
            .current_state(&bitcoin())
            .expect("state mapping")
            .expect("current state");
        assert_eq!(state.availability, AvailabilityState::Experimental as i32);
        assert_eq!(
            state.weight_eligibility,
            CuspWeightEligibility::ResearchOnly as i32
        );
        assert_eq!(
            state.production_use,
            CuspProductionUse::NotUsedInProductionProbability as i32
        );
        assert_eq!(state.availability_reason, "ablation_gate_research_only");
        assert!(state.alpha.parse::<f64>().is_ok());
        assert!(state.beta.parse::<f64>().is_ok());
        assert_eq!(
            state
                .branch_probabilities
                .iter()
                .map(|probability| probability.probability_ppm)
                .sum::<u32>(),
            1_000_000
        );
        assert_eq!(state.evidence_blake3.len(), 32);
        assert_eq!(state.model_evidence_blake3.len(), 32);
        assert_eq!(state.gate_evidence_blake3.len(), 32);
        assert!(
            state
                .gate_checks
                .iter()
                .any(|check| !check.passed && check.kind == "improved_fold_fraction")
        );
        assert!(
            state
                .quality
                .expect("quality")
                .degradation_reasons
                .contains(&"not_used_in_production_probability".to_owned())
        );
    }

    #[test]
    fn eligibility_does_not_assign_a_weight_and_model_evidence_must_match() {
        let mut engine = engine();
        let snapshot = engine
            .update(available_input(BASE_TIME_NS))
            .expect("online snapshot");
        let eligible = gate(engine.model_evidence_hash(), true);
        let mut store = CuspSnapshotStore::try_new(4).expect("store");
        store
            .publish(snapshot.clone(), eligible)
            .expect("eligible publish");
        let state = store
            .current_state(&bitcoin())
            .expect("state mapping")
            .expect("current state");
        assert_eq!(state.availability, AvailabilityState::Available as i32);
        assert_eq!(
            state.weight_eligibility,
            CuspWeightEligibility::EligibleForProductionWeight as i32
        );
        assert_eq!(
            state.production_use,
            CuspProductionUse::NotUsedInProductionProbability as i32
        );

        let mismatched = gate([0x99; 32], true);
        let mut mismatched_store = CuspSnapshotStore::try_new(4).expect("store");
        assert_eq!(
            mismatched_store.publish(snapshot, mismatched),
            Err(CuspServiceError::ModelEvidenceMismatch)
        );
    }

    #[test]
    fn unavailable_snapshot_preserves_absence_instead_of_zero_risk() {
        let mut engine = engine();
        let snapshot = engine
            .update(
                StructuralInput::try_new(
                    BASE_TIME_NS,
                    0.7,
                    bitcoin(),
                    vector(0.25, 0.30),
                    Some(feature_covariance()),
                    StructuralQuality::new(
                        QualityScore::from_millionths(600_000).expect("score"),
                        QualityScore::from_millionths(600_000).expect("coverage"),
                        SourceHealthState::Unhealthy,
                    ),
                )
                .expect("rejected input"),
            )
            .expect("unavailable snapshot");
        let gate = gate(engine.model_evidence_hash(), false);
        let mut store = CuspSnapshotStore::try_new(4).expect("store");
        store.publish(snapshot, gate).expect("publish");

        let state = store
            .current_state(&bitcoin())
            .expect("state mapping")
            .expect("current state");
        assert_eq!(
            state.availability,
            AvailabilityState::SourceUnhealthy as i32
        );
        assert_eq!(state.availability_reason, "quality_rejected");
        assert!(state.alpha.is_empty());
        assert!(state.beta.is_empty());
        assert!(state.fold_distance.is_empty());
        assert!(state.cusp_region_probability.is_none());
        assert!(state.signed_discriminant.is_none());
        assert!(state.equilibria.is_empty());
        assert!(state.branch_probabilities.is_empty());
        assert_eq!(state.posterior_sample_count, 0);
    }

    #[test]
    fn history_is_bounded_ordered_and_exclusive_at_the_cursor() {
        let mut engine = engine();
        let gate = gate(engine.model_evidence_hash(), false);
        let mut store = CuspSnapshotStore::try_new(2).expect("store");
        for index in 0..3 {
            let as_of_ns = BASE_TIME_NS + i64::from(index) * FIVE_MINUTES_NS;
            let snapshot = engine
                .update(available_input(as_of_ns))
                .expect("online snapshot");
            store.publish(snapshot, gate.clone()).expect("publish");
        }

        let history = store.history_states(&bitcoin(), 2, None).expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(
            history[0].as_of_time.as_ref().expect("time").value,
            BASE_TIME_NS + FIVE_MINUTES_NS
        );
        assert_eq!(
            history[1].as_of_time.as_ref().expect("time").value,
            BASE_TIME_NS + 2 * FIVE_MINUTES_NS
        );
        let before_latest = store
            .history_states(&bitcoin(), 2, Some(BASE_TIME_NS + 2 * FIVE_MINUTES_NS))
            .expect("cursor history");
        assert_eq!(before_latest.len(), 1);
        assert_eq!(
            before_latest[0].as_of_time.as_ref().expect("time").value,
            BASE_TIME_NS + FIVE_MINUTES_NS
        );
        assert_eq!(
            store.history_states(&bitcoin(), 0, None),
            Err(CuspServiceError::InvalidHistoryRequest)
        );
    }

    #[test]
    fn rejected_out_of_order_publish_does_not_mutate_model_or_history_state() {
        let mut first_engine = engine_with_seed(91);
        let first = first_engine
            .update(available_input(BASE_TIME_NS + FIVE_MINUTES_NS))
            .expect("first snapshot");
        let mut store = CuspSnapshotStore::try_new(4).expect("store");
        store
            .publish(first, gate(first_engine.model_evidence_hash(), false))
            .expect("first publish");

        let mut second_engine = engine_with_seed(92);
        let second = second_engine
            .update(available_input(BASE_TIME_NS))
            .expect("out-of-order snapshot");
        let second_model_hash = second_engine.model_evidence_hash();
        assert_eq!(
            store.publish(second, gate(second_model_hash, false)),
            Err(CuspServiceError::OutOfOrderSnapshot)
        );
        assert!(!store.gate_by_model.contains_key(&second_model_hash));
        assert_eq!(
            store.records.get(&bitcoin()).map(VecDeque::len),
            Some(1),
            "a rejected publish must leave retained history unchanged"
        );
    }

    #[test]
    fn store_limits_bound_unique_assets_and_frozen_models_without_partial_mutation() {
        let mut first_engine = engine_with_seed(91);
        let first = first_engine
            .update(available_input(BASE_TIME_NS))
            .expect("first snapshot");
        let first_gate = gate(first_engine.model_evidence_hash(), false);
        let mut store =
            CuspSnapshotStore::try_new_with_limits(4, 1, 1).expect("bounded test store");
        store
            .publish(first, first_gate.clone())
            .expect("first publish");

        let mut second_model = engine_with_seed(92);
        let second = second_model
            .update(available_input(BASE_TIME_NS + FIVE_MINUTES_NS))
            .expect("second-model snapshot");
        assert_eq!(
            store.publish(second, gate(second_model.model_evidence_hash(), false)),
            Err(CuspServiceError::ModelCapacity)
        );

        let mut second_asset = engine_with_seed(91);
        let ether_snapshot = second_asset
            .update(available_input_for(BASE_TIME_NS + FIVE_MINUTES_NS, ether()))
            .expect("second-asset snapshot");
        assert_eq!(
            store.publish(ether_snapshot, first_gate),
            Err(CuspServiceError::AssetCapacity)
        );
        assert_eq!(store.records.len(), 1);
        assert_eq!(store.gate_by_model.len(), 1);
        assert_eq!(store.records.get(&bitcoin()).map(VecDeque::len), Some(1));
    }

    #[test]
    fn global_record_capacity_evicts_the_deterministic_oldest_projection() {
        let mut first_engine = engine_with_seed(91);
        let model_hash = first_engine.model_evidence_hash();
        let mut second_engine = engine_with_seed(91);
        let mut store =
            CuspSnapshotStore::try_new_with_all_limits(4, 4, 4, 2).expect("bounded store");
        store
            .publish(
                first_engine
                    .update(available_input(BASE_TIME_NS))
                    .expect("first snapshot"),
                gate(model_hash, false),
            )
            .expect("first publish");
        store
            .publish(
                second_engine
                    .update(available_input_for(BASE_TIME_NS + FIVE_MINUTES_NS, ether()))
                    .expect("second snapshot"),
                gate(model_hash, false),
            )
            .expect("second publish");
        store
            .publish(
                first_engine
                    .update(available_input(BASE_TIME_NS + 2 * FIVE_MINUTES_NS))
                    .expect("third snapshot"),
                gate(model_hash, false),
            )
            .expect("third publish");

        assert_eq!(store.total_records, 2);
        let bitcoin_history = store
            .history_states(&bitcoin(), 4, None)
            .expect("bitcoin history");
        assert_eq!(bitcoin_history.len(), 1);
        assert_eq!(
            bitcoin_history[0].as_of_time.as_ref().expect("time").value,
            BASE_TIME_NS + 2 * FIVE_MINUTES_NS
        );
        assert_eq!(
            store
                .history_states(&ether(), 4, None)
                .expect("ether history")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn tonic_boundary_uses_specific_status_codes_and_bounded_requests() {
        let mut engine = engine();
        let snapshot = engine
            .update(available_input(BASE_TIME_NS))
            .expect("online snapshot");
        let gate = gate(engine.model_evidence_hash(), false);
        let mut store = CuspSnapshotStore::try_new(4).expect("store");
        store.publish(snapshot, gate).expect("publish");
        let (service, descriptor, token) = authenticated_service(store);

        let unauthenticated = service
            .get_cusp_state(Request::new(CuspServiceGetCuspStateRequest {
                asset: Some(asset_to_proto(&bitcoin())),
            }))
            .await
            .expect_err("missing authentication");
        assert_eq!(unauthenticated.code(), Code::Unauthenticated);
        assert_eq!(unauthenticated.message(), "authentication failed");

        let response = service
            .get_cusp_state(insert_authentication_metadata(
                Request::new(CuspServiceGetCuspStateRequest {
                    asset: Some(asset_to_proto(&bitcoin())),
                }),
                &descriptor,
                &token,
            ))
            .await
            .expect("current response")
            .into_inner();
        assert!(response.cusp_state.is_some());

        let missing = service
            .get_cusp_state(insert_authentication_metadata(
                Request::new(CuspServiceGetCuspStateRequest { asset: None }),
                &descriptor,
                &token,
            ))
            .await
            .expect_err("missing asset");
        assert_eq!(missing.code(), Code::InvalidArgument);

        let not_found = service
            .get_cusp_state(insert_authentication_metadata(
                Request::new(CuspServiceGetCuspStateRequest {
                    asset: Some(asset_to_proto(&ether())),
                }),
                &descriptor,
                &token,
            ))
            .await
            .expect_err("missing state");
        assert_eq!(not_found.code(), Code::NotFound);

        let unbounded = service
            .get_cusp_history(insert_authentication_metadata(
                Request::new(CuspServiceGetCuspHistoryRequest {
                    asset: Some(asset_to_proto(&bitcoin())),
                    limit: u32::try_from(MAX_CUSP_HISTORY_REQUEST + 1).expect("bounded constant"),
                    before_as_of_time: None,
                }),
                &descriptor,
                &token,
            ))
            .await
            .expect_err("unbounded history");
        assert_eq!(unbounded.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn authenticated_loopback_round_trip_returns_published_cusp_state() {
        let mut engine = engine();
        let snapshot = engine
            .update(available_input(BASE_TIME_NS))
            .expect("online snapshot");
        let gate = gate(engine.model_evidence_hash(), false);

        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock after epoch")
            .as_secs()
            .try_into()
            .expect("test clock fits i64");
        let descriptor = SessionDescriptor::issue(1, 0, 4_242, [0x31; 16], [0x42; 16], issued_at)
            .expect("test descriptor");
        let secret =
            || SessionSecret::try_from(Zeroizing::new(vec![0x6b; 32])).expect("test secret");
        let token = SessionAuthenticator::new(secret()).token(&descriptor);
        let server = LoopbackServer::spawn(
            "127.0.0.1:0".parse().expect("loopback bind"),
            secret(),
            descriptor.clone(),
            market_snapshot(),
        )
        .await
        .expect("loopback server");
        {
            let store = server.cusp_store();
            store
                .write()
                .expect("cusp store lock")
                .publish(snapshot, gate)
                .expect("publish snapshot");
        }

        let channel = Endpoint::from_shared(format!("http://{}", server.local_addr()))
            .expect("loopback endpoint")
            .connect()
            .await
            .expect("connect loopback");
        let mut client = Grpc::new(channel);
        client.ready().await.expect("cusp service ready");
        let request = insert_authentication_metadata(
            Request::new(CuspServiceGetCuspStateRequest {
                asset: Some(asset_to_proto(&bitcoin())),
            }),
            &descriptor,
            &token,
        );
        let response: Response<CuspServiceGetCuspStateResponse> = client
            .unary(
                request,
                PathAndQuery::from_static("/cmti.risk.v1.CuspService/GetCuspState"),
                tonic_prost::ProstCodec::default(),
            )
            .await
            .expect("authenticated wire response");
        let state = response
            .into_inner()
            .cusp_state
            .expect("published cusp state");
        assert_eq!(state.asset, Some(asset_to_proto(&bitcoin())));
        assert_eq!(
            state.production_use,
            CuspProductionUse::NotUsedInProductionProbability as i32
        );
        assert_eq!(state.model_evidence_blake3, engine.model_evidence_hash());
        assert_eq!(state.as_of_time.expect("as-of time").value, BASE_TIME_NS);

        server.shutdown().await.expect("clean loopback shutdown");
    }

    fn authenticated_service(
        store: CuspSnapshotStore,
    ) -> (
        LocalCuspService,
        SessionDescriptor,
        crate::auth::AuthenticationToken,
    ) {
        let descriptor =
            SessionDescriptor::issue(1, 0, 4_242, [0x11; 16], [0x22; 16], AUTH_TIME_SECONDS)
                .expect("test descriptor");
        let secret =
            || SessionSecret::try_from(Zeroizing::new(vec![0x6b; 32])).expect("test secret");
        let token = SessionAuthenticator::new(secret()).token(&descriptor);
        let request_authenticator = RequestAuthenticator::new(
            descriptor.clone(),
            Arc::new(SessionAuthenticator::new(secret())),
            Arc::new(FixedClock),
        );
        (
            LocalCuspService::new(store, request_authenticator),
            descriptor,
            token,
        )
    }

    fn market_snapshot() -> MarketSnapshot {
        let source = SourceId::new(SourceKind::Exchange, "binance", 1).expect("source");
        let instrument = InstrumentId::new(VenueId::new("binance").expect("venue"), "btcusdt", 1)
            .expect("instrument");
        let best_bid = Price::new(FixedDecimal::parse_canonical("67234.1").expect("bid"))
            .expect("positive bid");
        let best_ask = Price::new(FixedDecimal::parse_canonical("67234.11").expect("ask"))
            .expect("positive ask");
        MarketSnapshot::new(
            source,
            instrument,
            bitcoin(),
            1,
            best_bid,
            best_ask,
            SnapshotHealth::Healthy,
            UnixNanos::new(BASE_TIME_NS),
            UnixNanos::new(BASE_TIME_NS + 1_000_000),
            1,
        )
        .expect("market snapshot")
    }

    fn engine() -> CuspEngine {
        engine_with_seed(91)
    }

    fn engine_with_seed(feature_uncertainty_seed: u64) -> CuspEngine {
        let (fit, samples) = fitted_posterior();
        CuspEngine::try_new(
            fit,
            samples,
            ControlCovariance::try_new(0.08, 0.01, 0.12).expect("whitening covariance"),
            OnlineConfig::try_new(OnlineConfigInput {
                schema_version: 1,
                cadence: StructuralCadence::FiveMinutes,
                feature_uncertainty_seed,
                branch: BranchTrackerConfig::try_new(0.5, 0.01, 0.70).expect("branch config"),
                production_quality: quality_requirement(900_000),
                research_quality: quality_requirement(700_000),
            })
            .expect("online config"),
            Some(BranchId::Upper),
        )
        .expect("engine")
    }

    fn fitted_posterior() -> (FitResult, PosteriorParameterSamples) {
        static FITTED: OnceLock<(FitResult, PosteriorParameterSamples)> = OnceLock::new();
        FITTED
            .get_or_init(|| {
                let fitted =
                    fit(&transition_data(), smooth_config()).expect("converged transition fit");
                let approximation =
                    LaplaceApproximation::from_fit(&fitted, LaplaceConfig::fixture())
                        .expect("Laplace fit");
                let samples = approximation
                    .sample_parameters(73, 32)
                    .expect("parameter samples");
                (fitted, samples)
            })
            .clone()
    }

    fn gate(candidate_model_hash: [u8; 32], passes: bool) -> GateEvaluation {
        let mut folds = vec![
            fold(1, "low-volatility"),
            fold(2, "high-volatility"),
            fold(3, "liquidity-stress"),
            fold(4, "low-volatility"),
            fold(5, "high-volatility"),
        ];
        if !passes {
            for fold in &mut folds[1..] {
                fold.candidate_brier_score = 0.21;
            }
        }
        let report = AblationReport::try_new(AblationReportInput {
            schema_version: ABLATION_REPORT_SCHEMA_VERSION,
            dataset_manifest_hash: [0x11; 32],
            candidate_model_hash,
            baseline_model_hash: [0x33; 32],
            required_baselines: vec![
                RequiredBaseline::Volatility,
                RequiredBaseline::Leverage,
                RequiredBaseline::Regime,
                RequiredBaseline::Microstructure,
            ],
            folds,
            maximum_standardized_coefficient_drift: 0.75,
            sign_scaling_parity: true,
            numerical_failures: 0,
            attempted_model_variants: 3,
        })
        .expect("ablation report");
        CuspGate::default()
            .evaluate(&report)
            .expect("gate evaluation")
    }

    fn fold(index: usize, regime_id: &str) -> FoldMetricInput {
        FoldMetricInput {
            fold_id: format!("outer-{index}"),
            regime_id: regime_id.to_owned(),
            test_start_ns: BASE_TIME_NS + index as i64 * 1_000_000_000,
            test_end_ns: BASE_TIME_NS + index as i64 * 1_000_000_000 + 500_000_000,
            observation_count: 1_000,
            positive_event_count: 10,
            independent_event_episode_count: 2,
            baseline_brier_score: 0.20,
            candidate_brier_score: 0.16,
            calibration_slope: 1.0,
            calibration_intercept: 0.01,
            expected_calibration_error: 0.02,
            alert_budget_score_delta: 0.03,
        }
    }

    fn available_input(as_of_ns: i64) -> StructuralInput {
        available_input_for(as_of_ns, bitcoin())
    }

    fn available_input_for(as_of_ns: i64, asset: AssetId) -> StructuralInput {
        StructuralInput::try_new(
            as_of_ns,
            0.7,
            asset,
            vector(0.25, 0.30),
            Some(feature_covariance()),
            StructuralQuality::new(
                QualityScore::from_millionths(950_000).expect("score"),
                QualityScore::from_millionths(950_000).expect("coverage"),
                SourceHealthState::Healthy,
            ),
        )
        .expect("available input")
    }

    fn quality_requirement(score: u32) -> QualityRequirement {
        QualityRequirement::try_new(
            QualityScore::from_millionths(score).expect("score"),
            QualityScore::from_millionths(score).expect("coverage"),
            vec![SourceHealthState::Healthy],
        )
        .expect("quality requirement")
    }

    fn transition_data() -> FitDataset {
        let mut rows = Vec::new();
        for index in 0..96_usize {
            let alpha_feature = centered_cycle(index, 17, 8.0);
            let beta_feature = centered_cycle(index * 7 + 3, 19, 9.0);
            let state = centered_cycle(index * 13 + 5, 23, 7.0);
            let alpha = 0.12 + 0.65 * alpha_feature;
            let beta = 0.55 + 0.48 * beta_feature;
            let delta_time = 0.08;
            let scale = 0.12;
            let noise = [-0.8, -0.35, 0.0, 0.25, 0.65, 0.15, -0.2][index % 7];
            let drift = alpha + beta * state - state.powi(3);
            rows.push(
                FitRow::try_new(
                    bitcoin(),
                    vector(alpha_feature, beta_feature),
                    BASE_TIME_NS + index as i64 * 60_000_000_000,
                    state,
                    drift * delta_time + scale * delta_time.sqrt() * noise,
                    delta_time,
                    scale,
                    1.0,
                )
                .expect("transition row"),
            );
        }
        FitDataset::try_new(
            control_schema(),
            [17; 32],
            [23; 32],
            [29; 32],
            FitTimeRange::try_new(BASE_TIME_NS, BASE_TIME_NS + 96 * 60_000_000_000)
                .expect("time range"),
            ControlCovariance::try_new(0.08, 0.01, 0.12).expect("control covariance"),
            rows,
        )
        .expect("fit dataset")
    }

    fn smooth_config() -> FitConfig {
        FitConfig::fixture(EstimatorKind::StudentTTransition)
            .with_penalty(PenaltyConfig::try_new(0.002, 0.0, 2.0).expect("smooth penalty"))
    }

    fn control_schema() -> ControlSchema {
        ControlSchema::try_new(
            Version::new(1, 0, 0),
            vec![
                FeatureGroup::try_new(
                    "alpha_group",
                    CoefficientSign::Any,
                    CoefficientSign::Any,
                    1.0,
                )
                .expect("alpha group"),
                FeatureGroup::try_new(
                    "beta_group",
                    CoefficientSign::Any,
                    CoefficientSign::NonNegative,
                    1.0,
                )
                .expect("beta group"),
            ],
            vec![
                ControlFeature::try_new(
                    key("alpha_signal"),
                    true,
                    "alpha_group",
                    ControlTarget::Alpha,
                    0.0,
                    1.0,
                )
                .expect("alpha feature"),
                ControlFeature::try_new(
                    key("beta_signal"),
                    true,
                    "beta_group",
                    ControlTarget::Beta,
                    0.0,
                    1.0,
                )
                .expect("beta feature"),
            ],
        )
        .expect("control schema")
    }

    fn feature_covariance() -> FeatureCovariance {
        FeatureCovariance::try_new(
            vec![key("alpha_signal"), key("beta_signal")],
            vec![vec![0.01, 0.002], vec![0.002, 0.015]],
        )
        .expect("feature covariance")
    }

    fn vector(alpha: f64, beta: f64) -> ControlVector {
        ControlVector::try_new(vec![
            ControlValue::present(key("alpha_signal"), alpha).expect("alpha value"),
            ControlValue::present(key("beta_signal"), beta).expect("beta value"),
        ])
        .expect("control vector")
    }

    fn key(id: &str) -> ControlFeatureKey {
        ControlFeatureKey::try_new(id, Version::new(1, 0, 0)).expect("feature key")
    }

    fn bitcoin() -> AssetId {
        AssetId::new(AssetNamespace::Native, "bitcoin", "", "BTC", 1).expect("bitcoin")
    }

    fn ether() -> AssetId {
        AssetId::new(AssetNamespace::Native, "ethereum", "", "ETH", 1).expect("ether")
    }

    fn centered_cycle(index: usize, modulus: usize, denominator: f64) -> f64 {
        (index % modulus) as f64 / denominator - (modulus / 2) as f64 / denominator
    }
}
