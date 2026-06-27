# Contributing

## Code style

- Keep the CLI thin; put reusable behavior in `lean-agent-core`.
- Prefer domain-specific newtypes over raw strings and paths.
- Return structured errors from the library.
- Avoid panics in library code.
- Keep serialized schemas backward-compatible once published.
- Add fixtures for every parser behavior change.

## Checks

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo deny check
```
