# Prompt cache annotations

The caller knows which prompt sections stay stable. Declare that explicitly;
llmshim translates the declaration into the provider's caching mechanism.

```json
{
  "model": "anthropic/claude-sonnet-5",
  "messages": [
    {"role":"system","content":"Stable instructions"},
    {"role":"user","content":"Session context"},
    {"role":"user","content":"The current question"}
  ],
  "x-cache": {
    "segments": [
      {"upto_message":0,"label":"instructions","stability":"static"},
      {"upto_message":1,"label":"context","stability":"session"},
      {"upto_message":2,"label":"question","stability":"turn"}
    ],
    "key":"session:branch"
  }
}
```

`upto_message` is a zero-based index into the supplied messages. Labels are
informational and are never used to guess prompt semantics. Keep stable sections
before volatile sections. The proxy accepts the same top-level `x-cache` field;
Python and Ruby provide a `cache=`/`cache:` keyword, Go has `ChatRequest.Cache`,
and TypeScript accepts the field directly.

For Anthropic, `static` ends with a one-hour marker and `session` with a
five-minute marker. `turn` never creates a marker. Eligible boundaries are
selected from the end, with at most four markers. Existing explicit markers
consume that budget. Managed segments supersede automatic end-of-request
caching, so a volatile tail does not accidentally become a breakpoint. Invalid
indices and one-hour markers following five-minute markers fail locally.
These limits and TTL ordering follow the
[Anthropic cache contract](https://platform.claude.com/docs/en/build-with-claude/prompt-caching).

For Responses transports, `key` becomes `prompt_cache_key`. No key is derived
when it is absent. Other transports drop the annotation. Without `x-cache`,
llmshim adds no breakpoints and preserves existing native cache behavior.

Every response and usage event reports `cache_read_tokens` and
`cache_write_tokens`. Values are provider-reported cumulative snapshots; do not
add repeated streaming snapshots together. Zero means no cache tokens were
reported, not that the request was free.

`ProviderRequest::can_continue_from` compares endpoint, credential headers,
settings and the full prior input prefix. `include`, `store`, `reasoning`, tool
schemas and other settings participate. The library sends full stateless
requests; this helper does not create a continuation cache or enable provider
conversation storage.
