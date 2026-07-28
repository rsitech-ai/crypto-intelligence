use crate::{DomainError, ensure_generation};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use std::fmt;

const MAX_VENUE_LENGTH: usize = 64;
const MAX_CHAIN_LENGTH: usize = 64;
const MAX_CONTRACT_LENGTH: usize = 160;
const MAX_ASSET_SYMBOL_LENGTH: usize = 32;

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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum AssetNamespace {
    Native = 0,
    Evm = 1,
    Solana = 2,
    Fiat = 3,
    Synthetic = 4,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct AssetId {
    namespace: AssetNamespace,
    chain_id: String,
    contract_or_mint: String,
    canonical_symbol: String,
    generation: u32,
}

impl AssetId {
    pub fn new(
        namespace: AssetNamespace,
        chain_id: impl AsRef<str>,
        contract_or_mint: impl AsRef<str>,
        canonical_symbol: impl AsRef<str>,
        generation: u32,
    ) -> Result<Self, DomainError> {
        ensure_generation(generation)?;
        let chain_id = chain_id.as_ref();
        let contract_or_mint = contract_or_mint.as_ref();
        match namespace {
            AssetNamespace::Native if chain_id.is_empty() || !contract_or_mint.is_empty() => {
                return Err(DomainError::InvalidIdentity {
                    field: "native asset chain/contract",
                });
            }
            AssetNamespace::Evm | AssetNamespace::Solana
                if chain_id.is_empty() || contract_or_mint.is_empty() =>
            {
                return Err(DomainError::InvalidIdentity {
                    field: "token asset chain/contract",
                });
            }
            AssetNamespace::Fiat | AssetNamespace::Synthetic
                if !chain_id.is_empty() || !contract_or_mint.is_empty() =>
            {
                return Err(DomainError::InvalidIdentity {
                    field: "off-chain asset chain/contract",
                });
            }
            _ => {}
        }

        let chain_id = if chain_id.is_empty() {
            String::new()
        } else {
            validate_component(chain_id, "chain id", MAX_CHAIN_LENGTH)?.to_owned()
        };
        let contract_or_mint = match namespace {
            AssetNamespace::Evm => {
                normalize_lower(contract_or_mint, "contract", MAX_CONTRACT_LENGTH)?
            }
            AssetNamespace::Solana => {
                validate_component(contract_or_mint, "mint", MAX_CONTRACT_LENGTH)?.to_owned()
            }
            AssetNamespace::Native | AssetNamespace::Fiat | AssetNamespace::Synthetic => {
                String::new()
            }
        };
        let canonical_symbol = normalize_upper(
            canonical_symbol.as_ref(),
            "canonical symbol",
            MAX_ASSET_SYMBOL_LENGTH,
        )?;

        Ok(Self {
            namespace,
            chain_id,
            contract_or_mint,
            canonical_symbol,
            generation,
        })
    }

    pub const fn namespace(&self) -> AssetNamespace {
        self.namespace
    }

    pub fn chain_id(&self) -> &str {
        &self.chain_id
    }

    pub fn contract_or_mint(&self) -> &str {
        &self.contract_or_mint
    }

    pub fn canonical_symbol(&self) -> &str {
        &self.canonical_symbol
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssetIdWire {
    namespace: AssetNamespace,
    chain_id: String,
    contract_or_mint: String,
    canonical_symbol: String,
    generation: u32,
}

impl<'de> Deserialize<'de> for AssetId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = AssetIdWire::deserialize(deserializer)?;
        Self::new(
            wire.namespace,
            wire.chain_id,
            wire.contract_or_mint,
            wire.canonical_symbol,
            wire.generation,
        )
        .map_err(D::Error::custom)
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?}:{}:{}:{}:{}",
            self.namespace,
            self.chain_id,
            self.contract_or_mint,
            self.canonical_symbol,
            self.generation
        )
    }
}

pub(crate) fn normalize_lower(
    value: &str,
    field: &'static str,
    maximum_length: usize,
) -> Result<String, DomainError> {
    normalize(value, field, maximum_length, char::to_ascii_lowercase)
}

pub(crate) fn normalize_upper(
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
    Ok(validate_component(value, field, maximum_length)?
        .chars()
        .map(|character| case(&character))
        .collect())
}

fn validate_component<'a>(
    value: &'a str,
    field: &'static str,
    maximum_length: usize,
) -> Result<&'a str, DomainError> {
    if value.is_empty()
        || value.len() > maximum_length
        || !value.is_ascii()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(DomainError::InvalidIdentity { field });
    }
    Ok(value)
}
