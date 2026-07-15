# Conversations across models

llmshim does not store conversations. A conversation continues because the
caller sends the previous messages again.

Start with a history:

```json
[
  { "role": "user", "content": "What is a Rust closure?" },
  { "role": "assistant", "content": "A closure is..." },
  { "role": "user", "content": "Explain that another way." }
]
```

Send it to another provider by keeping `messages` and changing the address:

```diff
- "model": "anthropic/claude-opus-4-8"
+ "model": "openai/gpt-5.6-sol"
```

There is no session handoff between Anthropic and OpenAI. The new provider sees
the history because it is present in the new request.

## Who owns history?

- **Rust applications** keep and resend their own `messages` array.
- **HTTP callers and language clients** do the same. The proxy is stateless and
  does not assign conversation IDs.
- **`llmshim chat`** keeps history in memory for the interactive process. The
  `/model` command changes the model for the next request without clearing that
  history; `/clear` removes it.

Provider adapters translate recognized cross-provider artifacts before sending
the history upstream. This includes common message roles and supported tool
call/result shapes. They also remove provider-only response metadata that
should not be sent to another API.

That translation makes a shared history usable across providers, but it does
not make every provider-specific detail portable. A turn that depends on a
native extension, unsupported content block, or provider-only behavior can
still lose that behavior after a switch.

## What model switching does not do

Changing the model does not:

- summarize or truncate history;
- run more than one model;
- choose a model automatically;
- persist messages after the caller or CLI process discards them.

Multi-model conversation is therefore a small operation: preserve the history,
choose a new model address, and send the next request.

