# Recorded Responses CLI request

`codex-responses-request.json` was captured from the installed `codex-cli 0.159.3`
on 2026-10-01 using an isolated home and a loopback scripted Responses endpoint.
No provider account was used. Prompt text, identities, cache affinity and tool
schemas/descriptions were redacted; all top-level fields and input item kinds
remain. Namespace/custom-tool data is reduced to one representative definition.

This installed client sends extensions beyond the facade's function-tool subset:
`client_metadata`, `prompt_cache_key`, `parallel_tool_calls`, `reasoning.context`,
`text.verbosity`, and `additional_tools` input items containing namespaces/custom
grammars. The validation test asserts the complete captured shape is refused by
name. The replay test projects the supported fields and supplies ordinary
function tools plus reasoning from its previous scripted answer. It proves that
subset, not complete compatibility with this installed client.
