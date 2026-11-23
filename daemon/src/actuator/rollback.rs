//! Replay of an unclosed journal session.
//!
//! Used both on startup (crash recovery) and by `--restore-all` (uninstall). The
//! critical rule: never overwrite a value that somebody else has since written.

use crate::safety::transaction::JournalEntry;
use crate::sys::SysRoot;

#[derive(Debug, Clone, Default)]
pub struct RollbackReport {
    pub restored: usize,
    pub failed: usize,
    pub skipped: usize,
    pub conflicts: Vec<String>,
    pub released_nodes: Vec<String>,
    pub notes: Vec<String>,
}

impl RollbackReport {
    pub fn to_json(&self) -> crate::json::Json {
        use crate::json::Json;
        let mut j = Json::obj();
        j.set("restored", Json::num(self.restored as f64));
        j.set("failed", Json::num(self.failed as f64));
        j.set("skipped", Json::num(self.skipped as f64));
        j.set(
            "conflicts",
            Json::Arr(self.conflicts.iter().cloned().map(Json::Str).collect()),
        );
        j.set(
            "notes",
            Json::Arr(self.notes.iter().cloned().map(Json::Str).collect()),
        );
        j
    }
}

/// Restores entries newest-first. Only a node whose current value still equals the
/// value we wrote is restored; anything else is reported as a conflict.
pub fn replay(root: &SysRoot, entries: &[JournalEntry]) -> RollbackReport {
    let mut report = RollbackReport::default();
    let mut blocked = std::collections::BTreeSet::new();
    let mut previous: Option<&JournalEntry> = None;
    for e in entries.iter().rev() {
        let duplicate = previous.is_some_and(|p| {
            (p.ts_ms, &p.session, &p.node, &p.old, &p.new)
                == (e.ts_ms, &e.session, &e.node, &e.old, &e.new)
        });
        previous = Some(e);
        if blocked.contains(&e.node) || duplicate {
            continue;
        }
        if !super::uclamp::is_allowed_node(&e.node) {
            report.failed += 1;
            report.notes.push(format!(
                "{}: recovery target is outside the allowlist",
                e.node
            ));
            blocked.insert(e.node.clone());
            continue;
        }
        if e.result == "settled" {
            blocked.insert(e.node.clone());
            continue;
        }
        if e.result == "cancelled" {
            continue;
        }
        if super::uclamp::percentage(&e.old).is_none()
            || super::uclamp::percentage(&e.new).is_none()
        {
            report.failed += 1;
            report
                .notes
                .push(format!("{}: invalid recovery percentage", e.node));
            blocked.insert(e.node.clone());
            continue;
        }
        if e.old == e.new {
            report.skipped += 1;
            continue;
        }
        let current = match root.read_trim(&e.node) {
            Some(v) => v,
            None => {
                report.failed += 1;
                blocked.insert(e.node.clone());
                report.skipped += 1;
                report
                    .notes
                    .push(format!("{}: unreadable, skipped", e.node));
                continue;
            }
        };
        if !super::uclamp::same_value(&e.node, &current, &e.new) {
            report.skipped += 1;
            blocked.insert(e.node.clone());
            report.released_nodes.push(e.node.clone());
            report.conflicts.push(format!(
                "{}: current={} != ours={} (left alone)",
                e.node, current, e.new
            ));
            continue;
        }
        match root.write(&e.node, &e.old) {
            Ok(()) => {
                let verified = root.read_trim(&e.node).unwrap_or_default();
                if super::uclamp::same_value(&e.node, &verified, &e.old) {
                    report.restored += 1;
                } else {
                    report.failed += 1;
                    blocked.insert(e.node.clone());
                    report.skipped += 1;
                    report.conflicts.push(format!(
                        "{}: restore not honoured (wanted {}, got {})",
                        e.node, e.old, verified
                    ));
                }
            }
            Err(err) => {
                report.failed += 1;
                blocked.insert(e.node.clone());
                report.skipped += 1;
                report
                    .notes
                    .push(format!("{}: write failed: {err}", e.node));
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixture(name: &str, value: &str) -> (std::path::PathBuf, SysRoot) {
        let dir = std::env::temp_dir().join(format!("neurosched-rb-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sys/fs/cgroup/top-app")).unwrap();
        std::fs::write(dir.join("sys/fs/cgroup/top-app/cpu.uclamp.max"), value).unwrap();
        (dir.clone(), SysRoot::at(&dir))
    }

    fn entry(node: &str, old: &str, new: &str) -> JournalEntry {
        JournalEntry {
            ts_ms: 1,
            session: "s".into(),
            node: node.into(),
            old: old.into(),
            new: new.into(),
            state: "GAME".into(),
            mode: "enabled".into(),
            result: "ok".into(),
        }
    }

    #[test]
    fn restores_when_value_is_still_ours() {
        let (_d, root) = fixture("ok", "62.50");
        let node = "/sys/fs/cgroup/top-app/cpu.uclamp.max";
        let r = replay(&root, &[entry(node, "100", "62.50")]);
        assert_eq!(r.restored, 1);
        assert_eq!(root.read_trim(node).unwrap(), "100");
    }

    #[test]
    fn refuses_to_clobber_external_value() {
        let (_d, root) = fixture("conflict", "50.00");
        let node = "/sys/fs/cgroup/top-app/cpu.uclamp.max";
        let r = replay(&root, &[entry(node, "100", "62.50")]);
        assert_eq!(r.restored, 0);
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(root.read_trim(node).unwrap(), "50.00");
    }

    #[test]
    fn duplicate_entries_are_idempotent() {
        let (_d, root) = fixture("dup", "62.50");
        let node = "/sys/fs/cgroup/top-app/cpu.uclamp.max";
        let entries = vec![entry(node, "100", "62.50"), entry(node, "100", "62.50")];
        let r = replay(&root, &entries);
        assert_eq!(r.restored, 1);
        assert_eq!(root.read_trim(node).unwrap(), "100");
    }

    #[test]
    fn conflict_blocks_older_matching_values_on_the_same_node() {
        let (_d, root) = fixture("aba-conflict", "62.50");
        let node = "/sys/fs/cgroup/top-app/cpu.uclamp.max";
        let mut latest = entry(node, "62.50", "100");
        latest.ts_ms = 2;
        let r = replay(&root, &[entry(node, "100", "62.50"), latest]);
        assert_eq!(r.restored, 0);
        assert_eq!(root.read_trim(node).unwrap(), "62.50");
        assert_eq!(r.released_nodes, [node]);
    }

    #[test]
    fn pending_and_completion_are_one_operation_in_a_multi_step_chain() {
        let (_d, root) = fixture("chain", "50.00");
        let node = "/sys/fs/cgroup/top-app/cpu.uclamp.max";
        let mut first = entry(node, "100", "62.50");
        first.result = "pending".into();
        let mut first_ok = first.clone();
        first_ok.result = "ok".into();
        let mut second = entry(node, "62.50", "50.00");
        second.ts_ms = 2;
        second.result = "pending".into();
        let mut second_ok = second.clone();
        second_ok.result = "ok".into();
        let r = replay(&root, &[first, first_ok, second, second_ok]);
        assert_eq!(r.restored, 2);
        assert!(r.conflicts.is_empty());
        assert_eq!(root.read_trim(node).unwrap(), "100");
    }

    #[test]
    fn settled_generation_is_skipped_but_new_writes_are_restored() {
        let (_d, root) = fixture("settled", "50.00");
        let node = "/sys/fs/cgroup/top-app/cpu.uclamp.max";
        let mut settled = entry(node, "", "");
        settled.ts_ms = 2;
        settled.result = "settled".into();
        let mut next = entry(node, "62.50", "50.00");
        next.ts_ms = 3;
        let r = replay(&root, &[entry(node, "100", "62.50"), settled, next]);
        assert_eq!(r.restored, 1);
        assert_eq!(root.read_trim(node).unwrap(), "62.50");
    }

    #[test]
    fn cancelled_attempt_does_not_hide_an_earlier_successful_write() {
        let (_d, root) = fixture("cancelled", "62.50");
        let node = "/sys/fs/cgroup/top-app/cpu.uclamp.max";
        let mut pending = entry(node, "62.50", "50.00");
        pending.ts_ms = 2;
        pending.result = "pending".into();
        let mut cancelled = pending.clone();
        cancelled.result = "cancelled".into();
        let r = replay(&root, &[entry(node, "100", "62.50"), pending, cancelled]);
        assert_eq!(r.restored, 1);
        assert_eq!(root.read_trim(node).unwrap(), "100");
    }

    #[test]
    fn recovery_refuses_forbidden_paths_and_invalid_originals() {
        let (_d, root) = fixture("invalid", "62.50");
        let node = "/sys/fs/cgroup/top-app/cpu.uclamp.max";
        let r = replay(
            &root,
            &[
                entry("/proc/sys/vm/swappiness", "100", "62.50"),
                entry(node, "1024", "62.50"),
            ],
        );
        assert_eq!(r.failed, 2);
        assert!(root.recorded_writes().is_empty());
    }

    #[test]
    fn write_failure_is_reported_not_panicked() {
        let (_d, root) = fixture("fail", "62.50");
        let mut faults = BTreeMap::new();
        faults.insert(
            "/sys/fs/cgroup/top-app/cpu.uclamp.max".to_string(),
            crate::sys::Fault::WriteFail,
        );
        let root = root.with_faults(faults);
        let r = replay(
            &root,
            &[entry(
                "/sys/fs/cgroup/top-app/cpu.uclamp.max",
                "100",
                "62.50",
            )],
        );
        assert_eq!(r.restored, 0);
        assert!(!r.notes.is_empty());
    }
}
