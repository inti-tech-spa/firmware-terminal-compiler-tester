//! Side-effect ports injected into application services.

use std::{path::Path, time::Duration};

use crate::{CancellationToken, SamdebugResult};

pub trait FileSystem: std::fmt::Debug + Send + Sync {
    fn read_to_string(&self, path: &Path) -> SamdebugResult<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadReceipt {
    pub final_url: String,
}

pub trait Downloader: std::fmt::Debug + Send + Sync {
    fn download(
        &self,
        url: &str,
        allowed_hosts: &[String],
        destination: &Path,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<DownloadReceipt>;
}

pub trait Clock: std::fmt::Debug + Send + Sync {
    fn monotonic_millis(&self) -> u64;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeInfo {
    pub serial: String,
    pub product: String,
}

pub trait ProbeProvider: std::fmt::Debug + Send + Sync {
    fn list(&self) -> SamdebugResult<Vec<ProbeInfo>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub current_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait ManagedChild: std::fmt::Debug + Send {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> SamdebugResult<Option<i32>>;
    fn terminate(&mut self) -> SamdebugResult<()>;
    fn kill(&mut self) -> SamdebugResult<()>;
    fn wait_timeout(&mut self, timeout: Duration) -> SamdebugResult<Option<i32>>;
}

pub trait ProcessRunner: std::fmt::Debug + Send + Sync {
    fn run(&self, command: &CommandSpec) -> SamdebugResult<CommandOutput>;
    fn run_cancellable(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
    ) -> SamdebugResult<CommandOutput> {
        if cancellation.is_cancelled() {
            return Err(crate::SamdebugError::new(
                crate::ErrorCategory::Interrupted,
                "INTERRUPTED",
                "operation interrupted",
            ));
        }
        self.run(command)
    }
    fn run_cancellable_with_timeout(
        &self,
        command: &CommandSpec,
        cancellation: &CancellationToken,
        _timeout: Duration,
    ) -> SamdebugResult<CommandOutput> {
        self.run_cancellable(command, cancellation)
    }
    fn spawn(&self, command: &CommandSpec) -> SamdebugResult<Box<dyn ManagedChild>>;
}
