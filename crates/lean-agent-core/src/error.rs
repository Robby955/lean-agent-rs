//! Error types for `lean-agent-core`.

use camino::Utf8PathBuf;
use thiserror::Error;

/// Project-local result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by tracing, parsing, discovery, and writing.
#[derive(Debug, Error)]
pub enum Error {
    /// A filesystem path could not be represented as UTF-8.
    #[error("path is not valid UTF-8: {path:?}")]
    NonUtf8Path {
        /// Original path rendered lossily.
        path: std::path::PathBuf,
    },

    /// A requested path does not exist.
    #[error("path does not exist: {path}")]
    PathDoesNotExist {
        /// Missing path.
        path: Utf8PathBuf,
    },

    /// A requested path is not a Lean source file.
    #[error("not a .lean file: {path}")]
    NotLeanFile {
        /// Invalid file path.
        path: Utf8PathBuf,
    },

    /// A `FILE.lean:LINE` target could not be parsed.
    #[error("invalid FILE.lean:LINE target: {spec}")]
    InvalidLineSpec {
        /// The raw target string as given.
        spec: String,
    },

    /// IO failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// A config file could not be read from disk.
    #[error("reading config {path}: {source}")]
    ConfigRead {
        /// Config file path.
        path: Utf8PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// A config file could not be parsed as TOML.
    #[error("parsing config {path}: {source}")]
    ConfigParse {
        /// Config file path.
        path: Utf8PathBuf,
        /// Underlying TOML parse error.
        #[source]
        source: toml::de::Error,
    },

    /// External Lean/Lake process timed out.
    #[error("Lean process timed out after {timeout_seconds}s for {file}")]
    Timeout {
        /// File being checked.
        file: Utf8PathBuf,
        /// Timeout in seconds.
        timeout_seconds: u64,
    },

    /// An edit path is absolute or uses `..`, so it could escape the workspace.
    #[error("edit path escapes the workspace root: {path}")]
    OutsideWorkspace {
        /// The offending relative path as given.
        path: Utf8PathBuf,
    },

    /// A span carried a zero line number; lines are one-based.
    #[error("span line numbers must be one-based and non-zero (file {file})")]
    ZeroLineSpan {
        /// File the span pointed at.
        file: Utf8PathBuf,
    },

    /// A span's start line is greater than its end line.
    #[error("inverted span in {file}: start_line {start_line} > end_line {end_line}")]
    InvertedSpan {
        /// File the span pointed at.
        file: Utf8PathBuf,
        /// One-based first line.
        start_line: u32,
        /// One-based last line.
        end_line: u32,
    },

    /// A span falls outside the target file's line range.
    #[error("span {start_line}..={end_line} is outside {file} ({line_count} lines)")]
    SpanOutOfBounds {
        /// File the span pointed at.
        file: Utf8PathBuf,
        /// One-based first line.
        start_line: u32,
        /// One-based last line.
        end_line: u32,
        /// Number of lines actually present.
        line_count: usize,
    },

    /// An attempt edits more than one file without the multi-file flag.
    #[error("multi-file edit refused ({files} files); pass the multi-file flag to allow")]
    MultiFileEditNotAllowed {
        /// Distinct files the attempt touched.
        files: usize,
    },

    /// A replacement would splice a top-level command into the edited file.
    ///
    /// A single-span patch is a proof body; a column-zero command such as
    /// `#eval`/`#print`/`import`/`set_option`/`macro`/`elab`/`open` is refused so
    /// it cannot perturb the accept guards that read the compile.
    #[error("replacement in {file} introduces a top-level command: {detail}")]
    DisallowedReplacement {
        /// File the span pointed at.
        file: Utf8PathBuf,
        /// The offending line and why it was refused.
        detail: String,
    },

    /// Starting the external agent runner failed.
    #[error("failed to start runner {runner}: {source}")]
    RunnerSpawn {
        /// Runner path that could not be started.
        runner: Utf8PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// The runner broke the line-oriented request/response contract.
    #[error("runner protocol error: {detail}")]
    RunnerProtocol {
        /// What went wrong with the runner exchange.
        detail: String,
    },

    /// Copying the Lake project into an isolated workspace failed.
    #[error("workspace copy failed for {path}: {source}")]
    WorkspaceCopy {
        /// Path being copied when the failure happened.
        path: Utf8PathBuf,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// Placeholder for code paths intentionally left as TODO.
    #[error("not implemented yet: {feature}")]
    Todo {
        /// Feature name.
        feature: &'static str,
    },
}
