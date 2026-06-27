//! Configuration objects for tracing and reporting.

use crate::{Error, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// User-configurable tracing settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceConfig {
    /// Root of the Lean/Lake workspace.
    pub lake_root: Utf8PathBuf,
    /// Whether directories should be searched recursively.
    pub recursive: bool,
    /// Process timeout per Lean file.
    #[serde(with = "duration_seconds")]
    pub timeout: Duration,
    /// Keep raw stdout/stderr in trace records.
    pub keep_raw_output: bool,
    /// Include warnings in parsed diagnostics.
    pub include_warnings: bool,
    /// Include passing files in JSONL output.
    pub include_passes: bool,
    /// Substrings; a discovered file is skipped if its path contains any of them.
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl TraceConfig {
    /// Construct a default trace config rooted at the current working directory.
    #[must_use]
    pub fn new(lake_root: Utf8PathBuf) -> Self {
        Self {
            lake_root,
            recursive: false,
            timeout: Duration::from_secs(60),
            keep_raw_output: false,
            include_warnings: true,
            include_passes: true,
            exclude: Vec::new(),
        }
    }

    /// Set recursive directory traversal.
    #[must_use]
    pub const fn recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    /// Set process timeout.
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set raw-output retention.
    #[must_use]
    pub const fn keep_raw_output(mut self, keep_raw_output: bool) -> Self {
        self.keep_raw_output = keep_raw_output;
        self
    }
}

/// Report settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportConfig {
    /// Number of sample diagnostics to include.
    pub sample_limit: usize,
}

impl Default for ReportConfig {
    fn default() -> Self {
        Self { sample_limit: 10 }
    }
}

/// On-disk `lean-agent.toml` schema.
///
/// All fields are optional so a partial file is valid; the CLI fills gaps with
/// flag values and then hardcoded defaults.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// `[project]` table.
    #[serde(default)]
    pub project: ProjectConfig,
    /// `[trace]` table.
    #[serde(default)]
    pub trace: TraceFileConfig,
}

/// `[project]` settings from `lean-agent.toml`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Human-readable project name, used only in logs.
    pub name: Option<String>,
    /// Lake workspace root.
    pub lake_root: Option<Utf8PathBuf>,
    /// Default trace targets when no path is given on the command line.
    #[serde(default)]
    pub source_roots: Vec<Utf8PathBuf>,
    /// Path substrings to skip during discovery.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// `[trace]` settings from `lean-agent.toml`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TraceFileConfig {
    /// Per-file process timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Keep raw stdout/stderr in records.
    pub keep_raw_output: Option<bool>,
    /// Include warnings in parsed diagnostics.
    pub include_warnings: Option<bool>,
    /// Emit only non-passing files.
    pub only_failures: Option<bool>,
}

impl FileConfig {
    /// Load and parse a `lean-agent.toml` file.
    pub fn load(path: &Utf8Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| Error::ConfigParse {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<FileConfig> {
        toml::from_str(text).map_err(|source| Error::ConfigParse {
            path: Utf8PathBuf::from("<test>"),
            source,
        })
    }

    #[test]
    fn parses_full_config() -> Result<()> {
        let config = parse(
            r#"
[project]
name = "demo"
lake_root = "."
source_roots = ["src", "test"]
exclude = [".lake/"]

[trace]
timeout_secs = 45
keep_raw_output = true
include_warnings = false
only_failures = true
"#,
        )?;
        assert_eq!(config.project.name.as_deref(), Some("demo"));
        assert_eq!(config.project.source_roots.len(), 2);
        assert_eq!(config.project.exclude, vec![".lake/".to_owned()]);
        assert_eq!(config.trace.timeout_secs, Some(45));
        assert_eq!(config.trace.keep_raw_output, Some(true));
        assert_eq!(config.trace.include_warnings, Some(false));
        assert_eq!(config.trace.only_failures, Some(true));
        Ok(())
    }

    #[test]
    fn empty_config_is_all_defaults() -> Result<()> {
        let config = parse("")?;
        assert!(config.project.name.is_none());
        assert!(config.project.source_roots.is_empty());
        assert!(config.trace.timeout_secs.is_none());
        Ok(())
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(parse("[trace]\nbogus = 1\n").is_err());
    }
}

mod duration_seconds {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub(crate) fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let seconds = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(seconds))
    }
}
