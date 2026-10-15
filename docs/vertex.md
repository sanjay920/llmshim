# Google Vertex

The `vertex` Cargo feature is off by default. Enable it explicitly for library use. Bundled
release binaries leave it off; providers are registered by the application, with no environment
registration or changes to existing API-key precedence.

```rust,no_run
use llmshim::{providers::vertex::{GoogleIdentity, Vertex}, router::Router};
let identity = GoogleIdentity::service_account(
    "example-project", "us-central1", "selected-account", "service-account.json".into(),
);
let router = Router::new()
    .register("vertex-gemini", Box::new(Vertex::gemini(identity.clone())))
    .register("vertex-anthropic", Box::new(Vertex::anthropic(identity)));
```

Use `vertex-gemini/gemini-3.8-flash` or a Vertex Anthropic ID such as
`vertex-anthropic/claude-sonnet-5-5@20261001`. Model availability is project/location dependent;
this transport adds no regional catalog inventory or prices. Cost uses only a priced catalog row
for the exact `vertex-gemini/<model>` or `vertex-anthropic/<model>` target. Direct API prices
and bare model-name fallbacks are never used for Vertex. Without a known Vertex price, cost is
unknown (`null`) and strict budget admission treats the target as unpriceable.
The configured project and location select the path. Regional endpoints use
`https://{location}-aiplatform.googleapis.com/v1`; `global` uses
`https://aiplatform.googleapis.com/v1`. Anthropic's `us` and `eu` multi-region endpoints use
`aiplatform.{location}.rep.googleapis.com`. Project/location strings are validated and models
are encoded as single path segments. Invalid values fail locally before credential resolution.

Gemini uses `generateContent` or `streamGenerateContent?alt=sse` under publisher `google`.
Anthropic uses `rawPredict` or `streamRawPredict` under publisher `anthropic`, removes `model`
from the Messages body and adds `anthropic_version: "vertex-2023-10-16"`. The exact Vertex
model version stays in the URL. Both reuse the direct provider's request, response, tool,
reasoning and usage conversion, with a transport-specific identity/replay boundary. Native
controls remain under `x-gemini` and `x-anthropic`. Images/tools and supported model constraints
follow those converters. Reasoning and tool signatures cannot cross identity bindings.

Construction reads no file and performs no authentication. `GoogleIdentity::service_account`
selects exactly one service-account JSON file; only `type: "service_account"` is supported.
JWT assertions use RS256 and the cloud-platform scope at Google's fixed OAuth token endpoint.
Files selecting another token URI are refused. The JSON may contain private key material, which
stays in memory during refresh and is never exported to application configuration or diagnostics.
No environment variable, ambient ADC search, metadata service, installed CLI or alternate file
is consulted after failure. Authorized-user and workload-identity files are not implemented;
supply an explicit caller-owned `TokenSource` for those identities. A stable non-secret binding
is required for both file and custom token sources. Keep it stable across restarts/replicas;
change it when the selected identity changes, even if the file path stays the same.

A clone-shared cache checks the selected source on every dispatch/retry, refreshes before
expiry, and serializes concurrent refreshes. Cached tokens are read separately from the refresh
lock. Tokens carry monotonic expiry and are never printed through Debug. Bearer headers are
marked sensitive. Foreground credential resolution is bounded to 30 seconds and the existing
dispatch deadline, before policy admission; credential failure consumes no budget or rate
capacity. Background refresh has its own 30-second source bound. Transient token-service
failures/timeouts retry as 503; invalid files or rejected credentials stop as 401. While a
cached token has more than one second remaining, authorization returns it immediately. One
caller starts a background refresh using a separate mutex; other callers keep using the cached
token. A failed early refresh preserves that token and waits one second before another early
attempt. Only callers without a usable token wait for source I/O; expiry or rejection bypasses
the early-refresh cooldown. A timed-out refresh releases the mutex, so the next caller can
refresh again. Expired or rejected tokens are never used as a fallback. An inference HTTP 401,
or a 403 with a recognized token-rejection code, invalidates the token actually sent. Within the
configured retry limit, the same dispatch refreshes the selected identity once and retries. A
second rejection fails and invalidates that token too. With retries disabled, invalidation
applies to the next dispatch. An ordinary IAM permission-denied 403 does not trigger refresh.
Identity selection never changes.

Inference and token traffic use scoped HTTPS clients with redirects and ambient proxies disabled.
`with_ambient_proxy(true)` opts inference traffic into the caller client's proxy policy; a scoped
caller client remains scoped. A caller needing custom token networking can provide `TokenExchange`
on `ServiceAccount`. The exchange must not log its JWT assertion. `ShimClient::send` is unsigned;
use `send_with_transport` or the completion/stream entry points for call-time authorization.

Both stream paths use the existing bounded SSE parser. Google envelopes and Anthropic error
frames remain typed failures even after HTTP 200. Classification uses status and code, never
message text; upstream explanations are withheld because they can echo credentials or prompts.
Prompt blocking remains `content_filter`, distinct from a failed transport. Gemini output usage
includes thoughts once, exposes cache buckets and reasoning tokens, and retains native accounting
through the existing usage path. Anthropic stream usage follows cumulative snapshot semantics.

All new public structs/enums are non-exhaustive and have constructors. Tests are offline and
use synthetic tokens, fake exchanges and virtual time; see `tests/fixtures/vertex/README.md`.
