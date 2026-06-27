//! Mining replayable proof tasks out of a Lean project.
//!
//! A task points at one precise span the agent is allowed to rewrite: a `sorry`
//! or `admit` placeholder, or the declaration around a compiler error. Each
//! [`MineTask`] carries enough context (imports, enclosing declaration, the goal
//! state when available) plus an exact `target_span` and `allowed_edit` so a
//! later step can splice a candidate proof back in safely.
//!
//! Placeholder mining (`sorry`, `admit`) is a pure text scan that skips comments
//! and string literals, so it needs no Lean toolchain. Error mining runs the
//! file through the tracer and is therefore backed by real diagnostics.

use crate::writer::JsonlWriter;
use crate::{
    Declaration, Diagnostic, DiagnosticSeverity, GoalState, LeanFile, Provenance, Result,
    TraceConfig, capture_provenance, collect_imports, detect_declaration, discover_lean_files,
    run_lean_file,
};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Duration;
use tracing::{info, warn};

/// Standing instruction attached to every mined task.
const INSTRUCTIONS: &str = "Replace only the target span with a Lean proof that compiles.";

/// Default discovery skip pattern so build output is never mined.
const DEFAULT_EXCLUDE: &str = ".lake/";

/// What a mine run looks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MineKind {
    /// The `sorry` placeholder term/tactic.
    Sorry,
    /// The `admit` placeholder tactic.
    Admit,
    /// A compiler error, backed by a tracer diagnostic.
    Error,
}

impl MineKind {
    /// Placeholder keyword for the text-scan kinds, or `None` for [`MineKind::Error`].
    #[must_use]
    pub const fn placeholder_keyword(self) -> Option<&'static str> {
        match self {
            Self::Sorry => Some("sorry"),
            Self::Admit => Some("admit"),
            Self::Error => None,
        }
    }
}

/// The exact span a candidate proof must replace.
///
/// Lines are one-based; columns are zero-based codepoint offsets within a line,
/// with `end_column` exclusive. `text` is the verbatim current content of the
/// span, so `source_before + text + source_after` reproduces the file byte for
/// byte.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TargetSpan {
    /// One-based first line of the span.
    pub start_line: u32,
    /// Zero-based first column of the span.
    pub start_column: u32,
    /// One-based last line of the span.
    pub end_line: u32,
    /// Zero-based exclusive end column on the last line.
    pub end_column: u32,
    /// Verbatim current text of the span.
    pub text: String,
}

/// The single contiguous line range a replay step is permitted to edit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AllowedEdit {
    /// File the edit applies to.
    pub file: Utf8PathBuf,
    /// One-based first editable line.
    pub start_line: u32,
    /// One-based last editable line.
    pub end_line: u32,
}

/// One mined, replayable proof task.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MineTask {
    /// Stable identifier, shaped `Module.decl:line` (or `Module:line` when no
    /// declaration name is detected), with a `:column` suffix on collision.
    pub task_id: String,
    /// Project the task was mined from.
    pub project: String,
    /// Source file holding the span.
    pub file: LeanFile,
    /// Enclosing declaration, when one is detectable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Declaration>,
    /// What this task targets.
    pub kind: MineKind,
    /// One-based line of the placeholder token or error site.
    pub line: u32,
    /// Zero-based column of the placeholder token or error site.
    pub column: u32,
    /// Import lines in scope, in source order.
    pub imports: Vec<String>,
    /// File text before the target span.
    pub source_before: String,
    /// The exact span to replace.
    pub target_span: TargetSpan,
    /// File text after the target span.
    pub source_after: String,
    /// Backing diagnostic for error tasks; absent for placeholder tasks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
    /// Goal state when the backing diagnostic exposed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_state: Option<GoalState>,
    /// The single span the replay step may edit.
    pub allowed_edit: AllowedEdit,
    /// Standing instruction for the agent.
    pub instructions: String,
}

/// Runtime options for a mine run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MineOptions {
    /// What to mine.
    pub kind: MineKind,
    /// Project name stamped onto every task.
    pub project: String,
    /// Lake workspace root, also used to derive module names.
    pub lake_root: Utf8PathBuf,
    /// Search directories recursively.
    pub recursive: bool,
    /// Per-file Lean timeout for error mining.
    pub timeout: Duration,
    /// Path substrings to skip during discovery.
    pub exclude: Vec<String>,
}

impl MineOptions {
    /// Build options for `kind` rooted at `lake_root`, with the build directory
    /// excluded and a sixty-second per-file timeout.
    #[must_use]
    pub fn new(kind: MineKind, project: String, lake_root: Utf8PathBuf) -> Self {
        Self {
            kind,
            project,
            lake_root,
            recursive: false,
            timeout: Duration::from_secs(60),
            exclude: vec![DEFAULT_EXCLUDE.to_owned()],
        }
    }
}

/// Outcome counts from a single mine run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MineSummary {
    /// Lean files read and scanned.
    pub files_scanned: usize,
    /// Task records written.
    pub tasks_written: usize,
}

/// Discover files under `roots`, mine tasks of the requested kind, and stream
/// them to `writer`.
pub async fn run_mine(
    options: &MineOptions,
    roots: &[Utf8PathBuf],
    writer: &mut JsonlWriter,
) -> Result<MineSummary> {
    let files = collect_files(options, roots)?;
    info!(count = files.len(), kind = ?options.kind, "discovered Lean files");

    let provenance = match options.kind {
        MineKind::Error => capture_provenance(options.lake_root.as_path()).await,
        MineKind::Sorry | MineKind::Admit => Provenance::default(),
    };

    let mut summary = MineSummary::default();
    for file in files {
        let source = match std::fs::read_to_string(file.as_path()) {
            Ok(source) => source,
            Err(err) => {
                warn!(%file, error = %err, "skipping unreadable file");
                continue;
            }
        };
        summary.files_scanned += 1;

        let tasks = match options.kind {
            MineKind::Sorry | MineKind::Admit => mine_placeholders(
                &file,
                &source,
                options.kind,
                &options.project,
                options.lake_root.as_path(),
            ),
            MineKind::Error => {
                let mut config =
                    TraceConfig::new(options.lake_root.clone()).timeout(options.timeout);
                config.include_warnings = false;
                let trace = run_lean_file(&config, &provenance, file.clone()).await;
                mine_errors(
                    &file,
                    &source,
                    &trace.diagnostics,
                    &options.project,
                    options.lake_root.as_path(),
                )
            }
        };

        for task in &tasks {
            writer.write_record(task)?;
            summary.tasks_written += 1;
        }
    }

    writer.flush()?;
    Ok(summary)
}

/// Mine `sorry`/`admit` placeholder tasks from one file's source.
///
/// This is a pure text scan: no filesystem access beyond the passed source and
/// no Lean process. Returns an empty vector for [`MineKind::Error`].
#[must_use]
pub fn mine_placeholders(
    file: &LeanFile,
    source: &str,
    kind: MineKind,
    project: &str,
    root: &Utf8Path,
) -> Vec<MineTask> {
    let Some(keyword) = kind.placeholder_keyword() else {
        return Vec::new();
    };
    let lines: Vec<&str> = source.lines().collect();
    let imports = collect_imports(&lines);
    let module = module_name(file.as_path(), root);
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut tasks = Vec::new();

    for hit in scan_placeholders(source, keyword) {
        let target_idx = (hit.line.saturating_sub(1)) as usize;
        let declaration = detect_declaration(&lines, target_idx);
        let span = SpanBytes {
            start_byte: hit.byte_start,
            end_byte: hit.byte_end,
            start_line: hit.line,
            start_column: hit.column,
            end_line: hit.line,
            end_column: hit.end_column,
        };
        tasks.push(build_task(
            file,
            source,
            project,
            &module,
            &imports,
            declaration,
            kind,
            hit.line,
            hit.column,
            &span,
            None,
            None,
            &mut seen_ids,
        ));
    }
    tasks
}

/// Mine error tasks from one file's source plus its tracer diagnostics.
///
/// Each error diagnostic maps to the enclosing declaration (or the error line
/// when no declaration is found). Diagnostics that share a span are de-duplicated
/// so one broken declaration yields one task.
#[must_use]
pub fn mine_errors(
    file: &LeanFile,
    source: &str,
    diagnostics: &[Diagnostic],
    project: &str,
    root: &Utf8Path,
) -> Vec<MineTask> {
    let lines: Vec<&str> = source.lines().collect();
    let spans = line_content_spans(source);
    let imports = collect_imports(&lines);
    let module = module_name(file.as_path(), root);
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut seen_spans: HashSet<(usize, usize)> = HashSet::new();
    let mut tasks = Vec::new();

    for diag in diagnostics
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::Error)
    {
        if let Some(diag_file) = &diag.file {
            if diag_file.file_name() != file.as_path().file_name() {
                continue;
            }
        }
        let Some(line) = diag.line else { continue };
        let target_idx = (line.saturating_sub(1)) as usize;
        if target_idx >= spans.len() {
            continue;
        }

        let declaration = detect_declaration(&lines, target_idx);
        let span = match declaration_span(declaration.as_ref(), line, source, &spans) {
            Some(span) => span,
            None => continue,
        };
        if !seen_spans.insert((span.start_byte, span.end_byte)) {
            continue;
        }

        let column = diag.column.unwrap_or(0);
        tasks.push(build_task(
            file,
            source,
            project,
            &module,
            &imports,
            declaration,
            MineKind::Error,
            line,
            column,
            &span,
            Some(diag.clone()),
            diag.goal_state.clone(),
            &mut seen_ids,
        ));
    }
    tasks
}

/// Byte and line/column coordinates of one span within a source string.
struct SpanBytes {
    start_byte: usize,
    end_byte: usize,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

/// Resolve the span to replace for an error: the enclosing declaration when one
/// is found, otherwise the single error line.
fn declaration_span(
    declaration: Option<&Declaration>,
    error_line: u32,
    source: &str,
    spans: &[(usize, usize)],
) -> Option<SpanBytes> {
    let (start_line, end_line) = match declaration {
        Some(decl) => (decl.start_line, decl.end_line),
        None => (error_line, error_line),
    };
    let start = spans.get((start_line.saturating_sub(1)) as usize)?;
    let end = spans.get((end_line.saturating_sub(1)) as usize)?;
    let end_column = codepoint_len(source.get(end.0..end.1).unwrap_or(""));
    Some(SpanBytes {
        start_byte: start.0,
        end_byte: end.1,
        start_line,
        start_column: 0,
        end_line,
        end_column,
    })
}

/// Assemble one task record from a resolved span.
#[allow(clippy::too_many_arguments)]
fn build_task(
    file: &LeanFile,
    source: &str,
    project: &str,
    module: &str,
    imports: &[String],
    declaration: Option<Declaration>,
    kind: MineKind,
    line: u32,
    column: u32,
    span: &SpanBytes,
    diagnostic: Option<Diagnostic>,
    goal_state: Option<GoalState>,
    seen_ids: &mut HashSet<String>,
) -> MineTask {
    let text = source
        .get(span.start_byte..span.end_byte)
        .unwrap_or("")
        .to_owned();
    let source_before = source.get(..span.start_byte).unwrap_or("").to_owned();
    let source_after = source.get(span.end_byte..).unwrap_or("").to_owned();

    let decl_name = declaration.as_ref().and_then(|decl| decl.name.clone());
    let base_id = match &decl_name {
        Some(name) => format!("{module}.{name}:{line}"),
        None => format!("{module}:{line}"),
    };
    let task_id = if seen_ids.contains(&base_id) {
        format!("{base_id}:{column}")
    } else {
        base_id
    };
    seen_ids.insert(task_id.clone());

    MineTask {
        task_id,
        project: project.to_owned(),
        file: file.clone(),
        declaration,
        kind,
        line,
        column,
        imports: imports.to_vec(),
        source_before,
        target_span: TargetSpan {
            start_line: span.start_line,
            start_column: span.start_column,
            end_line: span.end_line,
            end_column: span.end_column,
            text,
        },
        source_after,
        diagnostic,
        goal_state,
        allowed_edit: AllowedEdit {
            file: file.as_path().clone(),
            start_line: span.start_line,
            end_line: span.end_line,
        },
        instructions: INSTRUCTIONS.to_owned(),
    }
}

/// Discover, sort, de-duplicate, and exclude-filter files under `roots`.
fn collect_files(options: &MineOptions, roots: &[Utf8PathBuf]) -> Result<Vec<LeanFile>> {
    let mut files = Vec::new();
    for root in roots {
        files.extend(discover_lean_files(root, options.recursive)?);
    }
    files.sort();
    files.dedup();
    files.retain(|file| {
        let path = file.as_path().as_str();
        !options
            .exclude
            .iter()
            .any(|pattern| path.contains(pattern.as_str()))
    });
    Ok(files)
}

/// Derive a dotted module name from a file path relative to the project root.
pub(crate) fn module_name(file: &Utf8Path, root: &Utf8Path) -> String {
    let relative = file.strip_prefix(root).unwrap_or(file);
    let no_extension = relative.with_extension("");
    let parts: Vec<&str> = no_extension
        .components()
        .filter_map(|component| match component {
            Utf8Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        no_extension.as_str().to_owned()
    } else {
        parts.join(".")
    }
}

/// Codepoint length of a string slice.
fn codepoint_len(text: &str) -> u32 {
    text.chars().count() as u32
}

/// Per-line `(content_start_byte, content_end_byte)` pairs, newline excluded.
///
/// Line indexing matches `str::lines`, so index `k` is one-based line `k + 1`.
fn line_content_spans(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            spans.push((start, end));
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        let mut end = bytes.len();
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        spans.push((start, end));
    }
    spans
}

/// A placeholder occurrence found by the scanner.
struct Hit {
    byte_start: usize,
    byte_end: usize,
    line: u32,
    column: u32,
    end_column: u32,
}

/// Lexer state for the placeholder scan.
enum ScanState {
    Normal,
    LineComment,
    BlockComment(u32),
    Str,
}

/// True for characters that continue a Lean identifier-like word.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '\''
}

/// Character at `idx`, if any.
fn peek(chars: &[(usize, char)], idx: usize) -> Option<char> {
    chars.get(idx).map(|&(_, c)| c)
}

/// True when `chars[i..j]` equals `keyword` codepoint for codepoint.
fn word_matches(chars: &[(usize, char)], i: usize, j: usize, keyword: &str) -> bool {
    let expected: Vec<char> = keyword.chars().collect();
    if j - i != expected.len() {
        return false;
    }
    chars[i..j]
        .iter()
        .map(|&(_, c)| c)
        .eq(expected.iter().copied())
}

/// Scan `source` for whole-word `keyword` occurrences outside comments and
/// string literals, skipping qualified uses preceded by `.`.
fn scan_placeholders(source: &str, keyword: &str) -> Vec<Hit> {
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let n = chars.len();
    let mut hits = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut col = 0u32;
    let mut state = ScanState::Normal;

    while i < n {
        let (byte, c) = chars[i];
        match state {
            ScanState::LineComment => {
                if c == '\n' {
                    state = ScanState::Normal;
                    i += 1;
                    line += 1;
                    col = 0;
                } else {
                    i += 1;
                    col += 1;
                }
            }
            ScanState::BlockComment(depth) => {
                if c == '/' && peek(&chars, i + 1) == Some('-') {
                    state = ScanState::BlockComment(depth + 1);
                    i += 2;
                    col += 2;
                } else if c == '-' && peek(&chars, i + 1) == Some('/') {
                    let next = depth - 1;
                    state = if next == 0 {
                        ScanState::Normal
                    } else {
                        ScanState::BlockComment(next)
                    };
                    i += 2;
                    col += 2;
                } else if c == '\n' {
                    i += 1;
                    line += 1;
                    col = 0;
                } else {
                    i += 1;
                    col += 1;
                }
            }
            ScanState::Str => {
                if c == '\\' {
                    i += 1;
                    col += 1;
                    if let Some(escaped) = peek(&chars, i) {
                        if escaped == '\n' {
                            line += 1;
                            col = 0;
                        } else {
                            col += 1;
                        }
                        i += 1;
                    }
                } else if c == '"' {
                    state = ScanState::Normal;
                    i += 1;
                    col += 1;
                } else if c == '\n' {
                    i += 1;
                    line += 1;
                    col = 0;
                } else {
                    i += 1;
                    col += 1;
                }
            }
            ScanState::Normal => {
                if c == '-' && peek(&chars, i + 1) == Some('-') {
                    state = ScanState::LineComment;
                    i += 2;
                    col += 2;
                } else if c == '/' && peek(&chars, i + 1) == Some('-') {
                    state = ScanState::BlockComment(1);
                    i += 2;
                    col += 2;
                } else if c == '"' {
                    state = ScanState::Str;
                    i += 1;
                    col += 1;
                } else if is_word_char(c) {
                    let start_line = line;
                    let start_col = col;
                    let mut j = i;
                    while j < n && is_word_char(chars[j].1) {
                        j += 1;
                    }
                    let word_len = (j - i) as u32;
                    let end_byte = if j < n { chars[j].0 } else { source.len() };
                    let preceded_by_dot = i > 0 && chars[i - 1].1 == '.';
                    if !preceded_by_dot && word_matches(&chars, i, j, keyword) {
                        hits.push(Hit {
                            byte_start: byte,
                            byte_end: end_byte,
                            line: start_line,
                            column: start_col,
                            end_column: start_col + word_len,
                        });
                    }
                    i = j;
                    col += word_len;
                } else if c == '\n' {
                    i += 1;
                    line += 1;
                    col = 0;
                } else {
                    i += 1;
                    col += 1;
                }
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "import Init\n\nnamespace Demo\n\ntheorem foo (n : Nat) : n = n := by\n  sorry\n\ndef bar : Nat := by admit\n\nend Demo\n";

    fn lean(path: &str) -> LeanFile {
        LeanFile(Utf8PathBuf::from(path))
    }

    fn root() -> Utf8PathBuf {
        Utf8PathBuf::from(".")
    }

    #[test]
    fn reconstructs_file_from_before_span_after() {
        let file = lean("Demo.lean");
        let tasks = mine_placeholders(&file, SAMPLE, MineKind::Sorry, "demo", root().as_path());
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        let rebuilt = format!(
            "{}{}{}",
            task.source_before, task.target_span.text, task.source_after
        );
        assert_eq!(rebuilt, SAMPLE);
    }

    #[test]
    fn sorry_task_has_precise_single_span() {
        let file = lean("Demo.lean");
        let tasks = mine_placeholders(&file, SAMPLE, MineKind::Sorry, "demo", root().as_path());
        let task = &tasks[0];
        assert_eq!(task.kind, MineKind::Sorry);
        assert_eq!(task.target_span.text, "sorry");
        assert_eq!(task.line, 6);
        assert_eq!(task.column, 2);
        assert_eq!(task.target_span.start_line, task.target_span.end_line);
        assert_eq!(task.allowed_edit.start_line, 6);
        assert_eq!(task.allowed_edit.end_line, 6);
        assert_eq!(task.target_span.end_column, 7);
        assert_eq!(task.instructions, INSTRUCTIONS);
    }

    #[test]
    fn task_id_uses_module_and_declaration() {
        let file = lean("Demo/Basic.lean");
        let tasks = mine_placeholders(&file, SAMPLE, MineKind::Sorry, "demo", Utf8Path::new("."));
        assert_eq!(tasks[0].task_id, "Demo.Basic.foo:6");
        assert_eq!(tasks[0].imports, vec!["import Init"]);
        let decl = tasks[0].declaration.as_ref().and_then(|d| d.name.clone());
        assert_eq!(decl.as_deref(), Some("foo"));
    }

    #[test]
    fn admit_is_mined_only_for_admit_kind() {
        let file = lean("Demo.lean");
        let sorry_tasks =
            mine_placeholders(&file, SAMPLE, MineKind::Sorry, "demo", root().as_path());
        assert!(sorry_tasks.iter().all(|t| t.target_span.text == "sorry"));
        let admit_tasks =
            mine_placeholders(&file, SAMPLE, MineKind::Admit, "demo", root().as_path());
        assert_eq!(admit_tasks.len(), 1);
        assert_eq!(admit_tasks[0].target_span.text, "admit");
        assert_eq!(admit_tasks[0].line, 8);
    }

    #[test]
    fn skips_placeholders_in_comments_and_strings() {
        let source = "-- a stray sorry here\n/- block sorry -/\ndef s : String := \"sorry\"\ntheorem real : True := by\n  sorry\n";
        let file = lean("S.lean");
        let tasks = mine_placeholders(&file, source, MineKind::Sorry, "demo", root().as_path());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].line, 5);
    }

    #[test]
    fn skips_qualified_and_partial_words() {
        let source = "def a := Foo.sorry\ndef b := sorryAx\ndef c := mysorry\n";
        let file = lean("Q.lean");
        let tasks = mine_placeholders(&file, source, MineKind::Sorry, "demo", root().as_path());
        assert!(tasks.is_empty());
    }

    #[test]
    fn error_task_targets_enclosing_declaration() {
        let source = "import Init\n\ntheorem broken : 1 = 2 := by\n  rfl\n";
        let file = lean("Broken.lean");
        let diag = Diagnostic {
            file: Some(Utf8PathBuf::from("Broken.lean")),
            line: Some(4),
            column: Some(2),
            severity: DiagnosticSeverity::Error,
            message: "error: unsolved goals\n⊢ 1 = 2".to_owned(),
            goal_state: Some(GoalState("⊢ 1 = 2".to_owned())),
        };
        let tasks = mine_errors(&file, source, &[diag], "demo", root().as_path());
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert_eq!(task.kind, MineKind::Error);
        assert_eq!(task.target_span.start_line, 3);
        assert_eq!(task.target_span.end_line, 4);
        assert!(task.target_span.text.contains("theorem broken"));
        assert!(task.target_span.text.contains("rfl"));
        assert_eq!(task.allowed_edit.start_line, 3);
        assert_eq!(task.allowed_edit.end_line, 4);
        assert!(task.goal_state.is_some());
        assert!(task.diagnostic.is_some());
        let rebuilt = format!(
            "{}{}{}",
            task.source_before, task.target_span.text, task.source_after
        );
        assert_eq!(rebuilt, source);
    }

    #[test]
    fn error_tasks_dedup_by_span() {
        let source = "theorem broken : 1 = 2 := by\n  rfl\n";
        let file = lean("Broken.lean");
        let make = |line: u32| Diagnostic {
            file: Some(Utf8PathBuf::from("Broken.lean")),
            line: Some(line),
            column: Some(0),
            severity: DiagnosticSeverity::Error,
            message: "error: something".to_owned(),
            goal_state: None,
        };
        let tasks = mine_errors(&file, source, &[make(1), make(2)], "demo", root().as_path());
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn warnings_are_ignored_for_error_mining() {
        let source = "theorem t : True := trivial\n";
        let file = lean("T.lean");
        let warning = Diagnostic {
            file: Some(Utf8PathBuf::from("T.lean")),
            line: Some(1),
            column: Some(0),
            severity: DiagnosticSeverity::Warning,
            message: "warning: declaration uses 'sorry'".to_owned(),
            goal_state: None,
        };
        let tasks = mine_errors(&file, source, &[warning], "demo", root().as_path());
        assert!(tasks.is_empty());
    }

    #[test]
    fn module_name_drops_root_and_extension() {
        assert_eq!(
            module_name(Utf8Path::new("./Demo/Basic.lean"), Utf8Path::new(".")),
            "Demo.Basic"
        );
        assert_eq!(
            module_name(
                Utf8Path::new("/tmp/proj/Demo.lean"),
                Utf8Path::new("/tmp/proj")
            ),
            "Demo"
        );
    }

    #[test]
    fn line_content_spans_match_str_lines() {
        let source = "abc\n\ndef\n";
        let spans = line_content_spans(source);
        let rebuilt: Vec<&str> = spans.iter().map(|&(a, b)| &source[a..b]).collect();
        let expected: Vec<&str> = source.lines().collect();
        assert_eq!(rebuilt, expected);
    }
}
