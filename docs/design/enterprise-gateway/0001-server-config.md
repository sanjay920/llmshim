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
| With no `[[keys]]`, the file is invalid unless `server.allow_unauthenticated = true` (which needs a loopback `server.host`, below). Setting both is also invalid. | Unauthenticated serving is a deliberate choice, never a default. |
| `server.allow_unauthenticated = true` needs a loopback `server.host` (`127.0.0.0/8` or `::1`). | With no caller identity on a routable interface, anyone who can reach the port spends the upstream keys. A deployment that listens on a routable address defines `[[keys]]`. |
| `server.host` defaults to `127.0.0.1`. | Listening on every interface is opt-in. |
| A provider's table name is its routing prefix. `route` is reserved. | Matches the existing `provider/model` grammar and named routes. |
| A hosted provider (`openai`, `anthropic`, `gemini`, `xai`, `cerebras`, `openrouter`) must be named for its kind, once. A self-hosted `openai-compatible` server may not use a hosted name. | Prices, limits and the catalog are looked up by provider name. Multiple credentials per provider is a later feature. |
| `server.tls` and `admin.tls` take absolute PEM file paths (never inline key material) and a reload interval of 1 to 86400 seconds. | Keys stay in files that a certificate manager can renew (0007). |
| `logging.access_log` and `logging.usage_log` are `"stdout"` or an absolute file path. | A relative path would depend on the working directory (0009). |
| `client_san` is `dns:<name>` or `uri:<uri>`, needs `server.tls.client_ca_file`, and excludes `secret`/`sha256`; `client_ca_file` is refused on `admin.tls`. | A certificate key could never match unless client certificates are required (0011). |
| `[admin]` needs a port that differs from `server.port` on the same address; its token is a separate secret from every provider and caller key. | The admin listener must not share a credential with traffic (0005). |
| `state.receipts_dir` is absolute; `state.idempotency_ttl_secs` is 1 to 604800. | Operators mount a volume for receipts; the bound keeps a typo from holding answers for years. |
| `base_url` must be `http(s)://host[/path]`, parsed as a URL (a host, and a port that is a number in range), with no credentials, query or fragment. | Redirects are disabled in the client, so this must be the final endpoint; credentials belong in `api_key`. |
| Routes carry their defaults in an explicit `settings` table, name their model as `<provider>/<model>` with a defined provider, and may not point at another route. | Keeps unknown-field checking on the route itself, and a route that could never resolve fails at `--check`, not at the first request. |
| Budget bounds match the gateway ledger (0 to 9007199.254740992 USD, finite). | One definition of a representable cap. |
| `budget_window_secs` is 1 to 31622400 (366 days); `limits.queue_timeout_ms` is at most 3600000 (one hour). | The Redis ledger multiplies the window by 1000 and sets an expiry from it, and an expiry beyond Redis's range makes the script error and admission refuse; a budget that never resets is not a window. A queue wait of hours is a mis-set unit, not a policy. |

### Errors

Semantic problems are collected and reported together, each with the dotted path
of the setting (`keys[2].budget_usd`). A syntax or shape error from the parser
is reported alone, since nothing after it can be trusted.

`--check` also resolves every secret reference and reports any that is unset,
empty or unreadable, naming the setting and the variable or file. It never
prints a value.

### Checking a file

| Command | Reads | Answers |
| --- | --- | --- |
| `serve --config F --check --structure-only` | the file only | is the shape right, and is every rule that needs no secret met |
| `serve --config F --check` | the file, each referenced secret, the TLS files | the above, plus every secret is available and meets the token rules, and the TLS material loads |
| `serve --print-schema` | nothing | prints the JSON Schema of the file |

`--structure-only` exists so a pipeline that holds no secret (a pull-request job,
a generator's test) can still gate on the file. It reads no environment variable,
no secret file and no certificate, and it says what it did not check. It still
reports what this *binary* cannot serve (the `[fleet]` table in a build without
`serve-redis`, say), because that is a fact about the binary a pipeline checking
with the production binary wants to know.

Exit status, for `--check` and for starting the server alike:

| Status | Meaning |
| --- | --- |
| `0` | valid (or the server ran and stopped cleanly) |
| `1` | the file is wrong, a secret's *value* breaks a rule (too short, equal to an upstream key), or the server could not start |
| `2` | bad arguments |
| `3` | a referenced secret is unset, blank or unreadable: this environment cannot check the file. A missing certificate or key file is status `1`, not `3`: a pipeline given neither uses `--structure-only` |

A pipeline can therefore treat `1` as a failure and `3` as "not provisioned
here". When both apply the status is `1` and both are reported: a defect outranks
an absence.

### The schema

`llmshim serve --print-schema` prints the file's JSON Schema (draft 2020-12),
derived with `schemars` from the same types the parser reads, so the two cannot
drift. A copy is checked in as `serve-config.schema.json` and a test compares it
with the binary's output: a change to the contract is a visible diff, and the
pull request that makes it says why a consumer is unaffected. Regenerate it with
`llmshim serve --print-schema > docs/design/enterprise-gateway/serve-config.schema.json`.

The schema describes shape: field names, which are required, types and each
type's own range, closed objects, the single version, and a secret as exactly one
of `env` or `file`. A file the schema refuses always fails the parser, and a test
holds the two to that; a file the schema accepts can still fail `--check`,
because the rules that compare values, read the environment or need two fields
together are not a schema's to state. A consumer can validate its generated files
against the schema in an editor or a pipeline with no llmshim binary at all. A
TOML file can name the schema for an editor with a `#:schema` directive comment
at the top; a JSON file cannot carry a `$schema` key, because unknown fields are
errors.

### What is deliberately not here yet

- **Precedence against environment variables.** Decided when `serve` boots from
  the file. The intent is that `serve` reads nothing from the environment except
  what the file names.
- **Cloud transports** (Bedrock, Vertex, Azure). New `kind` values, added with
  their identity settings.
- **Scheduler tuning** (aging, queue depth, leases, Redis). Added as the
  environment-free constructors land.
- **Reload.** The parsed value is immutable; `[[keys]]` reload without a restart
  (0012) and everything else is fixed at start.

## Compatibility

The `serve` feature is unstable: until it is declared stable, the file format,
the command and the Rust types may change in any release and are outside the
crate's semver promise. Once stable, within version 1, changes are additive: new optional fields and new `kind`
values. Removing or redefining a field needs version 2. The Rust types
(`ServeConfig` and every table in it, `Issue`, `Format` and `SecretRef`) are
`#[non_exhaustive]`.

**Additive does not mean an older binary accepts the file.** Unknown fields are
errors, so a binary that predates a new field refuses a file that sets it, and
`--check` with the old binary says which field. A running replica's reload
(0012) behaves the same way: it rejects the whole new file, so a revocation
edited into the same file as a new field is not applied on a replica that cannot
read the field. A rolling upgrade therefore replaces the binaries first and edits
the file after; during the roll, replicas
that disagree about the file must not be sent a file only some can read (0006,
"all replicas must agree"). That is the price of a typo never becoming an ignored
setting, and it is deliberate: the alternative, tolerating unknown fields with a
warning, would let a misspelled `rpm` serve with no limit.

Two more consequences of the same rule. Tightening a rule within version 1 turns
files that passed into files that fail, so once the feature is stable that needs
version 2 as well; this is the time to tighten. And `base_url` on a hosted kind is
intended, not an oversight: it is how a gateway or test upstream is put in front
of a hosted provider.
