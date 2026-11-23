//! Independent control-loop liveness monitor. No shared Runtime mutex is used.
//! A stalled process exits with its journal intact; the module supervisor then
//! restarts it through the normal recovery path. Never reboot or signal another PID.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const STALL_BUDGET_MS: u64 = 30_000;
pub const STALL_EXIT_CODE: i32 = 70;

#[derive(Default)]
pub struct Progress {
    deadline_ms: AtomicU64,
    stopped: AtomicBool,
}

impl Progress {
    pub fn allow_until(&self, deadline_ms: u64) {
        self.deadline_ms.store(deadline_ms, Ordering::Release);
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        let deadline = self.deadline_ms.load(Ordering::Acquire);
        !self.stopped.load(Ordering::Acquire) && deadline != 0 && now_ms > deadline
    }
}

pub struct Monitor {
    progress: Arc<Progress>,
    worker: Option<JoinHandle<()>>,
}

impl Monitor {
    pub fn start(
        progress: Arc<Progress>,
        interval: Duration,
        on_stall: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Self> {
        let state = progress.clone();
        let worker = thread::Builder::new()
            .name("oryon-liveness".into())
            .spawn(move || loop {
                if state.stopped.load(Ordering::Acquire) {
                    return;
                }
                if state.expired(crate::util::mono_ms()) {
                    on_stall();
                    return;
                }
                thread::park_timeout(interval);
            })?;
        Ok(Self {
            progress,
            worker: Some(worker),
        })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.progress.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Mutex};

    #[test]
    fn deadline_includes_planned_idle_and_can_be_renewed() {
        let p = Progress::default();
        assert!(!p.expired(u64::MAX)); // not armed during construction
        p.allow_until(1_000 + 120_000 + STALL_BUDGET_MS);
        assert!(!p.expired(125_000));
        assert!(p.expired(151_001));
        p.allow_until(190_000);
        assert!(!p.expired(151_001));
    }

    #[test]
    fn stalled_runtime_mutex_does_not_block_detection() {
        let runtime_mutex = Mutex::new(());
        let _locked = runtime_mutex.lock().unwrap();
        let progress = Arc::new(Progress::default());
        progress.allow_until(crate::util::mono_ms() + 20);
        let (tx, rx) = mpsc::channel();
        let _monitor = Monitor::start(progress, Duration::from_millis(2), move || {
            tx.send(()).unwrap();
        })
        .unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn monitor_drop_cancels_even_a_long_wait() {
        let progress = Arc::new(Progress::default());
        progress.allow_until(crate::util::mono_ms() + 100_000);
        let (tx, rx) = mpsc::channel();
        let monitor = Monitor::start(progress, Duration::from_secs(30), move || {
            let _ = tx.send(());
        })
        .unwrap();
        let started = std::time::Instant::now();
        drop(monitor);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
    }

    #[test]
    fn stalled_process_fixture() {
        if std::env::var_os("ORYON_LIVENESS_CHILD").is_none() {
            return;
        }
        let progress = Arc::new(Progress::default());
        progress.allow_until(crate::util::mono_ms() + 20);
        let _monitor = Monitor::start(progress, Duration::from_millis(2), || {
            std::process::exit(STALL_EXIT_CODE);
        })
        .unwrap();
        thread::sleep(Duration::from_secs(2));
        panic!("stalled process was not terminated");
    }

    #[test]
    fn stalled_process_exits_with_supervisor_recovery_code() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "safety::liveness::tests::stalled_process_fixture",
            ])
            .env("ORYON_LIVENESS_CHILD", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(STALL_EXIT_CODE));
    }
}
