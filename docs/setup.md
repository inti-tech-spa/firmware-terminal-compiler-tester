# Managed setup and doctor

`samdebug setup` installs only the embedded, pinned manifest into
`~/Library/Application Support/samdebug` on macOS. It does not use `sudo`, edit
`PATH`, install Homebrew packages, or write outside that user-local tree.

The setup transaction downloads through HTTPS with a final-host allowlist,
verifies SHA-256 before decompression, extracts into a new staging directory,
validates paths, entry types, executable versions, and license files, writes an
install marker, then atomically renames the tree into place. Symbolic links and
special files are rejected. Internal archive hard links are validated and
materialized as regular-file copies. Interrupted staging and partial-download
files are removed on the next setup run.

`samdebug setup --offline` never accesses the network. It reuses an already
verified installation or a cache archive whose current hash matches the
embedded manifest; otherwise it returns `OFFLINE_CACHE_MISS` with exit code 3.

`samdebug doctor` reports the platform, embedded-manifest state, installed-tool
and cache integrity, executable versions, Atmel-ICE USB visibility, target
connection result, target voltage when OpenOCD reports it, and corrective
guidance. No separate Atmel-ICE kernel driver is installed on macOS: OpenOCD
uses CMSIS-DAP through the operating system's USB/HID support.

System tools are accepted only when `samdebug.toml` explicitly selects
`channel = "system"` and provides absolute paths for GCC, GDB, OpenOCD,
objcopy, objdump, and size. The generated default remains `channel = "pinned"`.

## Current publication gate

The verified Arm GNU Toolchain 15.2.Rel1 record and the reproducible OpenOCD
0.12.0 macOS-arm64 candidate are complete. The embedded manifest remains
deliberately non-installable until the OpenOCD binary and corresponding-source
archives are published at, and downloaded back from, their pinned release URLs.
Until then `samdebug setup` returns `TOOL_MANIFEST_DISABLED`; it never downloads
a partial tool set.
