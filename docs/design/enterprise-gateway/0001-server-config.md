# 0001: Server configuration file

Status: implemented (parsing, validation, `--check`). Serving from the file is
not implemented yet.

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
| A caller key may be given as `sha256` instead of a secret reference. | The deployment then holds no caller secret. |
| With no `[[keys]]`, the file is invalid unless `server.allow_unauthenticated = true`. Setting both is also invalid. | Unauthenticated serving is a deliberate choice, never a default. |
| `server.host` defaults to `127.0.0.1`. | Listening on every interface is opt-in. |
| A provider's table name is its routing prefix. `route` is reserved. | Matches the existing `provider/model` grammar and named routes. |
| `base_url` must be `http(s)://host[/path]` with no credentials, query or fragment. | Redirects are disabled in the client, so this must be the final endpoint; credentials belong in `api_key`. |
| Routes carry their defaults in an explicit `settings` table and may not point at another route. | Keeps unknown-field checking on the route itself; routes do not chain. |
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

Within version 1, changes are additive: new optional fields and new `kind`
values. Removing or redefining a field needs version 2. The Rust types are
`#[non_exhaustive]`.
