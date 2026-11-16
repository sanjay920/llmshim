# 0009: The access log

Status: implemented.

## What it is

```toml
[logging]
access_log = "stdout"                      # or an absolute file path
```

One JSON line per request on the main listener:

```json
{"ts":"2026-11-13T10:15:30.412Z","request_id":"gw-3f9a1c0b7e42-12","method":"POST",
 "path":"/v1/chat/completions","status":200,"latency_ms":842,
 "tenant":"payments","key":"checkout","credential":"0a1b2c3d"}
```

| Field | Meaning |
|---|---|
| `ts` | UTC, when the response headers were ready |
| `request_id` | the id of the request, also on the response as `x-request-id`, so a line joins to a caller's own logs, to the gateway's stats and to the request's usage records (0010). The log's layer is outermost and settles the id before anything else sees the request, so **every line has one**: a request refused by a layer outside the gateway's own, or dropped before any response, included. A caller's own is kept when it is well formed, 1 to 128 visible ASCII characters (a run id, a UUID, a trace id) and not in the gateway's own `gw-` form: the caller keeps its own ids unique. Anything else (longer, with a space or a control character, non-ASCII, empty, or `gw-…`) is replaced and never echoed. A request that sent none gets `gw-<instance>-<n>`: `<instance>` is 12 random hex digits drawn once when the process starts and `<n>` a counter, so ids do not repeat across replicas or restarts. A build without `serve` (the `llmshim gateway` command of such a build) keeps `gw-<pid>-<n>`, unique within one process only: in a container the pid is often 1 |
| `method`, `path` | the request line **without its query string**; `path` is cut to 256 characters |
| `status` | the HTTP status sent in the response headers, including refusals (401, 429, 404, 503); `null` when the request was abandoned (the client went away, a deadline cancelled it) before a response existed. A stream that fails after its headers is still logged with the status it began with |
| `latency_ms` | until the response headers (or the abandonment); a stream's duration is not included |
| `tenant` | the caller's tenant; `null` when the credential was missing or not recognised. The credential is read as the gateway reads it, so on the native routes (`/v1/messages`, `/v1/chat/completions`, `/v1/responses`, Gemini `generateContent`) an `x-api-key` or `x-goog-api-key` counts |
| `key` | the name (`keys[].name`) of the caller key the credential belongs to, so a line says *which* key as well as which tenant; `null` when the credential was missing or not recognised. For a client-certificate workload it is that key's name |
| `auth_reason` | `null`, or why a token was refused before the gateway saw the request: `jwt_invalid`, `jwt_unknown_key`, `jwt_unknown_subject` or `jwks_unavailable` (0017). A class, never the token or the claim that failed |
| `credential` | first eight hex characters of the SHA-256 of the bearer token (or of that header's key); `null` without one (a certificate workload has none) |

## Why these fields

Finance and security teams ask two first questions of a gateway: *who used it*
and *who tried to*. `tenant` answers the first for every request. `credential`
answers the second without revealing anything: repeated attempts with one wrong
token share a fingerprint, so credential stuffing and a misconfigured client are
both visible, and a legitimate key is recognisable across its lines. Eight hex
characters (32 bits) tell credentials apart and do not let anyone reconstruct a
token. They are not a secret, though: the digest is unsalted, so anyone who can
read the log can confirm a *guess* at a low-entropy token by hashing it. Caller
tokens are meant to be long random strings (0003), for which a 32-bit prefix
reveals nothing useful, and any bearer value is fingerprinted, valid or not, so
treat the log as sensitive and restrict who can read it.

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

Each line is written with a single `write_all` under a mutex and flushed, so a
failure cannot leave half a line that the next one is glued to. A full disk or a
closed pipe never fails a request. A request that is abandoned before its
response (the client disconnects, a deadline cancels it) still gets its line,
with `status: null`: the gateway may already have sent it upstream and been
billed. The file is created owner-only (0600) and appended to; an existing file
keeps whatever mode it has (create it with the mode you want), and a symlink is
followed. Opening happens at startup, after the rest of the configuration has
been accepted, so an unwritable path exits 1 with `logging.access_log` named and
a configuration that fails for another reason creates no file.

## Not done

- **Token and cost per tenant.** See 0010 (`usage_log`): per attempt, because a
  retry or a fallback is a separate charge, and made possible by carrying the
  tenant's name in the fleet job scope.
- **A bounded, non-blocking writer.** See 0010: both logs share one writer that
  queues lines for a thread of its own, so a stalled `stdout` pipe cannot stall
  a request.
- **Rotation.** Use `copytruncate`, or log to `stdout` and let the platform
  collect and rotate it. A reopen-on-signal is a later item.
- **Remote address and TLS client details.** The listener does not yet pass the
  peer address to the middleware.
- **Tamper evidence and export** (hash chaining, delivery to a SIEM). Those
  belong to the audit-log item.
