//! Owned `OpenOCD` programming and GDB session boundary.

mod programming;

pub use programming::{
    FirmwareArtifact, OpenOcdConfig, OpenOcdProgrammer, ProbeListReport, ProbeRecord,
    ProgramOperation, ProgrammingReport, list_probes,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    ProbeSelected,
    ServerStarting,
    ServerReady,
    GdbStarting,
    Connected,
    Halted,
    Running,
    Failed,
    Cancelling,
    Disconnecting,
}
