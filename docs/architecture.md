# Architecture

## Boundaries

The Rust workspace will use a shared application core. Neither frontend may
contain build, programming, or debugging policy.

- `samdebug-core`: configuration, typed errors, state machines, authorization,
  and application services.
- `samdebug-project`: read-only `.cproj` import and normalized build plans.
- `samdebug-tools`: secure managed-tool installation and process supervision.
- `samdebug-debug`: OpenOCD lifecycle, GDB/MI parsing, and debug sessions.
- `samdebug-cli`: human commands and versioned JSON/NDJSON frontend.
- `samdebug-tui`: Ratatui frontend over the same core services.

Filesystem, downloader, process, clock, terminal, and probe access are injected
behind traits. Unit tests use fakes; physical-hardware tests are separately
marked and never silently skipped in an audit that requires them.

## Process ownership

`samdebug` owns every OpenOCD and GDB child it starts. Children are placed in a
supervised session and terminated on normal exit, error, cancellation, Ctrl-C,
or panic. The cleanup sequence is graceful request, bounded wait, process
termination, bounded wait, and final forced termination. A PID captured from the
spawn operation is used; broad process-name killing is forbidden.

OpenOCD binds only to loopback and uses dynamically reserved GDB/TCL/Telnet
ports. GDB is started with MI2 and no user initialization files. Arguments are
always passed as argv arrays, never through a shell.

## Debug state machine

The only valid forward path is:

`idle -> probe_selected -> server_starting -> server_ready -> gdb_starting -> connected -> halted|running -> disconnecting -> idle`

Any state may transition to `failed` or `cancelling`; both must pass through
`disconnecting` before returning to `idle`. Target power loss, probe removal,
OpenOCD exit, and GDB exit are typed events, not generic process failures.

Each session has a monotonically increasing generation. Starting, reconnecting,
or selecting a different probe increments it and invalidates authorization,
pending requests, breakpoints cached only in the target, and late asynchronous
records from earlier generations.

| Current state | Command/event | Next state | Result |
| --- | --- | --- | --- |
| `idle` | `session.start` | `probe_selected` | selection event |
| `probe_selected` | begin server | `server_starting` | starting event |
| `server_starting` | OpenOCD ready | `server_ready` | port event |
| `server_ready` | begin GDB | `gdb_starting` | starting event |
| `gdb_starting` | GDB connected and halted | `connected`, then `halted` | stopped event |
| `halted` | continue | `running` | running event |
| `running` | halt or breakpoint | `halted` | stopped event with reason |
| `halted` | step/next | `running`, then `halted` | running and stopped events |
| `halted` | reset-halt | `halted` | reset and stopped events |
| `running` | reset-halt | `halted` | reset and stopped events |
| `halted` | authorized firmware load | `halted` | progress and loaded events |
| `halted` | breakpoint insert/remove | `halted` | breakpoint result; no state event |
| any live state | `session.stop` | `disconnecting`, then `idle` | stopped event |
| `failed` | automatic cleanup | `disconnecting`, then `idle` | session error, then stopped event |
| any non-idle state | cancellation | `cancelling`, then `disconnecting`, then `idle` | cancelled event |

`session.start` is accepted only in `idle`. Continue is accepted only in
`halted`; halt only in `running`; step, next, firmware load, stack, variables,
register, and memory inspection only in `halted`. Session stop and cancellation
are idempotent. Public operation `target.reset` always means reset-halt and is
valid in `halted` or `running`. Breakpoint insertion and removal are accepted
only in `halted`. All other
command/state pairs fail with `INVALID_SESSION_STATE` and do not change state.

OpenOCD/GDB exit, target power loss, or probe removal moves any live state to
`failed`. Reconnect is not an implicit transition: cleanup reaches `idle`, then
the caller issues a new `session.start`, creating a new generation. Cancellation
during either startup state terminates the partially started children before
the terminal `cancelled` event.

## Build flow

The `.cproj` is parsed on every build into a deterministic normalized build
plan. The plan is written below a fixed project-local `.samdebug/` for
auditability. Compilation,
assembly, linking, and artifact conversion execute without a shell. Vendor
files are read-only inputs. Outputs are isolated under
`.samdebug/build/<configuration>/`. Configurable build or artifact roots are
not supported in v1. Before any write, the nearest existing ancestor and final
destination are canonicalized; a path or symlink escaping the canonical
`.samdebug` root is rejected. Linked sources outside the project remain
read-only inputs and never determine an output location.

ATSAM4SD32C builds always include `-mcpu=cortex-m4`, `-mthumb`, and
`-mfloat-abi=soft`. The imported startup files and linker script remain
authoritative; samdebug does not invent replacements for imported projects.

## Frontends

Finite CLI commands return a domain result that is rendered either as human
text or a versioned JSON envelope. Persistent agent debugging uses NDJSON over
stdio. The TUI subscribes to the same session event stream and invokes the same
commands. TUI and machine interfaces therefore cannot develop independent
debug behavior.
