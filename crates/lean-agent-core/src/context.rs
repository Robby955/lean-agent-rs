//! Building a compact, high-signal context bundle around one line of a Lean file.
//!
//! The daily speedup this replaces is hand-copying imports, the enclosing
//! theorem, the compiler errors, and the goal state into a chat prompt. Given a
//! `FILE.lean:LINE` target, [`gather_context`] reads the source, optionally
//! traces the file once for diagnostics and goal state, and returns a
//! [`ContextBundle`] that serializes to JSON or renders to Markdown.
//!
//! The bundle builder [`build_context`] is pure: it takes already-read source
//! text plus optional diagnostics, so it is fully testable without touching the
//! filesystem or spawning Lean.

use crate::{
    Diagnostic, GoalState, LeanFile, Provenance, Result, TraceConfig, capture_provenance,
    run_lean_file,
};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Lean keywords that introduce a top-level declaration.
const NAMED_DECL_KEYWORDS: &[&str] = &[
    "theorem",
    "lemma",
    "def",
    "abbrev",
    "instance",
    "example",
    "structure",
    "inductive",
    "class",
    "opaque",
    "axiom",
];

/// Modifiers that may precede a declaration keyword on the same line.
const DECL_MODIFIERS: &[&str] = &[
    "private",
    "protected",
    "noncomputable",
    "partial",
    "unsafe",
    "scoped",
    "local",
    "nonrec",
];

/// Column-zero keywords that close the current declaration's text block.
const BOUNDARY_KEYWORDS: &[&str] = &[
    "namespace",
    "section",
    "end",
    "open",
    "import",
    "variable",
    "universe",
    "set_option",
    "attribute",
    "mutual",
    "macro",
    "macro_rules",
    "syntax",
    "notation",
    "elab",
    "deriving",
];

/// Where to center a context bundle and how much surrounding source to include.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextRequest {
    /// Target Lean source file.
    pub file: LeanFile,
    /// One-based line the bundle is centered on.
    pub line: u32,
    /// Source lines to include before the target line.
    pub before: usize,
    /// Source lines to include after the target line.
    pub after: usize,
}

impl ContextRequest {
    /// Build a request with the default eight-line window on each side.
    #[must_use]
    pub const fn new(file: LeanFile, line: u32) -> Self {
        Self {
            file,
            line,
            before: 8,
            after: 8,
        }
    }
}

/// Runtime options for [`gather_context`] when a live Lean trace is wanted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextOptions {
    /// Run `lake lean` over the file to attach diagnostics and goal state.
    pub run_trace: bool,
    /// Lake workspace root used for the trace and provenance probes.
    pub lake_root: Utf8PathBuf,
    /// Process timeout for the trace.
    pub timeout: Duration,
    /// Keep warning diagnostics in the bundle.
    pub include_warnings: bool,
}

impl ContextOptions {
    /// Tracing options rooted at `lake_root` with a sixty-second timeout.
    #[must_use]
    pub fn new(lake_root: Utf8PathBuf) -> Self {
        Self {
            run_trace: true,
            lake_root,
            timeout: Duration::from_secs(60),
            include_warnings: true,
        }
    }
}

/// The enclosing declaration detected around the target line.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Declaration {
    /// Declaration keyword, such as `theorem` or `def`.
    pub kind: String,
    /// Declared name when one is present (absent for `example`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// One-based first line of the declaration text.
    pub start_line: u32,
    /// One-based last line of the declaration text.
    pub end_line: u32,
    /// Verbatim declaration source.
    pub source: String,
}

/// One numbered line of the surrounding source window.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceLine {
    /// One-based line number in the file.
    pub number: u32,
    /// Line text without its trailing newline.
    pub text: String,
    /// Whether this is the requested target line.
    pub is_target: bool,
}

/// A window of source lines centered on the target line.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceWindow {
    /// One-based first line in the window.
    pub start_line: u32,
    /// One-based last line in the window.
    pub end_line: u32,
    /// One-based target line the window is centered on.
    pub target_line: u32,
    /// The numbered window lines.
    pub lines: Vec<SourceLine>,
}

/// A compact context bundle around one line of a Lean file.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextBundle {
    /// Source file the bundle describes.
    pub file: LeanFile,
    /// One-based target line, clamped into the file's range.
    pub line: u32,
    /// Total number of source lines in the file.
    pub total_lines: usize,
    /// Enclosing declaration, when one is detectable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Declaration>,
    /// Import lines found in the file, in source order.
    pub imports: Vec<String>,
    /// Surrounding source window.
    pub surrounding: SourceWindow,
    /// Diagnostics relevant to the target line, when a trace was run.
    pub diagnostics: Vec<Diagnostic>,
    /// Goal state extracted from a relevant diagnostic, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_state: Option<GoalState>,
    /// Tooling provenance, when a trace was run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Ready-to-paste prompt assembled from the fields above.
    pub suggested_prompt: String,
}

impl ContextBundle {
    /// Render the bundle as Markdown for human review or a chat paste.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        render_markdown(self)
    }
}

/// Parse a `FILE.lean:LINE` target into its path and one-based line.
///
/// The split is on the final colon so Windows-style drive letters and paths
/// with colons in earlier segments still parse.
pub fn parse_file_line_spec(spec: &str) -> Result<(Utf8PathBuf, u32)> {
    let (path, line) = spec
        .rsplit_once(':')
        .ok_or_else(|| crate::Error::InvalidLineSpec {
            spec: spec.to_owned(),
        })?;
    let line: u32 = line.parse().map_err(|_| crate::Error::InvalidLineSpec {
        spec: spec.to_owned(),
    })?;
    if path.is_empty() || line == 0 {
        return Err(crate::Error::InvalidLineSpec {
            spec: spec.to_owned(),
        });
    }
    Ok((Utf8PathBuf::from(path), line))
}

/// Read the file, optionally trace it once, and assemble a [`ContextBundle`].
pub async fn gather_context(
    request: &ContextRequest,
    options: &ContextOptions,
) -> Result<ContextBundle> {
    let source = std::fs::read_to_string(request.file.as_path())?;

    let (diagnostics, provenance) = if options.run_trace {
        let provenance = capture_provenance(options.lake_root.as_path()).await;
        let mut config = TraceConfig::new(options.lake_root.clone()).timeout(options.timeout);
        config.include_warnings = options.include_warnings;
        let trace = run_lean_file(&config, &provenance, request.file.clone()).await;
        (trace.diagnostics, Some(provenance))
    } else {
        (Vec::new(), None)
    };

    Ok(build_context(
        request,
        &source,
        &diagnostics,
        provenance.as_ref(),
    ))
}

/// Assemble a bundle from already-read source and optional trace results.
///
/// This is the pure core: no filesystem access and no process spawning, so it
/// is the right entry point for tests and for callers that already hold the
/// source text and diagnostics.
#[must_use]
pub fn build_context(
    request: &ContextRequest,
    source: &str,
    diagnostics: &[Diagnostic],
    provenance: Option<&Provenance>,
) -> ContextBundle {
    let lines: Vec<&str> = source.lines().collect();
    let total_lines = lines.len();
    let target_line = clamp_line(request.line, total_lines);
    let target_idx = (target_line as usize).saturating_sub(1);

    let imports = collect_imports(&lines);
    let declaration = detect_declaration(&lines, target_idx);
    let surrounding = build_window(&lines, target_idx, request.before, request.after);
    let (selected, goal_state) = select_diagnostics(diagnostics, target_line);

    let suggested_prompt = build_prompt(
        request.file.as_path().as_str(),
        target_line,
        total_lines,
        declaration.as_ref(),
        &imports,
        &selected,
        goal_state.as_ref(),
    );

    ContextBundle {
        file: request.file.clone(),
        line: target_line,
        total_lines,
        declaration,
        imports,
        surrounding,
        diagnostics: selected,
        goal_state,
        provenance: provenance.cloned(),
        suggested_prompt,
    }
}

/// Clamp a one-based line into `[1, total]`, treating an empty file as line one.
fn clamp_line(line: u32, total: usize) -> u32 {
    if total == 0 {
        return 1;
    }
    let max = total as u32;
    line.clamp(1, max)
}

/// Collect `import` lines (those starting at column zero) in source order.
pub fn collect_imports(lines: &[&str]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| is_col0(line) && first_token(line) == Some("import"))
        .map(|line| line.trim_end().to_owned())
        .collect()
}

/// True when a line starts a top-level command (no leading whitespace).
fn is_col0(line: &str) -> bool {
    !line.is_empty() && !line.starts_with(char::is_whitespace)
}

/// First whitespace-delimited token of a line, if any.
fn first_token(line: &str) -> Option<&str> {
    line.split_whitespace().next()
}

/// Declaration keyword for a column-zero line, skipping leading modifiers.
fn decl_kind_of_line(line: &str) -> Option<&str> {
    if !is_col0(line) {
        return None;
    }
    let keyword = line
        .split_whitespace()
        .find(|token| !DECL_MODIFIERS.contains(token))?;
    NAMED_DECL_KEYWORDS.contains(&keyword).then_some(keyword)
}

/// True when a column-zero line ends the current declaration's text block.
fn is_boundary(line: &str) -> bool {
    if !is_col0(line) {
        return false;
    }
    if line.starts_with("@[") {
        return true;
    }
    let Some(token) = first_token(line) else {
        return false;
    };
    if token.starts_with('#') {
        return true;
    }
    BOUNDARY_KEYWORDS.contains(&token) || decl_kind_of_line(line).is_some()
}

/// Detect the declaration enclosing `target_idx`, if any.
///
/// `target_idx` is a zero-based line index. The returned [`Declaration`] carries
/// one-based start and end lines plus the verbatim source of the block.
pub fn detect_declaration(lines: &[&str], target_idx: usize) -> Option<Declaration> {
    if lines.is_empty() {
        return None;
    }
    let scan_from = target_idx.min(lines.len() - 1);
    let mut keyword_idx = None;
    for idx in (0..=scan_from).rev() {
        if decl_kind_of_line(lines[idx]).is_some() {
            keyword_idx = Some(idx);
            break;
        }
        if is_boundary(lines[idx]) {
            // A boundary above the cursor with no enclosing decl means the
            // cursor sits between declarations.
            break;
        }
    }
    let keyword_idx = keyword_idx?;
    let kind = decl_kind_of_line(lines[keyword_idx])?.to_owned();

    // Absorb attribute lines directly above the keyword into the declaration.
    let mut start = keyword_idx;
    while start > 0 {
        let prev = lines[start - 1];
        if is_col0(prev) && prev.starts_with("@[") {
            start -= 1;
        } else {
            break;
        }
    }

    // The block runs until the next top-level boundary or end of file.
    let mut end = lines.len();
    for (offset, line) in lines.iter().enumerate().skip(keyword_idx + 1) {
        if is_boundary(line) {
            end = offset;
            break;
        }
    }

    // Trim trailing blank lines from the captured block.
    while end > keyword_idx + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }

    let source = lines[start..end].join("\n");
    Some(Declaration {
        name: decl_name(lines[keyword_idx], &kind),
        kind,
        start_line: (start as u32) + 1,
        end_line: end as u32,
        source,
    })
}

/// Extract the declared name from a keyword line, when one is present.
fn decl_name(line: &str, kind: &str) -> Option<String> {
    if kind == "example" {
        return None;
    }
    let mut tokens = line.split_whitespace().skip_while(|t| t != &kind);
    let _keyword = tokens.next()?;
    let candidate = tokens.next()?;
    let cut = candidate
        .find(['(', '{', '[', ':'])
        .unwrap_or(candidate.len());
    let name = &candidate[..cut];
    let valid = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_');
    valid.then(|| name.to_owned())
}

/// Build the surrounding source window.
fn build_window(lines: &[&str], target_idx: usize, before: usize, after: usize) -> SourceWindow {
    if lines.is_empty() {
        return SourceWindow {
            start_line: 1,
            end_line: 0,
            target_line: 1,
            lines: Vec::new(),
        };
    }
    let start = target_idx.saturating_sub(before);
    let end = (target_idx + after + 1).min(lines.len());
    let window = lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, text)| {
            let number = (start + offset + 1) as u32;
            SourceLine {
                number,
                text: (*text).to_owned(),
                is_target: start + offset == target_idx,
            }
        })
        .collect();
    SourceWindow {
        start_line: (start as u32) + 1,
        end_line: end as u32,
        target_line: (target_idx as u32) + 1,
        lines: window,
    }
}

/// Pick diagnostics for the target line, falling back to all file diagnostics.
fn select_diagnostics(all: &[Diagnostic], line: u32) -> (Vec<Diagnostic>, Option<GoalState>) {
    let on_line: Vec<Diagnostic> = all
        .iter()
        .filter(|d| d.line == Some(line))
        .cloned()
        .collect();
    let chosen = if on_line.is_empty() {
        all.to_vec()
    } else {
        on_line
    };
    let goal = chosen
        .iter()
        .find_map(|d| d.goal_state.clone())
        .or_else(|| chosen.iter().find_map(|d| goal_from_message(&d.message)));
    (chosen, goal)
}

/// Recover a goal block from a diagnostic message that the upstream parser did
/// not tag (for example a `Tactic ... failed` error that still prints `⊢`).
///
/// The block is the turnstile line plus the contiguous local-context lines
/// directly above it, stopping at the first blank line.
fn goal_from_message(message: &str) -> Option<GoalState> {
    let lines: Vec<&str> = message.lines().collect();
    let goal_idx = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with('⊢'))?;
    let mut start = goal_idx;
    while start > 0 && !lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    let block = lines[start..=goal_idx].join("\n");
    let block = block.trim();
    (!block.is_empty()).then(|| GoalState(block.to_owned()))
}

/// Assemble the ready-to-paste prompt from the bundle's parts.
fn build_prompt(
    file: &str,
    line: u32,
    total_lines: usize,
    declaration: Option<&Declaration>,
    imports: &[String],
    diagnostics: &[Diagnostic],
    goal_state: Option<&GoalState>,
) -> String {
    let mut out = String::new();
    out.push_str("You are editing a Lean 4 proof.\n\n");
    out.push_str(&format!("File: {file} (line {line} of {total_lines})\n"));

    match declaration {
        Some(decl) => {
            let named = decl
                .name
                .as_deref()
                .map_or_else(|| decl.kind.clone(), |name| format!("{} {name}", decl.kind));
            out.push_str(&format!("Declaration: {named}\n"));
        }
        None => out.push_str("Declaration: not detected\n"),
    }

    out.push('\n');
    if imports.is_empty() {
        out.push_str("Imports in scope: none found in this file.\n");
    } else {
        out.push_str("Imports in scope:\n");
        for import in imports {
            out.push_str(import);
            out.push('\n');
        }
    }

    if let Some(decl) = declaration {
        out.push_str("\nCurrent declaration:\n");
        out.push_str(&decl.source);
        out.push('\n');
    }

    if let Some(goal) = goal_state {
        out.push_str("\nGoal state at the target line:\n");
        out.push_str(&goal.0);
        out.push('\n');
    }

    if diagnostics.is_empty() {
        out.push_str("\nCompiler diagnostics: none captured.\n");
    } else {
        out.push_str("\nCompiler diagnostics:\n");
        for diagnostic in diagnostics {
            let first = diagnostic.message.lines().next().unwrap_or("").trim();
            out.push_str(&format!("- {first}\n"));
        }
    }

    out.push_str(
        "\nTask: rewrite the declaration so the file compiles with no errors. \
Return only the corrected Lean source for this declaration. Keep the existing \
name and signature unless the error requires a change, and do not add imports \
unless they are needed.\n",
    );
    out
}

/// Render a [`ContextBundle`] as Markdown.
fn render_markdown(bundle: &ContextBundle) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Lean context: {}\n\n", bundle.file));
    out.push_str(&format!(
        "Target line **{}** of {} total.\n\n",
        bundle.line, bundle.total_lines
    ));

    if let Some(decl) = &bundle.declaration {
        let named = decl.name.as_deref().map_or_else(
            || decl.kind.clone(),
            |name| format!("{} `{name}`", decl.kind),
        );
        out.push_str(&format!(
            "## Declaration\n\n{named} (lines {}-{})\n\n```lean\n{}\n```\n\n",
            decl.start_line, decl.end_line, decl.source
        ));
    }

    out.push_str("## Imports\n\n");
    if bundle.imports.is_empty() {
        out.push_str("None found in this file.\n\n");
    } else {
        out.push_str("```lean\n");
        for import in &bundle.imports {
            out.push_str(import);
            out.push('\n');
        }
        out.push_str("```\n\n");
    }

    out.push_str("## Surrounding source\n\n```lean\n");
    for line in &bundle.surrounding.lines {
        let marker = if line.is_target { ">" } else { " " };
        out.push_str(&format!("{marker} {:>4} | {}\n", line.number, line.text));
    }
    out.push_str("```\n\n");

    if let Some(goal) = &bundle.goal_state {
        out.push_str(&format!("## Goal state\n\n```\n{}\n```\n\n", goal.0));
    }

    out.push_str("## Diagnostics\n\n");
    if bundle.diagnostics.is_empty() {
        out.push_str("None captured.\n\n");
    } else {
        for diagnostic in &bundle.diagnostics {
            let first = diagnostic.message.lines().next().unwrap_or("").trim();
            out.push_str(&format!("- {first}\n"));
        }
        out.push('\n');
    }

    if let Some(provenance) = &bundle.provenance {
        out.push_str("## Provenance\n\n");
        out.push_str(&format!(
            "- lean: {}\n- lake: {}\n- git: {}\n\n",
            provenance.lean_version.as_deref().unwrap_or("unknown"),
            provenance.lake_version.as_deref().unwrap_or("unknown"),
            provenance.git_commit.as_deref().unwrap_or("none"),
        ));
    }

    out.push_str("## Suggested prompt\n\n```\n");
    out.push_str(&bundle.suggested_prompt);
    out.push_str("```\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DiagnosticSeverity;

    const SAMPLE: &str = "import Init\nimport Mathlib.Tactic\n\nnamespace Demo\n\n@[simp]\ntheorem foo (n : Nat) : n = n := by\n  rfl\n\ndef bar : Nat := 0\n\nend Demo\n";

    fn request(line: u32) -> ContextRequest {
        let file = LeanFile(Utf8PathBuf::from("Demo.lean"));
        ContextRequest::new(file, line)
    }

    #[test]
    fn parses_file_line_spec() -> Result<()> {
        let (path, line) = parse_file_line_spec("src/Demo.lean:42")?;
        assert_eq!(path, Utf8PathBuf::from("src/Demo.lean"));
        assert_eq!(line, 42);
        Ok(())
    }

    #[test]
    fn rejects_bad_line_spec() {
        assert!(parse_file_line_spec("no-line-here").is_err());
        assert!(parse_file_line_spec("file.lean:0").is_err());
        assert!(parse_file_line_spec("file.lean:abc").is_err());
        assert!(parse_file_line_spec(":12").is_err());
    }

    #[test]
    fn collects_imports() {
        let bundle = build_context(&request(7), SAMPLE, &[], None);
        assert_eq!(bundle.imports, vec!["import Init", "import Mathlib.Tactic"]);
    }

    #[test]
    fn detects_enclosing_theorem_with_attribute() -> Result<()> {
        let bundle = build_context(&request(8), SAMPLE, &[], None);
        let Some(decl) = bundle.declaration else {
            return Err(crate::Error::Todo {
                feature: "expected a declaration",
            });
        };
        assert_eq!(decl.kind, "theorem");
        assert_eq!(decl.name.as_deref(), Some("foo"));
        // The attribute line above the keyword is absorbed.
        assert_eq!(decl.start_line, 6);
        assert!(decl.source.starts_with("@[simp]"));
        assert!(decl.source.contains("rfl"));
        // The block stops before the next declaration.
        assert!(!decl.source.contains("def bar"));
        Ok(())
    }

    #[test]
    fn detects_following_def() -> Result<()> {
        let bundle = build_context(&request(10), SAMPLE, &[], None);
        let Some(decl) = bundle.declaration else {
            return Err(crate::Error::Todo {
                feature: "expected a declaration",
            });
        };
        assert_eq!(decl.kind, "def");
        assert_eq!(decl.name.as_deref(), Some("bar"));
        Ok(())
    }

    #[test]
    fn window_marks_target_and_respects_bounds() {
        let mut req = request(7);
        req.before = 2;
        req.after = 1;
        let bundle = build_context(&req, SAMPLE, &[], None);
        assert_eq!(bundle.surrounding.start_line, 5);
        assert_eq!(bundle.surrounding.end_line, 8);
        let target: Vec<_> = bundle
            .surrounding
            .lines
            .iter()
            .filter(|l| l.is_target)
            .collect();
        assert_eq!(target.len(), 1);
        assert_eq!(target[0].number, 7);
    }

    #[test]
    fn clamps_out_of_range_line() {
        let bundle = build_context(&request(9999), SAMPLE, &[], None);
        assert_eq!(bundle.total_lines, 12);
        assert_eq!(bundle.line, 12);
    }

    #[test]
    fn selects_diagnostics_on_target_line_and_extracts_goal() {
        let diags = vec![
            Diagnostic {
                file: Some(Utf8PathBuf::from("Demo.lean")),
                line: Some(8),
                column: Some(2),
                severity: DiagnosticSeverity::Error,
                message: "error: unsolved goals\n⊢ n = n".to_owned(),
                goal_state: Some(GoalState("⊢ n = n".to_owned())),
            },
            Diagnostic {
                file: Some(Utf8PathBuf::from("Demo.lean")),
                line: Some(99),
                column: None,
                severity: DiagnosticSeverity::Warning,
                message: "warning: elsewhere".to_owned(),
                goal_state: None,
            },
        ];
        let bundle = build_context(&request(8), SAMPLE, &diags, None);
        assert_eq!(bundle.diagnostics.len(), 1);
        assert_eq!(bundle.diagnostics[0].line, Some(8));
        assert_eq!(
            bundle.goal_state.as_ref().map(|g| g.0.as_str()),
            Some("⊢ n = n")
        );
        assert!(bundle.suggested_prompt.contains("Goal state"));
    }

    #[test]
    fn recovers_goal_from_untagged_message() {
        let diags = vec![Diagnostic {
            file: Some(Utf8PathBuf::from("Demo.lean")),
            line: Some(7),
            column: Some(2),
            severity: DiagnosticSeverity::Error,
            message: "error: Tactic `rfl` failed: the sides differ\n  n + m\n\nn m : Nat\n⊢ n + m = m + n".to_owned(),
            goal_state: None,
        }];
        let bundle = build_context(&request(7), SAMPLE, &diags, None);
        assert_eq!(
            bundle.goal_state.as_ref().map(|g| g.0.as_str()),
            Some("n m : Nat\n⊢ n + m = m + n")
        );
    }

    #[test]
    fn empty_file_is_handled() {
        let bundle = build_context(&request(1), "", &[], None);
        assert_eq!(bundle.total_lines, 0);
        assert!(bundle.declaration.is_none());
        assert!(bundle.surrounding.lines.is_empty());
    }

    #[test]
    fn markdown_includes_key_sections() {
        let bundle = build_context(&request(8), SAMPLE, &[], None);
        let md = bundle.to_markdown();
        assert!(md.contains("# Lean context"));
        assert!(md.contains("## Declaration"));
        assert!(md.contains("## Imports"));
        assert!(md.contains("## Surrounding source"));
        assert!(md.contains("## Suggested prompt"));
        assert!(md.contains("> "));
    }

    #[test]
    fn prompt_opens_with_editing_lead() {
        let bundle = build_context(&request(8), SAMPLE, &[], None);
        assert!(
            bundle
                .suggested_prompt
                .starts_with("You are editing a Lean 4 proof.")
        );
    }
}
