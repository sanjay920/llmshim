# 0017: Workload JWT authentication and IP allowlists

Status: IP allowlists (slice 2) and the JWT verifier as a unit (slice 3, `src/serve/jwt.rs`,
not yet wired to anything) are implemented; the JWKS client and the wiring are not
(slices 4 and 5).
A contract note, written before any code (README working rule 3). Single
sign-on for people, SCIM and an admin UI are not part of this note and are not
started.

## What an operator needs

Callers today authenticate with a bearer key (0003) or a client certificate
(0011). A platform that already issues workload identity (a Kubernetes service
account token, a cloud provider's workload identity, an internal OIDC issuer)
wants a caller to present **that** token, with no llmshim key to mint, distribute
or rotate. Separately, an operator wants to say that a key or tenant is only
usable from certain networks, so a leaked bearer key is useless outside them.

## The contract

```toml
[[auth.jwt]]                       # one table per trusted issuer
name = "cluster"                   # shown in logs and /v1/serve/config; [A-Za-z0-9._-]
issuer = "https://oidc.example.com"          # exact `iss`
jwks_url = "https://oidc.example.com/keys"   # https; http only for a loopback address literal
ca_file = "/etc/llmshim/jwks-ca.pem"         # optional: a private CA for the JWKS fetch
headers = { authorization = { env = "JWKS_TOKEN" } }   # optional, secret references (as 0016)
audience = "llmshim"                         # `aud` must contain it
algorithms = ["RS256", "ES256"]              # required, non-empty; see Rules
clock_skew_secs = 30                         # default 30, at most 300
jwks_refresh_secs = 300                      # default 300, at least 30
jwks_max_stale_secs = 3600                   # default 3600; 0 = never verify on stale keys

[[keys]]
name = "checkout"
tenant = "payments"
jwt = { table = "cluster", subject = "system:serviceaccount:payments:checkout" }
allowed_cidrs = ["10.0.0.0/8", "2001:db8::/32"]   # optional, on any key
tier = 2
```

A `[[keys]]` entry sets exactly one of `secret`, `sha256`, `client_san` and `jwt`
(extending the rule 0011 set). `jwt.table` is the `name` of an `[[auth.jwt]]` table
(not its `issuer` URL); `jwt.subject` is the exact `sub`. Table names are unique,
`issuer` URLs are unique across tables, and two keys may not bind the same
`(table, subject)`: each collides at `--check`, as a duplicate `client_san` does
(0011). Without any `[[auth.jwt]]` table nothing below applies and every bearer
value is looked up as a key, exactly as today. Every other setting of the key (tenant, tier,
limits, budget) applies unchanged: the token only proves **which key**.

## Rules

- **Choosing the table.** The verifier reads the token's *unverified* `iss` for one
  purpose only: to pick the table whose `issuer` equals it exactly. No match is a
  401 without any network activity, so one issuer's outage cannot turn unrelated
  tokens into 503s. Nothing else from an unverified token is trusted.
- **Algorithms are configuration, never the token.** The token's `alg` header is
  accepted only if the issuer's `algorithms` lists it. `none`, every `HS*` and any
  algorithm the build cannot verify are refused at `--check`, not at request time.
  The supported set is `RS256`, `ES256` and `EdDSA`, verified with `ring`. A
  symmetric algorithm is refused outright: the issuer's JWKS is public, and an
  `HS256` token "signed" with a public RSA key is the classic confusion attack.
- **The key is chosen by `kid`, from the issuer's JWKS, and must fit the
  algorithm.** A token without a `kid` is accepted only when the JWKS has exactly
  one usable key. A JWK's `kty`/`crv` must match the token's `alg`; a mismatch is
  a failure, not a try-the-next-key. `ring` has no JWK parser, so the verifier
  builds keys itself, and refuses what it cannot vouch for: RSA only 2048 to 8192
  bits; EC only P-256 (`ES256`, as an uncompressed point with the fixed-length
  signature); `EdDSA` only Ed25519. A usable key has `use` absent or `sig`,
  `key_ops` absent or containing `verify`, and a JWK `alg`, if present, equal to
  the token's. A token whose header has `crit`, or `b64: false`, is refused (RFC
  7515 requires refusing a `crit` the verifier does not implement). A token with
  an unknown `kid` is a 401 while a refresh is allowed or just happened, and 503
  only if the refresh itself failed.
- **Claims.** `iss` equals the configured issuer exactly; `aud` (a string or an
  array) contains the audience; `exp` is required and must be in the future and
  `nbf`, when present, in the past, each within the skew; `sub` is required and is
  the whole identity. A token without `exp` is refused: a non-expiring bearer is a
  key with extra steps. **`exp` more than 24 hours from now is refused**, whether
  or not `iat` is present (the bound is on remaining life, so omitting `iat`
  buys nothing); an `iat` more than the skew in the future is refused.
- **A bearer key decides alone, as in 0011.** With at least one `[[auth.jwt]]`
  table, a request whose `Authorization: Bearer` value is not a JWT shape (three dot-separated base64url parts whose first
  part decodes to a JSON object with `alg`) is looked up as a key, exactly as
  today. A JWT-shaped value is verified as a JWT and **never** falls back to key
  lookup: a failed token cannot be rescued by happening to equal a key's secret.
  To keep an existing configuration from locking itself out, `--check` refuses a
  `secret` key whose value is JWT-shaped when any table exists (a `sha256` key is
  opaque and cannot be checked; the note in the key's docs says so).
- **Unknown workload fails closed.** A valid token whose issuer and subject no
  key lists gets 401, and the response says nothing about which part failed (the
  access log records a reason class: `jwt_invalid`, `jwt_unknown_subject`,
  `jwks_unavailable`).
- **Credential scope.** Idempotency entries, replay receipts, the usage log and the
  fleet job scope are keyed by a digest of the credential (0003, 0010). A JWT is
  re-minted about hourly, so a token digest would orphan that state on every
  renewal and let a replayed `Idempotency-Key` bill twice. A JWT caller's scope is
  the digest of `("jwt", table name, sub)`, domain-separated so it cannot equal a
  token digest or an mTLS scope (0011); only the digest ever reaches Redis, and
  the existing test that names stay out of Redis covers it.
- **One identity per request.** Presenting both a JWT and a certificate-derived
  identity is the same ambiguity 0011 refuses; the token wins as a bearer does,
  and the certificate header is ignored.
- **The JWKS is fetched by us, bounded and cached.** One fetch at a time per
  issuer, a 5-second connect and 10-second total timeout, a 1 MiB body limit, no
  redirects, https only, and the address is the configured one (no discovery
  document, no `jku` header, no `x5u`: a token never chooses where its key comes
  from). The fetch uses its own HTTP client, not the shared provider client, and
  ignores `HTTP_PROXY`/`ALL_PROXY` (the trust path is the configured address and
  `ca_file`, nothing ambient). `http://` is accepted only for a loopback address
  literal, as a validation rule, so a test issuer works and production does not
  fall to plain text. Keys are cached for `jwks_refresh_secs`. An unknown `kid` triggers one
  refresh, at most once per 30 seconds per issuer, so a flood of random `kid`s
  cannot turn the gateway into a request amplifier against the issuer.
- **JWKS failures fail closed, with stale keys served for a bounded, configured
  time.** If a refresh fails the cached keys keep verifying for up to
  `jwks_max_stale_secs` (default 1 hour) past their refresh time, so an issuer
  outage need not take the platform down, then verification fails with 503
  `jwks_unavailable` and `Retry-After`. **The revocation bound is therefore
  `jwks_refresh_secs + jwks_max_stale_secs` while the issuer is unreachable**, and
  `jwks_refresh_secs` otherwise; an operator who wants a tighter bound sets the
  stale window to 0. At startup an unreachable JWKS is not fatal (the replica
  would otherwise not boot in an outage that predates it) and `--check` reports it.
  **`/ready` does not depend on any JWKS**: an unready replica receives no
  traffic, so one issuer's outage would take down bearer-key and mTLS callers too.
  A tokenless issuer's callers get 503; its state is on `/v1/serve/config`
  (loaded, age, last error class) and in metrics.
- **Secrets stay secrets.** A token is never logged, echoed, put in a span, a
  header towards a provider, or an error message. The JWKS URL may carry no
  credentials (userinfo is refused at validation).
- **Reload.** `[[auth.jwt]]` is not reloadable in this contract (a change needs a
  restart, as for any table but `[[keys]]`); the `jwt` binding of a key reloads
  with the key, and a reload that names an unknown issuer is refused whole, by
  path.

### IP allowlists

- `allowed_cidrs` on a key, and `[server] trusted_proxies` for how the client
  address is known. CIDRs are IPv4 or IPv6, validated at load (a bare address is
  a /32 or /128), and a key with an empty list is refused: "allow nothing" is a
  mistake, not a configuration (omit the key to remove it).
- **The peer address is the TCP peer, unless the peer is a trusted proxy.**
  `trusted_proxies` is a list of CIDRs (default empty). Only when the connection's
  peer is inside it is the right-most `X-Forwarded-For` entry **not** in
  `trusted_proxies` taken as the client (multiple header lines are joined in
  order first); `Forwarded` and every other header are ignored. With no trusted
  proxies a forged `X-Forwarded-For` has no effect, which a test pins. **A trusted
  peer with no usable client address fails closed:** a missing header, an entry
  that is not a bare IP (a port, brackets, a zone id), or entries that are all
  trusted proxies leave the request with no client address, and a key with
  `allowed_cidrs` refuses it (403). The proxy's own address never stands in for
  the client, because trusted ranges often sit inside the allowed ones.
- **Addresses are normalised.** An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`, as
  a dual-stack listener reports an IPv4 peer) is unmapped before every comparison
  (peer, forwarded entries, CIDRs). A CIDR with host bits set (`10.1.2.3/8`) and
  one in `::ffff:0:0/96` are refused at validation.
- **Checked right after authentication, before idempotency replay, receipt
  lookup and admission, on every route** (`/v1/chat`, the native `/v1/messages`
  and `/v1/chat/completions`, `/v1/responses`). A request authenticated as a key
  whose `allowed_cidrs` do not contain the client address is refused with 403 and
  never reaches a cached response, the queue, a rate bucket or a budget. Under
  `allow_unauthenticated` there is no key, so no allowlist applies. The refusal is
  logged with the key's name and the address. It applies to all four credential
  kinds, so a stolen bearer key and a replayed JWT are both bound to the network.
- **The listener's own address is not a network policy.** `[server] host` still
  decides what the process listens on; allowlists are per key, because a platform
  has callers in different networks.

## Failure behaviour

- JWKS unreachable: see above (stale for an hour, then 503; never "accept").
- Clock skew: bounded by `clock_skew_secs`; not adjustable per token.
- An issuer that rotates keys: new `kid` triggers a refresh; old keys drop at the
  next successful fetch, so a revoked key stops verifying within
  `jwks_refresh_secs`. Token revocation before `exp` is not supported (short
  lifetimes, at most 24 h, are what bound it, as with certificates in 0011).
- Fleet: nothing is shared. Each replica fetches and caches the JWKS itself,
  because the data is public and fetching it is cheaper than coordinating it.

## Not decided here

- Per-issuer claim mappings (a `tenant` from a claim instead of a key table).
  Deliberately not offered: every identity maps to a configured key, so limits,
  budgets and audit stay one table.
- Certificate-bound or DPoP tokens, and token introspection (RFC 7662).
- OIDC discovery (`.well-known/openid-configuration`). The JWKS address is
  configured so that nothing a network peer sends can redirect trust.
- Opaque (non-JWT) access tokens.

## Slices

Every slice that adds a dependency or a response shape also adds its 0014 row and
an outage test in the `serve` suite (the JWKS endpoint going away is the new row),
the 401/403/503 shapes in `api/openapi.yaml`, and metrics for refresh outcomes,
stale serving and refresh refusals.

1. This note.
2. Done: IP allowlists: `allowed_cidrs`, `trusted_proxies`, the 403 path, `--check`
   validation, schema and OpenAPI, tests (forged `X-Forwarded-For` with and
   without a trusted proxy; each credential kind refused outside its network;
   admission untouched on refusal; mutation checks).
3. Done: the JWT verifier as a unit: parsing, claims, algorithms, JWK matching, against
   fixed test vectors and a local JWKS server (the confusion attacks: `alg: none`,
   `HS256` with the public key, wrong `kty`, missing `exp`, audience mismatch,
   skew edges, oversize token), with no wiring.
4. The JWKS client: fetch, cache, bounded refresh, stale-then-503, `/ready`.
5. Wiring: the `jwt` key binding, the bearer-shape rule, access-log reasons,
   `--check`, reload by path, the schema, an end-to-end test with a local issuer.
