//! Runtime: wires collectors, learner, policy, actuator and safety together and
//! publishes a single consistent view for the API and the CLI.
//!
//! The loop is deliberately boring: sample, decide, apply, publish, sleep. All the
//! interesting behaviour lives in the layers below, where it can be tested without a
//! device.

use crate::actuator::{rollback, Actuator};
use crate::collector::{process, Collectors};
use crate::config::Config;
use crate::json::Json;
use crate::policy::{Decision, Engine};
use crate::safety::ownership::NodeView;
use crate::safety::transaction::{Journal, JournalEntry, PendingRecovery};
use crate::safety::watchdog::{Watchdog, WatchdogStatus};
use crate::safety::SafetyView;
use crate::storage::{HistoryPoint, Level, Logger, Paths, Telemetry};
use crate::sys::SysRoot;
use crate::types::*;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const LEARNER_PERSIST_INTERVAL_MS: u64 = 300_000;

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    pub model: String,
    pub device: String,
    pub fingerprint: String,
    pub android: String,
    pub sdk: String,
    pub board: String,
    pub soc_model: String,
    pub kernel: String,
    pub ksu: String,
    pub selinux: String,
    pub abi: String,
    pub page_size: String,
    pub mmd: String,
    pub probed: bool,
}

impl DeviceInfo {
    /// One `getprop` call at startup, plus plain file reads. Never per-tick.
    #[allow(clippy::field_reassign_with_default)]
    pub fn probe(root: &SysRoot) -> Self {
        let mut info = DeviceInfo::default();
        info.kernel = root
            .read_trim("/proc/version")
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect();
        info.page_size = crate::collector::memory::page_size_kb(root)
            .map(|kb| format!("{kb} kB"))
            .unwrap_or_else(|| "unknown".into());
        info.selinux = match root.read_trim("/sys/fs/selinux/enforce").as_deref() {
            Some("1") => "enforcing".into(),
            Some("0") => "permissive".into(),
            _ => "unknown".into(),
        };
        if !root.is_system() {
            return info;
        }
        let props = read_props();
        info.model = props.get("ro.product.model").cloned().unwrap_or_default();
        info.device = props.get("ro.product.device").cloned().unwrap_or_default();
        info.fingerprint = props
            .get("ro.build.fingerprint")
            .cloned()
            .unwrap_or_default();
        info.android = props
            .get("ro.build.version.release")
            .cloned()
            .unwrap_or_default();
        info.sdk = props
            .get("ro.build.version.sdk")
            .cloned()
            .unwrap_or_default();
        info.board = props.get("ro.board.platform").cloned().unwrap_or_default();
        info.soc_model = props.get("ro.soc.model").cloned().unwrap_or_default();
        info.abi = props.get("ro.product.cpu.abi").cloned().unwrap_or_default();
        info.ksu = detect_ksu();
        info.mmd = props.get("init.svc.mmd").cloned().unwrap_or_default();
        info.probed = !info.model.is_empty();
        info
    }

    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("model", Json::str(self.model.clone()));
        j.set("device", Json::str(self.device.clone()));
        j.set("fingerprint", Json::str(self.fingerprint.clone()));
        j.set("android", Json::str(self.android.clone()));
        j.set("sdk", Json::str(self.sdk.clone()));
        j.set("board", Json::str(self.board.clone()));
        j.set("soc_model", Json::str(self.soc_model.clone()));
        j.set("kernel", Json::str(self.kernel.clone()));
        j.set("ksu", Json::str(self.ksu.clone()));
        j.set("selinux", Json::str(self.selinux.clone()));
        j.set("abi", Json::str(self.abi.clone()));
        j.set("page_size", Json::str(self.page_size.clone()));
        j.set("mmd", Json::str(self.mmd.clone()));
        j.set("probed", Json::Bool(self.probed));
        j
    }
}

fn read_props() -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    let Ok(out) = Command::new("getprop").output() else {
        return map;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // [key]: [value]
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some((key, tail)) = rest.split_once("]: [") else {
            continue;
        };
        let value = tail.trim_end_matches(']');
        map.insert(key.to_string(), value.to_string());
    }
    map
}

fn detect_ksu() -> String {
    for p in ["/data/adb/ksu/version", "/data/adb/ksud/version"] {
        if let Ok(v) = std::fs::read_to_string(p) {
            return v.trim().to_string();
        }
    }
    if let Ok(out) = Command::new("ksud").arg("-V").output() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    if std::path::Path::new("/data/adb/ksu").exists() {
        return "KernelSU (version unknown)".into();
    }
    "not detected".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Rollback,
    ResetModel,
}

pub struct Data {
    pub daemon: crate::collector::daemon::DaemonSnapshot,
    pub completed_ticks: u64,
    pub last_tick_ms: f64,
    pub max_tick_ms: f64,
    pub snapshot: Snapshot,
    pub decision: Decision,
    pub learner: crate::learner::LearnerSnapshot,
    pub report: crate::actuator::ApplyReport,
    pub history: VecDeque<HistoryPoint>,
    pub config: Config,
    pub device: DeviceInfo,
    pub watchdog: String,
    pub safe_mode: bool,
    pub safe_reason: String,
    pub nodes: Vec<NodeView>,
    pub journal: Vec<JournalEntry>,
    pub recovered: Vec<String>,
    pub capability: Json,
    pub usage: Vec<(String, u32)>,
    pub mode: RunMode,
    pub reload_requested: bool,
    pub action: Option<Action>,
    pub transitions: Vec<(u64, String, String, String)>,
    /// (restored, skipped, conflicts) from the most recent restore pass.
    pub last_restore: (usize, usize, usize),
    /// True when the CPU temperature sensor cannot be trusted; writes are suspended.
    pub thermal_fail_closed: bool,
    pub fail_closed_reason: String,
    /// Mode from config, before safe-mode / fail-closed downgrades.
    pub configured_mode: RunMode,
}

impl Default for Data {
    fn default() -> Self {
        Self {
            daemon: Default::default(),
            completed_ticks: 0,
            last_tick_ms: 0.0,
            max_tick_ms: 0.0,
            snapshot: Snapshot::default(),
            decision: Decision::default(),
            learner: Default::default(),
            report: Default::default(),
            history: VecDeque::new(),
            config: Config::default(),
            device: DeviceInfo::default(),
            watchdog: "starting".into(),
            safe_mode: false,
            safe_reason: String::new(),
            nodes: Vec::new(),
            journal: Vec::new(),
            recovered: Vec::new(),
            capability: Json::Null,
            usage: Vec::new(),
            mode: RunMode::ObserveOnly,
            reload_requested: false,
            action: None,
            transitions: Vec::new(),
            last_restore: (0, 0, 0),
            thermal_fail_closed: false,
            fail_closed_reason: String::new(),
            configured_mode: RunMode::ObserveOnly,
        }
    }
}

pub struct Shared {
    pub token: String,
    pub started_ms: u64,
    pub version: String,
    pub paths: Paths,
    pub logger: Arc<Logger>,
    pub data: Mutex<Data>,
}

impl Shared {
    pub fn with<R>(&self, f: impl FnOnce(&Data) -> R) -> R {
        let guard = self.data.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    pub fn mutate<R>(&self, f: impl FnOnce(&mut Data) -> R) -> R {
        let mut guard = self.data.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    pub fn request_reload(&self) {
        self.mutate(|d| d.reload_requested = true);
    }

    pub fn request_action(&self, a: Action) {
        self.mutate(|d| d.action = Some(a));
    }
}

pub struct Runtime {
    daemon_collector: crate::collector::daemon::DaemonCollector,
    last_daemon_sample_ms: Option<u64>,
    root: SysRoot,
    config: Config,
    shared: Arc<Shared>,
    collectors: Collectors,
    learner: crate::learner::Learner,
    engine: Engine,
    actuator: Actuator,
    watchdog: Watchdog,
    telemetry: Telemetry,
    paths: Paths,
    journal: Journal,
    last_history_ms: u64,
    last_model_save_ms: u64,
    thermal_fail_closed: bool,
    last_screen_off: bool,
    stop: Arc<AtomicBool>,
    _lock: crate::safety::lock::RuntimeLock,
}

impl Runtime {
    pub fn new(
        root: SysRoot,
        cfg: Config,
        paths: Paths,
        boot_ms: u64,
        token: String,
    ) -> std::io::Result<Self> {
        paths.ensure()?;
        let runtime_lock = crate::safety::lock::RuntimeLock::acquire(&paths.root)?;
        let logger = Arc::new(Logger::new(
            cfg.log.ring,
            Level::parse(&cfg.log.level),
            paths.logs().join("neurosched.log"),
            cfg.log.file_rotate_kb,
        ));
        logger.set_keep_files(cfg.log.keep_files);

        let session = format!(
            "{}-{}-{}",
            boot_ms,
            std::process::id(),
            crate::storage::random_hex(16)?
        );
        let journal = Journal::for_root(paths.journal(), session.clone(), &root)?;
        // Recovery must happen before this session claims the `active` marker.
        let pending = journal.pending_recovery()?;

        let mut recovered = Vec::new();
        let mut released_nodes = Vec::new();
        let mut previous_boot = false;
        if let Some(PendingRecovery::Replay {
            session: old_session,
            entries,
        }) = &pending
        {
            if !entries.is_empty() {
                let report = rollback::replay(&root, entries);
                if report.failed > 0 {
                    return Err(std::io::Error::other(format!(
                        "recovery incomplete: {:?}",
                        report.notes
                    )));
                }
                recovered.push(format!(
                    "session {}: restored {}, skipped {}, {} conflict(s)",
                    old_session,
                    report.restored,
                    report.skipped,
                    report.conflicts.len()
                ));
                for c in report.conflicts {
                    recovered.push(c);
                }
                released_nodes = report.released_nodes;
            }
            journal.end()?;
        } else if let Some(PendingRecovery::PreviousBoot {
            session,
            boot_id,
            empty,
        }) = pending
        {
            if empty {
                journal.end()?;
                recovered.push(format!(
                    "retired empty session {session} from boot {boot_id}; no kernel writes occurred"
                ));
            } else {
                journal.archive_previous_boot(&session)?;
                previous_boot = true;
                recovered.push(format!(
                    "archived session {session} from boot {boot_id}; no kernel values replayed"
                ));
            }
        } else if let Some(PendingRecovery::LegacyEmpty { session }) = pending {
            journal.end()?;
            recovered.push(format!("retired empty legacy session {session}"));
        }
        journal.begin()?;
        journal.rotate(cfg.storage.keep_files);

        let watchdog = Watchdog::load(
            paths.boot_state(),
            cfg.safety.consecutive_failure_limit,
            cfg.safety.safe_mode_boots,
            boot_ms,
            crate::VERSION,
            cfg.mode.as_str(),
        );
        let safe_mode = watchdog.in_safe_mode() || previous_boot;
        let safe_reason = if previous_boot {
            "previous boot journal archived; observation only for this run".into()
        } else if safe_mode {
            watchdog.safe_mode_reason()
        } else {
            String::new()
        };

        let mut engine = Engine::new(
            cfg.policy.clone(),
            cfg.actuator.profiles.clone(),
            cfg.apps.launchers.clone(),
            boot_ms,
        );
        if safe_mode {
            engine.set_safe_mode(true, safe_reason.clone());
        }

        // An active mode needs a fresh trusted sample before it can acquire nodes.
        let mode = effective_mode(&cfg, &root, safe_mode || cfg.mode.writes_allowed());

        let mut actuator = Actuator::new(
            root.clone(),
            &cfg.actuator,
            mode,
            journal.clone(),
            cfg.safety.on_conflict,
        );
        actuator.release_recovered_nodes(&released_nodes);

        // Do not leave the device in a tuned state if we are in safe mode.
        if safe_mode {
            let _ = actuator.restore_all();
        }

        let device = DeviceInfo::probe(&root);
        let mut collectors = Collectors::new(&cfg);
        let init_state = if device.mmd.is_empty() {
            None
        } else {
            Some(device.mmd.clone())
        };
        collectors.set_mmd(process::detect_mmd(&root, init_state));

        let telemetry = Telemetry::new(
            &paths.telemetry(),
            cfg.storage.rotate_kb,
            cfg.storage.keep_files,
            cfg.storage.telemetry,
        );

        let learner = crate::learner::Learner::load(&paths.sched_model(), &paths.learner_state());
        let capability = actuator.capability_json();
        let uclamp_present = capability.get("uclamp").is_some();

        let data = Data {
            config: cfg.clone(),
            configured_mode: cfg.mode,
            device,
            mode,
            safe_mode,
            safe_reason,
            recovered: recovered.clone(),
            capability,
            nodes: actuator.nodes(),
            journal: journal.recent(50),
            ..Default::default()
        };
        let _ = journal.take_dirty();

        let shared = Arc::new(Shared {
            token,
            started_ms: boot_ms,
            version: crate::VERSION.to_string(),
            paths: paths.clone(),
            logger: logger.clone(),
            data: Mutex::new(data),
        });

        let pid_path = paths.root.join("daemon.pid");
        if std::fs::write(&pid_path, format!("{}\n", std::process::id())).is_ok() {
            crate::storage::secure_file(&pid_path);
        }
        logger.info(
            "runtime",
            format!(
                "NeuroSched {} starting: mode={} safe_mode={} uclamp={}",
                crate::VERSION,
                mode.as_str(),
                safe_mode,
                uclamp_present
            ),
        );
        for r in recovered {
            logger.warn("recovery", r);
        }

        Ok(Self {
            daemon_collector: crate::collector::daemon::DaemonCollector::new(),
            last_daemon_sample_ms: None,
            root,
            config: cfg,
            shared,
            collectors,
            learner,
            engine,
            actuator,
            watchdog,
            telemetry,
            paths,
            journal,
            last_history_ms: 0,
            last_model_save_ms: boot_ms,
            thermal_fail_closed: false,
            last_screen_off: false,
            stop: Arc::new(AtomicBool::new(false)),
            _lock: runtime_lock,
        })
    }

    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }

    pub fn start_event_sources(&mut self) {
        let log = self.paths.companion_log().to_string_lossy().to_string();
        self.collectors.start_event_sources(&self.root, log);
    }

    /// One iteration of the control loop. Split out so tests can drive it directly.
    pub fn tick(&mut self, now: u64) {
        self.tick_at(now, crate::util::now_ms());
    }

    /// Inject both clocks independently for deterministic normal-path tests.
    pub fn tick_at(&mut self, now: u64, wall_ms: u64) {
        let tick_started = std::time::Instant::now();
        self.handle_requests();

        let mut snap = self.collectors.sample(&self.root, now);
        snap.wall_ts_ms = wall_ms;
        self.last_screen_off = snap.apps.screen == ScreenState::Off;
        let _ = self.collectors.drain_app_events();

        let is_launcher = snap
            .apps
            .foreground
            .as_deref()
            .is_some_and(|fg| fg == "-" || self.config.apps.launchers.iter().any(|l| l == fg));
        let learner_snap = self.learner.on_tick(now, wall_ms, &snap, is_launcher);

        let decision = self.engine.evaluate(&snap, now);

        // ---- thermal fail-closed -------------------------------------------
        // If the CPU temperature cannot be trusted, active writes stop entirely.
        // This is a *runtime* downgrade: config.json keeps the user's chosen mode so
        // the daemon returns to it as soon as the sensor is healthy again.
        let fail_closed = !snap.thermal.trust.is_trusted();
        if fail_closed != self.thermal_fail_closed {
            if fail_closed {
                self.shared.logger.warn(
                    "thermal",
                    format!(
                        "fail-closed: {} ({}); switching to observe_only",
                        snap.thermal.trust.as_str(),
                        snap.thermal.detail
                    ),
                );
            } else {
                self.shared.logger.info(
                    "thermal",
                    "thermal sensors trusted again; restoring configured mode",
                );
            }
            self.thermal_fail_closed = fail_closed;
        }
        let configured_mode = self.config.mode;
        let effective_mode = effective_mode(
            &self.config,
            &self.root,
            self.thermal_fail_closed || self.engine.safe_mode(),
        );
        if self.actuator.mode() != effective_mode {
            if self.actuator.mode().writes_allowed() && !effective_mode.writes_allowed() {
                self.restore_owned("mode downgrade");
            }
            self.actuator.set_mode(effective_mode);
        }
        // Every sample revalidates ownership and the full target. Unchanged values
        // incur reads only, including when thermal ceilings change within a state.
        let report = self.actuator.apply(&decision, wall_ms);
        let mut new_safe_reason: Option<String> = None;
        if self.actuator.recovery_required() {
            let reason = "actuator recovery requires inspection".to_string();
            self.engine.set_safe_mode(true, reason.clone());
            self.actuator.set_mode(RunMode::ObserveOnly);
            new_safe_reason = Some(reason);
        }

        let telemetry_healthy = snap
            .health
            .collectors
            .iter()
            .filter(|c| matches!(c.name.as_ref(), "cpu" | "memory" | "thermal"))
            .all(|c| c.ok);
        let wd_status = self
            .watchdog
            .tick_with_health(now, snap.ts_ms, telemetry_healthy);
        let wd_text = match &wd_status {
            WatchdogStatus::Ok => "ok".to_string(),
            WatchdogStatus::Stale(ms) => format!("stale {ms}ms"),
            WatchdogStatus::SafeMode(n) => format!("safe mode ({n} boots left)"),
        };
        if matches!(wd_status, WatchdogStatus::SafeMode(_)) && !self.engine.safe_mode() {
            let reason = self.watchdog.safe_mode_reason();
            self.engine.set_safe_mode(true, reason.clone());
            new_safe_reason = Some(reason);
        }

        let new_history_point =
            if now.saturating_sub(self.last_history_ms) >= self.config.sample.history_interval_ms {
                let point = HistoryPoint::from_snapshot(&snap, decision.state.as_str());
                self.telemetry.append(&point);
                self.last_history_ms = now;
                Some(point)
            } else {
                None
            };

        if self.learner.is_dirty()
            && now.saturating_sub(self.last_model_save_ms) >= LEARNER_PERSIST_INTERVAL_MS
        {
            let _ = self.learner.save_state(&self.paths.learner_state());
            self.last_model_save_ms = now;
        }

        let transitions: Vec<(u64, String, String, String)> = self
            .engine
            .machine()
            .log()
            .iter()
            .rev()
            .take(30)
            .map(|t| {
                (
                    t.ts_ms,
                    t.from.as_str().into(),
                    t.to.as_str().into(),
                    t.reason.clone(),
                )
            })
            .collect();

        let usage = self.learner.usage_ranking(12);
        let nodes = self.actuator.nodes();
        let journal_recent = if self.journal.take_dirty() {
            Some(self.journal.recent(50))
        } else {
            None
        };
        let capability = self.actuator.capability_json();
        let mode = self.actuator.mode();
        let daemon_sample = if self
            .last_daemon_sample_ms
            .map(|last| now < last || now.saturating_sub(last) >= 5_000)
            .unwrap_or(true)
        {
            self.last_daemon_sample_ms = Some(now);
            Some(self.daemon_collector.collect(&self.root, now))
        } else {
            None
        };
        let tick_ms = tick_started.elapsed().as_secs_f64() * 1_000.0;

        self.shared.mutate(|d| {
            if let Some(sample) = daemon_sample {
                d.daemon = sample;
            }
            if let Some(reason) = new_safe_reason {
                d.safe_reason = reason;
            }
            if let Some(point) = new_history_point {
                d.history.push_back(point);
                while d.history.len() > 720 {
                    d.history.pop_front();
                }
            }
            if let Some(j) = journal_recent {
                d.journal = j;
            }
            d.completed_ticks = d.completed_ticks.saturating_add(1);
            d.last_tick_ms = tick_ms;
            d.max_tick_ms = d.max_tick_ms.max(tick_ms);
            d.snapshot = snap;
            d.decision = decision;
            d.learner = learner_snap;
            d.report = report;
            d.watchdog = wd_text;
            d.safe_mode = self.engine.safe_mode();
            d.nodes = nodes;
            d.capability = capability;
            d.usage = usage;
            d.transitions = transitions;
            d.mode = mode;
            d.configured_mode = configured_mode;
            d.thermal_fail_closed = self.thermal_fail_closed;
            d.fail_closed_reason = if self.thermal_fail_closed {
                format!(
                    "{}: {}",
                    d.snapshot.thermal.trust.as_str(),
                    d.snapshot.thermal.detail
                )
            } else {
                String::new()
            };
        });
    }

    fn handle_requests(&mut self) {
        let (reload, action) = self.shared.mutate(|d| {
            let r = d.reload_requested;
            let a = d.action;
            d.reload_requested = false;
            d.action = None;
            (r, a)
        });

        if reload {
            let (cfg, warn) = Config::load_or_recover(&self.paths.config());
            if let Some(w) = warn {
                self.shared.logger.warn("config", w);
            }
            self.apply_config(cfg);
            self.shared.logger.info("config", "configuration reloaded");
        }

        match action {
            Some(Action::Rollback) => {
                self.restore_owned("manual rollback");
                self.actuator.set_mode(RunMode::ObserveOnly);
                self.config.mode = RunMode::ObserveOnly;
                self.config.actuator.enabled = false;
                self.shared.mutate(|d| {
                    d.config.mode = RunMode::ObserveOnly;
                    d.config.actuator.enabled = false;
                });
                if let Err(err) = self.config.save(&self.paths.config()) {
                    self.shared.logger.warn(
                        "config",
                        format!("rollback stays read-only in memory, persistence failed: {err}"),
                    );
                }
            }
            Some(Action::ResetModel) => {
                self.learner.reset();
                let _ = std::fs::remove_file(self.paths.learner_state());
                self.shared.logger.warn("learner", "model reset");
            }
            None => {}
        }
    }

    fn apply_config(&mut self, cfg: Config) {
        if self.actuator.writes_allowed() {
            self.restore_owned("configuration reload");
        }
        self.engine.update_config(
            cfg.policy.clone(),
            cfg.actuator.profiles.clone(),
            cfg.apps.launchers.clone(),
        );
        self.learner.reload_model(&self.paths.sched_model());
        let mode = effective_mode(
            &cfg,
            &self.root,
            self.watchdog.in_safe_mode() || self.thermal_fail_closed || self.engine.safe_mode(),
        );
        self.actuator.set_mode(mode);
        self.actuator
            .update_config(&cfg.actuator, cfg.safety.on_conflict);
        self.shared.logger.set_keep_files(cfg.log.keep_files);
        self.collectors.update_config(&cfg);
        self.config = cfg.clone();
        self.shared.mutate(|d| {
            d.config = cfg;
            d.mode = mode;
        });
    }

    fn restore_owned(&mut self, reason: &str) -> bool {
        let report = self.actuator.restore_all();
        self.shared.mutate(|d| {
            d.last_restore = (report.restored, report.skipped, report.conflicts.len());
            d.recovered.extend(report.notes.iter().cloned());
            d.recovered.truncate(100);
        });
        if report.failed > 0 {
            self.engine
                .set_safe_mode(true, format!("{reason}: recovery incomplete"));
            self.shared.mutate(|d| {
                d.safe_mode = true;
                d.safe_reason = format!("{reason}: recovery incomplete");
            });
            self.shared.logger.warn(
                "safety",
                format!("{reason}: recovery failed: {:?}", report.notes),
            );
            return false;
        }
        self.actuator.forget();
        true
    }

    /// Blocking control loop.
    pub fn run(&mut self) {
        use crate::safety::liveness::{Monitor, Progress, STALL_BUDGET_MS, STALL_EXIT_CODE};
        let progress = Arc::new(Progress::default());
        progress.allow_until(crate::util::mono_ms().saturating_add(STALL_BUDGET_MS));
        let poll_ms = self.config.safety.watchdog_interval_ms.clamp(1_000, 10_000);
        let _monitor =
            match Monitor::start(progress.clone(), Duration::from_millis(poll_ms), || {
                // No logger/shared locks here: the loop may be stalled while owning one.
                // Exit preserves active journal evidence. Only this process is stopped.
                std::process::exit(STALL_EXIT_CODE);
            }) {
                Ok(monitor) => monitor,
                Err(error) => {
                    self.shared.logger.warn(
                        "watchdog",
                        format!("cannot start liveness monitor: {error}"),
                    );
                    self.shutdown();
                    return;
                }
            };
        self.start_event_sources();
        self.shared.logger.info("runtime", "control loop started");
        let stop = self.stop.clone();
        while !stop.load(Ordering::SeqCst) && !self.paths.root.join("stop").exists() {
            let now = crate::util::mono_ms();
            progress.allow_until(now.saturating_add(STALL_BUDGET_MS));
            self.tick(now);
            let interval = if self.last_screen_off {
                self.config.sample.screen_off_interval_ms
            } else {
                self.config.sample.interval_ms
            };
            progress.allow_until(
                crate::util::mono_ms()
                    .saturating_add(interval)
                    .saturating_add(STALL_BUDGET_MS),
            );
            // Bound cooperative stop latency without waking the control thread
            // five times every second while the screen is off.
            let mut slept = 0u64;
            while slept < interval
                && !stop.load(Ordering::SeqCst)
                && !self.paths.root.join("stop").exists()
            {
                let step = 1_000u64.min(interval - slept);
                thread::sleep(Duration::from_millis(step));
                slept += step;
            }
            // Cooperative stop: `neuroschedd --stop` drops this file so the daemon can
            // restore nodes before exiting instead of being killed mid-flight.
            if self.paths.root.join("stop").exists() {
                self.shared.logger.info("runtime", "stop file seen");
                break;
            }
        }
        progress.allow_until(crate::util::mono_ms().saturating_add(STALL_BUDGET_MS));
        self.shutdown();
    }

    pub fn shutdown(&mut self) {
        self.shared.logger.info("runtime", "shutting down");
        if self.learner.is_dirty() {
            let _ = self.learner.save_state(&self.paths.learner_state());
        }
        if self.restore_owned("shutdown") && !self.actuator.recovery_required() {
            match self.journal.end() {
                Ok(()) => self.watchdog.mark_healthy(),
                Err(err) => self
                    .shared
                    .logger
                    .warn("safety", format!("journal close failed: {err}")),
            }
        }
        let _ = std::fs::remove_file(self.paths.root.join("daemon.pid"));
        self.collectors.apps().stop();
    }
}

/// Export only diagnostic counters and device capabilities, without app history.
pub fn diagnostics_json(d: &Data) -> Json {
    // Deliberate allow-list: never serialize Shared, Config, model, raw logs,
    // app history, recovery text or the complete device identity into an export.
    let mut report = Json::obj();
    report.set("schema", Json::num(1.0));
    report.set("version", Json::str(crate::VERSION.to_string()));
    report.set("mode", Json::str(d.mode.as_str()));
    report.set("configured_mode", Json::str(d.configured_mode.as_str()));
    report.set(
        "iso",
        Json::str(crate::util::iso_utc(crate::util::now_ms())),
    );
    report.set("ts_ms", Json::num(d.snapshot.ts_ms as f64));
    report.set("wall_ts_ms", Json::num(d.snapshot.wall_ts_ms as f64));
    report.set("uptime_s", Json::num(d.snapshot.uptime_s));
    report.set("daemon", daemon_json(d));
    report.set("cpu", cpu_json(d));
    report.set("memory", memory_json(d));
    report.set("psi", psi_json(d));
    report.set("thermal", thermal_status_json(d));
    report.set("battery", battery_json(d));

    let mut device = Json::obj();
    let identity = d.device.to_json();
    for key in [
        "model",
        "android",
        "sdk",
        "soc_model",
        "abi",
        "page_size",
        "selinux",
        "ksu",
    ] {
        if let Some(value) = identity.get(key) {
            device.set(key, value.clone());
        }
    }
    report.set("device", device);
    report.set(
        "health",
        Json::Arr(
            d.snapshot
                .health
                .collectors
                .iter()
                .map(|c| {
                    let mut entry = Json::obj();
                    entry.set("name", Json::str(c.name.clone()));
                    entry.set("ok", Json::Bool(c.ok));
                    entry
                })
                .collect(),
        ),
    );
    report.set(
        "sample_errors",
        Json::num(d.snapshot.health.sample_errors as f64),
    );
    let mut sampling = Json::obj();
    sampling.set("interval_ms", Json::num(d.config.sample.interval_ms as f64));
    sampling.set(
        "screen_off_interval_ms",
        Json::num(d.config.sample.screen_off_interval_ms as f64),
    );
    sampling.set(
        "thermal_interval_ms",
        Json::num(d.config.sample.thermal_interval_ms as f64),
    );
    sampling.set(
        "history_interval_ms",
        Json::num(d.config.sample.history_interval_ms as f64),
    );
    sampling.set(
        "proc_scan_interval_ms",
        Json::num(d.config.sample.proc_scan_interval_ms as f64),
    );
    report.set("sampling", sampling);
    report.set("safe_mode", Json::Bool(d.safe_mode));
    report.set("ACTUATOR_ENABLED", Json::Bool(crate::sys::ACTUATOR_ENABLED));
    report.set("learner", d.learner.to_diagnostics_json());
    report
}

pub fn daemon_json(d: &Data) -> Json {
    let mut daemon = d.daemon.to_json();
    daemon.set("completed_ticks", Json::num(d.completed_ticks as f64));
    daemon.set("last_tick_ms", Json::num(d.last_tick_ms));
    daemon.set("max_tick_ms", Json::num(d.max_tick_ms));
    daemon.set(
        "liveness_budget_ms",
        Json::num(crate::safety::liveness::STALL_BUDGET_MS as f64),
    );
    daemon
}

pub fn cpu_json(d: &Data) -> Json {
    let mut cpu = Json::obj();
    cpu.set("total_pct", Json::num(d.snapshot.cpu.total_pct));
    cpu.set("present", Json::str(fmt_list(&d.snapshot.cpu.present)));
    cpu.set("online", Json::str(fmt_list(&d.snapshot.cpu.online)));
    cpu.set("walt", Json::Bool(d.snapshot.cpu.walt_present));
    cpu.set("uclamp", Json::Bool(d.snapshot.cpu.uclamp_supported));
    cpu.set(
        "clusters",
        Json::Arr(
            d.snapshot
                .cpu
                .clusters
                .iter()
                .map(|c| {
                    let mut o = Json::obj();
                    o.set("name", Json::str(c.name.clone()));
                    o.set("cpus", Json::str(fmt_list(&c.cpus)));
                    o.set("governor", Json::str(c.governor.clone()));
                    o.set("available", Json::str(c.available_governors.join(" ")));
                    o.set("cur_mhz", Json::num(c.cur_khz as f64 / 1000.0));
                    o.set("min_mhz", Json::num(c.min_khz as f64 / 1000.0));
                    o.set("max_mhz", Json::num(c.max_khz as f64 / 1000.0));
                    o.set(
                        "cpuinfo_max_mhz",
                        Json::num(c.cpuinfo_max_khz as f64 / 1000.0),
                    );
                    o.set("capacity", Json::num(c.capacity.unwrap_or(0) as f64));
                    o.set("load_pct", Json::num(c.load_pct));
                    o.set("path", Json::str(c.policy_path.clone()));
                    o
                })
                .collect(),
        ),
    );
    cpu.set(
        "cores",
        Json::Arr(
            d.snapshot
                .cpu
                .cores
                .iter()
                .map(|c| {
                    let mut o = Json::obj();
                    o.set("id", Json::num(c.id as f64));
                    o.set("pct", Json::num(c.usage_pct));
                    o.set("mhz", Json::num(c.freq_khz.unwrap_or(0) as f64 / 1000.0));
                    o
                })
                .collect(),
        ),
    );
    cpu.set(
        "knobs",
        Json::Obj(
            d.snapshot
                .cpu
                .knobs
                .iter()
                .map(|(k, v)| (k.clone(), Json::str(v.clone())))
                .collect(),
        ),
    );
    cpu
}

pub fn memory_json(d: &Data) -> Json {
    let m = &d.snapshot.mem;
    let mut mem = Json::obj();
    mem.set("total_mb", Json::num(m.total_kb / 1024.0));
    mem.set("available_mb", Json::num(m.available_kb / 1024.0));
    mem.set("available_pct", Json::num(m.available_pct()));
    mem.set("cached_mb", Json::num(m.cached_kb / 1024.0));
    mem.set("swap_total_mb", Json::num(m.swap_total_kb / 1024.0));
    mem.set("swap_used_mb", Json::num(m.swap_used_kb() / 1024.0));
    mem.set("swap_devices", Json::str(m.swap_devices.join(" ")));
    mem.set("pswpin", Json::num(m.pswpin));
    mem.set("pswpout", Json::num(m.pswpout));
    mem.set("swap_in_rate", Json::num(m.swap_in_rate_kb_s));
    mem.set("pgscan_kswapd", Json::num(m.pgscan_kswapd));
    mem.set("pgsteal_kswapd", Json::num(m.pgsteal_kswapd));
    mem.set("pgmajfault", Json::num(m.pgmajfault));
    mem.set("kswapd_pct", Json::num(m.kswapd_cpu_pct));
    mem.set("kswapd_pid", Json::num(m.kswapd_pid.unwrap_or(0) as f64));
    mem.set(
        "zram",
        Json::Arr(
            m.zram
                .iter()
                .map(|z| {
                    let mut o = Json::obj();
                    o.set("name", Json::str(z.name.clone()));
                    o.set("orig_mb", Json::num(z.orig_kb / 1024.0));
                    o.set("compr_mb", Json::num(z.compr_kb / 1024.0));
                    o.set("used_mb", Json::num(z.mem_used_kb / 1024.0));
                    o.set("ratio", Json::num(z.ratio));
                    o
                })
                .collect(),
        ),
    );
    let mut mmd = Json::obj();
    mmd.set("present", Json::Bool(m.mmd.present));
    mmd.set("pid", Json::num(m.mmd.pid.unwrap_or(0) as f64));
    mmd.set(
        "service",
        Json::str(m.mmd.service_state.clone().unwrap_or_default()),
    );
    mem.set("mmd", mmd);
    mem
}

pub fn psi_json(d: &Data) -> Json {
    let mut psi = Json::obj();
    for (name, res) in [
        ("cpu", d.snapshot.psi.cpu),
        ("memory", d.snapshot.psi.memory),
        ("io", d.snapshot.psi.io),
    ] {
        let mut o = Json::obj();
        o.set("available", Json::Bool(res.available));
        o.set("some10", Json::num(res.some10));
        o.set("some60", Json::num(res.some60));
        o.set("some300", Json::num(res.some300));
        o.set("full10", Json::num(res.full10));
        o.set("full60", Json::num(res.full60));
        o.set("full300", Json::num(res.full300));
        psi.set(name, o);
    }
    psi
}

pub fn thermal_status_json(d: &Data) -> Json {
    let t = &d.snapshot.thermal;
    let mut th = Json::obj();
    th.set(
        "status",
        Json::str(
            t.level
                .map(|l| l.as_str())
                .unwrap_or(if t.trust.is_trusted() {
                    "NONE"
                } else {
                    "UNKNOWN"
                }),
        ),
    );
    th.set("source", Json::str(t.level_source.clone()));
    th.set("trust", Json::str(t.trust.as_str()));
    th.set("trust_detail", Json::str(t.detail.clone()));
    th.set("age_ms", Json::num(t.age_ms.min(86_400_000) as f64));
    th.set("fail_closed", Json::Bool(d.thermal_fail_closed));
    th.set(
        "fail_closed_reason",
        Json::str(d.fail_closed_reason.clone()),
    );
    th.set("cpu_c", opt_num(t.cpu_max_c));
    th.set("gpu_c", opt_num(t.gpu_c));
    th.set("skin_c", opt_num(t.skin_c));
    th.set("battery_c", opt_num(t.battery_c));
    th.set(
        "zones",
        Json::Arr(
            t.zones
                .iter()
                .map(|z| {
                    let mut o = Json::obj();
                    o.set("name", Json::str(z.name.clone()));
                    o.set("kind", Json::str(z.kind.as_str()));
                    o.set("temp_c", Json::num(z.temp_c));
                    o
                })
                .collect(),
        ),
    );
    th
}

pub fn battery_json(d: &Data) -> Json {
    let b = &d.snapshot.battery;
    let mut bat = Json::obj();
    bat.set("present", Json::Bool(b.present));
    bat.set("pct", opt_num(b.capacity_pct));
    bat.set("temp_c", opt_num(b.temp_c));
    bat.set("status", Json::str(b.status.clone()));
    bat.set("charging", Json::Bool(b.charging));
    bat.set("current_ma", opt_num(b.current_ma));
    bat.set("voltage_mv", opt_num(b.voltage_mv));
    bat
}

/// Full status document, shared by the HTTP API and `--status`.
pub fn status_json(d: &Data) -> Json {
    let mut j = Json::obj();
    j.set("version", Json::str(crate::VERSION.to_string()));
    j.set("mode", Json::str(d.mode.as_str()));
    j.set("configured_mode", Json::str(d.configured_mode.as_str()));
    j.set("ts_ms", Json::num(d.snapshot.ts_ms as f64));
    j.set("wall_ts_ms", Json::num(d.snapshot.wall_ts_ms as f64));
    j.set(
        "iso",
        Json::str(crate::util::iso_utc(crate::util::now_ms())),
    );
    j.set("uptime_s", Json::num(d.snapshot.uptime_s));
    j.set("device", d.device.to_json());
    j.set("daemon", daemon_json(d));
    j.set("policy", d.decision.to_json());
    j.set("learner", d.learner.to_json());
    j.set("cpu", cpu_json(d));
    j.set("memory", memory_json(d));
    j.set("psi", psi_json(d));
    j.set("thermal", thermal_status_json(d));
    j.set("battery", battery_json(d));

    // Apps
    let mut apps = Json::obj();
    apps.set(
        "foreground",
        d.snapshot
            .apps
            .foreground
            .clone()
            .map(Json::Str)
            .unwrap_or(Json::Null),
    );
    apps.set("since_ms", Json::num(d.snapshot.apps.since_ms as f64));
    apps.set(
        "source",
        Json::str(d.snapshot.apps.source.map(|s| s.as_str()).unwrap_or("none")),
    );
    apps.set("screen", Json::str(d.snapshot.apps.screen.as_str()));
    j.set("apps", apps);

    // Health / safety / predictor stats
    j.set(
        "health",
        Json::Arr(
            d.snapshot
                .health
                .collectors
                .iter()
                .map(|c| {
                    let mut o = Json::obj();
                    o.set("name", Json::str(c.name.clone()));
                    o.set("ok", Json::Bool(c.ok));
                    o.set("note", Json::str(c.note.clone()));
                    o
                })
                .collect(),
        ),
    );
    let mut safety = Json::obj();
    safety.set("watchdog", Json::str(d.watchdog.clone()));
    safety.set("safe_mode", Json::Bool(d.safe_mode));
    safety.set("safe_reason", Json::str(d.safe_reason.clone()));
    safety.set(
        "writes_allowed",
        d.capability
            .get("writes_allowed")
            .cloned()
            .unwrap_or(Json::Bool(false)),
    );
    safety.set("thermal_fail_closed", Json::Bool(d.thermal_fail_closed));
    safety.set(
        "fail_closed_reason",
        Json::str(d.fail_closed_reason.clone()),
    );
    safety.set("applied", Json::num(d.report.applied() as f64));
    safety.set("failed", Json::num(d.report.failed() as f64));
    safety.set(
        "conflicts",
        Json::Arr(d.report.conflicts.iter().cloned().map(Json::Str).collect()),
    );
    safety.set("capability", d.capability.clone());
    j.set("safety", safety);

    j.set(
        "transitions",
        Json::Arr(
            d.transitions
                .iter()
                .map(|(ts, from, to, why)| {
                    let mut o = Json::obj();
                    o.set("ts", Json::num(*ts as f64));
                    o.set("clock", Json::str("monotonic"));
                    o.set("from", Json::str(from.clone()));
                    o.set("to", Json::str(to.clone()));
                    o.set("reason", Json::str(why.clone()));
                    o
                })
                .collect(),
        ),
    );
    j.set(
        "report",
        Json::Arr(
            d.report
                .results
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
    j
}

impl Data {
    /// Thermal block used by `/api/capability` and the Safety page.
    pub fn thermal_json(&self) -> Json {
        let t = &self.snapshot.thermal;
        let mut j = Json::obj();
        j.set("trust", Json::str(t.trust.as_str()));
        j.set("detail", Json::str(t.detail.clone()));
        j.set("age_ms", Json::num(t.age_ms.min(86_400_000) as f64));
        j.set("invalid_zones", Json::num(t.invalid_zones as f64));
        j.set("cpu_c", opt_num(t.cpu_max_c));
        j.set("skin_c", opt_num(t.skin_c));
        j.set(
            "level",
            Json::str(
                t.level
                    .map(|l| l.as_str())
                    .unwrap_or(if t.trust.is_trusted() {
                        "NONE"
                    } else {
                        "UNKNOWN"
                    }),
            ),
        );
        j.set("fail_closed", Json::Bool(self.thermal_fail_closed));
        j.set(
            "fail_closed_reason",
            Json::str(self.fail_closed_reason.clone()),
        );
        j.set(
            "zones",
            Json::Arr(
                t.zones
                    .iter()
                    .map(|z| {
                        let mut o = Json::obj();
                        o.set("name", Json::str(z.name.clone()));
                        o.set("kind", Json::str(z.kind.as_str()));
                        o.set("temp_c", Json::num(z.temp_c));
                        o
                    })
                    .collect(),
            ),
        );
        j
    }
}

pub fn safety_json(d: &Data) -> Json {
    let view = SafetyView {
        watchdog: d.watchdog.clone(),
        safe_mode: d.safe_mode,
        safe_reason: d.safe_reason.clone(),
        writes_allowed: d
            .capability
            .get("writes_allowed")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        mode: d.mode.as_str().to_string(),
        nodes: d.nodes.clone(),
        conflicts: d.report.conflicts.clone(),
        journal: d.journal.clone(),
        recovered: d.recovered.clone(),
    };
    view.to_json()
}

fn fmt_list(v: &[u32]) -> String {
    v.iter()
        .map(|x| x.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn opt_num(v: Option<f64>) -> Json {
    v.map(Json::Num).unwrap_or(Json::Null)
}

fn effective_mode(cfg: &Config, root: &SysRoot, safety_blocked: bool) -> RunMode {
    if safety_blocked
        || (cfg.mode.writes_allowed()
            && (!cfg.actuator.enabled || (root.is_system() && !crate::sys::ACTUATOR_ENABLED)))
    {
        RunMode::ObserveOnly
    } else {
        cfg.mode
    }
}

/// Resolves the data directory: CLI override, then config, then the module default.
pub fn resolve_paths(data_dir: Option<PathBuf>, cfg: &Config) -> Paths {
    Paths::new(data_dir.unwrap_or_else(|| PathBuf::from(&cfg.storage.data_dir)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_does_not_require_actuator_consent() {
        let mut cfg = Config {
            mode: RunMode::DryRun,
            ..Default::default()
        };
        assert_eq!(
            effective_mode(&cfg, &SysRoot::system(), false),
            RunMode::DryRun
        );
        cfg.mode = RunMode::Enabled;
        assert_eq!(
            effective_mode(&cfg, &SysRoot::system(), false),
            RunMode::ObserveOnly
        );
    }

    #[test]
    fn safety_needs_capability_permission_even_if_mode_says_enabled() {
        let data = Data {
            mode: RunMode::Enabled,
            ..Default::default()
        };
        assert_eq!(
            safety_json(&data)
                .get("writes_allowed")
                .and_then(Json::as_bool),
            Some(false)
        );
        assert_eq!(
            status_json(&data)
                .get_path("safety.writes_allowed")
                .and_then(Json::as_bool),
            Some(false)
        );
    }
}
