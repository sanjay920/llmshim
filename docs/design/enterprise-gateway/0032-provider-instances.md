# 0032: Named instances of hosted providers

Status: design, 2026-11-17. A contract note, written before any code (README working rule 3): a relaxed
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

Separate the two meanings with the smallest change that keeps every behavior check exactly as it is:

- **`Provider::name()` keeps meaning (1): the kind.** Nothing that reads it changes: a second OpenAI
  account is `name() == "openai"` for pricing, replay and tool handling.
- **A new defaulted method `Provider::instance(&self) -> &str`**, default `self.name()`, is meaning (2):
  the registered key. It is used by admission only: the admission target's provider name, the
  client's breaker observation, `fallback.rs`'s breaker use, the usage record, the deployment selector's
  health question and the lane key. Because it is **defaulted**, no implementation of the public trait
  breaks and every existing provider keeps its current admission name.
- Each hosted provider (`OpenAi`, `Anthropic`, `Gemini`, `Xai`, `Cerebras`, `OpenRouter`) gains a builder
  `with_instance(name)` that sets what `instance()` returns. `serve` builds each `[providers.<name>]`
  entry with `with_instance(<name>)`, so for the default (an entry named for its kind) `instance() ==
  name()` and **nothing changes for any existing file**.
- **The file rule relaxes**: a hosted entry may have any valid provider name, as long as the name is not
  the **reserved name of another kind** (`anthropic` must be an Anthropic entry; an OpenAI entry called
  `anthropic` stays refused) and not `route`/`deployment`. `[providers.openai]` stays the entry that
  `openai/<model>` and a bare `gpt-*` infer to; a second entry, `[providers.openai-b]`, is addressed
  explicitly (`openai-b/gpt-6-luna`), as OpenRouter, vLLM and SGLang entries already are, because a name
  prefix cannot be inferred.
- **Model strings.** `openai-b/gpt-6-luna` splits at the first `/` as it does today; the model after it is
  resolved against the **kind's** catalog (the instance's `name()`), so canonical spelling and pricing
  work for the second account exactly as for the first. A test pins this: the router resolves provider
  key `openai-b` to the instance and asks the catalog about `openai`.
- **Limits by instance.** `[limits.providers.openai-b]` (and `LLMSHIM_OPENAI_B_RPM` in the env form) key on
  the instance name; the kind's own table does not leak to the second account. A request to the second
  account is rate limited, queued, concurrency capped and circuit-broken on its own.
- **Credentials.** Each entry has its own `api_key` and `base_url`; nothing is inherited from a sibling.
  The usage record's `provider` is the instance (for the default entry, equal to the kind, so existing
  records are unchanged), and the `model` is what the instance was asked.
- **Reasoning replay.** An encrypted reasoning block is bound to the account that issued it
  (CLAUDE.md, "Multi-model conversations"). The binding is derived from the actual request's origin, so
  a block issued by account A is not replayed to account B. This is **verified, not assumed**: the first
  slice's review reads the binding and a test pins it (two instances, one issued block, replay to the
  other is dropped). A deployment that serves reasoning traffic should use the `conversation` strategy
  (0031) so a conversation stays on one account.
- **Not in scope.** The cloud transports (`bedrock`, `vertex-*`, `azure`): their identity is already
  the deployment or account they are configured with and they have their own multiplicity rules (0020).
  The `chatgpt` OAuth provider and the environment-derived routers keep their fixed names.

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

- **`Provider::instance()`** (defaulted; `src/provider.rs`), additive. A minor release, as with
  `observe_native` (0014 in the crate's history).
- **`with_instance`** on the six hosted providers (additive, public in `providers/*`).
- **The file rule** for hosted names (the schema's text and `unit_serve_cli`'s refusal strings change; the
  JSON Schema itself does not, the field set is unchanged), and `serve-config.schema.json` is regenerated
  only if a description changes.
- **0014** gains the rows: two instances of one kind have independent limits, queues and circuits; a
  second instance in a deployment; a name reserved for another kind is still refused.

## Slices

1. This note.
2. `Provider::instance`, the builders, every admission site, the relaxed rule, and the process test and
   mutations above.
3. (Only if the review finds it necessary) the encrypted-reasoning binding fix, if the verification above
   shows two accounts of one host share a binding.

## Not decided here

- Instances of the cloud transports.
- Inference of `openai-b/...` from a bare model name (it cannot be inferred and will not be).
- Whether `llmshim` the library (not `serve`) should expose instance names on `Router::register`
  directly; today `register(name, provider)` already names the registry key, and `instance()` makes the
  provider agree with it.
