# 0052: Payload capture and the cache debugger

Status: **all three slices are built: 5a (structure), 5b (sealed payloads) and 5c (the content diff).** Slice 5 of the admin
UI (0051). It carries a privacy decision, so it was written down and decided before any code. The owner accepted the
recommendations under "Decisions needed" (2026-11-20) and asked for 5a first; 5b is a separate decision that can still be
declined without wasting 5a.

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
and a size and a hash, never their bytes. A request reports at most 2,048 segments: from the 2,048th on, one `other` segment stands for the rest, so a change
anywhere in the tail still shows.

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
| A viewer reads payloads without a trace | The control plane writes its row before the fetch, and the gateway writes its own audit record (`payload.open`, a digest of the reference, the viewer, how they were identified) before it opens the object, so a compromised control plane cannot read without a trace on the gateway; a viewer with no role gets 403 |
| The token that scrapes metrics reads customer data | The open route has its own `capture.open_token`, different from the admin token (refused at start if equal); the control plane holds it in `payload_token_file`, separate from `token_file` |
| A shared token makes "who" a typed label | Payload routes refuse with 403 `identity_required` unless the caller signed in through the identity provider; `--allow-payload-views-without-sso` is the explicit break-glass, and the history still says `via: shared_token` |
| Answers hold more than the request (reasoning, tool output) | `[capture] response` is false by default: only the request is kept |
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

## Slice 5a as built

- **Gateway.** `[capture] hmac_key = { env | file }` (at least 32 random bytes, base64 encoded, fixed at start) and, per client,
  `capture = "off" | "structure"` on a `[[keys]]` entry (reloadable with the keys, and a reload **replaces** the set so
  turning it off stops it). A `[capture]` table needs `[usage_export]`; a key that captures needs `[capture]`; both are
  refused at load by path. `src/gateway/structure.rs` cuts the **provider-native** body, handed to the attempt policy at
  admission (`PreparedAttempt::native_body`, the same body the rate limiter reads), into segments in the order the wire
  reads them (the table above), each `{kind, hash, bytes}` with the hash the first 8 bytes of an HMAC-SHA-256 (our own
  RFC 2104 over `sha2`, checked against the RFC 4231 vectors) over the kind and the segment's canonical JSON (keys
  sorted, `cache_control` removed). The list is `UsageRecord.structure`, present only for a client that captures and a
  body the walker understood (`llmshim_structure_capture_total{outcome}` counts `captured` and `unrecognized`). The
  usage **log** drops it; only the export carries it. A snapshot-delivered key never captures: the opt-in is a reviewed
  change to the gateway's own file.
- **Control plane.** Schema 7: `request_structure` (attempt id, time, client, provider, model, the list), written in the
  ingest transaction with its row, once; a field that is not what a gateway sends is dropped and the row (its spend) is
  kept; a record with no client keeps none. It ages out with the ledger. `GET /v1/requests/{attempt}/structure` (read
  role) returns the list and the comparison with the same client's request before it (same provider and model, within an
  hour; `within_short_window` says whether it was inside five minutes): `identical`, `appended` (the normal growth of a
  conversation), `shorter`, or `changed` with the first differing segment named ("system prompt", "tool definition 2").
  `GET /v1/requests/misses` (read role) counts the pairs of a window and, for the changed ones, the kind of the first
  difference, in total and for the twenty clients and models with the most.
- **UI.** The request panel shows the prompt as a strip (the same part, the first change, what followed, or what was
  added) with a sentence and, when the prompt did not change but the cache hit was low, a pointer to the provider-side
  causes. Home gets "Why the cache missed": how many consecutive requests changed an earlier part, a ranked bar of
  where, and a table by client and model; without capture it explains how to turn it on.
- **Checked** by the segmenter's unit tests (order per wire, kinds, key and key-order independence, the cap, no text in a
  segment, the RFC vectors), by a real `serve` process exporting to a loopback control plane (an opted-in client's
  fingerprints equal for equal prompts and moving only for the changed segment, none for another client, no prompt text
  or secret in the export, none in the usage log, a reload turning capture on and off, the refusals), by the control
  plane's tests on SQLite and Postgres (pairing, windows, relations, the aggregate, ageing, migration from every earlier
  version) and by an end-to-end test of the real exporter into the real router; mutation-checked (everyone captures, the
  log keeping the structure, markers in the hash, the key ignored, the window ignored, appended read as changed).
- **Not here.** 5b and 5c. The fleet case needs nothing new: the structure is computed on the replica that dispatches,
  from its own copy of the file, and rides the same export.

### Review follow-ups (PR 303)

- **Work on the request's path is bounded.** The segment fingerprints are computed while the body is walked, streamed
  into the keyed hash with no copy of a segment, and a request past 8 MiB of canonical bytes has **no structure**
  (`llmshim_structure_capture_total{outcome="too_large"}`) instead of costing the request time. Only a cache marker on
  the object a provider reads it from (a tool, a system block, a content block) is removed before hashing; the same
  name deeper in a tool's schema or a caller's own JSON is data, and a change there shows.
- **A batch is cut by size.** The export's batches are cut at 1 MiB of JSON as well as by record count, and a body the
  control plane calls too large (413) is **split and sent again**, not dropped; only a single record it refuses is
  dropped. A capturing client's record can hold thousands of segments, and the 2 MiB limit must never cost another
  client's spend rows.
- **Known limits, kept honest.** A request is paired by client name, provider and model, so a client that holds several
  concurrent conversations is compared with whichever came just before (an inflated change rate), two deployments
  that both name a key `ci` interleave, and two requests in one millisecond pair in an arbitrary order. The fingerprints
  cover the prompt, not the request's other controls (the model's thinking settings, `tool_choice`), so a miss caused by
  one of those reads as `identical` or `appended`. The aggregate reads at most 20,000 stored lists and, past that,
  covers the first clients by name (it says so).

## Slice 5b as built

- **Gateway.** `capture = "payload"` on a key (it captures structure too). `[capture]` gains `payload_key` (at least 32 random bytes,
  base64 encoded, fixed at start), `ttl_secs` (default 86,400; 600 to 604,800) and `max_payload_bytes` (default 262,144; 1,024
  to 1,048,576, applied to the request and to the answer each). A payload client needs `payload_key`, `[media_store]` and an
  `[admin]` table **with a token**, all refused at load by path. The document is `{v, attempt_id, ts, wire, provider, model,
  outcome, request: {text, bytes, truncated}, response: {text, bytes, truncated}}`: the request is the **provider-native** body
  the attempt policy was handed (what the provider saw, aligned with the structure), serialized into a bounded copy as it is
  written; the answer is the provider's own frames, as `observe_native` hands them (a non-streaming body as it is, a stream's
  events one per line, up to the cap). A stream is therefore kept as the provider sent it, not assembled into text.
- **Sealing.** When the attempt ends the document is sealed (AES-256-GCM, a fresh per-object key and nonce, the object name as
  associated data) and the per-object key is **wrapped** under a key derived from `payload_key` (AES-256-GCM again, the name as
  associated data). The reference is opaque text: name, length, nonce and the wrapped key. The sealed object is put in the
  media store under `capture/<date>/<random>`, a shape the media sweeper may not delete, by a bounded queue (256): a full queue,
  an object the store would refuse, or a failing store **drops the capture and counts it**
  (`llmshim_payload_capture_total{outcome}`: `queued`, `stored`, `failed`, `dropped_full`, `too_large`); sealing is microseconds
  and nothing waits for the write. The usage record carries `payload: {reference, bytes, expires_at}`; the usage log never does.
  Blocked requests never reach the provider attempt where capture happens, so they are never captured (`capture.blocked` is
  therefore not a setting).
- **Retention.** A sweeper per replica deletes `capture/` objects older than `ttl_secs` (and nothing else); the ledger row
  outlives them and says `expired`.
- **Opening.** `POST /v1/serve/payload/open` on the **admin** listener only (behind the admin token; never on the main
  listener) takes `{"reference"}` and answers `{"payload": {…}}` with `Cache-Control: no-store`, or a class
  (`not_found`, `bad_reference`, `too_large`, `unavailable`) that says nothing the store or the reference said. A reference
  is only ever opened for a name under `capture/`, so it cannot be made to read another object.
- **Control plane.** Schema 8 adds `payload_ref`, `payload_bytes`, `payload_expires_ms` to the ledger: an opaque reference
  it can open nothing with. The request list says that a payload exists (bytes, expiry, expired); **the reference is never in
  an answer to a browser**. `GET /v1/requests/{attempt}/payload` is **admin only**: it writes a `payload.viewed` row to the
  chained change history (actor and attempt id, never text) **before** it asks any gateway, then asks every listed gateway at
  once to open the reference (`--gateways-file`; in a fleet any replica can, with a local store only the writer) and relays the
  first document with `Cache-Control: no-store`. 404 `no_payload`, 410 `payload_expired` (no row, nothing was read),
  404 `payload_gone`, 422 `payload_unreadable` (another capture key), 501 `gateways_not_configured`, 502
  `gateways_unavailable`; a view that fails after the row was written still has its row.
- **UI.** A request with a payload gets a "Captured payload" section: for the admin role a button, a plain statement that
  opening it is recorded under their name, and then the request and the answer as text (pretty-printed when JSON), each with a
  copy button and a notice when it was cut; for other roles only that one exists and until when; once expired, that it is gone.
- **Checked** by the sealing tests (round trip; ciphertext at rest with none of the text; another key, an altered wrapped key, a
  reference naming a media object, a path out, a longer length and a version all refused; the sweep deletes old payloads and
  nothing else, and the media sweep never touches one), the recorder tests (an opted-in client's request and answer in the
  document, none for another client, a long request cut and said to be), a real `serve` process (the reference reaches the
  control plane with no text anywhere in the export or the usage log; the object on disk holds none of the prompt, the answer or
  the client's name; the admin listener opens it with its token and with nothing else; altered and missing references and the
  main listener refused; a reload turning payload capture off and on; every refusal at load), the control plane's tests on
  SQLite and Postgres (the route, the audit row before the fetch and when the gateway cannot answer, each error, the list never
  holding the reference, the role, migration from every earlier schema version), and an end-to-end run of a real gateway into a
  real control plane in Chrome. Mutation-checked: the usage log keeping the reference, the audit row removed, the read role allowed.
- **Known limits.** A body past `max_payload_bytes` is cut (said so), so it cannot be cut into segments for 5c; raise the
  limit for the clients that need the content diff. The answer of a stream is the raw events. A payload whose write was dropped or
  failed has a reference that will not open (`payload_gone`).

### Review follow-ups (PR 304)

- **The open route is never open.** The admin listener must have a token whenever `payload_key` is set, not only while some
  client captures a payload: the route exists whenever the key and the store do, and the clients reload while the listener
  does not.
- **A capture secret is random bytes.** `hmac_key` and `payload_key` must be at least 32 random bytes, base64 encoded
  (`openssl rand -base64 32`), and a phrase is refused by name. A reference held by the control plane is an offline test of a
  guessed key, so the format, not a length, is what makes a guess hopeless.
- **Expiry is enforced where the key is.** The expiry is sealed into the reference (it is part of what the wrapped key is
  bound to, so editing it makes the reference not open) and a gateway refuses an expired reference whether or not the sweeper
  has deleted the object yet. The old window of up to ten minutes after `expires_at` is gone.
- **Retention does not depend on the worst object.** The sweep continues past a delete that fails and reports it, and lists
  only `capture/` (S3, Google Cloud Storage and Azure list that prefix alone), so a store with millions of other objects is
  not walked and its page limit cannot stop the sweep.
- **Waiting captures are bounded by bytes** (64 MiB, with the count), and eight writes overlap, so a slow store drops captures
  late and counts them, instead of holding gigabytes or dropping at twenty a second.
- **CPU and locks on the request path.** The request is serialized into its bounded copy and the serializer is stopped at
  the cap, so a huge body costs the time of its first `max_payload_bytes`; the recorder's lock is not taken per stream frame
  unless payload capture exists.
- **The record says how far its "who" can be trusted.** A `payload.viewed` row carries `via`: `oidc` (a verified person),
  `break_glass`, or `shared_token` (with one shared token and no identity provider the actor is a label the caller gave). With
  single sign-on every view names a verified subject.

## Slice 5c as built

- **One walker, so the diff shows what was compared.** `gateway::structure::segment_texts` is the same walker that makes the
  fingerprints, with each segment's canonical text kept beside its hash (a tee on the writer the hash is fed through). A test
  re-hashes every text and requires the keyed hash to equal the exported fingerprint, for each wire, so the segment a viewer
  reads is exactly the segment its fingerprint names, in the same place. The folded tail (past 2,048 segments) has no text.
- **The gateway's open answer carries the segments.** `POST /v1/serve/payload/open` answers `{"payload": {…}, "segments":
  [{kind, text}, …] or null}`; `segments` is `null` when the request was cut at the capture limit (a cut body cannot be
  segmented) or is not a body the walker reads. Nothing new is stored: the segments are cut from the opened payload on demand.
- **Control plane.** `GET /v1/requests/{attempt}/payload/diff` (admin only). From the stored fingerprints it finds the request
  before (as for 5a) and compares. Only for a **changed** relation does it open anything: it checks that both payloads exist and
  have not expired, writes a `payload.viewed` row **for each of the two attempts before anything is fetched**, has the
  gateways open both references at once, and answers with the segments around the first difference (two before, two after,
  from both requests, each with kind, text cut at 64 KiB and a `changed` flag). A request that only grew, repeated or shortened
  its prompt, or has no earlier one, opens nothing and records nothing; a missing or expired payload says so; a cut request
  says `request_cut`. `Cache-Control: no-store`, nothing kept.
- **UI.** In the request's prompt-structure section, for the admin role and a changed prompt with a captured payload: a button
  and a plain statement that both views are recorded under their name; then each segment of the window with the previous request
  above and this one below, the part that differs marked (what the two share at the start and at the end is left alone, so a
  changed timestamp marks only the digits that changed), text only.
- **Checked** by the alignment test above, a real gateway into a real control plane in Chrome (a system prompt whose timestamp
  moved: the digits marked, both views in the history), the control plane's tests on SQLite and Postgres (the window and the
  context, the two recorded views and none for unchanged prompts, each unavailable case, a failed fetch still recorded, no
  gateways, the 64 KiB cut, the role), and a real `serve` process (the open answer's segments, and none for a cut request).
  Mutation-checked: only one view recorded, the text not cut.
- **Not here.** A word-level diff inside a segment beyond the shared start and end, and comparison with a request other than the
  one just before it.

### Review follow-ups (PR 305)

- **The diff shows only the prompt the fingerprints named.** The opened segments must have the same number of segments, the same
  kinds, and texts of the sizes the stored fingerprints recorded; otherwise (a gateway whose walker has changed, a reference to
  another document) the answer is `unavailable: segments_do_not_match`. A first difference that falls in the folded tail, which
  has no text, is `segment_not_shown`.
- **`changed` is decided on the whole texts**, not the cut ones: two segments that first differ past 64 KiB still show as
  changed (with `cut`).
- **Every answer of the comparison is `no-store`**, errors and the ones with no text included.
- **The open answer carries segments only when asked** (`"segments": true`): a plain payload view does no segmentation.
- **The page shows every field that differs.** A segment is shown as `role: words` only when that is all it holds, and as the
  whole indented JSON otherwise; the marking never splits a character made of two code units; an empty window and the
  unavailable reasons each have their own sentence.

## Hardening after the advisor's review

Done in one change, because each piece closes the same path (who can read a payload, and what proof is left):

- **Its own credential.** `[capture] open_token` guards `POST /v1/serve/payload/open`; the admin token no longer does.
  The route needs an `[admin]` table, the token (at least 32 characters, not equal to the admin token), and
  `[logging] audit_log`. The control plane reads the token from a gateway entry's `payload_token_file`, which must
  differ from `token_file`; with none, opening is unavailable.
- **Gateway-side audit before the read.** Two records per opening: `requested` before the object is touched (a failed
  write is 503 `audit_unavailable` and nothing opens), then the outcome. Only a digest of the reference, the viewer
  label and `via` are recorded.
- **Verified identity by default.** Without `--allow-payload-views-without-sso` a payload view needs an OIDC identity.
  Honest limit: in break-glass shared-token mode the viewer is whatever label the caller sent.
- **The labels are advisory.** `viewer` and `via` come from the caller, so whoever holds `open_token` is the real
  authority; keep it in the control plane's `payload_token_file` and nowhere else. A verified subject that is long or
  not plain ASCII is recorded cut and with `?` for the unprintable, not as `unknown`.
- **Gateways sharing an object store share `ttl_secs`.** A sealed expiry is unreadable to the sweeper, so a replica
  with no capture settings sweeps at the 7-day maximum, and one with them at its own TTL.
- **Request only by default.** `[capture] response = true` also keeps the provider's answer, raw and unredacted.
- **Sweeper independent of capture.** It runs whenever `[media_store]` exists, so turning capture off never strands
  sealed objects; `llmshim_payload_swept_total` and `llmshim_payload_sweep_last_success_timestamp_seconds` show it works.
