//! Connector-neutral authority retained for normalized derivative observations.

use std::num::NonZeroU64;

use domain::{InstrumentDefinition, UnixNanos};
use event_envelope::{EventEnvelope, EventType};
use instrument_registry::{CatalogSnapshot, ResolvedInstrument};
use thiserror::Error;

use crate::{Completeness, ConnectorCapabilities, DurableRawReference, StreamClass};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DerivativeStream {
    Funding,
    OpenInterest,
    MarkIndex,
    Liquidation,
}

impl DerivativeStream {
    const fn from_event_type(event_type: EventType) -> Option<Self> {
        match event_type {
            EventType::FundingObservation => Some(Self::Funding),
            EventType::OpenInterestObservation => Some(Self::OpenInterest),
            EventType::MarkIndexObservation => Some(Self::MarkIndex),
            EventType::LiquidationObservation => Some(Self::Liquidation),
            _ => None,
        }
    }

    const fn stream_class(self) -> StreamClass {
        match self {
            Self::Funding => StreamClass::Funding,
            Self::OpenInterest => StreamClass::OpenInterest,
            Self::MarkIndex => StreamClass::MarkAndIndex,
            Self::Liquidation => StreamClass::Liquidations,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CatalogAuthority {
    as_known_at: UnixNanos,
    catalog_revision: NonZeroU64,
    definition_revision: NonZeroU64,
    catalog_digest: [u8; 32],
    definition_hash: [u8; 32],
}

impl CatalogAuthority {
    fn try_from_resolved(
        resolved: ResolvedInstrument<'_>,
    ) -> Result<Self, DerivativeAuthorityError> {
        let as_known_at = resolved.as_known_at();
        let catalog_revision = NonZeroU64::new(resolved.catalog_revision().get())
            .ok_or(DerivativeAuthorityError::InvalidCatalogAuthority)?;
        let definition_revision = NonZeroU64::new(resolved.definition_revision().get())
            .ok_or(DerivativeAuthorityError::InvalidCatalogAuthority)?;
        let catalog_digest = *resolved.catalog_digest();
        let definition_hash = *resolved.definition_hash();
        if as_known_at.value() <= 0 || catalog_digest == [0; 32] || definition_hash == [0; 32] {
            return Err(DerivativeAuthorityError::InvalidCatalogAuthority);
        }
        Ok(Self {
            as_known_at,
            catalog_revision,
            definition_revision,
            catalog_digest,
            definition_hash,
        })
    }

    pub const fn as_known_at(self) -> UnixNanos {
        self.as_known_at
    }

    pub const fn catalog_revision(self) -> NonZeroU64 {
        self.catalog_revision
    }

    pub const fn definition_revision(self) -> NonZeroU64 {
        self.definition_revision
    }

    pub const fn catalog_digest(self) -> [u8; 32] {
        self.catalog_digest
    }

    pub const fn definition_hash(self) -> [u8; 32] {
        self.definition_hash
    }
}

/// Opaque evidence that remains attached to live normalized output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivativeNormalizationReceipt {
    event: EventEnvelope,
    stream: DerivativeStream,
    completeness: Completeness,
    connector_version: String,
    instrument_definition: InstrumentDefinition,
    catalog: CatalogAuthority,
}

impl DerivativeNormalizationReceipt {
    /// Mints connector-neutral derivative authority only from an acknowledged
    /// raw-WAL record and an integrity-checked point-in-time catalog.
    ///
    /// Callers cannot self-assert catalog revisions or hashes: the matching
    /// definition and every catalog identity field are derived from the
    /// verified snapshot after the event is bound to the exact durable record.
    pub fn try_from_verified(
        capabilities: &ConnectorCapabilities,
        raw: &DurableRawReference,
        catalog_snapshot: &CatalogSnapshot,
        event: EventEnvelope,
    ) -> Result<Self, DerivativeAuthorityError> {
        event
            .verify()
            .map_err(|_| DerivativeAuthorityError::InvalidEvent)?;
        let stream = DerivativeStream::from_event_type(event.event_type())
            .ok_or(DerivativeAuthorityError::UnsupportedEvent)?;
        let metadata = event.metadata().as_unchecked();
        if !catalog_snapshot.verify_integrity()
            || catalog_snapshot.as_known_at() > metadata.normalization_timestamp
            || &metadata.source != raw.source()
            || metadata.connection_epoch != raw.connection_epoch().get()
            || &metadata.raw_payload_hash != raw.payload_hash()
            || metadata.receive_wall_timestamp != raw.receive_wall_time()
            || metadata.receive_monotonic_ns != raw.receive_monotonic_ns()
        {
            return Err(DerivativeAuthorityError::AuthorityMismatch);
        }
        let instrument_id = metadata
            .instrument_id
            .as_ref()
            .ok_or(DerivativeAuthorityError::AuthorityMismatch)?;
        let event_time = metadata
            .exchange_transaction_timestamp
            .or(metadata.exchange_timestamp)
            .ok_or(DerivativeAuthorityError::AuthorityMismatch)?;
        let resolved = catalog_snapshot
            .resolve_id(instrument_id, event_time)
            .map_err(|_| DerivativeAuthorityError::AuthorityMismatch)?;
        let instrument_definition = resolved.definition().clone();
        if instrument_definition.id().venue() != capabilities.venue()
            || metadata.source.name() != capabilities.venue().as_str()
        {
            return Err(DerivativeAuthorityError::AuthorityMismatch);
        }
        let catalog = CatalogAuthority::try_from_resolved(resolved)?;
        let completeness = if stream == DerivativeStream::Liquidation {
            capabilities.liquidation_completeness()
        } else {
            capabilities.completeness().get(stream.stream_class())
        };
        Ok(Self {
            event,
            stream,
            completeness,
            connector_version: capabilities.connector_version().to_owned(),
            instrument_definition,
            catalog,
        })
    }

    pub const fn event(&self) -> &EventEnvelope {
        &self.event
    }

    pub const fn stream(&self) -> DerivativeStream {
        self.stream
    }

    pub const fn completeness(&self) -> Completeness {
        self.completeness
    }

    pub fn connector_version(&self) -> &str {
        &self.connector_version
    }

    pub const fn instrument_definition(&self) -> &InstrumentDefinition {
        &self.instrument_definition
    }

    pub const fn catalog_authority(&self) -> CatalogAuthority {
        self.catalog
    }

    pub const fn catalog_as_known_at(&self) -> UnixNanos {
        self.catalog.as_known_at
    }

    pub const fn catalog_digest(&self) -> &[u8; 32] {
        &self.catalog.catalog_digest
    }

    pub const fn definition_hash(&self) -> &[u8; 32] {
        &self.catalog.definition_hash
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum DerivativeAuthorityError {
    #[error("derivative event is not supported by the authority contract")]
    UnsupportedEvent,
    #[error("derivative event failed canonical verification")]
    InvalidEvent,
    #[error("derivative event, connector, instrument, or catalog authority does not match")]
    AuthorityMismatch,
    #[error("catalog authority is invalid")]
    InvalidCatalogAuthority,
}
