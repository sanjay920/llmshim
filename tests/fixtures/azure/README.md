# Azure v1 offline fixtures

These sanitized wire records are synthetic examples derived from Microsoft's public v1 schema.
They are not captured cloud calls. Resource/deployment names, response IDs, tokens and tool data
are invented. No registered key, tenant, account identifier or private endpoint appears here.

- `request.json`: deployment `model`, tool schema and streaming usage request.
- `response.json`: native tool call, cache/reasoning token details and filter annotations.
- `chat.sse`: prompt annotations, fragmented function arguments, usage-only tail and DONE.
- `filtered.json`, `filtered.sse`: a content-filter stop, distinct from normal completion.
- `filter-error.json`: request rejection with a nested policy code.
- `error.json`: typed deployment error; synthetic message text must never reach diagnostics.

Sources, consulted for this implementation:

- [Chat v1 reference](https://learn.microsoft.com/en-us/rest/api/microsoft-foundry/azureopenai/chat)
- [Endpoint/key and Cognitive Services scope][openai]
- [Foundry Entra scope][entra]

The examples follow the published JSON/SSE contracts; Microsoft documentation is available under
its site's documentation terms. Fixture data and test logic are original repository test data.
Tests read these files offline and use explicitly injected token sources and HTTP executors.
There is no OAuth, metadata, CLI, tenant or inference network traffic.

[openai]: https://learn.microsoft.com/en-us/azure/developer/ai/how-to/switching-endpoints
[entra]: https://learn.microsoft.com/en-us/azure/foundry/foundry-models/how-to/configure-entra-id
