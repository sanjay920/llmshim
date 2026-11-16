# 0019: Published, signed images and an SBOM

Status: implemented, and **off until an owner turns it on**. Bar item 7 of 0013:
images are signed (verifiable with `cosign verify`) and carry an SPDX SBOM. A
contract note, written before the workflow (README working rule 3).

## What an operator gets

One image per release, built from the repository's `Dockerfile` with the feature
set a deployment of `llmshim serve` needs:

```text
ghcr.io/<owner>/llmshim:<X.Y.Z>        # one immutable tag per release, no "latest"
```

- **Features:** `serve-redis,serve-otel`, so one image runs a single instance, a
  fleet (a `[fleet]` table) or with traces (a `[telemetry]` table), selected by the
  configuration file and not by the image. `llmshim proxy` and `llmshim gateway`
  are in it too.
- **Platform:** `linux/amd64`. An `arm64` image needs a native builder (building
  Rust under emulation takes hours) and is a later change.
- **Signature:** a keyless cosign signature on the image digest, made with the
  publishing workflow's own OIDC identity. There is no signing key to store, rotate
  or leak.
- **SBOM:** an SPDX JSON document generated from the pushed image (syft), attached
  as a signed cosign attestation of type `spdxjson`.
- **No tag is the identity.** The signature and the attestation are on the
  **digest**; a tag is a pointer, and a consumer should pin the digest it verified.

### Verifying

```sh
IMAGE=ghcr.io/<owner>/llmshim@sha256:<digest>
cosign verify "$IMAGE" \
  --certificate-identity-regexp '^https://github.com/<owner>/llmshim/\.github/workflows/images\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
cosign verify-attestation --type spdxjson "$IMAGE" \
  --certificate-identity-regexp '^https://github.com/<owner>/llmshim/\.github/workflows/images\.yml@refs/tags/v' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The identity names the workflow file and a tag ref, so a signature made from a
branch, a fork or another workflow does not verify.

## Rules

- **Off by default.** The publish job runs only when the repository **variable**
  `PUBLISH_IMAGE` is `true`. A tag pushed without it builds and publishes nothing.
  Turning it on is the repository owner's decision (a repository variable, not a
  file in the tree), and so is the first push to GHCR; nothing here does either.
- **Only a release tag.** The workflow triggers on `v*.*.*` tags and on nothing
  else (a manual dispatch would sign with a branch identity, which the verification
  above would rightly refuse); it refuses a tag whose name is not
  `v<Cargo.toml version>`, so an image cannot be published from a stale tag.
- **Least privilege.** The job has `contents: read`, `packages: write` (to push to
  GHCR with the run's own token) and `id-token: write` (for keyless signing), and
  nothing else. No long-lived registry credential exists.
- **Verified before it counts.** After signing, the job runs the two `cosign verify`
  commands above against the digest it just pushed, with the workflow's own
  identity; a signature that does not verify fails the job.
- **The feature set builds on every pull request** that touches the Dockerfile, the
  manifests or this workflow (`docker-check.yml`): the image is built with the same
  `FEATURES` and `serve --print-schema` is run inside it, so a release is not the
  first time the published feature set meets the distroless runtime.
- **Reproducibility is not claimed.** The build pins the toolchain image by tag;
  digest-pinned base images and a reproducible-build claim are separate work.
- **Third-party actions are pinned by major tag**, as the rest of the repository's
  workflows are; pinning by commit is a repository-wide change, not this one.

## Not decided here

- An `arm64` image, a `latest` tag, and a Helm chart default that points at the
  published image (the chart keeps `repository: llmshim`, which an operator sets).
- Retention of old image versions in GHCR, and package visibility. Both are
  repository settings the owner controls.
- Signing the other release artifacts (the CLI binaries, the packages).
