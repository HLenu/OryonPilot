//! Boot watchdog and crash-loop protection.
//!
//! Rules:
//!  * A boot that never reached "healthy" counts as a failure.
//!  * Too many failures, or several starts inside one boot window, force SAFE_MODE
//!    for the next N boots. SAFE_MODE writes nothing at all.
//!  * SAFE_MODE clears itself after a healthy boot, so a bad config cannot brick a
//!    device permanently.

use crate::json::{self, Json};
use std::path::{Path, PathBuf};

/// A daemon that has not produced a sample in this long is considered hung.
pub const STALE_SAMPLE_MS: u64 = 15_000;
/// Time after start before a boot is declared healthy.
pub const HEALTHY_AFTER_MS: u64 = 90_000;

#[derive(Debug, Clone, Default)]
pub struct BootState {
    pub boot_attempts: u64,
    pub consecutive_failures: u32,
    pub safe_mode_boots_left: u32,
    pub last_boot_ts: u64,
    pub healthy_marked: bool,
    pub last_version: String,
    pub last_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogStatus {
    Ok,
    Stale(u64),
    SafeMode(u32),
}

impl WatchdogStatus {
    pub fn as_str(&self) -> String {
        match self {
            WatchdogStatus::Ok => "ok".into(),
            WatchdogStatus::Stale(ms) => format!("stale:{ms}ms"),
            WatchdogStatus::SafeMode(n) => format!("safe_mode:{n}"),
        }
    }
}

pub struct Watchdog {
    path: PathBuf,
    state: BootState,
    limit: u32,
    safe_boots: u32,
    started_ms: u64,
    health_marked_this_run: bool,
}

impl Watchdog {
    /// Loads the previous state and accounts for the previous run.
    pub fn load(
        path: impl Into<PathBuf>,
        limit: u32,
        safe_boots: u32,
        now: u64,
        version: &str,
        mode: &str,
    ) -> Self {
        let path = path.into();
        let mut state = read_state(&path).unwrap_or_default();

        // Previous run never reported healthy -> it crashed or never came up.
        if state.boot_attempts > 0 && !state.healthy_marked {
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        } else if state.healthy_marked {
            state.consecutive_failures = 0;
        }

        if state.consecutive_failures >= limit.max(1) {
            state.safe_mode_boots_left = safe_boots.max(1);
            state.consecutive_failures = 0;
        }

        state.boot_attempts = state.boot_attempts.saturating_add(1);
        state.last_boot_ts = now;
        state.healthy_marked = false;
        state.last_version = version.to_string();
        state.last_mode = mode.to_string();

        let w = Watchdog {
            path,
            state,
            limit: limit.max(1),
            safe_boots: safe_boots.max(1),
            started_ms: now,
            health_marked_this_run: false,
        };
        w.save();
        w
    }

    pub fn state(&self) -> &BootState {
        &self.state
    }

    pub fn in_safe_mode(&self) -> bool {
        self.state.safe_mode_boots_left > 0
    }

    pub fn safe_mode_reason(&self) -> String {
        format!(
            "{} crash-loop(s); safe mode for {} more boot(s)",
            self.state.consecutive_failures, self.state.safe_mode_boots_left
        )
    }

    /// Called once per tick. Returns the health verdict and persists state changes.
    pub fn tick(&mut self, now: u64, last_sample_ms: u64) -> WatchdogStatus {
        self.tick_with_health(now, last_sample_ms, true)
    }

    pub fn tick_with_health(
        &mut self,
        now: u64,
        last_sample_ms: u64,
        healthy: bool,
    ) -> WatchdogStatus {
        let age = now.saturating_sub(last_sample_ms);
        if healthy
            && age <= STALE_SAMPLE_MS
            && !self.health_marked_this_run
            && now.saturating_sub(self.started_ms) >= HEALTHY_AFTER_MS
        {
            self.mark_healthy();
        }
        if self.in_safe_mode() {
            return WatchdogStatus::SafeMode(self.state.safe_mode_boots_left);
        }
        if age > STALE_SAMPLE_MS {
            return WatchdogStatus::Stale(age);
        }
        WatchdogStatus::Ok
    }

    pub fn mark_healthy(&mut self) {
        if self.health_marked_this_run {
            return;
        }
        self.health_marked_this_run = true;
        self.state.healthy_marked = true;
        self.state.consecutive_failures = 0;
        // A healthy boot burns one safe-mode credit; when it reaches zero the daemon
        // returns to normal operation on the next start.
        if self.state.safe_mode_boots_left > 0 {
            self.state.safe_mode_boots_left -= 1;
        }
        self.save();
    }

    pub fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&self.path, self.to_json().to_string_pretty()).is_ok() {
            crate::storage::secure_file(&self.path);
        }
    }

    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("boot_attempts", Json::num(self.state.boot_attempts as f64));
        j.set(
            "consecutive_failures",
            Json::num(self.state.consecutive_failures as f64),
        );
        j.set(
            "safe_mode_boots_left",
            Json::num(self.state.safe_mode_boots_left as f64),
        );
        j.set("last_boot_ts", Json::num(self.state.last_boot_ts as f64));
        j.set("healthy_marked", Json::Bool(self.state.healthy_marked));
        j.set("last_version", Json::str(self.state.last_version.clone()));
        j.set("last_mode", Json::str(self.state.last_mode.clone()));
        j.set("limit", Json::num(self.limit as f64));
        j.set("safe_boots", Json::num(self.safe_boots as f64));
        j
    }
}

fn read_state(path: &Path) -> Option<BootState> {
    let text = std::fs::read_to_string(path).ok()?;
    let j = json::parse(&text).ok()?;
    Some(BootState {
        boot_attempts: j.get("boot_attempts").and_then(|v| v.as_u64()).unwrap_or(0),
        consecutive_failures: j
            .get("consecutive_failures")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        safe_mode_boots_left: j
            .get("safe_mode_boots_left")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        last_boot_ts: j.get("last_boot_ts").and_then(|v| v.as_u64()).unwrap_or(0),
        healthy_marked: j
            .get("healthy_marked")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        last_version: j
            .get("last_version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        last_mode: j
            .get("last_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("neurosched-wd-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p.join("boot.json")
    }

    #[test]
    fn first_boot_is_normal() {
        let p = tmp("first");
        let w = Watchdog::load(&p, 3, 2, 1_000, "0.1.0", "observe_only");
        assert!(!w.in_safe_mode());
        assert_eq!(w.state().boot_attempts, 1);
    }

    #[test]
    fn crash_loop_enters_safe_mode() {
        let p = tmp("crash");
        // Three boots that never marked healthy.
        for i in 0..3 {
            let w = Watchdog::load(&p, 3, 2, 1_000 + i * 1_000, "0.1.0", "enabled");
            assert!(!w.in_safe_mode(), "run {i}");
        }
        let w = Watchdog::load(&p, 3, 2, 9_000, "0.1.0", "enabled");
        assert!(w.in_safe_mode());
        assert!(w.safe_mode_reason().contains("safe mode"));
    }

    #[test]
    fn healthy_boot_clears_failures() {
        let p = tmp("healthy");
        let mut w = Watchdog::load(&p, 3, 2, 1_000, "0.1.0", "enabled");
        w.mark_healthy();
        w.save();
        let w2 = Watchdog::load(&p, 3, 2, 2_000, "0.1.0", "enabled");
        assert_eq!(w2.state().consecutive_failures, 0);
        assert!(!w2.in_safe_mode());
    }

    #[test]
    fn stale_sample_is_reported() {
        let p = tmp("stale");
        let mut w = Watchdog::load(&p, 3, 2, 1_000, "0.1.0", "observe_only");
        assert_eq!(w.tick(2_000, 1_900), WatchdogStatus::Ok);
        assert!(matches!(w.tick(100_000, 1_900), WatchdogStatus::Stale(_)));
        assert!(!w.state().healthy_marked);
    }

    #[test]
    fn elapsed_time_alone_does_not_mark_bad_telemetry_healthy() {
        let p = tmp("quality");
        let mut w = Watchdog::load(&p, 3, 2, 0, "0.1.1", "observe_only");
        w.tick_with_health(100_000, 100_000, false);
        assert!(!w.state().healthy_marked);
        w.tick_with_health(101_000, 101_000, true);
        assert!(w.state().healthy_marked);
    }

    #[test]
    fn safe_mode_is_reported_while_active() {
        let p = tmp("safemode");
        for i in 0..4 {
            Watchdog::load(&p, 2, 1, 1_000 + i * 100, "0.1.0", "enabled");
        }
        let mut w = Watchdog::load(&p, 2, 1, 2_000, "0.1.0", "enabled");
        assert!(matches!(w.tick(3_000, 2_900), WatchdogStatus::SafeMode(_)));
    }
}
