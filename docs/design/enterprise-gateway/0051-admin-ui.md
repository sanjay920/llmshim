# 0051: The admin UI

Status: slice 1 (the request ledger and the gateway's `[usage_export]`) built; the UI itself, payload capture and the other views are described and not
built. Written on the owner's instruction to start the admin UI, prompted by a screenshot of a proxy's request view: a
30-day history, one row per request with time, status, model and endpoint, client and route, tokens in and out, cache
percentage, duration and price, with the footnotes "input tokens include cache" and "API = estimate, not spend".

## What it is for

Two jobs, in the order a team that spends real money on inference asks for them:

1. **Cost and cache visibility.** Who spent what, on which model, with how much of the prompt served from cache, and what
   the provider said versus what a price list says. This is the screenshot's request table and a summary above it.
2. **Management.** Clients (the caller keys), organizations, teams and projects, the change history: what the control
   plane already holds and today can only be reached by a `curl` of its API.

Debugging a poor cache hit rate from the payloads, the second thing that post praises, is a third job that needs captured
payloads. That is a privacy decision of its own and is **not** in the first slices (below).

## The decisions

- **The UI is the control plane's.** `llmshim-control` already has what a UI needs and a gateway does not: identity with
  roles (0049), durable storage that is shared by a fleet (SQLite or Postgres), and the objects the management views are
  about (organizations, teams, projects, keys, the chained change history, 0047). A gateway replica sees only its own
  traffic and is built to be stateless and replaceable; a UI on it would show one replica's slice of a fleet. The cost
  is that the control plane now stores request metadata, which it did not.
- **Gateways push; the control plane stores.** An opt-in `[usage_export]` table makes a gateway send its usage records
  (the same per-attempt records the usage log writes, 0034) to the control plane in batches. The push is **bounded and
  fails open**: a full queue drops the oldest records and counts them, an unreachable control plane delays nothing a
  caller sees. Observability must never be able to stop serving.
- **A request ledger in the control plane's database.** One row per provider attempt, keyed by the attempt id so that a
  retried batch is stored once (delivery is at least once, storage is exactly once). Kept for `--request-retention-days`
  (default 30) and deleted past it by the control plane itself.
- **Metadata only, for now.** A row holds who, what model, how it ended, tokens, cache tokens, duration, cost and where
  the cost figure came from. It holds **no prompt, no completion and no header**. Payload capture is a separate slice with
  its own design (sealed objects, an explicit per-key opt-in, guardrails applied first, a short retention).
- **The UI is a static page set the control plane serves.** Plain HTML, CSS and JavaScript compiled into the binary: no
  build toolchain, no framework, no CDN, no third-party code on a page that handles an admin token, and a strict
  content-security policy (own origin only). It speaks to the same JSON API as every other client; there is no
  UI-only backend.
- **Sign-in is the browser's, with no server session.** The control plane only verifies bearer tokens (0049). The page
  signs a person in with the identity provider (authorization code with PKCE, from the browser) and sends the access
  token as a bearer, exactly as a script would; the page learns the issuer and client id from a small public
  `/ui/config.json`. Break-glass is pasting the shared token. Roles gate what the page shows and what the API allows
  (read, key-issuer, admin); the UI is never the only thing standing between a user and an action.
- **Cost is shown for what it is.** `cost_usd` is nullable and the source rides with it (`provider`: an exact bill;
  `provider_floor`: a lower bound; `catalog`: an estimate from a price list). The page marks estimates and never turns
  an unknown cost into zero (0034).

## Slices

1. **The request ledger (this note's first slice).** In the control plane: schema version 6 (`requests`), an ingest
   endpoint with its own token, query and summary endpoints, retention. In the gateway: the `[usage_export]` sink. Tests
   across both, on SQLite and Postgres.
2. **The UI shell and the Requests and Home views.** The static pages, sign-in, the request table with filters and a
   detail drawer, the spend and cache summary.
3. **Clients, Admin and History views.** Keys (create, revoke, rotate, limits), organizations, teams, projects, the change
   history and its chain check, over the API that exists.
4. **Routes and Providers (read-only).** What each gateway is configured with and its health, from the gateways' admin
   listeners. The configuration stays a file under version control: the UI shows it, it does not edit it.
5. **Payload capture and the cache debugger.** Opt-in capture of a request's payload, sealed and short-lived, a diff of
   consecutive requests' prefixes to show *where* a cache miss began. Needs its own note before any code.

## Not built, and not planned

- Editing the gateway's configuration from the UI, or a secrets manager: the file stays the source of truth (0001).
- A UI-specific user store, passwords or sessions: identity is the identity provider's.
- Alerting and dashboards beyond the views above: the metrics endpoint and the usage log exist for the tools that do that.
