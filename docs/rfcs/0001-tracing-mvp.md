# RFC 0001: Lean tracing MVP

## Decision

Start with `lake lean <file>` process execution and parsed diagnostics. Defer Lean LSP / `lake serve` integration until the file-level tracer is stable and useful.

## Why

The first deliverable is not an ambitious theorem prover. It is reproducible infrastructure that runs on real Lean projects, emits clean artifacts, and enables evaluation.

## Scope for v0.1

- Discover `.lean` files.
- Run Lean through Lake.
- Capture exit code, elapsed time, stdout, stderr.
- Parse file/line/column/severity/message diagnostics.
- Extract obvious unsolved-goal blocks.
- Emit JSONL.
- Produce a summary report.

## Explicit non-goals for v0.1

- Interactive tactic stepping.
- Full LSP support.
- Perfect declaration attribution.
- Parquet output.
- Agent loop orchestration.
- mathlib-scale caching.

## TODO

- Build a fixture corpus from real Lean failures.
- Snapshot-test parser behavior.
- Emit runner-error records on timeout/process failures.
- Capture Lean/Lake/Git versions once per run.
- Add parallelism with bounded concurrency.
