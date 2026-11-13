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

## Roadmap

Phase 0: a production server mode

- [x] `serve` feature flag and the versioned configuration contract, with
      `llmshim serve --config <file> --check` (0001)
- [x] Environment-free constructors, first part: scheduler tuning, rate
      limits and single-instance gateway assembly; precedence written down (0002)
- [x] Caller keys: hashed key store built from the file, errors returned (0003)
- [x] Environment-free constructors for origin policy, deadlines, receipts,
      breaker, limiter and application assembly (0002)
- [ ] Remaining inventory rows in 0002 (accounting retention, penalty, receipt
      lock timeout, per-router HTTP client settings)
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
- [ ] JWT/OIDC workload identity; optional client certificates; revocation
- [x] Access log with tenant attribution and credential fingerprints (0009)
- [x] Token and cost attribution per tenant and attempt (0010)
- [ ] OpenTelemetry traces and metrics
- [x] Helm chart, validated by rendering and `serve --check` (0008)
- [ ] Published, signed images and an SBOM
- [ ] Reproducible benchmark and an outage-survival test

Later phases (each needs its own design note before work starts)

- [ ] Control plane: organizations, teams, projects, key lifecycle, signed
      policy snapshots, bounded local spend allowance during a Redis outage
- [ ] Hierarchical budgets, model allowlists, tags, alerts, cost exports
- [ ] Deployments and load balancing across credentials and regions
- [ ] Workload JWT authentication, IP allowlists (mTLS is done: 0011)
- [ ] Audit log
- [ ] Guardrail pipeline on the existing `AttemptPolicy` hooks; residency policy
- [ ] Secret-manager integrations
- [ ] Config-registered providers; embeddings, images and audio on the server
- [ ] Admin UI, SSO and SCIM (blocked on the licensing decision)

## Review follow-ups

Every PR in the first stack (0001 to 0012) was reviewed by an independent
reviewer before it merged, and each real finding was fixed in the PR. These are
the findings that were judged out of scope or a design decision, so that none is
lost. They are not blockers; the ones that touch a contract are marked, because a
contract is cheaper to change before the feature is declared stable.

Contracts and decisions (change before stability)

- **`--check` needs the secrets to be present**, so it cannot run in a CI job on
  a machine that does not hold them, and it exits 1 for "a secret is missing"
  as for "the file is wrong". A structure-only mode, or a distinct exit code, is
  needed before anyone gates a pull request on it.
- **`server.allow_unauthenticated` with a non-loopback host** is accepted.
  Requiring loopback when unauthenticated is a tightening that is easier now than
  after files exist.
- **`routes.*.settings` is an open map**: a misspelled setting is accepted
  silently. Define the accepted keys.
- **`base_url`** accepts `http://:8000` and a port that is not a number, and is
  accepted on hosted kinds where it has no defined meaning. Parse it with a URL
  parser.
- **Upper bounds** are missing for `budget_window_secs` and `queue_timeout_ms`
  (the Redis ledger multiplies by 1000 and sets an expiry from them).
- **`deny_unknown_fields` and "additive within v1"** conflict during a rolling
  upgrade: a new field breaks the old binary. Say so in 0001, or tolerate unknown
  fields with a warning.
- **`#[non_exhaustive]`** on `SecretRef`, `Format` and `Issue`, which 0001 says
  the Rust types are.
- **Key names on usage records** (not only tenants), and **request ids** to join
  a request's attempts. The request id `gw-<pid>-<n>` repeats across containers,
  where the pid is 1: add an instance part.
- **Redis keys carry no namespace** (0006): two fleets that share a Redis and a
  provider name take each other's jobs. A `fleet.namespace` prefix.
- **Optional client certificates and revocation** (0011), a CRL file watched like
  the CA bundle.
- **Tokens**: the length floor of 16 characters is below the 128 bits that 0003
  advises; consider 32. A `sha256` caller key is compared with provider and admin
  keys by digest in `check_references`, but `build_key_store` alone does not.

Operability

- **The CI jobs run `serve` only.** The default, `proxy`, `gateway` and
  `gateway-redis` test suites run in the release gate, not on a pull request, so
  a shared-code regression is found at tag time. A pull-request job that runs
  `cargo test --features proxy --tests` and `--features gateway-redis` (with a
  Redis service) closes that.
- **Gemini thinking tokens are left out of the normalized completion count** on
  the native path (`usage::normalize_native_usage`), while the non-streaming
  response folds them in. The spend ledger, the cap and the usage records all use
  the smaller figure. It is a fix to shared accounting, so it belongs in its own
  change with its own tests.
- **The log writer** (`jsonl.rs`) drops and counts lines when its queue is full;
  a metric for the count (and for write failures) belongs with the
  OpenTelemetry item.
- **Shutdown**: a replica does not release leases it holds for peers' jobs on
  SIGTERM (as `gateway`); they wait out the lease timeout.
- **The Helm chart** has no `preStop` hook and a 60 s grace period that is
  shorter than a long stream; the admin Service name can exceed 63 characters;
  `appVersion` is not in the release checklist's lockstep.
- **The Docker check** runs on pull requests only and does not cover a
  `src/**` change or a moved base image tag; add a push and a weekly run.

Tests worth adding

- SIGTERM drains both listeners and exits 0; a taken main port exits 1;
  `allow_unauthenticated` serving; stderr scanned for secrets after requests.
- A fleet test that a replica which fails to bind leaves no lease on a peer's job
  (the window is not deterministic today).
- `Limits::new` takes three positional numbers (entries, bytes, ttl); a named
  struct removes the transposition risk. `build_limiter_with` and
  `build_breaker_with` are public in the `proxy` feature and could be
  `pub(crate)`.

## Open decisions

- **Licensing** of identity and audit-export features. Until this is decided,
  that code is not started in this repository.
- **Database** for the control plane (proposal: Postgres, SQLite for one node,
  never in the request path).
