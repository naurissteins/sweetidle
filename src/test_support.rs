use std::{
    thread,
    time::{Duration, Instant},
};

const WAIT_LIMIT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(crate) struct TestDeadline {
    expires_at: Instant,
}

impl TestDeadline {
    pub(crate) fn new() -> Self {
        Self {
            expires_at: Instant::now() + WAIT_LIMIT,
        }
    }

    pub(crate) fn remaining(&self, waiting_for: &str) -> Duration {
        let remaining = self.expires_at.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {waiting_for}");
        remaining
    }

    pub(crate) fn wait(&self, waiting_for: &str) {
        thread::sleep(self.remaining(waiting_for).min(POLL_INTERVAL));
    }
}
