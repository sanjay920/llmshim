# 0032: Named instances of hosted providers

Status: slice 2 built (2026-11-17; see the end). A contract note, written before any code (README working rule 3): a relaxed
file rule, a defaulted trait method and the meaning of the admission name become promises. It is the
prerequisite 0031 named for balancing two accounts of one hosted vendor. Nothing here is started.

## The problem

`serve` takes one provider entry per hosted kind: `[providers.openai]` must be `kind = "openai"`, and the
name `openai` is refused for anything else ("a openai provider must be named \"openai\" (one per kind for
now)"). A tenant that has two OpenAI accounts, or an OpenAI account and a second one in another region
(`base_url`), cannot say so. The balancing of 0031 needs targets that are distinct **by the name admission
uses**, and for a hosted kind that name is fixed: `OpenAi::name()` returns `"openai"` whatever the entry is
called. Two entries would therefore share one queue lane, one rate bucket, one concurrency cap and one
circuit, and their usage records would not say which account served.

## What the name is used for

`Provider::name()` carries two different meanings today:

1. **Behavior and catalog identity: what kind of provider this is.** The pricing lookup
   (`cost::for_target(provider, model)`), the reasoning replay policy and its provenance, the Responses
   tool handling (`target.provider != "xai"`), the default base URLs, the curated model list. These are
   properties of the **vendor**, and a second account of the same vendor has the same ones.
2. **Admission identity: which endpoint a request is charged to.** The queue lane (one per provider, in
   `Scheduler` and in Redis), the rate buckets (`limits.providers.<name>`, `LLMSHIM_<NAME>_RPM`), the
   concurrency semaphore, the breaker target, the usage record's `provider`, the metric labels. These are
   properties of the **account and address**, and two accounts must not share them.

For `openai-compatible` entries the two coincide (the configured name is the kind's behavior and the
admission identity, and they were never ambiguous because the kind carries no vendor behavior). For hosted
kinds they are the same string, and only one entry per kind exists, so nothing has had to separate them.

## The design

Separate the two meanings, keeping every behavior check on the kind exactly as it is.

- **`Provider::name()` keeps meaning the kind.** Pricing (`cost::stamp`/`for_target`), the budget estimator
  (`gateway/budget.rs`: `catalog::lookup_id("{provider}/{model}")`, the OpenRouter shape checks), the
  token estimator (`proxy/ratelimit.rs`: reasoning budgets and the omitted-output default per vendor), the
  reasoning replay policy and its provenance, tool handling, the plan, images and the ChatGPT check all read
  the kind, and **`ReplayTarget.provider` and `AdmissionTarget.provider_name` stay the kind**. A second
  OpenAI account is `openai` to every one of them, so `openai-b/gpt-6-luna` is priceable and estimated
  exactly as `openai/gpt-6-luna` is. (Changing those fields to the instance would break every one of
  these readers silently: a strict budget would find no price for `openai-b/gpt-6-luna` and refuse every
  request.)
- **A new defaulted method `Provider::instance(&self) -> &str`** (default `self.name()`) is the admission
  identity, and **an `instance` field is added beside the kind** in `AdmissionTarget` and in
  `AttemptIdentity`. The admission readers move to it, **and only they**: the queue lane
  (`gateway/http.rs`), the rate-bucket acquire (`proxy/attempt.rs`), the concurrency semaphore, the 429
  penalty (`gateway/attempt.rs`), the tenant quota key, the usage record and its span
  (`gateway/usage.rs`), the breaker observation (`client.rs`), `fallback.rs` (the breaker and its
  `policy_limited_providers` set), the deployment selector's health question (0031), the metric labels,
  and the library's `LogEntry.provider` and returned stream provider, and the embeddings model ids
  (`"{instance}/{model}"`, so an id returned is requestable back and reaches the same account). Because
  the method is defaulted, no implementation of the public trait breaks, and every provider that does not
  set an instance reports its name.
- **`instance()` equals the registry key by construction in `serve`**: each `[providers.<name>]` is built
  with `with_instance(<name>)` and registered under `<name>`, so the lane a request is queued in is the
  lane the worker that `serve` starts for that registry key serves. A library caller that registers a
  provider under a key different from its `instance()` has a lane with no worker; the reference says
  `instance()` must equal the key, and `Router::register` states the rule (it does not enforce it: today's
  library callers already register under arbitrary keys).
- **`with_instance`** is a builder on `OpenAi`, `Anthropic`, `Gemini`, `Xai`, `Cerebras` and `OpenRouter`.
  It adds a private field to structs whose fields are public, so a downstream **struct literal** of one of
  them stops compiling; `new` and the other builders are unaffected. These types are not among the files
  the semver rule names, and the crate is `0.x`, where a minor release is the channel for this; the release
  that ships it says so.
- **The file rule relaxes**: a hosted entry may have any valid provider name unless the name is the
  **reserved name of another kind** (an OpenAI entry called `anthropic` stays refused); `route` and
  `deployment` are refused as provider names (the `deployment` check is added: a provider with that name
  is unreachable, because `deployment/` is resolved first). An `openai-compatible` entry may not take a
  hosted name, as today. `[providers.openai]` stays the entry that `openai/<model>` and a bare `gpt-*`
  infer to; a second entry, `[providers.openai-b]`, is addressed explicitly (`openai-b/gpt-6-luna`), as
  OpenRouter, vLLM and SGLang entries are, because a name prefix cannot be inferred.
- **Model resolution is extended, not assumed.** Today `Router::resolve_key` asks the catalog only about a
  prefix that is a known provider or alias, so `openai-b/<alias or non-canonical spelling>` would be sent
  **verbatim**, where `openai/<the same>` is rewritten to the catalog's spelling. For a key whose provider
  reports a different kind, `resolve_key` asks the catalog about `"{kind}/{model}"` and keeps the instance
  as the provider key, so canonical spelling works for the second account as for the first. A test pins it.
- **Limits by instance.** `[limits.providers.openai-b]` keys on the instance name and the kind's table does
  not leak to it (validation already accepts any name in `[providers]`). The **environment form**
  (`LLMSHIM_<NAME>_RPM`) exists today only for four kinds and a hyphenated instance name has no environment
  spelling: instance limits are **file-only**, and the note makes no environment promise.
- **Credentials.** Each entry has its own `api_key` and `base_url`; nothing is inherited. The usage record's
  `provider` is the instance (equal to the kind for the default entry, so existing records are unchanged).
- **Reasoning replay is verified, and holds.** An encrypted reasoning block is bound to the issuing
  account: `ReplayTarget::bind_account` hashes the kind, base URL **and API key** (`reasoning.rs`), and the
  filter drops an encrypted block whose account differs (`AccountMismatch`). Two OpenAI accounts, even on
  one host, get different bindings, so no extra slice is needed. Text and redacted blocks are not
  account-bound (existing behavior). Two instances configured with the same key and base URL share a
  binding, which is acceptable. A deployment serving reasoning traffic should still use `conversation`
  (0031) to keep a conversation on one account.
- **Reload.** Renaming an instance is a removal and an addition (`providers.removed`/`providers.added`): its
  lane, bucket and circuit state are the old name's and are not carried to the new one. 0022's sentence that
  a hosted provider's table "must be named for its kind" changes accordingly.
- **Not in scope.** The cloud transports (`bedrock`, `vertex-*`, `azure`): their identity is the
  deployment or account they are configured with (0020). `chatgpt` and the environment-derived routers
  keep their fixed names.

## What it does not change

- Behavior for any existing file or caller: a provider that does not set an instance reports its name,
  and the six hosted kinds built by `serve` for an entry named for their kind are identical to today.
- The default builds (`default`, `proxy`, `gateway`, `gateway-redis`): the trait method is defaulted and
  nothing calls `with_instance` there.
- Pricing, the catalog, reasoning policy and tool handling: they read `name()`.

## Risk, and how the tests find it

The risk is a place that used `name()` for admission and is missed: two instances would then share a lane,
bucket, semaphore or circuit without any error. The enumerated sites are listed above; the test is a
process test over **two `kind = "openai"` entries on two mock upstreams**, asserting each of them
separately, so a missed site fails visibly:

- two queue lanes (`/v1/gateway/stats` lists a lane per instance), and a limit on one instance
  (`limits.providers.openai-b.rpm = 1`) refuses its second request while the other instance still serves;
- an open circuit on one instance does not skip the other (a deployment over both, 0031, sends every
  request to the healthy one);
- usage records name the instance; metrics label it;
- priced budget enforcement works through the second instance (a priced model is priceable because the
  catalog is asked about the kind);
- a `deployment/chat` of `weighted` over both reaches both in the ratio;
- mode off: a file that names no second instance behaves byte for byte as before (the existing suites).

A mutation check removes `instance()` from one site (the breaker, the rate bucket, the lane key) in turn
and each fails a test.

## Contract changes (each with its own test when built)

- **`Provider::instance()`** (defaulted; `src/provider.rs`), additive, with the `instance` fields beside
  the kind in `AdmissionTarget` and `AttemptIdentity`.
- **`with_instance`** on the six hosted providers (a private field: see the semver sentence above; a
  `0.x` minor release).
- **`Router::resolve_key`** asks the catalog about the kind for an instance key (above).
- **The file rule** for hosted names, the `deployment` provider-name refusal and `unit_serve_cli`'s refusal
  strings; the JSON Schema is regenerated only if a description changes.
- **Documents updated with it**: 0001 (the "named for its kind, once" row), 0022 (the reload sentence),
  0031 (the prerequisite paragraph becomes "built").
- **0014** gains the rows: two instances of one kind have independent limits, queues and circuits; a
  second instance in a deployment; a name reserved for another kind is still refused; a renamed instance
  starts fresh state.

## Slices

1. This note.
2. `Provider::instance`, the builders, every admission site, the relaxed rule, and the process test and
   mutations above.
3. (None planned: the encrypted-reasoning binding was verified to be account-specific.)

## Not decided here

- Instances of the cloud transports.
- Inference of `openai-b/...` from a bare model name (it cannot be inferred and will not be).
- Whether `llmshim` the library (not `serve`) should expose instance names on `Router::register`
  directly; today `register(name, provider)` already names the registry key, and `instance()` makes the
  provider agree with it.

## As built (slice 2)

- `Provider::instance()` (defaulted to `name()`); `ReplayTarget.instance` (set by the default
  `request_replay_target` only when it differs from the kind); `AdmissionTarget.instance` and
  `AttemptIdentity.instance` beside the kind. Admission readers moved to the instance: the gateway lane and
  the response's provider (`http.rs`), the proxy's rate key and 429 penalty, the gateway's semaphore and
  penalty and tenant quota key, the usage record and its span, the client's breaker observation, the whole
  of `fallback.rs`, the deployment selector's health question, and the library's `LogEntry.provider`, returned
  stream provider and embeddings model id. Pricing, the budget and token estimators, replay, tool handling and
  the plan still read the kind.
- `with_instance` on `OpenAi`, `Anthropic`, `Gemini`, `Xai`, `OpenRouter` and `Cerebras` (a private field),
  called by `serve` for every hosted entry with the entry's name (equal to the kind for an entry named for it,
  so existing files are unchanged).
- The file rule: an API-key hosted kind may have any name except another kind's reserved name; `route` and
  `deployment` are refused as provider names; the cloud kinds keep their fixed names.
- `Router::resolve_key` asks the catalog about the kind for an instance key and keeps the instance as the
  provider key. **No built-in spelling differs from its id today**, so this branch is defensive and its test
  shows parity only (a catalog row whose name differs from a spelling would exercise it).
- Tests: process (two `kind = "openai"` entries on two mocks: each account's own limit refuses its second
  request while the other serves, usage records name the account, both lanes exist, a priced budget works
  through the second account, a 3:1 deployment over both reaches 6:2, the name rule); unit (resolution, the
  dispatch target carries the instance only when it differs, one account's open circuit does not skip the
  other). Mutations that move the semaphore key or the gateway lane back to the kind fail the process test.
- Not done: the environment form of limits for instances (file-only, as the note says); 0001 and 0022 and 0031
  are updated by this change.
