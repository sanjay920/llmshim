# 0028: Per-key model allowlists

Status: design, 2026-11-17. A contract note, written before any code (README working rule 3): a
`[[keys]]` field, a refusal and a failure row each become promises. It is the first slice of the
roadmap item "hierarchical budgets, model allowlists, tags, alerts, cost exports"; the others are
separate notes. It changes nothing until it is built. Nothing here is started.

## The problem

A caller key can use every model the gateway routes to. An operator wants a key limited to the
models it needs: a batch key that may call only a cheap model, a team key that may not reach an
expensive tier, a key that may call a route (`route/compaction`) but not the model behind it.
Budgets bound cost after the fact of a request being priced; an allowlist says which requests
are admitted at all, and it is checked before anything is reserved or queued.

## The rule

```toml
[[keys]]
name = "batch"
tenant = "platform"
secret = { env = "BATCH_KEY" }
allowed_models = ["openai/gpt-6-luna", "anthropic/claude-haiku-5-5", "openrouter/anthropic/*", "route/compaction"]
```

- **Omitted** means no restriction, as today. **An empty list is a configuration error**: it would
  refuse every request, and a key that cannot be used is a mistake, not a policy (the file's
  `allowed_cidrs` rule is the same).
- An entry is **an exact `provider/model` id** or **a prefix ending in `/*`** (`openai/*`,
  `openrouter/anthropic/*`). A `*` anywhere else, a bare `*`, an empty entry, an entry with no `/`
  or with whitespace or a control character, and more than 256 entries are refused. Matching is
  case-sensitive and byte-for-byte: no globbing, no regex, no version ranges, so what a prefix
  admits can be read off the file.
- **What is matched.** The request's model is resolved exactly as the gateway resolves it for
  dispatch (provider inference from a bare name, aliases, a named route's indirection), and the
  **routing id** that results (`provider/model` in the router's canonical spelling, the id
  `GET /v1/models` lists as `data[].id`, not a pricing or replay name) is matched against the list.
  Entries are therefore written in canonical form: an alias or a non-canonical spelling in the file
  never matches and fails closed, and operators copy ids from `/v1/models`. The check is built on
  the resolution's own provider and model, not on `PreparedRequest.target.model`, which for Azure is
  the underlying model of a deployment (an entry `azure/prod-chat` names the deployment). In addition, a request that addressed a
  route (`route/<name>`) is admitted when the list names `route/<name>`, so a key can be allowed a
  route without being allowed the model behind it; the model behind it is still the one that runs.
  Naming a model never admits a route that resolves to it: a route is a request alias with
  settings, and the list is read as written.
- **Where it is checked.** In `build_request`, once, after the request is resolved and before the
  policy scope, the queue, any rate bucket or any spend reservation exists. Every entry point goes
  through it (`/v1/chat`, `/v1/chat/stream`, and the native `/v1/chat/completions`, `/v1/messages`,
  `/v1/responses` facades, unary and streaming), so there is no path around it. In fleet mode the
  check runs on the origin replica; a replayed job is not checked again (the origin's decision is
  the decision).
- **The refusal** is `403` with code `model_not_allowed` and the fixed message `model not allowed
  for this key`. It is built as a provider-style error (`ShimError::ProviderError` with status 403,
  as the idempotency conflict is), so the proxy's own wires answer
  `{"error":{"code":"model_not_allowed","message":"..."}}` and the native facades keep the type
  (`permission_error` on the Anthropic wire, `invalid_request_error`-family on the OpenAI wires
  as their normalizer maps a 403). It names no model and lists nothing. Two things the caller can
  still learn, both existing behavior and not new: a resolution failure (an unknown `route/<name>`,
  an unconfigured provider) is answered before the check, as today, and the unknown-route answer
  lists route names. Nothing reaches the queue, a limiter or the provider (the prequeue
  preparation permit has been taken by then, as for any request that fails after it); a refusal is
  counted in `llmshim_gateway_rejected_total{provider="<resolved provider>",reason="model_not_allowed"}`
  and appears in the access log like any 403. No usage record is written, because no attempt was admitted (0010).
- **Fallback chains** are not a gateway feature (the gateway has no `fallback` field). One request
  field can still name more than one model: OpenRouter's `x-openrouter.models` (its own fallback
  list, with `route`). For a key with a list, **every** entry of `x-openrouter.models` in the final
  request, after a route's settings are merged, is matched as `openrouter/<entry>`, and the request
  is refused if any fails. A prefix such as `openrouter/*` admits OpenRouter's server-selected
  slugs (`openrouter/auto`, `openrouter/free`), because the choice is made upstream: an operator who
  needs a bound lists slugs. A route's settings can carry `x-openrouter.models` (an operator
  wrote them), and the same check applies to the merged request.
- **Reload.** The list is part of the key and follows the key's reload (0012): a reload takes
  effect for the next request; a request already admitted finishes.

## What it does not do

- It does not hide models: `GET /v1/models` lists the gateway's providers' models for everyone, as
  today (the endpoint is not authenticated per key). Filtering it by key is a separate, smaller
  decision recorded in "Not decided".
- It is not a spend control. A key allowed an expensive model is bounded by its budget, not by this.
- It does not limit a key to a provider's *capabilities* (tools, vision): a model is the unit.
- It cannot be set from a control-plane snapshot yet: the snapshot's key record (0024) has no such
  field. Carrying it is its own slice (an optional field in the signed payload, which older data
  planes ignore only if the format version allows it; that is decided there, not here).

## Contract changes (each with its own test when built)

- **`keys[].allowed_models`** (an optional list of strings), validated by the file's rules above,
  reported by path (`keys[2].allowed_models[1]`); the JSON Schema changes and
  `serve-config.schema.json` is regenerated (compared by `tests/unit_serve_cli.rs`).
- **`Identity`** gains `allowed_models: Option<Vec<String>>` (serde default `None`), so an existing
  key map is unchanged.
- **The refusal** and its metric reason.
- **0014** gains the rows: a model outside the list is refused 403 before the provider and before
  any reservation (also when the key has a budget: nothing is reserved); a route allowed without its
  model; a prefix entry; a request that streams; the native facades; a reload that narrows the list.
- The access log's existing fields are unchanged.

## Tests (when built)

- Process: a key with a list, a mock provider; an `x-openrouter.models` entry outside the list is
  refused while the same request without it is served.
- Process: a key with a list, a mock provider. An allowed model is served; an unlisted model is
  refused 403 with the fixed body and the mock is **not** called; a key with a budget refuses
  without any ledger movement (the spend counter is untouched in a fleet test); streaming and the
  three native facades refuse the same way; a prefix admits its family and not a lookalike
  (`openai/*` does not admit `openai-compatible/x`: the separator is part of the prefix); an alias
  or non-canonical spelling in the list never matches; an Azure deployment is matched by its
  deployment name (`serve-cloud` build); a route
  is admitted by `route/<name>` and the model behind it is refused to the same key.
- Unit: validation (empty list, `*` placement, whitespace, no `/`, over-long list, duplicate
  entries are accepted and de-duplicated); matching (exact, prefix, route, case).
- A mutation check: removing the check in `build_request` fails the process test.
- A reload that narrows a key's list refuses the next request.
- Mode off (no `allowed_models` anywhere) leaves every existing test unchanged.

## Slices

1. This note.
2. The field, its validation, the schema, `Identity`, and the check in `build_request` with its
   refusal, metric and tests.
3. The control-plane snapshot field and the `llmshim-control` API for it (its own note section in
   0024's follow-up, because the signed format is a contract).

## Not decided here

- Whether `GET /v1/models` should be filtered by the calling key. It needs the endpoint to
  authenticate, which changes who may discover the list.
- Deny lists (`denied_models`), and tenant-level lists above keys: they belong with hierarchical
  budgets, where the tenant/team/project levels are decided once.
