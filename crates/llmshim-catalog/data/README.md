`models.dev.json` is the unmodified response downloaded from
https://models.dev/api.json on 2026-09-16. It is third-party data under the MIT
license in `LICENSE.models.dev`. No credentials, local overrides, provider
account discovery, or private fixtures belong in this directory.

The refresh workflow replaces only this public snapshot. Tests validate its
structure and minimum coverage before proposing an update. The builtin table
continues to win conflicts for the fields it asserts.

`verified.json` contains separate verified builtin assertions, checked on
2026-09-21. It is maintained here, not copied into the models.dev artifact:

- Native Grok 4.7 capabilities, reasoning efforts, dates, and standard token
  prices: https://docs.x.ai/developers/grok-4-7 and
  https://docs.x.ai/developers/release-notes (including Grok 4.6 context pricing).
- Grok 4.7 streaming, image input, structured output, named tool selection,
  and encrypted-reasoning round trips: `tests/integration_grok_4_7.rs` in the
  parent repository, verified against the live xAI Responses API.
- OpenRouter's Grok 4.7 identity, capabilities, limits and standard endpoint
  prices: https://openrouter.ai/api/v1/models/x-ai/grok-4.7/endpoints . Priority
  endpoint rates are different; the catalog entries describe standard rates.
  The `:nitro` entry supplies replay metadata without a fixed price because
  standard and priority endpoints can compete for that route.

No public `grok-4.7-fast` API route is asserted. Its launch availability is
limited to Cursor and Grok Build.
