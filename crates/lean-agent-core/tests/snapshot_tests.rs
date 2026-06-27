//! Snapshot tests pinning the stable artifact shapes the pipeline emits.
//!
//! Each test exercises one pure stage of the loop on a fixture and snapshots the
//! result, so a change to a serialized shape (diagnostics, mined tasks, context
//! bundles, applied patches) shows up as a reviewable diff rather than a silent
//! schema drift.

use camino::Utf8PathBuf;
use lean_agent_core::{
    ContextRequest, DiagnosticSeverity, Error, LeanFile, MineKind, Result, SpanReplacement,
    apply_single_span, build_context, mine_placeholders, parse_lean_diagnostics,
};
use tempfile::TempDir;

const DIAGNOSTICS: &str = include_str!("fixtures/diagnostics_multi.stderr");
const SORRY_SAMPLE: &str = include_str!("fixtures/sorry_sample.lean");

fn lean(path: &str) -> LeanFile {
    LeanFile(Utf8PathBuf::from(path))
}

#[test]
fn diagnostic_parsing_snapshot() {
    let diagnostics = parse_lean_diagnostics(DIAGNOSTICS, true);
    assert_eq!(diagnostics.len(), 3);
    // The unsolved-goals error carries a recovered goal state.
    assert!(diagnostics[0].goal_state.is_some());
    insta::assert_json_snapshot!("diagnostic_parsing", diagnostics);
}

#[test]
fn diagnostic_parsing_drops_warnings_when_disabled() {
    let diagnostics = parse_lean_diagnostics(DIAGNOSTICS, false);
    assert_eq!(diagnostics.len(), 2);
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity != DiagnosticSeverity::Warning)
    );
}

#[test]
fn sorry_mining_snapshot() {
    let file = lean("Sample.lean");
    let tasks = mine_placeholders(
        &file,
        SORRY_SAMPLE,
        MineKind::Sorry,
        "demo",
        Utf8PathBuf::from(".").as_path(),
    );
    assert_eq!(tasks.len(), 1);
    // `source_before + target_span.text + source_after` reproduces the file.
    let task = &tasks[0];
    let rebuilt = format!(
        "{}{}{}",
        task.source_before, task.target_span.text, task.source_after
    );
    assert_eq!(rebuilt, SORRY_SAMPLE);
    insta::assert_json_snapshot!("sorry_mining", tasks);
}

#[test]
fn context_extraction_snapshot() {
    // Center on the `sorry` line so the enclosing theorem is detected.
    let request = ContextRequest {
        file: lean("Sample.lean"),
        line: 7,
        before: 3,
        after: 2,
    };
    let bundle = build_context(&request, SORRY_SAMPLE, &[], None);
    assert_eq!(
        bundle.declaration.as_ref().and_then(|d| d.name.clone()),
        Some("foo".to_owned())
    );
    insta::assert_json_snapshot!("context_extraction", bundle);
}

#[test]
fn patch_application_snapshot() -> Result<()> {
    let dir = TempDir::new()?;
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|path| Error::NonUtf8Path { path })?;
    std::fs::write(root.join("Sample.lean"), SORRY_SAMPLE)?;

    let edit = SpanReplacement {
        file: Utf8PathBuf::from("Sample.lean"),
        start_line: 7,
        end_line: 7,
        replacement: "  rfl".to_owned(),
    };
    let applied = apply_single_span(&root, &edit)?;
    assert_eq!(applied.replaced_text, "  sorry");

    let rewritten = std::fs::read_to_string(root.join("Sample.lean"))?;
    insta::assert_json_snapshot!("patch_application_record", applied);
    insta::assert_snapshot!("patch_application_source", rewritten);
    Ok(())
}
