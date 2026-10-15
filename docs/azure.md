# Azure OpenAI and Foundry v1

The `azure` Cargo feature is off by default and adds no dependency crates. Bundled binaries
leave it off. Register the provider explicitly; there is no environment registration or implicit
credential chain.

```rust,no_run
use llmshim::{providers::azure::{Azure, AzureIdentity}, router::Router};
let identity = AzureIdentity::api_key("selected-resource", "your-key")?;
let provider = Azure::new("https://example.openai.azure.com", identity)?
    .with_deployment("chat-production", "gpt-6-sol")?;
let router = Router::new().register("azure", Box::new(provider));
# Ok::<(), llmshim::error::ShimError>(())
```

Request `azure/chat-production`. The body `model` is the configured deployment name, rather
than the catalog model. `with_deployment` explicitly records its underlying model for catalog
family and replay metadata. Unknown deployments are refused locally. The endpoint accepts an
HTTPS resource origin or its `/openai/v1` base, including the Azure OpenAI
`https://{resource}.openai.azure.com` and Foundry `https://{resource}.services.ai.azure.com`
forms. Custom HTTPS resource domains can be selected explicitly. User information, query strings,
fragments and other base paths are refused. The v1 operation is always
`POST /openai/v1/chat/completions`; legacy `/models` and dated API versions are not used.
See Microsoft's [v1 reference][v1].

`AzureIdentity::api_key(binding, key)` selects one explicit key. It is added as the sensitive
`api-key` header at dispatch time; no credential appears in `ProviderRequest.headers`.
`AzureIdentity::entra(binding, audience, source)` instead takes one caller-owned `TokenSource`.
Its `token(audience)` method must resolve only that identity and return an `AccessToken` with
its remaining lifetime. Integrate an explicitly configured Entra credential handle there;
llmshim never creates a default credential chain, reads an environment key, probes metadata,
launches a CLI or switches identity after failure.

Choose the audience explicitly:

| Audience | Requested scope |
|---|---|
| `Audience::CognitiveServices` | `https://cognitiveservices.azure.com/.default` |
| `Audience::Foundry` | `https://ai.azure.com/.default` |

The scopes follow Microsoft's [Azure OpenAI][openai] and [Foundry Entra][entra] documentation.
They are passed to the selected source, never inferred from a deployment name. Keep the
non-secret binding stable across restarts and replicas, and change it when the identity changes.
Replay metadata hashes the binding with the endpoint and authentication kind/audience; it never
contains the key or bearer. Identity and token structs intentionally do not implement Debug.

The clone-shared Entra cache uses the same private refresh mechanism as Vertex. A token with
more than one second remaining is returned without waiting for source I/O. Inside the early
margin (one quarter of its lifetime, capped at 30 seconds), one caller starts a background
refresh using a separate mutex acquired without waiting. Other readers continue using the live
token. Failed early refresh retains it and allows another early attempt after one second.
Expired or rejected tokens cannot be reused; callers without a usable token wait for refresh
and bypass that cooldown. Source calls have a 30-second bound. Foreground authorization also
obeys the existing dispatch deadline and happens before budget/rate admission.

An HTTP 401, or a typed expired/invalid-token 403, invalidates the bearer actually sent. Within
the configured retry limit the same dispatch refreshes the same selected source once. Repeated
rejection stops; a delayed rejection cannot evict a newer different bearer. Ordinary permission
403 responses and API-key failures do not trigger Entra refresh. Invalid/rejected credentials
fail as typed 401 errors; transient source failure or timeout is a typed 503.

The adapter reuses the Chat Completions conversion for messages, tools, tool results, vision,
structured output, cache accounting and reasoning metadata. It preserves native token-detail
objects, including cached and reasoning counters; completion counts already include reasoning.
Streaming requests force `stream_options.include_usage: true`, including when native options
are supplied. Empty-choice usage tails and prompt-filter annotation frames survive SSE framing.
A choice stopped by filtering keeps `finish_reason: content_filter`; a rejected request or an
error frame after HTTP 200 yields a typed `AzureErrorKind::ContentFilter` error. Classification
uses status and allowlisted machine codes, never message text. Untrusted error explanations
and nested details are withheld to avoid exposing credentials or prompts.

Native controls use `x-azure`. The fields `model`, `messages`, `stream`, and
`stream_options` cannot be overridden there. Controls that
have no mapping (`reasoning_mode`, `reasoning_summary`, `top_k`) are refused locally by name.
Other Chat controls are forwarded to the selected deployment, whose actual support is enforced
by Azure; this transport invents no deployment capability inventory.

Cost uses only a priced `azure/<mapped model>` catalog row from the explicit deployment map.
Deployment labels never select prices, and an unmapped deployment is unpriceable. It never
borrows a direct OpenAI rate. This transport adds no prices. Without a known Azure
price, catalog cost is unknown (`null`), and strict budget admission treats it as unpriceable.

Inference uses a scoped client without ambient proxies or redirects. `with_ambient_proxy(true)`
opts inference into the caller client's proxy policy; a scoped caller stays scoped. Token-source
networking belongs to the caller's explicitly supplied source and is subject to its own proxy
configuration. `ShimClient::send` is unsigned; use `send_with_transport` or the completion and
stream entry points to resolve authorization on each attempt.

Tests use synthetic, public-example-derived fixtures and in-memory token/HTTP endpoints. No
Azure credential, tenant, metadata service or real token endpoint is needed. The fixture README
lists the public schema sources. All new public enums and structs are non-exhaustive and have
constructors.

[v1]: https://learn.microsoft.com/en-us/rest/api/microsoft-foundry/azureopenai/chat
[openai]: https://learn.microsoft.com/en-us/azure/developer/ai/how-to/switching-endpoints
[entra]: https://learn.microsoft.com/en-us/azure/foundry/foundry-models/how-to/configure-entra-id
