//! Lean file discovery.

use crate::{Error, LeanFile, Result};
use camino::{Utf8Path, Utf8PathBuf};
use ignore::WalkBuilder;

/// Discover Lean source files from a file or directory.
pub fn discover_lean_files(path: &Utf8Path, recursive: bool) -> Result<Vec<LeanFile>> {
    if !path.exists() {
        return Err(Error::PathDoesNotExist {
            path: path.to_path_buf(),
        });
    }

    if path.is_file() {
        return Ok(vec![LeanFile::new(path.to_path_buf())?]);
    }

    let mut files = Vec::new();
    let mut builder = WalkBuilder::new(path);
    builder
        .hidden(false)
        .parents(true)
        .git_ignore(true)
        .git_exclude(true);

    if !recursive {
        builder.max_depth(Some(1));
    }

    for entry in builder.build() {
        let entry = entry.map_err(|err| Error::Io(std::io::Error::other(err)))?;
        if !entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            continue;
        }
        let std_path = entry.into_path();
        let utf8 = Utf8PathBuf::from_path_buf(std_path.clone())
            .map_err(|_| Error::NonUtf8Path { path: std_path })?;
        if utf8.extension() == Some("lean") {
            files.push(LeanFile::new(utf8)?);
        }
    }

    files.sort();
    Ok(files)
}
