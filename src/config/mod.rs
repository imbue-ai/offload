//! Configuration loading and schema definitions.

pub mod schema;

pub use schema::*;

use std::path::Path;

use anyhow::{Context, Result};

/// Load configuration from a TOML file.
pub fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    Ok(config)
}

/// Load configuration from a string.
pub fn load_config_str(content: &str) -> Result<Config> {
    let config: Config = toml::from_str(content)
        .context("Failed to parse config")?;

    Ok(config)
}
