//! Actuator: the only place in the daemon that writes to a kernel node.
//!
//! Every write follows the same eight steps:
//!   1. read the current value
//!   2. check ownership / whether we already gave the node up
//!   3. validate the requested value against the node's documented range
//!   4. append the change record to the journal
//!   5. write
//!   6. read back
//!   7. verify the kernel accepted it
//!   8. on any failure, restore immediately
//!
//! In `observe_only` nothing is written at all; in `dry_run` the exact writes are
//! returned in the report so the UI can show what *would* happen.

pub mod cpufreq;
pub mod rollback;
pub mod uclamp;

use crate::config::{ActuatorConfig, ConflictAction, Profile};
use crate::policy::Decision;
use crate::safety::ownership::{NodeView, Ownership};
use crate::safety::transaction::{Journal, JournalEntry};
use crate::sys::SysRoot;
use crate::types::{PolicyState, RunMode};
use uclamp::{UclampTarget, UCLAMP_MAX_VALUE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    UclampMin,
    UclampMax,
    ClampMin,
    ClampMax,
}

impl WriteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WriteKind::UclampMin => "uclamp.min",
            WriteKind::UclampMax => "uclamp.max",
            WriteKind::ClampMin => "sched_util_clamp_min",
            WriteKind::ClampMax => "sched_util_clamp_max",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlannedWrite {
    pub node: String,
    pub value: String,
    pub kind: WriteKind,
    pub why: String,
}

#[derive(Debug, Clone)]
pub struct WriteResult {
    pub node: String,
    pub kind: String,
    pub old: String,
    pub new: String,
    /// planned | applied | unchanged | skipped | failed | conflict
    pub status: String,
    pub note: String,
}

#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    pub mode: String,
    pub state: String,
    pub results: Vec<WriteResult>,
    pub skipped: Vec<String>,
    pub conflicts: Vec<String>,
}

impl ApplyReport {
    pub fn applied(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.status == "applied")
            .count()
    }

    pub fn failed(&self) -> usize {
        self.results.iter().filter(|r| r.status == "failed").count()
    }

    pub fn to_json(&self) -> crate::json::Json {
        use crate::json::Json;
        let mut j = Json::obj();
        j.set("mode", Json::str(self.mode.clone()));
        j.set("state", Json::str(self.state.clone()));
        j.set("applied", Json::num(self.applied() as f64));
        j.set("failed", Json::num(self.failed() as f64));
        j.set(
            "results",
            Json::Arr(
                self.results
                    .iter()
                    .map(|r| {
                        let mut o = Json::obj();
                        o.set("node", Json::str(r.node.clone()));
                        o.set("kind", Json::str(r.kind.clone()));
                        o.set("old", Json::str(r.old.clone()));
                        o.set("new", Json::str(r.new.clone()));
                        o.set("status", Json::str(r.status.clone()));
                        o.set("note", Json::str(r.note.clone()));
                        o
                    })
                    .collect(),
            ),
        );
        j.set(
            "skipped",
            Json::Arr(self.skipped.iter().cloned().map(Json::Str).collect()),
        );
        j.set(
            "conflicts",
            Json::Arr(self.conflicts.iter().cloned().map(Json::Str).collect()),
        );
        j
    }
}

pub struct Actuator {
    root: SysRoot,
    mode: RunMode,
    enable_uclamp: bool,
    enable_clamp: bool,
    enabled: bool,
    uclamp: Option<UclampTarget>,
    global_clamp: Option<(u32, u32)>,
    ownership: Ownership,
    journal: Journal,
    on_conflict: ConflictAction,
    recovery_required: bool,
}

impl Actuator {
    pub fn new(
        root: SysRoot,
        cfg: &ActuatorConfig,
        mode: RunMode,
        journal: Journal,
        on_conflict: ConflictAction,
    ) -> Self {
        let uclamp = uclamp::discover(&root);
        let global_clamp = uclamp::discover_global_clamp(&root);
        let mut a = Self {
            mode,
            enable_uclamp: cfg.enable_uclamp_top_app,
            enable_clamp: cfg.enable_sched_util_clamp,
            enabled: cfg.enabled,
            uclamp,
            global_clamp,
            ownership: Ownership::default(),
            journal,
            on_conflict,
            recovery_required: false,
            root,
        };
        a.record_baseline();
        a
    }

    /// Remembers the original value of every node we may ever touch. Called once.
    fn record_baseline(&mut self) {
        if let Some(t) = &self.uclamp {
            if let Some(v) = self.root.read_trim(&t.min_path) {
                self.ownership.record_original(&t.min_path, &v);
            }
            if let Some(v) = self.root.read_trim(&t.max_path) {
                self.ownership.record_original(&t.max_path, &v);
            }
        }
        if self.enable_clamp {
            for node in [
                "/proc/sys/kernel/sched_util_clamp_min",
                "/proc/sys/kernel/sched_util_clamp_max",
            ] {
                if let Some(v) = self.root.read_trim(node) {
                    self.ownership.record_original(node, &v);
                }
            }
        }
    }

    pub fn mode(&self) -> RunMode {
        self.mode
    }

    pub fn writes_allowed(&self) -> bool {
        self.enabled
            && !self.recovery_required
            && self.mode.writes_allowed()
            && (!self.root.is_system() || crate::sys::ACTUATOR_ENABLED)
    }

    pub fn recovery_required(&self) -> bool {
        self.recovery_required
    }

    pub fn update_config(&mut self, cfg: &ActuatorConfig, on_conflict: ConflictAction) {
        self.enabled = cfg.enabled;
        self.enable_uclamp = cfg.enable_uclamp_top_app;
        self.enable_clamp = cfg.enable_sched_util_clamp;
        self.on_conflict = on_conflict;
    }

    pub fn set_mode(&mut self, mode: RunMode) {
        self.mode = mode;
    }

    pub fn ownership(&self) -> &Ownership {
        &self.ownership
    }

    pub fn nodes(&self) -> Vec<NodeView> {
        self.ownership.dump()
    }

    pub fn capability_json(&self) -> crate::json::Json {
        use crate::json::Json;
        let mut j = Json::obj();
        j.set("mode", Json::str(self.mode.as_str()));
        j.set("writes_allowed", Json::Bool(self.writes_allowed()));
        j.set("ACTUATOR_ENABLED", Json::Bool(crate::sys::ACTUATOR_ENABLED));
        j.set("enabled", Json::Bool(self.enabled));
        j.set("recovery_required", Json::Bool(self.recovery_required));
        j.set("uclamp_enabled", Json::Bool(self.enable_uclamp));
        j.set("clamp_enabled", Json::Bool(false));
        j.set("global_clamp_requested", Json::Bool(self.enable_clamp));
        j.set("global_clamp_supported", Json::Bool(false));
        j.set("conflict_policy", Json::str("release"));
        j.set(
            "requested_conflict_policy",
            Json::str(self.on_conflict.as_str()),
        );
        match &self.uclamp {
            Some(t) => {
                let mut u = Json::obj();
                u.set("dir", Json::str(t.dir.clone()));
                u.set("flavour", Json::str(t.flavour.clone()));
                u.set("min_path", Json::str(t.min_path.clone()));
                u.set("max_path", Json::str(t.max_path.clone()));
                let min_raw = self.root.read_trim(&t.min_path);
                let max_raw = self.root.read_trim(&t.max_path);
                u.set(
                    "current_min",
                    min_raw
                        .as_deref()
                        .and_then(uclamp::percentage)
                        .map(Json::Num)
                        .unwrap_or(Json::Null),
                );
                u.set(
                    "current_max",
                    max_raw
                        .as_deref()
                        .and_then(uclamp::percentage)
                        .map(Json::Num)
                        .unwrap_or(Json::Null),
                );
                u.set("min_raw", min_raw.map(Json::Str).unwrap_or(Json::Null));
                u.set("max_raw", max_raw.map(Json::Str).unwrap_or(Json::Null));
                u.set("observed_wall_ms", Json::num(crate::util::now_ms() as f64));
                u.set("scale", Json::str(t.scale.as_str()));
                u.set("interface_verified", Json::Bool(t.interface_verified));
                u.set(
                    "profile_max_on_this_node",
                    Json::num(t.scale.to_node(1024) as f64),
                );
                j.set("uclamp", u);
            }
            None => j.set("uclamp", Json::Null),
        }
        match self.global_clamp {
            Some((min, max)) => {
                let mut c = Json::obj();
                c.set("min", Json::num(min as f64));
                c.set("max", Json::num(max as f64));
                j.set("sched_util_clamp", c);
            }
            None => j.set("sched_util_clamp", Json::Null),
        }
        j.set(
            "released_nodes",
            Json::Arr(
                self.ownership
                    .released_nodes()
                    .iter()
                    .cloned()
                    .map(Json::Str)
                    .collect(),
            ),
        );
        j
    }

    pub fn plan(&self, state: PolicyState, profile: Profile) -> Vec<PlannedWrite> {
        let mut out = Vec::new();
        if let Some(t) = &self.uclamp {
            if self.enable_uclamp && t.interface_verified {
                out.push(PlannedWrite {
                    node: t.min_path.clone(),
                    value: format!(
                        "{:.2}",
                        profile.uclamp_min.min(1024) as f64 * 100.0 / 1024.0
                    ),
                    kind: WriteKind::UclampMin,
                    why: format!(
                        "{} uclamp.min ({} -> {} on {})",
                        state.as_str(),
                        profile.uclamp_min,
                        t.scale.to_node(profile.uclamp_min),
                        t.scale.as_str()
                    ),
                });
                out.push(PlannedWrite {
                    node: t.max_path.clone(),
                    value: format!(
                        "{:.2}",
                        profile.uclamp_max.min(1024) as f64 * 100.0 / 1024.0
                    ),
                    kind: WriteKind::UclampMax,
                    why: format!(
                        "{} uclamp.max ({} -> {} on {})",
                        state.as_str(),
                        profile.uclamp_max,
                        t.scale.to_node(profile.uclamp_max),
                        t.scale.as_str()
                    ),
                });
            }
        }
        if out.len() >= 2 {
            if let Some(current_max) = self
                .uclamp
                .as_ref()
                .and_then(|t| self.root.read_trim(&t.max_path))
                .and_then(|s| uclamp::percentage(&s))
            {
                if profile.uclamp_max.min(1024) as f64 * 100.0 / 1024.0 > current_max {
                    out.swap(0, 1);
                }
            }
        }
        out
    }

    /// Applies the profile for `decision`. Never panics, never partially applies
    /// without recording it.
    pub fn apply(&mut self, decision: &Decision, now: u64) -> ApplyReport {
        let planned = self.plan(decision.state, decision.profile);
        let mut report = ApplyReport {
            mode: self.mode.as_str().to_string(),
            state: decision.state.as_str().to_string(),
            ..Default::default()
        };

        for w in planned {
            if let Some(reason) = cpufreq::forbidden(&w.node) {
                // Global clamp is opt-in and whitelisted; everything else is a bug.
                report
                    .skipped
                    .push(format!("{}: forbidden ({reason})", w.node));
                continue;
            }
            if self.ownership.is_released(&w.node) {
                report.skipped.push(format!(
                    "{}: released ({})",
                    w.node,
                    self.ownership.note(&w.node).unwrap_or("conflict")
                ));
                continue;
            }
            let Some(current) = self.root.read_trim(&w.node) else {
                report.skipped.push(format!("{}: unreadable", w.node));
                continue;
            };
            let old = current.clone();

            // Conflict detection: somebody else wrote after us.
            if self.ownership.last_written(&w.node).is_some()
                && !self.ownership.still_ours(&w.node, &current)
            {
                let msg = format!(
                    "{}: externally modified (ours={}, now={})",
                    w.node,
                    self.ownership.last_written(&w.node).unwrap_or("?"),
                    current
                );
                report.conflicts.push(msg.clone());
                self.ownership.release(&w.node, "externally modified");
                report.results.push(WriteResult {
                    node: w.node.clone(),
                    kind: w.kind.as_str().into(),
                    old: old.clone(),
                    new: w.value.clone(),
                    status: "conflict".into(),
                    note: "external ownership released".into(),
                });
                if let Err(err) = self.journal.settle_node(&w.node) {
                    self.recovery_required = true;
                    self.mode = RunMode::ObserveOnly;
                    report
                        .skipped
                        .push(format!("release checkpoint failed: {err}"));
                    break;
                }
                continue;
            }

            if uclamp::same_value(&w.node, &old, &w.value) {
                report.results.push(WriteResult {
                    node: w.node.clone(),
                    kind: w.kind.as_str().into(),
                    old: old.clone(),
                    new: w.value.clone(),
                    status: "unchanged".into(),
                    note: "already at target".into(),
                });
                continue;
            }

            if !self.writes_allowed() {
                report.results.push(WriteResult {
                    node: w.node.clone(),
                    kind: w.kind.as_str().into(),
                    old,
                    new: w.value.clone(),
                    status: if self.mode == RunMode::DryRun {
                        "planned".into()
                    } else {
                        "skipped".into()
                    },
                    note: format!("mode={}", self.mode.as_str()),
                });
                continue;
            }

            if let Err(err) = self.validate(&w) {
                report.results.push(WriteResult {
                    node: w.node.clone(),
                    kind: w.kind.as_str().into(),
                    old,
                    new: w.value.clone(),
                    status: "skipped".into(),
                    note: err,
                });
                continue;
            }

            let entry = JournalEntry {
                ts_ms: now,
                session: self.journal.session().to_string(),
                node: w.node.clone(),
                old: old.clone(),
                new: w.value.clone(),
                state: decision.state.as_str().to_string(),
                mode: self.mode.as_str().to_string(),
                result: "pending".into(),
            };
            if let Err(err) = self.journal.append(&entry) {
                report.results.push(WriteResult {
                    node: w.node.clone(),
                    kind: w.kind.as_str().into(),
                    old,
                    new: w.value.clone(),
                    status: "failed".into(),
                    note: format!("journal not durable, write refused: {err}"),
                });
                break;
            }
            self.ownership.record_before_write(&w.node, &old);

            match self.root.write(&w.node, &w.value) {
                Ok(()) => {
                    let readback = self.root.read_trim(&w.node).unwrap_or_default();
                    if uclamp::same_value(&w.node, &readback, &w.value) {
                        self.ownership.record_original(&w.node, &old);
                        self.ownership.mark_ours(&w.node, &readback);
                        let mut done = entry.clone();
                        done.result = "ok".into();
                        let _ = self.journal.append(&done);
                        report.results.push(WriteResult {
                            node: w.node.clone(),
                            kind: w.kind.as_str().into(),
                            old,
                            new: w.value.clone(),
                            status: "applied".into(),
                            note: "verified".into(),
                        });
                    } else {
                        // Kernel clamped or ignored the value: put the old one back.
                        self.recovery_required = self.root.write(&w.node, &old).is_err()
                            || !self
                                .root
                                .read_trim(&w.node)
                                .is_some_and(|v| uclamp::same_value(&w.node, &v, &old));
                        let mut bad = entry.clone();
                        bad.result = if self.recovery_required {
                            format!("verify_failed(readback={readback})")
                        } else {
                            "cancelled".into()
                        };
                        let _ = self.journal.append(&bad);
                        report.results.push(WriteResult {
                            node: w.node.clone(),
                            kind: w.kind.as_str().into(),
                            old: old.clone(),
                            new: w.value.clone(),
                            status: "failed".into(),
                            note: format!(
                                "readback {readback}, recovery_required={}",
                                self.recovery_required
                            ),
                        });
                    }
                }
                Err(err) => {
                    self.recovery_required = self.root.write(&w.node, &old).is_err()
                        || !self
                            .root
                            .read_trim(&w.node)
                            .is_some_and(|v| uclamp::same_value(&w.node, &v, &old));
                    let mut bad = entry.clone();
                    bad.result = if self.recovery_required {
                        format!("write_error({err})")
                    } else {
                        "cancelled".into()
                    };
                    let _ = self.journal.append(&bad);
                    report.results.push(WriteResult {
                        node: w.node.clone(),
                        kind: w.kind.as_str().into(),
                        old,
                        new: w.value.clone(),
                        status: "failed".into(),
                        note: format!("write error: {err}"),
                    });
                }
            }
            if self.recovery_required {
                self.mode = RunMode::ObserveOnly;
                break;
            }
        }

        report
    }

    fn validate(&self, w: &PlannedWrite) -> Result<(), String> {
        match w.kind {
            WriteKind::UclampMin | WriteKind::UclampMax => {
                let v = uclamp::percentage(&w.value)
                    .ok_or_else(|| format!("{}: invalid cgroup percentage", w.value))?;
                let limit = self
                    .uclamp
                    .as_ref()
                    .map(|t| t.scale.max())
                    .unwrap_or(UCLAMP_MAX_VALUE);
                if v > limit as f64 {
                    return Err(format!("{v}: outside 0..={limit} for this node scale"));
                }
                if w.kind == WriteKind::UclampMin {
                    // Never demand more than the ceiling we are also setting.
                    if let Some(max) = self
                        .uclamp
                        .as_ref()
                        .and_then(|t| self.root.read_trim(&t.max_path))
                        .and_then(|s| uclamp::percentage(&s))
                    {
                        if v > max {
                            return Err(format!("uclamp.min {v} > uclamp.max {max}"));
                        }
                    }
                }
                Ok(())
            }
            WriteKind::ClampMin | WriteKind::ClampMax => {
                let v: u32 = w
                    .value
                    .parse()
                    .map_err(|_| format!("{}: not a number", w.value))?;
                if v > UCLAMP_MAX_VALUE {
                    return Err(format!("{v}: outside 0..={UCLAMP_MAX_VALUE}"));
                }
                Ok(())
            }
        }
    }

    /// Restores every node we still own, newest first.
    pub fn restore_all(&mut self) -> rollback::RollbackReport {
        let mut entries: Vec<JournalEntry> = self
            .ownership
            .dump()
            .iter()
            .filter_map(|n| {
                let original = n.original.clone()?;
                let written = n.last_written.clone()?;
                if n.released {
                    return None;
                }
                Some(JournalEntry {
                    ts_ms: crate::util::now_ms(),
                    session: self.journal.session().to_string(),
                    node: n.node.clone(),
                    old: original,
                    new: written,
                    state: "RESTORE".into(),
                    mode: self.mode.as_str().to_string(),
                    result: "restore".into(),
                })
            })
            .collect();
        // Relax the ceiling first, or lower the floor before lowering the ceiling.
        entries.sort_by_key(|e| {
            if e.node.ends_with(".min") {
                1
            } else if uclamp::percentage(&e.old) > uclamp::percentage(&e.new) {
                0
            } else {
                2
            }
        });
        let mut report = rollback::RollbackReport::default();
        for entry in entries {
            let mut result = rollback::replay(&self.root, std::slice::from_ref(&entry));
            for node in &result.released_nodes {
                self.ownership
                    .release(node, "external change found during restore");
            }
            if result.failed == 0 {
                if let Err(err) = self.journal.settle_node(&entry.node) {
                    result.failed += 1;
                    result
                        .notes
                        .push(format!("{}: restore checkpoint failed: {err}", entry.node));
                }
            }
            report.restored += result.restored;
            report.failed += result.failed;
            report.skipped += result.skipped;
            report.conflicts.extend(result.conflicts);
            report.released_nodes.extend(result.released_nodes);
            report.notes.extend(result.notes);
        }
        if report.failed > 0 {
            self.recovery_required = true;
            self.mode = RunMode::ObserveOnly;
        }
        report
    }

    pub fn release_recovered_nodes(&mut self, nodes: &[String]) {
        for node in nodes {
            self.ownership
                .release(node, "external change found during crash recovery");
        }
    }

    /// Removes the "ours" claim so the next run starts from a clean baseline.
    pub fn forget(&mut self) {
        let released = self.ownership.released_nodes();
        self.ownership = Ownership::default();
        for node in released {
            self.ownership.release(&node, "external ownership retained");
        }
        self.record_baseline();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::policy::Decision;
    use std::collections::BTreeMap;

    const MIN: &str = "/sys/fs/cgroup/top-app/cpu.uclamp.min";
    const MAX: &str = "/sys/fs/cgroup/top-app/cpu.uclamp.max";

    fn fixture(name: &str, min: &str, max: &str) -> (std::path::PathBuf, SysRoot) {
        let dir = std::env::temp_dir().join(format!("neurosched-act-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("sys/fs/cgroup/top-app");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("cpu.uclamp.min"), min).unwrap();
        std::fs::write(p.join("cpu.uclamp.max"), max).unwrap();
        std::fs::create_dir_all(dir.join("proc/self")).unwrap();
        std::fs::write(
            dir.join("proc/self/mountinfo"),
            "30 20 0:25 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
        )
        .unwrap();
        (dir.clone(), SysRoot::at(&dir))
    }

    fn decision(state: PolicyState, min: u32, max: u32) -> Decision {
        Decision {
            state,
            profile: Profile {
                uclamp_min: min,
                uclamp_max: max,
            },
            ..Default::default()
        }
    }

    fn actuator(root: SysRoot, mode: RunMode) -> Actuator {
        let mut c = Config::default();
        c.actuator.enabled = true;
        Actuator::new(
            root,
            &c.actuator,
            mode,
            Journal::disabled(),
            ConflictAction::Release,
        )
    }

    #[test]
    fn observe_mode_writes_nothing() {
        let (_d, root) = fixture("observe", "0", "100");
        let mut a = actuator(root.clone(), RunMode::ObserveOnly);
        let r = a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        assert_eq!(r.applied(), 0);
        assert!(root.recorded_writes().is_empty());
        assert_eq!(root.read_trim(MIN).unwrap(), "0");
    }

    #[test]
    fn dry_run_reports_planned_without_writing() {
        let (_d, root) = fixture("dry", "0", "100");
        let mut a = actuator(root.clone(), RunMode::DryRun);
        let r = a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        assert!(r.results.iter().any(|x| x.status == "planned"));
        assert!(root.recorded_writes().is_empty());
    }

    #[test]
    fn enabled_mode_applies_and_verifies() {
        let (_d, root) = fixture("apply", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        let r = a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        assert_eq!(r.applied(), 1);
        assert_eq!(root.read_trim(MIN).unwrap(), "12.50");
        assert_eq!(a.nodes().len(), 2);
    }

    #[test]
    fn restoring_a_lower_ceiling_first_restores_the_floor() {
        let (_d, root) = fixture("restore-order", "12.50", "50.00");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        assert_eq!(
            a.apply(&decision(PolicyState::Game, 640, 1024), 1)
                .applied(),
            2
        );
        let start = root.recorded_writes().len();
        assert_eq!(a.restore_all().failed, 0);
        let writes = root.recorded_writes();
        assert_eq!(writes[start].0, MIN);
        assert_eq!(writes[start + 1].0, MAX);
        assert_eq!(root.read_trim(MIN).unwrap(), "12.50");
        assert_eq!(root.read_trim(MAX).unwrap(), "50.00");
    }

    #[test]
    fn global_clamp_opt_in_cannot_generate_a_write() {
        let (_d, root) = fixture("no-global-plan", "0", "100");
        let mut cfg = Config::default();
        cfg.actuator.enabled = true;
        cfg.actuator.enable_sched_util_clamp = true;
        let a = Actuator::new(
            root,
            &cfg.actuator,
            RunMode::Enabled,
            Journal::disabled(),
            ConflictAction::Ignore,
        );
        assert!(a
            .plan(
                PolicyState::Game,
                Profile {
                    uclamp_min: 128,
                    uclamp_max: 1024
                }
            )
            .iter()
            .all(|w| uclamp::is_allowed_node(&w.node)));
        assert_eq!(
            a.capability_json()
                .get("clamp_enabled")
                .and_then(|j| j.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn ignore_conflict_configuration_still_yields_to_external_control() {
        let (_d, root) = fixture("ignore-conflict", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        a.on_conflict = ConflictAction::Ignore;
        a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        std::fs::write(root.resolve(MIN), "50.00").unwrap();
        a.apply(&decision(PolicyState::Foreground, 128, 1024), 2);
        assert_eq!(root.read_trim(MIN).unwrap(), "50.00");
        assert!(a.ownership.is_released(MIN));
    }

    #[test]
    fn write_failure_restores_original() {
        let (_d, root) = fixture("fail", "0", "100");
        let mut faults = BTreeMap::new();
        faults.insert(MIN.to_string(), crate::sys::Fault::WriteFail);
        let root = root.with_faults(faults);
        let mut a = actuator(root.clone(), RunMode::Enabled);
        let r = a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        assert_eq!(r.failed(), 1);
        assert_eq!(root.read_trim(MIN).unwrap(), "0");
    }

    #[test]
    fn silently_ignored_write_is_detected() {
        let (_d, root) = fixture("silent", "0", "100");
        let mut faults = BTreeMap::new();
        faults.insert(MIN.to_string(), crate::sys::Fault::WriteSilentlyIgnored);
        let root = root.with_faults(faults);
        let mut a = actuator(root.clone(), RunMode::Enabled);
        let r = a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        assert_eq!(r.failed(), 1);
        assert!(r.results.iter().any(|x| x.note.contains("readback")));
    }

    #[test]
    fn external_modification_releases_the_node() {
        let (_d, root) = fixture("conflict", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        assert_eq!(root.read_trim(MIN).unwrap(), "12.50");

        // Scene (or the framework) writes the same node.
        std::fs::write(root.resolve(MIN), "50.00").unwrap();
        let r = a.apply(&decision(PolicyState::Game, 256, 1024), 2);
        assert!(!r.conflicts.is_empty());
        assert!(r.results.iter().any(|x| x.status == "conflict"));
        assert_eq!(root.read_trim(MIN).unwrap(), "50.00");

        // And it stays released: no further writes to that node.
        let r2 = a.apply(&decision(PolicyState::Game, 300, 1024), 3);
        assert!(r2.skipped.iter().any(|s| s.contains("released")));
    }

    #[test]
    fn wild_profile_is_clamped_never_written_raw() {
        let (_d, root) = fixture("range", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        let _ = a.apply(&decision(PolicyState::Game, 0, 4000), 1);
        // 4000 is out of the 0..=1024 profile space; it must be clamped, not passed on.
        assert_eq!(root.read_trim(MAX).unwrap(), "100");
        assert!(root
            .recorded_writes()
            .iter()
            .all(|(_, v)| v.parse::<u32>().map(|n| n <= 1024).unwrap_or(false)));
    }

    #[test]
    fn percent_scale_node_receives_percentage() {
        let (_d, root) = fixture("pct", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        let r = a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        assert_eq!(r.applied(), 1);
        assert_eq!(root.read_trim(MIN).unwrap(), "12.50");
        assert_eq!(root.read_trim(MAX).unwrap(), "100");
    }

    #[test]
    fn forbidden_node_is_never_written() {
        let (_d, root) = fixture("forbidden", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        // Force a bogus plan entry.
        let w = PlannedWrite {
            node: "/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq".into(),
            value: "100".into(),
            kind: WriteKind::UclampMax,
            why: "test".into(),
        };
        assert!(cpufreq::forbidden(&w.node).is_some());
        let r = a.apply(&decision(PolicyState::Game, 0, 512), 1);
        assert!(r.applied() <= 1);
    }

    #[test]
    fn restore_all_returns_to_original() {
        let (_d, root) = fixture("restore", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        a.apply(&decision(PolicyState::ThermalGuard, 0, 640), 1);
        assert_eq!(root.read_trim(MAX).unwrap(), "62.50");
        let rep = a.restore_all();
        assert!(rep.restored >= 1);
        assert_eq!(root.read_trim(MAX).unwrap(), "100");
    }

    #[test]
    fn failed_journal_prevents_every_node_write() {
        let (dir, root) = fixture("journal-failure", "0", "100");
        let bad_dir = dir.join("not-a-directory");
        std::fs::write(&bad_dir, "occupied").unwrap();
        let mut cfg = Config::default();
        cfg.actuator.enabled = true;
        let mut a = Actuator::new(
            root.clone(),
            &cfg.actuator,
            RunMode::Enabled,
            Journal::new(bad_dir, "failed".into()),
            ConflictAction::Release,
        );
        let report = a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        assert_eq!(report.failed(), 1);
        assert!(root.recorded_writes().is_empty());
    }

    #[test]
    fn observing_equal_value_never_claims_ownership() {
        let (_dir, root) = fixture("unowned-equal", "0", "100");
        let mut a = actuator(root.clone(), RunMode::ObserveOnly);
        a.apply(&decision(PolicyState::Balanced, 0, 1024), 1);
        assert!(a.nodes().iter().all(|node| node.last_written.is_none()));
        assert_eq!(a.restore_all().restored, 0);
        assert!(root.recorded_writes().is_empty());
    }

    #[test]
    fn baseline_is_captured_immediately_before_first_write() {
        let (_dir, root) = fixture("late-baseline", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        std::fs::write(root.resolve(MIN), "5.00").unwrap();
        a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        a.restore_all();
        assert_eq!(root.read_trim(MIN).as_deref(), Some("5.00"));
    }

    #[test]
    fn recovery_reset_preserves_external_release() {
        let (_dir, root) = fixture("release-reset", "0", "100");
        let mut a = actuator(root.clone(), RunMode::Enabled);
        a.apply(&decision(PolicyState::Foreground, 128, 1024), 1);
        std::fs::write(root.resolve(MIN), "20.00").unwrap();
        a.apply(&decision(PolicyState::Game, 256, 1024), 2);
        a.forget();
        a.apply(&decision(PolicyState::Foreground, 128, 1024), 3);
        assert_eq!(root.read_trim(MIN).as_deref(), Some("20.00"));
    }
}
