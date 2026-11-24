#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
validator="$repository_root/.github/scripts/verify-release-inputs.sh"
fixture_source="$repository_root/tests/fixtures/release-input-validation"
fixture_root="$(mktemp -d "${TMPDIR:-/tmp}/llmshim-release-inputs.XXXXXX")"
trap 'rm -rf "$fixture_root"' EXIT

copy_fixture() {
  rm -rf "$fixture_root"/*
  while IFS= read -r fixture_file; do
    fixture_relative_path="${fixture_file#"$fixture_source"/}"
    mkdir -p "$(dirname "$fixture_root/$fixture_relative_path")"
    cp "$fixture_file" "$fixture_root/$fixture_relative_path"
  done < <(find "$fixture_source" -type f | sort)
}

expect_valid() {
  RELEASE_INPUT_ROOT="$fixture_root" "$validator" "$1" >/dev/null
}

expect_invalid() {
  if RELEASE_INPUT_ROOT="$fixture_root" "$validator" "$1" >/dev/null 2>&1; then
    echo "expected validation failure for $1" >&2
    exit 1
  fi
}

copy_fixture
expect_valid v1.2.3

copy_fixture
expect_invalid v1.2.3\;touch-injected
test ! -e "$fixture_root/touch-injected"

copy_fixture
expect_invalid v1.2

copy_fixture
perl -0pi -e 's/version = "1\.2\.3"/version = "1.2.4"/' "$fixture_root/Cargo.toml"
expect_invalid v1.2.3

copy_fixture
perl -0pi -e 's/version = "0\.4\.5"/version = "0.4.6"/' "$fixture_root/crates/llmshim-catalog/Cargo.toml"
expect_invalid v1.2.3

copy_fixture
python3 - "$fixture_root/clients/typescript/package-lock.json" <<'PY'
import json
import sys
path = sys.argv[1]
data = json.load(open(path))
data["version"] = "99.0.0"
open(path, "w").write(json.dumps(data, separators=(",", ":")))
PY
expect_invalid v1.2.3

copy_fixture
python3 - "$fixture_root/clients/typescript/package-lock.json" <<'PY'
import json
import sys
package_lock_path = sys.argv[1]
package_lock_data = json.load(open(package_lock_path))
package_lock_data["packages"][""]["version"] = "99.0.0"
open(package_lock_path, "w").write(json.dumps(package_lock_data, separators=(",", ":")))
PY
expect_invalid v1.2.3

copy_fixture
python3 - "$fixture_root/clients/typescript/package.json" <<'PY'
import json
import sys
path = sys.argv[1]
data = json.load(open(path))
data["optionalDependencies"]["llmshim-linux-x64"] = "99.0.0"
data["devDependencies"] = {"llmshim-linux-x64": "1.2.3"}
open(path, "w").write(json.dumps({"devDependencies": data.pop("devDependencies"), **data}, separators=(",", ":")))
PY
expect_invalid v1.2.3

copy_fixture
python3 - "$fixture_root/clients/typescript/package.json" <<'PY'
import json
import sys
path = sys.argv[1]
data = json.load(open(path))
open(path, "w").write(json.dumps({"optionalDependencies": data.pop("optionalDependencies"), **data}, separators=(",", ":")))
PY
expect_valid v1.2.3

copy_fixture
python3 - "$fixture_root/clients/typescript/package-lock.json" <<'PY'
import json
import sys
path = sys.argv[1]
data = json.load(open(path))
data["packages"]["node_modules/llmshim-linux-x64"]["version"] = "99.0.0"
open(path, "w").write(json.dumps(data, separators=(",", ":")))
PY
expect_invalid v1.2.3

copy_fixture
perl -0pi -e 's/VERSION = "1\.2\.3"/VERSION = "1.2.4"/' "$fixture_root/clients/ruby/lib/llmshim/version.rb"
expect_invalid v1.2.3

copy_fixture
perl -0pi -e 's/version: 1\.2\.3/version: 1.2.4/' "$fixture_root/api/openapi.yaml"
expect_invalid v1.2.3

copy_fixture
perl -0pi -e 's/manifest-path = "\.\.\/\.\.\/Cargo\.toml"/manifest-path = "..\/Cargo.toml"/' "$fixture_root/clients/python/pyproject.toml"
expect_invalid v1.2.3

echo "release input validation fixtures passed"
