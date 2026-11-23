//! CPU topology and load collection.
//!
//! Hard rule: cluster identity is derived from `related_cpus` / measured capacity /
//! `cpuinfo_max_freq`, never from the CPU number. On Snapdragon 8 Elite the two
//! policies are `policy0` (6x performance) and `policy6` (2x prime); a device with a
//! different layout must still be described correctly.

use crate::sys::{parse_cpu_list, SysRoot};
use crate::types::{ClusterInfo, CoreLoad, CpuSnapshot};
use std::collections::{BTreeMap, BTreeSet};

const CPU_ROOT: &str = "/sys/devices/system/cpu";

#[derive(Debug, Default, Clone)]
struct Times {
    busy: u64,
    total: u64,
}

pub struct CpuCollector {
    prev_total: Option<Times>,
    prev_cores: BTreeMap<u32, Times>,
    prev_ts: u64,
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuCollector {
    pub fn new() -> Self {
        Self {
            prev_total: None,
            prev_cores: BTreeMap::new(),
            prev_ts: 0,
        }
    }

    pub fn collect(&mut self, root: &SysRoot, now: u64) -> CpuSnapshot {
        let mut snap = CpuSnapshot::default();

        snap.present = root
            .read_trim(&format!("{CPU_ROOT}/present"))
            .map(|s| parse_cpu_list(&s))
            .unwrap_or_default();
        snap.online = root
            .read_trim(&format!("{CPU_ROOT}/online"))
            .map(|s| parse_cpu_list(&s))
            .unwrap_or_else(|| {
                // Some kernels omit `online`; fall back to per-cpu `online` files.
                let mut v = Vec::new();
                for cpu in &snap.present {
                    if root
                        .read_trim(&format!("{CPU_ROOT}/cpu{cpu}/online"))
                        .map(|s| s.trim() == "1")
                        .unwrap_or(true)
                    {
                        v.push(*cpu);
                    }
                }
                v
            });

        if let Some(text) = root.read_opt("/proc/stat") {
            let (total, cores) = parse_proc_stat(&text);
            let dt = self.prev_total.as_ref().map(|p| {
                (
                    total.busy.saturating_sub(p.busy),
                    total.total.saturating_sub(p.total),
                )
            });
            snap.total_pct = match dt {
                Some((db, dtt)) if dtt > 0 => (db as f64 / dtt as f64 * 100.0).clamp(0.0, 100.0),
                _ => 0.0,
            };

            for (id, t) in &cores {
                let pct = self
                    .prev_cores
                    .get(id)
                    .map(|p| {
                        let db = t.busy.saturating_sub(p.busy);
                        let dtt = t.total.saturating_sub(p.total);
                        if dtt > 0 {
                            (db as f64 / dtt as f64 * 100.0).clamp(0.0, 100.0)
                        } else {
                            0.0
                        }
                    })
                    .unwrap_or(0.0);
                snap.cores.push(CoreLoad {
                    id: *id,
                    usage_pct: pct,
                    freq_khz: None,
                });
            }
            self.prev_total = Some(total);
            self.prev_cores = cores;
            self.prev_ts = now;
        }

        snap.clusters = read_clusters(root, &snap.cores);

        // Attach per-core frequency (best effort; some kernels only expose policy level).
        for core in snap.cores.iter_mut() {
            core.freq_khz = root
                .read_i64(&format!(
                    "{CPU_ROOT}/cpu{}/cpufreq/scaling_cur_freq",
                    core.id
                ))
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v > 0)
                .or_else(|| {
                    snap.clusters
                        .iter()
                        .find(|c| c.cpus.contains(&core.id))
                        .map(|c| c.cur_khz)
                });
        }

        snap.walt_present = root.exists("/proc/sys/kernel/sched_walt_rotate_window")
            || root.exists("/sys/module/walt")
            || root.exists("/proc/sys/kernel/sched_walt_low_latency")
            // Built-in vendor WALT may expose none of the optional knob paths.
            // Xiaomi 15 reports the active governor explicitly as `walt`.
            || snap.clusters.iter().any(|cluster| cluster.governor == "walt");
        snap.uclamp_supported = detect_uclamp(root).is_some();
        snap.knobs = probe_knobs(root);
        snap
    }
}

fn parse_proc_stat(text: &str) -> (Times, BTreeMap<u32, Times>) {
    let mut total = Times::default();
    let mut cores = BTreeMap::new();
    for line in text.lines() {
        if !line.starts_with("cpu") {
            continue;
        }
        let mut it = line.split_whitespace();
        let label = it.next().unwrap_or("");
        let nums: Vec<u64> = it.filter_map(|v| v.parse().ok()).collect();
        if nums.len() < 5 {
            continue;
        }
        let user = nums[0];
        let nice = nums[1];
        let system = nums[2];
        let idle = nums[3];
        let iowait = nums[4];
        let irq = nums.get(5).copied().unwrap_or(0);
        let softirq = nums.get(6).copied().unwrap_or(0);
        let steal = nums.get(7).copied().unwrap_or(0);
        let busy = user + nice + system + irq + softirq + steal;
        let t = Times {
            busy,
            total: busy + idle + iowait,
        };
        if label == "cpu" {
            total = t;
        } else if let Ok(id) = label[3..].parse::<u32>() {
            cores.insert(id, t);
        }
    }
    (total, cores)
}

/// Reads cpufreq policies and turns them into capability-ordered clusters.
fn read_clusters(root: &SysRoot, cores: &[CoreLoad]) -> Vec<ClusterInfo> {
    let mut policies: Vec<String> = root
        .list(&format!("{CPU_ROOT}/cpufreq"))
        .into_iter()
        .filter(|n| n.starts_with("policy"))
        .map(|n| format!("{CPU_ROOT}/cpufreq/{n}"))
        .collect();

    if policies.is_empty() {
        // Fallback: per-CPU cpufreq directories.
        let mut seen = BTreeMap::new();
        for name in root.list(CPU_ROOT) {
            if let Some(id) = name.strip_prefix("cpu").and_then(|s| s.parse::<u32>().ok()) {
                let path = format!("{CPU_ROOT}/cpu{id}/cpufreq");
                if root.is_dir(&path) {
                    seen.insert(id, path);
                }
            }
        }
        policies = seen.into_values().collect();
    }

    let mut clusters: Vec<ClusterInfo> = Vec::new();
    let mut seen_cpus = BTreeSet::new();
    for path in policies {
        let cpus = root
            .read_trim(&format!("{path}/related_cpus"))
            .or_else(|| root.read_trim(&format!("{path}/affected_cpus")))
            .map(|s| parse_cpu_list(&s))
            .unwrap_or_default();
        if cpus.is_empty() || !seen_cpus.insert(cpus.clone()) {
            continue;
        }
        let cur = root
            .read_i64(&format!("{path}/scaling_cur_freq"))
            .unwrap_or(0)
            .max(0) as u32;
        let max = root
            .read_i64(&format!("{path}/scaling_max_freq"))
            .unwrap_or(0)
            .max(0) as u32;
        let info_max = root
            .read_i64(&format!("{path}/cpuinfo_max_freq"))
            .unwrap_or(0)
            .max(0) as u32;
        let min = root
            .read_i64(&format!("{path}/scaling_min_freq"))
            .unwrap_or(0)
            .max(0) as u32;
        let governor = root
            .read_trim(&format!("{path}/scaling_governor"))
            .unwrap_or_else(|| "unknown".into());
        let available = root
            .read_trim(&format!("{path}/scaling_available_governors"))
            .map(|s| s.split_whitespace().map(|g| g.to_string()).collect())
            .unwrap_or_default();
        let capacity = cpus
            .first()
            .and_then(|c| root.read_i64(&format!("{CPU_ROOT}/cpu{c}/cpu_capacity")))
            .and_then(|v| u32::try_from(v).ok());

        let load_pct = {
            let vals: Vec<f64> = cores
                .iter()
                .filter(|c| cpus.contains(&c.id))
                .map(|c| c.usage_pct)
                .collect();
            if vals.is_empty() {
                0.0
            } else {
                vals.iter().sum::<f64>() / vals.len() as f64
            }
        };

        clusters.push(ClusterInfo {
            name: String::new(),
            cpus,
            governor,
            available_governors: available,
            cur_khz: cur,
            min_khz: min,
            max_khz: if max > 0 { max } else { info_max },
            cpuinfo_max_khz: info_max,
            capacity,
            load_pct,
            policy_path: path,
        });
    }

    // Rank by silicon capability (cpuinfo_max_freq, then capacity), not by path order.
    clusters.sort_by(|a, b| {
        b.cpuinfo_max_khz
            .cmp(&a.cpuinfo_max_khz)
            .then(b.capacity.unwrap_or(0).cmp(&a.capacity.unwrap_or(0)))
            .then(b.max_khz.cmp(&a.max_khz))
    });
    let labels = ["prime", "gold", "silver", "bronze"];
    for (i, c) in clusters.iter_mut().enumerate() {
        c.name = labels
            .get(i)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("cluster{i}"));
    }
    clusters
}

/// Returns the first writable-looking uclamp control path, if the kernel exposes one.
/// Existence only: probing by writing is forbidden.
pub fn detect_uclamp(root: &SysRoot) -> Option<String> {
    const CANDIDATES: [&str; 4] = [
        "/sys/fs/cgroup/top-app/cpu.uclamp.min",
        "/dev/cpuctl/top-app/cpu.uclamp.min",
        "/sys/fs/cgroup/cpu/top-app/cpu.uclamp.min",
        "/sys/fs/cgroup/system/top-app/cpu.uclamp.min",
    ];
    CANDIDATES
        .iter()
        .find(|p| root.exists(p))
        .map(|p| p.to_string())
}

fn probe_knobs(root: &SysRoot) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let probes = [
        (
            "sched_util_clamp_min",
            "/proc/sys/kernel/sched_util_clamp_min",
        ),
        (
            "sched_util_clamp_max",
            "/proc/sys/kernel/sched_util_clamp_max",
        ),
        (
            "uclamp_top_app_min",
            "/sys/fs/cgroup/top-app/cpu.uclamp.min",
        ),
        (
            "uclamp_top_app_max",
            "/sys/fs/cgroup/top-app/cpu.uclamp.max",
        ),
        (
            "uclamp_v1_top_app_min",
            "/dev/cpuctl/top-app/cpu.uclamp.min",
        ),
        ("cgroup_mode", "/sys/fs/cgroup/cgroup.controllers"),
        ("schedtune_boost", "/dev/stune/top-app/schedtune.boost"),
        (
            "sched_autogroup",
            "/proc/sys/kernel/sched_autogroup_enabled",
        ),
        ("sched_walt", "/proc/sys/kernel/sched_walt_rotate_window"),
        ("cpu_boost", "/sys/devices/system/cpu/cpu_boost/enabled"),
    ];
    for (name, path) in probes {
        if let Some(v) = root.read_trim(path) {
            m.insert(
                name.to_string(),
                format!("{path} = {}", v.lines().next().unwrap_or("")),
            );
        } else if root.exists(path) {
            m.insert(name.to_string(), format!("{path} (exists, unreadable)"));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat() {
        let text = "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 50 0 50 400 0 0 0 0 0 0\ncpu1 50 0 50 400 0 0 0 0 0 0\nintr 1\n";
        let (total, cores) = parse_proc_stat(text);
        assert_eq!(total.busy, 200);
        assert_eq!(total.total, 1000);
        assert_eq!(cores.len(), 2);
        assert_eq!(cores[&0].busy, 100);
    }

    #[test]
    fn walt_is_detected_from_active_governor_without_optional_knobs() {
        let dir = std::env::temp_dir().join("neurosched-cpu-walt-governor");
        let _ = std::fs::remove_dir_all(&dir);
        let policy = dir.join("sys/devices/system/cpu/cpufreq/policy0");
        std::fs::create_dir_all(&policy).unwrap();
        std::fs::write(policy.join("related_cpus"), "0 1 2 3 4 5\n").unwrap();
        std::fs::write(policy.join("scaling_available_governors"), "walt schedutil").unwrap();
        let root = SysRoot::at(&dir);
        let mut collector = CpuCollector::new();
        for (governor, expected) in [("walt\n", true), ("schedutil", false), ("not_walt", false)] {
            std::fs::write(policy.join("scaling_governor"), governor).unwrap();
            assert_eq!(collector.collect(&root, 1_000).walt_present, expected);
        }
        // Keep support for a WALT knob even when the governor probe is unavailable.
        std::fs::remove_file(policy.join("scaling_governor")).unwrap();
        std::fs::create_dir_all(dir.join("sys/module/walt")).unwrap();
        assert!(collector.collect(&root, 2_000).walt_present);
    }

    #[test]
    fn cluster_labels_follow_capability_not_index() {
        // Two policies whose directory order is deliberately "wrong".
        let dir = std::env::temp_dir().join("neurosched-cpu-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        for (pol, cpus, freq, _cap) in [
            ("policy0", "0 1 2 3 4 5", 1_900_000, 700),
            ("policy6", "6 7", 3_500_000, 1024),
        ] {
            let p = dir.join(format!("sys/devices/system/cpu/cpufreq/{pol}"));
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("related_cpus"), cpus).unwrap();
            std::fs::write(p.join("scaling_cur_freq"), freq.to_string()).unwrap();
            std::fs::write(p.join("cpuinfo_max_freq"), freq.to_string()).unwrap();
            std::fs::write(p.join("scaling_governor"), "schedutil").unwrap();
        }
        for (cpu, cap) in [(0, 700), (6, 1024)] {
            let p = dir.join(format!("sys/devices/system/cpu/cpu{cpu}"));
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("cpu_capacity"), cap.to_string()).unwrap();
        }
        std::fs::write(dir.join("sys/devices/system/cpu/present"), "0-7").unwrap();

        let root = SysRoot::at(&dir);
        let clusters = read_clusters(&root, &[]);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].name, "prime");
        assert_eq!(clusters[0].cpus, vec![6, 7]);
        assert_eq!(clusters[1].name, "gold");
        assert_eq!(clusters[1].cpus, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn per_cpu_fallback_deduplicates_shared_policies() {
        let dir = std::env::temp_dir().join("neurosched-cpu-shared-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        for cpu in 0..4 {
            let p = dir.join(format!("sys/devices/system/cpu/cpu{cpu}/cpufreq"));
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("related_cpus"), "0 1 2 3\n").unwrap();
            std::fs::write(p.join("cpuinfo_max_freq"), "2000000").unwrap();
        }
        let clusters = read_clusters(&SysRoot::at(&dir), &[]);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].cpus, vec![0, 1, 2, 3]);
    }
}
