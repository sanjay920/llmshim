# 0024: The control plane

Status: design, 2026-11-16. A contract note, written before any code (README working
rule 3): a snapshot format and an HTTP surface each become a compatibility promise.
It is step 13 of 0013's order of work. It does not start the admin UI, SSO, SCIM or
audit export (their licensing is undecided, and all of them follow this).

## What a control plane is for

Today the file is the source of truth for who may call (`[[keys]]`, 0001, reloaded by
0012), with what limits, and from where (0017). That is right for one team that owns
its file. It does not serve an organization with many teams: someone must create a key
for a team, rotate it, revoke it, set the team's budget and see what changed, without
editing a file on every replica. The control plane is the place that state lives and
changes. It is **not** in the request path, and the data plane never depends on it
being up.

## Decisions already taken (0013)

- **Postgres for the control plane, SQLite for one node.** Key lifecycle, budgets and the
  change history need transactions and queries that a file or Redis do not give.
- **Never in the request path.** The data plane (`serve`) keeps serving from the last
  good **signed policy snapshot** when the control plane or its database is down.

## The model

Entities, each with a stable opaque id (`org_…`, `team_…`, `proj_…`, `key_…`) that never
changes and is never reused. Names are labels and may change.

- **Organization**: the root; owns teams and the signing keys of its snapshots.
- **Team** and **project**: the grouping a budget or a limit attaches to. A project
  belongs to one team. (Hierarchical budgets are a later note; this one only fixes that
  every key belongs to exactly one project, so a budget has somewhere to attach.)
- **Key**: a caller credential: `key_id`, project, a label, the **SHA-256 of the secret**
  (the secret is shown once at creation and never stored), optional `expires_at`,
  optional `allowed_cidrs` (0017), optional `jwt` binding (0017), limits (`rpm`, `tpm`,
  `tier`), a state, and timestamps.
- **Key states**: `active` → `revoked` (terminal) and `active` → `expired` (by time). A
  rotation creates a **second** key and revokes the first after an operator-chosen
  overlap; there is no "replace in place", so an incident can name exactly which
  credential did what. A revoked key's hash stays in the table so a snapshot can say it is
  revoked, and a reused secret cannot be minted again.
- **Change history**: every mutation appends one row (who, what, when, before and after
  by id, never a secret). It exists so a snapshot's content can be explained. Exporting
  it is the audit-export feature and is not part of this note.

Provider credentials (upstream API keys) are **not** control-plane data: they stay in
the file or a secret store referenced from it (0001, 0022). A snapshot carries policy
about callers, never a secret an upstream accepts.

## The signed policy snapshot

One JSON document, canonical (sorted keys, no insignificant whitespace, UTF-8), signed
with **Ed25519** (`ring`, already a dependency for JWT, 0017):

```json
{
  "version": 1,
  "org": "org_…",
  "generation": 42,
  "issued_at": "2026-11-16T12:00:00Z",
  "tenants": [{"id": "team_…", "name": "platform"}],
  "keys": [{"id": "key_…", "tenant": "team_…", "sha256": "…hex…", "state": "active",
            "expires_at": null, "tier": 1, "rpm": 600, "tpm": 100000,
            "allowed_cidrs": ["10.0.0.0/8"], "jwt": null}]
}
```

and a detached signature over those exact bytes, with the id of the signing key.

- **It is exactly what `[[keys]]` and `[[tenants]]` are today**, in a verifiable
  envelope. The data plane builds the same `KeyStore`, `AllowTable` and tenant names from
  it that it builds from the file, so everything downstream (admission, limits, access
  and usage logs, 0009/0010/0012/0017) is unchanged. The snapshot is a **keys source**, not
  a new authentication path.
- **`generation` is monotonic.** A data plane never applies a snapshot whose generation
  is not greater than the one in force (so a captured old snapshot cannot un-revoke a key),
  and a replica that restarts reads the generation it last applied from its state
  directory.
- **Verification keys are configuration** (`[control_plane] verify_keys`, a list, so a
  signing key rotates by adding the new public key first). An unsigned snapshot, a wrong
  signature, an unknown key id, an unknown `version` and a malformed document are each
  refused whole, with the previous snapshot still serving, by the same rules as a rejected
  reload (0012: all-or-nothing, by path, no secret in the message).
- **Revoked keys are in it** (state `revoked`) rather than absent, so the data plane can
  say why a credential is refused in its log, and the document can be diffed.

## How the data plane gets it

`[control_plane]` is a new table, **mutually exclusive with `[[keys]]`** (a file with both
is refused: two sources of truth for who may call is a defect, not a feature).

```toml
[control_plane]
verify_keys = ["ed25519:…"]        # public keys; any may have signed
snapshot_file = "/var/lib/llmshim/snapshot.json"   # slice 2: a watched file
# source = { url = "https://cp.internal/v1/snapshot", token = { file = "…" } }  # slice 5
max_age_secs = 86400               # report (alert), never refuse, by default
```

- **Slice 2 is a file**: the control plane (or an operator, or a pipeline) writes the
  signed snapshot to a path and the data plane watches it with the machinery 0012 already
  has (two-poll debounce, `SIGHUP`, all-or-nothing, a status on `/v1/serve/config`). No
  network, no database: the data-plane half ships and is testable before a control plane
  exists.
- **Slice 5 is a pull**: `GET /v1/snapshot` with `If-None-Match: <generation>`; the data
  plane polls (default 15 s), verifies, and applies through the same code. It is a pull
  so that a control plane that is down costs only freshness.
- **Stale is served, and said.** With no new snapshot, the last good one keeps serving
  indefinitely; `max_age_secs` only turns the status field `stale: true` and a metric on.
  The 0013 rule (identity and money fail closed) holds because a caller is still
  identified from the snapshot in force. An operator who wants a hard stop sets
  `max_age_secs` with `refuse_when_stale = true` (off by default), which refuses new
  requests with 503 once the snapshot is older than that.
- **Revocation latency is the snapshot interval** (file watch or pull), stated, not hidden.
  A control plane that must revoke faster can also write the file (a signal reloads at once).

## Spend allowance during a Redis outage

With a fleet (0006) a Redis outage today refuses every request (0014): safe, not
available. A deployment may prefer to keep serving a bounded amount. The design:

- While Redis is healthy, each replica holds a **lease** of each tenant's budget (a
  slice of the window's remaining allowance, refreshed in the existing Redis spend
  window). During an outage a replica admits against its lease **only**, and refuses
  when it is spent. Overspend is therefore bounded by the sum of outstanding leases, a number
  the operator sets (`[fleet] outage_allowance_percent`, default 0 = today's behaviour).
- When Redis returns, spent leases are reconciled into the shared window and unspent
  ones are returned. A request admitted on a lease is recorded as such in the usage log.
- It is off by default, applies only to budgeted tenants, and needs its own measured test
  (cut Redis with the TCP proxy of `integration_serve_outage`, spend past the lease, expect
  refusals at the bound). It is slice 6 and independent of the snapshot work.

## The control plane's own API

A separate binary and crate (`llmshim-control`, working rule 2: a published crate cannot
depend on an unpublished one), added only when slice 3 starts. HTTP+JSON, an OpenAPI
document, bearer-token callers (identity for operators beyond a token is SSO: not started).
Operations: create/list/get org, team, project; create key (returns the secret once);
revoke key; rotate key (creates the successor); set limits and expiry; read the change
history; `POST /v1/snapshots` (build, sign and store the next generation) and
`GET /v1/snapshot` (slice 5). The signing key is a file or a secret-manager reference
(never in the database).

## What must stay true

- A data plane with no control plane behaves exactly as today. Default, proxy, gateway
  and gateway-redis builds do not change; the snapshot source is `serve` only.
- No secret in a snapshot, the change history, a log line or a status.
- A snapshot that does not verify, or does not advance, never changes what is serving.
- The control plane's database holds hashes of caller keys, never the keys.

## Not decided here

- Hierarchical budgets, model allowlists, tags, alerts and cost exports (the next roadmap
  item; they attach to the project and team of this note and to the snapshot as new
  versioned fields).
- Deployments and load balancing across credentials and regions.
- Licensing of SSO, SCIM, audit export and the admin UI (still the owner's).
- Postgres schema and migration tooling (decided with slice 3 against a running schema).

## Slices

1. This note.
2. The snapshot format, canonical encoding, Ed25519 signing and verification, and
   `[control_plane] snapshot_file` as a keys source reloaded by the 0012 machinery, with
   a small `snapshot` subcommand set (`sign`, `verify`) so a pipeline can produce one
   before a control plane exists. Tests: signature, tamper, wrong key, unknown version,
   non-advancing generation, mutual exclusion with `[[keys]]`, stale status, revocation
   effect on the next request.
3. The `llmshim-control` crate: SQLite store, the model above, key lifecycle API.
4. Postgres.
5. The pull delivery and the data plane's poller.
6. The outage spend allowance.
