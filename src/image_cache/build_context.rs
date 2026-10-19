//! The tree a base-image build reads from, with the resolution of copy sources into it.

use std::path::{Path, PathBuf};

use crate::config::ProviderConfig;

/// The tree a base-image build reads from, so a cached image depends only on the checkpoint
/// export (or the fallback's snapshot) and never on the live working tree.
pub(crate) struct BuildContext<'a> {
    /// Directory the image is built from: an export of the checkpoint tree, or a snapshot of
    /// the invoking directory.
    pub dir: &'a Path,
    /// The invoking directory relative to `root`; empty when `dir` is a snapshot of the invoking
    /// directory itself.
    pub cwd_rel: &'a Path,
    /// The on-disk directory `dir` mirrors (the repository root for an export, the invoking
    /// directory for a snapshot), so absolute sources under it map to their copy in `dir`.
    pub root: &'a Path,
    /// What a relative copy source is written against, as [`copy_origin`] chose it for the
    /// provider, so the builder reads the directory the recipe digested.
    pub origin: &'a Path,
}

/// The directory a relative copy source is written against, relative to the invoking directory:
/// `[provider].working_dir` for the default provider, because its `prepare_command` runs there
/// and a relative source has always named a path under it, and the invoking directory itself
/// (the empty path) for every other provider. The recipe and the build both take the origin
/// from here, so a cached image cannot be keyed by one directory and built from another.
pub(crate) fn copy_origin(provider: &ProviderConfig) -> &Path {
    match provider {
        ProviderConfig::Default(default) => default.working_dir.as_deref().unwrap_or(Path::new("")),
        ProviderConfig::Modal(_) | ProviderConfig::Local(_) => Path::new(""),
    }
}

impl BuildContext<'_> {
    /// The context's copy of the invoking directory, which `include_cwd` adds to the image as
    /// `/app`.
    pub(crate) fn include_dir(&self) -> PathBuf {
        normalized(self.dir.join(self.cwd_rel))
    }

    /// Where the builder reads a copy source: its live location (a relative source resolves
    /// against `origin` under the invoking directory, an absolute one ignores both) is mapped to
    /// its copy in the context when it lies under `root`, and read as given otherwise because
    /// the context holds no counterpart.
    pub(crate) fn resolve_source(&self, source: &Path) -> PathBuf {
        let live = self.root.join(self.cwd_rel).join(self.origin).join(source);
        match path_under_root(self.root, &live) {
            Some(inside) => normalized(self.dir.join(inside)),
            None => live,
        }
    }

    /// `source:target` copy specs with every source resolved, config entries first and runtime
    /// `--copy-dir` pairs second, which is the order the recipe records; a config entry without
    /// a `:` is passed through as written.
    pub(crate) fn copy_specs(
        &self,
        config_copy_dirs: &[String],
        runtime_copy_dirs: &[(PathBuf, PathBuf)],
    ) -> Vec<String> {
        let config = config_copy_dirs
            .iter()
            .map(|spec| match spec.split_once(':') {
                Some((source, target)) => {
                    format!(
                        "{}:{}",
                        self.resolve_source(Path::new(source)).display(),
                        target
                    )
                }
                None => spec.clone(),
            });
        let runtime = runtime_copy_dirs.iter().map(|(source, target)| {
            format!(
                "{}:{}",
                self.resolve_source(source).display(),
                target.display()
            )
        });
        config.chain(runtime).collect()
    }
}

/// Drop the `.` components and trailing separators that `Path::join` leaves in place.
fn normalized(path: PathBuf) -> PathBuf {
    path.components().collect()
}

/// `path` relative to `root` when it lies inside, so the build and the recipe classify a copy
/// source identically: both are canonicalized when they exist, because a spelling through a
/// symlink (macOS `/var` for `/private/var`) must not be inside for one and outside for the
/// other, and a path that does not exist is compared as written.
pub(crate) fn path_under_root(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.strip_prefix(root).ok().map(Path::to_path_buf)
}

/// Where a symlink of the checkpoint tree leads when its text is followed from the live
/// repository. The recipe digests by this classification and the export is rewritten by it, so
/// the build reads what was digested: `Inside` carries the repository-relative path of the
/// target, which the export holds a copy of; `Escaping` carries the absolute live path the link
/// resolves to, which lies outside the repository or does not exist and so is read live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    Inside(PathBuf),
    Escaping(PathBuf),
}

/// Classify the symlink at repository-relative `link` whose text is `target`: the text is
/// resolved against the link's directory under `root` (an absolute text stands alone), and the
/// result is inside when it exists under the root and escaping otherwise. A dangling link is
/// escaping whatever its text spells, because the export cannot hold what does not exist.
pub(crate) fn link_target(root: &Path, link: &Path, target: &Path) -> LinkTarget {
    let live = root
        .join(link.parent().unwrap_or(Path::new("")))
        .join(target);
    if !live.exists() {
        return LinkTarget::Escaping(live);
    }
    match path_under_root(root, &live) {
        Some(inside) => LinkTarget::Inside(inside),
        None => LinkTarget::Escaping(live),
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;
    use crate::image_cache::test_support::{build_context, build_context_from};

    #[test]
    fn test_path_under_root_sees_through_a_symlinked_spelling() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("data"))?;
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&root, &link)?;
        assert_eq!(
            path_under_root(&root, &link.join("data")),
            Some(PathBuf::from("data"))
        );
        assert_eq!(
            path_under_root(&link, &root.join("data")),
            Some(PathBuf::from("data"))
        );
        Ok(())
    }

    #[test]
    fn test_link_target_resolves_a_relative_text_against_the_link_directory() -> Result<()> {
        let base = tempfile::tempdir()?;
        let root = base.path().join("repo");
        std::fs::create_dir_all(root.join("docs"))?;
        std::fs::write(root.join("README.md"), "readme")?;
        std::fs::write(base.path().join("model.bin"), "weights")?;
        assert_eq!(
            link_target(&root, Path::new("docs/readme"), Path::new("../README.md")),
            LinkTarget::Inside(PathBuf::from("README.md"))
        );
        assert_eq!(
            link_target(&root, Path::new("readme"), Path::new("README.md")),
            LinkTarget::Inside(PathBuf::from("README.md"))
        );
        assert_eq!(
            link_target(&root, Path::new("docs/model"), Path::new("../../model.bin")),
            LinkTarget::Escaping(root.join("docs/../../model.bin"))
        );
        Ok(())
    }

    #[test]
    fn test_link_target_classifies_an_absolute_text_by_where_it_lies() -> Result<()> {
        let base = tempfile::tempdir()?;
        let root = base.path().join("repo");
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("README.md"), "readme")?;
        std::fs::write(base.path().join("model.bin"), "weights")?;
        let spelled_through_link = base.path().join("link");
        std::os::unix::fs::symlink(&root, &spelled_through_link)?;
        assert_eq!(
            link_target(&root, Path::new("readme"), &root.join("README.md")),
            LinkTarget::Inside(PathBuf::from("README.md"))
        );
        assert_eq!(
            link_target(
                &root,
                Path::new("readme"),
                &spelled_through_link.join("README.md")
            ),
            LinkTarget::Inside(PathBuf::from("README.md"))
        );
        assert_eq!(
            link_target(&root, Path::new("model"), &base.path().join("model.bin")),
            LinkTarget::Escaping(base.path().join("model.bin"))
        );
        Ok(())
    }

    #[test]
    fn test_link_target_is_escaping_for_a_dangling_link_whatever_its_text_spells() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        assert_eq!(
            link_target(root, Path::new("late"), Path::new("missing.txt")),
            LinkTarget::Escaping(root.join("missing.txt"))
        );
        assert_eq!(
            link_target(root, Path::new("late"), &root.join("missing.txt")),
            LinkTarget::Escaping(root.join("missing.txt"))
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_relative_is_under_the_invoking_directory_in_the_context() -> Result<()> {
        let ctx = build_context();
        assert_eq!(
            ctx.resolve_source(Path::new("./data")),
            PathBuf::from("/ctx/sub/data")
        );
        assert_eq!(
            ctx.resolve_source(Path::new("data")),
            PathBuf::from("/ctx/sub/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_absolute_inside_the_repository_maps_into_the_context() -> Result<()> {
        let ctx = build_context();
        assert_eq!(
            ctx.resolve_source(Path::new("/repo/other/data")),
            PathBuf::from("/ctx/other/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_absolute_outside_the_repository_is_unchanged() -> Result<()> {
        let ctx = build_context();
        assert_eq!(
            ctx.resolve_source(Path::new("/elsewhere/data")),
            PathBuf::from("/elsewhere/data")
        );
        // A sibling whose name merely starts with the root's name is outside it.
        assert_eq!(
            ctx.resolve_source(Path::new("/repo-archive/data")),
            PathBuf::from("/repo-archive/data")
        );
        Ok(())
    }

    #[test]
    fn test_include_dir_is_the_context_at_the_invoking_directory() -> Result<()> {
        assert_eq!(build_context().include_dir(), PathBuf::from("/ctx/sub"));
        let snapshot = BuildContext {
            dir: Path::new("/snap"),
            cwd_rel: Path::new(""),
            root: Path::new("/repo/sub"),
            origin: Path::new(""),
        };
        assert_eq!(snapshot.include_dir(), PathBuf::from("/snap"));
        Ok(())
    }

    #[test]
    fn test_copy_specs_resolve_sources_and_keep_targets_in_builder_order() -> Result<()> {
        let config = vec!["./data:/data".to_string(), "no-colon".to_string()];
        let runtime = vec![(PathBuf::from("/repo/tests"), PathBuf::from("/app/tests"))];
        assert_eq!(
            build_context().copy_specs(&config, &runtime),
            vec!["/ctx/sub/data:/data", "no-colon", "/ctx/tests:/app/tests"]
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_relative_origin_is_under_the_invoking_directory() -> Result<()> {
        assert_eq!(
            build_context_from(Path::new("scripts")).resolve_source(Path::new("data")),
            PathBuf::from("/ctx/sub/scripts/data")
        );
        assert_eq!(
            build_context_from(Path::new("./scripts")).resolve_source(Path::new("./data")),
            PathBuf::from("/ctx/sub/scripts/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_absolute_origin_under_root_maps_into_the_context() -> Result<()> {
        assert_eq!(
            build_context_from(Path::new("/repo/scripts")).resolve_source(Path::new("data")),
            PathBuf::from("/ctx/scripts/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_absolute_origin_outside_root_reads_live_paths() -> Result<()> {
        assert_eq!(
            build_context_from(Path::new("/elsewhere")).resolve_source(Path::new("data")),
            PathBuf::from("/elsewhere/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_ignores_the_origin_for_absolute_sources() -> Result<()> {
        for origin in ["scripts", "/repo/scripts", "/elsewhere"] {
            let ctx = build_context_from(Path::new(origin));
            assert_eq!(
                ctx.resolve_source(Path::new("/repo/other/data")),
                PathBuf::from("/ctx/other/data"),
                "origin {origin}"
            );
            assert_eq!(
                ctx.resolve_source(Path::new("/elsewhere/data")),
                PathBuf::from("/elsewhere/data"),
                "origin {origin}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_copy_specs_resolve_relative_sources_against_the_origin() -> Result<()> {
        let config = vec!["./data:/data".to_string(), "no-colon".to_string()];
        let runtime = vec![(PathBuf::from("/repo/tests"), PathBuf::from("/app/tests"))];
        assert_eq!(
            build_context_from(Path::new("scripts")).copy_specs(&config, &runtime),
            vec![
                "/ctx/sub/scripts/data:/data",
                "no-colon",
                "/ctx/tests:/app/tests"
            ]
        );
        Ok(())
    }
}
