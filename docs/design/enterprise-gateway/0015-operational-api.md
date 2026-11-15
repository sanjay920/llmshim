# 0015: The operational endpoints are in the OpenAPI document

Status: implemented. Meets 0013's stability criterion 3 and its step 3.

## What it is

`api/openapi.yaml` already described the proxy contract. It now also describes
the operational endpoints of the gateway and of `llmshim serve`:

| Path | Auth | Body |
| --- | --- | --- |
| `GET /ready` | none, even on the admin listener | `ready\n` (200) or `not ready\n` (503) |
| `GET /metrics` | admin token | Prometheus text |
| `GET /v1/gateway/stats` | admin token | `GatewayStats`: mode, `auth_enforced`, providers, lanes |
| `GET /v1/serve/config` | admin token; `serve` only | `ServeConfigStatus` (0012) |

(`/health` was already in the document: it is part of the proxy contract.) Each
operation carries `x-llmshim-operational: true`, the admin token is a bearer
security scheme (`adminToken`), and `/ready` says `security: []` because probes
must not need a credential. The status of the whole group stays what 0013 says:
unstable until `serve` is declared stable; the gateway's additions are
experimental.

## The rule

The document is the contract and the server is held to it, by
`tests/integration_serve_openapi.rs`, which starts a real `serve` with an admin
listener and token and checks, for every operation marked operational:

- it is served on the admin listener and is a 404 on the main one;
- it needs the token exactly when the document says it does (401 with
  `WWW-Authenticate: Bearer`, also for a wrong token), and `/ready` never does;
- the body satisfies the schema the document gives: the schemas are compiled out
  of the YAML itself, so a field the server adds that the schema forbids
  (`additionalProperties: false`), or drops that it requires, fails the test;
- the shapes the quiet path never produces still validate: after a rejected
  reload, `last_reload` and `in_sync: false`.

The list of paths is pinned in the test, so a path added to the document without
the server (or the reverse, with the marker) fails, and adding one is a
deliberate edit in two places. The test was checked by mutation: narrowing a
schema, documenting a path the server lacks, and claiming a secured path is open
each fail it.

## Not done

- **Responses the test does not provoke:** `/ready` 503 (it needs a dead Redis;
  0014's outage test shows it, the schema is not applied), the `distributed`
  stats mode with `dead_letter`, and an `applied` reload attempt.
- **Plain `llmshim gateway` and the proxy have no admin token**: there
  `/metrics` and the stats are open on the main listener, which the document's
  `adminToken` describes only for `serve`. On the main listener of `serve` (no
  `[admin]` table) caller-key auth does not guard them either.
- **The Prometheus metric names and labels** are not described: the body is
  `text/plain`, and the test checks its form, not its families. A metric an
  operator alerts on should be in the document before the feature is stable.
- **`/v1/serve/config` without an `[admin]` table** is not served at all (0012);
  `/ready`, `/metrics` and the stats then share the main listener, which the
  document says and the test does not exercise.
- **The OpenAPI document is validated as JSON Schema, not as OpenAPI**: nothing
  here lints the document as a whole.
- **The bundled clients** do not expose these endpoints; they speak the proxy
  contract.
