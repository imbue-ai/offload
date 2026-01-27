//! shotgun CLI - Flexible parallel test runner.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

use shotgun::config::{self, DiscoveryConfig, ProviderConfig};
use shotgun::discovery::{
    TestDiscoverer, cargo::CargoDiscoverer, generic::GenericDiscoverer, pytest::PytestDiscoverer,
};
use shotgun::executor::Orchestrator;
use shotgun::provider::{
    SandboxProvider, docker::DockerProvider, process::ProcessProvider, remote::ConnectorProvider,
    ssh::SshProvider,
};
use shotgun::report::{ConsoleReporter, JUnitReporter, MultiReporter};

#[derive(Parser)]
#[command(name = "shotgun")]
#[command(about = "Flexible parallel test runner", long_about = None)]
#[command(version)]
struct Cli {
    /// Configuration file path
    #[arg(short, long, default_value = "shotgun.toml")]
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
        /// Provider type (process, docker, ssh, remote)
        #[arg(short, long, default_value = "process")]
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
        config.shotgun.max_parallel = parallel;
    }

    info!("Loaded configuration from {}", config_path.display());

    // Match on provider and discoverer to get concrete types
    match (&config.provider, &config.discovery) {
        (ProviderConfig::Process(p_cfg), DiscoveryConfig::Pytest(d_cfg)) => {
            let provider = ProcessProvider::new(p_cfg.clone());
            let discoverer = PytestDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Process(p_cfg), DiscoveryConfig::Cargo(d_cfg)) => {
            let provider = ProcessProvider::new(p_cfg.clone());
            let discoverer = CargoDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Process(p_cfg), DiscoveryConfig::Generic(d_cfg)) => {
            let provider = ProcessProvider::new(p_cfg.clone());
            let discoverer = GenericDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Docker(p_cfg), DiscoveryConfig::Pytest(d_cfg)) => {
            let provider = DockerProvider::new(p_cfg.clone())?;
            let discoverer = PytestDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Docker(p_cfg), DiscoveryConfig::Cargo(d_cfg)) => {
            let provider = DockerProvider::new(p_cfg.clone())?;
            let discoverer = CargoDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Docker(p_cfg), DiscoveryConfig::Generic(d_cfg)) => {
            let provider = DockerProvider::new(p_cfg.clone())?;
            let discoverer = GenericDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Ssh(p_cfg), DiscoveryConfig::Pytest(d_cfg)) => {
            let provider = SshProvider::new(p_cfg.clone());
            let discoverer = PytestDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Ssh(p_cfg), DiscoveryConfig::Cargo(d_cfg)) => {
            let provider = SshProvider::new(p_cfg.clone());
            let discoverer = CargoDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Ssh(p_cfg), DiscoveryConfig::Generic(d_cfg)) => {
            let provider = SshProvider::new(p_cfg.clone());
            let discoverer = GenericDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Remote(p_cfg), DiscoveryConfig::Pytest(d_cfg)) => {
            let provider = ConnectorProvider::from_config(p_cfg.clone());
            let discoverer = PytestDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Remote(p_cfg), DiscoveryConfig::Cargo(d_cfg)) => {
            let provider = ConnectorProvider::from_config(p_cfg.clone());
            let discoverer = CargoDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
        (ProviderConfig::Remote(p_cfg), DiscoveryConfig::Generic(d_cfg)) => {
            let provider = ConnectorProvider::from_config(p_cfg.clone());
            let discoverer = GenericDiscoverer::new(d_cfg.clone());
            run_with(
                config,
                provider,
                discoverer,
                collect_only,
                junit_path,
                verbose,
            )
            .await
        }
    }
}

async fn run_with<P, D>(
    config: config::Config,
    provider: P,
    discoverer: D,
    collect_only: bool,
    junit_path: Option<PathBuf>,
    verbose: bool,
) -> Result<()>
where
    P: SandboxProvider + 'static,
    D: TestDiscoverer + 'static,
{
    info!("Using provider: {}", provider.name());
    info!("Using discoverer: {}", discoverer.name());

    if collect_only {
        let tests = discoverer.discover(&[]).await?;
        println!("Discovered {} tests:", tests.len());
        for test in &tests {
            println!("  {}", test.id);
        }
        return Ok(());
    }

    let reporter = create_reporter(&config, junit_path, verbose);
    let orchestrator = Orchestrator::new(config, provider, discoverer, reporter);

    let result = orchestrator.run().await?;
    std::process::exit(result.exit_code());
}

async fn collect_tests(config_path: &Path, format: &str) -> Result<()> {
    let config = config::load_config(config_path)?;

    let tests = match &config.discovery {
        DiscoveryConfig::Pytest(cfg) => PytestDiscoverer::new(cfg.clone()).discover(&[]).await?,
        DiscoveryConfig::Cargo(cfg) => CargoDiscoverer::new(cfg.clone()).discover(&[]).await?,
        DiscoveryConfig::Generic(cfg) => GenericDiscoverer::new(cfg.clone()).discover(&[]).await?,
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

    Ok(())
}

fn validate_config(config_path: &Path) -> Result<()> {
    match config::load_config(config_path) {
        Ok(config) => {
            println!("Configuration is valid!");
            println!();
            println!("Settings:");
            println!("  Max parallel: {}", config.shotgun.max_parallel);
            println!("  Test timeout: {}s", config.shotgun.test_timeout_secs);
            println!("  Retry count: {}", config.shotgun.retry_count);

            let provider_name = match &config.provider {
                ProviderConfig::Process(_) => "process",
                ProviderConfig::Docker(_) => "docker",
                ProviderConfig::Ssh(_) => "ssh",
                ProviderConfig::Remote(_) => "remote",
            };
            println!("  Provider: {}", provider_name);

            let discoverer_name = match &config.discovery {
                DiscoveryConfig::Pytest(_) => "pytest",
                DiscoveryConfig::Cargo(_) => "cargo",
                DiscoveryConfig::Generic(_) => "generic",
            };
            println!("  Discovery: {}", discoverer_name);

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
        "process" => {
            r#"[provider]
type = "process"
working_dir = "."
shell = "/bin/sh""#
        }
        "docker" => {
            r#"[provider]
type = "docker"
image = "python:3.11"
volumes = []
working_dir = "/workspace""#
        }
        "ssh" => {
            r#"[provider]
type = "ssh"
hosts = ["localhost"]
user = "ubuntu"
port = 22
working_dir = "/home/ubuntu/workspace""#
        }
        "remote" => {
            r#"[provider]
type = "remote"
# Your script that handles everything: spin up cloud compute, run tests, return results
# Test command is appended to this
execute_command = "./scripts/run-remote.sh"
# Optional: sync code before running
setup_command = "./scripts/sync-code.sh"
# Timeout for remote execution
timeout_secs = 3600"#
        }
        _ => {
            eprintln!(
                "Unknown provider: {}. Use: process, docker, ssh, remote",
                provider
            );
            std::process::exit(1);
        }
    };

    let discovery_config = match framework {
        "pytest" => {
            r#"[discovery]
type = "pytest"
paths = ["tests"]
python = "python""#
        }
        "cargo" => {
            r#"[discovery]
type = "cargo""#
        }
        "generic" => {
            r#"[discovery]
type = "generic"
discover_command = "echo test1 test2"
run_command = "echo Running {tests}""#
        }
        _ => {
            eprintln!(
                "Unknown framework: {}. Use: pytest, cargo, generic",
                framework
            );
            std::process::exit(1);
        }
    };

    let config = format!(
        r#"# shotgun configuration file

[shotgun]
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
        provider_config, discovery_config
    );

    let path = PathBuf::from("shotgun.toml");
    if path.exists() {
        eprintln!("shotgun.toml already exists. Remove it first or edit manually.");
        std::process::exit(1);
    }

    std::fs::write(&path, config)?;
    println!("Created shotgun.toml");
    println!();
    println!("Edit the configuration as needed, then run:");
    println!("  shotgun run");

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
