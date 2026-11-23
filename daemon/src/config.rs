//! Configuration: defaults, JSON round-trip, validation and corruption recovery.
//!
//! The defaults are deliberately inert: `observe_only`, no uclamp writes, no
//! scheduler writes. Installing the module must never change device behaviour.

use crate::json::{self, Json};
use crate::types::{PolicyState, RunMode};
use std::path::{Path, PathBuf};

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct Config {
    pub version: u32,
    pub mode: RunMode,
    pub sample: SampleConfig,
    pub log: LogConfig,
    pub api: ApiConfig,
    pub policy: PolicyConfig,
    pub actuator: ActuatorConfig,
    pub predictor: PredictorConfig,
    pub safety: SafetyConfig,
    pub apps: AppsConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone)]
pub struct SampleConfig {
    pub interval_ms: u64,
    pub screen_off_interval_ms: u64,
    pub thermal_interval_ms: u64,
    pub history_interval_ms: u64,
    pub proc_scan_interval_ms: u64,
}

#[derive(Debug, Clone)]
pub struct LogConfig {
    pub level: String,
    pub ring: usize,
    pub file_rotate_kb: u64,
    pub keep_files: usize,
}

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct PolicyConfig {
    pub min_dwell_ms: u64,
    pub cooldown_ms: u64,
    pub boot_grace_ms: u64,
    pub thermal: ThermalPolicy,
    pub memory: MemoryPolicy,
    pub battery: BatteryPolicy,
    pub app_launch: AppLaunchPolicy,
    pub game: GamePolicy,
    pub idle: IdlePolicy,
}

#[derive(Debug, Clone)]
pub struct ThermalPolicy {
    pub enter_light_c: f64,
    pub enter_moderate_c: f64,
    pub enter_severe_c: f64,
    pub enter_critical_c: f64,
    pub exit_margin_c: f64,
    /// When true the guard also reacts to skin temperature (more conservative).
    pub include_skin: bool,
}

#[derive(Debug, Clone)]
pub struct MemoryPolicy {
    pub psi_some_enter: f64,
    pub psi_some_exit: f64,
    pub psi_full_enter: f64,
    pub psi_full_exit: f64,
    pub avail_pct_enter: f64,
    pub avail_pct_exit: f64,
    pub kswapd_pct_enter: f64,
    pub kswapd_pct_exit: f64,
    /// Swap growth (KiB/s) above which we treat paging as active thrashing.
    pub swap_in_rate_enter: f64,
}

#[derive(Debug, Clone)]
pub struct BatteryPolicy {
    pub saver_pct: f64,
    pub exit_pct: f64,
    /// Ignore the saver state while charging.
    pub ignore_while_charging: bool,
}

#[derive(Debug, Clone)]
pub struct AppLaunchPolicy {
    pub boost_ms: u64,
}

#[derive(Debug, Clone)]
pub struct GamePolicy {
    pub packages: Vec<String>,
    pub detect_by_gpu_load: bool,
    pub gpu_load_threshold: f64,
}

#[derive(Debug, Clone)]
pub struct IdlePolicy {
    pub screen_off_after_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ActuatorConfig {
    /// Independent software consent gate. Missing fields in older configs stay off.
    pub enabled: bool,
    /// Write `cpu.uclamp.min/max` on the top-app cgroup. Off until verified on device.
    pub enable_uclamp_top_app: bool,
    /// Write `/proc/sys/kernel/sched_util_clamp_min|max`. Global, off by default.
    pub enable_sched_util_clamp: bool,
    pub profiles: Vec<(PolicyState, Profile)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Profile {
    pub uclamp_min: u32,
    pub uclamp_max: u32,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            uclamp_min: 0,
            uclamp_max: 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PredictorConfig {
    pub enabled: bool,
    pub min_samples: u64,
    pub min_confidence: f64,
    pub max_candidates: usize,
    pub persist_interval_ms: u64,
}

#[derive(Debug, Clone)]
pub struct SafetyConfig {
    pub watchdog_interval_ms: u64,
    pub consecutive_failure_limit: u32,
    pub safe_mode_boots: u32,
    /// What to do when another service clobbers a node we own.
    pub on_conflict: ConflictAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    /// Stop writing that node for the rest of the session and log it.
    Release,
    /// Keep writing (only for users who know the other service is gone).
    Ignore,
}

impl ConflictAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictAction::Release => "release",
            ConflictAction::Ignore => "ignore",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "release" => Some(ConflictAction::Release),
            "ignore" => Some(ConflictAction::Ignore),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppsConfig {
    pub launchers: Vec<String>,
    pub event_source: String,
    pub companion_log: String,
}

#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub telemetry: bool,
    pub rotate_kb: u64,
    pub keep_files: usize,
    pub data_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            mode: RunMode::ObserveOnly,
            sample: SampleConfig {
                interval_ms: 1_000,
                screen_off_interval_ms: 5_000,
                thermal_interval_ms: 4_000,
                history_interval_ms: 60_000,
                proc_scan_interval_ms: 5_000,
            },
            log: LogConfig {
                level: "info".into(),
                ring: 2_000,
                file_rotate_kb: 512,
                keep_files: 5,
            },
            api: ApiConfig {
                enabled: true,
                bind: "127.0.0.1".into(),
                port: 8787,
            },
            policy: PolicyConfig {
                min_dwell_ms: 4_000,
                cooldown_ms: 8_000,
                boot_grace_ms: 90_000,
                thermal: ThermalPolicy {
                    enter_light_c: 65.0,
                    enter_moderate_c: 75.0,
                    enter_severe_c: 83.0,
                    enter_critical_c: 90.0,
                    exit_margin_c: 5.0,
                    include_skin: false,
                },
                memory: MemoryPolicy {
                    psi_some_enter: 12.0,
                    psi_some_exit: 4.0,
                    psi_full_enter: 3.0,
                    psi_full_exit: 0.5,
                    avail_pct_enter: 8.0,
                    avail_pct_exit: 14.0,
                    kswapd_pct_enter: 25.0,
                    kswapd_pct_exit: 8.0,
                    swap_in_rate_enter: 4096.0,
                },
                battery: BatteryPolicy {
                    saver_pct: 15.0,
                    exit_pct: 20.0,
                    ignore_while_charging: true,
                },
                app_launch: AppLaunchPolicy { boost_ms: 2_500 },
                game: GamePolicy {
                    packages: Vec::new(),
                    detect_by_gpu_load: false,
                    gpu_load_threshold: 70.0,
                },
                idle: IdlePolicy {
                    screen_off_after_ms: 5_000,
                },
            },
            actuator: ActuatorConfig {
                enabled: false,
                enable_uclamp_top_app: true,
                enable_sched_util_clamp: false,
                profiles: default_profiles(),
            },
            predictor: PredictorConfig {
                enabled: true,
                min_samples: 40,
                min_confidence: 0.35,
                max_candidates: 5,
                persist_interval_ms: 300_000,
            },
            safety: SafetyConfig {
                watchdog_interval_ms: 10_000,
                consecutive_failure_limit: 3,
                safe_mode_boots: 3,
                on_conflict: ConflictAction::Release,
            },
            apps: AppsConfig {
                launchers: vec!["com.miui.home".into(), "com.android.launcher3".into()],
                event_source: "auto".into(),
                companion_log: "fg_events.log".into(),
            },
            storage: StorageConfig {
                telemetry: true,
                rotate_kb: 8 * 1024,
                keep_files: 8,
                data_dir: "/data/adb/neurosched".into(),
            },
        }
    }
}

/// Deliberately conservative profiles. `uclamp.max` below 1024 caps the *ceiling*
/// of the top-app task, which is the only lever v1 is allowed to use.
fn default_profiles() -> Vec<(PolicyState, Profile)> {
    use PolicyState::*;
    vec![
        (
            SafeMode,
            Profile {
                uclamp_min: 0,
                uclamp_max: 1024,
            },
        ),
        (
            ThermalGuard,
            Profile {
                uclamp_min: 0,
                uclamp_max: 640,
            },
        ),
        (
            MemoryPressure,
            Profile {
                uclamp_min: 0,
                uclamp_max: 768,
            },
        ),
        (
            BatterySaver,
            Profile {
                uclamp_min: 0,
                uclamp_max: 512,
            },
        ),
        (
            AppLaunch,
            Profile {
                uclamp_min: 320,
                uclamp_max: 1024,
            },
        ),
        (
            Game,
            Profile {
                uclamp_min: 256,
                uclamp_max: 1024,
            },
        ),
        (
            Foreground,
            Profile {
                uclamp_min: 128,
                uclamp_max: 1024,
            },
        ),
        (
            Balanced,
            Profile {
                uclamp_min: 0,
                uclamp_max: 1024,
            },
        ),
        (
            Idle,
            Profile {
                uclamp_min: 0,
                uclamp_max: 1024,
            },
        ),
        (
            Boot,
            Profile {
                uclamp_min: 0,
                uclamp_max: 1024,
            },
        ),
    ]
}

impl Config {
    pub fn profile_for(&self, state: PolicyState) -> Profile {
        self.actuator
            .profiles
            .iter()
            .find(|(s, _)| *s == state)
            .map(|(_, p)| *p)
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        j.set("version", Json::num(self.version as f64));
        j.set("mode", Json::str(self.mode.as_str()));

        let mut sample = Json::obj();
        sample.set("interval_ms", Json::num(self.sample.interval_ms as f64));
        sample.set(
            "screen_off_interval_ms",
            Json::num(self.sample.screen_off_interval_ms as f64),
        );
        sample.set(
            "thermal_interval_ms",
            Json::num(self.sample.thermal_interval_ms as f64),
        );
        sample.set(
            "history_interval_ms",
            Json::num(self.sample.history_interval_ms as f64),
        );
        sample.set(
            "proc_scan_interval_ms",
            Json::num(self.sample.proc_scan_interval_ms as f64),
        );
        j.set("sample", sample);

        let mut log = Json::obj();
        log.set("level", Json::str(self.log.level.clone()));
        log.set("ring", Json::num(self.log.ring as f64));
        log.set("file_rotate_kb", Json::num(self.log.file_rotate_kb as f64));
        log.set("keep_files", Json::num(self.log.keep_files as f64));
        j.set("log", log);

        let mut api = Json::obj();
        api.set("enabled", Json::Bool(self.api.enabled));
        api.set("bind", Json::str(self.api.bind.clone()));
        api.set("port", Json::num(self.api.port as f64));
        j.set("api", api);

        let mut policy = Json::obj();
        policy.set("min_dwell_ms", Json::num(self.policy.min_dwell_ms as f64));
        policy.set("cooldown_ms", Json::num(self.policy.cooldown_ms as f64));
        policy.set("boot_grace_ms", Json::num(self.policy.boot_grace_ms as f64));

        let t = &self.policy.thermal;
        let mut thermal = Json::obj();
        thermal.set("enter_light_c", Json::num(t.enter_light_c));
        thermal.set("enter_moderate_c", Json::num(t.enter_moderate_c));
        thermal.set("enter_severe_c", Json::num(t.enter_severe_c));
        thermal.set("enter_critical_c", Json::num(t.enter_critical_c));
        thermal.set("exit_margin_c", Json::num(t.exit_margin_c));
        thermal.set("include_skin", Json::Bool(t.include_skin));
        policy.set("thermal", thermal);

        let m = &self.policy.memory;
        let mut mem = Json::obj();
        mem.set("psi_some_enter", Json::num(m.psi_some_enter));
        mem.set("psi_some_exit", Json::num(m.psi_some_exit));
        mem.set("psi_full_enter", Json::num(m.psi_full_enter));
        mem.set("psi_full_exit", Json::num(m.psi_full_exit));
        mem.set("avail_pct_enter", Json::num(m.avail_pct_enter));
        mem.set("avail_pct_exit", Json::num(m.avail_pct_exit));
        mem.set("kswapd_pct_enter", Json::num(m.kswapd_pct_enter));
        mem.set("kswapd_pct_exit", Json::num(m.kswapd_pct_exit));
        mem.set("swap_in_rate_enter", Json::num(m.swap_in_rate_enter));
        policy.set("memory", mem);

        let b = &self.policy.battery;
        let mut batt = Json::obj();
        batt.set("saver_pct", Json::num(b.saver_pct));
        batt.set("exit_pct", Json::num(b.exit_pct));
        batt.set("ignore_while_charging", Json::Bool(b.ignore_while_charging));
        policy.set("battery", batt);

        let mut launch = Json::obj();
        launch.set(
            "boost_ms",
            Json::num(self.policy.app_launch.boost_ms as f64),
        );
        policy.set("app_launch", launch);

        let mut game = Json::obj();
        game.set(
            "packages",
            Json::Arr(
                self.policy
                    .game
                    .packages
                    .iter()
                    .cloned()
                    .map(Json::Str)
                    .collect(),
            ),
        );
        game.set(
            "detect_by_gpu_load",
            Json::Bool(self.policy.game.detect_by_gpu_load),
        );
        game.set(
            "gpu_load_threshold",
            Json::num(self.policy.game.gpu_load_threshold),
        );
        policy.set("game", game);

        let mut idle = Json::obj();
        idle.set(
            "screen_off_after_ms",
            Json::num(self.policy.idle.screen_off_after_ms as f64),
        );
        policy.set("idle", idle);
        j.set("policy", policy);

        let mut actuator = Json::obj();
        actuator.set("enabled", Json::Bool(self.actuator.enabled));
        actuator.set(
            "enable_uclamp_top_app",
            Json::Bool(self.actuator.enable_uclamp_top_app),
        );
        actuator.set(
            "enable_sched_util_clamp",
            Json::Bool(self.actuator.enable_sched_util_clamp),
        );
        let mut profiles = Json::obj();
        for (state, p) in &self.actuator.profiles {
            let mut pj = Json::obj();
            pj.set("uclamp_min", Json::num(p.uclamp_min as f64));
            pj.set("uclamp_max", Json::num(p.uclamp_max as f64));
            profiles.set(state.as_str(), pj);
        }
        actuator.set("profiles", profiles);
        j.set("actuator", actuator);

        let mut pred = Json::obj();
        pred.set("enabled", Json::Bool(self.predictor.enabled));
        pred.set("min_samples", Json::num(self.predictor.min_samples as f64));
        pred.set("min_confidence", Json::num(self.predictor.min_confidence));
        pred.set(
            "max_candidates",
            Json::num(self.predictor.max_candidates as f64),
        );
        pred.set(
            "persist_interval_ms",
            Json::num(self.predictor.persist_interval_ms as f64),
        );
        j.set("predictor", pred);

        let mut safety = Json::obj();
        safety.set(
            "watchdog_interval_ms",
            Json::num(self.safety.watchdog_interval_ms as f64),
        );
        safety.set(
            "consecutive_failure_limit",
            Json::num(self.safety.consecutive_failure_limit as f64),
        );
        safety.set(
            "safe_mode_boots",
            Json::num(self.safety.safe_mode_boots as f64),
        );
        safety.set("on_conflict", Json::str(self.safety.on_conflict.as_str()));
        j.set("safety", safety);

        let mut apps = Json::obj();
        apps.set(
            "launchers",
            Json::Arr(self.apps.launchers.iter().cloned().map(Json::Str).collect()),
        );
        apps.set("event_source", Json::str(self.apps.event_source.clone()));
        apps.set("companion_log", Json::str(self.apps.companion_log.clone()));
        j.set("apps", apps);

        let mut storage = Json::obj();
        storage.set("telemetry", Json::Bool(self.storage.telemetry));
        storage.set("rotate_kb", Json::num(self.storage.rotate_kb as f64));
        storage.set("keep_files", Json::num(self.storage.keep_files as f64));
        storage.set("data_dir", Json::str(self.storage.data_dir.clone()));
        j.set("storage", storage);

        j
    }

    pub fn from_json(j: &Json) -> Config {
        let d = Config::default();
        let mut c = Config {
            version: ju(j, "version", d.version as f64) as u32,
            mode: j
                .get("mode")
                .and_then(|v| v.as_str())
                .and_then(RunMode::parse)
                .unwrap_or(d.mode),
            sample: SampleConfig {
                interval_ms: ju(j, "sample.interval_ms", d.sample.interval_ms as f64),
                screen_off_interval_ms: ju(
                    j,
                    "sample.screen_off_interval_ms",
                    d.sample.screen_off_interval_ms as f64,
                ),
                thermal_interval_ms: ju(
                    j,
                    "sample.thermal_interval_ms",
                    d.sample.thermal_interval_ms as f64,
                ),
                history_interval_ms: ju(
                    j,
                    "sample.history_interval_ms",
                    d.sample.history_interval_ms as f64,
                ),
                proc_scan_interval_ms: ju(
                    j,
                    "sample.proc_scan_interval_ms",
                    d.sample.proc_scan_interval_ms as f64,
                ),
            },
            log: LogConfig {
                level: js(j, "log.level", &d.log.level),
                ring: ju(j, "log.ring", d.log.ring as f64) as usize,
                file_rotate_kb: ju(j, "log.file_rotate_kb", d.log.file_rotate_kb as f64),
                keep_files: ju(j, "log.keep_files", d.log.keep_files as f64) as usize,
            },
            api: ApiConfig {
                enabled: j
                    .get_path("api.enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(d.api.enabled),
                bind: js(j, "api.bind", &d.api.bind),
                port: ju(j, "api.port", d.api.port as f64).clamp(1, 65_535) as u16,
            },
            policy: PolicyConfig {
                min_dwell_ms: ju(j, "policy.min_dwell_ms", d.policy.min_dwell_ms as f64),
                cooldown_ms: ju(j, "policy.cooldown_ms", d.policy.cooldown_ms as f64),
                boot_grace_ms: ju(j, "policy.boot_grace_ms", d.policy.boot_grace_ms as f64),
                thermal: ThermalPolicy {
                    enter_light_c: jf(
                        j,
                        "policy.thermal.enter_light_c",
                        d.policy.thermal.enter_light_c,
                    ),
                    enter_moderate_c: jf(
                        j,
                        "policy.thermal.enter_moderate_c",
                        d.policy.thermal.enter_moderate_c,
                    ),
                    enter_severe_c: jf(
                        j,
                        "policy.thermal.enter_severe_c",
                        d.policy.thermal.enter_severe_c,
                    ),
                    enter_critical_c: jf(
                        j,
                        "policy.thermal.enter_critical_c",
                        d.policy.thermal.enter_critical_c,
                    ),
                    exit_margin_c: jf(
                        j,
                        "policy.thermal.exit_margin_c",
                        d.policy.thermal.exit_margin_c,
                    ),
                    include_skin: j
                        .get_path("policy.thermal.include_skin")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(d.policy.thermal.include_skin),
                },
                memory: MemoryPolicy {
                    psi_some_enter: jf(
                        j,
                        "policy.memory.psi_some_enter",
                        d.policy.memory.psi_some_enter,
                    ),
                    psi_some_exit: jf(
                        j,
                        "policy.memory.psi_some_exit",
                        d.policy.memory.psi_some_exit,
                    ),
                    psi_full_enter: jf(
                        j,
                        "policy.memory.psi_full_enter",
                        d.policy.memory.psi_full_enter,
                    ),
                    psi_full_exit: jf(
                        j,
                        "policy.memory.psi_full_exit",
                        d.policy.memory.psi_full_exit,
                    ),
                    avail_pct_enter: jf(
                        j,
                        "policy.memory.avail_pct_enter",
                        d.policy.memory.avail_pct_enter,
                    ),
                    avail_pct_exit: jf(
                        j,
                        "policy.memory.avail_pct_exit",
                        d.policy.memory.avail_pct_exit,
                    ),
                    kswapd_pct_enter: jf(
                        j,
                        "policy.memory.kswapd_pct_enter",
                        d.policy.memory.kswapd_pct_enter,
                    ),
                    kswapd_pct_exit: jf(
                        j,
                        "policy.memory.kswapd_pct_exit",
                        d.policy.memory.kswapd_pct_exit,
                    ),
                    swap_in_rate_enter: jf(
                        j,
                        "policy.memory.swap_in_rate_enter",
                        d.policy.memory.swap_in_rate_enter,
                    ),
                },
                battery: BatteryPolicy {
                    saver_pct: jf(j, "policy.battery.saver_pct", d.policy.battery.saver_pct),
                    exit_pct: jf(j, "policy.battery.exit_pct", d.policy.battery.exit_pct),
                    ignore_while_charging: j
                        .get_path("policy.battery.ignore_while_charging")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(d.policy.battery.ignore_while_charging),
                },
                app_launch: AppLaunchPolicy {
                    boost_ms: ju(
                        j,
                        "policy.app_launch.boost_ms",
                        d.policy.app_launch.boost_ms as f64,
                    ),
                },
                game: GamePolicy {
                    packages: jstr_list(j, "policy.game.packages")
                        .unwrap_or_else(|| d.policy.game.packages.clone()),
                    detect_by_gpu_load: j
                        .get_path("policy.game.detect_by_gpu_load")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(d.policy.game.detect_by_gpu_load),
                    gpu_load_threshold: jf(
                        j,
                        "policy.game.gpu_load_threshold",
                        d.policy.game.gpu_load_threshold,
                    ),
                },
                idle: IdlePolicy {
                    screen_off_after_ms: ju(
                        j,
                        "policy.idle.screen_off_after_ms",
                        d.policy.idle.screen_off_after_ms as f64,
                    ),
                },
            },
            actuator: ActuatorConfig {
                enabled: j
                    .get_path("actuator.enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                enable_uclamp_top_app: j
                    .get_path("actuator.enable_uclamp_top_app")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(d.actuator.enable_uclamp_top_app),
                enable_sched_util_clamp: j
                    .get_path("actuator.enable_sched_util_clamp")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(d.actuator.enable_sched_util_clamp),
                profiles: parse_profiles(j, &d),
            },
            predictor: PredictorConfig {
                enabled: j
                    .get_path("predictor.enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(d.predictor.enabled),
                min_samples: ju(j, "predictor.min_samples", d.predictor.min_samples as f64),
                min_confidence: jf(j, "predictor.min_confidence", d.predictor.min_confidence),
                max_candidates: ju(
                    j,
                    "predictor.max_candidates",
                    d.predictor.max_candidates as f64,
                ) as usize,
                persist_interval_ms: ju(
                    j,
                    "predictor.persist_interval_ms",
                    d.predictor.persist_interval_ms as f64,
                ),
            },
            safety: SafetyConfig {
                watchdog_interval_ms: ju(
                    j,
                    "safety.watchdog_interval_ms",
                    d.safety.watchdog_interval_ms as f64,
                ),
                consecutive_failure_limit: ju(
                    j,
                    "safety.consecutive_failure_limit",
                    d.safety.consecutive_failure_limit as f64,
                ) as u32,
                safe_mode_boots: ju(j, "safety.safe_mode_boots", d.safety.safe_mode_boots as f64)
                    as u32,
                on_conflict: j
                    .get_path("safety.on_conflict")
                    .and_then(|v| v.as_str())
                    .and_then(ConflictAction::parse)
                    .unwrap_or(d.safety.on_conflict),
            },
            apps: AppsConfig {
                launchers: jstr_list(j, "apps.launchers")
                    .unwrap_or_else(|| d.apps.launchers.clone()),
                event_source: js(j, "apps.event_source", &d.apps.event_source),
                companion_log: js(j, "apps.companion_log", &d.apps.companion_log),
            },
            storage: StorageConfig {
                telemetry: j
                    .get_path("storage.telemetry")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(d.storage.telemetry),
                rotate_kb: ju(j, "storage.rotate_kb", d.storage.rotate_kb as f64),
                keep_files: ju(j, "storage.keep_files", d.storage.keep_files as f64) as usize,
                data_dir: js(j, "storage.data_dir", &d.storage.data_dir),
            },
        };
        c.validate();
        c
    }

    /// Clamp everything into a sane range. A malformed config must degrade into
    /// "do less", never into "write a wild value".
    pub fn validate(&mut self) {
        self.sample.interval_ms = self.sample.interval_ms.clamp(500, 60_000);
        self.sample.screen_off_interval_ms = self
            .sample
            .screen_off_interval_ms
            .clamp(self.sample.interval_ms, 120_000);
        self.sample.thermal_interval_ms = self.sample.thermal_interval_ms.clamp(1_000, 60_000);
        self.sample.history_interval_ms = self.sample.history_interval_ms.clamp(5_000, 3_600_000);
        self.sample.proc_scan_interval_ms = self.sample.proc_scan_interval_ms.clamp(1_000, 60_000);

        self.log.ring = self.log.ring.clamp(100, 20_000);
        self.log.keep_files = self.log.keep_files.clamp(1, 30);
        self.log.file_rotate_kb = self.log.file_rotate_kb.clamp(64, 16_384);

        if !self.api.bind.starts_with("127.")
            && self.api.bind != "::1"
            && self.api.bind != "localhost"
        {
            // The API must never listen on a public interface.
            self.api.bind = "127.0.0.1".into();
        }

        let t = &mut self.policy.thermal;
        t.enter_light_c = t.enter_light_c.clamp(30.0, 95.0);
        t.enter_moderate_c = t.enter_moderate_c.clamp(t.enter_light_c + 1.0, 100.0);
        t.enter_severe_c = t.enter_severe_c.clamp(t.enter_moderate_c + 1.0, 105.0);
        t.enter_critical_c = t.enter_critical_c.clamp(t.enter_severe_c + 1.0, 110.0);
        t.exit_margin_c = t.exit_margin_c.clamp(1.0, 15.0);

        let m = &mut self.policy.memory;
        m.psi_some_enter = m.psi_some_enter.clamp(1.0, 100.0);
        m.psi_some_exit = m.psi_some_exit.clamp(0.0, m.psi_some_enter - 0.5);
        m.psi_full_enter = m.psi_full_enter.clamp(0.5, 100.0);
        m.psi_full_exit = m.psi_full_exit.clamp(0.0, m.psi_full_enter - 0.1);
        m.avail_pct_enter = m.avail_pct_enter.clamp(1.0, 50.0);
        m.avail_pct_exit = m.avail_pct_exit.clamp(m.avail_pct_enter + 0.5, 80.0);
        m.kswapd_pct_enter = m.kswapd_pct_enter.clamp(1.0, 100.0);
        m.kswapd_pct_exit = m.kswapd_pct_exit.clamp(0.0, m.kswapd_pct_enter - 0.5);
        m.swap_in_rate_enter = m.swap_in_rate_enter.clamp(0.0, 1_048_576.0);

        self.policy.battery.saver_pct = self.policy.battery.saver_pct.clamp(1.0, 60.0);
        self.policy.battery.exit_pct = self
            .policy
            .battery
            .exit_pct
            .clamp(self.policy.battery.saver_pct + 1.0, 90.0);

        self.policy.app_launch.boost_ms = self.policy.app_launch.boost_ms.clamp(500, 10_000);
        self.policy.min_dwell_ms = self.policy.min_dwell_ms.clamp(500, 120_000);
        self.policy.cooldown_ms = self.policy.cooldown_ms.clamp(0, 300_000);
        self.policy.boot_grace_ms = self.policy.boot_grace_ms.clamp(5_000, 600_000);

        self.predictor.min_samples = self.predictor.min_samples.clamp(5, 100_000);
        self.predictor.min_confidence = self.predictor.min_confidence.clamp(0.05, 0.95);
        self.predictor.max_candidates = self.predictor.max_candidates.clamp(1, 20);

        self.safety.watchdog_interval_ms = self.safety.watchdog_interval_ms.clamp(1_000, 120_000);
        self.safety.consecutive_failure_limit = self.safety.consecutive_failure_limit.clamp(1, 20);
        self.safety.safe_mode_boots = self.safety.safe_mode_boots.clamp(1, 20);

        self.storage.rotate_kb = self.storage.rotate_kb.clamp(64, 1_048_576);
        self.storage.keep_files = self.storage.keep_files.clamp(1, 100);

        for (_, p) in self.actuator.profiles.iter_mut() {
            p.uclamp_min = p.uclamp_min.min(1024);
            p.uclamp_max = p.uclamp_max.min(1024);
            if p.uclamp_min > p.uclamp_max {
                p.uclamp_min = p.uclamp_max;
            }
        }
        // Guarantee every state has a profile even after a hand-edited config.
        for state in PolicyState::all() {
            if !self.actuator.profiles.iter().any(|(s, _)| *s == state) {
                self.actuator.profiles.push((state, Profile::default()));
            }
        }
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let j = json::parse(&text)?;
        Ok(Config::from_json(&j))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, self.to_json().to_string_pretty())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
        crate::storage::secure_file(path);
        // Keep the last known-good file so a corrupted write is recoverable without
        // falling back to defaults and losing the user's thresholds.
        let backup = path.with_extension("json.bak");
        let _ = std::fs::copy(path, &backup);
        crate::storage::secure_file(&backup);
        Ok(())
    }

    /// Loads the config, recovering from corruption by quarantining the bad file.
    /// Returns the config plus an optional human readable warning.
    pub fn load_or_recover(path: &Path) -> (Config, Option<String>) {
        match Config::load(path) {
            Ok(c) => (c, None),
            Err(e) => {
                if !path.exists() {
                    return (Config::default(), None);
                }
                // Quarantine the broken file first: never overwrite evidence.
                let stamp = crate::util::now_ms();
                let bad: PathBuf = path.with_extension(format!("bad.{stamp}.json"));
                let _ = std::fs::rename(path, &bad);

                // Prefer the last known-good copy over defaults: losing tuned
                // thresholds because of a truncated write would be a regression.
                let backup = path.with_extension("json.bak");
                if let Ok(good) = Config::load(&backup) {
                    let _ = good.save(path);
                    return (
                        good,
                        Some(format!(
                            "config unreadable ({e}); quarantined as {} and restored from {}",
                            bad.display(),
                            backup.display()
                        )),
                    );
                }
                (
                    Config::default(),
                    Some(format!(
                        "config unreadable ({e}); moved to {} and defaults restored",
                        bad.display()
                    )),
                )
            }
        }
    }
}

fn jf(j: &Json, path: &str, default: f64) -> f64 {
    j.get_path(path).and_then(|v| v.as_f64()).unwrap_or(default)
}

fn ju(j: &Json, path: &str, default: f64) -> u64 {
    j.get_path(path)
        .and_then(|v| v.as_f64())
        .map(|v| v.max(0.0) as u64)
        .unwrap_or(default.max(0.0) as u64)
}

fn js(j: &Json, path: &str, default: &str) -> String {
    j.get_path(path)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

fn jstr_list(j: &Json, path: &str) -> Option<Vec<String>> {
    let arr = j.get_path(path)?.as_arr();
    Some(
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

fn parse_profiles(j: &Json, d: &Config) -> Vec<(PolicyState, Profile)> {
    let mut out = d.actuator.profiles.clone();
    if let Some(obj) = j.get_path("actuator.profiles") {
        for state in PolicyState::all() {
            if let Some(pj) = obj.get(state.as_str()) {
                if let Some(slot) = out.iter_mut().find(|(s, _)| *s == state) {
                    slot.1.uclamp_min =
                        pj.get("uclamp_min")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(slot.1.uclamp_min as u64) as u32;
                    slot.1.uclamp_max =
                        pj.get("uclamp_max")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(slot.1.uclamp_max as u64) as u32;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_inert() {
        let c = Config::default();
        assert_eq!(c.mode, RunMode::ObserveOnly);
        assert!(!c.mode.writes_allowed());
        assert!(!c.actuator.enabled);
        assert_eq!(c.api.bind, "127.0.0.1");
        assert!(!c.actuator.enable_sched_util_clamp);
    }

    #[test]
    fn json_roundtrip_preserves_values() {
        let mut c = Config::default();
        c.policy.thermal.enter_severe_c = 85.5;
        c.actuator.profiles[0].1.uclamp_max = 900;
        let text = c.to_json().to_string_pretty();
        let back = Config::from_json(&json::parse(&text).unwrap());
        assert!((back.policy.thermal.enter_severe_c - 85.5).abs() < 1e-6);
        assert_eq!(back.profile_for(PolicyState::SafeMode).uclamp_max, 900);
    }

    #[test]
    fn legacy_enabled_mode_needs_independent_consent() {
        let cfg = Config::from_json(&json::parse(r#"{"mode":"enabled"}"#).unwrap());
        assert!(!cfg.actuator.enabled);
    }

    #[test]
    fn invalid_values_are_clamped() {
        let mut c = Config::default();
        c.api.bind = "0.0.0.0".into();
        c.policy.thermal.enter_light_c = 90.0;
        c.policy.thermal.enter_moderate_c = 20.0;
        c.policy.memory.psi_some_exit = 999.0;
        c.actuator.profiles[0].1.uclamp_min = 5000;
        c.validate();
        assert_eq!(c.api.bind, "127.0.0.1");
        assert!(c.policy.thermal.enter_moderate_c > c.policy.thermal.enter_light_c);
        assert!(c.policy.memory.psi_some_exit < c.policy.memory.psi_some_enter);
        assert!(c.actuator.profiles[0].1.uclamp_min <= 1024);
    }

    #[test]
    fn corrupt_config_is_quarantined() {
        let dir = std::env::temp_dir().join("neurosched-cfg-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        std::fs::write(&p, "{ this is not json").unwrap();
        let (c, warn) = Config::load_or_recover(&p);
        assert!(warn.is_some());
        assert_eq!(c.mode, RunMode::ObserveOnly);
        assert!(!p.exists());
    }

    #[test]
    fn corrupt_config_recovers_from_backup() {
        let dir = std::env::temp_dir().join("neurosched-cfg-bak");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let mut c = Config::default();
        c.policy.thermal.enter_light_c = 47.5;
        c.save(&p).expect("save"); // also writes config.json.bak
        std::fs::write(&p, "{ truncated").unwrap();

        let (restored, warn) = Config::load_or_recover(&p);
        assert!(warn.is_some());
        assert!((restored.policy.thermal.enter_light_c - 47.5).abs() < 1e-9);
        assert!(
            Config::load(&p).is_ok(),
            "file must be rewritten from the backup"
        );
    }

    #[test]
    fn save_then_load() {
        let dir = std::env::temp_dir().join("neurosched-cfg-save");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.json");
        let c = Config::default();
        c.save(&p).expect("save");
        let back = Config::load(&p).expect("load");
        assert_eq!(back.mode, c.mode);
        assert_eq!(back.policy.game.packages.len(), 0);
    }
}
