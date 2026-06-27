//! Isolated, disposable copies of a Lake project for replay.
//!
//! A [`Workspace`] is a temp-directory copy of the project tree with build
//! output and version-control metadata skipped, so an attempt can be patched and
//! compiled without touching the source of truth. Dropping the workspace removes
//! the copy unless it was materialized with `keep` set.

use crate::{Error, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::fs;
use tempfile::TempDir;
use tracing::warn;
use walkdir::WalkDir;

/// Directory names never copied into a workspace.
///
/// Build output is rebuilt on demand by `lake lean`, and git metadata is never
/// needed, so both are skipped to keep copies small and free of baked-in paths.
const DEFAULT_SKIP: &[&str] = &[".git", ".lake"];

/// What to copy when materializing a workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CopyOptions {
    /// Directory component names skipped while copying.
    pub skip_dirs: Vec<String>,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            skip_dirs: DEFAULT_SKIP.iter().map(|name| (*name).to_owned()).collect(),
        }
    }
}

impl CopyOptions {
    /// True when `name` is a directory the copy should skip.
    fn skips(&self, name: &str) -> bool {
        self.skip_dirs.iter().any(|skip| skip == name)
    }
}

/// An isolated copy of a Lake project.
///
/// When `keep` is false the backing temp directory is deleted on drop. When
/// `keep` is true the directory is persisted and its path is returned by
/// [`Workspace::root`] for inspection.
pub struct Workspace {
    root: Utf8PathBuf,
    handle: Option<TempDir>,
}

impl Workspace {
    /// Copy `lake_root` into a fresh temp directory and return the workspace.
    ///
    /// The copy follows `options.skip_dirs`. With `keep` set, the temp directory
    /// is persisted past drop so its contents can be inspected after a run.
    pub fn materialize(lake_root: &Utf8Path, keep: bool, options: &CopyOptions) -> Result<Self> {
        let temp = TempDir::new().map_err(|source| Error::WorkspaceCopy {
            path: lake_root.to_path_buf(),
            source,
        })?;
        let dest = Utf8PathBuf::from_path_buf(temp.path().to_path_buf())
            .map_err(|path| Error::NonUtf8Path { path })?;

        copy_tree(lake_root, &dest, options)?;

        if keep {
            let kept = temp.keep();
            let root =
                Utf8PathBuf::from_path_buf(kept).map_err(|path| Error::NonUtf8Path { path })?;
            Ok(Self { root, handle: None })
        } else {
            Ok(Self {
                root: dest,
                handle: Some(temp),
            })
        }
    }

    /// Path to the workspace copy's root.
    #[must_use]
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// True when this workspace will outlive its drop.
    #[must_use]
    pub const fn is_kept(&self) -> bool {
        self.handle.is_none()
    }
}

/// Recursively copy `src` into `dest`, skipping configured directories and
/// symlinks. Regular files are copied; the destination tree mirrors `src`.
fn copy_tree(src: &Utf8Path, dest: &Utf8Path, options: &CopyOptions) -> Result<()> {
    let copy_err = |path: &Utf8Path, source: std::io::Error| Error::WorkspaceCopy {
        path: path.to_path_buf(),
        source,
    };

    let mut walker = WalkDir::new(src).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let entry = entry.map_err(|err| Error::WorkspaceCopy {
            path: src.to_path_buf(),
            source: err.into(),
        })?;
        let path = entry.path();
        let Some(utf8_path) = Utf8Path::from_path(path) else {
            warn!(path = %path.display(), "skipping non-UTF-8 path while copying workspace");
            continue;
        };

        let relative = match utf8_path.strip_prefix(src) {
            Ok(relative) => relative,
            Err(_) => continue,
        };

        let file_type = entry.file_type();
        if file_type.is_dir() {
            if let Some(name) = utf8_path.file_name() {
                if options.skips(name) {
                    walker.skip_current_dir();
                    continue;
                }
            }
            if relative.as_str().is_empty() {
                fs::create_dir_all(dest).map_err(|source| copy_err(dest, source))?;
            } else {
                let target = dest.join(relative);
                fs::create_dir_all(&target).map_err(|source| copy_err(&target, source))?;
            }
        } else if file_type.is_file() {
            let target = dest.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|source| copy_err(parent, source))?;
            }
            fs::copy(utf8_path, &target).map_err(|source| copy_err(&target, source))?;
        } else {
            warn!(path = %utf8_path, "skipping non-regular file while copying workspace");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(root: &Utf8Path, rel: &str, contents: &str) -> Result<()> {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
        Ok(())
    }

    #[test]
    fn copies_sources_and_skips_build_and_git() -> Result<()> {
        let source = TempDir::new()?;
        let src = Utf8PathBuf::from_path_buf(source.path().to_path_buf())
            .map_err(|path| Error::NonUtf8Path { path })?;
        seed(&src, "Demo.lean", "theorem t : True := trivial\n")?;
        seed(&src, "sub/Inner.lean", "def x := 1\n")?;
        seed(&src, ".lake/build/lib/stale.olean", "binary")?;
        seed(&src, ".git/config", "[core]\n")?;

        let ws = Workspace::materialize(&src, false, &CopyOptions::default())?;
        let root = ws.root().to_path_buf();

        assert!(root.join("Demo.lean").exists());
        assert!(root.join("sub/Inner.lean").exists());
        assert!(!root.join(".lake").exists());
        assert!(!root.join(".git").exists());
        assert_eq!(
            fs::read_to_string(root.join("sub/Inner.lean"))?,
            "def x := 1\n"
        );
        Ok(())
    }

    #[test]
    fn drop_removes_workspace_unless_kept() -> Result<()> {
        let source = TempDir::new()?;
        let src = Utf8PathBuf::from_path_buf(source.path().to_path_buf())
            .map_err(|path| Error::NonUtf8Path { path })?;
        seed(&src, "Demo.lean", "x\n")?;

        let disposable_root = {
            let ws = Workspace::materialize(&src, false, &CopyOptions::default())?;
            let root = ws.root().to_path_buf();
            assert!(root.exists());
            assert!(!ws.is_kept());
            root
        };
        assert!(!disposable_root.exists());

        let ws = Workspace::materialize(&src, true, &CopyOptions::default())?;
        let kept_root = ws.root().to_path_buf();
        assert!(ws.is_kept());
        drop(ws);
        assert!(kept_root.exists());
        fs::remove_dir_all(&kept_root)?;
        Ok(())
    }
}
