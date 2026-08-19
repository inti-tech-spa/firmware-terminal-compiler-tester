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

Commands that require a halted target are rejected while running. A reconnect
creates a new session generation so late asynchronous GDB records cannot mutate
new session state.

## Build flow

The `.cproj` is parsed on every build into a deterministic normalized build
plan. The plan is written below `.samdebug/` for auditability. Compilation,
assembly, linking, and artifact conversion execute without a shell. Vendor
files are read-only inputs. Outputs are isolated under
`.samdebug/build/<configuration>/`.

ATSAM4SD32C builds always include `-mcpu=cortex-m4`, `-mthumb`, and
`-mfloat-abi=soft`. The imported startup files and linker script remain
authoritative; samdebug does not invent replacements for imported projects.

## Frontends

Finite CLI commands return a domain result that is rendered either as human
text or a versioned JSON envelope. Persistent agent debugging uses NDJSON over
stdio. The TUI subscribes to the same session event stream and invokes the same
commands. TUI and machine interfaces therefore cannot develop independent
debug behavior.

