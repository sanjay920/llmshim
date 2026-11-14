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
that use it (working rule 6).

## Interfaces and what each promises

| Interface | Contract | Status |
| --- | --- | --- |
| Rust library (`llmshim`) | semver; the protected files named in `CLAUDE.md` | stable |
| Proxy and gateway HTTP API | `api/openapi.yaml`; the bundled clients in lockstep | stable |
| `llmshim serve` configuration file | 0001; a JSON Schema checked in and compared by a test (planned) | unstable |
| Operational endpoints (`/metrics`, `/ready`, stats, `/v1/serve/config`) | 0005, 0012; described in the OpenAPI document (planned) | unstable |
| Access and usage logs | 0009, 0010; fields are added, never renamed or retyped | unstable |

A consumer pins to one of these. A change that cannot be made additively gets a
new `version` of the file or a new path, never a silent edit.

## When `serve` is declared stable

Every one of these, and then the semver rule applies to it:

1. The configuration schema is emitted by the binary, checked in, and compared by
   a test, so a change to it is a visible diff.
2. Every operational endpoint is in the OpenAPI document and a test holds the
   server to it.
3. The failure-mode table (0014) exists and every row has a test that
   deliberately breaks the dependency.
4. A benchmark with a stated method and a dated result is published.
5. Signed images and an SBOM are published.
6. The release gate builds and tests `serve`, and a release has gone out with it.

## The bar for each step

1. **Every dependency has a failure row.** What happens when it is down, slow or
   wrong, written down and tested. A step that adds or changes a dependency adds
   or changes the row.
2. **Identity and money fail closed; telemetry fails open.** A request whose
   caller cannot be identified, or whose spend cannot be accounted for, is
   refused. A failing log sink, metrics exporter or health probe never blocks or
   fails a request; it is counted and reported.
3. **Every rule has a test that fails without it.** For a security-relevant rule
   the test is checked by deleting the rule and watching the test fail.
4. **Every build, every pull request.** The default, `proxy`, `gateway`,
   `gateway-redis`, `serve`, `serve-redis` and cloud-transport builds are linted
   and tested on each pull request and on main (`ci.yml`, `serve-check.yml`),
   including the Redis-backed tests that are ignored without a server.
5. **Independent review before merge,** by a reviewer who did not write the
   change. Findings that hold up are fixed in the same pull request or written to
   "Review follow-ups" in the roadmap.
6. **No secret in any output, and no consumer identity in the repository.**

## Order of work

Harden what exists before adding surface. Reviews of the first stack found a real
defect in most pull requests; more surface on top of untested corners makes that
worse, not better.

1. CI coverage for every build, including the Redis-backed tests.
2. Gemini thinking-token accounting in the shared usage normalization.
3. The configuration-contract tightenings listed under "Review follow-ups", a
   structure-only `--check` with its own exit code, and `--print-schema`.
4. Outage-survival tests and the failure-mode table (0014).
5. OpenTelemetry traces and metrics, with the log writer's drop counters.
6. JWT/OIDC workload identity and IP allowlists (design note first).
7. Published, signed images and an SBOM.
8. Bedrock, Vertex and Azure from the file.
9. Reload of everything beyond keys.
10. The control plane (design note first).

## Decisions taken with this note

- **Control-plane database:** Postgres, with SQLite for a single node, and never
  in the request path. The data plane keeps serving from signed policy snapshots
  when the database is down.
- **Images:** GHCR, signed keyless with the workflow's OIDC identity, with an
  SBOM, from a publish job that does nothing until a repository variable enables
  it. A tag never publishes an image by accident.
- **Stability:** as above; until then `serve` stays unstable and default-off.
- **Licensing** of SSO, SCIM, audit export and the admin UI is not decided here.
  All of them follow the control plane, which does not exist yet, so nothing
  waits on it. They are not started: this repository is public under
  `MIT OR Apache-2.0`, published versions stay under those terms, and the
  maintainer decides the repository's visibility and the license of future
  versions before any of that code is committed.
