//! Integration tests for the `offload run --override-image-id` escape hatch.
//!
//! These cover the non-modal provider guard at the CLI seam. The short-circuit
//! behavior of the modal provider itself is covered by unit tests in
//! `src/provider/modal.rs`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use assert_cmd::Command;
use offload::config::{
    CargoFrameworkConfig, Config, FrameworkConfig, GroupConfig, LocalProviderConfig, OffloadConfig,
    ProviderConfig, ReportConfig,
};
use predicates::prelude::*;
use tempfile::TempDir;

#[allow(deprecated)]
fn offload_cmd() -> anyhow::Result<Command> {
    Command::cargo_bin("offload").context("offload binary not found")
}

/// Write a local-provider config (cargo framework) to the given path.
fn write_local_config(config_path: &Path, output_dir: &Path) -> anyhow::Result<()> {
    let config = Config {
        offload: OffloadConfig {
            max_parallel: 1,
            test_timeout_secs: 300,
            working_dir: None,
            sandbox_project_root: Some(".".to_string()),
            sandbox_repo_root: None,
            sandbox_init_cmd: None,
            post_patch_cmd: None,
            impatiently_requeue_batches: true,
        },
        provider: ProviderConfig::Local(LocalProviderConfig::default()),
        framework: FrameworkConfig::Cargo(CargoFrameworkConfig::default()),
        groups: HashMap::from([("all".to_string(), GroupConfig::default())]),
        report: ReportConfig {
            output_dir: PathBuf::from(output_dir),
            junit: true,
            junit_file: "junit.xml".to_string(),
            download_globs: vec![],
            download_globs_failure_only: false,
        },
        checkpoint: None,
        history: None,
    };
    let content = toml::to_string_pretty(&config).context("failed to serialize config")?;
    fs::write(config_path, content).context("failed to write config")?;
    Ok(())
}

#[test]
fn override_image_id_rejected_for_non_modal_provider() -> anyhow::Result<()> {
    let tmp = TempDir::new()?;
    let output_dir = tmp.path().join("results");
    fs::create_dir_all(&output_dir)?;

    let config_path = tmp.path().join("offload.toml");
    write_local_config(&config_path, &output_dir)?;

    // The guard fires before any test discovery, so this is fast and offline.
    offload_cmd()?
        .args([
            "-c",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path"))?,
            "run",
            "--override-image-id",
            "im-x",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--override-image-id is only supported with the modal provider",
        ));
    Ok(())
}
