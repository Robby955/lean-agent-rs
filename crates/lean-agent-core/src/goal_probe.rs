//! Recovering the goal state at a placeholder by re-running Lean with the
//! placeholder swapped for a typed hole.
//!
//! A `sorry`/`admit` elaborates with only a `declaration uses 'sorry'` warning,
//! so the goal is never printed and the diagnostic parser has nothing to
//! recover. Swapping the placeholder token for `?_` forces Lean to print the
//! goal at that exact position: term-mode reports an unsynthesized placeholder
//! with the local context, tactic-mode reports an unsolved goal. Either way the
//! turnstile block is recovered by [`recover_goal`].
//!
//! The edit is made in an isolated workspace copy, so the source of truth is
//! never touched. The probe inherits the workspace-copy cost of [`replay`], so
//! on a mathlib-backed project it is as heavy as one replay and belongs on a
//! host with the dependencies already built.
//!
//! [`replay`]: crate::replay

use crate::{
    CopyOptions, GoalState, LeanFile, Provenance, TraceConfig, Workspace, recover_goal,
    run_lean_file,
};
use camino::Utf8Path;
use std::time::Duration;
use tracing::warn;

/// The typed hole swapped in for a placeholder so Lean prints the goal.
const HOLE: &str = "?_";

/// Splice the typed hole between the two halves of a file whose placeholder span
/// has been removed.
///
/// `source_before` and `source_after` are exactly the fields a mined placeholder
/// task carries, so `source_before + placeholder + source_after` is the original
/// file and this returns that file with the placeholder replaced by `?_`. Pure:
/// no Lean, no filesystem.
#[must_use]
pub fn splice_hole(source_before: &str, source_after: &str) -> String {
    let mut out = String::with_capacity(source_before.len() + HOLE.len() + source_after.len());
    out.push_str(source_before);
    out.push_str(HOLE);
    out.push_str(source_after);
    out
}

/// Recover the goal state at a placeholder by re-running Lean with the
/// placeholder replaced by `?_` inside an isolated copy of `lake_root`.
///
/// `rel_file` is the target file's path as `lake lean` sees it (relative to
/// `lake_root`, or absolute). `source_before`/`source_after` are the file halves
/// around the placeholder span. Returns `None` when no goal can be recovered:
/// the toolchain is missing, the copy or write failed, the probe timed out, or
/// the statement itself does not elaborate far enough to print a goal.
pub async fn probe_goal_state(
    lake_root: &Utf8Path,
    rel_file: &Utf8Path,
    source_before: &str,
    source_after: &str,
    timeout: Duration,
) -> Option<GoalState> {
    let workspace = match Workspace::materialize(lake_root, false, &CopyOptions::default()) {
        Ok(workspace) => workspace,
        Err(err) => {
            warn!(error = %err, "goal probe: workspace copy failed");
            return None;
        }
    };

    let target = workspace.root().join(rel_file);
    let probe_source = splice_hole(source_before, source_after);
    if let Err(err) = std::fs::write(target.as_std_path(), probe_source) {
        warn!(path = %target, error = %err, "goal probe: write failed");
        return None;
    }

    // The probe deliberately produces an error file, so warnings are irrelevant;
    // what matters is the unsolved-goal / unsynthesized-placeholder diagnostic.
    let mut config = TraceConfig::new(workspace.root().to_path_buf()).timeout(timeout);
    config.include_warnings = false;
    let provenance = Provenance::default();
    let trace = run_lean_file(&config, &provenance, LeanFile(rel_file.to_path_buf())).await;

    recover_goal(&trace.diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_reconstructs_the_hole_in_place() {
        // Tactic-mode placeholder: `by\n  sorry\n` with the span removed.
        let before = "theorem foo (n : Nat) : n = n := by\n  ";
        let after = "\n";
        assert_eq!(
            splice_hole(before, after),
            "theorem foo (n : Nat) : n = n := by\n  ?_\n"
        );
    }

    #[test]
    fn splice_handles_term_mode_placeholder() {
        // Term-mode placeholder: `:= sorry` with the span removed.
        let before = "theorem foo : True := ";
        let after = "\n";
        assert_eq!(splice_hole(before, after), "theorem foo : True := ?_\n");
    }

    #[test]
    fn splice_is_empty_safe() {
        assert_eq!(splice_hole("", ""), "?_");
    }
}
