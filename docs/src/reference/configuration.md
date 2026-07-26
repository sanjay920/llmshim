# Configuration reference

llmshim reads provider keys from environment variables. The CLI and proxy can
also load `~/.llmshim/config.toml`, filling only variables that are not already
set. Therefore **environment variables take precedence over the file**.

## Provider keys

| Provider | Environment variable | Config key |
|---|---|---|
| OpenAI | `OPENAI_API_KEY` | `keys.openai` |
| Anthropic | `ANTHROPIC_API_KEY` | `keys.anthropic` |
| Google Gemini | `GEMINI_API_KEY` | `keys.gemini` |
| xAI | `XAI_API_KEY` | `keys.xai` |
| OpenRouter | `OPENROUTER_API_KEY` | `keys.openrouter` |
| vLLM (self-hosted) | `VLLM_BASE_URL` (+ optional `VLLM_API_KEY`) | — (env only) |
| SGLang (self-hosted) | `SGLANG_BASE_URL` (+ optional `SGLANG_API_KEY`) | — (env only) |

The config file shape is:

```toml
[keys]
openai = "..."
anthropic = "..."
gemini = "..."
xai = "..."

[proxy]
host = "0.0.0.0"
port = 3000
```

Manage it without editing TOML by hand:

```bash
llmshim configure
llmshim set anthropic '...'
llmshim get anthropic
llmshim list
llmshim path
```

Valid `set`/`get` keys are `openai`, `anthropic`, `gemini`, `xai`,
`proxy.host`, and `proxy.port`. Displayed API keys are masked.

`Router::from_env()` reads environment variables only. A Rust application that
wants the file behavior must call `llmshim::env::load_all()` before constructing
the Router. See [Models and the Router](../concepts/routing.md).

## Proxy listener

| Variable | Default | Meaning |
|---|---|---|
| `LLMSHIM_HOST` | config value, then `0.0.0.0` | Bind address |
| `LLMSHIM_PORT` | config value, then `3000` | Bind port |

The environment overrides `[proxy]`. The proxy has no built-in authentication
or TLS; the bind address is not a security boundary by itself. See
[Deploy the proxy safely](../proxy/deployment.md).

## Retries

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_MAX_RETRIES` | `3` | Retries after the initial provider attempt |
| `LLMSHIM_MAX_BACKOFF_SECS` | `60` | Maximum delay for one reactive retry |

The retry policy is resolved when the shared client is initialized. See
[Errors and retries](errors.md).

## Proxy admission and rate limits

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_MAX_CONCURRENCY` | `256` | In-flight upstream requests per proxy instance |
| `LLMSHIM_QUEUE_TIMEOUT_MS` | `5000` | Wait for a concurrency slot before `503` |
| `LLMSHIM_RATE_LIMIT_RPM` | unset | Per-provider default requests per minute |
| `LLMSHIM_RATE_LIMIT_TPM` | unset | Per-provider default estimated tokens per minute |
| `LLMSHIM_<PROVIDER>_RPM` | unset | Provider RPM override |
| `LLMSHIM_<PROVIDER>_TPM` | unset | Provider TPM override |
| `LLMSHIM_PENALTY_SECS` | `5` | Bucket penalty after an upstream `429` |
| `LLMSHIM_REDIS_URL` | unset | Redis URL for optional shared coordination |

`<PROVIDER>` is `OPENAI`, `ANTHROPIC`, `GEMINI`, or `XAI`. Per-provider values
override their global dimension; an omitted dimension inherits its global
value. Redis coordination requires a binary built with `redis-coordination`.
See [Scaling and rate limits](../proxy/scaling.md).

## JSONL request logging

Set `LLMSHIM_LOG` to append one JSON object per completed request to a file:

```bash
LLMSHIM_LOG=./llmshim.jsonl llmshim proxy
```

Interactive chat also accepts an explicit path, which wins over the variable:

```bash
llmshim chat --log ./chat.jsonl
```

Each line contains `ts`, `model`, `provider`, `latency_ms`, token counts,
`status`, and optional `error` and `request_id`. Logging is local to the process;
coordinate collection and retention in your deployment platform.
