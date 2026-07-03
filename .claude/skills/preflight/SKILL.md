---
name: preflight
description: Run llmshim's pre-commit / pre-PR checks — the exact fmt, clippy, and test trio that CI enforces. Use before committing, opening a PR, or tagging a release to catch failures locally instead of in the release workflow.
allowed-tools: Bash(cargo fmt*), Bash(cargo clippy*), Bash(cargo test*)
---

# Preflight checks

These are the exact checks the `test` job in `.github/workflows/release.yml` runs. The release pipeline (crates.io publish, GitHub release, Homebrew tap) is gated on them, so a failure here means a broken release. Run all three and report results.

## Checks

```!
cargo fmt --check
cargo clippy --features proxy -- -D warnings
cargo test --features proxy --tests
```

## Interpreting results

- **`cargo fmt --check`** — non-zero exit means formatting drift. Fix with `cargo fmt` (no `--check`).
- **`cargo clippy ... -D warnings`** — every warning is an error in CI. Fix the lint; do not blanket-`#[allow]` unless there's a clear reason.
- **`cargo test --features proxy --tests`** — runs ~326 unit tests including the proxy. `--tests` skips doctests; `--features proxy` is required or proxy tests won't compile. Integration tests (`#[ignore]`, need API keys) are not part of preflight — run `cargo test -- --ignored` separately when you have keys.

Report each check's pass/fail plainly. If anything fails, show the relevant output and fix it before proceeding — do not report success on a failing tree.
