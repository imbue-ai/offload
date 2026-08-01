//! Pytest framework implementation using `pytest --collect-only` for discovery.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::pytest_filter::{FilterComponent, emit_hoisted_args, hoist_common_components};
use super::pytest_single_pass::{
    GroupSpec, PartitionConfig, PartitionReport, Routing, effective_components,
    group_spec_from_components, records_from_report, route_group,
};
use super::{
    FrameworkError, FrameworkResult, TestFramework, TestInstance, TestRecord,
    discovery_error_detail,
};
use crate::config::{GroupConfig, PytestFrameworkConfig};
use crate::provider::Command;
use crate::report::junit::TestsuiteXml;

/// Test framework for Python pytest projects.
///
/// Uses `pytest --collect-only -q` for test discovery and generates
/// commands with JUnit XML output for structured result parsing.
///
/// # Configuration
///
/// See [`PytestFrameworkConfig`] for available options including:
/// - `paths`: Directories to search
/// - `command`: Full pytest invocation command
/// - `run_args`: Extra arguments for execution only
/// - `discovery_args`: Extra arguments for discovery only
pub struct PytestFramework {
    config: PytestFrameworkConfig,
    /// The program to invoke (first token of `command`).
    program: String,
    /// Additional arguments parsed from `command` (tokens after the program).
    prefix_args: Vec<String>,
}

impl PytestFramework {
    /// Creates a new pytest framework, validating the command at construction time.
    pub fn new(config: PytestFrameworkConfig) -> FrameworkResult<Self> {
        let mut parts = shell_words::split(&config.command).map_err(|e| {
            FrameworkError::DiscoveryFailed(format!(
                "Failed to parse command '{}': {}",
                config.command, e
            ))
        })?;

        if parts.is_empty() {
            return Err(FrameworkError::DiscoveryFailed(format!(
                "Command '{}' produced no tokens after parsing",
                config.command
            )));
        }

        let program = parts.remove(0);
        let prefix_args = parts;

        Ok(Self {
            config,
            program,
            prefix_args,
        })
    }

    /// Tokenize the configured `discovery_args`, returning an empty vec when none are set.
    ///
    /// Shared by both discovery paths so they parse and report `discovery_args`
    /// errors identically.
    fn discovery_args_tokens(&self) -> FrameworkResult<Vec<String>> {
        match &self.config.discovery_args {
            Some(discovery_args) => shell_words::split(discovery_args).map_err(|e| {
                FrameworkError::DiscoveryFailed(format!(
                    "Invalid discovery_args '{}': {}",
                    discovery_args, e
                ))
            }),
            None => Ok(Vec::new()),
        }
    }

    /// Build the discovery-command tail appended after the shared collect-only
    /// base: `discovery_args` tokens, then the group filters, then search paths.
    ///
    /// The shared base is supplied by [`collect_only_command`](Self::collect_only_command);
    /// both the executed command and its display string derive their tail here
    /// so the two can never drift.
    fn discovery_extra_args(
        &self,
        search_paths: &[String],
        filters: &str,
    ) -> FrameworkResult<Vec<String>> {
        let mut args = self.discovery_args_tokens()?;

        if !filters.is_empty() {
            let tokens = shell_words::split(filters).map_err(|e| {
                FrameworkError::DiscoveryFailed(format!(
                    "Invalid filter string '{}': {}",
                    filters, e
                ))
            })?;
            args.extend(tokens);
        }

        args.extend(search_paths.iter().cloned());
        Ok(args)
    }

    /// Parse `pytest --collect-only -q` output to extract test records.
    fn parse_collect_output(&self, output: &str, group: &str) -> Vec<TestRecord> {
        let mut tests = Vec::new();

        for line in output.lines() {
            let trimmed = line.trim();
            // Simple format: tests/test_foo.py::test_bar
            if trimmed.contains("::") && !trimmed.starts_with('<') && !trimmed.contains(' ') {
                tests.push(TestRecord::new(trimmed, group));
            }
        }

        tests
    }

    /// Resolve the discovery search paths, preferring caller paths over config.
    fn discovery_search_paths(&self, paths: &[PathBuf]) -> Vec<String> {
        if paths.is_empty() {
            self.config
                .paths
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        } else {
            paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect()
        }
    }

    /// Seed the `pytest --collect-only -q` command shared by both discovery paths.
    ///
    /// Returns the program, its prefix args, the collect-only flags, and
    /// `PYTHONDONTWRITEBYTECODE=1` so neither the legacy per-group pass nor the
    /// single-pass pool writes `__pycache__` for a throwaway collection. Callers
    /// append their own filters, search paths, plugin selection, and extra env.
    fn collect_only_command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.program);
        for arg in &self.prefix_args {
            cmd.arg(arg);
        }
        cmd.arg("--collect-only").arg("-q");
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");
        cmd
    }

    /// Discover every group in one pytest collection pass, partitioned by the
    /// bundled `offload_partition` plugin.
    ///
    /// Groups whose filters use tokens the plugin cannot model (or that fail to
    /// parse) fall back to legacy per-group `discover`. Every remaining group
    /// forms the single-pass pool; if the pool collection fails for any reason
    /// the legacy path would have survived, the whole pool falls back per-group
    /// rather than failing the run.
    pub async fn discover_all_groups(
        &self,
        groups: &HashMap<String, GroupConfig>,
    ) -> FrameworkResult<Vec<TestRecord>> {
        let mut pool: Vec<(String, Vec<FilterComponent>)> = Vec::new();
        let mut records: Vec<TestRecord> = Vec::new();

        for (name, cfg) in groups {
            match route_group(&cfg.filters) {
                Ok(Routing::Pool(components)) => pool.push((name.clone(), components)),
                Ok(Routing::Fallback) => {
                    tracing::warn!(
                        "Could not collect group {} in one pass due to unsupported pytest collection args: {}. Falling back to individual group collection. Remove unsupported args to speed up collection.",
                        name,
                        cfg.filters
                    );
                    records.extend(self.discover_group_legacy(name, cfg).await?);
                }
                Err(_) => {
                    records.extend(self.discover_group_legacy(name, cfg).await?);
                }
            }
        }

        if pool.is_empty() {
            return Ok(records);
        }

        let pool_records = match self.collect_pool(&pool, groups).await {
            Ok(pool_records) => pool_records,
            Err(err) => {
                tracing::warn!(
                    "Single-pass pytest collection failed ({}); falling back to individual collection for {} pool group(s).",
                    err,
                    pool.len()
                );
                let mut legacy = Vec::new();
                for (name, _) in &pool {
                    if let Some(cfg) = groups.get(name) {
                        legacy.extend(self.discover_group_legacy(name, cfg).await?);
                    }
                }
                legacy
            }
        };

        records.extend(pool_records);
        Ok(records)
    }

    /// Discover one group through the legacy per-group path, tagging records.
    async fn discover_group_legacy(
        &self,
        name: &str,
        cfg: &GroupConfig,
    ) -> FrameworkResult<Vec<TestRecord>> {
        let tests = self.discover(&[], &cfg.filters, name).await?;
        Ok(tests
            .into_iter()
            .map(|t| {
                t.with_retry_count(cfg.retry_count)
                    .with_schedule_individual(cfg.schedule_individual)
            })
            .collect())
    }

    /// Run the single collection pass for the eligible pool and map the plugin
    /// report into tagged records.
    ///
    /// Any launch failure, missing/empty/malformed report, or serialization
    /// error surfaces as `Err`, which the caller treats as a signal to fall
    /// back to legacy per-group collection for the whole pool.
    async fn collect_pool(
        &self,
        pool: &[(String, Vec<FilterComponent>)],
        groups: &HashMap<String, GroupConfig>,
    ) -> FrameworkResult<Vec<TestRecord>> {
        // Hoist over each group's effective (last-wins) filter so a non-last
        // but common mark/keyword can never pre-narrow the shared collection.
        // The per-group spec below keeps the raw components: its own last-wins
        // collapse already yields the group's true effective filter.
        let all_components: Vec<Vec<FilterComponent>> = pool
            .iter()
            .map(|(_, components)| effective_components(components))
            .collect();
        let hoisted = hoist_common_components(&all_components).unwrap_or_default();
        let hoisted_args = emit_hoisted_args(&hoisted);

        let mut group_specs: BTreeMap<String, GroupSpec> = BTreeMap::new();
        for (name, components) in pool {
            group_specs.insert(name.clone(), group_spec_from_components(components));
        }

        // The temp files must outlive the collection command: the config is
        // read by the plugin and the out file is written by it.
        let out_file = tempfile::NamedTempFile::new().map_err(FrameworkError::Io)?;
        let out_path = out_file.path().to_path_buf();

        let partition_config = PartitionConfig {
            groups: group_specs,
            out: out_path.clone(),
        };
        let config_json = serde_json::to_string(&partition_config).map_err(|e| {
            FrameworkError::DiscoveryFailed(format!("failed to serialize partition config: {e}"))
        })?;

        let cfg_file = tempfile::NamedTempFile::new().map_err(FrameworkError::Io)?;
        let cfg_path = cfg_file.path().to_path_buf();
        std::fs::write(&cfg_path, &config_json).map_err(FrameworkError::Io)?;

        let scripts_dir = crate::bundled::scripts_dir().map_err(|e| {
            FrameworkError::DiscoveryFailed(format!("failed to locate bundled scripts: {e}"))
        })?;

        let search_paths = self.discovery_search_paths(&[]);
        let mut cmd =
            self.build_pool_command(&scripts_dir, &cfg_path, &hoisted_args, &search_paths)?;

        let output = cmd
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let report_str = match std::fs::read_to_string(&out_path) {
            Ok(contents) if !contents.trim().is_empty() => contents,
            _ => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(FrameworkError::DiscoveryFailed(format!(
                    "offload_partition produced no usable output ({}): {}",
                    output.status,
                    discovery_error_detail(&stderr, &stdout)
                )));
            }
        };
        drop(cfg_file);
        drop(out_file);

        let report: PartitionReport = serde_json::from_str(&report_str).map_err(|e| {
            FrameworkError::DiscoveryFailed(format!("partition output was not valid JSON: {e}"))
        })?;

        for (name, _) in pool {
            let count = report.groups.get(name).map(Vec::len).unwrap_or(0);
            if count == 0 {
                tracing::warn!("No tests discovered for group '{}'.", name);
            }
        }

        Ok(records_from_report(&report, groups))
    }

    /// Assemble the single-pass pool collection command from explicit inputs.
    ///
    /// Kept a pure function of its arguments so it is unit-testable without
    /// launching pytest; the caller owns the temp-file lifetime behind
    /// `cfg_path`.
    fn build_pool_command(
        &self,
        scripts_dir: &Path,
        cfg_path: &Path,
        hoisted_args: &[String],
        search_paths: &[String],
    ) -> FrameworkResult<tokio::process::Command> {
        let mut cmd = self.collect_only_command();
        // discovery_args precede the hoisted filters, matching the legacy order.
        for arg in self.discovery_args_tokens()? {
            cmd.arg(arg);
        }
        for arg in hoisted_args {
            cmd.arg(arg);
        }
        for path in search_paths {
            cmd.arg(path);
        }
        cmd.arg("-p").arg("offload_partition");
        super::env::apply_discovery_env(
            &mut cmd,
            &self.config.env,
            self.config.prepend_path.as_deref(),
        )?;
        // Set PYTHONPATH last so it wins over any value apply_discovery_env
        // took from the config `env`.
        let base = match self.config.env.get("PYTHONPATH") {
            Some(value) => {
                let cwd = std::env::current_dir().map_err(|e| {
                    FrameworkError::DiscoveryFailed(format!(
                        "Failed to get current directory: {}",
                        e
                    ))
                })?;
                Some(super::env::resolve_root_placeholder(
                    value,
                    cwd.to_string_lossy().as_ref(),
                ))
            }
            None => None,
        };
        let pythonpath = build_pythonpath(scripts_dir, base.as_deref())?;
        cmd.env("PYTHONPATH", &pythonpath);
        cmd.env("OFFLOAD_PARTITION_CONFIG", cfg_path);
        Ok(cmd)
    }
}

/// Build a `PYTHONPATH` with `scripts_dir` first, followed by `base` when given
/// (the config `env` PYTHONPATH) or the process `PYTHONPATH` otherwise.
///
/// `scripts_dir` stays first so the bundled `offload_partition` plugin imports.
fn build_pythonpath(scripts_dir: &Path, base: Option<&str>) -> FrameworkResult<std::ffi::OsString> {
    let mut entries: Vec<PathBuf> = vec![scripts_dir.to_path_buf()];
    match base {
        Some(base) => entries.extend(std::env::split_paths(base)),
        None => {
            if let Some(existing) = std::env::var_os("PYTHONPATH") {
                entries.extend(std::env::split_paths(&existing));
            }
        }
    }
    std::env::join_paths(entries)
        .map_err(|e| FrameworkError::DiscoveryFailed(format!("failed to build PYTHONPATH: {e}")))
}

#[async_trait]
impl TestFramework for PytestFramework {
    async fn discover(
        &self,
        paths: &[PathBuf],
        filters: &str,
        group: &str,
    ) -> FrameworkResult<Vec<TestRecord>> {
        // Add paths to search (caller-provided paths take precedence over config)
        let search_paths = self.discovery_search_paths(paths);

        // Reuse the shared collect-only base so legacy discovery matches the
        // single-pass pool.
        let extra_args = self.discovery_extra_args(&search_paths, filters)?;
        let mut cmd = self.collect_only_command();
        for arg in &extra_args {
            cmd.arg(arg);
        }

        super::env::apply_discovery_env(
            &mut cmd,
            &self.config.env,
            self.config.prepend_path.as_deref(),
        )?;

        // Build a display string that mirrors the executed command; its tail
        // comes from the same discovery_extra_args value.
        let mut cmd_parts: Vec<&str> = Vec::new();
        cmd_parts.push(&self.program);
        for arg in &self.prefix_args {
            cmd_parts.push(arg);
        }
        cmd_parts.push("--collect-only");
        cmd_parts.push("-q");
        for arg in &extra_args {
            cmd_parts.push(arg);
        }
        let cmd_display = cmd_parts.join(" ");

        let output = cmd
            .output()
            .await
            .map_err(|e| FrameworkError::DiscoveryFailed(e.to_string()))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() && !stdout.contains("::") {
            let detail = discovery_error_detail(&stderr, &stdout);
            return Err(FrameworkError::DiscoveryFailed(format!(
                "pytest --collect-only failed ({}):\n  command: {}\n  {}",
                output.status, cmd_display, detail
            )));
        }

        let tests = self.parse_collect_output(&stdout, group);

        if tests.is_empty() {
            tracing::warn!(
                "No tests discovered. Output: {}",
                discovery_error_detail(&stderr, &stdout)
            );
        }

        Ok(tests)
    }

    fn produce_test_execution_command(
        &self,
        tests: &[TestInstance],
        result_path: &str,
        fail_fast: bool,
    ) -> Command {
        let mut cmd = Command::new(&self.program);
        for arg in &self.prefix_args {
            cmd = cmd.arg(arg);
        }

        cmd = cmd
            .arg("-v")
            .arg("--tb=short")
            .arg(format!("--junitxml={}", result_path));

        if fail_fast {
            cmd = cmd.arg("-x");
        }

        // Append run_args for test execution only (not discovery)
        if let Some(run_args) = &self.config.run_args {
            match shell_words::split(run_args) {
                Ok(args) => {
                    for arg in args {
                        cmd = cmd.arg(arg);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to parse run_args '{}': {}", run_args, e);
                }
            }
        }

        // Add test IDs
        for test in tests {
            cmd = cmd.arg(test.id());
        }

        super::env::attach_execution_env(
            &mut cmd,
            &self.config.env,
            self.config.prepend_path.as_deref(),
        );

        cmd
    }

    fn resolve_test_ids(
        &self,
        testsuites: &mut [TestsuiteXml],
        batch_test_ids: &[String],
    ) -> FrameworkResult<()> {
        for testsuite in testsuites.iter_mut() {
            for testcase in &mut testsuite.testcases {
                match super::resolve_test_id_suffix_matching(
                    &testcase.name,
                    testcase.classname.as_deref(),
                    batch_test_ids,
                ) {
                    Ok(resolved) => {
                        testcase.name = resolved.to_string();
                        testcase.classname = None;
                    }
                    Err(msg) => {
                        return Err(FrameworkError::Other(anyhow::anyhow!(
                            "Failed to resolve JUnit testcase: {}",
                            msg
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::PytestFrameworkConfig;
    use crate::framework::TestInstance;

    #[test]
    fn test_command_prefix_with_command() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        assert_eq!(fw.program, "uv");
        assert_eq!(fw.prefix_args, vec!["run", "pytest"]);
        Ok(())
    }

    #[test]
    fn test_command_prefix_default() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        assert_eq!(fw.program, "python");
        assert_eq!(fw.prefix_args, vec!["-m", "pytest"]);
        Ok(())
    }

    #[test]
    fn test_new_rejects_invalid_command() {
        let config = PytestFrameworkConfig {
            command: "unclosed 'quote".to_string(),
            ..Default::default()
        };
        assert!(PytestFramework::new(config).is_err());
    }

    #[test]
    fn test_new_rejects_empty_command() {
        let config = PytestFrameworkConfig {
            command: "".to_string(),
            ..Default::default()
        };
        assert!(PytestFramework::new(config).is_err());
    }

    #[test]
    fn test_collect_only_command_base_and_bytecode_env() -> Result<(), Box<dyn std::error::Error>> {
        use std::ffi::OsStr;

        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let cmd = fw.collect_only_command();
        let std_cmd = cmd.as_std();

        assert_eq!(std_cmd.get_program(), OsStr::new("uv"));
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["run", "pytest", "--collect-only", "-q"]);

        let sets_bytecode = std_cmd.get_envs().any(|(key, value)| {
            key == OsStr::new("PYTHONDONTWRITEBYTECODE") && value == Some(OsStr::new("1"))
        });
        assert!(sets_bytecode);
        Ok(())
    }

    #[test]
    fn test_build_pool_command_args_and_env() -> Result<(), Box<dyn std::error::Error>> {
        use std::ffi::OsStr;
        use std::path::Path;

        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            discovery_args: Some("--no-cov".to_string()),
            env: HashMap::from([
                ("PYTHONPATH".to_string(), "{root}/mysrc".to_string()),
                ("SOME_VAR".to_string(), "{root}/v".to_string()),
            ]),
            prepend_path: Some(vec![".venv/bin".to_string()]),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;

        let scripts_dir = Path::new("/fake/scripts");
        let cfg_path = Path::new("/fake/partition-config.json");
        let hoisted_args = vec!["-m".to_string(), "not slow".to_string()];
        let search_paths = vec!["tests".to_string()];

        let cmd = fw.build_pool_command(scripts_dir, cfg_path, &hoisted_args, &search_paths)?;
        let std_cmd = cmd.as_std();

        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "run",
                "pytest",
                "--collect-only",
                "-q",
                "--no-cov",
                "-m",
                "not slow",
                "tests",
                "-p",
                "offload_partition",
            ]
        );

        let cwd = std::env::current_dir()?;
        let cwd_str = cwd.to_string_lossy().into_owned();

        let expected_some_var = format!("{}/v", cwd_str);
        let has_some_var = std_cmd.get_envs().any(|(k, v)| {
            k == OsStr::new("SOME_VAR") && v == Some(OsStr::new(expected_some_var.as_str()))
        });
        assert!(has_some_var, "SOME_VAR should resolve {{root}} against cwd");

        let expected_venv = format!("{}/.venv/bin", cwd_str);
        let has_venv_on_path = std_cmd.get_envs().any(|(k, v)| {
            k == OsStr::new("PATH")
                && v.map(|val| val.to_string_lossy().contains(&expected_venv))
                    .unwrap_or(false)
        });
        assert!(has_venv_on_path, "PATH should contain the prepend_path dir");

        let scripts_prefix = scripts_dir.to_string_lossy().into_owned();
        let expected_mysrc = format!("{}/mysrc", cwd_str);
        let pythonpath_ok = std_cmd.get_envs().any(|(k, v)| {
            if k != OsStr::new("PYTHONPATH") {
                return false;
            }
            match v {
                Some(val) => {
                    let val = val.to_string_lossy();
                    val.starts_with(&scripts_prefix) && val.contains(&expected_mysrc)
                }
                None => false,
            }
        });
        assert!(
            pythonpath_ok,
            "PYTHONPATH should start with scripts_dir and honor the config PYTHONPATH"
        );

        let has_cfg = std_cmd.get_envs().any(|(k, v)| {
            k == OsStr::new("OFFLOAD_PARTITION_CONFIG") && v == Some(cfg_path.as_os_str())
        });
        assert!(has_cfg, "OFFLOAD_PARTITION_CONFIG should be the cfg_path");

        Ok(())
    }

    #[test]
    fn test_discovery_extra_args_without_discovery_args() -> Result<(), Box<dyn std::error::Error>>
    {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let paths = vec!["tests".to_string()];
        let args = fw.discovery_extra_args(&paths, "-m 'not slow'")?;
        assert_eq!(args, vec!["-m", "not slow", "tests"]);
        Ok(())
    }

    #[test]
    fn test_discovery_extra_args_with_discovery_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            discovery_args: Some("--no-cov".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let paths = vec!["tests".to_string()];
        let args = fw.discovery_extra_args(&paths, "-m 'not slow'")?;
        assert_eq!(args, vec!["--no-cov", "-m", "not slow", "tests"]);
        Ok(())
    }

    #[test]
    fn test_discovery_extra_args_with_discovery_args_no_filters()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            discovery_args: Some("--no-cov -p no:cacheprovider".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let paths = vec!["tests".to_string(), "examples".to_string()];
        let args = fw.discovery_extra_args(&paths, "")?;
        assert_eq!(
            args,
            vec!["--no-cov", "-p", "no:cacheprovider", "tests", "examples"]
        );
        Ok(())
    }

    #[test]
    fn test_discovery_extra_args_rejects_invalid_quoting() -> Result<(), Box<dyn std::error::Error>>
    {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            discovery_args: Some("--no-cov 'unclosed".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let paths = vec!["tests".to_string()];
        match fw.discovery_extra_args(&paths, "") {
            Err(e) => assert!(matches!(e, FrameworkError::DiscoveryFailed(_))),
            Ok(_) => return Err("expected DiscoveryFailed for unbalanced quoting".into()),
        }
        Ok(())
    }

    #[test]
    fn test_discovery_args_tokens_shared_by_both_paths() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            discovery_args: Some("--ignore examples/tests/sub".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        assert_eq!(
            fw.discovery_args_tokens()?,
            vec!["--ignore", "examples/tests/sub"]
        );
        Ok(())
    }

    #[test]
    fn test_discovery_args_tokens_empty_when_unset() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        assert!(fw.discovery_args_tokens()?.is_empty());
        Ok(())
    }

    #[test]
    fn test_execution_command_with_run_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "uv run pytest".to_string(),
            run_args: Some("--no-cov --timeout=30".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "test-group");
        let tests = vec![TestInstance::new(&record)];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);
        assert_eq!(cmd.program, "uv");
        assert!(cmd.args.contains(&"--no-cov".to_string()));
        assert!(cmd.args.contains(&"--timeout=30".to_string()));
        assert!(cmd.args.contains(&"tests/test_a.py::test_one".to_string()));
        Ok(())
    }

    #[test]
    fn test_execution_command_excludes_discovery_args() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            discovery_args: Some("--no-cov".to_string()),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];
        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);
        assert!(!cmd.args.contains(&"--no-cov".to_string()));
        assert!(!cmd.args.contains(&"--collect-only".to_string()));
        Ok(())
    }

    #[test]
    fn test_execution_command_fail_fast() -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];

        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", true);
        assert!(cmd.args.contains(&"-x".to_string()));

        let cmd_no = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);
        assert!(!cmd_no.args.contains(&"-x".to_string()));

        Ok(())
    }

    #[test]
    fn test_execution_command_attaches_env_and_prepend_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            env: HashMap::from([("VIRTUAL_ENV".to_string(), "{root}/.venv".to_string())]),
            prepend_path: Some(vec![".venv/bin".to_string()]),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];

        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);

        // `{root}` is left unresolved; providers resolve it at execution time.
        assert_eq!(
            cmd.env,
            vec![("VIRTUAL_ENV".to_string(), "{root}/.venv".to_string())]
        );
        assert_eq!(cmd.prepend_path, vec![".venv/bin".to_string()]);
        Ok(())
    }

    #[test]
    fn test_execution_command_default_env_and_prepend_path_empty()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = PytestFrameworkConfig {
            command: "python -m pytest".to_string(),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;
        let record = TestRecord::new("tests/test_a.py::test_one", "grp");
        let tests = vec![TestInstance::new(&record)];

        let cmd = fw.produce_test_execution_command(&tests, "/tmp/junit.xml", false);

        assert!(cmd.env.is_empty());
        assert!(cmd.prepend_path.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_discover_applies_env_and_prepend_path() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let script = dir.path().join("fake-pytest");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s::test_echoed\\n' \"$OFFLOAD_FAKE_ROOT\"\n",
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;

        let config = PytestFrameworkConfig {
            command: "fake-pytest".to_string(),
            env: HashMap::from([("OFFLOAD_FAKE_ROOT".to_string(), "{root}/marker".to_string())]),
            prepend_path: Some(vec![dir.path().to_string_lossy().into_owned()]),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;

        let tests = fw.discover(&[], "", "grp").await?;

        let cwd = std::env::current_dir()?;
        let expected = format!("{}/marker::test_echoed", cwd.display());
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].id, expected);
        Ok(())
    }

    #[tokio::test]
    async fn test_discover_sets_dont_write_bytecode() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir()?;
        let script = dir.path().join("fake-pytest");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf 'dontwrite_%s::test_bytecode\\n' \"$PYTHONDONTWRITEBYTECODE\"\n",
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;

        let config = PytestFrameworkConfig {
            command: "fake-pytest".to_string(),
            prepend_path: Some(vec![dir.path().to_string_lossy().into_owned()]),
            ..Default::default()
        };
        let fw = PytestFramework::new(config)?;

        let tests = fw.discover(&[], "", "grp").await?;

        // The shared collect-only base exports PYTHONDONTWRITEBYTECODE=1, so the
        // fake pytest echoes `1`. If discover stopped reusing that base the id
        // would lose the `1` and this assertion would fail.
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].id, "dontwrite_1::test_bytecode");
        Ok(())
    }
}
