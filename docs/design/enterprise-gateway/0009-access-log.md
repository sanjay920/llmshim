# 0009: The access log

Status: implemented.

## What it is

```toml
[logging]
access_log = "stdout"                      # or an absolute file path
```

One JSON line per request on the main listener:

```json
{"ts":"2026-11-13T10:15:30.412Z","request_id":"llmshim-7f3a-12","method":"POST",
 "path":"/v1/chat/completions","status":200,"latency_ms":842,
 "tenant":"payments","credential":"0a1b2c3d"}
```

| Field | Meaning |
|---|---|
| `ts` | UTC, when the response headers were ready |
| `request_id` | the `x-request-id` the gateway put on the response, so a line joins to a caller's own logs and to the gateway's stats |
| `method`, `path` | the request line **without its query string** |
| `status` | the HTTP status, including refusals (401, 429, 404, 503) |
| `latency_ms` | until the response headers; a stream's duration is not included |
| `tenant` | the caller's tenant; `null` when the credential was missing or not recognised |
| `credential` | first eight hex characters of the SHA-256 of the bearer token; `null` without one |

## Why these fields

Finance and security teams ask two first questions of a gateway: *who used it*
and *who tried to*. `tenant` answers the first for every request. `credential`
answers the second without revealing anything: repeated attempts with one wrong
token share a fingerprint, so credential stuffing and a misconfigured client are
both visible, and a legitimate key is recognisable across its lines. Eight hex
characters (32 bits) tell credentials apart; they do not let anyone reconstruct
a token or check a guess against the log.

## What is never written

No token, header value, query string, request or response body, or model output.
A test sends a wrong key, a query string containing a marker, a prompt and a
completion, and asserts that none of them appears anywhere in the file.

## How it is built

A middleware around the whole main application, outermost, so it needs nothing
from the gateway's internals and records requests refused before any handler,
and the 404 for a path hidden by `[admin]`. It resolves the caller with a key
store built from the same file the gateway authenticates with, so the log cannot
disagree with the decision. The admin listener is not logged: probes would drown
the file.

Writes are one short line under a mutex, flushed per line. A full disk or a
closed pipe never fails a request. The file is created owner-only (0600) and
appended to; opening it happens at startup, so an unwritable path exits 1 with
`logging.access_log` named.

## Not done

- **Token and cost per tenant.** See 0010 (`usage_log`): per attempt, because a
  retry or a fallback is a separate charge, and made possible by carrying the
  tenant's name in the fleet job scope.
- **Rotation.** Use `copytruncate`, or log to `stdout` and let the platform
  collect and rotate it. A reopen-on-signal is a later item.
- **Remote address and TLS client details.** The listener does not yet pass the
  peer address to the middleware.
- **Tamper evidence and export** (hash chaining, delivery to a SIEM). Those
  belong to the audit-log item.
