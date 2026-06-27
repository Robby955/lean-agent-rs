//! Trace-run orchestration.
//!
//! This module owns the per-run pipeline so the CLI stays thin: capture
//! provenance once, discover files under each root, run Lean over each, filter,
//! and stream records to a writer.

use crate::{
    FileStatus, LeanFile, Result, TraceConfig, TraceRecord, TraceWriter, capture_provenance,
    discover_lean_files, run_lean_file,
};
use camino::Utf8PathBuf;
use tracing::{info, warn};

/// Outcome counts from a single trace run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TraceSummary {
    /// Lean files discovered and executed.
    pub files_run: usize,
    /// Records actually written (passes can be filtered out).
    pub records_written: usize,
    /// Files whose status was not [`FileStatus::Passed`].
    pub non_passing: usize,
}

/// Capture provenance, run Lean over every file under `roots`, and stream
/// records to `writer`.
///
/// Files are discovered per root (honoring `config.recursive`), de-duplicated,
/// and filtered by `config.exclude`. Non-passing files are always written;
/// passing files are written only when `config.include_passes` is set.
pub async fn run_trace(
    config: &TraceConfig,
    roots: &[Utf8PathBuf],
    writer: &mut TraceWriter,
) -> Result<TraceSummary> {
    let provenance = capture_provenance(&config.lake_root).await;
    info!(
        lean_version = provenance.lean_version.as_deref().unwrap_or("unknown"),
        lake_version = provenance.lake_version.as_deref().unwrap_or("unknown"),
        git_commit = provenance.git_commit.as_deref().unwrap_or("none"),
        "captured run provenance"
    );

    let files = collect_files(config, roots)?;
    info!(count = files.len(), "discovered Lean files");

    let mut summary = TraceSummary::default();
    for file in files {
        info!(%file, "checking file");
        let trace = run_lean_file(config, &provenance, file).await;
        let passed = trace.status == FileStatus::Passed;
        if !passed {
            summary.non_passing += 1;
            warn!(file = %trace.file, status = ?trace.status, "file did not pass");
        }
        summary.files_run += 1;

        if config.include_passes || !passed || !trace.diagnostics.is_empty() {
            writer.write_record(&TraceRecord::FileTrace(trace))?;
            summary.records_written += 1;
        }
    }

    writer.flush()?;
    Ok(summary)
}

/// Discover, sort, de-duplicate, and exclude-filter the files under `roots`.
fn collect_files(config: &TraceConfig, roots: &[Utf8PathBuf]) -> Result<Vec<LeanFile>> {
    let mut files = Vec::new();
    for root in roots {
        files.extend(discover_lean_files(root, config.recursive)?);
    }
    files.sort();
    files.dedup();
    files.retain(|file| !is_excluded(file, &config.exclude));
    Ok(files)
}

/// A file is excluded when any pattern is a substring of its path.
fn is_excluded(file: &LeanFile, exclude: &[String]) -> bool {
    let path = file.as_path().as_str();
    exclude
        .iter()
        .any(|pattern| path.contains(pattern.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lean(path: &str) -> LeanFile {
        LeanFile(Utf8PathBuf::from(path))
    }

    #[test]
    fn exclude_matches_substring() {
        let patterns = vec![".lake/".to_owned(), "Generated".to_owned()];
        assert!(is_excluded(
            &lean("project/.lake/build/Foo.lean"),
            &patterns
        ));
        assert!(is_excluded(&lean("src/GeneratedThing.lean"), &patterns));
        assert!(!is_excluded(&lean("src/Real.lean"), &patterns));
    }

    #[test]
    fn empty_exclude_keeps_everything() {
        assert!(!is_excluded(&lean("anything.lean"), &[]));
    }
}
