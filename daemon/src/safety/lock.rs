//! Kernel-owned lifetime lock. A stale PID file never authorizes recovery.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

pub struct RuntimeLock {
    _file: File,
}

impl RuntimeLock {
    pub fn acquire(data_dir: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(data_dir.join("daemon.lock"))?;
        lock(&file)?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn lock(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // flock is released by the kernel on every exit path, including SIGKILL.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn lock(file: &File) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LockFileEx(
            handle: *mut c_void,
            flags: u32,
            reserved: u32,
            low: u32,
            high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    // LOCKFILE_FAIL_IMMEDIATELY | LOCKFILE_EXCLUSIVE_LOCK, one byte at offset zero.
    let result = unsafe { LockFileEx(file.as_raw_handle(), 3, 0, 1, 0, &mut overlapped) };
    if result != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_owner_and_crash_safe_release() {
        let dir = std::env::temp_dir().join("oryonpilot-runtime-lock");
        std::fs::create_dir_all(&dir).unwrap();
        let first = RuntimeLock::acquire(&dir).unwrap();
        assert!(RuntimeLock::acquire(&dir).is_err());
        drop(first);
        assert!(RuntimeLock::acquire(&dir).is_ok());
    }
}
