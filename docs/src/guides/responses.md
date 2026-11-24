# Responses requests

`POST /v1/responses` uses the shared engine, admission, quota and gateway paths.
It accepts the stateless request built by Codex CLI 0.159.3, including tool
namespaces, custom grammar tools and `additional_tools` input declarations.
See the [source-derived fixture][fixture] for the audited source links.

Send full message, call, result and issued reasoning history on every turn.
`store: true`, `conversation` and `previous_response_id` remain unsupported.
Unknown request fields are refused rather than silently discarded.

## Tools

Responses providers receive the original custom formats and namespaces.
Function-only providers receive a custom tool as a function whose only argument
is the required `input` string. Its description includes the original grammar.
This transport cannot enforce a freeform grammar natively; the grammar remains
an instruction to the model. The returned argument must be exactly an object
with one string `input` member. Invalid envelopes fail the response.

Namespaced tools use bounded transport names and retain their original qualified
name, description and namespace description in the function declaration.
Responses output restores the original name and namespace. Both JSON and SSE
return `custom_tool_call` with the original input bytes. The server buffers calls
until complete, consistent with its other tool streams; ordinary text remains incremental.
Streams with hosted tools buffer text until completion to preserve native output
indices across search, discovered calls and messages.
Keep issued call IDs and complete output items when replaying a turn.

Hosted web search and tool search declarations require a native Responses
provider. They are refused on function-only providers; the library does not
execute tools or emulate provider search. Search calls and results retain their
order in output and replay. Discovered declarations restore subsequent calls
without being hoisted into the next native request's initial tools. Structured
text/image tool results use portable content where supported; audio/encrypted
results require Responses.

## Controls

OpenAI Responses providers receive `reasoning`, `text`, `parallel_tool_calls`,
`service_tier`, `prompt_cache_key` and `access_programs`. Access programs select
upstream behavior; the upstream account remains responsible for authorization.
The facade does not grant an entitlement or manufacture credentials.

Parallel calls use the provider's native control. Providers without that control
refuse `parallel_tool_calls` by name. A provider returning multiple calls when
parallel calls are disabled fails instead of violating that contract.
`text.verbosity` remains native on OpenAI Responses providers. Other providers drop it
and report `metadata.controls_dropped: ["text.verbosity"]`; the conversation and
its cache prefix are preserved.
`reasoning.context: current_turn` excludes older reasoning from replay;
`all_turns` keeps every eligible issued item. `auto` uses ordinary provider
replay policy. Effort uses the existing portable effort mapping. Numeric effort
and explicit concise/detailed summaries require a native Responses provider.
`summary: none` suppresses public summaries and selects omission where supported.

The only accepted controls that may be ignored are listed here:

| Field | Reason |
|---|---|
| `client_metadata` | Client telemetry and correlation; never model instructions. |
| `stream_options.reasoning_summary_delivery` | Summary scheduling; output is retained in full. |
| `prompt_cache_key` on an unsupported provider | Cache affinity does not change prompt content. |
| `service_tier` on an unsupported provider | Scheduling choice; prompt content is unchanged. |

Nullable reasoning/text controls mean no requested control. Other malformed
values fail admission. Storage and issued-reasoning receipt rules remain the
same for both JSON and SSE.

[fixture]: ../../../tests/fixtures/codex-responses-request.md
