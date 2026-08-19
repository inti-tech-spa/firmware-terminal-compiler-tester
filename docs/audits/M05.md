# M05 audit — attempt 1

Verdict: **REJECTED**

- Audited commit: `a4e3c3d4bea01a34dd86ddbc3034e364c4109b7b`
- Comparison base: `f1a491a`
- Audit task identity: `/root/audit_m4_remediation`
- Environment: macOS 26.5.2 arm64
- Rust: rustc/cargo 1.97.1
- OpenOCD: 0.12.0
- Arm GNU Toolchain: 15.2.Rel1, GCC 15.2.1, GDB 16.3.90,
  binutils 2.45.1
- cargo-deny: 0.20.2
- cargo-audit: 0.22.2

## Passing evidence

- Formatting, strict all-target/all-feature Clippy, cargo-deny, and cargo-audit
  passed. Locked workspace tests passed 66 tests with four explicit ignores.
- Live discovery returned the Atmel-ICE CMSIS-DAP serial `J42700028955`, and
  doctor reported the target connected with OpenOCD 0.12.0 verified.
- Authorization failures and unknown serials produced pure JSON and the fixed
  exit codes without starting OpenOCD.
- OpenOCD was launched directly with CMSIS-DAP HID, SWD, the SAM4S target,
  serial selection, 1000 kHz, erase checking, separate verification, reset,
  halt, and shutdown commands. No raw address or arbitrary ELF input is public.
- Cancellation killed and reaped the OpenOCD process group. No OpenOCD or GDB
  process or listener remained after acceptance checks.
- The SUNSIGHT firmware repository remained clean, its `.cproj` hash stayed
  unchanged, and the managed ELF was independently validated as ELF32
  little-endian ARMv7E-M EABI5 soft-float.

## Blocking findings

1. In-flight USB removal diagnostics from the pinned OpenOCD binary, including
   USB read/write errors, HID read errors, and CMSIS-DAP command failures, were
   not classified as `PROBE_DISCONNECTED` and exit 5.
2. Dynamic loopback listeners were released before OpenOCD started, leaving a
   port-allocation race with no retry or truthful bind-failure classification.
3. One timeout covered startup and the complete destructive operation, but all
   timeouts were mislabeled `OPENOCD_START_TIMEOUT`; a partial erase, write, or
   verification could therefore be reported as a startup failure.

## Required remediation

- Recognize the pinned backend's real USB/HID disconnect diagnostics and add
  startup-absence and in-flight-disconnect CLI regressions.
- Retry OpenOCD with newly allocated distinct loopback ports after bind
  collisions and report exhausted collisions as `LOCAL_PORT_UNAVAILABLE`.
- Report destructive-operation timeouts as programming failures that explicitly
  warn that target state may be partial or unknown, with separate tests from
  non-destructive command timeouts.

M06 remains blocked until the remediated M05 commit is independently approved.
