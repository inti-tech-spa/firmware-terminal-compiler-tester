//! Owned `OpenOCD` programming and GDB session boundary.

mod debug_server;
mod gdb_process;
mod mi;
mod owned_session;
mod programming;
mod session;

pub use mi::{MiListItem, MiRecord, MiResult, MiStreamParser, MiValue};
pub use owned_session::{DebugCancellationController, OwnedDebugSession};

pub use debug_server::{DebugServerPorts, OpenOcdDebugServer};
pub use gdb_process::{GdbMiConfig, GdbMiProcess};
pub use programming::{
    FirmwareArtifact, OpenOcdConfig, OpenOcdProgrammer, ProbeListReport, ProbeRecord,
    ProgramOperation, ProgrammingReport, list_probes,
};
pub use session::{
    Breakpoint, DebuggerTransport, DisassemblyInstruction, MemoryBlock, MiCommandOutput,
    RegisterValue, SessionEngine, SessionEvent, SessionState, StackFrame, Variable,
};
