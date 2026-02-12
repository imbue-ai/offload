//! offload CLI - Flexible parallel test runner.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

use tokio::sync::Mutex;

use offload::config::{self, FrameworkConfig, ProviderConfig};
use offload::framework::{
    TestFramework, TestRecord, cargo::CargoFramework, default::DefaultFramework,
    pytest::PytestFramework,
};
use offload::orchestrator::{Orchestrator, SandboxPool};
use offload::provider::{default::DefaultProvider, local::LocalProvider, modal::ModalProvider};
use offload::report::{ConsoleReporter, cleanup_parts, merge_junit_files};

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

        /// Test framework (pytest, cargo, generic)
        #[arg(short, long, default_value = "pytest")]
        framework: String,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Set up logging
    let log_level = if cli.verbose {
        Level::DEBUG
    } else {
        Level::INFO
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
        } => run_tests(&cli.config, parallel, collect_only, copy_dir, cli.verbose).await,
        Commands::Collect { format } => collect_tests(&cli.config, &format).await,
        Commands::Validate => validate_config(&cli.config),
        Commands::Init {
            provider,
            framework,
        } => init_config(&provider, &framework),
    }
}

/// Tracks group boundaries within the flat test list.
struct GroupBoundary {
    name: String,
    start: usize,
    count: usize,
}

/// Helper to get framework type name for validation.
fn framework_type_name(framework: &FrameworkConfig) -> &'static str {
    match framework {
        FrameworkConfig::Pytest(_) => "pytest",
        FrameworkConfig::Cargo(_) => "cargo",
        FrameworkConfig::Default(_) => "default",
    }
}

async fn run_tests(
    config_path: &Path,
    parallel_override: Option<usize>,
    collect_only: bool,
    copy_dir_args: Vec<String>,
    verbose: bool,
) -> Result<()> {
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

    info!("Loaded configuration from {}", config_path.display());

    // Validate all groups use the same framework type
    let mut framework_types = config
        .groups
        .values()
        .map(|g| framework_type_name(&g.framework));
    let first_type = framework_types
        .next()
        .ok_or_else(|| anyhow!("No groups configured"))?;
    for ft in framework_types {
        if ft != first_type {
            return Err(anyhow!(
                "All groups must use the same framework type. Found '{}' and '{}'",
                first_type,
                ft
            ));
        }
    }

    // Phase 1: Discover tests for all groups into a single Vec
    info!("Discovering tests for all groups...");
    let mut all_tests: Vec<TestRecord> = Vec::new();
    let mut boundaries: Vec<GroupBoundary> = Vec::new();

    for (group_name, group_config) in &config.groups {
        let start = all_tests.len();
        let tests = match &group_config.framework {
            FrameworkConfig::Pytest(cfg) => PytestFramework::new(cfg.clone()).discover(&[]).await?,
            FrameworkConfig::Cargo(cfg) => CargoFramework::new(cfg.clone()).discover(&[]).await?,
            FrameworkConfig::Default(cfg) => {
                DefaultFramework::new(cfg.clone()).discover(&[]).await?
            }
        };
        let count = tests.len();
        info!("Group '{}': discovered {} tests", group_name, count);

        // Set retry count and group name for each test
        let retry_count = group_config
            .retry_count
            .unwrap_or(config.offload.retry_count);
        let tests_with_config: Vec<TestRecord> = tests
            .into_iter()
            .map(|t| {
                t.with_retry_count(retry_count)
                    .with_group(group_name.clone())
            })
            .collect();

        all_tests.extend(tests_with_config);
        boundaries.push(GroupBoundary {
            name: group_name.clone(),
            start,
            count,
        });
    }

    info!(
        "Total: {} tests across {} groups",
        all_tests.len(),
        boundaries.len()
    );

    if collect_only {
        for boundary in &boundaries {
            println!("\nGroup '{}':", boundary.name);
            for test in &all_tests[boundary.start..boundary.start + boundary.count] {
                println!("  {}", test.id);
            }
        }
        return Ok(());
    }

    if all_tests.is_empty() {
        info!("No tests to run");
        return Ok(());
    }

    // Phase 2: Run ALL tests at once with a single orchestrator
    // Get framework config from first group (all groups have same type)
    let first_group_config = config
        .groups
        .values()
        .next()
        .ok_or_else(|| anyhow!("No groups configured"))?;

    let exit_code = match (&config.provider, &first_group_config.framework) {
        (ProviderConfig::Local(p_cfg), FrameworkConfig::Pytest(f_cfg)) => {
            run_all_tests(
                &config,
                &all_tests,
                LocalProvider::new(p_cfg.clone()),
                PytestFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
        (ProviderConfig::Local(p_cfg), FrameworkConfig::Cargo(f_cfg)) => {
            run_all_tests(
                &config,
                &all_tests,
                LocalProvider::new(p_cfg.clone()),
                CargoFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
        (ProviderConfig::Local(p_cfg), FrameworkConfig::Default(f_cfg)) => {
            run_all_tests(
                &config,
                &all_tests,
                LocalProvider::new(p_cfg.clone()),
                DefaultFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
        (ProviderConfig::Default(p_cfg), FrameworkConfig::Pytest(f_cfg)) => {
            // Convert CopyDir to tuples for provider
            let copy_dir_tuples: Vec<(PathBuf, PathBuf)> = copy_dirs
                .iter()
                .map(|cd| (cd.local.clone(), cd.remote.clone()))
                .collect();
            let provider = DefaultProvider::from_config(p_cfg.clone(), &copy_dir_tuples)
                .await
                .context("Failed to create Default provider")?;
            run_all_tests(
                &config,
                &all_tests,
                provider,
                PytestFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
        (ProviderConfig::Default(p_cfg), FrameworkConfig::Cargo(f_cfg)) => {
            // Convert CopyDir to tuples for provider
            let copy_dir_tuples: Vec<(PathBuf, PathBuf)> = copy_dirs
                .iter()
                .map(|cd| (cd.local.clone(), cd.remote.clone()))
                .collect();
            let provider = DefaultProvider::from_config(p_cfg.clone(), &copy_dir_tuples)
                .await
                .context("Failed to create Default provider")?;
            run_all_tests(
                &config,
                &all_tests,
                provider,
                CargoFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
        (ProviderConfig::Default(p_cfg), FrameworkConfig::Default(f_cfg)) => {
            // Convert CopyDir to tuples for provider
            let copy_dir_tuples: Vec<(PathBuf, PathBuf)> = copy_dirs
                .iter()
                .map(|cd| (cd.local.clone(), cd.remote.clone()))
                .collect();
            let provider = DefaultProvider::from_config(p_cfg.clone(), &copy_dir_tuples)
                .await
                .context("Failed to create Default provider")?;
            run_all_tests(
                &config,
                &all_tests,
                provider,
                DefaultFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
        (ProviderConfig::Modal(p_cfg), FrameworkConfig::Pytest(f_cfg)) => {
            let working_dir = config.offload.working_dir.clone();
            let provider = ModalProvider::from_config(p_cfg.clone(), working_dir)
                .context("Failed to create Modal provider")?;
            run_all_tests(
                &config,
                &all_tests,
                provider,
                PytestFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
        (ProviderConfig::Modal(p_cfg), FrameworkConfig::Cargo(f_cfg)) => {
            let working_dir = config.offload.working_dir.clone();
            let provider = ModalProvider::from_config(p_cfg.clone(), working_dir)
                .context("Failed to create Modal provider")?;
            run_all_tests(
                &config,
                &all_tests,
                provider,
                CargoFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
        (ProviderConfig::Modal(p_cfg), FrameworkConfig::Default(f_cfg)) => {
            let working_dir = config.offload.working_dir.clone();
            let provider = ModalProvider::from_config(p_cfg.clone(), working_dir)
                .context("Failed to create Modal provider")?;
            run_all_tests(
                &config,
                &all_tests,
                provider,
                DefaultFramework::new(f_cfg.clone()),
                &copy_dirs,
                verbose,
            )
            .await?
        }
    };

    // Phase 3: Report results per group
    info!("Results by group:");
    for boundary in &boundaries {
        let group_tests = &all_tests[boundary.start..boundary.start + boundary.count];
        let passed = group_tests.iter().filter(|t| t.passed()).count();
        let failed = group_tests
            .iter()
            .filter(|t| {
                t.final_result().is_some_and(|r| {
                    r.outcome == offload::framework::TestOutcome::Failed
                        || r.outcome == offload::framework::TestOutcome::Error
                })
            })
            .count();
        let flaky = group_tests.iter().filter(|t| t.is_flaky()).count();
        info!(
            "  {}: {} passed, {} failed, {} flaky",
            boundary.name, passed, failed, flaky
        );
    }

    if exit_code != 0 {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Run all tests with a single orchestrator call.
/// Returns the exit code (0 = success, 1 = failures/not run, 2 = flaky only).
async fn run_all_tests<P, D>(
    config: &config::Config,
    tests: &[TestRecord],
    provider: P,
    framework: D,
    copy_dirs: &[CopyDir],
    verbose: bool,
) -> Result<i32>
where
    P: offload::provider::SandboxProvider,
    D: TestFramework,
{
    let sandbox_pool = Mutex::new(SandboxPool::new());
    let reporter = ConsoleReporter::new(verbose);

    // Convert CopyDir to tuples
    let copy_dir_tuples: Vec<(PathBuf, PathBuf)> = copy_dirs
        .iter()
        .map(|cd| (cd.local.clone(), cd.remote.clone()))
        .collect();

    let orchestrator = Orchestrator::new(
        config.clone(),
        provider,
        framework,
        reporter,
        &copy_dir_tuples,
    );

    let result = orchestrator.run_with_tests(tests, &sandbox_pool).await?;
    sandbox_pool.lock().await.terminate_all().await;

    // Merge JUnit XML files from parts directory
    let parts_dir = config.report.output_dir.join("parts");
    let junit_path = config.report.output_dir.join(&config.report.junit_file);

    if parts_dir.exists() {
        if let Err(e) = merge_junit_files(&parts_dir, &junit_path) {
            tracing::error!("Failed to merge JUnit XML files: {}", e);
        }
        if let Err(e) = cleanup_parts(&parts_dir) {
            tracing::warn!("Failed to clean up JUnit parts: {}", e);
        }
    }

    Ok(result.exit_code())
}

async fn collect_tests(config_path: &Path, format: &str) -> Result<()> {
    let config = config::load_config(config_path)?;

    for group_config in config.groups.values() {
        let tests = match &group_config.framework {
            FrameworkConfig::Pytest(cfg) => PytestFramework::new(cfg.clone()).discover(&[]).await?,
            FrameworkConfig::Cargo(cfg) => CargoFramework::new(cfg.clone()).discover(&[]).await?,
            FrameworkConfig::Default(cfg) => {
                DefaultFramework::new(cfg.clone()).discover(&[]).await?
            }
        };

        match format {
            "json" => {
                let json = serde_json::to_string_pretty(&tests)?;
                println!("{}", json);
            }
            _ => {
                println!("Discovered {} tests:", tests.len());
                for test in &tests {
                    let markers = if test.markers.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", test.markers.join(", "))
                    };
                    println!("  {}{}", test.id, markers);
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
            println!("  Retry count: {}", config.offload.retry_count);

            let provider_name = match &config.provider {
                ProviderConfig::Local(_) => "local",
                ProviderConfig::Modal(_) => "modal",
                ProviderConfig::Default(_) => "default",
            };
            println!("  Provider: {}", provider_name);

            // TODO: validate each group

            for (group_name, group_config) in &config.groups {
                let framework_name = match group_config.framework {
                    FrameworkConfig::Pytest(_) => "pytest",
                    FrameworkConfig::Cargo(_) => "cargo",
                    FrameworkConfig::Default(_) => "default",
                };
                println!("Group: {}  Framework: {}", group_name, framework_name);
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
        "local" => {
            r#"[provider]
type = "local"
working_dir = "."
shell = "/bin/sh""#
        }
        "default" => {
            r#"[provider]
type = "default"
# Your script that handles everything: spin up cloud compute, run tests, return results
# Test command is appended to this
execute_command = "./scripts/run-remote.sh"
# Optional: sync code before running
setup_command = "./scripts/sync-code.sh"
# Timeout for remote execution
timeout_secs = 3600"#
        }
        _ => {
            eprintln!("Unknown provider: {}. Use: local, default", provider);
            std::process::exit(1);
        }
    };

    let framework_config = match framework {
        "pytest" => {
            r#"[groups.default]
type = "pytest"
paths = ["tests"]
python = "python""#
        }
        "cargo" => {
            r#"[groups.default]
type = "cargo""#
        }
        "default" => {
            r#"[groups.default]
type = "default"
discover_command = "echo test1 test2"
run_command = "echo Running {tests}""#
        }
        _ => {
            eprintln!(
                "Unknown framework: {}. Use: pytest, cargo, default",
                framework
            );
            std::process::exit(1);
        }
    };

    let config = format!(
        r#"# offload configuration file

[offload]
max_parallel = 10
test_timeout_secs = 900
retry_count = 3

{}

{}

[report]
output_dir = "test-results"
junit = true
junit_file = "junit.xml"
"#,
        provider_config, framework_config
    );

    let path = PathBuf::from("offload.toml");
    if path.exists() {
        eprintln!("offload.toml already exists. Remove it first or edit manually.");
        std::process::exit(1);
    }

    std::fs::write(&path, config)?;
    println!("Created offload.toml");
    println!();
    println!("Edit the configuration as needed, then run:");
    println!("  offload run");

    Ok(())
}
