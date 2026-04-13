//! Configuration loading and TOML schema definitions.

pub mod schema;

pub use schema::*;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Formats a test ID from JUnit XML attributes using the provided format string.
///
/// # Arguments
///
/// * `format` - Format string with placeholders like `{name}` and `{classname}`
/// * `name` - The testcase name attribute from JUnit XML
/// * `classname` - The testcase classname attribute from JUnit XML (optional)
pub fn format_test_id(format: &str, name: &str, classname: Option<&str>) -> String {
    let mut result = format.to_string();
    result = result.replace("{name}", name);
    result = result.replace("{classname}", classname.unwrap_or(""));
    result
}

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
pub fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let mut config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    expand_provider_env(&mut config.provider)?;
    resolve_checkpoint_defaults(&mut config);
    validate_config(&config)?;

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
pub fn load_config_str(content: &str) -> Result<Config> {
    let mut config: Config = toml::from_str(content).context("Failed to parse config")?;

    expand_provider_env(&mut config.provider)?;
    resolve_checkpoint_defaults(&mut config);
    validate_config(&config)?;

    Ok(config)
}

/// Normalizes a path lexically by stripping `.` components.
///
/// This does not touch the filesystem, so it works even if the file does not exist.
/// It does not resolve `..` — that would require knowing the real directory structure.
fn normalize_path(raw: &str) -> PathBuf {
    use std::path::Component;
    Path::new(raw)
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

/// Injects the provider's dockerfile into `build_inputs` when checkpoint is enabled.
///
/// The dockerfile is a build input by definition — changes to it invalidate the
/// image cache. Rather than requiring users to duplicate the path, we resolve it
/// from the provider config and prepend it automatically.
fn resolve_checkpoint_defaults(config: &mut Config) {
    let dockerfile = match &config.provider {
        ProviderConfig::Modal(cfg) => cfg.dockerfile.clone(),
        // Default provider embeds the dockerfile path in prepare_command;
        // there is no structured field to resolve from.
        ProviderConfig::Default(_) | ProviderConfig::Local(_) => None,
    };

    if let Some(ref mut checkpoint) = config.checkpoint
        && let Some(path) = dockerfile
        && !checkpoint.build_inputs.contains(&path)
    {
        checkpoint.build_inputs.insert(0, path);
    }
}

/// Validates configuration invariants that cannot be expressed in the schema.
///
/// # Errors
///
/// Returns an error if:
/// - No groups are defined
/// - Default framework's `discover_command` is missing the `{filters}` placeholder
fn validate_config(config: &Config) -> Result<()> {
    // Require at least one group
    if config.groups.is_empty() {
        anyhow::bail!(
            "Configuration must define at least one group. Add a [groups.NAME] section, e.g.:\n\
             [groups.all]\n\
             retry_count = 0\n\
             filters = \"\""
        );
    }

    if let FrameworkConfig::Default(ref cfg) = config.framework
        && !cfg.discover_command.contains("{filters}")
    {
        anyhow::bail!(
            "Default framework discover_command must contain '{{filters}}' placeholder for group filtering. \
             Got: '{}'. \
             Add '{{filters}}' to your discover_command, e.g., 'my-command {{filters}}'",
            cfg.discover_command
        );
    }

    if let Some(ref checkpoint) = config.checkpoint {
        if checkpoint.build_inputs.is_empty() {
            anyhow::bail!(
                "Checkpoint configuration requires at least one entry in build_inputs. \
                 Add file paths whose contents determine the image cache key, e.g.:\n\
                 [checkpoint]\n\
                 build_inputs = [\"Dockerfile\", \"requirements.txt\"]"
            );
        }
        if matches!(config.provider, ProviderConfig::Local(_)) {
            anyhow::bail!(
                "Checkpoint is not supported with the local provider. \
                 Use a remote provider (default or modal) for checkpoint support."
            );
        }

        // Detect duplicate entries by normalizing paths lexically.
        // This catches "./Dockerfile" vs "Dockerfile" without requiring files to exist.
        let mut seen = HashSet::new();
        for raw in &checkpoint.build_inputs {
            let normalized = normalize_path(raw);
            if !seen.insert(normalized.clone()) {
                anyhow::bail!(
                    "Duplicate entry in build_inputs: '{}' (normalizes to '{}'). \
                     build_inputs must be a set of unique file paths.",
                    raw,
                    normalized.display()
                );
            }
        }
    }

    Ok(())
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
        ProviderConfig::Default(config) => expand_env_hashmap(&mut config.env),
        ProviderConfig::Modal(config) => expand_env_hashmap(&mut config.env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_with_empty_groups_returns_error() {
        // Test with empty [groups] table (explicit but empty)
        let toml = r#"
            [offload]
            max_parallel = 4
            sandbox_project_root = "/app"

            [provider]
            type = "local"

            [framework]
            type = "pytest"

            [groups]
        "#;

        let result = load_config_str(toml);
        assert!(result.is_err(), "Expected error for empty groups");
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("at least one group"),
            "Error message should mention requiring at least one group, got: {err_msg}"
        );
    }

    #[test]
    fn test_config_missing_groups_section_returns_error() {
        // Test with missing groups section entirely (TOML parsing will fail)
        let toml = r#"
            [offload]
            max_parallel = 4
            sandbox_project_root = "/app"

            [provider]
            type = "local"

            [framework]
            type = "pytest"
        "#;

        let result = load_config_str(toml);
        assert!(result.is_err(), "Expected error for missing groups section");
        // The error comes from TOML parsing, wrapped by anyhow
        // Check the full error chain by formatting with alternate display
        let err = result.unwrap_err();
        let err_chain = format!("{err:#}");
        assert!(
            err_chain.contains("groups") || err_chain.contains("missing field"),
            "Error chain should mention groups, got: {err_chain}"
        );
    }

    #[test]
    fn test_default_framework_missing_filters_placeholder_returns_error() {
        let toml = r#"
            [offload]
            max_parallel = 4
            sandbox_project_root = "/app"

            [provider]
            type = "local"

            [framework]
            type = "default"
            discover_command = "echo test1 test2"
            run_command = "echo {tests}"
            test_id_format = "{name}"

            [groups.all]
            retry_count = 0
        "#;

        let result = load_config_str(toml);
        assert!(
            result.is_err(),
            "Expected error for missing {{filters}} placeholder"
        );
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("{filters}"),
            "Error message should mention {{filters}} placeholder, got: {err_msg}"
        );
    }

    #[test]
    fn test_default_framework_with_filters_placeholder_succeeds() {
        let toml = r#"
            [offload]
            max_parallel = 4
            sandbox_project_root = "/app"

            [provider]
            type = "local"

            [framework]
            type = "default"
            discover_command = "echo test1 test2 {filters}"
            run_command = "echo {tests}"
            test_id_format = "{name}"

            [groups.all]
            retry_count = 0
        "#;

        let result = load_config_str(toml);
        assert!(
            result.is_ok(),
            "Expected success with {{filters}} placeholder: {:?}",
            result.err()
        );
    }

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

    #[test]
    fn test_checkpoint_empty_build_inputs_returns_error() {
        let toml = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "modal"

            [framework]
            type = "nextest"

            [groups.all]
            retry_count = 0

            [checkpoint]
            build_inputs = []
        "#;

        let result = load_config_str(toml);
        assert!(result.is_err(), "Expected error for empty build_inputs");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("build_inputs"),
            "Error should mention build_inputs, got: {err_msg}"
        );
    }

    #[test]
    fn test_checkpoint_with_local_provider_returns_error() {
        let toml = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "local"

            [framework]
            type = "nextest"

            [groups.all]
            retry_count = 0

            [checkpoint]
            build_inputs = ["Dockerfile"]
        "#;

        let result = load_config_str(toml);
        assert!(
            result.is_err(),
            "Expected error for checkpoint with local provider"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("local provider"),
            "Error should mention local provider, got: {err_msg}"
        );
    }

    #[test]
    fn test_checkpoint_with_modal_provider_succeeds() {
        let toml = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "modal"

            [framework]
            type = "nextest"

            [groups.all]
            retry_count = 0

            [checkpoint]
            build_inputs = ["Dockerfile"]
        "#;

        let result = load_config_str(toml);
        assert!(
            result.is_ok(),
            "Expected success for checkpoint with modal provider: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_checkpoint_auto_includes_modal_dockerfile() -> Result<()> {
        let toml = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "modal"
            dockerfile = "infra/Dockerfile"

            [framework]
            type = "nextest"

            [groups.all]
            retry_count = 0

            [checkpoint]
            build_inputs = ["requirements.txt"]
        "#;

        let config = load_config_str(toml)?;
        let checkpoint = config.checkpoint.context("checkpoint should be set")?;
        assert_eq!(
            checkpoint.build_inputs,
            vec!["infra/Dockerfile", "requirements.txt"],
            "Dockerfile from modal provider should be prepended to build_inputs"
        );
        Ok(())
    }

    #[test]
    fn test_checkpoint_auto_include_skips_if_already_present() -> Result<()> {
        let toml = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "modal"
            dockerfile = "Dockerfile"

            [framework]
            type = "nextest"

            [groups.all]
            retry_count = 0

            [checkpoint]
            build_inputs = ["Dockerfile", "requirements.txt"]
        "#;

        let config = load_config_str(toml)?;
        let checkpoint = config.checkpoint.context("checkpoint should be set")?;
        assert_eq!(
            checkpoint.build_inputs,
            vec!["Dockerfile", "requirements.txt"],
            "Should not duplicate Dockerfile when already present"
        );
        Ok(())
    }

    #[test]
    fn test_checkpoint_modal_dockerfile_alone_suffices() -> Result<()> {
        let toml = r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "modal"
            dockerfile = "Dockerfile"

            [framework]
            type = "nextest"

            [groups.all]
            retry_count = 0

            [checkpoint]
            build_inputs = []
        "#;

        let config = load_config_str(toml)?;
        let checkpoint = config.checkpoint.context("checkpoint should be set")?;
        assert_eq!(
            checkpoint.build_inputs,
            vec!["Dockerfile"],
            "Dockerfile from modal provider should make empty build_inputs valid"
        );
        Ok(())
    }
}
