# 0002: Environment variables and the configuration file

Status: partly implemented. The gateway's single-instance state can now be built
from explicit values; `serve` does not use that yet.

## Problem

`llmshim proxy` and `llmshim gateway` read their settings from the process
environment inside constructors named `from_env`. `serve` must build the same
components from a configuration file. Two shortcuts are ruled out:

- Setting environment variables and then calling `from_env`. The environment is
  process-global, so this is racy, invisible to review, and makes reload
  impossible.
- Copying the parsing into a second code path. The two would drift.

## Decision

Each `from_env()` becomes a thin wrapper over a constructor that takes explicit
values, so there is one implementation and the environment is one source among
others:

- Parsing: `from_env()` is `from_lookup(&|key| std::env::var(key).ok())`. Tests
  call `from_lookup` with a fixed table, so the environment contract is pinned
  without touching the real environment.
- Assembly: `GatewayState::from_env(router, logger)` is
  `from_parts(router, logger, LocalParts::from_env())`.

Existing behavior must not change. Each conversion lands with tests that pin
the environment parsing as it was, including its quirks (see below).

### Precedence under `serve`

`serve` reads nothing from the environment except the variables the file names
in secret references. A setting absent from the file takes its documented
default, not an `LLMSHIM_*` variable. One artifact fully describes the server.

`proxy` and `gateway` keep their current rules (CLI flag, then environment,
then saved configuration).

## Inventory

Converted in this step:

| Setting | Where | Now built from |
|---|---|---|
| `LLMSHIM_GATEWAY_*` scheduler tuning (10 variables) | `GatewayConfig` | `from_lookup` |
| `LLMSHIM_RATE_LIMIT_RPM/TPM`, `LLMSHIM_<PROVIDER>_RPM/TPM` | `RateLimitConfig` | `from_lookup`, `with_limits` |
| Gateway state assembly | `GatewayState` | `from_parts(LocalParts)` |

Still read from the environment, to convert before `serve` can boot:

| Setting | Where | Notes |
|---|---|---|
| `LLMSHIM_GATEWAY_KEYS_FILE` | `KeyStore::from_env` | Exits the process on a bad file. `serve` uses the hashed store in 0003 instead. |
| `LLMSHIM_GATEWAY_IDEMPOTENCY_TTL_SECS` | `gateway/http.rs` | Already a plain field of `LocalParts`. |
| `LLMSHIM_TRUSTED_ORIGINS` | `OriginPolicy::from_env` | Read when the router is built. |
| `LLMSHIM_PROXY_*_TIMEOUT_MS`, gateway job deadlines | `LogicalRequestDeadlines` | Read when the router is built. |
| `LLMSHIM_REPLAY_RECEIPTS_*` | `Receipts::from_env` | A process-wide default store. |
| `LLMSHIM_REDIS_URL` | limiter, breaker, gateway | Selects fleet mode. |
| `LLMSHIM_BREAKER_*` | `BreakerConfig::from_env` | |
| `LLMSHIM_PENALTY_SECS` | `penalty_duration()` | Read on each upstream 429. |
| `LLMSHIM_GATEWAY_MAX_RETAINED_ACCOUNTING_ATTEMPTS` | `gateway/attempt.rs` | Read when the ledger is built. |
| `LLMSHIM_MAX_RETRIES`, `LLMSHIM_MAX_BACKOFF_SECS`, `LLMSHIM_UPSTREAM_*` | shared HTTP client | Process-wide, initialized once. Needs a per-router client before it can come from a file. |
| `LLMSHIM_MAX_CONCURRENCY`, `LLMSHIM_QUEUE_TIMEOUT_MS` | `Backpressure::from_env` | Proxy only. |
| `LLMSHIM_HTTP_MAX_CONNECTIONS`, `LLMSHIM_HTTP_HEADER_TIMEOUT_MS` | `http_server` | Inbound listener limits; read by the proxy and gateway entry points. |
| `LLMSHIM_ATTEMPT_POLICY_TIMEOUT_MS` | shared HTTP client deadlines | Process-wide, with the `LLMSHIM_UPSTREAM_*` timeouts above. |
| `LLMSHIM_NO_STRICT`, `LLMSHIM_NO_SCHEMA_NORMALIZATION`, `LLMSHIM_NO_SCHEMA_CACHE` | schema normalization | Debugging bypasses; process-wide, read at use. A deployment that sets them changes what the server sends upstream, so `serve` documents them as unsupported settings. |
| Distributed-mode limits | `gateway/distributed*` | Converted with fleet support. |

## Quirks found and preserved

These are pinned by tests, not fixed, because fixing them would change
`gateway` and `proxy` behavior:

- Gateway variables are not trimmed, while rate-limit variables are. `" 12 "`
  is ignored for `LLMSHIM_GATEWAY_MAX_WAIT_MS` and accepted for
  `LLMSHIM_RATE_LIMIT_RPM`.
- An unparsable value is ignored silently and the default is used.
- Zero is refused for the two job timeouts but accepted for `MAX_WAIT_MS` and
  `QUEUE_DEPTH`.
- Per-provider rate-limit variables exist only for `openai`, `anthropic`,
  `gemini` and `xai`. Other providers get the global limit only.

Under `serve` none of these apply: the file is strictly validated, and limits
can name any configured provider.
