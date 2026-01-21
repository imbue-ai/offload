//! shotgun CLI - Flexible parallel test runner.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

use shotgun::config::{self, DiscoveryConfig, ProviderConfig};
use shotgun::discovery::{
    cargo::CargoDiscoverer, generic::GenericDiscoverer, pytest::PytestDiscoverer, TestDiscoverer,
};
use shotgun::executor::Orchestrator;
use shotgun::provider::{
    docker::DockerProvider, ondemand::{CommandSpawner, OnDemandProvider},
    process::ProcessProvider, remote::RemoteProvider, ssh::SshProvider, DynProviderWrapper,
    DynSandboxProvider,
};
use shotgun::report::{ConsoleReporter, JUnitReporter, MultiReporter, Reporter};

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
        /// Provider type (process, docker, ssh, ondemand, remote)
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
    let log_level = if cli.verbose { Level::DEBUG } else { Level::INFO };
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
        } => {
            run_tests(&cli.config, parallel, collect_only, junit, cli.verbose).await
        }
        Commands::Collect { format } => collect_tests(&cli.config, &format).await,
        Commands::Validate => validate_config(&cli.config),
        Commands::Init { provider, framework } => init_config(&provider, &framework),
    }
}

async fn run_tests(
    config_path: &PathBuf,
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

    // Create provider
    let provider: Arc<dyn DynSandboxProvider> = create_provider(&config.provider)?;
    info!("Using provider: {}", provider.name());

    // Create discoverer
    let discoverer: Arc<dyn TestDiscoverer> = create_discoverer(&config.discovery)?;
    info!("Using discoverer: {}", discoverer.name());

    if collect_only {
        // Just discover and print tests
        let tests = discoverer.discover(&[]).await?;
        println!("Discovered {} tests:", tests.len());
        for test in &tests {
            println!("  {}", test.id);
        }
        return Ok(());
    }

    // Create reporter
    let reporter = create_reporter(&config, junit_path, verbose);

    // Create and run orchestrator
    let orchestrator = Orchestrator::new(config, provider, discoverer, reporter);

    let result = orchestrator.run().await?;

    // Exit with appropriate code
    std::process::exit(result.exit_code());
}

async fn collect_tests(config_path: &PathBuf, format: &str) -> Result<()> {
    let config = config::load_config(config_path)?;
    let discoverer: Arc<dyn TestDiscoverer> = create_discoverer(&config.discovery)?;

    let tests = discoverer.discover(&[]).await?;

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

fn validate_config(config_path: &PathBuf) -> Result<()> {
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
                ProviderConfig::Ondemand(_) => "ondemand",
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
        "process" => r#"[provider]
type = "process"
working_dir = "."
shell = "/bin/sh""#,
        "docker" => r#"[provider]
type = "docker"
image = "python:3.11"
volumes = []
working_dir = "/workspace""#,
        "ssh" => r#"[provider]
type = "ssh"
hosts = ["localhost"]
user = "ubuntu"
port = 22
working_dir = "/home/ubuntu/workspace""#,
        "ondemand" => r#"[provider]
type = "ondemand"
# Command to spawn compute - must output JSON: {"instance_id": "...", "host": "...", "port": 22, "user": "..."}
spawn_command = "./scripts/spawn-instance.sh {id}"
# Command to destroy compute
destroy_command = "./scripts/destroy-instance.sh {instance_id}"
# SSH key for connecting
key_path = "~/.ssh/id_rsa"
working_dir = "/home/ubuntu/workspace"
health_check_timeout_secs = 120"#,
        "remote" => r#"[provider]
type = "remote"
# Your script that handles everything: spin up EC2/GCP/Fly, run tests, return results
# Test command is appended to this
execute_command = "./scripts/run-remote.sh"
# Optional: sync code before running
setup_command = "./scripts/sync-code.sh"
# Timeout for remote execution
timeout_secs = 3600"#,
        _ => {
            eprintln!("Unknown provider: {}. Use: process, docker, ssh, ondemand, remote", provider);
            std::process::exit(1);
        }
    };

    let discovery_config = match framework {
        "pytest" => r#"[discovery]
type = "pytest"
paths = ["tests"]
python = "python""#,
        "cargo" => r#"[discovery]
type = "cargo""#,
        "generic" => r#"[discovery]
type = "generic"
discover_command = "echo test1 test2"
run_command = "echo Running {tests}""#,
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

fn create_provider(config: &ProviderConfig) -> Result<Arc<dyn DynSandboxProvider>> {
    match config {
        ProviderConfig::Process(cfg) => {
            let provider = ProcessProvider::new(cfg.clone());
            Ok(Arc::new(DynProviderWrapper::new(provider)))
        }
        ProviderConfig::Docker(cfg) => {
            let provider = DockerProvider::new(cfg.clone())?;
            Ok(Arc::new(DynProviderWrapper::new(provider)))
        }
        ProviderConfig::Ssh(cfg) => {
            let provider = SshProvider::new(cfg.clone());
            Ok(Arc::new(DynProviderWrapper::new(provider)))
        }
        ProviderConfig::Ondemand(cfg) => {
            let spawner = CommandSpawner {
                spawn_command: cfg.spawn_command.clone(),
                destroy_command: cfg.destroy_command.clone(),
                key_path: cfg.key_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            };
            let env: Vec<(String, String)> = cfg.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let provider = OnDemandProvider::new(spawner, cfg.working_dir.clone())
                .with_env(env)
                .with_health_check_timeout(cfg.health_check_timeout_secs);
            Ok(Arc::new(DynProviderWrapper::new(provider)))
        }
        ProviderConfig::Remote(cfg) => {
            let remote_cfg = shotgun::provider::remote::RemoteProviderConfig {
                execute_command: cfg.execute_command.clone(),
                setup_command: cfg.setup_command.clone(),
                teardown_command: cfg.teardown_command.clone(),
                working_dir: cfg.working_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
                env: cfg.env.clone(),
                timeout_secs: cfg.timeout_secs,
            };
            let provider = RemoteProvider::new(remote_cfg);
            Ok(Arc::new(DynProviderWrapper::new(provider)))
        }
    }
}

fn create_discoverer(config: &DiscoveryConfig) -> Result<Arc<dyn TestDiscoverer>> {
    match config {
        DiscoveryConfig::Pytest(cfg) => Ok(Arc::new(PytestDiscoverer::new(cfg.clone()))),
        DiscoveryConfig::Cargo(cfg) => Ok(Arc::new(CargoDiscoverer::new(cfg.clone()))),
        DiscoveryConfig::Generic(cfg) => Ok(Arc::new(GenericDiscoverer::new(cfg.clone()))),
    }
}

fn create_reporter(
    config: &config::Config,
    junit_override: Option<PathBuf>,
    verbose: bool,
) -> Arc<dyn Reporter> {
    let mut multi = MultiReporter::new();

    // Add console reporter
    multi = multi.add(ConsoleReporter::new(verbose));

    // Add JUnit reporter if enabled
    if config.report.junit {
        let junit_path = junit_override.unwrap_or_else(|| {
            config.report.output_dir.join(&config.report.junit_file)
        });
        multi = multi.add(JUnitReporter::new(junit_path));
    }

    Arc::new(multi)
}
