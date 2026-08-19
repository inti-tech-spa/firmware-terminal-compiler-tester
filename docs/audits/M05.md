# M05 audit — attempt 2

Verdict: **REJECTED**

- Audited commit: `bd74d46838cec0e308d776d961ab323f340d621f`
- Remediation base: `a4e3c3d4bea01a34dd86ddbc3034e364c4109b7b`
- Audit task identity: `/root/audit_m4_remediation`
- Environment: macOS 26.5.2 arm64
- Rust: rustc/cargo 1.97.1
- OpenOCD: 0.12.0
- Arm GNU Toolchain: 15.2.Rel1
- cargo-deny: 0.20.2
- cargo-audit: 0.22.2

## Passing evidence

- Formatting, strict Clippy, dependency policy, and vulnerability checks passed.
  The locked workspace suite passed 68 tests with four explicit ignores.
- Bind failures retry four fresh three-port sets and exhaust as
  `LOCAL_PORT_UNAVAILABLE`/5. Destructive timeouts now truthfully return
  `ERASE_TIMEOUT` or `FLASH_TIMEOUT`/6 and warn of partial unknown target state.
- The SUNSIGHT fixture rebuilt 33/0 and incrementally 0/33. Its managed ELF and
  source integrity checks passed, and the live target remained connected.
- Direct argv, exact authorization, managed-ELF confinement, process-group
  cleanup, JSON purity, and the restricted public CLI remained intact.

## Blocking findings

1. Port-bind retry was evaluated before transport classification. A log with
   both `USB is disconnected` and a bind error retried and could return
   `LOCAL_PORT_UNAVAILABLE` instead of immediately returning
   `PROBE_DISCONNECTED`/5.
2. The pinned OpenOCD binary has command-specific CMSIS-DAP failures, mismatch
   and protocol errors, interface-reset failures, HID timeouts/write failures,
   USB discovery failures, and libusb bulk errors that were not all recognized
   by the transport classifier.

## Required remediation

- Give probe-transport diagnostics unconditional precedence over bind retry and
  all operation fallbacks, with a combined transport-plus-bind no-retry test.
- Table-test the exact pinned CMSIS-DAP, HID, USB, and libusb diagnostic family
  without treating target-side failures as probe transport failures.

M06 remains blocked until the remediated M05 commit is independently approved.
