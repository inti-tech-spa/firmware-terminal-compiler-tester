//! Managed tool and process adapters.

use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use samdebug_core::{
    CancellationToken, ErrorCategory, SamdebugError, SamdebugResult,
    ports::{CommandOutput, CommandSpec, ManagedChild, ProcessRunner},
};

/// Owns one child and performs best-effort bounded escalation on every exit path.
#[derive(Debug)]
pub struct ChildSupervisor {
    child: Option<Box<dyn ManagedChild>>,
    grace: Duration,
}

impl ChildSupervisor {
    #[must_use]
    pub fn new(child: Box<dyn ManagedChild>, grace: Duration) -> Self {
        Self {
            child: Some(child),
            grace,
        }
    }

    #[must_use]
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().map(|child| child.id())
    }

    /// Polls a running child until it exits or cancellation requests cleanup.
    pub fn wait_until_exit(
        &mut self,
        cancellation: &CancellationToken,
        poll_interval: Duration,
    ) -> SamdebugResult<i32> {
        loop {
            if cancellation.is_cancelled() {
                let _ = self.shutdown();
                return Err(SamdebugError::new(
                    ErrorCategory::Interrupted,
                    "INTERRUPTED",
                    "operation interrupted",
                ));
            }

            let Some(child) = self.child.as_mut() else {
                return Err(SamdebugError::new(
                    ErrorCategory::Tool,
                    "CHILD_NOT_RUNNING",
                    "managed child is not running",
                ));
            };
            match child.try_wait() {
                Ok(Some(code)) => {
                    self.child.take();
                    return Ok(code);
                }
                Ok(None) => thread::sleep(poll_interval),
                Err(error) => {
                    let _ = self.shutdown();
                    return Err(error);
                }
            }
        }
    }

    /// Attempts every cleanup stage even when an earlier stage fails. Ownership
    /// is released only after the child is observed to have exited.
    pub fn shutdown(&mut self) -> SamdebugResult<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let mut first_error = None;
        let mut exited = false;

        match child.try_wait() {
            Ok(Some(_)) => exited = true,
            Ok(None) => {}
            Err(error) => remember_first(&mut first_error, error),
        }
        if !exited {
            if let Err(error) = child.terminate() {
                remember_first(&mut first_error, error);
            }
            match child.wait_timeout(self.grace) {
                Ok(Some(_)) => exited = true,
                Ok(None) => {}
                Err(error) => remember_first(&mut first_error, error),
            }
        }
        if !exited {
            if let Err(error) = child.kill() {
                remember_first(&mut first_error, error);
            }
            match child.wait_timeout(self.grace) {
                Ok(Some(_)) => exited = true,
                Ok(None) => {}
                Err(error) => remember_first(&mut first_error, error),
            }
        }

        if exited {
            self.child.take();
        } else if first_error.is_none() {
            first_error = Some(SamdebugError::new(
                ErrorCategory::Tool,
                "CHILD_CLEANUP_INCOMPLETE",
                "child did not exit within the bounded cleanup period",
            ));
        }

        first_error.map_or(Ok(()), Err)
    }
}

fn remember_first(first: &mut Option<SamdebugError>, error: SamdebugError) {
    if first.is_none() {
        *first = Some(error);
    }
}

impl Drop for ChildSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[derive(Debug, Default)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, command: &CommandSpec) -> SamdebugResult<CommandOutput> {
        let output = make_command(command)
            .output()
            .map_err(|error| process_error("PROCESS_RUN_FAILED", &error))?;
        Ok(CommandOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn spawn(&self, command: &CommandSpec) -> SamdebugResult<Box<dyn ManagedChild>> {
        let child = make_command(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| process_error("PROCESS_SPAWN_FAILED", &error))?;
        Ok(Box::new(SystemChild { child }))
    }
}

fn make_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(directory) = &spec.current_dir {
        command.current_dir(directory);
    }
    command
}

fn process_error(code: &str, error: &std::io::Error) -> SamdebugError {
    SamdebugError::new(ErrorCategory::Tool, code, error.to_string())
}

#[derive(Debug)]
struct SystemChild {
    child: std::process::Child,
}

impl ManagedChild for SystemChild {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> SamdebugResult<Option<i32>> {
        self.child
            .try_wait()
            .map(|status| {
                status
                    .and_then(|value| value.code())
                    .or_else(|| status.map(|_| 1))
            })
            .map_err(|error| process_error("PROCESS_WAIT_FAILED", &error))
    }

    fn terminate(&mut self) -> SamdebugResult<()> {
        self.child
            .kill()
            .map_err(|error| process_error("PROCESS_TERMINATE_FAILED", &error))
    }

    fn kill(&mut self) -> SamdebugResult<()> {
        self.child
            .kill()
            .map_err(|error| process_error("PROCESS_KILL_FAILED", &error))
    }

    fn wait_timeout(&mut self, timeout: Duration) -> SamdebugResult<Option<i32>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(code) = self.try_wait()? {
                return Ok(Some(code));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::ChildSupervisor;
    use samdebug_core::{ErrorCategory, SamdebugError, SamdebugResult, ports::ManagedChild};

    #[derive(Debug, Default)]
    struct Calls {
        try_wait: usize,
        terminate: usize,
        kill: usize,
        waits: usize,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum FailurePoint {
        TryWait,
        Terminate,
        FirstWait,
        SecondWait,
        Kill,
    }

    #[derive(Debug, Default)]
    struct Failures {
        points: Vec<FailurePoint>,
    }

    impl Failures {
        fn at(point: FailurePoint) -> Self {
            Self {
                points: vec![point],
            }
        }

        fn at_both(first: FailurePoint, second: FailurePoint) -> Self {
            Self {
                points: vec![first, second],
            }
        }

        fn contains(&self, point: FailurePoint) -> bool {
            self.points.contains(&point)
        }
    }

    #[derive(Debug)]
    struct FakeChild {
        calls: Arc<Mutex<Calls>>,
        failures: Failures,
        exits_after_terminate: bool,
    }

    fn failure(code: &str) -> SamdebugError {
        SamdebugError::new(ErrorCategory::Tool, code, "injected cleanup failure")
    }

    impl ManagedChild for FakeChild {
        fn id(&self) -> u32 {
            42
        }
        fn try_wait(&mut self) -> SamdebugResult<Option<i32>> {
            self.calls.lock().expect("lock").try_wait += 1;
            if self.failures.contains(FailurePoint::TryWait) {
                Err(failure("TRY_WAIT"))
            } else {
                Ok(None)
            }
        }
        fn terminate(&mut self) -> SamdebugResult<()> {
            self.calls.lock().expect("lock").terminate += 1;
            if self.failures.contains(FailurePoint::Terminate) {
                Err(failure("TERMINATE"))
            } else {
                Ok(())
            }
        }
        fn kill(&mut self) -> SamdebugResult<()> {
            self.calls.lock().expect("lock").kill += 1;
            if self.failures.contains(FailurePoint::Kill) {
                Err(failure("KILL"))
            } else {
                Ok(())
            }
        }
        fn wait_timeout(&mut self, _timeout: Duration) -> SamdebugResult<Option<i32>> {
            let mut calls = self.calls.lock().expect("lock");
            calls.waits += 1;
            if (calls.waits == 1 && self.failures.contains(FailurePoint::FirstWait))
                || (calls.waits == 2 && self.failures.contains(FailurePoint::SecondWait))
            {
                return Err(failure("WAIT"));
            }
            Ok((self.exits_after_terminate || calls.kill > 0).then_some(0))
        }
    }

    fn supervisor(
        failures: Failures,
        exits_after_terminate: bool,
    ) -> (ChildSupervisor, Arc<Mutex<Calls>>) {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let child = FakeChild {
            calls: Arc::clone(&calls),
            failures,
            exits_after_terminate,
        };
        (
            ChildSupervisor::new(Box::new(child), Duration::from_millis(1)),
            calls,
        )
    }

    #[test]
    fn drop_terminates_cooperative_child() {
        let (supervisor, calls) = supervisor(Failures::default(), true);
        drop(supervisor);
        let calls = calls.lock().expect("lock");
        assert_eq!(calls.terminate, 1);
        assert_eq!(calls.kill, 0);
    }

    #[test]
    fn drop_kills_uncooperative_child() {
        let (supervisor, calls) = supervisor(Failures::default(), false);
        drop(supervisor);
        let calls = calls.lock().expect("lock");
        assert_eq!(calls.terminate, 1);
        assert_eq!(calls.kill, 1);
        assert_eq!(calls.waits, 2);
    }

    #[test]
    fn cleanup_escalates_after_try_wait_terminate_and_first_wait_errors() {
        for failures in [
            Failures::at(FailurePoint::TryWait),
            Failures::at(FailurePoint::Terminate),
            Failures::at(FailurePoint::FirstWait),
        ] {
            let (mut supervisor, calls) = supervisor(failures, false);
            assert!(supervisor.shutdown().is_err());
            let calls = calls.lock().expect("lock");
            assert_eq!(calls.terminate, 1);
            assert_eq!(calls.kill, 1);
            assert_eq!(calls.waits, 2);
        }
    }

    #[test]
    fn cleanup_attempts_final_wait_after_kill_error() {
        let failures = Failures::at(FailurePoint::Kill);
        let (mut supervisor, calls) = supervisor(failures, false);
        assert!(supervisor.shutdown().is_err());
        let calls = calls.lock().expect("lock");
        assert_eq!(calls.kill, 1);
        assert_eq!(calls.waits, 2);
    }

    #[test]
    fn ownership_is_retained_when_both_waits_fail() {
        let failures = Failures::at_both(FailurePoint::FirstWait, FailurePoint::SecondWait);
        let (mut supervisor, _calls) = supervisor(failures, false);
        assert!(supervisor.shutdown().is_err());
        assert_eq!(supervisor.id(), Some(42));
    }
}
