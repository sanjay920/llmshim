# Security Policy

llmshim forwards requests to third-party LLM providers and, in proxy mode,
holds provider API keys, so we take reports seriously.

## Reporting a vulnerability

**Please do not open a public issue.** Use GitHub's private vulnerability
reporting: [Security → Report a vulnerability](https://github.com/sanjay920/llmshim/security/advisories/new)
on this repository. We will acknowledge reports on a best-effort basis and
coordinate a fix and disclosure with you.

## Threat model (what counts as a vulnerability)

llmshim is a translation layer, not a security boundary. Its posture:

- **An operator-started proxy ships with no authentication and no TLS**, and is
  intended to run on a trusted network behind your own gateway. Exposing it
  directly to the public internet is an operator mistake, not a vulnerability.
  The Python and TypeScript packages use a separate internal managed mode with
  per-launch authentication and certificate pinning.
- Provider API keys live in the proxy process environment (or `~/.llmshim/`) and
  are never returned to clients. A defect that **leaks a configured key** — into
  a response body, a log line, an error message, or another request's context —
  is a vulnerability. Please report it.
- So is anything that lets a request reach an unintended destination
  (server-side request forgery beyond the configured providers), crashes the
  **proxy process** rather than failing a single request, achieves code
  execution, or lets one request read another's data.
- A translation that merely produces wrong, incomplete, or provider-rejected
  output is a bug, not a vulnerability — an ordinary public issue is perfect for
  those.

## Outbound network activity

llmshim contacts one host you did not configure.

- **`models.dev` on startup.** `Router::from_env` kicks off a model-catalog fetch so that
  context windows, capability flags and pricing are current rather than compiled in. The request
  carries no prompt content, no API keys, and no identifying information — it is an unauthenticated
  GET of a public catalog — but it is an outbound connection, and you should know about it before
  deploying into a restricted network.
- **Disable it with `LLMSHIM_CATALOG_OFFLINE=1`.** Offline mode makes the catalog use its
  vendored snapshot and performs no catalog or provider-discovery HTTP at all. A cached
  catalog on disk is also ignored in this mode.
- **Everything else is a provider call you asked for.** Requests go to the provider implied by
  the model you addressed, at its documented endpoint or a base URL you set yourself.

llmshim sends no telemetry, analytics, crash reports, or usage statistics anywhere.

## Supported versions

The latest release and `main`. We do not backport fixes to older releases.

## Hardening guidance for operators

Run the proxy behind your own authentication, TLS termination, and rate limits;
do not expose it directly. Keep provider keys in the server's environment or
config file, never in client code. See the deployment documentation for the
concurrency, rate-limit, and backpressure controls.

Browser requests that include an `Origin` header are rejected before dispatch
unless the operator explicitly allows that exact origin with
`LLMSHIM_TRUSTED_ORIGINS`. This protects a credential-backed local or private
proxy from an unrelated website a user visits. It does not authenticate the
proxy or restrict non-browser clients, so keep the network and authentication
boundary described above.

## No warranty

llmshim is provided "AS IS", without warranty of any kind. Operating it is at
your own risk; see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE)
(§7–§8) and [NOTICE](NOTICE).
