# Security

`lean-agent-rs` executes external commands and will eventually run model-generated code/proofs. Treat untrusted Lean projects and generated patches as executable input.

## Policy

- Do not run untrusted projects outside a sandbox.
- Keep patch replay isolated in temporary workspaces.
- Prefer explicit allowlists for runner commands.
- Never upload trace artifacts containing private source code unless the user opted in.

## TODO

- Add sandboxing notes for Linux/macOS/CI.
- Add a threat model for agent-generated patches.
- Add redaction options for source paths and raw diagnostics.
