# Amazon Bedrock

The `bedrock` Cargo feature is off by default. Bundled release binaries leave it off. Library
users enable it with `features = ["bedrock"]`; build a custom binary with `cargo build --features
bedrock` when embedding a registered Bedrock provider. There is no automatic CLI or environment
registration.

Register an explicit profile binding; constructing it performs no authentication or network call:

```rust,no_run
use llmshim::{providers::bedrock::{AwsProfile, Bedrock}, router::Router};

let router = Router::new().register(
    "bedrock",
    Box::new(Bedrock::new(AwsProfile::new("development", "us-east-1"))),
);
```

Address a model as `bedrock/<model-id>`. The suffix is the exact Bedrock model or inference-
profile ID, or ARN; slashes and colons are preserved by routing and encoded as one path parameter.
The explicit region selects `https://bedrock-runtime.{region}.{suffix}`. The supported partitions
are `aws` and `aws-us-gov` (`amazonaws.com`), and `aws-cn` (`amazonaws.com.cn`). FIPS, dual-stack,
isolated partitions and the European Sovereign Cloud partition are not supported. Region strings
accept only lowercase letters, digits and hyphens; unsupported partitions are refused locally.
There is no environment-driven registration or change to existing API-key precedence.

`AwsProfile` stores a name and region, not credentials. Every HTTP attempt, including a retry,
checks a per-provider, shared SDK lazy identity cache. It resolves only the named SDK profile
provider on a cache miss and refreshes temporary credentials before expiry. Clones share the
cache; calling `with_profile_files` or `with_credential_provider` creates a new cache. The profile
may explicitly select an assumed role, SSO or a credential process. A missing/invalid profile
fails; it never falls through to exported keys, a default profile, container credentials or
instance metadata. A profile that explicitly declares `credential_source` still selects that
source as documented by the AWS SDK. `with_profile_files` selects both files without default
files; an application can instead supply a refreshable SDK `ProvideCredentials` handle with
`with_credential_provider(provider, "stable-identity-reference")`. This non-secret binding must
stay stable across restarts/replicas and change when the identity changes. For a caller-supplied
provider it replaces unused profile/file identity settings; its namespace is distinct from named
profiles. The endpoint remains part of the replay boundary. llmshim does not export tokens or
write them to application configuration. The AWS SDK's SSO provider may refresh and write its own
AWS SSO cache. Resolution is bounded to 30 seconds and by the dispatch deadline. Resolution
happens before policy admission, so credential failures reserve neither budget nor rate capacity.
Signing inputs and header encodings are validated before admission; the final SigV4 pass follows
admission with a fresh timestamp and uses the exact serialized bytes sent. Validation adds one
extra SigV4 pass per attempt without credential network traffic on a cache hit. A residual local
signer failure reports explicit zero usage; it never fabricates a provider bill, and existing
unpriced-fee policy stays conservative. Response-header time starts after authorization; the total
attempt deadline still applies. Redirects are refused. Converse traffic uses a reused client
without environment-selected proxies by default. `with_ambient_proxy(true)` opts into the caller
client's ambient proxy policy (a scoped client still has no ambient proxy). Credential traffic
(STS, SSO and explicit profile credential sources) uses the SDK's separate ring-based HTTPS client
and its own proxy rules; this option controls only Converse traffic. Applications requiring custom
credential networking should supply a configured SDK credential provider.

Credential errors include the profile name, or the caller binding for custom providers, and a
fixed reason with plain diagnostic wording, without SDK text or paths. Rustls uses ring for both
reqwest and the SDK; the feature does not add a second crypto provider or `aws-lc-sys`. Timeouts
and SDK provider failures are 503 and retry; missing profiles, invalid configuration and expired
returned credentials stop. SDK errors with opaque causes use `provider_failed`: this SDK erases
credential-process failure details, so these are not guessed from message text. `ShimClient::send`
sends unsigned JSON. Use `send_with_transport(&request, &provider)` when calling the HTTP layer
directly; completion and stream entry points select the transport.

Converse and ConverseStream support text, tool definitions, tool choice, tool results and signed
reasoning replay. Anthropic model IDs map common reasoning controls through the shared Anthropic
profile and model constraints, including clamped effort and budget tokens. Mapping requires
`reasoning_effort`; `reasoning_mode` or `reasoning_summary` alone never enable thinking. Other
families reject each unsupported reasoning control by name. Tool history without enabled
definitions (`tools` missing or `tool_choice: "none"`) is refused locally with a `toolConfig`
error. Adjacent same-role turns are merged with their blocks in order, including parallel tool
results and a following user message. Tool text-part arrays become text blocks; null/empty tool
content and non-text tool parts are refused. Tools without parameters use an empty object schema.
Empty assistant text is omitted. Content `cache_control` markers require native cachePoint support
and are refused instead of silently dropped. System/developer instructions must precede
conversation turns; appended system instructions are refused, including on bound-thinking models.
Other content blocks and unsupported common controls return a local error. Native model controls
may be supplied through `x-bedrock.additionalModelRequestFields`; the extension cannot replace
messages, system instructions or tools. Stream framing uses the AWS EventStream codec, verifies
both CRCs, bounds retained frame/block state, and rejects truncated or malformed streams. Tools
are exposed only with complete JSON arguments. A modeled exception after HTTP 200 remains a
failure, available as `BedrockError::from_shim(&error)` with a typed `kind`, machine `code` and
status. Classification never reads message text. Signature-error payloads are replaced with fixed
text because canonical-request diagnostics can echo credentials. Streaming and unary IDs retain
the AWS request identifier; a headerless manual decoder generates its own stable stream ID.

Usage retains native Bedrock counters. `inputTokens` excludes cache reads and writes, so the
normalized prompt total includes all three buckets, while `uncached_input_tokens` includes only
`inputTokens`. Streaming and unary responses use the same convention; policy accounting also
receives cache read/write counters. This transport adds no prices or regional availability entries
to the catalog. Signed reasoning is bound to the configured transport and identity reference.
Unknown model families cannot authorize reasoning replay. This includes ARN model identifiers: use
a cataloged model or inference-profile ID for signed reasoning across tool turns.

Offline tests use recorded protocol examples, synthetic credentials, the official AWS signing
vectors and in-memory HTTP responses. They make no cloud calls and need no AWS CLI or account. See
`tests/fixtures/bedrock/README.md` for fixture sources.

Public transport structs and error enums are non-exhaustive. Construct bindings/providers with
`AwsProfile::new` and `Bedrock::new`, service errors with `BedrockError::service`, and manual
decoders with `EventStreamDecoder::new`. Match error enums with a wildcard for future variants.
