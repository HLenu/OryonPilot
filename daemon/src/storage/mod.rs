//! Data directory layout, log ring, telemetry CSV and history ring.

use crate::json::Json;
use crate::types::Snapshot;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    config_path: Option<PathBuf>,
}

impl Paths {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: data_dir.into(),
            config_path: None,
        }
    }

    pub fn with_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn telemetry(&self) -> PathBuf {
        self.root.join("telemetry")
    }
    pub fn journal(&self) -> PathBuf {
        self.root.join("journal")
    }
    pub fn backups(&self) -> PathBuf {
        self.root.join("backups")
    }
    pub fn config(&self) -> PathBuf {
        self.config_path
            .clone()
            .unwrap_or_else(|| self.root.join("config.json"))
    }
    pub fn sched_model(&self) -> PathBuf {
        self.root.join("sched_model.json")
    }
    pub fn learner_state(&self) -> PathBuf {
        self.root.join("learner.json")
    }
    pub fn boot_state(&self) -> PathBuf {
        self.root.join("boot.json")
    }
    pub fn token(&self) -> PathBuf {
        self.root.join("token")
    }
    pub fn companion_log(&self) -> PathBuf {
        self.root.join("fg_events.log")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        for d in [
            self.root.clone(),
            self.logs(),
            self.telemetry(),
            self.journal(),
            self.backups(),
        ] {
            std::fs::create_dir_all(d)?;
        }
        restrict_permissions(&self.root);
        Ok(())
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// Creates the API token once, 32 hex chars, never logged.
pub fn ensure_token(path: &Path) -> std::io::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let t = existing.trim().to_string();
        if t.len() >= 16 && t.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(t);
        }
    }
    let token = random_hex(32)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "{token}")?;
    f.sync_all()?;
    secure_file(path);
    Ok(token)
}

/// Hex string from the OS CSPRNG (`getrandom`), used for the API token.
///
/// Returns an error instead of silently falling back to a predictable seed: a weak
/// token on a loopback API is still a weak token.
pub fn random_hex(len: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; len.div_ceil(2)];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(format!("getrandom: {e}")))?;
    let mut out = String::with_capacity(len);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    out.truncate(len);
    Ok(out)
}

/// Restricts a file to its owner. No-op on non-unix hosts (tests run on Windows).
#[cfg(unix)]
pub fn secure_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub fn secure_file(_path: &Path) {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
    pub fn parse(s: &str) -> Level {
        match s.trim().to_ascii_lowercase().as_str() {
            "debug" => Level::Debug,
            "warn" | "warning" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }
    fn rank(self) -> u8 {
        match self {
            Level::Debug => 0,
            Level::Info => 1,
            Level::Warn => 2,
            Level::Error => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts_ms: u64,
    pub level: Level,
    pub target: String,
    pub message: String,
}

impl LogEntry {
    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("ts", Json::num(self.ts_ms as f64));
        j.set("iso", Json::str(crate::util::iso_utc(self.ts_ms)));
        j.set("level", Json::str(self.level.as_str()));
        j.set("target", Json::str(self.target.clone()));
        j.set("message", Json::str(self.message.clone()));
        j
    }
}

/// In-memory ring plus an optional rotating file.
pub struct Logger {
    ring: Mutex<VecDeque<LogEntry>>,
    cap: usize,
    min_level: Level,
    file: Mutex<Option<std::fs::File>>,
    path: PathBuf,
    rotate_bytes: u64,
    keep_files: AtomicUsize,
}

impl Logger {
    pub fn new(cap: usize, min_level: Level, path: PathBuf, rotate_kb: u64) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        Self {
            ring: Mutex::new(VecDeque::with_capacity(cap.min(4096))),
            cap: cap.max(1),
            min_level,
            file: Mutex::new(file),
            path,
            rotate_bytes: rotate_kb.max(64) * 1024,
            keep_files: AtomicUsize::new(5),
        }
    }

    pub fn set_keep_files(&self, keep: usize) {
        self.keep_files.store(keep.max(1), Ordering::Relaxed);
        self.prune_rotated();
    }

    pub fn log(&self, level: Level, target: &str, message: impl Into<String>) {
        if level.rank() < self.min_level.rank() {
            return;
        }
        let entry = LogEntry {
            ts_ms: crate::util::now_ms(),
            level,
            target: target.to_string(),
            message: message.into(),
        };
        if let Ok(mut ring) = self.ring.lock() {
            ring.push_back(entry.clone());
            while ring.len() > self.cap {
                ring.pop_front();
            }
        }
        // The guard must be dropped before rotating: `maybe_rotate` takes the same
        // mutex, and std::sync::Mutex is not re-entrant.
        {
            if let Ok(mut guard) = self.file.lock() {
                if let Some(f) = guard.as_mut() {
                    let _ = writeln!(
                        f,
                        "{} {} [{}] {}",
                        crate::util::iso_utc(entry.ts_ms),
                        entry.level.as_str(),
                        entry.target,
                        entry.message
                    );
                    let _ = f.flush();
                }
            }
        }
        self.maybe_rotate();
    }

    fn maybe_rotate(&self) {
        let Ok(mut guard) = self.file.lock() else {
            return;
        };
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return;
        };
        if meta.len() < self.rotate_bytes {
            return;
        }
        let mut stamp = crate::util::now_ms();
        let mut rotated = self.path.with_extension(format!("{stamp}.log"));
        while rotated.exists() {
            stamp = stamp.saturating_add(1);
            rotated = self.path.with_extension(format!("{stamp}.log"));
        }
        // Windows cannot rename an open log. Closing under the same mutex also
        // prevents a concurrent writer from continuing to append to the old file.
        guard.take();
        let rotated_ok = std::fs::rename(&self.path, &rotated).is_ok();
        *guard = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        drop(guard);
        if rotated_ok {
            self.prune_rotated();
        }
    }

    fn prune_rotated(&self) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        let Some(stem) = self.path.file_stem().and_then(|name| name.to_str()) else {
            return;
        };
        let prefix = format!("{stem}.");
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_prefix(&prefix))
                    .and_then(|name| name.strip_suffix(".log"))
                    .is_some_and(|stamp| {
                        !stamp.is_empty() && stamp.bytes().all(|c| c.is_ascii_digit())
                    })
                    && path.is_file()
            })
            .collect();
        files.sort();
        let remove_count = files
            .len()
            .saturating_sub(self.keep_files.load(Ordering::Relaxed));
        for path in files.into_iter().take(remove_count) {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn recent(&self, limit: usize) -> Vec<LogEntry> {
        self.ring
            .lock()
            .map(|r| r.iter().rev().take(limit).rev().cloned().collect())
            .unwrap_or_default()
    }

    pub fn info(&self, target: &str, msg: impl Into<String>) {
        self.log(Level::Info, target, msg);
    }
    pub fn warn(&self, target: &str, msg: impl Into<String>) {
        self.log(Level::Warn, target, msg);
    }
    pub fn error(&self, target: &str, msg: impl Into<String>) {
        self.log(Level::Error, target, msg);
    }
    pub fn debug(&self, target: &str, msg: impl Into<String>) {
        self.log(Level::Debug, target, msg);
    }
}

#[derive(Debug, Clone, Default)]
pub struct HistoryPoint {
    pub ts_ms: u64,
    pub state: String,
    pub cpu_pct: f64,
    pub prime_mhz: f64,
    pub mem_avail_mb: f64,
    pub swap_used_mb: f64,
    pub zram_ratio: f64,
    pub psi_some10: f64,
    pub psi_full10: f64,
    pub kswapd_pct: f64,
    pub cpu_temp_c: f64,
    pub battery_temp_c: f64,
    pub battery_pct: f64,
    pub foreground: String,
}

impl HistoryPoint {
    pub fn from_snapshot(s: &Snapshot, state: &str) -> Self {
        let prime = s
            .cpu
            .clusters
            .iter()
            .max_by_key(|c| c.cpuinfo_max_khz)
            .map(|c| c.cur_khz as f64 / 1000.0)
            .unwrap_or(0.0);
        let zram_ratio = s.mem.zram.first().map(|z| z.ratio).unwrap_or(0.0);
        HistoryPoint {
            ts_ms: s.wall_ts_ms,
            state: state.to_string(),
            cpu_pct: s.cpu.total_pct,
            prime_mhz: prime,
            mem_avail_mb: s.mem.available_kb / 1024.0,
            swap_used_mb: s.mem.swap_used_kb() / 1024.0,
            zram_ratio,
            psi_some10: s.psi.memory.some10,
            psi_full10: s.psi.memory.full10,
            kswapd_pct: s.mem.kswapd_cpu_pct,
            cpu_temp_c: s.thermal.cpu_max_c.unwrap_or(0.0),
            battery_temp_c: s.battery.temp_c.unwrap_or(0.0),
            battery_pct: s.battery.capacity_pct.unwrap_or(0.0),
            foreground: s.apps.foreground.clone().unwrap_or_default(),
        }
    }

    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("ts", Json::num(self.ts_ms as f64));
        j.set("state", Json::str(self.state.clone()));
        j.set("cpu", Json::num(self.cpu_pct));
        j.set("prime_mhz", Json::num(self.prime_mhz));
        j.set("mem_avail_mb", Json::num(self.mem_avail_mb));
        j.set("swap_used_mb", Json::num(self.swap_used_mb));
        j.set("zram_ratio", Json::num(self.zram_ratio));
        j.set("psi_some10", Json::num(self.psi_some10));
        j.set("psi_full10", Json::num(self.psi_full10));
        j.set("kswapd", Json::num(self.kswapd_pct));
        j.set("cpu_temp", Json::num(self.cpu_temp_c));
        j.set("batt_temp", Json::num(self.battery_temp_c));
        j.set("batt_pct", Json::num(self.battery_pct));
        j.set("fg", Json::str(self.foreground.clone()));
        j
    }

    pub fn csv_header() -> &'static str {
        "ts_iso,state,cpu_pct,prime_mhz,mem_avail_mb,swap_used_mb,zram_ratio,psi_some10,psi_full10,kswapd_pct,cpu_temp_c,batt_temp_c,batt_pct,foreground"
    }

    pub fn to_csv(&self) -> String {
        format!(
            "{},{},{:.1},{:.0},{:.0},{:.0},{:.2},{:.2},{:.2},{:.1},{:.1},{:.1},{:.0},{}",
            crate::util::iso_utc(self.ts_ms),
            self.state,
            self.cpu_pct,
            self.prime_mhz,
            self.mem_avail_mb,
            self.swap_used_mb,
            self.zram_ratio,
            self.psi_some10,
            self.psi_full10,
            self.kswapd_pct,
            self.cpu_temp_c,
            self.battery_temp_c,
            self.battery_pct,
            if self.foreground.is_empty() {
                "-"
            } else {
                &self.foreground
            }
        )
    }
}

pub struct Telemetry {
    path: PathBuf,
    rotate_bytes: u64,
    keep: usize,
    enabled: bool,
}

impl Telemetry {
    pub fn new(dir: &Path, rotate_kb: u64, keep: usize, enabled: bool) -> Self {
        let _ = std::fs::create_dir_all(dir);
        Self {
            path: dir.join("history.csv"),
            rotate_bytes: rotate_kb.max(64) * 1024,
            keep: keep.max(1),
            enabled,
        }
    }

    pub fn append(&self, p: &HistoryPoint) {
        if !self.enabled {
            return;
        }
        let needs_header = !self.path.exists();
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        if needs_header {
            let _ = writeln!(f, "{}", HistoryPoint::csv_header());
        }
        let _ = writeln!(f, "{}", p.to_csv());
        let _ = f.flush();
        drop(f);
        self.maybe_rotate();
    }

    fn maybe_rotate(&self) {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return;
        };
        if meta.len() < self.rotate_bytes {
            return;
        }
        let stamp = crate::util::now_ms();
        let rotated = self.path.with_file_name(format!("history-{stamp}.csv"));
        if std::fs::rename(&self.path, rotated).is_err() {
            return;
        }
        let mut files: Vec<PathBuf> =
            std::fs::read_dir(self.path.parent().unwrap_or(Path::new(".")))
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.starts_with("history-") && n.ends_with(".csv"))
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
        files.sort();
        while files.len() > self.keep {
            if let Some(old) = files.first().cloned() {
                let _ = std::fs::remove_file(old);
                files.remove(0);
            } else {
                break;
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_created_once_and_stable() {
        let dir = std::env::temp_dir().join("neurosched-token-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("token");
        let a = ensure_token(&p).expect("token");
        let b = ensure_token(&p).expect("token");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn log_ring_keeps_newest() {
        let dir = std::env::temp_dir().join("neurosched-log-test");
        let _ = std::fs::remove_dir_all(&dir);
        let l = Logger::new(5, Level::Info, dir.join("x.log"), 64);
        for i in 0..10 {
            l.info("t", format!("m{i}"));
        }
        let recent = l.recent(100);
        assert_eq!(recent.len(), 5);
        assert_eq!(recent.last().unwrap().message, "m9");
    }

    #[test]
    fn debug_is_filtered_by_level() {
        let dir = std::env::temp_dir().join("neurosched-log-level");
        let _ = std::fs::remove_dir_all(&dir);
        let l = Logger::new(10, Level::Warn, dir.join("x.log"), 64);
        l.debug("t", "hidden");
        l.info("t", "hidden");
        l.warn("t", "shown");
        assert_eq!(l.recent(10).len(), 1);
    }

    #[test]
    fn csv_has_header_and_escapes_empty_fg() {
        let dir = std::env::temp_dir().join("neurosched-csv-test");
        let _ = std::fs::remove_dir_all(&dir);
        let t = Telemetry::new(&dir, 64, 2, true);
        let p = HistoryPoint {
            ts_ms: 0,
            state: "BALANCED".into(),
            ..Default::default()
        };
        t.append(&p);
        t.append(&p);
        let text = std::fs::read_to_string(t.path()).unwrap();
        assert!(text.starts_with("ts_iso,"));
        assert_eq!(text.lines().count(), 3);
        assert!(text.contains(",BALANCED,"));
    }

    #[test]
    fn random_hex_length_and_charset() {
        let s = random_hex(32).expect("entropy");
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(s, random_hex(32).expect("entropy"));
    }

    #[test]
    fn history_uses_wall_clock_instead_of_monotonic_time() {
        let snapshot = Snapshot {
            ts_ms: 120_000,
            wall_ts_ms: 1_783_612_800_000,
            ..Default::default()
        };
        let point = HistoryPoint::from_snapshot(&snapshot, "BALANCED");
        assert_eq!(point.ts_ms, snapshot.wall_ts_ms);
        assert!(!point.to_csv().starts_with("1970-"));
    }

    #[test]
    fn logger_retention_only_removes_its_timestamped_files() {
        let dir = std::env::temp_dir().join("neurosched-log-retention");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for stamp in 1..=4 {
            std::fs::write(dir.join(format!("x.{stamp}.log")), "old").unwrap();
        }
        for name in ["other.1.log", "x.backup.log"] {
            std::fs::write(dir.join(name), "retain").unwrap();
        }
        let logger = Logger::new(10, Level::Info, dir.join("x.log"), 64);
        logger.set_keep_files(2);
        assert!(!dir.join("x.1.log").exists());
        assert!(!dir.join("x.2.log").exists());
        assert!(dir.join("x.3.log").exists());
        assert!(dir.join("x.4.log").exists());
        assert!(dir.join("other.1.log").exists());
        assert!(dir.join("x.backup.log").exists());
    }

    #[test]
    fn logger_reopens_after_rotation_and_bounds_archives() {
        let dir = std::env::temp_dir().join("neurosched-log-rotation");
        let _ = std::fs::remove_dir_all(&dir);
        let logger = Logger::new(10, Level::Info, dir.join("x.log"), 64);
        logger.set_keep_files(2);
        for _ in 0..4 {
            logger.info("rotation", "x".repeat(70_000));
        }
        logger.info("rotation", "still writable");
        let archives = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name() != "x.log")
            .count();
        assert_eq!(archives, 2);
        assert!(std::fs::read_to_string(dir.join("x.log"))
            .unwrap()
            .contains("still writable"));
    }
}
