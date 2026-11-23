//! Shared data types. Kept dependency-free so every layer can speak in terms of a
//! single immutable [`Snapshot`] instead of re-reading sysfs.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Read everything, write nothing. Default.
    ObserveOnly,
    /// Compute and log the exact writes that *would* happen. Writes nothing.
    DryRun,
    /// Perform the writes allowed by the current capability set.
    Enabled,
}

impl RunMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RunMode::ObserveOnly => "observe_only",
            RunMode::DryRun => "dry_run",
            RunMode::Enabled => "enabled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observe_only" | "observe" | "read_only" | "readonly" => Some(RunMode::ObserveOnly),
            "dry_run" | "dryrun" | "dry-run" | "dry" => Some(RunMode::DryRun),
            "enabled" | "active" | "apply" => Some(RunMode::Enabled),
            _ => None,
        }
    }

    pub fn writes_allowed(self) -> bool {
        matches!(self, RunMode::Enabled)
    }
}

/// Policy states, ordered by priority (highest first) in [`PolicyState::priority`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PolicyState {
    Boot,
    Idle,
    Balanced,
    Foreground,
    AppLaunch,
    Game,
    MemoryPressure,
    ThermalGuard,
    BatterySaver,
    SafeMode,
}

impl PolicyState {
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyState::Boot => "BOOT",
            PolicyState::Idle => "IDLE",
            PolicyState::Balanced => "BALANCED",
            PolicyState::Foreground => "FOREGROUND",
            PolicyState::AppLaunch => "APP_LAUNCH",
            PolicyState::Game => "GAME",
            PolicyState::MemoryPressure => "MEMORY_PRESSURE",
            PolicyState::ThermalGuard => "THERMAL_GUARD",
            PolicyState::BatterySaver => "BATTERY_SAVER",
            PolicyState::SafeMode => "SAFE_MODE",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::all()
            .iter()
            .copied()
            .find(|st| st.as_str().eq_ignore_ascii_case(s.trim()))
    }

    pub fn all() -> [PolicyState; 10] {
        [
            PolicyState::Boot,
            PolicyState::Idle,
            PolicyState::Balanced,
            PolicyState::Foreground,
            PolicyState::AppLaunch,
            PolicyState::Game,
            PolicyState::MemoryPressure,
            PolicyState::ThermalGuard,
            PolicyState::BatterySaver,
            PolicyState::SafeMode,
        ]
    }

    /// Larger number wins when several states are simultaneously eligible.
    pub fn priority(self) -> u8 {
        match self {
            PolicyState::SafeMode => 100,
            PolicyState::ThermalGuard => 90,
            PolicyState::MemoryPressure => 80,
            PolicyState::BatterySaver => 70,
            PolicyState::AppLaunch => 60,
            PolicyState::Game => 50,
            PolicyState::Foreground => 40,
            PolicyState::Balanced => 30,
            PolicyState::Idle => 20,
            PolicyState::Boot => 10,
        }
    }

    /// States that must be held for a while before leaving, to stop flapping.
    pub fn min_dwell_ms(self) -> u64 {
        match self {
            PolicyState::Boot => 60_000,
            PolicyState::AppLaunch => 0,
            PolicyState::ThermalGuard => 20_000,
            PolicyState::MemoryPressure => 10_000,
            PolicyState::BatterySaver => 15_000,
            _ => 4_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThermalLevel {
    Normal,
    Light,
    Moderate,
    Severe,
    Critical,
    Emergency,
}

impl ThermalLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ThermalLevel::Normal => "NONE",
            ThermalLevel::Light => "LIGHT",
            ThermalLevel::Moderate => "MODERATE",
            ThermalLevel::Severe => "SEVERE",
            ThermalLevel::Critical => "CRITICAL",
            ThermalLevel::Emergency => "EMERGENCY",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScreenState {
    On,
    Off,
    #[default]
    Unknown,
}

impl ScreenState {
    pub fn as_str(self) -> &'static str {
        match self {
            ScreenState::On => "on",
            ScreenState::Off => "off",
            ScreenState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CoreLoad {
    pub id: u32,
    pub usage_pct: f64,
    pub freq_khz: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ClusterInfo {
    /// Human label derived from measured capability, never from CPU index.
    pub name: String,
    pub cpus: Vec<u32>,
    pub governor: String,
    pub available_governors: Vec<String>,
    pub cur_khz: u32,
    pub min_khz: u32,
    pub max_khz: u32,
    pub cpuinfo_max_khz: u32,
    pub capacity: Option<u32>,
    pub load_pct: f64,
    pub policy_path: String,
}

#[derive(Debug, Clone, Default)]
pub struct CpuSnapshot {
    pub total_pct: f64,
    pub cores: Vec<CoreLoad>,
    pub clusters: Vec<ClusterInfo>,
    pub present: Vec<u32>,
    pub online: Vec<u32>,
    /// Scheduler knobs discovered on this device (paths only, never probed by writing).
    pub knobs: BTreeMap<String, String>,
    pub walt_present: bool,
    pub uclamp_supported: bool,
}

#[derive(Debug, Clone)]
pub struct ZramDisk {
    pub name: String,
    pub orig_kb: f64,
    pub compr_kb: f64,
    pub mem_used_kb: f64,
    pub limit_kb: f64,
    pub ratio: f64,
}

#[derive(Debug, Clone, Default)]
pub struct MmdInfo {
    pub present: bool,
    pub pid: Option<u32>,
    pub service_state: Option<String>,
    pub init_service: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MemSnapshot {
    pub total_kb: f64,
    pub available_kb: f64,
    pub free_kb: f64,
    pub cached_kb: f64,
    pub swap_total_kb: f64,
    pub swap_free_kb: f64,
    pub swap_devices: Vec<String>,
    pub zram: Vec<ZramDisk>,
    pub pswpin: f64,
    pub pswpout: f64,
    /// KiB/s paged in since the previous sample. The single most useful thrashing signal.
    pub swap_in_rate_kb_s: f64,
    pub swap_out_rate_kb_s: f64,
    pub pgscan_kswapd: f64,
    pub pgsteal_kswapd: f64,
    pub pgmajfault: f64,
    pub kswapd_cpu_pct: f64,
    pub kswapd_pid: Option<u32>,
    pub mmd: MmdInfo,
    pub meminfo_ok: bool,
}

impl MemSnapshot {
    pub fn swap_used_kb(&self) -> f64 {
        (self.swap_total_kb - self.swap_free_kb).max(0.0)
    }
    pub fn available_pct(&self) -> f64 {
        if self.total_kb > 0.0 {
            self.available_kb / self.total_kb * 100.0
        } else {
            0.0
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PsiRes {
    pub available: bool,
    pub some10: f64,
    pub some60: f64,
    pub some300: f64,
    pub some_total: f64,
    pub full10: f64,
    pub full60: f64,
    pub full300: f64,
    pub full_total: f64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PsiSnapshot {
    pub cpu: PsiRes,
    pub memory: PsiRes,
    pub io: PsiRes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneKind {
    Cpu,
    Gpu,
    Skin,
    Battery,
    Charger,
    Display,
    Other,
}

impl ZoneKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ZoneKind::Cpu => "cpu",
            ZoneKind::Gpu => "gpu",
            ZoneKind::Skin => "skin",
            ZoneKind::Battery => "battery",
            ZoneKind::Charger => "charger",
            ZoneKind::Display => "display",
            ZoneKind::Other => "other",
        }
    }

    /// Classification is by the zone's own `type` string, never by index and never by
    /// "the highest value wins".
    pub fn classify(zone_type: &str) -> ZoneKind {
        let t = zone_type.to_ascii_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
        if has(&["batt", "chg", "charger", "usb"]) {
            if has(&["batt"]) {
                ZoneKind::Battery
            } else {
                ZoneKind::Charger
            }
        } else if has(&["gpu", "adreno", "kgsl"]) {
            ZoneKind::Gpu
        } else if has(&["skin", "back", "shell", "frame", "cover"]) {
            ZoneKind::Skin
        } else if has(&["disp", "panel", "ddic"]) {
            ZoneKind::Display
        } else if has(&[
            "cpu", "soc", "apc", "cluster", "core", "silver", "gold", "prime",
        ]) {
            ZoneKind::Cpu
        } else {
            ZoneKind::Other
        }
    }
}

/// Whether the CPU temperature reading can be trusted.
///
/// Fail-closed rule: anything other than `Trusted` forbids *active* boost and drops the
/// daemon back to `observe_only`. Losing the sensor must never make the device faster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThermalTrust {
    /// No sample taken yet (first tick).
    #[default]
    Unknown,
    Trusted,
    /// `/sys/class/thermal` exists but no zone is readable.
    NoSensor,
    /// Zones exist but every value was out of range / unparsable.
    Invalid,
    /// Last trustworthy reading is older than the freshness budget.
    Stale,
    /// The thermal class directory itself is missing (service/HAL absent).
    ServiceUnavailable,
    /// Zones exist, none maps to a CPU-class sensor, and unknown types are present.
    Ambiguous,
}

impl ThermalTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            ThermalTrust::Unknown => "unknown",
            ThermalTrust::Trusted => "trusted",
            ThermalTrust::NoSensor => "no_sensor",
            ThermalTrust::Invalid => "invalid_data",
            ThermalTrust::Stale => "stale_data",
            ThermalTrust::ServiceUnavailable => "thermal_service_unavailable",
            ThermalTrust::Ambiguous => "ambiguous_mapping",
        }
    }

    pub fn is_trusted(self) -> bool {
        matches!(self, ThermalTrust::Trusted)
    }

    /// Operator-facing explanation, used by the UI and the log.
    pub fn reason(self) -> &'static str {
        match self {
            ThermalTrust::Trusted => "CPU temperature sensors are readable",
            ThermalTrust::Unknown => "no temperature sample yet",
            ThermalTrust::NoSensor => "no CPU-class thermal zone could be mapped",
            ThermalTrust::Invalid => "thermal readings are out of range or unparsable",
            ThermalTrust::Stale => "the last valid temperature reading is too old",
            ThermalTrust::ServiceUnavailable => "/sys/class/thermal is not present",
            ThermalTrust::Ambiguous => {
                "thermal zones exist but none can be classified as a CPU sensor"
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThermalZoneInfo {
    pub name: String,
    pub kind: ZoneKind,
    pub temp_c: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ThermalSnapshot {
    pub zones: Vec<ThermalZoneInfo>,
    pub level: Option<ThermalLevel>,
    pub level_source: String,
    pub cpu_c: Option<f64>,
    pub gpu_c: Option<f64>,
    pub skin_c: Option<f64>,
    pub battery_c: Option<f64>,
    /// Highest temperature seen across *CPU-class* zones only.
    pub cpu_max_c: Option<f64>,
    /// Sensor trust. `cpu_max_c` is forced to `None` whenever this is not `Trusted`.
    pub trust: ThermalTrust,
    /// Why trust was lost, in operator terms.
    pub detail: String,
    /// Age of the last trustworthy reading.
    pub age_ms: u64,
    /// Zones that were skipped because their value was implausible.
    pub invalid_zones: usize,
}

#[derive(Debug, Clone, Default)]
pub struct BatterySnapshot {
    pub present: bool,
    pub capacity_pct: Option<f64>,
    pub temp_c: Option<f64>,
    pub status: String,
    pub current_ma: Option<f64>,
    pub voltage_mv: Option<f64>,
    pub charging: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEventKind {
    Foreground,
    Launch,
    ScreenOn,
    ScreenOff,
}

#[derive(Debug, Clone)]
pub struct AppEvent {
    pub ts_ms: u64,
    pub package: String,
    pub kind: AppEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppEventSource {
    Logcat,
    Companion,
    ProcScan,
    #[default]
    None,
}

impl AppEventSource {
    pub fn as_str(self) -> &'static str {
        match self {
            AppEventSource::Logcat => "logcat",
            AppEventSource::Companion => "companion",
            AppEventSource::ProcScan => "proc_scan",
            AppEventSource::None => "none",
        }
    }

    /// How much the predictor should trust events from this source.
    pub fn trust(self) -> f64 {
        match self {
            AppEventSource::Logcat => 1.0,
            AppEventSource::Companion => 1.0,
            AppEventSource::ProcScan => 0.5,
            AppEventSource::None => 0.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AppSnapshot {
    pub foreground: Option<String>,
    pub since_ms: u64,
    pub source: Option<AppEventSource>,
    pub screen: ScreenState,
}

#[derive(Debug, Clone)]
pub struct CollectorStatus {
    pub name: String,
    pub ok: bool,
    pub note: String,
}

#[derive(Debug, Clone, Default)]
pub struct Health {
    pub collectors: Vec<CollectorStatus>,
    pub last_sample_ms: u64,
    pub sample_errors: u64,
}

impl Health {
    pub fn all_ok(&self) -> bool {
        self.collectors.iter().all(|c| c.ok)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Process-relative monotonic time used for durations and freshness.
    pub ts_ms: u64,
    /// Unix epoch time used for persisted history and prediction time buckets.
    pub wall_ts_ms: u64,
    pub uptime_s: f64,
    pub cpu: CpuSnapshot,
    pub mem: MemSnapshot,
    pub psi: PsiSnapshot,
    pub thermal: ThermalSnapshot,
    pub battery: BatterySnapshot,
    pub apps: AppSnapshot,
    pub health: Health,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_order_matches_spec() {
        assert!(PolicyState::ThermalGuard.priority() > PolicyState::MemoryPressure.priority());
        assert!(PolicyState::MemoryPressure.priority() > PolicyState::BatterySaver.priority());
        assert!(PolicyState::BatterySaver.priority() > PolicyState::AppLaunch.priority());
        assert!(PolicyState::AppLaunch.priority() > PolicyState::Game.priority());
        assert!(PolicyState::Game.priority() > PolicyState::Foreground.priority());
        assert!(PolicyState::Foreground.priority() > PolicyState::Balanced.priority());
        assert!(PolicyState::Balanced.priority() > PolicyState::Idle.priority());
        assert!(PolicyState::SafeMode.priority() > PolicyState::ThermalGuard.priority());
    }

    #[test]
    fn zone_classification_never_guesses_by_index() {
        assert_eq!(ZoneKind::classify("cpu-0-0"), ZoneKind::Cpu);
        assert_eq!(ZoneKind::classify("soc_therm"), ZoneKind::Cpu);
        assert_eq!(ZoneKind::classify("gpuss-0"), ZoneKind::Gpu);
        assert_eq!(ZoneKind::classify("battery"), ZoneKind::Battery);
        assert_eq!(ZoneKind::classify("skin-therm"), ZoneKind::Skin);
        assert_eq!(ZoneKind::classify("charger"), ZoneKind::Charger);
        assert_eq!(ZoneKind::classify("pm8350_tz"), ZoneKind::Other);
    }

    #[test]
    fn run_mode_parsing() {
        assert_eq!(RunMode::parse("OBSERVE_ONLY"), Some(RunMode::ObserveOnly));
        assert_eq!(RunMode::parse("dry-run"), Some(RunMode::DryRun));
        assert_eq!(RunMode::parse("enabled"), Some(RunMode::Enabled));
        assert_eq!(RunMode::parse("nonsense"), None);
        assert!(!RunMode::DryRun.writes_allowed());
    }
}
