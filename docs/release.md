# Apple Silicon macOS release

`samdebug` version 1 is released as one native `arm64` Mach-O executable. It
does not require Rust, Homebrew, a PATH change, `sudo`, or a system-wide driver.
The application installs its pinned Arm GNU and OpenOCD tools only after an
explicit `samdebug setup`, under `~/Library/Application Support/samdebug`.

Build a candidate from a clean, locked checkout on Apple Silicon macOS:

```sh
./scripts/build-release-macos-arm64.sh
./scripts/accept-release-macos-arm64.sh
```

The release directory contains the executable, SHA-256 digest, SPDX 2.3 JSON
SBOM covering the binary, locked Rust crates, managed tools, and corresponding
sources; third-party Rust notices and license texts; project license; embedded tool manifest; and
both public JSON schemas.
The acceptance script checks the architecture and digest, parses the metadata,
and starts the binary with an empty HOME and a minimal `/usr/bin:/bin` PATH. It
also confirms that `version` does not mutate the clean home directory.

On a clean Mac, copy `samdebug` to any user-writable directory, make it
executable if necessary, then run:

```sh
./samdebug setup
./samdebug doctor
./samdebug init --from-cproj path/to/firmware.cproj --configuration Debug
./samdebug build
./samdebug debug
```

macOS quarantine is intentionally not removed automatically. A downloaded
release must be signed/notarized by the publisher or approved by the user using
normal macOS security UI. Release publication records the signing identity,
notarization result, source commit, Rust version, tool-bundle versions, hashes,
and clean-machine acceptance output.
