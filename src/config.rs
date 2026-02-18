//! Configuration loading and schema definitions for offload.
//!
//! This module provides types and functions for loading offload configuration
//! from TOML files or strings. The configuration schema defines all settings
//! for providers, test frameworks, and reporting.
//!
//! # The Configuration File Format is described in the README.

pub mod schema;

pub use schema::*;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

/// Loads offload configuration from a TOML file.
///
/// This is the primary way to load configuration. The file must be valid TOML
/// and conform to the offload configuration schema.
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
/// use offload::config::load_config;
/// use std::path::Path;
///
/// let config = load_config(Path::new("offload.toml"))?;
/// println!("Max parallel: {}", config.offload.max_parallel);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let mut config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    expand_provider_env(&mut config.provider)?;

    Ok(config)
}

/// Loads offload configuration from a TOML string.
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
/// use offload::config::load_config_str;
///
/// let config = load_config_str(r#"
///     [offload]
///     max_parallel = 4
///
///     [provider]
///     type = "local"
///
///     [groups.all]
///     type = "pytest"
/// "#)?;
///
/// assert_eq!(config.offload.max_parallel, 4);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn load_config_str(content: &str) -> Result<Config> {
    let mut config: Config = toml::from_str(content).context("Failed to parse config")?;

    expand_provider_env(&mut config.provider)?;

    Ok(config)
}

/// Expands environment variable references in a string value.
///
/// Syntax:
/// - `${VAR}` - required, fails if VAR is not set
/// - `${VAR:-default}` - optional, uses "default" if VAR not set
/// - `$$` - escaped dollar sign (becomes single `$`)
///
/// # Errors
/// Returns error if a required variable is not set.
fn expand_env_value(value: &str) -> Result<String, String> {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' {
            match chars.peek() {
                Some('$') => {
                    // Escaped dollar sign: $$ -> $
                    chars.next();
                    result.push('$');
                }
                Some('{') => {
                    // Variable reference: ${VAR} or ${VAR:-default}
                    chars.next(); // consume '{'

                    // Parse variable name and optional default
                    let mut var_content = String::new();
                    let mut found_close = false;

                    for c in chars.by_ref() {
                        if c == '}' {
                            found_close = true;
                            break;
                        }
                        var_content.push(c);
                    }

                    if !found_close {
                        return Err(format!("Unclosed variable reference: ${{{var_content}"));
                    }

                    // Check for default value syntax: VAR:-default
                    let (var_name, default_value) = if let Some(idx) = var_content.find(":-") {
                        let (name, rest) = var_content.split_at(idx);
                        (name, Some(&rest[2..])) // Skip ":-"
                    } else {
                        (var_content.as_str(), None)
                    };

                    if var_name.is_empty() {
                        return Err("Empty variable name in ${}".to_string());
                    }

                    // Look up the environment variable
                    match std::env::var(var_name) {
                        Ok(val) => result.push_str(&val),
                        Err(_) => {
                            if let Some(default) = default_value {
                                result.push_str(default);
                            } else {
                                return Err(format!(
                                    "Required environment variable not set: {var_name}"
                                ));
                            }
                        }
                    }
                }
                _ => {
                    // Lone $ without { or $, treat as literal
                    result.push('$');
                }
            }
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

/// Expands environment variables in all values of a HashMap.
fn expand_env_hashmap(env: &mut HashMap<String, String>) -> Result<()> {
    for (key, value) in env.iter_mut() {
        *value = expand_env_value(value)
            .map_err(|e| anyhow::anyhow!("Failed to expand env var '{key}': {e}"))?;
    }
    Ok(())
}

/// Expands environment variables in the provider's env HashMap.
fn expand_provider_env(provider: &mut ProviderConfig) -> Result<()> {
    match provider {
        ProviderConfig::Local(config) => expand_env_hashmap(&mut config.env),
        ProviderConfig::Modal(config) => expand_env_hashmap(&mut config.env),
        ProviderConfig::Default(config) => expand_env_hashmap(&mut config.env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_value_no_variables() -> Result<(), String> {
        let result = expand_env_value("hello world")?;
        assert_eq!(result, "hello world");
        Ok(())
    }

    #[test]
    fn test_expand_env_value_escaped_dollar() -> Result<(), String> {
        let result = expand_env_value("price is $$100")?;
        assert_eq!(result, "price is $100");
        Ok(())
    }

    #[test]
    fn test_expand_env_value_multiple_escaped_dollars() -> Result<(), String> {
        let result = expand_env_value("$$$$")?;
        assert_eq!(result, "$$");
        Ok(())
    }

    #[test]
    fn test_expand_env_value_literal_dollar_no_brace() -> Result<(), String> {
        let result = expand_env_value("$x and $y")?;
        assert_eq!(result, "$x and $y");
        Ok(())
    }

    #[test]
    fn test_expand_env_value_empty_var_name() {
        let result = expand_env_value("${}");
        assert!(
            matches!(&result, Err(e) if e.contains("Empty variable name")),
            "expected error about empty variable name, got: {result:?}"
        );
    }

    #[test]
    fn test_expand_env_value_unclosed_brace() {
        let result = expand_env_value("${VAR");
        assert!(
            matches!(&result, Err(e) if e.contains("Unclosed variable reference")),
            "expected error about unclosed brace, got: {result:?}"
        );
    }

    // Tests using predictable environment variables (HOME exists, _OFFLOAD_TEST_* do not)

    #[test]
    fn test_expand_env_value_var_set() -> Result<(), String> {
        // HOME is always set in any Unix environment
        let result = expand_env_value("${HOME}")?;
        assert!(!result.is_empty(), "HOME should expand to non-empty value");
        Ok(())
    }

    #[test]
    fn test_expand_env_value_var_unset() {
        // This variable is guaranteed not to exist
        let result = expand_env_value("${_OFFLOAD_TEST_NONEXISTENT_VAR}");
        assert!(result.is_err(), "Unset var should return error");
    }

    #[test]
    fn test_expand_env_value_default_not_used_when_set() -> Result<(), String> {
        // HOME is set, so fallback should not be used
        let result = expand_env_value("${HOME:-fallback}")?;
        assert_ne!(result, "fallback", "Should return HOME value, not fallback");
        assert!(!result.is_empty());
        Ok(())
    }

    #[test]
    fn test_expand_env_value_default_used_when_unset() -> Result<(), String> {
        // This variable does not exist, so default should be used
        let result = expand_env_value("${_OFFLOAD_TEST_MISSING:-fallback}")?;
        assert_eq!(result, "fallback");
        Ok(())
    }

    #[test]
    fn test_expand_env_value_just_escaped_dollar() -> Result<(), String> {
        let result = expand_env_value("$$")?;
        assert_eq!(result, "$");
        Ok(())
    }

    #[test]
    fn test_expand_env_value_mixed() -> Result<(), String> {
        // Test expansion with prefix and suffix around HOME
        let result = expand_env_value("prefix_${HOME}_suffix")?;
        assert!(result.starts_with("prefix_"), "Should start with prefix_");
        assert!(result.ends_with("_suffix"), "Should end with _suffix");
        assert!(
            result.len() > "prefix__suffix".len(),
            "Should contain HOME value"
        );
        Ok(())
    }

    #[test]
    fn test_expand_env_value_empty_default() -> Result<(), String> {
        // Empty default: ${VAR:-} returns empty string if unset
        let result = expand_env_value("${_OFFLOAD_TEST_MISSING:-}")?;
        assert_eq!(result, "");
        Ok(())
    }
}
