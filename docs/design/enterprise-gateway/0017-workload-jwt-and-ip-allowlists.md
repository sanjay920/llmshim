# 0017: Workload JWT authentication and IP allowlists

Status: design only. A contract note, written before any code (README working
rule 3). Nothing here is implemented; the slices are listed at the end. Single
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
jwks_url = "https://oidc.example.com/keys"   # https only (loopback http allowed in tests)
audience = "llmshim"                         # `aud` must contain it
algorithms = ["RS256", "ES256"]              # required, non-empty; see Rules
clock_skew_secs = 30                         # default 30, at most 300
jwks_refresh_secs = 300                      # default 300, at least 30

[[keys]]
name = "checkout"
tenant = "payments"
jwt = { issuer = "cluster", subject = "system:serviceaccount:payments:checkout" }
allowed_cidrs = ["10.0.0.0/8", "2001:db8::/32"]   # optional, on any key
tier = 2
```

A `[[keys]]` entry sets exactly one of `secret`, `sha256`, `client_san` and `jwt`
(extending the rule 0011 set). `jwt.issuer` names an `[[auth.jwt]]` table;
`jwt.subject` is the exact `sub`. Every other setting of the key (tenant, tier,
limits, budget) applies unchanged: the token only proves **which key**.

## Rules

- **Algorithms are configuration, never the token.** The token's `alg` header is
  accepted only if the issuer's `algorithms` lists it. `none`, every `HS*` and any
  algorithm the build cannot verify are refused at `--check`, not at request time.
  The supported set is `RS256`, `ES256` and `EdDSA`, verified with `ring`. A
  symmetric algorithm is refused outright: the issuer's JWKS is public, and an
  `HS256` token "signed" with a public RSA key is the classic confusion attack.
- **The key is chosen by `kid`, from the issuer's JWKS, and must fit the
  algorithm.** A token without a `kid` is accepted only when the JWKS has exactly
  one usable key. A JWK's `kty`/`crv` must match the token's `alg`; a mismatch is
  a failure, not a try-the-next-key.
- **Claims.** `iss` equals the configured issuer exactly; `aud` (a string or an
  array) contains the audience; `exp` is required and must be in the future and
  `nbf`, when present, in the past, each within the skew; `sub` is required and is
  the whole identity. A token without `exp` is refused: a non-expiring bearer is a
  key with extra steps. A token whose lifetime (`exp` minus `iat`, when `iat` is
  present) exceeds 24 hours is refused too.
- **A bearer key decides alone, as in 0011.** A request whose `Authorization:
  Bearer` value is not a JWT shape (three dot-separated base64url parts whose first
  part decodes to a JSON object with `alg`) is looked up as a key, exactly as
  today. A JWT-shaped value is verified as a JWT and **never** falls back to key
  lookup: a failed token cannot be rescued by happening to equal a key's secret.
- **Unknown workload fails closed.** A valid token whose issuer and subject no
  key lists gets 401, and the response says nothing about which part failed (the
  access log records a reason class: `jwt_invalid`, `jwt_unknown_subject`,
  `jwks_unavailable`).
- **One identity per request.** Presenting both a JWT and a certificate-derived
  identity is the same ambiguity 0011 refuses; the token wins as a bearer does,
  and the certificate header is ignored.
- **The JWKS is fetched by us, bounded and cached.** One fetch at a time per
  issuer, a 5-second connect and 10-second total timeout, a 1 MiB body limit, no
  redirects, https only, and the address is the configured one (no discovery
  document, no `jku` header, no `x5u`: a token never chooses where its key comes
  from). Keys are cached for `jwks_refresh_secs`. An unknown `kid` triggers one
  refresh, at most once per 30 seconds per issuer, so a flood of random `kid`s
  cannot turn the gateway into a request amplifier against the issuer.
- **JWKS failures fail closed, with stale keys served for a bounded time.** If a
  refresh fails the cached keys keep verifying for up to 1 hour past their
  refresh time (an issuer outage must not take the platform down), then verification
  fails with 503 `jwks_unavailable` and `Retry-After`. At startup an unreachable
  JWKS is not fatal (the replica would otherwise not boot in an outage that
  predates it), but `--check` reports it, and `/ready` is not ready until every
  issuer has loaded keys once.
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
  `trusted_proxies` taken as the client; `Forwarded` and every other header are
  ignored. With no trusted proxies a forged `X-Forwarded-For` has no effect, which
  a test pins. An unparseable or missing entry falls back to the peer address.
- **Checked after authentication, before admission.** A request authenticated as
  a key whose `allowed_cidrs` do not contain the client address is refused with
  403 and never reaches the queue, a rate bucket or a budget. The refusal is
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

1. This note.
2. IP allowlists: `allowed_cidrs`, `trusted_proxies`, the 403 path, `--check`
   validation, schema and OpenAPI, tests (forged `X-Forwarded-For` with and
   without a trusted proxy; each credential kind refused outside its network;
   admission untouched on refusal; mutation checks).
3. The JWT verifier as a unit: parsing, claims, algorithms, JWK matching, against
   fixed test vectors and a local JWKS server (the confusion attacks: `alg: none`,
   `HS256` with the public key, wrong `kty`, missing `exp`, audience mismatch,
   skew edges, oversize token), with no wiring.
4. The JWKS client: fetch, cache, bounded refresh, stale-then-503, `/ready`.
5. Wiring: the `jwt` key binding, the bearer-shape rule, access-log reasons,
   `--check`, reload by path, the schema, an end-to-end test with a local issuer.
