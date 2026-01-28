//! Configuration loading and schema definitions for shotgun.
//!
//! This module provides types and functions for loading shotgun configuration
//! from TOML files or strings. The configuration schema defines all settings
//! for providers, test frameworks, and reporting.
//!
//! # The Configuration File Format is described in the README.

pub mod schema;

pub use schema::*;

use std::path::Path;

use anyhow::{Context, Result};

/// Loads shotgun configuration from a TOML file.
///
/// This is the primary way to load configuration. The file must be valid TOML
/// and conform to the shotgun configuration schema.
///
/// # Arguments
///
/// * `path` - Path to the TOML configuration file
///
/// # Errors
///
/// Returns an error if:
/// - The file cannot be read (e.g., doesn't exist or permission denied)
/// - The file contains invalid TOML syntax
/// - The configuration doesn't match the expected schema
///
/// # Example
///
/// ```no_run
/// use shotgun::config::load_config;
/// use std::path::Path;
///
/// let config = load_config(Path::new("shotgun.toml"))?;
/// println!("Max parallel: {}", config.shotgun.max_parallel);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    Ok(config)
}

/// Loads shotgun configuration from a TOML string.
///
/// Useful for testing, embedding configuration, or generating configuration
/// programmatically.
///
/// # Arguments
///
/// * `content` - A string containing valid TOML configuration
///
/// # Errors
///
/// Returns an error if:
/// - The string contains invalid TOML syntax
/// - The configuration doesn't match the expected schema
///
/// # Example
///
/// ```
/// use shotgun::config::load_config_str;
///
/// let config = load_config_str(r#"
///     [shotgun]
///     max_parallel = 4
///
///     [provider]
///     type = "local"
///
///     [groups.all.framework]
///     type = "pytest"
/// "#)?;
///
/// assert_eq!(config.shotgun.max_parallel, 4);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn load_config_str(content: &str) -> Result<Config> {
    let config: Config = toml::from_str(content).context("Failed to parse config")?;

    Ok(config)
}
