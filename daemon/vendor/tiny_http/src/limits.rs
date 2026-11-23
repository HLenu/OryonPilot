//! Optional transport bounds used by OryonPilot. See ../PATCHES.md.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Resource limits for a short-lived HTTP connection.
///
/// Limited servers serve one request per connection. The deadline is measured
/// from accept and never extended by received bytes or a completed request.
#[derive(Clone, Copy, Debug)]
pub struct ServerLimits {
    pub max_connections: usize,
    pub connection_timeout: Duration,
    pub max_header_bytes: usize,
    pub max_connection_bytes: usize,
}

impl ServerLimits {
    pub(crate) fn validate(self) -> io::Result<()> {
        if self.max_connections == 0
            || self.connection_timeout.is_zero()
            || self.connection_timeout > Duration::from_secs(60)
            || self.max_header_bytes == 0
            || self.max_connection_bytes < self.max_header_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid HTTP limits",
            ));
        }
        Ok(())
    }
}

pub(crate) struct ConnectionGuard {
    active: Arc<AtomicUsize>,
    deadline: Instant,
    remaining_bytes: AtomicUsize,
}

impl ConnectionGuard {
    #[allow(deprecated)] // fetch_update remains available on the project's Rust 1.80 minimum.
    pub(crate) fn acquire(active: &Arc<AtomicUsize>, limits: ServerLimits) -> Option<Arc<Self>> {
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < limits.max_connections).then(|| n + 1)
            })
            .ok()?;
        Some(Arc::new(Self {
            active: active.clone(),
            deadline: Instant::now() + limits.connection_timeout,
            remaining_bytes: AtomicUsize::new(limits.max_connection_bytes),
        }))
    }

    pub(crate) fn timeout(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "HTTP connection deadline"))
    }

    pub(crate) fn remaining_bytes(&self) -> usize {
        self.remaining_bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn received(&self, count: usize) {
        self.remaining_bytes.fetch_sub(count, Ordering::Relaxed);
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}
