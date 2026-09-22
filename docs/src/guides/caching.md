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

`upto_message` is a zero-based index into the `messages` array exactly as you
sent it — system and developer messages count, and each `role: "tool"` result
counts as its own entry — not into the provider-native array (Anthropic hoists
the system prompt out, so native numbering differs). A caller whose own message
model expands into more wire messages than it holds must remap before copying
an index across. On Anthropic an index past the end is rejected with a 400,
and one that lands too early silently caches less than intended; on every
other provider segments are parsed but neither checked nor placed. Labels are
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

They are joined by `uncached_input_tokens` — the prompt minus whatever cache
read the provider already counted inside it. Providers disagree on that:
Anthropic's `input_tokens` excludes the cache read, while the OpenAI Responses,
Chat Completions and Gemini prompt totals include it. llmshim resolves the
disagreement at the transport boundary so one counter means one thing.

`cost_source` distinguishes three accounting meanings:

- `cost_source: "provider"` — the provider reported what it charged for this
  generation and `cost_usd` is that figure, not an estimate. OpenRouter does
  this (as `usage.cost`), unconditionally and on streams too. It needs no
  catalog entry, so it answers for aggregator slugs the catalog has never
  heard of.
- `cost_source: "provider_floor"` — a partial provider bill is the highest
  known lower bound, but no exact terminal provider bill survived. `cost_usd`
  is not a final invoice in this case.
- `cost_source: "catalog"` — computed here from catalog prices, as below.

A terminal provider bill always wins. A partial provider bill remains a floor
that a later catalog estimate cannot undercut, without being mislabeled as an
exact final invoice.

The catalog estimate charges standard token classes: uncached input, output,
cache reads and cache writes each at their own catalog rate, so a cached prompt
is never billed twice. **`null` means the catalog carries no price for the
model — it never means free.** A model priced for input but not for the cache reads a
response actually used charges that class at the model's highest published
rate, yielding a conservative estimate. Context tiers include cached input
when choosing the rate and apply to the entire request. For native Grok 4.7,
rates double above 200,000 input tokens. Provider tool fees, regional premiums,
and priority endpoint surcharges are not included in these catalog estimates.

`ProviderRequest::can_continue_from` compares endpoint, credential headers,
settings and the full prior input prefix. `include`, `store`, `reasoning`, tool
schemas and other settings participate. The library sends full stateless
requests; this helper does not create a continuation cache or enable provider
conversation storage.
