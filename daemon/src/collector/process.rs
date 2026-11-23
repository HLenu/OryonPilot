//! Process level probes: kswapd0 CPU share, MMD presence, foreground fallback.
//!
//! All of it is plain file reads so the 1 Hz loop never spawns a shell.

use crate::sys::SysRoot;
use crate::types::MmdInfo;

/// Jiffies per second. `/proc/stat` and `/proc/<pid>/stat` always use USER_HZ=100 on
/// Android, independent of CONFIG_HZ.
const USER_HZ: f64 = 100.0;

pub struct ProcCollector {
    kswapd_pid: Option<u32>,
    prev_ticks: Option<(f64, u64)>,
    kswapd_pct: f64,
    rescan_at: u64,
}

impl Default for ProcCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcCollector {
    pub fn new() -> Self {
        Self {
            kswapd_pid: None,
            prev_ticks: None,
            kswapd_pct: 0.0,
            rescan_at: 0,
        }
    }

    pub fn kswapd_pid(&self) -> Option<u32> {
        self.kswapd_pid
    }

    /// `kswapd0` share of one core, in percent.
    pub fn kswapd_cpu_pct(&mut self, root: &SysRoot, now: u64) -> f64 {
        if self.kswapd_pid.is_none() || now >= self.rescan_at {
            self.kswapd_pid = find_pid_by_comm(root, "kswapd0");
            self.rescan_at = now + 60_000;
        }
        let Some(pid) = self.kswapd_pid else {
            self.kswapd_pct = 0.0;
            return 0.0;
        };
        let Some(ticks) = read_cpu_ticks(root, pid) else {
            self.kswapd_pid = None;
            return self.kswapd_pct;
        };
        if let Some((prev, prev_ts)) = self.prev_ticks {
            let dt = (now.saturating_sub(prev_ts)) as f64 / 1000.0;
            if dt > 0.2 {
                self.kswapd_pct =
                    ((ticks - prev).max(0.0) / USER_HZ / dt * 100.0).clamp(0.0, 100.0);
            }
        }
        self.prev_ticks = Some((ticks, now));
        self.kswapd_pct
    }

    pub fn count_processes(root: &SysRoot) -> usize {
        root.list("/proc")
            .iter()
            .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
            .count()
    }
}

pub fn find_pid_by_comm(root: &SysRoot, comm: &str) -> Option<u32> {
    for name in root.list("/proc") {
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if root
            .read_trim(&format!("/proc/{pid}/comm"))
            .map(|c| c == comm)
            .unwrap_or(false)
        {
            return Some(pid);
        }
    }
    None
}

/// utime+stime from `/proc/<pid>/stat`. The `comm` field may contain spaces and
/// parentheses, so parsing restarts after the *last* `)`.
fn read_cpu_ticks(root: &SysRoot, pid: u32) -> Option<f64> {
    let text = root.read_opt(&format!("/proc/{pid}/stat"))?;
    let close = text.rfind(')')?;
    let rest = text.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After comm: state(0) ppid(1) ... utime is field 14 overall => index 11 here.
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Detects MMD (Android's Memory Management Daemon) without taking over its job.
/// `init.svc.mmd` needs `getprop`, so the boot probe fills that part in.
pub fn detect_mmd(root: &SysRoot, init_state: Option<String>) -> MmdInfo {
    let pid = find_pid_by_comm(root, "mmd").or_else(|| {
        // The first NUL-delimited field is argv[0]. Later arguments may mention
        // mmd (e.g. a shell running diagnostics) without being that service.
        root.list("/proc").into_iter().find_map(|name| {
            let pid = name.parse::<u32>().ok()?;
            let cmd = root.read_opt(&format!("/proc/{pid}/cmdline"))?;
            let executable = cmd.split('\0').next()?;
            if executable.rsplit('/').next() == Some("mmd") {
                Some(pid)
            } else {
                None
            }
        })
    });
    let binary = root.exists("/system/bin/mmd") || root.exists("/system_ext/bin/mmd");
    let service_state = init_state.clone();
    let running = service_state.as_deref() == Some("running");
    MmdInfo {
        present: pid.is_some() || running || binary,
        pid,
        service_state,
        init_service: Some("init.svc.mmd".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_kswapd_ticks() {
        let dir = std::env::temp_dir().join("neurosched-proc-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("proc/1234");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("comm"), "kswapd0").unwrap();
        std::fs::write(
            p.join("stat"),
            "1234 (kswapd0) S 2 0 0 0 -1 69238880 0 0 0 0 150 50 0 0 20 0 1 0 100 0 0\n",
        )
        .unwrap();
        let root = SysRoot::at(&dir);
        assert_eq!(find_pid_by_comm(&root, "kswapd0"), Some(1234));
        let mut c = ProcCollector::new();
        let first = c.kswapd_cpu_pct(&root, 1_000);
        assert_eq!(first, 0.0);
        // +50 ticks over 1s == 50% of one core
        std::fs::write(
            p.join("stat"),
            "1234 (kswapd0) S 2 0 0 0 -1 69238880 0 0 0 0 200 50 0 0 20 0 1 0 100 0 0\n",
        )
        .unwrap();
        let second = c.kswapd_cpu_pct(&root, 2_000);
        assert!((second - 50.0).abs() < 0.01, "{second}");
    }

    #[test]
    fn comm_with_spaces_parses() {
        let dir = std::env::temp_dir().join("neurosched-proc-space");
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("proc/99");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(
            p.join("stat"),
            "99 (weird name) S 1 1 1 0 -1 0 0 0 0 0 10 20 0 0 20 0 1 0 5 0 0\n",
        )
        .unwrap();
        let root = SysRoot::at(&dir);
        assert_eq!(read_cpu_ticks(&root, 99), Some(30.0));
    }

    #[test]
    fn mmd_cmdline_matches_only_the_executable_basename() {
        let dir = std::env::temp_dir().join("neurosched-mmd-cmdline");
        let _ = std::fs::remove_dir_all(&dir);
        let process = dir.join("proc/321");
        std::fs::create_dir_all(&process).unwrap();
        let root = SysRoot::at(&dir);
        for command in [
            "/system/bin/sh\0-c\0pidof mmd\0",
            "/system/bin/dumpsys\0mmd\0",
            "/vendor/bin/mmd_helper\0",
            "com.example.mmdapp\0",
            "/vendor/bin/vendor.xiaomi.hardware.mimd2-service\0",
            "\0/system/bin/mmd\0",
            "/system/bin/mmd/\0",
        ] {
            std::fs::write(process.join("cmdline"), command).unwrap();
            let detected = detect_mmd(&root, None);
            assert!(!detected.present, "false positive for {command:?}");
            assert_eq!(detected.pid, None);
        }
        for command in [
            "mmd\0",
            "/system/bin/mmd\0--flag\0",
            "/system_ext/bin/mmd\0",
        ] {
            std::fs::write(process.join("cmdline"), command).unwrap();
            let detected = detect_mmd(&root, None);
            assert!(detected.present, "missed {command:?}");
            assert_eq!(detected.pid, Some(321));
        }
    }

    #[test]
    fn mmd_binary_presence_does_not_claim_a_running_process() {
        let dir = std::env::temp_dir().join("neurosched-mmd-binary-only");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("system/bin")).unwrap();
        std::fs::write(dir.join("system/bin/mmd"), "").unwrap();
        let info = detect_mmd(&SysRoot::at(&dir), Some("stopped".into()));
        assert!(info.present);
        assert_eq!(info.pid, None);
        assert_eq!(info.service_state.as_deref(), Some("stopped"));
    }

    #[test]
    fn mmd_detection_by_binary_and_service() {
        let dir = std::env::temp_dir().join("neurosched-mmd-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("system/bin")).unwrap();
        std::fs::write(dir.join("system/bin/mmd"), "").unwrap();
        let root = SysRoot::at(&dir);
        let info = detect_mmd(&root, Some("running".into()));
        assert!(info.present);
        assert_eq!(info.service_state.as_deref(), Some("running"));

        let none = detect_mmd(
            &SysRoot::at(std::env::temp_dir().join("neurosched-mmd-none")),
            None,
        );
        assert!(!none.present);
    }
}
