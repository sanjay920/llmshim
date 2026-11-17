# 0033: Secrets from a secret manager

Status: built (one slice).

## Decision

`serve` links **no secret-manager client** (no AWS, GCP, Azure or Vault SDK, no HTTP fetch of a
secret). A secret reference stays `{ env = "NAME" }` or `{ file = "/path" }` (note 0001), and the
supported integration is the **file**: every manager has a way to put a secret in a file the process
can read, and the file is already what rotates without a restart (note 0022).

Why not a native client: each adds a dependency tree and a release cadence to a gateway that has
neither, each needs a credential of its own to start (the bootstrap problem moves, it does not go
away), and each is a new failure mode on the request path or at reload. A manager's own agent or
driver already solves it, is maintained by the manager's vendor, and is what a security review
expects to see.

## Patterns that work today

| Manager | How the secret reaches a file |
| --- | --- |
| Kubernetes Secrets | a `secret` volume; the kubelet swaps the file's symlink when the Secret changes |
| External Secrets Operator, Sealed Secrets | they write a Kubernetes Secret; mount it as above |
| Secrets Store CSI driver (AWS, GCP, Azure, Vault providers) | the driver mounts the secret as a file and rotates it |
| Vault Agent, `consul-template`, cloud-provider agents | a template renders the value to a path |
| Docker / Compose / Swarm secrets | mounted under `/run/secrets/<name>` |

Point `{ file = "/mounted/path" }` at the file. Only `[providers.*]` secrets rotate on a reload;
everything else is read at start (note 0022), so rotating a caller key, the admin token or the Redis
URL is a rolling restart.

## What this note changes

A file or variable that holds a secret is now held to the same rule everywhere (`read_secret`, the
one reader that `serve --check`, boot, reload, keys, JWT, telemetry and snapshot pull all use):

- **At most 64 KiB.** A wrong path (a device, a log file) is never read into memory whole: the read
  stops one byte past the cap and the file is refused, by path.
- **Valid UTF-8, one line.** Surrounding whitespace is trimmed (a mount's trailing newline), but a
  value with an interior newline, a control character, a byte-order mark (it survives trimming, and a
  Windows editor writes it) or a Unicode line separator is refused: it would split an HTTP header or a
  URL. A file that is not UTF-8 is refused the same way. The message names the variable or file, never
  the value.

These refusals are a defect in what was configured, so `serve --check` reports them with status 1
(`check_secret_values`), not 3. Status 3 stays what it was, a secret that is unset, blank or unreadable
(a fact about the environment, which a pipeline may retry); a value that is present but malformed will
not get better by retrying. Boot and reload refuse the same values through the same reader.

## Not built

- Fetching from a manager's API, caching, or leasing dynamic secrets.
- Rotation of non-provider secrets without a restart.
- Checking file permissions or ownership (the mount's policy, not ours; a check that fails on one
  platform's volume layout would be wrong on another's).

## Tests

`config::tests::a_secret_is_one_bounded_line_and_the_error_never_holds_the_value`: a trailing newline
is accepted, an interior newline and a tab are refused without echoing the value, the file cap is exact
(at the cap accepted, one over refused, also when the cut falls inside a multibyte character), a BOM, a
separator, CRLF, NUL and non-UTF-8 are malformed, an absent file stays unavailable, and an environment
value is held to the same rule. Disabling either the control-character check or the size check fails it.
`unit_serve_cli::a_secret_that_is_present_but_not_one_line_is_a_defect_not_an_absence` pins status 1 and
the absence of the value from the output; dropping `check_secret_values` fails it. Environment values have
no size cap (the OS bounds them).
