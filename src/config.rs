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
    fn test_expand_env_value_no_variables() {
        let result = expand_env_value("hello world").unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_expand_env_value_escaped_dollar() {
        let result = expand_env_value("price is $$100").unwrap();
        assert_eq!(result, "price is $100");
    }

    #[test]
    fn test_expand_env_value_multiple_escaped_dollars() {
        let result = expand_env_value("$$$$").unwrap();
        assert_eq!(result, "$$");
    }

    #[test]
    fn test_expand_env_value_literal_dollar_no_brace() {
        let result = expand_env_value("$x and $y").unwrap();
        assert_eq!(result, "$x and $y");
    }

    #[test]
    fn test_expand_env_value_required_var_set() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::set_var("TEST_EXPAND_VAR", "test_value");
        }
        let result = expand_env_value("prefix_${TEST_EXPAND_VAR}_suffix").unwrap();
        assert_eq!(result, "prefix_test_value_suffix");
        // SAFETY: Cleanup after test.
        unsafe {
            std::env::remove_var("TEST_EXPAND_VAR");
        }
    }

    #[test]
    fn test_expand_env_value_required_var_not_set() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::remove_var("TEST_NONEXISTENT_VAR");
        }
        let result = expand_env_value("${TEST_NONEXISTENT_VAR}");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Required environment variable not set: TEST_NONEXISTENT_VAR")
        );
    }

    #[test]
    fn test_expand_env_value_default_when_var_not_set() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::remove_var("TEST_MISSING_VAR");
        }
        let result = expand_env_value("${TEST_MISSING_VAR:-default_value}").unwrap();
        assert_eq!(result, "default_value");
    }

    #[test]
    fn test_expand_env_value_default_not_used_when_var_set() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::set_var("TEST_SET_VAR", "actual_value");
        }
        let result = expand_env_value("${TEST_SET_VAR:-default_value}").unwrap();
        assert_eq!(result, "actual_value");
        // SAFETY: Cleanup after test.
        unsafe {
            std::env::remove_var("TEST_SET_VAR");
        }
    }

    #[test]
    fn test_expand_env_value_empty_default() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::remove_var("TEST_EMPTY_DEFAULT");
        }
        let result = expand_env_value("${TEST_EMPTY_DEFAULT:-}").unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_expand_env_value_empty_var_name() {
        let result = expand_env_value("${}");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Empty variable name"));
    }

    #[test]
    fn test_expand_env_value_unclosed_brace() {
        let result = expand_env_value("${VAR");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unclosed variable reference"));
    }

    #[test]
    fn test_expand_env_value_multiple_variables() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::set_var("TEST_VAR1", "one");
            std::env::set_var("TEST_VAR2", "two");
        }
        let result = expand_env_value("${TEST_VAR1} and ${TEST_VAR2}").unwrap();
        assert_eq!(result, "one and two");
        // SAFETY: Cleanup after test.
        unsafe {
            std::env::remove_var("TEST_VAR1");
            std::env::remove_var("TEST_VAR2");
        }
    }

    #[test]
    fn test_expand_env_value_mixed_syntax() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::set_var("TEST_MIX", "value");
        }
        let result = expand_env_value("$$${TEST_MIX}$x").unwrap();
        assert_eq!(result, "$value$x");
        // SAFETY: Cleanup after test.
        unsafe {
            std::env::remove_var("TEST_MIX");
        }
    }

    #[test]
    fn test_load_config_expands_local_provider_env() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::set_var("TEST_CONFIG_VAR", "expanded_value");
        }
        let config = load_config_str(
            r#"
            [offload]
            max_parallel = 4

            [provider]
            type = "local"

            [provider.env]
            MY_VAR = "${TEST_CONFIG_VAR}"
            LITERAL = "no expansion"

            [groups.all]
            type = "pytest"
        "#,
        )
        .unwrap();

        if let ProviderConfig::Local(local_config) = &config.provider {
            assert_eq!(
                local_config.env.get("MY_VAR"),
                Some(&"expanded_value".to_string())
            );
            assert_eq!(
                local_config.env.get("LITERAL"),
                Some(&"no expansion".to_string())
            );
        } else {
            panic!("Expected Local provider");
        }
        // SAFETY: Cleanup after test.
        unsafe {
            std::env::remove_var("TEST_CONFIG_VAR");
        }
    }

    #[test]
    fn test_load_config_fails_on_missing_required_var() {
        // SAFETY: This is a test running in isolation; env var manipulation is acceptable.
        unsafe {
            std::env::remove_var("DEFINITELY_NOT_SET_VAR");
        }
        let result = load_config_str(
            r#"
            [offload]
            max_parallel = 4

            [provider]
            type = "local"

            [provider.env]
            MY_VAR = "${DEFINITELY_NOT_SET_VAR}"

            [groups.all]
            type = "pytest"
        "#,
        );

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("DEFINITELY_NOT_SET_VAR"));
    }
}
