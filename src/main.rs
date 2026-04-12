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
    CheckpointContext, SandboxProvider, default::DefaultProvider, local::LocalProvider,
    modal::ModalProvider,
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
            // Run discovery and image preparation concurrently
            let discovery_done = AtomicBool::new(false);
            let mut provider = DefaultProvider::from_config(p_cfg.clone());

            // Determine if we can skip prepare via cached image (Mode 2)
            let skip_prepare = if let Some(CacheState::CachedImage(ref image_id)) = cache_state {
                provider.set_image_id(image_id.clone());
                info!("Using cached image from git notes: {}", image_id);
                eprintln!("[cache] Using cached image: {}", image_id);
                true
            } else {
                false
            };

            // Apply checkpoint context if available (Mode 1), track if we set one
            let has_checkpoint = if let Some(CacheState::Checkpoint {
                checkpoint_sha,
                cached_image: Some(cached),
            }) = &cache_state
            {
                provider = provider.with_checkpoint(CheckpointContext {
                    image_id: cached.image_id.clone(),
                    commit_sha: checkpoint_sha.clone(),
                    sandbox_project_root: config.offload.sandbox_project_root.clone(),
                });
                info!(
                    "Using checkpoint image {} from commit {}",
                    cached.image_id, checkpoint_sha
                );
                eprintln!(
                    "[cache] Using checkpoint image from {}",
                    &checkpoint_sha[..8.min(checkpoint_sha.len())]
                );
                true
            } else {
                false
            };

            let (all_tests, prepare_result): (Vec<TestRecord>, Option<String>) = if skip_prepare {
                // Discovery only -- prepare already satisfied by cached image
                let tests =
                    discover_with_signal(&config.framework, &config.groups, &discovery_done)
                        .await?;
                (tests, None)
            } else {
                tokio::try_join!(
                    discover_with_signal(&config.framework, &config.groups, &discovery_done),
                    async {
                        let _span = tracer.span(
                            "image_prepare",
                            "local",
                            offload::trace::PID_LOCAL,
                            offload::trace::TID_MAIN,
                        );
                        // If checkpoint prepare fails, fall back to full build
                        let image_id = if has_checkpoint {
                            match with_retry!(provider.prepare(
                                &copy_dir_tuples,
                                no_cache,
                                config.offload.sandbox_init_cmd.as_deref(),
                                Some(&discovery_done),
                                None,
                            )) {
                                Ok(id) => Ok(id),
                                Err(e) => {
                                    warn!(
                                        "Checkpoint prepare failed, falling back to full build: {}",
                                        e
                                    );
                                    eprintln!(
                                        "[cache] Checkpoint prepare failed, falling back to full build"
                                    );
                                    provider.clear_checkpoint();
                                    with_retry!(provider.prepare(
                                        &copy_dir_tuples,
                                        no_cache,
                                        config.offload.sandbox_init_cmd.as_deref(),
                                        Some(&discovery_done),
                                        None,
                                    ))
                                    .context("Failed to prepare Default provider (fallback)")
                                }
                            }
                        } else {
                            with_retry!(provider.prepare(
                                &copy_dir_tuples,
                                no_cache,
                                config.offload.sandbox_init_cmd.as_deref(),
                                Some(&discovery_done),
                                None,
                            ))
                            .context("Failed to prepare Default provider")
                        }?;
                        Ok(image_id)
                    }
                )?
            };

            // Write note on cache miss
            write_note_on_cache_miss(&cache_state, &prepare_result, config_path, &config).await;

            if all_tests.is_empty() {
                info!("No tests to run");
                return Ok(());
            }
            dispatch_framework(
                &config,
                &all_tests,
                provider,
                &copy_dirs,
                verbose,
                &tracer,
                show_estimated_cost,
                fail_fast,
            )
            .await?
        }
        ProviderConfig::Modal(p_cfg) => {
            // Run discovery and image preparation concurrently
            let discovery_done = AtomicBool::new(false);
            let mut provider = ModalProvider::from_config(p_cfg.clone());

            // Determine if we can skip prepare via cached image (Mode 2)
            let skip_prepare = if let Some(CacheState::CachedImage(ref image_id)) = cache_state {
                provider.set_image_id(image_id.clone());
                info!("Using cached image from git notes: {}", image_id);
                eprintln!("[cache] Using cached image: {}", image_id);
                true
            } else {
                false
            };

            // Apply checkpoint context if available (Mode 1), track if we set one
            let has_checkpoint = if let Some(CacheState::Checkpoint {
                checkpoint_sha,
                cached_image: Some(cached),
            }) = &cache_state
            {
                provider = provider.with_checkpoint(CheckpointContext {
                    image_id: cached.image_id.clone(),
                    commit_sha: checkpoint_sha.clone(),
                    sandbox_project_root: config.offload.sandbox_project_root.clone(),
                });
                info!(
                    "Using checkpoint image {} from commit {}",
                    cached.image_id, checkpoint_sha
                );
                eprintln!(
                    "[cache] Using checkpoint image from {}",
                    &checkpoint_sha[..8.min(checkpoint_sha.len())]
                );
                true
            } else {
                false
            };

            let (all_tests, prepare_result): (Vec<TestRecord>, Option<String>) = if skip_prepare {
                // Discovery only -- prepare already satisfied by cached image
                let tests =
                    discover_with_signal(&config.framework, &config.groups, &discovery_done)
                        .await?;
                (tests, None)
            } else {
                tokio::try_join!(
                    discover_with_signal(&config.framework, &config.groups, &discovery_done),
                    async {
                        let _span = tracer.span(
                            "image_prepare",
                            "local",
                            offload::trace::PID_LOCAL,
                            offload::trace::TID_MAIN,
                        );
                        // If checkpoint prepare fails, fall back to full build
                        let image_id = if has_checkpoint {
                            match with_retry!(provider.prepare(
                                &copy_dir_tuples,
                                no_cache,
                                config.offload.sandbox_init_cmd.as_deref(),
                                Some(&discovery_done),
                                None,
                            )) {
                                Ok(id) => Ok(id),
                                Err(e) => {
                                    warn!(
                                        "Checkpoint prepare failed, falling back to full build: {}",
                                        e
                                    );
                                    eprintln!(
                                        "[cache] Checkpoint prepare failed, falling back to full build"
                                    );
                                    provider.clear_checkpoint();
                                    with_retry!(provider.prepare(
                                        &copy_dir_tuples,
                                        no_cache,
                                        config.offload.sandbox_init_cmd.as_deref(),
                                        Some(&discovery_done),
                                        None,
                                    ))
                                    .context("Failed to prepare Modal provider (fallback)")
                                }
                            }
                        } else {
                            with_retry!(provider.prepare(
                                &copy_dir_tuples,
                                no_cache,
                                config.offload.sandbox_init_cmd.as_deref(),
                                Some(&discovery_done),
                                None,
                            ))
                            .context("Failed to prepare Modal provider")
                        }?;
                        Ok(image_id)
                    }
                )?
            };

            // Write note on cache miss
            write_note_on_cache_miss(&cache_state, &prepare_result, config_path, &config).await;

            if all_tests.is_empty() {
                info!("No tests to run");
                return Ok(());
            }
            dispatch_framework(
                &config,
                &all_tests,
                provider,
                &copy_dirs,
                verbose,
                &tracer,
                show_estimated_cost,
                fail_fast,
            )
            .await?
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
    /// Mode 2: HEAD has a cached image in git notes (no `[checkpoint]` section).
    CachedImage(String),
    /// Mode 1: A checkpoint commit was found (with `[checkpoint]` section).
    Checkpoint {
        checkpoint_sha: String,
        cached_image: Option<checkpoint::CachedImage>,
    },
    /// Mode 1 or 2: No cached image or checkpoint found -- full build needed.
    CacheMiss,
}

/// Fetch notes and resolve checkpoint/cache state before provider dispatch.
///
/// Returns `None` if not in a git repo or if resolution fails (best-effort).
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

    if let Some(ref checkpoint_cfg) = config.checkpoint {
        // Mode 1: With [checkpoint] section
        match checkpoint::resolve_checkpoint(&config_path_str, checkpoint_cfg, 100).await {
            Ok(Some(info)) => Some(CacheState::Checkpoint {
                checkpoint_sha: info.checkpoint_sha,
                cached_image: info.cached_image,
            }),
            Ok(None) => {
                info!("No checkpoint commit found in ancestor window");
                Some(CacheState::CacheMiss)
            }
            Err(e) => {
                warn!("Checkpoint resolution failed: {}", e);
                None
            }
        }
    } else {
        // Mode 2: Without [checkpoint] section
        match checkpoint::resolve_cached_image(&config_path_str).await {
            Ok(Some(image_id)) => Some(CacheState::CachedImage(image_id)),
            Ok(None) => Some(CacheState::CacheMiss),
            Err(e) => {
                warn!("Cached image resolution failed: {}", e);
                None
            }
        }
    }
}

/// Write a git note and push after a cache miss (best-effort).
///
/// Called after a successful prepare when the cache state indicates a miss.
/// For Mode 1 (checkpoint): writes note on the checkpoint commit.
/// For Mode 2 (no checkpoint): writes note on HEAD.
async fn write_note_on_cache_miss(
    cache_state: &Option<CacheState>,
    prepare_result: &Option<String>,
    config_path: &Path,
    config: &Config,
) {
    // Only write on cache miss where we actually built an image
    let image_id = match prepare_result {
        Some(id) => id,
        None => return, // Skipped prepare (cache hit) or no image returned
    };

    let is_miss = matches!(
        cache_state,
        Some(CacheState::CacheMiss)
            | Some(CacheState::Checkpoint {
                cached_image: None,
                ..
            })
    );
    if !is_miss {
        return;
    }

    let config_path_str = config_path.to_string_lossy();

    // Determine which commit SHA to write the note on
    let commit_sha = match cache_state {
        Some(CacheState::Checkpoint { checkpoint_sha, .. }) => checkpoint_sha.clone(),
        _ => match git::head_sha().await {
            Ok(sha) => sha,
            Err(e) => {
                warn!("Failed to get HEAD SHA for note: {}", e);
                return;
            }
        },
    };

    // Canonicalize config path
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

    // Compute build inputs hash if checkpoint config is present
    let build_inputs_hash = if let Some(ref checkpoint_cfg) = config.checkpoint {
        match git::repo_root().await {
            Ok(root) => {
                match git::compute_build_inputs_hash(&root, &checkpoint_cfg.build_inputs).await {
                    Ok(hash) => Some(hash),
                    Err(e) => {
                        warn!("Failed to compute build inputs hash: {}", e);
                        None
                    }
                }
            }
            Err(_) => None,
        }
    } else {
        None
    };

    let mut contents = git::NoteContents::new();
    contents.insert(
        config_key,
        git::ImageEntry {
            image_id: image_id.clone(),
            build_inputs_hash,
        },
    );

    if let Err(e) = git::write_note(&commit_sha, &contents).await {
        warn!("Failed to write git note: {}", e);
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
            // Compute current build inputs hash
            let current_hash =
                git::compute_build_inputs_hash(&repo_root, &checkpoint_cfg.build_inputs).await;

            let hash_display = match (&current_hash, &entry.build_inputs_hash) {
                (Ok(current), Some(cached)) if current == cached => {
                    format!("{} (matches cached)", &current[..8.min(current.len())])
                }
                (Ok(current), Some(cached)) => {
                    format!(
                        "{} (MISMATCH: cached {})",
                        &current[..8.min(current.len())],
                        &cached[..8.min(cached.len())]
                    )
                }
                (Ok(current), None) => {
                    format!(
                        "{} (no cached hash to compare)",
                        &current[..8.min(current.len())]
                    )
                }
                (Err(_), _) => "(failed to compute)".to_string(),
            };

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
            println!("Build inputs hash:  {}", hash_display);
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
            paths: vec![PathBuf::from("tests")],
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
