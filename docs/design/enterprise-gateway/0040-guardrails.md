# 0040: Request guardrails

Status: slice 1 built (request-side pattern rules). With residency (0039) it is the first half of the roadmap
item "guardrail pipeline; residency policy". Response-side checks, redaction and external classifiers are not
built.

## The problem

An organization needs to stop some text from leaving for a provider at all: a cloud credential pasted into a
prompt, a private key, a customer identifier. The gateway sits where every request passes, before anything is
reserved, queued or sent, which is where such a rule belongs.

## The rule

```toml
[[guardrails]]
name = "cloud-keys"
tenants = ["platform"]            # optional: omit for every caller
block_if_matches = ["AKIA[0-9A-Z]{16}", "-----BEGIN [A-Z ]*PRIVATE KEY-----"]
```

- A request whose text matches **any** pattern of **any** applicable rule is refused with
  `400 guardrail_blocked`, `"request blocked by guardrail <name>"`, **before admission**: nothing is reserved,
  queued, sent or recorded as a provider attempt (there is no usage record), and
  `llmshim_gateway_rejected_total{reason="guardrail"}` counts it by provider.
- **The refusal names the rule and nothing else.** The pattern, and the text it matched, are never returned,
  logged, counted or traced. The rule's name is the operator's own and is not secret.
- **What is scanned.** Every string in the request as the provider will see it (the prepared payload: messages,
  content parts, system prompts, tool-call arguments, tool definitions, embeddings `input`, an image `prompt`,
  speech `input`, a transcription `prompt`), on every route: chat, `/v1/messages`, `/v1/responses`, the Gemini
  facade, streaming, and the four media routes. **Not scanned:** structural keys (`model`, `role`, `type`, `id`,
  `tool_call_id`, `call_id`, `finish_reason`), a string beginning `data:` (an inline image or audio), the
  transcription audio itself, and the *response*.
- **Cost.** All the patterns that apply to a caller are compiled into **one set per scope** (one for the rules
  that apply to everyone, one per tenant that has rules of its own), so a request is scanned once per scope in a
  single pass rather than once per rule; the earliest rule in the file that matches is the one named. The scan runs
  on the request's own task and is not offloaded: its work is bounded by the request size (2 MiB on every route
  that parses JSON) and the combined set, and a unit test pins a 2 MiB near-match against 512 patterns at well
  under a second. The set is built at start; a combination too large to compile together (64 MiB for a scope) is a
  configuration error by scope.
- **Patterns** are Rust `regex` syntax: matching is linear in the text, with no look-around or back-references,
  so neither a hostile pattern nor hostile text can stall a request. A pattern is compiled once at start under a
  1 MiB size limit. There are 1 to 64 patterns per rule, each at most 1024 characters, and at most 64 rules.
- **`tenants`** names tenants that some key belongs to; a tenant that is not any key's is a configuration error
  (a rule that silently never fires is a mistake). **With `[control_plane]` the keys, and so the tenants, come from
  the signed snapshot after the file is read, so this cannot be checked** and a rule for a tenant the snapshot does
  not have never fires (the same silence the check prevents); write the tenant as `[tenants.<name>]` or check the
  snapshot. Unset applies to every caller.
- **Fixed at start.** A reload that changes `[[guardrails]]` is refused by path like any section a reload does
  not replace. Rules are never edited under traffic.
- **Fail closed on configuration.** A pattern that does not compile, a duplicate or malformed name, an empty rule
  and a rule for an unknown tenant stop `serve --check` (status 1) and start, by path, never echoing the pattern.

## How it is built

`GatewayState::check_guardrails` is asked in the two places every route funnels through: `build_request_for`
(chat, every facade, streaming, failover retries; on the *prepared* payload, so one scan covers each provider's
own shape) and `submit_media_call` (embeddings, images, speech, transcription). The rules sit behind a
`RequestGuard` trait the `serve` build installs once from its file (`src/serve/guardrails.rs`); a build without
`serve` has no field and no check. The `regex` crate is a new **optional dependency enabled only by `serve`**
(it was already in the lockfile transitively), so default, `proxy`, `gateway` and `gateway-redis` builds are
unchanged; the control plane's lockfile was refreshed.

## Not built

- **Response guardrails** (checking what the model says) and **streaming** response filters.
- **Redaction** (rewriting instead of refusing) and **allow-lists** (`allow_only`).
- **External classifiers or moderation APIs** (a network call in the request path, with its own failure modes).
- Per-key rules (tenant is the scope today), per-route or per-model scoping, and a dry-run mode that counts
  without refusing.
- Scanning images, audio and other non-text content.

## Tests

`serve::guardrails::tests` (unit): a match names the rule and nothing else; text is found in nested content
parts, a system string, tool arguments and an input array, while a matching `model` and a `data:` URL are not
scanned; a tenant-scoped rule applies to that tenant only. `integration_serve_guardrails` (process): a clean
request is served and matching ones are refused 400 on chat, streaming, `/v1/messages` and embeddings without
echoing the secret; a blocked request leaves no usage record, is counted by reason and its text is in no log; a
rule for an unknown tenant and an invalid pattern are refused by path without echoing the pattern. Disabling the
check fails two of them. `serve::reload::tests::guardrails_are_a_fixed_section...` pins that a reload cannot
change them.
