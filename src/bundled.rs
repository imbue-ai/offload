//! Bundled scripts for common provider integrations.
//!
//! This module embeds scripts (like `modal_sandbox.py`) directly into the
//! binary and extracts them on demand to a cache directory. Users can
//! reference these scripts in their configuration using `@filename.ext` syntax.
//!
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use include_dir::{Dir, include_dir};
use regex::Regex;

/// Embedded scripts directory.
static SCRIPTS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/scripts");

/// Lazily initialized cache of extracted scripts.
static SCRIPTS_CACHE: OnceLock<Result<PathBuf, BundledError>> = OnceLock::new();

/// Lazily compiled regex for `@filename.ext` patterns.
static SCRIPT_PATTERN: OnceLock<Result<Regex, BundledError>> = OnceLock::new();

/// Result type for bundled script operations.
pub type BundledResult<T> = Result<T, BundledError>;

/// Errors that can occur during bundled script operations.
#[derive(Debug, thiserror::Error)]
pub enum BundledError {
    /// Failed to create cache directory.
    #[error("Offload failed to create cache directory: {0}")]
    CacheCreationFailed(std::io::Error),

    /// Failed to extract a bundled script.
    #[error("Failed to extract script '{name}' for Offload: {source}")]
    ExtractionFailed {
        name: String,
        #[source]
        source: std::io::Error,
    },

    /// Regex pattern compilation failed.
    #[error("Failed to compile regex pattern in Offload: {0}")]
    RegexCompilationFailed(regex::Error),

    /// Referenced script is not bundled.
    #[error("Script not found in bundled scripts in Offload: {0}")]
    ScriptNotFound(String),
}

/// Returns the cache directory for extracted scripts.
///
/// Uses platform-appropriate cache locations:
/// - macOS: `~/Library/Caches/offload/scripts`
/// - Linux: `$XDG_CACHE_HOME/offload/scripts` or `~/.cache/offload/scripts`
/// - Windows: `%LOCALAPPDATA%/offload/scripts`
/// - Fallback: `/tmp/offload/scripts`
fn get_cache_dir() -> BundledResult<PathBuf> {
    let base_cache = if cfg!(target_os = "macos") {
        env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join("Library/Caches"))
    } else if cfg!(target_os = "windows") {
        env::var("LOCALAPPDATA").ok().map(PathBuf::from)
    } else {
        // Linux/Unix: XDG_CACHE_HOME or ~/.cache
        env::var("XDG_CACHE_HOME")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".cache"))
            })
    };

    let cache_dir = base_cache
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("offload")
        .join("scripts");

    fs::create_dir_all(&cache_dir).map_err(BundledError::CacheCreationFailed)?;

    Ok(cache_dir)
}

/// Extracts all bundled scripts to the cache directory (once).
///
/// This is called lazily on first use and cached thereafter.
fn ensure_scripts_extracted() -> BundledResult<PathBuf> {
    let result = SCRIPTS_CACHE.get_or_init(|| {
        let cache_dir = get_cache_dir()?;

        for file in SCRIPTS_DIR.files() {
            let target_path = cache_dir.join(file.path());

            // Skip if file already exists with same content
            if target_path.exists()
                && let Ok(existing) = fs::read(&target_path)
                && existing == file.contents()
            {
                continue;
            }

            // Create parent directories if needed
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent).map_err(BundledError::CacheCreationFailed)?;
            }

            fs::write(&target_path, file.contents()).map_err(|e| {
                BundledError::ExtractionFailed {
                    name: file.path().display().to_string(),
                    source: e,
                }
            })?;

            // Make executable on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = fs::metadata(&target_path)
                    .map_err(|e| BundledError::ExtractionFailed {
                        name: file.path().display().to_string(),
                        source: e,
                    })?
                    .permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&target_path, perms).map_err(|e| {
                    BundledError::ExtractionFailed {
                        name: file.path().display().to_string(),
                        source: e,
                    }
                })?;
            }
        }

        Ok(cache_dir)
    });

    // Clone the result since we can't move out of the OnceLock
    match result {
        Ok(path) => Ok(path.clone()),
        Err(e) => Err(BundledError::ExtractionFailed {
            name: "cache initialization".to_string(),
            source: std::io::Error::other(e.to_string()),
        }),
    }
}

/// Returns the compiled regex for script patterns.
fn get_script_pattern() -> BundledResult<&'static Regex> {
    let result = SCRIPT_PATTERN.get_or_init(|| {
        // (?:^|\s) - match start of string or whitespace (non-capturing)
        // @([\w\-]+\.\w+) - match @filename.ext, capturing filename.ext
        Regex::new(r"(?:^|\s)@([\w\-]+\.\w+)").map_err(BundledError::RegexCompilationFailed)
    });

    match result {
        Ok(regex) => Ok(regex),
        Err(e) => Err(BundledError::RegexCompilationFailed(regex::Error::Syntax(
            e.to_string(),
        ))),
    }
}

/// Expands `@filename.ext` references in a command string to full cache paths.
///
/// # Arguments
///
/// * `command` - Command string potentially containing `@script.ext` references
///
/// # Returns
///
/// The command string with all `@script.ext` references replaced with their
/// full paths in the cache directory.
///
/// # Errors
///
/// Returns an error if:
/// - Script extraction fails
/// - A referenced script is not bundled
pub fn expand_command(command: &str) -> BundledResult<String> {
    let pattern = get_script_pattern()?;

    // Check if there are any matches first
    if !pattern.is_match(command) {
        return Ok(command.to_string());
    }

    // Extract scripts (lazy, only on first use)
    let cache_dir = ensure_scripts_extracted()?;

    // Collect all matches and validate they exist before replacing
    let mut result = command.to_string();
    for cap in pattern.captures_iter(command) {
        let script_name = &cap[1];
        let script_path = cache_dir.join(script_name);

        // Verify the script exists in our bundled set
        if SCRIPTS_DIR.get_file(script_name).is_none() {
            return Err(BundledError::ScriptNotFound(script_name.to_string()));
        }

        // Replace only the @script.ext part, not the preceding whitespace
        // cap[0] includes the whitespace prefix, so we replace @script_name directly
        let at_script = format!("@{}", script_name);
        result = result.replace(&at_script, &script_path.display().to_string());
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_command_no_refs() -> Result<(), Box<dyn std::error::Error>> {
        let cmd = "echo hello world";
        let expanded = expand_command(cmd)?;
        assert_eq!(expanded, cmd);
        Ok(())
    }

    #[test]
    fn test_expand_command_with_ref() -> Result<(), Box<dyn std::error::Error>> {
        let cmd = "uv run @modal_sandbox.py create {image_id}";
        let expanded = expand_command(cmd)?;

        assert!(!expanded.contains('@'));
        assert!(expanded.contains("modal_sandbox.py"));
        assert!(expanded.contains("offload/scripts"));
        Ok(())
    }

    #[test]
    fn test_expand_command_multiple_refs() -> Result<(), Box<dyn std::error::Error>> {
        let cmd = "uv run @modal_sandbox.py @modal_sandbox.py";
        let expanded = expand_command(cmd)?;

        // Both refs should be expanded
        assert!(!expanded.contains('@'));
        // Should contain the path twice
        let count = expanded.matches("modal_sandbox.py").count();
        assert_eq!(count, 2);
        Ok(())
    }

    #[test]
    fn test_expand_command_unknown_script() {
        let cmd = "run @nonexistent.py";
        let result = expand_command(cmd);

        assert!(result.is_err());
        match result {
            Err(BundledError::ScriptNotFound(name)) => {
                assert_eq!(name, "nonexistent.py");
            }
            _ => panic!("Expected ScriptNotFound error"),
        }
    }

    #[test]
    fn test_script_pattern_regex() -> Result<(), Box<dyn std::error::Error>> {
        let pattern = get_script_pattern()?;

        // Should match
        assert!(pattern.is_match("@script.py"));
        assert!(pattern.is_match("@modal_sandbox.py"));
        assert!(pattern.is_match("@my-script.sh"));
        assert!(pattern.is_match("run @my-script.sh")); // ws before

        // Should not match
        assert!(!pattern.is_match("script.py"));
        assert!(!pattern.is_match("@script")); // no extension
        assert!(!pattern.is_match("email@domain.com")); // no whitespace before @
        assert!(!pattern.is_match("/path/to/@tty1.service")); // @ in middle of path

        // Note: email@domain.com does match the pattern, but this is acceptable
        // because we validate against SCRIPTS_DIR in expand_command()
        Ok(())
    }
}
