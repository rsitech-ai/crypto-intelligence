//! Canonical generation-aware identities shared by local runtime crates.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::fmt;
use thiserror::Error;

const MAX_VENUE_LENGTH: usize = 64;
const MAX_SOURCE_LENGTH: usize = 96;
const MAX_SYMBOL_LENGTH: usize = 96;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DomainError {
    #[error("invalid {field} identity field")]
    InvalidIdentity { field: &'static str },
    #[error("identity generation must be nonzero")]
    GenerationZero,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct VenueId(String);

impl VenueId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, DomainError> {
        Ok(Self(normalize_lower(
            value.as_ref(),
            "venue",
            MAX_VENUE_LENGTH,
        )?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for VenueId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl fmt::Display for VenueId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Exchange,
    Chain,
    Macro,
    News,
    Derived,
    System,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct SourceId {
    kind: SourceKind,
    name: String,
    generation: u32,
}

impl SourceId {
    pub fn new(
        kind: SourceKind,
        name: impl AsRef<str>,
        generation: u32,
    ) -> Result<Self, DomainError> {
        ensure_generation(generation)?;
        Ok(Self {
            kind,
            name: normalize_lower(name.as_ref(), "source", MAX_SOURCE_LENGTH)?,
            generation,
        })
    }

    pub const fn kind(&self) -> SourceKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceIdWire {
    kind: SourceKind,
    name: String,
    generation: u32,
}

impl<'de> Deserialize<'de> for SourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SourceIdWire::deserialize(deserializer)?;
        Self::new(wire.kind, wire.name, wire.generation).map_err(D::Error::custom)
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?}:{}:{}",
            self.kind, self.name, self.generation
        )
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct InstrumentId {
    venue: VenueId,
    venue_symbol: String,
    generation: u32,
}

impl InstrumentId {
    pub fn new(
        venue: VenueId,
        venue_symbol: impl AsRef<str>,
        generation: u32,
    ) -> Result<Self, DomainError> {
        ensure_generation(generation)?;
        Ok(Self {
            venue,
            venue_symbol: normalize_upper(
                venue_symbol.as_ref(),
                "venue symbol",
                MAX_SYMBOL_LENGTH,
            )?,
            generation,
        })
    }

    pub fn venue(&self) -> &VenueId {
        &self.venue
    }

    pub fn venue_symbol(&self) -> &str {
        &self.venue_symbol
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstrumentIdWire {
    venue: VenueId,
    venue_symbol: String,
    generation: u32,
}

impl<'de> Deserialize<'de> for InstrumentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InstrumentIdWire::deserialize(deserializer)?;
        Self::new(wire.venue, wire.venue_symbol, wire.generation).map_err(D::Error::custom)
    }
}

impl fmt::Display for InstrumentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}:{}",
            self.venue, self.venue_symbol, self.generation
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixNanos(i64);

impl UnixNanos {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

fn ensure_generation(generation: u32) -> Result<(), DomainError> {
    if generation == 0 {
        Err(DomainError::GenerationZero)
    } else {
        Ok(())
    }
}

fn normalize_lower(
    value: &str,
    field: &'static str,
    maximum_length: usize,
) -> Result<String, DomainError> {
    normalize(value, field, maximum_length, char::to_ascii_lowercase)
}

fn normalize_upper(
    value: &str,
    field: &'static str,
    maximum_length: usize,
) -> Result<String, DomainError> {
    normalize(value, field, maximum_length, char::to_ascii_uppercase)
}

fn normalize(
    value: &str,
    field: &'static str,
    maximum_length: usize,
    case: fn(&char) -> char,
) -> Result<String, DomainError> {
    if value.is_empty()
        || value.len() > maximum_length
        || !value.is_ascii()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(DomainError::InvalidIdentity { field });
    }

    Ok(value.chars().map(|character| case(&character)).collect())
}
