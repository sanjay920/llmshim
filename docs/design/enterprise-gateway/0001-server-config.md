# 0001: Server configuration file

Status: implemented (parsing, validation, `--check`). Serving from the file is
described in 0004.

## Problem

`llmshim proxy` and `llmshim gateway` are configured by about sixty environment
variables, a keys JSON file and `~/.llmshim/config.toml`. That works for one
operator. It does not survive a change-review process: there is no single
artifact to diff, no validation before rollout, and a misspelled variable is
silently ignored.

## Decision

`llmshim serve --config <path>` reads one versioned file. `--check` validates it
and exits, so a pipeline can gate a rollout on it.

The file is a new type (`llmshim::serve::ServeConfig`), not an extension of
`llmshim::config::Config`. That type is semver-protected, describes a personal
CLI setup, and its file is rewritten by `llmshim set`.

### Format

TOML or JSON, chosen by file extension. Both are already dependencies. JSON is
there so templating tools can generate the file; YAML is not offered because it
would add a parser dependency and its implicit typing causes exactly the silent
surprises this file exists to prevent.

See [example.toml](example.toml). A unit test parses that file, so it cannot
drift from the code.

### Rules

| Rule | Reason |
|---|---|
| `version` is required and must equal 1. Another version is refused before anything else is checked. | A future version may change the meaning of other fields. |
| Unknown fields are errors at every level. | A typo must not become an ignored setting. |
| Secrets are references: `{ env = "NAME" }` or `{ file = "/absolute/path" }`. An inline string is refused, and the error does not quote it. | The file can be committed, reviewed and logged. |
| `env` names use upper-case letters, digits and `_`. | A token pasted there by mistake is refused instead of being printed as a "name". |
| A caller key may be given as `sha256` instead of a secret reference. | The deployment then holds no caller secret. |
| A caller key may not reference, or resolve to the same value as, a provider's `api_key` or another caller key. | Otherwise an upstream key would be a valid bearer token. The value check runs in `--check`. |
| `tenant` uses 1 to 64 letters, digits, `-`, `_` or `.`. | Tenants become parts of ledger keys and metric labels. |
| A repeated table name is an error in JSON too. | JSON parsers otherwise keep the last one silently. |
| With no `[[keys]]`, the file is invalid unless `server.allow_unauthenticated = true`. Setting both is also invalid. | Unauthenticated serving is a deliberate choice, never a default. |
| `server.host` defaults to `127.0.0.1`. | Listening on every interface is opt-in. |
| A provider's table name is its routing prefix. `route` is reserved. | Matches the existing `provider/model` grammar and named routes. |
| A hosted provider (`openai`, `anthropic`, `gemini`, `xai`, `cerebras`, `openrouter`) must be named for its kind, once. A self-hosted `openai-compatible` server may not use a hosted name. | Prices, limits and the catalog are looked up by provider name. Multiple credentials per provider is a later feature. |
| `server.tls` and `admin.tls` take absolute PEM file paths (never inline key material) and a reload interval of 1 to 86400 seconds. | Keys stay in files that a certificate manager can renew (0007). |
| `logging.access_log` is `"stdout"` or an absolute file path. | A relative path would depend on the working directory (0009). |
| `[admin]` needs a port that differs from `server.port` on the same address; its token is a separate secret from every provider and caller key. | The admin listener must not share a credential with traffic (0005). |
| `state.receipts_dir` is absolute; `state.idempotency_ttl_secs` is 1 to 604800. | Operators mount a volume for receipts; the bound keeps a typo from holding answers for years. |
| `base_url` must be `http(s)://host[/path]` with no credentials, query or fragment. | Redirects are disabled in the client, so this must be the final endpoint; credentials belong in `api_key`. |
| Routes carry their defaults in an explicit `settings` table, name their model as `<provider>/<model>` with a defined provider, and may not point at another route. | Keeps unknown-field checking on the route itself, and a route that could never resolve fails at `--check`, not at the first request. |
| Budget bounds match the gateway ledger (0 to 9007199.254740992 USD, finite). | One definition of a representable cap. |

### Errors

Semantic problems are collected and reported together, each with the dotted path
of the setting (`keys[2].budget_usd`). A syntax or shape error from the parser
is reported alone, since nothing after it can be trusted.

`--check` also resolves every secret reference and reports any that is unset,
empty or unreadable, naming the setting and the variable or file. It never
prints a value.

Exit codes: `0` valid, `1` invalid, `2` usage error or (for now) a valid file
without `--check`.

### What is deliberately not here yet

- **Precedence against environment variables.** Decided when `serve` boots from
  the file. The intent is that `serve` reads nothing from the environment except
  what the file names.
- **Cloud transports** (Bedrock, Vertex, Azure). New `kind` values, added with
  their identity settings.
- **Scheduler tuning** (aging, queue depth, leases, Redis). Added as the
  environment-free constructors land.
- **Reload.** The parsed value is immutable so it can be swapped later.

## Compatibility

The `serve` feature is unstable: until it is declared stable, the file format,
the command and the Rust types may change in any release and are outside the
crate's semver promise. Once stable, within version 1, changes are additive: new optional fields and new `kind`
values. Removing or redefining a field needs version 2. The Rust types are
`#[non_exhaustive]`.
