# 0005: A separate listener for operational endpoints

Status: implemented.

## Problem

The gateway serves `/metrics`, `/v1/gateway/stats`, `/health` and `/ready` on
the same listener as inference traffic, without authentication. Metrics reveal
traffic shape, tenants' queue depths and provider names; the stats route shows
queue state. In an enterprise network these belong on an interface that callers
cannot reach, and a monitoring system needs a credential that is not a caller
key.

## Decision

An optional `[admin]` table gives the operational endpoints a listener of their
own:

```toml
[admin]
host = "127.0.0.1"           # default
port = 9090
token = { env = "ADMIN_TOKEN" }   # optional
```

- The admin listener serves exactly the four operational routes, with the same
  handlers as the main application (`gateway::http::admin_routes`).
- Once `[admin]` is present, the main listener answers **404** for those four
  paths, with or without a valid caller key. Callers cannot reach them.
- With a `token`, `/metrics` and `/v1/gateway/stats` require
  `Authorization: Bearer <token>`. `/health` and `/ready` stay open so an
  orchestrator can probe without a credential. Everything else on the admin
  listener, including unknown paths, requires the token too, so a probe
  cannot be used to map the listener.
- The token is compared as a SHA-256 digest in constant time. It follows the
  caller-token rules (at least 16 characters, no whitespace), and it must be a
  different secret from every provider key and caller key, by reference at load
  time and by resolved value in `--check` and at startup.
- Without `[admin]` nothing changes: the endpoints are on the main listener, as
  under `gateway`. The startup banner says so and points at `[admin]`.
- Both listeners are bound before anything is announced, so a port conflict on
  either leaves no half-started server. One SIGTERM or Ctrl-C drains both.

## Not done

- The admin token is a single shared secret. Per-scraper credentials, mTLS and
  an audit trail of admin access belong with the identity and audit work.
- TLS is still not terminated by `serve`; use a terminating proxy.
- `/metrics` still reports the whole process; per-tenant scoping is a separate
  item.
