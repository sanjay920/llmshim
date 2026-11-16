# 0020: Bedrock, Vertex and Azure from the file

Status: design only. A contract note, written before any code (README working
rule 3). Nothing here is implemented; the slices are listed at the end.

## What an operator needs

The three cloud transports (`docs/bedrock.md`, `docs/vertex.md`, `docs/azure.md`)
exist as library types a host registers in code. `llmshim serve` is configured by a
file and has no host code, so a deployment on AWS, Google Cloud or Azure cannot use
them. This note puts them in the file, with the same rules the rest of the file
follows: secrets by reference, no ambient credentials, unknown fields refused, a
build without the feature says which flag to rebuild with.

## The contract

```toml
[providers.bedrock]
kind = "bedrock"
profile = "prod"                  # an AWS SDK profile name, never an exported key
region = "us-east-1"

[providers.vertex-gemini]         # and/or vertex-anthropic
kind = "vertex-gemini"
project = "my-project"
region = "us-central1"
binding = "vertex-prod"           # a stable, non-secret name for this identity
service_account_file = "/etc/llmshim/vertex-sa.json"

[providers.azure]
kind = "azure"
endpoint = "https://example.openai.azure.com"
binding = "azure-prod"
api_key = { env = "AZURE_OPENAI_KEY" }
deployments = { "chat-production" = "gpt-6-sol" }
```

Models are addressed as the library already addresses them: `bedrock/<model-id>`,
`vertex-gemini/<model>`, `vertex-anthropic/<model>`, `azure/<deployment>`. Routes
(`route/<name>`), per-provider limits (`[limits.providers.<name>]`) and the usage
log work as for any provider, keyed by the provider name.

## Rules

- **The provider name is the routing prefix, one provider per kind.** The table key
  must be `bedrock`, `vertex-gemini`, `vertex-anthropic` or `azure` for its kind,
  because the transports route by that prefix and the catalog, limits and prices
  are looked up by it. A second Bedrock region or Azure resource is a second
  deployment of `serve`, not a second table; this is a limit of the transports,
  stated here so it is not mistaken for an oversight.
- **Each kind takes only its own fields**, and a field of another kind is refused
  by path (`providers.azure.profile: applies to kind "bedrock" only`).
  Required: Bedrock `profile` and `region`; Vertex `project`, `region`, `binding`
  and `service_account_file`; Azure `endpoint`, `binding` and `api_key`.
  `deployments` (deployment name to the catalog model it serves) is optional for
  Azure, and the library refuses an unknown deployment locally.
- **No ambient credentials, as the transports already promise.** Bedrock names an
  SDK profile and never falls through to exported keys, a default profile,
  container credentials or instance metadata. Vertex reads one service-account
  file and never application-default credentials. Azure uses a key by reference.
  Entra tokens need a token source, which a file cannot describe; they stay a
  library-only identity for now (the file refuses an `entra` field by name).
- **Secrets stay references.** `api_key` is `{ env }` or `{ file }`.
  `service_account_file` is an absolute path whose content is a credential, so it
  is treated as a secret file: `--check` reports it missing or empty (exit 3, the
  same as an unavailable secret) and never prints it. A Bedrock profile is a name,
  not a secret: its existence and its keys are the AWS SDK's to resolve on first
  use, and `--check` does not read the AWS files.
- **`binding` is part of the replay boundary** (reasoning signatures and receipts
  are bound to it, as in the library docs). It must be stable across restarts and
  replicas and change when the identity changes; it is 1 to 64 characters of the
  name alphabet, and two providers may not share one.
- **Validation is by path and offline.** `region` is the transports' own rule
  (lower-case letters, digits and hyphens); `endpoint` is the Azure transport's own
  rule (an https URL without credentials, as its constructor enforces); `project`
  and `profile` are bounded visible ASCII. Nothing in `--check` touches a cloud.
- **A build without the feature refuses, naming it.** A new default-off Cargo
  feature `serve-cloud` (`= ["serve", "bedrock", "vertex", "azure"]`) builds the
  three transports into `serve`. A file that sets one of these kinds on a binary
  built without it fails at start and at `--check` with
  `needs a binary built with --features serve-cloud`, as `[telemetry]` does for
  `serve-otel`. The default, `proxy`, `gateway` and `gateway-redis` builds do not
  change.
- **Fixed at start.** Providers are outside the keys, so a reload that changes one
  is refused by path (0012).
- **Failure behaviour is the transports'.** Credential resolution happens before
  admission, so a missing profile, an unreadable service account or an expired
  token fails the request (503 and retried when transient, stopped when the
  configuration is wrong) and reserves neither budget nor rate capacity, and the
  error names the profile or binding, never the SDK text or a path. That is
  already documented per transport; this note adds the row to 0014 and a test that
  a configured but unresolvable identity fails a request without touching the
  budget.

## What is not in the published image yet

`serve-cloud` adds the AWS SDK and the Google and Azure paths to the binary. The
published image (0019) keeps `serve-redis,serve-otel` until the size and build-time
cost is measured and the owner decides; a deployment that needs these providers
builds the image with `--build-arg FEATURES=serve-redis,serve-otel,serve-cloud`,
and `docker-check.yml` builds that combination on every pull request that touches
the Dockerfile, so it does not rot.

## Not decided here

- Entra (managed identity, workload identity federation) for Azure, and token
  sources for Vertex other than a service-account file. Each needs a way to
  describe a token source in a file that does not run arbitrary commands.
- More than one instance of a kind in one file.
- Catalog rows or prices for cloud-hosted models beyond what the catalog already
  carries for the exact `vertex-*/<model>` and Azure rows.

## Slices

1. This note.
2. The feature, the config types and their validation, the schema, and the feature
   refusal. Tests: validation by path, one kind's field on another kind, the
   missing-feature refusal, secret-file checks at `--check` (exit 3), the example
   file.
3. Building the providers into the router from the file, and the process tests: a
   mock Azure endpoint end to end (key header, deployment mapping, an unknown
   deployment refused locally, an unknown field refused), and Bedrock and Vertex
   starting from a file and failing a request cleanly when their identity cannot be
   resolved, with no budget reserved.
4. `serve-check.yml` builds and tests `serve-cloud`; `docker-check.yml` builds the
   image with it; the 0014 row; the docs.
