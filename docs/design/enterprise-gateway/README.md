# Enterprise gateway: roadmap and working rules

Status: in development, behind the default-off `serve` Cargo feature. Nothing
here changes `llmshim proxy`, `llmshim gateway`, the library API, or any default
build.

The goal is a gateway a large organization can standardize on: accurate spend
accounting, no database in the request path, strict protocol fidelity, and the
identity, audit and packaging a security review asks for. There is no deadline.
Each step must be correct, tested and reversible before the next begins.

## Working rules

1. **Default builds do not change.** New behavior lives behind `serve` (or a
   later, equally explicit feature). `serve` is implied by no other feature, and
   the Dockerfile's default `FEATURES` is not changed.
2. **Root crate first.** The published crate cannot depend on an unpublished
   one, so server work lives in `src/serve/`. A separate control-plane crate
   comes later, when there is a control plane.
3. **Contracts before code.** A file format, HTTP surface or snapshot format gets
   a numbered design note here before it is implemented, because each becomes a
   compatibility promise.
4. **Fail closed, and say why.** Unknown fields, missing secrets and
   unauthenticated configurations are errors. Errors name the setting and never
   echo a secret.
5. **No process-global shortcuts.** `serve` never sets environment variables to
   drive `from_env` constructors, and never calls `process::exit` below `main`.
   Configuration is an immutable value; a reload swaps the caller keys (0012)
   while rate buckets, the spend ledger, breakers and queues survive.
6. **llmshim does not learn harness vocabulary.** Names of teams, routes, tags
   and policies are opaque.
7. **Every step is verified against every build:** default, `proxy`, `gateway`,
   `gateway-redis` and `serve`, plus the release gate and `cargo package`.

## Design notes

- [0001: Server configuration file](0001-server-config.md)
- [0002: Environment variables and the configuration file](0002-environment-and-configuration.md)
- [0003: Caller keys under serve](0003-caller-keys.md)
- [0004: serve runs the gateway from a file](0004-serve-boot.md)
- [0005: A separate listener for operational endpoints](0005-admin-listener.md)
- [0006: Fleet mode under serve](0006-fleet-mode.md)
- [0007: TLS termination and certificate renewal](0007-tls.md)
- [0008: Helm chart](0008-helm-chart.md)
- [0009: The access log](0009-access-log.md)
- [0010: Usage and cost records per tenant](0010-usage-records.md)
- [0011: Mutual TLS and certificate-derived identity](0011-mutual-tls.md)
- [0012: Reloading caller keys without a restart](0012-key-reload.md)
- [0013: llmshim as a component](0013-component-bar.md)
- [0014: What happens when a dependency fails](0014-failure-modes.md)
- [0015: The operational endpoints are in the OpenAPI document](0015-operational-api.md)
- [0016: OpenTelemetry traces](0016-opentelemetry.md)
- [0017: Workload JWT authentication and IP allowlists](0017-workload-jwt-and-ip-allowlists.md)
- [0018: Benchmark](0018-benchmark.md)
- [0019: Published, signed images and an SBOM](0019-signed-images.md)

## Roadmap

Phase 0: a production server mode

- [x] `serve` feature flag and the versioned configuration contract, with
      `llmshim serve --config <file> --check` (0001)
- [x] Environment-free constructors, first part: scheduler tuning, rate
      limits and single-instance gateway assembly; precedence written down (0002)
- [x] Caller keys: hashed key store built from the file, errors returned (0003)
- [x] Environment-free constructors for origin policy, deadlines, receipts,
      breaker, limiter and application assembly (0002)
- [ ] Remaining inventory row in 0002: the per-router HTTP client settings
      (the other rows, including accounting retention, the penalty and the
      receipt lock timeout, are converted). The benchmark (0018) shows why it
      matters: the shared client's pool of 4 idle connections per host caps
      throughput under concurrency
- [x] `llmshim serve` boots the single-instance gateway from the file, including
      hashed caller keys (0004)
- [ ] Bedrock, Vertex and Azure configurable from the file; a provider whose
      feature is not compiled is a startup error naming the rebuild flag
- [x] Redis fleet mode under `serve`, behind `serve-redis` (0006)
- [x] Caller keys reload without a restart: SIGHUP and a watched file (0012)
- [ ] Reload of everything else (providers, routes, limits)
- [x] Separately bound, optionally token-protected `/metrics`, stats and probes (0005)
- [x] TLS termination with renewal on file change (0007)
- [x] Mutual TLS and certificate-derived identity (0011)
- [x] JWT/OIDC workload identity (0017); optional client certificates (0011).
      Token revocation before `exp` is not supported (short lifetimes bound it)
- [x] Access log with tenant attribution and credential fingerprints (0009)
- [x] Token and cost attribution per tenant and attempt (0010)
- [x] OpenTelemetry traces (design note 0016; metrics stay on `/metrics`): the
      request span, the attempt span and the exporter (`serve-otel`), and the log
      writers' counters in `/metrics`; linking a fleet attempt to a request on
      another replica is a later slice
- [x] Helm chart, validated by rendering and `serve --check` (0008)
- [x] Published, signed images and an SBOM (0019): the workflow exists and the
      feature set is built on every Dockerfile pull request; publishing is **off**
      until the owner sets the repository variable `PUBLISH_IMAGE` to `true`
- [x] The failure-mode table (0014), with outage-survival tests for losing Redis
      (before and during a request) and for `SIGTERM`; its "Not yet tested" list
      is what remains
- [x] Reproducible benchmark (0018). It found two limits that are open items:
      the upstream connection pool (4 idle per host, shared client) and the
      replay-receipt executor (8 ingress and 4 egress slots); neither is changed yet
- [x] Configuration-contract tightenings, a structure-only `--check` and a
      checked-in JSON Schema (see "Review follow-ups"; 0013)
- [x] Pull-request CI for every build, including the Redis-backed tests that are
      ignored without a server (`ci.yml`; 0013)
- [x] The operational endpoints in the OpenAPI document, with a conformance test
      (0015)
- [x] A request id unique across instances, with the tenant and key name joined
      across the access log, the usage records and the trace (0013)
- [x] The release gate builds and tests `serve` and waits for a green `ci.yml`
      and `serve-check.yml` on the tagged commit (0013). Requiring them by branch
      protection on `main` is a repository setting for the owner, not changed

Later phases (each needs its own design note before work starts)

- [ ] Control plane: organizations, teams, projects, key lifecycle, signed
      policy snapshots, bounded local spend allowance during a Redis outage
- [ ] Hierarchical budgets, model allowlists, tags, alerts, cost exports
- [ ] Deployments and load balancing across credentials and regions
- [x] Workload JWT authentication and per-key IP allowlists (0017; mTLS is 0011)
- [ ] Audit log
- [ ] Guardrail pipeline on the existing `AttemptPolicy` hooks; residency policy
- [ ] Secret-manager integrations
- [ ] Config-registered providers; embeddings, images and audio on the server
- [ ] Admin UI, SSO and SCIM (not started: they follow the control plane and the
      licensing decision)

## Review follow-ups

Every PR in the first stack (0001 to 0012) was reviewed by an independent
reviewer before it merged. A finding that held up was fixed in its PR unless it
is listed here; these are the findings that were judged out of scope, or a
decision about a contract, so that none is lost. They are not blockers; the ones that touch a contract are marked, because a
contract is cheaper to change before the feature is declared stable.

Contracts and decisions (change before stability)

- **`routes.*.settings` is an open map**: a misspelled setting is accepted
  silently (the adapters ignore a top-level key they do not read). Closing it
  needs one authoritative list of request keys, which the adapters do not keep in
  one place: the request-field table in the docs is the nearest, and it omits keys
  the adapters read, such as `response_format` and `parallel_tool_calls`. A list
  that rejects a legitimate key is worse than an open map, so this waits for the
  list.
- **Redis keys carry no namespace** (0006): two fleets that share a Redis and a
  provider name take each other's jobs. A `fleet.namespace` prefix.
- **Optional client certificates and revocation** (0011), a CRL file watched like
  the CA bundle.
- **Tokens**: caller keys are compared with provider and admin keys by digest in
  `check_same_values`, which `--check` and startup run, but `build_key_store`
  alone does not.

Operability

- **The Redis-backed tests run one at a time.** `ci.yml` runs every build on each
  pull request and on main, and the 64 ignored tests that need a Redis (63 in
  `gateway::`, one in `proxy::ratelimit::`; they ran nowhere before) with
  `--test-threads=1` and a floor on how many ran. Run in parallel against one
  Redis on a development machine, 13 of the 63 failed: every gateway's reaper
  sweeps the origin-lease index and the shared capacity counters of all the
  others, so tests that simulate a crashed origin or a shutdown reap each
  other's gateways. Each passed alone. A `fleet.namespace` (above) would give
  each test its own keys and should let them run in parallel; that is expected,
  not yet shown.
- **Timing-dependent tests flake on a loaded runner.** The first dozen runs of
  `ci.yml` and `serve-check.yml` failed on nine tests, each a different race: a
  heartbeat tick on a stream's total deadline; 10, 20 and 50 ms client
  deadlines that also cover connecting; a 50 ms server head deadline; a TLS
  close that arrives as a reset rather than an end of file; a test that read an
  asynchronous log before its writer had written; one that searched a status
  document for digits that its config hash and timestamps also contain; and
  one dead helper under a feature nobody built. Each is fixed with margins or
  a looser, still-meaningful assertion. Others keep short margins and will need
  the same if they fail: `fallback.rs` (a 20 ms header timeout against live
  providers), the rest of the 90 ms-lease tests in `gateway/distributed.rs` and
  `gateway/http.rs`, `http_server.rs` and `managed_server.rs` (50 ms head
  deadline, a TLS handshake inside 50 ms), and `proxy/mod.rs` (a first response
  inside 50 ms). Local load does not find them (four runs under twice the core
  count of busy loops passed): burn-in on the CI runners does.
- **A stream that reaches its total deadline can end as `origin canceled`
  instead of a timeout.** The origin lease expires at the deadline, so the
  stream's own timer and the notice that the origin is gone race, and either can
  win (a heartbeat tick on the deadline was one way to lose, not the only one:
  moving the test's deadline off the tick still failed in CI). Both are errors at
  the deadline, in different classes: a timeout is reported as overload (with
  `Retry-After`), `origin canceled` as a provider error. The test accepts either;
  reporting a timeout whenever the deadline has passed is a separate change to
  gateway code.
- **A Gemini response with thoughts but no `candidatesTokenCount` settles as
  incomplete.** Google omits a zero, so the absence there is the provider's
  zero, yet the counters stay marked incomplete and, under catalog authority,
  the ledger keeps the reservation instead of the observed figure. Treating the
  missing count as zero when the response's own `totalTokenCount` reconciles
  (prompt plus thoughts equals the total) is a separate change to shared
  accounting with its own tests.
- **A Gemini reservation is an upper bound only if Google's output cap covers
  thinking.** The quote is the larger of the output cap (or the catalog's) and
  the thinking budget; a request that sets only a thinking level declares no
  budget. Settlement above the reserved liability raises it, so a quote that is
  too low is corrected rather than missed, but nothing pins the assumption.
- **A tag is not tied to `ci.yml`.** The release workflow still runs only fmt,
  clippy and `cargo test --workspace --features proxy --tests`, plus the Windows
  check. Making it wait for a green `ci.yml` on the tagged commit, and adding
  the `serve` builds, is a step toward declaring `serve` stable.
- **Nothing forces a red check to be read.** `main` has no branch protection or
  ruleset, so a failing check does not block a merge. Requiring `ci.yml` and
  `serve-check.yml` is a repository setting for the maintainer.
- **A caller whose request loses Redis mid-flight is told `gateway shutting
  down`** (a 503, from `GatewayError::Shutdown`, which the unary wait also
  returns when the origin's deadline passes). The status is the right one to retry
  on; the words are wrong. 0014 records it.
- **0006 and the code disagree about failing closed.** 0006 says spend and rate
  coordination fail closed when Redis is lost; the shared rate limiter
  (`RedisRateLimiter`) and the older per-tenant `SpendStore` are documented in
  the code as failing open. Establish which of them `serve` can reach, test it,
  or correct 0006 (0014, "Not yet tested").
- **Shutdown**: a replica does not release leases it holds for peers' jobs on
  SIGTERM (as `gateway`); they wait out the lease timeout.
- **The Helm chart** has no `preStop` hook and a 60 s grace period that is
  shorter than a long stream; the admin Service name can exceed 63 characters;
  `appVersion` is not in the release checklist's lockstep.
- **The Docker check** runs on pull requests only and does not cover a
  `src/**` change or a moved base image tag; add a push and a weekly run.

Tests worth adding

- A taken main port exits 1 (the admin port's is tested); `allow_unauthenticated`
  serving; stderr scanned for secrets after requests. (`SIGTERM` draining both
  listeners and exiting 0 is tested for one replica, 0014.)
- A fleet test that a replica which fails to bind leaves no lease on a peer's job
  (the window is not deterministic today).
- `Limits::new` takes three positional numbers (entries, bytes, ttl); a named
  struct removes the transposition risk. `build_limiter_with` and
  `build_breaker_with` are public in the `proxy` feature and could be
  `pub(crate)`.

## Decisions

Taken 2026-11-14, with the reasons in 0013.

- **Database** for the control plane: Postgres, SQLite for one node, never in the
  request path. The design note comes before the code.
- **Images:** GHCR, signed keyless, with an SPDX SBOM, from an opt-in publish job.
- **Stability:** `serve` stays unstable until the criteria in 0013 are met.

## Open decisions

- **Licensing** of identity and audit-export features (SSO, SCIM, audit export,
  the admin UI). None of that code is started. All of it follows the control
  plane, so nothing in the order of work (0013) waits on the decision.
