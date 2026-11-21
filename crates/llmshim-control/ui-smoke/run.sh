#!/usr/bin/env bash
# Runs the admin UI's headless-browser smoke test against a real control plane (note 0055).
# Needs a built binary (cargo build), node, and a Chrome or Chromium (CHROME, or on PATH).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="${LLMSHIM_CONTROL_BIN:-$here/../target/debug/llmshim-control}"
chrome="${CHROME:-$(command -v google-chrome || command -v chromium || command -v chromium-browser || true)}"
[ -x "$bin" ] || { echo "build the binary first: cargo build (looked for $bin)" >&2; exit 2; }
[ -n "$chrome" ] || { echo "no Chrome found: set CHROME" >&2; exit 2; }
umask 077
# A throwaway database and two tokens; the OS (or an ephemeral CI runner) clears the directory.
dir="$(mktemp -d)"
server=""
stop_server() { if [ -n "$server" ]; then kill "$server" 2>/dev/null || true; fi; }
trap stop_server EXIT
admin="smoke-admin-token-0123456789-abcdef"
ingest="smoke-ingest-token-0123456789-abcdef"
printf '%s' "$admin" > "$dir/admin.tok"
printf '%s' "$ingest" > "$dir/ingest.tok"
"$bin" migrate --db "$dir/c.db" > /dev/null
"$bin" serve --db "$dir/c.db" --listen 127.0.0.1:0 --token-file "$dir/admin.tok" \
  --ingest-token-file "$dir/ingest.tok" 2> "$dir/server.log" &
server=$!
base=""
for _ in $(seq 1 100); do
  base="$(grep -o 'http://[0-9.:]*' "$dir/server.log" | head -1 || true)"
  if [ -n "$base" ]; then break; fi
  if ! kill -0 "$server" 2>/dev/null; then break; fi
  sleep 0.2
done
if [ -z "$base" ]; then
  echo "the server did not start:" >&2
  cat "$dir/server.log" >&2
  exit 1
fi
cd "$here"
# The lockfile is committed: install exactly it (puppeteer-core has no download step, so no scripts are needed).
if [ ! -d node_modules ]; then npm ci --ignore-scripts --no-audit --no-fund --silent; fi
BASE="$base" TOKEN="$admin" INGEST="$ingest" CHROME="$chrome" node smoke.mjs
