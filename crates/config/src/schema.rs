//! Deterministic JSON Schema generation for the public configuration contract.

use crate::{AppConfig, ConfigError};

pub fn generate() -> Result<String, ConfigError> {
    let schema = schemars::schema_for!(AppConfig);
    let mut rendered = serde_json::to_string_pretty(&schema)?;
    rendered.push('\n');
    Ok(rendered)
}
