# 0051: The admin UI

Status: slices 1 (the request ledger and the gateway's `[usage_export]`), 2 (the UI shell, Home and Requests) and 3 (Clients, Admin and History) and 4 (Gateways: routes, providers, circuits, configuration state) built; the other views, payload capture and the other views are described and not
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
3. **Clients, Admin and History views (built).** Keys (create, revoke, rotate, limits), organizations, teams, projects, the change
   history and its chain check, over the API that exists.
4. **Routes and Providers (read-only, built as the Gateways view).** What each gateway is configured with and its health, from the gateways' admin
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

## Slice 3 as built

- **New API route:** `GET /v1/history/verify` (admin role, one check at a time: a second is a 429, because it reads the whole history under the
  store's lock) runs the hash-chain check that `audit-verify` runs and answers
  `{ok: true, records, unchained, head}` or `{ok: false, row, reason}`. It reads the whole history once. The head
  hash is shown with the instruction to keep a copy where the database's writer cannot reach: the chain alone cannot
  show that the last rows were removed (0047). Everything else the views do uses routes that already existed.
- **Where.** `ui/manage.js`, loaded before `app.js` (the page now has exactly these two scripts, both ours; a test
  pins the list and their order). `api()` gained an optional method and JSON body.
- **Clients.** Per organization: every key with its project (team / project), state, credential kind, tier, limits,
  budget, expiry. A detail panel shows every field. With the key-issuer or admin role: create (project, name,
  credential `secret`, `client_san` or `jwt`, tier, limits, allowed models and addresses, expiry), edit limits (all five
  named, blank clears one, as the API requires), rotate (a replacement with the same limits; the old key stays until
  revoked) and revoke (two clicks). A new secret appears **once**, in a panel with a copy button, and is held by
  nothing else: not the table, not the page's state, not storage.
- **Admin.** Organizations, their teams and each team's projects and budget. With the admin role: create each, set
  or clear a team budget (clearing takes two clicks).
- **History.** The newest 100, 500 or 1000 changes (actor, action, subject, detail) and, for the admin role, a "Check the
  chain" button (the others are pointed at `audit-verify`).
  Older rows than 1000 are reached by the API's `limit` only; paging by id is left for when it is asked for.
- **Roles.** The page asks `whoami` and hides what the role cannot do, but never relies on that: each action is the API
  call a script would make, and the API refuses it by role (0049). A failed action shows its message beside its form.
- **Checked** by `tests/ui.rs` (both scripts, headers, hygiene, every `/v1` string in the scripts and every verb the
  page sends is a route the API has), `history_chain::over_http` (the route reports a good chain, and names the first row
  of a tampered one), and by driving the page in Chrome: create an organization, team (with a name that is markup),
  project and budget; create a client and see its secret once; edit limits; rotate; revoke; check the chain; no
  console error, no dialog, no element made from the markup name.

## Slice 4 as built

- **How the control plane learns about gateways.** `--gateways-file` names each gateway's admin listener (`name`,
  `admin_url` as an origin, optional `token_file` and `ca_file`, `allow_insecure_http` for a private network). It is the
  operator's file, not a caller's input, so there is nothing to forge a request through: https unless said otherwise,
  no credentials, path, query or fragment in the address, a bounded list (64), owner-only token files re-read at every
  fetch (a rotation needs no restart), redirects not followed, ambient proxies not used. Errors name the entry and the
  field and never echo a value.
- **`GET /v1/gateways`** (read role): per gateway, `reachable`, `ready` (from the open `/ready`), the gateway's
  `/v1/gateway/stats` and `/v1/serve/config`, or an `error` code (`unreachable`, `unauthorized`, `bad_response`,
  `token_unreadable`). The gateway's own error text is never repeated. A body over 1 MiB is a `bad_response`. One answer
  is shared for five seconds, one fetch at a time, so any number of viewers is one request per gateway per interval.
  With no file the answer is `{configured: false}` and the rest of the control plane is unchanged.
- **Gateway side (root crate, additive).** The admin stats gained `provider_circuits` (each provider's circuit as that
  process sees it) and `routes` (each named route's model and request defaults). The stats are served by listeners that
  may be open (without `[admin]` they share the main listener), so a route's own text is never put on them: only a
  well-known generation control (`reasoning_effort`, `max_tokens`, `temperature` and a few more) with a bool, number or
  short string is shown, and every other default (a `user`, a `system` text, a list, an object) is named with the value
  `<hidden>`. `Router::route_overview`
  and `provider_overview` are new public methods (additive).
- **The view** (`Gateways`): per gateway its readiness, mode, providers with circuit and queue depth, deployments with
  each target's weight and circuit, named routes with their defaults, and the configuration state (in sync with its file,
  generations, credentials in force, file hash, load time, last reload, provider secrets that a reload cannot rotate,
  snapshot and audit status). It shows the configuration and never changes it: that stays a file under version control.
- **One gateway per address.** A replica set behind one Service shows whichever replica answers: list each replica (or
  the headless names) to see each one. A circuit is a process's own view (the fleet's shared view is in Redis).
- **Checked** by `tests/gateways.rs` (a gateway read with its own token and the answer shared; every failure a code and
  none carrying the gateway's text or a token; a redirect not followed; a rotated token picked up; the file's validation by
  entry and field), mutation (following redirects, no cache), `unit_router` (the overview and its masking),
  `integration_serve_deployments` (a real `serve` process reports provider circuits and routes, long defaults masked), and
  by running a real `llmshim serve` with its admin listener against a real control plane and the page in Chrome (one
  gateway read, one unreachable).

## The redesign (after slices 1 to 4)

The first pages were functional and plain. This pass rebuilt the look and the interaction, not the contracts: the same
routes, the same roles, the same data.

- **Shell.** A left navigation (Observe: Home, Requests; Manage: Clients, Organizations, Gateways, History), a page header
  with the page's own controls, the signed-in person and role at the foot. Light and dark follow the system. Geist and
  Geist Mono are served from this origin (`/ui/geist.woff2`, `/ui/geist-mono.woff2`, SIL OFL 1.1, licence at
  `/ui/OFL.txt`); `font-src 'self'` is the only addition to the policy. The pages stay a no-build, three-script set
  (`kit.js` helpers and charts, `manage.js`, `app.js`).
- **Home leads with spend.** One hero figure with its change on the previous window of the same length, a column chart
  (hourly for 24 hours, UTC days for 7 and 30), stat tiles with 12-point trends, a cache-hit line chart, spend by model
  as ranked bars with each model's cached share, a clients table, and each model's prompt composition (uncached, read
  from cache, written to cache) as a thin segmented bar. Charts follow the data-visualisation rules the project adopted:
  one accent, the validated reference palette, 24px columns with 4px rounded tops, hairline grid, a hover and keyboard
  tooltip on every mark, and a **Table** toggle that shows the same values. Unknown cost is a dash and "no price" in a
  tooltip, never a zero bar; `est` and `≥` mark estimates and lower bounds.
- **Requests** keeps one row per attempt, with relative time, a status dot and label, model and provider, client and
  tenant, tokens in and out, a cache meter and the cost. A request opens a slide-over: the cost as a figure with where
  the number came from, the prompt split, usage, who, identifiers with copy buttons.
- **Forms are slide-overs, not inline.** New client, edit limits, rotate, team budgets; a secret is shown in a sheet that
  Escape and a click outside cannot dismiss, only "I have stored it". Actions say what happened in a toast; failures
  stay beside the form.
- **One API addition.** `GET /v1/requests/summary?group_by=hour` buckets by UTC hour, named by the whole hours since the
  epoch (so the page, not the server, picks the zone).
- **Checked** by the existing page tests (now three scripts, the font assets with their own cache rule, `font-src`), a new
  hourly-grouping test on SQLite and Postgres, and by driving the page in Chrome in light and dark: a wrong token says it
  was not accepted (a stale one says the session ended), hover tooltips, the 24-hour range, filters, creating a client
  and seeing its secret once (Escape does not close it), revoking, adding a team named like markup, the history check.
