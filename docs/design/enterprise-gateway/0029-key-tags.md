# 0029: Key tags on usage records

Status: design, 2026-11-17. A contract note, written before any code (README working rule 3): a
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
  starting with a letter. Values are 1 to 64 printable ASCII characters (space to `~`), so a value
  is safe in a log line and a spreadsheet. At most 16 tags per key. Anything else is refused by
  path (`keys[2].tags.Env`, `keys[2].tags.env`). An empty table is accepted and means none.
- **On the usage record** a new field `tags` is an object with the key's tags, names sorted, or
  `null` when this replica's configuration does not name the key (the same condition under which
  `key` is `null`: 0010). A key with no tags is `{}`, so a consumer can tell "no tags" from "unknown".
  The field is additive; a consumer that ignores unknown fields is unaffected.
- **They never enter shared storage.** The job's policy scope that crosses Redis carries only the
  credential's hash prefix, as for the key's name (0010); each replica resolves tags from its own
  configuration. A test pins that no tag text reaches Redis. A fleet shares one configuration, so
  replicas agree; during a rolling change a replica may attribute with its older or newer tags,
  and a replica that does not know the key writes `null`.
- **Reload.** Tags follow the key's reload (0012) through the same grow-only table as the key's name:
  a request admitted after the reload carries the new tags; an attempt already admitted reports
  the tags its recorder read when it ended, which is after the reload. A key that is removed keeps
  resolving to its last tags until restart (a label, as its name does).
- **Not carried by a snapshot yet.** The control plane's key record (0024) has no tags. Carrying
  them is a signed-format change with the same shape as 0028's slice 3 and is a later slice; a key
  from a snapshot has `tags: {}` until then.

## Cost exports

The usage log is the export. 0010 gains a section with the three recipes that need no new feature:
spend per tag value (`jq -s 'group_by(.tags.cost_center) | map({k: .[0].tags.cost_center, usd:
(map(.cost_usd // 0) | add), unknown: (map(select(.cost_usd == null)) | length)})'`), the same
per tenant and tag, and a note that `cost_usd: null` is unknown and not free, so an export reports
the unknown count beside the sum. A scheduled export to object storage or a warehouse is a
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
- Mode off: records of a build with no tags anywhere differ from today's only by `"tags":{}`.

## Slices

1. This note.
2. The field, validation, `KeyNames`, the usage field, the 0010 text and the tests.
3. Tags in the control plane and its signed snapshot (with the 0028 slice 3 shape).

## Not decided here

- Tenant-level tags (inherited by every key of a tenant) and tag-based budgets: they belong with
  hierarchical budgets, where the levels are decided once.
- Free-form (non-string) values and per-request tags supplied by the caller: a caller-supplied tag
  would be unauthenticated data in a billing record, so it needs its own trust decision.
