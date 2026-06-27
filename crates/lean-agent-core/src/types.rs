//! Stable data model for trace and evaluation artifacts.

use camino::Utf8PathBuf;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;
use uuid::Uuid;

/// A Lean source file path.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeanFile(pub Utf8PathBuf);

impl LeanFile {
    /// Create a validated Lean source file path.
    pub fn new(path: Utf8PathBuf) -> crate::Result<Self> {
        if path.extension() != Some("lean") {
            return Err(crate::Error::NotLeanFile { path });
        }
        Ok(Self(path))
    }

    /// Borrow the underlying path.
    #[must_use]
    pub const fn as_path(&self) -> &Utf8PathBuf {
        &self.0
    }
}

impl fmt::Display for LeanFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Lean declaration name, when attribution is available.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeclarationName(pub String);

/// A Lean goal-state snapshot or diagnostic-extracted goal text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GoalState(pub String);

/// Diagnostic severity as emitted or inferred from Lean output.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// Compilation/elaboration error.
    Error,
    /// Warning emitted by Lean or Lake.
    Warning,
    /// Informational message.
    Info,
    /// Severity could not be inferred.
    Unknown,
}

/// A parsed Lean diagnostic block.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Source file path from the diagnostic, if present.
    pub file: Option<Utf8PathBuf>,
    /// One-based line number, if present.
    pub line: Option<u32>,
    /// One-based column number, if present.
    pub column: Option<u32>,
    /// Diagnostic severity.
    pub severity: DiagnosticSeverity,
    /// Human-readable Lean/Lake message.
    pub message: String,
    /// Extracted unsolved-goal text, if present.
    pub goal_state: Option<GoalState>,
}

/// Tooling and repository provenance, captured once per trace run.
///
/// Every [`FileTrace`] in a run copies these fields so each JSONL record is
/// independently reproducible without consulting an external run header.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Output of `lean --version`, if the binary was reachable.
    pub lean_version: Option<String>,
    /// Output of `lake --version`, if the binary was reachable.
    pub lake_version: Option<String>,
    /// `git rev-parse HEAD` of the lake root, if it is a git repository.
    pub git_commit: Option<String>,
}

/// Coarse result status for one Lean file execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// Lean accepted the file.
    Passed,
    /// Lean rejected the file.
    Failed,
    /// Process timeout.
    TimedOut,
    /// Runner crashed before Lean result could be trusted.
    RunnerError,
}

/// Trace output for one Lean source file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileTrace {
    /// Stable unique run identifier.
    pub run_id: Uuid,
    /// Source file checked.
    pub file: LeanFile,
    /// Status for this file.
    pub status: FileStatus,
    /// External process exit code, if available.
    pub exit_code: Option<i32>,
    /// Wall-clock duration in milliseconds.
    #[serde(rename = "elapsed_ms", with = "duration_millis")]
    pub elapsed: Duration,
    /// Parsed diagnostics.
    pub diagnostics: Vec<Diagnostic>,
    /// Raw standard output. Keep optional because it can be large.
    pub stdout: Option<String>,
    /// Raw standard error. Keep optional because it can be large.
    pub stderr: Option<String>,
    /// Lean version, if captured.
    pub lean_version: Option<String>,
    /// Lake version, if captured.
    pub lake_version: Option<String>,
    /// Git commit hash, if captured.
    pub git_commit: Option<String>,
    /// UTC timestamp for reproducibility.
    pub created_at: DateTime<Utc>,
}

/// A normalized JSONL record. Start with file records; declaration/task records can be added compatibly.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub enum TraceRecord {
    /// One checked Lean file.
    FileTrace(FileTrace),
}

mod duration_millis {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub(crate) fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_millis().try_into().unwrap_or(u64::MAX))
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(millis))
    }
}
