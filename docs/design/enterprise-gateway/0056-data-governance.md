# 0056: Deleting a customer's request data, and keeping tenants apart

Status: built (first step). Follows the advisor's review of 0052.

## Problem

The request ledger, the prompt structures and the payload references are customer data. The only way they left was age
(the retention). There was no way to honour a deletion request for one customer or one key. And the cache debugger
paired a request with "the same client's request before it" by key *name* alone, so two tenants whose gateways name a
key the same way (`checkout` is not rare) were paired with each other: one customer's prompt fingerprints set beside
another's, and the content diff able to put one customer's text beside another's.

## What is built

- **Erasure.** `POST /v1/requests/erase` (admin only): `{tenant?, key?, confirm}`. At least one selector; both means
  that key within that tenant. `confirm` repeats what is erased (`tenant`, `key` or `tenant/key`), so a deletion is not one stray
  request away (400 `confirmation_mismatch`). A key alone means that key in every tenant, and has to say so with
  `all_tenants: true` (400 `tenant_required`): `checkout` is not rare. It deletes the ledger rows and the stored structures found through them,
  in one transaction with one `requests.erased` row in the hash-chained history: who, how they were identified
  (`via`), the selector and the counts, never what a row said. The answer is `{requests, structures,
  payload_references}`.
- **It stays erased.** One row in `erasures` (when, tenant, key). A record that arrives afterwards from a gateway (an
  export retry queue, an acknowledgement lost on the way), matches a selector and is no newer than the erasure is
  refused and counted as `erased` on the batch answer, instead of re-inserting what was deleted. Newer activity is
  stored. Tombstones are deleted at the ledger's retention.
- **Payloads.** A row's reference to a sealed payload goes with the row, so nothing can open that object any more
  (the control plane could not open it anyway, and no one else holds the reference). The sealed object on a
  gateway's store is ciphertext and is deleted by that gateway's sweeper at its retention; `payload_references` says how
  many such objects are left to age out. Telling the gateways to delete them now is not built.
- **Tenant-scoped pairing** (schema 9). `request_structure` has a `tenant` column (additive, nullable, migrated by
  `llmshim-control migrate` like any other step). The predecessor lookup and the miss aggregate both include it, and
  the miss groups name it. The migration reads each existing structure's tenant back from its ledger row, so rows from before version 9 are
  separated too (a record that came with no tenant still pairs with other tenant-less ones).
- **The vendored fonts are pinned by digest** in a test, so replacing a binary served to every browser is a reviewed
  change.

## Limits worth knowing

- Rows with neither a tenant nor a key match no selector; only the retention removes them.
- The history keeps the selector (tenant and key names) for ever, as it keeps every other action: if a name identifies a
  person, so does the record that it was erased. Counts, not content, are all it holds.

## Not built

- Erasing at the gateways' stores on request; a payload-reader role and tenant-scoped admins (an owner decision).
- Erasure through the UI (the API is the contract; a button is a small step on top).
- Deleting a single request by id, or by time range; the retention covers age.
- Erasure from backups: a `.pre-vN.bak` copy (0054) and any `pg_dump` hold what they held. Retire old copies on the
  schedule the deletion promise needs.
