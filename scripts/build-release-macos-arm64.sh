#!/bin/sh
set -eu

if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
  echo "release build requires Apple Silicon macOS" >&2
  exit 2
fi

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cargo_bin=${CARGO:-"$HOME/.cargo/bin/cargo"}
release_dir="$project_root/dist/release/macos-arm64"

mkdir -p "$release_dir"
"$cargo_bin" build --release --locked --target aarch64-apple-darwin
cp "$project_root/target/aarch64-apple-darwin/release/samdebug" "$release_dir/samdebug"
chmod 755 "$release_dir/samdebug"
shasum -a 256 "$release_dir/samdebug" > "$release_dir/samdebug.sha256"
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git -C "$project_root" show -s --format=%ct HEAD)}
export SOURCE_DATE_EPOCH
python3 "$project_root/scripts/generate-release-sbom.py" \
  --cargo "$cargo_bin" --binary "$release_dir/samdebug" \
  --tool-manifest "$project_root/tools/manifest-v1.json" \
  --output "$release_dir/samdebug.spdx.json"
python3 "$project_root/scripts/generate-rust-notices.py" \
  --cargo "$cargo_bin" --output "$release_dir/THIRD_PARTY_NOTICES.md" \
  --licenses-output "$release_dir/THIRD_PARTY_LICENSES.txt"
cp "$project_root/LICENSE" "$release_dir/LICENSE"
cp "$project_root/schemas/result-v1.schema.json" "$release_dir/result-v1.schema.json"
cp "$project_root/schemas/debug-v1.schema.json" "$release_dir/debug-v1.schema.json"
cp "$project_root/tools/manifest-v1.json" "$release_dir/tool-manifest-v1.json"

file "$release_dir/samdebug"
"$release_dir/samdebug" --version
echo "release assembled at $release_dir"
