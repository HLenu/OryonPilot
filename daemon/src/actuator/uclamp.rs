//! uclamp (utilization clamping) discovery.
//!
//! Android exposes `cpu.uclamp.min` / `cpu.uclamp.max` per cgroup. Writing them caps
//! the *utilisation request* of the tasks in that group, which lets the scheduler pick
//! a lower OPP without touching the frequency table itself.
//!
//! Discovery reads the cgroup percentage ABI and confirms its CPU-controller mount.
//! No write probe is used. Unknown scales remain unsupported.

use crate::sys::SysRoot;

/// Internal profiles use the kernel's 0..=1024 "performance point" scale.
pub const UCLAMP_MAX_VALUE: u32 = 1024;

/// The cgroup interface uses a *percentage* (0..=100) while per-task and sysctl
/// interfaces use 0..=1024. A current value cannot identify an interface's scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UclampScale {
    Percent,
    Per1024,
}

impl UclampScale {
    pub fn as_str(self) -> &'static str {
        match self {
            UclampScale::Percent => "percent_0_100",
            UclampScale::Per1024 => "perf_0_1024",
        }
    }

    pub fn max(self) -> u32 {
        match self {
            UclampScale::Percent => 100,
            UclampScale::Per1024 => UCLAMP_MAX_VALUE,
        }
    }

    /// 0..=1024 profile value -> value accepted by this node.
    pub fn to_node(self, value: u32) -> u32 {
        match self {
            UclampScale::Percent => ((value.min(UCLAMP_MAX_VALUE) * 100 + 512) / 1024).min(100),
            UclampScale::Per1024 => value.min(UCLAMP_MAX_VALUE),
        }
    }

    /// Node value -> 0..=1024 profile value (used when reporting current state).
    pub fn from_node(self, value: u32) -> u32 {
        match self {
            UclampScale::Percent => (value.min(100) * 1024 / 100).min(UCLAMP_MAX_VALUE),
            UclampScale::Per1024 => value.min(UCLAMP_MAX_VALUE),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UclampTarget {
    pub dir: String,
    pub min_path: String,
    pub max_path: String,
    /// `cgroup_v2`, `cgroup_v1` or `dev_cpuctl`.
    pub flavour: String,
    pub current_min: f64,
    pub current_max: f64,
    pub scale: UclampScale,
    pub interface_verified: bool,
}

const CANDIDATES: [(&str, &str); 3] = [
    ("/sys/fs/cgroup/top-app", "cgroup_v2"),
    ("/dev/cpuctl/top-app", "dev_cpuctl"),
    ("/sys/fs/cgroup/cpu/top-app", "cgroup_v1"),
];

pub fn discover(root: &SysRoot) -> Option<UclampTarget> {
    for (dir, flavour) in CANDIDATES {
        let min_path = format!("{dir}/cpu.uclamp.min");
        let max_path = format!("{dir}/cpu.uclamp.max");
        if !root.exists(&min_path) || !root.exists(&max_path) {
            continue;
        }
        let (Some(min_raw), Some(max_raw)) = (root.read_trim(&min_path), root.read_trim(&max_path))
        else {
            continue;
        };
        let (Some(min), Some(max)) = (percentage(&min_raw), percentage(&max_raw)) else {
            continue;
        };
        if min > max {
            continue;
        }
        return Some(UclampTarget {
            dir: dir.to_string(),
            min_path,
            max_path,
            flavour: flavour.to_string(),
            current_min: min,
            current_max: max,
            scale: UclampScale::Percent,
            interface_verified: mount_confirms_cpu_cgroup(root, dir),
        });
    }
    None
}

/// The documented cgroup ABI is a rational percentage, including textual `max`.
/// Values alone cannot identify a vendor-specific ABI.
pub fn percentage(raw: &str) -> Option<f64> {
    if raw.trim() == "max" {
        return Some(100.0);
    }
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
}

pub fn is_allowed_node(node: &str) -> bool {
    CANDIDATES.iter().any(|(dir, _)| {
        node == format!("{dir}/cpu.uclamp.min") || node == format!("{dir}/cpu.uclamp.max")
    })
}

pub fn same_value(node: &str, left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    is_allowed_node(node)
        && percentage(left)
            .zip(percentage(right))
            .is_some_and(|(a, b)| (a - b).abs() < 0.000_001)
}

fn mount_confirms_cpu_cgroup(root: &SysRoot, dir: &str) -> bool {
    root.read_opt("/proc/self/mountinfo").is_some_and(|text| {
        text.lines().any(|line| {
            let Some((left, right)) = line.split_once(" - ") else {
                return false;
            };
            let Some(mount) = left.split_whitespace().nth(4) else {
                return false;
            };
            let fields: Vec<_> = right.split_whitespace().collect();
            let controller = fields.first() == Some(&"cgroup2")
                || (fields.first() == Some(&"cgroup")
                    && fields
                        .get(2)
                        .is_some_and(|opts| opts.split(',').any(|s| s == "cpu")));
            controller
                && (dir == mount
                    || dir
                        .strip_prefix(mount)
                        .is_some_and(|rest| rest.starts_with('/')))
        })
    })
}

/// `sched_util_clamp_min|max` are global and affect every task on the device. They are
/// discovered for reporting only and are disabled by default.
pub fn discover_global_clamp(root: &SysRoot) -> Option<(u32, u32)> {
    let min = root.read_trim("/proc/sys/kernel/sched_util_clamp_min")?;
    let max = root.read_trim("/proc/sys/kernel/sched_util_clamp_max")?;
    Some((min.parse().ok()?, max.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str, min: &str, max: &str) -> SysRoot {
        let dir = std::env::temp_dir().join(format!("neurosched-uclamp-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("sys/fs/cgroup/top-app");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cpu.uclamp.min"), min).unwrap();
        std::fs::write(p.join("cpu.uclamp.max"), max).unwrap();
        SysRoot::at(&dir)
    }

    #[test]
    fn discovers_cgroup_v2_nodes() {
        let root = fixture("ok", "0.00", "100.00");
        let t = discover(&root).expect("target");
        assert_eq!(t.flavour, "cgroup_v2");
        assert_eq!(t.current_max, 100.0);
    }

    #[test]
    fn textual_max_is_accepted() {
        let root = fixture("text", "0", "max");
        let t = discover(&root).expect("target");
        assert_eq!(t.current_max, 100.0);
    }

    #[test]
    fn unexpected_scale_is_rejected() {
        let root = fixture("scale", "0", "4096");
        assert!(discover(&root).is_none());
    }

    #[test]
    fn percentage_nodes_are_detected_and_converted() {
        let root = fixture("percent", "0", "100");
        let t = discover(&root).expect("target");
        assert_eq!(t.scale, UclampScale::Percent);
        // 128/1024 -> 12%, 640/1024 -> 62%, 1024 -> 100%
        assert_eq!(t.scale.to_node(0), 0);
        assert_eq!(t.scale.to_node(128), 13); // 12.5% rounds up
        assert_eq!(t.scale.to_node(640), 63); // 62.5% rounds up
        assert_eq!(t.scale.to_node(1024), 100);
        assert_eq!(t.scale.from_node(100), 1024);
    }

    #[test]
    fn undocumented_cgroup_scale_is_rejected() {
        let root = fixture("perf", "0", "1024");
        assert!(discover(&root).is_none());
        assert_eq!(UclampScale::Per1024.to_node(640), 640);
    }

    #[test]
    fn missing_nodes_yield_none() {
        assert!(discover(&SysRoot::at(
            std::env::temp_dir().join("neurosched-uclamp-none")
        ))
        .is_none());
    }

    #[test]
    fn interface_requires_a_cpu_controller_mount_with_a_path_boundary() {
        let root = fixture("mount-check", "12.50", "max");
        std::fs::create_dir_all(root.resolve("/proc/self")).unwrap();
        for mount in [
            "30 20 0:25 / /sys/fs/cgroup rw - tmpfs tmpfs rw\n",
            "30 20 0:25 / /sys/fs/cgroup/top rw - cgroup2 cgroup rw\n",
            "30 20 0:25 / /sys/fs/cgroup rw - cgroup cgroup rw,memory\n",
        ] {
            std::fs::write(root.resolve("/proc/self/mountinfo"), mount).unwrap();
            assert!(!discover(&root).unwrap().interface_verified);
        }
        std::fs::write(
            root.resolve("/proc/self/mountinfo"),
            "30 20 0:25 / /sys/fs/cgroup rw - cgroup cgroup rw,cpu\n",
        )
        .unwrap();
        assert!(discover(&root).unwrap().interface_verified);
    }
}
