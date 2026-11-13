# 0004: `llmshim serve` runs the gateway from a file

Status: implemented for a single instance. Fleet mode, reload and TLS are not
implemented.

## What it does

`llmshim serve --config <file>` validates the file, resolves its secret
references, builds the gateway from it, binds `server.host:server.port` and
serves until SIGTERM or Ctrl-C, draining in-flight requests. The HTTP surface is
the existing gateway's: `/v1/chat`, `/v1/chat/stream`, `/v1/chat/completions`,
`/v1/messages`, `/v1/responses`, the Gemini `generateContent` wire, `/v1/models`,
`/health`, `/ready`, `/metrics` and `/v1/gateway/stats`. There is no second
dispatch, authentication or queue path: `serve` builds a `GatewayState` and an
application through `app_with_parts`, the same constructors `gateway` uses.

Exit codes: `0` clean shutdown, `1` invalid configuration or failure to start,
`2` usage error. A configuration problem is reported with every issue and its
path before any port is bound or directory is created.

## What the file controls

| Setting | Becomes |
|---|---|
| `[server] host, port` | the listener (`port = 0` picks a free port; the chosen address is printed) |
| `[providers.<name>]` | one registered provider; its key is read from the secret reference |
| `[limits] max_concurrency, queue_timeout_ms` | scheduler concurrency per provider, and how long a request may wait in the queue |
| `[limits] rpm, tpm`, `[limits.providers.<name>]` | provider rate limits, for the scheduler and for every retry, repair and fallback attempt |
| `[[keys]]` | the hashed caller-key store (0003), with each key's tier, rate limits and spend cap |
| `[routes.<name>]` | named routes: `route/<name>` |
| `[state] receipts_dir, idempotency_ttl_secs` | where replay receipts live, and how long an idempotent answer is kept |

Everything else takes the gateway's documented default, never an `LLMSHIM_*`
variable. `serve` reads the environment only to resolve `{ env = ... }`
references. A test starts the server with `LLMSHIM_GATEWAY_KEYS_FILE` pointing
at a missing file, `LLMSHIM_GATEWAY_QUEUE_DEPTH=0`, `LLMSHIM_RATE_LIMIT_RPM=0`
and a dead `LLMSHIM_REDIS_URL`; each would break the server if read, and the
server works.

## Rules worth knowing

- **One hosted provider per kind, named for its kind.** A `kind = "anthropic"`
  provider must be called `anthropic`, because prices, limits and the catalog
  are looked up by that name. Self-hosted servers (`openai-compatible`) may
  have any name that is not a hosted name. Several credentials or regions for
  one provider are the deployments feature on the roadmap, not an overload of
  this name.
- **No browser origin is trusted.** A browser-originated request is refused.
  The gateway is called by services.
- **Caller keys are authenticated before the upstream is touched**, and a
  caller token is never forwarded; the upstream sees only the provider key.
- **`ProviderBreaker` and the rate limiter are in-process.** A fleet needs the
  Redis mode, which `serve` does not offer yet.

## Known limits (each is a roadmap item)

- `/metrics`, `/ready` and `/health` are unauthenticated, as they are under
  `gateway`. Bind to an internal interface or restrict them at the network
  layer until they are separately configurable.
- No TLS termination. Run behind a terminating proxy or service mesh.
- No reload. A change needs a restart; the parsed configuration is immutable so
  a swap can be added without touching the data plane.
- Process-wide settings still read the environment where they are consumed:
  the shared HTTP client's retry and timeout variables, and the HTTP listener's
  connection and header limits (`LLMSHIM_HTTP_*`). They default sensibly and are
  listed in 0002.
- The catalog is the vendored snapshot plus any local override files; `serve`
  makes no catalog refresh call at startup.
- No logging sink beyond stderr banners. Structured request logs and traces are
  the observability roadmap item.
