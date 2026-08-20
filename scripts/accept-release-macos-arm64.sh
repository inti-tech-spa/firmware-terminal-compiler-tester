#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
release_dir="$project_root/dist/release/macos-arm64"
binary="$release_dir/samdebug"

test -x "$binary"
file "$binary" | grep -q 'Mach-O 64-bit executable arm64'
(cd "$release_dir" && shasum -a 256 -c samdebug.sha256)
python3 -m json.tool "$release_dir/samdebug.spdx.json" >/dev/null
python3 "$project_root/scripts/validate-release-sbom.py" "$release_dir/samdebug.spdx.json"
grep -q '"name": "arm-gnu-toolchain"' "$release_dir/samdebug.spdx.json"
grep -q '"name": "openocd"' "$release_dir/samdebug.spdx.json"
grep -q 'corresponding-source' "$release_dir/samdebug.spdx.json"
test -s "$release_dir/THIRD_PARTY_NOTICES.md"
test -s "$release_dir/THIRD_PARTY_LICENSES.txt"
python3 -m json.tool "$release_dir/result-v1.schema.json" >/dev/null
python3 -m json.tool "$release_dir/debug-v1.schema.json" >/dev/null
python3 -m json.tool "$release_dir/tool-manifest-v1.json" >/dev/null
clean_home=$(mktemp -d)
trap 'rm -rf "$clean_home"' EXIT HUP INT TERM
output=$(env -i HOME="$clean_home" PATH=/usr/bin:/bin "$binary" version --output=json)
printf '%s\n' "$output" | grep -q '"ok":true'
test ! -e "$clean_home/Library/Application Support/samdebug"
echo "clean-environment standalone acceptance passed"
