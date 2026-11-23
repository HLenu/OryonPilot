//! Sampling orchestration: one [`Snapshot`] per tick, built from file reads only.

pub mod appevent;
pub mod battery;
pub mod cpu;
pub mod daemon;
pub mod memory;
pub mod pressure;
pub mod process;
pub mod thermal;

use crate::config::Config;
use crate::sys::SysRoot;
use crate::types::{
    AppEvent, AppEventSource, AppSnapshot, CollectorStatus, Health, MemSnapshot, ScreenState,
    Snapshot,
};
use appevent::AppEventWatcher;
use cpu::CpuCollector;
use memory::MemoryCollector;
use process::ProcCollector;
use thermal::ThermalCollector;

pub struct Collectors {
    cpu: CpuCollector,
    mem: MemoryCollector,
    thermal: ThermalCollector,
    proc: ProcCollector,
    apps: AppEventWatcher,
    proc_scan_ms: u64,
    started_events: bool,
    sample_errors: u64,
}

impl Collectors {
    pub fn new(cfg: &Config) -> Self {
        Self {
            cpu: CpuCollector::new(),
            mem: MemoryCollector::new(),
            thermal: ThermalCollector::new(
                cfg.policy.thermal.clone(),
                cfg.sample.thermal_interval_ms,
            ),
            proc: ProcCollector::new(),
            apps: AppEventWatcher::new(),
            proc_scan_ms: cfg.sample.proc_scan_interval_ms,
            started_events: false,
            sample_errors: 0,
        }
    }

    pub fn apps(&self) -> &AppEventWatcher {
        &self.apps
    }

    pub fn drain_app_events(&self) -> Vec<AppEvent> {
        self.apps.drain()
    }

    pub fn set_mmd(&mut self, info: crate::types::MmdInfo) {
        self.mem.set_mmd(info);
    }

    /// Applies a hot-reloaded config without losing counters or caches.
    pub fn update_config(&mut self, cfg: &Config) {
        self.thermal
            .set_policy(cfg.policy.thermal.clone(), cfg.sample.thermal_interval_ms);
        self.proc_scan_ms = cfg.sample.proc_scan_interval_ms;
        self.apps.update_proc_scan_interval(self.proc_scan_ms);
    }

    pub fn start_event_sources(&mut self, root: &SysRoot, companion_log: String) {
        if self.started_events {
            return;
        }
        self.apps.start(root, companion_log, self.proc_scan_ms);
        self.started_events = true;
    }

    pub fn sample(&mut self, root: &SysRoot, now: u64) -> Snapshot {
        let uptime_s = root
            .read_opt("/proc/uptime")
            .and_then(|s| crate::util::first_f64(&s))
            .unwrap_or(0.0);

        let watch = self.apps.snapshot();
        let screen = if watch.screen != ScreenState::Unknown {
            watch.screen
        } else {
            appevent::read_screen(root)
        };

        let cpu = self.cpu.collect(root, now);
        let mut mem = self.mem.collect(root, now);
        mem.kswapd_cpu_pct = self.proc.kswapd_cpu_pct(root, now);
        mem.kswapd_pid = self.proc.kswapd_pid();
        let psi = pressure::collect(root);
        let thermal = self.thermal.collect(root, now);
        let battery = battery::collect(root);

        // Battery temperature from power_supply wins over any thermal zone.
        let mut thermal = thermal;
        if thermal.battery_c.is_none() {
            thermal.battery_c = battery.temp_c;
        }

        let apps = AppSnapshot {
            foreground: watch.foreground.clone(),
            since_ms: watch.since_ms,
            source: if watch.source == AppEventSource::None {
                None
            } else {
                Some(watch.source)
            },
            screen,
        };

        let health = Health {
            collectors: self.health(&cpu, &mem, &psi, &thermal, &battery, &watch, screen),
            last_sample_ms: now,
            sample_errors: self.sample_errors,
        };

        Snapshot {
            ts_ms: now,
            wall_ts_ms: crate::util::now_ms(),
            uptime_s,
            cpu,
            mem,
            psi,
            thermal,
            battery,
            apps,
            health,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn health(
        &mut self,
        cpu: &crate::types::CpuSnapshot,
        mem: &MemSnapshot,
        psi: &crate::types::PsiSnapshot,
        thermal: &crate::types::ThermalSnapshot,
        battery: &crate::types::BatterySnapshot,
        watch: &appevent::WatchState,
        screen: ScreenState,
    ) -> Vec<CollectorStatus> {
        let cpu_ok = !cpu.clusters.is_empty() && !cpu.cores.is_empty();
        let collectors = vec![
            CollectorStatus {
                name: "cpu".into(),
                ok: cpu_ok,
                note: if cpu_ok {
                    format!("{} cluster(s)", cpu.clusters.len())
                } else if cpu.clusters.is_empty() {
                    "no cpufreq policy found".into()
                } else {
                    "/proc/stat invalid or unreadable".into()
                },
            },
            CollectorStatus {
                name: "memory".into(),
                ok: mem.meminfo_ok,
                note: if mem.meminfo_ok {
                    format!("zram:{}", mem.zram.len())
                } else {
                    "/proc/meminfo invalid or unreadable".into()
                },
            },
            CollectorStatus {
                name: "psi".into(),
                ok: psi.memory.available,
                note: if psi.memory.available {
                    "enabled".into()
                } else {
                    "/proc/pressure/memory unavailable or invalid".into()
                },
            },
            CollectorStatus {
                name: "thermal".into(),
                ok: thermal.trust.is_trusted(),
                note: format!(
                    "{} zone(s), trust={}, level={}",
                    thermal.zones.len(),
                    thermal.trust.as_str(),
                    thermal.level.map(|l| l.as_str()).unwrap_or("UNKNOWN")
                ),
            },
            CollectorStatus {
                name: "battery".into(),
                ok: battery.present && battery.capacity_pct.is_some(),
                note: battery.status.clone(),
            },
            CollectorStatus {
                name: "app_events".into(),
                ok: !watch.started || watch.source != AppEventSource::None,
                note: format!(
                    "source={} started={} logcat={} companion={} proc_scan={} last_observed_ms={}",
                    watch.source.as_str(),
                    watch.started,
                    watch.logcat_alive,
                    watch.companion_seen,
                    watch.proc_scan_used,
                    watch.last_observed_ms
                ),
            },
            CollectorStatus {
                name: "screen".into(),
                ok: screen != ScreenState::Unknown,
                note: format!("{screen:?}"),
            },
        ];
        self.sample_errors = self
            .sample_errors
            .saturating_add(collectors.iter().filter(|collector| !collector.ok).count() as u64);
        collectors
    }
}
