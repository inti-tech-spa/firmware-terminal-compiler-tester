//! OpenOCD/GDB session boundary. Implemented in milestones M5 and M6.

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
