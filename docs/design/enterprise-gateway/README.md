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

## Roadmap

Phase 0: a production server mode

- [x] `serve` feature flag and the versioned configuration contract, with
      `llmshim serve --config <file> --check` (0001)
- [ ] Environment-free constructors for the proxy and gateway internals
      (`from_env()` becomes `from_parts(Parts::from_env())`, behavior pinned by
      existing tests); config-versus-environment precedence written down
- [ ] `llmshim serve` boots the gateway backend from the file, including
      hashed caller keys
- [ ] Bedrock, Vertex and Azure configurable from the file; a provider whose
      feature is not compiled is a startup error naming the rebuild flag
- [ ] Configuration reload without restart
- [ ] OpenTelemetry traces and metrics
- [ ] Helm chart, signed images, SBOM
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
