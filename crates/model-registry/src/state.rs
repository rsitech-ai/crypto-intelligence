use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CompatiblePackage, VerifiedPackage};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    Research,
    Candidate,
    Shadow,
    Production,
    Superseded,
    Revoked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionRecord {
    pub sequence: u64,
    pub from: Option<ModelState>,
    pub to: ModelState,
    pub actor: String,
    pub reviewer: Option<String>,
    pub evidence_id: String,
    pub transitioned_at_ns: i64,
    pub package_blake3: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndependentReview {
    requester: String,
    reviewer: String,
    evidence_id: String,
    reviewed_at_ns: i64,
}

impl IndependentReview {
    pub fn new(
        requester: impl Into<String>,
        reviewer: impl Into<String>,
        evidence_id: impl Into<String>,
        reviewed_at_ns: i64,
    ) -> Result<Self, PromotionError> {
        let requester = requester.into();
        let reviewer = reviewer.into();
        let evidence_id = evidence_id.into();
        validate_actor(&requester)?;
        validate_actor(&reviewer)?;
        validate_evidence_id(&evidence_id)?;
        if requester == reviewer {
            return Err(PromotionError::MissingIndependentReview);
        }
        if reviewed_at_ns <= 0 {
            return Err(PromotionError::InvalidTime);
        }
        Ok(Self {
            requester,
            reviewer,
            evidence_id,
            reviewed_at_ns,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowEvidence {
    package_blake3: [u8; 32],
    evidence_id: String,
    observations: u64,
    positives: u64,
    started_at_ns: i64,
    ended_at_ns: i64,
}

impl ShadowEvidence {
    pub fn new(
        package_blake3: [u8; 32],
        evidence_id: impl Into<String>,
        observations: u64,
        positives: u64,
        started_at_ns: i64,
        ended_at_ns: i64,
    ) -> Result<Self, PromotionError> {
        let evidence_id = evidence_id.into();
        validate_evidence_id(&evidence_id)?;
        if package_blake3 == [0; 32]
            || observations == 0
            || positives == 0
            || positives > observations
        {
            return Err(PromotionError::InsufficientShadowEvidence);
        }
        if started_at_ns <= 0 || ended_at_ns <= started_at_ns {
            return Err(PromotionError::InvalidTime);
        }
        Ok(Self {
            package_blake3,
            evidence_id,
            observations,
            positives,
            started_at_ns,
            ended_at_ns,
        })
    }
}

/// Auditable metadata retained even after supersession or revocation.
#[derive(Clone)]
pub struct ModelRecord {
    package: VerifiedPackage,
    state: ModelState,
    author: String,
    history: Vec<TransitionRecord>,
}

impl ModelRecord {
    pub fn model_id(&self) -> &str {
        self.package.model_id()
    }

    pub const fn state(&self) -> ModelState {
        self.state
    }

    pub fn author(&self) -> &str {
        &self.author
    }

    pub fn history(&self) -> &[TransitionRecord] {
        &self.history
    }

    pub fn package(&self) -> &VerifiedPackage {
        &self.package
    }
}

#[derive(Default)]
pub struct ModelRegistry {
    records: BTreeMap<String, ModelRecord>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_research(
        &mut self,
        package: VerifiedPackage,
        author: impl Into<String>,
        evidence_id: impl Into<String>,
        registered_at_ns: i64,
    ) -> Result<(), PromotionError> {
        let author = author.into();
        let evidence_id = evidence_id.into();
        validate_actor(&author)?;
        validate_evidence_id(&evidence_id)?;
        if registered_at_ns <= 0 {
            return Err(PromotionError::InvalidTime);
        }
        let model_id = package.model_id().to_owned();
        if self.records.contains_key(&model_id) {
            return Err(PromotionError::DuplicateModel);
        }
        let package_blake3 = package.package_blake3();
        self.records.insert(
            model_id,
            ModelRecord {
                package,
                state: ModelState::Research,
                author: author.clone(),
                history: vec![TransitionRecord {
                    sequence: 1,
                    from: None,
                    to: ModelState::Research,
                    actor: author,
                    reviewer: None,
                    evidence_id,
                    transitioned_at_ns: registered_at_ns,
                    package_blake3,
                }],
            },
        );
        Ok(())
    }

    pub fn promote_candidate(
        &mut self,
        model_id: &str,
        review: &IndependentReview,
    ) -> Result<(), PromotionError> {
        let record = self.record_mut(model_id)?;
        validate_review(record, review)?;
        transition(
            record,
            ModelState::Research,
            ModelState::Candidate,
            &review.requester,
            Some(&review.reviewer),
            &review.evidence_id,
            review.reviewed_at_ns,
        )
    }

    pub fn promote_shadow(
        &mut self,
        model_id: &str,
        review: &IndependentReview,
    ) -> Result<(), PromotionError> {
        let record = self.record_mut(model_id)?;
        validate_review(record, review)?;
        transition(
            record,
            ModelState::Candidate,
            ModelState::Shadow,
            &review.requester,
            Some(&review.reviewer),
            &review.evidence_id,
            review.reviewed_at_ns,
        )
    }

    pub fn promote_production(
        &mut self,
        model_id: &str,
        compatibility: Option<&CompatiblePackage>,
        review: Option<&IndependentReview>,
        shadow: Option<&ShadowEvidence>,
    ) -> Result<(), PromotionError> {
        let record = self.record_mut(model_id)?;
        let review = review.ok_or(PromotionError::MissingIndependentReview)?;
        validate_review(record, review)?;
        let compatibility = compatibility.ok_or(PromotionError::MissingCompatibility)?;
        if compatibility.model_id() != model_id
            || compatibility.package_blake3() != record.package.package_blake3()
        {
            return Err(PromotionError::CompatibilityMismatch);
        }
        let shadow = shadow.ok_or(PromotionError::InsufficientShadowEvidence)?;
        if shadow.package_blake3 != record.package.package_blake3()
            || shadow.positives
                < record
                    .package
                    .package()
                    .manifest
                    .quality_requirements
                    .minimum_shadow_positives
            || shadow.started_at_ns
                <= record
                    .history
                    .last()
                    .map_or(0, |transition| transition.transitioned_at_ns)
            || shadow.ended_at_ns >= review.reviewed_at_ns
        {
            return Err(PromotionError::InsufficientShadowEvidence);
        }
        transition(
            record,
            ModelState::Shadow,
            ModelState::Production,
            &review.requester,
            Some(&review.reviewer),
            &format!(
                "{}:{}:{}",
                review.evidence_id,
                shadow.evidence_id,
                hex::encode(compatibility.request_blake3())
            ),
            review.reviewed_at_ns,
        )
    }

    pub fn supersede(
        &mut self,
        model_id: &str,
        replacement_model_id: &str,
        actor: &str,
        evidence_id: &str,
        at_ns: i64,
    ) -> Result<(), PromotionError> {
        if model_id == replacement_model_id
            || self
                .records
                .get(replacement_model_id)
                .is_none_or(|record| record.state != ModelState::Production)
        {
            return Err(PromotionError::InvalidReplacement);
        }
        let record = self.record_mut(model_id)?;
        transition(
            record,
            ModelState::Production,
            ModelState::Superseded,
            actor,
            None,
            evidence_id,
            at_ns,
        )
    }

    pub fn revoke(
        &mut self,
        model_id: &str,
        actor: &str,
        evidence_id: &str,
        at_ns: i64,
    ) -> Result<(), PromotionError> {
        let record = self.record_mut(model_id)?;
        if matches!(record.state, ModelState::Superseded | ModelState::Revoked) {
            return Err(PromotionError::InvalidTransition);
        }
        let from = record.state;
        transition(
            record,
            from,
            ModelState::Revoked,
            actor,
            None,
            evidence_id,
            at_ns,
        )
    }

    pub fn may_infer_production(&self, model_id: &str) -> bool {
        self.records
            .get(model_id)
            .is_some_and(|record| record.state == ModelState::Production)
    }

    pub fn get(&self, model_id: &str) -> Result<&ModelRecord, PromotionError> {
        self.records
            .get(model_id)
            .ok_or(PromotionError::ModelNotFound)
    }

    fn record_mut(&mut self, model_id: &str) -> Result<&mut ModelRecord, PromotionError> {
        self.records
            .get_mut(model_id)
            .ok_or(PromotionError::ModelNotFound)
    }
}

fn validate_review(record: &ModelRecord, review: &IndependentReview) -> Result<(), PromotionError> {
    if review.requester != record.author || review.reviewer == record.author {
        return Err(PromotionError::MissingIndependentReview);
    }
    let previous_time = record
        .history
        .last()
        .map_or(0, |transition| transition.transitioned_at_ns);
    if review.reviewed_at_ns <= previous_time {
        return Err(PromotionError::InvalidTime);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn transition(
    record: &mut ModelRecord,
    expected: ModelState,
    target: ModelState,
    actor: &str,
    reviewer: Option<&str>,
    evidence_id: &str,
    at_ns: i64,
) -> Result<(), PromotionError> {
    validate_actor(actor)?;
    if let Some(reviewer) = reviewer {
        validate_actor(reviewer)?;
    }
    validate_evidence_id(evidence_id)?;
    if record.state != expected {
        return Err(PromotionError::InvalidTransition);
    }
    if at_ns
        <= record
            .history
            .last()
            .map_or(0, |transition| transition.transitioned_at_ns)
    {
        return Err(PromotionError::InvalidTime);
    }
    let sequence = u64::try_from(record.history.len())
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or(PromotionError::HistoryCapacity)?;
    if sequence > 64 {
        return Err(PromotionError::HistoryCapacity);
    }
    let from = record.state;
    record.state = target;
    record.history.push(TransitionRecord {
        sequence,
        from: Some(from),
        to: target,
        actor: actor.to_owned(),
        reviewer: reviewer.map(str::to_owned),
        evidence_id: evidence_id.to_owned(),
        transitioned_at_ns: at_ns,
        package_blake3: record.package.package_blake3(),
    });
    Ok(())
}

fn validate_actor(actor: &str) -> Result<(), PromotionError> {
    if actor.is_empty()
        || actor.len() > 256
        || actor
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        Err(PromotionError::InvalidActor)
    } else {
        Ok(())
    }
}

fn validate_evidence_id(evidence_id: &str) -> Result<(), PromotionError> {
    if evidence_id.is_empty()
        || evidence_id.len() > 1_024
        || evidence_id.bytes().any(|byte| byte.is_ascii_control())
    {
        Err(PromotionError::InvalidEvidence)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PromotionError {
    #[error("model is not registered")]
    ModelNotFound,
    #[error("model identifier is already registered")]
    DuplicateModel,
    #[error("lifecycle transition is invalid")]
    InvalidTransition,
    #[error("independent review is required")]
    MissingIndependentReview,
    #[error("verified runtime compatibility is required")]
    MissingCompatibility,
    #[error("compatibility evidence belongs to another package")]
    CompatibilityMismatch,
    #[error("shadow evidence is insufficient or belongs to another package")]
    InsufficientShadowEvidence,
    #[error("replacement must name another production model")]
    InvalidReplacement,
    #[error("actor identifier is invalid")]
    InvalidActor,
    #[error("evidence identifier is invalid")]
    InvalidEvidence,
    #[error("transition time is invalid")]
    InvalidTime,
    #[error("transition history exceeds its bounded capacity")]
    HistoryCapacity,
}
