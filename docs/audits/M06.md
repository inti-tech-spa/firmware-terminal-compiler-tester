# M06 audit — attempt 1

Verdict: **REJECTED**

- Audited commit: `79d74ab0de7a5e5d317f9194dce3843258479248`
- Base: `67dd3a7173786632a7db729fbb3896163aae59b9`
- Audit task identity: `/root/audit_m4_remediation`
- Environment: macOS 26.5.2 arm64
- Rust: rustc/cargo 1.97.1
- OpenOCD: 0.12.0
- Arm GNU Toolchain: 15.2.Rel1
- cargo-deny: 0.20.2
- cargo-audit: 0.22.2

## Passing evidence

- Formatting, strict Clippy, cargo-deny, and cargo-audit passed. The locked
  workspace passed 82 tests with six explicit external/hardware ignores.
- Direct OpenOCD and GDB argv, loopback binding, exact probe quoting, CMSIS-DAP
  HID/SWD configuration, MI2 with initialization disabled, firmware-load
  authorization, and absence of public raw monitor/memory writes passed review.
- M05 programming regressions remained green. Atmel-ICE serial `J42700028955`
  remained visible, and the SUNSIGHT source, `.cproj`, and managed ELF integrity
  evidence remained unchanged.

## Blocking findings

1. Cancellation could not preempt an in-flight synchronous MI operation because
   both the operation and `cancel` required exclusive mutable session access.
2. Fatal MI framing errors and some child/transport failures did not immediately
   drive failed-session cleanup and could leave both children live.
3. Query and load paths discarded interleaved target/log records, and asynchronous
   stops were not correlated with the current operation/generation.
4. A complete record larger than 1 MiB bypassed the parser limit when its newline
   arrived in the same input chunk.
5. Each new owned reconnect reset the generation to 1, making late events from
   prior sessions indistinguishable. Startup events were emitted retrospectively.
6. Both M06 physical tests remained unexecuted because sandbox binding failed and
   escalation was rejected by the approval service's usage limit.

## Required remediation

- Add a separately callable shared cancellation controller that preempts every
  startup/live operation and reaps both process groups with exit 130 semantics.
- Centralize generation-aware MI ingestion and dispatch every stream/async record;
  wait for actual halt completion and clean up immediately on all fatal failures.
- Enforce the size limit per newline-delimited record.
- Own monotonically increasing generations above individual reconnect instances.
- Rerun both physical tests with exact authorization, verify cleanup, and restore
  and verify the final SUNSIGHT firmware.

M07 remains blocked until the remediated M06 commit is independently approved.
