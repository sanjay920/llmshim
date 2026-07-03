---
name: add-provider
description: Add a brand-new LLM provider to llmshim (a company/API not already supported, i.e. not openai, anthropic, gemini, or xai). Use when wiring up a new upstream API — implementing the Provider trait, registering it in the router, and hooking up env-var key discovery. For a new model on an existing provider, use /add-provider is wrong — use /add-model instead.
disable-model-invocation: true
argument-hint: [provider-key] [ProviderName]
allowed-tools: Bash(cargo fmt*), Bash(cargo test*), Bash(cargo clippy*)
---

# Add a provider to llmshim

A provider is a translation adapter: OpenAI-format JSON in → provider-native JSON out → OpenAI-format back. Everything flows as `serde_json::Value`; there is no canonical request struct. Study an existing provider that resembles the new API before writing anything:

- **`src/providers/xai.rs`** — closest to OpenAI's shape; smallest file, best starting template.
- **`src/providers/openai.rs`** — Responses API (function_call items, streaming event translation).
- **`src/providers/anthropic.rs`** — different message/tool/thinking model; `x-anthropic` extension namespace.
- **`src/providers/gemini.rs`** — `functionDeclarations`, `inline_data` vision.

Provider to add: **$ARGUMENTS**

## Steps

1. **Create `src/providers/<key>.rs`** implementing the `Provider` trait from `src/provider.rs`:
   - `fn name(&self) -> &str`
   - `fn transform_request(&self, model, request) -> Result<ProviderRequest>` — build URL, auth headers, and native body. Strip fields other providers add (`reasoning_content` always; `annotations`/`refusal` if the API rejects unknown keys) so multi-model conversations don't leak foreign fields.
   - `fn transform_response(&self, model, response) -> Result<Value>` — map back to OpenAI Chat Completions shape; normalize tool calls to OpenAI format and usage to the shared shape.
   - `fn transform_stream_chunk(&self, model, chunk) -> Result<Option<String>>` — translate one SSE `data:` line; return `Ok(None)` to skip keepalives.
   - A `pub fn new(api_key: String) -> Self` constructor (and `with_base_url` if useful for tests).
   - Put provider-only features under an `x-<key>` extension namespace, mirroring `x-anthropic`.

2. **`src/providers/mod.rs`** — add `pub mod <key>;`.

3. **`src/router.rs`**:
   - `use crate::providers::<key>::<ProviderName>;`
   - In `parse_model`, add prefix inference (e.g. `else if lower.starts_with("<prefix>")`) so bare model names route correctly.
   - In `Router::from_env`, register from the API-key env var:
     ```rust
     if let Ok(key) = std::env::var("<KEY>_API_KEY") {
         router = router.register("<key>", Box::new(<ProviderName>::new(key)));
     }
     ```

4. **`src/config.rs`** — if config-file key storage / `llmshim configure` should support it, add the provider there (check how existing providers are wired).

5. **Models** — register the provider's models via `/add-model` (both `src/models.rs` and `src/main.rs`).

6. **Tests** — add `tests/unit_<key>.rs` covering request/response/stream transforms in the style of `tests/unit_xai.rs`. Add `tests/integration_<key>.rs` with `#[ignore]` tests that need a real API key. Add a `parse_model` case in `tests/unit_router.rs`.

7. **Docs** — update `CLAUDE.md` (provider list, tool-format section, env-var precedence) and `README.md`.

## Verify

```bash
cargo fmt --check
cargo clippy --features proxy -- -D warnings
cargo test --features proxy --tests
```

Set `<KEY>_API_KEY` and run `cargo test -- --ignored` to exercise the live integration tests, and `cargo run -- chat` to smoke-test streaming and `/model` switching.

## Public-API note

`src/router.rs` and `src/provider.rs` are public and used by `ragents`. New provider modules and registrations are additive. Do not change existing trait signatures without a semver bump — see `/release`.
