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

Architecture and public contracts are defined in `docs/`. Implementation begins
only after the M0 audit is approved.

