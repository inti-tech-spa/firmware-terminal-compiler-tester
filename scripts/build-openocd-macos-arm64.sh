#!/bin/sh
set -eu

export LC_ALL=C
export TZ=UTC
export SOURCE_DATE_EPOCH=1672444800
export ZERO_AR_DATE=1

OPENOCD_VERSION=0.12.0
OPENOCD_SHA256=af254788be98861f2bd9103fe6e60a774ec96a8c374744eef9197f6043075afa
LIBUSB_VERSION=1.0.29
LIBUSB_SHA256=5977fc950f8d1395ccea9bd48c06b3f808fd3c2c961b44b0c2e6e29fc3a70a85
HIDAPI_VERSION=0.15.0
HIDAPI_SHA256=5d84dec684c27b97b921d2f3b73218cb773cf4ea915caee317ac8fc73cef8136
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
script_path=$script_dir/$(basename -- "$0")
repo_root=$(dirname -- "$script_dir")

if [ "$(uname -s)" != Darwin ] || [ "$(uname -m)" != arm64 ]; then
  echo "this recipe requires an Apple Silicon macOS builder" >&2
  exit 2
fi

for program in curl shasum tar make pkg-config dylibbundler python3; do
  command -v "$program" >/dev/null 2>&1 || {
    echo "missing release-build dependency: $program" >&2
    exit 3
  }
done

libusb_actual=$(brew list --versions libusb 2>/dev/null || true)
hidapi_actual=$(brew list --versions hidapi 2>/dev/null || true)
if [ "$libusb_actual" != "libusb $LIBUSB_VERSION" ]; then
  echo "expected libusb $LIBUSB_VERSION, found: $libusb_actual" >&2
  exit 3
fi
if [ "$hidapi_actual" != "hidapi $HIDAPI_VERSION" ]; then
  echo "expected hidapi $HIDAPI_VERSION, found: $hidapi_actual" >&2
  exit 3
fi

build_parent=${RUNNER_TEMP:-/private/tmp}
build_root=$(mktemp -d "$build_parent/samdebug-openocd-build.XXXXXX")
trap 'rm -rf "$build_root"' EXIT HUP INT TERM
source_archive=$build_root/openocd-$OPENOCD_VERSION.tar.bz2
libusb_source=$build_root/libusb-$LIBUSB_VERSION.tar.bz2
hidapi_source=$build_root/hidapi-$HIDAPI_VERSION.tar.gz
source_dir=$build_root/openocd-$OPENOCD_VERSION
bundle_name=samdebug-openocd-$OPENOCD_VERSION-darwin-arm64
source_bundle_name=samdebug-openocd-$OPENOCD_VERSION-sources
stage=$build_root/$bundle_name
source_stage=$build_root/$source_bundle_name
dist=${SAMDEBUG_DIST_DIR:-$PWD/dist}

mkdir -p "$build_root" "$dist"
curl --fail --location --proto '=https' --proto-redir '=https' \
  --output "$source_archive" \
  "https://downloads.sourceforge.net/project/openocd/openocd/$OPENOCD_VERSION/openocd-$OPENOCD_VERSION.tar.bz2"
printf '%s  %s\n' "$OPENOCD_SHA256" "$source_archive" | shasum -a 256 -c
curl --fail --location --proto '=https' --proto-redir '=https' \
  --output "$libusb_source" \
  "https://github.com/libusb/libusb/releases/download/v$LIBUSB_VERSION/libusb-$LIBUSB_VERSION.tar.bz2"
printf '%s  %s\n' "$LIBUSB_SHA256" "$libusb_source" | shasum -a 256 -c
curl --fail --location --proto '=https' --proto-redir '=https' \
  --output "$hidapi_source" \
  "https://github.com/libusb/hidapi/archive/refs/tags/hidapi-$HIDAPI_VERSION.tar.gz"
printf '%s  %s\n' "$HIDAPI_SHA256" "$hidapi_source" | shasum -a 256 -c
tar -xjf "$source_archive" -C "$build_root"

cd "$source_dir"
CFLAGS="-O2 -g0 -ffile-prefix-map=$build_root=/usr/src/samdebug-openocd" ./configure \
  --prefix=/ \
  --enable-cmsis-dap \
  --without-capstone \
  --disable-werror \
  --disable-doxygen-html \
  --disable-doxygen-pdf
make -j"$(sysctl -n hw.logicalcpu)"
make install DESTDIR="$stage"

mkdir -p "$stage/lib" "$stage/licenses" "$stage/share/samdebug"
dylibbundler -od -b -x "$stage/bin/openocd" -d "$stage/lib/" -p '@executable_path/../lib/'
cp COPYING "$stage/licenses/OpenOCD-COPYING"
cp jimtcl/LICENSE "$stage/licenses/JimTcl-LICENSE"
cp jimtcl/tcl.license.terms "$stage/licenses/JimTcl-tcl.license.terms"
cp "$(brew --prefix libusb)/COPYING" "$stage/licenses/libusb-COPYING"
cp "$(brew --prefix hidapi)/LICENSE.txt" "$stage/licenses/hidapi-LICENSE.txt"
"$stage/bin/openocd" --version 2>&1 | grep "Open On-Chip Debugger $OPENOCD_VERSION"
test -f "$stage/share/openocd/scripts/interface/cmsis-dap.cfg"
test -f "$stage/share/openocd/scripts/target/at91sam4sXX.cfg"
if otool -L "$stage/bin/openocd" "$stage"/lib/*.dylib | grep -q '/opt/homebrew\|/usr/local'; then
  echo "bundle retains a package-manager library reference" >&2
  exit 3
fi

python3 "$repo_root/scripts/generate-tool-sbom.py" \
  "$stage/share/samdebug/sbom.spdx.json" \
  "$OPENOCD_VERSION" "$LIBUSB_VERSION" "$HIDAPI_VERSION" 0.80

mkdir -p "$source_stage"
cp "$source_archive" "$source_stage/"
cp "$libusb_source" "$source_stage/"
cp "$hidapi_source" "$source_stage/"
cp "$script_path" "$source_stage/"
cp "$repo_root/scripts/create-deterministic-tar-xz.py" "$source_stage/"
cp "$stage/share/samdebug/sbom.spdx.json" "$source_stage/"
python3 "$repo_root/scripts/create-deterministic-tar-xz.py" \
  "$dist/$bundle_name.tar.xz" "$stage"
shasum -a 256 "$dist/$bundle_name.tar.xz" > "$dist/$bundle_name.tar.xz.sha256"
python3 "$repo_root/scripts/create-deterministic-tar-xz.py" \
  "$dist/$source_bundle_name.tar.xz" "$source_stage"
shasum -a 256 "$dist/$source_bundle_name.tar.xz" > "$dist/$source_bundle_name.tar.xz.sha256"
