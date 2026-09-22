#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <release-tag>" >&2
  exit 2
fi

release_tag="$1"
if [[ ! "$release_tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "release tag must use the documented vMAJOR.MINOR.PATCH format" >&2
  exit 1
fi

repository_root="${RELEASE_INPUT_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
script_directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 "$script_directory/verify-release-inputs.py" "$repository_root" "${release_tag#v}"
