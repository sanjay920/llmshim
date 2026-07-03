---
name: release
description: Cut a new llmshim release — bump the version, run preflight, and tag so CI publishes to crates.io, GitHub releases, and the Homebrew tap. Use when releasing, cutting a version, or publishing a new crate version. Enforces the semver rules for this public crate.
disable-model-invocation: true
argument-hint: [new-version e.g. 0.1.22]
allowed-tools: Bash(cargo fmt*), Bash(cargo clippy*), Bash(cargo test*), Bash(cargo build*), Bash(git *)
---

# Cut an llmshim release

llmshim is published on crates.io and depended on by the `ragents` crate. Releases are tag-driven: pushing a `v*` tag triggers `.github/workflows/release.yml`, which runs the test gate, then publishes to crates.io, builds macOS binaries + a GitHub release, and updates the Homebrew tap. There is no manual `cargo publish` step — the tag does it.

Target version: **$ARGUMENTS**

## Semver gate — check BEFORE bumping

Breaking changes to `pub` items require a **minor** bump (pre-1.0, minor is the breaking channel), not a patch. `ragents` depends on this crate. The public surface under semver protection:

`src/lib.rs`, `src/router.rs`, `src/provider.rs`, `src/error.rs`, `src/fallback.rs`, `src/log.rs`, `src/config.rs`, `src/models.rs`, `src/vision.rs`.

- Additive changes (new models, new provider modules, new `pub fn`) → **patch** bump is fine.
- Renaming/removing/retyping any `pub` item, or changing a trait signature → **minor** bump and call it out in the PR/notes.

Review the diff since the last release tag and classify it before choosing the version:
```!
git describe --tags --abbrev=0
git log --oneline $(git describe --tags --abbrev=0)..HEAD
```

## Steps

1. **Bump the version** in `Cargo.toml` (`version = "$ARGUMENTS"`).
2. **Update `Cargo.lock`** — run `cargo build` (or `cargo build --features proxy`) so the lockfile's llmshim entry matches. Both `Cargo.toml` and `Cargo.lock` change; the release commit should include both (see commit `c393418`).
3. **Preflight** — run the CI gate locally (this is what blocks the release job on failure):
   ```
   cargo fmt --check
   cargo clippy --features proxy -- -D warnings
   cargo test --features proxy --tests
   ```
   Or invoke `/preflight`.
4. **Commit** on a branch: `Release v$ARGUMENTS`. Open a PR and merge to `main` (do not push tags from a feature branch).
5. **Tag from `main`** after merge:
   ```
   git checkout main && git pull
   git tag v$ARGUMENTS
   git push origin v$ARGUMENTS
   ```
6. **Watch CI** — the `test` → `crates-io` / `github-release` → `homebrew` jobs. If `test` fails the tag published nothing; fix, delete the tag, and re-tag.

## Do not

- Do not tag or push a release without explicit user confirmation — publishing to crates.io is irreversible (versions can be yanked but not overwritten).
- Do not run `cargo publish` by hand; the workflow owns publishing.
