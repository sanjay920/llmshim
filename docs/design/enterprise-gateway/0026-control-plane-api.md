# 0026: The control plane's HTTP API

Status: implemented (0024 slice 3b: `crates/llmshim-control`, `src/api.rs`, `src/main.rs`, `openapi.yaml`); the note was its own
commit, written before the code (README working rule 3). Bodies are read by hand so that every malformed
or unexpected one is the API's own 400 `invalid_request` (axum's extractor answers 422 for some). It fixes the surface of the
`llmshim-control` server so that the OpenAPI document and the server cannot drift.

## What it is

`llmshim-control serve --db <FILE> --listen <ADDR> --token-file <FILE> [--signing-key-file <FILE>]`
serves a JSON API over the store of 0024. One process, one SQLite file. **Not** on the
data plane's request path: a data plane reads the snapshot this API publishes (a file today;
`GET /v1/orgs/{org}/snapshot` is the pull of slice 5).

## Authentication and the actor

- Every route requires `Authorization: Bearer <token>`. The token is read from `--token-file`
  at start (owner-only on Unix is checked, as for the admin token: a group- or world-readable
  file is refused), at least 22 visible-ASCII characters, compared as SHA-256 digests in constant
  time. No token, no start. A wrong or missing token is 401 with `WWW-Authenticate: Bearer` and
  no detail.
- **One token, so one identity.** Who made a change is the `x-llmshim-actor` request header
  (1 to 128 visible characters, default `api`), recorded in the change history. It is a label
  the caller asserts, not an authenticated identity: operator identity beyond a token is SSO,
  which is not started. The note says so rather than pretending.
- Listening on a non-loopback address without TLS is allowed (a deployment terminates TLS in
  front, as for the data plane's own listener behind a proxy) but the process prints a warning
  line; the control plane's own TLS is a later slice.

## Routes

All bodies and answers are JSON (`application/json`); ids are the opaque ids of 0024.

| Method and path | Body | Answer |
| --- | --- | --- |
| `POST /v1/orgs` | `{name}` | 201 org |
| `GET /v1/orgs` | | 200 `{orgs: [...]}` |
| `POST /v1/orgs/{org}/teams` | `{name}` | 201 team |
| `GET /v1/orgs/{org}/teams` | | 200 `{teams: [...]}` |
| `POST /v1/teams/{team}/projects` | `{name}` | 201 project |
| `GET /v1/teams/{team}/projects` | | 200 `{projects: [...]}` |
| `POST /v1/projects/{project}/keys` | `{name, credential?, tier?, rpm?, tpm?, budget_usd?, budget_window_secs?, budget_allow_unpriced?, allowed_cidrs?, allowed_models?, expires_at?}` | 201 `{key, secret}` |
| `GET /v1/orgs/{org}/keys` | | 200 `{keys: [...]}` |
| `GET /v1/keys/{key}` | | 200 key |
| `POST /v1/keys/{key}/revoke` | | 200 key |
| `POST /v1/keys/{key}/rotate` | `{name}` | 201 `{key, secret}` (the successor) |
| `PATCH /v1/keys/{key}/limits` | `{rpm?, tpm?, budget_usd?, budget_window_secs?, expires_at?}` | 200 key |
| `GET /v1/history?limit=` | | 200 `{history: [...]}`, newest first, limit 1 to 1000 (default 100) |
| `POST /v1/orgs/{org}/snapshots` | | 201 `{generation}`: builds, signs and stores the next generation |
| `GET /v1/orgs/{org}/snapshot` | | 200 the latest signed envelope; `ETag: "<generation>"`; `If-None-Match` of the same generation is 304; 404 before the first publish |

- `credential` is `{"type":"secret"}` (the default), `{"type":"client_san","san":"uri:..."}` or
  `{"type":"jwt","table":"...","subject":"..."}`.
- `PATCH .../limits` is a whole-limits update, not a merge: **every** one of the five fields is named and
  `null` clears one. A body that leaves one out is a 400, so `{"rpm": 5}` cannot silently remove a budget or
  an expiry.
- Unknown fields in a body are refused (400), as in the data plane's file. The `Authorization` scheme
  is case-insensitive. The store refuses what the data plane would refuse as a whole snapshot: networks
  that do not parse (and more than 256 of them), a second active key for the same client identity
  (`dns:` names compared case-insensitively) or the same token binding, a token subject outside 1 to 512
  visible ASCII characters (409 or 400).

## Secrets

The secret appears in exactly two answers, key creation and rotation, with
`Cache-Control: no-store`. No other answer, no log line and no history row carries it. A key's
SHA-256 is in no key answer. (The published snapshot, which a data plane reads, carries each secret
key's hash by design; it is a hash of a 256-bit random secret.)

## Errors

`{"error": {"code": "...", "message": "..."}}`:
`401 unauthorized`, `400 invalid_request` (the store's `Invalid`, malformed JSON, bad ids or
limits), `404 not_found`, `409 conflict` (a taken name, a revoked key, a lost generation race:
publish again), `500 internal` with a fixed message (a database or signing failure: the detail is
logged to stderr, never sent). A signing-key file that is unreadable or not a key is a 500 for
`POST .../snapshots` naming `signing_key` and nothing else, and nothing is published.

## The signing key

`--signing-key-file` names a file holding a seed (what `llmshim snapshot keygen` writes). It is
read **at each publish** (off the async workers), never stored, so rotating it needs no restart. On Unix
a seed readable by group or others is refused at start and at each publish, like the token. The database
file is created owner-only. Without
the option, `POST .../snapshots` is 503 `signing_key_unconfigured`.

## OpenAPI

`crates/llmshim-control/openapi.yaml` is the contract; a test compares its paths and methods with
the router's route table, in both directions, so neither can gain a route without the other.

## Not here

TLS of its own, a second token or per-token scopes, rate limiting, the Postgres backend (slice 4),
the data plane's poller (slice 5), and everything an admin UI or SSO would need.
