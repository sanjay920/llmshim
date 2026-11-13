# 0009: The access log

Status: implemented.

## What it is

```toml
[logging]
access_log = "stdout"                      # or an absolute file path
```

One JSON line per request on the main listener:

```json
{"ts":"2026-11-13T10:15:30.412Z","request_id":"gw-4127-12","method":"POST",
 "path":"/v1/chat/completions","status":200,"latency_ms":842,
 "tenant":"payments","credential":"0a1b2c3d"}
```

| Field | Meaning |
|---|---|
| `ts` | UTC, when the response headers were ready |
| `request_id` | the `x-request-id` the gateway put on the response, so a line joins to a caller's own logs and to the gateway's stats. A caller may send its own, and it is echoed; otherwise the gateway mints `gw-<pid>-<n>`, which is unique within one process only (in containers the pid is often 1, so two replicas, or a restart, can repeat an id: join on `ts` and the replica's own identity as well) |
| `method`, `path` | the request line **without its query string**; `path` is cut to 256 characters |
| `status` | the HTTP status sent in the response headers, including refusals (401, 429, 404, 503); `null` when the request was abandoned (the client went away, a deadline cancelled it) before a response existed. A stream that fails after its headers is still logged with the status it began with |
| `latency_ms` | until the response headers (or the abandonment); a stream's duration is not included |
| `tenant` | the caller's tenant; `null` when the credential was missing or not recognised. The credential is read as the gateway reads it, so on the native routes (`/v1/messages`, `/v1/chat/completions`, `/v1/responses`, Gemini `generateContent`) an `x-api-key` or `x-goog-api-key` counts |
| `credential` | first eight hex characters of the SHA-256 of the bearer token (or of that header's key); `null` without one |

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

- **Token and cost per request.** Usage and cost are known inside the gateway at
  the attempt level (retries and fallbacks are separate charges), and the job
  scope that crosses Redis in fleet mode carries a hash of the tenant, not its
  name. Attributing tokens and cost to a tenant needs a change to that job
  format, so it gets its own design note before any code.
- **A bounded, non-blocking writer.** Lines are written on the request's own
  task, so a stalled `stdout` pipe stalls requests. A queue and a writer thread,
  with a counter for dropped lines, is the fix; it comes with the shared writer
  the usage log (0010) uses.
- **Rotation.** Use `copytruncate`, or log to `stdout` and let the platform
  collect and rotate it. A reopen-on-signal is a later item.
- **Remote address and TLS client details.** The listener does not yet pass the
  peer address to the middleware.
- **Tamper evidence and export** (hash chaining, delivery to a SIEM). Those
  belong to the audit-log item.
