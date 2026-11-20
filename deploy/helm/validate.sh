#!/usr/bin/env bash
# Validate the chart: lint it, render it in several configurations, check the
# manifests against the Kubernetes schemas, and feed each rendered gateway
# configuration back through `llmshim serve --check` so the chart and the
# server cannot drift apart.
#
# Needs docker, openssl and python3. LLMSHIM_BIN is a binary built with
# `--features serve-redis,serve-cloud` (default: target/debug/llmshim): serve-cloud because the
# media store examples include the AWS credential chain.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
chart="$here/llmshim"
bin="${LLMSHIM_BIN:-$here/../../target/debug/llmshim}"
helm_image="alpine/helm:3.16.2"
kubeconform_image="ghcr.io/yannh/kubeconform:v0.6.7"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

helm() { docker run --rm -v "$here:/charts:ro" "$helm_image" "$@"; }

echo "== helm lint"
helm lint /charts/llmshim
helm lint /charts/llmshim -f /charts/llmshim/ci/full.yaml
for example in "$chart"/ci/media-*.yaml; do
  helm lint /charts/llmshim -f "/charts/llmshim/ci/$(basename "$example")"
done

# A self-signed pair for the TLS variant, and secrets for every reference.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$work/tls.key" -out "$work/tls.crt" -days 1 -subj "/CN=127.0.0.1" \
  -addext "subjectAltName=IP:127.0.0.1" 2>/dev/null
export OPENAI_API_KEY=provider-key-openai-0123456789 ANTHROPIC_API_KEY=provider-key-anthropic-0123456789
export EXAMPLE_GATEWAY_KEY=example-caller-key-0123456789 CHECKOUT_GATEWAY_KEY=checkout-caller-key-0123456789
export GATEWAY_ADMIN_TOKEN=admin-token-0123456789abcdef REDIS_URL=redis://127.0.0.1:6379/0
# The media store examples' secrets (their values are never printed).
export MEDIA_S3_ACCESS_KEY_ID=media-key-id-0123 MEDIA_S3_SECRET_ACCESS_KEY=media-secret-0123456789
export MEDIA_AZURE_SAS_TOKEN='sv=2021-12-02&sp=rwdl&sig=abc'

variants=(default full)
for example in "$chart"/ci/media-*.yaml; do variants+=("$(basename "$example" .yaml)"); done

for variant in "${variants[@]}"; do
  echo "== render [$variant]"
  args=()
  [ "$variant" != default ] && args=(-f "/charts/llmshim/ci/$variant.yaml")
  helm template release /charts/llmshim ${args[@]+"${args[@]}"} > "$work/$variant.yaml"

  echo "== kubeconform [$variant]"
  docker run --rm -v "$work:/m:ro" "$kubeconform_image" -strict -summary \
    -kubernetes-version 1.30.0 "/m/$variant.yaml"

  echo "== serve --check on the rendered configuration [$variant]"
  python3 - "$work/$variant.yaml" "$work/$variant.toml" "$work" <<'PY'
import sys, re
text = open(sys.argv[1]).read()
match = re.search(r"^  gateway\.toml: \|\n((?:    .*\n|\n)+)", text, re.M)
if not match:
    sys.exit("no gateway.toml in the rendered ConfigMap")
body = "".join(line[4:] if line.startswith("    ") else line for line in match.group(1).splitlines(True))
work = sys.argv[3]
# Point the pod's mount paths at files that exist here.
body = body.replace("/etc/llmshim/tls/tls.crt", f"{work}/tls.crt")
body = body.replace("/etc/llmshim/tls/tls.key", f"{work}/tls.key")
body = body.replace("/etc/llmshim/tls/ca.crt", f"{work}/tls.crt")
body = body.replace("/var/lib/llmshim/receipts", f"{work}/receipts")
body = body.replace("/var/lib/llmshim/media", f"{work}/media")
open(sys.argv[2], "w").write(body)
PY
  LLMSHIM_CATALOG_OFFLINE=1 "$bin" serve --config "$work/$variant.toml" --check
done

echo "== each media store example renders what its platform needs"
has() { grep -q -- "$2" "$work/$1.yaml" || { echo "[$1] expected: $2"; exit 1; }; }
lacks() { ! grep -q -- "$2" "$work/$1.yaml" || { echo "[$1] unexpected: $2"; exit 1; }; }
has media-file-rwx "kind: PersistentVolumeClaim"
has media-file-rwx "ReadWriteMany"
has media-file-rwx "mountPath: /var/lib/llmshim/media"
has media-file-rwx 'path = "/var/lib/llmshim/media"'
has media-s3-minio 'endpoint = "https://minio.minio.svc.cluster.local:9000"'
has media-s3-minio 'access_key_id = { env = "MEDIA_S3_ACCESS_KEY_ID" }'
has media-s3-minio 'ca_file = "/etc/llmshim-media-ca/ca.crt"'
has media-s3-minio "name: minio-ca"
has media-s3-minio "mountPath: /etc/llmshim-media-ca"
has media-s3-irsa "eks.amazonaws.com/role-arn: arn:aws:iam::123456789012:role/llmshim-media"
has media-s3-irsa 'credentials = "aws"'
lacks media-s3-irsa "access_key_id"
has media-gcs-workload-identity "iam.gke.io/gcp-service-account"
has media-gcs-workload-identity 'credentials = "metadata"'
has media-gcs-service-account "name: gcp-service-account"
has media-gcs-service-account 'service_account_file = "/etc/llmshim-gcp/key.json"'
has media-gcs-service-account "mountPath: /etc/llmshim-gcp"
has media-azure-workload-identity 'azure.workload.identity/use: "true"'
has media-azure-workload-identity "azure.workload.identity/client-id"
has media-azure-workload-identity 'federated_token_file = "/var/run/secrets/azure/tokens/azure-identity-token"'
has media-azure-sas 'sas_token = { env = "MEDIA_AZURE_SAS_TOKEN" }'
lacks media-azure-sas "azure.workload.identity/use"
# Nothing about the media store leaks into a chart that does not use it.
for variant in default full; do
  lacks "$variant" "media_store"
  lacks "$variant" "azure.workload.identity"
  lacks "$variant" "PersistentVolumeClaim"
done

echo "== path_style: an explicit false is a value, not an absence"
s3=(--set existingSecret=s --set fleet.enabled=true --set mediaStore.enabled=true --set mediaStore.kind=s3
    --set mediaStore.s3.bucket=b --set mediaStore.s3.region=r --set mediaStore.s3.endpoint=https://store.example)
helm template release /charts/llmshim "${s3[@]}" > "$work/ps-unset.yaml"
helm template release /charts/llmshim "${s3[@]}" --set mediaStore.s3.pathStyle=false > "$work/ps-false.yaml"
helm template release /charts/llmshim "${s3[@]}" --set-string mediaStore.s3.pathStyle=false > "$work/ps-false-string.yaml"
helm template release /charts/llmshim "${s3[@]}" --set mediaStore.s3.pathStyle=true > "$work/ps-true.yaml"
! grep -q "path_style" "$work/ps-unset.yaml"
grep -q "path_style = false" "$work/ps-false.yaml"
grep -q "path_style = false" "$work/ps-false-string.yaml"
grep -q "path_style = true" "$work/ps-true.yaml"

echo "== the settings for private and sovereign clouds reach the configuration"
az=(--set existingSecret=s --set fleet.enabled=true --set mediaStore.enabled=true --set mediaStore.kind=azure-blob
    --set mediaStore.azureBlob.account=a --set mediaStore.azureBlob.container=cont
    --set mediaStore.azureBlob.tenantId=t --set mediaStore.azureBlob.clientId=c)
helm template release /charts/llmshim "${az[@]}" --set mediaStore.azureBlob.authorityHost=https://login.microsoftonline.us | grep -q 'authority_host = "https://login.microsoftonline.us"'
helm template release /charts/llmshim "${az[@]}" --set mediaStore.azureBlob.credentials=managed-identity --set mediaStore.azureBlob.identityEndpoint=http://169.254.169.254 | grep -q 'identity_endpoint = "http://169.254.169.254"'
helm template release /charts/llmshim --set existingSecret=s --set fleet.enabled=true --set mediaStore.enabled=true --set mediaStore.kind=gcs \
  --set mediaStore.gcs.bucket=b --set mediaStore.gcs.metadataEndpoint=http://169.254.169.254 | grep -q 'metadata_endpoint = "http://169.254.169.254"'

echo "== media store misconfigurations are refused at render time"
refuse() {
  local expect="$1"; shift
  if helm template release /charts/llmshim "$@" >/dev/null 2>"$work/err"; then
    echo "expected the render to fail: $*"; exit 1
  fi
  grep -q -- "$expect" "$work/err" || { echo "wrong error for $*:"; cat "$work/err"; exit 1; }
}
media=(--set existingSecret=s --set fleet.enabled=true --set mediaStore.enabled=true)
refuse "mediaStore needs fleet.enabled" --set existingSecret=s --set mediaStore.enabled=true --set mediaStore.kind=file --set mediaStore.file.claimName=c
refuse "mediaStore.kind must be one of" "${media[@]}"
refuse "mediaStore.kind must be one of" "${media[@]}" --set mediaStore.kind=ftp
refuse "mediaStore.file needs" "${media[@]}" --set mediaStore.kind=file
refuse "mediaStore.s3 needs bucket and region" "${media[@]}" --set mediaStore.kind=s3
refuse "reads its keys from existingSecret" --set fleet.enabled=true --set mediaStore.enabled=true --set mediaStore.kind=s3 --set mediaStore.s3.bucket=b --set mediaStore.s3.region=r
refuse "credentials must be keys or aws" "${media[@]}" --set mediaStore.kind=s3 --set mediaStore.s3.bucket=b --set mediaStore.s3.region=r --set mediaStore.s3.credentials=magic
refuse "mediaStore.gcs needs bucket" "${media[@]}" --set mediaStore.kind=gcs
refuse "needs serviceAccountSecret" "${media[@]}" --set mediaStore.kind=gcs --set mediaStore.gcs.bucket=b --set mediaStore.gcs.credentials=service-account
refuse "credentials: none needs an endpoint" "${media[@]}" --set mediaStore.kind=gcs --set mediaStore.gcs.bucket=b --set mediaStore.gcs.credentials=none
refuse "needs account and container" "${media[@]}" --set mediaStore.kind=azure-blob
refuse "needs tenantId and clientId" "${media[@]}" --set mediaStore.kind=azure-blob --set mediaStore.azureBlob.account=a --set mediaStore.azureBlob.container=c
refuse "mediaStore.prefix must end in /" "${media[@]}" --set mediaStore.kind=s3 --set mediaStore.s3.bucket=b --set mediaStore.s3.region=r --set mediaStore.s3.credentials=aws --set mediaStore.prefix=llmshim
refuse "mediaStore.prefix does not apply to kind file" "${media[@]}" --set mediaStore.kind=file --set mediaStore.file.claimName=c --set mediaStore.prefix=x/
refuse "mediaStore.ttlSecs must be" "${media[@]}" --set mediaStore.kind=file --set mediaStore.file.claimName=c --set mediaStore.ttlSecs=30
refuse "mediaStore.maxObjectBytes must be" "${media[@]}" --set mediaStore.kind=file --set mediaStore.file.claimName=c --set mediaStore.maxObjectBytes=10
refuse "remove it from config" "${media[@]}" --set mediaStore.kind=file --set mediaStore.file.claimName=c --set-string config='[media_store]'
refuse "mediaStore.caFile does not apply to kind file" "${media[@]}" --set mediaStore.kind=file --set mediaStore.file.claimName=c --set mediaStore.caFile=/x
refuse "serviceAccountKey must be a file name" "${media[@]}" --set mediaStore.kind=gcs --set mediaStore.gcs.bucket=b --set mediaStore.gcs.credentials=service-account --set mediaStore.gcs.serviceAccountSecret=k --set mediaStore.gcs.serviceAccountKey=../etc/passwd
refuse "allowInsecureHttp needs an endpoint" "${media[@]}" --set mediaStore.kind=s3 --set mediaStore.s3.bucket=b --set mediaStore.s3.region=r --set mediaStore.s3.credentials=aws --set mediaStore.s3.allowInsecureHttp=true
refuse "credentials must be workload-identity" "${media[@]}" --set mediaStore.kind=azure-blob --set mediaStore.azureBlob.account=a --set mediaStore.azureBlob.container=c --set mediaStore.azureBlob.credentials=oauth

echo "== requiring client certificates without TLS is refused"
if helm template release /charts/llmshim --set server.tls.requireClientCertificate=true >/dev/null 2>"$work/err2"; then
  echo "expected the render to fail"; exit 1
fi
grep -q "requireClientCertificate needs server.tls.enabled" "$work/err2"

echo "== a missing TLS secret name is refused"
if helm template release /charts/llmshim --set server.tls.enabled=true >/dev/null 2>"$work/err"; then
  echo "expected the render to fail"; exit 1
fi
grep -q "server.tls.secretName is required" "$work/err"
echo "== the admin port is never open by accident"
if helm template release /charts/llmshim --set admin.tokenEnv= >/dev/null 2>"$work/err"; then
  echo "expected a render with neither a token nor an opt-out to fail"; exit 1
fi
grep -q "admin.allowUnauthenticated=true" "$work/err"
helm template release /charts/llmshim --set admin.tokenEnv= --set admin.allowUnauthenticated=true \
  > "$work/open.yaml"
grep -q "allow_unauthenticated = true" "$work/open.yaml"
if helm template release /charts/llmshim --set admin.allowUnauthenticated=true >/dev/null 2>"$work/err"; then
  echo "expected a token and an opt-out together to fail"; exit 1
fi
grep -q "mutually exclusive" "$work/err"

echo "== [admin.tls] in the operator's config is refused"
if helm template release /charts/llmshim --set-string config='[admin.tls]' >/dev/null 2>"$work/err"; then
  echo "expected the render to fail"; exit 1
fi
grep -q "does not support \[admin.tls\]" "$work/err"

echo "== a changed Secret revision rolls the pods; a single replica has no PDB"
helm template release /charts/llmshim --set secretRevision=a > "$work/rev-a.yaml"
helm template release /charts/llmshim --set secretRevision=b > "$work/rev-b.yaml"
! cmp -s "$work/rev-a.yaml" "$work/rev-b.yaml"
grep -q "llmshim/secret-revision" "$work/rev-a.yaml"
helm template release /charts/llmshim --set replicaCount=1 > "$work/one.yaml"
! grep -q "kind: PodDisruptionBudget" "$work/one.yaml"
grep -q "kind: PodDisruptionBudget" "$work/default.yaml"

echo "chart validation passed"
