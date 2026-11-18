# 0042: Streaming failover through a fleet

Status: built. It closes the limits recorded under 0031's "Streaming failover".

## The gap

0031 retries a deployment stream that fails to open on the next target, but only where the open is what the
handler waits for. That is true of a single instance. With `[fleet]` the origin enqueues the job and the handler
gets its receiver as soon as the job is **accepted**; an upstream open failure arrives later, as an error item on
that receiver, so the handler never saw it as a failure to open and did not retry. For the same reason an error
that arrives as the first item of an opened stream (a managed stream that fails while it buffers, say) was not
retried on either backend.

## The rule

A request addressed to a deployment that still has a target it may try (the bound of 0031: fewer than
`min(targets, 3)` tried, an untried target left) is judged by **its first item as well as by its open**. The
handler reads the first item before it sends the response head:

- an **error** item that is a provider failure of the kind 0031 retries (a 5xx or a transport failure, never a
  local diagnostic, a 4xx, a 429 or a refusal) is treated exactly as a failed open: the next target is tried as a
  new submission with its own admission, spend and usage record, the failover metric is counted when the retry
  has been prepared, and a retry that cannot be made returns the original failure;
- **anything else** (a data chunk, a non-retryable error, the end of the stream) is **forwarded first**, in order,
  to the caller, and the rest of the stream follows. Nothing is lost or reordered.

A request that did not address a deployment, or whose targets are all tried, is not read ahead: the response head
goes out as soon as the stream is accepted, as before. A stream whose first byte has been sent is never retried.

**A side effect to avoid.** Whether a deployment stream has an untried target is asked of every such stream,
including the ones that succeed, so it is a pure lookup (`Router::deployment_has_untried_target`) and never the
retry selection, which reserves the next target's circuit probe: a probe taken for a target that is then never
tried would stall its recovery (found in review; `unit_deployment` pins it).

**Statuses.** A retry that cannot be made after a retryable first-item error (the key's list refuses the next
target, no permit) returns the original failure as an HTTP error response (5xx), as a failed open on a single
instance always did; on a fleet that case used to be a 200 with an SSE `error` event. A non-retryable first-item
error is still forwarded as an SSE `error` event on a 200.

The cost is that a deployment stream's response head waits for the first item (the first token's latency, which the
caller would wait for anyway) when a retry is possible; a caller that relied on seeing headers before any token from
a deployment stream sees them with the first event instead.

## Not built

- Retrying after the first item: once a byte has gone out the caller has the start of an answer, and a second
  target's would be a different one.
- Images, speech and transcription through a fleet (0041).

## Tests

`integration_serve_fleet::a_stream_that_fails_to_open_on_a_fleet_is_retried_on_the_next_target` (two replicas and a
Redis): an ordered deployment whose first target answers 502 and whose second streams; both replicas' requests
get the second target's stream with no error event, and each target is hit twice. Not reading the first item
fails it. `unit_deployment::asking_whether_a_target_is_untried_never_reserves_its_probe` pins the pure lookup (routing it
through the selector fails it). The single-instance stream tests of 0031 (`integration_serve_deployments`) pass unchanged: the
open-failure path is the same code.
