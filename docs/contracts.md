# Public contracts

## CLI

The v1 command surface is:

```text
samdebug setup [--offline] [--output human|json]
samdebug doctor [--output human|json]
samdebug init --from-cproj PATH --configuration Debug|Release [--output human|json]
samdebug build [--output human|json]
samdebug clean [--output human|json]
samdebug artifacts [--output human|json]
samdebug probe list [--output human|json]
samdebug erase --probe SERIAL --confirm erase:SERIAL [--output human|json]
samdebug flash --probe SERIAL --confirm flash:SERIAL [--output human|json]
samdebug debug
samdebug debug --agent --stdio
```

Machine mode never prompts, emits ANSI escapes, or writes non-protocol data to
stdout. Logs go to stderr. Finite results validate against
`schemas/result-v1.schema.json`. Persistent sessions validate each line against
`schemas/debug-v1.schema.json`.

## Exit codes

- `0`: success
- `2`: command or configuration error
- `3`: tool installation or dependency error
- `4`: project import or build error
- `5`: probe or target connection error
- `6`: erase, flash, or verification error
- `7`: debugger or session error
- `8`: authorization required or rejected
- `130`: interrupted

Error objects also contain stable uppercase snake-case codes. Consumers must not
parse human messages.

## Agent protocol

The first line sent by samdebug is a `hello` event containing protocol version
`1`. Requests contain `schema_version`, unique `id`, `kind: "request"`, an
operation, and an object payload. Each request receives exactly one response
with the same `id`; events may be interleaved and have no request id.

Initial operations are `session.start`, `session.stop`, `target.halt`,
`target.continue`, `target.reset`, `target.step`, `target.next`,
`breakpoint.insert`, `breakpoint.remove`, `stack.list`, `variables.list`,
`registers.read`, `memory.read`, and `firmware.load`.

Unknown protocol fields are rejected in v1; compatibility is provided by an
explicit schema-version change rather than by silently accepting ambiguous
input. Unknown operations or unsupported protocol versions are rejected. Input lines have a 1 MiB
limit. Malformed input produces a typed response when an id can be recovered;
otherwise it produces a protocol error event and the session remains alive
unless framing can no longer be trusted.

Operation payloads and results are normative in
`schemas/debug-v1.schema.json`. Agent `session.start` requires the exact probe
serial. The ELF is the artifact selected by `samdebug.toml`; v1 does not accept
an arbitrary ELF path over the agent protocol. Memory reads are limited to
65,536 bytes per request.

Asynchronous events are also closed, normative schema variants. Session startup
emits `probe.selected`, `server.starting`, `server.ready`, `gdb.starting`, state,
and stopped events in order. Target execution emits running/stopped/reset;
loading emits progress/loaded; termination emits session errors, cancellation,
and `session.stopped` as applicable. Every session event carries its generation
so consumers can discard stale records.

## Authorization

`erase`, `flash`, `firmware.load`, memory writes, and raw monitor commands are
privileged. CLI authorization is exactly `<operation>:<probe-serial>` and is
compared after probe selection. Agent requests carry the same authorization in
their `payload.authorization` object as `operation` and `probe_serial`. A token
authorizes one operation only, is consumed before execution, is never cached,
and is invalid if the selected probe differs.

The TUI displays the operation and detected probe serial, then requires the
operator to type the exact `<operation>:<probe-serial>` text into a modal. The
modal clears on cancel, failure, completion, probe change, disconnect, or
session-generation change. Debug firmware loading uses
`firmware.load:<probe-serial>` through both TUI and agent interfaces.

There is no generic `--yes`. Read-only inspection never acquires write
authorization implicitly. Raw monitor commands and memory writes are not in the
v1 public interface.

## Configuration

`samdebug.toml` schema version 1 contains project kind
`microchip-studio-cproj`, project path, configuration, fixed device
`ATSAM4SD32C`, managed tool channel, probe kind
`atmel-ice`, transport `swd`, speed in kHz (default 1000), and optional serial.
Build plans and artifacts always use the fixed project-local `.samdebug` root.
Unknown keys warn in human mode and appear as structured warnings in machine
mode. Unknown schema major versions fail.
