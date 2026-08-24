# Contributing

Contributions are welcome through focused pull requests.

Before submitting a change:

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Native actions must fail closed. New action paths should include tests for stale
references, protected targets, user interference, and ambiguous verification.
Avoid adding network services or telemetry to the core runtime.

By contributing, you agree that your contribution is licensed under Apache-2.0.
