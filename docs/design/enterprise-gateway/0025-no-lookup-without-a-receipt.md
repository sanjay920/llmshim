# 0025: No receipt lookup for an assistant turn that cannot have one

Status: implemented with its measurement. Not a new contract: an internal change with an
equivalence argument, recorded here because 0018's third run asked for it (README working
rule 3 is about formats and surfaces).

## The problem

0023 stopped receipt-free requests taking an executor slot. A request that replays history
was still refused about 91 % of the time at 32 and 64 concurrent (0018, third run), because
the native Chat wire (and the Responses wire, which converts through it) looks **every
assistant message** up in the receipt store (`get_bounded(scope, "reasoning", …)`), which
takes the store's file lock and reads its index, so the request needs the executor
(2 workers, one serial ingress worker).

## The argument

A `reasoning` receipt has exactly one writer: `response_from_chat` on the Chat wire
(`response.rs`), and only when the answer's message has a reasoning array. It stores under
`message_key(exported)`, and it sets `exported["reasoning_details"]` to that array
immediately before. So **every stored key has a non-null `reasoning_details`**
(`reasoning_content` is then a string, never null). The Chat ingress builds the lookup key
from the request's assistant message with the same `message_key`, after aliasing a message
exported before `reasoning_details` existed (its array under `reasoning`) into
`reasoning_details` (existing code). If that key's `reasoning_details` is null, **no stored
key can equal it**, so the lookup would return nothing; it is skipped. Everything that could
match is still looked up: a message with `reasoning_details`, one with the legacy
`reasoning` array (aliased), and (separately, unchanged) every tool call in history.

The result is identical to before for every request, because a skipped lookup is one that
returned nothing; what changes is that such a message no longer takes the store's lock, and
(through 0023) a request whose assistant turns are all plain text no longer needs the
executor.

## What it must not change

- Behaviour for any message that carries reasoning: looked up exactly as before.
- Tool-call history: `import_call` still looks every call up (a receipt can exist for it;
  that is the fail-closed owned-id rule).
- The store's contents and key format.

## Test and measurement

- `a_chat_assistant_turn_is_looked_up_only_when_a_receipt_can_exist` runs the conversion
  against the tripwire store (0023) for plain text, null content, null `reasoning_details`
  (no lookup) and for `reasoning_details`, a legacy `reasoning` array and a tool call
  (lookup). Making the skip unconditional (always look up) fails its first three cases.
- The wire-level test now serves a text history with the executor saturated and still
  refuses a history carrying reasoning.
- The benchmark's `--history 2` run is repeated (0018, fourth run).

## Not changed, and still limited

An assistant turn that carries reasoning or tool calls still looks the store up and still
needs the executor: a caller replaying reasoning-bearing history through the native wire is
still limited by it. Not measured here. The options 0018 listed for that case (a negative
check against the store's in-memory index, a bounded wait, more workers) remain open and
would need their own note.
