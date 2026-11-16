#!/usr/bin/env bash
# Wait until each named workflow has a green run for a commit; fail on a red run,
# a timeout, or no run at all after a grace period. The release workflow uses it to
# refuse a tag whose commit has not passed the same CI as every merge (0013).
#
#   wait-for-green.sh <full-commit-sha> <workflow-file>...
#
# Needs `gh` (authenticated through GH_TOKEN) and `jq`. The newest run of a
# workflow decides, so a re-run that turned green outranks the red one before it.
# WAIT_TIMEOUT_SECS (3600), NO_RUN_GRACE_SECS (900) and POLL_SECS (30) tune it.
set -euo pipefail

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <full-commit-sha> <workflow-file>..." >&2
  exit 2
fi
sha="$1"
shift
if [[ ! "$sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "the commit must be a full 40-character sha (gh does not match an abbreviation)" >&2
  exit 2
fi
timeout_secs="${WAIT_TIMEOUT_SECS:-3600}"
grace_secs="${NO_RUN_GRACE_SECS:-900}"
poll_secs="${POLL_SECS:-30}"
started=$(date +%s)

for workflow in "$@"; do
  while true; do
    runs=$(gh run list --workflow "$workflow" --commit "$sha" --limit 20 --json status,conclusion)
    count=$(jq 'length' <<<"$runs")
    elapsed=$(($(date +%s) - started))
    if [ "$count" -eq 0 ]; then
      if [ "$elapsed" -ge "$grace_secs" ]; then
        echo "no $workflow run exists for $sha after ${elapsed}s: refusing" >&2
        exit 1
      fi
      echo "waiting for a $workflow run for $sha (${elapsed}s)"
    else
      status=$(jq -r '.[0].status' <<<"$runs")
      conclusion=$(jq -r '.[0].conclusion' <<<"$runs")
      if [ "$status" = "completed" ]; then
        if [ "$conclusion" = "success" ]; then
          echo "$workflow is green for $sha"
          break
        fi
        echo "$workflow finished '$conclusion' for $sha: refusing" >&2
        exit 1
      fi
      echo "$workflow is $status for $sha (${elapsed}s)"
    fi
    if [ "$elapsed" -ge "$timeout_secs" ]; then
      echo "timed out after ${elapsed}s waiting for $workflow: refusing" >&2
      exit 1
    fi
    sleep "$poll_secs"
  done
done
