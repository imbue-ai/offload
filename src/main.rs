//! offload CLI - Flexible parallel test runner.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use tracing::{Level, info, warn};
use tracing_subscriber::FmtSubscriber;

use offload::config::{
    self, CargoFrameworkConfig, Config, DefaultFrameworkConfig, DefaultProviderConfig,
    FrameworkConfig, GroupConfig, LocalProviderConfig, OffloadConfig, ProviderConfig,
    PytestFrameworkConfig, ReportConfig, SandboxConfig, VitestFrameworkConfig,
};
use offload::framework::{
    TestFramework, TestRecord, cargo::CargoFramework, default::DefaultFramework,
    pytest::PytestFramework, vitest::VitestFramework,
};
use offload::orchestrator::{Orchestrator, SandboxPool};
use offload::provider::{
    SandboxProvider, default::DefaultProvider, local::LocalProvider, modal::ModalProvider,
};
use offload::{checkpoint, git, with_retry};

/// A directory copy directive: local path -> sandbox path
#[derive(Debug, Clone)]
pub struct CopyDir {
    pub local: PathBuf,
    pub remote: PathBuf,
}

#[derive(Parser)]
#[command(name = "offload")]
#[command(about = "Flexible parallel test runner", long_about = None)]
#[command(version)]
struct Cli {
    /// Configuration file path
    #[arg(short, long, default_value = "offload.toml")]
    config: PathBuf,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run tests
    Run {
        /// Override maximum parallel sandboxes
        #[arg(short, long)]
        parallel: Option<usize>,

        /// Only discover tests, don't run them
        #[arg(long)]
        collect_only: bool,

        /// Directories to copy to sandbox (format: /local/path:/sandbox/path)
        #[arg(long, value_name = "LOCAL:REMOTE")]
        copy_dir: Vec<String>,

        /// Environment variables to set in sandboxes (format: KEY=VALUE)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env_vars: Vec<String>,

        /// Skip cached image lookup during prepare (forces fresh build)
        #[arg(long)]
        no_cache: bool,

        /// Emit a Perfetto trace to {output_dir}/trace.json
        #[arg(long)]
        trace: bool,

        /// Show estimated sandbox cost after run.
        ///
        /// Note: This is calculated client-side using simple formulas and
        /// may not reflect actual billing, discounts, or pricing adjustments.
        #[arg(long)]
        show_estimated_cost: bool,

        /// Stop immediately when a test failure is detected
        #[arg(long)]
        fail_fast: bool,
    },

    /// Discover tests without running them
    Collect {
        /// Output format (text, json)
        #[arg(short, long, default_value = "text")]
        format: String,
    },

    /// Validate configuration file
    Validate,

    /// Initialize a new configuration file
    Init {
        /// Provider type (local, default)
        #[arg(short, long, default_value = "local")]
        provider: String,

        /// Test framework (pytest, nextest, vitest, default)
        #[arg(short, long, default_value = "pytest")]
        framework: String,
    },

    /// Show checkpoint cache status for the current HEAD.
    CheckpointStatus {
        #[arg(long, default_value = "origin")]
        remote: String,
    },

    /// View test run logs
    Logs {
        /// Show only failure logs
        #[arg(long)]
        failures: bool,

        /// Show only error logs
        #[arg(long)]
        errors: bool,

        /// Show only tests matching this exact ID (repeatable)
        #[arg(long)]
        test: Vec<String>,

        /// Show only tests whose ID matches this regex (substring match)
        #[arg(long)]
        test_regex: Option<String>,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set up logging
    let log_level = if cli.verbose {
        Level::INFO
    } else {
        Level::WARN
    };
    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    match cli.command {
        Commands::Run {
            parallel,
            collect_only,
            copy_dir,
            env_vars,
            no_cache,
            trace,
            show_estimated_cost,
            fail_fast,
        } => {
            run_tests(
                &cli.config,
                parallel,
                collect_only,
                copy_dir,
                env_vars,
                no_cache,
                cli.verbose,
                trace,
                show_estimated_cost,
                fail_fast,
            )
            .await
        }
        Commands::Collect { format } => collect_tests(&cli.config, &format).await,
        Commands::Validate => validate_config(&cli.config),
        Commands::Init {
            provider,
            framework,
        } => init_config(&provider, &framework),
        Commands::CheckpointStatus { remote } => {
            let config_path_str = cli.config.to_string_lossy().to_string();
            checkpoint_status_handler(&config_path_str, &remote).await
        }
        Commands::Logs {
            failures,
            errors,
            test,
            test_regex,
        } => show_logs(&cli.config, failures, errors, &test, test_regex.as_deref()),
    }
}

/// Helper to get framework type name for validation.
fn framework_type_name(framework: &FrameworkConfig) -> &'static str {
    match framework {
        FrameworkConfig::Pytest(_) => "pytest",
        FrameworkConfig::Cargo(_) => "nextest",
        FrameworkConfig::Default(_) => "default",
        FrameworkConfig::Vitest(_) => "vitest",
    }
}

/// Discover tests for every group, tagging each with its group config.
async fn discover_all_tests(
    framework: &FrameworkConfig,
    groups: &HashMap<String, GroupConfig>,
) -> Result<Vec<TestRecord>> {
    let mut all_tests: Vec<TestRecord> = Vec::new();

    for (group_name, group_cfg) in groups {
        let tests = match framework {
            FrameworkConfig::Pytest(cfg) => {
                PytestFramework::new(cfg.clone())?
                    .discover(&[], &group_cfg.filters, group_name)
                    .await?
            }
            FrameworkConfig::Cargo(cfg) => {
                CargoFramework::new(cfg.clone())
                    .discover(&[], &group_cfg.filters, group_name)
                    .await?
            }
            FrameworkConfig::Default(cfg) => {
                DefaultFramework::new(cfg.clone())
                    .discover(&[], &group_cfg.filters, group_name)
                    .await?
            }
            FrameworkConfig::Vitest(cfg) => {
                VitestFramework::new(cfg.clone())?
                    .discover(&[], &group_cfg.filters, group_name)
                    .await?
            }
        };

        // Tag tests with group retry count
        let group_tests: Vec<TestRecord> = tests
            .into_iter()
            .map(|t| {
                t.with_retry_count(group_cfg.retry_count)
                    .with_schedule_individual(group_cfg.schedule_individual)
            })
            .collect();

        all_tests.extend(group_tests);
    }

    Ok(all_tests)
}

/// Discover tests concurrently with provider preparation, signalling completion.
async fn discover_with_signal(
    framework: &FrameworkConfig,
    groups: &HashMap<String, GroupConfig>,
    discovery_done: &AtomicBool,
) -> Result<Vec<TestRecord>> {
    eprintln!("[discover] Discovering tests...");
    let result = discover_all_tests(framework, groups).await;
    if let Ok(ref tests) = result {
        eprintln!(
            "[discover] found {} tests across {} groups",
            tests.len(),
            groups.len()
        );
    }
    discovery_done.store(true, Ordering::Release);
    result
}

/// Dispatch test execution to the appropriate framework, using the given provider.
#[allow(clippy::too_many_arguments)]
async fn dispatch_framework<P: offload::provider::SandboxProvider>(
    config: &Config,
    all_tests: &[TestRecord],
    provider: P,
    copy_dirs: &[CopyDir],
    verbose: bool,
    tracer: &offload::trace::Tracer,
    show_estimated_cost: bool,
    fail_fast: bool,
) -> Result<i32> {
    match &config.framework {
        FrameworkConfig::Pytest(f_cfg) => {
            run_all_tests(
                config,
                all_tests,
                provider,
                PytestFramework::new(f_cfg.clone())?,
                copy_dirs,
                verbose,
                tracer,
                show_estimated_cost,
                fail_fast,
            )
            .await
        }
        FrameworkConfig::Cargo(f_cfg) => {
            run_all_tests(
                config,
                all_tests,
                provider,
                CargoFramework::new(f_cfg.clone()),
                copy_dirs,
                verbose,
                tracer,
                show_estimated_cost,
                fail_fast,
            )
            .await
        }
        FrameworkConfig::Default(f_cfg) => {
            if fail_fast {
                warn!(
                    "--fail-fast: the default framework does not pass a stop flag to the test runner. Batches will still be cancelled on failure, but tests within a running batch will not stop early."
                );
            }
            run_all_tests(
                config,
                all_tests,
                provider,
                DefaultFramework::new(f_cfg.clone()),
                copy_dirs,
                verbose,
                tracer,
                show_estimated_cost,
                fail_fast,
            )
            .await
        }
        FrameworkConfig::Vitest(f_cfg) => {
            run_all_tests(
                config,
                all_tests,
                provider,
                VitestFramework::new(f_cfg.clone())?,
                copy_dirs,
                verbose,
                tracer,
                show_estimated_cost,
                fail_fast,
            )
            .await
        }
    }
}

/// Build a thin-diff image on top of a checkpoint base image.
///
/// Calls the Python script directly with --from-checkpoint flags, bypassing
/// the provider's prepare() method. Returns the target image ID on success.
async fn build_thin_diff_image(
    base_image_id: &str,
    checkpoint_sha: &str,
    sandbox_project_root: &str,
    discovery_done: Option<&AtomicBool>,
) -> Result<String, offload::provider::ProviderError> {
    use futures::StreamExt;
    use offload::connector::Connector;
    use offload::provider::{OutputLine, ProviderError};

    let cmd = format!(
        "uv run @modal_sandbox.py prepare --from-checkpoint={} --checkpoint-sha={} --sandbox-project-root={}",
        base_image_id, checkpoint_sha, sandbox_project_root
    );
    let connector = offload::connector::ShellConnector::new();

    // Buffer output while discovery is in progress, then flush
    let mut buffer: Vec<String> = Vec::new();
    let emit = |msg: String, buf: &mut Vec<String>| {
        if discovery_done.is_some_and(|flag| !flag.load(Ordering::Acquire)) {
            buf.push(msg);
        } else {
            for buffered in buf.drain(..) {
                eprintln!("{}", buffered);
            }
            eprintln!("{}", msg);
        }
    };

    emit(
        "[prepare] Building thin diff image...".to_string(),
        &mut buffer,
    );

    let mut stream = connector.run_stream(&cmd).await?;
    let mut last_stdout_line = String::new();
    let mut exit_code = 0;

    while let Some(line) = stream.next().await {
        match line {
            OutputLine::Stdout(s) => {
                emit(format!("[prepare]   {}", s), &mut buffer);
                last_stdout_line = s;
            }
            OutputLine::Stderr(s) => {
                emit(format!("[prepare]   {}", s), &mut buffer);
            }
            OutputLine::ExitCode(code) => {
                exit_code = code;
            }
        }
    }

    // Flush any remaining buffered output
    for buffered in buffer.drain(..) {
        eprintln!("{}", buffered);
    }

    if exit_code != 0 {
        return Err(ProviderError::ExecFailed(format!(
            "thin diff prepare command failed with exit code {}",
            exit_code
        )));
    }

    let image_id = last_stdout_line.trim().to_string();

    if image_id.is_empty() {
        return Err(ProviderError::ExecFailed(
            "thin diff prepare command returned empty image_id".to_string(),
        ));
    }

    Ok(image_id)
}

/// Shared logic for Default and Modal provider arms: checkpoint caching pipeline,
/// concurrent discovery + prepare, and framework dispatch.
#[allow(clippy::too_many_arguments)]
async fn run_with_caching<P: SandboxProvider>(
    mut provider: P,
    config: &Config,
    cache_state: &Option<CacheState>,
    copy_dir_tuples: &[(PathBuf, PathBuf)],
    copy_dirs: &[CopyDir],
    no_cache: bool,
    verbose: bool,
    tracer: &offload::trace::Tracer,
    show_estimated_cost: bool,
    fail_fast: bool,
    config_path: &Path,
) -> Result<Option<i32>> {
    let discovery_done = AtomicBool::new(false);
    let mut parent_base_sha: Option<String> = None;

    // --no-cache with [checkpoint]: use checkpoint build procedure without cache.
    // This still exports a clean tree and builds via context_dir + thin diff,
    // but skips all note reading/writing.
    if let Some(checkpoint_cfg) = config.checkpoint.as_ref().filter(|_| no_cache) {
        // Resolve checkpoint SHA (walks git log + checks build_inputs, no notes)
        let checkpoint_sha = if git::repo_root().await.is_ok() {
            match checkpoint::find_checkpoint_sha(checkpoint_cfg, 100).await {
                Ok(sha) => sha,
                Err(e) => {
                    warn!("Checkpoint resolution failed (--no-cache): {}", e);
                    None
                }
            }
        } else {
            None
        };

        if let Some(checkpoint_sha) = checkpoint_sha {
            eprintln!(
                "[prepare] --no-cache with checkpoint: building from {} (no cache lookup)",
                &checkpoint_sha[..8.min(checkpoint_sha.len())]
            );

            // Export checkpoint tree
            let tree_dir =
                tempfile::tempdir().context("failed to create temp dir for checkpoint tree")?;
            git::export_tree(&checkpoint_sha, tree_dir.path())
                .await
                .with_context(|| format!("failed to export tree for {}", checkpoint_sha))?;

            // Build base image from checkpoint source (normal prepare with context_dir)
            eprintln!("[prepare] Building checkpoint base image (--no-cache)...");
            let base_image_id = {
                let _span = tracer.span(
                    "checkpoint_base_prepare",
                    "local",
                    offload::trace::PID_LOCAL,
                    offload::trace::TID_MAIN,
                );
                with_retry!(provider.prepare(
                    copy_dir_tuples,
                    no_cache,
                    config.offload.sandbox_init_cmd.as_deref(),
                    None,
                    Some(tree_dir.path()),
                ))
                .context("Failed to build checkpoint base image")?
            };

            // No note writing -- this is --no-cache

            // Build thin diff on top
            if let Some(base_id) = base_image_id {
                let (all_tests, thin_diff_result) = tokio::try_join!(
                    discover_with_signal(&config.framework, &config.groups, &discovery_done),
                    async {
                        let _span = tracer.span(
                            "thin_diff",
                            "local",
                            offload::trace::PID_LOCAL,
                            offload::trace::TID_MAIN,
                        );
                        Ok::<_, anyhow::Error>(
                            build_thin_diff_image(
                                &base_id,
                                &checkpoint_sha,
                                &config.offload.sandbox_project_root,
                                Some(&discovery_done),
                            )
                            .await,
                        )
                    }
                )?;

                match thin_diff_result {
                    Ok(target_id) => {
                        provider.set_image_id(target_id);
                        if all_tests.is_empty() {
                            return Ok(None);
                        }
                        return dispatch_framework(
                            config,
                            &all_tests,
                            provider,
                            copy_dirs,
                            verbose,
                            tracer,
                            show_estimated_cost,
                            fail_fast,
                        )
                        .await
                        .map(Some);
                    }
                    Err(e) => {
                        warn!(
                            "Thin diff failed after --no-cache base build, falling back to full build: {}",
                            e
                        );
                        eprintln!("[prepare] Thin diff failed, falling back to full build");
                        // Fall through to full build below
                    }
                }
            }
            // Fall through to full build if base build failed or thin diff failed
        }
        // If no checkpoint found, fall through to full build (same as today)
    }

    // Step 1: Handle cache states that bypass or modify prepare
    match cache_state {
        // Checkpoint cache hit: build thin diff on existing base
        Some(CacheState::Checkpoint {
            checkpoint_sha,
            cached_image: Some(cached),
        }) => {
            eprintln!(
                "[cache] Using checkpoint image from {}",
                &checkpoint_sha[..8.min(checkpoint_sha.len())]
            );

            // Try thin diff; on failure, fall through to full build
            let (all_tests, thin_diff_result) = tokio::try_join!(
                discover_with_signal(&config.framework, &config.groups, &discovery_done),
                async {
                    let _span = tracer.span(
                        "thin_diff",
                        "local",
                        offload::trace::PID_LOCAL,
                        offload::trace::TID_MAIN,
                    );
                    // Return the result as a value, not using ?
                    Ok::<_, anyhow::Error>(
                        build_thin_diff_image(
                            &cached.image_id,
                            checkpoint_sha,
                            &config.offload.sandbox_project_root,
                            Some(&discovery_done),
                        )
                        .await,
                    )
                }
            )?;

            match thin_diff_result {
                Ok(target_id) => {
                    provider.set_image_id(target_id);
                    if all_tests.is_empty() {
                        return Ok(None);
                    }
                    return dispatch_framework(
                        config,
                        &all_tests,
                        provider,
                        copy_dirs,
                        verbose,
                        tracer,
                        show_estimated_cost,
                        fail_fast,
                    )
                    .await
                    .map(Some);
                }
                Err(e) => {
                    warn!("Thin diff failed, falling back to full build: {}", e);
                    eprintln!("[cache] Thin diff failed, falling back to full build");
                    // Fall through to full build below
                }
            }
        }

        // Checkpoint cache miss: build base image, cache it, then thin diff
        Some(CacheState::Checkpoint {
            checkpoint_sha,
            cached_image: None,
        }) => {
            eprintln!(
                "[cache] No cached checkpoint image for {} — building base",
                &checkpoint_sha[..8.min(checkpoint_sha.len())]
            );

            // Export checkpoint tree
            let tree_dir =
                tempfile::tempdir().context("failed to create temp dir for checkpoint tree")?;
            git::export_tree(checkpoint_sha, tree_dir.path())
                .await
                .with_context(|| format!("failed to export tree for {}", checkpoint_sha))?;

            // Build base image from checkpoint source (normal prepare with context_dir)
            eprintln!("[prepare] Building checkpoint base image...");
            let base_image_id = {
                let _span = tracer.span(
                    "checkpoint_base_prepare",
                    "local",
                    offload::trace::PID_LOCAL,
                    offload::trace::TID_MAIN,
                );
                with_retry!(provider.prepare(
                    copy_dir_tuples,
                    no_cache,
                    config.offload.sandbox_init_cmd.as_deref(),
                    None,
                    Some(tree_dir.path()),
                ))
                .context("Failed to build checkpoint base image")?
            };

            if let Some(ref base_id) = base_image_id {
                write_note_for_commit(checkpoint_sha, base_id, config_path).await;
            }

            // Now build thin diff on top
            if let Some(base_id) = base_image_id {
                let (all_tests, thin_diff_result) = tokio::try_join!(
                    discover_with_signal(&config.framework, &config.groups, &discovery_done),
                    async {
                        let _span = tracer.span(
                            "thin_diff",
                            "local",
                            offload::trace::PID_LOCAL,
                            offload::trace::TID_MAIN,
                        );
                        Ok::<_, anyhow::Error>(
                            build_thin_diff_image(
                                &base_id,
                                checkpoint_sha,
                                &config.offload.sandbox_project_root,
                                Some(&discovery_done),
                            )
                            .await,
                        )
                    }
                )?;

                match thin_diff_result {
                    Ok(target_id) => {
                        provider.set_image_id(target_id);
                        if all_tests.is_empty() {
                            return Ok(None);
                        }
                        return dispatch_framework(
                            config,
                            &all_tests,
                            provider,
                            copy_dirs,
                            verbose,
                            tracer,
                            show_estimated_cost,
                            fail_fast,
                        )
                        .await
                        .map(Some);
                    }
                    Err(e) => {
                        warn!(
                            "Thin diff failed after base build, falling back to full build: {}",
                            e
                        );
                        eprintln!("[cache] Thin diff failed, falling back to full build");
                    }
                }
            }
            // Fall through to full build if base build failed or thin diff failed
        }

        // Parent-commit cache hit: parent has a cached base image, build thin diff on top
        Some(CacheState::ParentBase {
            parent_sha,
            cached_image_id: Some(image_id),
        }) => {
            eprintln!(
                "[cache] Parent-commit caching: using cached parent image from {}",
                &parent_sha[..8.min(parent_sha.len())]
            );

            // Try thin diff from parent; on failure, fall through to full build
            let thin_diff_result = {
                let _span = tracer.span(
                    "thin_diff",
                    "local",
                    offload::trace::PID_LOCAL,
                    offload::trace::TID_MAIN,
                );
                build_thin_diff_image(
                    image_id,
                    parent_sha,
                    &config.offload.sandbox_project_root,
                    Some(&discovery_done),
                )
                .await
            };

            match thin_diff_result {
                Ok(target_id) => {
                    provider.set_image_id(target_id);
                    let all_tests =
                        discover_with_signal(&config.framework, &config.groups, &discovery_done)
                            .await?;
                    if all_tests.is_empty() {
                        return Ok(None);
                    }
                    return dispatch_framework(
                        config,
                        &all_tests,
                        provider,
                        copy_dirs,
                        verbose,
                        tracer,
                        show_estimated_cost,
                        fail_fast,
                    )
                    .await
                    .map(Some);
                }
                Err(e) => {
                    eprintln!(
                        "[cache] Thin diff from parent failed, falling back to full build: {}",
                        e
                    );
                    // Fall through to full build
                }
            }
        }

        // Parent-commit cache miss: no cached parent image, do full build and cache on parent
        Some(CacheState::ParentBase {
            parent_sha,
            cached_image_id: None,
        }) => {
            eprintln!(
                "[cache] Parent-commit caching: no cached image for parent {}, will cache after build",
                &parent_sha[..8.min(parent_sha.len())]
            );
            // Fall through to full build; after build, cache the image on parent_sha
            parent_base_sha = Some(parent_sha.clone());
        }

        // No cache state: full build
        None => {}
    }

    // Full build fallthrough: concurrent discover + prepare
    let label = match &config.provider {
        ProviderConfig::Local(_) => "Local",
        ProviderConfig::Default(_) => "Default",
        ProviderConfig::Modal(_) => "Modal",
    };

    let (all_tests, prepare_result) = tokio::try_join!(
        discover_with_signal(&config.framework, &config.groups, &discovery_done),
        async {
            let _span = tracer.span(
                "image_prepare",
                "local",
                offload::trace::PID_LOCAL,
                offload::trace::TID_MAIN,
            );
            with_retry!(provider.prepare(
                copy_dir_tuples,
                no_cache,
                config.offload.sandbox_init_cmd.as_deref(),
                Some(&discovery_done),
                None,
            ))
            .context(format!("Failed to prepare {} provider", label))
        }
    )?;

    // Cache image on parent commit (parent-commit cache miss)
    if let (Some(parent_sha), Some(image_id)) = (&parent_base_sha, &prepare_result) {
        write_note_for_commit(parent_sha, image_id, config_path).await;
    }

    if all_tests.is_empty() {
        return Ok(None);
    }
    dispatch_framework(
        config,
        &all_tests,
        provider,
        copy_dirs,
        verbose,
        tracer,
        show_estimated_cost,
        fail_fast,
    )
    .await
    .map(Some)
}

#[allow(clippy::too_many_arguments)]
async fn run_tests(
    config_path: &Path,
    parallel_override: Option<usize>,
    collect_only: bool,
    copy_dir_args: Vec<String>,
    env_vars: Vec<String>,
    no_cache: bool,
    verbose: bool,
    trace: bool,
    show_estimated_cost: bool,
    fail_fast: bool,
) -> Result<()> {
    let tracer = if trace {
        offload::trace::Tracer::new()
    } else {
        offload::trace::Tracer::noop()
    };

    tracer.metadata_event(
        "process_name",
        offload::trace::PID_LOCAL,
        offload::trace::TID_MAIN,
        serde_json::json!({"name": "Offload (Local)"}),
    );
    tracer.metadata_event(
        "thread_name",
        offload::trace::PID_LOCAL,
        offload::trace::TID_MAIN,
        serde_json::json!({"name": "Main"}),
    );

    // Load configuration
    let mut config = config::load_config(config_path)
        .with_context(|| format!("Failed to load config from {}", config_path.display()))?;

    // Apply overrides
    if let Some(parallel) = parallel_override {
        config.offload.max_parallel = parallel;
    }

    // Parse copy_dir arguments
    let copy_dirs: Vec<CopyDir> = copy_dir_args
        .iter()
        .map(|arg| {
            let parts: Vec<&str> = arg.splitn(2, ':').collect();
            if parts.len() != 2 {
                return Err(anyhow!(
                    "Invalid copy-dir format: '{}'. Expected format: /local/path:/sandbox/path",
                    arg
                ));
            }
            Ok(CopyDir {
                local: PathBuf::from(parts[0]),
                remote: PathBuf::from(parts[1]),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Parse CLI env vars and merge into provider config (CLI overrides config)
    let cli_env: HashMap<String, String> = env_vars
        .iter()
        .filter_map(|s| {
            let mut parts = s.splitn(2, '=');
            match (parts.next(), parts.next()) {
                (Some(k), Some(v)) if !k.is_empty() => Some((k.to_string(), v.to_string())),
                _ => {
                    tracing::warn!("Ignoring invalid --env value: '{}' (expected KEY=VALUE)", s);
                    None
                }
            }
        })
        .collect();

    if !cli_env.is_empty() {
        info!("CLI --env vars: {:?}", cli_env.keys().collect::<Vec<_>>());
        match &mut config.provider {
            ProviderConfig::Local(cfg) => cfg.env.extend(cli_env),
            ProviderConfig::Default(cfg) => cfg.env.extend(cli_env),
            ProviderConfig::Modal(cfg) => cfg.env.extend(cli_env),
        }
    }

    info!("Loaded configuration from {}", config_path.display());

    // Handle collect-only: only discovery needed, no provider setup
    if collect_only {
        eprint!("Discovering tests... ");
        let all_tests = discover_all_tests(&config.framework, &config.groups).await?;
        eprintln!(
            "found {} tests across {} groups",
            all_tests.len(),
            config.groups.len()
        );
        for group_name in config.groups.keys() {
            let group_tests: Vec<_> = all_tests
                .iter()
                .filter(|t| t.group == *group_name)
                .collect();
            if !group_tests.is_empty() {
                println!("\nGroup '{}':", group_name);
                for test in group_tests {
                    println!("  {}", test.id);
                }
            }
        }
        return Ok(());
    }

    // Convert copy_dirs to tuples once (used by Default and Modal providers)
    let copy_dir_tuples: Vec<(PathBuf, PathBuf)> = copy_dirs
        .iter()
        .map(|cd| (cd.local.clone(), cd.remote.clone()))
        .collect();

    // Resolve checkpoint / cache state if not --no-cache and not local provider.
    // We fetch notes and resolve checkpoint info upfront so the provider arms
    // can use it during the concurrent discover+prepare phase.
    let cache_state = if !no_cache && !matches!(&config.provider, ProviderConfig::Local(_)) {
        let _span = tracer.span(
            "resolve_cache_state",
            "local",
            offload::trace::PID_LOCAL,
            offload::trace::TID_MAIN,
        );
        resolve_cache_state(config_path, &config).await
    } else {
        None
    };

    // Phase 1+2: Discover tests and prepare provider (concurrently where possible)
    let exit_code = match &config.provider {
        ProviderConfig::Local(p_cfg) => {
            // Local provider is synchronous — no concurrency benefit
            eprint!("Discovering tests... ");
            let all_tests = discover_all_tests(&config.framework, &config.groups).await?;
            eprintln!(
                "found {} tests across {} groups",
                all_tests.len(),
                config.groups.len()
            );
            if all_tests.is_empty() {
                info!("No tests to run");
                return Ok(());
            }
            dispatch_framework(
                &config,
                &all_tests,
                LocalProvider::new(p_cfg.clone()),
                &copy_dirs,
                verbose,
                &tracer,
                show_estimated_cost,
                fail_fast,
            )
            .await?
        }
        ProviderConfig::Default(p_cfg) => {
            let provider = DefaultProvider::from_config(p_cfg.clone());
            match run_with_caching(
                provider,
                &config,
                &cache_state,
                &copy_dir_tuples,
                &copy_dirs,
                no_cache,
                verbose,
                &tracer,
                show_estimated_cost,
                fail_fast,
                config_path,
            )
            .await?
            {
                Some(code) => code,
                None => return Ok(()),
            }
        }
        ProviderConfig::Modal(p_cfg) => {
            let provider = ModalProvider::from_config(p_cfg.clone());
            match run_with_caching(
                provider,
                &config,
                &cache_state,
                &copy_dir_tuples,
                &copy_dirs,
                no_cache,
                verbose,
                &tracer,
                show_estimated_cost,
                fail_fast,
                config_path,
            )
            .await?
            {
                Some(code) => code,
                None => return Ok(()),
            }
        }
    };

    // Write trace file if tracing was enabled
    let trace_path = config.report.output_dir.join("trace.json");
    if let Err(e) = tracer.write_to_file(&trace_path) {
        eprintln!("Warning: failed to write trace file: {}", e);
    } else if trace {
        eprintln!("Trace written to {}", trace_path.display());
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Pre-resolved cache/checkpoint state, determined before provider dispatch.
enum CacheState {
    /// Checkpoint image caching: A checkpoint commit was found (with `[checkpoint]` section).
    Checkpoint {
        checkpoint_sha: String,
        cached_image: Option<checkpoint::CachedImage>,
    },
    /// Parent-commit image caching: parent commit base image (non-checkpoint mode).
    ParentBase {
        parent_sha: String,
        cached_image_id: Option<String>,
    },
}

/// Fetch notes and resolve checkpoint/cache state before provider dispatch.
///
/// Returns `None` if not in a git repo, if resolution fails (best-effort), or if
/// this is an initial commit with no parent (non-checkpoint workflow).
async fn resolve_cache_state(config_path: &Path, config: &Config) -> Option<CacheState> {
    // Check if we're in a git repo
    if git::repo_root().await.is_err() {
        info!("Not in a git repo, skipping checkpoint/cache resolution");
        return None;
    }

    // Best-effort fetch and configure notes
    if let Err(e) = git::fetch_notes("origin").await {
        warn!("Failed to fetch notes: {}", e);
    }
    if let Err(e) = git::configure_notes_fetch("origin").await {
        warn!("Failed to configure notes fetch: {}", e);
    }

    let config_path_str = config_path.to_string_lossy();

    // Checkpoint caching: if we have a [checkpoint] section, use checkpoint-based caching
    if let Some(checkpoint_cfg) = config.checkpoint.as_ref() {
        return match checkpoint::resolve_checkpoint(&config_path_str, checkpoint_cfg, 100).await {
            Ok(Some(info)) => Some(CacheState::Checkpoint {
                checkpoint_sha: info.checkpoint_sha,
                cached_image: info.cached_image,
            }),
            Ok(None) => {
                info!("No checkpoint commit found in ancestor window");
                None
            }
            Err(e) => {
                warn!("Checkpoint resolution failed: {}", e);
                None
            }
        };
    }

    // Parent-commit caching: no [checkpoint] config — use parent commit as base
    let parent_base =
        match checkpoint::resolve_parent_base(config_path.to_str().unwrap_or("offload.toml")).await
        {
            Ok(result) => result,
            Err(e) => {
                warn!("Parent base resolution failed: {}", e);
                return None;
            }
        };
    match parent_base {
        Some((parent_sha, image_id)) => Some(CacheState::ParentBase {
            parent_sha,
            cached_image_id: Some(image_id),
        }),
        None => {
            // Check if we have a parent at all (for caching after full build)
            let parent = match git::parent_sha().await {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to get parent SHA: {}", e);
                    return None;
                }
            };
            parent.map(|parent_sha| CacheState::ParentBase {
                parent_sha,
                cached_image_id: None,
            })
        }
    }
}

/// Write a git note for an image on a specific commit (best-effort).
async fn write_note_for_commit(commit_sha: &str, image_id: &str, config_path: &Path) {
    let config_path_str = config_path.to_string_lossy();

    let config_key = match git::repo_root().await {
        Ok(root) => match git::canonicalize_config_path(&config_path_str, &root) {
            Ok(key) => key,
            Err(e) => {
                warn!("Failed to canonicalize config path for note: {}", e);
                return;
            }
        },
        Err(e) => {
            warn!("Failed to get repo root for note: {}", e);
            return;
        }
    };

    let mut contents = git::NoteContents::new();
    contents.insert(
        config_key,
        git::ImageEntry {
            image_id: image_id.to_string(),
        },
    );

    if let Err(e) = git::write_note(commit_sha, &contents).await {
        warn!("Failed to write note: {}", e);
        return;
    }
    info!(
        "Wrote image cache note on {}",
        &commit_sha[..8.min(commit_sha.len())]
    );

    if let Err(e) = git::push_notes("origin").await {
        warn!("Failed to push notes: {}", e);
    }
}

/// Show checkpoint cache status for the current HEAD.
async fn checkpoint_status_handler(config_path: &str, remote: &str) -> Result<()> {
    let path = Path::new(config_path);
    let config = config::load_config(path)
        .with_context(|| format!("Failed to load config from {}", config_path))?;

    let checkpoint_cfg = match config.checkpoint {
        Some(ref cfg) => cfg,
        None => {
            println!("Checkpoint mode not configured");
            return Ok(());
        }
    };

    // Best-effort fetch and configure notes
    let _ = git::fetch_notes(remote).await;
    let _ = git::configure_notes_fetch(remote).await;

    let head = git::head_sha().await.context("Failed to get HEAD SHA")?;
    let repo_root = git::repo_root().await.context("Failed to get repo root")?;
    let config_key = git::canonicalize_config_path(config_path, &repo_root)
        .context("Failed to canonicalize config path")?;

    let ancestors = git::ancestors(100)
        .await
        .context("Failed to list ancestors")?;

    // Find nearest checkpoint
    let mut checkpoint_sha: Option<String> = None;
    let mut checkpoint_distance: usize = 0;
    for (i, sha) in ancestors.iter().enumerate() {
        let touches = git::commit_touches_paths(sha, &checkpoint_cfg.build_inputs)
            .await
            .with_context(|| format!("Failed to check paths for commit {}", sha))?;
        if touches {
            checkpoint_sha = Some(sha.clone());
            checkpoint_distance = i;
            break;
        }
    }

    let short_head = &head[..8.min(head.len())];

    let checkpoint_sha = match checkpoint_sha {
        Some(sha) => sha,
        None => {
            println!("HEAD:               {}", short_head);
            println!("Checkpoint:         (no checkpoint found in last 100 commits)");
            println!("Next run mode:      full build (no checkpoint found)");
            return Ok(());
        }
    };

    let short_checkpoint = &checkpoint_sha[..8.min(checkpoint_sha.len())];

    // Read note for the checkpoint commit
    let note = git::read_note(&checkpoint_sha)
        .await
        .context("Failed to read note for checkpoint commit")?;

    let cached_entry = note.and_then(|contents| {
        contents
            .get(&config_key)
            .filter(|e| !e.image_id.is_empty())
            .cloned()
    });

    match cached_entry {
        Some(entry) => {
            // Determine run mode
            let run_mode = if checkpoint_sha == head {
                "use checkpoint image directly (HEAD is the checkpoint)".to_string()
            } else {
                match git::diff_file_count(&checkpoint_sha, &head).await {
                    Ok(count) => format!("thin diff ({} files changed since checkpoint)", count),
                    Err(_) => "thin diff".to_string(),
                }
            };

            println!("HEAD:               {}", short_head);
            println!(
                "Checkpoint:         {} ({} commits back)",
                short_checkpoint, checkpoint_distance
            );
            println!("Cached image:       {}", entry.image_id);
            println!("Next run mode:      {}", run_mode);
        }
        None => {
            println!("HEAD:               {}", short_head);
            println!(
                "Checkpoint:         {} ({} commits back)",
                short_checkpoint, checkpoint_distance
            );
            println!("Cached image:       (none)");
            println!("Next run mode:      full build (no cached checkpoint image)");
        }
    }

    Ok(())
}

/// Run all tests with a single orchestrator call.
/// Returns the exit code (0 = success, 1 = failures/not run, 2 = flaky only).
#[allow(clippy::too_many_arguments)]
async fn run_all_tests<P, D>(
    config: &config::Config,
    tests: &[TestRecord],
    provider: P,
    framework: D,
    copy_dirs: &[CopyDir],
    verbose: bool,
    tracer: &offload::trace::Tracer,
    show_estimated_cost: bool,
    fail_fast: bool,
) -> Result<i32>
where
    P: offload::provider::SandboxProvider,
    D: TestFramework,
{
    // Convert CopyDir to tuples
    let copy_dir_tuples: Vec<(PathBuf, PathBuf)> = copy_dirs
        .iter()
        .map(|cd| (cd.local.clone(), cd.remote.clone()))
        .collect();

    // Pre-populate sandbox pool
    let mut env = provider.base_env();
    env.push((
        "OFFLOAD_ROOT".to_string(),
        config.offload.sandbox_project_root.clone(),
    ));

    let sandbox_config = SandboxConfig {
        id: format!("offload-{}", uuid::Uuid::new_v4()),
        working_dir: config
            .offload
            .working_dir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        env,
        copy_dirs: copy_dir_tuples.clone(),
    };

    let mut sandbox_pool = SandboxPool::new();
    let _pool_span = tracer.span(
        "sandbox_pool_create",
        "local",
        offload::trace::PID_LOCAL,
        offload::trace::TID_MAIN,
    );
    sandbox_pool
        .populate(config.offload.max_parallel, &provider, &sandbox_config)
        .await
        .context("Failed to create sandboxes")?;
    drop(_pool_span);

    let orchestrator = Orchestrator::new(
        config.clone(),
        framework,
        verbose,
        tracer.clone(),
        show_estimated_cost,
        fail_fast,
    );

    let result = orchestrator.run_with_tests(tests, sandbox_pool).await?;

    Ok(result.exit_code())
}

async fn collect_tests(config_path: &Path, format: &str) -> Result<()> {
    let config = config::load_config(config_path)?;

    let all_tests = discover_all_tests(&config.framework, &config.groups).await?;

    match format {
        "json" => {
            let json = serde_json::to_string_pretty(&all_tests)?;
            println!("{}", json);
        }
        _ => {
            println!(
                "Discovered {} tests across {} groups:",
                all_tests.len(),
                config.groups.len()
            );
            for group_name in config.groups.keys() {
                let group_tests: Vec<_> = all_tests
                    .iter()
                    .filter(|t| t.group == *group_name)
                    .collect();
                if !group_tests.is_empty() {
                    println!("\nGroup '{}':", group_name);
                    for test in group_tests {
                        println!("  {}", test.id);
                    }
                }
            }
        }
    }

    Ok(())
}

fn validate_config(config_path: &Path) -> Result<()> {
    match config::load_config(config_path) {
        Ok(config) => {
            println!("Configuration is valid!");
            println!();
            println!("Settings:");
            println!("  Max parallel: {}", config.offload.max_parallel);
            println!("  Test timeout: {}s", config.offload.test_timeout_secs);

            let provider_name = match &config.provider {
                ProviderConfig::Local(_) => "local",
                ProviderConfig::Default(_) => "default",
                ProviderConfig::Modal(_) => "modal",
            };
            println!("  Provider: {}", provider_name);

            let framework_name = framework_type_name(&config.framework);
            println!("  Framework: {}", framework_name);

            if let Some(ref init_cmd) = config.offload.sandbox_init_cmd {
                println!("  Sandbox init cmd: {}", init_cmd);
            }

            println!();
            println!("Groups:");
            for (group_name, group_config) in &config.groups {
                println!(
                    "  {}: retry_count = {}",
                    group_name, group_config.retry_count
                );
            }

            Ok(())
        }
        Err(e) => {
            eprintln!("Configuration error: {}", e);
            std::process::exit(1);
        }
    }
}

fn init_config(provider: &str, framework: &str) -> Result<()> {
    let provider_config = match provider {
        "local" => ProviderConfig::Local(LocalProviderConfig {
            working_dir: Some(PathBuf::from(".")),
            ..Default::default()
        }),
        "default" => ProviderConfig::Default(DefaultProviderConfig {
            create_command: "./scripts/create-sandbox.sh".into(),
            exec_command: "./scripts/exec-sandbox.sh {sandbox_id} {command}".into(),
            destroy_command: "./scripts/destroy-sandbox.sh {sandbox_id}".into(),
            prepare_command: None,
            download_command: None,
            working_dir: None,
            timeout_secs: 3600,
            copy_dirs: vec![],
            env: HashMap::new(),
            cpu_cores: 1.0,
        }),
        _ => {
            eprintln!("Unknown provider: {}. Use: local, default", provider);
            std::process::exit(1);
        }
    };

    let framework_config = match framework {
        "pytest" => FrameworkConfig::Pytest(PytestFrameworkConfig {
            paths: None,
            command: "python -m pytest".into(),
            test_id_format: "{name}".into(),
            ..Default::default()
        }),
        "nextest" => FrameworkConfig::Cargo(CargoFrameworkConfig {
            test_id_format: "{classname} {name}".into(),
            ..Default::default()
        }),
        "default" => FrameworkConfig::Default(DefaultFrameworkConfig {
            discover_command: "echo test1 test2".into(),
            run_command: "echo Running {tests}".into(),
            test_id_format: "{name}".into(),
            result_file: None,
            working_dir: None,
        }),
        "vitest" => FrameworkConfig::Vitest(VitestFrameworkConfig {
            command: "npx vitest".into(),
            test_id_format: "{classname} > {name}".into(),
            ..Default::default()
        }),
        _ => {
            eprintln!(
                "Unknown framework: {}. Use: pytest, nextest, vitest, default",
                framework
            );
            std::process::exit(1);
        }
    };

    let config = Config {
        offload: OffloadConfig {
            max_parallel: 10,
            test_timeout_secs: 900,
            working_dir: None,
            sandbox_project_root: "/app".to_string(),
            sandbox_init_cmd: None,
        },
        provider: provider_config,
        framework: framework_config,
        groups: HashMap::from([(
            "default".to_string(),
            GroupConfig {
                retry_count: 0,
                filters: String::new(),
                ..Default::default()
            },
        )]),
        report: ReportConfig::default(),
        checkpoint: None,
    };

    let toml_content = toml::to_string_pretty(&config)?;
    let output = format!("# offload configuration file\n\n{}", toml_content);

    let path = PathBuf::from("offload.toml");
    if path.exists() {
        eprintln!("offload.toml already exists. Remove it first or edit manually.");
        std::process::exit(1);
    }

    std::fs::write(&path, output)?;
    println!("Created offload.toml");
    println!();
    println!("Edit the configuration as needed, then run:");
    println!("  offload run");

    Ok(())
}

fn show_logs(
    config_path: &Path,
    failures: bool,
    errors: bool,
    test_ids: &[String],
    test_regex: Option<&str>,
) -> Result<()> {
    let config = config::load_config(config_path)
        .with_context(|| format!("Failed to load config from {}", config_path.display()))?;

    let junit_path = config.report.output_dir.join(&config.report.junit_file);

    if !junit_path.is_file() {
        eprintln!(
            "No test results found at {}. Run `offload run` first.",
            junit_path.display()
        );
        std::process::exit(1);
    }

    let re = test_regex
        .map(regex::Regex::new)
        .transpose()
        .context("Invalid --test-regex pattern")?;

    let xml_content = std::fs::read_to_string(&junit_path)
        .with_context(|| format!("Failed to read {}", junit_path.display()))?;

    let testsuites = offload::report::parse_all_testsuites_xml(&xml_content);

    // Collect all testcases, deduplicating by test name (keep the one with failure/error info if any)
    use std::collections::BTreeMap;
    let mut tests: BTreeMap<String, &offload::report::TestcaseXml> = BTreeMap::new();
    for suite in &testsuites {
        for tc in &suite.testcases {
            let existing = tests.get(tc.name.as_str());
            // Prefer the entry that has failure/error info over a passing one
            let dominated = match existing {
                None => true,
                Some(prev) => {
                    (tc.failure.is_some() || tc.error.is_some())
                        && prev.failure.is_none()
                        && prev.error.is_none()
                }
            };
            if dominated {
                tests.insert(tc.name.clone(), tc);
            }
        }
    }

    // Filter by status flags, then by test selection
    let filtered: Vec<(&String, &&offload::report::TestcaseXml)> = tests
        .iter()
        .filter(|(name, tc)| {
            // Status filter
            let status_ok = if failures && errors {
                tc.failure.is_some() || tc.error.is_some()
            } else if failures {
                tc.failure.is_some()
            } else if errors {
                tc.error.is_some()
            } else {
                true
            };
            if !status_ok {
                return false;
            }

            // Exact ID filter
            if !test_ids.is_empty() && !test_ids.iter().any(|id| id == name.as_str()) {
                return false;
            }

            // Regex filter
            if let Some(ref re) = re
                && !re.is_match(name)
            {
                return false;
            }

            true
        })
        .collect();

    if filtered.is_empty() {
        eprintln!("No matching test results found in {}", junit_path.display());
        return Ok(());
    }

    for (name, tc) in &filtered {
        let status = if tc.failure.is_some() {
            "FAILED"
        } else if tc.error.is_some() {
            "ERROR"
        } else {
            "PASSED"
        };

        println!("=== {} [{}] ===", name, status);

        if let Some(ref failure) = tc.failure {
            if let Some(ref msg) = failure.message {
                println!("{}", msg);
            }
            if !failure.content.is_empty() {
                println!("{}", failure.content);
            }
        }
        if let Some(ref error) = tc.error {
            if let Some(ref msg) = error.message {
                println!("{}", msg);
            }
            if !error.content.is_empty() {
                println!("{}", error.content);
            }
        }
        println!();
    }

    Ok(())
}
