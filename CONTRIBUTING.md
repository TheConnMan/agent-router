# Contributing

Please open an issue before substantial work so the intended outcome is clear.

Use a focused branch, add or update tests for behavior changes, and run the checks below before opening a pull request.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
```

Do not include credentials, customer data, personal paths, or operational prompts in commits, fixtures, issues, or pull requests.
