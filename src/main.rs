//! offload CLI - Flexible parallel test runner.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

use tokio::sync::Mutex;

use offload::config::{self, FrameworkConfig, ProviderConfig};
use offload::framework::{
    TestFramework, TestGroup, cargo::CargoFramework, default::DefaultFramework,
    pytest::PytestFramework,
};
use offload::orchestrator::{Orchestrator, SandboxPool};
use offload::provider::{SandboxProvider, default::DefaultProvider, local::LocalProvider};
use offload::report::{ConsoleReporter, JUnitReporter, MultiReporter};

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

        /// JUnit XML output path
        #[arg(long)]
        junit: Option<PathBuf>,
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

#[tokio::main]
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
            junit,
        } => run_tests(&cli.config, parallel, collect_only, junit, cli.verbose).await,
        Commands::Collect { format } => collect_tests(&cli.config, &format).await,
        Commands::Validate => validate_config(&cli.config),
        Commands::Init {
            provider,
            framework,
        } => init_config(&provider, &framework),
    }
}

async fn run_tests(
    config_path: &Path,
    parallel_override: Option<usize>,
    collect_only: bool,
    junit_path: Option<PathBuf>,
    verbose: bool,
) -> Result<()> {
    // Load configuration
    let mut config = config::load_config(config_path)
        .with_context(|| format!("Failed to load config from {}", config_path.display()))?;

    // Apply overrides
    if let Some(parallel) = parallel_override {
        config.offload.max_parallel = parallel;
    }

    info!("Loaded configuration from {}", config_path.display());

    // Phase 1: Discover tests for all groups
    info!("Discovering tests for all groups...");
    let mut groups: Vec<TestGroup> = Vec::new();

    for (group_name, group_config) in &config.groups {
        let tests = match &group_config.framework {
            FrameworkConfig::Pytest(cfg) => PytestFramework::new(cfg.clone()).discover(&[]).await?,
            FrameworkConfig::Cargo(cfg) => CargoFramework::new(cfg.clone()).discover(&[]).await?,
            FrameworkConfig::Default(cfg) => {
                DefaultFramework::new(cfg.clone()).discover(&[]).await?
            }
        };
        info!("Group '{}': discovered {} tests", group_name, tests.len());
        groups.push(TestGroup::new(group_name.clone(), tests));
    }

    let total_tests: usize = groups.iter().map(|g| g.len()).sum();
    info!(
        "Total: {} tests across {} groups",
        total_tests,
        groups.len()
    );

    if collect_only {
        for group in &groups {
            println!("\nGroup '{}':", group.name());
            for test in group.tests() {
                println!("  {}", test.id);
            }
        }
        return Ok(());
    }

    // Phase 2: Run all groups with shared sandbox pool
    match &config.provider {
        ProviderConfig::Local(p_cfg) => {
            run_all_groups_local(&config, &groups, p_cfg, junit_path, verbose).await?;
        }
        ProviderConfig::Default(p_cfg) => {
            run_all_groups_default(&config, &groups, p_cfg, junit_path, verbose).await?;
        }
    }

    // Phase 3: Report results per group
    info!("Results by group:");
    for group in &groups {
        let passed = group.passed_count();
        let failed = group.failed_count();
        let flaky = group.flaky_count();
        info!(
            "  {}: {} passed, {} failed, {} flaky",
            group.name(),
            passed,
            failed,
            flaky
        );
    }

    Ok(())
}

/// Run all groups with a shared sandbox pool using LocalProvider.
async fn run_all_groups_local(
    config: &config::Config,
    groups: &[TestGroup],
    provider_config: &offload::config::LocalProviderConfig,
    junit_path: Option<PathBuf>,
    verbose: bool,
) -> Result<()> {
    let sandbox_pool: Mutex<SandboxPool<offload::provider::local::LocalSandbox>> =
        Mutex::new(SandboxPool::new());

    for group in groups {
        if group.is_empty() {
            continue;
        }

        let group_config = config
            .groups
            .get(group.name())
            .ok_or_else(|| anyhow!("Group '{}' not found in config", group.name()))?;

        // Create fresh provider for each group
        let provider = LocalProvider::new(provider_config.clone());

        match &group_config.framework {
            FrameworkConfig::Pytest(cfg) => {
                let framework = PytestFramework::new(cfg.clone());
                run_group(
                    config,
                    group,
                    provider,
                    framework,
                    &sandbox_pool,
                    junit_path.clone(),
                    verbose,
                )
                .await?;
            }
            FrameworkConfig::Cargo(cfg) => {
                let framework = CargoFramework::new(cfg.clone());
                run_group(
                    config,
                    group,
                    provider,
                    framework,
                    &sandbox_pool,
                    junit_path.clone(),
                    verbose,
                )
                .await?;
            }
            FrameworkConfig::Default(cfg) => {
                let framework = DefaultFramework::new(cfg.clone());
                run_group(
                    config,
                    group,
                    provider,
                    framework,
                    &sandbox_pool,
                    junit_path.clone(),
                    verbose,
                )
                .await?;
            }
        }
    }

    sandbox_pool.lock().await.terminate_all().await;
    Ok(())
}

/// Run all groups with a shared sandbox pool using DefaultProvider.
async fn run_all_groups_default(
    config: &config::Config,
    groups: &[TestGroup],
    provider_config: &offload::config::DefaultProviderConfig,
    junit_path: Option<PathBuf>,
    verbose: bool,
) -> Result<()> {
    let sandbox_pool: Mutex<SandboxPool<offload::provider::default::DefaultSandbox>> =
        Mutex::new(SandboxPool::new());

    for group in groups {
        if group.is_empty() {
            continue;
        }

        let group_config = config
            .groups
            .get(group.name())
            .ok_or_else(|| anyhow!("Group '{}' not found in config", group.name()))?;

        // Create fresh provider for each group
        let provider = DefaultProvider::from_config(provider_config.clone());

        match &group_config.framework {
            FrameworkConfig::Pytest(cfg) => {
                let framework = PytestFramework::new(cfg.clone());
                run_group(
                    config,
                    group,
                    provider,
                    framework,
                    &sandbox_pool,
                    junit_path.clone(),
                    verbose,
                )
                .await?;
            }
            FrameworkConfig::Cargo(cfg) => {
                let framework = CargoFramework::new(cfg.clone());
                run_group(
                    config,
                    group,
                    provider,
                    framework,
                    &sandbox_pool,
                    junit_path.clone(),
                    verbose,
                )
                .await?;
            }
            FrameworkConfig::Default(cfg) => {
                let framework = DefaultFramework::new(cfg.clone());
                run_group(
                    config,
                    group,
                    provider,
                    framework,
                    &sandbox_pool,
                    junit_path.clone(),
                    verbose,
                )
                .await?;
            }
        }
    }

    sandbox_pool.lock().await.terminate_all().await;
    Ok(())
}

/// Run a single group's tests.
async fn run_group<P, D>(
    config: &config::Config,
    group: &TestGroup,
    provider: P,
    framework: D,
    sandbox_pool: &Mutex<SandboxPool<P::Sandbox>>,
    junit_path: Option<PathBuf>,
    verbose: bool,
) -> Result<()>
where
    P: SandboxProvider,
    D: TestFramework,
{
    info!("Running group '{}'...", group.name());

    let reporter = create_reporter(config, junit_path, verbose);
    let orchestrator = Orchestrator::new(
        config.clone(),
        group.name().to_string(),
        provider,
        framework,
        reporter,
    );

    orchestrator
        .run_with_tests(group.tests(), sandbox_pool)
        .await?;

    Ok(())
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

fn create_reporter(
    config: &config::Config,
    junit_override: Option<PathBuf>,
    verbose: bool,
) -> MultiReporter {
    let mut multi = MultiReporter::new();

    // Add console reporter
    multi = multi.with_reporter(ConsoleReporter::new(verbose));

    // Add JUnit reporter if enabled
    if config.report.junit {
        let junit_path = junit_override
            .unwrap_or_else(|| config.report.output_dir.join(&config.report.junit_file));
        multi = multi.with_reporter(JUnitReporter::new(junit_path));
    }

    multi
}
