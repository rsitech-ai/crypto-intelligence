//! Canonical evidence hashing primitives for online cusp snapshots.

use domain::AssetId;

use crate::{ControlDatum, ControlFeatureKey, ControlMissingReason, ControlVector};

pub(crate) struct EvidenceBuilder {
    hasher: blake3::Hasher,
}

impl EvidenceBuilder {
    pub(crate) fn new(domain: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(domain.len() as u64).to_le_bytes());
        hasher.update(domain);
        Self { hasher }
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.length(value.len());
        self.hasher.update(value);
    }

    pub(crate) fn digest(&mut self, value: [u8; 32]) {
        self.hasher.update(&value);
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.hasher.update(&[value]);
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.hasher.update(&value.to_le_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.hasher.update(&value.to_le_bytes());
    }

    pub(crate) fn usize(&mut self, value: usize) {
        self.length(value);
    }

    pub(crate) fn i64(&mut self, value: i64) {
        self.hasher.update(&value.to_le_bytes());
    }

    pub(crate) fn f64(&mut self, value: f64) {
        self.hasher.update(&value.to_bits().to_le_bytes());
    }

    pub(crate) fn boolean(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    pub(crate) fn asset(&mut self, asset: &AssetId) {
        self.u8(asset.namespace() as u8);
        self.bytes(asset.chain_id().as_bytes());
        self.bytes(asset.contract_or_mint().as_bytes());
        self.bytes(asset.canonical_symbol().as_bytes());
        self.u32(asset.generation());
    }

    pub(crate) fn feature_key(&mut self, key: &ControlFeatureKey) {
        self.bytes(key.id().as_bytes());
        self.bytes(key.version().to_string().as_bytes());
    }

    pub(crate) fn control_vector(&mut self, features: &ControlVector) {
        self.usize(features.values().len());
        for value in features.values() {
            self.feature_key(&value.key);
            match value.datum {
                ControlDatum::Present(number) => {
                    self.u8(1);
                    self.f64(number);
                }
                ControlDatum::Missing(reason) => {
                    self.u8(2);
                    self.u8(control_missing_reason_tag(reason));
                }
            }
        }
    }

    pub(crate) fn finish(self) -> [u8; 32] {
        *self.hasher.finalize().as_bytes()
    }

    fn length(&mut self, value: usize) {
        self.u64(value as u64);
    }
}

pub(crate) const fn control_missing_reason_tag(reason: ControlMissingReason) -> u8 {
    match reason {
        ControlMissingReason::Stale => 1,
        ControlMissingReason::InsufficientHistory => 2,
        ControlMissingReason::WindowNotFinal => 3,
        ControlMissingReason::QualityRejected => 4,
        ControlMissingReason::SourceUnavailable => 5,
    }
}
