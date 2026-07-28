use crate::{DomainError, ensure_generation, id::normalize_lower};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::fmt;

const MAX_SOURCE_LENGTH: usize = 96;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SourceKind {
    Exchange = 0,
    Chain = 1,
    Macro = 2,
    News = 3,
    Derived = 4,
    System = 5,
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
