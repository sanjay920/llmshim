# 0050: Media through a fleet, over an object store

Status: slice 1 (the store) built; slices 2 and 3 are described and not built. Written on the owner's instruction to
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
| AWS | `s3` | S3 with an access key (slice 1); IRSA, EKS Pod Identity, ECS and EC2 roles through the AWS credential chain (slice 3) |
| GCP | `s3` or `gcs` | Cloud Storage's S3-compatible API with HMAC keys is the vendor's documented interoperability path (not tested by us); native `gcs` with workload identity (slice 3) |
| Azure | `azure-blob` | Blob Storage with workload identity or a SAS token (slice 3); Azure has no S3 API |
| Elsewhere | `s3` | Cloudflare R2, DigitalOcean Spaces, Backblaze B2, Wasabi, Alibaba OSS: anything that speaks S3 |

**No SDK in the core build.** The `file` and `s3` backends add no dependency: S3's request signing (SigV4) is HMAC-
SHA256 over a canonical request, written with `ring`, which `serve` already links, and the calls are four plain
HTTP operations (`PUT`, `GET`, `DELETE`, and a paginated `ListObjectsV2` for the sweeper) on the HTTP client the
gateway already has. The cloud-identity backends and the AWS credential chain use the SDK crates that `serve-cloud`
already links for Bedrock, Vertex and Azure, so a build without cloud features stays as lean as it is (0020, 0033).
Credentials are secret references (`{ env }` or `{ file }`, 0033) and are **re-read at most once a minute**, so a
rotated key file or a projected Kubernetes secret takes effect without a restart.

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
  one job or answer moves at most four objects.

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
- **Credentials rotate or expire:** re-read once a minute; a `403` from the store is the same `503`, counted by class.
- **The store loses an object:** a read of a missing object is an error naming nothing of the caller's.
- The store is **not** a queue and holds no state the fleet needs after a request: losing the whole bucket loses
  only requests in flight.

## Slices

1. **The store (built).** `serve::blobstore`: the trait, the `file` and `s3` backends, sealing, the references,
   the sweep function, `[media_store]` and its validation (`--check` reads and validates; the store is built at start,
   so a wrong path or an unreadable credential fails there), a MinIO-backed CI job (`serve-s3-check`). No route uses it
   yet: a configured `[media_store]` is built and held, nothing more.
2. **The fleet routes.** The job's scope gains the media operations (scope version 4, with 0041's transition rule),
   the origin stages and the worker dispatches `images`, `speech` and `transcription`; the `501` becomes a real
   answer when `[media_store]` is configured (and stays a `501` that names the setting when it is not).
3. **Cloud identities and native backends.** The AWS credential chain for `s3` (IRSA, Pod Identity, ECS/EC2),
   `gcs` (workload identity, via `serve-cloud`'s Vertex identity), `azure-blob` (workload identity or SAS, via the
   Azure identity), with `serve-check`-style fixtures; the Helm chart's PVC and service-account examples.

## Not built (and not planned)

- Streaming a large object without holding it in memory: an object is sealed and opened whole (AEAD over a buffer),
  which is why `max_object_bytes` exists. Chunked sealing is the next step if 256 MiB is not enough.
- Pre-signed URLs, handing a caller a link to the object, public objects, or server-side encryption keys (the
  gateway's own sealing replaces them).
- Deduplication, caching or reuse across requests; an object belongs to one request.

## Tests

Slice 1: unit (`serve::blobstore::tests`): SigV4 against the signing examples AWS publishes (the canonical request,
string to sign and signature of a GET and a PUT), a sealed object opens and any flipped bit, swapped name, wrong
length or wrong key refuses, references round-trip through a JSON value and only strings of 32 KiB or more move,
the file backend (put, get, delete, sweep by age, path escape refused, atomic replace); an S3 mock (mockito) sees
signed requests and answers each failure class; the configuration rules by path. A real **MinIO** run (an
`#[ignore]`d test on `LLMSHIM_TEST_S3_ENDPOINT`, run in CI beside the Redis tests) proves put/get/delete/list and the
signature against a server that really verifies it.
