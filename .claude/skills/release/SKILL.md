---
name: release
description: Cut a new llmshim release — bump the version, run preflight, and tag so CI publishes to crates.io, GitHub releases, and the Homebrew tap. Use when releasing, cutting a version, or publishing a new crate version. Enforces the semver rules for this public crate.
disable-model-invocation: true
argument-hint: [new-version e.g. 0.1.22]
allowed-tools: Bash(cargo fmt*), Bash(cargo clippy*), Bash(cargo test*), Bash(cargo build*), Bash(git *)
---

# Cut an llmshim release

llmshim is published on crates.io and depended on by the `ragents` crate. Releases are tag-driven: pushing a `v*` tag triggers `.github/workflows/release.yml`, which runs the test gate, then publishes to crates.io, builds macOS binaries + a GitHub release, updates the Homebrew tap, publishes the Python/npm/RubyGems clients, and tags the Go client module. There is no manual publish step for any of these — the tag does it.

**All language clients ship in lockstep with the crate** — every release publishes Python, TypeScript, Ruby, and Go at the same version number as `Cargo.toml`, even if a given client's code didn't change (matches how Stripe/AWS/etc. version multi-language SDKs). Keep it that way; don't special-case a client's version.

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
3. **Bump the client versions to match** — the `npm` and `rubygems` CI jobs verify these against the tag and fail the release if they drift:
   - `clients/typescript/package.json` — `"version": "$ARGUMENTS"`
   - `clients/ruby/lib/llmshim/version.rb` — `VERSION = "$ARGUMENTS"`
   - Python's version is derived automatically from `Cargo.toml` by maturin — no separate file to bump.
   - Go has no in-file version — `go-tag` tags `clients/go/v$ARGUMENTS` from the release commit automatically.
4. **Preflight** — run the CI gate locally (this is what blocks the release job on failure):
   ```
   cargo fmt --check
   cargo clippy --features proxy -- -D warnings
   cargo test --features proxy --tests
   ```
   Or invoke `/preflight`.
5. **Commit** on a branch: `Release v$ARGUMENTS`. Open a PR and merge to `main` (do not push tags from a feature branch).
6. **Tag from `main`** after merge:
   ```
   git checkout main && git pull
   git tag v$ARGUMENTS
   git push origin v$ARGUMENTS
   ```
7. **Watch CI** — `test` gates everything; `crates-io` / `github-release` / `npm` / `rubygems` / `go-tag` / `sdist`+wheels→`pypi` all run independently off it, so one failing (e.g. npm before trusted publishing is configured) doesn't block the others. If `test` fails the tag published nothing; fix, delete the tag, and re-tag.

## Do not

- Do not tag or push a release without explicit user confirmation — publishing to crates.io/PyPI/npm/RubyGems is irreversible (versions can be yanked/deprecated but not overwritten or reused).
- Do not run `cargo publish`, `npm publish`, or `gem push` by hand; the workflow owns publishing via OIDC trusted publishing (no stored tokens for npm/RubyGems/PyPI).
