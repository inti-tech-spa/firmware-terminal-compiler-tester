# GDB/MI debugging engine

The M06 engine owns a persistent loopback-only OpenOCD server and a managed
`arm-none-eabi-gdb --interpreter=mi2 --nx --quiet` child. OpenOCD uses the exact
selected Atmel-ICE serial, CMSIS-DAP HID, SWD, `at91sam4sXX.cfg`, and configured
adapter speed. It selects distinct dynamic GDB, TCL, and telnet ports and retries
a fresh set after a bind collision. Readiness is taken from OpenOCD's GDB-listener
event, not from a disposable client connection.

Both children run in owned process groups. Normal stop, startup failure, command
timeout, cancellation, panic/drop, GDB exit, and OpenOCD exit close input,
request graceful termination, then terminate and reap the exact process groups.
Reconnect is explicit: a failed generation cleans up through `failed` and
`disconnecting` to `idle`; the caller then starts a new owned session.

The shared `SessionEngine` is the only command/state implementation used by
future TUI and agent frontends. It supports continue, halt, reset-halt, source
step, next, permanent and temporary breakpoints, stack frames, frame variables,
named registers, and memory reads of at most 65,536 bytes. Target stdout and
GDB console/log streams become typed generation-bearing events.

Firmware load is accepted only while halted and only after the exact
`firmware.load:<probe-serial>` authorization. GDB downloads the already selected
managed ELF, runs `compare-sections`, rejects mismatch diagnostics, and then
reset-halts the target. Raw monitor commands, arbitrary ELF paths, memory writes,
and raw addresses for writes are not public operations. The fixed internal
`monitor reset halt` command is constructed by samdebug and cannot contain user
input.

The MI parser accepts fragmented byte streams, result/exec/status/notify records,
console/target/log streams, tuples, mixed lists, C escapes, and asynchronous stop
records. It rejects invalid UTF-8, malformed syntax, unsupported escapes,
truncated records, and records over 1 MiB without panicking. Each command has a
monotonic token and a bounded deadline; late records remain generation-scoped.

Physical acceptance uses the managed SUNSIGHT ELF and covers connection halted,
break at `main`, continue, stop events, stack/variables/registers/memory, step,
next, reset-halt, authorized load and compare, clean stop, explicit reconnect,
and final cleanup.
