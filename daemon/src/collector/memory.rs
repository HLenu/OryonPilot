//! Memory, swap and zRAM collection.
//!
//! Swap being used is *not* an error. What matters is the derivative: sustained
//! `pswpin` growth plus PSI `full` growth means the device is thrashing, while a
//! stable few hundred MiB in zRAM is healthy.

use crate::sys::SysRoot;
use crate::types::{MemSnapshot, MmdInfo, ZramDisk};
use std::collections::BTreeMap;

pub struct MemoryCollector {
    prev_pswpin: Option<(f64, u64)>,
    prev_pswpout: Option<(f64, u64)>,
    page_kb: Option<f64>,
    mmd: MmdInfo,
}

impl Default for MemoryCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryCollector {
    pub fn new() -> Self {
        Self {
            prev_pswpin: None,
            prev_pswpout: None,
            page_kb: None,
            mmd: MmdInfo::default(),
        }
    }

    /// MMD presence is established once during the boot probe (it needs `getprop`),
    /// then cached here so the 1 Hz loop stays file-only.
    pub fn set_mmd(&mut self, info: MmdInfo) {
        self.mmd = info;
    }

    pub fn collect(&mut self, root: &SysRoot, now: u64) -> MemSnapshot {
        let mut snap = MemSnapshot {
            mmd: self.mmd.clone(),
            ..Default::default()
        };

        if let Some(text) = root.read_opt("/proc/meminfo") {
            let map = parse_meminfo(&text);
            snap.total_kb = map.get("MemTotal").copied().unwrap_or(0.0);
            snap.available_kb = map.get("MemAvailable").copied().unwrap_or(0.0);
            snap.free_kb = map.get("MemFree").copied().unwrap_or(0.0);
            snap.cached_kb = map.get("Cached").copied().unwrap_or(0.0);
            snap.swap_total_kb = map.get("SwapTotal").copied().unwrap_or(0.0);
            snap.swap_free_kb = map.get("SwapFree").copied().unwrap_or(0.0);
            snap.meminfo_ok = snap.total_kb > 0.0
                && map.contains_key("MemAvailable")
                && snap.available_kb <= snap.total_kb;
        }

        if let Some(text) = root.read_opt("/proc/swaps") {
            for line in text.lines().skip(1) {
                if let Some(dev) = line.split_whitespace().next() {
                    snap.swap_devices.push(dev.to_string());
                }
            }
        }

        if let Some(text) = root.read_opt("/proc/vmstat") {
            let vm = parse_vmstat(&text);
            snap.pswpin = vm.get("pswpin").copied().unwrap_or(0.0);
            snap.pswpout = vm.get("pswpout").copied().unwrap_or(0.0);
            snap.pgscan_kswapd = vm.get("pgscan_kswapd").copied().unwrap_or(0.0);
            snap.pgsteal_kswapd = vm.get("pgsteal_kswapd").copied().unwrap_or(0.0);
            snap.pgmajfault = vm.get("pgmajfault").copied().unwrap_or(0.0);
            self.page_kb = self.page_kb.or_else(|| page_size_kb(root));
            if let Some(current) = vm.get("pswpin").copied() {
                if let (Some((prev, prev_ts)), Some(page_kb)) = (self.prev_pswpin, self.page_kb) {
                    let dt = (now.saturating_sub(prev_ts)) as f64 / 1000.0;
                    if dt > 0.2 {
                        snap.swap_in_rate_kb_s = (current - prev).max(0.0) * page_kb / dt;
                    }
                }
                self.prev_pswpin = Some((current, now));
            }
            if let Some(current) = vm.get("pswpout").copied() {
                if let (Some((prev, prev_ts)), Some(page_kb)) = (self.prev_pswpout, self.page_kb) {
                    let dt = (now.saturating_sub(prev_ts)) as f64 / 1000.0;
                    if dt > 0.2 {
                        snap.swap_out_rate_kb_s = (current - prev).max(0.0) * page_kb / dt;
                    }
                }
                self.prev_pswpout = Some((current, now));
            }
        }

        snap.zram = read_zram(root);
        snap
    }
}

/// The running kernel supplies its page size. Fixtures use the same AT_PAGESZ
/// entry from a synthetic ARM64 auxv, so 4 KiB and 16 KiB are both exercised.
pub(crate) fn page_size_kb(root: &SysRoot) -> Option<f64> {
    if root.is_system() {
        #[cfg(unix)]
        {
            // sysconf has no side effects and returns -1 when the value is unavailable.
            let bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            return (bytes > 0).then_some(bytes as f64 / 1024.0);
        }
        #[cfg(not(unix))]
        return None;
    }
    let auxv = std::fs::read(root.resolve("/proc/self/auxv")).ok()?;
    auxv.chunks_exact(16).find_map(|entry| {
        let key = u64::from_le_bytes(entry[..8].try_into().ok()?);
        let bytes = u64::from_le_bytes(entry[8..].try_into().ok()?);
        (key == 6 && bytes.is_power_of_two() && (4096..=65536).contains(&bytes))
            .then_some(bytes as f64 / 1024.0)
    })
}

fn parse_meminfo(text: &str) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(key) = it.next() else { continue };
        let Some(val) = it.next() else { continue };
        if let Ok(v) = val.parse::<f64>() {
            if !v.is_finite() || v < 0.0 {
                continue;
            }
            map.insert(key.trim_end_matches(':').to_string(), v);
        }
    }
    map
}

fn parse_vmstat(text: &str) -> BTreeMap<String, f64> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            if let Ok(v) = v.parse::<u64>() {
                map.insert(k.to_string(), v as f64);
            }
        }
    }
    map
}

/// `/sys/block/zram0/mm_stat` layout (stable since 4.7):
/// `orig_data_size compr_data_size mem_used_total mem_limit mem_used_max
///  same_pages pages_compacted huge_pages huge_pages_since`
fn read_zram(root: &SysRoot) -> Vec<ZramDisk> {
    let mut out = Vec::new();
    for name in root.list("/sys/block") {
        if !name.starts_with("zram") {
            continue;
        }
        let base = format!("/sys/block/{name}");
        let Some(text) = root.read_opt(&format!("{base}/mm_stat")) else {
            continue;
        };
        let nums: Vec<f64> = text
            .split_whitespace()
            .filter_map(|v| v.parse().ok())
            .collect();
        if nums.len() < 3 {
            continue;
        }
        let orig_kb = nums[0] / 1024.0;
        let compr_kb = nums[1] / 1024.0;
        let mem_used_kb = nums[2] / 1024.0;
        let limit_kb = nums.get(3).copied().unwrap_or(0.0) / 1024.0;
        let ratio = if compr_kb > 0.0 {
            orig_kb / compr_kb
        } else {
            0.0
        };
        out.push(ZramDisk {
            name,
            orig_kb,
            compr_kb,
            mem_used_kb,
            limit_kb,
            ratio,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meminfo_units() {
        let text = "MemTotal:       11534336 kB\nMemAvailable:    4194304 kB\nSwapTotal:       8388608 kB\nSwapFree:        6291456 kB\nCached:          2097152 kB\n";
        let m = parse_meminfo(text);
        assert_eq!(m["MemTotal"], 11534336.0);
        assert_eq!(m["MemAvailable"], 4194304.0);
        assert_eq!(m["SwapFree"], 6291456.0);
    }

    #[test]
    fn parses_vmstat_counters() {
        let text = "nr_free_pages 12345\npswpin 100\npswpout 250\npgscan_kswapd 900\npgsteal_kswapd 700\npgmajfault 42\n";
        let m = parse_vmstat(text);
        assert_eq!(m["pswpin"], 100.0);
        assert_eq!(m["pgmajfault"], 42.0);
    }

    #[test]
    fn zram_ratio_and_missing_device() {
        let dir = std::env::temp_dir().join("neurosched-mem-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        let z = dir.join("sys/block/zram0");
        std::fs::create_dir_all(&z).unwrap();
        // 1 GiB original, 256 MiB compressed, 300 MiB used.
        std::fs::write(
            z.join("mm_stat"),
            "1073741824 268435456 314572800 0 314572800 0 0 0 0\n",
        )
        .unwrap();
        let root = SysRoot::at(&dir);
        let disks = read_zram(&root);
        assert_eq!(disks.len(), 1);
        assert!((disks[0].ratio - 4.0).abs() < 1e-6);
        assert!((disks[0].orig_kb - 1048576.0).abs() < 1.0);

        let empty = SysRoot::at(dir.join("nowhere"));
        assert!(read_zram(&empty).is_empty());
    }

    #[test]
    fn swap_rate_is_computed_from_deltas() {
        let dir = std::env::temp_dir().join("neurosched-mem-rate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proc")).unwrap();
        std::fs::write(
            dir.join("proc/vmstat"),
            "pswpin 0\npswpout 0\npgscan_kswapd 0\npgsteal_kswapd 0\npgmajfault 0\n",
        )
        .unwrap();
        let root = SysRoot::at(&dir);
        let mut c = MemoryCollector::new();
        c.page_kb = Some(4.0);
        let a = c.collect(&root, 1_000);
        assert_eq!(a.swap_in_rate_kb_s, 0.0);
        // 100 pages in after 1s = 400 KiB/s
        std::fs::write(
            dir.join("proc/vmstat"),
            "pswpin 100\npswpout 0\npgscan_kswapd 0\npgsteal_kswapd 0\npgmajfault 0\n",
        )
        .unwrap();
        let b = c.collect(&root, 2_000);
        assert!(
            (b.swap_in_rate_kb_s - 400.0).abs() < 1.0,
            "{}",
            b.swap_in_rate_kb_s
        );
    }

    #[test]
    fn missing_vmstat_does_not_reset_the_rate_baseline() {
        let dir = std::env::temp_dir().join("neurosched-mem-rate-gap");
        std::fs::create_dir_all(dir.join("proc")).unwrap();
        let root = SysRoot::at(&dir);
        let mut c = MemoryCollector::new();
        c.page_kb = Some(16.0);
        std::fs::write(root.resolve("/proc/vmstat"), "pswpin 100000\npswpout 0\n").unwrap();
        c.collect(&root, 1_000);
        std::fs::remove_file(root.resolve("/proc/vmstat")).unwrap();
        assert_eq!(c.collect(&root, 2_000).swap_in_rate_kb_s, 0.0);
        std::fs::write(root.resolve("/proc/vmstat"), "pswpin 100100\npswpout 0\n").unwrap();
        assert_eq!(c.collect(&root, 3_000).swap_in_rate_kb_s, 800.0);
    }

    #[test]
    fn page_size_is_read_from_fixture_auxv() {
        let dir = std::env::temp_dir().join("neurosched-mem-pagesize");
        std::fs::create_dir_all(dir.join("proc/self")).unwrap();
        let root = SysRoot::at(&dir);
        for bytes in [4096_u64, 16384] {
            let mut auxv = 6_u64.to_le_bytes().to_vec();
            auxv.extend(bytes.to_le_bytes());
            std::fs::write(root.resolve("/proc/self/auxv"), auxv).unwrap();
            assert_eq!(page_size_kb(&root), Some(bytes as f64 / 1024.0));
        }
    }

    #[test]
    fn missing_or_invalid_page_size_is_unknown() {
        let dir = std::env::temp_dir().join("neurosched-mem-pagesize-invalid");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proc/self")).unwrap();
        let root = SysRoot::at(&dir);
        assert_eq!(page_size_kb(&root), None);
        for bytes in [0_u64, 2048, 12345, 131072] {
            let mut auxv = 6_u64.to_le_bytes().to_vec();
            auxv.extend(bytes.to_le_bytes());
            std::fs::write(root.resolve("/proc/self/auxv"), auxv).unwrap();
            assert_eq!(page_size_kb(&root), None);
        }
        std::fs::write(root.resolve("/proc/self/auxv"), [0_u8; 15]).unwrap();
        assert_eq!(page_size_kb(&root), None);
    }

    #[test]
    fn incomplete_meminfo_is_not_valid() {
        let dir = std::env::temp_dir().join("neurosched-mem-incomplete");
        std::fs::create_dir_all(dir.join("proc")).unwrap();
        let root = SysRoot::at(&dir);
        let mut c = MemoryCollector::new();
        for text in [
            "Cached: 100 kB\n",
            "MemTotal: 100 kB\n",
            "MemTotal: 100 kB\nMemAvailable: NaN kB\n",
            "MemTotal: 100 kB\nMemAvailable: 200 kB\n",
        ] {
            std::fs::write(root.resolve("/proc/meminfo"), text).unwrap();
            assert!(!c.collect(&root, 1_000).meminfo_ok);
        }
    }
}
