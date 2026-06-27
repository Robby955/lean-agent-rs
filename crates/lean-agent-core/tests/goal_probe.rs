//! End-to-end checks for the context goal probe.
//!
//! A `sorry` elaborates with only a warning, so the plain trace recovers no
//! goal. The probe re-runs Lean with the placeholder swapped for `?_` so the
//! goal is printed and recovered. Each test stands up a tiny, mathlib-free Lake
//! project in a temp directory and drives `gather_context` against it.
//!
//! The tests skip (rather than fail) when `lake` is not on `PATH`, so the suite
//! stays green on machines without a Lean toolchain.

use camino::Utf8PathBuf;
use lean_agent_core::{ContextOptions, ContextRequest, LeanFile, gather_context};
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

/// Write a one-file Lake project whose library module is `Demo`, returning the
/// project root and the absolute path of `Demo.lean`.
fn setup_project(demo: &str) -> Result<(TempDir, Utf8PathBuf, Utf8PathBuf), Box<dyn Error>> {
    let dir = TempDir::new()?;
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
        .map_err(|path| format!("non-UTF-8 temp path: {}", path.display()))?;
    fs::write(root.join("lakefile.toml"), LAKEFILE)?;
    fs::write(root.join("lean-toolchain"), TOOLCHAIN)?;
    let demo_path = root.join("Demo.lean");
    fs::write(&demo_path, demo)?;
    Ok((dir, root, demo_path))
}

fn options(lake_root: Utf8PathBuf, goal_probe: bool) -> ContextOptions {
    ContextOptions {
        run_trace: true,
        lake_root,
        timeout: Duration::from_secs(120),
        include_warnings: true,
        goal_probe,
    }
}

#[tokio::test]
async fn probe_recovers_the_goal_at_a_sorry() -> TestResult {
    if !lake_available() {
        eprintln!("skipping goal_probe: lake not found on PATH");
        return Ok(());
    }
    // The plain trace yields only a `declaration uses 'sorry'` warning, so the
    // goal is absent until the probe swaps the placeholder for `?_`.
    let (_dir, root, demo) =
        setup_project("theorem foo (n : Nat) (h : n > 0) : n = n := by\n  sorry\n")?;
    let request = ContextRequest::new(LeanFile::new(demo)?, 2);

    let bundle = gather_context(&request, &options(root, true)).await?;

    let goal = bundle
        .goal_state
        .as_ref()
        .ok_or("probe should recover a goal at the sorry")?;
    assert!(
        goal.0.contains('⊢'),
        "recovered goal should carry a turnstile, got: {}",
        goal.0
    );
    assert!(
        goal.0.contains("n = n"),
        "recovered goal should restate the target, got: {}",
        goal.0
    );
    // The local hypotheses are part of the goal the agent must discharge.
    assert!(
        goal.0.contains("h : n > 0"),
        "recovered goal should include the local context, got: {}",
        goal.0
    );
    assert!(
        bundle.suggested_prompt.contains("Goal state"),
        "the recovered goal should reach the prompt"
    );
    Ok(())
}

#[tokio::test]
async fn no_goal_probe_leaves_the_goal_absent() -> TestResult {
    if !lake_available() {
        eprintln!("skipping goal_probe: lake not found on PATH");
        return Ok(());
    }
    // With the probe disabled the sorry warning carries no goal, so the bundle
    // has none: this is the `--no-goal-probe` contract.
    let (_dir, root, demo) = setup_project("theorem foo (n : Nat) : n = n := by\n  sorry\n")?;
    let request = ContextRequest::new(LeanFile::new(demo)?, 2);

    let bundle = gather_context(&request, &options(root, false)).await?;

    assert!(
        bundle.goal_state.is_none(),
        "no-goal-probe should leave the goal absent, got: {:?}",
        bundle.goal_state
    );
    Ok(())
}

#[tokio::test]
async fn probe_recovers_the_goal_at_an_admit() -> TestResult {
    if !lake_available() {
        eprintln!("skipping goal_probe: lake not found on PATH");
        return Ok(());
    }
    let (_dir, root, demo) = setup_project("theorem bar (p : Prop) (hp : p) : p := by\n  admit\n")?;
    let request = ContextRequest::new(LeanFile::new(demo)?, 2);

    let bundle = gather_context(&request, &options(root, true)).await?;

    let goal = bundle
        .goal_state
        .as_ref()
        .ok_or("probe should recover a goal at the admit")?;
    assert!(
        goal.0.contains("hp : p"),
        "recovered goal should include the local context, got: {}",
        goal.0
    );
    Ok(())
}
