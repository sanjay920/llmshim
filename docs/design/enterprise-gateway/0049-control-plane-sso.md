# 0049: Single sign-on for the control plane

Status: built. Written on the owner's instruction to implement it. OIDC token **verification** on the control-plane
API, the verified subject as the actor, three fixed roles. **SCIM and an admin UI are not part of this** and were not
started.

## Why

The control plane (0024, 0026) has one bearer token, so "who made this change" is whatever a caller typed into
`x-llmshim-actor`. The hash chain over the change history (0047) proves rows were not altered; it cannot make the
actor in them true. With SSO the actor is the subject of a token an identity provider signed.

## What it is

`llmshim-control serve --sso-config <FILE>` points at a JSON file for **one issuer**:

```json
{
  "issuer": "https://idp.example",
  "audience": "llmshim-control",
  "jwks_url": "https://idp.example/.well-known/jwks.json",
  "algorithms": ["RS256"],
  "ca_file": "/etc/llmshim/idp-ca.pem",
  "clock_skew_secs": 30,
  "jwks_refresh_secs": 300,
  "jwks_max_stale_secs": 3600,
  "break_glass": true
}
```

Verification is the data plane's (`serve::jwt`, `serve::jwks`, 0017): RS256, ES256 or EdDSA only (never `none` or a
symmetric algorithm), the issuer's rules, keys from its JWKS (https, or http to a loopback literal; cached, refreshed,
served stale within a bound, one refresh per interval for an unknown `kid`), claims believed only after the signature,
`exp` required and at most 24 hours ahead. The unverified `iss` is read only to see whether a token is ours; the
issuer's own rules then decide.

## The actor

With `--sso-config` the actor of every change is **derived from the credential and nothing the caller sends**:

- a verified token: `oidc:<sub>` (the subject, at most 100 characters of visible ASCII; a longer or unusual one is
  `oidc-hash:<first 16 hex digits of its digest>`, so a row is never unreadable or a header-injection vector, and a
  hashed actor can never equal a plain one: `oidc:` and `oidc-hash:` differ at the fifth character);
- the break-glass token: `break-glass`.

`x-llmshim-actor` is **removed from every request** in this mode and replaced with the derived value, so a caller can
no longer claim to be someone else. **Without `--sso-config` nothing changes**: the one token, the header as the actor.

## Roles

Fixed, not configurable, read from the token's `roles` claim (RFC 9068 names it for JWT access tokens; an array of
strings, or one space-separated string). The values are namespaced so that a token carrying roles for other
applications grants nothing here, and an unknown value is ignored:

| Role value | Allows |
| --- | --- |
| `control:read` | every `GET`: organizations, teams, projects, keys, the history, the latest snapshot, `whoami` |
| `control:key-issuer` | read, and the key lifecycle: create, revoke, rotate, change limits |
| `control:admin` | everything: also organizations, teams, projects, team budgets and **publishing a snapshot** |

The highest one wins (admin ⊇ key-issuer ⊇ read). A verified token with none of them is authenticated and **refused
everything with 403, `whoami` apart** (`forbidden`, naming the role needed, never the roles held). A route's required role is a
table in the code, and a test fails if a route has none, so a new route cannot ship open. Publishing is admin-only
because a published snapshot is what the data plane enforces.

`GET /v1/whoami` answers `{"actor", "role", "via"}` (`via`: `oidc` or `break_glass`) for **any authenticated caller,
including one whose token carries no role** (`role: null`): the one route that needs no role, so an operator whose
identity provider's mapping produced nothing can see that. A `HEAD` needs what the `GET` does.

## The shared token

It stays as **break-glass** (`--token-file`), with the admin role, as the actor `break-glass`. `"break_glass": false`
turns it off: the token is then neither required nor accepted (a `--token-file` given with it is a startup error,
not a silent ignore). A bearer value shaped like a JWT is **only ever verified as one**, never compared with the shared
token, and one that fails is refused 401 and not retried as the shared token; anything else is compared with the shared
token when break-glass is on. A server with SSO and break-glass off, and a JWKS that cannot be fetched, answers 503
`unavailable` with `Retry-After`, never a pass.

## Decisions

- **`roles`, namespaced values, three fixed roles.** IdPs differ in where they put groups (`groups`, `roles`,
  `realm_access.roles`); one claim name keeps the contract small, and every mainstream IdP can emit a custom `roles`
  claim (an app role, a mapper, a claims rule). A configurable mapping is a larger surface than the first slice needs.
- **One issuer.** A second issuer is a second file section later; one is enough to ask "who".
- **Refusals do not write history.** The change history (and so its chain) records changes that happened. A token that
  fails, or a role that is too low, changes nothing and is answered 401 or 403; it is not counted anywhere yet.
- **Actor is a derived header.** The router replaces `x-llmshim-actor` before any handler runs, so every handler,
  present and future, reads the verified actor through the one function it already used.
- **Tokens are not logged**, in any message; the failure class is not in the answer either (401 is 401).

## Not built

- SCIM (provisioning and de-provisioning), an admin UI, login flows (the control plane only verifies tokens; getting
  one is the identity provider's and the operator's tooling's).
- Several issuers, a configurable role mapping, per-organization roles (every role is global), session revocation
  before `exp` (keep tokens short-lived), counting or auditing refusals.
- The gateway's own callers: they authenticate with keys, certificates and workload tokens (0017), unchanged.

## Tests

`tests/sso.rs` (a mock issuer that signs real Ed25519 tokens, served from a mock JWKS): a token with each role gets
exactly its routes (read cannot create, key-issuer cannot create an org or publish, admin can); no role is 403; the
actor in the history is `oidc:<sub>` and a caller's own `x-llmshim-actor` is ignored; the break-glass token is admin
as `break-glass` and `break_glass: false` refuses it; a JWT-shaped bad token (wrong key, wrong audience, expired,
`alg: none`, wrong issuer) is 401 and never falls back to the shared token; an unavailable JWKS is 503; `whoami`;
every route has a required role; without `--sso-config` the old behaviour is unchanged. `tests/binary.rs`: the
startup rules for the flags. The OpenAPI document is compared with the route table as before.
