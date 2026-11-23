//! Read-only resource accounting for this daemon, sampled by the runtime.
//!
//! CPU percent uses one fully occupied core as 100% and may exceed 100% when
//! several threads run at once. Context switches from `/proc/self/status` cover
//! the main thread only; they are scheduler accounting, **not** CPU wakeups or
//! hardware wakeup events. Missing data remains unknown.

use crate::json::Json;
use crate::sys::SysRoot;

#[derive(Debug, Clone, Default)]
pub struct DaemonSnapshot {
    pub observed_mono_ms: Option<u64>,
    pub cpu_pct: Option<f64>,
    pub rss_kb: Option<u64>,
    pub threads: Option<u32>,
    pub voluntary_context_switches: Option<u64>,
    pub nonvoluntary_context_switches: Option<u64>,
    pub context_switches_total: Option<u64>,
    pub context_switches_per_sec: Option<f64>,
    pub clock_ticks_per_second: Option<u64>,
    /// The requested stat fields were parsed, independently of rate warm-up.
    pub stat_valid: bool,
    /// All four requested status fields were parsed with the expected units.
    pub status_valid: bool,
}

impl DaemonSnapshot {
    pub fn to_json(&self) -> Json {
        let mut j = Json::obj();
        let number = |value: Option<u64>| {
            value
                .map(|value| Json::num(value as f64))
                .unwrap_or(Json::Null)
        };
        j.set("observed_mono_ms", number(self.observed_mono_ms));
        j.set("cpu_pct", self.cpu_pct.map(Json::num).unwrap_or(Json::Null));
        j.set("cpu_scale", Json::str("one_core_100_percent"));
        j.set("context_switch_scope", Json::str("main_thread"));
        j.set("rss_kb", number(self.rss_kb));
        j.set("threads", number(self.threads.map(u64::from)));
        j.set(
            "voluntary_context_switches",
            number(self.voluntary_context_switches),
        );
        j.set(
            "nonvoluntary_context_switches",
            number(self.nonvoluntary_context_switches),
        );
        j.set(
            "context_switches_total",
            number(self.context_switches_total),
        );
        j.set(
            "context_switches_per_sec",
            self.context_switches_per_sec
                .map(Json::num)
                .unwrap_or(Json::Null),
        );
        j.set(
            "clock_ticks_per_second",
            number(self.clock_ticks_per_second),
        );
        j.set("stat_valid", Json::Bool(self.stat_valid));
        j.set("status_valid", Json::Bool(self.status_valid));
        j.set("valid", Json::Bool(self.stat_valid && self.status_valid));
        j
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    pid: u32,
    start_ticks: u64,
}

#[derive(Clone, Copy)]
struct CpuCounters {
    identity: Identity,
    ticks: u64,
}

#[derive(Clone, Copy)]
struct ContextCounters {
    identity: Identity,
    voluntary: u64,
    nonvoluntary: u64,
}

pub struct DaemonCollector {
    ticks_per_second: Option<u64>,
    previous_cpu: Option<(CpuCounters, u64)>,
    previous_context: Option<(ContextCounters, u64)>,
}

impl Default for DaemonCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonCollector {
    pub fn new() -> Self {
        #[cfg(unix)]
        let ticks_per_second = {
            // USER_HZ is a userspace ABI value, not the kernel's CONFIG_HZ.
            let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            (hz > 0).then_some(hz as u64)
        };
        #[cfg(not(unix))]
        let ticks_per_second = None;
        Self {
            ticks_per_second,
            previous_cpu: None,
            previous_context: None,
        }
    }

    #[cfg(test)]
    fn with_clock_ticks_per_second(hz: u64) -> Self {
        Self {
            ticks_per_second: (hz > 0).then_some(hz),
            previous_cpu: None,
            previous_context: None,
        }
    }

    /// Reads just two self files. The caller controls the collection interval.
    /// Rates cover the interval between valid samples, including unreadable gaps;
    /// counter resets, identity changes and non-increasing time restart warm-up.
    pub fn collect(&mut self, root: &SysRoot, now_mono_ms: u64) -> DaemonSnapshot {
        let cpu = root
            .read_opt("/proc/self/stat")
            .and_then(|text| parse_stat(&text));
        let status = root.read_opt("/proc/self/status");
        let mut snap = DaemonSnapshot {
            observed_mono_ms: Some(now_mono_ms),
            clock_ticks_per_second: self.ticks_per_second,
            stat_valid: cpu.is_some(),
            ..Default::default()
        };

        if let Some(current) = cpu {
            if let (Some((previous, ts)), Some(hz)) = (self.previous_cpu, self.ticks_per_second) {
                if current.identity == previous.identity && now_mono_ms > ts {
                    if let Some(delta) = current.ticks.checked_sub(previous.ticks) {
                        snap.cpu_pct =
                            Some(delta as f64 / hz as f64 * 100_000.0 / (now_mono_ms - ts) as f64);
                    }
                }
            }
            self.previous_cpu = Some((current, now_mono_ms));
        }

        if let Some(status) = status {
            snap.rss_kb = status_number(&status, "VmRSS", Some("kB"));
            snap.threads = status_number(&status, "Threads", None)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0);
            snap.voluntary_context_switches =
                status_number(&status, "voluntary_ctxt_switches", None);
            snap.nonvoluntary_context_switches =
                status_number(&status, "nonvoluntary_ctxt_switches", None);
            snap.context_switches_total = snap
                .voluntary_context_switches
                .zip(snap.nonvoluntary_context_switches)
                .and_then(|(v, n)| v.checked_add(n));
            snap.status_valid = snap.rss_kb.is_some()
                && snap.threads.is_some()
                && snap.context_switches_total.is_some();
            if let (Some(cpu), Some(voluntary), Some(nonvoluntary)) = (
                cpu,
                snap.voluntary_context_switches,
                snap.nonvoluntary_context_switches,
            ) {
                let current = ContextCounters {
                    identity: cpu.identity,
                    voluntary,
                    nonvoluntary,
                };
                if let Some((previous, ts)) = self.previous_context {
                    if current.identity == previous.identity && now_mono_ms > ts {
                        let delta = voluntary
                            .checked_sub(previous.voluntary)
                            .zip(nonvoluntary.checked_sub(previous.nonvoluntary))
                            .and_then(|(v, n)| v.checked_add(n));
                        snap.context_switches_per_sec =
                            delta.map(|n| n as f64 * 1_000.0 / (now_mono_ms - ts) as f64);
                    }
                }
                self.previous_context = Some((current, now_mono_ms));
            }
        }
        snap
    }
}

fn parse_stat(text: &str) -> Option<CpuCounters> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid = text[..open]
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)?;
    // comm (field 2) may contain whitespace and parentheses. Fields after the
    // final ')' start with state(3), then utime(14), stime(15), starttime(22).
    let fields: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    Some(CpuCounters {
        identity: Identity { pid, start_ticks },
        ticks: utime.checked_add(stime)?,
    })
}

fn status_number(text: &str, key: &str, unit: Option<&str>) -> Option<u64> {
    let value = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name == key).then_some(value)
    })?;
    let mut fields = value.split_whitespace();
    let number = fields.next()?.parse().ok()?;
    if fields.next() != unit || fields.next().is_some() {
        return None;
    }
    Some(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> SysRoot {
        let dir = std::env::temp_dir().join(format!("neurosched-self-metrics-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("proc/self")).unwrap();
        SysRoot::at(dir)
    }

    fn stat(utime: u64, stime: u64, start: u64) -> String {
        let mut fields = vec!["0".to_string(); 20];
        fields[0] = "S".into();
        fields[11] = utime.to_string();
        fields[12] = stime.to_string();
        fields[19] = start.to_string();
        format!("321 (daemon (worker) name) {}\n", fields.join(" "))
    }

    fn sample_files(root: &SysRoot, cpu: u64, voluntary: u64, nonvoluntary: u64) {
        std::fs::write(root.resolve("/proc/self/stat"), stat(cpu, 0, 10)).unwrap();
        std::fs::write(root.resolve("/proc/self/status"), format!(
            "Name:\tneuroschedd\nVmRSS:\t8192 kB\nThreads:\t3\nvoluntary_ctxt_switches:\t{voluntary}\nnonvoluntary_ctxt_switches:\t{nonvoluntary}\n"
        )).unwrap();
    }

    #[test]
    fn first_sample_at_zero_has_counters_and_unknown_rates() {
        let root = fixture("first");
        sample_files(&root, 100, 11, 4);
        let mut collector = DaemonCollector::with_clock_ticks_per_second(100);
        let snap = collector.collect(&root, 0);
        assert_eq!(snap.observed_mono_ms, Some(0));
        assert!(snap.stat_valid && snap.status_valid);
        assert_eq!(snap.cpu_pct, None);
        assert_eq!(snap.context_switches_per_sec, None);
        assert_eq!(snap.rss_kb, Some(8192));
        assert_eq!(snap.threads, Some(3));
        assert_eq!(snap.context_switches_total, Some(15));
        let json = snap.to_json();
        assert!(json.get("cpu_pct").unwrap().is_null());
        assert_eq!(json.get("valid").and_then(Json::as_bool), Some(true));
        assert_eq!(
            json.get("context_switch_scope").and_then(Json::as_str),
            Some("main_thread")
        );
    }

    #[test]
    fn multicore_cpu_can_exceed_one_hundred_percent() {
        let root = fixture("multicore");
        let mut collector = DaemonCollector::with_clock_ticks_per_second(100);
        sample_files(&root, 100, 10, 5);
        collector.collect(&root, 1_000);
        sample_files(&root, 350, 40, 15);
        let snap = collector.collect(&root, 2_000);
        assert_eq!(snap.cpu_pct, Some(250.0));
        assert_eq!(snap.context_switches_per_sec, Some(40.0));
    }

    #[test]
    fn missing_reads_preserve_baselines_and_do_not_report_zero() {
        let root = fixture("missing");
        let mut collector = DaemonCollector::with_clock_ticks_per_second(100);
        sample_files(&root, 100, 10, 5);
        collector.collect(&root, 1_000);
        std::fs::remove_file(root.resolve("/proc/self/stat")).unwrap();
        std::fs::remove_file(root.resolve("/proc/self/status")).unwrap();
        let snap = collector.collect(&root, 2_000);
        assert!(!snap.stat_valid && !snap.status_valid);
        assert_eq!(snap.cpu_pct, None);
        assert_eq!(snap.rss_kb, None);
        assert_eq!(snap.threads, None);
        assert_eq!(snap.context_switches_total, None);
        sample_files(&root, 300, 60, 15);
        let snap = collector.collect(&root, 3_000);
        assert_eq!(snap.cpu_pct, Some(100.0));
        assert_eq!(snap.context_switches_per_sec, Some(30.0));
    }

    #[test]
    fn counter_resets_restart_rate_baselines() {
        let root = fixture("reset");
        let mut collector = DaemonCollector::with_clock_ticks_per_second(100);
        sample_files(&root, 500, 50, 40);
        collector.collect(&root, 1_000);
        sample_files(&root, 3, 1, 1);
        let reset = collector.collect(&root, 2_000);
        assert_eq!(reset.cpu_pct, None);
        assert_eq!(reset.context_switches_per_sec, None);
        sample_files(&root, 103, 11, 1);
        let snap = collector.collect(&root, 3_000);
        assert_eq!(snap.cpu_pct, Some(100.0));
        assert_eq!(snap.context_switches_per_sec, Some(10.0));
    }

    #[test]
    fn new_identity_or_non_increasing_time_restarts_warmup() {
        let root = fixture("identity-time");
        let mut collector = DaemonCollector::with_clock_ticks_per_second(100);
        sample_files(&root, 100, 10, 5);
        collector.collect(&root, 1_000);
        sample_files(&root, 200, 20, 10);
        std::fs::write(root.resolve("/proc/self/stat"), stat(200, 0, 11)).unwrap();
        let changed = collector.collect(&root, 2_000);
        assert_eq!(changed.cpu_pct, None);
        assert_eq!(changed.context_switches_per_sec, None);
        let backwards = collector.collect(&root, 1_500);
        assert_eq!(backwards.cpu_pct, None);
        assert_eq!(backwards.context_switches_per_sec, None);
        let repeated = collector.collect(&root, 1_500);
        assert_eq!(repeated.cpu_pct, None);
    }

    #[test]
    fn bad_values_and_wrong_units_are_not_zero_measurements() {
        let root = fixture("invalid");
        let mut collector = DaemonCollector::with_clock_ticks_per_second(100);
        std::fs::write(root.resolve("/proc/self/stat"), stat(u64::MAX, 1, 10)).unwrap();
        std::fs::write(root.resolve("/proc/self/status"),
            "VmRSS: 8 MB\nThreads: 0\nvoluntary_ctxt_switches: -1\nnonvoluntary_ctxt_switches: 2 extra\n"
        ).unwrap();
        let snap = collector.collect(&root, 0);
        assert!(!snap.stat_valid && !snap.status_valid);
        assert_eq!(snap.cpu_pct, None);
        assert_eq!(snap.rss_kb, None);
        assert_eq!(snap.threads, None);
        assert_eq!(snap.context_switches_total, None);
        assert!(parse_stat("321 (unterminated").is_none());
        assert!(parse_stat("321 (x) S 1 2").is_none());
    }

    #[test]
    fn unknown_clock_frequency_does_not_guess_cpu_usage() {
        let root = fixture("unknown-hz");
        let mut collector = DaemonCollector::with_clock_ticks_per_second(0);
        sample_files(&root, 100, 10, 5);
        collector.collect(&root, 1_000);
        sample_files(&root, 200, 20, 15);
        let snap = collector.collect(&root, 2_000);
        assert!(snap.stat_valid && snap.status_valid);
        assert_eq!(snap.clock_ticks_per_second, None);
        assert_eq!(snap.cpu_pct, None);
        assert_eq!(snap.context_switches_per_sec, Some(20.0));
    }
}
