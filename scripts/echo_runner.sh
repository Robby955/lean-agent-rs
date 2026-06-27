#!/usr/bin/env sh
#
# Example `lean-agent eval` runner (a wiring stub, not a real prover).
#
# Process contract:
#   - read one task JSON per line on stdin;
#   - write one attempt JSON per line on stdout, flushing after each line.
#
# This stub ignores the task body and returns a fixed replacement, so it is only
# useful for testing the pipeline end to end. A real runner would build a prompt
# from the task (imports, the enclosing declaration, the goal state) and call a
# model; `lean-agent` itself never calls a model. The lake root is available in
# the LEAN_AGENT_LAKE_ROOT environment variable.
set -eu

REPLACEMENT='  rfl'
MODEL='echo-runner'

while IFS= read -r line; do
  task_id=$(printf '%s\n' "$line" | sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p')
  if [ -z "$task_id" ]; then
    task_id='unknown'
  fi
  printf '{"task_id":"%s","attempt_id":"echo","replacement":"%s","model":"%s","prompt_hash":null,"metadata":{"stub":true}}\n' \
    "$task_id" "$REPLACEMENT" "$MODEL"
done
