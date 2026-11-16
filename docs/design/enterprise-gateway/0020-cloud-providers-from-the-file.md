# 0020: Bedrock, Vertex and Azure from the file

Status: implemented. The configuration and its validation (slice 2), the providers
built into the router from the file, with Azure's deployments listed in
`/v1/models` through a new defaulted `Provider::served_models` (slice 3), and the
CI that keeps it building (slice 4: a `serve-cloud-check` job that runs every test
in the combined build, an image build with the cloud transports on each Dockerfile
pull request, and the 0014 row). A build without `serve-cloud` refuses such a
file. A contract note, written before any code (README working rule 3).

## What an operator needs

The three cloud transports (`docs/bedrock.md`, `docs/vertex.md`, `docs/azure.md`)
exist as library types a host registers in code. `llmshim serve` is configured by a
file and has no host code, so a deployment on AWS, Google Cloud or Azure cannot use
them. This note puts them in the file, with the rules the rest of the file follows:
secrets by reference, unknown fields refused, a build without the feature says
which flag to rebuild with. It is honest about the one place those transports
differ from the rest: a Bedrock profile is a file the operator controls and may
select more than a static key (see "Credentials").

## The contract

```toml
[providers.bedrock]
kind = "bedrock"
profile = "prod"                  # an AWS SDK profile name
region = "us-east-1"
config_file = "/etc/llmshim/aws/config"            # optional, with credentials_file
credentials_file = "/etc/llmshim/aws/credentials"  # optional

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
deployments = { "chat-production" = "gpt-6-sol" }   # required, at least one
```

Models are addressed as the library already addresses them: `bedrock/<model-id>`,
`vertex-gemini/<model>`, `vertex-anthropic/<model>`, `azure/<deployment>`. Routes
(`route/<name>`), per-provider limits (`[limits.providers.<name>]`) and the usage
log work as for any provider, keyed by the provider name.

## Rules

- **The provider name is the routing prefix, one provider per kind.** The table key
  must be `bedrock`, `vertex-gemini`, `vertex-anthropic` or `azure` for its kind,
  because the transports route by that prefix and the catalog, limits and prices
  are looked up by it. The four names are reserved: `kind = "openai-compatible"`
  under one of them is refused (they join the hosted names). A second Bedrock
  region or Azure resource is a second deployment of `serve`, not a second table;
  that is a limit of the transports, stated so it is not mistaken for an oversight.
- **Each kind takes only its own fields**, and a field of another kind is refused
  by path (`providers.azure.profile: applies to kind "bedrock" only`). The new
  kinds do not take an `api_key` unless they are Azure (the existing rule that
  every hosted kind needs one is for hosted kinds only). Required: Bedrock
  `profile` and `region`; Vertex `project`, `region`, `binding` and
  `service_account_file`; Azure `endpoint`, `binding`, `api_key` and a non-empty
  `deployments`.
- **Validation calls the transports' own checks.** Their constructors do not
  validate (`AwsProfile::new`, `GoogleIdentity::service_account` cannot fail); the
  rules live in private functions that build the endpoints, so they now live in one
  module (`providers::cloud_rules`, with no dependency on a transport feature) that
  the transports **and** the file both call: the file cannot disagree with a
  transport because they are the same function. So a value that would fail every
  request fails `--check` instead:
  - Bedrock `region`: lower-case letters, digits and hyphens, and a partition the
    transport supports (`aws`, `aws-us-gov`, `aws-cn`); the isolated partitions are
    refused. `profile` is bounded visible ASCII.
  - Vertex `project`: lower-case letters, digits and hyphens, at most 128;
    `region`: the transport's own set, including `global`, `us` and `eu`.
  - Azure `endpoint`: https, no credentials, as `Azure::new` enforces;
    `deployments`: each name 1 to 256 characters of `[A-Za-z0-9_.-]` and a
    non-empty model. An Azure provider with no deployment could serve nothing (the
    transport refuses every unconfigured model locally), so an empty or missing
    `deployments` is refused.
  - `binding`: 1 to 64 characters of the name alphabet. The two Vertex kinds share
    one identity and one token cache, so when both tables exist their `project`,
    `region`, `binding` and `service_account_file` must be equal (refused by path
    otherwise); no other binding must be unique.
- **`/v1/models`** lists `azure/<deployment>` for each configured deployment
  (requestable as they are), and nothing for Bedrock and Vertex, whose catalogs are
  not enumerated (as for OpenRouter).
- **A build without the feature refuses, naming it.** A new default-off Cargo
  feature `serve-cloud` (`= ["serve", "bedrock", "vertex", "azure"]`) builds the
  three transports into `serve`. A file that sets one of these kinds on a binary
  built without it fails at start and at `--check` with
  `needs a binary built with --features serve-cloud`, as `[telemetry]` does for
  `serve-otel`. The default, `proxy`, `gateway` and `gateway-redis` builds do not
  change (feature unification was checked: the combination adds no second crypto
  provider and the transports have no hooks in shared code).
- **Fixed at start.** Providers are outside the keys, so a reload that changes one
  is refused by path (0012). An Azure key, like every provider secret, is read at
  start; rotating it needs a restart.
- **A provider that cannot be built is an error, not a skipped provider.** A key
  the Azure identity refuses (interior whitespace, say) is reported by path at start,
  where today a provider with no key is silently left out.

## Credentials

- **Azure** uses a key by reference. Entra tokens need a token source, which a file
  cannot describe, so they stay library-only for now (the file refuses an `entra`
  field by name).
- **Vertex** reads one service-account file, never application-default
  credentials. The file is a credential, so it is treated as a secret file:
  `--check` reads and **parses it with the transport's own rules** (type
  `service_account`, the Google token endpoint, a usable private key) and reports a
  missing, empty or malformed file by path (exit 3 for a missing or empty file, 1
  for a malformed one), never printing its content. Resolution is lazy: the
  transport reads it on the first token, and `/ready` does not reflect identity
  health, so a rotated-away or revoked account shows as failing requests, not as an
  unready replica.
- **Bedrock names an SDK profile, and the profile file is trusted operator input.**
  The transport never falls through to exported keys, a default profile, container
  credentials or instance metadata *when the named profile is missing*; but a
  profile may itself declare what it selects: a role to assume, SSO, a
  `credential_source` (environment, ECS, EC2) or a `credential_process` (a command
  the AWS SDK runs). That is the AWS profile format, and `llmshim` does not
  restrict it: whoever can write the profile file decides what identity Bedrock
  runs as. `config_file` and `credentials_file` (both or neither) name the profile
  files and replace the SDK's default locations; without them the SDK's own
  environment and default paths (`AWS_CONFIG_FILE`, `AWS_SHARED_CREDENTIALS_FILE`)
  apply, as the transport documents. `--check` does not read the AWS files: the
  profile's existence and keys are the SDK's to resolve on first use.
- **The replay boundary.** For Azure and Vertex it is the `binding` (with the
  endpoint or root and the kind of authentication). For Bedrock it is the profile
  name, the files and the endpoint: pointing a profile at a different AWS account
  does **not** change it, so an operator who does that should rename the profile,
  and the note says so because the file has no way to bind Bedrock more tightly
  (the library's `with_credential_provider` binding replaces the profile
  semantics).
- **No ambient proxy, and no setting for one.** The transports use a client that
  ignores `HTTP_PROXY`/`HTTPS_PROXY` (their default), and Vertex's token exchange
  never uses a proxy at all. A deployment that reaches the internet only through a
  proxy cannot use them from `serve` yet; an `ambient_proxy` switch for inference
  traffic is possible later and does not help Vertex's exchange.
- **Failure behaviour is the transports'.** Credential resolution happens before
  admission, so a missing profile, an unreadable service account or an expired
  token fails the request (503 and retried when transient, stopped when the
  configuration is wrong) and reserves neither budget nor rate capacity, and the
  error names the profile or binding, never the SDK text or a path. A strict
  `budget_usd` needs trusted pricing, and the catalog has no row for most
  `bedrock/<model>` or Azure deployments, so such a request under a strict cap is
  refused unless `budget_allow_unpriced` is set; that is the existing budget rule,
  stated here so it is not a surprise.

## What is not in the published image yet

`serve-cloud` adds the AWS SDK and the Google and Azure paths to the binary. The
published image (0019) keeps `serve-redis,serve-otel` until the owner decides. The
cost, as `docker image ls` reports it for the image built from the Dockerfile on
an arm64 laptop: about 58 MB without and 63 MB with `serve-cloud` (roughly 5 MB, 9 %).
The pull-request job prints both sizes for its own build as a job summary; it runs on
amd64, so its figures will differ from these, and the added build time is visible in
the job. A deployment that needs these providers builds the image with
`--build-arg FEATURES=serve-redis,serve-otel,serve-cloud`, and `docker-check.yml`
builds that combination on every pull request that touches the Dockerfile, so it
does not rot.

## Not decided here

- Entra (managed identity, workload identity federation) for Azure, and token
  sources for Vertex other than a service-account file. Each needs a way to
  describe a token source in a file.
- More than one instance of a kind in one file.
- An inference-only `ambient_proxy` (0021 defines `[upstream] ambient_proxy`, which
  does not apply to these transports: their transport is always scoped).
- Catalog rows or prices for cloud-hosted models beyond what the catalog already
  carries for the exact `vertex-*/<model>` and Azure rows.

## Slices

1. This note.
2. Done: the feature, the config types and their validation (the transports' own
   checks moved into `cloud_rules`, which both call), the schema, the feature refusal,
   the reserved names, and the service-account check at `--check`. Tests:
   validation by path, one kind's field on another kind, the shared-Vertex-identity
   rule, the missing-feature refusal, the secret-file checks (exit 3 and 1), the
   example file.
3. Done: building the providers into the router from the file (build errors as issues,
   `/v1/models` from deployments) and the process tests. An Azure success path
   cannot be tested end to end (the transport refuses a non-https endpoint and
   `llmshim` has no test trust anchor), so the process tests cover what can be
   observed: each kind starts from a file; an unknown Azure deployment is refused
   locally; Bedrock and Vertex fail a request cleanly when their identity cannot be
   resolved, with no budget reserved; and their request construction stays
   covered by the transports' own fixture tests.
4. Done: `serve-check.yml` builds and tests `serve-cloud`; `docker-check.yml` builds the
   image with it; the 0014 row; the docs.
