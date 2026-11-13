# 0003: Caller keys under `serve`

Status: key store implemented (`llmshim::serve::build_key_store`); not yet used
to serve, which is the next roadmap item.

## Problem

`llmshim gateway` loads caller keys from a JSON file named by an environment
variable, keeps the plaintext tokens in a map, and exits the process on a bad
file. For `serve` that has three problems: plaintext tokens stay in memory for
the life of the process, a deployment must hold caller secrets at all, and a
startup error cannot be reported with the other configuration problems.

## Decision

`KeyStore` gains a `Hashed` variant (`KeyStore::from_digests`) that holds
identities keyed by the lower-case hex SHA-256 of the bearer token. A presented
token is digested and looked up; the digest of a token is never itself accepted
as a token.

`serve::build_key_store(config, env)` builds it from the validated
configuration:

- A key given as `secret = { env | file }` is read once, digested and dropped.
- A key given as `sha256 = "..."` is used as is, so a deployment can hold no
  caller secret at all.
- With `server.allow_unauthenticated = true` the store is `KeyStore::Open`,
  exactly the gateway's existing open mode.
- Problems are returned with the path of the setting. Nothing exits the process
  and no message contains a token.

`llmshim serve --check` runs this builder, so token problems surface before
rollout.

## Credential scope is unchanged

The gateway scopes idempotency entries and replay receipts to a credential by
the SHA-256 of the token. `Hashed` computes the same digest, so a token gives the
same scope whether it is stored in plaintext or hashed. The test
`a_hashed_store_authenticates_and_scopes_like_a_plaintext_store` pins this, so
moving a key to hashed storage does not orphan state it owns.

## Token rules

A configured secret must be at least 16 characters and consist only of visible
ASCII characters (`!` to `~`). The gateway reads the `Authorization` header as
visible ASCII, so a space, a control character, a non-ASCII character or a
byte-order mark left by an editor could never match; refusing it at `--check`
turns a silent lockout into an error naming the key. The 16-character floor is
a guard against a typo, not a strength guarantee: generate tokens with at least
128 bits of randomness. A digest cannot be checked for strength; that is the
operator's responsibility.

The store holds only digests. A secret given as `{ env = ... }` is read once
and not kept, but it remains in the process environment (readable by the same
user and inherited by child processes); `{ file = ... }` and `sha256` keys do
not have that property.

Two keys that resolve to the same token are refused, as is a caller token equal
to a provider key (checked in `--check` against resolved values).

## Limits of this design

- Lookup is a hash-map lookup on a digest. An attacker controls the token but not
  how its digest compares to stored digests, so the lookup leaks nothing useful;
  it is not a constant-time comparison and does not need to be.
- A plain SHA-256 of a high-entropy token is not a password hash. It would be
  unsuitable for human-chosen passwords; these are machine tokens.
- Adding a variant to the public `KeyStore` enum is a breaking change for code
  that matches it exhaustively. The gateway is experimental and outside the
  semver rule; this is noted for the release that ships it.
- Rotation, expiry and revocation need a control plane and are later roadmap
  items. Today a key changes by editing the file and restarting.
