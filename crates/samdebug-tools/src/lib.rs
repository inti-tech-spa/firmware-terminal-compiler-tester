//! Managed tool and process adapters.

use std::time::Duration;

use samdebug_core::{SamdebugResult, ports::ManagedChild};

/// Owns one child and guarantees bounded cleanup on every drop path.
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

    pub fn shutdown(&mut self) -> SamdebugResult<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        child.terminate()?;
        if child.wait_timeout(self.grace)?.is_none() {
            child.kill()?;
            let _ = child.wait_timeout(self.grace)?;
        }
        Ok(())
    }
}

impl Drop for ChildSupervisor {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::ChildSupervisor;
    use samdebug_core::{SamdebugResult, ports::ManagedChild};

    #[derive(Debug, Default)]
    struct Calls {
        terminate: usize,
        kill: usize,
        waits: usize,
    }

    #[derive(Debug)]
    struct FakeChild {
        calls: Arc<Mutex<Calls>>,
        exits_after_terminate: bool,
    }

    impl ManagedChild for FakeChild {
        fn id(&self) -> u32 {
            42
        }
        fn try_wait(&mut self) -> SamdebugResult<Option<i32>> {
            Ok(None)
        }
        fn terminate(&mut self) -> SamdebugResult<()> {
            self.calls.lock().expect("lock").terminate += 1;
            Ok(())
        }
        fn kill(&mut self) -> SamdebugResult<()> {
            self.calls.lock().expect("lock").kill += 1;
            Ok(())
        }
        fn wait_timeout(&mut self, _timeout: Duration) -> SamdebugResult<Option<i32>> {
            let mut calls = self.calls.lock().expect("lock");
            calls.waits += 1;
            Ok((self.exits_after_terminate || calls.kill > 0).then_some(0))
        }
    }

    #[test]
    fn drop_terminates_cooperative_child() {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let child = FakeChild {
            calls: Arc::clone(&calls),
            exits_after_terminate: true,
        };
        drop(ChildSupervisor::new(
            Box::new(child),
            Duration::from_millis(1),
        ));
        let calls = calls.lock().expect("lock");
        assert_eq!(calls.terminate, 1);
        assert_eq!(calls.kill, 0);
    }

    #[test]
    fn drop_kills_uncooperative_child() {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let child = FakeChild {
            calls: Arc::clone(&calls),
            exits_after_terminate: false,
        };
        drop(ChildSupervisor::new(
            Box::new(child),
            Duration::from_millis(1),
        ));
        let calls = calls.lock().expect("lock");
        assert_eq!(calls.terminate, 1);
        assert_eq!(calls.kill, 1);
        assert_eq!(calls.waits, 2);
    }
}
