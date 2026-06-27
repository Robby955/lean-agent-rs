# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `context` goal probe: when the plain trace recovers no goal at a `sorry`/`admit`
  (the placeholder elaborates with only a warning), Lean is re-run in an isolated
  copy with the placeholder swapped for `?_`, which prints the goal at that exact
  position for the parser to recover. On by default; `--no-goal-probe` skips it.
  This populates `goal_state` for placeholder bundles, which was previously empty.
- `goal_probe` module with `splice_hole` (pure) and `probe_goal_state` (isolated
  re-run), plus `recover_goal`/`goal_from_message` shared out of `diagnostics`.

### Fixed
- Accept predicate, axiom guard: close two bypasses of the axiom whitelist.
  - Probe-output injection: the axiom set is now read from a probe bracketed by
    per-run sentinels and parsed only between them, so a top-level `#eval` in the
    edited file can no longer forge the "does not depend on any axioms" marker.
  - Skipped-as-accepted: an anonymous declaration (`example`) is aliased to a
    named `def` probe instead of being skipped, and a skipped axiom guard is
    treated as a rejection, so an `example` on a false axiom is no longer
    accepted.
- Patch layer refuses a replacement that introduces a top-level command
  (`#eval`, `#print`, `#check`, `import`, `set_option`, `macro`, `elab`, `open`),
  since a single-span patch is a proof body; this keeps the axiom probe sound.
- Reverse-dependency guard derives the module name from the lake library source
  directories (`srcDir`), so a `srcDir` layout maps to the real module name.

### Changed
- README and the `accept` module doc now state that the guards address known
  bypass classes and are not a proof of soundness.
- CI runs `cargo deny check`, matching the documented quality bar.
- Fixed the placeholder FormalSLT link in the README.

## [0.2.0] - 2026-06-27

First crates.io release. Both `lean-agent-core` (library) and `lean-agent` (CLI
binary) are published together at this version.

### Added
- `trace`: `lake lean` execution per file, JSONL records, a first-pass diagnostic
  parser, `timed_out`/`runner_error` records, and per-record Lean/Lake/Git versions.
- `context FILE.lean:LINE`: a paste-ready prompt bundle in JSON or Markdown.
- `mine`: single-span tasks from `sorry`/`admit` placeholders (text scan) and from
  real errors (tracer-backed), each reproducing the source file byte for byte.
- `eval`: a line-oriented runner contract. The library never calls a model; a
  runner is an external process read in lock step, with a SHA-256 prompt-hash
  fallback and a per-task reply timeout.
- `replay`: deterministic single-span patching into an isolated copy, baseline
  comparison, regression scoring, and an accept predicate with three live guards
  (statement-unchanged, axiom-whitelist, reverse-dependency build).
- `report`: a pass/fail/timeout and diagnostic roll-up.
- JSON Schemas for every on-disk record type, shipped inside `lean-agent-core`.
- Snapshot tests for diagnostic parsing, sorry mining, context extraction, and
  patch application; accept-guard integration tests against a bare Lake project.

### Changed
- The schemas and the `unsolved_goal` diagnostic fixture now live inside the
  `lean-agent-core` crate so they ship in the published package and downstream
  `cargo test` runs against the same files.

### Notes
- A fourth accept guard, NEGATIVE-CONTROL, is wired but stubbed pending the claim
  manifest (`TODO(loop-phase)`).
- Parquet output, a dataset manifest, per-task artifact directories, bounded
  parallelism, and `lake serve` / LSP goal states are tracked but not yet
  implemented; see the README and the searchable `TODO(...)` tags in the code.

[Unreleased]: https://github.com/Robby955/lean-agent-rs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Robby955/lean-agent-rs/releases/tag/v0.2.0
