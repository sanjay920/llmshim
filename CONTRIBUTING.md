# Contributing to llmshim

Thanks for your interest! llmshim is a small, sharply-scoped pure-Rust codebase
and we want contributions to be pleasant. This page is the human onboarding
path; the deep technical context lives in [CLAUDE.md](CLAUDE.md) — written for
coding agents but equally useful to people, and kept rigorously current.

## Where to start

- **Try it first**: the [documentation site](https://sanjay920.github.io/llmshim/)
  shows what llmshim does and the contracts it keeps. `cargo run` drops you into
  the interactive chat.
- **Issues** labeled `good first issue` are curated entry points; `bug` issues
  with a minimal reproducing request (model + messages) are the most valuable
  thing you can pick up.
- Open an issue before large changes — especially anything touching the public
  API surface (see below), which is under semver protection.

## Development setup

```bash
git clone https://github.com/sanjay920/llmshim
cd llmshim
cargo build
cargo test --features proxy --tests   # unit tests; ~CI's set
```

Rust stable. Provider API keys are only needed for the integration tests
(`cargo test --features proxy -- --ignored`); set them via environment variables
(`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`) or
`llmshim configure`. The proxy is feature-gated: build it with `--features proxy`.

## Working with coding agents

This repo is deliberately agent-friendly: `CLAUDE.md` is the canonical context,
scoped rules live in `.claude/rules/`, and task-specific procedures live in
`.claude/skills/` (e.g. `/preflight`, `/add-model`, `/add-provider`, `/release`).
If you contribute with Claude Code, Codex, Cursor, or similar, your agent
already has what it needs. Agent-assisted PRs are welcome; you own what you
submit.

## The gates (CI enforces all of these)

```bash
cargo fmt --check
cargo clippy --features proxy -- -D warnings
cargo test --features proxy --tests
```

The `/preflight` skill runs exactly this trio.

## The rules that protect users

1. The public API is **semver-protected**. Do not make breaking changes to
   `pub` items in `src/lib.rs`, `src/router.rs`, `src/provider.rs`,
   `src/error.rs`, `src/fallback.rs`, `src/log.rs`, `src/config.rs`,
   `src/models.rs`, or `src/vision.rs` without a version bump (pre-1.0, the
   minor is the breaking channel). Additive changes are a patch.
2. **Value-based transforms.** Requests flow as `serde_json::Value`; each
   provider maps only what it understands and puts provider-specific controls
   under `x-openai` / `x-anthropic` / `x-gemini`. Don't invent a canonical
   struct or forward a field a provider doesn't accept.
3. **Accuracy over freshness.** Per-model behavior (reasoning tiers, model
   specs) is verified against the live provider APIs. Leave a value `Unknown`
   rather than guessing it, and update the pinning tests when a mapping changes.
4. **No cross-provider leakage.** A message from one provider must be sanitized
   before it reaches another — opaque tokens and provider-only fields never leak.

## Tests

Every test must pin real behavior that could regress — a fix's test should fail
on the pre-fix code. We decline tests that exist for coverage's own sake, and we
decline fixes without a test that would have caught the bug. Unit tests run
offline; integration tests (`--ignored`) hit real provider APIs and need keys.

## Pull requests

- Small and focused beats big-bang. The PR template's checklist mirrors the
  gates above.
- If your change alters the request/response shapes, the CLI, the proxy HTTP
  API, or configuration, update the corresponding page under `docs/src/` in the
  same PR — and keep `api/openapi.yaml` and the language clients in `clients/` in
  sync with the proxy contract.
- All language clients ship in lockstep with the crate version.

## Developer Certificate of Origin (DCO)

By contributing, you certify the [Developer Certificate of
Origin](https://developercertificate.org/) — that you wrote the code or
otherwise have the right to submit it under this project's licenses. Sign off
each commit:

```bash
git commit -s    # adds "Signed-off-by: Your Name <you@example.com>"
```

## Licensing of contributions

llmshim is dual-licensed under [MIT](LICENSE-MIT) and
[Apache-2.0](LICENSE-APACHE). Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in llmshim by you, as defined
in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.

## Security issues

Do **not** open public issues for suspected vulnerabilities — see
[SECURITY.md](SECURITY.md).

## Conduct

We follow the [Contributor Covenant](CODE_OF_CONDUCT.md). Be excellent to each
other.
