//! A hard denylist of nodes the daemon must never write, plus read-only helpers for
//! reporting scheduler capabilities.
//!
//! This list is enforced in one place — [`crate::actuator::Actuator::apply`] — so a
//! future profile entry cannot accidentally start poking the frequency table, the
//! thermal HAL or zRAM.

/// Returns `Some(reason)` when a node is out of bounds for v1.
pub fn forbidden(node: &str) -> Option<&'static str> {
    const RULES: [(&str, &str); 12] = [
        (
            "/sys/devices/system/cpu/cpu",
            "per-CPU frequency/online/affinity control",
        ),
        (
            "/sys/class/thermal",
            "thermal zone thresholds and trip points",
        ),
        ("/sys/class/hwmon", "thermal sensors"),
        ("/sys/block/zram", "zRAM size/algorithm/compression"),
        (
            "/proc/sys/vm",
            "vm tunables (swappiness, min_free_kbytes, dirty_ratio)",
        ),
        ("/sys/module/lowmemorykiller", "LMKD parameters"),
        ("/sys/module/mmd", "MMD parameters"),
        ("/dev/stune", "schedtune boost"),
        ("/sys/kernel/debug", "debugfs writes"),
        (
            "/proc/sys/kernel/sched_",
            "global scheduler tunables (opt-in only)",
        ),
        ("/sys/class/kgsl", "GPU frequency"),
        ("/sys/power", "power management"),
    ];
    for (prefix, reason) in RULES {
        if node.starts_with(prefix) {
            // The single exception is the opt-in global clamp, which is checked by the
            // actuator before this function is consulted.
            if prefix == "/proc/sys/kernel/sched_"
                && (node == "/proc/sys/kernel/sched_util_clamp_min"
                    || node == "/proc/sys/kernel/sched_util_clamp_max")
            {
                return None;
            }
            return Some(reason);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_dangerous_nodes() {
        assert!(forbidden("/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq").is_some());
        assert!(forbidden("/sys/class/thermal/thermal_zone0/trip_point_0_temp").is_some());
        assert!(forbidden("/sys/block/zram0/disksize").is_some());
        assert!(forbidden("/proc/sys/vm/swappiness").is_some());
        assert!(forbidden("/sys/class/kgsl/kgsl-3d0/max_gpuclk").is_some());
        assert!(forbidden("/sys/module/lowmemorykiller/parameters/minfree").is_some());
    }

    #[test]
    fn allows_only_the_opt_in_clamp() {
        assert!(forbidden("/proc/sys/kernel/sched_util_clamp_min").is_none());
        assert!(forbidden("/proc/sys/kernel/sched_util_clamp_max").is_none());
        assert!(forbidden("/proc/sys/kernel/sched_latency_ns").is_some());
    }

    #[test]
    fn allows_uclamp_cgroup_nodes() {
        assert!(forbidden("/sys/fs/cgroup/top-app/cpu.uclamp.min").is_none());
        assert!(forbidden("/dev/cpuctl/top-app/cpu.uclamp.max").is_none());
    }
}
