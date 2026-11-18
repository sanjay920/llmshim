# 0014: What happens when a dependency fails

Status: a standing record, extended with every step that adds or changes a
dependency (0013, bar item 1).

## Why a table

An unattended caller cannot ask what a refusal meant. For each thing `llmshim
serve` depends on, this says what a replica does when it is down, slow or wrong,
and which test fails if that stops being true. A row with no test says so: the
rows in "Not yet tested" are not claims.

The rule behind the rows (0013): on the inference path, identity and money fail
closed (a request whose caller cannot be identified, or whose spend cannot be
reserved, is refused); telemetry fails open (a log sink or exporter never blocks
or fails a request). At startup the opposite holds: a dependency that is needed
and absent stops the start, with a status that says which kind of absence it is.

## Tested

Each row names its test. "Process" means a real `llmshim serve` process was
started and the dependency broken under it; "unit" means the component alone.

| Dependency and event | What the replica does | Test |
| --- | --- | --- |
| Redis unreachable at start (fleet) | exits 1 before serving; the message names `fleet.redis_url`, never the URL or its password | process: `integration_serve_fleet::an_unreachable_redis_fails_the_start_without_leaking_the_url` |
| Redis lost while serving | a new request is refused with a 5xx at once (under a millisecond in the recorded run; the test allows 3 s, well inside the 5 s Redis deadline). Every fleet request goes through the Redis queue, so it cannot be queued: it is **never admitted** and the provider is not called (the mock must have been hit exactly once, before the outage). `/ready` answers 503, `/health` stays 200, the replica stays up | process: `integration_serve_outage::redis_outage::losing_redis_refuses_new_work_and_recovery_needs_no_restart` |
| Redis lost while a request is with the provider | the caller is told the request failed (a 5xx; a 503 after 5.3 s in the recorded run; the test allows 30 s) and is not left waiting for an answer that cannot be delivered. The replica stays up. *The 503's body says `gateway shutting down`, which is the wrong words for a lost Redis; the status is the right one to retry on.* | process: `…::losing_redis_mid_request_ends_the_call_promptly_and_the_replica_recovers` |
| Redis returns | the replica serves again with no restart, and `/ready` answers 200 (0.6 s after the connection returned in the recorded run; the tests allow 60 s) | both Redis tests above |
| `SIGTERM`, one replica | with a request in flight (held by the provider until the test releases it) both listeners, main and admin, stop accepting at once while the request is still pending; the request then finishes with its answer; the process exits 0 and says it received the signal | process: `integration_serve_outage::sigterm_lets_an_in_flight_request_finish_then_exits_zero` |
| A provider answers 500 | the client retries; each attempt is its own usage record (`http_failure`, unknown cost, never free); the caller gets a 5xx | process: `integration_serve::every_failed_attempt_is_recorded_with_an_unknown_cost` |
| A secret is unset, blank or unreadable at start | exits 3, naming the setting; a secret whose value breaks a rule exits 1 and outranks it | process: `unit_serve_cli` |
| A reload that does not load, or changes something fixed at start | refused by path, nothing changes, the previous keys, providers, routes and limits keep serving | process: `integration_serve_reload`. That `/v1/serve/config` then says `in_sync: false` is pinned by the `serve::reload` unit tests |
| A reload adds a provider whose secret file is unreadable, or whose Vertex service-account file is missing, empty or malformed | the whole reload is rejected by path (`providers.b.api_key`); no other change in the same edit applies; no secret is in the report | unit: `serve::reload::tests::a_provider_that_cannot_be_built_rejects_the_whole_reload_and_echoes_no_secret`, `a_service_account_file_that_is_missing_or_empty_rejects_the_reload` (needs `serve-cloud`) |
| A provider key rotates (the secret file changes, the configuration text does not) | the next request uses the new key; a request already admitted finishes on the key it began with | process: `integration_serve_reload::a_rotated_provider_key_file_is_used_by_the_next_request`; unit: `a_rotated_file_key_is_used_by_the_next_request_and_not_by_one_in_flight` |
| A provider limit is lowered by a reload | applies to the next attempt in every holder, keeps the tokens (not a reset) | process: `integration_serve_reload::a_lowered_provider_limit_applies_to_the_next_request_and_keeps_the_tokens`; unit: the in-memory limiter, `LocalAttemptRates`, and the Redis limiter and attempt rates against a Redis |
| A fleet replica reloads a provider set | workers start for an added provider and stop leasing for a removed one (a leased job runs in its own task and keeps its permit; the tests pin the start and the stop, not an in-flight job) | process: `integration_serve_fleet::a_provider_added_by_a_reload_is_served_by_the_replica_from_then_on`; unit against Redis: `synced_workers_serve_an_added_provider_and_stop_leasing_for_a_removed_one`. Both are `#[ignore]`d without Redis and run in the Redis CI jobs |
| A fleet job reaches a replica whose router does not know its provider | 503 `provider_unavailable` with `Retry-After`, never 502. The failure is at provider resolution, before any attempt exists (by construction: the test pins the status, header and code, not the absence of a reservation or breaker observation) | unit: `a_replica_without_the_provider_answers_503_provider_unavailable` |
| A policy snapshot that does not verify, does not advance, or is not a snapshot at all, on a reload | refused by path with a fixed message (no content, no digest), the previous snapshot's keys keep serving, `in_sync: false` | process: `integration_serve_snapshot::a_snapshot_that_does_not_verify_or_advance_never_changes_what_serves` |
| A replica restarts and finds an older snapshot (or the same generation with different content) than the one it applied | exits 1, naming the setting; it never serves the older one | process: `integration_serve_snapshot::the_applied_generation_survives_a_restart` |
| The policy snapshot gets old (the control plane or its distributor stops) | with `max_age_secs`, `/v1/serve/config` reports `snapshot.stale: true`; the last good snapshot keeps serving, inference included, unless `refuse_when_stale` is set, which refuses new inference with 503 `Retry-After: 30` while `/health`, `/ready` and the operational endpoints keep answering; a new generation restarts the age | process: `integration_serve_snapshot::a_stale_snapshot_is_reported_and_with_refuse_when_stale_inference_is_refused`, `a_stale_snapshot_without_refuse_when_stale_only_alerts` |
| A signing key is removed from `verify_keys` while its snapshot is in force | the reload is refused (the snapshot on disk no longer verifies), what applied keeps serving, `snapshot.signer_untrusted: true` until a trusted signer's snapshot replaces it | process: `integration_serve_snapshot::verify_keys_reload_and_a_removed_signer_is_reported_not_obeyed` |
| The control plane is down, wrong or sends something that does not verify (a pulled snapshot) | the replica keeps serving the cached snapshot; `snapshot.last_pull_error` says why in fixed text (no token, no body); nothing unverified is ever cached or applied; a restart with a cache serves it, a start with none exits 1 | process: `integration_serve_snapshot::an_unreachable_or_wrong_control_plane_never_changes_what_serves_and_is_reported`, `a_pulled_snapshot_that_does_not_verify_is_refused_and_never_cached`, `a_wrong_token_is_reported_without_echoing_it`, `start_without_a_cache_needs_the_control_plane_and_with_one_it_does_not` |
| A TLS replacement that does not load | the previous certificate stays in service | process: `integration_serve_tls::a_replacement_that_does_not_load_keeps_the_previous_certificate` |
| The admin port is taken at start | exits 1 before serving, naming the listener | process: `integration_serve::a_taken_admin_port_fails_the_start_cleanly` |
| A log path that cannot be written at start | exits 1 naming `logging.access_log` or `logging.usage_log`; nothing is created | process: `integration_serve::an_unwritable_access_log_fails_the_start_with_the_setting_named` |
| A log sink that stalls or fails while serving | the writer queue absorbs it; overflow is dropped and counted, a failed write is counted, neither reaches the request | unit: `serve::jsonl` (`a_stalled_destination_never_blocks_the_caller_and_overflow_is_counted`, `a_failing_destination_is_counted_and_never_reaches_the_caller`) |
| A token issuer's JWKS is unreachable at start or goes away while serving | at start nothing fails and `/ready` is unaffected; a token of that issuer is refused with 503 `jwks_unavailable` and `Retry-After`, while bearer keys and probes keep answering and the provider is not called for the refused token. Keys already loaded keep verifying (they are fresh for `jwks_refresh_secs`, then stale for up to `jwks_max_stale_secs` while a refresh fails), so an issuer that goes away after the first token costs nothing for that time | process: `integration_serve_jwt::an_unreachable_issuer_fails_its_tokens_only_and_never_a_bearer_key`, `…::loaded_keys_keep_verifying_while_the_issuer_goes_away`; the schedule (fresh, stale served at once while a background refresh runs, unavailable, retry floor) and the two availability rules (a caller that disconnects neither aborts the fetch nor costs the issuer the retry interval; a hanging issuer holds no caller that has usable stale keys) are unit-tested in `serve::jwks` on a paused clock |
| A Bedrock profile that does not exist, or a cloud identity the build refuses | the request fails with an error naming the profile (or the setting), never the SDK's text or a path; it fails **before admission**, so no attempt is recorded (the usage log, which has one line per admitted attempt, gains none). Admission is where budget and rate capacity are taken (0010), so none is taken: that half follows from the order and is not separately tested. The replica stays up, `/ready` stays 200 and the other providers keep answering after it. At start, a Vertex service-account file that is missing or empty is exit 3 and a malformed one exit 1. An Azure key the build refuses (interior whitespace) stops the start naming `api_key`; **a wrong but well-formed Azure key or Vertex account is the provider's rejection after the attempt was admitted**, recorded like any provider failure | process (in the `serve-cloud` build): `integration_serve_cloud::a_bedrock_profile_that_does_not_exist_fails_the_request_cleanly` (the usage log, `/ready` and a following request on another provider); configuration: `serve::config` unit tests and `unit_serve_cli::a_vertex_service_account_is_status_3_when_absent_and_status_1_when_malformed` (missing, empty and malformed). No test fails a Vertex service account at request time: its token exchange is a fixed Google host |
| Redis is lost with `[fleet.outage_admission]` and a budgeted key | the replica notices after three failed probes, then serves the key from its lease (a request is re-scoped to a ledger of its own limited to the lease), `/ready` stays 200 while a lease is held, status and `/metrics` say so (`outage_admission`, `llmshim_gateway_outage_admission_active`), usage records carry `admission: "outage"`; when Redis has answered steadily the running requests drain, the lease is drawn at what the local ledger spent and released, the replica takes a new lease and returns to the fleet path | process: `integration_serve_outage::redis_outage::a_budgeted_key_is_served_from_its_lease_while_redis_is_gone` |
| The same, with an unbudgeted key, an unpriceable attempt, a tenant with no lease, or a lease within 30 s of its window's end | refused: `prepare_outage` maps every refusal to 503 with `Retry-After`, before a request is built, so nothing reaches the provider (the mapping is by construction; the tests below pin the refusals themselves, not the HTTP answer) | unit (real Redis for the `outage` ones): `gateway::outage::tests::nothing_is_admitted_inside_the_window_margin` (the margin) and `…::an_outage_is_served_from_the_lease_and_settled_on_recovery` (no lease); the unbudgeted, unpriced and invalid refusals are `gateway::attempt::tests::only_a_priced_budgeted_key_has_an_outage_view` |
| A flapping Redis with the mode on | the replica stays in outage state and on its lease until Redis has answered steadily for `recover_after_secs`; no settlement starts | unit (real Redis, cuttable proxy): `gateway::outage::tests::a_flapping_redis_keeps_local_admission_on_the_lease_and_settles_nothing` |
| Recovery that is interrupted | every draw is idempotent per attempt id, so the next pass resumes and nothing is charged twice; a lease with a Redis failure is left whole and reported pending | unit (real Redis): `gateway::lease::tests::an_interrupted_recovery_resumes_to_the_uninterrupted_result`, `…::recovery_with_redis_down_leaves_everything_pending` |
| A second outage in the same window | the replica holds a new lease of its own (a lease epoch) and a local ledger of its own; the first outage's spend is charged once | unit: `gateway::outage::tests::a_second_outage_in_the_same_window_has_its_own_lease`, `gateway::attempt::tests::each_outage_epoch_has_its_own_local_ledger` |
| A tenant has a `[tenants.<name>]` cap and a key has its own cap too, and Redis is lost with outage admission on | that key is refused (the second lease is not built); a key under the same tenant with no cap of its own is served from the tenant's lease | unit: `gateway::attempt::tests::outage_admission_follows_the_counter_the_ledger_holds`; process: `integration_serve_outage::redis_outage::a_key_under_a_tenant_cap_is_served_from_the_tenants_lease_while_redis_is_gone` (the refusal of a key with its own cap is pinned at the scope level, not by a process test) |
| Two budget levels race for the same capacity (a fleet) | both counters move together or neither, neither passes its cap, no reservation is orphaned | unit (real Redis): `gateway::attempt::tests::redis_racing_requests_never_leave_either_counter_over_its_cap`, `redis_two_levels_are_judged_together_and_a_refusal_holds_nothing` |
| A deployment's target has an open circuit | the target is skipped and its share goes to the others; when every target is open health is ignored (no local refusal); a target past its cooldown is probed so it can recover | unit: `unit_deployment::round_robin_alternates_and_an_open_circuit_is_skipped`, `…::a_target_past_its_cooldown_is_probed_so_it_can_recover` (the selector asks the local breaker only; fleet-shared health is not visible at resolution) |
| An embeddings request reaches a fleet gateway, or a key with a spend cap embeds | a fleet answers 501 (a job there is chat); a capped key with no trusted input price for the model is refused `unpriceable_under_budget` (the opt-in does not admit it), and with one it reserves the bound in 0035 and a cap below it is a 429; neither reaches the provider | process: `integration_serve_embeddings::a_key_with_a_spend_cap_is_refused_until_embeddings_have_a_spend_bound`; the fleet 501 is not tested (needs a Redis fleet) |
| An image generation reaches a fleet gateway, or a key with a spend cap generates | a fleet answers 501; a capped key is refused `unpriceable_under_budget` (the opt-in does not admit it: no bound on the output); neither reaches the provider | process: `integration_serve_images::a_key_with_a_spend_cap_cannot_generate_images_even_with_the_opt_in`; the fleet 501 is not tested (needs a Redis fleet) |
| A speech synthesis reaches a fleet gateway, or a capped key asks for more than it has left | a fleet answers 501; the reservation is the request's exact price, so a request above the remaining cap is a 429 before the provider and a model with no rate is refused `unpriceable_under_budget` | process: `integration_serve_speech::one_request_bigger_than_the_cap_is_refused_before_the_provider`, `::a_capped_key_reserves_the_exact_price_and_is_stopped_when_the_cap_is_spent`; the fleet 501 is not tested (needs a Redis fleet) |
| A transcription upload is larger than the limit, or the key has a spend cap | an oversized upload is a 413 while it streams in (it is never buffered whole and never queued); a capped key is refused `unpriceable_under_budget`; a fleet answers 501; none reaches the provider | process: `integration_serve_transcription::an_upload_over_the_limit_is_413_before_it_is_queued`, `::a_key_with_a_spend_cap_cannot_transcribe_even_with_the_opt_in`; the fleet 501 is not tested (needs a Redis fleet) |
| A residency-restricted key addresses a provider outside its labels (or one with no label) | refused 403 `model_not_allowed` before anything is reserved or queued, on every route; a reload that relabels a provider swaps the key store | process: `integration_serve_residency::a_key_in_one_residency_reaches_its_providers_only_and_the_others_never_see_the_request` |
| A request's text matches a guardrail rule | refused 400 `guardrail_blocked` naming only the rule, before anything is reserved, queued or sent (no usage record, counted as `rejected_total{reason="guardrail"}`); the pattern and the text are never echoed or logged; a reload cannot change the rules | process: `integration_serve_guardrails::a_matching_request_is_refused_by_rule_name_and_a_clean_one_is_served`, `::a_blocked_request_leaves_no_usage_record_and_is_counted_by_reason` |
| Embeddings reach a fleet whose replicas are being upgraded | the job's scope is version 3 with its operation; a replica from before fails it with 503 (terminal, not released to another) rather than running it as chat, so the failing share is the old workers' share; a fleet batch is capped at 512 texts (400); a current replica sends the embeddings call and the origin answers its caller; images, speech and transcription answer 501 on a fleet | process (needs Redis): `integration_serve_fleet::embeddings_run_through_the_fleet_and_are_answered_whichever_replica_dispatches`, `::the_other_media_routes_still_answer_501_on_a_fleet`; the old binary's refusal is its own check, not exercised |
| A provider's answer matches a guardrail rule that reads responses (a stream is filtered as it runs: 0046) | the answer is replaced by 422 `guardrail_blocked_response` naming only the rule (the attempt is recorded as completed and paid, nothing cached, no failover) | process: `integration_serve_guardrails::an_answer_that_breaks_a_response_rule_is_replaced_by_a_422_naming_the_rule`, `::a_clean_answer_is_returned_and_a_request_rule_does_not_read_it` |
| A request carries text a redaction rule masks | the provider receives the replacement and never the original (message content, parts and tool-call arguments; structure and inline media untouched), on every chat route including the streaming ones; redaction runs before blocking rules, which still judge what is left; the rewrite is counted by rule name only | process: `integration_serve_guardrails::a_redaction_rule_sends_the_provider_the_masked_text_not_the_secret`, `::redaction_and_blocking_compose_and_a_rule_cannot_do_both` |
| A provider's answer carries text a response redaction rule masks | the caller receives the masked answer with a 200 (message content, refusal, tool-call arguments, a transcript; reasoning and the envelope untouched), the attempt is recorded as completed and paid, the cached answer is the masked one, a match redaction could not reach is refused 422, and a stream by that caller is filtered as it runs (0046) | process: `integration_serve_guardrails::a_response_redaction_rule_masks_the_answer_the_caller_receives` |
| A streamed answer carries text a response rule matches | the text is held back by 256 bytes, so a redaction match split across chunks is masked whole and a blocking match cuts the stream with an `error` event naming the rule before the secret is released (a match longer than the hold-back can leak its first bytes); tool calls are masked and judged whole; everything else streams as sent | process: `integration_serve_guardrails::a_stream_is_masked_across_chunks_for_every_route_that_streams`, `::a_stream_that_breaks_a_blocking_rule_is_cut_with_an_error_event_and_leaks_nothing` |
| The audit log is damaged, tampered with, or cannot be written | a log whose last record is incomplete or altered, a symlink or a directory refuses the start (`logging.audit_log`); `llmshim audit verify` names the first broken line of an edited, removed, reordered or forged record; a failed write counts `llmshim_audit_write_failures_total`, is reported and closes the log (no appends after a half record); records removed from the end are caught only by the anchored head (`/v1/serve/config` `audit`) | process: `integration_serve_audit::tampering_is_named_by_the_verifier_and_a_damaged_log_stops_the_server_from_starting`; unit: `serve::audit::tests` |
| A reload changes or removes a deployment target | the next request is resolved on the new router; a request already admitted finishes on its snapshot | process: `integration_serve_deployments::a_reload_changes_the_targets_and_unknown_names_are_a_400` |
| A key lists a deployment's targets but not the deployment | requests that pick a listed target are served and the others are refused 403 before the provider; a key that lists the deployment name is allowed every target | process: `integration_serve_deployments::the_allowlist_sees_the_pick_and_a_list_naming_the_deployment_allows_every_target` |
| A deployment's target answers 5xx or the connection fails | a non-streaming request, or a stream that fails to open or whose first item is a provider failure, before any byte reaches the caller (a fleet too, 0042), is retried on the next untried target (at most three attempts, each target once), each attempt admitted and recorded on its own under one request id; a 4xx, a 429 and any local refusal are returned as they are; if the retry may not be made (the key's list) the original failure is returned | process: `integration_serve_deployments::a_5xx_from_the_first_target_is_retried_on_the_next_and_each_attempt_is_recorded`, `…::a_400_or_a_429_is_not_retried_elsewhere`, `…::every_target_is_tried_once_and_the_last_failure_is_what_the_caller_sees`, `…::a_retry_the_key_may_not_make_returns_the_original_failure_not_a_403` `::a_stream_that_fails_to_open_is_retried_on_the_next_target`, and on a fleet (needs Redis) `integration_serve_fleet::a_stream_that_fails_to_open_on_a_fleet_is_retried_on_the_next_target` |
| A settlement that cannot be written to the spend ledger | the liability the attempt reserved is kept, not released | unit (against a real Redis): `gateway::attempt::tests::redis_failed_settlement_keeps_the_acquired_liability` |

The numbers are one run on a developer machine, to show the order of magnitude;
the tests assert the bounds, not the figures.

## Not yet tested

Each of these has behavior (often documented elsewhere) but no test that breaks
the dependency, so none of it is a promise yet.

- **Outage admission, the edges.** Not tested: a replica killed during an outage
  (its lease stays in the shared counter until the window ends, by design); a
  window boundary crossed while a request runs (the local ledger never rolls, so
  the lease is the limit, but no process test crosses one); spend by an attempt
  that starts or ends after the drain timeout (not read again); more than one
  tenant under one replica in the process test; and a Redis that is slow rather
  than gone.

- **The money path in isolation under Redis loss.** The Redis-loss tests refuse
  at the queue, which every fleet request needs, so they do not reach the spend
  ledger or the rate limiter. Two components are documented as failing **open**
  when Redis is unreachable: the shared rate limiter used by the proxy
  (`RedisRateLimiter`: "admits the request and logs") and the older per-tenant
  `SpendStore` ("a Redis failure loses the charge rather than the request").
  0006 says spend and rate coordination fail closed. Which of these `serve` can
  reach has to be established and the answer tested, or 0006 corrected; until
  then this note claims only what the rows above show.
- **`SIGTERM` in a fleet.** A replica drains its own requests but does not
  release the leases it holds for peers' jobs, which wait out the lease timeout
  (0006); no two-process test. Also untested: `SIGTERM` while a provider hangs
  (the drain has no deadline of its own), and a signal that arrives between the
  banner and the handler being installed.
- **The provider circuit breaker** at the server level. The library tests pin
  its states; the cooldown (30 s by default) makes a process-level test slow, and
  the breaker's settings are not in the configuration file yet. Likewise a
  provider that refuses connections, hangs, or answers 429.
- **Redis slow rather than gone**, a partition between replicas rather than
  between a replica and Redis, a Redis that restarts with its data lost, one that
  rejects authentication or answers `READONLY`, and a request streaming when
  Redis is lost. The outage tests cut every connection; a stalled one exercises
  the operation deadlines instead.
- **A replica killed with `SIGKILL`** while it holds leases. Library tests cover
  origin crash and lease expiry against a real Redis; there is no two-process
  test.
- **The main listener's port being taken at start** (the admin listener's is
  tested).
- **The receipts directory becoming unwritable**, the **configuration or a
  secret file being removed or rotated** while the server runs, **log rotation**,
  **a full disk under a log**, **wall-clock changes under spend windows and
  certificate validity**, and **DNS**.

0013's fifth stability criterion needs this section empty (or each item here
turned into a row with a test).

## How the Redis tests cut Redis

`RedisProxy`, in `tests/integration_serve_outage.rs`, forwards TCP to the real
Redis and can cut every open connection and refuse new ones, then restore. A
replica sees what it would see if Redis, or the network to it, went away. The
replica is pointed at the proxy, so the real Redis is untouched; but a replica
abandoned in the middle of a request leaves a lease that other tests' replicas
would sweep (the origin-lease index is Redis-wide), so these tests run one at a
time (`--test-threads=1`) in the `serve-redis-check` job, after the fleet tests.
