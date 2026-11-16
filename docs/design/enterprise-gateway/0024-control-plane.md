# 0024: The control plane

Status: design, 2026-11-16; slice 2 implemented (2a, 2b-1, 2b-2) (see "Slice 2a as built"). A contract note, written before any code (README working
rule 3): a snapshot format and an HTTP surface each become a compatibility promise.
It is step 13 of 0013's order of work. It does not start the admin UI, SSO, SCIM or
audit export (their licensing is undecided, and all of them follow this).

## What a control plane is for

Today the file is the source of truth for who may call (`[[keys]]`, 0001, reloaded by
0012), with what limits, and from where (0017). That is right for one team that owns
its file. It does not serve an organization with many teams: someone must create a key
for a team, rotate it, revoke it, set its limits and budget and see what changed,
without editing a file on every replica. The control plane is the place that state
lives and changes. It is **not** in the request path, and the data plane never depends
on it being up.

## Decisions already taken (0013)

- **Postgres for the control plane, SQLite for one node.** Key lifecycle, budgets and the
  change history need transactions and queries that a file or Redis do not give.
- **Never in the request path.** The data plane (`serve`) keeps serving from the last
  good **signed policy snapshot** when the control plane or its database is down.

## The model

Entities, each with a stable opaque id (`org_…`, `team_…`, `proj_…`, `key_…`) that never
changes and is never reused. Names are labels and may change.

- **Organization**: the root; owns teams.
- **Team** and **project**: the grouping a limit or budget attaches to. A project belongs
  to one team. Every key belongs to exactly one project. (Budgets above the key, and
  hierarchies, are a later note; this one fixes only where they will attach.)
- **Key**: a caller credential of one kind: a **secret** (its SHA-256 is stored; the secret
  is shown once at creation and never stored), a **client certificate** identity
  (`client_san`, 0011) or a **workload token** binding (`jwt`, 0017). It carries a label,
  optional `expires_at`, `allowed_cidrs` (0017), `tier`, `rpm`, `tpm`, the budget fields a
  `[[keys]]` entry has today (`budget_usd`, `budget_window_secs`, `budget_allow_unpriced`,
  0010), a state and timestamps.
- **Key states**: `active` → `revoked` (terminal) and `active` → `expired` (by time). A
  rotation creates a **second** key and revokes the first after an operator-chosen
  overlap; there is no "replace in place", so an incident can name exactly which credential
  did what. A revoked key's hash stays so a snapshot can say it is revoked and a reused
  secret cannot be minted again.
- **Change history**: every mutation appends one row (who, what, when, ids, never a
  secret). It exists so a snapshot's content can be explained. Exporting it is the
  audit-export feature and is not part of this note.

Provider credentials (upstream API keys) are **not** control-plane data: they stay in the
file or a secret store referenced from it (0001, 0022). A snapshot carries policy about
callers, never a secret an upstream accepts.

## The signed policy snapshot

### Envelope (one file)

```json
{
  "format": "llmshim-policy-snapshot/1",
  "key_id": "ed25519:…",
  "payload": "<base64 of the canonical payload bytes>",
  "signature": "<base64 Ed25519 signature over exactly those bytes>"
}
```

The payload is base64 so the signed bytes are exactly the bytes verified: no
re-serialization step can change what was signed. An unknown `format`, a missing field, a
`key_id` that is not in `verify_keys`, a bad signature and a malformed payload are each
refused whole, by the rules of a rejected reload (0012: all-or-nothing, by path, no secret
and no part of the document in the message).

### Payload (canonical JSON: sorted keys, no insignificant whitespace, UTF-8)

```json
{
  "version": 1,
  "org": "org_…",
  "generation": 42,
  "issued_at": "2026-11-16T12:00:00Z",
  "tenants": [{"id": "team_…", "name": "platform"}],
  "keys": [{"id": "key_…", "name": "ci", "tenant": "team_…", "state": "active",
            "sha256": "…hex…", "expires_at": null, "tier": 1, "rpm": 600, "tpm": 100000,
            "budget_usd": 50.0, "budget_window_secs": 86400, "budget_allow_unpriced": false,
            "allowed_cidrs": ["10.0.0.0/8"]}]
}
```

(One credential kind per key: `sha256`, or `client_san`, or `jwt: {table, subject}`.)

- **It is what `[[keys]]` is today, in a verifiable envelope.** There is no `[[tenants]]`
  table today: a tenant is the string a key carries, and its name for the logs is that
  string. In a snapshot the key's `tenant` is the **team id** (opaque, what the fleet and
  the spend ledger key by) and the snapshot's `tenants` gives the name the logs print
  (feeding `TenantNames`/`KeyNames`, 0010). `name` is the key's label, as today.
- The data plane builds the same `KeyStore`, `AllowTable` and name tables from it that it
  builds from the file, so admission, limits, budgets, the logs and 0012/0017 are
  unchanged. **The snapshot is a keys source, not a new authentication path.**
- **Budgets stay per credential**, as today (distinct keys of one tenant have separate
  allowances, `gateway/auth.rs`); a budget set on a team or project is a later note.
- **The same rules apply to snapshot keys as to file keys**, by the same functions: a
  caller digest must not equal an upstream secret's digest; `allowed_cidrs` need
  `server.trusted_proxies` where a file key would; a `jwt` binding needs the matching
  `[[auth.jwt]]` issuer; a snapshot that names a table the file lacks is refused whole,
  by path. Otherwise the snapshot would be a way around `--check`.
- **`expires_at` is enforced by the data plane at request time, against its own clock**
  (the key store learns an expiry in slice 2; the file's `[[keys]]` gains the same optional
  field). Expiry therefore does not depend on a snapshot being regenerated, and a skewed
  clock moves expiry by the skew, which the note states rather than hides.
- **Revoked keys are in the snapshot** (state `revoked`) rather than absent, so the log can
  say why a credential is refused and the document can be diffed.

### Order, rollback and keys

- **`generation` decides order, not time.** A data plane never applies a snapshot whose
  generation is not greater than the one in force, so a captured old snapshot cannot
  un-revoke a key. `issued_at` is informational.
- **A floor survives restart.** The applied generation is written to a file under
  `[state] policy_dir` (default beside the receipts directory) and read at start; if the
  directory cannot be written the replica does not start (exit 1, naming the setting),
  because a replica that cannot remember its floor can be rolled back. A deployment with
  a read-only or ephemeral volume sets `policy_dir` to a writable one.
- **First boot has no floor.** A fresh replica (or one whose file is lost) would accept any
  validly signed snapshot, including an old one. The limit is stated, and bounded by
  `[control_plane] min_generation` (default 0), which an operator raises as a floor in the
  file that bootstraps the replica. The status reports the generation in force and the
  signing key id, so skew across a fleet is visible, not silent.
- **Generation is global across signing keys.** A new signing key continues the sequence.
  `verify_keys` is a list so a key rotates by adding the new public key first. A
  compromised key is removed from the list; a snapshot it signed that is already applied
  stays in force until a higher generation signed by a trusted key replaces it, and the
  status says `signer_untrusted: true` (an alert) while that is the case.
- **Key encodings.** `verify_keys` entries are `ed25519:<base64 of the 32-byte public
  key>`; a signing key is a file holding the base64 of its 32-byte seed. Verification is in
  the data plane (`ring`, as for JWT, 0017). Signing is needed in the root crate only for
  the `snapshot sign` subcommand (slice 2), and in the control crate (slice 3), which takes
  its own `ring` dependency.

## How the data plane gets it

`[control_plane]` is a new table, **mutually exclusive with `[[keys]]`** (a file with both
is refused: two sources of truth for who may call is a defect, not a feature; switching
between them needs a restart).

```toml
[control_plane]
verify_keys = ["ed25519:…"]                         # reloadable with the snapshot
snapshot_file = "/var/lib/llmshim/snapshot.json"    # slice 2: a watched file
min_generation = 0
max_age_secs = 86400            # measured from when it was applied; alert, never refuse
refuse_when_stale = false
# source = { url = "https://cp.internal/v1/snapshot", token = { file = "…" } }  # slice 5
```

- **Slice 2 is a file**: the control plane (or an operator, or a pipeline) writes the
  signed snapshot to the path and the data plane watches it. No network and no database,
  so the data-plane half ships and is testable before a control plane exists. The reload
  is a second source in the 0012 machinery (two-poll debounce, `SIGHUP`, all-or-nothing, the
  status on `/v1/serve/config`); `verify_keys` joins what a reload may change, and the rest
  of `[control_plane]` stays fixed at start, refused by path.
- **Slice 5 is a pull** (`GET /v1/snapshot`, `If-None-Match: <generation>`, polled, default
  15 s), verified and applied through the same code. It is a pull so that a control plane
  that is down costs only freshness.
- **Stale is served, and said: a named exception to 0013.** 0013 says identity and money
  fail closed on the inference path; a caller is still identified from the snapshot in
  force, and "a change that cannot be applied leaves the running one in force and says so"
  (0012) is the same rule, but indefinite serving of a snapshot that may omit a revocation
  is an identity decision and is recorded as such. `max_age_secs` turns the status field
  `stale: true` and a metric on. `refuse_when_stale = true` (off by default) refuses new
  inference requests with 503 once the snapshot is older than that; `/health`, `/ready`
  and the admin listener keep answering.
- **Revocation latency** is the poll interval or the file's write, per replica. A fleet has
  one file per replica (a shared volume, or whatever writes each copy) and a pull converges
  fleet-wide only eventually: the generation in each replica's status shows where it is.
- **The control plane is a dependency with failure rows.** Slice 2 adds the rows to 0014
  and a process test for each (a snapshot that does not verify keeps the previous one; a
  stale snapshot is reported and, with `refuse_when_stale`, refused).

## The control plane's own API

A separate binary and crate (`llmshim-control`, working rule 2: a published crate cannot
depend on an unpublished one), added only when slice 3 starts. HTTP+JSON, an OpenAPI
document, bearer-token callers (identity for operators beyond a token is SSO: not started).
Operations: create/list/get org, team, project; create key (returns the secret once);
revoke key; rotate key (creates the successor); set limits, budget and expiry; read the
change history; `POST /v1/snapshots` (build, sign and store the next generation) and
`GET /v1/snapshot` (slice 5). The signing key is a file or a secret-manager reference,
never in the database.

## What must stay true

- A data plane with no control plane behaves exactly as today. Default, proxy, gateway
  and gateway-redis builds do not change; the snapshot source is `serve` only.
- No secret in a snapshot, the change history, a log line or a status; a refused snapshot
  reports a reason and a path, never its content.
- A snapshot that does not verify, or does not advance, never changes what is serving.
- The control plane's database holds hashes of caller keys, never the keys.

## Not decided here

- **Bounded admission during a Redis outage** (a local spend allowance), which the
  roadmap pairs with this. It is **not** designed here: a fleet request cannot be admitted
  without the Redis queue (0014), so it is a second admission path with its own rate and
  concurrency rules, not only a spend question; the bound it can offer is on admitted
  reservations, not spend (unpriced and underestimated reservations settle later, 0010);
  a replica that dies strands its lease; leases cross window boundaries; and a usage-log
  field for lease-admitted requests is a 0010 contract change. It needs its own note,
  after the snapshot work, with 0006 and 0014.
- Hierarchical budgets, model allowlists, tags, alerts and cost exports (the next roadmap
  item; they attach to the project and team of this note and to the snapshot as new
  versioned fields).
- Deployments and load balancing across credentials and regions.
- Licensing of SSO, SCIM, audit export and the admin UI (still the owner's).
- Postgres schema and migration tooling (decided with slice 3 against a running schema).

## Slice 2a as built

The data-plane half that needs no control plane exists: `src/serve/snapshot.rs`, the
`[control_plane]` table (`verify_keys`, `snapshot_file`, `min_generation`), `state.policy_dir`
(required with it), and `llmshim snapshot keygen|sign|verify`.

- **The organization is bound.** `[control_plane] org` is required; a snapshot whose `org` differs,
  or a floor recorded for another organization, is refused even if a trusted key signed it. `sign`
  also refuses the content the data plane would refuse about a key's state and expiry.
- A snapshot whose generation is huge (a signer's mistake) raises the floor to it and so refuses every
  later one until the record is removed by an operator: the signer, not the data plane, must not do
  that. A floor write that fails (a full or read-only directory) rejects the reload, so a revocation
  waits on a writable `policy_dir`: stated, not hidden.
- The snapshot's active keys become the `GatewayKey` list `[[keys]]` would have been, so the key
  store, allowlists, budgets, logs and the existing reload all work unchanged: a reload reads the
  file again, and a higher generation applies by the same all-or-nothing, debounced path as a
  keys edit. The snapshot's keys go through the file's own key rules (`validate_keys`,
  `validate_tenant_limits`, `check_same_values`), reported at
  `control_plane.snapshot_file (keys[N].…)`; a snapshot with no active key is refused.
- The envelope is as specified: `format`, `key_id` (the `ed25519:<base64 public key>` itself),
  `payload` (base64 of the canonical bytes) and `signature`. `sign` re-encodes the payload
  canonically (sorted keys, no whitespace; the writer is hand-made so it does not depend on a
  `serde_json` feature) and refuses a payload the data plane would refuse.
- **The floor**: `applied-snapshot.json` in `state.policy_dir` holds the organization, the applied
  generation and the SHA-256 of its payload. It is written once the snapshot has verified and passed
  every check, **just before** it replaces the keys (at start, after the application is built and
  before serving; on every reload that applies it or finds it unchanged), so a crash leaves a floor
  equal to the file, which reloads as the same snapshot. It is written through a temporary name of
  its own with the file and directory synced, and never lowers what is recorded (two replicas on one
  directory cannot move it back); `--check` never writes it. A running replica also keeps the
  generation it applied in memory and never reloads behind it, so lowering or deleting the file
  cannot roll a live replica back (a restart is judged by the file: the stated first-boot limit,
  bounded by `min_generation`). A snapshot below the
  floor or `min_generation` is refused; the same generation with different content is refused; the
  same snapshot again is fine. An unreadable floor file is an error, never "no floor".
- `serve --check --structure-only` reads no snapshot; `--check` reads it and the floor and writes
  nothing.
- **Slice 2b-1 (done):** `verify_keys` reloads with the snapshot (the rest of `[control_plane]` stays
  fixed, refused by path); `/v1/serve/config` gains `snapshot` (org, generation, signer,
  `applied_at_epoch_secs`, `max_age_secs`, `stale`, `signer_untrusted`); `max_age_secs` (age counted from
  when a **new generation** was applied, so a distributor that wants silence to count as fresh
  re-issues) and `refuse_when_stale` (503 `Retry-After: 30` for inference only, never for `/health`,
  `/ready` or the operational endpoints; the native wires answer in their own error shape). A signer
  removed from `verify_keys` while its snapshot is in force is reported (`signer_untrusted`) and keeps
  serving until a trusted signer's snapshot replaces it; the reload that would apply anything from it
  is refused because the file no longer verifies.
- **The age survives a restart.** The floor records when the generation was first applied
  (`applied_at_epoch_secs`); a restart into the same snapshot counts from then, so a crash loop or an
  old file does not make it fresh again (a floor from slice 2a has no time and counts from the
  start). Age is wall-clock (this process's), so a forward step of the clock makes a snapshot stale
  early and a backward step fresh. `GET /v1/models` is not gated (discovery is not inference).
  The stale gate sits before authentication, so an unauthenticated caller also gets the 503; it
  names no key and no tenant.
- **Slice 2b-2 (done):** `expires_at` is a field of `[[keys]]` too (RFC 3339) and is **enforced at request
  time** against this process's clock, with the allowlists: a key at or past its expiry is refused with 401
  (`key_expired` in the gateway shape) wherever it is used from, with no reload; a key already expired
  when a snapshot is read is not loaded; a reload that changes only an expiry is a change (it is in the
  fingerprint). The earlier text of this bullet on the 2a limit no longer applies.
- Expiry uses the wall clock: a backward step of it revives an expired key, stated like the stale age. A request already past the check (queued, or streaming) is not cut off at its key's expiry.
- **The snapshot's `tenants` are not used for the logs**: a key's `tenant` is the team id and the logs
  print it (a name is for the operator's tools; mapping ids to names in a usage record is a log contract
  change, 0010, not made here).
## Slice 3a as built

`crates/llmshim-control` (`publish = false`): the store and its operations as a library, SQLite
(`rusqlite`, bundled), schema version 1 (`PRAGMA user_version`). It has **its own workspace and
lockfile** (the root workspace excludes it), so the database and its C library never enter the root
crate's graph or lockfile: default, proxy, gateway and `serve` builds do not change, and
`control-check.yml` fails if `rusqlite` ever appears in the root `Cargo.lock`. It depends on the root
crate with `features = ["serve"]` for the snapshot signer and loader, so the two cannot drift.

- Orgs, teams, projects, keys (secret, client certificate, workload token), each with a stable opaque id.
  A secret is 32 random bytes (`ring`), URL-safe, returned once; only its SHA-256 is stored (a test greps
  every byte of the database file and its log for it). Key names are unique within an organization.
- Lifecycle: `revoke_key` is terminal; `rotate_key` creates a successor with the same project, limits
  and restrictions under a new name and leaves the old key active until it is revoked (only secret keys
  rotate: a certificate or token identity is the workload's own); `update_key_limits` changes limits,
  budget and expiry. Credentials never change in place.
- The change history appends one row per mutation (actor, action, subject, detail; never a secret).
- `build_payload` / `publish_snapshot` produce the next generation (a revoked key is listed `revoked`, an
  active key past its expiry `expired`), sign it with the seed the caller read from a file (never stored
  here), and keep the envelope. A test loads a published snapshot with the data plane's own
  `snapshot::load`, so the format and the rules are the one `serve` applies. Keys and teams are listed in
  insertion order, so a snapshot's bytes do not depend on random ids.
- **Writes are one immediate transaction each** (`BEGIN IMMEDIATE`): a key's state is checked where it is
  written (`AND state = 'active'`, the changed-row count tested), so two processes on one database cannot both
  revoke or change a key; a rotation is one unit (no successor without its record); `publish_snapshot` reads
  the keys, takes the generation and stores the envelope in one write transaction, so a revocation made before
  it returns is in the snapshot and two publishers never share a generation. A stored value out of its range or a
  corrupt network list is an error, never a wrapped number or "no restriction". `CreatedKey`'s `Debug` hides the
  secret; a key's serialized form carries no hash.
- **The data plane's context-free key rules apply here**, so a snapshot never carries a key `serve` would refuse:
  networks that parse, a client identity (`valid_client_san`), a budget in range with its window and flag consistent,
  an RFC 3339 expiry, the name rule. What depends on the data plane's file (an issuer table for a token binding,
  `trusted_proxies`) is judged there, which refuses the whole snapshot by path with the previous one serving.
- The control crate's lockfile pins the root crate's version: a root or catalog version bump needs
  `cargo update -p llmshim -p llmshim-catalog` in `crates/llmshim-control` (noted in CLAUDE.md).
- Slice 3b added the HTTP API (0026), `openapi.yaml` (compared with the router's route table in both directions by a test) and the `llmshim-control serve` binary. Postgres is slice 4.

## Slice 5 as built (the data plane pulls)

`[control_plane.source]` (`url`, `token`, `poll_interval_secs`, `allow_insecure_http`) replaces
`snapshot_file` (exactly one of the two). The replica polls `GET {url}/v1/orgs/{org}/snapshot` (0026)
with the bearer token and `If-None-Match: "<generation>"`.

- **Nothing is trusted until it verifies.** The body is verified against `verify_keys` and checked for the
  organization (`snapshot::verify_for_cache`) and only a **newer** generation is written, atomically, to
  `state.policy_dir/snapshot.json`; then the reload a signal would run applies it at once, through the same
  all-or-nothing path as a file (the floor, the key rules, the in-memory generation). A body that does not
  verify, a lower generation, a 401, a 404, a 5xx, an unreachable host, a body over 8 MiB: the cache and what
  serves are unchanged, and the status says why in fixed text (`snapshot.last_pull_error`; never the token
  or the body). `snapshot.last_pull_ok_at_epoch_secs` is the last useful answer (a snapshot or "not modified").
- **The token** is read from its reference at each poll (a rotated file applies with no restart), is sent
  only to `url`, redirects are not followed (they would carry it elsewhere), and the url is just a
  scheme and host: no credentials, path or query. Plain `http://` needs `allow_insecure_http`.
- **Start.** One pull before the keys are read. With a verified snapshot already cached a control plane that
  is down is not a reason to refuse to start (it logs and serves the cache); with none it is
  (exit 1: no caller could be identified). `--check` and `--structure-only` fetch nothing.
- The age (`max_age_secs`, `stale`) counts from when a **new generation** applied, as for a file: a control
  plane that is up but re-issues nothing is stale by the rule stated in 2b, and one that is down is stale the
  same way; the replica keeps serving the cache either way unless `refuse_when_stale`.
- The control plane's `ETag` is the generation; the replica uses its own verified cache generation, not the header.
- **Judged exactly as a load would judge it, before it can replace the cache.** A pulled body is written
  to a temporary file (`create_new`, synced, never following a link) and checked with the **same** function a
  load uses (`check_snapshot_candidate`: signature, organization, floor, `min_generation`, content, the key
  rules) against the configuration **as it is now**; only then is it renamed over `snapshot.json` (and the
  directory synced). A snapshot that is signed but that this configuration would refuse therefore never
  replaces the last good one, so a restart cannot find a cache it cannot load.
- **The configuration is re-read at every poll**, so `verify_keys` added or removed by a reload are the list
  the poller judges by; the poller and the loader cannot disagree about who may sign.
- **The body is bounded as it arrives** (8 MiB, no `Content-Length` needed) and the client asks for no
  compression, so a small compressed body cannot inflate past the cap. Redirects are not followed. The
  standard proxy variables apply to the client (a plain `http://` url with `HTTP_PROXY` sends the token to the
  proxy: another reason `allow_insecure_http` is a choice).
- **The organization is a path segment**: with a source it may hold only letters, digits, `.`, `_` and `-`,
  starting with a letter or digit, and the url must be a scheme, a host and an optional port (parsed, no
  backslash, no port 0).
- A reload that refuses a snapshot just cached (another change in the same file) is logged and shown in
  `last_pull_error`, not swallowed.

## Slices

1. This note.
2. (a, b-1 and b-2 done; see above) The snapshot envelope and canonical payload, Ed25519 verification and the `snapshot
   sign`/`verify` subcommands, `[control_plane]` with `snapshot_file` as a keys source in
   the 0012 machinery, `expires_at` in the key store (and `[[keys]]`), the persisted
   floor, the 0014 rows, and the regenerated JSON Schema (`tests/unit_serve_cli.rs`
   compares it). Tests: signature, tamper, wrong key, unknown format/version,
   non-advancing generation, the floor across restart, `min_generation`, mutual exclusion
   with `[[keys]]`, the `--check` rules on snapshot keys, stale status and refusal,
   revocation and expiry effect on the next request, a removed signer.
3. The `llmshim-control` crate: **3a done** (SQLite store, the model above, key lifecycle, snapshot
   publication as a library); 3b done (the HTTP API of 0026, its OpenAPI document and the `llmshim-control` binary).
4. Postgres.
5. **Done:** the pull delivery and the data plane's poller (above).
