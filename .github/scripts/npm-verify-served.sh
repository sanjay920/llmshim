#!/usr/bin/env bash
# `npm publish` exiting 0 means the registry *accepted* the tarball, not that it
# serves it. 0.7.1 was accepted, logged to Sigstore, answered "may take a few
# minutes to become available" — and half an hour later still served nothing,
# behind a green job. So a publish is not done until `npm view` returns the
# version. Run from the package's directory, after its publish step.
#
# Bound: 20 polls, 30 s apart — about ten minutes. npm's "few minutes" has been
# minutes, never seconds, and a version still unserved after ten is stuck in
# npm's processing, which more waiting here will not fix.
#
# Compares `npm view`'s output to the version rather than its exit status: for a
# package that exists with a version that does not, npm's exit status has
# differed across releases. `--prefer-online` revalidates on every poll, so the
# first poll's packument — cached without the new version — cannot answer for
# the rest of the window.
set -euo pipefail

ATTEMPTS=20
INTERVAL=30
NAME=$(node -p "require('./package.json').name")
VER=$(node -p "require('./package.json').version")

for attempt in $(seq 1 "$ATTEMPTS"); do
  served=$(npm view --prefer-online "$NAME@$VER" version 2>/dev/null || true)
  if [ "$served" = "$VER" ]; then
    echo "$NAME@$VER is served by the registry."
    exit 0
  fi
  echo "poll $attempt/$ATTEMPTS: $NAME@$VER not served yet"
  if [ "$attempt" -lt "$ATTEMPTS" ]; then
    sleep "$INTERVAL"
  fi
done

echo "::error::$NAME@$VER was accepted by npm but is still not served after $((ATTEMPTS * INTERVAL / 60)) minutes. The publish step exited 0 and its output carries the Sigstore provenance line, so the tarball is stuck in npm's processing, not missing from this run. Look at https://www.npmjs.com/package/$NAME?activeTab=versions and https://status.npmjs.org; once it appears, re-run this job — the publish step sees it and skips. If it never appears, npm will refuse a second publish of the same version and support has to release it."
exit 1
