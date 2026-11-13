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
   Configuration is an immutable value that can later be swapped for hot reload
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
- [ ] Configuration reload without restart
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
- [ ] Workload JWT authentication, mTLS, IP allowlists
- [ ] Audit log
- [ ] Guardrail pipeline on the existing `AttemptPolicy` hooks; residency policy
- [ ] Secret-manager integrations
- [ ] Config-registered providers; embeddings, images and audio on the server
- [ ] Admin UI, SSO and SCIM (blocked on the licensing decision)

## Open decisions

- **Licensing** of identity and audit-export features. Until this is decided,
  that code is not started in this repository.
- **Database** for the control plane (proposal: Postgres, SQLite for one node,
  never in the request path).
