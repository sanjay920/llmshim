# Native provider controls

Use native controls when the portable request cannot express a provider
feature precisely enough. This is an intentional trade: exact provider
behavior in exchange for portability.

> **Availability:** Rust: top-level `x-*` key · CLI: not exposed · Proxy/clients: `provider_config["x-*"]`

The built-in namespaces are:

| Namespace | Target |
|---|---|
| `x-openai` | OpenAI Responses request fields |
| `x-anthropic` | Anthropic Messages fields and llmshim-managed Anthropic header controls |
| `x-gemini` | Gemini request fields and `thinkingConfig` |

There is no `x-xai` namespace. llmshim's unified reasoning control already
emits xAI's supported native `reasoning: {effort}` shape, including its
model-family clamps.

## OpenAI

`x-openai` fields are copied into the OpenAI Responses request body. This gives
access to the full native reasoning object, including `effort`, `mode`,
`summary`, and `context` when accepted by the selected model.

```json
{
  "model": "openai/gpt-5.6-sol",
  "messages": [{"role": "user", "content": "Analyze this carefully."}],
  "x-openai": {
    "reasoning": {
      "effort": "high",
      "mode": "pro",
      "summary": "auto"
    }
  }
}
```

The object is native OpenAI configuration. llmshim does not clamp it, so the
selected model must accept the values.

## Anthropic

Most `x-anthropic` fields become Anthropic Messages body fields. Two special
keys control headers instead:

- `disable_1m_context` disables llmshim's automatic 1M-context beta header;
- `extra_betas` appends and de-duplicates caller-supplied `anthropic-beta`
  tokens.

```json
{
  "model": "anthropic/claude-sonnet-5",
  "messages": [{"role": "user", "content": "Analyze this carefully."}],
  "x-anthropic": {
    "thinking": {"type": "adaptive"},
    "output_config": {"effort": "high"},
    "disable_1m_context": true,
    "extra_betas": ["extended-cache-ttl-2025-04-11"]
  }
}
```

`thinking` and `output_config` are sent in the request body. The two control
keys are consumed by llmshim while building Anthropic headers and are not sent
as body fields.

The Rust contract also accepts top-level `thinking` and `output_config`, but
the namespace keeps provider-native fields visibly grouped.

## Gemini

`x-gemini.thinkingConfig` replaces the unified Gemini thinking configuration.
It accepts Gemini's native object, including the legacy integer
`thinkingBudget`:

```json
{
  "model": "gemini/gemini-3.5-flash",
  "messages": [{"role": "user", "content": "Analyze this carefully."}],
  "x-gemini": {
    "thinkingConfig": {
      "thinkingBudget": 2048,
      "includeThoughts": true
    }
  }
}
```

Other keys inside `x-gemini` are copied to the Gemini request body. As with all
native controls, the provider validates their shapes and values.

## Where the namespace goes

In the Rust `serde_json::Value`, use the namespace as a top-level key, as in
the examples above.

The proxy reserves `provider_config` for fields that are merged into that core
request. Wrap the same namespace inside it:

```json
{
  "model": "openai/gpt-5.6-sol",
  "messages": [{"role": "user", "content": "Analyze this carefully."}],
  "provider_config": {
    "x-openai": {
      "reasoning": {
        "effort": "high",
        "mode": "pro",
        "summary": "auto"
      }
    }
  }
}
```

Language clients send the same object through their `provider_config` field.
The CLI chat workflow does not expose native request configuration.

## Precedence and portability

An explicit native reasoning object takes precedence over
`reasoning_effort`/`reasoning_mode`. llmshim does not translate the object into
another provider's dialect. Anthropic header controls are applied as headers;
the other native fields are copied to their documented native destination.

If you change the model address to another provider, remove or replace the old
provider namespace. Keeping `x-openai` in an Anthropic request does not recreate
the OpenAI behavior. See [Portable core, native edges](../concepts/portability.md)
for the underlying contract.

