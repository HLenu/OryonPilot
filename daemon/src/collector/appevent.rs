//! Foreground application events.
//!
//! Priority order (never `dumpsys` in a loop):
//!  1. `logcat -b events` — one long-lived child process, zero polling.
//!  2. Companion app log (`fg_events.log`) — tailed by size, no polling of the system.
//!  3. `/proc` scan for the `top-app` cgroup — fallback only, low frequency, and it
//!     reports itself as low-trust so the predictor can discount it.
//!
//! `dumpsys activity` is never used: it is expensive, and a per-second call would
//! cost more CPU than everything this daemon is trying to save.

use crate::sys::SysRoot;
use crate::types::{AppEvent, AppEventKind, AppEventSource, ScreenState};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const MAX_EVENTS: usize = 512;
const SOURCE_STALE_MS: u64 = 60_000;

#[cfg(any(target_os = "linux", target_os = "android"))]
fn terminate_when_parent_exits(command: &mut Command, expected_parent: u32) {
    use std::os::unix::process::CommandExt;

    // This runs after fork in a multithreaded daemon. Keep the hook limited to
    // syscall wrappers and allocation-free OS errors, with no locks or logging.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // If the creating parent died before prctl, the signal would have
            // missed that death. Refuse to exec an already orphaned logcat.
            if libc::getppid() as u32 != expected_parent {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn terminate_when_parent_exits(_command: &mut Command, _expected_parent: u32) {}

#[derive(Debug, Clone, Default)]
pub struct WatchState {
    pub foreground: Option<String>,
    pub since_ms: u64,
    /// Last successful foreground observation, separate from the last app change.
    pub last_observed_ms: u64,
    pub source: AppEventSource,
    pub screen: ScreenState,
    pub screen_observed_ms: u64,
    pub logcat_alive: bool,
    pub companion_seen: bool,
    pub proc_scan_used: bool,
    /// False until `start()` has been called; used by the health report so a
    /// not-yet-started watcher is not reported as a failure.
    pub started: bool,
}

#[derive(Clone, Default)]
pub struct AppEventWatcher {
    events: Arc<Mutex<VecDeque<AppEvent>>>,
    state: Arc<Mutex<WatchState>>,
    running: Arc<AtomicBool>,
    proc_scan_ms: Arc<AtomicU64>,
    logcat_child: Arc<Mutex<Option<Child>>>,
    companion: Arc<Mutex<Option<(String, u64)>>>,
}

impl AppEventWatcher {
    pub fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(VecDeque::new())),
            state: Arc::new(Mutex::new(WatchState::default())),
            running: Arc::new(AtomicBool::new(false)),
            proc_scan_ms: Arc::new(AtomicU64::new(5_000)),
            logcat_child: Arc::new(Mutex::new(None)),
            companion: Arc::new(Mutex::new(None)),
        }
    }

    /// Spawns the watcher threads. Safe to call on a device without logcat: each
    /// thread degrades to a no-op and the state reports which source is live.
    pub fn start(&self, root: &SysRoot, companion_log: String, proc_scan_ms: u64) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        self.update_proc_scan_interval(proc_scan_ms);
        if let Ok(mut st) = self.state.lock() {
            st.started = true;
        }
        if !companion_log.is_empty() {
            if let Ok(mut c) = self.companion.lock() {
                *c = Some((companion_log, 0));
            }
        }
        let logcat_ok = root.is_system() && root.exists("/system/bin/logcat");
        if logcat_ok {
            let this = self.clone();
            thread::spawn(move || this.logcat_loop());
        }

        if root.is_system() {
            let this = self.clone();
            thread::spawn(move || this.proc_scan_loop());
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Ok(mut child) = self.logcat_child.lock() {
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
            }
        }
    }

    pub fn update_proc_scan_interval(&self, interval_ms: u64) {
        self.proc_scan_ms
            .store(interval_ms.max(1_000), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> WatchState {
        self.snapshot_at(crate::util::now_ms())
    }

    fn snapshot_at(&self, now: u64) -> WatchState {
        let companion_state = self.companion.lock().ok().and_then(|mut c| c.take());
        if let Some((path, mut offset)) = companion_state {
            self.read_companion(&path, &mut offset);
            if let Ok(mut c) = self.companion.lock() {
                if c.is_none() {
                    *c = Some((path, offset));
                }
            }
        }
        let mut state = self.state.lock().map(|s| s.clone()).unwrap_or_default();
        let live_logcat = state.source == AppEventSource::Logcat && state.logcat_alive;
        if !live_logcat && now.saturating_sub(state.last_observed_ms) > SOURCE_STALE_MS {
            state.foreground = None;
            state.source = AppEventSource::None;
        }
        if now.saturating_sub(state.screen_observed_ms) > SOURCE_STALE_MS {
            state.screen = ScreenState::Unknown;
        }
        state
    }

    pub fn drain(&self) -> Vec<AppEvent> {
        let mut q = match self.events.lock() {
            Ok(q) => q,
            Err(_) => return Vec::new(),
        };
        q.drain(..).collect()
    }

    pub fn recent(&self, limit: usize) -> Vec<AppEvent> {
        self.events
            .lock()
            .map(|q| q.iter().rev().take(limit).rev().cloned().collect())
            .unwrap_or_default()
    }

    fn push(&self, ev: AppEvent, source: AppEventSource) {
        let mut changed = true;
        if let Ok(mut st) = self.state.lock() {
            match ev.kind {
                AppEventKind::Foreground => {
                    if ev.ts_ms < st.last_observed_ms {
                        return;
                    }
                    changed = st.foreground.as_deref() != Some(ev.package.as_str());
                    if changed {
                        st.foreground = Some(ev.package.clone());
                        st.since_ms = ev.ts_ms;
                    }
                    st.last_observed_ms = ev.ts_ms;
                    st.source = source;
                    st.companion_seen |= source == AppEventSource::Companion;
                    st.proc_scan_used |= source == AppEventSource::ProcScan;
                }
                // A process creation or a launch request does not establish the
                // resumed foreground activity.
                AppEventKind::Launch => return,
                AppEventKind::ScreenOn => {
                    st.screen = ScreenState::On;
                    st.screen_observed_ms = ev.ts_ms;
                }
                AppEventKind::ScreenOff => {
                    st.screen = ScreenState::Off;
                    st.screen_observed_ms = ev.ts_ms;
                }
            }
        }
        if changed {
            if let Ok(mut q) = self.events.lock() {
                q.push_back(ev);
                while q.len() > MAX_EVENTS {
                    q.pop_front();
                }
            }
        }
    }

    fn logcat_loop(&self) {
        let mut backoff = 1u64;
        while self.running.load(Ordering::SeqCst) {
            let mut command = Command::new("/system/bin/logcat");
            command
                .args([
                    "-b",
                    "events",
                    "-v",
                    "brief",
                    "-T",
                    "1",
                    "-s",
                    "wm_set_resumed_activity:I",
                    "wm_on_resume_called:I",
                    "wm_on_resumed_called:I",
                    "wm_on_top_resumed_gained_called:I",
                    "screen_toggled:I",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            terminate_when_parent_exits(&mut command, std::process::id());
            let child = command.spawn();

            let mut child: Child = match child {
                Ok(c) => c,
                Err(_) => {
                    if let Ok(mut st) = self.state.lock() {
                        st.logcat_alive = false;
                    }
                    thread::sleep(Duration::from_secs(backoff.min(60)));
                    backoff = (backoff * 2).min(60);
                    continue;
                }
            };
            backoff = 1;
            if let Ok(mut st) = self.state.lock() {
                st.logcat_alive = true;
            }

            let stdout = child.stdout.take();
            if let Ok(mut slot) = self.logcat_child.lock() {
                if !self.running.load(Ordering::SeqCst) {
                    let _ = child.kill();
                }
                *slot = Some(child);
            } else {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            if let Some(stdout) = stdout {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    if !self.running.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(line) = line else { break };
                    if let Some(ev) = parse_logcat_line(&line, crate::util::now_ms()) {
                        self.push(ev, AppEventSource::Logcat);
                    }
                }
            }
            if let Ok(mut slot) = self.logcat_child.lock() {
                if let Some(mut child) = slot.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            if let Ok(mut st) = self.state.lock() {
                st.logcat_alive = false;
            }
            thread::sleep(Duration::from_secs(2));
        }
    }

    fn read_companion(&self, path: &str, offset: &mut u64) {
        if let Ok(mut f) = std::fs::File::open(path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < *offset {
                *offset = 0;
            }
            if len > *offset && f.seek(SeekFrom::Start(*offset)).is_ok() {
                {
                    let mut reader = BufReader::new(f);
                    let mut line = String::new();
                    while let Ok(bytes) = reader.read_line(&mut line) {
                        if bytes == 0 || !line.ends_with('\n') {
                            // Retry a partially appended record on the next pass.
                            break;
                        }
                        *offset += bytes as u64;
                        if let Some(ev) = parse_companion_line(&line, crate::util::now_ms()) {
                            self.push(ev, AppEventSource::Companion);
                        }
                        line.clear();
                    }
                }
            }
        }
    }

    fn proc_scan_loop(&self) {
        while self.running.load(Ordering::SeqCst) {
            let interval = self.proc_scan_ms.load(Ordering::Relaxed);
            thread::sleep(Duration::from_millis(interval));
            let stale = {
                let st = match self.state.lock() {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let now = crate::util::now_ms();
                if st.screen == ScreenState::Off
                    && now.saturating_sub(st.screen_observed_ms) <= SOURCE_STALE_MS
                {
                    continue;
                }
                if st.logcat_alive && st.source == AppEventSource::Logcat && st.foreground.is_some()
                {
                    continue;
                }
                let age = now.saturating_sub(st.last_observed_ms);
                st.foreground.is_none() || age > 20_000
            };
            if !stale {
                continue;
            }
            if let Some(pkg) = scan_top_app(std::path::Path::new("/")) {
                let ev = AppEvent {
                    ts_ms: crate::util::now_ms(),
                    package: pkg.clone(),
                    kind: AppEventKind::Foreground,
                };
                self.push(ev, AppEventSource::ProcScan);
            }
        }
    }
}

/// Parses one `logcat -b events -v brief` line. Pure function: unit tested.
///
/// Examples:
/// `I/wm_set_resumed_activity( 3559): [0,com.android.settings/.Settings,resumeTopActivity]`
/// `10-30 20:00:00.123  1234  5678 I screen_toggled: 1`
pub fn parse_logcat_line(line: &str, now_ms: u64) -> Option<AppEvent> {
    let (tag, payload) = split_tag(line)?;
    match tag {
        "wm_set_resumed_activity"
        | "wm_on_resume_called"
        | "wm_on_top_resumed_gained_called"
        | "wm_on_resumed_called" => {
            // Application lifecycle callbacks may contain a fully qualified
            // Activity class only. It is not an application ID, and truncating
            // its namespace cannot recover one. Require an explicit component.
            let pkg = payload
                .trim()
                .strip_prefix('[')?
                .strip_suffix(']')?
                .split(',')
                .find_map(|field| {
                    let (pkg, activity) = field.trim().split_once('/')?;
                    let class = activity.strip_prefix('.').unwrap_or(activity);
                    (is_package(pkg)
                        && !class.is_empty()
                        && class
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '$')))
                    .then_some(pkg)
                })?;
            Some(AppEvent {
                ts_ms: now_ms,
                package: pkg.to_string(),
                kind: AppEventKind::Foreground,
            })
        }
        "screen_toggled" => {
            let v = payload.trim().trim_start_matches('[').trim_end_matches(']');
            let kind = match v {
                "1" | "on" | "ON" => AppEventKind::ScreenOn,
                "0" | "off" | "OFF" => AppEventKind::ScreenOff,
                _ => return None,
            };
            Some(AppEvent {
                ts_ms: now_ms,
                package: String::new(),
                kind,
            })
        }
        _ => None,
    }
}

fn split_tag(line: &str) -> Option<(&str, &str)> {
    // Brief output is `I/tag( pid): payload`. Also accept captured threadtime
    // records to keep saved diagnostic logs usable.
    let colon = line.find(": ")?;
    let head = &line[..colon];
    let tag = if let Some((_, tag_pid)) = head.split_once('/') {
        tag_pid.split('(').next()?.trim()
    } else {
        head.split_whitespace().last()?
    };
    Some((tag, &line[colon + 2..]))
}

fn is_package(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 3 || s.len() > 127 {
        return false;
    }
    if !s.contains('.') || s.contains('/') || s.contains(' ') {
        return false;
    }
    s.split('.').all(|part| {
        part.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Companion app log: either a bare package per line or one JSON object per line.
pub fn parse_companion_line(line: &str, now_ms: u64) -> Option<AppEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line.starts_with('{') {
        let j = crate::json::parse(line).ok()?;
        let pkg = j
            .get("package")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let kind = match j
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("foreground")
        {
            "screen_on" => AppEventKind::ScreenOn,
            "screen_off" => AppEventKind::ScreenOff,
            "launch" => AppEventKind::Launch,
            _ => AppEventKind::Foreground,
        };
        if matches!(kind, AppEventKind::Foreground | AppEventKind::Launch) && !is_package(&pkg) {
            return None;
        }
        let ts = j.get("ts").and_then(|v| v.as_u64()).unwrap_or(now_ms);
        if ts > now_ms.saturating_add(5_000) {
            return None;
        }
        return Some(AppEvent {
            ts_ms: ts,
            package: pkg,
            kind,
        });
    }
    if is_package(line) {
        Some(AppEvent {
            ts_ms: now_ms,
            package: line.to_string(),
            kind: AppEventKind::Foreground,
        })
    } else {
        None
    }
}

/// Best-effort foreground package from the `top-app` cgroup.
/// Picks the member with the lowest `oom_score_adj` (the resumed activity usually wins).
pub fn scan_top_app(root: &std::path::Path) -> Option<String> {
    let mut best: Option<(i32, String)> = None;
    let rd = std::fs::read_dir(root.join("proc")).ok()?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(cgroup) = std::fs::read_to_string(root.join(format!("proc/{pid}/cgroup"))) else {
            continue;
        };
        if !cgroup.contains("top-app") {
            continue;
        }
        let Ok(cmdline) = std::fs::read(root.join(format!("proc/{pid}/cmdline"))) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&cmdline);
        let Some(first) = cmd.split('\0').next() else {
            continue;
        };
        if !is_package(first) {
            continue;
        }
        let adj = std::fs::read_to_string(root.join(format!("proc/{pid}/oom_score_adj")))
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(1000);
        if best.as_ref().map(|(a, _)| adj < *a).unwrap_or(true) {
            best = Some((adj, first.to_string()));
        }
    }
    best.map(|(_, p)| p)
}

/// Screen state from the backlight, which needs no shell and no binder call.
pub fn read_screen(root: &SysRoot) -> ScreenState {
    for name in root.list("/sys/class/backlight") {
        if let Some(v) = root.read_i64(&format!("/sys/class/backlight/{name}/brightness")) {
            return if v > 0 {
                ScreenState::On
            } else {
                ScreenState::Off
            };
        }
    }
    for name in root.list("/sys/class/leds") {
        if name.contains("lcd-backlight") || name.contains("backlight") {
            if let Some(v) = root.read_i64(&format!("/sys/class/leds/{name}/brightness")) {
                return if v > 0 {
                    ScreenState::On
                } else {
                    ScreenState::Off
                };
            }
        }
    }
    ScreenState::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn parent_death_child_fixture() {
        let Some(mode) = std::env::var_os("ORYONPILOT_PDEATH_CHILD_TEST") else {
            return;
        };
        let mut signal: libc::c_int = 0;
        let result = unsafe {
            libc::prctl(
                libc::PR_GET_PDEATHSIG,
                &mut signal as *mut libc::c_int,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(signal, libc::SIGKILL);
        if mode == "hold" {
            // Bounded lifetime even if the parent-death mechanism regresses.
            thread::sleep(Duration::from_secs(10));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    #[allow(clippy::zombie_processes)] // This fixture must exit before its child.
    fn parent_death_parent_fixture() {
        if std::env::var_os("ORYONPILOT_PDEATH_PARENT_TEST").is_none() {
            return;
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "collector::appevent::tests::parent_death_child_fixture",
            ])
            .env("ORYONPILOT_PDEATH_CHILD_TEST", "hold")
            .stdout(Stdio::inherit())
            .stderr(Stdio::null());
        terminate_when_parent_exits(&mut command, std::process::id());
        // Exiting this fixture process must terminate the executed child. Child's
        // stdout inherits our pipe, so the outer test observes its death as EOF.
        let _child = command.spawn().unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn parent_death_signal_survives_exec() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "collector::appevent::tests::parent_death_child_fixture",
            ])
            .env("ORYONPILOT_PDEATH_CHILD_TEST", "check")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        terminate_when_parent_exits(&mut command, std::process::id());
        assert!(command.status().unwrap().success());
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn unexpected_parent_is_rejected_before_exec() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        terminate_when_parent_exits(&mut command, 0);
        let error = command.spawn().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn parent_exit_terminates_an_executed_child() {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "collector::appevent::tests::parent_death_parent_fixture",
            ])
            .env("ORYONPILOT_PDEATH_PARENT_TEST", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdout = parent.stdout.take().unwrap();
        assert!(parent.wait().unwrap().success());
        let fd = stdout.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut eof = false;
        let mut buffer = [0_u8; 1024];
        while std::time::Instant::now() < deadline {
            match stdout.read(&mut buffer) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("reading child lifetime pipe failed: {error}"),
            }
        }
        assert!(eof, "the executed child outlived its parent");
    }

    #[test]
    fn silent_child_fixture() {
        if std::env::var_os("ORYONPILOT_SILENT_CHILD_TEST").is_some() {
            thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn stop_terminates_a_silent_child_without_waiting_for_an_event() {
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "collector::appevent::tests::silent_child_fixture",
            ])
            .env("ORYONPILOT_SILENT_CHILD_TEST", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let watcher = AppEventWatcher::new();
        watcher.running.store(true, Ordering::SeqCst);
        *watcher.logcat_child.lock().unwrap() = Some(child);
        watcher.stop();

        let mut child = watcher.logcat_child.lock().unwrap().take().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut terminated = false;
        while std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                terminated = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // Reap the fixture even when the assertion fails.
        let _ = child.kill();
        let _ = child.wait();
        assert!(terminated, "stop must interrupt a silent logcat child");
    }

    #[test]
    fn parses_resume_event() {
        let line = "10-30 20:00:00.123  1234  5678 I wm_on_resume_called: [0,123,com.android.settings/.Settings,RESUMED]";
        let ev = parse_logcat_line(line, 42).expect("event");
        assert_eq!(ev.package, "com.android.settings");
        assert_eq!(ev.kind, AppEventKind::Foreground);
        assert_eq!(ev.ts_ms, 42);
    }

    #[test]
    fn parses_top_resumed_gained() {
        let line = "10-30 20:00:00.123  1234  5678 I wm_on_top_resumed_gained_called: [123,com.miui.home/.Launcher,RESUMED]";
        let ev = parse_logcat_line(line, 1).expect("event");
        assert_eq!(ev.package, "com.miui.home");
    }

    #[test]
    fn xiaomi_activity_class_is_not_an_application_id() {
        // Captured HyperOS 4 / Android 17 records: these fields are class names,
        // not ComponentName.flattenToShortString() or application IDs.
        for line in [
            "I/wm_on_resume_called(26533): [0,112383420,com.coolapk.market.view.feed.FeedDetailActivityV8,RESUME_ACTIVITY,2]",
            "I/wm_on_top_resumed_gained_called(26533): [208143044,com.coolapk.market.view.feed.ReplyActivity,topStateChangedWhenResumed]",
        ] {
            assert!(parse_logcat_line(line, 42).is_none());
        }
    }

    #[test]
    fn xiaomi_system_events_keep_one_application_across_activities() {
        let watcher = AppEventWatcher::new();
        for (i, line) in [
            "I/wm_set_resumed_activity( 3559): [0,com.coolapk.market/.view.feed.ReplyActivity,resumeTopActivity - onActivityStateChanged]",
            "I/wm_set_resumed_activity( 3559): [0,com.coolapk.market/.view.feed.FeedDetailActivityV8,resumeTopActivity - onActivityStateChanged]",
            "I/wm_set_resumed_activity( 3559): [0,com.miui.home/.launcher.Launcher,setFocusedTask-alreadyTop]",
        ].iter().enumerate() {
            let event = parse_logcat_line(line, 1_000 + i as u64).expect("system foreground");
            watcher.push(event, AppEventSource::Logcat);
        }
        let events = watcher.drain();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].package, "com.coolapk.market");
        assert_eq!(events[1].package, "com.miui.home");
    }

    #[test]
    fn process_creation_does_not_establish_foreground() {
        assert!(parse_logcat_line(
            "10-30 20:00:00.123  1234  5678 I am_proc_start: [0,4321,10123,com.foo.bar,activity,{x}]",
            1,
        ).is_none());

        let off = parse_logcat_line("10-30 20:00:00.123  1  2 I screen_toggled: 0", 1).unwrap();
        assert_eq!(off.kind, AppEventKind::ScreenOff);
        assert!(parse_logcat_line("I/screen_toggled( 1): 2", 1).is_none());
    }

    #[test]
    fn parses_the_brief_format_requested_from_logcat() {
        let ev = parse_logcat_line(
            "I/wm_on_resume_called( 1234): [0,123,com.android.settings/.Settings,RESUME_ACTIVITY]",
            42,
        )
        .unwrap();
        assert_eq!(ev.package, "com.android.settings");
        assert_eq!(ev.kind, AppEventKind::Foreground);
        assert!(
            parse_logcat_line("[0,123,com.android.settings/.Settings,RESUME_ACTIVITY]", 42)
                .is_none()
        );
    }

    #[test]
    fn repeated_observations_keep_switch_time_and_use_the_actual_source() {
        let watcher = AppEventWatcher::new();
        let event = |ts_ms| AppEvent {
            ts_ms,
            package: "com.example.app".into(),
            kind: AppEventKind::Foreground,
        };
        watcher.push(event(1_000), AppEventSource::Logcat);
        watcher.push(event(30_000), AppEventSource::ProcScan);
        let state = watcher.snapshot_at(30_000);
        assert_eq!(state.since_ms, 1_000);
        assert_eq!(state.last_observed_ms, 30_000);
        assert_eq!(state.source, AppEventSource::ProcScan);
        assert_eq!(watcher.drain().len(), 1);
        watcher.push(
            AppEvent {
                ts_ms: 31_000,
                package: "com.example.background".into(),
                kind: AppEventKind::Launch,
            },
            AppEventSource::Logcat,
        );
        assert_eq!(
            watcher.snapshot_at(31_000).foreground.as_deref(),
            Some("com.example.app")
        );
    }

    #[test]
    fn failed_event_sources_expire_instead_of_retaining_a_foreground_forever() {
        let watcher = AppEventWatcher::new();
        watcher.push(
            AppEvent {
                ts_ms: 1_000,
                package: "com.example.app".into(),
                kind: AppEventKind::Foreground,
            },
            AppEventSource::Logcat,
        );
        watcher.state.lock().unwrap().logcat_alive = true;
        assert_eq!(watcher.snapshot_at(120_000).source, AppEventSource::Logcat);
        watcher.state.lock().unwrap().logcat_alive = false;
        let state = watcher.snapshot_at(120_000);
        assert_eq!(state.source, AppEventSource::None);
        assert!(state.foreground.is_none());
    }

    #[test]
    fn companion_waits_for_a_complete_record_and_counts_crlf_bytes() {
        let dir = std::env::temp_dir().join("neurosched-companion-partial");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.log");
        let path_str = path.to_str().unwrap();
        let watcher = AppEventWatcher::new();
        let mut offset = 0;
        std::fs::write(&path, "com.example.").unwrap();
        watcher.read_companion(path_str, &mut offset);
        assert_eq!(offset, 0);
        assert!(watcher.snapshot().foreground.is_none());
        std::fs::write(&path, "com.example.app\r\n").unwrap();
        watcher.read_companion(path_str, &mut offset);
        assert_eq!(offset, 17);
        assert_eq!(
            watcher.snapshot().foreground.as_deref(),
            Some("com.example.app")
        );
        assert_eq!(watcher.drain().len(), 1);
        watcher.read_companion(path_str, &mut offset);
        assert!(watcher.drain().is_empty());
    }

    #[test]
    fn ignores_noise_and_malformed_lines() {
        assert!(parse_logcat_line("", 0).is_none());
        assert!(parse_logcat_line("no colon here", 0).is_none());
        assert!(parse_logcat_line("x I wm_on_resume_called: [0,,,]", 0).is_none());
        assert!(parse_logcat_line("x I some_other_tag: [0,com.foo.bar]", 0).is_none());
    }

    #[test]
    fn companion_lines_both_formats() {
        let a = parse_companion_line("com.tencent.mm", 7).unwrap();
        assert_eq!(a.package, "com.tencent.mm");
        let b =
            parse_companion_line(r#"{"package":"com.x.y","kind":"launch","ts":99}"#, 7).unwrap();
        assert_eq!(b.package, "com.x.y");
        assert_eq!(b.ts_ms, 99);
        assert_eq!(b.kind, AppEventKind::Launch);
        assert!(parse_companion_line("# comment", 7).is_none());
        assert!(parse_companion_line(r#"{"package":"bad/package","ts":7}"#, 7).is_none());
        assert!(
            parse_companion_line(r#"{"package":"com.x.y","ts":999999999999999999}"#, 7).is_none()
        );
        assert!(parse_companion_line(r#"{"kind":"screen_off"}"#, 7).is_some());
    }

    #[test]
    fn screen_reads_backlight() {
        let dir = std::env::temp_dir().join("neurosched-screen-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("sys/class/backlight/panel0-backlight");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("brightness"), "0").unwrap();
        assert_eq!(read_screen(&SysRoot::at(&dir)), ScreenState::Off);
        std::fs::write(p.join("brightness"), "2047").unwrap();
        assert_eq!(read_screen(&SysRoot::at(&dir)), ScreenState::On);
    }
}
