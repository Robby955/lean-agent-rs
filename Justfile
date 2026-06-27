default:
    just check

fmt:
    cargo fmt --all

check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    cargo test --workspace

deny:
    cargo deny check
