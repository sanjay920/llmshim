# 0039: Residency

Status: slice 1 built (file keys). First slice of the roadmap item "guardrail pipeline; residency policy".

## The problem

An organization with data-residency rules needs to say "this team's requests may only go to providers that
process data in the EU". The gateway already refuses a key a model it may not call (0028), but listing every
EU model by hand does not scale and rots as providers and models change; the rule is about **where a provider
runs**, which the operator knows and the gateway does not.

## The rule

```toml
[providers.openai_eu]
kind = "openai-compatible"
base_url = "https://eu.example.com/v1"
residency = "eu"          # the operator's statement of where it runs

[[keys]]
name = "emea"
tenant = "platform"
secret = { env = "EMEA_KEY" }
allowed_residency = ["eu"]
```

- `residency` is **an opaque label** on a provider (`[a-z][a-z0-9_-]{0,31}`). llmshim checks nothing about it
  against the provider's real location: it is the operator's statement, and the gateway's job is to enforce the
  statement consistently. (It is not `region`, which for Bedrock and Vertex is the cloud API's own region.)
- `allowed_residency` on a key lists the labels it may use. Omitted means no restriction, as today. **An empty
  list, a repeated or malformed label, and a label no provider carries are configuration errors**: the last would
  fail closed and silently on a typo.
- **A provider with no label is outside every residency.** A restricted key never reaches it (fail closed).

## How it is enforced: through the allowlist

Residency is **expressed in the allowlist's own terms** when the key store is built
(`ServeConfig::effective_allowed_models`), so there is no second rule and every path that checks the allowlist
enforces it identically (chat, the embeddings, image and speech and transcription routes, OpenRouter's own
fallback list, a deployment's pick) before anything is reserved or queued, with the same 403 `model_not_allowed`:

- with only `allowed_residency`, the key's list is `<provider>/*` for each provider carrying a listed label;
- with `allowed_models` too, **both must admit**: an entry survives only if it names a provider in the labels.
  A `route/<name>` or `deployment/<name>` entry is **dropped**, because an alias is matched without regard to
  what it resolves to and could carry the request to a provider outside the residency (a route or deployment
  is still usable through the models it resolves to, which the provider prefix admits);
- a combination that leaves no entry is a configuration error naming the key.

The expansion is part of each identity's fingerprint, so a reload that changes a provider's label (or a key's
labels) swaps the key store like any other key change.

## Limits (not built)

- **Control-plane keys and signed snapshots** carry no residency yet; a key managed there can use
  `allowed_models` only. A snapshot field needs the older-data-plane care 0030 describes (a data plane that
  predates a field refuses the whole snapshot).
- **Tenants** (`[tenants.<name>]`) have no residency; a tenant-wide rule is one `allowed_residency` per key.
- **Provider-side routing is trusted.** A provider that itself routes to other regions (OpenRouter, a vendor
  with global endpoints) is as resident as its label says; the gateway cannot see past it. OpenRouter's own
  `x-openrouter.models` and `preset` are already refused for any allowlist-restricted key (0028).
- Residency of what is **logged** (usage records, the access log, traces) is not addressed here.

## Tests

`serve::config::tests::residency_narrows_the_allowlist_to_the_providers_in_the_allowed_labels` (the expansion:
labels only, both, aliases dropped, an unlabeled provider never admitted, no residency unchanged) and
`::residency_mistakes_are_refused_by_path_not_left_to_fail_closed` (each configuration error by path);
`integration_serve_residency` (process): a key in one residency reaches its provider and the other labeled and
unlabeled providers never see the request (403 before the provider); residency and an allowlist must both admit,
and the embeddings route obeys it too; a key with no residency is unrestricted. Making the expansion a no-op
fails the process tests. The JSON Schema is regenerated.
