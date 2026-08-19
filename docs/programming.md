# Atmel-ICE programming

`samdebug probe list` discovers Atmel-ICE devices through macOS USB metadata and
reports their serials without opening the target. Destructive operations require
an explicitly selected, unique serial and the exact one-operation confirmation:

```text
samdebug erase --probe SERIAL --confirm erase:SERIAL
samdebug flash --probe SERIAL --confirm flash:SERIAL
```

Selection occurs before confirmation is consumed. Missing, unknown, ambiguous,
or disconnected probes fail with connection exit code 5; missing or mismatched
confirmation fails with authorization exit code 8. No separate Atmel-ICE driver
is installed on macOS because OpenOCD uses the CMSIS-DAP HID interface.

For each command, samdebug reserves dynamic GDB, TCL, and telnet ports on
`127.0.0.1`, starts the pinned OpenOCD with `cmsis_dap_backend hid`, SWD,
`target/at91sam4sXX.cfg`, the configured probe serial, and the configured adapter
speed. The process is time-bounded, cancellation-aware, and owned until it has
exited; normal and error paths issue OpenOCD shutdown and leave no server or
probe lock behind.

Erase operates on flash bank 0 and runs OpenOCD's erase check before reporting
success. Flash accepts only the ELF selected by `samdebug.toml` under the managed
build directory. OpenOCD derives load addresses from that ELF, writes with
erase, performs a separate `verify_image`, then resets the target to run. The
public CLI does not accept raw addresses, arbitrary ELF paths, monitor commands,
or memory writes.

Connection diagnostics distinguish absent or ambiguous probes, missing target
voltage, unreachable targets, locked/protected targets, and USB removal.
Programming and independent verification failures use exit code 6. Ctrl-C
terminates the complete OpenOCD process group and returns exit code 130.
