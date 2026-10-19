//! The recipe that, together with the checkpoint tree, identifies a cached base image.

use std::fs::{File, Metadata};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{copy_origin, path_under_root};
use crate::bundled::SCRIPTS_DIR;
use crate::config::{Config, ProviderConfig};
use crate::git;

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
        /// The repository-relative directory `include_cwd` adds at `/app` (empty at the root),
        /// or `None` when it adds nothing: the invoking directory enters the recipe as what it
        /// selects, so one config run from two directories cannot share an image.
        include_dir: Option<String>,
    },
    Default {
        prepare_command: String,
        working_dir: Option<String>,
    },
}

/// A directory added to the image, in the order the builder receives it; `source` is the
/// repository-relative path of a source inside the repository, which the build reads from the
/// export, and the path as written for one outside; `content` digests an outside source, or is
/// [`ABSENT_SOURCE`] when that source is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyDir {
    pub source: String,
    pub target: String,
    pub content: Option<String>,
}

/// The gitignored but not dockerignored files that `export_tree` copies into the export from
/// the live tree, with one digest of their content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportExtras {
    pub paths: Vec<String>,
    pub content: String,
}

/// Recorded as a copy source's `content` when it lies outside the repository and is missing, so
/// its later appearance is a cache miss.
pub const ABSENT_SOURCE: &str = "absent";

impl Recipe {
    /// Derives the recipe from the loaded config without touching the disk: `cwd_rel`, the
    /// invoking directory relative to the repository root, becomes the directory `include_cwd`
    /// adds, while copy sources stay as written and every live-tree digest is left unset for
    /// the caller to fill. `None` when the provider builds no base image.
    pub fn from_config(
        config: &Config,
        cwd_rel: &Path,
        runtime_copy_dirs: &[(PathBuf, PathBuf)],
    ) -> Option<Recipe> {
        let (builder, config_copy_dirs) = match &config.provider {
            ProviderConfig::Local(_) => return None,
            ProviderConfig::Modal(modal) => (
                Builder::Modal {
                    script: modal_script_digest()?,
                    dockerfile: modal.dockerfile.clone(),
                    include_dir: modal.include_cwd.then(|| repo_relative(cwd_rel)),
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

/// The recipe the current invocation looks up or builds under: [`Recipe::from_config`] for the
/// invoking directory, with each copy source inside the repository rewritten to the
/// repository-relative path the build reads, and digests of the two live-tree inputs a base
/// build still reads, copy sources outside the repository and the gitignored files
/// `export_tree` copies in, so a change in what enters the image is a cache miss. `None` when
/// the provider builds no base image.
pub(crate) async fn current_recipe(
    repo: &Path,
    config: &Config,
    copy_dirs: &[(PathBuf, PathBuf)],
) -> Result<Option<Recipe>> {
    let cwd_rel = git::path_within_repo(repo).await?;
    let Some(mut recipe) = Recipe::from_config(config, &cwd_rel, copy_dirs) else {
        return Ok(None);
    };
    let root = git::repo_root(repo).await?;
    // `repo` is the invoking directory, so this is the build's `root.join(cwd_rel).join(origin)`.
    let origin = repo.join(copy_origin(&config.provider));
    tokio::task::spawn_blocking(move || -> Result<Option<Recipe>> {
        for dir in &mut recipe.copy_dirs {
            let live = origin.join(&dir.source);
            record_copy_source(&root, &live, dir)?;
        }
        recipe.export_extras = export_extras(&root)?;
        Ok(Some(recipe))
    })
    .await?
}

/// What the recipe records for `dir`, whose source the build reads at `live`: inside the
/// repository the source becomes its repository-relative path, the one the build reads in the
/// export, so the spelling and invoking directory that named it drop out; outside it the source
/// stays as written and `content` digests the live directory the build reads, or is
/// [`ABSENT_SOURCE`] when that directory is missing so its later appearance is a cache miss.
fn record_copy_source(root: &Path, live: &Path, dir: &mut CopyDir) -> Result<()> {
    if let Some(inside) = path_under_root(root, live) {
        dir.source = repo_relative(&inside);
        return Ok(());
    }
    dir.content = Some(if live.exists() {
        digest_dir(live)?
    } else {
        ABSENT_SOURCE.to_string()
    });
    Ok(())
}

/// `path` as the recipe spells a repository-relative path: its components alone, without the
/// trailing separator git leaves on `rev-parse --show-prefix` or an interior `.`, so one
/// directory cannot yield two recipes.
fn repo_relative(path: &Path) -> String {
    path.components().collect::<PathBuf>().display().to_string()
}

/// The files `export_tree` copies in from the live tree with their digest; `None` when there
/// are none, so a repository whose `.dockerignore` covers its `.gitignore` pays nothing.
fn export_extras(root: &Path) -> Result<Option<ExportExtras>> {
    let paths = git::export_extra_paths(root)?;
    if paths.is_empty() {
        return Ok(None);
    }
    let content = digest_files(root, &paths)?;
    Ok(Some(ExportExtras { paths, content }))
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

/// Hex SHA-256 over `paths` under `root` in the given order, each as its path, a newline, its
/// permission bits in unpadded octal, a newline, its decimal length, a newline, and its bytes,
/// so a rename, chmod, resize, or edit changes the digest.
fn digest_files(root: &Path, paths: &[String]) -> Result<String> {
    let mut hasher = Sha256::new();
    for path in paths {
        let file_path = root.join(path);
        let mut file = File::open(&file_path)
            .with_context(|| format!("failed to open {}", file_path.display()))?;
        let metadata = file.metadata()?;
        let mode = permission_bits(&metadata);
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
        hasher.update(format!("{mode:o}").as_bytes());
        hasher.update(b"\n");
        hasher.update(metadata.len().to_string().as_bytes());
        hasher.update(b"\n");
        std::io::copy(&mut file, &mut hasher)
            .with_context(|| format!("failed to read {}", file_path.display()))?;
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The permission bits the builder's tar upload records for a file, `st_mode & 0o7777`, so
/// the digest follows a chmod the way the image does.
#[cfg(unix)]
fn permission_bits(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

/// The permission bits the builder's tar upload records for a file (non-unix: none).
#[cfg(not(unix))]
fn permission_bits(_metadata: &Metadata) -> u32 {
    0
}

/// [`digest_files`] over every file under `dir`, walked in sorted path order and following
/// symlinks, for a copy source the export holds no copy of.
fn digest_dir(dir: &Path) -> Result<String> {
    let mut paths = Vec::new();
    collect_files(dir, Path::new(""), &mut paths)?;
    paths.sort();
    digest_files(dir, &paths)
}

/// Appends every file under `root.join(prefix)` to `paths`, relative to `root`.
fn collect_files(root: &Path, prefix: &Path, paths: &mut Vec<String>) -> Result<()> {
    let dir = root.join(prefix);
    let entries =
        std::fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let relative = prefix.join(entry.file_name());
        if entry.path().is_dir() {
            collect_files(root, &relative, paths)?;
        } else {
            paths.push(relative.to_string_lossy().into_owned());
        }
    }
    Ok(())
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
    use crate::config::{ModalProviderConfig, load_config_str};
    use crate::image_cache::BuildContext;
    use crate::image_cache::test_support::{
        git_cmd, init_repo_in, init_temp_repo, minimal_modal_config,
    };

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
                include_dir: None,
            },
            copy_dirs: vec![],
            sandbox_init_cmd: Some("bash system/scripts/offload_sandbox_init.sh".to_string()),
            export_extras: None,
        };
        assert_eq!(recipe.name()?, "recipe:ba79b860a7dfd4bd");
        Ok(())
    }

    #[test]
    fn modal_recipe_serializes_to_the_design_shape() -> Result<(), Box<dyn std::error::Error>> {
        let recipe = Recipe {
            format: 1,
            builder: Builder::Modal {
                script: "9b1d".to_string(),
                dockerfile: Some("system/Dockerfile".to_string()),
                include_dir: None,
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
                    "include_dir": null
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
        let recipe = Recipe::from_config(&config, Path::new(""), &[]).ok_or("expected a recipe")?;
        assert_eq!(recipe.format, RECIPE_FORMAT);
        assert_eq!(
            recipe.builder,
            Builder::Modal {
                script: modal_script_digest().ok_or("bundled script missing")?,
                dockerfile: Some("system/Dockerfile".to_string()),
                include_dir: None,
            }
        );
        assert_eq!(recipe.sandbox_init_cmd.as_deref(), Some("bash init.sh"));
        assert_eq!(recipe.export_extras, None);
        Ok(())
    }

    /// Helper: the directory the recipe's Modal builder adds at `/app`.
    fn included_dir(recipe: &Recipe) -> Result<Option<&str>> {
        match &recipe.builder {
            Builder::Modal { include_dir, .. } => Ok(include_dir.as_deref()),
            Builder::Default { .. } => anyhow::bail!("expected the Modal builder"),
        }
    }

    #[test]
    fn from_config_records_the_included_directory_relative_to_the_root() -> Result<()> {
        let including_cwd = load_config_str(&config_toml(
            "",
            &format!("{MODAL_PROVIDER}include_cwd = true"),
        ))?;
        let from_root =
            Recipe::from_config(&including_cwd, Path::new(""), &[]).context("expected a recipe")?;
        assert_eq!(included_dir(&from_root)?, Some(""));
        // git spells the prefix with a trailing slash; the recipe stores the directory alone.
        let from_sub = Recipe::from_config(&including_cwd, Path::new("sub/dir/"), &[])
            .context("expected a recipe")?;
        assert_eq!(included_dir(&from_sub)?, Some("sub/dir"));
        let config = load_config_str(&config_toml("", MODAL_PROVIDER))?;
        let without =
            Recipe::from_config(&config, Path::new("sub"), &[]).context("expected a recipe")?;
        assert_eq!(included_dir(&without)?, None);
        Ok(())
    }

    #[test]
    fn from_config_fills_the_default_builder() -> Result<(), Box<dyn std::error::Error>> {
        let config = load_config_str(&config_toml("", DEFAULT_PROVIDER))?;
        let recipe = Recipe::from_config(&config, Path::new(""), &[]).ok_or("expected a recipe")?;
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
        let recipe = Recipe::from_config(&config, Path::new(""), &[]).ok_or("expected a recipe")?;
        assert_eq!(recipe.sandbox_init_cmd, None);
        Ok(())
    }

    #[test]
    fn copy_dirs_are_config_entries_then_runtime_entries() -> Result<(), Box<dyn std::error::Error>>
    {
        let config = load_config_str(&config_toml("", MODAL_PROVIDER))?;
        let runtime = [(PathBuf::from("/tmp/extra"), PathBuf::from("/extra"))];
        let recipe =
            Recipe::from_config(&config, Path::new(""), &runtime).ok_or("expected a recipe")?;
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
        assert_eq!(Recipe::from_config(&config, Path::new(""), &[]), None);
        Ok(())
    }

    #[test]
    fn from_config_is_none_for_default_without_prepare_command()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = load_config_str(&config_toml("", DEFAULT_PROVIDER_WITHOUT_PREPARE))?;
        assert_eq!(Recipe::from_config(&config, Path::new(""), &[]), None);
        Ok(())
    }

    /// Helper: `chmod` `path` to `mode`.
    fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    #[test]
    fn digest_files_covers_path_mode_length_and_bytes_in_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("a"), "x")?;
        set_mode(&dir.path().join("a"), 0o644)?;
        std::fs::create_dir(dir.path().join("b"))?;
        std::fs::write(dir.path().join("b/c"), "yz")?;
        set_mode(&dir.path().join("b/c"), 0o755)?;
        let paths = ["a".to_string(), "b/c".to_string()];
        assert_eq!(
            digest_files(dir.path(), &paths)?,
            sha256_hex(b"a\n644\n1\nxb/c\n755\n2\nyz")
        );
        assert_eq!(digest_dir(dir.path())?, digest_files(dir.path(), &paths)?);
        let reversed = ["b/c".to_string(), "a".to_string()];
        assert_ne!(
            digest_files(dir.path(), &reversed)?,
            digest_files(dir.path(), &paths)?
        );
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

    /// Helper: a Modal config with the given `local:remote` copy dirs.
    fn modal_config_with_copy_dirs(copy_dirs: Vec<String>) -> Config {
        let mut config = minimal_modal_config();
        config.provider = ProviderConfig::Modal(ModalProviderConfig {
            dockerfile: Some("Dockerfile".to_string()),
            copy_dirs,
            ..ModalProviderConfig::default()
        });
        config
    }

    /// Helper: a Modal config whose `include_cwd` adds the invoking directory at `/app`.
    fn modal_config_including_cwd() -> Config {
        let mut config = minimal_modal_config();
        config.provider = ProviderConfig::Modal(ModalProviderConfig {
            dockerfile: Some("Dockerfile".to_string()),
            include_cwd: true,
            ..ModalProviderConfig::default()
        });
        config
    }

    /// Helper: a default-provider config whose `prepare_command` runs in `working_dir` with one
    /// `local:remote` copy dir.
    fn default_config_in(working_dir: &Path, copy_dir: &str) -> Result<Config> {
        load_config_str(&format!(
            r#"
            [offload]
            sandbox_project_root = "/app"

            [provider]
            type = "default"
            prepare_command = "./prepare.sh"
            create_command = "create"
            exec_command = "exec {{command}}"
            destroy_command = "destroy"
            working_dir = "{}"
            copy_dirs = ["{}"]

            [framework]
            type = "pytest"

            [groups.all]
            retry_count = 0
            "#,
            working_dir.display(),
            copy_dir
        ))
    }

    /// Helper: commit a `.gitignore` for tarballs and a `.dockerignore` for logs.
    fn commit_ignore_files(dir: &Path) -> Result<()> {
        std::fs::write(dir.join(".gitignore"), "*.tar.gz\n")?;
        std::fs::write(dir.join(".dockerignore"), "*.log\n")?;
        git_cmd(dir, &["add", ".gitignore", ".dockerignore"])?;
        git_cmd(dir, &["commit", "-m", "add ignore files"])?;
        Ok(())
    }

    /// Helper: the recipe `offload build` would look up from `dir` with `config`.
    async fn current_recipe_in(dir: &Path, config: &Config) -> Result<Recipe> {
        current_recipe(dir, config, &[])
            .await?
            .context("expected a recipe")
    }

    /// Helper: the `content` recorded for the recipe's first copy dir.
    fn first_copy_content(recipe: &Recipe) -> Result<Option<&str>> {
        let first = recipe.copy_dirs.first().context("expected a copy dir")?;
        Ok(first.content.as_deref())
    }

    #[tokio::test]
    async fn test_current_recipe_export_extras_are_the_gitignored_files_the_export_copies()
    -> Result<()> {
        let dir = init_temp_repo()?;
        commit_ignore_files(dir.path())?;
        std::fs::write(dir.path().join("build.tar.gz"), "artifact")?;
        std::fs::write(dir.path().join("debug.log"), "dockerignored")?;
        let recipe = current_recipe_in(dir.path(), &minimal_modal_config()).await?;
        let extras = recipe.export_extras.context("expected export extras")?;
        assert_eq!(extras.paths, vec!["build.tar.gz"]);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_export_extras_is_none_without_gitignored_files() -> Result<()> {
        let dir = init_temp_repo()?;
        let recipe = current_recipe_in(dir.path(), &minimal_modal_config()).await?;
        assert_eq!(recipe.export_extras, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_name_changes_when_an_export_extra_changes() -> Result<()> {
        let dir = init_temp_repo()?;
        commit_ignore_files(dir.path())?;
        std::fs::write(dir.path().join("build.tar.gz"), "artifact")?;
        let config = minimal_modal_config();
        let before = current_recipe_in(dir.path(), &config).await?;
        assert_eq!(current_recipe_in(dir.path(), &config).await?, before);
        std::fs::write(dir.path().join("build.tar.gz"), "artifact v2")?;
        let after = current_recipe_in(dir.path(), &config).await?;
        assert_eq!(
            after.export_extras.as_ref().map(|extras| &extras.paths),
            before.export_extras.as_ref().map(|extras| &extras.paths)
        );
        assert_ne!(after.name()?, before.name()?);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_name_changes_when_an_export_extra_mode_changes() -> Result<()> {
        let dir = init_temp_repo()?;
        commit_ignore_files(dir.path())?;
        let artifact = dir.path().join("build.tar.gz");
        std::fs::write(&artifact, "artifact")?;
        let config = minimal_modal_config();
        let before = current_recipe_in(dir.path(), &config).await?;
        set_mode(&artifact, 0o755)?;
        let after = current_recipe_in(dir.path(), &config).await?;
        assert_eq!(
            after.export_extras.as_ref().map(|extras| &extras.paths),
            before.export_extras.as_ref().map(|extras| &extras.paths)
        );
        assert_ne!(after.name()?, before.name()?);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_digests_a_copy_source_outside_the_repository() -> Result<()> {
        let dir = init_temp_repo()?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("model.bin"), "weights")?;
        let outside_source = outside.path().display().to_string();
        let config = modal_config_with_copy_dirs(vec![format!("{outside_source}:/data")]);
        let before = current_recipe_in(dir.path(), &config).await?;
        assert_eq!(
            before.copy_dirs.first().map(|dir| dir.source.as_str()),
            Some(outside_source.as_str())
        );
        let digest = first_copy_content(&before)?.context("expected a digest")?;
        assert_eq!(digest.len(), 64);
        assert_eq!(current_recipe_in(dir.path(), &config).await?, before);
        std::fs::write(outside.path().join("model.bin"), "weights v2")?;
        let after = current_recipe_in(dir.path(), &config).await?;
        assert_ne!(first_copy_content(&after)?, Some(digest));
        assert_ne!(after.name()?, before.name()?);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_digests_the_mode_of_a_copy_source_outside_the_repository()
    -> Result<()> {
        let dir = init_temp_repo()?;
        let outside = tempfile::tempdir()?;
        let model = outside.path().join("model.bin");
        std::fs::write(&model, "weights")?;
        let config =
            modal_config_with_copy_dirs(vec![format!("{}:/data", outside.path().display())]);
        let before = current_recipe_in(dir.path(), &config).await?;
        set_mode(&model, 0o755)?;
        let after = current_recipe_in(dir.path(), &config).await?;
        assert_ne!(first_copy_content(&after)?, first_copy_content(&before)?);
        assert_ne!(after.name()?, before.name()?);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_records_an_inside_copy_source_relative_to_the_root() -> Result<()>
    {
        let dir = init_temp_repo()?;
        std::fs::create_dir(dir.path().join("data"))?;
        std::fs::write(dir.path().join("data/seed.txt"), "seed")?;
        let absolute = dir.path().join("data").display().to_string();
        for source in ["data", absolute.as_str()] {
            let config = modal_config_with_copy_dirs(vec![format!("{source}:/data")]);
            let recipe = current_recipe_in(dir.path(), &config).await?;
            let first = recipe.copy_dirs.first().context("expected a copy dir")?;
            assert_eq!(first.source, "data", "source {source}");
            assert_eq!(first.content, None, "source {source}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_records_a_relative_inside_source_under_the_invoking_directory()
    -> Result<()> {
        let dir = init_temp_repo()?;
        std::fs::create_dir_all(dir.path().join("sub/data"))?;
        let config = modal_config_with_copy_dirs(vec!["./data:/data".to_string()]);
        let recipe = current_recipe_in(&dir.path().join("sub"), &config).await?;
        let first = recipe.copy_dirs.first().context("expected a copy dir")?;
        assert_eq!(first.source, "sub/data");
        assert_eq!(first.content, None);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_differs_between_invoking_directories_when_include_cwd_is_set()
    -> Result<()> {
        let dir = init_temp_repo()?;
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub)?;
        let config = modal_config_including_cwd();
        let from_root = current_recipe_in(dir.path(), &config).await?;
        let from_sub = current_recipe_in(&sub, &config).await?;
        assert_eq!(included_dir(&from_root)?, Some(""));
        assert_eq!(included_dir(&from_sub)?, Some("sub"));
        assert_ne!(from_root, from_sub);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_is_shared_between_invoking_directories_without_cwd_inputs()
    -> Result<()> {
        let dir = init_temp_repo()?;
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub)?;
        let config = minimal_modal_config();
        assert_eq!(
            current_recipe_in(&sub, &config).await?,
            current_recipe_in(dir.path(), &config).await?
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_marks_a_missing_copy_source_absent() -> Result<()> {
        let dir = init_temp_repo()?;
        let outside = tempfile::tempdir()?;
        let missing = outside.path().join("gone");
        let config = modal_config_with_copy_dirs(vec![format!("{}:/data", missing.display())]);
        let before = current_recipe_in(dir.path(), &config).await?;
        assert_eq!(first_copy_content(&before)?, Some("absent"));
        std::fs::create_dir(&missing)?;
        std::fs::write(missing.join("late.txt"), "late")?;
        let after = current_recipe_in(dir.path(), &config).await?;
        assert_ne!(after.name()?, before.name()?);
        Ok(())
    }

    #[tokio::test]
    async fn test_current_recipe_resolves_default_provider_sources_against_its_working_dir()
    -> Result<()> {
        let dir = init_temp_repo()?;
        let outside = tempfile::tempdir()?;
        std::fs::create_dir(outside.path().join("data"))?;
        std::fs::write(outside.path().join("data/seed.txt"), "seed")?;
        let config = default_config_in(outside.path(), "data:/data")?;
        let recipe = current_recipe_in(dir.path(), &config).await?;
        assert_eq!(
            first_copy_content(&recipe)?,
            Some(digest_dir(&outside.path().join("data"))?.as_str())
        );
        Ok(())
    }

    /// The recipe and the build take a relative copy source's origin from the one rule, so the
    /// directory `current_recipe` digests is the directory `copy_specs` hands the builder for
    /// the same invocation.
    #[tokio::test]
    async fn test_current_recipe_digests_the_directory_copy_specs_resolves_under_working_dir()
    -> Result<()> {
        let base = tempfile::tempdir()?;
        let repo = base.path().join("repo");
        std::fs::create_dir(&repo)?;
        init_repo_in(&repo)?;
        std::fs::create_dir_all(base.path().join("ops/data"))?;
        std::fs::write(base.path().join("ops/data/seed.txt"), "seed")?;
        let copy_dir = "data:/data";
        let config = default_config_in(Path::new("../ops"), copy_dir)?;
        let recipe = current_recipe_in(&repo, &config).await?;

        let root = git::repo_root(&repo).await?;
        let cwd_rel = git::path_within_repo(&repo).await?;
        let build = BuildContext {
            dir: Path::new("/ctx"),
            cwd_rel: &cwd_rel,
            root: &root,
            origin: copy_origin(&config.provider),
        };
        let specs = build.copy_specs(&[copy_dir.to_string()], &[]);
        let source = specs
            .first()
            .and_then(|spec| spec.strip_suffix(":/data"))
            .context("expected one resolved copy spec")?;
        assert_eq!(
            first_copy_content(&recipe)?,
            Some(digest_dir(Path::new(source))?.as_str())
        );
        Ok(())
    }
}
