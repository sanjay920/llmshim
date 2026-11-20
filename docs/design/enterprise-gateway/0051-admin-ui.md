# 0051: The admin UI

Status: slices 1 (the request ledger and the gateway's `[usage_export]`) and 2 (the UI shell, Home and Requests) built; the other views, payload capture and the other views are described and not
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
2. **The UI shell and the Requests and Home views (built).** The static pages, sign-in, the request table with filters and a
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

## Slice 2 as built

- **Where.** `crates/llmshim-control/ui/{index.html,app.js,style.css}` are compiled in (`include_str!`) and served by
  `src/ui.rs` at `/ui/`, `/ui/app.js`, `/ui/style.css` and `/ui/config.json`. They are public (they hold no data): `/ui` and what is under it by exact prefix (`/uix` and the API are still behind the token), `GET` and
  `HEAD` only, `404` for any other file.
- **Headers.** Every page carries `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self';
  img-src 'self' data:; connect-src 'self' [<token endpoint origin>]; base-uri 'none'; form-action 'none';
  frame-ancestors 'none'`, plus `nosniff`, `no-referrer`, `X-Frame-Options: DENY` and `no-store`. A test refuses
  `unsafe-` anywhere in it. The page uses no inline script or style and sets styles only through the DOM.
- **Data is text.** Tenant, key and model names are typed by callers, so the script builds elements with
  `textContent` only; a test refuses `innerHTML`, `eval` and the other ways of parsing data as markup, and the
  smoke run below used a key named `<img src=x onerror=alert(1)>`.
- **Sign-in.** The issuer file (0049) may carry a `ui` object: `client_id`, `authorization_endpoint`,
  `token_endpoint` and an optional `scope` (default `openid`). Validation is by field and never echoes a value: https
  only (http for `localhost`), a plain host (letters, digits, dots, hyphens, or a bracketed IPv6 literal) and a numeric
  port, no credentials, fragment, backslash, quote or wildcard in an endpoint (so the origin put in `connect-src` is
  exactly the origin the browser will call), visible ASCII for the id and scope. The
  page runs authorization code with PKCE (a random verifier and `state` held in `sessionStorage` for the one round
  trip, `S256`) with the redirect to `/ui/`, exchanges the code at the token endpoint, and sends the **access
  token** as a bearer exactly as a script would, so `audience` in the issuer file must be the audience of the access
  token. The token endpoint's origin, and only that, is added to `connect-src`, so the provider must allow a
  cross-origin POST from this origin (the usual setting for a public browser client). With no `ui` object, or with
  `break_glass` on, the page offers pasting a token, held for the tab only.
- **Views.** Home: a range (24 hours, 7 or 30 days), spend, attempts and failures, tokens, cache hit, slowest and
  average duration, spend by day and a table by model. Requests: filters (range, tenant, client, model, provider,
  outcome, request id, minimum cost from the facet lists), one row per attempt as in the screenshot that prompted
  this (time, status, model and provider, client and tenant, tokens in and out, cache share, duration, cost), a
  detail panel for every field, keyset paging. A cost is a dash when unknown, `est` for a catalog price and `≥` for
  a provider floor; the footnotes say what the numbers include.
- **Checked** by `tests/ui.rs` (public pages and nothing else, headers, config, the sign-in refusals, markup and
  origin hygiene, every route the script calls exists in `ROUTES`), by mutation (a prefix match for `/ui`, a token
  endpoint other than the configured one in the policy), and by driving the real page in Chrome against a running
  server with sample data: sign in by token, Home, Requests, a filter, the detail panel, no console error, no dialog,
  no element created from the hostile name.
- **Not here.** The OIDC redirect is verified by its parts (the validation, the policy, the page's use of PKCE), not
  against a live identity provider: that needs one, and is part of using it.
