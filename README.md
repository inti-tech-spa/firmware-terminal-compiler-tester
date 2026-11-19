# samdebug

`samdebug` is a terminal-first compiler, programmer, and debugger for
Microchip ATSAM4SD32C firmware using Atmel-ICE. The application is intended to
provide one human-facing TUI and one stable machine-facing protocol while it
coordinates an Arm GNU toolchain, OpenOCD, and GDB/MI.

The first supported host is Apple Silicon macOS. Version 1 imports a bounded
subset of Microchip Studio 7 ARM-GCC C executable projects without modifying
their `.cproj`, `.atsln`, source, ASF, or generated build files.

Development is milestone-gated. A milestone is incomplete until an independent
audit subagent has reviewed the committed implementation and an `APPROVED`
report is committed under `docs/audits/`.

## Status

M0 through M6 are independently approved. M7 integrates the native TUI, NDJSON
agent protocol, and Apple Silicon release packaging and is undergoing its final
implementation and audit gate. See `docs/debugging.md` and `docs/release.md`.

## Branches and releases

Start feature work from `develop`, merge and test there, then promote the verified revision to `master` for release. Both promotion steps use merge commits. Delete feature branches after integration testing passes and keep `develop`. See [the branch and release workflow](AGENTS.md#branch-and-release-workflow) for approvals, test records, and release tags.
