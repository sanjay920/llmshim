# 0013: llmshim as a component

Status: accepted 2026-11-14. Decisions and a bar; no code.

## Why this note exists

llmshim is not only a program someone starts by hand. It is embedded as a
library, run beside an application, and run as a shared gateway that unattended
callers (CI jobs, agents, other services) use with nobody watching. Those
callers do not read logs and cannot ask what a message meant. They depend on
three things:

- a **contract** that does not move silently;
- **behavior under failure** that is written down and tested;
- **attribution** precise enough to join a request to the work that caused it.

This note fixes the bar so each roadmap step can be held to it, and says in what
order to meet it. It names no consumer: llmshim learns nothing about the systems
that use it (working rule 6, and the rule in `CLAUDE.md` that public documents
carry no private consumer identities).

## Interfaces and what each promises

| Interface | Contract | Status |
| --- | --- | --- |
| Rust library (`llmshim`) | semver (0.x: a breaking change needs a minor bump); the protected files named in `CLAUDE.md` | stable |
| Proxy HTTP API | `api/openapi.yaml`; the bundled clients in lockstep | stable |
| Gateway HTTP API | the proxy contract plus the gateway's headers and endpoints, in the same document, marked experimental there | experimental |
| `llmshim serve` configuration file | 0001; a JSON Schema checked in and compared by a test (planned) | unstable |
| Operational endpoints (`/ready`, `/metrics`, stats, `/v1/serve/config`; `/health` is part of the proxy contract) | 0005, 0012; to be described in the OpenAPI document (planned) | unstable |
| Access and usage logs | 0009, 0010 | unstable |

**Stable:** a breaking change needs a version bump (a minor release while the
crate is 0.x). **Experimental:** may change in a release, and the change is
called out. **Unstable:** may change in any release.

Once a row is declared stable, a change that cannot be made additively gets a
new `version` of the file or a new path, and log fields are added, never renamed
or retyped. Until then an unstable row may change in any release (0001), and the
roadmap lists where "additive" is not yet safe: an older binary rejects a field
it does not know.

## When `serve` is declared stable

The maintainer declares it, as a commit to this note, when all of these hold.
From then on the semver rule applies to `serve`, and the configuration file, the
operational endpoints and the access and usage logs become stable with it.

1. Every item under "Contracts and decisions" in the roadmap's review follow-ups
   is done or has a written decision.
2. The configuration schema is emitted by the binary (`--print-schema`), checked
   in, and compared by a test, so a change to it is a visible diff.
3. Every operational endpoint is in the OpenAPI document and a conformance test
   holds the server to it.
4. A request's tenant, key name, credential fingerprint and a request id that is
   unique across instances appear in the access log, the usage record and the
   trace, so one request can be joined across all three.
5. The failure-mode table (0014) exists and every row has a test that
   deliberately breaks the dependency.
6. A benchmark is published with its method, hardware, build and date, reporting
   added latency (p50 and p99), throughput and memory against a mock upstream.
7. Images are signed (verifiable with `cosign verify`) and carry an SPDX SBOM.
8. The release gate builds and tests `serve`, and refuses a tag whose commit
   lacks a green `ci.yml`; and branch protection requires `ci.yml` and
   `serve-check.yml` on main.

## The bar for each step

1. **Every dependency has a failure row.** What happens when it is down, slow or
   wrong, written down and tested. A step that adds or changes a dependency adds
   or changes the row.
2. **On the inference path, identity and money fail closed; telemetry fails
   open.** Once serving, a request whose caller cannot be identified, or whose
   spend cannot be reserved against a cap, is refused (0006 for a Redis outage).
   A failing log sink or metrics exporter never blocks or fails a request; it is
   counted and reported. At startup the opposite holds: a log path that cannot
   be written refuses to start (0009). A configuration change that cannot be
   applied leaves the running one in force and says so (0012). The deliberate
   exceptions are configuration, not accident, and are named where they are
   granted: unauthenticated serving (0001); `/health` and `/ready`, which stay
   open under an admin token so orchestrators can probe, and every operational
   endpoint when no `[admin]` table is configured (0005); and a key without a
   budget or with `budget_allow_unpriced` (`docs/src/proxy/scaling.md`).
3. **Every rule gets a test that fails without it.** For a security-relevant rule
   the test is checked by deleting the rule and watching it fail. A rule that has
   no test yet is listed under "Tests worth adding" in the roadmap until it has
   one.
4. **Every build, on every pull request and on main.** The default, `proxy`,
   `gateway`, `gateway-redis`, `serve`, `serve-redis` and cloud-transport
   (`bedrock`, `vertex`, `azure`) builds are linted and tested, including the
   Redis-backed tests that are ignored without a server. `ci.yml` and
   `serve-check.yml` do this; a new feature gets a job in the change that adds it.
5. **Review by someone who did not write the change,** who sees the diff and the
   repository and not the author's reasoning, before it merges. Findings that
   hold up are fixed in the same pull request or written to "Review follow-ups".
6. **No credential value in any output** (a short fingerprint of a token is not
   the token; 0009), **and no consumer identity in the repository.**

## Order of work

Harden what exists before adding surface. Reviews of the first stack found real
defects before merge: a secret echoed by a parse error, an admin listener that
served without a token, fleet workers leasing jobs before the listeners had
bound, TLS session resumption that outlived a certificate. More surface on top
of untested corners makes that worse, not better.

1. CI coverage for every build, including the Redis-backed tests (`ci.yml`, and
   every `serve` test in `serve-check.yml`). Done.
2. Gemini thinking-token accounting in the shared usage normalization. Done.
3. The configuration-contract tightenings under "Review follow-ups", the
   remaining inventory rows of 0002, a structure-only `--check` with its own
   exit code, `--print-schema`, and the operational endpoints in the OpenAPI
   document with a conformance test.
4. A request id that is unique across instances, and the tenant, key name and
   credential fingerprint beside it in the access log and the usage records.
5. Outage-survival tests and the failure-mode table (0014). Started: the table
   and its first tests exist; its "Not yet tested" list is what remains.
6. OpenTelemetry traces and metrics, with the log writer's drop counters.
7. JWT/OIDC workload identity and IP allowlists (design note first).
8. A reproducible benchmark.
9. Published, signed images and an SBOM.
10. The release gate builds and tests `serve` and waits for a green `ci.yml`.
11. Bedrock, Vertex and Azure from the file.
12. Reload of everything beyond keys.
13. The control plane (design note first).

## Decisions taken with this note

- **Control-plane database: Postgres, with SQLite for a single node, and never
  in the request path.** Key lifecycle, budgets and audit need transactions and
  queries that a file or Redis does not give; SQLite keeps a one-node deployment
  free of infrastructure; and with the database off the request path the data
  plane will keep serving from signed policy snapshots when it is down.
- **Images: GHCR, signed keyless with the workflow's OIDC identity, with an SPDX
  SBOM, from a publish job that will do nothing until a repository variable
  enables it.** The registry sits beside the repository and its identity;
  keyless signing leaves no long-lived signing key to guard or leak; and the
  opt-in means a tag never publishes an image by accident.
- **The proxy HTTP API is stable:** `api/openapi.yaml` is its contract and the
  bundled clients move with it. The gateway's additions to it are experimental.
- **Stability:** as above. Until then `serve` stays unstable; afterwards it stays
  behind its feature (working rule 1).
- **Licensing** of SSO, SCIM, audit export and the admin UI is undecided, and
  none of it is started. All of them follow the control plane, which does not
  exist yet, so nothing in the order of work waits on it.
