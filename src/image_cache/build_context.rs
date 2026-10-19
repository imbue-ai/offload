//! The tree a base-image build reads from, with the resolution of copy sources into it.

use std::path::{Path, PathBuf};

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
}

impl BuildContext<'_> {
    /// The context's copy of the invoking directory, which `include_cwd` adds to the image as
    /// `/app`.
    pub(crate) fn include_dir(&self) -> PathBuf {
        normalized(self.dir.join(self.cwd_rel))
    }

    /// Where the builder reads a copy source: the source's live location (relative sources
    /// resolve against the invoking directory) is mapped to its copy in the context when it lies
    /// under `root`, and read as given otherwise because the context holds no counterpart.
    pub(crate) fn resolve_source(&self, source: &Path) -> PathBuf {
        let live = self.root.join(self.cwd_rel).join(source);
        match path_under_root(self.root, &live) {
            Some(inside) => normalized(self.dir.join(inside)),
            None => live,
        }
    }

    /// [`Self::resolve_source`] for a builder whose relative sources are written relative to
    /// `origin` rather than the invoking directory: the default provider runs the user's
    /// `prepare_command` in `[provider].working_dir`, so a relative source has always meant a
    /// path under that directory for it. An absolute source ignores the origin.
    pub(crate) fn resolve_source_from(&self, origin: &Path, source: &Path) -> PathBuf {
        self.resolve_source(&origin.join(source))
    }

    /// `source:target` copy specs with every source resolved, config entries first and runtime
    /// `--copy-dir` pairs second, which is the order the recipe records.
    pub(crate) fn copy_specs(
        &self,
        config_copy_dirs: &[String],
        runtime_copy_dirs: &[(PathBuf, PathBuf)],
    ) -> Vec<String> {
        copy_specs_with(
            |source| self.resolve_source(source),
            config_copy_dirs,
            runtime_copy_dirs,
        )
    }

    /// [`Self::copy_specs`] for a builder whose relative sources are written relative to
    /// `origin`; see [`Self::resolve_source_from`].
    pub(crate) fn copy_specs_from(
        &self,
        origin: &Path,
        config_copy_dirs: &[String],
        runtime_copy_dirs: &[(PathBuf, PathBuf)],
    ) -> Vec<String> {
        copy_specs_with(
            |source| self.resolve_source_from(origin, source),
            config_copy_dirs,
            runtime_copy_dirs,
        )
    }
}

/// Formats copy specs with each source passed through `resolve`, config entries first and
/// runtime `--copy-dir` pairs second; a config entry without a `:` is passed through as written.
fn copy_specs_with(
    resolve: impl Fn(&Path) -> PathBuf,
    config_copy_dirs: &[String],
    runtime_copy_dirs: &[(PathBuf, PathBuf)],
) -> Vec<String> {
    let config = config_copy_dirs
        .iter()
        .map(|spec| match spec.split_once(':') {
            Some((source, target)) => {
                format!("{}:{}", resolve(Path::new(source)).display(), target)
            }
            None => spec.clone(),
        });
    let runtime = runtime_copy_dirs
        .iter()
        .map(|(source, target)| format!("{}:{}", resolve(source).display(), target.display()));
    config.chain(runtime).collect()
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

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;
    use crate::image_cache::test_support::build_context;

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
    fn test_resolve_source_from_relative_origin_is_under_the_invoking_directory() -> Result<()> {
        let ctx = build_context();
        assert_eq!(
            ctx.resolve_source_from(Path::new("scripts"), Path::new("data")),
            PathBuf::from("/ctx/sub/scripts/data")
        );
        assert_eq!(
            ctx.resolve_source_from(Path::new("./scripts"), Path::new("./data")),
            PathBuf::from("/ctx/sub/scripts/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_from_absolute_origin_under_root_maps_into_the_context() -> Result<()> {
        assert_eq!(
            build_context().resolve_source_from(Path::new("/repo/scripts"), Path::new("data")),
            PathBuf::from("/ctx/scripts/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_from_absolute_origin_outside_root_reads_live_paths() -> Result<()> {
        assert_eq!(
            build_context().resolve_source_from(Path::new("/elsewhere"), Path::new("data")),
            PathBuf::from("/elsewhere/data")
        );
        Ok(())
    }

    #[test]
    fn test_resolve_source_from_leaves_absolute_sources_to_the_origin_free_rule() -> Result<()> {
        let ctx = build_context();
        for origin in ["scripts", "/repo/scripts", "/elsewhere"] {
            assert_eq!(
                ctx.resolve_source_from(Path::new(origin), Path::new("/repo/other/data")),
                PathBuf::from("/ctx/other/data"),
                "origin {origin}"
            );
            assert_eq!(
                ctx.resolve_source_from(Path::new(origin), Path::new("/elsewhere/data")),
                PathBuf::from("/elsewhere/data"),
                "origin {origin}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_copy_specs_from_resolve_relative_sources_against_the_origin() -> Result<()> {
        let config = vec!["./data:/data".to_string(), "no-colon".to_string()];
        let runtime = vec![(PathBuf::from("/repo/tests"), PathBuf::from("/app/tests"))];
        assert_eq!(
            build_context().copy_specs_from(Path::new("scripts"), &config, &runtime),
            vec![
                "/ctx/sub/scripts/data:/data",
                "no-colon",
                "/ctx/tests:/app/tests"
            ]
        );
        Ok(())
    }
}
