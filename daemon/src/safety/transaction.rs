//! Write journal: every parameter change is recorded *before* it is applied, and the
//! session is closed only after the daemon is sure it succeeded.
//!
//! On the next start, an unclosed session is replayed in reverse so that a daemon
//! killed mid-write cannot leave the device in a half-tuned state.

use crate::json::{self, Json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Recovery eligibility is established before any caller is given node entries.
/// A journal from another kernel boot must never be replayed into this boot.
#[derive(Debug)]
pub enum PendingRecovery {
    Replay {
        session: String,
        entries: Vec<JournalEntry>,
    },
    PreviousBoot {
        session: String,
        boot_id: String,
        empty: bool,
    },
    /// An old-format journal with zero bytes cannot contain a durable write intent.
    LegacyEmpty { session: String },
}

fn valid_session(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 160
        && session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn parse_boot_id(text: &str) -> Option<String> {
    let text = text.trim();
    if text.len() != 36
        || !text.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
        || text.bytes().all(|b| b == b'0' || b == b'-')
    {
        return None;
    }
    Some(text.to_ascii_lowercase())
}

#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub ts_ms: u64,
    pub session: String,
    pub node: String,
    pub old: String,
    pub new: String,
    pub state: String,
    pub mode: String,
    pub result: String,
}

impl JournalEntry {
    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("ts", Json::num(self.ts_ms as f64));
        j.set("session", Json::str(self.session.clone()));
        j.set("node", Json::str(self.node.clone()));
        j.set("old", Json::str(self.old.clone()));
        j.set("new", Json::str(self.new.clone()));
        j.set("state", Json::str(self.state.clone()));
        j.set("mode", Json::str(self.mode.clone()));
        j.set("result", Json::str(self.result.clone()));
        j
    }

    pub fn from_json(j: &Json) -> Option<Self> {
        Some(JournalEntry {
            ts_ms: j.get("ts").and_then(|v| v.as_u64()).unwrap_or(0),
            session: j.get("session")?.as_str()?.to_string(),
            node: j.get("node")?.as_str()?.to_string(),
            old: j
                .get("old")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            new: j
                .get("new")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            state: j
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            mode: j
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            result: j
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
    }
}

#[derive(Clone)]
pub struct Journal {
    dir: PathBuf,
    session: String,
    enabled: bool,
    boot_id: Option<String>,
    dirty: Arc<AtomicBool>,
}

impl Journal {
    /// Legacy constructor for synthetic actuator tests. Device runtime and CLI
    /// recovery must use `for_root`, which requires a kernel boot identity.
    pub fn new(dir: impl Into<PathBuf>, session: String) -> Self {
        Self {
            dir: dir.into(),
            session,
            enabled: true,
            boot_id: None,
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn for_root(
        dir: impl Into<PathBuf>,
        session: String,
        root: &crate::sys::SysRoot,
    ) -> std::io::Result<Self> {
        let boot_id = root
            .read_trim("/proc/sys/kernel/random/boot_id")
            .and_then(|text| parse_boot_id(&text))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "kernel boot_id missing, unreadable or malformed; journal recovery refused",
                )
            })?;
        if !valid_session(&session) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid journal session",
            ));
        }
        Ok(Self {
            dir: dir.into(),
            session,
            enabled: true,
            boot_id: Some(boot_id),
            dirty: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Used by `--self-test` and by dry runs that must not touch the data dir.
    pub fn disabled() -> Self {
        Self {
            dir: PathBuf::new(),
            session: "disabled".into(),
            enabled: false,
            boot_id: None,
            dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    fn file(&self) -> PathBuf {
        self.dir.join(format!("{}.jsonl", self.session))
    }

    fn active(&self) -> PathBuf {
        self.dir.join("active")
    }

    pub fn begin(&self) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !valid_session(&self.session) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid journal session",
            ));
        }
        std::fs::create_dir_all(&self.dir)?;
        if self.boot_id.is_some() && self.active().try_exists()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "resolve or archive the active journal before beginning a session",
            ));
        }
        // Create the unique session before replacing any previous active marker.
        let session_file = self.file();
        std::fs::File::create_new(&session_file)?.sync_all()?;
        crate::storage::secure_file(&session_file);
        if let Some(boot_id) = &self.boot_id {
            // Preserve identity after the active marker is closed or archived.
            let path = self.dir.join(format!("{}.boot_id", self.session));
            let mut metadata = std::fs::File::create_new(&path)?;
            writeln!(metadata, "{boot_id}")?;
            metadata.sync_all()?;
            crate::storage::secure_file(&path);
        }
        let mut f = if self.boot_id.is_some() {
            std::fs::File::create_new(self.active())?
        } else {
            std::fs::File::create(self.active())?
        };
        writeln!(f, "{}", self.session)?;
        writeln!(f, "{}", std::process::id())?;
        if let Some(boot_id) = &self.boot_id {
            writeln!(f, "boot_id={boot_id}")?;
        }
        f.sync_all()?;
        crate::storage::secure_file(&self.active());
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    /// Ends the node's previous ownership generation without removing its history.
    pub fn settle_node(&self, node: &str) -> std::io::Result<()> {
        self.append(&JournalEntry {
            ts_ms: crate::util::now_ms(),
            session: self.session.clone(),
            node: node.into(),
            old: String::new(),
            new: String::new(),
            state: "RESTORE".into(),
            mode: "observe_only".into(),
            result: "settled".into(),
        })
    }

    pub fn append(&self, e: &JournalEntry) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file())?;
        writeln!(f, "{}", e.to_json())?;
        // fsync on every entry: a crash between write and readback must not lose the
        // record of what we changed.
        f.sync_all()?;
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    pub fn end(&self) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        match std::fs::remove_file(self.active()) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    pub fn read_session(&self, session: &str) -> Vec<JournalEntry> {
        let path = self.dir.join(format!("{session}.jsonl"));
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| json::parse(l).ok())
            .filter_map(|j| JournalEntry::from_json(&j))
            .collect()
    }

    /// Returns the session id and its entries when a previous run did not close cleanly.
    pub fn pending(&self) -> Option<(String, Vec<JournalEntry>)> {
        self.pending_checked().ok().flatten()
    }

    pub fn pending_checked(&self) -> std::io::Result<Option<(String, Vec<JournalEntry>)>> {
        match self.pending_recovery()? {
            None => Ok(None),
            Some(PendingRecovery::Replay { session, entries }) => Ok(Some((session, entries))),
            Some(PendingRecovery::LegacyEmpty { session }) => Ok(Some((session, Vec::new()))),
            Some(PendingRecovery::PreviousBoot { .. }) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "journal belongs to a previous boot; archive it without replay",
            )),
        }
    }

    /// Inspect the previous active session without changing any file. Production
    /// callers match this result before deciding to replay, archive or stop.
    pub fn pending_recovery(&self) -> std::io::Result<Option<PendingRecovery>> {
        use std::io::{Error, ErrorKind};
        if !self.enabled {
            return Ok(None);
        }
        let text = match std::fs::read_to_string(self.active()) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut lines = text.lines();
        let session = lines.next().unwrap_or("").trim().to_string();
        if !valid_session(&session)
            || lines
                .next()
                .and_then(|pid| pid.parse::<u32>().ok())
                .filter(|pid| *pid > 0)
                .is_none()
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "invalid active journal session or PID",
            ));
        }
        let marker_boot = match lines.next() {
            Some(line) => Some(
                line.strip_prefix("boot_id=")
                    .and_then(parse_boot_id)
                    .ok_or_else(|| Error::new(ErrorKind::InvalidData, "invalid journal boot_id"))?,
            ),
            None => None,
        };
        if lines.next().is_some() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "extra active journal fields",
            ));
        }
        let body = std::fs::read_to_string(self.dir.join(format!("{session}.jsonl")))?;
        if let Some(current_boot) = &self.boot_id {
            match marker_boot {
                Some(previous_boot) => {
                    let metadata =
                        std::fs::read_to_string(self.dir.join(format!("{session}.boot_id")))?;
                    if parse_boot_id(&metadata).as_deref() != Some(previous_boot.as_str()) {
                        return Err(Error::new(
                            ErrorKind::InvalidData,
                            "active marker and journal boot metadata disagree",
                        ));
                    }
                    if &previous_boot != current_boot {
                        return Ok(Some(PendingRecovery::PreviousBoot {
                            session,
                            boot_id: previous_boot,
                            empty: body.is_empty(),
                        }));
                    }
                }
                None if body.is_empty()
                    && !self.dir.join(format!("{session}.boot_id")).exists() =>
                {
                    return Ok(Some(PendingRecovery::LegacyEmpty { session }));
                }
                None => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "legacy nonempty journal has no verified boot identity; evidence retained",
                    ));
                }
            }
        } else if marker_boot.is_some() {
            // A caller using the legacy test constructor must not bypass a real
            // boot-bound journal's eligibility check.
            return Err(Error::new(
                ErrorKind::InvalidData,
                "boot-bound journal requires a current kernel boot_id",
            ));
        }
        let mut entries = Vec::new();
        for line in body.split_inclusive('\n') {
            // An incomplete final record cannot precede a successful durable write.
            if !line.ends_with('\n') {
                break;
            }
            let entry = json::parse(line)
                .ok()
                .and_then(|j| JournalEntry::from_json(&j))
                .filter(|e| e.session == session)
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "corrupt journal record"))?;
            entries.push(entry);
        }
        Ok(Some(PendingRecovery::Replay { session, entries }))
    }

    /// Retire only a verified previous-boot session. The active marker is copied
    /// durably before removal; the marker, boot identity and raw journal remain
    /// protected from normal rotation for later inspection. No node is touched.
    pub fn archive_previous_boot(&self, expected_session: &str) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};
        match self.pending_recovery()? {
            Some(PendingRecovery::PreviousBoot { session, .. }) if session == expected_session => {}
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "active journal is not the expected previous-boot session",
                ));
            }
        }
        let marker = std::fs::read(self.active())?;
        let archive = self.dir.join(format!("{expected_session}.previous-boot"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&archive)
        {
            Ok(mut file) => {
                file.write_all(&marker)?;
                file.sync_all()?;
                crate::storage::secure_file(&archive);
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if std::fs::read(&archive)? != marker {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "archive marker mismatch",
                    ));
                }
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&archive)?
                    .sync_all()?;
            }
            Err(error) => return Err(error),
        }
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        std::fs::remove_file(self.active())?;
        #[cfg(unix)]
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }

    pub fn recent(&self, limit: usize) -> Vec<JournalEntry> {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(self.file()) else {
            return Vec::new();
        };
        let length = file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = length.saturating_sub(64 * 1024);
        if file.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
        let mut tail = String::new();
        if file.take(64 * 1024).read_to_string(&mut tail).is_err() {
            return Vec::new();
        }
        let complete = if start > 0 {
            tail.split_once('\n').map(|(_, tail)| tail).unwrap_or("")
        } else {
            &tail
        };
        complete
            .lines()
            .rev()
            .filter_map(|line| json::parse(line).ok())
            .filter_map(|j| JournalEntry::from_json(&j))
            .take(limit)
            .collect()
    }

    /// Deletes old session files, keeping the newest `keep`.
    pub fn rotate(&self, keep: usize) {
        if !self.enabled {
            return;
        }
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    files.push((modified, p));
                }
            }
        }
        files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
        for (index, (_, p)) in files.into_iter().enumerate() {
            if index >= keep.max(1)
                && p != self.file()
                && !p.with_extension("previous-boot").exists()
                && std::fs::remove_file(&p).is_ok()
            {
                let _ = std::fs::remove_file(p.with_extension("boot_id"));
            }
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT_A: &str = "01234567-89ab-4def-8123-456789abcdef";
    const BOOT_B: &str = "11234567-89ab-4def-8123-456789abcdef";

    fn boot_fixture(name: &str, boot_id: &str) -> (crate::sys::SysRoot, PathBuf) {
        let base = std::env::temp_dir().join(format!("neurosched-journal-boot-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let root = crate::sys::SysRoot::at(base.join("device"));
        let path = root.resolve("/proc/sys/kernel/random/boot_id");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, boot_id).unwrap();
        (root, base.join("journal"))
    }

    #[test]
    fn same_boot_provides_entries_and_persists_identity() {
        let (root, dir) = boot_fixture("same", BOOT_A);
        let old = Journal::for_root(&dir, "old".into(), &root).unwrap();
        old.begin().unwrap();
        old.append(&entry("old", "/n", "0", "10")).unwrap();
        let next = Journal::for_root(&dir, "next".into(), &root).unwrap();
        assert!(next.begin().is_err());
        assert!(!next.file().exists());
        match next.pending_recovery().unwrap().unwrap() {
            PendingRecovery::Replay { session, entries } => {
                assert_eq!(session, "old");
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].old, "0");
            }
            other => panic!("unexpected recovery: {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("old.boot_id"))
                .unwrap()
                .trim(),
            BOOT_A
        );
        assert!(std::fs::read_to_string(old.active())
            .unwrap()
            .contains(BOOT_A));
        assert!(Journal::new(&dir, "unchecked".into())
            .pending_checked()
            .is_err());
    }

    #[test]
    fn different_boot_is_archived_without_replay_and_survives_rotation() {
        let (root, dir) = boot_fixture("different", BOOT_A);
        let old = Journal::for_root(&dir, "old".into(), &root).unwrap();
        old.begin().unwrap();
        old.append(&entry("old", "/n", "0", "10")).unwrap();
        let old_bytes = std::fs::read(old.file()).unwrap();
        let old_marker = std::fs::read(old.active()).unwrap();
        std::fs::write(root.resolve("/proc/sys/kernel/random/boot_id"), BOOT_B).unwrap();
        let next = Journal::for_root(&dir, "next".into(), &root).unwrap();
        match next.pending_recovery().unwrap().unwrap() {
            PendingRecovery::PreviousBoot {
                session,
                boot_id,
                empty,
            } => {
                assert_eq!(session, "old");
                assert_eq!(boot_id, BOOT_A);
                assert!(!empty);
            }
            other => panic!("unexpected recovery: {other:?}"),
        }
        assert!(next.pending_checked().is_err());
        assert!(next.archive_previous_boot("wrong-session").is_err());
        next.archive_previous_boot("old").unwrap();
        assert!(!old.active().exists());
        assert_eq!(
            std::fs::read(dir.join("old.previous-boot")).unwrap(),
            old_marker
        );
        next.begin().unwrap();
        next.rotate(1);
        assert_eq!(std::fs::read(old.file()).unwrap(), old_bytes);
        assert_eq!(
            std::fs::read_to_string(dir.join("old.boot_id"))
                .unwrap()
                .trim(),
            BOOT_A
        );
    }

    #[test]
    fn empty_previous_boot_session_is_reported_empty_while_partial_record_is_not() {
        let (root, dir) = boot_fixture("empty-diff-boot", BOOT_A);
        let old = Journal::for_root(&dir, "old".into(), &root).unwrap();
        old.begin().unwrap();
        std::fs::write(root.resolve("/proc/sys/kernel/random/boot_id"), BOOT_B).unwrap();
        let next = Journal::for_root(&dir, "next".into(), &root).unwrap();
        assert!(matches!(
            next.pending_recovery().unwrap(),
            Some(PendingRecovery::PreviousBoot {
                ref session,
                ref boot_id,
                empty: true
            }) if session == "old" && boot_id == BOOT_A
        ));
        std::fs::write(old.file(), "{unfinished").unwrap();
        assert!(matches!(
            next.pending_recovery().unwrap(),
            Some(PendingRecovery::PreviousBoot {
                ref session,
                ref boot_id,
                empty: false
            }) if session == "old" && boot_id == BOOT_A
        ));
    }

    #[test]
    fn only_a_truly_empty_legacy_journal_can_migrate_without_boot_identity() {
        let (root, dir) = boot_fixture("legacy", BOOT_A);
        let old = Journal::new(&dir, "legacy".into());
        old.begin().unwrap();
        let next = Journal::for_root(&dir, "next".into(), &root).unwrap();
        assert!(matches!(
            next.pending_recovery().unwrap(),
            Some(PendingRecovery::LegacyEmpty { .. })
        ));
        assert!(next.archive_previous_boot("legacy").is_err());
        // Even an incomplete final record is not proof that an old implementation
        // never wrote a node. Preserve it and demand manual inspection.
        std::fs::write(old.file(), "{unfinished").unwrap();
        let marker = std::fs::read(old.active()).unwrap();
        assert!(next.pending_recovery().is_err());
        assert_eq!(std::fs::read(old.active()).unwrap(), marker);
        std::fs::write(old.file(), "").unwrap();
        old.append(&entry("legacy", "/n", "0", "10")).unwrap();
        assert!(next.pending_recovery().is_err());
    }

    #[test]
    fn missing_or_invalid_current_boot_identity_is_rejected_without_mutation() {
        let (root, dir) = boot_fixture("invalid-current", BOOT_A);
        let old = Journal::for_root(&dir, "old".into(), &root).unwrap();
        old.begin().unwrap();
        old.append(&entry("old", "/n", "0", "10")).unwrap();
        let marker = std::fs::read(old.active()).unwrap();
        for value in [
            "",
            "not-a-uuid",
            "00000000-0000-0000-0000-000000000000",
            "../../escape",
        ] {
            std::fs::write(root.resolve("/proc/sys/kernel/random/boot_id"), value).unwrap();
            assert!(Journal::for_root(&dir, "next".into(), &root).is_err());
            assert_eq!(std::fs::read(old.active()).unwrap(), marker);
        }
        std::fs::remove_file(root.resolve("/proc/sys/kernel/random/boot_id")).unwrap();
        assert!(Journal::for_root(&dir, "next".into(), &root).is_err());
        assert_eq!(std::fs::read(old.active()).unwrap(), marker);
        std::fs::write(root.resolve("/proc/sys/kernel/random/boot_id"), BOOT_A).unwrap();
        let blocked = root.with_faults(std::collections::BTreeMap::from([(
            "/proc/sys/kernel/random/boot_id".into(),
            crate::sys::Fault::Unreadable,
        )]));
        assert!(Journal::for_root(&dir, "next".into(), &blocked).is_err());
        assert_eq!(std::fs::read(old.active()).unwrap(), marker);
    }

    #[test]
    fn corrupt_or_inconsistent_boot_metadata_is_not_replayable() {
        let (root, dir) = boot_fixture("corrupt-marker", BOOT_A);
        let old = Journal::for_root(&dir, "old".into(), &root).unwrap();
        old.begin().unwrap();
        let next = Journal::for_root(&dir, "next".into(), &root).unwrap();
        let valid = std::fs::read(old.active()).unwrap();
        for marker in [
            "old\n1\nboot_id=garbage\n".to_string(),
            format!("old\n0\nboot_id={BOOT_A}\n"),
            format!("old\n1\nboot_id={BOOT_A}\nextra\n"),
            "old\n1\n".to_string(),
        ] {
            std::fs::write(old.active(), &marker).unwrap();
            assert!(next.pending_recovery().is_err());
            assert_eq!(std::fs::read_to_string(old.active()).unwrap(), marker);
        }
        std::fs::write(old.active(), valid).unwrap();
        std::fs::write(dir.join("old.boot_id"), BOOT_B).unwrap();
        assert!(next.pending_recovery().is_err());
        std::fs::remove_file(dir.join("old.boot_id")).unwrap();
        assert!(next.pending_recovery().is_err());
    }

    #[test]
    fn previous_boot_archive_refuses_to_overwrite_conflicting_evidence() {
        let (root, dir) = boot_fixture("archive-conflict", BOOT_A);
        let old = Journal::for_root(&dir, "old".into(), &root).unwrap();
        old.begin().unwrap();
        std::fs::write(root.resolve("/proc/sys/kernel/random/boot_id"), BOOT_B).unwrap();
        let next = Journal::for_root(&dir, "next".into(), &root).unwrap();
        std::fs::write(dir.join("old.previous-boot"), "unrelated evidence").unwrap();
        assert!(next.archive_previous_boot("old").is_err());
        assert!(old.active().exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("old.previous-boot")).unwrap(),
            "unrelated evidence"
        );
        // An interrupted archive after its durable copy but before removing
        // active can finish idempotently once both marker bytes agree.
        std::fs::write(
            dir.join("old.previous-boot"),
            std::fs::read(old.active()).unwrap(),
        )
        .unwrap();
        next.archive_previous_boot("old").unwrap();
        assert!(!old.active().exists());
    }

    fn entry(session: &str, node: &str, old: &str, new: &str) -> JournalEntry {
        JournalEntry {
            ts_ms: 1,
            session: session.into(),
            node: node.into(),
            old: old.into(),
            new: new.into(),
            state: "FOREGROUND".into(),
            mode: "enabled".into(),
            result: "ok".into(),
        }
    }

    #[test]
    fn session_roundtrip_and_pending_detection() {
        let dir = std::env::temp_dir().join("neurosched-journal-test");
        let _ = std::fs::remove_dir_all(&dir);
        let j = Journal::new(&dir, "s1".into());
        j.begin().unwrap();
        j.append(&entry("s1", "/n", "0", "128")).unwrap();
        let (session, entries) = j.pending().expect("pending");
        assert_eq!(session, "s1");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].new, "128");
        j.end().unwrap();
        assert!(j.pending().is_none());
        assert_eq!(j.recent(5).len(), 1);
    }

    #[test]
    fn disabled_journal_writes_nothing() {
        let j = Journal::disabled();
        assert!(j.begin().is_ok());
        assert!(j.append(&entry("x", "/n", "a", "b")).is_ok());
        assert!(j.pending().is_none());
    }

    #[test]
    fn pending_rejects_corrupt_records_and_session_path_traversal() {
        let dir = std::env::temp_dir().join("neurosched-journal-corrupt");
        let _ = std::fs::remove_dir_all(&dir);
        let j = Journal::new(&dir, "s1".into());
        j.begin().unwrap();
        std::fs::write(j.file(), "broken\n").unwrap();
        assert!(j.pending_checked().is_err());
        std::fs::write(j.active(), "../escape\n1\n").unwrap();
        assert!(j.pending_checked().is_err());
    }

    #[test]
    fn incomplete_trailing_record_does_not_hide_durable_intent() {
        let dir = std::env::temp_dir().join("neurosched-journal-tail");
        let _ = std::fs::remove_dir_all(&dir);
        let j = Journal::new(&dir, "s1".into());
        j.begin().unwrap();
        j.append(&entry("s1", "/n", "0", "12.50")).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(j.file())
            .unwrap();
        file.write_all(b"{unfinished").unwrap();
        assert_eq!(j.pending_checked().unwrap().unwrap().1.len(), 1);
    }

    #[test]
    fn begin_refuses_session_reuse_without_replacing_active() {
        let dir = std::env::temp_dir().join("neurosched-journal-reuse");
        let _ = std::fs::remove_dir_all(&dir);
        let j = Journal::new(&dir, "s1".into());
        j.begin().unwrap();
        let active = std::fs::read(j.active()).unwrap();
        assert!(j.begin().is_err());
        assert_eq!(std::fs::read(j.active()).unwrap(), active);
    }

    #[test]
    fn rotation_keeps_newest() {
        let dir = std::env::temp_dir().join("neurosched-journal-rotate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            let j = Journal::new(&dir, format!("s{i}"));
            j.append(&entry(&format!("s{i}"), "/n", "0", "1")).unwrap();
        }
        Journal::new(&dir, "current".into()).rotate(2);
        let count = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .count();
        assert_eq!(count, 2);
    }
}
