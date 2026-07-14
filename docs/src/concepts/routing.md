# Models and the Router

The `model` string is a routing address. It tells llmshim which provider
adapter should receive the request and which model name that adapter should
send upstream.

## Prefer explicit addresses

The most explicit form is `provider/model`:

```text
openai/gpt-5.5
anthropic/claude-sonnet-4-6
gemini/gemini-3.5-flash
xai/grok-4.5
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

The static model registry powers `llmshim models` and `GET /v1/models`. Those
commands are discovery aids, filtered to configured providers. The registry is
not an allowlist: routing does not reject a model merely because it is absent
from that list.

For that reason, this documentation does not maintain another static model
table. Use runtime discovery for the current curated list.

## Aliases are a Rust Router feature

Rust applications can attach a one-level alias while building a Router:

```rust
let router = llmshim::router::Router::from_env()
    .alias("smart", "anthropic/claude-sonnet-4-6");
```

The Router checks an alias before parsing the provider address. An alias target
may be an explicit address or a bare model name, but aliases do not recursively
chain. If `a` points to `b` and `b` points to a model, resolving `a` does not
perform the second lookup.

Aliases are not currently configurable through the CLI, config file, proxy
API, or language clients.

## Environment variables versus `config.toml`

`Router::from_env()` means exactly what its name says: it reads
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, and `XAI_API_KEY`. It
does not read `~/.llmshim/config.toml` by itself.

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

