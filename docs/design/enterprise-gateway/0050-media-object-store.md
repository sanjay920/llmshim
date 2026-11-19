# 0050: Media through a fleet, over an object store

Status: slice 1 (the store), slice 2 (the fleet routes) and slice 3a (the AWS credential chain) built; slices 3b to 3d
(Google Cloud Storage, Azure Blob, the Helm examples) are described and not built. Written on the owner's instruction to
start it, with the requirement that it plug into Kubernetes and the native cloud services, not one of them.

## The problem

With `[fleet]`, embeddings travel through Redis (0041). Images, speech and transcription answer `501` on a fleet
because their payloads or answers are megabytes (a transcription upload up to 25 MB, a generated image's base64, a
synthesized audio file) and Redis is neither the place nor, past its terminal limit, able to carry them. A fleet
that needs them must today leave the fleet.

## The decision, in one paragraph

Large strings in a job or an answer are **moved to an object store** and replaced by a small reference; the Redis
machinery (queue, leases, bus, terminal budgets, 0014's failure rows) is untouched because it only ever sees small
JSON. The store is **pluggable**: one `BlobStore` trait with a backend per environment, all configured in one
`[media_store]` table, so the same gateway runs on a laptop, on Kubernetes with a shared volume or an in-cluster S3,
and on AWS, GCP and Azure with their own services. Every object is **encrypted by the gateway before it is stored**,
so the bucket (and its credentials, lifecycle and access logs) never sees a caller's audio or images in the clear.

## Backends

```toml
[media_store]
kind = "file"                      # file | s3  (slice 3: gcs | azure-blob)
max_object_bytes = 67108864        # 64 MiB per object, up to 256 MiB
ttl_secs = 3600                    # an object older than this is an orphan: the sweeper deletes it
# kind = "file"
path = "/var/lib/llmshim/media"    # a directory every replica can read and write
# kind = "s3"
bucket = "llmshim-media"
region = "us-east-1"
endpoint = "https://minio.media.svc:9000"   # omit for AWS S3
path_style = true                  # default: true with an endpoint, false without
prefix = "llmshim/"                # optional key prefix, so a shared bucket can be shared
access_key_id = { env = "MEDIA_S3_KEY_ID" }
secret_access_key = { file = "/var/run/secrets/media/secret" }
session_token = { env = "MEDIA_S3_TOKEN" }  # optional
ca_file = "/etc/llmshim/minio-ca.pem"       # optional: a private CA for the endpoint
allow_insecure_http = false
```

| Where it runs | Backend | Notes |
| --- | --- | --- |
| One machine, a developer, Compose | `file` | a local directory |
| Kubernetes, shared volume | `file` | a `ReadWriteMany` PVC (EFS, Filestore, Azure Files, NFS, CephFS) mounted at `path` |
| Kubernetes, in-cluster object store | `s3` | MinIO, Ceph RGW (Rook), SeaweedFS |
| AWS | `s3` | S3 with an access key (slice 1), or with `credentials = "aws"` (slice 3a, built): EKS IAM roles for service accounts (IRSA), EKS Pod Identity, ECS task roles, EC2 instance profiles, SSO and profile files, through the AWS credential chain |
| GCP | `s3` or `gcs` | Cloud Storage's S3-compatible API with HMAC keys is the vendor's documented interoperability path (not tested by us); native `gcs` with workload identity (slice 3) |
| Azure | `azure-blob` | Blob Storage with workload identity or a SAS token (slice 3); Azure has no S3 API |
| Elsewhere | `s3` | Cloudflare R2, DigitalOcean Spaces, Backblaze B2, Wasabi, Alibaba OSS: anything that speaks S3 |

**No SDK in the core build.** The `file` and `s3` backends add no dependency: S3's request signing (SigV4) is HMAC-
SHA256 over a canonical request, written with `ring`, which `serve` already links, and the calls are four plain
HTTP operations (`PUT`, `GET`, `DELETE`, and a paginated `ListObjectsV2` for the sweeper) on the HTTP client the
gateway already has. The cloud-identity backends and the AWS credential chain use the SDK crates that `serve-cloud`
already links for Bedrock, Vertex and Azure, so a build without cloud features stays as lean as it is (0020, 0033).
Credentials are secret references (`{ env }` or `{ file }`, 0033): a **file** is re-read at most once a minute (and
at once after the store refuses a request, since a rotation may have landed between two files), so a rotated key
file or a projected Kubernetes secret takes effect without a restart; an **environment variable** is read once, as
it cannot change in a running process.

## What is stored, and how it is protected

- **Whole large strings, not files.** Any string of **32 KiB or more** in a job payload or an answer (the base64 of an
  audio upload, an image, synthesized audio) becomes an object; every smaller string stays where it is. The object is
  the string's bytes (base64 text: a third larger than the media, in exchange for no per-route decoding).
- **Sealed before it leaves the process.** AES-256-GCM (`ring`), a fresh random 256-bit key and 96-bit nonce **per
  object**, the object's name as the associated data. The reference that replaces the string carries the key and the
  nonce: it travels in the Redis job or answer, the channel that already carries the same caller's prompts. The
  bucket holds ciphertext only, so a bucket policy mistake, a public listing, a backup or an access log leaks
  nothing, and **deleting the reference is deleting the data** (an orphan whose reference is gone cannot be read).
- **Names say nothing.** `<UTC date>/<128 random bits, hex>` under the backend's own prefix or directory: no tenant, key, model or caller text; the date
  only lets an operator (and a lifecycle rule) age objects.
- **The reference:** `{"$blob": {"v": 1, "key": "...", "len": N, "k": "<key>", "n": "<nonce>"}}`. A reader
  checks `len`, the tag, and the name it asked for, and an object that fails any of them is an error, never partial data.
- **Bounded.** One object is at most `max_object_bytes` (a `400` before anything is sent for a larger upload), and
  one job or answer moves at most sixteen objects (an image request may ask for ten).

## Lifecycle: nothing outlives its request

1. The origin replica seals and puts the request's large strings, enqueues the job with references.
2. A worker reads them back (one `GET` each), dispatches, and seals and puts the answer's large strings.
3. The origin reads the answer's objects, **then deletes every object of the request** (its own and the answer's),
   after the caller has the response or the request has ended in any way.
4. **Orphans** (a replica died mid-request, a delete failed) are removed by the sweeper (`blobstore::sweep` is built in
   slice 1; it is scheduled with the routes in slice 2): every replica sweeps every
   10 minutes, deleting objects older than `ttl_secs` (default one hour, which must exceed the longest request: the
   configuration refuses anything under 600 seconds). Deletes are idempotent, so
   replicas sweeping at once is harmless. For a bucket, also set a **lifecycle rule** that expires the prefix after a
   day: the gateway's sweep is the prompt cleanup, the lifecycle rule is the backstop that needs no process.
5. A job redelivered after a lease expiry (0014) finds its objects still there: they are deleted at step 3, never
   by a worker, so at-least-once delivery cannot find a hole.

## Failure modes (added to 0014 with slice 2)

- **The store is down or slow** when the origin stages a request: `503 media_store_unavailable` with `Retry-After`,
  before anything is queued or sent to a provider. On a worker (a read fails): the job fails with the same, not
  retried on a worker that cannot read either. The provider is never paid for a call whose input was lost.
- **The answer's objects cannot be read** by the origin: `502`, and the call **was** paid for (it is recorded as completed
  with its tokens and cost, as 0043 records a blocked answer); the objects are left to the sweeper.
- **Credentials rotate or expire:** a file is re-read once a minute and after a refusal (one retry); a refusal that
  persists is the same `503`, counted by class. An unreadable credential file is `Unavailable`, not `Denied`.
- **The store loses an object:** a read of a missing object is an error naming nothing of the caller's.
- The store is **not** a queue and holds no state the fleet needs after a request: losing the whole bucket loses
  only requests in flight.

## Slices

1. **The store (built).** `serve::blobstore`: the trait, the `file` and `s3` backends, sealing, the references,
   the sweep function, `[media_store]` and its validation (`--check` reads and validates; the store is built at start,
   so a wrong path or an unreadable credential fails there), a MinIO-backed CI job (`serve-s3-check`). No route uses it
   yet: a configured `[media_store]` is built and held, nothing more.
2. **The fleet routes (built).** With `[fleet]` and `[media_store]`, `POST /v1/images/generations`,
   `/v1/audio/speech` and `/v1/audio/transcriptions` run through the shared queue like a chat completion. Details in
   "Slice 2" below.
3. **Cloud identities and native backends**, in four parts:
   - **3a. The AWS credential chain (built).** `credentials = "aws"` on an `s3` store. See below.
   - **3b. Google Cloud Storage**, native (`kind = "gcs"`): the JSON API with an OAuth token from the metadata
     server (GKE Workload Identity, GCE) or a service-account file. Not built.
   - **3c. Azure Blob Storage** (`kind = "azure-blob"`): the Blob REST API with a SAS token, an account key, or an
     Entra token (workload identity, managed identity). Not built.
   - **3d. The Helm chart's examples:** a shared `ReadWriteMany` claim for `file`, a service account annotated for
     IRSA / Workload Identity / Azure Workload Identity, and the media store settings. Not built.

## Slice 3a: the AWS credential chain

```toml
[media_store]
kind = "s3"
bucket = "llmshim-media"
region = "eu-west-1"
credentials = "aws"          # no access_key_id, secret_access_key or session_token
```

With `credentials = "aws"` the store signs with whatever the AWS credential chain finds, in the SDK's own order:
environment variables; **IRSA** (the projected service account token in `AWS_WEB_IDENTITY_TOKEN_FILE`, traded at STS for
the role in `AWS_ROLE_ARN`: set by the service account's `eks.amazonaws.com/role-arn` annotation); **EKS Pod Identity and
ECS task roles** (the container credentials endpoint, `AWS_CONTAINER_CREDENTIALS_FULL_URI` with its authorization token);
**SSO and profile files**; and the **EC2 instance profile** (IMDS). Nothing needs to be configured in the file beyond
the region; the Kubernetes or ECS service account or the instance profile carries the identity, with no key in a Secret.

- **Explicit, not ambient.** The provider transports (Bedrock, Vertex, Azure) refuse an ambient identity on purpose:
  a gateway serves many callers and must not let any of them borrow the host's. The object store is the operator's own
  infrastructure, where the pod's role is exactly the identity wanted, so the chain is used when, and only when, the
  file says `credentials = "aws"`. A file that does not say so never consults it.
- **Built with `serve-cloud`.** It uses the SDK crates that `serve-cloud` links for Bedrock. A build without it refuses
  the setting by name (`media_store.credentials`, in `--check` and at start), and the default build is unchanged.
- **Kept until it expires.** Temporary credentials are held until five minutes before they expire (a credential that
  lives less than that is replaced almost at once); credentials with no expiry (environment or profile keys) are asked
  for again every minute, so a rotated profile file is seen. A request that the store **refuses** (a revoked or rotated
  role) drops what is held and is retried once with a fresh answer, as for key files.
- **Errors.** No credentials anywhere in the chain is `Denied`; a source that fails or takes more than 30 seconds is
  `Unavailable`. Neither carries the SDK's message.
- **No credential I/O at construction:** the chain is built and asked at the first request, so `serve --check` and a
  start never contact STS or the metadata service.
- IAM: the role needs `s3:PutObject`, `s3:GetObject`, `s3:DeleteObject` and (for the sweeper) `s3:ListBucket` on the
  bucket and prefix. Add a lifecycle rule that expires the prefix after a day as the backstop.

## Slice 2: how the routes use the store

- **Origin.** After authentication, the allowlist, guardrails and classifiers (which see the plaintext) and the
  route's cheap refusals, the origin replica moves the payload's large strings to the store (an upload's base64 is
  the one that matters), and enqueues the job. A reference in a *caller's* body is refused `400` before anything is
  staged: only a replica makes one, so a caller can never name an object. The staged objects are deleted when the
  call ends **however** it ends: normally, on an error, or because the caller disconnected (a dropped request deletes
  them from a task its drop starts).
- **Scope version 4.** **Every** fleet job on these three routes is marked scope **version 4** when the store is in
  use, small or large: its answer may come back as references, and an older replica that ran it would return the
  megabytes inline through Redis after the provider was paid. A replica from before this change accepts versions 1 to 3 only, so it **refuses**
  the job (`503 coordinator unavailable`, the safe failure of 0041) instead of sending a reference to a provider as the
  caller's data. As in 0041: upgrade every replica before relying on staged media through the fleet.
- **Worker.** Reads the references back into the payload, calls the provider (the same dispatch, usage record, spend
  observation and breaker as one instance), and moves the answer's large strings (an image's base64, synthesized
  audio) to the store, so only references travel back through Redis. A worker with no store (or a store that has lost
  the object) fails the job `503`, before any provider is called.
- **Origin again.** Reads the answer's objects, answers the caller, and deletes the request's objects, its own and the
  worker's.
- **Refusals and statuses.** No `[media_store]` on a fleet: `501`, naming `[media_store]`. The store cannot take the
  upload (or a worker cannot read it): `503 media_store_unavailable` with `Retry-After: 30`; nothing was sent to a
  provider. The upload exceeds `max_object_bytes`: `400`. The store cannot carry the *answer* (a worker cannot put it, or the origin cannot read it back): `502
  media_store_unavailable` **without** `Retry-After`, because the call was paid for (and is recorded as completed) and a
  retry would pay again; an answer larger than `max_object_bytes` fails this way every time. The store's own error text and the
  object names are never in an answer.
- **One instance ignores it.** A server without `[fleet]` keeps its payloads in memory whatever `[media_store]` says
  (it has no replica to hand an object to); the store is built and validated, and unused.
- **The sweeper runs** on every replica of a fleet, every ten minutes (the first pass after up to a minute, so
  replicas do not sweep in the same second), deleting generated-shape objects older than `ttl_secs`.
- **Counters.** `llmshim_gateway_media_store_total{op="stage|read|write|delete|sweep",outcome="ok|<class>"}` and
  `llmshim_gateway_media_store_swept_total`. Both labels are fixed sets.

## Known limits (found in review)

- **Answer objects of an abandoned request** (the origin timed out, or the caller left before the worker answered) are
  not tracked by the origin, so they wait for the sweeper (up to `ttl_secs` and a pass). A redelivered duplicate run
  can leave answer objects the same way.
- **Memory.** A 25 MB upload (33 MB of base64) exists at the origin as the body, the base64 string and the sealed
  copy at once, and again at the worker. Strings are moved, not copied, into the sealing; chunked sealing would
  reduce the rest.
- **Staging precedes admission:** a request that is then refused by a quota or the spend rules still costs a put and
  a delete. Deletes are sequential and inside the response path.
- The origin deletes only names of the generated shape that it finds in an answer; a reference-shaped value a
  hostile provider puts in `usage` cannot name another object for deletion.

## Not built (and not planned)

- Streaming a large object without holding it in memory: an object is sealed and opened whole (AEAD over a buffer),
  which is why `max_object_bytes` exists. Chunked sealing is the next step if 256 MiB is not enough.
- Pre-signed URLs, handing a caller a link to the object, public objects, or server-side encryption keys (the
  gateway's own sealing replaces them).
- Deduplication, caching or reuse across requests; an object belongs to one request.

## Tests

Slice 3a (`integration_serve_aws_chain`, in a process of its own because the chain reads the real environment; each test
sets one world and holds a lock; the sources are mock servers): environment keys; nothing to find is `Denied`; **EKS Pod
Identity / ECS** (a container credentials endpoint that demands the projected authorization token); the **EC2 instance
profile** (a metadata service that hands out a session token, a role name and credentials); **IRSA** (the projected
token traded at a mock STS for `AssumeRoleWithWebIdentity`, the role ARN and token in the request); a credential inside
its renewal margin is asked for again; a failing source is `Unavailable`; and the store signs with the chain's
credentials, is refused, drops them, and signs again with the new ones. Not covered: credentials with no expiry are
kept for a minute (a wait too long for a test), and the SSO and profile-file links are the SDK's own.

Slice 2 (`integration_serve_fleet`, two real replicas, a Redis and a directory they share): all three routes answer
through the fleet on whichever replica takes the job, with an upload reaching the provider whole (60 KB of a known
pattern, counted by the mock), a 100 KB audio answer byte for byte and a 60 KB image; the store is empty afterwards
and the counters show two uploads staged and four answers put; while a request is in flight the object in the store
is ciphertext (neither the audio nor its base64) under a name of a date and 128 random bits, and the job in Redis
carries a reference and scope version 4, not the audio; a store that cannot take the upload is `503` with
`Retry-After` and the provider is called zero times; a reference in a caller's body is `400`; a provider that fails
leaves nothing behind; a caller that disconnects leaves nothing behind; without `[media_store]` a fleet answers 501
naming it. Unit tests: a worker restores a payload and moves a large answer, passes a failed call through untouched,
and a worker that cannot read a reference fails `503` naming no object; a scope with references is version 4 and
round-trips; the background sweeper deletes an old orphan and keeps a fresh one. A single instance with a store
configured writes nothing to it (`integration_serve_media_store`).

Slice 1: unit (`serve::blobstore::tests`): SigV4 against the signing examples AWS publishes (the canonical request,
string to sign and signature of a GET and a PUT), a sealed object opens and any flipped bit, swapped name, wrong
length or wrong key refuses, references round-trip through a JSON value and only strings of 32 KiB or more move,
the file backend (put, get, delete, sweep by age, path escape refused, atomic replace); an S3 mock (mockito) sees
signed requests and answers each failure class; the configuration rules by path. A real **MinIO** run (an
`#[ignore]`d test on `LLMSHIM_TEST_S3_ENDPOINT`, run in CI beside the Redis tests) proves put/get/delete/list and the
signature against a server that really verifies it.
