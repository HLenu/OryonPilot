//! Path-injected view of `/proc` and `/sys`.
//!
//! Every collector and actuator takes a [`SysRoot`]. In production it is rooted at
//! `/`; in tests it is rooted at a fixture directory. Faults can be injected per
//! path so that "write fails", "node missing" and "value gets clobbered by another
//! service" can be exercised without a phone.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Production kernel writes require a separately reviewed build to enable them.
/// Fixtures remain writable so actuator and recovery paths can be tested safely.
pub const ACTUATOR_ENABLED: bool = false;

/// Injected behaviour for a single path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Reads fail with `PermissionDenied`.
    Unreadable,
    /// Reads succeed but the file is empty.
    Empty,
    /// Writes fail with `PermissionDenied`.
    WriteFail,
    /// Writes succeed but the value read back differs (hardware/kernel clamp).
    WriteSilentlyIgnored,
}

/// What the daemon learned about a node without touching it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeProbe {
    pub path: String,
    pub exists: bool,
    pub readable: bool,
    pub owner_write_bit: bool,
    pub value: Option<String>,
}

impl NodeProbe {
    pub fn usable(&self) -> bool {
        self.exists && self.readable
    }
}

#[derive(Debug, Clone, Default)]
pub struct SysRoot {
    base: PathBuf,
    faults: Arc<BTreeMap<String, Fault>>,
    writes: Arc<Mutex<Vec<(String, String)>>>,
    writable: bool,
}

impl SysRoot {
    /// Root at the real filesystem.
    pub fn system() -> Self {
        Self {
            base: PathBuf::new(),
            faults: Arc::new(BTreeMap::new()),
            writes: Arc::new(Mutex::new(Vec::new())),
            writable: true,
        }
    }

    /// Root at a fixture directory. Absolute paths are re-based, e.g.
    /// `/proc/meminfo` becomes `<base>/proc/meminfo`.
    pub fn at(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            faults: Arc::new(BTreeMap::new()),
            writes: Arc::new(Mutex::new(Vec::new())),
            writable: true,
        }
    }

    pub fn with_faults(mut self, faults: BTreeMap<String, Fault>) -> Self {
        self.faults = Arc::new(faults);
        self
    }

    /// Disables all writes (used by SAFE_MODE and by `--self-test`).
    pub fn read_only(mut self) -> Self {
        self.writable = false;
        self
    }

    pub fn is_system(&self) -> bool {
        self.base.as_os_str().is_empty()
            || (self.base.has_root() && self.base.parent().is_none())
            || self
                .base
                .canonicalize()
                .map(|base| base.parent().is_none())
                .unwrap_or(false)
    }

    pub fn resolve(&self, abs: &str) -> PathBuf {
        if self.base.as_os_str().is_empty() {
            PathBuf::from(abs)
        } else {
            self.base.join(abs.trim_start_matches('/'))
        }
    }

    fn fault(&self, abs: &str) -> Option<Fault> {
        self.faults.get(abs).copied()
    }

    pub fn exists(&self, abs: &str) -> bool {
        !matches!(self.fault(abs), Some(Fault::Unreadable)) && self.resolve(abs).exists()
    }

    pub fn read(&self, abs: &str) -> io::Result<String> {
        match self.fault(abs) {
            Some(Fault::Unreadable) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{abs}: injected unreadable"),
            )),
            Some(Fault::Empty) => Ok(String::new()),
            _ => std::fs::read_to_string(self.resolve(abs)),
        }
    }

    pub fn read_opt(&self, abs: &str) -> Option<String> {
        self.read(abs).ok()
    }

    pub fn read_trim(&self, abs: &str) -> Option<String> {
        self.read(abs).ok().map(|s| s.trim().to_string())
    }

    pub fn read_f64(&self, abs: &str) -> Option<f64> {
        self.read_trim(abs)?
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
    }

    pub fn read_i64(&self, abs: &str) -> Option<i64> {
        let t = self.read_trim(abs)?;
        t.parse::<i64>()
            .ok()
            .or_else(|| t.split_whitespace().next()?.parse().ok())
    }

    /// Sorted directory entry names (files and dirs), empty when missing.
    pub fn list(&self, abs: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(self.resolve(abs)) {
            for entry in rd.flatten() {
                out.push(entry.file_name().to_string_lossy().to_string());
            }
        }
        out.sort();
        out
    }

    pub fn is_dir(&self, abs: &str) -> bool {
        self.resolve(abs).is_dir()
    }

    pub fn write(&self, abs: &str, value: &str) -> io::Result<()> {
        let system = self.is_system();
        if system && !ACTUATOR_ENABLED {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{abs}: production actuator is disabled"),
            ));
        }
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{abs}: SysRoot is read-only"),
            ));
        }
        let target = if system {
            self.resolve(abs)
        } else {
            let base = self.base.canonicalize()?;
            if base.parent().is_none() || protected_fixture_base(&base) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "fixture root is a protected system path",
                ));
            }
            let target = self.resolve(abs).canonicalize()?;
            if target == base || !target.starts_with(&base) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "fixture write escapes its root",
                ));
            }
            target
        };
        match self.fault(abs) {
            Some(Fault::WriteFail) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{abs}: injected write failure"),
                ))
            }
            Some(Fault::WriteSilentlyIgnored) => {
                if !system {
                    self.writes
                        .lock()
                        .map_err(|_| io::Error::other("write log poisoned"))?
                        .push((abs.to_string(), value.to_string()));
                }
                return Ok(());
            }
            _ => {}
        }
        std::fs::write(target, value.as_bytes())?;
        if !system {
            self.writes
                .lock()
                .map_err(|_| io::Error::other("write log poisoned"))?
                .push((abs.to_string(), value.to_string()));
        }
        Ok(())
    }

    /// Writes recorded since process start. Only meaningful for fixtures, but the
    /// list is also used by `--self-test` to prove that observe mode wrote nothing.
    pub fn recorded_writes(&self) -> Vec<(String, String)> {
        self.writes.lock().map(|w| w.clone()).unwrap_or_default()
    }

    pub fn probe(&self, abs: &str) -> NodeProbe {
        let path = self.resolve(abs);
        let exists = path.exists() && !matches!(self.fault(abs), Some(Fault::Unreadable));
        let value = if exists { self.read_trim(abs) } else { None };
        let owner_write_bit = write_bit(&path);
        NodeProbe {
            path: abs.to_string(),
            exists,
            readable: value.is_some(),
            owner_write_bit,
            value,
        }
    }
}

#[cfg(unix)]
fn protected_fixture_base(base: &Path) -> bool {
    ["/proc", "/sys", "/dev"]
        .iter()
        .any(|path| base.starts_with(path))
}

#[cfg(not(unix))]
fn protected_fixture_base(_base: &Path) -> bool {
    false
}

#[cfg(unix)]
fn write_bit(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o200 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn write_bit(path: &Path) -> bool {
    // Windows has no meaningful mode bits; fixtures are written by the test process
    // itself, so treat existing files as writable.
    path.exists()
}

/// Accepts comma-separated ranges and the space-separated cpufreq CPU lists.
/// Returns an empty vector on malformed input
/// instead of panicking: a wrong CPU list must never take the daemon down.
pub fn parse_cpu_list(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in s.split(|c: char| c == ',' || c.is_ascii_whitespace()) {
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                if a <= b && b - a < 1024 {
                    out.extend(a..=b);
                }
            }
        } else if let Ok(v) = part.parse::<u32>() {
            out.push(v);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_list_parsing() {
        assert_eq!(parse_cpu_list("0-3,6-7"), vec![0, 1, 2, 3, 6, 7]);
        assert_eq!(parse_cpu_list("0"), vec![0]);
        assert_eq!(parse_cpu_list("0-1,1-2"), vec![0, 1, 2]);
        assert_eq!(parse_cpu_list("0 1 2 3 4 5\n"), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(parse_cpu_list("0-2, 4\t6-7"), vec![0, 1, 2, 4, 6, 7]);
        assert!(parse_cpu_list("garbage").is_empty());
        assert!(parse_cpu_list("5-1").is_empty());
    }

    #[test]
    fn read_only_root_refuses_writes() {
        let dir = std::env::temp_dir().join("neurosched-sysro-test-ro");
        std::fs::create_dir_all(&dir).unwrap();
        let root = SysRoot::at(&dir).read_only();
        assert!(root.write("/proc/x", "1").is_err());
        assert!(root.recorded_writes().is_empty());
    }

    #[test]
    fn production_gate_rejects_writes_before_accessing_the_filesystem() {
        let root = SysRoot::system();
        let error = root
            .write("/oryonpilot-nonexistent-gate-test", "1")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error
            .to_string()
            .contains("production actuator is disabled"));
        assert!(root.recorded_writes().is_empty());
    }

    #[test]
    fn filesystem_root_cannot_be_used_as_a_writable_fixture() {
        let root = SysRoot::at("/");
        assert!(root.is_system());
        let error = root
            .write("/oryonpilot-nonexistent-gate-test", "1")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error
            .to_string()
            .contains("production actuator is disabled"));
        assert!(root.recorded_writes().is_empty());
    }

    #[test]
    fn fixture_parent_traversal_does_not_write_outside_the_fixture() {
        let dir = std::env::temp_dir().join("neurosched-sys-boundary");
        let base = dir.join("fixture");
        std::fs::create_dir_all(&base).unwrap();
        let outside = dir.join("outside");
        std::fs::write(&outside, "untouched").unwrap();
        let root = SysRoot::at(&base);
        let error = root.write("/../outside", "changed").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
        assert!(root.recorded_writes().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fixture_symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join("neurosched-sys-symlink-boundary");
        let base = dir.join("fixture");
        std::fs::create_dir_all(&base).unwrap();
        let outside = dir.join("outside");
        std::fs::write(&outside, "untouched").unwrap();
        let link = base.join("alias");
        let _ = std::fs::remove_file(&link);
        symlink(&outside, &link).unwrap();
        let root = SysRoot::at(&base);
        assert_eq!(
            root.write("/alias", "changed").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
    }

    #[cfg(unix)]
    #[test]
    fn kernel_filesystems_cannot_be_used_as_fixture_roots() {
        for path in ["/proc", "/sys", "/dev"] {
            let root = SysRoot::at(path);
            assert_eq!(
                root.write("/oryonpilot-nonexistent-gate-test", "1")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }
}
