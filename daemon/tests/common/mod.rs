//! Synthetic device tree used by the integration tests.
//!
//! It mirrors the shape of a Xiaomi 15 (Snapdragon 8 Elite, HyperOS) closely enough to
//! exercise cluster discovery, PSI, zRAM, thermal mapping, cgroup uclamp and the
//! journal — without a phone attached. Values are plausible, not measured; the point is
//! the *shape* of the tree, not the numbers.

#![allow(dead_code)]
#![allow(clippy::field_reassign_with_default)]

use std::path::{Path, PathBuf};

pub struct Fixture {
    pub root: PathBuf,
}

impl Fixture {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("neurosched-it-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Fixture { root }
    }

    fn write(&self, path: &str, body: &str) {
        let full = self.root.join(path.trim_start_matches('/'));
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, body).unwrap();
    }

    /// Full tree for a 8 Elite device: 2 clusters (6 + 2), PSI on, zRAM, MMD, uclamp v2.
    pub fn xiaomi15(name: &str) -> Self {
        let f = Fixture::new(name);

        f.write(
            "/proc/sys/kernel/random/boot_id",
            "11111111-1111-4111-8111-111111111111\n",
        );

        f.write(
            "/proc/self/mountinfo",
            "30 20 0:25 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
        );

        // ---- proc ----
        f.write(
            "/proc/version",
            "Linux version 6.6.30-android15-8-g1234-ab (builder@host) #1 SMP PREEMPT\n",
        );
        f.write("/proc/uptime", "8421.55 30000.12\n");
        f.write(
            "/proc/stat",
            "cpu  120000 200 45000 900000 5000 300 1200 0 0 0\n\
             cpu0 20000 30 8000 110000 900 40 150 0 0 0\n\
             cpu1 19000 20 7000 112000 800 30 140 0 0 0\n\
             cpu2 18000 30 6500 113000 700 40 130 0 0 0\n\
             cpu3 17000 20 6000 114000 600 30 120 0 0 0\n\
             cpu4 16000 30 5500 115000 500 40 110 0 0 0\n\
             cpu5 15000 20 5000 116000 400 30 100 0 0 0\n\
             cpu6 10000 30 4500 110000 600 40 250 0 0 0\n\
             cpu7 5000 20 2500 110000 500 50 200 0 0 0\n\
             intr 123456 0 0\nctxt 987654\n",
        );
        f.write(
            "/proc/meminfo",
            "MemTotal:       11534336 kB\n\
             MemFree:         1048576 kB\n\
             MemAvailable:    5242880 kB\n\
             Buffers:           65536 kB\n\
             Cached:          3145728 kB\n\
             SwapCached:       262144 kB\n\
             SwapTotal:       8388608 kB\n\
             SwapFree:        6291456 kB\n",
        );
        f.write(
            "/proc/vmstat",
            "nr_free_pages 262144\npswpin 15234\npswpout 40211\npgscan_kswapd 90210\n\
             pgsteal_kswapd 70123\npgmajfault 8123\npgscan_direct 120\n",
        );
        f.write(
            "/proc/swaps",
            "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n\
             /dev/block/zram0                        partition\t8388604\t\t2097152\t\t-2\n",
        );
        f.write(
            "/proc/pressure/cpu",
            "some avg10=3.21 avg60=2.10 avg300=1.05 total=882134\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
        );
        f.write(
            "/proc/pressure/memory",
            "some avg10=1.42 avg60=0.95 avg300=0.44 total=421337\nfull avg10=0.18 avg60=0.11 avg300=0.05 total=12034\n",
        );
        f.write(
            "/proc/pressure/io",
            "some avg10=0.85 avg60=0.60 avg300=0.30 total=120000\nfull avg10=0.05 avg60=0.02 avg300=0.01 total=3000\n",
        );

        // kswapd0
        f.write("/proc/1234/comm", "kswapd0\n");
        f.write(
            "/proc/1234/stat",
            "1234 (kswapd0) S 2 0 0 0 -1 69238880 0 0 0 0 900 120 0 0 20 0 1 0 100 0 0\n",
        );
        f.write("/proc/1234/cgroup", "0::/system\n");
        // mmd
        f.write("/proc/1560/comm", "mmd\n");
        f.write("/proc/1560/cmdline", "/system/bin/mmd\u{0}--daemon\u{0}");
        f.write("/proc/1560/cgroup", "0::/system\n");
        // a foreground app in top-app
        f.write("/proc/4242/comm", "com.tencent.mm\n");
        f.write("/proc/4242/cmdline", "com.tencent.mm\u{0}");
        f.write("/proc/4242/cgroup", "0::/top-app\n");
        f.write("/proc/4242/oom_score_adj", "0\n");

        // ---- cpu topology ----
        f.write("/sys/devices/system/cpu/present", "0-7\n");
        f.write("/sys/devices/system/cpu/online", "0-7\n");
        for (pol, cpus, max_khz, _cap) in [
            ("policy0", "0-5", 1_900_000u32, 700u32),
            ("policy6", "6-7", 3_500_000, 1024),
        ] {
            let base = format!("/sys/devices/system/cpu/cpufreq/{pol}");
            f.write(&format!("{base}/related_cpus"), &format!("{cpus}\n"));
            f.write(&format!("{base}/affected_cpus"), &format!("{cpus}\n"));
            f.write(
                &format!("{base}/scaling_cur_freq"),
                &format!("{}\n", max_khz / 2),
            );
            f.write(&format!("{base}/scaling_min_freq"), "300000\n");
            f.write(&format!("{base}/scaling_max_freq"), &format!("{max_khz}\n"));
            f.write(&format!("{base}/cpuinfo_max_freq"), &format!("{max_khz}\n"));
            f.write(&format!("{base}/cpuinfo_min_freq"), "300000\n");
            f.write(&format!("{base}/scaling_governor"), "schedutil\n");
            f.write(
                &format!("{base}/scaling_available_governors"),
                "performance powersave schedutil\n",
            );
        }
        for cpu in 0..8u32 {
            let cap = if cpu >= 6 { 1024 } else { 700 };
            f.write(
                &format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity"),
                &format!("{cap}\n"),
            );
            f.write(
                &format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_id"),
                &format!("{}\n", cpu % 6),
            );
            f.write(
                &format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq"),
                &format!("{}\n", if cpu >= 6 { 1_750_000 } else { 950_000 }),
            );
        }

        // ---- thermal ----
        for (i, (name, temp)) in [
            ("cpu-0-0", 46_000),
            ("cpu-1-0", 44_500),
            ("gpuss-0", 43_000),
            ("battery", 32_100),
            ("charger", 36_000),
            ("skin-therm", 38_500),
            ("quiet-therm", 40_000),
        ]
        .iter()
        .enumerate()
        {
            f.write(&format!("/sys/class/thermal/thermal_zone{i}/type"), name);
            f.write(
                &format!("/sys/class/thermal/thermal_zone{i}/temp"),
                &format!("{temp}\n"),
            );
        }

        // ---- power supply ----
        f.write("/sys/class/power_supply/battery/type", "Battery\n");
        f.write("/sys/class/power_supply/battery/capacity", "68\n");
        f.write("/sys/class/power_supply/battery/temp", "321\n");
        f.write("/sys/class/power_supply/battery/status", "Discharging\n");
        f.write("/sys/class/power_supply/battery/current_now", "-850000\n");
        f.write("/sys/class/power_supply/battery/voltage_now", "3980000\n");
        f.write("/sys/class/power_supply/usb/type", "USB\n");
        f.write("/sys/class/power_supply/usb/online", "0\n");

        // ---- zram ----
        f.write(
            "/sys/block/zram0/mm_stat",
            "2147483648 402653184 520093696 0 520093696 0 1200 0 0\n",
        );
        f.write("/sys/block/zram0/disksize", "8589934592\n");

        // ---- cgroup v2 uclamp ----
        f.write(
            "/sys/fs/cgroup/cgroup.controllers",
            "cpuset cpu io memory\n",
        );
        f.write("/sys/fs/cgroup/top-app/cpu.uclamp.min", "0\n");
        f.write("/sys/fs/cgroup/top-app/cpu.uclamp.max", "100\n");
        f.write("/sys/fs/cgroup/top-app/cpu.weight", "1024\n");

        // ---- backlight ----
        f.write("/sys/class/backlight/panel0-backlight/brightness", "2047\n");

        // ---- system bits ----
        f.write("/system/bin/mmd", "");
        f.write("/system/bin/logcat", "");
        f.write("/sys/fs/selinux/enforce", "1\n");
        f
    }

    pub fn path(&self, p: &str) -> PathBuf {
        self.root.join(p.trim_start_matches('/'))
    }

    pub fn read(&self, p: &str) -> String {
        std::fs::read_to_string(self.path(p)).unwrap_or_default()
    }

    pub fn set(&self, p: &str, value: &str) {
        let full = self.path(p);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, value).unwrap();
    }

    /// Simulates an external service (e.g. Scene) taking over a node.
    pub fn clobber(&self, p: &str, value: &str) {
        self.set(p, value);
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
