use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Cloneable cooperative cancellation signal shared by long-running services.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::CancellationToken;

    #[test]
    fn cancellation_is_shared_across_clones() {
        let first = CancellationToken::new();
        let second = first.clone();
        second.cancel();
        assert!(first.is_cancelled());
    }
}
