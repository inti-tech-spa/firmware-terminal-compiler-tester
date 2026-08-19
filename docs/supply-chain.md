# Tool supply chain and redistribution

## Trust model

Managed tools are installed per-user under the platform application-data
directory. The application never uses `sudo`, edits PATH, invokes a native
installer, or modifies global package-manager/registry state.

Every downloadable artifact must have a checked-in manifest record containing
name, version, OS, architecture, exact HTTPS URL, allowed final hosts, SHA-256,
archive type and root, executable relative paths, license identifiers, notice
files, upstream source URL, and source-offer obligations. A record with missing
or placeholder integrity/licensing data is invalid and cannot install.

Downloads go to a new temporary file. The SHA-256 is verified before parsing.
Extraction rejects absolute paths, `..`, device entries, and all symbolic
links. Archive hard links are never created: an internal, validated regular-file
target is materialized as an independent copy, while external, missing, chained,
or otherwise unsafe targets are rejected. The staged tree is validated and
atomically renamed into a versioned destination. Interrupted staging trees are
removed on the next run. Offline mode only accepts a completely verified cache.

## Pins

- Arm GNU Toolchain: `15.2.Rel1`, `darwin-arm64-arm-none-eabi`, obtained from
  Arm's official release service. This supplies GCC, GDB, and binutils.
- OpenOCD: upstream `v0.12.0` (`9ea7f3d` tag), built by this project's release
  workflow for `darwin-arm64` with CMSIS-DAP HID support and the upstream Tcl
  scripts intact.

The project-built OpenOCD candidate includes its exact checksum, deterministic
build recipe, SPDX SBOM, notices, and a separate corresponding-source archive.
Two consecutive local builds produced identical binary and source archive
hashes. Both archives are published at their pinned release URLs; anonymous
download-back verification matched the manifest hashes before the production
manifest was enabled.

## Licensing decisions

`samdebug` is licensed under MIT and communicates with GCC, GDB, and
OpenOCD as separate executables. No GPL code is linked into the Rust process.
OpenOCD statically includes its upstream JimTcl 0.80 snapshot and dynamically
loads the bundled libusb 1.0.29 and HIDAPI 0.15.0 libraries; each is represented
in the SBOM and its license text is included. Redistribution nevertheless ships
the complete applicable GPL notices and license texts. For every redistributed
GPL binary, the same release and download location will provide the exact
corresponding-source archive and build scripts at no additional charge. The
project will use this accompanying-source mechanism, not a written source
offer, and retain the release assets for at least as long as the binary is
distributed.

Microchip ASF, CMSIS, DFP headers, startup files, linker scripts, and libraries
are not bundled by default in samdebug. Imported projects may use copies they
already contain. Any future DFP download or redistribution requires a separate
manifest entry and legal approval before implementation.

## Updates and overrides

Updates occur only through an explicit setup/update command. Managed pinned
tools are the default. A user may explicitly select system tools, but doctor
must report the loss of reproducibility and validate minimum capabilities. No
automatic downgrade occurs. Version 1 has no remotely mutable manifest. The
manifest is embedded in the samdebug release and trusted through the signed and
notarized application artifact. Revocation requires a new signed samdebug
release whose embedded manifest marks the old artifact revoked; the signing
identity and notarization evidence are recorded in the release provenance.

## Rust dependency policy

All Rust dependencies are locked in `Cargo.lock`. Allowed dependency licenses
are Apache-2.0, MIT, BSD-2-Clause, BSD-3-Clause, ISC, Unicode-3.0, and Zlib.
MPL-2.0 requires file-level review. GPL, AGPL, LGPL, unlicensed, unknown, or
noncommercial dependencies are denied unless a later audited architecture and
legal decision explicitly approves them. `cargo deny check licenses` and
`cargo audit` are required at each Rust milestone. Release SBOMs use SPDX JSON
and include Rust crates plus every managed executable and source archive.
