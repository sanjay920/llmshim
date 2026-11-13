# 0012: Reloading caller keys without a restart

Status: implemented for `[[keys]]`. Nothing else in the file reloads.

## What it is

An operator revokes a leaked key, adds a team, or raises a tenant's limit by
editing the configuration file (or a secret file it references) and the running
server picks the change up. No restart, no dropped connection, no reset of any
limit.

```toml
[reload]
watch = true          # also look for changes on a timer (default true)
interval_secs = 30    # 1 to 86400 (default 30)
```

`SIGHUP` always asks for a reload (on platforms that have it), and applies it at
once. Installing that handler means a hangup no longer ends the process, as it
does for any daemon that reloads on it: a terminal closing does not stop a
foreground server. The handler is installed before the application is built, so
a hangup that arrives while the server is starting is held until it can be acted
on, not taken as the default action. The timer reads the file and every secret
file a key references, and does nothing unless the result differs from what is in
force. The server prints one line per applied reload, one per distinct
rejection, and a "no change" line only when a signal asked.

**A poll applies a change only after two polls in a row have seen exactly the
same one.** A file that is rewritten in place can be caught half written, and a
half-written file can parse as a smaller, valid set of keys (cut at a `[[keys]]`
boundary) or as a key without its limits. Applying that would revoke callers or
drop a cap until the next check. The first sighting is deferred and a different
file the next time starts over; a signal never defers. A change therefore takes
two intervals to arrive by polling. Write the file by replacing it (write a
temporary file and rename it over the original), which is also what makes the
read atomic; a configuration manager or `kubectl` projected volume does.

## The rules

- **Keys only.** Providers, listeners, TLS paths, the admin listener, fleet
  settings, limits, state, logging, routes and `[reload]` itself are held by
  objects that cannot be swapped safely (the router sits behind an `Arc` in
  semver-protected code). They are fixed at start.
- **All or nothing.** The new file must pass the checks `serve --check` applies
  to the configuration and its keys: shape, unknown fields, referenced secrets
  present, caller-key rules (length, whitespace, duplicates, client-certificate
  rules). The checks that read secrets are the caller keys' own, and the rule
  that a caller token must not equal an upstream key, which skips an upstream
  secret that cannot be read right now: a provider, admin or Redis secret that is
  unreadable at that moment must not stop a revocation from applying. TLS files
  are not reloaded here; they have their own watcher (0007). Then everything outside
  `[[keys]]` must equal what the server started with, compared as parsed
  documents, so formatting and comments do not matter. A difference is refused
  **by path** (`limits`, `server.port`, `providers.openai.base_url`); the report
  names the path and never a value. If anything fails, nothing changes and the
  previous keys keep serving.
- **Content, not bytes.** What is compared is the resolved result: each key's
  token digest and identity. A rotated `{ file = "..." }` secret changes the
  result though the configuration file is byte-identical, and a comment edit
  changes the file though not the result. `{ env = "..." }` references are read
  from the process environment, which cannot change while it runs, so rotating
  one needs a restart.
- **Authentication cannot be turned on or off.** `server.allow_unauthenticated`
  is outside the keys, so a reload cannot open the server; the engine also
  refuses it directly, so a later change to the rules cannot.

## What is swapped, and how

One shared handle, `LiveKeys`, holds the key store. The gateway's admission, the
access log and the client-certificate binding (0011) all consult the same
handle, so one swap changes what all three see. Each request is decided against
exactly one version, once, when it arrives: a key revoked by a reload is refused
on the next request, including on a connection that was already open (HTTP/1.1
keep-alive and HTTP/2 alike). A request already admitted keeps going; it was
authenticated under the keys of its time.

A workload added with `client_san` is recognized at once, because the binding
asks the live store, not a copy.

## What survives a reload

Rate buckets, spend windows, idempotency answers, queued jobs, breakers and
receipts are not touched. A reload is not a limit reset.

- **A changed rate limit reaches the bucket that already exists.** Time up to the
  reload is credited at the old rate; a lowered limit caps what was saved at
  once; a raised one refills at the new rate from what is there. Nothing is
  granted instantly. (The fleet's Redis script already took the limit on every
  call.) This is on for `serve` only. The rate bucket is per tenant and provider,
  not per key, so `llmshim gateway` (where two keys of one tenant may carry
  different limits and the first to arrive fixes the shared bucket) keeps exactly
  the behaviour it has always had; retuning there would let a 2-per-minute key
  borrow a 1200-per-minute key's refill rate whenever the two alternate. Under
  `serve` the keys of one tenant must carry the same `rpm` and `tpm`, which
  `--check` enforces, so no two of them can pull one bucket in different
  directions.
- **A changed budget** applies to the same window: the cap comes from the key on
  each request and the window's ledger is keyed by tenant and window length. A
  changed `budget_window_secs` starts a fresh window under its new key.
- **A renamed tenant** is a different tenant: its buckets and ledger start empty
  under the new name. Rename a key (its `name`) freely; the credential's scope is
  its token digest, which is unchanged.

## Usage records during a reload

A usage record names its tenant from a table of tenant names that only grows. A
reload adds the names of new tenants and forgets none, so an attempt admitted
under a tenant that a reload then removed, or one a fleet replica dispatches
before that replica has reloaded, still resolves where the replica knows the
name; where it does not (a tenant a replica has not yet heard of) the record's
`tenant` is `null`, a defined value, and the record is never dropped. Replicas
reload on their own schedule, so a fleet is briefly mixed after an edit: a
revoked key is refused by each replica from the moment that replica reloads.
Shorten `interval_secs`, or `SIGHUP` every replica, when that window matters.

## Seeing that it worked

A failed reload that nobody sees would leave a revoked key live, so the outcome
is recorded where an operator looks. With `[admin]` configured,
`GET /v1/serve/config` (behind the admin token, never on the main listener)
returns:

```json
{
  "generation": 3,
  "config_sha256": "…",
  "credentials": 12,
  "loaded_at_epoch_secs": 1767225600,
  "last_check_epoch_secs": 1767225630,
  "in_sync": false,
  "last_reload": {
    "at_epoch_secs": 1767225610,
    "outcome": "rejected",
    "issues": [{"path": "limits", "message": "cannot change while the server is running; restart to apply"}]
  }
}
```

`generation` is 0 for the configuration the process started with. Alert on
`in_sync == false` (the running keys are not what the file says: the file was
rejected, or a valid change is waiting to be seen twice), or on a `generation`
that is not the one the deploy expects. `in_sync` turns true again when the file
is accepted, applied, or put back to what is running, so reverting a bad edit
clears the alert; `last_reload` keeps the history of what was rejected and why.
The same facts go to stderr.

## Deployment

The Helm chart (0008) does not rely on reload. It rolls pods when the
configuration changes, and the secrets it mounts are environment variables,
which a running process cannot see change. A deployment that keeps keys in
mounted files, or hashed keys in a ConfigMap, can use reload instead of a
rollout; the pods then pick up the kubelet's updated files within the polling
interval plus the kubelet's sync delay.

## Not done

- Reloading anything but keys (providers, routes, limits) needs the router and
  rate limiter to be swappable behind the semver rule.
- A reload cannot make a fleet agree instantly; see above.
- The access log resolves the caller before the handler runs, so a request that
  straddles the swap by microseconds can be attributed to the previous version
  while the gateway decided with the next.
