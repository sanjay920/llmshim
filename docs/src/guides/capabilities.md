# Capability shims

Send a JSON Schema once, regardless of the provider:

```json
{
  "model": "openai/gpt-6-astra",
  "messages": [{"role": "user", "content": "Return the first three primes."}],
  "response_format": {
    "type": "json_schema",
    "json_schema": {
      "name": "primes",
      "schema": {"type": "array", "items": {"type": "integer"}, "minItems": 3, "maxItems": 3}
    }
  },
  "x-shim": {
    "structured_output": "auto",
    "tool_calling": "auto",
    "reasoning_capture": "off"
  }
}
```

The final `message.content` is JSON text, such as `[2,3,5]`. Native transports
and forced functions wrap non-object schemas internally and unwrap the result.
A successful final answer must validate against the original schema, including
constraints relaxed by wire normalization. Strict-schema nulls for optional,
non-nullable properties are restored to omission; explicitly nullable values stay
null. External schema resources are never fetched. Invalid or oversized schemas
fail before dispatch.

`structured_output: auto` uses a native format when the catalog asserts support,
then a pinned function when tools are supported and forced choice is not known
to be forbidden, otherwise a JSON instruction in the prompt. A native-format
assertion takes precedence for Anthropic too. `native`, `forced_tool`, and
`prompt` select a path explicitly. Explicit native/forced selection may be
rejected by a provider that does not implement it. Unknown capability flags are
not treated as verified support. One request holds one catalog snapshot.

Native mappings are Responses `text.format`, Chat Completions `response_format`,
Anthropic `output_config.format`, and GenerateContent
`generationConfig.responseMimeType` plus `responseSchema`. See the official
[OpenAI](https://developers.openai.com/api/docs/guides/structured-outputs),
[Anthropic](https://platform.claude.com/docs/en/build-with-claude/structured-outputs),
and [Google](https://ai.google.dev/gemini-api/docs/generate-content/structured-output)
documentation for the provider contracts.

An invalid complete JSON answer gets one corrective request. Both attempts'
reported usage contributes to the final totals, including cache tokens. A second
validation failure is an error. Refusals are preserved without repair;
incomplete responses and transport failures are not schema-repair requests.
Ordinary native user tool calls can still be intermediate turns before a native
or prompted structured final answer. Forced-output mode requests a final answer.

`tool_calling: auto` switches to a prompt protocol only when the catalog says the
model has no tool API. `prompt` selects that protocol explicitly; `native` uses
the provider's function API. Prompt calls are checked against the original tool
names and argument schemas, and honor `none`, `required`, pinned function choice,
and `parallel_tool_calls:false`. They receive ordinary owned call IDs and wire
mappings. The library never executes tools. Keep the same declarations and mode
on a follow-up request; paired tool history is rendered as text for a prompt
protocol. Native-capable destinations can consume the same canonical history.

`reasoning_capture: forced_tool` requests a brief explanation through an internal
function with one text argument. Its envelope contains the explanation and the
answer or requested user tool calls. The public explanation appears in
`message.reasoning`; internal functions never appear in the caller's tool list
or returned calls. On endpoints that cannot force functions, the same envelope
uses the prompt protocol. This is a generated rationale, not a provider's private
reasoning trace. Native deliberation about hidden protocol calls is not exposed,
and no replay signature is fabricated for the rationale.

Managed output streams buffer until validation succeeds and then emit canonical
content, reasoning, tool calls, usage, and finish metadata. Invalid attempts and
internal calls never become stream events or successful log entries. Ordinary
streams remain incremental. Buffering is bounded to 32 MiB of normalized chunks.
Top-level `stream` and `ShimClient::stream_owned` open the first HTTP response
before buffering, allowing frontends to send keepalives; the borrowed-provider
`ShimClient::stream` awaits validation before returning its buffered stream.

The HTTP client applies plans to completions, streams, and fallback attempts.
Direct `Provider::transform_request` calls only translate native formats; they do
not execute a shim or repair loop. Providers never receive `x-shim`. Native
extension overrides cannot replace a managed tool/output protocol. Models with
verified unsupported reasoning controls have those controls removed before
serialization. Changing a schema or shim mode changes the prompt/cache identity;
keep them stable for a conversation when relying on provider prefix caching.

`/v1/chat` and `/v1/chat/stream` accept `response_format` and `x-shim` at the top
level. Python and Ruby provide `shim=` / `shim:` and `response_format` options.
Go exposes `ChatRequest.Shim` and `ResponseFormat`; TypeScript uses the JSON keys.
