# 0029: Key tags on usage records

Status: slice 2 built (2026-11-17; see the end). A contract note, written before any code (README working rule 3): a
`[[keys]]` field and a usage-record field become promises. It is the second slice of the roadmap
item "hierarchical budgets, model allowlists, tags, alerts, cost exports" (after 0028). Nothing here
is started.

## The problem

A usage record names the tenant and the key (0010), so spend can be totalled per tenant and per
key. Finance and platform teams need other cuts: per cost center, per environment, per product.
Today the only way is to encode them in key names and parse the names. A tag is the explicit form:
a few operator-chosen labels on a key, copied onto every usage record that key produces, so the
usage log is directly the cost-export feed (`jq` or a warehouse load groups by tag).

## The rule

```toml
[[keys]]
name = "checkout-prod"
tenant = "payments"
secret = { env = "CHECKOUT_KEY" }
tags = { cost_center = "cc-4417", env = "prod", product = "checkout" }
```

- **Tags are labels, never permissions.** Nothing in the gateway admits, refuses, limits or routes
  on a tag. They are written to the usage log and nowhere else (not the access log, not spans,
  not metrics labels: a metric label per tag value would be unbounded cardinality, and 0016's span
  attributes are a closed set).
- **A tag is a name and a value, both strings.** Names are 1 to 32 characters of `a-z`, `0-9` and `_`,
  starting with a letter. Values are 1 to 64 printable ASCII characters (`!` to `~` and interior spaces: no leading or
  trailing space), so a value is safe in a JSON log line. A value that begins with `=`, `+`, `-` or
  `@` is **accepted** and is the exporter's to neutralize before it opens a file in a spreadsheet
  (formula injection is a property of the consumer, not of the log). At most 16 tags per key. Anything else is refused by
  path (`keys[2].tags.Env`, `keys[2].tags.env`). An empty table is accepted and means none.
- **On the usage record** a new field `tags` is an object with the key's tags, names sorted, or
  `null` exactly when `key` is `null`: the record's credential is not one this replica can name.
  That happens in open mode (`allow_unauthenticated`, no credential), for a fleet job queued by a
  replica that attached none, for a key this replica's configuration does not list, and for a key it
  lists but whose secret it cannot read (such a key cannot authenticate on that replica either, so
  its credential scope is not known there; 0010). A key with no tags is `{}`, so a consumer can tell "no tags" from "unknown".
  The field is additive; a consumer that ignores unknown fields is unaffected.
- **They never enter shared storage.** The job's policy scope that crosses Redis carries only the
  credential's hash prefix, as for the key's name (0010); each replica resolves tags from its own
  configuration. A test pins that no tag text reaches Redis. A fleet shares one configuration, so
  replicas agree; during a rolling change a replica may attribute with its older or newer tags,
  and a replica that does not know the key writes `null`.
- **Reload.** Tags follow the key's reload (0012) through the same grow-only table as the key's name.
  They are resolved when the attempt is **admitted** (with the key's name, at `begin`), so an attempt
  admitted before a reload keeps the tags it was admitted under, and `key` and `tags` on one record
  always agree; a request admitted after the reload carries the new tags. A key that is removed, or
  whose secret rotates, keeps its old credential resolving to its last tags until restart (the table
  is grow-only, as it is for names; a label, never a permission).
- **Not carried by a snapshot yet.** The control plane's key record (0024) has no tags. Carrying
  them is a signed-format change with the same shape as 0028's slice 3 and is a later slice; a key
  from a snapshot has `tags: {}` until then.

## Cost exports

The usage log is the export. 0010 gains a section with the three recipes that need no new feature:
spend per tag value (`jq -s 'map(select(.event == "usage")) | group_by(.tags.cost_center // "(untagged or unknown)") | map({tag: (.[0].tags.cost_center // "(untagged or unknown)"), usd: (map(.cost_usd // 0) | add), unknown: (map(select(.cost_usd == null)) | length)})'`),
the same per tenant and tag, and two notes: the filter on `event` is needed when the usage and access
logs share stdout, and `cost_usd: null` is unknown and not free, so an export reports the unknown
count beside the sum. `tags: null` (an unnamed credential) and `tags: {}` (no tags) are different
facts that this recipe deliberately merges; a consumer that needs them apart tests `.tags == null`.
A scheduled export to object storage or a warehouse is a
separate item (it needs credentials for the destination, so it belongs with secret-manager
integrations).

## Contract changes (each with its own test when built)

- **`keys[].tags`** (a table of strings), validated by path; the JSON Schema changes and
  `serve-config.schema.json` is regenerated.
- **Usage record field `tags`** (object or null), documented in 0010.
- **`KeyNames`** (the table that names keys from a credential) also holds tags.
- **0014**: no new failure row (tags add no dependency); a row for "a key unknown to the replica
  writes `tags: null`" if it is not already covered by the `key: null` row.

## Tests (when built)

- Process: a key with tags produces usage records with those tags, sorted; a key without produces
  `{}`; a reload that changes tags changes the next record; an unnamed key (a fleet job whose key this
  replica does not know) writes `null`.
- Validation: names, values (non-ASCII, control characters, empty), count; by path.
- A fleet test that no tag value appears in any Redis key or value during a request.
- Mode off: records of a build with no tags anywhere differ from today's only by `"tags":{}` for a
  named key and `"tags":null` where `key` is `null` (open mode included).
- A stream admitted before a reload that changes the tags ends after it and carries the old tags.

## Slices

1. This note.
2. The field, validation, `KeyNames`, the usage field, the 0010 text and the tests.
3. Tags in the control plane and its signed snapshot (with the 0028 slice 3 shape).

## Not decided here

- Tenant-level tags (inherited by every key of a tenant) and tag-based budgets: they belong with
  hierarchical budgets, where the levels are decided once.
- Free-form (non-string) values and per-request tags supplied by the caller: a caller-supplied tag
  would be unauthenticated data in a billing record, so it needs its own trust decision.

## As built (slice 2)

- `keys[].tags` (`BTreeMap`, so names are sorted), validated by path (`keys[0].tags.Env`; more than 16
  is `keys[0].tags`); `KeyNames` holds a second table, `add_tags`, and `tags(credential)` is `None`
  exactly when `name(credential)` is; the usage recorder reads name and tags together at `begin`.
- Reload: `key_names.add` and `add_tags` run before the fingerprint comparison, so a tags-only change
  applies without a new generation (who may call is unchanged); a test waits for it by asking.
- A unit test pins the fields of the scope that crosses Redis, so a key's name or tags cannot start to
  travel without a decision; the open-mode record has `key: null, tags: null`.
- Tests: `integration_serve_tags` (sorted tags on the wire, an untagged key `{}`, reload change and
  removal, open mode `null`, bad tags by path and the 17-tag cap). Not done: the usage-log recipe is
  in this note only, not run; the snapshot slice (3).

## As built (slice 3): the control plane

Same shape as 0028's slice 3. `SnapshotKey.tags` is written only when a key has tags (an older data
plane refuses such a snapshot whole and keeps the previous one); the data plane puts them on the key
it serves, validated by the file's rules. The control plane stores `keys.tags` (JSON text; schema
version 3, migrated in place from version 1 or 2 on SQLite and Postgres under the same locks as
0028), validates with the data plane's own `tag_name_problem` / `tag_value_problem` and the 16-tag
cap, sets them at creation (`tags` in the key body), copies them on rotation, lists them on the key
and records them in the `key.created` history detail. No `PATCH`: a changed label set is a successor
key. Tests: both backends (refusals, storage, rotation, payload presence/absence, a published
snapshot read back by the data plane), migration from versions 1 and 2, and a process test that a
snapshot key's tags are on its usage records.

**Upgrading the control plane.** The schema migrates in place when the new binary first opens the
database. Stop every older `llmshim-control` writer first: an older binary does not read the new
column, so a snapshot it published afterwards would omit the new field and the data plane would serve
the key without it (without tags, or, for 0028, without its model list). A newer schema version is
refused by an older binary and the other way round, so the mixed state is detected at start rather
than discovered later, except for a writer that was already running when the migration happened.
