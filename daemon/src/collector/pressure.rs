//! PSI (/proc/pressure/*) reader.
//!
//! PSI is the primary memory-pressure signal because it measures *stall time*, not
//! occupancy: a device with 90% RAM used but no stalls is fine, and a device with
//! 40% free but constant reclaim stalls is not.

use crate::sys::SysRoot;
use crate::types::{PsiRes, PsiSnapshot};

pub fn read_resource(root: &SysRoot, path: &str) -> PsiRes {
    let mut res = PsiRes::default();
    let Some(text) = root.read_opt(path) else {
        return res;
    };
    let (mut some_seen, mut full_seen) = (false, false);
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(kind) = it.next() else { continue };
        if kind != "some" && kind != "full" {
            continue;
        }
        let (mut total, mut a10, mut a60, mut a300) = (None, None, None, None);
        for field in it {
            if let Some(v) = field.strip_prefix("avg10=") {
                a10 = v.parse::<f64>().ok();
            } else if let Some(v) = field.strip_prefix("avg60=") {
                a60 = v.parse::<f64>().ok();
            } else if let Some(v) = field.strip_prefix("avg300=") {
                a300 = v.parse::<f64>().ok();
            } else if let Some(v) = field.strip_prefix("total=") {
                total = v.parse::<u64>().ok();
            }
        }
        let (Some(a10), Some(a60), Some(a300), Some(total)) = (a10, a60, a300, total) else {
            return PsiRes::default();
        };
        if ![a10, a60, a300].iter().all(|v| (0.0..=100.0).contains(v)) {
            return PsiRes::default();
        }
        match kind {
            "some" => {
                some_seen = true;
                res.some10 = a10;
                res.some60 = a60;
                res.some300 = a300;
                res.some_total = total as f64;
            }
            "full" => {
                full_seen = true;
                res.full10 = a10;
                res.full60 = a60;
                res.full300 = a300;
                res.full_total = total as f64;
            }
            _ => {}
        }
    }
    res.available = some_seen && (path.ends_with("/cpu") || full_seen);
    if !res.available {
        return PsiRes::default();
    }
    res
}

pub fn collect(root: &SysRoot) -> PsiSnapshot {
    PsiSnapshot {
        cpu: read_resource(root, "/proc/pressure/cpu"),
        memory: read_resource(root, "/proc/pressure/memory"),
        io: read_resource(root, "/proc/pressure/io"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_some_and_full() {
        let dir = std::env::temp_dir().join("neurosched-psi-fixture");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proc/pressure")).unwrap();
        std::fs::write(
            dir.join("proc/pressure/memory"),
            "some avg10=1.25 avg60=0.80 avg300=0.40 total=123456\nfull avg10=0.30 avg60=0.10 avg300=0.05 total=4321\n",
        )
        .unwrap();
        let root = SysRoot::at(&dir);
        let res = read_resource(&root, "/proc/pressure/memory");
        assert!(res.available);
        assert!((res.some10 - 1.25).abs() < 1e-9);
        assert!((res.full10 - 0.30).abs() < 1e-9);
        assert!((res.some_total - 123456.0).abs() < 1e-9);
    }

    #[test]
    fn missing_psi_is_reported_not_faked() {
        let root = SysRoot::at(std::env::temp_dir().join("neurosched-psi-none"));
        let res = read_resource(&root, "/proc/pressure/memory");
        assert!(!res.available);
        assert_eq!(res.some10, 0.0);
    }

    #[test]
    fn malformed_lines_do_not_panic() {
        let dir = std::env::temp_dir().join("neurosched-psi-bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proc/pressure")).unwrap();
        std::fs::write(
            dir.join("proc/pressure/cpu"),
            "some avg10=xx avg60= avg300=1 total=z\n\nfull\n",
        )
        .unwrap();
        let res = read_resource(&SysRoot::at(&dir), "/proc/pressure/cpu");
        assert!(!res.available);
        assert_eq!(res.some300, 0.0);
    }

    #[test]
    fn empty_partial_and_nonfinite_psi_are_unavailable() {
        let dir = std::env::temp_dir().join("neurosched-psi-invalid");
        std::fs::create_dir_all(dir.join("proc/pressure")).unwrap();
        let root = SysRoot::at(&dir);
        for text in [
            "",
            "some avg10=0 avg60=0 avg300=0 total=0\n",
            "some avg10=NaN avg60=0 avg300=0 total=0\nfull avg10=0 avg60=0 avg300=0 total=0\n",
            "some avg10=101 avg60=0 avg300=0 total=0\nfull avg10=0 avg60=0 avg300=0 total=0\n",
        ] {
            std::fs::write(root.resolve("/proc/pressure/memory"), text).unwrap();
            assert!(!read_resource(&root, "/proc/pressure/memory").available);
        }
        std::fs::write(
            root.resolve("/proc/pressure/cpu"),
            "some avg10=0 avg60=0 avg300=0 total=0\n",
        )
        .unwrap();
        assert!(read_resource(&root, "/proc/pressure/cpu").available);
    }
}
