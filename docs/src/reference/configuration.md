# Configuration reference

llmshim reads provider keys from environment variables. The CLI and proxy can
also load `~/.llmshim/config.toml`, filling only variables that are not already
set. Therefore **environment variables take precedence over the file**.

## Provider keys

| Provider | Environment variable | Config key |
|---|---|---|
| OpenAI | `OPENAI_API_KEY` | `keys.openai` |
| ChatGPT subscription | `llmshim login chatgpt` (OAuth cache) | — |
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

`Router::from_env()` reads environment variables and discovers the ChatGPT OAuth cache. A Rust application that
wants the file behavior must call `llmshim::env::load_all()` before constructing
the Router. See [Models and the Router](../concepts/routing.md).

## ChatGPT OAuth

Run `llmshim login chatgpt` and complete the printed device-code login before
using `chatgpt/<model>`. `llmshim login chatgpt --status` checks the cache
locally; `llmshim logout chatgpt` removes it without revoking the upstream
session. Device-code login may need enabling in ChatGPT security settings or
workspace permissions. See [OpenAI authentication](https://learn.chatgpt.com/docs/auth).

| Variable | Default | Meaning |
|---|---|---|
| `CHATGPT_TOKEN_DIR` | `~/.llmshim/chatgpt` | Writable OAuth cache directory |
| `CHATGPT_AUTH_FILE` | `auth.json` | Cache filename (an absolute path overrides the directory) |
| `CHATGPT_API_BASE` | `https://chatgpt.com/backend-api/codex` | Backend base URL; `/responses` is appended |
| `OPENAI_CHATGPT_API_BASE` | unset | Alias used when `CHATGPT_API_BASE` is unset or empty |
| `CHATGPT_ORIGINATOR` | `codex_cli_rs` | Backend originator header |
| `CHATGPT_USER_AGENT` | version/platform string identifying llmshim | User-Agent override |
| `CHATGPT_USER_AGENT_SUFFIX` | unset | Text appended to User-Agent |

Tokens use LiteLLM's flat JSON format (`access_token`, `refresh_token`,
`id_token`, `expires_at`, `account_id`). The default cache is separate from
LiteLLM and Codex. Refreshes are serialized across processes sharing that file
and saved atomically; new token files have Unix mode `0600`. An expired or
unreadable session returns an error, never a background interactive login.

On Unix, reads of the default cache also repair owned directory modes to
`0700` and the file mode to `0600`. Symlinks, additional hard links, nonregular
files, and paths owned by another user are refused before token parsing.
Already-private handles do not require a chmod. Explicit `CHATGPT_TOKEN_DIR`
or `CHATGPT_AUTH_FILE` overrides remain operator-managed and are read without
this default-path permission repair; secure those locations separately.

The router registers ChatGPT when the selected cache file exists. Create a new
router or restart the proxy after the first login. Once registered, requests
read the cache each time, so refreshed or replaced tokens need no restart.
Container users must mount the cache directory writable, including space for
its lock and temporary files. The stock Docker helper does not mount it.

## Proxy listener

| Variable | Default | Meaning |
|---|---|---|
| `LLMSHIM_HOST` | config value, then `0.0.0.0` | Bind address |
| `LLMSHIM_PORT` | config value, then `3000` | Bind port |
| `LLMSHIM_HTTP_MAX_CONNECTIONS` | `1024` | Accepted TCP connections per ordinary proxy or gateway process |
| `LLMSHIM_HTTP_HEADER_TIMEOUT_MS` | `15000` | First request/protocol-preface deadline and subsequent HTTP/1 header deadline |
| `LLMSHIM_TRUSTED_ORIGINS` | unset | Comma-separated exact browser origins allowed to call the proxy or gateway |

The environment overrides `[proxy]`. The proxy has no built-in authentication
or TLS; the bind address is not a security boundary by itself. See
[Deploy the proxy safely](../proxy/deployment.md). When `LLMSHIM_TRUSTED_ORIGINS`
is unset, requests with an `Origin` header are rejected before dispatch; SDK
requests without one are unchanged.

The ordinary `proxy` and `gateway` commands acquire a connection slot before
accepting a socket and retain it through the response body and connection close.
Excess sockets wait in the operating system's listen backlog. Invalid, zero, or
unrepresentable limit values retain the defaults. The first-header deadline also
covers silent sockets and incomplete HTTP/2 prefaces; it ends when the application
receives the first request headers, so valid inference and streams can outlast it.

HTTP/1 retains keepalive with a 64 KiB read buffer and at most 100 headers.
HTTP/2 advertises a 64 KiB header-list limit and at most 128 concurrent streams
per connection, with 30-second keepalive probes and a 10-second acknowledgement
timeout. These transport controls are separate from request-body upload limits,
logical request lifetimes, and provider concurrency. Managed client processes
retain their separate TLS listener limits. Rust applications serving the exported
routers through their own listener must configure equivalent transport limits.

## Retries

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_MAX_RETRIES` | `3` | Retries after the initial provider attempt |
| `LLMSHIM_MAX_BACKOFF_SECS` | `60` | Maximum delay for one reactive retry |

The retry policy is resolved when the shared client is initialized. See
[Errors and retries](errors.md).

## Upstream attempt deadlines

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_UPSTREAM_CONNECT_TIMEOUT_MS` | `30000` | TCP/TLS connect timeout |
| `LLMSHIM_UPSTREAM_HEADER_TIMEOUT_MS` | `600000` | Response-header timeout for each physical send |
| `LLMSHIM_ATTEMPT_POLICY_TIMEOUT_MS` | `10000` | Timeout for each trusted attempt-policy callback |
| `LLMSHIM_UPSTREAM_ERROR_BODY_IDLE_TIMEOUT_MS` | `10000` | Idle timeout while reading a provider error body |
| `LLMSHIM_UPSTREAM_ERROR_BODY_TOTAL_TIMEOUT_MS` | `30000` | Total provider error-body read time |
| `LLMSHIM_UPSTREAM_UNARY_IDLE_TIMEOUT_MS` | `300000` | Idle timeout while reading a successful unary body |
| `LLMSHIM_UPSTREAM_UNARY_ATTEMPT_TIMEOUT_MS` | `1800000` | Total unary physical-attempt lifetime |
| `LLMSHIM_UPSTREAM_STREAM_IDLE_TIMEOUT_MS` | `300000` | Idle time between complete, non-empty SSE data events |
| `LLMSHIM_UPSTREAM_STREAM_ATTEMPT_TIMEOUT_MS` | `7200000` | Total streaming physical-attempt lifetime |

Values are resolved when `ShimClient` is created. Zero, invalid, and
unrepresentable values retain the finite default. Rust callers can instead pass
checked `AttemptDeadlines` through `ShimClient::with_attempt_deadlines`.

These clocks bound a physical provider send, its response body, normalization,
accounting, and attempt-policy callbacks. The proxy's separate logical clocks
below cover the complete HTTP operation. Gateway response and job lifetimes are
separate. The distributed gateway's existing 120-second request timeout is
unchanged and may be the narrower boundary.

## Proxy admission and rate limits

| Variable | Default | Meaning |
|---|---:|---|
| `LLMSHIM_MAX_CONCURRENCY` | `256` | In-flight upstream requests per proxy instance |
| `LLMSHIM_QUEUE_TIMEOUT_MS` | `5000` | Wait for a concurrency slot before `503` |
| `LLMSHIM_PROXY_UNARY_TIMEOUT_MS` | `7200000` | Absolute unary request and final-body lifetime after preparation admission |
| `LLMSHIM_PROXY_STREAM_TIMEOUT_MS` | `21600000` | Absolute streaming request and final-body lifetime after preparation admission |
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

The proxy logical clock includes request decoding, provider preparation,
retries, repair and fallback, native response conversion, and final response
body production. `/v1/chat/stream` selects the stream clock immediately. Routes
whose mode comes from JSON begin on the unary clock while the body is read and
extend to the stream deadline, anchored to the original admission time, only
after canonical `stream:true` has been parsed. Zero, invalid, and
unrepresentable values retain the finite defaults.

Inbound upload parsing consumes this total clock. It is not a separate upload
header, body-idle, or per-chunk timeout; deployments still need their HTTP
front end to enforce those narrower upload controls.

Each built-in provider attempt checks this logical clock after provider request
preparation, before and after attempt-policy admission, and again immediately
before initiating the HTTP request. If successful admission finishes after the
deadline, the attempt is abandoned through the existing conservative policy
accounting path. The check prevents a send once built-in code observes expiry;
it cannot preempt synchronous code or retract a kernel operation that was
already authorized while the clock was live.

Expiry before response headers returns `504` in the selected route's JSON
shape. After SSE headers, expiry emits the route's error event when the prior
event ended at a valid boundary; expiry in a partial event ends the body with a
transport error. A unary response whose headers are already committed also
ends with a transport error.

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
