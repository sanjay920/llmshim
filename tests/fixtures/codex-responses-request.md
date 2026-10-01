# Source-derived Responses CLI request

`codex-responses-request.json` is written by hand from the request builder in
`codex-cli 0.159.3`. This audit read source; it did not launch a model turn.
Prompt text, identities, cache affinity and grammar are synthetic. The fixture
uses the
Responses Lite form: `additional_tools` supplies a namespace containing the
custom `apply_patch` tool. Empty instructions and absent optional controls are
omitted as the source serializer does.

Sources at the installed version:

- [Request builder][builder]: `build_responses_request` supplies model, input,
  instructions, tools, tool choice, parallel calls, reasoning, storage, stream,
  stream options, include, service tier, cache key, text and client metadata.
  The HTTP/WebSocket callers attach access programs after building the request.
- [Request types][request]: optional-field serialization, numeric custom effort,
  reasoning contexts, summary delivery and access-program wire values.
- [Tool specifications][tools]: function, namespace, custom, web search and tool
  search declarations. Lite mode groups functions and custom tools in the
  `functions` namespace.
- [Tool formats][formats]: custom grammar format and namespaced members.
- [Input models][models]: custom/function calls carry optional namespaces;
  tool results accept strings or structured content arrays.

The integration test sends this complete fixture through the facade on both
native Responses and function-only Chat Completions transports. It verifies
JSON, SSE and a second-turn replay with exact `apply_patch` input bytes.
Separate control tests cover optional fields from the same source builder.

[builder]: https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/core/src/client.rs
[request]: https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/codex-api/src/common.rs
[tools]: https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/tools/src/tool_spec.rs
[formats]: https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/tools/src/responses_api.rs
[models]: https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/protocol/src/models.rs
