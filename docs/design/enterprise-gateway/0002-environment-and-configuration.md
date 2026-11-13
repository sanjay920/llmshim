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

Converted (the parser takes a lookup; `from_env` is the lookup applied to the
environment):

| Setting | Where | Now built from |
|---|---|---|
| `LLMSHIM_GATEWAY_*` scheduler tuning (10 variables) | `GatewayConfig` | `from_lookup` |
| `LLMSHIM_RATE_LIMIT_RPM/TPM`, `LLMSHIM_<PROVIDER>_RPM/TPM` | `RateLimitConfig` | `from_lookup`, `with_limits` |
| Gateway state assembly | `GatewayState` | `from_parts(LocalParts)` |
| `LLMSHIM_BREAKER_*` | `BreakerConfig` | `from_lookup`; `build_breaker_with(config, redis_url)` |
| `LLMSHIM_REDIS_URL` for limiter and breaker | `build_limiter`, `build_breaker` | `build_limiter_with`, `build_breaker_with` |
| `LLMSHIM_PROXY_*_TIMEOUT_MS`, `LLMSHIM_GATEWAY_*_JOB_TIMEOUT_MS` | `LogicalRequestDeadlines` | `from_lookup`; `from_durations` (errors instead of defaulting) |
| `LLMSHIM_REPLAY_RECEIPTS_*` (dir, three limits, rescan flag) | `Receipts` | `from_lookup`; `from_parts` with `Limits::new` (errors instead of defaulting) |
| Application assembly | proxy and gateway `app*` | `app_with_parts(state, origin_policy, deadlines, receipt_store)` |
| Caller keys | `KeyStore` | the hashed store in 0003 |
| `LLMSHIM_TRUSTED_ORIGINS` | `OriginPolicy` | already had an env-free constructor, `from_csv`; `from_env` only handles the missing and non-Unicode cases |

Still read from the environment, to convert before `serve` can boot:

| Setting | Where | Notes |
|---|---|---|
| `LLMSHIM_GATEWAY_MAX_RETAINED_ACCOUNTING_ATTEMPTS` | `gateway/attempt.rs` | Read when the ledger is built. |
| `LLMSHIM_PENALTY_SECS` | `penalty_duration()` | Read on each upstream 429. |
| `LLMSHIM_REPLAY_RECEIPTS_LOCK_TIMEOUT_MS` | `configured_http_lock_timeout()` | Read per request. |
| `LLMSHIM_MAX_RETRIES`, `LLMSHIM_MAX_BACKOFF_SECS`, `LLMSHIM_UPSTREAM_*`, `LLMSHIM_HTTP_*` | shared HTTP client | Process-wide, initialized once. Needs a per-router client before it can come from a file. |
| `LLMSHIM_MAX_CONCURRENCY`, `LLMSHIM_QUEUE_TIMEOUT_MS` | `Backpressure::from_env` | Proxy only; `Backpressure::new` already takes values. |
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
- Receipt-store numbers outside their allowed range, and the lifetimes when zero,
  fall back to the default. The explicit constructors (`Limits::new`,
  `from_durations`) return an error for the same values instead.
- Only the exact string `1` turns on `LLMSHIM_REPLAY_RECEIPTS_RESCAN_INCOMPLETE_BASELINE`.
- Per-provider rate-limit variables exist only for `openai`, `anthropic`,
  `gemini` and `xai`. Other providers get the global limit only.

Under `serve` none of these apply: the file is strictly validated, and limits
can name any configured provider.
