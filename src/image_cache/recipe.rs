//! The recipe that, together with the checkpoint tree, identifies a cached base image.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bundled::SCRIPTS_DIR;
use crate::config::{Config, ProviderConfig};

/// Version of the Rust side of the image pipeline; bumped when export rules or argument
/// passing change what an unchanged recipe builds.
pub const RECIPE_FORMAT: u32 = 1;

/// Name of the bundled script that assembles Modal images.
const MODAL_SCRIPT: &str = "modal_sandbox.py";

/// Everything other than the checkpoint tree that determines the base image; stored in the
/// note entry and compared for equality on lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipe {
    pub format: u32,
    pub builder: Builder,
    pub copy_dirs: Vec<CopyDir>,
    pub sandbox_init_cmd: Option<String>,
    pub export_extras: Option<ExportExtras>,
}

/// What turns the recipe into an image, carrying only the fields that builder reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Builder {
    Modal {
        script: String,
        dockerfile: Option<String>,
        include_cwd: bool,
    },
    Default {
        prepare_command: String,
        working_dir: Option<String>,
    },
}

/// A directory added to the image, in the order the builder receives it; `content` digests a
/// source that lies outside the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyDir {
    pub source: String,
    pub target: String,
    pub content: Option<String>,
}

/// The gitignored but not dockerignored files that `export_tree` copies into the export from
/// the live tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportExtras {
    pub paths: Vec<String>,
    pub content: String,
}

impl Recipe {
    /// Derives the recipe from the loaded config; `None` when the provider builds no base image.
    pub fn from_config(
        config: &Config,
        runtime_copy_dirs: &[(PathBuf, PathBuf)],
    ) -> Option<Recipe> {
        let (builder, config_copy_dirs) = match &config.provider {
            ProviderConfig::Local(_) => return None,
            ProviderConfig::Modal(modal) => (
                Builder::Modal {
                    script: modal_script_digest()?,
                    dockerfile: modal.dockerfile.clone(),
                    include_cwd: modal.include_cwd,
                },
                &modal.copy_dirs,
            ),
            ProviderConfig::Default(default) => (
                Builder::Default {
                    prepare_command: default.prepare_command.clone()?,
                    working_dir: default
                        .working_dir
                        .as_ref()
                        .map(|dir| dir.display().to_string()),
                },
                &default.copy_dirs,
            ),
        };

        let copy_dirs = config_copy_dirs
            .iter()
            .map(|spec| CopyDir::from_spec(spec))
            .chain(runtime_copy_dirs.iter().map(|(source, target)| CopyDir {
                source: source.display().to_string(),
                target: target.display().to_string(),
                content: None,
            }))
            .collect();

        let sandbox_init_cmd = config
            .offload
            .sandbox_init_cmd
            .clone()
            .filter(|cmd| !cmd.is_empty());

        Some(Recipe {
            format: RECIPE_FORMAT,
            builder,
            copy_dirs,
            sandbox_init_cmd,
            export_extras: None,
        })
    }

    /// The note entry name for this recipe: a label derived from its JSON, never used for
    /// matching.
    pub fn name(&self) -> Result<String> {
        let json = serde_json::to_string(&serde_json::to_value(self)?)?;
        let digest = sha256_hex(json.as_bytes());
        Ok(format!("recipe:{}", &digest[..16]))
    }
}

impl CopyDir {
    /// Splits a config `local:remote` entry at its first colon; a missing colon leaves `target`
    /// empty so the recipe still records the entry.
    fn from_spec(spec: &str) -> CopyDir {
        let (source, target) = spec.split_once(':').unwrap_or((spec, ""));
        CopyDir {
            source: source.to_string(),
            target: target.to_string(),
            content: None,
        }
    }
}

/// SHA-256 of the bundled Modal script, which assembles the image; `None` when the script is
/// not embedded.
fn modal_script_digest() -> Option<String> {
    SCRIPTS_DIR
        .get_file(MODAL_SCRIPT)
        .map(|file| sha256_hex(file.contents()))
}

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;
    use crate::config::load_config_str;

    const MODAL_PROVIDER: &str = r#"
        type = "modal"
        dockerfile = "system/Dockerfile"
        copy_dirs = ["data:/data", "nested:/a:b", "no-colon"]
    "#;

    const DEFAULT_PROVIDER: &str = r#"
        type = "default"
        prepare_command = "./prepare.sh"
        create_command = "create"
        exec_command = "exec {command}"
        destroy_command = "destroy"
        working_dir = "ops"
    "#;

    const DEFAULT_PROVIDER_WITHOUT_PREPARE: &str = r#"
        type = "default"
        create_command = "create"
        exec_command = "exec {command}"
        destroy_command = "destroy"
    "#;

    fn config_toml(offload_extra: &str, provider: &str) -> String {
        format!(
            r#"
            [offload]
            sandbox_project_root = "/app"
            {offload_extra}

            [provider]
            {provider}

            [framework]
            type = "pytest"

            [groups.all]
            retry_count = 0
            "#
        )
    }

    fn copy_dir(source: &str, target: &str) -> CopyDir {
        CopyDir {
            source: source.to_string(),
            target: target.to_string(),
            content: None,
        }
    }

    #[test]
    fn fixed_recipe_yields_fixed_name() -> Result<(), Box<dyn std::error::Error>> {
        let recipe = Recipe {
            format: RECIPE_FORMAT,
            builder: Builder::Modal {
                script: "9b1d".repeat(16),
                dockerfile: Some("system/Dockerfile".to_string()),
                include_cwd: false,
            },
            copy_dirs: vec![],
            sandbox_init_cmd: Some("bash system/scripts/offload_sandbox_init.sh".to_string()),
            export_extras: None,
        };
        assert_eq!(recipe.name()?, "recipe:c0cd23c48b675ba1");
        Ok(())
    }

    #[test]
    fn modal_recipe_serializes_to_the_design_shape() -> Result<(), Box<dyn std::error::Error>> {
        let recipe = Recipe {
            format: 1,
            builder: Builder::Modal {
                script: "9b1d".to_string(),
                dockerfile: Some("system/Dockerfile".to_string()),
                include_cwd: false,
            },
            copy_dirs: vec![copy_dir("data", "/data")],
            sandbox_init_cmd: Some("bash init.sh".to_string()),
            export_extras: Some(ExportExtras {
                paths: vec!["build/out".to_string()],
                content: "c0ffee".to_string(),
            }),
        };
        let expected = json!({
            "format": 1,
            "builder": {
                "modal": {
                    "script": "9b1d",
                    "dockerfile": "system/Dockerfile",
                    "include_cwd": false
                }
            },
            "copy_dirs": [{"source": "data", "target": "/data", "content": null}],
            "sandbox_init_cmd": "bash init.sh",
            "export_extras": {"paths": ["build/out"], "content": "c0ffee"}
        });
        assert_eq!(serde_json::to_value(&recipe)?, expected);
        assert_eq!(serde_json::from_value::<Recipe>(expected)?, recipe);
        Ok(())
    }

    #[test]
    fn default_recipe_serializes_to_the_design_shape() -> Result<(), Box<dyn std::error::Error>> {
        let recipe = Recipe {
            format: 1,
            builder: Builder::Default {
                prepare_command: "./prepare.sh".to_string(),
                working_dir: None,
            },
            copy_dirs: vec![],
            sandbox_init_cmd: None,
            export_extras: None,
        };
        let expected = json!({
            "format": 1,
            "builder": {
                "default": {"prepare_command": "./prepare.sh", "working_dir": null}
            },
            "copy_dirs": [],
            "sandbox_init_cmd": null,
            "export_extras": null
        });
        assert_eq!(serde_json::to_value(&recipe)?, expected);
        assert_eq!(serde_json::from_value::<Recipe>(expected)?, recipe);
        Ok(())
    }

    #[test]
    fn from_config_fills_the_modal_builder() -> Result<(), Box<dyn std::error::Error>> {
        let toml = config_toml(r#"sandbox_init_cmd = "bash init.sh""#, MODAL_PROVIDER);
        let config = load_config_str(&toml)?;
        let recipe = Recipe::from_config(&config, &[]).ok_or("expected a recipe")?;
        assert_eq!(recipe.format, RECIPE_FORMAT);
        assert_eq!(
            recipe.builder,
            Builder::Modal {
                script: modal_script_digest().ok_or("bundled script missing")?,
                dockerfile: Some("system/Dockerfile".to_string()),
                include_cwd: false,
            }
        );
        assert_eq!(recipe.sandbox_init_cmd.as_deref(), Some("bash init.sh"));
        assert_eq!(recipe.export_extras, None);
        Ok(())
    }

    #[test]
    fn from_config_fills_the_default_builder() -> Result<(), Box<dyn std::error::Error>> {
        let config = load_config_str(&config_toml("", DEFAULT_PROVIDER))?;
        let recipe = Recipe::from_config(&config, &[]).ok_or("expected a recipe")?;
        assert_eq!(
            recipe.builder,
            Builder::Default {
                prepare_command: "./prepare.sh".to_string(),
                working_dir: Some("ops".to_string()),
            }
        );
        assert_eq!(recipe.copy_dirs, vec![]);
        assert_eq!(recipe.sandbox_init_cmd, None);
        Ok(())
    }

    #[test]
    fn empty_sandbox_init_cmd_normalizes_to_none() -> Result<(), Box<dyn std::error::Error>> {
        let config = load_config_str(&config_toml(r#"sandbox_init_cmd = """#, MODAL_PROVIDER))?;
        let recipe = Recipe::from_config(&config, &[]).ok_or("expected a recipe")?;
        assert_eq!(recipe.sandbox_init_cmd, None);
        Ok(())
    }

    #[test]
    fn copy_dirs_are_config_entries_then_runtime_entries() -> Result<(), Box<dyn std::error::Error>>
    {
        let config = load_config_str(&config_toml("", MODAL_PROVIDER))?;
        let runtime = [(PathBuf::from("/tmp/extra"), PathBuf::from("/extra"))];
        let recipe = Recipe::from_config(&config, &runtime).ok_or("expected a recipe")?;
        assert_eq!(
            recipe.copy_dirs,
            vec![
                copy_dir("data", "/data"),
                copy_dir("nested", "/a:b"),
                copy_dir("no-colon", ""),
                copy_dir("/tmp/extra", "/extra"),
            ]
        );
        Ok(())
    }

    #[test]
    fn from_config_is_none_for_the_local_provider() -> Result<(), Box<dyn std::error::Error>> {
        let config = load_config_str(&config_toml("", r#"type = "local""#))?;
        assert_eq!(Recipe::from_config(&config, &[]), None);
        Ok(())
    }

    #[test]
    fn from_config_is_none_for_default_without_prepare_command()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = load_config_str(&config_toml("", DEFAULT_PROVIDER_WITHOUT_PREPARE))?;
        assert_eq!(Recipe::from_config(&config, &[]), None);
        Ok(())
    }

    #[test]
    fn modal_script_digest_is_lowercase_hex_and_stable() -> Result<(), Box<dyn std::error::Error>> {
        let first = modal_script_digest().ok_or("bundled script missing")?;
        assert_eq!(first.len(), 64);
        assert!(
            first
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
        assert_eq!(
            modal_script_digest().ok_or("bundled script missing")?,
            first
        );
        Ok(())
    }
}
