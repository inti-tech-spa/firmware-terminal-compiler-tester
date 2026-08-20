# Terminal debugging

`samdebug debug` starts the native Ratatui interface. It displays source and
disassembly around the current instruction, stack, locals, named registers,
breakpoints, target output, OpenOCD/GDB logs, status, and keyboard help. It
falls back to a resize message below 70×20 cells. `Tab` moves between panes;
`c`, `h`, `s`, `n`, and `r` continue, halt, step, next, and reset-halt; `b` and
`d` insert and remove breakpoints; `l` opens a firmware-load modal that requires
typing the exact `firmware.load:<probe-serial>` authorization; and `q` shuts
down the owned session. Normal exit, Ctrl-C, errors,
and unwinding restore raw mode and the alternate screen.

`samdebug debug --agent --stdio` starts the noninteractive NDJSON protocol from
`schemas/debug-v1.schema.json`. Standard output contains only one JSON message
per line; diagnostic logging stays on standard error. The process emits a
`hello` event, accepts versioned requests, returns one response per valid
request, and can emit generation-bearing asynchronous events. Malformed or
oversized input produces `protocol.error` without prompting. EOF requests a
clean shutdown. Firmware loading requires an exact
`firmware.load:<probe-serial>` authorization in its request payload.

## Shared GDB/MI engine

The M06 engine owns a persistent loopback-only OpenOCD server and a managed
`arm-none-eabi-gdb --interpreter=mi2 --nx --quiet` child. OpenOCD uses the exact
selected Atmel-ICE serial, CMSIS-DAP HID, SWD, `at91sam4sXX.cfg`, and configured
adapter speed. It selects distinct dynamic GDB, TCL, and telnet ports and retries
a fresh set after a bind collision. Readiness is taken from OpenOCD's GDB-listener
event, not from a disposable client connection.

Both children run in owned process groups under one background supervisor. A
shared cancellation token covers OpenOCD readiness, GDB launch and connection,
every live MI operation (including firmware download), and stop waits. A cloned
`DebugCancellationController` can be called from another thread while the
session owner is blocked in an operation. Normal stop, startup failure, command
timeout, cancellation, panic/drop, parser failure, transport loss, GDB exit, and
OpenOCD exit terminate and reap both exact process groups. A frontend polling
events observes failure or cancellation cleanup through `failed`/`cancelling`,
`disconnecting`, and `idle`. Reconnect is explicit and receives a process-wide
monotonically increasing generation rather than resetting the counter in a new
engine instance.

The shared `SessionEngine` is the only command/state implementation used by
the TUI and agent frontends. It supports continue, halt, reset-halt, source
step, next, permanent and temporary breakpoints, stack frames, frame variables,
named registers, and memory reads of at most 65,536 bytes. All command and idle
poll paths use the same MI record dispatcher, so target stdout and GDB
console/log streams become typed generation-bearing events even when they are
interleaved with a query or download. Untokened stop records are accepted for a
command only after that command's tokened result; records drained before a new
command cannot complete it.

Firmware load is accepted only while halted and only after the exact
`firmware.load:<probe-serial>` authorization. GDB downloads the already selected
managed ELF, runs `compare-sections`, rejects mismatch diagnostics, and then
reset-halts the target. Each startup, reset, and load verifies the halt by
requesting the current stack frame before publishing the halted state. Raw
monitor commands, arbitrary ELF paths, memory writes,
and raw addresses for writes are not public operations. The fixed internal
`monitor reset halt` command is constructed by samdebug and cannot contain user
input.

The MI parser accepts fragmented byte streams, result/exec/status/notify records,
console/target/log streams, tuples, mixed lists, C escapes, and asynchronous stop
records. It rejects invalid UTF-8, malformed syntax, unsupported escapes,
truncated records, and each individual newline-delimited record over 1 MiB,
including one delivered complete in a single chunk, without panicking. Each
command has a monotonic token and a bounded deadline; late records remain
generation-scoped.

Physical acceptance uses the managed SUNSIGHT ELF and covers connection halted,
break at `main`, continue, stop events, stack/variables/registers/memory, step,
next, reset-halt, authorized load and compare, clean stop, explicit reconnect,
and final cleanup.
