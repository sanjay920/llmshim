# 0052: Payload capture and the cache debugger

Status: **proposed. Nothing is built.** Slice 5 of the admin UI (0051). It carries a privacy decision, so it is written
down and decided before any code. The decisions the owner must make are collected under "Decisions needed", each with
a recommendation; the rest of the note assumes the recommendations.

## The problem

The request ledger (0051) says a request had a 12% cache hit and cost $0.41. It cannot say *why* the hit was low, and
that is the question that saves money. A provider's prompt cache keys on an exact **prefix**: everything from the first
token up to a cache boundary must be byte-identical to an earlier request. One changed token near the front (a
timestamp in the system prompt, a tool list in a different order, an id inserted before the conversation) makes every
token after it a miss. The ledger shows the miss; it does not show where the prefix first differed from the request
before it.

Two things would answer it:

1. **Structure**: the prompt cut into its parts (each tool definition, the system text, each message and content block,
   in the order the provider reads them) with a fingerprint of each part. Comparing two requests' fingerprints finds the
   first part that differs, and says what kind of part it was ("a tool definition", "the system prompt", "message 3"),
   without storing a word of the prompt.
2. **Content**: the prompt and the completion themselves, so that the *change* (the timestamp) can be seen, and so a
   failing request can be read.

They have very different risk. Structure stores keyed hashes and lengths. Content stores what callers sent and what
models answered, which is customer data, may be regulated, and is the most sensitive thing this system could hold.

## The decision, in one paragraph

Build **structure capture first and on its own** (slice 5a): it answers "where did the prefix change" for most cache
problems, is cheap, and holds nothing readable. Build **content capture second** (5b) as an explicit per-client opt-in
that stays on the data plane: payloads are sealed with the 0050 object-store machinery, kept for a short time, readable
only through the gateway that holds the key, behind an authorization the control plane enforces and **audits on every
view**. The content diff (5c) is a view over 5b. Both are off unless a client's configuration turns them on, a failure
to capture never delays or fails a request, and the control plane never receives a payload it was not asked to show.

## Slice 5a: structure

**What is captured.** At the dispatch boundary, from the provider-native request body (what the provider actually
received, after llmshim's translation, not the caller's original), a list of **segments** in the order the provider
reads them:

| Provider wire | Order of segments |
|---|---|
| Anthropic Messages | `tools[]`, `system[]`, then each `messages[].content[]` block |
| OpenAI Responses | `tools[]`, `instructions`, then each `input[]` item |
| Chat Completions (OpenRouter, vLLM, SGLang, xAI, Cerebras) | `tools[]`, then each `messages[]` entry (a long message by its content parts) |
| Gemini | `tools`, `systemInstruction`, then each `contents[].parts[]` |

A segment is `{kind, hash, bytes}`: its kind (`tool`, `system`, `user`, `assistant`, `tool_result`, `image`, `other`),
the first 8 bytes of an **HMAC-SHA-256 of its canonical JSON**, and its size in bytes. Cache-control markers are removed
before hashing, so moving a breakpoint does not look like a content change. Images and audio are a segment with a kind
and a size and a hash, never their bytes. A request is capped at 2,048 segments; beyond that the tail is one segment
`other`.

**Why a keyed hash.** A plain hash of a low-entropy segment (a system prompt a dictionary can guess, a short
user message) can be reversed by anyone who can read the table. The HMAC key is a deployment secret (`{ env }` or
`{ file }`, like every secret in the file) shared by the gateways of one deployment and **not given to the control
plane**: the control plane compares hashes for equality and never needs to compute one. Rotating the key makes earlier
requests incomparable with later ones, which is the correct failure.

**Where it goes.** The usage record (0034) gains `structure`: the segment list (`[{kind, hash, bytes}, …]`), omitted when
the client has capture off. It rides the existing `[usage_export]` push (0051). The control plane stores it in a
`request_structure` table keyed by `attempt_id`, next to the ledger, with the ledger's retention. No new transport,
no new credential.

**The diff.** For a request with a stored structure, the control plane finds the **previous request of the same client
and model** that has one, within the provider's cache lifetime (five minutes, or the hour when the request asked for it),
and reports:

- the length of the common prefix in segments and in bytes,
- the first segment that differs, by position and kind ("the 3rd tool definition", "the system prompt", "message 5"),
- whether the later request only **appended** (the usual shape of a conversation, and not a miss) or **changed** an
  earlier segment (the miss),
- the cache tokens the request did read, so a large common prefix with a low read is visible as a provider-side
  explanation (below the minimum cacheable length, an expired entry, a boundary not placed) rather than blamed on the
  prompt.

The request panel (0051) shows this as a strip of segments with the first difference marked. An aggregate view lists,
per client and model, where misses start: "62% of misses begin at the system prompt". That is the number that says what
to fix.

**What it cannot say.** It cannot show *what* changed, only where. That is what 5b is for.

**Cost.** Hashing a request body is microseconds against a provider call of hundreds of milliseconds. The segment list
is about 12 bytes a segment: a 60-segment request adds under a kilobyte to a usage record.

## Slice 5b: sealed payloads, per client, short-lived

**Opt-in.** A client key in the gateway's file carries `capture = "off" | "structure" | "payload"` (default `off`).
It is in the file, not set from the UI, so turning on payload capture for a client is a reviewed change under version
control. (A control-plane-set flag would have to ride the signed snapshot and change its schema; that is a decision for
later, not a convenience to add now.)

**Capture.** After a request has been **admitted and has passed the guardrail classifiers (0048)**, the gateway seals
the request body and the answer (non-streaming: the body; streaming: the assembled text, reasoning excluded unless the
client's file says otherwise) as one object in the media store (0050): AES-256-GCM under a fresh per-object key, a name
that reveals nothing. In 0050 that per-object key travels inside the reference, which is fine there because the
reference never leaves the data plane. Here the reference **is** handed to the control plane, so the per-object key is
first **wrapped under a deployment secret**, `capture.key` (`{ env }` or `{ file }`, 256 bits, shared by the
gateways of one deployment and given to nobody else). The control plane holds the wrapped reference and can open
nothing with it. Capture is done off the request's path through a bounded queue; a full
queue or a failing store **drops the capture and counts it** (`llmshim_capture_total{outcome}`), it never delays or
fails the request. An image or audio part is replaced by a placeholder with its size. A payload over 256 KiB is
truncated with a flag, never refused.

A request the classifiers **blocked** is captured only if its client is `payload` *and* a setting
(`capture.blocked = true`) says so: the reasons a request was blocked are often the reasons it should not be stored.

**Reference, not content.** The usage record carries `payload_ref` (the object's name and its wrapped key, opaque to
everyone but a gateway holding `capture.key`) and `payload_bytes`. The control plane
stores the reference. It stores no payload.

**Retention.** The object store's lifecycle and the 0050 sweeper delete a payload after `capture.ttl_secs` (default 24
hours, at most 7 days). The ledger row outlives it; its `payload_ref` then answers "expired".

**Reading.** The control plane does not read the object store and does not hold `capture.key`. A viewer asks the
control plane for `GET /v1/requests/{attempt}/payload`; the control plane checks the viewer's role, **writes an audit
row (who, which attempt, when) before it answers**, then asks a gateway's admin listener (0051, slice 4) to unwrap the
reference, read the object and open it, and relays the answer with `Cache-Control: no-store`. A gateway that cannot be reached, or an object that is gone,
is a plain error. In a fleet any replica can open any payload because the store and the secret are shared.

## Slice 5c: the content diff

With two payloads of the same client and model that 5a paired, show the prompt side by side at the first differing
segment and a few segments on either side. Pure presentation over 5a and 5b; it adds no new data and no new access.

## Decisions needed

1. **Capture levels.** Structure only; or structure plus payload. *Recommendation:* build both, ship 5a first, and
   treat 5b as a separate, later decision that can be declined without wasting 5a. Default `off` for every client.
2. **Where payload bytes live.** On the data plane in the sealed object store, with the key wrapped under a
   deployment secret the control plane lacks, read through a gateway on demand (recommended); or copied into the control plane's database (simpler to query, but it moves customer text into the
   database the UI and every backup reads).
3. **Who may view a payload.** Today there are three fixed roles (0049): read, key-issuer, admin. *Recommendation:*
   **admin only** at first, every view audited. A fourth role (`payload-reader`) is the right long-term answer, because
   "can see spend" must not imply "can read prompts", but adding a role changes 0049's fixed set and is its own decision.
   Reading structure needs only the read role: it holds no text.
4. **Retention.** Structure follows the ledger (30 days by default). Payloads: 24 hours by default, 7 days at most.
   Confirm or change.
5. **Opt-in lives in the file.** Per client in the gateway's configuration (recommended), or settable from the UI
   through the signed snapshot.
6. **Blocked requests.** Never captured unless `capture.blocked = true` (recommended).

## Threat model

| Risk | Mitigation |
|---|---|
| Someone with database access reads structure | Keyed hashes, kinds and sizes only |
| Someone with object-store access reads prompts | Every object is sealed by the gateway under its own key; the store holds ciphertext, and the key is not stored beside it |
| Someone with the control plane's database reads prompts | The references it holds carry a key wrapped under `capture.key`, which the control plane does not have; it also has no object-store credentials |
| Dictionary attack on structure hashes | HMAC with a deployment secret the control plane never has |
| A viewer reads payloads without a trace | The audit row is written before the answer; a viewer with no role gets 403 |
| Capture slows or breaks serving | Bounded queue, drop and count, never on the request's path |
| Payloads linger | A short default TTL, a hard maximum, the sweeper, and `expired` shown honestly |
| Payloads reach an operator's browser cache or a log | `Cache-Control: no-store`; the payload is never written to an access log, a usage log or a trace attribute |
| A key is rotated and old hashes mislead | Rotation makes old and new incomparable; the diff reports "not comparable", never a false match |

## Failure modes (rows for 0014)

| Failure | Effect |
|---|---|
| The segment walker meets a body it does not understand | The request is served; the record carries no structure; counted |
| The control plane is down | 0051's bounded export queue applies: the oldest records drop, serving continues |
| The object store is down during capture | Capture dropped and counted; serving continues |
| The object store is down when a viewer asks | A plain error to the viewer; the audit row is already written |
| A gateway restarts with captures queued | Those captures are lost: capture is best-effort, not an audit log |

## Not built, and not planned

- Capture of anything for a client that did not opt in.
- A payload search or export: a payload is read one request at a time.
- Editing or replaying a captured request.
- Cross-organization comparison.

## Tests (when built)

- The segmenter per provider wire: order, kinds, canonical hashing, marker stripping, the 2,048 cap, images as sizes.
- Equal bodies hash equal; any one-byte change moves only that segment and the ones after it in the diff.
- The diff: appended versus changed, the first difference's position and kind, an expired window, a rotated key.
- Capture off sends nothing: a record without `structure` and without `payload_ref`.
- A failing store and a full queue drop the capture and not the request (a mutation removing the guard must fail a test).
- A payload round trip through the object store is ciphertext at rest, and a viewer without the role gets 403 and no audit row is skipped for one with it.
- The retention sweep removes payloads and the ledger answers `expired`.
- A grep-style test that no payload text can reach a log, a metric label or a span attribute.
