# Models and the Router

The `model` string is a routing address. It tells llmshim which provider
adapter should receive the request and which model name that adapter should
send upstream.

## Prefer explicit addresses

The most explicit form is `provider/model`:

```text
openai/gpt-5.6-sol
anthropic/claude-opus-5
gemini/gemini-3.8-flash
xai/grok-4.6
```

The part before the first slash is the Router registration key. The remainder
is sent to that provider as the model name. Explicit addresses are easiest to
read and do not depend on naming conventions.

## Bare-name inference

When there is no slash, llmshim lowercases the name for prefix matching while
preserving the original model string:

| Prefix | Provider key |
|---|---|
| `gpt*`, `o1*`, `o3*`, `o4*` | `openai` |
| `claude*` | `anthropic` |
| `gemini*` | `gemini` |
| `grok*` | `xai` |

A bare name outside those prefixes produces an unknown-provider error. Use an
explicit address when inference cannot identify the provider.

## Registration and discovery are different

Resolution succeeds only when the selected provider key is registered on the
Router. The built-in `Router::from_env()` registers OpenAI, Anthropic, Gemini,
and xAI only when their corresponding environment variables are present.
ChatGPT is registered when its OAuth cache exists; sign in with
`llmshim login chatgpt` before starting the proxy. Use `chatgpt/<model>` to
select subscription access. Bare GPT names continue to use OpenAI API keys.

The static model registry powers `llmshim models` and `GET /v1/models`. Those
commands are discovery aids, filtered to configured providers. The registry is
not generally an allowlist: most providers accept models absent from that list.
ChatGPT accepts only `chatgpt/gpt-6-astra` and the three
`chatgpt/gpt-5.6-{sol,terra,luna}` models. The provider rejects other IDs
before authentication or network calls, including through aliases.

For that reason, this documentation does not maintain another static model
table. Use runtime discovery for the current curated list.

## Aliases are a Rust Router feature

Rust applications can attach a one-level alias while building a Router:

```rust
let router = llmshim::router::Router::from_env()
    .alias("smart", "anthropic/claude-opus-5");
```

The Router checks an alias before parsing the provider address. An alias target
may be an explicit address or a bare model name, but aliases do not recursively
chain. If `a` points to `b` and `b` points to a model, resolving `a` does not
perform the second lookup.

Aliases are not currently configurable through the CLI, config file, proxy
API, or language clients.

## Named routes

An alias renames a model. A **named route** goes further: it maps a
caller-defined name to a model *plus* request settings, configured in
`~/.llmshim/config.toml`.

```toml
[routes.compaction]
model = "anthropic/claude-haiku-4-5-20251001"
reasoning_effort = "low"
max_tokens = 4096
```

Address it as a model:

```json
{"model": "route/compaction", "messages": [{"role": "user", "content": "…"}]}
```

Because it reuses the `provider/model` grammar, a route works everywhere a
model address does — the Rust API, the CLI, the proxy, the native endpoints and
an unmodified OpenAI SDK.

The name is **opaque to llmshim**. A harness may call a route `compaction`,
`advisor` or `webSearch`; llmshim never interprets it and has no built-in role
vocabulary. The harness decides what a name means; llmshim provides only the
mechanism.

Three rules:

- **Settings are defaults.** A key the request already carries wins, so a
  caller can pick a route and still raise `reasoning_effort` for one call.
- **An unknown name is an error** (HTTP 400), never a silent fall back to a
  default model.
- **Routes do not chain.** A route's `model` may not be another `route/…`.

A Rust application can register routes directly:

```rust
use llmshim::config::Route;

let router = llmshim::router::Router::new()
    .route("compaction", Route { model: "anthropic/claude-haiku-4-5-20251001".into(), ..Default::default() });
```

## Environment variables versus `config.toml`

`Router::from_env()` reads provider environment variables such as
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, and `XAI_API_KEY`. It
does not read API keys from `~/.llmshim/config.toml` by itself; it does read
that file's `[routes]` table, which has no environment equivalent. It also discovers the selected
ChatGPT OAuth cache, whose default location is `~/.llmshim/chatgpt/auth.json`.

The CLI and proxy call `llmshim::env::load_all()` before constructing their
Router. That function loads the config file and fills only environment
variables that are not already set, so environment variables take precedence.

A Rust application that wants the same config-file behavior must request it:

```rust
llmshim::env::load_all();
let router = llmshim::router::Router::from_env();
```

Applications that manage secrets themselves can call `Router::from_env()`
directly or construct a Router by registering provider implementations.
