//! The agent stage: hand mined tasks to an external runner, collect attempts.
//!
//! `lean-agent` itself never calls a model. Instead it talks to a user-supplied
//! runner over a line-oriented process contract: one task JSON per line goes to
//! the runner's stdin, and one attempt JSON per line is read back from its
//! stdout. The runner owns prompt construction and any model calls; this crate
//! owns the task model, the pairing, and turning each reply into a replayable
//! [`Attempt`].
//!
//! ## Process contract
//!
//! - The runner is spawned once and read in lock step: a task is written and
//!   flushed, then exactly one reply line is read before the next task is sent.
//!   A well-behaved runner emits one line per task and flushes after each.
//! - Blank lines from the runner are ignored, so a chatty runner does not
//!   desynchronize the exchange.
//! - The lake root is passed to the runner in the `LEAN_AGENT_LAKE_ROOT`
//!   environment variable so the runner can read the project if it needs to.
//!
//! Each reply is `{task_id, attempt_id, replacement, model?, prompt_hash?,
//! metadata?}`. The reply carries only the proof text; the editable span,
//! target file, and backing diagnostic come from the mined task, so the merged
//! [`Attempt`] is everything `replay` needs.

use crate::writer::JsonlWriter;
use crate::{Attempt, Error, MineTask, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time;
use tracing::warn;

/// Default attempt identifier when a runner omits one.
const DEFAULT_ATTEMPT_ID: &str = "attempt";

/// Environment variable carrying the lake root through to the runner.
pub const LAKE_ROOT_ENV: &str = "LEAN_AGENT_LAKE_ROOT";

/// One line the runner writes back for one task.
///
/// Only `replacement` is required beyond the identifiers; the span and file are
/// taken from the mined task, so the runner stays a pure text producer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunnerResponse {
    /// Task this reply answers; advisory, since pairing is positional.
    pub task_id: String,
    /// Identifier for this attempt; defaults to `attempt` when omitted.
    #[serde(default = "default_attempt_id")]
    pub attempt_id: String,
    /// New content the runner proposes for the task's editable span.
    pub replacement: String,
    /// Model the runner used, when it reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Hash of the prompt the runner built, when it reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_hash: Option<String>,
    /// Free-form runner metadata (cost, latency, sampling settings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

fn default_attempt_id() -> String {
    DEFAULT_ATTEMPT_ID.to_owned()
}

/// Runtime options for an eval run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvalOptions {
    /// Runner executable or script that speaks the process contract.
    pub runner: Utf8PathBuf,
    /// Lake workspace root, forwarded to the runner via [`LAKE_ROOT_ENV`].
    pub lake_root: Utf8PathBuf,
    /// How long to wait for one reply before treating the runner as stuck.
    pub timeout: Duration,
}

impl EvalOptions {
    /// Options for `runner` rooted at `lake_root`, with a two-minute per-task
    /// reply timeout.
    #[must_use]
    pub fn new(runner: Utf8PathBuf, lake_root: Utf8PathBuf) -> Self {
        Self {
            runner,
            lake_root,
            timeout: Duration::from_secs(120),
        }
    }
}

/// Counts from an eval run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EvalSummary {
    /// Tasks sent to the runner.
    pub tasks_read: usize,
    /// Attempts written to the output.
    pub attempts_written: usize,
    /// Tasks where the runner errored, timed out, or replied malformed.
    pub runner_errors: usize,
    /// Replies whose `task_id` did not match the task that was sent.
    pub id_mismatches: usize,
}

/// Outcome of reading one reply line from the runner.
enum ReplyOutcome {
    /// A reply parsed into a [`RunnerResponse`].
    Parsed(Box<RunnerResponse>),
    /// A line arrived but did not parse; the detail explains why.
    Malformed(String),
    /// The runner closed its output before replying.
    Closed,
    /// No reply arrived within the timeout.
    TimedOut,
}

/// Stream every task to the runner and write one attempt per reply.
///
/// Failures are converted into counts and log lines rather than aborting the
/// run, except when the runner cannot be started at all.
pub async fn run_eval(
    options: &EvalOptions,
    tasks: &[MineTask],
    writer: &mut JsonlWriter,
) -> Result<EvalSummary> {
    let mut summary = EvalSummary::default();
    if tasks.is_empty() {
        return Ok(summary);
    }

    let runner = resolve_runner(&options.runner)?;
    let mut child = spawn_runner(&runner, &options.lake_root)?;
    let mut stdin = child.stdin.take().ok_or_else(|| Error::RunnerProtocol {
        detail: "runner stdin was not captured".to_owned(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| Error::RunnerProtocol {
        detail: "runner stdout was not captured".to_owned(),
    })?;
    let mut lines = BufReader::new(stdout).lines();

    for task in tasks {
        summary.tasks_read += 1;
        let task_line = serde_json::to_string(task)?;

        if let Err(err) = write_task(&mut stdin, &task_line).await {
            warn!(task = %task.task_id, error = %err, "failed to send task to runner; stopping");
            summary.runner_errors += 1;
            break;
        }

        let response = match read_reply(&mut lines, options.timeout).await {
            ReplyOutcome::Parsed(response) => *response,
            ReplyOutcome::Malformed(detail) => {
                warn!(task = %task.task_id, %detail, "runner reply was malformed; skipping task");
                summary.runner_errors += 1;
                continue;
            }
            ReplyOutcome::Closed => {
                warn!(task = %task.task_id, "runner closed its output early; stopping");
                summary.runner_errors += 1;
                break;
            }
            ReplyOutcome::TimedOut => {
                warn!(task = %task.task_id, seconds = options.timeout.as_secs(), "runner timed out; stopping");
                summary.runner_errors += 1;
                let _ = child.start_kill();
                break;
            }
        };

        if response.task_id != task.task_id {
            summary.id_mismatches += 1;
            warn!(sent = %task.task_id, got = %response.task_id, "runner task_id mismatch; keeping the sent id");
        }

        let attempt = merge_attempt(task, response, &task_line);
        writer.write_record(&attempt)?;
        summary.attempts_written += 1;
    }

    drop(stdin);
    let _ = child.wait().await;
    writer.flush()?;
    Ok(summary)
}

/// Merge a mined task with a runner reply into a replayable attempt.
///
/// The task is authoritative for the editable span, target file, and backing
/// diagnostic; the reply supplies the proof text and provenance. The prompt
/// hash falls back to a hash of the exact task line that was sent.
fn merge_attempt(task: &MineTask, response: RunnerResponse, task_line: &str) -> Attempt {
    let prompt_hash = response
        .prompt_hash
        .unwrap_or_else(|| sha256_hex(task_line));
    Attempt {
        task_id: task.task_id.clone(),
        attempt_id: response.attempt_id,
        allowed_edit: task.allowed_edit.clone(),
        replacement: response.replacement,
        target_file: None,
        extra_edits: Vec::new(),
        original_diagnostic: task.diagnostic.clone(),
        model: response.model,
        prompt_hash: Some(prompt_hash),
        metadata: response.metadata,
    }
}

/// Send one task line to the runner and flush so it can act immediately.
async fn write_task(stdin: &mut ChildStdin, task_line: &str) -> Result<()> {
    stdin.write_all(task_line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}

/// Read one reply, skipping blank lines, within `timeout`.
async fn read_reply(lines: &mut Lines<BufReader<ChildStdout>>, timeout: Duration) -> ReplyOutcome {
    loop {
        match time::timeout(timeout, lines.next_line()).await {
            Err(_) => return ReplyOutcome::TimedOut,
            Ok(Err(err)) => {
                return ReplyOutcome::Malformed(format!("reading runner output: {err}"));
            }
            Ok(Ok(None)) => return ReplyOutcome::Closed,
            Ok(Ok(Some(line))) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                return match serde_json::from_str::<RunnerResponse>(trimmed) {
                    Ok(response) => ReplyOutcome::Parsed(Box::new(response)),
                    Err(err) => ReplyOutcome::Malformed(format!("parsing runner attempt: {err}")),
                };
            }
        }
    }
}

/// Spawn the runner with the lake root in its environment.
fn spawn_runner(runner: &Utf8Path, lake_root: &Utf8Path) -> Result<Child> {
    Command::new(runner.as_str())
        .env(LAKE_ROOT_ENV, lake_root.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| Error::RunnerSpawn {
            runner: runner.to_path_buf(),
            source,
        })
}

/// Resolve the runner path: canonicalize a real file, else pass it through so a
/// bare command name can still be found on `PATH`.
fn resolve_runner(runner: &Utf8Path) -> Result<Utf8PathBuf> {
    match std::fs::canonicalize(runner) {
        Ok(path) => Utf8PathBuf::from_path_buf(path).map_err(|path| Error::NonUtf8Path { path }),
        Err(_) => Ok(runner.to_path_buf()),
    }
}

/// Lowercase hex SHA-256 of `input`.
fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AllowedEdit, Diagnostic, DiagnosticSeverity, GoalState, LeanFile, MineKind, TargetSpan,
    };
    use camino::Utf8PathBuf;

    fn sample_task(task_id: &str, line: u32, with_diagnostic: bool) -> MineTask {
        let diagnostic = with_diagnostic.then(|| Diagnostic {
            file: Some(Utf8PathBuf::from("Demo.lean")),
            line: Some(line),
            column: Some(2),
            severity: DiagnosticSeverity::Error,
            message: "error: unsolved goals".to_owned(),
            goal_state: Some(GoalState("⊢ n = n".to_owned())),
        });
        MineTask {
            task_id: task_id.to_owned(),
            project: "demo".to_owned(),
            file: LeanFile(Utf8PathBuf::from("Demo.lean")),
            declaration: None,
            kind: MineKind::Sorry,
            line,
            column: 2,
            imports: vec!["import Init".to_owned()],
            source_before: "theorem t : True := by\n  ".to_owned(),
            target_span: TargetSpan {
                start_line: line,
                start_column: 2,
                end_line: line,
                end_column: 7,
                text: "sorry".to_owned(),
            },
            source_after: "\n".to_owned(),
            diagnostic,
            goal_state: None,
            allowed_edit: AllowedEdit {
                file: Utf8PathBuf::from("Demo.lean"),
                start_line: line,
                end_line: line,
            },
            instructions: "Replace only the target span.".to_owned(),
        }
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // SHA-256 of the empty string.
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn merge_takes_span_from_task_and_proof_from_reply() {
        let task = sample_task("Demo.t:2", 2, true);
        let response = RunnerResponse {
            task_id: "Demo.t:2".to_owned(),
            attempt_id: "cand-1".to_owned(),
            replacement: "  rfl".to_owned(),
            model: Some("test-model".to_owned()),
            prompt_hash: Some("deadbeef".to_owned()),
            metadata: Some(serde_json::json!({"latency_ms": 12})),
        };
        let attempt = merge_attempt(&task, response, "{\"task_id\":\"Demo.t:2\"}");
        assert_eq!(attempt.task_id, "Demo.t:2");
        assert_eq!(attempt.attempt_id, "cand-1");
        assert_eq!(attempt.replacement, "  rfl");
        assert_eq!(attempt.allowed_edit.start_line, 2);
        assert_eq!(attempt.allowed_edit.end_line, 2);
        assert_eq!(attempt.model.as_deref(), Some("test-model"));
        assert_eq!(attempt.prompt_hash.as_deref(), Some("deadbeef"));
        assert!(attempt.original_diagnostic.is_some());
        assert!(attempt.metadata.is_some());
    }

    #[test]
    fn merge_computes_prompt_hash_when_reply_omits_it() {
        let task = sample_task("Demo.t:2", 2, false);
        let response = RunnerResponse {
            task_id: "Demo.t:2".to_owned(),
            attempt_id: DEFAULT_ATTEMPT_ID.to_owned(),
            replacement: "  rfl".to_owned(),
            model: None,
            prompt_hash: None,
            metadata: None,
        };
        let attempt = merge_attempt(&task, response, "task-line");
        assert_eq!(
            attempt.prompt_hash.as_deref(),
            Some(sha256_hex("task-line").as_str())
        );
        assert!(attempt.original_diagnostic.is_none());
    }

    #[test]
    fn minimal_runner_response_deserializes_with_defaults() -> Result<()> {
        let line = r#"{"task_id":"T","replacement":"  rfl"}"#;
        let parsed: RunnerResponse = serde_json::from_str(line)?;
        assert_eq!(parsed.task_id, "T");
        assert_eq!(parsed.attempt_id, DEFAULT_ATTEMPT_ID);
        assert_eq!(parsed.replacement, "  rfl");
        assert!(parsed.model.is_none());
        assert!(parsed.metadata.is_none());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_eval_streams_tasks_through_the_example_runner() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        let runner = Utf8PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/echo_runner.sh"
        ));
        // Make sure the shipped example stays executable.
        let mut perms = std::fs::metadata(runner.as_std_path())?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(runner.as_std_path(), perms)?;

        let tasks = vec![
            sample_task("A.foo:2", 2, false),
            sample_task("B.bar:3", 3, true),
        ];
        let dir = TempDir::new()?;
        let out = Utf8PathBuf::from_path_buf(dir.path().join("attempts.jsonl"))
            .map_err(|path| Error::NonUtf8Path { path })?;

        let options = EvalOptions {
            runner,
            lake_root: Utf8PathBuf::from("."),
            timeout: Duration::from_secs(30),
        };
        let mut writer = JsonlWriter::create(&out)?;
        let summary = run_eval(&options, &tasks, &mut writer).await?;

        assert_eq!(summary.tasks_read, 2);
        assert_eq!(summary.attempts_written, 2);
        assert_eq!(summary.runner_errors, 0);
        assert_eq!(summary.id_mismatches, 0);

        let content = std::fs::read_to_string(out.as_std_path())?;
        let mut attempts = Vec::new();
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            attempts.push(serde_json::from_str::<Attempt>(line)?);
        }
        assert_eq!(attempts.len(), 2);
        // task_id comes from the sent task, replacement from the runner.
        assert_eq!(attempts[0].task_id, "A.foo:2");
        assert_eq!(attempts[1].task_id, "B.bar:3");
        assert_eq!(attempts[0].replacement, "  rfl");
        assert_eq!(attempts[0].model.as_deref(), Some("echo-runner"));
        // prompt_hash falls back to a hash of the sent task line.
        assert!(attempts[0].prompt_hash.is_some());
        // The error task carries its diagnostic through for replay scoring.
        assert!(attempts[1].original_diagnostic.is_some());
        Ok(())
    }
}
