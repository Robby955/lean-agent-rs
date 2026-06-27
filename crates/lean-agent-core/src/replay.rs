//! Replaying bounded proof attempts against isolated workspace copies.
//!
//! Each [`Attempt`] names a task, a single bounded edit (the same `allowed_edit`
//! the miner emits), and the replacement text. Replay copies the Lake project
//! into a throwaway workspace, applies the one span, runs `lake lean` on the
//! target file, and emits a [`ReplayResult`] that decomposes the outcome into
//! whether it compiled, whether the original problem went away, and whether the
//! patch introduced anything new.
//!
//! Comparisons use an optional baseline: the same target file compiled in an
//! unpatched copy. The baseline for a target file is computed once and reused
//! across attempts, since the unpatched project is identical every time.

use crate::accept::{AcceptReport, AcceptRequest, RejectReason};
use crate::patch::{SpanReplacement, apply_edits};
use crate::workspace::{CopyOptions, Workspace};
use crate::writer::JsonlWriter;
use crate::{
    AllowedEdit, Diagnostic, DiagnosticSeverity, FileStatus, FileTrace, GoalState, LeanFile,
    Provenance, Result, TraceConfig, accept, capture_provenance, run_lean_file,
};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// A diagnostic identity that is stable across workspace copies.
///
/// Workspace paths differ between the baseline and patched runs, so a signature
/// uses only the line and message text, which carry no absolute path.
type DiagnosticSignature = (Option<u32>, String);

/// One bounded proof attempt to replay.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    /// Task this attempt answers.
    pub task_id: String,
    /// Identifier for this attempt; defaults to `attempt` when omitted.
    #[serde(default = "default_attempt_id")]
    pub attempt_id: String,
    /// The single bounded edit, reusing the miner's `allowed_edit` shape.
    pub allowed_edit: AllowedEdit,
    /// New content spliced over the allowed line range.
    pub replacement: String,
    /// File to compile; defaults to `allowed_edit.file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_file: Option<Utf8PathBuf>,
    /// Extra edits, honored only when multi-file application is enabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_edits: Vec<SpanReplacement>,
    /// The original problem this attempt targets, used to decide resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_diagnostic: Option<Diagnostic>,
    /// Model identifier the runner reported, when one was provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Hash of the prompt that produced this attempt, for reproducibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_hash: Option<String>,
    /// Free-form runner metadata (cost, latency, sampling settings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

fn default_attempt_id() -> String {
    "attempt".to_owned()
}

impl Attempt {
    /// The primary, bounded single-span edit this attempt makes.
    #[must_use]
    pub fn primary_span(&self) -> SpanReplacement {
        SpanReplacement {
            file: self.allowed_edit.file.clone(),
            start_line: self.allowed_edit.start_line,
            end_line: self.allowed_edit.end_line,
            replacement: self.replacement.clone(),
        }
    }

    /// File compiled to score the attempt.
    #[must_use]
    pub fn target(&self) -> Utf8PathBuf {
        self.target_file
            .clone()
            .unwrap_or_else(|| self.allowed_edit.file.clone())
    }
}

/// Coarse status for one replayed attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayStatus {
    /// Patched file compiled clean and every live accept guard passed.
    Passed,
    /// Patched file compiled clean but an accept guard refused it.
    Rejected,
    /// Patched file compiled but still has errors.
    Failed,
    /// The patch was refused before any compile (out of bounds, escaping, or
    /// multi-file without the flag).
    PatchRefused,
    /// The compile timed out.
    TimedOut,
    /// The workspace or process failed before a result could be trusted.
    RunnerError,
}

/// Result record for one replayed attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayResult {
    /// Task the attempt answered.
    pub task_id: String,
    /// Attempt identifier.
    pub attempt_id: String,
    /// Coarse outcome.
    pub status: ReplayStatus,
    /// True when the patched file compiled with no errors.
    pub compile_passed: bool,
    /// True only when the compile passed and every live accept guard passed.
    #[serde(default)]
    pub accepted: bool,
    /// Number of diagnostics from the patched compile.
    pub diagnostic_count: usize,
    /// Patched errors whose signature was not in the baseline.
    pub new_errors: usize,
    /// Whether the original problem is gone after the patch.
    pub resolved_original_error: bool,
    /// Whether the patch introduced at least one new error.
    pub regression: bool,
    /// Wall-clock time for the whole attempt (copy, patch, compile).
    pub elapsed_ms: u64,
    /// First goal state surfaced by the patched compile, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_goal_state: Option<GoalState>,
    /// Per-guard accept outcomes, present when the compile passed and guards ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guards: Option<AcceptReport>,
    /// The accept guard that refused the attempt, when `status` is `rejected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<RejectReason>,
    /// Why the patch was refused, when `status` is `patch_refused` or
    /// `runner_error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_error: Option<String>,
}

/// Runtime options for a replay run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayOptions {
    /// Lake workspace root that gets copied per attempt.
    pub lake_root: Utf8PathBuf,
    /// Per-compile timeout.
    pub timeout: Duration,
    /// Keep each workspace copy on disk instead of deleting it.
    pub keep_workdir: bool,
    /// Allow an attempt to edit more than one file.
    pub allow_multi_file: bool,
    /// Compile an unpatched baseline so new errors and regressions are scored.
    pub compute_baseline: bool,
    /// Run the reverse-dependency accept guard (`lake build` of the module).
    pub reverse_dep: bool,
    /// Run `lake exe cache get` in the workspace when the project pulls mathlib,
    /// so dependency oleans are restored before the edited file is recompiled.
    pub cache_get: bool,
}

impl ReplayOptions {
    /// Default options rooted at `lake_root`: one-minute timeout, disposable
    /// workspaces, single-file edits, baseline on, accept guards on.
    #[must_use]
    pub fn new(lake_root: Utf8PathBuf) -> Self {
        Self {
            lake_root,
            timeout: Duration::from_secs(60),
            keep_workdir: false,
            allow_multi_file: false,
            compute_baseline: true,
            reverse_dep: true,
            cache_get: true,
        }
    }
}

/// Counts from a replay run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReplaySummary {
    /// Attempts replayed.
    pub attempts: usize,
    /// Attempts whose patched file compiled clean and passed every guard.
    pub compiled_pass: usize,
    /// Attempts that compiled clean but were refused by an accept guard.
    pub rejected: usize,
    /// Attempts whose patched file compiled with errors.
    pub compiled_fail: usize,
    /// Attempts whose patch was refused.
    pub patch_refused: usize,
    /// Attempts that timed out or hit a runner error.
    pub errored: usize,
}

impl ReplaySummary {
    fn record(&mut self, result: &ReplayResult) {
        self.attempts += 1;
        match result.status {
            ReplayStatus::Passed => self.compiled_pass += 1,
            ReplayStatus::Rejected => self.rejected += 1,
            ReplayStatus::Failed => self.compiled_fail += 1,
            ReplayStatus::PatchRefused => self.patch_refused += 1,
            ReplayStatus::TimedOut | ReplayStatus::RunnerError => self.errored += 1,
        }
    }
}

/// The unpatched compile of one target file, summarized for comparison.
#[derive(Clone, Debug, Default)]
struct Baseline {
    error_signatures: BTreeSet<DiagnosticSignature>,
}

/// Replay every attempt and stream one result record per attempt to `writer`.
pub async fn run_replay(
    options: &ReplayOptions,
    attempts: &[Attempt],
    writer: &mut JsonlWriter,
) -> Result<ReplaySummary> {
    let provenance = capture_provenance(options.lake_root.as_path()).await;
    let copy_options = CopyOptions::default();
    let mut baselines: HashMap<String, Baseline> = HashMap::new();
    let mut summary = ReplaySummary::default();

    for attempt in attempts {
        let result =
            replay_attempt(options, &provenance, &copy_options, &mut baselines, attempt).await;
        writer.write_record(&result)?;
        summary.record(&result);
    }

    writer.flush()?;
    Ok(summary)
}

/// Replay one attempt, converting every failure into a result record rather than
/// an error return.
async fn replay_attempt(
    options: &ReplayOptions,
    provenance: &Provenance,
    copy_options: &CopyOptions,
    baselines: &mut HashMap<String, Baseline>,
    attempt: &Attempt,
) -> ReplayResult {
    let started = Instant::now();
    let target = attempt.target();

    let baseline = if options.compute_baseline {
        baseline_for(options, provenance, copy_options, baselines, &target).await
    } else {
        Baseline::default()
    };

    let workspace =
        match Workspace::materialize(&options.lake_root, options.keep_workdir, copy_options) {
            Ok(workspace) => workspace,
            Err(err) => {
                return terminal_result(
                    attempt,
                    ReplayStatus::RunnerError,
                    started.elapsed(),
                    err.to_string(),
                );
            }
        };
    if workspace.is_kept() {
        info!(task = %attempt.task_id, attempt = %attempt.attempt_id, workdir = %workspace.root(), "kept replay workspace");
    }

    let mut edits = vec![attempt.primary_span()];
    edits.extend(attempt.extra_edits.iter().cloned());
    if let Err(err) = apply_edits(workspace.root(), &edits, options.allow_multi_file) {
        return terminal_result(
            attempt,
            ReplayStatus::PatchRefused,
            started.elapsed(),
            err.to_string(),
        );
    }

    if options.cache_get {
        cache_get_if_available(workspace.root(), options.timeout).await;
    }

    let trace = run_lean_file(
        &compile_config(workspace.root(), options.timeout),
        provenance,
        LeanFile(target.clone()),
    )
    .await;

    let mut result = score(attempt, &baseline, &trace, started.elapsed());
    if result.compile_passed {
        let request = AcceptRequest {
            lake_root: &options.lake_root,
            workspace_root: workspace.root(),
            target: &target,
            edit_line: attempt.allowed_edit.start_line,
            patched_diagnostics: &trace.diagnostics,
            provenance,
            timeout: options.timeout,
            run_reverse_dep: options.reverse_dep,
            negative_control: None,
        };
        let outcome = accept::evaluate(&request).await;
        result.accepted = outcome.accepted;
        result.guards = Some(outcome.report);
        result.reject_reason = outcome.reject_reason;
        if !outcome.accepted {
            result.status = ReplayStatus::Rejected;
        }
    }
    result.elapsed_ms = millis(started.elapsed());
    result
}

/// Best-effort `lake exe cache get`, used only when the workspace pulls mathlib.
///
/// The workspace copy intentionally drops `.lake`, so a mathlib-backed project
/// needs its dependency oleans restored before the edited file is recompiled.
/// Failure is ignored: a project without the cache executable simply skips this.
async fn cache_get_if_available(workspace_root: &Utf8Path, timeout: Duration) {
    let manifest = workspace_root.join("lake-manifest.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return;
    };
    if !text.contains("mathlib") {
        return;
    }
    let mut command = tokio::process::Command::new("lake");
    command
        .args(["exe", "cache", "get"])
        .current_dir(workspace_root)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match command.spawn() {
        Ok(child) => {
            if tokio::time::timeout(timeout, child.wait_with_output())
                .await
                .is_err()
            {
                warn!("lake exe cache get timed out; continuing without it");
            }
        }
        Err(err) => debug!(error = %err, "lake exe cache get unavailable; continuing"),
    }
}

/// Look up, or compute and cache, the baseline for `target`.
async fn baseline_for(
    options: &ReplayOptions,
    provenance: &Provenance,
    copy_options: &CopyOptions,
    baselines: &mut HashMap<String, Baseline>,
    target: &Utf8Path,
) -> Baseline {
    if let Some(baseline) = baselines.get(target.as_str()) {
        return baseline.clone();
    }
    let baseline = compute_baseline(options, provenance, copy_options, target).await;
    baselines.insert(target.as_str().to_owned(), baseline.clone());
    baseline
}

/// Compile `target` in an unpatched copy and record its error signatures.
async fn compute_baseline(
    options: &ReplayOptions,
    provenance: &Provenance,
    copy_options: &CopyOptions,
    target: &Utf8Path,
) -> Baseline {
    let workspace = match Workspace::materialize(&options.lake_root, false, copy_options) {
        Ok(workspace) => workspace,
        Err(err) => {
            warn!(target = %target, error = %err, "baseline workspace failed; scoring without it");
            return Baseline::default();
        }
    };
    let trace = run_lean_file(
        &compile_config(workspace.root(), options.timeout),
        provenance,
        LeanFile(target.to_path_buf()),
    )
    .await;
    Baseline {
        error_signatures: error_signatures(&trace.diagnostics),
    }
}

/// Compare a patched compile against its baseline into a result record.
fn score(
    attempt: &Attempt,
    baseline: &Baseline,
    trace: &FileTrace,
    elapsed: Duration,
) -> ReplayResult {
    let patched_errors = error_signatures(&trace.diagnostics);
    let new_errors = patched_errors
        .iter()
        .filter(|signature| !baseline.error_signatures.contains(*signature))
        .count();

    let status = match trace.status {
        FileStatus::Passed => ReplayStatus::Passed,
        FileStatus::Failed => ReplayStatus::Failed,
        FileStatus::TimedOut => ReplayStatus::TimedOut,
        FileStatus::RunnerError => ReplayStatus::RunnerError,
    };
    let compile_passed = trace.status == FileStatus::Passed;

    let patch_error = match status {
        ReplayStatus::TimedOut | ReplayStatus::RunnerError => trace
            .diagnostics
            .first()
            .map(|diagnostic| diagnostic.message.clone()),
        _ => None,
    };

    ReplayResult {
        task_id: attempt.task_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        status,
        compile_passed,
        accepted: false,
        diagnostic_count: trace.diagnostics.len(),
        new_errors,
        resolved_original_error: resolved_original(attempt, baseline, &trace.diagnostics),
        regression: new_errors > 0,
        elapsed_ms: millis(elapsed),
        final_goal_state: trace
            .diagnostics
            .iter()
            .find_map(|diagnostic| diagnostic.goal_state.clone()),
        guards: None,
        reject_reason: None,
        patch_error,
    }
}

/// Whether the original problem is gone after the patch.
///
/// With an `original_diagnostic`, true iff that exact diagnostic (any severity)
/// no longer appears. Otherwise, true iff the baseline had at least one error
/// and none of those baseline errors remain.
fn resolved_original(attempt: &Attempt, baseline: &Baseline, patched: &[Diagnostic]) -> bool {
    if let Some(original) = &attempt.original_diagnostic {
        let target = signature(original);
        return !patched
            .iter()
            .any(|diagnostic| signature(diagnostic) == target);
    }
    if baseline.error_signatures.is_empty() {
        return false;
    }
    let patched_errors = error_signatures(patched);
    baseline
        .error_signatures
        .iter()
        .all(|signature| !patched_errors.contains(signature))
}

/// Build a result for an attempt that never reached scoring.
fn terminal_result(
    attempt: &Attempt,
    status: ReplayStatus,
    elapsed: Duration,
    message: String,
) -> ReplayResult {
    ReplayResult {
        task_id: attempt.task_id.clone(),
        attempt_id: attempt.attempt_id.clone(),
        status,
        compile_passed: false,
        accepted: false,
        diagnostic_count: 0,
        new_errors: 0,
        resolved_original_error: false,
        regression: false,
        elapsed_ms: millis(elapsed),
        final_goal_state: None,
        guards: None,
        reject_reason: None,
        patch_error: Some(message),
    }
}

/// Trace config for compiling one file inside a workspace copy.
fn compile_config(lake_root: &Utf8Path, timeout: Duration) -> TraceConfig {
    let mut config = TraceConfig::new(lake_root.to_path_buf());
    config.timeout = timeout;
    config.include_warnings = true;
    config
}

/// Signatures of the error diagnostics in `diagnostics`.
fn error_signatures(diagnostics: &[Diagnostic]) -> BTreeSet<DiagnosticSignature> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
        .map(signature)
        .collect()
}

/// Stable signature for one diagnostic.
///
/// Lean prints the source path in the message, and that path is the workspace
/// copy's temp directory, which differs between the baseline and patched runs.
/// The path is stripped so the same logical diagnostic matches across copies.
fn signature(diagnostic: &Diagnostic) -> DiagnosticSignature {
    let message = match &diagnostic.file {
        Some(file) => diagnostic
            .message
            .replace(file.as_str(), "")
            .trim()
            .to_owned(),
        None => diagnostic.message.clone(),
    };
    (diagnostic.line, message)
}

/// Milliseconds, saturating instead of panicking on overflow.
fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn diagnostic(line: u32, severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            file: Some(Utf8PathBuf::from("/work/Demo.lean")),
            line: Some(line),
            column: Some(0),
            severity,
            message: message.to_owned(),
            goal_state: None,
        }
    }

    fn trace_with(status: FileStatus, diagnostics: Vec<Diagnostic>) -> FileTrace {
        FileTrace {
            run_id: Uuid::new_v4(),
            file: LeanFile(Utf8PathBuf::from("Demo.lean")),
            status,
            exit_code: Some(0),
            elapsed: Duration::from_millis(5),
            diagnostics,
            stdout: None,
            stderr: None,
            lean_version: None,
            lake_version: None,
            git_commit: None,
            created_at: Utc::now(),
        }
    }

    fn attempt(original: Option<Diagnostic>) -> Attempt {
        Attempt {
            task_id: "Demo.demo_one:1".to_owned(),
            attempt_id: "a1".to_owned(),
            allowed_edit: AllowedEdit {
                file: Utf8PathBuf::from("Demo.lean"),
                start_line: 1,
                end_line: 1,
            },
            replacement: "theorem demo_one : 1 + 1 = 2 := by rfl".to_owned(),
            target_file: None,
            extra_edits: Vec::new(),
            original_diagnostic: original,
            model: None,
            prompt_hash: None,
            metadata: None,
        }
    }

    #[test]
    fn minimal_attempt_deserializes_with_defaults() -> Result<()> {
        let line = r#"{"task_id":"T","allowed_edit":{"file":"Demo.lean","start_line":1,"end_line":1},"replacement":"by rfl"}"#;
        let parsed: Attempt = serde_json::from_str(line)?;
        assert_eq!(parsed.task_id, "T");
        assert_eq!(parsed.attempt_id, "attempt");
        assert_eq!(parsed.target(), Utf8PathBuf::from("Demo.lean"));
        assert!(parsed.extra_edits.is_empty());
        let span = parsed.primary_span();
        assert_eq!(span.start_line, 1);
        assert_eq!(span.replacement, "by rfl");
        Ok(())
    }

    #[test]
    fn clean_proof_passes_with_no_new_errors() {
        let warning = diagnostic(1, DiagnosticSeverity::Warning, "declaration uses `sorry`");
        let baseline = Baseline::default();
        let trace = trace_with(FileStatus::Passed, Vec::new());
        let result = score(
            &attempt(Some(warning)),
            &baseline,
            &trace,
            Duration::from_millis(12),
        );
        assert_eq!(result.status, ReplayStatus::Passed);
        assert!(result.compile_passed);
        assert_eq!(result.new_errors, 0);
        assert!(!result.regression);
        assert!(result.resolved_original_error);
        assert_eq!(result.diagnostic_count, 0);
    }

    #[test]
    fn broken_proof_fails_and_flags_regression() {
        let warning = diagnostic(1, DiagnosticSeverity::Warning, "declaration uses `sorry`");
        let baseline = Baseline::default();
        let error = diagnostic(1, DiagnosticSeverity::Error, "Type mismatch");
        let trace = trace_with(FileStatus::Failed, vec![error]);
        let result = score(
            &attempt(Some(warning)),
            &baseline,
            &trace,
            Duration::from_millis(20),
        );
        assert_eq!(result.status, ReplayStatus::Failed);
        assert!(!result.compile_passed);
        assert_eq!(result.new_errors, 1);
        assert!(result.regression);
        // The sorry warning is gone even though a different error appeared.
        assert!(result.resolved_original_error);
    }

    #[test]
    fn fixing_a_baseline_error_resolves_without_original_diagnostic() {
        let baseline = Baseline {
            error_signatures: error_signatures(&[diagnostic(
                3,
                DiagnosticSeverity::Error,
                "unsolved goals",
            )]),
        };
        let trace = trace_with(FileStatus::Passed, Vec::new());
        let result = score(&attempt(None), &baseline, &trace, Duration::from_millis(8));
        assert!(result.compile_passed);
        assert!(result.resolved_original_error);
        assert_eq!(result.new_errors, 0);
        assert!(!result.regression);
    }

    #[test]
    fn baseline_error_that_persists_is_not_a_new_error() {
        let persistent = diagnostic(3, DiagnosticSeverity::Error, "unsolved goals");
        let baseline = Baseline {
            error_signatures: error_signatures(std::slice::from_ref(&persistent)),
        };
        let trace = trace_with(FileStatus::Failed, vec![persistent]);
        let result = score(&attempt(None), &baseline, &trace, Duration::from_millis(8));
        assert_eq!(result.new_errors, 0);
        assert!(!result.regression);
        assert!(!result.resolved_original_error);
    }
}
