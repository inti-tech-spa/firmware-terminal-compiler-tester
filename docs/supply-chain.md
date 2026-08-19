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
Extraction rejects absolute paths, `..`, device entries, hard links, and
symlinks that can escape the staging root. The staged tree is validated and
atomically renamed into a versioned destination. Interrupted staging trees are
removed on the next run. Offline mode only accepts a completely verified cache.

## Pins

- Arm GNU Toolchain: `15.2.Rel1`, `darwin-arm64-arm-none-eabi`, obtained from
  Arm's official release service. This supplies GCC, GDB, and binutils.
- OpenOCD: upstream `v0.12.0` (`9ea7f3d` tag), built by this project's release
  workflow for `darwin-arm64` with CMSIS-DAP HID support and the upstream Tcl
  scripts intact.

The project-built OpenOCD binary is not installable until its release artifact,
exact checksum, build recipe, SBOM, notices, and corresponding-source archive
are published and added to the manifest. The same rule applies to the exact Arm
artifact checksum. M1 requires no redistributed binaries; M2 cannot be approved
until these records are complete.

## Licensing decisions

`samdebug` communicates with GCC, GDB, and OpenOCD as separate executables. No
GPL code is linked into the Rust process. Redistribution will nevertheless ship
the complete applicable GPL notices, license texts, and corresponding-source
offer/source archive required for OpenOCD and GNU toolchain components.

Microchip ASF, CMSIS, DFP headers, startup files, linker scripts, and libraries
are not bundled by default in samdebug. Imported projects may use copies they
already contain. Any future DFP download or redistribution requires a separate
manifest entry and legal approval before implementation.

## Updates and overrides

Updates occur only through an explicit setup/update command. Managed pinned
tools are the default. A user may explicitly select system tools, but doctor
must report the loss of reproducibility and validate minimum capabilities. No
automatic downgrade occurs. Security revocation is represented by a signed
application release containing a manifest that marks an artifact revoked.

