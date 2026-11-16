#!/usr/bin/env bash
# Tests for wait-for-green.sh against a fake `gh` that answers from a script.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sha=0123456789abcdef0123456789abcdef01234567
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
# The fake gh prints the next line of $FAKE_GH_SCRIPT (one JSON answer per call),
# repeating the last, for the workflow named in its arguments.
cat > "$work/bin/gh" <<'GH'
#!/usr/bin/env bash
workflow=""
while [ "$#" -gt 0 ]; do
  [ "$1" = "--workflow" ] && workflow="$2"
  shift
done
file="$FAKE_GH_DIR/$workflow"
n=$(cat "$file.n" 2>/dev/null || echo 0)
total=$(wc -l < "$file")
[ "$n" -lt "$total" ] && n=$((n + 1))
echo "$n" > "$file.n"
sed -n "${n}p" "$file"
GH
chmod +x "$work/bin/gh"
export PATH="$work/bin:$PATH" FAKE_GH_DIR="$work" POLL_SECS=0 WAIT_TIMEOUT_SECS=5 NO_RUN_GRACE_SECS=0

run() { # answers... ; prints the exit status
  rm -f "$work"/*.n
  : > "$work/ci.yml"
  for answer in "$@"; do echo "$answer" >> "$work/ci.yml"; done
  set +e
  "$here/wait-for-green.sh" "$sha" ci.yml >"$work/out" 2>&1
  status=$?
  set -e
  echo "$status"
}
said() { # the last run's output must contain $1
  if ! grep -q "$1" "$work/out"; then
    echo "FAIL: expected the output to say: $1; got: $(cat "$work/out")" >&2
    exit 1
  fi
}
expect() {
  if [ "$2" != "$3" ]; then
    echo "FAIL: $1: expected exit $2, got $3" >&2
    exit 1
  fi
  echo "ok: $1"
}

ok='[{"status":"completed","conclusion":"success"}]'
red='[{"status":"completed","conclusion":"failure"}]'
busy='[{"status":"in_progress","conclusion":""}]'
none='[]'
rerun='[{"status":"completed","conclusion":"success"},{"status":"completed","conclusion":"failure"}]'
stale='[{"status":"completed","conclusion":"failure"},{"status":"completed","conclusion":"success"}]'

expect "a green run passes" 0 "$(run "$ok")"
expect "a red run refuses" 1 "$(run "$red")"
expect "a cancelled run refuses" 1 "$(run '[{"status":"completed","conclusion":"cancelled"}]')"
expect "an in-progress run is waited for, then green passes" 0 "$(run "$busy" "$busy" "$ok")"
expect "an in-progress run that goes red refuses" 1 "$(run "$busy" "$red")"
expect "no run at all refuses after the grace" 1 "$(run "$none")"
run "$none" >/dev/null; said "no ci.yml run exists"
run "$busy" >/dev/null; WAIT_TIMEOUT_SECS=0 run "$busy" >/dev/null; said "timed out"
expect "a run that appears after a wait is used" 0 "$(NO_RUN_GRACE_SECS=60 run "$none" "$none" "$ok")"
expect "a run that never finishes times out" 1 "$(WAIT_TIMEOUT_SECS=0 run "$busy")"
expect "the newest run decides: green after red passes" 0 "$(run "$rerun")"
expect "the newest run decides: red after green refuses" 1 "$(run "$stale")"

# Every named workflow must be green, not just the first.
rm -f "$work"/*.n
echo "$ok" > "$work/ci.yml"
echo "$red" > "$work/serve-check.yml"
set +e
"$here/wait-for-green.sh" "$sha" ci.yml serve-check.yml >/dev/null 2>&1
both=$?
set -e
expect "a red second workflow refuses" 1 "$both"

# A short or malformed sha is refused before gh is asked anything.
set +e
"$here/wait-for-green.sh" abc1234 ci.yml >/dev/null 2>&1
short=$?
set -e
expect "an abbreviated sha is a usage error" 2 "$short"
echo "all wait-for-green tests passed"
