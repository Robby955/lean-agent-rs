//! Bounded, line-spanned patch application into an isolated workspace copy.
//!
//! A patch is one contiguous line range in one file, replaced verbatim. The
//! application is deliberately strict: it refuses paths that escape the
//! workspace root and spans that fall outside the target file. Multi-file
//! application is gated behind an explicit flag so the default stays one span in
//! one file, matching the single-span tasks the miner emits.

use crate::{Error, Result};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// One contiguous, line-bounded replacement in a single file.
///
/// Lines are one-based and inclusive. `replacement` becomes the new content for
/// lines `start_line..=end_line`; line content is replaced, while the newline
/// that terminates the last line and all surrounding lines are preserved.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpanReplacement {
    /// File to edit, relative to the workspace root.
    pub file: Utf8PathBuf,
    /// One-based first line to replace.
    pub start_line: u32,
    /// One-based last line to replace.
    pub end_line: u32,
    /// New content spliced over the line range.
    pub replacement: String,
}

/// Audit record of one applied span.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppliedPatch {
    /// File that was edited, relative to the workspace root.
    pub file: Utf8PathBuf,
    /// One-based first line replaced.
    pub start_line: u32,
    /// One-based last line replaced.
    pub end_line: u32,
    /// The text that was replaced, for reversibility and logging.
    pub replaced_text: String,
}

/// Apply one span replacement inside `root`.
///
/// Refuses an `edit.file` that is absolute or contains `..`, a zero or inverted
/// line range, or a range past the end of the file. On success the file on disk
/// is rewritten and the replaced text is returned.
pub fn apply_single_span(root: &Utf8Path, edit: &SpanReplacement) -> Result<AppliedPatch> {
    reject_top_level_command(edit)?;
    let target = resolve_within(root, &edit.file)?;

    if edit.start_line == 0 || edit.end_line == 0 {
        return Err(Error::ZeroLineSpan {
            file: edit.file.clone(),
        });
    }
    if edit.start_line > edit.end_line {
        return Err(Error::InvertedSpan {
            file: edit.file.clone(),
            start_line: edit.start_line,
            end_line: edit.end_line,
        });
    }

    let source = std::fs::read_to_string(&target)?;
    let ranges = line_content_ranges(&source);
    if (edit.end_line as usize) > ranges.len() {
        return Err(Error::SpanOutOfBounds {
            file: edit.file.clone(),
            start_line: edit.start_line,
            end_line: edit.end_line,
            line_count: ranges.len(),
        });
    }

    let start = ranges[(edit.start_line - 1) as usize].0;
    let end = ranges[(edit.end_line - 1) as usize].1;
    let replaced_text = source.get(start..end).unwrap_or("").to_owned();

    let mut rewritten = String::with_capacity(source.len() + edit.replacement.len());
    rewritten.push_str(source.get(..start).unwrap_or(""));
    rewritten.push_str(&edit.replacement);
    rewritten.push_str(source.get(end..).unwrap_or(""));
    std::fs::write(&target, rewritten)?;

    Ok(AppliedPatch {
        file: edit.file.clone(),
        start_line: edit.start_line,
        end_line: edit.end_line,
        replaced_text,
    })
}

/// Apply a set of edits inside `root`.
///
/// With `allow_multi_file` unset, the edits must all target the same file;
/// otherwise the whole batch is refused so the default stays single-file. Edits
/// are applied from the highest start line down so earlier splices do not shift
/// the offsets of later ones in the same file.
pub fn apply_edits(
    root: &Utf8Path,
    edits: &[SpanReplacement],
    allow_multi_file: bool,
) -> Result<Vec<AppliedPatch>> {
    let distinct_files: BTreeSet<&Utf8PathBuf> = edits.iter().map(|edit| &edit.file).collect();
    if distinct_files.len() > 1 && !allow_multi_file {
        return Err(Error::MultiFileEditNotAllowed {
            files: distinct_files.len(),
        });
    }

    let mut ordered: Vec<&SpanReplacement> = edits.iter().collect();
    ordered.sort_by(|a, b| a.file.cmp(&b.file).then(b.start_line.cmp(&a.start_line)));

    let mut applied = Vec::with_capacity(ordered.len());
    for edit in ordered {
        applied.push(apply_single_span(root, edit)?);
    }
    Ok(applied)
}

/// Command keywords a proof-body replacement must not introduce at the top level.
const TOP_LEVEL_COMMANDS: [&str; 5] = ["import", "set_option", "macro", "elab", "open"];

/// Refuse a replacement that would splice a top-level command into the file.
///
/// A single-span patch is a proof body, which is indented under its declaration.
/// A replacement line at column zero whose first token opens a command (any
/// `#...` command such as `#eval`/`#print`/`#check`, or one of
/// [`TOP_LEVEL_COMMANDS`]) is refused: spliced in, such a command is elaborated
/// at the top level and could perturb the accept guards that read the compile.
/// Column-zero declaration keywords (`theorem`/`def`/...) are allowed; a changed
/// statement is judged by the statement guard, not here.
fn reject_top_level_command(edit: &SpanReplacement) -> Result<()> {
    for line in edit.replacement.lines() {
        // Indented lines stay inside the proof body; only column-zero lines
        // become top-level commands once spliced in.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let trimmed = line.trim_end();
        let opens_command = trimmed.starts_with('#')
            || trimmed
                .split_whitespace()
                .next()
                .is_some_and(|token| TOP_LEVEL_COMMANDS.contains(&token));
        if opens_command {
            return Err(Error::DisallowedReplacement {
                file: edit.file.clone(),
                detail: format!("line `{trimmed}` opens a top-level command"),
            });
        }
    }
    Ok(())
}

/// Join `rel` under `root`, refusing absolute paths and `..` traversal.
fn resolve_within(root: &Utf8Path, rel: &Utf8Path) -> Result<Utf8PathBuf> {
    if rel.is_absolute() {
        return Err(Error::OutsideWorkspace {
            path: rel.to_path_buf(),
        });
    }
    for component in rel.components() {
        match component {
            Utf8Component::Normal(_) | Utf8Component::CurDir => {}
            _ => {
                return Err(Error::OutsideWorkspace {
                    path: rel.to_path_buf(),
                });
            }
        }
    }
    Ok(root.join(rel))
}

/// Per-line `(content_start_byte, content_end_byte)` pairs, newline excluded.
///
/// Indexing matches `str::lines`, so index `k` is one-based line `k + 1`.
fn line_content_ranges(source: &str) -> Vec<(usize, usize)> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let mut end = i;
            if end > start && bytes[end - 1] == b'\r' {
                end -= 1;
            }
            ranges.push((start, end));
            start = i + 1;
        }
        i += 1;
    }
    if start < bytes.len() {
        let mut end = bytes.len();
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        ranges.push((start, end));
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workspace_with(name: &str, contents: &str) -> Result<(TempDir, Utf8PathBuf)> {
        let dir = TempDir::new()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| Error::NonUtf8Path { path })?;
        std::fs::write(root.join(name), contents)?;
        Ok((dir, root))
    }

    fn span(file: &str, start_line: u32, end_line: u32, replacement: &str) -> SpanReplacement {
        SpanReplacement {
            file: Utf8PathBuf::from(file),
            start_line,
            end_line,
            replacement: replacement.to_owned(),
        }
    }

    #[test]
    fn replaces_single_line_and_preserves_newlines() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "line one\n  sorry\nline three\n")?;
        let applied = apply_single_span(&root, &span("A.lean", 2, 2, "  exact rfl"))?;
        assert_eq!(applied.replaced_text, "  sorry");
        let after = std::fs::read_to_string(root.join("A.lean"))?;
        assert_eq!(after, "line one\n  exact rfl\nline three\n");
        Ok(())
    }

    #[test]
    fn replaces_multi_line_range() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "a\nb\nc\nd\n")?;
        apply_single_span(&root, &span("A.lean", 2, 3, "X\nY\nZ"))?;
        let after = std::fs::read_to_string(root.join("A.lean"))?;
        assert_eq!(after, "a\nX\nY\nZ\nd\n");
        Ok(())
    }

    #[test]
    fn replaces_final_line_without_trailing_newline() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "a\nb")?;
        apply_single_span(&root, &span("A.lean", 2, 2, "bb"))?;
        let after = std::fs::read_to_string(root.join("A.lean"))?;
        assert_eq!(after, "a\nbb");
        Ok(())
    }

    #[test]
    fn rejects_span_past_end_of_file() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "a\nb\n")?;
        let result = apply_single_span(&root, &span("A.lean", 3, 3, "c"));
        assert!(matches!(
            result,
            Err(Error::SpanOutOfBounds { line_count: 2, .. })
        ));
        Ok(())
    }

    #[test]
    fn rejects_zero_and_inverted_spans() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "a\nb\n")?;
        assert!(matches!(
            apply_single_span(&root, &span("A.lean", 0, 1, "x")),
            Err(Error::ZeroLineSpan { .. })
        ));
        assert!(matches!(
            apply_single_span(&root, &span("A.lean", 2, 1, "x")),
            Err(Error::InvertedSpan { .. })
        ));
        Ok(())
    }

    #[test]
    fn rejects_paths_escaping_workspace() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "a\n")?;
        assert!(matches!(
            apply_single_span(&root, &span("../escape.lean", 1, 1, "x")),
            Err(Error::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            apply_single_span(&root, &span("/etc/passwd", 1, 1, "x")),
            Err(Error::OutsideWorkspace { .. })
        ));
        Ok(())
    }

    #[test]
    fn multi_file_is_refused_by_default_and_allowed_behind_flag() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "a\n")?;
        std::fs::write(root.join("B.lean"), "b\n")?;
        let edits = vec![span("A.lean", 1, 1, "aa"), span("B.lean", 1, 1, "bb")];
        assert!(matches!(
            apply_edits(&root, &edits, false),
            Err(Error::MultiFileEditNotAllowed { files: 2 })
        ));

        let applied = apply_edits(&root, &edits, true)?;
        assert_eq!(applied.len(), 2);
        assert_eq!(std::fs::read_to_string(root.join("A.lean"))?, "aa\n");
        assert_eq!(std::fs::read_to_string(root.join("B.lean"))?, "bb\n");
        Ok(())
    }

    #[test]
    fn rejects_replacement_that_injects_a_top_level_command() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "theorem t : True := by\n  sorry\n")?;
        // A proof body followed by an injected top-level `#eval` is refused.
        let injection = "  exact trivial\n#eval IO.println \"pwn\"";
        assert!(matches!(
            apply_single_span(&root, &span("A.lean", 2, 2, injection)),
            Err(Error::DisallowedReplacement { .. })
        ));
        // `import`/`set_option`/`open` at column zero are refused too.
        for command in ["import Foo", "set_option x true", "open Foo"] {
            assert!(
                matches!(
                    apply_single_span(&root, &span("A.lean", 2, 2, command)),
                    Err(Error::DisallowedReplacement { .. })
                ),
                "expected refusal for `{command}`"
            );
        }
        // The indented proof body alone is still applied.
        let applied = apply_single_span(&root, &span("A.lean", 2, 2, "  exact trivial"))?;
        assert_eq!(applied.replaced_text, "  sorry");
        Ok(())
    }

    #[test]
    fn allows_column_zero_declaration_replacement() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "theorem t : 2 = 3 := rfl\n")?;
        // Replacing the whole declaration line is allowed; a changed statement is
        // the statement guard's call, not the patch layer's.
        let applied = apply_single_span(&root, &span("A.lean", 1, 1, "theorem t : 2 = 2 := rfl"))?;
        assert_eq!(applied.start_line, 1);
        assert_eq!(
            std::fs::read_to_string(root.join("A.lean"))?,
            "theorem t : 2 = 2 := rfl\n"
        );
        Ok(())
    }

    #[test]
    fn same_file_edits_apply_top_down_without_offset_drift() -> Result<()> {
        let (_dir, root) = workspace_with("A.lean", "one\ntwo\nthree\n")?;
        let edits = vec![span("A.lean", 1, 1, "ONE"), span("A.lean", 3, 3, "THREE")];
        apply_edits(&root, &edits, false)?;
        assert_eq!(
            std::fs::read_to_string(root.join("A.lean"))?,
            "ONE\ntwo\nTHREE\n"
        );
        Ok(())
    }
}
