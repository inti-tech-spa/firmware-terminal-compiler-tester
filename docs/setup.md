# Managed setup and doctor

`samdebug setup` installs only the embedded, pinned manifest into
`~/Library/Application Support/samdebug` on macOS. It does not use `sudo`, edit
`PATH`, install Homebrew packages, or write outside that user-local tree.

The setup transaction downloads through HTTPS with a final-host allowlist,
verifies SHA-256 before decompression, extracts into a new staging directory,
validates paths, entry types, executable versions, and license files, writes an
install marker, then atomically renames the tree into place. Symbolic links and
special files are rejected. Internal archive hard links are validated and
materialized as regular-file copies whose bytes count against the extraction
quota. The versioned install marker records the size and SHA-256 of every file;
reuse and doctor recompute the complete inventory so modified libraries, Tcl
scripts, notices, or executables are rejected. Interrupted downloads,
decompression, extraction, hashing, validation, and version checks return exit
130, terminate the active downloader, and remove partial state immediately.

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
objcopy, objdump, and size. Doctor loads that configuration, runs every tool's
version check, and warns that the override is not reproducible. The generated
default remains `channel = "pinned"`.

## Published tool channel

The verified Arm GNU Toolchain 15.2.Rel1 and reproducible OpenOCD 0.12.0
macOS-arm64 archives are published at the pinned URLs in the manifest. Both
release archives were downloaded anonymously after publication and matched
their checked-in SHA-256 values. The production manifest is installable.
