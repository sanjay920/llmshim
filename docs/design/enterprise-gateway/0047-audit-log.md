# 0047: The audit log

Status: built, both slices (the gateway's log; the control plane's chained change history). Written on the owner's instruction to implement it ("implement the audit log to completion").

## What it is, and what it is not

A record of **what changed the running system, who or what changed it, and when**, that cannot be edited,
reordered or thinned out without the change being detectable. A security review asks for it first, and SSO and SCIM
(not started) will want their events recorded in it.

It is **local**: one append-only file per writer, checked by a command (`llmshim audit verify`). It has no export,
shipping, query API, retention or UI: an operator ships the file with their own log agent and keeps what their
policy keeps. That boundary is deliberate, and it is where the owner's "audit export" fence runs: nothing here sends
an audit record anywhere.

It is **not** the request log. The access log and the usage log (one line per request and per provider attempt) stay
what they are; the audit log records the events that change what the system is.

## The record

One JSON line per event, written and flushed to disk (`fsync`) before the call that caused it returns:

```json
{"seq":7,"ts":"2026-11-18T15:04:05.123Z","event":"config.reloaded","actor":"operator","outcome":"applied",
 "instance":"3f9a1c0b27de","detail":{...},"prev":"<hash of record 6>","hash":"<sha256 of this record>"}
```

- `hash` is the SHA-256 of the record's **canonical form** (object keys sorted at every depth, compact) without the
  `hash` field, so a reader that orders keys differently computes the same hash; `prev` is inside what is hashed and
  binds each record to the one before. The first record's `prev` is 64 zeros.
- `verify` checks every record: well formed, its hash, the sequence (1, 2, 3, ...) and the `prev` link. An edited
  field, a removed, inserted, duplicated or reordered record, and a record rewritten with a self-consistent hash are
  each named at a line (a forged record is exposed by the **next** record's `prev`, so it is named at the next line).
- **What the file alone cannot show: records removed from its end.** An intact prefix is an intact chain. So the head
  (record count and last hash) is also on the admin status (`/v1/serve/config`, `audit`) and printed by
  `llmshim audit verify`; **anchor it somewhere the log's writer cannot reach** (a ticket, another system's log, a
  signed message) and compare.
- **Never recorded:** a secret, a token, a credential digest, a prompt, a completion or any request body, the text
  of a refused value. Key names, provider names, configuration section names, hashes and counts are.

## Slice 1: the gateway (`[logging] audit_log`)

```toml
[logging]
audit_log = "/var/log/llmshim/audit.jsonl"   # an absolute file path; never "stdout"; not another log's file
```

- Created owner-only (`0600`) and **continued across restarts**: opening an existing log reads its last record and
  carries the chain on. A symbolic link, a directory, a file that ends in an **incomplete** record (a crash mid-write)
  or whose last record does not match its own hash is **refused at start** (`logging.audit_log`): a server that
  cannot be audited must not run unaudited. Keep the file, run `llmshim audit verify`, and start a new log.
  The file is opened without following a link, and **one writer per file** is enforced with an exclusive advisory
  lock held for the process's life: a second gateway on the same path is refused at start. The mode `0600` applies
  to a file the gateway creates; a file that already exists keeps the permissions the operator gave it.
- **One spelling.** A line verifies only if it is byte-for-byte what the writer wrote: whitespace, a carriage return,
  a reordered key or a repeated key (which a JSON parser resolves to its last value) all fail at that line, because
  the hash is over the parsed record.
- **Events:**
  - `server.started` (actor `process`): version, the configuration file's SHA-256, the provider names, the credential
    count, whether callers authenticate, and the snapshot in force (org, generation, signer) with `[control_plane]`.
    If it cannot be written the server does not serve.
  - `server.stopping` (actor `process`): on SIGTERM / Ctrl-C, before the listeners drain. The signal handlers are
    registered before `server.started` is written, so a signal in that window is recorded, not fatal. If a listener
    ends without a signal, a `server.stopping` with `cause: "a listener ended"` is written instead.
  - `config.reloaded` with `outcome: applied` (actor `operator` for a signal, `poller` for the file watcher, `control-plane` for a pulled snapshot): the new
    generation, the credential count, the file's SHA-256, `changes` (the provider, route and limit changes a reload
    lists; empty for a keys-only reload), `keys_added` and `keys_removed` **by name** (at most 100 each, with `keys_added_total` / `keys_removed_total`), and the snapshot (org,
    generation, signer). With `outcome: rejected` (the first time a given refusal happens; a repeated one is not
    re-recorded): the **paths** of the settings at fault, never their values or messages. Checks that find nothing
    new, and a change waiting to be seen twice, change nothing and are not recorded.
- **A failed write** (disk full, permission) is counted (`llmshim_audit_write_failures_total`), reported on stderr, and
  **closes the log**: it takes no more records, because the file may end in half a record and appending after it would
  corrupt the chain; `/v1/serve/config` shows it as `audit.write_failed`. A reload that was already applied stays applied; the gap is visible as the failure counter and as
  the next restart's refusal to continue a damaged log. (A write-ahead design would refuse the reload instead; the
  reload's outcome is not known before it happens, so the record is written after it.)
- `llmshim audit verify <FILE>`: exit 0 and `intact, N record(s), head <hash>`; exit 1 naming the first broken line and
  why; exit 2 for bad arguments. It reads the file only.
- Rotation is not built. To rotate, stop the gateway, move the file aside (keep it) and start: the new file begins a
  new chain at genesis. The old file's head is the anchor that closes it.
- **Fixed at start**, like the other `[logging]` settings: a reload cannot change it.

## Slice 2: the control plane (`llmshim-control audit-verify`)

`crates/llmshim-control` appends one row to its change history for every mutation (who, what, when, ids, never a
secret). Each row now carries `prev` and `hash` (schema version 5; both nullable, added by an in-place migration on
SQLite and Postgres). `hash` is SHA-256 over the JSON array `[prev, at, actor, action, subject, detail]` (an array,
so no key order or repeated key can spell one row two ways; `detail` as the exact stored text); `prev` is the
previous row's hash, `0`×64 for the first.

- The chain is extended inside the transaction that makes the change. That transaction is already the only writer
  (SQLite `BEGIN IMMEDIATE`; Postgres `pg_advisory_xact_lock`), so reading the last hash and inserting the next row
  cannot interleave: no separate head lock was needed.
- Rows from before the migration have no hash. They are the **unchained prefix**: counted, never vouched for. An
  unhashed row after hashed ones is a removed hash and breaks the chain.
- `llmshim-control audit-verify (--db FILE | --database-url-file FILE …)`: exit 0 and
  `history: intact, N chained row(s), M before the chain, head <hash>`; exit 1 naming the first broken row id; exit 2
  for bad arguments (it takes only the database options); it never creates a SQLite file (missing or empty is refused). On Postgres the target is opened like any other, so a mistyped `--database-schema` is initialized empty and reports `0 chained row(s)`: check the count against what you expect. Opening a store runs the
  schema migration like any open does.
- **Upgrade:** stop every old control-plane writer before the first new one starts. A version-4 process still running after the migration inserts rows without a hash, which the verifier reports as `a row without a hash after chained rows`.
- As for the gateway, a removed **last** row cannot be seen from the database: the printed `head` is the anchor to
  keep elsewhere. Anyone who can rewrite the whole table and recompute the chain can also forge it; the anchor is
  what they cannot reach.
- No API endpoint and no change to `GET /v1/history` or the OpenAPI document: slice 2 adds the chain and the
  command only (no export, shipping, query or UI).

## Not built

- Export, shipping, query, retention, rotation and any UI (fenced).
- Events for request-level activity (the access and usage logs), for admin reads, and for a key modified in place by a
  reload (its limits changing is visible as the credential count and the file hash, not named).
- Signing the head (an anchor the writer cannot reach is the operator's).

## Tests

`serve::audit::tests` (unit): a chain verifies and resumes after a restart with the same head; every kind of
tampering (an edited field, a removed, reordered, duplicated or head-removed record, a forged self-consistent record,
junk and a half record) is named at its line; editing any single field of a record, or adding one, is detected;
truncating the end still verifies, which is why the head is anchored; a partial last record, an altered last
record and a symlink are refused at open; eight threads writing 200 records keep one unbroken chain and a failed
write closes the log; the hash does not depend on key order. `integration_serve_audit` (process): a real server's
`server.started`, a reload that adds a key (its name, not its digest), a refused reload (its path, not its value),
`server.stopping`, no secret anywhere in the file, the admin head equal to the file's last record, `llmshim audit
verify` exit 0 and counting every record, a second run continuing the same chain; an edited record is named by the
verifier and a damaged log stops a start by path; the path rules. Not calling the audit from a reload, ignoring the
`prev` link, or leaving `detail` out of the hash each fail a test.

Control plane (`tests/history_chain.rs`, SQLite files and, with `LLMSHIM_CONTROL_TEST_POSTGRES_URL`, Postgres for the
concurrent writers): every change chains to the one before and a reopened store continues the chain; an edit to any
one field, a removal in the middle and a forged hash are each named at their row; pre-chain rows are counted and an
unhashed row after chained ones is refused; two handles writing at once make one chain of 30; the command exits 0
with the head, 1 with the broken row, never creates a missing database and refuses serving options.
`lifecycle::an_older_database_is_migrated_in_place` covers the version-4 to 5 migration on both backends. Making `prev`
constant or leaving `detail` out of the hash fails these tests.
