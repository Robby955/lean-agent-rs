//! End-to-end checks for the replay accept predicate (guards 1-3).
//!
//! Each test stands up a tiny, mathlib-free Lake project in a temp directory and
//! drives one attempt through `run_replay`, so the guards are exercised against a
//! real `lake lean`/`lake build`. The load-bearing cases:
//!
//! - a statement-weakening edit is REJECTED even though `lake lean` exits 0;
//! - an honest proof (named, and via an `example`) PASSES every live guard;
//! - a `sorry` attempt is REJECTED by the axiom guard;
//! - Break 1: a probe-output injection that forges the "no axioms" marker is
//!   refused (at patch time, and by the sentinel-bracketed probe directly);
//! - Break 2: an `example` depending on a false axiom is REJECTED, not skipped.
//!
//! The tests skip (rather than fail) when `lake` is not on `PATH`, so the suite
//! stays green on machines without a Lean toolchain.

use camino::{Utf8Path, Utf8PathBuf};
use lean_agent_core::{
    AllowedEdit, Attempt, JsonlWriter, RejectReason, ReplayOptions, ReplayResult, ReplayStatus,
    run_replay,
};
use std::error::Error;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;

const LAKEFILE: &str =
    "name = \"demo\"\ndefaultTargets = [\"Demo\"]\n\n[[lean_lib]]\nname = \"Demo\"\n";
const TOOLCHAIN: &str = "leanprover/lean4:v4.28.0\n";

/// True when a usable `lake` is on `PATH`.
fn lake_available() -> bool {
    std::process::Command::new("lake")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Write a one-file Lake project whose library module is `Demo`.
fn setup_project(demo: &str) -> Result<(TempDir, Utf8PathBuf), Box<dyn Error>> {
    let dir = TempDir::new()?;
    let root = dir.path();
    fs::write(root.join("lakefile.toml"), LAKEFILE)?;
    fs::write(root.join("lean-toolchain"), TOOLCHAIN)?;
    fs::write(root.join("Demo.lean"), demo)?;
    let lake_root = Utf8PathBuf::from_path_buf(root.to_path_buf())
        .map_err(|path| format!("non-UTF-8 temp path: {}", path.display()))?;
    Ok((dir, lake_root))
}

/// One single-span attempt over `Demo.lean`.
fn attempt(start_line: u32, end_line: u32, replacement: &str) -> Attempt {
    Attempt {
        task_id: "Demo.foo:1".to_owned(),
        attempt_id: "a1".to_owned(),
        allowed_edit: AllowedEdit {
            file: Utf8PathBuf::from("Demo.lean"),
            start_line,
            end_line,
        },
        replacement: replacement.to_owned(),
        target_file: None,
        extra_edits: Vec::new(),
        original_diagnostic: None,
        model: None,
        prompt_hash: None,
        metadata: None,
    }
}

/// Replay one attempt and read back its single result record.
async fn replay_one(
    lake_root: &Utf8Path,
    attempt: Attempt,
) -> Result<ReplayResult, Box<dyn Error>> {
    let out_dir = TempDir::new()?;
    let out = Utf8PathBuf::from_path_buf(out_dir.path().join("results.jsonl"))
        .map_err(|path| format!("non-UTF-8 temp path: {}", path.display()))?;

    let options = ReplayOptions {
        lake_root: lake_root.to_path_buf(),
        timeout: Duration::from_secs(120),
        keep_workdir: false,
        allow_multi_file: false,
        // Baseline scoring and cache-get are orthogonal to the guards under test.
        compute_baseline: false,
        reverse_dep: true,
        cache_get: false,
    };

    let mut writer = JsonlWriter::create(&out)?;
    run_replay(&options, std::slice::from_ref(&attempt), &mut writer).await?;

    let content = fs::read_to_string(out.as_std_path())?;
    let line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or("replay wrote no result record")?;
    let result: ReplayResult = serde_json::from_str(line)?;
    Ok(result)
}

#[tokio::test]
async fn statement_weakening_is_rejected_even_though_lake_lean_exits_zero() -> TestResult {
    if !lake_available() {
        eprintln!("skipping accept_guards: lake not found on PATH");
        return Ok(());
    }
    // The original claims `2 + 2 = 5` (false; the error task). The attempt
    // "fixes" it by quietly weakening the statement to `2 + 2 = 4`.
    let (_dir, lake_root) = setup_project("theorem foo : 2 + 2 = 5 := by rfl\n")?;
    let result = replay_one(
        &lake_root,
        attempt(1, 1, "theorem foo : 2 + 2 = 4 := by rfl"),
    )
    .await?;

    // The weakened file compiles: a bare exit-code check would accept it.
    assert!(
        result.compile_passed,
        "the weakened statement should still compile"
    );
    // The accept predicate refuses it.
    assert_eq!(result.status, ReplayStatus::Rejected);
    assert!(!result.accepted);
    let reason = result.reject_reason.ok_or("expected a reject reason")?;
    assert!(
        matches!(reason, RejectReason::StatementChanged { .. }),
        "expected a statement-changed rejection, got {reason:?}"
    );
    Ok(())
}

#[tokio::test]
async fn honest_proof_passes_all_live_guards() -> TestResult {
    if !lake_available() {
        eprintln!("skipping accept_guards: lake not found on PATH");
        return Ok(());
    }
    // A sorry task whose body is honestly discharged with `rfl`.
    let (_dir, lake_root) = setup_project("theorem foo : 1 + 1 = 2 := by\n  sorry\n")?;
    let result = replay_one(&lake_root, attempt(2, 2, "  rfl")).await?;

    assert!(result.compile_passed);
    assert_eq!(result.status, ReplayStatus::Passed);
    assert!(result.accepted, "honest proof should be accepted");
    assert!(result.reject_reason.is_none());
    Ok(())
}

#[tokio::test]
async fn break1_eval_injection_cannot_forge_the_no_axioms_marker() -> TestResult {
    if !lake_available() {
        eprintln!("skipping accept_guards: lake not found on PATH");
        return Ok(());
    }
    // Break 1 setup: the editable span (line 3) discharges the goal with a false
    // axiom and tries to forge the axiom guard's "no axioms" marker.
    let source = "axiom evil : (2 : Nat) + 2 = 5\ntheorem foo : (2 : Nat) + 2 = 5 := by\n  sorry\n";
    let (_dir, lake_root) = setup_project(source)?;

    // Control: depending on the false axiom (no injection) is rejected outright.
    let control = replay_one(&lake_root, attempt(3, 3, "  exact evil")).await?;
    assert_ne!(control.status, ReplayStatus::Passed);
    assert!(!control.accepted);
    assert!(
        matches!(
            control.reject_reason,
            Some(RejectReason::DisallowedAxiom { .. })
        ),
        "control should be a disallowed-axiom rejection, got {:?}",
        control.reject_reason
    );

    // Exploit: the same dependency plus an injected top-level `#eval` that prints
    // the forged marker. The patch layer refuses the top-level command, so the
    // exploit never reaches a compile.
    let exploit = "  exact evil\n#eval IO.println \"'foo' does not depend on any axioms\"";
    let result = replay_one(&lake_root, attempt(3, 3, exploit)).await?;
    assert_eq!(
        result.status,
        ReplayStatus::PatchRefused,
        "injection must be refused at patch time, got {:?}",
        result.status
    );
    assert!(
        !result.accepted,
        "the forged-marker injection must not be accepted"
    );
    Ok(())
}

#[tokio::test]
async fn break1_probe_ignores_a_forged_marker_already_in_the_file() -> TestResult {
    if !lake_available() {
        eprintln!("skipping accept_guards: lake not found on PATH");
        return Ok(());
    }
    // The forged marker lives in the source as a top-level `#eval`, so the edit
    // itself is an innocent proof-body change that reaches the axiom probe. The
    // sentinel-bracketed probe still reads the real axiom set.
    let source = "axiom evil : (2 : Nat) + 2 = 5\n\
         #eval IO.println \"'foo' does not depend on any axioms\"\n\
         theorem foo : (2 : Nat) + 2 = 5 := by\n  sorry\n";
    let (_dir, lake_root) = setup_project(source)?;
    let result = replay_one(&lake_root, attempt(4, 4, "  exact evil")).await?;

    assert!(
        result.compile_passed,
        "the file compiles: sorry replaced, exit 0"
    );
    assert_ne!(result.status, ReplayStatus::Passed);
    assert!(
        !result.accepted,
        "a forged marker must not flip the verdict"
    );
    assert!(
        matches!(
            result.reject_reason,
            Some(RejectReason::DisallowedAxiom { .. })
        ),
        "probe must report the real axiom dependency, got {:?}",
        result.reject_reason
    );
    Ok(())
}

#[tokio::test]
async fn break2_example_depending_on_a_false_axiom_is_rejected() -> TestResult {
    if !lake_available() {
        eprintln!("skipping accept_guards: lake not found on PATH");
        return Ok(());
    }
    // Break 2: a sorry inside an `example`. The axiom guard used to skip
    // anonymous declarations and leave the attempt accepted; now the example is
    // aliased to a named probe and its false-axiom dependency is caught.
    let source = "axiom evil : (2 : Nat) + 2 = 5\nexample : (2 : Nat) + 2 = 5 := by\n  sorry\n";
    let (_dir, lake_root) = setup_project(source)?;
    let result = replay_one(&lake_root, attempt(3, 3, "  exact evil")).await?;

    assert!(result.compile_passed, "the example compiles, exit 0");
    assert_ne!(result.status, ReplayStatus::Passed);
    assert!(
        !result.accepted,
        "an example on a false axiom must not be accepted"
    );
    assert!(
        matches!(
            result.reject_reason,
            Some(RejectReason::DisallowedAxiom { .. })
        ),
        "expected a disallowed-axiom rejection, got {:?}",
        result.reject_reason
    );
    Ok(())
}

#[tokio::test]
async fn honest_example_is_accepted_via_the_aliased_probe() -> TestResult {
    if !lake_available() {
        eprintln!("skipping accept_guards: lake not found on PATH");
        return Ok(());
    }
    // An honest example must still pass: the alias probe reads no axioms.
    let (_dir, lake_root) = setup_project("example : (1 : Nat) + 1 = 2 := by\n  sorry\n")?;
    let result = replay_one(&lake_root, attempt(2, 2, "  rfl")).await?;

    assert!(result.compile_passed);
    assert_eq!(
        result.status,
        ReplayStatus::Passed,
        "honest example should pass, got {:?}",
        result.reject_reason
    );
    assert!(
        result.accepted,
        "an honest example should be accepted via the alias"
    );
    Ok(())
}

#[tokio::test]
async fn sorry_attempt_is_rejected_by_the_axiom_guard() -> TestResult {
    if !lake_available() {
        eprintln!("skipping accept_guards: lake not found on PATH");
        return Ok(());
    }
    // The attempt leaves the `sorry` in place. `lake lean` exits 0 (sorry is a
    // warning, not an error), so a bare exit-code check would accept it.
    let (_dir, lake_root) = setup_project("theorem foo : 1 + 1 = 2 := by\n  sorry\n")?;
    let result = replay_one(&lake_root, attempt(2, 2, "  sorry")).await?;

    assert!(result.compile_passed, "a sorry still exits 0");
    assert_eq!(result.status, ReplayStatus::Rejected);
    assert!(!result.accepted);
    let reason = result.reject_reason.ok_or("expected a reject reason")?;
    assert!(
        matches!(reason, RejectReason::DisallowedAxiom { .. }),
        "expected a disallowed-axiom rejection, got {reason:?}"
    );
    if let RejectReason::DisallowedAxiom { offending, .. } = reason {
        assert!(
            offending.iter().any(|axiom| axiom == "sorryAx"),
            "sorryAx should be flagged as the offending axiom"
        );
    }
    Ok(())
}
