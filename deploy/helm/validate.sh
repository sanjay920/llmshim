#!/usr/bin/env bash
# Validate the chart: lint it, render it in several configurations, check the
# manifests against the Kubernetes schemas, and feed each rendered gateway
# configuration back through `llmshim serve --check` so the chart and the
# server cannot drift apart.
#
# Needs docker, openssl and python3. LLMSHIM_BIN is a binary built with
# `--features serve-redis` (default: target/debug/llmshim).
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

# A self-signed pair for the TLS variant, and secrets for every reference.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout "$work/tls.key" -out "$work/tls.crt" -days 1 -subj "/CN=127.0.0.1" \
  -addext "subjectAltName=IP:127.0.0.1" 2>/dev/null
export OPENAI_API_KEY=provider-key-openai-0123456789 ANTHROPIC_API_KEY=provider-key-anthropic-0123456789
export EXAMPLE_GATEWAY_KEY=example-caller-key-0123456789 CHECKOUT_GATEWAY_KEY=checkout-caller-key-0123456789
export GATEWAY_ADMIN_TOKEN=admin-token-0123456789abcdef REDIS_URL=redis://127.0.0.1:6379/0

for variant in default full; do
  echo "== render [$variant]"
  args=()
  [ "$variant" = full ] && args=(-f /charts/llmshim/ci/full.yaml)
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
body = body.replace("/var/lib/llmshim/receipts", f"{work}/receipts")
open(sys.argv[2], "w").write(body)
PY
  LLMSHIM_CATALOG_OFFLINE=1 "$bin" serve --config "$work/$variant.toml" --check
done

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
