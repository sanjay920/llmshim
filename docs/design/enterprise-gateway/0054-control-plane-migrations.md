# 0054: Migrating the control plane's database

Status: built. Follows the advisor's review of 0052.

## Problem

Every open of the control plane's store migrated the schema: `serve`, and also `audit-verify`, which is meant to be a
read-only check. A server pointed at the wrong database, or started by a rolled-back image, would silently alter it.
There was no copy to go back to, and nothing said what a rolling deploy may assume.

## The policy

- **Only `llmshim-control migrate` changes the schema** (and `serve --migrate-on-start`, which runs the same steps for
  development and single-box use). It creates a new database, or brings an older one to this build's version.
- **`serve` and `audit-verify` never migrate and never create.** Against an old, empty or newer schema they exit 1,
  say what they found, say to run `migrate` (or, for a newer one, to run the build that wrote it or restore a
  backup), and leave the file or schema byte-for-byte as it was (a test compares the bytes). Postgres is read with
  `to_regclass`, so not even the metadata table is created by looking.
- **A copy first, for SQLite.** `migrate` takes a consistent snapshot (`VACUUM INTO`) of an older database to
  `<db>.pre-v<N>.bak`, owner-only, before touching it, and refuses to overwrite one that exists. Rolling back is:
  stop the servers, put the copy in place of the database, run the previous build. For Postgres `migrate` runs in one
  transaction under an advisory lock and says no copy was taken: take a `pg_dump` first (a snapshot of a shared
  database is the operator's, not ours to guess).
- **Migrations are additive** (new tables, new nullable or defaulted columns), as every one to date has been. That
  keeps a rolling deploy simple: run `migrate` with the new build, then replace replicas. A replica of the old build
  that is already running keeps working on the additive schema; one that *restarts* is refused (it needs its own
  version exactly) until it is replaced. The window is therefore "replicas already running", not "any old build may
  start". There is no N-1 start-up compatibility yet; it would need a declared minimum reader version per
  migration, and is not built.
- Kubernetes: run `migrate` as a Job (or init container) before the Deployment rolls; the Deployment's `serve` has no
  schema permissions it needs.

## Tests

`tests/migrate.rs`: refusal leaves old and empty databases untouched (bytes compared) for `serve` and `audit-verify`;
`migrate` copies, migrates, keeps the data, restores from the copy, is idempotent, refuses to overwrite a copy and
leaves the database as it was; a new database has nothing to copy; a newer schema is refused; Postgres is read-only
until migrated. Mutation: letting `open_existing` migrate makes the refusal test start a server instead of failing,
which is how it was found to bite.

## Not built

N-1 start-up compatibility; automatic Postgres snapshots; a `migrate --dry-run`.
