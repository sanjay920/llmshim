---
name: add-model
description: Add a new model to llmshim's supported model list. Use when adding, registering, or exposing a new model ID (OpenAI, Anthropic, Gemini, or xAI) so it shows up in the CLI, proxy, and docs. This covers models on an already-supported provider — for a brand-new provider, use /add-provider first.
disable-model-invocation: true
argument-hint: [provider/model-id] [Display Label]
allowed-tools: Bash(cargo fmt*), Bash(cargo test*), Bash(cargo clippy*)
---

# Add a model to llmshim

Adding a model that runs on an **already-supported provider** (openai, anthropic, gemini, xai) is a data change: the transform code already knows how to talk to the provider, so you only edit registries, tests, and docs. No new provider logic is needed.

If the model belongs to a provider llmshim does not support yet, stop and use `/add-provider` first.

Model to add: **$ARGUMENTS**

## The model registry is duplicated — update BOTH copies

The canonical list lives in `src/models.rs` (`MODELS: &[ModelInfo]`, used by the library and proxy). The CLI has a **second, hand-maintained copy** in `src/main.rs` (`const MODELS: &[(&str, &str)]`). They must stay in sync or the CLI picker and the proxy disagree.

## Steps

1. **`src/models.rs`** — add a `ModelInfo` entry in the provider's section:
   ```rust
   ModelInfo {
       id: "<provider>/<model-name>",   // e.g. "openai/gpt-5.6"
       provider: "<provider>",          // openai | anthropic | gemini | xai
       name: "<model-name>",            // the id without the provider prefix
       label: "<Display Label>",        // e.g. "GPT-5.6"
   },
   ```

2. **`src/main.rs`** — add the matching tuple to `const MODELS` in the same order:
   ```rust
   ("<provider>/<model-name>", "<Display Label>"),
   ```

3. **`tests/unit_models.rs`** — bump `models_registry_has_expected_count` (the `assert_eq!(MODELS.len(), N)`) by one. This test is the guardrail that catches a forgotten registry edit.

4. **Docs** — update the "Supported models" list in `CLAUDE.md` and the model list in `README.md`. If the id appears in `api/openapi.yaml` examples and is a good representative, you may add it there too (optional).

5. **Provider-specific capability flags** (only if the model needs different handling). Model-family checks live in the provider file, e.g. `src/providers/anthropic.rs`: `is_claude_4_6`, `supports_1m_context`, `supports_thinking`. If the new model reasons, supports 1M context, thinking, or fast mode differently from the family default, update the relevant helper and its tests (`tests/unit_<provider>.rs`, `tests/unit_fast_mode.rs`).

## Verify

Run the preflight checks (same as CI) before finishing:

```bash
cargo fmt --check
cargo clippy --features proxy -- -D warnings
cargo test --features proxy --tests
```

Then confirm the model appears: `cargo run -- models` should list it, and `cargo run --features proxy -- proxy` then `GET /v1/models` should include it when the provider's API key is set.

## Public-API note

`MODELS` and `ModelInfo` in `src/models.rs` are `pub` and depended on by the `ragents` crate. Adding entries is additive and safe. Do **not** rename or remove fields without a semver bump — see `/release`.
