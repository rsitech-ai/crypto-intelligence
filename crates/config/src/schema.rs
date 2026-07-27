//! Deterministic JSON Schema generation for the public configuration contract.

use crate::{ConfigError, RawAppConfig};

pub fn generate() -> Result<String, ConfigError> {
    let schema = schemars::schema_for!(RawAppConfig);
    let mut rendered = serde_json::to_string_pretty(&schema)?;
    rendered.push('\n');
    Ok(rendered)
}
