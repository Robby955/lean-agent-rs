//! Running external Lean/Lake processes and capturing run provenance.

use crate::{
    Diagnostic, DiagnosticSeverity, Error, FileStatus, FileTrace, LeanFile, Provenance, Result,
    TraceConfig, parse_lean_diagnostics,
};
use camino::Utf8Path;
use chrono::Utc;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time;
use uuid::Uuid;

/// Lean process invocation metadata.
#[derive(Clone, Debug)]
pub struct LeanInvocation {
    /// File being checked.
    pub file: LeanFile,
    /// Arguments sent to Lake/Lean.
    pub args: Vec<String>,
}

/// Raw process output from a Lean check.
#[derive(Clone, Debug)]
pub struct LeanRunOutput {
    /// Exit code if process completed.
    pub exit_code: Option<i32>,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

/// Capture tooling and repository provenance once for a trace run.
///
/// Each probe is best-effort: a missing binary or a non-git `lake_root` yields a
/// `None` field rather than an error, so tracing still proceeds.
pub async fn capture_provenance(lake_root: &Utf8Path) -> Provenance {
    Provenance {
        lean_version: tool_version("lean").await,
        lake_version: tool_version("lake").await,
        git_commit: git_head(lake_root).await,
    }
}

/// Run Lean against one file and return a normalized trace.
///
/// This never fails: a timeout becomes a [`FileStatus::TimedOut`] record and a
/// spawn/wait failure becomes a [`FileStatus::RunnerError`] record, each carrying
/// the run provenance so every record stays self-describing.
pub async fn run_lean_file(
    config: &TraceConfig,
    provenance: &Provenance,
    file: LeanFile,
) -> FileTrace {
    let started = Instant::now();
    let result = run_lake_lean(config, &file).await;
    let elapsed = started.elapsed();

    match result {
        Ok(output) => {
            let combined = format!("{}\n{}", output.stderr, output.stdout);
            let diagnostics = parse_lean_diagnostics(&combined, config.include_warnings);
            let has_error = diagnostics
                .iter()
                .any(|d| d.severity == DiagnosticSeverity::Error);
            let status = if output.exit_code == Some(0) && !has_error {
                FileStatus::Passed
            } else {
                FileStatus::Failed
            };

            FileTrace {
                run_id: Uuid::new_v4(),
                file,
                status,
                exit_code: output.exit_code,
                elapsed,
                diagnostics,
                stdout: config.keep_raw_output.then_some(output.stdout),
                stderr: config.keep_raw_output.then_some(output.stderr),
                lean_version: provenance.lean_version.clone(),
                lake_version: provenance.lake_version.clone(),
                git_commit: provenance.git_commit.clone(),
                created_at: Utc::now(),
            }
        }
        Err(Error::Timeout {
            timeout_seconds, ..
        }) => runner_failure(
            file,
            provenance,
            FileStatus::TimedOut,
            elapsed,
            format!("Lean process timed out after {timeout_seconds}s"),
        ),
        Err(err) => runner_failure(
            file,
            provenance,
            FileStatus::RunnerError,
            elapsed,
            format!("runner error: {err}"),
        ),
    }
}

/// Build a non-success trace record that carries enough metadata to debug.
fn runner_failure(
    file: LeanFile,
    provenance: &Provenance,
    status: FileStatus,
    elapsed: Duration,
    message: String,
) -> FileTrace {
    FileTrace {
        run_id: Uuid::new_v4(),
        file,
        status,
        exit_code: None,
        elapsed,
        diagnostics: vec![Diagnostic {
            file: None,
            line: None,
            column: None,
            severity: DiagnosticSeverity::Unknown,
            message: message.clone(),
            goal_state: None,
        }],
        stdout: None,
        stderr: Some(message),
        lean_version: provenance.lean_version.clone(),
        lake_version: provenance.lake_version.clone(),
        git_commit: provenance.git_commit.clone(),
        created_at: Utc::now(),
    }
}

async fn run_lake_lean(config: &TraceConfig, file: &LeanFile) -> Result<LeanRunOutput> {
    // We start with `lake lean <file>` instead of LSP. It builds imports and runs Lean in
    // Lake's environment. This is simpler, reproducible, and enough for v0.1.
    let mut command = Command::new("lake");
    command
        .arg("lean")
        .arg(file.as_path().as_str())
        .current_dir(&config.lake_root)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = command.spawn()?;
    let output = match time::timeout(config.timeout, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(crate::Error::Timeout {
                file: file.as_path().clone(),
                timeout_seconds: config.timeout.as_secs(),
            });
        }
    };

    Ok(LeanRunOutput {
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run `<tool> --version` and return its trimmed first line of output.
async fn tool_version(tool: &str) -> Option<String> {
    let output = Command::new(tool).arg("--version").output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        text.into_owned()
    };
    let line = text.lines().next().unwrap_or("").trim();
    (!line.is_empty()).then(|| line.to_owned())
}

/// Return `git rev-parse HEAD` for `lake_root`, or `None` if it is not a git repo.
async fn git_head(lake_root: &Utf8Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(lake_root.as_str())
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!commit.is_empty()).then_some(commit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[tokio::test]
    async fn spawn_failure_becomes_runner_error() {
        // A non-existent working directory makes the child fail to spawn, which
        // must surface as a record rather than a dropped error.
        let config = TraceConfig::new(Utf8PathBuf::from("/no/such/lake/root/xyzzy"));
        let provenance = Provenance::default();
        let file = LeanFile(Utf8PathBuf::from("/no/such/lake/root/xyzzy/Foo.lean"));

        let trace = run_lean_file(&config, &provenance, file).await;

        assert_eq!(trace.status, FileStatus::RunnerError);
        assert_eq!(trace.exit_code, None);
        assert_eq!(trace.diagnostics.len(), 1);
        assert!(trace.stderr.is_some());
    }
}
